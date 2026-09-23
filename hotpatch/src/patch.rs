//! Reduce a new compilation to the patch a running image needs.
//!
//! The patch is built against the image the process launched with, never
//! against an earlier patch: every function whose content differs from the
//! launch compilation is carried, together with every function that refers to
//! one, up to the routed entry points. Everything else the patch calls is bound
//! to the running image's own copy at an absolute address, and every static
//! the image already has is shared, not duplicated — so a patch sees the same
//! state the launched code does.
//!
//! What decides that a change cannot be patched is gathered into
//! [`Outcome::Restart`] as sentences naming the cause. Anything this module
//! cannot resolve is a restart reason, never a guess.

use crate::image::{Arch, Format, Image, SymKind};
use crate::ir::{self, Kind, Module, Token, Tokens};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write;

/// The patch ABI this builder emits and `frustrate::hot_patch::apply` accepts.
pub const ABI: u32 = 1;

/// The prefix codegen gives the slot of a routed entry point: `<prefix><export>`.
pub const SLOT_PREFIX: &str = "frustrate_hot_slot_";

/// The descriptor symbol a patch exports.
pub const DESCRIPTOR: &str = "frustrate_hot_patch_descriptor";

/// A mutable static as the launch compilation defined it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaticShape {
    pub value_type: u64,
    pub initializer: u64,
}

/// What `patch` needs to remember about the launch compilation.
pub struct Baseline {
    pub hashes: HashMap<String, u64>,
    pub statics: HashMap<String, StaticShape>,
    pub layouts: crate::debuginfo::Layouts,
}

pub enum Outcome {
    Unchanged,
    Restart(Vec<String>),
    Patch(Patch),
}

pub struct Patch {
    /// The patch module.
    pub ir: String,
    /// Symbols bound to the running image: thunks and absolute addresses.
    pub stubs: String,
    /// The functions whose code changed, demangled.
    pub functions: Vec<String>,
    /// The entry points this patch redirects, by export name.
    pub routed: Vec<String>,
    /// Statics whose initializer changed; the running value was kept.
    pub kept_statics: Vec<String>,
}

pub fn statics(module: &Module, hashes: &HashMap<&str, u64>) -> HashMap<String, StaticShape> {
    module
        .entities
        .iter()
        .filter(|e| e.kind == Kind::Global)
        .filter_map(|e| {
            let name = e.name?;
            let linkage = ir::global_linkage(e.text);
            if linkage.constant || linkage.declaration || name.starts_with("llvm.") {
                return None;
            }
            let mut h = std::collections::hash_map::DefaultHasher::new();
            std::hash::Hash::hash(ir::global_value_type(e.text), &mut h);
            Some((
                name.to_string(),
                StaticShape {
                    value_type: std::hash::Hasher::finish(&h),
                    initializer: hashes.get(name).copied().unwrap_or(0),
                },
            ))
        })
        .collect()
}

pub fn baseline(module: &Module) -> Baseline {
    let hashes = module.hashes();
    Baseline {
        statics: statics(module, &hashes),
        hashes: hashes.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        layouts: crate::debuginfo::layouts(
            module
                .entities
                .iter()
                .filter(|e| e.kind == Kind::Metadata)
                .map(|e| e.text),
        ),
    }
}

pub struct Target<'a> {
    pub image: &'a Image,
    /// Running address minus file address.
    pub slide: i64,
    /// The running address of the anchor export, which the patch records.
    pub anchor: u64,
}

