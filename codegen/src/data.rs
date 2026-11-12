// `write_with_newline` is off for this file, for the reason `emit_dart.rs`
// gives: the format strings mirror the generated output line for line.
#![allow(clippy::write_with_newline)]

//! The data library: a bridge's plain data classes, in a Dart library of
//! their own.
//!
//! The bindings declare everything a Dart program needs to call the bridge,
//! and their package carries the bridge's native library. Code that only
//! builds, compares and passes around data values needs neither, so a bridge
//! given a data library (`frustrate_bridge.data_package_name`) declares those
//! classes there instead. The bindings import and re-export them, so an
//! application importing the bindings sees no difference, and write the
//! codecs, which belong to the wire.
//!
//! The library holds no codec, no schema fingerprint and nothing derived from
//! a function, so it changes only when a data declaration does.
//!
//! A class goes there when nothing in it needs the bridge:
//!
//! - it reaches no handle, and is not `inbound` or a `dart_interface`;
//! - it has no bridged members, which are methods on its class that call the
//!   bridge (a generic template's members are an extension per
//!   instantiation, which the bindings keep);
//! - it is not taken from a bridge in `models`, whose class that bridge
//!   declares, and is not also a handle;
//! - every data type its fields name goes there too.

use crate::emit_dart::{emit_enum_class, emit_struct_class};
use crate::ir::*;
use std::collections::BTreeSet;
use std::fmt::Write;

/// Record on `iface` that its data library is importable as `import`, and
/// which classes it holds. Called on an interface `models::apply` has seen.
pub fn apply(iface: &mut Interface, import: &str) {
    let types = held(iface);
    iface.data = Some(DataLibrary {
        import: import.to_string(),
        types,
    });
}

/// Whether the class of the declaration `name` (an authored struct or enum,
/// or a generic template) is declared in the data library.
pub(crate) fn holds(iface: &Interface, name: &str) -> bool {
    iface
        .data
        .as_ref()
        .is_some_and(|d| d.types.iter().any(|t| t == name))
}

/// Every declaration that mints a class, in emission order: templates first,
/// as the surfaces emit them.
fn class_names(iface: &Interface) -> Vec<&str> {
    iface
        .generic_structs
        .iter()
        .map(|s| s.name.as_str())
        .chain(iface.generic_enums.iter().map(|e| e.name.as_str()))
        .chain(
            iface
                .structs
                .iter()
                .filter(|s| s.instance.is_none())
                .map(|s| s.name.as_str()),
        )
        .chain(
            iface
                .enums
                .iter()
                .filter(|e| e.instance.is_none())
                .map(|e| e.name.as_str()),
        )
        .collect()
}

/// The field types a class declares: a template's own, with its parameters
/// in them.
fn field_types<'a>(iface: &'a Interface, name: &str) -> Vec<&'a Type> {
    let struct_fields = iface
        .generic_struct(name)
        .or_else(|| iface.struct_decl(name))
        .map(|s| s.fields.iter().map(|f| &f.ty).collect::<Vec<_>>());
    let enum_fields = iface
        .generic_enum(name)
        .or_else(|| iface.enum_decl(name))
        .map(|e| {
            e.variants
                .iter()
                .flat_map(|v| v.fields.iter().map(|f| &f.ty))
                .collect::<Vec<_>>()
        });
    struct_fields
        .into_iter()
        .chain(enum_fields)
        .flatten()
        .collect()
}

/// What rules the class `name` out of the data library on its own, without
/// looking at its fields.
fn excluded(iface: &Interface, name: &str) -> bool {
    let flagged = iface
        .generic_struct(name)
        .or_else(|| iface.struct_decl(name))
        .is_some_and(|s| s.inbound || s.dart_interface);
    flagged
        || iface.model_of(name).is_some()
        || iface.opaque(name).is_some()
        || iface.members(name, Repr::Data).next().is_some()
}

/// Whether a field of type `ty` can be declared in the data library, given
/// the classes `held` it may name.
fn field_ok(iface: &Interface, ty: &Type, held: &BTreeSet<&str>) -> bool {
    match ty {
        Type::Bool
        | Type::I8
        | Type::I16
        | Type::I32
        | Type::I64
        | Type::U8
        | Type::U16
        | Type::U32
        | Type::U64
        | Type::I128
        | Type::U128
        | Type::F32
        | Type::F64
        | Type::Usize
        | Type::Isize
        | Type::String
        | Type::Char
        | Type::Duration(_)
        | Type::SystemTime(_)
        | Type::Bytes
        | Type::ByteArray(_)
        | Type::Param(_)
        | Type::Extern(_) => true,
        Type::Array(t, _)
        | Type::List(t, _)
        | Type::Set(t, _)
        | Type::Option(t)
        | Type::Boxed(t) => field_ok(iface, t, held),
        Type::Map(k, v, _) => field_ok(iface, k, held) && field_ok(iface, v, held),
        Type::Tuple(ts) => ts.iter().all(|t| field_ok(iface, t, held)),
        // A use of a template inside another template's fields, before
        // expansion: the template's class and its arguments.
        Type::App(template, args) => {
            held.contains(template.as_str()) && args.iter().all(|t| field_ok(iface, t, held))
        }
        Type::Struct(n) | Type::Enum(n) => match iface.instance_of(n) {
            Some(i) => {
                held.contains(i.template.as_str())
                    && i.args.iter().all(|t| field_ok(iface, t, held))
            }
            None => held.contains(n.as_str()),
        },
        // Handles, and anything not resolved to one of the above, stay with
        // the bindings.
        Type::Named(_)
        | Type::Claimed(..)
        | Type::Opaque(_)
        | Type::DartObject(_)
        | Type::Ref { .. } => false,
    }
}

