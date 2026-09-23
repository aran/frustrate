//! Type layouts, read from a module's debug info.
//!
//! A patch runs new code against values the running image already built, so a
//! type whose layout moved cannot be patched: the new code would read old bytes
//! at new offsets. Types are matched across two compilations by their
//! qualified name (`crate::module::Type<Args>`), not by the debug info's
//! `identifier`: rustc derives that from the type's definition, fields
//! included, so the one type whose layout moved is exactly the one whose
//! identifier would not match.
//!
//! What a layout covers: the composite's tag, size and alignment; each member's
//! name, offset, size and type; each enum variant's discriminant and payload
//! type; each C-like enumerator. A member's type is named by qualified name when
//! it is itself a composite — that composite is compared on its own — and spelled
//! out otherwise, through pointers and typedefs, so that `Box<A>` becoming
//! `Box<B>` is a change while `A` gaining a field is reported once, as `A`.
//! A trait object's vtable is a composite too (`<T as Trait>::{vtable_type}`),
//! so a trait gaining a method is a changed layout.
//!
//! What it cannot see: an invariant that moved without the layout moving (a
//! field reinterpreted at the same type and offset). That is outside what any
//! layout check can decide, and a patch carries it silently.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

/// Composite types by qualified name: a hash of the layout, and the name again
/// for messages. Distinct types that print alike (rare: two unnamed variant
/// parts in one scope) share an entry whose hash covers all of them.
pub struct Layouts {
    pub types: HashMap<String, (u64, String)>,
}

/// Metadata nodes by number.
struct Nodes<'a> {
    by_id: Vec<Option<&'a str>>,
}

impl<'a> Nodes<'a> {
    fn get(&self, id: &str) -> Option<&'a str> {
        let id: usize = id.parse().ok()?;
        self.by_id.get(id).copied().flatten()
    }
}

pub fn layouts<'a>(metadata: impl Iterator<Item = &'a str>) -> Layouts {
    let mut by_id: Vec<Option<&'a str>> = Vec::new();
    let mut composites: Vec<&'a str> = Vec::new();
    for line in metadata {
        let Some(rest) = line.strip_prefix('!') else {
            continue;
        };
        let Some((id, body)) = rest.split_once(" = ") else {
            continue;
        };
        let Ok(id) = id.parse::<usize>() else {
            // Named metadata (`!llvm.dbg.cu`).
            continue;
        };
        let body = body.strip_prefix("distinct ").unwrap_or(body);
        if by_id.len() <= id {
            by_id.resize(id + 1, None);
        }
        by_id[id] = Some(body);
        if body.starts_with("!DICompositeType(") {
            composites.push(body);
        }
    }
    let nodes = Nodes { by_id };
    let mut by_name: HashMap<String, Vec<u64>> = HashMap::new();
    for body in composites {
        // An unnamed composite (an array, an anonymous aggregate) is no type a
        // value is built as on its own: its layout is part of the named type or
        // static that holds it, which is compared there.
        let Some(name) = display_name(&nodes, body) else {
            continue;
        };
        let mut h = std::collections::hash_map::DefaultHasher::new();
        signature(&nodes, body, &mut h);
        by_name.entry(name).or_default().push(h.finish());
    }
    let types = by_name
        .into_iter()
        .map(|(name, mut hashes)| {
            hashes.sort_unstable();
            hashes.dedup();
            let mut h = std::collections::hash_map::DefaultHasher::new();
            hashes.hash(&mut h);
            (name.clone(), (h.finish(), name))
        })
        .collect();
    Layouts { types }
}

impl Layouts {
    /// Readable names of the types present in both whose layout differs.
    pub fn changed(&self, newer: &Layouts) -> Vec<String> {
        let mut out: Vec<String> = self
            .types
            .iter()
            .filter_map(|(id, (hash, name))| match newer.types.get(id) {
                Some((h, _)) if h != hash => Some(name.clone()),
                _ => None,
            })
            .collect();
        out.sort();
        out.dedup();
        out
    }
}