pub fn build(base: &Baseline, new: &Module, target: &Target) -> Outcome {
    let image = target.image;
    let hashes = new.hashes();

    let dirty: BTreeSet<&str> = new
        .entities
        .iter()
        .filter(|e| e.kind == Kind::Define)
        .filter_map(|e| e.name)
        .filter(|n| base.hashes.get(*n) != hashes.get(n))
        .collect();
    if dirty.is_empty() {
        return Outcome::Unchanged;
    }

    let mut reasons = Vec::new();
    let newer_layouts = crate::debuginfo::layouts(
        new.entities
            .iter()
            .filter(|e| e.kind == Kind::Metadata)
            .map(|e| e.text),
    );
    for name in base.layouts.changed(&newer_layouts) {
        reasons.push(format!(
            "`{name}` changed layout, and values the running code built still have the old one"
        ));
    }

    let mut kept_statics = Vec::new();
    for (name, shape) in statics(new, &hashes) {
        if let Some(old) = base.statics.get(&name) {
            if old.value_type != shape.value_type {
                reasons.push(format!("static `{}` changed type", demangle(&name)));
            } else if old.initializer != shape.initializer {
                kept_statics.push(demangle(&name));
            }
        }
    }

    // Who refers to whom, over function definitions.
    let references = all_references(new);
    let mut referrers: HashMap<&str, Vec<&str>> = HashMap::new();
    for (name, refs) in &references {
        for r in refs {
            referrers.entry(r).or_default().push(name);
        }
    }

    let roots: HashSet<&str> = new
        .entities
        .iter()
        .filter(|e| e.kind == Kind::Define)
        .filter_map(|e| e.name)
        .filter(|n| image.exported.contains(&format!("{SLOT_PREFIX}{}", ir::unescape(n))))
        .collect();

    // Every function that changed, and every function that refers to one.
    let mut include: BTreeSet<&str> = dirty.clone();
    let mut stack: Vec<&str> = dirty.iter().copied().collect();
    while let Some(n) = stack.pop() {
        for r in referrers.get(n).into_iter().flatten() {
            if include.insert(r) {
                stack.push(r);
            }
        }
    }

    // A changed function that no routed entry point reaches through the patch
    // would be delivered and never run.
    let mut reached: HashSet<&str> = HashSet::new();
    let mut stack: Vec<&str> = include.iter().copied().filter(|n| roots.contains(n)).collect();
    while let Some(n) = stack.pop() {
        if !reached.insert(n) {
            continue;
        }
        for r in references.get(n).into_iter().flatten() {
            if include.contains(r) {
                stack.push(r);
            }
        }
    }
    // What the running library can reach other than through a routed entry
    // point: its own exports, and functions a static holds a pointer to. A
    // changed function reachable only that way cannot be delivered. One
    // reachable from nothing at all is dead code — the library is compiled with
    // `-Clink-dead-code`, so its IR carries functions nothing calls — and an
    // edit to it changes nothing that runs.
    let mut other_entries: Vec<&str> = new
        .entities
        .iter()
        .filter(|e| e.kind == Kind::Define)
        .filter_map(|e| e.name)
        .filter(|n| image.exported.contains(&ir::unescape(n)))
        .collect();
    for e in new.entities.iter().filter(|e| e.kind == Kind::Global) {
        let held = e.name.is_some_and(|n| !new.is_content_addressed(n));
        if held {
            other_entries.extend(new.references(e).into_iter().filter(|r| references.contains_key(r)));
        }
    }
    let mut live: HashSet<&str> = HashSet::new();
    let mut stack = other_entries;
    while let Some(n) = stack.pop() {
        if !live.insert(n) {
            continue;
        }
        stack.extend(references.get(n).into_iter().flatten().copied());
    }
    for d in &dirty {
        if reached.contains(d) || !live.contains(d) {
            continue;
        }
        if image.exported.contains(&ir::unescape(d)) {
            reasons.push(format!(
                "`{}` changed, and it is exported to callers a patch cannot redirect",
                demangle(d)
            ));
        } else {
            reasons.push(format!(
                "`{}` changed, but the running library reaches it only through code a patch cannot replace",
                demangle(d)
            ));
        }
    }

    // Functions the image lacks must travel with the patch, whatever they are.
    let mut kept_constants: BTreeSet<&str> = BTreeSet::new();
    let mut bound: BTreeSet<&str> = BTreeSet::new();
    let mut work: Vec<&str> = include.iter().copied().collect();
    let mut visited: HashSet<&str> = HashSet::new();
    while let Some(n) = work.pop() {
        if !visited.insert(n) {
            continue;
        }
        let refs: Vec<&str> = match new.get(n) {
            Some(e) if e.kind == Kind::Define => references
                .get(n)
                .map(|r| r.iter().copied().collect())
                .unwrap_or_default(),
            Some(e) => new.references(e).into_iter().collect(),
            None => Vec::new(),
        };
        for r in refs {
            if r.starts_with("llvm.") || include.contains(r) {
                continue;
            }
            let Some(entity) = new.get(r) else {
                reasons.push(format!("refers to `{}`, which the compilation does not declare", demangle(r)));
                continue;
            };
            let symbol = image.symbols.get(&ir::unescape(r));
            match entity.kind {
                Kind::Define => match symbol {
                    Some(s) if s.kind == SymKind::Text => {
                        bound.insert(r);
                    }
                    _ => {
                        include.insert(r);
                        work.push(r);
                    }
                },
                Kind::Declare => match symbol {
                    Some(s) if s.kind == SymKind::Text => {
                        bound.insert(r);
                    }
                    _ if image.imported.contains(&ir::unescape(r)) => {}
                    _ => reasons.push(format!(
                        "calls `{}`, which this build of the running library does not contain",
                        demangle(r)
                    )),
                },
                Kind::Global => {
                    let linkage = ir::global_linkage(entity.text);
                    if new.is_content_addressed(r) {
                        kept_constants.insert(r);
                        continue;
                    }
                    match symbol {
                        Some(s) if s.duplicated && s.kind != SymKind::Text => reasons.push(format!(
                            "uses static `{}`, which the running library defines more than once",
                            demangle(r)
                        )),
                        Some(s) if s.kind == SymKind::Tls && image.format == Format::Elf => {
                            reasons.push(format!(
                                "uses thread-local `{}`, which a patch cannot share on ELF",
                                demangle(r)
                            ))
                        }
                        Some(_) => {
                            bound.insert(r);
                        }
                        None if image.imported.contains(&ir::unescape(r)) => {}
                        None if linkage.constant && !linkage.declaration => {
                            kept_constants.insert(r);
                            work.push(r);
                        }
                        None if linkage.declaration => reasons.push(format!(
                            "uses `{}`, which the running library does not contain",
                            demangle(r)
                        )),
                        None => reasons.push(format!(
                            "adds static `{}`, which the running library has no storage for",
                            demangle(r)
                        )),
                    }
                }
                _ => {}
            }
        }
    }

    if !reasons.is_empty() {
        reasons.sort();
        reasons.dedup();
        return Outcome::Restart(reasons);
    }
    // Everything that changed is dead code: there is no entry point to route
    // and nothing for the app to run differently.
    if !dirty.iter().any(|d| reached.contains(d)) {
        return Outcome::Unchanged;
    }

    let tls: BTreeSet<&str> = bound
        .iter()
        .copied()
        .filter(|n| {
            image
                .symbols
                .get(&ir::unescape(n))
                .is_some_and(|s| s.kind == SymKind::Tls)
        })
        .collect();

    let mut routed: Vec<&str> = include.iter().copied().filter(|n| roots.contains(n)).collect();
    routed.sort();

    let ir_text = emit_module(new, &include, &kept_constants, &bound, &tls, &routed, target);
    let stubs = emit_stubs(new, &bound, &tls, target);
    Outcome::Patch(Patch {
        ir: ir_text,
        stubs,
        functions: dirty.iter().map(|d| demangle(d)).collect(),
        routed: routed.iter().map(|r| ir::unescape(r)).collect(),
        kept_statics,
    })
}