/// The classes the data library holds: every class name, less those
/// [`excluded`], then less those naming a class not held, until nothing
/// changes.
fn held(iface: &Interface) -> Vec<String> {
    let names = class_names(iface);
    let mut held: BTreeSet<&str> = names
        .iter()
        .copied()
        .filter(|n| !excluded(iface, n))
        .collect();
    loop {
        let dropped: Vec<&str> = held
            .iter()
            .copied()
            .filter(|n| {
                !field_types(iface, n)
                    .into_iter()
                    .all(|t| field_ok(iface, t, &held))
            })
            .collect();
        if dropped.is_empty() {
            break;
        }
        for n in dropped {
            held.remove(n);
        }
    }
    names
        .into_iter()
        .filter(|n| held.contains(n))
        .map(str::to_string)
        .collect()
}

/// The import URIs of the bridge-external types the held classes name.
fn extern_imports(iface: &Interface, types: &[String]) -> Vec<String> {
    fn collect(ty: &Type, out: &mut Vec<String>) {
        match ty {
            Type::Extern(n) => out.push(n.clone()),
            Type::Array(t, _)
            | Type::List(t, _)
            | Type::Set(t, _)
            | Type::Option(t)
            | Type::Boxed(t) => collect(t, out),
            Type::Map(k, v, _) => {
                collect(k, out);
                collect(v, out);
            }
            Type::Tuple(ts) | Type::App(_, ts) => ts.iter().for_each(|t| collect(t, out)),
            _ => {}
        }
    }
    let mut names = vec![];
    for t in types {
        for ty in field_types(iface, t) {
            collect(ty, &mut names);
        }
    }
    let mut uris: Vec<String> = iface
        .externs
        .iter()
        .filter(|e| names.contains(&e.name))
        .map(|e| e.dart_import.clone())
        .collect();
    uris.sort_unstable();
    uris.dedup();
    uris
}

/// The data library's source. Empty of classes when none qualifies; the
/// bindings import it all the same.
pub(crate) fn emit_data_library(iface: &Interface) -> String {
    let data = iface.data.as_ref().expect("data::apply ran");
    let mut out = format!(
        "// @generated by frustrate-codegen for crate `{}`. Do not edit.\n\
         //\n\
         // The bridge's plain data classes, which need neither the bridge's\n\
         // native library nor its bindings. The bindings re-export them.\n\
         // ignore_for_file: unused_import\n\n\
         import 'dart:typed_data';\n\n\
         import 'package:frustrate/frustrate.dart';\n",
        iface.crate_name
    );
    for uri in extern_imports(iface, &data.types) {
        let _ = write!(out, "import '{uri}';\n");
    }
    out.push('\n');
    for name in &data.types {
        if let Some(s) = iface
            .generic_struct(name)
            .or_else(|| iface.struct_decl(name))
        {
            emit_struct_class(&mut out, iface, s, false);
        } else if let Some(e) = iface.generic_enum(name).or_else(|| iface.enum_decl(name)) {
            emit_enum_class(&mut out, iface, e, false);
        }
    }
    out
}