fn signature(nodes: &Nodes, composite: &str, h: &mut impl Hasher) {
    let f = fields(composite);
    for key in ["tag", "size", "align"] {
        f.get(key).hash(h);
    }
    if let Some(d) = f.get("discriminator") {
        "discriminator".hash(h);
        member(nodes, d, h);
    }
    for element in tuple(nodes, f.get("elements").copied()) {
        match element {
            Some(reference) => member(nodes, reference, h),
            None => "?".hash(h),
        }
    }
}

fn member(nodes: &Nodes, reference_or_body: &str, h: &mut impl Hasher) {
    let body = match reference_or_body.strip_prefix('!') {
        Some(id) if id.bytes().all(|c| c.is_ascii_digit()) => match nodes.get(id) {
            Some(b) => b,
            None => {
                "?".hash(h);
                return;
            }
        },
        _ => reference_or_body,
    };
    let f = fields(body);
    if body.starts_with("!DIDerivedType(") {
        "member".hash(h);
        for key in ["tag", "name", "offset", "size", "extraData"] {
            f.get(key).hash(h);
        }
        type_ref(nodes, f.get("baseType").copied(), h, 0);
    } else if body.starts_with("!DIEnumerator(") {
        "enumerator".hash(h);
        f.get("name").hash(h);
        f.get("value").hash(h);
    } else if body.starts_with("!DICompositeType(") {
        // A variant part nested in an enum: compared on its own.
        "nested".hash(h);
        display_name(nodes, body).hash(h);
    } else {
        // Methods and anything else a composite lists do not occupy it.
    }
}

fn type_ref(nodes: &Nodes, reference: Option<&str>, h: &mut impl Hasher, depth: u32) {
    let Some(reference) = reference else {
        "void".hash(h);
        return;
    };
    let Some(body) = reference.strip_prefix('!').and_then(|id| nodes.get(id)) else {
        reference.hash(h);
        return;
    };
    let f = fields(body);
    if body.starts_with("!DICompositeType(") {
        match display_name(nodes, body) {
            Some(name) => {
                "#".hash(h);
                name.hash(h);
            }
            // An array or anonymous aggregate member is spelled out in full.
            None => signature(nodes, body, h),
        }
        return;
    }
    if depth > 8 {
        "deep".hash(h);
        return;
    }
    // Basic types, pointers, typedefs, qualifiers, subroutine types.
    body.split('(').next().hash(h);
    for key in ["tag", "name", "size", "encoding"] {
        f.get(key).hash(h);
    }
    type_ref(nodes, f.get("baseType").copied(), h, depth + 1);
}

fn tuple<'a>(nodes: &Nodes<'a>, reference: Option<&str>) -> Vec<Option<&'a str>> {
    let Some(body) = reference
        .and_then(|r| r.strip_prefix('!'))
        .and_then(|id| nodes.get(id))
    else {
        return Vec::new();
    };
    let Some(inner) = body.strip_prefix("!{").and_then(|b| b.strip_suffix('}')) else {
        return Vec::new();
    };
    split_top_level(inner)
        .into_iter()
        .map(|e| {
            let e = e.trim();
            (!e.is_empty()).then_some(e)
        })
        .collect()
}

fn display_name(nodes: &Nodes, composite: &str) -> Option<String> {
    let mut parts = Vec::new();
    let mut body = composite;
    for _ in 0..32 {
        let f = fields(body);
        if let Some(name) = f.get("name") {
            parts.push(unquote(name));
        }
        match f
            .get("scope")
            .and_then(|s| s.strip_prefix('!'))
            .and_then(|id| nodes.get(id))
        {
            Some(parent) if !parent.starts_with("!DIFile(") && !parent.starts_with("!DICompileUnit(") => {
                body = parent
            }
            _ => break,
        }
    }
    if parts.is_empty() {
        return None;
    }
    parts.reverse();
    Some(parts.join("::"))
}