fn all_references<'a>(module: &Module<'a>) -> HashMap<&'a str, HashSet<&'a str>> {
    let defs: Vec<_> = module
        .entities
        .iter()
        .filter(|e| e.kind == Kind::Define && e.name.is_some())
        .collect();
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(16);
    let chunk = defs.len().div_ceil(threads).max(1);
    let mut out = HashMap::with_capacity(defs.len());
    std::thread::scope(|scope| {
        let handles: Vec<_> = defs
            .chunks(chunk)
            .map(|part| {
                scope.spawn(move || {
                    part.iter()
                        .map(|e| (e.name.unwrap(), module.references(e)))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        for h in handles {
            out.extend(h.join().expect("reference thread panicked"));
        }
    });
    out
}

pub fn demangle(name: &str) -> String {
    let name = ir::unescape(name);
    format!("{:#}", rustc_demangle::demangle(&name))
}

fn emit_module(
    new: &Module,
    include: &BTreeSet<&str>,
    kept_constants: &BTreeSet<&str>,
    bound: &BTreeSet<&str>,
    tls: &BTreeSet<&str>,
    routed: &[&str],
    target: &Target,
) -> String {
    let mut out = String::new();
    let mut body = String::new();
    let mut metadata_roots: Vec<String> = Vec::new();

    for e in &new.entities {
        match e.kind {
            Kind::Other => {
                let t = e.text;
                if t.starts_with("source_filename")
                    || t.starts_with("target ")
                    || t.starts_with('%')
                    || t.starts_with('$')
                {
                    out.push_str(t);
                    out.push('\n');
                }
            }
            Kind::Attributes => {
                body.push_str(e.text);
                body.push('\n');
            }
            Kind::Global => {
                let Some(name) = e.name else { continue };
                if kept_constants.contains(name) {
                    let text = strip_debug(e.text, &mut metadata_roots);
                    body.push_str(&text);
                    body.push('\n');
                } else if bound.contains(name) {
                    let value_type = ir::global_value_type(e.text);
                    let _ = writeln!(body, "{} = external global {value_type}", ir::spell(&ir::unescape(name)));
                }
            }
            Kind::Define => {
                let Some(name) = e.name else { continue };
                if include.contains(name) {
                    let mut text = strip_debug(e.text, &mut metadata_roots);
                    if !tls.is_empty() {
                        text = text.replace("@llvm.threadlocal.address.p0(", "@frustrate_hot_tlv_get(");
                    }
                    body.push_str(&text);
                    body.push('\n');
                } else if bound.contains(name) {
                    body.push_str(&declaration(e.text));
                    body.push('\n');
                }
            }
            Kind::Declare => {
                let Some(name) = e.name else { continue };
                if name.starts_with("llvm.dbg.") {
                    continue;
                }
                body.push_str(e.text);
                body.push('\n');
            }
            Kind::Metadata => {
                if e.text.starts_with("!llvm.module.flags") || e.text.starts_with("!llvm.ident") {
                    metadata_roots.extend(metadata_refs(e.text));
                    body.push_str(e.text);
                    body.push('\n');
                }
            }
        }
    }
    if !tls.is_empty() {
        body.push_str("declare ptr @frustrate_hot_tlv_get(ptr)\n");
    }

    // The non-debug metadata the kept text still names, and what that names.
    let nodes: HashMap<&str, &str> = new
        .entities
        .iter()
        .filter(|e| e.kind == Kind::Metadata)
        .filter_map(|e| {
            let rest = e.text.strip_prefix('!')?;
            let (id, _) = rest.split_once(" = ")?;
            id.bytes().all(|c| c.is_ascii_digit()).then_some((id, e.text))
        })
        .collect();
    let mut emitted: BTreeSet<&str> = BTreeSet::new();
    let mut stack: Vec<String> = metadata_roots;
    while let Some(id) = stack.pop() {
        let Some((&key, text)) = nodes.get_key_value(id.as_str()) else {
            continue;
        };
        if !emitted.insert(key) {
            continue;
        }
        stack.extend(metadata_refs(text));
    }
    for id in &emitted {
        body.push_str(nodes[id]);
        body.push('\n');
    }

    body.push_str(&descriptor(routed, target));
    out.push_str(&body);
    out
}

fn metadata_refs(text: &str) -> Vec<String> {
    Tokens::new(text)
        .filter_map(|t| match t {
            Token::MetadataRef(d) => Some(d.to_string()),
            _ => None,
        })
        .collect()
}

/// Remove debug info from an entity's text: `!dbg` attachments, debug records
/// and debug intrinsic calls. Any other metadata attachment is kept unless it
/// reaches debug info, and its numbers are collected for emission.
fn strip_debug(text: &str, roots: &mut Vec<String>) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.split('\n').enumerate() {
        let trimmed = line.trim_start();
        if i > 0
            && (trimmed.starts_with("#dbg_")
                || trimmed.contains("call void @llvm.dbg."))
        {
            continue;
        }
        if i > 0 {
            out.push('\n');
        }
        let mut pending_attachment: Option<usize> = None;
        let mut line_out = String::with_capacity(line.len());
        for token in Tokens::new(line) {
            match token {
                Token::Text(t) => {
                    line_out.push_str(t);
                    pending_attachment = attachment_start(&line_out);
                }
                Token::MetadataRef(d) => match pending_attachment.take() {
                    Some(start) if line_out[start..].contains("!dbg") => {
                        line_out.truncate(start);
                    }
                    Some(start) => {
                        // A non-debug attachment such as `!noundef !7`. Loop
                        // metadata names source locations, so it goes too.
                        if line_out[start..].contains("!llvm.loop") {
                            line_out.truncate(start);
                        } else {
                            line_out.push('!');
                            line_out.push_str(d);
                            roots.push(d.to_string());
                        }
                    }
                    None => {
                        line_out.push('!');
                        line_out.push_str(d);
                        roots.push(d.to_string());
                    }
                },
                Token::Global(n) => {
                    line_out.push_str(&ir::spell(&ir::unescape(n)));
                    pending_attachment = None;
                }
                Token::AttributeGroup(g) => {
                    line_out.push('#');
                    line_out.push_str(g);
                    pending_attachment = None;
                }
                Token::Comment(_) => {}
            }
        }
        out.push_str(line_out.trim_end());
    }
    out
}

/// If `s` ends with a metadata attachment's kind (`, !dbg ` or ` !dbg `, `!srcloc`
/// …), where that attachment starts.
fn attachment_start(s: &str) -> Option<usize> {
    let trimmed = s.strip_suffix(' ')?;
    let bang = trimmed.rfind('!')?;
    let kind = &trimmed[bang + 1..];
    if kind.is_empty() || !kind.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'.' || c == b'_') {
        return None;
    }
    let before = &trimmed[..bang];
    if let Some(p) = before.strip_suffix(", ") {
        Some(p.len())
    } else {
        before.strip_suffix(' ').map(|p| p.len())
    }
}

/// A `declare` for a `define`'s header.
fn declaration(define: &str) -> String {
    let header = define.split('\n').next().unwrap_or(define);
    let header = header.strip_suffix(" {").unwrap_or(header);
    let Some(at) = find_global_token(header) else {
        return String::new();
    };
    let prefix = &header["define ".len()..at];
    let rest = &header[at..];
    let Some(close) = matching_paren(rest) else {
        return String::new();
    };
    let (signature, suffix) = rest.split_at(close + 1);

    const DROP: &[&str] = &[
        "private", "internal", "linkonce_odr", "linkonce", "weak_odr", "weak",
        "available_externally", "common", "appending", "extern_weak", "unnamed_addr",
        "local_unnamed_addr", "dso_local",
    ];
    let prefix: Vec<&str> = prefix.split(' ').filter(|w| !w.is_empty() && !DROP.contains(w)).collect();

    let words: Vec<&str> = split_words(suffix);
    let mut kept = Vec::new();
    let mut i = 0;
    while i < words.len() {
        let w = words[i];
        if w.starts_with('#') && w[1..].bytes().all(|c| c.is_ascii_digit()) {
            kept.push(w);
            i += 1;
        } else if w.starts_with("comdat") || w == "unnamed_addr" || w == "local_unnamed_addr" {
            i += 1;
        } else if matches!(w, "section" | "partition" | "gc" | "align") {
            i += 2;
        } else if matches!(w, "personality" | "prefix" | "prologue") {
            i += 3;
        } else if w.starts_with('!') {
            i += 2;
        } else {
            kept.push(w);
            i += 1;
        }
    }
    let mut out = String::from("declare ");
    for w in prefix {
        out.push_str(w);
        out.push(' ');
    }
    out.push_str(signature);
    for w in kept {
        out.push(' ');
        out.push_str(w);
    }
    out
}

fn find_global_token(header: &str) -> Option<usize> {
    let bytes = header.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    i += 1;
                }
            }
            b'@' => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

/// Index of the `)` closing the first `(` after the name.
fn matching_paren(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 1;
    if bytes.get(1) == Some(&b'"') {
        i = 2;
        while i < bytes.len() && bytes[i] != b'"' {
            i += 1;
        }
    }
    let open = s[i..].find('(')? + i;
    let mut depth = 0;
    let mut j = open;
    while j < bytes.len() {
        match bytes[j] {
            b'"' => {
                j += 1;
                while j < bytes.len() && bytes[j] != b'"' {
                    j += 1;
                }
            }
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(j);
                }
            }
            _ => {}
        }
        j += 1;
    }
    None
}

