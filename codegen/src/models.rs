// `write_with_newline` is off for this file, for the reason `emit_dart.rs`
// gives: the format strings mirror the generated output line for line.
#![allow(clippy::write_with_newline)]

//! Data types shared between bridges: `models`.
//!
//! A bridge that names another in `models` takes that bridge's plain data
//! types instead of declaring its own Dart copies, so one Dart object crosses
//! both. Matching is by name: a struct or enum this interface declares under
//! the same name as one in a model bridge's interface is that model's type.
//! The importing bridge still parses the declaration from its own sources —
//! the interface JSON a model hands over is a wire description and lacks what
//! the Rust glue needs (tuple or named, a `Box`) — and the two must be one
//! Rust item with one wire shape, or codegen refuses.
//!
//! What changes is the Dart surface only. The importer emits no class and no
//! codec for a taken type: it imports the class from the model's surface,
//! re-exports it, and forwards its private codec names to the model's
//! [`MODELS_CLASS`]. The Rust glue and the schema fingerprint treat the type
//! like any of the bridge's own, so a change to its shape moves the
//! importer's fingerprint too.

use crate::check::Diagnostic;
use crate::emit_dart::{data_class, data_dart_type, decl_has_handle, decl_owns_handles};
use crate::ir::*;
use std::fmt::Write;

/// The class each surface declares its plain data codecs on, public so that
/// another bridge's bindings can reach them and hidden by the entry so that no
/// application does. `$` keeps it out of reach of any name derived from Rust.
pub(crate) const MODELS_CLASS: &str = "$FrustrateModels";

/// A model bridge as handed to codegen: its interface and the import URI of
/// its public bindings entry (`package:<pkg>/<crate>.frustrate.dart`).
#[derive(Debug, Clone)]
pub struct ModelInterface {
    pub iface: Interface,
    pub dart_import: String,
}

/// Why the data declaration `name` cannot be shared with another bridge, or
/// `None` when it can. Shared types are decoded by one bridge's codec and
/// handed to another's, so nothing in them may be bound to one bridge: a
/// handle is an index into the registry of the library that minted it.
pub(crate) fn not_plain(iface: &Interface, name: &str) -> Option<String> {
    if iface.instance_of(name).is_some() {
        return Some(
            "is an instantiation of a generic data type, whose codecs exist per \
             instantiation in each bridge"
                .into(),
        );
    }
    if iface.opaque(name).is_some() {
        return Some("is also declared as a handle".into());
    }
    if let Some(s) = iface.struct_decl(name) {
        if s.inbound {
            return Some("is `inbound`, and carries the handles a call consumes".into());
        }
        if s.dart_interface {
            return Some("is a `dart_interface`, whose fields are Dart-object handles".into());
        }
    }
    if decl_has_handle(iface, name) || decl_owns_handles(iface, name) {
        return Some("reaches a handle, which is valid only in the bridge that minted it".into());
    }
    None
}

/// The data declarations a surface offers on [`MODELS_CLASS`].
fn shareable(iface: &Interface) -> Vec<&str> {
    iface
        .structs
        .iter()
        .map(|s| s.name.as_str())
        .chain(iface.enums.iter().map(|e| e.name.as_str()))
        .filter(|n| not_plain(iface, n).is_none())
        .collect()
}

/// Whether the surfaces declare [`MODELS_CLASS`], which the entry must hide.
pub(crate) fn has_models_class(iface: &Interface) -> bool {
    !shareable(iface).is_empty()
}

/// A module path with a leading `crate` replaced by the crate it is relative
/// to, so that two bridges' spellings of one item compare equal.
fn canonical_path(crate_name: &str, module_path: &str) -> String {
    match module_path.split_once("::") {
        Some(("crate", rest)) => format!("{crate_name}::{rest}"),
        None if module_path == "crate" => crate_name.to_string(),
        _ => module_path.to_string(),
    }
}