/// The directives that bring the data library's classes into a surface and
/// re-export them, so the bindings' API is unchanged by having one.
pub(crate) fn emit_imports(out: &mut String, iface: &Interface) {
    if let Some(data) = &iface.data {
        let uri = &data.import;
        let _ = write!(out, "import '{uri}';\nexport '{uri}';\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::{merge, parse_source};

    const URI: &str = "package:a_data/a.frustrate.data.dart";

    fn iface(src: &str) -> Interface {
        let parts = vec![parse_source(src, "crate::api").unwrap()];
        let mut iface = crate::check::check(merge("a", parts)).unwrap();
        apply(&mut iface, URI);
        iface
    }

    const DATA: &str = "
        #[bridge(data)] pub struct Block { pub id: i64, pub text: String, pub tags: Vec<String> }
        #[bridge(data)] pub enum Shape { Circle { r: f64 }, Square(f64), Empty }
        #[bridge(data)] pub enum Mood { Calm, Loud }
        #[bridge(data)] pub struct Graph { pub blocks: Vec<Block>, pub shape: Shape, pub mood: Option<Mood> }
        #[bridge(sync)] pub fn grow(g: Graph) -> Graph { g }
    ";

    #[test]
    fn plain_classes_move_and_the_codecs_stay() {
        let a = iface(DATA);
        assert_eq!(
            a.data.as_ref().unwrap().types,
            ["Block", "Graph", "Shape", "Mood"]
        );
        let out = crate::emit_dart::emit(&a, "a.frustrate", "a_app");
        let data = out.data.expect("a data library");
        for decl in [
            "class Block {",
            "class Graph {",
            "sealed class Shape {",
            "final class ShapeCircle extends Shape {",
            "enum Mood { calm, loud }",
        ] {
            assert!(data.contains(decl), "{decl}:\n{data}");
        }
        assert!(!data.contains("_enc"), "{data}");
        assert!(!data.contains("dart:ffi"), "{data}");
        for surface in [&out.native, &out.web] {
            assert!(
                surface.contains(&format!("import '{URI}';\nexport '{URI}';\n")),
                "{surface}"
            );
            assert!(!surface.contains("class Block {"), "{surface}");
            assert!(!surface.contains("enum Mood {"), "{surface}");
            assert!(
                surface.contains("void _encBlock(BinaryWriter w, Block v) {"),
                "{surface}"
            );
            assert!(
                surface.contains("Graph _decGraph(BinaryReader r) {"),
                "{surface}"
            );
        }
    }

    #[test]
    fn without_a_data_library_the_surfaces_declare_the_classes() {
        let parts = vec![parse_source(DATA, "crate::api").unwrap()];
        let a = crate::check::check(merge("a", parts)).unwrap();
        let out = crate::emit_dart::emit(&a, "a.frustrate", "a_app");
        assert!(out.data.is_none());
        assert!(out.native.contains("class Block {"), "{}", out.native);
    }

    #[test]
    fn a_class_needing_the_bridge_stays_and_so_does_what_names_it() {
        let a = iface(
            "
            #[bridge(confined)] pub struct Doc;
            #[bridge] impl Doc { #[bridge(sync)] pub fn new() -> Self { todo!() } }
            #[bridge(data)] pub struct Holder { pub doc: Doc }
            #[bridge(data)] pub struct Measured { pub n: i64 }
            #[bridge] impl Measured { #[bridge(sync)] pub fn twice(&self) -> i64 { self.n * 2 } }
            #[bridge(data)] pub struct UsesMeasured { pub m: Vec<Measured> }
            #[bridge(data)] pub enum Either { Holds(Holder), Plain(i64) }
            #[bridge(data)] pub struct Free { pub n: i64 }
            #[bridge(sync)] pub fn hold() -> Holder { todo!() }
            #[bridge(sync)] pub fn uses(u: UsesMeasured, f: Free) -> Either { todo!() }
            ",
        );
        assert_eq!(a.data.as_ref().unwrap().types, ["Free"]);
        let out = crate::emit_dart::emit(&a, "a.frustrate", "a_app");
        assert!(out.native.contains("class Measured {"), "{}", out.native);
        assert!(out.native.contains("int twice()"), "{}", out.native);
        assert!(
            out.native.contains("class UsesMeasured {"),
            "{}",
            out.native
        );
        assert!(!out.native.contains("class Free {"), "{}", out.native);
    }

    #[test]
    fn a_generic_template_moves_and_its_instantiations_codecs_stay() {
        let a = iface(
            "
            #[bridge(data)] pub struct Page<T> { pub items: Vec<T>, pub next: Option<i64> }
            #[bridge(data)] pub struct Item { pub id: i64 }
            #[bridge(data)] pub struct Feed { pub page: Page<Item> }
            #[bridge(sync)] pub fn feed() -> Feed { todo!() }
            ",
        );
        assert_eq!(a.data.as_ref().unwrap().types, ["Page", "Item", "Feed"]);
        let out = crate::emit_dart::emit(&a, "a.frustrate", "a_app");
        let data = out.data.unwrap();
        assert!(data.contains("class Page<T> {"), "{data}");
        assert!(data.contains("  final Page<Item> page;"), "{data}");
        assert!(!out.native.contains("class Page<T> {"), "{}", out.native);
        assert!(
            out.native
                .contains("Page<Item> _dec16Page_3c_Item_3e_(BinaryReader r) {"),
            "{}",
            out.native
        );
    }

    /// A Rust body is not part of the data library's input: the library
    /// generated before and after an edit to one is the same text.
    #[test]
    fn an_edit_to_a_body_leaves_the_data_library_unchanged() {
        let before = iface(DATA);
        let after = iface(&DATA.replace("-> Graph { g }", "-> Graph { let h = g; h }"));
        let emit = |i: &Interface| crate::emit_dart::emit(i, "a.frustrate", "a_app").data;
        assert_eq!(emit(&before), emit(&after));
        assert!(emit(&before).is_some());
    }
}