fn split_words(s: &str) -> Vec<&str> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i] == b' ' {
            i += 1;
        }
        let start = i;
        let mut depth = 0;
        while i < bytes.len() && (bytes[i] != b' ' || depth > 0) {
            match bytes[i] {
                b'"' => {
                    i += 1;
                    while i < bytes.len() && bytes[i] != b'"' {
                        i += 1;
                    }
                }
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ => {}
            }
            i += 1;
        }
        if i > start {
            out.push(&s[start..i]);
        }
    }
    out
}

fn descriptor(routed: &[&str], target: &Target) -> String {
    let mut out = String::new();
    let identity = target.image.identity.clone().unwrap_or_default();
    let mut id_bytes = [0u8; 32];
    id_bytes[..identity.len().min(32)].copy_from_slice(&identity[..identity.len().min(32)]);
    let entries = if routed.is_empty() {
        "ptr null".to_string()
    } else {
        for (i, r) in routed.iter().enumerate() {
            let slot = format!("{SLOT_PREFIX}{}", ir::unescape(r));
            let _ = writeln!(
                out,
                "@__frustrate_hot_patch_slot.{i} = private unnamed_addr constant [{} x i8] c\"{}\\00\"",
                slot.len() + 1,
                escape_bytes(slot.as_bytes())
            );
        }
        let items: Vec<String> = routed
            .iter()
            .enumerate()
            .map(|(i, r)| {
                format!(
                    "{{ ptr, ptr }} {{ ptr @__frustrate_hot_patch_slot.{i}, ptr {} }}",
                    ir::spell(&ir::unescape(r))
                )
            })
            .collect();
        let _ = writeln!(
            out,
            "@__frustrate_hot_patch_entries = private constant [{} x {{ ptr, ptr }}] [{}]",
            routed.len(),
            items.join(", ")
        );
        "ptr @__frustrate_hot_patch_entries".to_string()
    };
    let _ = writeln!(
        out,
        "@{DESCRIPTOR} = constant {{ i32, i32, [32 x i8], i64, i64, ptr }} {{ i32 {ABI}, i32 {}, [32 x i8] c\"{}\", i64 {}, i64 {}, {entries} }}",
        identity.len().min(32),
        escape_bytes(&id_bytes),
        target.anchor,
        routed.len(),
    );
    out
}