/// The declaration's wire description — its serialized IR, the same facts the
/// schema fingerprint covers — minus the module path, which each bridge
/// spells relative to itself.
fn wire_shape(iface: &Interface, name: &str) -> Option<(String, serde_json::Value)> {
    let (path, value) = match (iface.struct_decl(name), iface.enum_decl(name)) {
        (Some(s), _) => (&s.module_path, serde_json::to_value(s)),
        (None, Some(e)) => (&e.module_path, serde_json::to_value(e)),
        (None, None) => return None,
    };
    let mut value = value.expect("IR serializes");
    // A struct and an enum of one name are not one shape.
    let kind = if iface.struct_decl(name).is_some() { "struct" } else { "enum" };
    let path = path.clone();
    if let Some(obj) = value.as_object_mut() {
        obj.insert("kind".into(), kind.into());
        obj.remove("module_path");
    }
    Some((path, value))
}

/// The declaration as Rust, for a diagnostic.
fn render(iface: &Interface, name: &str) -> String {
    let fields = |fs: &[Field]| {
        fs.iter()
            .map(|f| format!("{}: {}", f.name, f.ty.rust_display()))
            .collect::<Vec<_>>()
            .join(", ")
    };
    if let Some(s) = iface.struct_decl(name) {
        return format!("struct {name} {{ {} }}", fields(&s.fields));
    }
    if let Some(e) = iface.enum_decl(name) {
        let variants: Vec<String> = e
            .variants
            .iter()
            .map(|v| {
                if v.fields.is_empty() {
                    v.name.clone()
                } else {
                    format!("{} {{ {} }}", v.name, fields(&v.fields))
                }
            })
            .collect();
        return format!("enum {name} {{ {} }}", variants.join(", "));
    }
    name.to_string()
}

/// The URIs of the two surfaces behind a bindings entry: `src/<stem>.native.dart`
/// and `src/<stem>.web.dart` beside `<stem>.dart`, as `generate` lays them out.
fn surface_imports(entry: &str) -> (String, String) {
    let (dir, file) = entry.rsplit_once('/').unwrap_or(("", entry));
    let stem = file.strip_suffix(".dart").unwrap_or(file);
    let at = |variant: &str| {
        if dir.is_empty() {
            format!("src/{stem}.{variant}.dart")
        } else {
            format!("{dir}/src/{stem}.{variant}.dart")
        }
    };
    (at("native"), at("web"))
}