fn unquote(s: &str) -> String {
    crate::ir::unescape(s.trim_matches('"'))
}

/// The top-level `key: value` fields of `!DIKind(…)`.
fn fields(body: &str) -> HashMap<&str, &str> {
    let mut out = HashMap::new();
    let Some(open) = body.find('(') else {
        return out;
    };
    let inner = body[open + 1..].strip_suffix(')').unwrap_or(&body[open + 1..]);
    for part in split_top_level(inner) {
        if let Some((k, v)) = part.split_once(": ") {
            out.insert(k.trim(), v.trim());
        }
    }
    out
}

/// Split on commas outside brackets and strings.
fn split_top_level(s: &str) -> Vec<&str> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    i += 1;
                }
            }
            b'(' | b'{' | b'[' | b'<' => depth += 1,
            b')' | b'}' | b']' | b'>' => depth -= 1,
            b',' if depth == 0 => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    out.push(&s[start..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn module(point_fields: &str) -> String {
        format!(
            r#"!1 = !DINamespace(name: "api", scope: !2)
!2 = !DINamespace(name: "fixture", scope: null)
!3 = !DIBasicType(name: "i64", size: 64, encoding: DW_ATE_signed)
!4 = !DICompositeType(tag: DW_TAG_structure_type, name: "Point", scope: !1, size: 128, align: 64, elements: !5, identifier: "p1")
!5 = !{{{point_fields}}}
!6 = !DIDerivedType(tag: DW_TAG_member, name: "x", scope: !4, baseType: !3, size: 64, align: 64, offset: 0)
!7 = !DIDerivedType(tag: DW_TAG_member, name: "y", scope: !4, baseType: !3, size: 64, align: 64, offset: 64)
!8 = !DIDerivedType(tag: DW_TAG_member, name: "y", scope: !4, baseType: !3, size: 64, align: 64, offset: 0)
!9 = !DIDerivedType(tag: DW_TAG_member, name: "x", scope: !4, baseType: !3, size: 64, align: 64, offset: 64)
!10 = !DIDerivedType(tag: DW_TAG_pointer_type, name: "&Point", baseType: !4, size: 64, align: 64)
!11 = !DICompositeType(tag: DW_TAG_structure_type, name: "Holder", scope: !1, size: 64, align: 64, elements: !12, identifier: "h1")
!12 = !{{!13}}
!13 = !DIDerivedType(tag: DW_TAG_member, name: "p", scope: !11, baseType: !10, size: 64, align: 64, offset: 0)
!14 = !DICompositeType(tag: DW_TAG_array_type, baseType: !3, size: 128, align: 64, elements: !15)
!15 = !{{}}
"#
        )
    }

    #[test]
    fn a_field_reorder_is_a_changed_layout_named_by_path() {
        let a = module("!6, !7");
        // rustc's identifier moves with the fields, so it cannot be the key.
        let b = module("!8, !9").replace("identifier: \"p1\"", "identifier: \"p2\"");
        let la = layouts(a.lines());
        let lb = layouts(b.lines());
        assert_eq!(la.changed(&lb), vec!["fixture::api::Point".to_string()]);
    }

    #[test]
    fn an_identical_layout_is_not_a_change_and_pointers_do_not_propagate() {
        let a = module("!6, !7");
        let la = layouts(a.lines());
        let lb = layouts(a.lines());
        assert!(la.changed(&lb).is_empty());
        let reordered = layouts(module("!8, !9").lines());
        // An unnamed array changing size (a string literal's bytes, say) is not
        // a layout a running value has.
        let longer = layouts(a.replace("DW_TAG_array_type, baseType: !3, size: 128", "DW_TAG_array_type, baseType: !3, size: 256").lines());
        assert!(la.changed(&longer).is_empty(), "{:?}", la.changed(&longer));
        // Holder points at Point; only Point is reported.
        assert!(!la.changed(&reordered).contains(&"fixture::api::Holder".to_string()));
    }
}