fn escape_bytes(bytes: &[u8]) -> String {
    let mut s = String::new();
    for &b in bytes {
        if b.is_ascii_graphic() && b != b'"' && b != b'\\' {
            s.push(b as char);
        } else {
            let _ = write!(s, "\\{b:02X}");
        }
    }
    s
}

/// Thunks for bound functions and absolute symbols for bound data, as a module
/// of assembly for the image's architecture.
fn emit_stubs(new: &Module, bound: &BTreeSet<&str>, tls: &BTreeSet<&str>, target: &Target) -> String {
    let image = target.image;
    let mangle = |n: &str| match image.format {
        Format::MachO => format!("\"_{n}\""),
        Format::Elf => format!("\"{n}\""),
    };
    let mut asm = String::new();
    let _ = writeln!(asm, ".text");
    for name in bound {
        let raw = ir::unescape(name);
        let Some(sym) = image.symbols.get(&raw) else { continue };
        let address = (sym.address as i64 + target.slide) as u64;
        let label = mangle(&raw);
        match sym.kind {
            SymKind::Text => {
                let _ = writeln!(asm, ".globl {label}");
                if image.format == Format::Elf {
                    let _ = writeln!(asm, ".type {label}, @function");
                }
                let _ = writeln!(asm, ".p2align 2");
                let _ = writeln!(asm, "{label}:");
                match image.arch {
                    Arch::Aarch64 => {
                        let _ = writeln!(asm, "  ldr x16, 1f");
                        let _ = writeln!(asm, "  br x16");
                        let _ = writeln!(asm, "1:");
                        let _ = writeln!(asm, "  .quad {address:#x}");
                    }
                    Arch::X86_64 => {
                        let _ = writeln!(asm, "  jmpq *1f(%rip)");
                        let _ = writeln!(asm, "1:");
                        let _ = writeln!(asm, "  .quad {address:#x}");
                    }
                }
            }
            SymKind::Data | SymKind::Tls => {
                let _ = writeln!(asm, ".globl {label}");
                let _ = writeln!(asm, ".set {label}, {address:#x}");
            }
        }
    }
    if !tls.is_empty() {
        let label = mangle("frustrate_hot_tlv_get");
        let _ = writeln!(asm, ".globl {label}");
        let _ = writeln!(asm, ".p2align 2");
        let _ = writeln!(asm, "{label}:");
        match image.arch {
            // A Mach-O TLV descriptor's first word is its getter, which takes the
            // descriptor in the first argument register.
            Arch::Aarch64 => {
                let _ = writeln!(asm, "  ldr x16, [x0]");
                let _ = writeln!(asm, "  br x16");
            }
            Arch::X86_64 => {
                let _ = writeln!(asm, "  jmpq *(%rdi)");
            }
        }
    }
    let mut out = String::new();
    if let Some(triple) = new.target_triple() {
        let _ = writeln!(out, "target triple = \"{triple}\"");
    }
    for line in asm.lines() {
        let _ = writeln!(out, "module asm \"{}\"", escape_bytes_keep_space(line.as_bytes()));
    }
    out
}