/// Take from `models` every data declaration of `iface` that one of them
/// declares, or refuse.
///
/// Called on a checked interface. Members a taken type carries are its model's
/// — its Dart class is the model's and dispatches there — so this bridge's
/// copies of them are dropped; a member only this bridge declares is refused,
/// because it has no class to go on.
pub fn apply(mut iface: Interface, models: &[ModelInterface]) -> Result<Interface, Vec<Diagnostic>> {
    let mut diags = vec![];
    let mut bridges: Vec<ModelBridge> = models
        .iter()
        .map(|m| {
            let (native_import, web_import) = surface_imports(&m.dart_import);
            ModelBridge {
                crate_name: m.iface.crate_name.clone(),
                native_import,
                web_import,
                types: vec![],
            }
        })
        .collect();
    let names: Vec<String> = iface
        .structs
        .iter()
        .map(|s| s.name.clone())
        .chain(iface.enums.iter().map(|e| e.name.clone()))
        .collect();
    for name in &names {
        let providers: Vec<usize> = (0..models.len())
            .filter(|&i| wire_shape(&models[i].iface, name).is_some())
            .collect();
        let [i] = providers[..] else {
            if providers.len() > 1 {
                let which: Vec<String> = providers
                    .iter()
                    .map(|&i| format!("`{}`", models[i].iface.crate_name))
                    .collect();
                diags.push(Diagnostic {
                    code: "FR0080",
                    message: format!(
                        "`{name}` is declared by more than one bridge in `models` ({}); \
                         a data type is taken from one bridge, so name only the one that \
                         declares it",
                        which.join(", ")
                    ),
                });
            }
            continue;
        };
        let model = &models[i];
        let a = &model.iface.crate_name;
        let (our_path, ours) = wire_shape(&iface, name).expect("declared here");
        let (their_path, theirs) = wire_shape(&model.iface, name).expect("declared there");
        let our_item = canonical_path(&iface.crate_name, &our_path);
        let their_item = canonical_path(a, &their_path);
        if our_item != their_item {
            diags.push(Diagnostic {
                code: "FR0080",
                message: format!(
                    "`{name}` is declared here at `{our_item}::{name}` and by the model \
                     bridge `{a}` at `{their_item}::{name}`. A type named like one in \
                     `models` is that type: list the source that declares it, with a \
                     module path that names the same item, or rename this one"
                ),
            });
            continue;
        }
        if ours != theirs {
            diags.push(Diagnostic {
                code: "FR0081",
                message: format!(
                    "`{name}` differs from the model bridge `{a}`'s declaration of it: this \
                     bridge reads `{}`, and `{a}`'s interface has `{}`. Both must be \
                     generated from the same source",
                    render(&iface, name),
                    render(&model.iface, name)
                ),
            });
            continue;
        }
        if let Some(reason) = not_plain(&iface, name) {
            diags.push(Diagnostic {
                code: "FR0082",
                message: format!(
                    "`{name}` is declared by the model bridge `{a}`, but only plain data \
                     crosses between bridges and `{name}` {reason}. Drop `{a}` from \
                     `models`, or give this bridge its own type under another name"
                ),
            });
            continue;
        }
        bridges[i].types.push(name.clone());
    }
    iface.models = bridges.into_iter().filter(|b| !b.types.is_empty()).collect();

    let mut keep = Vec::with_capacity(iface.functions.len());
    for f in std::mem::take(&mut iface.functions) {
        let Some(parent) = f.parent.as_deref() else {
            keep.push(f);
            continue;
        };
        let Some(model) = iface.model_of(parent) else {
            keep.push(f);
            continue;
        };
        let source = models
            .iter()
            .find(|m| m.iface.crate_name == model.crate_name)
            .expect("a model bridge applied above");
        let theirs = source
            .iface
            .functions
            .iter()
            .any(|g| g.parent.as_deref() == Some(parent) && g.name == f.name);
        if !theirs {
            diags.push(Diagnostic {
                code: "FR0083",
                message: format!(
                    "`{parent}::{}` is a member of `{parent}`, which this bridge takes from \
                     the model bridge `{}`; its Dart class is that bridge's and cannot \
                     carry a member of this one. Bridge it as a free function taking \
                     `{parent}`, or declare it in `{}`",
                    f.name, model.crate_name, model.crate_name
                ),
            });
        }
    }
    iface.functions = keep;

    if diags.is_empty() {
        Ok(iface)
    } else {
        Err(diags)
    }
}

/// The Dart names a taken declaration brings: its class, and for an enum with
/// fields, each variant's class.
fn dart_names(iface: &Interface, name: &str) -> Vec<String> {
    let mut out = vec![data_class(iface, name)];
    if let Some(e) = iface.enum_decl(name) {
        if e.variants.iter().any(|v| !v.fields.is_empty()) {
            out.extend(
                e.variants
                    .iter()
                    .map(|v| crate::emit_dart::variant_class(iface, e, v)),
            );
        }
    }
    out
}

fn import_prefix(model: &ModelBridge) -> String {
    format!("frustrate_model_{}", model.crate_name)
}