fn escape_bytes_keep_space(bytes: &[u8]) -> String {
    let mut s = String::new();
    for &b in bytes {
        if (b.is_ascii_graphic() || b == b' ') && b != b'"' && b != b'\\' {
            s.push(b as char);
        } else {
            let _ = write!(s, "\\{b:02X}");
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declaration_drops_linkage_and_trailing_clauses() {
        let define = "define internal fastcc { i64, i1 } @\"_RNv$x\"(ptr noalias %self, i64 %n) unnamed_addr #3 personality ptr @rust_eh_personality !dbg !4561 {\nstart:\n  ret void\n}";
        assert_eq!(
            declaration(define),
            "declare fastcc { i64, i1 } @\"_RNv$x\"(ptr noalias %self, i64 %n) #3"
        );
        let comdat = "define linkonce_odr hidden void @f() #1 comdat !dbg !7 {\n}";
        assert_eq!(declaration(comdat), "declare hidden void @f() #1");
    }

    /// A library compiled with `-Clink-dead-code` carries functions nothing
    /// calls. Editing one changes nothing that runs, so there is nothing to
    /// deliver — while an edit the running library reaches only outside the
    /// routed entry points cannot be delivered and says so.
    #[test]
    fn what_reaches_a_changed_function_decides_the_answer() {
        const MODULE: &str = r#"target triple = "arm64-apple-macosx26.0"
@frustrate_hot_slot_frustrate_call_sync = internal global ptr null, align 8
@HOOK = internal global ptr @held, align 8

define i32 @frustrate_call_sync(i32 %0) unnamed_addr {
start:
  %r = call i32 @helper(i32 %0)
  ret i32 %r
}

define internal i32 @helper(i32 %0) unnamed_addr {
start:
  ret i32 %0
}

define internal i32 @held(i32 %0) unnamed_addr {
start:
  ret i32 %0
}

define internal i32 @dead(i32 %0) unnamed_addr {
start:
  ret i32 %0
}
"#;
        let module = Module::parse(MODULE).unwrap();
        let image = crate::image::Image::for_test(
            &["frustrate_call_sync", "helper", "held", "dead"],
            &["frustrate_hot_slot_frustrate_call_sync", "HOOK"],
            &["frustrate_call_sync", "frustrate_hot_slot_frustrate_call_sync"],
        );
        let target = Target { image: &image, slide: 0x4000, anchor: 0x5000 };
        let baseline = |edited: &str| {
            let mut b = baseline(&module);
            *b.hashes.get_mut(edited).unwrap() += 1;
            b
        };

        match build(&baseline("helper"), &module, &target) {
            Outcome::Patch(p) => assert_eq!(p.functions, ["helper"]),
            Outcome::Restart(r) => panic!("{r:?}"),
            Outcome::Unchanged => panic!("a routed edit has something to deliver"),
        }
        assert!(
            matches!(build(&baseline("dead"), &module, &target), Outcome::Unchanged),
            "an edit to a function nothing calls delivers nothing"
        );
        match build(&baseline("held"), &module, &target) {
            Outcome::Restart(reasons) => assert!(
                reasons[0].contains("`held` changed") && reasons[0].contains("only through"),
                "{reasons:?}"
            ),
            _ => panic!("a function held by a static cannot be redirected"),
        }
    }

    #[test]
    fn debug_attachments_and_records_are_stripped() {
        let text = "define void @f() #0 !dbg !5 {\nstart:\n    #dbg_value(i64 0, !9, !DIExpression(), !10)\n  %x = load i64, ptr @G, align 8, !dbg !11, !noundef !12\n  br label %l, !llvm.loop !13\n}";
        let mut roots = Vec::new();
        let out = strip_debug(text, &mut roots);
        assert_eq!(
            out,
            "define void @f() #0 {\nstart:\n  %x = load i64, ptr @G, align 8, !noundef !12\n  br label %l\n}"
        );
        assert_eq!(roots, vec!["12".to_string()]);
    }
}