/// The directives that bring each model's classes and codecs into a surface,
/// and re-export the classes so this bridge's API is usable on its own.
pub(crate) fn emit_imports(out: &mut String, iface: &Interface, web: bool) {
    for m in &iface.models {
        let uri = if web { &m.web_import } else { &m.native_import };
        let shown: Vec<String> = m.types.iter().flat_map(|t| dart_names(iface, t)).collect();
        let shown = shown.join(", ");
        let _ = write!(
            out,
            "import '{uri}' show {shown};\n\
             import '{uri}' as {} show {MODELS_CLASS};\n\
             export '{uri}' show {shown};\n",
            import_prefix(m)
        );
    }
}

/// The codec names a taken declaration's every use site calls, forwarded to
/// the model's.
pub(crate) fn emit_forwarded_codecs(out: &mut String, iface: &Interface, name: &str) {
    let model = iface.model_of(name).expect("a taken declaration");
    let cls = data_dart_type(iface, name);
    let stem = iface.codec_stem(name);
    let prefix = import_prefix(model);
    let _ = write!(
        out,
        "void _enc{stem}(BinaryWriter w, {cls} v) => {prefix}.{MODELS_CLASS}.enc{stem}(w, v);\n\
         {cls} _dec{stem}(BinaryReader r) => {prefix}.{MODELS_CLASS}.dec{stem}(r);\n\n"
    );
}

/// [`MODELS_CLASS`]: this surface's plain data codecs, for the bindings of a
/// bridge that names this one in `models`.
pub(crate) fn emit_models_class(out: &mut String, iface: &Interface) {
    let names = shareable(iface);
    if names.is_empty() {
        return;
    }
    let _ = write!(
        out,
        "/// The codecs of this bridge's plain data types, for the bindings of a\n\
         /// bridge that takes them through `models`. The public entry hides it.\n\
         abstract final class {MODELS_CLASS} {{\n"
    );
    for name in names {
        let cls = data_dart_type(iface, name);
        let stem = iface.codec_stem(name);
        let _ = write!(
            out,
            "  static void enc{stem}(BinaryWriter w, {cls} v) => _enc{stem}(w, v);\n\
             \x20 static {cls} dec{stem}(BinaryReader r) => _dec{stem}(r);\n"
        );
    }
    out.push_str("}\n\n");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::{merge, parse_source};

    const SHARED: &str = "
        #[bridge(data)] pub struct Block { pub id: i64, pub text: String }
        #[bridge(data)] pub enum Shape { Circle { r: f64 }, Square(f64), Empty }
        #[bridge(data)] pub enum Mood { Calm, Loud }
        #[bridge(data)] pub struct Graph { pub blocks: Vec<Block>, pub shape: Shape, pub mood: Mood }
    ";

    /// A checked interface for `crate_name` over `(source, module path)` pairs.
    fn iface(crate_name: &str, parts: &[(&str, &str)]) -> Interface {
        let parts = parts
            .iter()
            .map(|(src, module)| parse_source(src, module).unwrap())
            .collect();
        crate::check::check(merge(crate_name, parts)).unwrap()
    }

    fn bridge_a(shared: &str) -> ModelInterface {
        ModelInterface {
            iface: iface(
                "a",
                &[(shared, "shared::model"), ("#[bridge(sync)] pub fn make() -> Block { todo!() }", "crate::api")],
            ),
            dart_import: "package:a_app/a.frustrate.dart".into(),
        }
    }

    const B_API: &str = "
        #[bridge(sync)] pub fn echo(b: Block) -> Block { b }
        #[bridge(sync)] pub fn grow(g: Graph) -> Graph { g }
    ";

    fn apply_b(shared: &str, api: &str, models: &[ModelInterface]) -> Result<Interface, Vec<Diagnostic>> {
        apply(iface("b", &[(shared, "shared::model"), (api, "crate::api")]), models)
    }

    fn codes(r: Result<Interface, Vec<Diagnostic>>) -> Vec<&'static str> {
        r.expect_err("refused").iter().map(|d| d.code).collect()
    }

    #[test]
    fn taken_types_are_imported_and_not_declared() {
        let b = apply_b(SHARED, B_API, &[bridge_a(SHARED)]).unwrap();
        let out = crate::emit_dart::emit(&b, "b.frustrate", "b_app");
        for (surface, variant) in [(&out.native, "native"), (&out.web, "web")] {
            let uri = format!("package:a_app/src/a.frustrate.{variant}.dart");
            let shown = "Block, Graph, Shape, ShapeCircle, ShapeSquare, ShapeEmpty, Mood";
            assert!(surface.contains(&format!("import '{uri}' show {shown};")), "{surface}");
            assert!(surface.contains(&format!("export '{uri}' show {shown};")), "{surface}");
            assert!(
                surface.contains(&format!("import '{uri}' as frustrate_model_a show $FrustrateModels;")),
                "{surface}"
            );
            for decl in ["class Block ", "class Shape ", "enum Mood ", "class Graph ", "class ShapeCircle "] {
                assert!(!surface.contains(decl), "{decl} declared again:\n{surface}");
            }
            assert!(
                surface.contains(
                    "void _encBlock(BinaryWriter w, Block v) => \
                     frustrate_model_a.$FrustrateModels.encBlock(w, v);"
                ),
                "{surface}"
            );
            assert!(
                surface.contains("Mood _decMood(BinaryReader r) => frustrate_model_a.$FrustrateModels.decMood(r);"),
                "{surface}"
            );
            // The bridge's own members still cross through the codec names,
            // and so does its fake, which answers through the same ones.
            assert!(surface.contains("_encBlock(w, b);"), "{surface}");
            assert!(surface.contains("_decGraph(r)"), "{surface}");
        }
    }

    #[test]
    fn a_bridge_offers_its_plain_codecs_and_its_entry_hides_them() {
        let a = bridge_a(SHARED).iface;
        let out = crate::emit_dart::emit(&a, "a.frustrate", "a_app");
        assert!(out.entry.contains("'src/a.frustrate.web.dart'\n    hide $FrustrateModels;"), "{}", out.entry);
        for surface in [&out.native, &out.web] {
            assert!(surface.contains("abstract final class $FrustrateModels {"), "{surface}");
            assert!(
                surface.contains("  static Block decBlock(BinaryReader r) => _decBlock(r);"),
                "{surface}"
            );
            assert!(
                surface.contains("  static void encShape(BinaryWriter w, Shape v) => _encShape(w, v);"),
                "{surface}"
            );
        }
    }

    #[test]
    fn a_bridge_with_no_plain_data_hides_nothing() {
        let a = iface("a", &[("#[bridge(sync)] pub fn f() -> i64 { 0 }", "crate::api")]);
        let out = crate::emit_dart::emit(&a, "a.frustrate", "a_app");
        assert!(!out.entry.contains("hide"), "{}", out.entry);
        assert!(!out.native.contains("$FrustrateModels"), "{}", out.native);
    }

    #[test]
    fn a_crate_relative_path_names_the_same_item_from_the_other_crate() {
        let a = ModelInterface {
            iface: iface("a", &[(SHARED, "crate::model")]),
            dart_import: "package:a_app/a.frustrate.dart".into(),
        };
        let b = apply(iface("b", &[(SHARED, "a::model"), (B_API, "crate::api")]), &[a]).unwrap();
        assert_eq!(b.models[0].types, ["Block", "Graph", "Shape", "Mood"]);
    }

    #[test]
    fn a_local_type_named_like_a_model_type_is_refused() {
        let local = "#[bridge(data)] pub struct Block { pub id: i64, pub text: String }
                     #[bridge(sync)] pub fn echo(b: Block) -> Block { b }";
        let r = apply(iface("b", &[(local, "crate::api")]), &[bridge_a(SHARED)]);
        let diags = r.expect_err("refused");
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert_eq!(diags[0].code, "FR0080");
        assert!(diags[0].message.contains("`b::api::Block`"), "{}", diags[0].message);
        assert!(diags[0].message.contains("`shared::model::Block`"), "{}", diags[0].message);
    }

    #[test]
    fn a_type_two_models_declare_is_refused() {
        let mut other = bridge_a(SHARED);
        other.iface.crate_name = "c".into();
        assert!(codes(apply_b(SHARED, B_API, &[bridge_a(SHARED), other])).contains(&"FR0080"));
    }

    #[test]
    fn a_shape_that_differs_from_the_model_is_refused() {
        let grown = SHARED.replace("pub text: String }", "pub text: String, pub done: bool }");
        let r = apply_b(&grown, B_API, &[bridge_a(SHARED)]);
        let diags = r.expect_err("refused");
        assert_eq!(diags.iter().map(|d| d.code).collect::<Vec<_>>(), ["FR0081"]);
        assert!(diags[0].message.contains("done: bool"), "{}", diags[0].message);
        // A struct where the model has an enum is a different shape too.
        let swapped = "#[bridge(data)] pub enum Block { A } #[bridge(sync)] pub fn f(b: Block) {}";
        assert_eq!(codes(apply(iface("b", &[(swapped, "shared::model")]), &[bridge_a(SHARED)])), ["FR0081"]);
    }

    #[test]
    fn a_type_that_reaches_a_handle_is_refused() {
        let shared = "
            #[bridge(confined)] pub struct Doc;
            #[bridge] impl Doc { #[bridge(sync)] pub fn new() -> Self { todo!() } }
            #[bridge(data)] pub struct Holder { pub doc: Doc }
            #[bridge(sync)] pub fn hold() -> Holder { todo!() }
        ";
        let a = ModelInterface {
            iface: iface("a", &[(shared, "shared::model")]),
            dart_import: "package:a_app/a.frustrate.dart".into(),
        };
        let r = apply(iface("b", &[(shared, "shared::model")]), &[a]);
        let diags = r.expect_err("refused");
        assert_eq!(diags.iter().map(|d| d.code).collect::<Vec<_>>(), ["FR0082"]);
        assert!(diags[0].message.contains("reaches a handle"), "{}", diags[0].message);
    }

    #[test]
    fn a_models_own_members_are_left_to_it_and_new_ones_are_refused() {
        let shared = format!(
            "{SHARED} #[bridge] impl Block {{ #[bridge(sync)] pub fn len(&self) -> i64 {{ 0 }} }}"
        );
        let b = apply_b(&shared, B_API, &[bridge_a(&shared)]).unwrap();
        assert!(b.functions.iter().all(|f| f.parent.is_none()), "{:?}", b.functions);
        let added = format!(
            "{B_API} #[bridge] impl Block {{ #[bridge(sync)] pub fn width(&self) -> i64 {{ 0 }} }}"
        );
        assert_eq!(codes(apply_b(SHARED, &added, &[bridge_a(SHARED)])), ["FR0083"]);
    }

    #[test]
    fn the_fingerprint_covers_the_shape_of_a_taken_type() {
        let grown = SHARED.replace("pub text: String }", "pub text: String, pub done: bool }");
        let before = apply_b(SHARED, B_API, &[bridge_a(SHARED)]).unwrap();
        let after = apply_b(&grown, B_API, &[bridge_a(&grown)]).unwrap();
        assert_ne!(crate::hash::schema_hash(&before), crate::hash::schema_hash(&after));
    }

    #[test]
    fn the_model_interface_round_trips_through_its_json() {
        let a = bridge_a(SHARED).iface;
        let read: Interface = serde_json::from_str(&serde_json::to_string(&a).unwrap()).unwrap();
        let model = ModelInterface { iface: read, dart_import: "package:a_app/a.frustrate.dart".into() };
        assert!(apply_b(SHARED, B_API, &[model]).is_ok());
    }
}
