//! Source extraction: syn over explicitly declared bridge files.
//!
//! Only items carrying `#[bridge]` / `#[frustrate::bridge]` participate.
//! The attribute is inert at compile time (see frustrate-macros); here it is
//! read straight from the source text, so nothing is ever macro-expanded.

use crate::check::Warning;
use crate::ir::*;
use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use syn::punctuated::Punctuated;

/// Options parsed from a `#[bridge(...)]` attribute.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct BridgeAttr {
    pub sync: bool,
    /// `#[bridge(data)]`: this struct/enum crosses by value. One of the six
    /// representation keywords a type declaration must name (the others are
    /// `model`, below) — `bytes(...)` is the exception, a representation of
    /// its own that stands alone and needs neither (see the struct arm in
    /// `parse_source_reporting`). Mutually exclusive with `model`, enforced
    /// where both are set (see `parse_bridge_option`'s
    /// `data`/`confined`/`resident`/`frozen`/`locked`/`actor` arms).
    pub data: bool,
    /// `#[bridge(confined | resident | frozen | locked | actor)]`: this struct/trait
    /// crosses as a handle under the named concurrency model.
    pub model: Option<Model>,
    pub on_contention: Option<OnContention>,
    pub bytes: Option<BytesAttr>,
    /// `#[bridge(no_eq)]`: opt out of generated value equality on a data
    /// struct/enum. Only meaningful on data types; rejected loudly elsewhere.
    pub no_eq: bool,
    /// `#[bridge(getter)]`: emit this member as a Dart property rather than a
    /// method. See [`crate::ir::Function::getter`].
    pub getter: bool,
    /// `#[bridge(dart_identifier = "…")]`: the Dart name this item lands
    /// under. See [`crate::ir::StructDecl::dart_identifier`] — the derivation
    /// from a Rust name is not injective, so this is how an author says which
    /// of two colliding items moves, and to what.
    pub dart_identifier: Option<String>,
    /// `#[bridge(data(dart_identifier = "…"))]`: the Dart class name for the
    /// **value** half of a declaration specifically.
    ///
    /// Only [`Self::dart_identifier`] nests, and only on a type declaration,
    /// because a declaration that names two representations mints two Dart
    /// classes and every other option has one reader for both halves (see the
    /// struct arm). Accepted on a single-representation declaration too — one
    /// grammar, one meaning — where it is the flat form written longhand.
    pub data_dart_identifier: Option<String>,
    /// `#[bridge(locked(dart_identifier = "…"))]` and the other three models:
    /// the Dart class name for the **handle** half. See
    /// [`Self::data_dart_identifier`].
    pub model_dart_identifier: Option<String>,
    /// `#[bridge(dart_interface)]`: this struct's Dart surface is an
    /// `abstract interface class` to implement, one method per field, rather
    /// than a class of `required` named fields. Only meaningful on a data
    /// struct whose
    /// fields are all closure mirrors; rejected loudly everywhere else here,
    /// and shape-checked by FR0050–FR0052.
    pub dart_interface: bool,
    /// `#[bridge(inbound)]`: this data struct crosses Dart → Rust only, and its
    /// handle fields are therefore `Consumed<T>` tokens on the Dart side. See
    /// [`crate::ir::StructDecl::inbound`] for what the direction buys and why
    /// it is declared rather than inferred.
    pub inbound: bool,
    /// `#[bridge(skip)]`: this item is deliberately NOT bridged. Never reaches
    /// a call site as a `Some(attr)` — [`bridge_attr`] returns `None` for it,
    /// because "skip" and "no attribute at all" must produce the same
    /// interface. What it changes is everything that treats a *missing*
    /// annotation as a question: the FR0042 omission warning stops asking, and
    /// a method inside an annotated `impl`/`trait` block stops inheriting the
    /// block's options (see `parse_impl`).
    pub skip: bool,
    /// `#[bridge(web = "runtime_fail")]`: opt a genuinely native-only member
    /// INTO the web Dart surface as a runtime-throwing stub (an
    /// `UnsupportedError`), instead of the default compile-time absence. The
    /// checker rejects it (FR0030) on any member that is not native-only.
    pub web_runtime_fail: bool,
    /// `#[bridge(native_only)]`: declare that this member cannot exist on web,
    /// because its implementation (or a crate it reaches) does not build for
    /// wasm32. An explicit input to the checker's `requires_native`.
    ///
    /// Unlike every other inherited option, an impl/trait-level `native_only`
    /// is OR-merged into its methods rather than replaced by a method-level
    /// `#[bridge(...)]` — see `parse_impl`.
    pub native_only: bool,
    /// `#[bridge(no_block)]`: the author's claim that main-thread Dart calling
    /// this member cannot be stalled — nothing the *caller* runs, this body and
    /// everything it transitively reaches included, arrives at a wait
    /// instruction. A claim about the body together with where the body runs,
    /// which is what lets placement settle it (`emit_rust::claims`).
    ///
    /// Inherited like [`Self::native_only`] (OR-merged, never replaced): a
    /// method cannot be *more* blocking than the block that claims it. Also
    /// settable for a whole file with `frustrate::bridge_file!(no_block);`.
    pub no_block: bool,
    /// Not an option the author writes inside `#[bridge(...)]`: set by
    /// [`bridge_attr`] when the item *also* carries a `#[cfg(...)]` or
    /// `#[cfg_attr(...)]`. Codegen never evaluates cfg predicates, so this is
    /// only ever a hazard to report (FR0034).
    pub cfg_gated: bool,
}

/// `#[bridge(bytes(dart = "...", import = "...", ...))]`: a bridge-external
/// type. `encode`/`decode` default to the
/// protobuf conventions (`writeToBuffer` / `<dart>.fromBuffer`).
#[derive(Debug, Clone, PartialEq)]
pub struct BytesAttr {
    pub dart: String,
    pub import: String,
    pub encode: Option<String>,
    pub decode: Option<String>,
}

/// Parse one bridge source file. `module_path` is the path through which the
/// file's items are reachable from the crate root, e.g. "crate::api".
///
/// Drops the omission warnings; see [`parse_source_reporting`] for the entry
/// that keeps them. Everything downstream of the interface (checking, hashing,
/// emission) is unaffected by them, so those callers keep the simpler shape.
pub fn parse_source(source: &str, module_path: &str) -> Result<Interface> {
    Ok(parse_source_reporting(source, module_path)?.0)
}

/// [`parse_source`], plus the items it *skipped* that look like they were
/// meant to be bridged (FR0042).
///
/// Two returns rather than one, because they answer different questions and
/// travel to different places: the interface is the artifact, the warnings are
/// a message to the author. A warning here is never fatal — it cannot be, since
/// it is a heuristic about intent — so it must not be able to fail a build by
/// accident, and giving it its own channel is what guarantees that.
pub fn parse_source_reporting(
    source: &str,
    module_path: &str,
) -> Result<(Interface, Vec<crate::check::Warning>)> {
    let file: syn::File = syn::parse_str(source).map_err(|e| {
        // proc-macro2's `span-locations` feature makes the failure's location
        // available: line is 1-based, column 0-based (report it 1-based).
        let start = e.span().start();
        anyhow::anyhow!(
            "failed to parse Rust source at line {}, column {}: {e}",
            start.line,
            start.column + 1,
        )
    })?;
    let mut iface = Interface {
        crate_name: String::new(),
        structs: vec![],
        enums: vec![],
        generic_structs: vec![],
        generic_enums: vec![],
        opaques: vec![],
        externs: vec![],
        functions: vec![],
    };
    // A file-level claim is read before any item, and merged into each as it is
    // parsed. Claims only ever strengthen (see `parse_impl`), so this is an
    // OR-seed rather than a default that per-item annotations replace.
    let file_attr = parse_bridge_file(&file)?;
    for item in &file.items {
        match item {
            syn::Item::Fn(f) => {
                if let Some(mut attr) = bridge_attr(&f.attrs)? {
                    attr.no_block |= file_attr.no_block;
                    reject_no_eq(&attr, "function", &f.sig.ident.to_string())?;
                    reject_dart_interface(attr.dart_interface, "function", &f.sig.ident.to_string())?;
                    reject_inbound(
                        attr.inbound,
                        "function",
                        &f.sig.ident.to_string(),
                        "A function already says which way each of its own positions travels: a \
                         parameter goes in, the return comes back",
                    )?;
                    reject_representation(
                        attr.data,
                        attr.model,
                        "function",
                        &f.sig.ident.to_string(),
                    )?;
                    iface.functions.push(parse_fn(
                        &f.sig,
                        &attr,
                        module_path,
                        None,
                        None,
                        doc_lines(&f.attrs),
                    )?);
                }
            }
            syn::Item::Struct(s) => {
                if let Some(attr) = bridge_attr(&s.attrs)? {
                    // Before the model split: no struct of any model has a body
                    // to claim, including an opaque one — its methods live in
                    // an `impl` block, which may carry the claim itself.
                    reject_no_block(attr.no_block, "struct", &s.ident.to_string())?;
                    if let Some(bytes) = attr.bytes {
                        if attr.no_eq {
                            bail!(
                                "struct `{}`: no_eq is only valid on a data struct, not a \
                                 bytes(...) external type (it crosses by value with no \
                                 generated equality)",
                                s.ident
                            );
                        }
                        reject_dart_interface(attr.dart_interface, "struct", &s.ident.to_string())?;
                        reject_inbound(
                            attr.inbound,
                            "struct",
                            &s.ident.to_string(),
                            "A `bytes(...)` external crosses as one payload written by codecs the \
                             app supplies on both sides; it has no generated class with fields for \
                             a handle to sit in",
                        )?;
                        if attr.model.is_some() || attr.data {
                            bail!(
                                "struct `{}`: bytes(...) is mutually exclusive with every \
                                 representation keyword (data, confined, resident, frozen, \
                                 locked, actor) — an external type crosses by value as bytes; \
                                 a representation names how a *bridge-native* type crosses",
                                s.ident
                            );
                        }
                        reject_native_only(
                            attr.native_only,
                            "struct",
                            &s.ident.to_string(),
                            "a bytes(...) external type crosses by value through codecs \
                             the app supplies on both sides",
                        )?;
                        reject_cfg_gated_data_type(
                            attr.cfg_gated,
                            "struct",
                            &s.ident.to_string(),
                        )?;
                        reject_generic_representation(
                            &s.generics,
                            "struct",
                            &s.ident.to_string(),
                            "a `bytes(...)` external type crosses through codecs the app \
                             supplies on both sides, and its Dart peer is one named class \
                             from one import — there is nowhere for a type argument to go",
                        )?;
                        let dart_decode = bytes
                            .decode
                            .unwrap_or_else(|| format!("{}.fromBuffer", bytes.dart));
                        iface.externs.push(ExternDecl {
                            name: s.ident.to_string(),
                            module_path: module_path.to_string(),
                            dart_type: bytes.dart,
                            dart_import: bytes.import,
                            dart_encode: bytes.encode.unwrap_or_else(|| "writeToBuffer".into()),
                            dart_decode,
                        });
                    } else {
                        let name = s.ident.to_string();
                        require_representation(attr.data, attr.model, "struct", &name)?;
                        // Both keywords: the type crosses as a value class
                        // *and* as a handle class, from one Rust struct. Both
                        // declarations are pushed; every rule below that
                        // differs between them is stated per half.
                        let (data_id, model_id) =
                            half_dart_identifiers(&attr, "struct", &name)?;
                        if let Some(model) = attr.model {
                            if attr.no_eq && !attr.data {
                                bail!(
                                    "struct `{name}`: no_eq is only valid on a data struct, not a \
                                     handle type (a handle already has identity equality)"
                                );
                            }
                            // A `dart_interface` Dart side is an interface the
                            // *caller* implements and Rust only reads through;
                            // a handle is Rust's own object handed out. One
                            // declaration cannot be both directions, and on a
                            // dual type `is_dart_interface` is keyed on the
                            // name, so FR0050 would refuse every handle-half
                            // member as well.
                            reject_dart_interface(attr.dart_interface, "struct", &name)?;
                            if !attr.data {
                                reject_inbound(
                                    attr.inbound,
                                    "struct",
                                    &name,
                                    "This declaration names a concurrency model and not `data`, so it \
                                     crosses as a handle. A handle already travels both ways — out as \
                                     a mint, in by reference or as a `take()`",
                                )?;
                            }
                            reject_generic_representation(
                                &s.generics,
                                "struct",
                                &name,
                                "a handle type is a registry entry, and its drop and \
                                 finalize exports name the Rust type absolutely — one \
                                 export and one Dart class for a type that would need one \
                                 of each per instantiation",
                            )?;
                            if attr.data {
                                // `native_only` and the `#[cfg]` fact are read
                                // against a type's *name*, not against a half,
                                // so a declaration with two halves has no way
                                // to say which is absent — and the value half
                                // is present on every target by construction.
                                reject_native_only(
                                    attr.native_only,
                                    "struct",
                                    &name,
                                    "this declaration also names `data`, so the type has a value \
                                     half whose fields are wire types that exist on every target",
                                )?;
                                reject_cfg_gated_data_type(attr.cfg_gated, "struct", &name)?;
                            }
                            iface.opaques.push(OpaqueDecl {
                                name: name.clone(),
                                module_path: module_path.to_string(),
                                model,
                                dyn_trait: false,
                                supertraits: vec![],
                                native_only: attr.native_only,
                                cfg_gated: attr.cfg_gated,
                                dart_identifier: model_id,
                                docs: doc_lines(&s.attrs),
                            });
                        }
                        if attr.data {
                            if attr.model.is_none() {
                                reject_native_only(
                                    attr.native_only,
                                    "struct",
                                    &name,
                                    "a data struct crosses by value, and its fields are wire types \
                                     that exist on every target",
                                )?;
                                reject_cfg_gated_data_type(attr.cfg_gated, "struct", &name)?;
                            }
                            let decl = parse_struct(
                                s,
                                module_path,
                                attr.no_eq,
                                attr.dart_interface,
                                attr.inbound,
                                data_id,
                            )?;
                            // A declaration with type parameters is a
                            // **template**, not a type: it has no fields the
                            // wire can describe until they are bound. `check`
                            // expands every fully-applied use of it into a
                            // synthetic non-generic declaration; those are what
                            // `structs` holds and what every rule sees.
                            if decl.generics.is_empty() && decl.const_generics.is_empty() {
                                iface.structs.push(decl);
                            } else {
                                iface.generic_structs.push(decl);
                            }
                        }
                    }
                }
            }
            syn::Item::Enum(e) => {
                if let Some(attr) = bridge_attr(&e.attrs)? {
                    if attr.model.is_some() {
                        bail!(
                            "enum `{}`: a concurrency model (confined/resident/frozen/locked/actor) \
                             is only supported on a struct or a trait — an enum is always \
                             `data`",
                            e.ident
                        );
                    }
                    if attr.bytes.is_some() {
                        bail!(
                            "enum `{}`: bytes(...) is only supported on structs (wrap \
                             the enum in a newtype)",
                            e.ident
                        );
                    }
                    // Unconditional, like the struct arm's: `no_block` claims
                    // something about a body, which a declaration never has
                    // regardless of representation — so it is judged before
                    // `require_representation`, not after.
                    reject_no_block(attr.no_block, "enum", &e.ident.to_string())?;
                    require_representation(attr.data, attr.model, "enum", &e.ident.to_string())?;
                    reject_dart_interface(attr.dart_interface, "enum", &e.ident.to_string())?;
                    reject_inbound(
                        attr.inbound,
                        "enum",
                        &e.ident.to_string(),
                        "Not decided for an enum: its Dart side is a sealed hierarchy with one class \
                         per variant, and which of those a token would sit in is a question this \
                         option has not been given an answer for. Put the handle fields in an \
                         inbound *struct* and give the enum a variant carrying it",
                    )?;
                    reject_native_only(
                        attr.native_only,
                        "enum",
                        &e.ident.to_string(),
                        "a data enum crosses by value, and its variants are wire types \
                         that exist on every target",
                    )?;
                    reject_cfg_gated_data_type(attr.cfg_gated, "enum", &e.ident.to_string())?;
                    let (data_id, _) =
                        half_dart_identifiers(&attr, "enum", &e.ident.to_string())?;
                    // See the struct arm: a parameterized declaration is a
                    // template, and `check` expands its uses.
                    let decl = parse_enum(e, module_path, attr.no_eq, data_id)?;
                    if decl.generics.is_empty() && decl.const_generics.is_empty() {
                        iface.enums.push(decl);
                    } else {
                        iface.generic_enums.push(decl);
                    }
                }
            }
            syn::Item::Impl(im) => {
                if let Some(mut impl_attr) = bridge_attr(&im.attrs)? {
                    // Seeded here rather than inside `parse_impl`, so the
                    // existing block->method OR-merge carries it the rest of
                    // the way and there is only one inheritance rule.
                    impl_attr.no_block |= file_attr.no_block;
                    reject_no_eq(&impl_attr, "impl block", "")?;
                    reject_dart_interface(impl_attr.dart_interface, "impl block", "")?;
                    reject_inbound(
                        impl_attr.inbound,
                        "impl block",
                        "",
                        "An `impl` block declares members, not a type; the declaration it hangs off \
                         is where a direction is said",
                    )?;
                    reject_representation(impl_attr.data, impl_attr.model, "impl block", "")?;
                    parse_impl(im, &impl_attr, module_path, &mut iface)?;
                }
            }
            syn::Item::Trait(t) => {
                if let Some(mut attr) = bridge_attr(&t.attrs)? {
                    attr.no_block |= file_attr.no_block;
                    reject_no_eq(&attr, "trait", &t.ident.to_string())?;
                    reject_dart_interface(attr.dart_interface, "trait", &t.ident.to_string())?;
                    reject_inbound(
                        attr.inbound,
                        "trait",
                        &t.ident.to_string(),
                        "A bridged trait crosses as a handle behind `dyn Trait`, not as a value class \
                         with fields",
                    )?;
                    require_representation(attr.data, attr.model, "trait", &t.ident.to_string())?;
                    reject_data_on_trait(attr.data, &t.ident.to_string())?;
                    parse_trait(t, &attr, module_path, &mut iface)?;
                }
            }
            _ => {}
        }
    }
    // A claim that binds nothing is refused, not ignored. The whole value of
    // `bridge_file!` is that a reader can take one line at the top as covering
    // the file; a file with no bridged member covers nothing, which is either
    // a stale claim left behind by a move or one that landed in the wrong file.
    if file_attr != BridgeAttr::default() && iface.functions.is_empty() {
        bail!(
            "`bridge_file!` in a file with no bridged function or method. The \
             claim would bind nothing — move it to the file whose members it \
             is about, or delete it"
        );
    }
    // Only after the file has parsed cleanly: an omission warning about a file
    // that does not even parse would be noise on top of a real error.
    let mut warnings = omission_warnings(&file, &iface, module_path);
    warnings.extend(shadowed_time_import_warnings(&file));
    Ok((iface, warnings))
}

/// FR0047 — this file rebinds a time name whose bare spelling the parser
/// resolves to a different crate's type.
///
/// `parse_path_type` resolves a time name by path *root*, and a bare name has
/// no root — so it keeps whichever meaning it has always had: `Duration` and
/// `SystemTime` are std's (what every bridge source in this repo relies on),
/// `TimeDelta` is chrono's, `OffsetDateTime` is `time`'s. Rust name resolution
/// disagrees when the file says `use chrono::Duration;`: there, the bare
/// `Duration` in a signature is chrono's, and the glue emits
/// `std::time::Duration::from_micros(…)` into a fn that wants a `TimeDelta`.
/// The build still fails — as a type error inside generated code, naming
/// neither the bridge nor the fix.
///
/// A warning rather than a rejection, and for the same reason FR0042 is one:
/// the fact is provable (this file rebinds the name) but its *relevance* is
/// not — the import may serve helper code that never touches a bridged
/// signature, and refusing that file would be a rule the author cannot satisfy
/// without contorting code the bridge does not even look at. Its own code
/// rather than FR0045's for the same reason: FR0045 rejects, this reports, and
/// no diagnostic in this repo renders as both.
///
/// The question is always about the name a `use` **binds**, never the last
/// segment of its path: `use chrono::Duration;` and
/// `use chrono::TimeDelta as Duration;` are the same hazard, and
/// `use chrono::Duration as Delta;` is none at all. A glob
/// (`use chrono::prelude::*;`) is not resolvable from one file, and a
/// file-level `type Duration = chrono::TimeDelta;` is the same rebinding by
/// another syntax; both are the acknowledged residue.
fn shadowed_time_import_warnings(file: &syn::File) -> Vec<Warning> {
    let mut out = vec![];
    for item in &file.items {
        let syn::Item::Use(u) = item else { continue };
        collect_shadowed_time_imports(&u.tree, &mut vec![], &mut out);
    }
    out
}

fn collect_shadowed_time_imports(
    tree: &syn::UseTree,
    prefix: &mut Vec<String>,
    out: &mut Vec<Warning>,
) {
    match tree {
        syn::UseTree::Path(p) => {
            prefix.push(p.ident.to_string());
            collect_shadowed_time_imports(&p.tree, prefix, out);
            prefix.pop();
        }
        syn::UseTree::Group(g) => {
            for t in &g.items {
                collect_shadowed_time_imports(t, prefix, out);
            }
        }
        // `use chrono::TimeDelta as Duration;` binds a watched name just as
        // firmly as `use chrono::Duration;` does — the *bound* name is what a
        // signature writes, so that is what this scan asks about, never the
        // path's last segment. (Missing this was the review's catch: the
        // original arm skipped every rename on the theory that a rename leaves
        // the bare spelling alone, which is false exactly when the new name is
        // one of these.)
        syn::UseTree::Rename(r) => {
            let bound = r.rename.to_string();
            let source = prefix
                .iter()
                .map(String::as_str)
                .chain(std::iter::once(r.ident.to_string().as_str()))
                .collect::<Vec<_>>()
                .join("::");
            report_rebound_time_name(&bound, &source, r.rename.span().start().line, out);
        }
        syn::UseTree::Name(n) => {
            let name = n.ident.to_string();
            let full = prefix
                .iter()
                .map(String::as_str)
                .chain(std::iter::once(name.as_str()))
                .collect::<Vec<_>>()
                .join("::");
            report_rebound_time_name(&name, &full, n.ident.span().start().line, out);
        }
        // A glob names nothing this scan can resolve, and is the acknowledged
        // residue — along with a file-level `type Duration = …;`, which is the
        // same rebinding by another syntax.
        syn::UseTree::Glob(_) => {}
    }
}

/// One report for both `use x::Duration;` and `use x::TimeDelta as Duration;`
/// — the question is only ever "what name did this bind, and does the parser
/// read that name as something else?".
fn report_rebound_time_name(bound: &str, source: &str, line: usize, out: &mut Vec<Warning>) {
    // Every time name whose *bare* spelling the parser already assigns to one
    // crate: the paths that agree with it, and the path it resolves to (which
    // the message has to quote). Anything else is the divergence.
    let (owners, bare_means): (&[&str], &str) = match bound {
        "Duration" | "SystemTime" => (
            &["std::time::Duration", "std::time::SystemTime", "core::time::Duration"],
            if bound == "Duration" {
                "std::time::Duration"
            } else {
                "std::time::SystemTime"
            },
        ),
        "TimeDelta" => (&["chrono::TimeDelta"], "chrono::TimeDelta"),
        "OffsetDateTime" => (&["time::OffsetDateTime"], "time::OffsetDateTime"),
        _ => return,
    };
    if owners.contains(&source) {
        return;
    }
    out.push(Warning {
        code: "FR0047",
        file: String::new(),
        line,
        message: format!(
            "`use {source}{};` binds `{bound}`, but a bare `{bound}` in a bridged \
             signature is read as `{bare_means}` — codegen resolves the mapping from \
             the path it can see, and a bare name has no path. If a bridged signature \
             here means `{source}`, write it out: the generated glue would otherwise \
             reconstruct a `{bare_means}` and fail to compile against your own \
             function, with the error inside generated code. Qualify the signature, or \
             bind a name that is not a time mapping, to silence this.",
            if source.ends_with(bound) {
                String::new()
            } else {
                format!(" as {bound}")
            }
        ),
    });
}

/// FR0042 — the items this file skipped that look like a forgotten `#[bridge]`.
///
/// # Why this exists
///
/// Skipping an unannotated item is correct and stays correct: a bridge source
/// legitimately holds helpers, private types, `use` statements and foreign
/// trait impls, and the opt-in default is what makes an un-bridgeable type drop
/// out for free. But a *silent* skip means a forgotten attribute produces a
/// clean build, no output, and a name simply missing from the Dart surface —
/// with the symptom arriving much later, in another language, pointing
/// nowhere. That is the one outcome the charter forbids ("everything not
/// supported is rejected loudly … with a named diagnostic"). It is also the
/// first thing code ported from flutter_rust_bridge meets: there every `pub`
/// item is bridged by default, so such code starts out with no `#[bridge]`
/// attributes at all.
///
/// # Why a warning, and why this narrow
///
/// An error is not available: "the author meant to bridge this" is an inference
/// about intent, not a fact codegen can prove, and the repo's own bridge
/// sources contain dozens of items that are deliberately unbridged. So the
/// report is advisory — and it earns that by being nearly certain when it
/// fires. A warning people learn to scroll past is worse than no warning, so
/// the false-positive rate is the design constraint, not coverage.
///
/// Two shapes qualify, and only two:
///
/// 1. A fully-`pub` free `fn` whose *entire* signature is bridgeable — every
///    parameter, the return, the error — with every named type in it bridged in
///    this same file. This is F-00's shape exactly.
/// 2. An unannotated *inherent* `impl` block on a type this file bridges as a
///    handle (`confined`/`resident`/`frozen`/`locked`/`actor`), holding at least one
///    fully-`pub`, fully-bridgeable method. The type reaches Dart; its methods
///    vanish; nothing is said.
///
/// # What is deliberately NOT reported, and why
///
/// - **`pub struct` / `pub enum`.** Weak signal, and redundant: if anything
///   bridged names an unbridged type, FR0003 already rejects it loudly at the
///   use site. An unreferenced one is an internal type.
/// - **Trait impls** (`impl T for U`). These are `Drop`, `Future`,
///   `BytesCodec`, internal traits; not forgotten annotations.
/// - **Impls on non-opaque types.** FR0005 refuses `#[bridge] impl` on a data
///   struct, so the warning would prescribe a fix another diagnostic rejects —
///   the exact defect FR0040's message was fixed for.
/// - **`pub static`, `pub type`, `pub const`, `pub mod`, `pub use`.** Not
///   bridgeable in any form, so `#[bridge]` is never the answer.
/// - **Restricted visibility** (`pub(crate)`, `pub(super)`) and private items.
///   Not part of a crate's public surface; never candidates.
/// - **`#[cfg]`-gated items.** A deliberate conditional, and `#[bridge]` is the
///   wrong fix — FR0034 rejects a cfg-gated bridged member.
/// - **Anything whose signature does not fully resolve.** A `pub fn` naming a
///   type this file does not bridge reads as a helper.
///
/// # The escape hatch
///
/// `#[bridge(skip)]`: same vocabulary as `#[bridge(no_eq)]` / `#[bridge(
/// native_only)]`, and it changes nothing about what is bridged — it only turns
/// the question off. Without an escape hatch a heuristic warning becomes noise,
/// and noise is worse than silence.
///
/// # Scope: one file at a time
///
/// The scan is per-file, so a `pub fn` naming a type bridged in a *different*
/// bridge source does not resolve here and is not reported. That is a miss, not
/// a false alarm, and deliberately in that direction: making it cross-file
/// would mean carrying unparsed items through `merge` so they could be
/// re-examined against the whole interface, to buy coverage of a shape that
/// barely occurs (impls and free functions sit beside the types they name).
fn omission_warnings(file: &syn::File, iface: &Interface, module_path: &str) -> Vec<Warning> {
    // Read off the parsed interface rather than re-inspecting attributes: this
    // is by definition the set of names that reached the Dart surface, so the
    // scan cannot drift from what the parser actually accepted.
    let bridged: HashSet<&str> = iface
        .structs
        .iter()
        .map(|s| s.name.as_str())
        .chain(iface.enums.iter().map(|e| e.name.as_str()))
        .chain(iface.opaques.iter().map(|o| o.name.as_str()))
        .chain(iface.externs.iter().map(|e| e.name.as_str()))
        .collect();
    // Every handle type, for the position rules in `signature_is_bridgeable`.
    let opaques: HashSet<&str> = iface.opaques.iter().map(|o| o.name.as_str()).collect();
    // Every declared name that *reaches* a handle through its fields, opaques
    // included. Such a value may only travel Rust → Dart (FR0004), so a
    // signature taking one is not bridgeable and must not be warned about.
    // Computed by fixpoint over the pre-check declarations, where a field's
    // type is still a bare `Type::Named`.
    let carriers = opaque_carriers(iface, &opaques);
    // Every type an inherent `impl` block can carry bridged members for: the
    // concrete handles, and the data structs and enums. A `dyn_trait`
    // opaque's surface is the trait declaration, not an impl block, and
    // `#[bridge] impl Trait for T` is a different feature with different
    // rules; a `bytes(...)` extern has no generated class at all (FR0005).
    let impl_targets: HashSet<&str> = iface
        .opaques
        .iter()
        .filter(|o| !o.dyn_trait)
        .map(|o| o.name.as_str())
        .chain(iface.structs.iter().map(|s| s.name.as_str()))
        .chain(iface.enums.iter().map(|e| e.name.as_str()))
        // The templates too: `impl Page<Item>` is a bridgeable block now, so
        // an unannotated one loses its methods in silence — the omission this
        // warning exists to prevent. The *generic* spelling is filtered out
        // one level down, where the reason it cannot be decided is stated.
        .chain(iface.generic_structs.iter().map(|s| s.name.as_str()))
        .chain(iface.generic_enums.iter().map(|e| e.name.as_str()))
        .collect();
    // Names declared under two representations, whose `impl` block needs a
    // marker on its self type as well as `#[bridge]`.
    let duals: HashSet<&str> = iface
        .opaques
        .iter()
        .filter(|o| iface.is_dual(&o.name))
        .map(|o| o.name.as_str())
        .collect();

    let mut out = vec![];
    for item in &file.items {
        match item {
            syn::Item::Fn(f) => {
                if !is_public(&f.vis) || has_bridge_attr(&f.attrs) || has_cfg(&f.attrs) {
                    continue;
                }
                // `extern "C"` / `unsafe fn`: an FFI or unsafe surface of its
                // own, not something a bridge annotation was forgotten on.
                if f.sig.abi.is_some() || f.sig.unsafety.is_some() {
                    continue;
                }
                if !signature_is_bridgeable(&f.sig, None, module_path, &bridged, &opaques, &carriers) {
                    continue;
                }
                let name = f.sig.ident.to_string();
                out.push(Warning {
                    code: "FR0042",
                    file: String::new(),
                    line: f.sig.ident.span().start().line,
                    message: format!(
                        "`{name}`: this file is a declared bridge source, but `pub fn \
                         {name}` carries no `#[bridge]`, so codegen skipped it — and a \
                         skipped item produces no error anywhere: it is simply absent \
                         from the Dart surface, and the first sign is Dart failing to \
                         resolve a name it never had. Its signature is fully bridgeable \
                         and every type it names is bridged in this file, so this looks \
                         like a forgotten annotation. Add `#[bridge]` to bridge it, or \
                         `#[bridge(skip)]` to declare the omission and silence this. \
                         (Only fully-`pub`, fully-bridgeable free functions are \
                         reported; a helper naming an unbridged type is not.)"
                    ),
                });
            }
            syn::Item::Impl(im) => {
                if let Some(w) =
                    impl_omission(
                        im,
                        module_path,
                        &bridged,
                        &opaques,
                        &carriers,
                        &impl_targets,
                        &duals,
                    )
                {
                    out.push(w);
                }
            }
            _ => {}
        }
    }
    out
}

/// Shape 2: an inherent `impl` on a bridged type that can carry members —
/// a concrete handle, or a data struct or enum — with no `#[bridge]`.
fn impl_omission(
    im: &syn::ItemImpl,
    module_path: &str,
    bridged: &HashSet<&str>,
    opaques: &HashSet<&str>,
    carriers: &HashSet<&str>,
    impl_targets: &HashSet<&str>,
    duals: &HashSet<&str>,
) -> Option<Warning> {
    if im.trait_.is_some() || has_bridge_attr(&im.attrs) || has_cfg(&im.attrs) {
        return None;
    }
    // A generic impl IS bridgeable — `#[bridge] impl<T> Page<T>` puts each
    // member on every instantiation — but whether any one member is depends on
    // the instantiation its parameters are bound to, and this pass runs before
    // the expansion that computes them. `Page<Item>::len` is fine where
    // `Page<TextDoc>::len` is FR0004, and the two are one written method. So
    // this cannot decide, and says nothing rather than prescribing `#[bridge]`
    // for a block that would then be refused — the defect the FR0040 message
    // was fixed for. Lifetimes are fine and ignored, as `generic_params`
    // treats them.
    if im.generics.type_params().next().is_some() || im.generics.const_params().next().is_some() {
        return None;
    }
    let syn::Type::Path(tp) = im.self_ty.as_ref() else {
        return None;
    };
    let last = tp.path.segments.last()?;
    let name = last.ident.to_string();
    if !impl_targets.contains(name.as_str()) {
        return None;
    }
    // The self type **as written**, so the report names the block the author
    // is looking at: `impl Page<Item>`, not `impl Page`, which on a generic
    // data type is a different block and is refused.
    let written = match &last.arguments {
        syn::PathArguments::None => name.clone(),
        // `<…>` rather than the arguments: this warning points at a block, and
        // the block's own line is in the report, so the ellipsis is enough to
        // tell it from the bare `impl Page` that a generic data type refuses.
        _ => format!("{name}<…>"),
    };
    // A name may be declared under both, in which case neither word alone is
    // true of it. The warning still prescribes `#[bridge]`, which is still a
    // step forward: the block then needs a marker on its self type (FR0067),
    // and that two-step is what this heuristic's contract allows rather than
    // a wrong answer.
    let what = if !opaques.contains(name.as_str()) {
        "a data type"
    } else if duals.contains(name.as_str()) {
        "a type that crosses both by value and as a handle"
    } else {
        "a handle type"
    };
    let dropped: Vec<String> = im
        .items
        .iter()
        .filter_map(|i| match i {
            syn::ImplItem::Fn(m)
                if is_public(&m.vis) && !has_bridge_attr(&m.attrs) && !has_cfg(&m.attrs) =>
            {
                Some(m)
            }
            _ => None,
        })
        .filter(|m| {
            signature_is_bridgeable(&m.sig, Some(&name), module_path, bridged, opaques, carriers)
        })
        .map(|m| format!("`{}`", m.sig.ident))
        .collect();
    if dropped.is_empty() {
        return None;
    }
    let list = dropped.join(", ");
    let plural = if dropped.len() == 1 { "" } else { "s" };
    Some(Warning {
        code: "FR0042",
        file: String::new(),
        line: im.impl_token.span.start().line,
        message: format!(
            "`{name}`: `{name}` is bridged as {what}, but this inherent \
             `impl {written}` block carries no `#[bridge]` — so its bridgeable public \
             method{plural} ({list}) {} skipped, and Dart gets the `{name}` class with \
             {} missing and no error on either side. Put `#[bridge]` on the `impl` block \
             to bridge its methods, or `#[bridge(skip)]` to declare the omission. (Only \
             inherent impls on types this interface declares a representation for are \
             reported: trait impls, and impls on unbridged or `bytes(...)` types, are \
             never bridgeable this way.)",
            if dropped.len() == 1 { "is" } else { "are" },
            if dropped.len() == 1 { "it" } else { "them" },
        ),
    })
}

/// Every bridged name that reaches a handle through its fields, the handles
/// themselves included — so `carriers.contains(n)` reads as "a value of `n`
/// carries ownership of at least one Rust object".
///
/// A fixpoint rather than one pass: a struct reaches a handle through another
/// struct that reaches one, at any depth. Runs over the *pre-check*
/// declarations, where a field's type is still `Type::Named`, so it matches on
/// names rather than on resolved kinds.
fn opaque_carriers<'a>(iface: &'a Interface, opaques: &HashSet<&'a str>) -> HashSet<&'a str> {
    let mut carriers: HashSet<&str> = opaques.clone();
    loop {
        let before = carriers.len();
        let mentions = |ty: &Type, hit: &mut bool| {
            ty.walk(&mut |t| {
                if let Type::Named(n) = t {
                    if carriers.contains(n.as_str()) {
                        *hit = true;
                    }
                }
            });
        };
        let mut add: Vec<&str> = vec![];
        for s in &iface.structs {
            let mut hit = false;
            for f in &s.fields {
                mentions(&f.ty, &mut hit);
            }
            if hit {
                add.push(s.name.as_str());
            }
        }
        for e in &iface.enums {
            let mut hit = false;
            for v in &e.variants {
                for f in &v.fields {
                    mentions(&f.ty, &mut hit);
                }
            }
            if hit {
                add.push(e.name.as_str());
            }
        }
        carriers.extend(add);
        if carriers.len() == before {
            return carriers;
        }
    }
}

/// True when the whole signature would cross the bridge: it parses as a bridged
/// function, every named type in it is bridged in this file, and every handle
/// sits somewhere the checker allows one.
///
/// Reuses `parse_fn` rather than re-deriving the rule, so "bridgeable" here
/// means exactly what it means everywhere else — the predicate cannot drift
/// from the parser as types are added.
///
/// Two conditions are layered on top of parsing:
///
/// - **Resolution.** `parse_type` maps any unknown path to `Type::Named`, so
///   without this a `pub fn` taking a private helper struct would read as
///   bridgeable. Every name must be one this file bridges.
/// - **Handle position.** A signature can parse and still be rejected by the
///   checker for *where* a handle sits: an opaque by value, or reachable from
///   anything travelling Dart → Rust (FR0004 and friends), a Dart-object
///   handle in return position (FR0031 — Rust cannot mint a Dart object), an
///   opaque as the error type (FR0035). Warning on one of those would tell the author to add `#[bridge]`
///   and then have a different diagnostic refuse it — the exact defect the
///   FR0040 message was fixed for. The rule is not the checker's full
///   knowledge, and cannot be; it covers the positions a real signature
///   actually gets these wrong in. Anything it misses fails loudly at the
///   suggested fix rather than silently, so the residue is a two-step, never a
///   wrong answer.
fn signature_is_bridgeable(
    sig: &syn::Signature,
    parent: Option<&str>,
    module_path: &str,
    bridged: &HashSet<&str>,
    opaques: &HashSet<&str>,
    carriers: &HashSet<&str>,
) -> bool {
    let Ok(mut f) = parse_fn(
        sig,
        &BridgeAttr::default(),
        module_path,
        parent.map(str::to_string),
        None,
        vec![],
    ) else {
        return false;
    };
    // Shapes the parser records for the checker to refuse outright (FR0056,
    // FR0057). A warning here would prescribe `#[bridge]` and then have that
    // diagnostic reject it — the exact defect the FR0040 message was fixed for.
    if !f.generics.is_empty() || f.receiver == Some(Receiver::Typed) {
        return false;
    }
    // The same trap one representation over: `&mut self` on a **data** type
    // mutates a decoded copy and is refused by FR0013, so prescribing
    // `#[bridge]` for it would prescribe a rejection. A by-value receiver is
    // not in this set — a data type takes one, and the omission is worth
    // reporting because prescribing `#[bridge]` leads somewhere.
    let parent_is_data = parent.is_some_and(|p| !opaques.contains(p));
    if parent_is_data && f.receiver == Some(Receiver::RefMut) {
        return false;
    }
    // A receiver on a type that *reaches* a handle is decoded Dart → Rust and
    // refused by FR0004, for the same reason such a parameter is.
    if parent_is_data
        && f.receiver.is_some()
        && parent.is_some_and(|p| carriers.contains(p))
    {
        return false;
    }
    // A borrowed return that reaches a handle cannot mint (FR0004 again): the
    // reference is a view of something this Rust code still owns.
    if let Some(t) = &f.ret {
        let mut carries = false;
        t.walk(&mut |x| {
            if matches!(x, Type::Named(n) if carriers.contains(n.as_str())) {
                carries = true;
            }
        });
        if f.ret_borrow && carries {
            return false;
        }
    }
    // A representation-marker wrapper (`Locked<Doc>`) is invisible to the
    // shape tests below, which match on `Type::Named` and count opaques
    // structurally — the same defect the FR0040 message was fixed for, one
    // layer earlier: an unerased wrapper would make a handle invisible here,
    // this function would call the signature ordinary and warn "add
    // `#[bridge]`", and the checker would then refuse the handle the warning
    // just told the author to expose. Erasing first restores exact parity
    // with the bare spelling, which is what the tests below are about: what a
    // marker names is a *representation*, and the shapes here turn on whether
    // a name is a handle at all — a question a type declaring two answers both
    // ways, and answering it either way keeps this warning inside its stated
    // contract (the residue is a two-step, never a wrong answer).
    for p in &mut f.params {
        p.ty.erase_claims();
    }
    if let Some(t) = &mut f.ret {
        t.erase_claims();
    }
    if let Some(t) = &mut f.err {
        t.erase_claims();
    }
    let is_opaque = |t: &Type| matches!(t, Type::Named(n) if opaques.contains(n.as_str()));
    // A reference in a position that has no borrowed form (FR0077), or a
    // `&mut` of a value type at any depth (FR0013). Asked of the checker's own
    // rule rather than restated here, with the handle test spelled in this
    // pass's vocabulary: a handle is still a `Type::Named` before resolution.
    // After `erase_claims`, so a wrapped `&mut Locked<Doc>` is read as the
    // handle borrow it is.
    {
        let mut refused = vec![];
        for p in &f.params {
            // The same trap at the top of the parameter: `&mut` of a value type
            // mutates a decoded local, which FR0013 refuses — the parameter
            // twin of the `&mut self` test above.
            if p.borrow == Borrow::RefMut && !is_opaque(&p.ty) {
                return false;
            }
            crate::check::check_borrow_positions(&p.ty, "", None, &is_opaque, &mut refused);
        }
        if let Some(t) = &f.ret {
            crate::check::check_borrow_positions(t, "", None, &is_opaque, &mut refused);
        }
        if !refused.is_empty() {
            return false;
        }
    }
    // Every name the signature mentions must be bridged here. The parent of a
    // method needs no special case: it is in `bridged` by construction.
    let resolves = |t: &Type| {
        let mut ok = true;
        t.walk(&mut |t| {
            if let Type::Named(n) = t {
                if !bridged.contains(n.as_str()) {
                    ok = false;
                }
            }
        });
        ok
    };
    // A handle anywhere BELOW the top level of the type — inside a `Vec`, an
    // `Option`, a map, a tuple. Counted rather than position-tested: the total
    // exceeds the at-most-one the root itself can contribute exactly when
    // something deeper is a handle. (`Option<Opaque>` in return position is the
    // one legal nesting; its caller unwraps before asking.)
    let nests_opaque = |t: &Type| {
        let mut count = 0usize;
        t.walk(&mut |inner| {
            if is_opaque(inner) {
                count += 1;
            }
        });
        count > usize::from(is_opaque(t))
    };
    let has_dart_object = |t: &Type| {
        let mut found = false;
        t.walk(&mut |inner| {
            if matches!(inner, Type::DartObject(_)) {
                found = true;
            }
        });
        found
    };

    for p in &f.params {
        if !resolves(&p.ty) || nests_opaque(&p.ty) {
            return false;
        }
        // An opaque crosses as a handle, so a parameter takes it by reference;
        // by value the checker refuses it.
        if is_opaque(&p.ty) && p.borrow == Borrow::Value {
            return false;
        }
        // A value that *reaches* a handle travels Rust → Dart only (FR0004):
        // decoding one here would need Dart to give up an ownership it has no
        // syntax to give up. Only a bare handle, borrowed, is a parameter.
        let mut carries = false;
        p.ty.walk(&mut |t| {
            if matches!(t, Type::Named(n) if carriers.contains(n.as_str())) {
                carries = true;
            }
        });
        if carries && !is_opaque(&p.ty) {
            return false;
        }
    }
    if let Some(t) = &f.ret {
        if !resolves(t) || has_dart_object(t) {
            return false;
        }
        // This heuristic's own bar, deliberately below the checker's: FR0004
        // admits a handle in every position a return consumes on the way out,
        // and this stops at `-> Opaque` and `-> Option<Opaque>`. Under-
        // approximating is the safe direction for a *warning* about a possibly
        // forgotten `#[bridge]` — it stays quiet about a shape it has not been
        // taught, rather than telling an author to bridge something.
        let unwrapped = match t {
            Type::Option(inner) => inner.as_ref(),
            other => other,
        };
        if nests_opaque(unwrapped) {
            return false;
        }
    }
    if let Some(t) = &f.err {
        // An error crosses by value, so it can be neither a handle nor a
        // Dart object, at any depth.
        if !resolves(t) || has_dart_object(t) || is_opaque(t) || nests_opaque(t) {
            return false;
        }
    }
    true
}

/// True for `pub`, false for `pub(crate)` / `pub(super)` / `pub(in …)` /
/// private. Restricted visibility is a deliberate statement that the item is
/// not part of the crate's public surface, so it was never a bridge candidate.
fn is_public(vis: &syn::Visibility) -> bool {
    matches!(vis, syn::Visibility::Public(_))
}

/// Merge several parsed files into one interface.
pub fn merge(crate_name: &str, parts: Vec<Interface>) -> Interface {
    let mut out = Interface {
        crate_name: crate_name.to_string(),
        structs: vec![],
        enums: vec![],
        generic_structs: vec![],
        generic_enums: vec![],
        opaques: vec![],
        externs: vec![],
        functions: vec![],
    };
    for p in parts {
        out.structs.extend(p.structs);
        out.enums.extend(p.enums);
        out.generic_structs.extend(p.generic_structs);
        out.generic_enums.extend(p.generic_enums);
        out.opaques.extend(p.opaques);
        out.externs.extend(p.externs);
        out.functions.extend(p.functions);
    }
    out
}

fn bridge_attr(attrs: &[syn::Attribute]) -> Result<Option<BridgeAttr>> {
    // Computed over the whole list, not inside the loop below: a `#[cfg]` may
    // sit either side of the `#[bridge]`, and the loop returns at the first
    // `#[bridge]` it finds.
    // `cfg` only, deliberately not `cfg_attr`: `cfg_attr` never removes the
    // item, it only conditions another attribute on it, so
    // `#[cfg_attr(test, allow(dead_code))]` is no hazard at all. Flagging it
    // would prescribe `native_only` as the fix and thereby remove a portable
    // member from the web surface — a false contract is worse than none.
    let cfg_gated = has_cfg(attrs);
    for attr in attrs {
        if !is_bridge_attr(attr) {
            continue;
        }
        let mut out = BridgeAttr {
            cfg_gated,
            ..BridgeAttr::default()
        };
        match &attr.meta {
            syn::Meta::Path(_) => {}
            syn::Meta::List(_) => {
                let metas = attr.parse_args_with(
                    Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
                )?;
                for meta in metas {
                    parse_bridge_option(&meta, &mut out)?;
                }
            }
            syn::Meta::NameValue(_) => bail!("#[bridge = ...] is not a valid form"),
        }
        if out.skip {
            // `skip` says "do not bridge this"; every other option configures
            // HOW something is bridged. Together they contradict, and the
            // repo's rule is to say so rather than silently let one win.
            let bare = BridgeAttr {
                skip: true,
                cfg_gated,
                ..BridgeAttr::default()
            };
            if out != bare {
                bail!(
                    "`#[bridge(skip)]` declares an item deliberately unbridged, so it \
                     cannot be combined with options that configure bridging. Drop the \
                     other options to keep the item out of the Dart surface, or drop \
                     `skip` to bridge it"
                );
            }
            // The same `None` an unannotated item produces: identical
            // interface, by construction. Only the *silence* differs.
            return Ok(None);
        }
        return Ok(Some(out));
    }
    Ok(None)
}

/// True when this attribute is the `#[bridge]` marker, however it is pathed
/// (`#[bridge]`, `#[frustrate::bridge]`, a renamed import). One predicate,
/// shared by the parser and by the omission scan, so the two can never
/// disagree about what counts as annotated.
fn is_bridge_attr(attr: &syn::Attribute) -> bool {
    attr.path()
        .segments
        .last()
        .is_some_and(|s| s.ident == "bridge")
}

fn has_bridge_attr(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(is_bridge_attr)
}

/// `cfg` only, deliberately not `cfg_attr` — see [`bridge_attr`].
fn has_cfg(attrs: &[syn::Attribute]) -> bool {
    attrs
        .iter()
        .any(|a| a.path().segments.last().is_some_and(|s| s.ident == "cfg"))
}

/// True when `attrs` carries `#[bridge(skip)]`.
///
/// [`bridge_attr`] collapses "skip" and "no annotation" to the same `None`,
/// which is exactly right at the top level of a file — both mean the item is
/// not bridged. It is NOT right inside an annotated `impl`/`trait` block,
/// where `None` means "inherit the block's options" and would bridge the
/// method after all. Those two loops ask this question instead, so that
/// `#[bridge(skip)]` is never an annotation that does nothing (FR0041).
fn declares_skip(attrs: &[syn::Attribute]) -> Result<bool> {
    Ok(has_bridge_attr(attrs) && bridge_attr(attrs)?.is_none())
}

/// Read a file-level `frustrate::bridge_file!(...)` claim, if present.
///
/// Only the options in the allowlist below are legal here. That is a closed
/// world for the same reason `parse_bridge_option`'s final arm is: an option
/// silently accepted-and-ignored at file scope would read as covering the file
/// and cover nothing. `no_block` qualifies because it is a *claim* that only
/// ever strengthens; `sync` or `on_contention` would be policies whose file-wide
/// meaning is a different question, and they are refused until asked for.
fn parse_bridge_file(file: &syn::File) -> Result<BridgeAttr> {
    const ALLOWED: &[&str] = &["no_block"];

    let mut found: Option<&syn::ItemMacro> = None;
    for item in &file.items {
        let syn::Item::Macro(m) = item else { continue };
        if m.mac.path.segments.last().map(|s| s.ident.to_string())
            != Some("bridge_file".to_string())
        {
            continue;
        }
        if found.is_some() {
            bail!(
                "more than one `bridge_file!` in a bridge file. The claim is \
                 file-scoped, so a second one is either a duplicate or a \
                 disagreement — write one invocation listing every option"
            );
        }
        found = Some(m);
    }
    let Some(m) = found else {
        return Ok(BridgeAttr::default());
    };

    let metas = m
        .mac
        .parse_body_with(
            syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
        )
        .map_err(|e| anyhow::anyhow!("could not parse `bridge_file!(...)`: {e}"))?;
    if metas.is_empty() {
        bail!(
            "`bridge_file!()` claims nothing. Give it an option ({}), or delete it",
            ALLOWED.join(", ")
        );
    }

    let mut out = BridgeAttr::default();
    for meta in &metas {
        let name = meta
            .path()
            .get_ident()
            .map(|i| i.to_string())
            .unwrap_or_default();
        if !ALLOWED.contains(&name.as_str()) {
            bail!(
                "`bridge_file!({name})`: not a file-level option. A file-level \
                 claim may only *strengthen* every item in the file, so the \
                 accepted set is: {}",
                ALLOWED.join(", ")
            );
        }
        parse_bridge_option(meta, &mut out)?;
    }
    Ok(out)
}

fn parse_bridge_option(meta: &syn::Meta, out: &mut BridgeAttr) -> Result<()> {
    match meta {
        syn::Meta::Path(p) if p.is_ident("sync") => out.sync = true,
        syn::Meta::Path(p) if p.is_ident("no_eq") => out.no_eq = true,
        syn::Meta::Path(p) if p.is_ident("dart_interface") => out.dart_interface = true,
        syn::Meta::Path(p) if p.is_ident("inbound") => out.inbound = true,
        syn::Meta::Path(p) if p.is_ident("skip") => out.skip = true,
        syn::Meta::Path(p) if p.is_ident("native_only") => out.native_only = true,
        syn::Meta::Path(p) if p.is_ident("no_block") => out.no_block = true,
        syn::Meta::Path(p) if p.is_ident("getter") => out.getter = true,
        // The six representation keywords. `data` sets the bare flag; the
        // five concurrency models go through `set_model`, which catches two
        // models at once (FR0060). A declaration may name `data` *and* one
        // model: the type then crosses as both a value class and a handle
        // class, and each `impl` block and use site says which half it means.
        //
        // Each keyword also takes a nested `dart_identifier`, naming that
        // half's Dart class. Whether the nested form is required, optional or
        // refused is the *declaration's* rule, not the grammar's — see the
        // struct arm.
        syn::Meta::Path(p) if p.is_ident("data") => out.data = true,
        syn::Meta::List(l) if l.path.is_ident("data") => {
            out.data = true;
            out.data_dart_identifier = Some(nested_dart_identifier(l)?);
        }
        syn::Meta::Path(p) if p.is_ident("confined") => set_model(out, Model::Confined)?,
        syn::Meta::Path(p) if p.is_ident("resident") => set_model(out, Model::Resident)?,
        syn::Meta::Path(p) if p.is_ident("frozen") => set_model(out, Model::Frozen)?,
        syn::Meta::Path(p) if p.is_ident("locked") => set_model(out, Model::Locked)?,
        syn::Meta::Path(p) if p.is_ident("actor") => set_model(out, Model::Actor)?,
        syn::Meta::List(l) if MODEL_KEYWORDS.iter().any(|(k, _)| l.path.is_ident(k)) => {
            let model = MODEL_KEYWORDS
                .iter()
                .find(|(k, _)| l.path.is_ident(k))
                .map(|(_, m)| *m)
                .expect("the guard just matched one");
            set_model(out, model)?;
            out.model_dart_identifier = Some(nested_dart_identifier(l)?);
        }
        syn::Meta::List(l) if l.path.is_ident("bytes") => {
            let metas =
                l.parse_args_with(Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated)?;
            let mut dart = None;
            let mut import = None;
            let mut encode = None;
            let mut decode = None;
            for m in metas {
                let syn::Meta::NameValue(nv) = &m else {
                    bail!("bytes(...) options are name = \"value\" pairs");
                };
                let syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(s),
                    ..
                }) = &nv.value
                else {
                    bail!("bytes(...) option values must be string literals");
                };
                let v = Some(s.value());
                if nv.path.is_ident("dart") {
                    dart = v;
                } else if nv.path.is_ident("import") {
                    import = v;
                } else if nv.path.is_ident("encode") {
                    encode = v;
                } else if nv.path.is_ident("decode") {
                    decode = v;
                } else {
                    bail!(
                        "unknown bytes(...) option `{}`; expected dart, import, encode, decode",
                        quote_meta(&m)
                    );
                }
            }
            out.bytes = Some(BytesAttr {
                dart: dart.context(
                    "bytes(...) requires dart = \"<DartType>\" (the Dart-side type name)",
                )?,
                import: import.context(
                    "bytes(...) requires import = \"<uri>\" (where the Dart type lives)",
                )?,
                encode,
                decode,
            });
        }
        syn::Meta::NameValue(nv) if nv.path.is_ident("dart_identifier") => {
            let syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(lit),
                ..
            }) = &nv.value
            else {
                bail!("dart_identifier expects a string: dart_identifier = \"wordCount\"");
            };
            out.dart_identifier = Some(lit.value());
        }
        syn::Meta::NameValue(nv) if nv.path.is_ident("web") => {
            let syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(s),
                ..
            }) = &nv.value
            else {
                bail!("web expects a string: web = \"runtime_fail\"");
            };
            match s.value().as_str() {
                // The only web policy today. Absent = the default (a native-only
                // member is compile-time absent from the web surface); this
                // value opts it back in as a runtime-throwing stub.
                "runtime_fail" => out.web_runtime_fail = true,
                other => bail!(
                    "unknown web policy `{other}`; the only value is \"runtime_fail\" \
                     (include a native-only member in the web build, failing loudly at \
                     runtime if it is actually called). Omit `web` for the default: the \
                     member is compile-time absent from the web surface"
                ),
            }
        }
        syn::Meta::NameValue(nv) if nv.path.is_ident("on_contention") => {
            let syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(s),
                ..
            }) = &nv.value
            else {
                bail!("on_contention expects a string: on_contention = \"error\" | \"block\"");
            };
            out.on_contention = Some(match s.value().as_str() {
                "error" => OnContention::Error,
                "block" => OnContention::Block,
                other => bail!(
                    "unknown on_contention `{other}`; expected \"error\" or \"block\""
                ),
            });
        }
        other => bail!("unknown #[bridge] option: {}", quote_meta(other)),
    }
    Ok(())
}

/// FR0058 — a bridged type declaration (struct/enum/trait) must name at least
/// one representation: `data`, or one of the five concurrency models
/// (`confined`/`resident`/`frozen`/`locked`/`actor`), or — on a struct — `data` and one
/// model, which crosses as both. Called after the `bytes(...)` early-out (a
/// `bytes(...)` external needs no representation of its own — see the struct
/// arm), so a struct reaching this has already ruled that out.
///
/// Only zero is caught here. Two *models* is FR0060, in `set_model`; whether
/// a given pair is legal on a given item kind is that arm's rule (an enum is
/// always `data`, a trait is never).
fn require_representation(data: bool, model: Option<Model>, kind: &str, name: &str) -> Result<()> {
    if !data && model.is_none() {
        bail!(
            "FR0058: {kind} `{name}`: a bridged type declaration must name its \
             representation — #[bridge(data)] (crosses by value), or one of the \
             five concurrency models, #[bridge(confined)] / #[bridge(resident)] / \
             #[bridge(frozen)] / #[bridge(locked)] / #[bridge(actor)] (crosses as \
             a handle). \
             Functions and impl blocks are unaffected — they have no \
             representation to name"
        );
    }
    Ok(())
}

/// The Dart class name for each half of a type declaration — `(data, handle)`
/// — from the flat `dart_identifier` and the nested per-keyword forms.
///
/// A declaration naming one representation mints one class, so its one name
/// may be written either way and both spellings mean the same thing. A
/// declaration naming two mints two, and a single unqualified name would have
/// to be given to one of them arbitrarily — so FR0068 asks which.
fn half_dart_identifiers(
    attr: &BridgeAttr,
    kind: &str,
    name: &str,
) -> Result<(Option<String>, Option<String>)> {
    let dual = attr.data && attr.model.is_some();
    let nested = attr.data_dart_identifier.is_some() || attr.model_dart_identifier.is_some();
    if attr.dart_identifier.is_some() && (dual || nested) {
        let why = if dual {
            "this declaration names two representations, so it mints two Dart classes and \
             a bare `dart_identifier` does not say which one it renames"
        } else {
            "the same class is named twice"
        };
        bail!(
            "FR0068: {kind} `{name}`: {why}. Write the name inside the keyword whose \
             class it is — `#[bridge(data(dart_identifier = \"…\"))]` for the value \
             class, `#[bridge(locked(dart_identifier = \"…\"))]` (or confined/\
             resident/frozen/actor) for the handle class"
        );
    }
    if dual {
        Ok((
            attr.data_dart_identifier.clone(),
            attr.model_dart_identifier.clone(),
        ))
    } else if attr.data {
        Ok((
            attr.data_dart_identifier
                .clone()
                .or_else(|| attr.dart_identifier.clone()),
            None,
        ))
    } else {
        Ok((
            None,
            attr.model_dart_identifier
                .clone()
                .or_else(|| attr.dart_identifier.clone()),
        ))
    }
}

/// The five concurrency-model keywords, for the arms that must accept any of
/// them: [`parse_bridge_option`]'s nested `<model>(dart_identifier = "…")`
/// form.
const MODEL_KEYWORDS: &[(&str, Model)] = &[
    ("confined", Model::Confined),
    ("resident", Model::Resident),
    ("frozen", Model::Frozen),
    ("locked", Model::Locked),
    ("actor", Model::Actor),
];

/// The one option a representation keyword nests:
/// `data(dart_identifier = "…")`. Grammar precedent in this attribute:
/// `bytes(dart = "…", import = "…")`.
fn nested_dart_identifier(l: &syn::MetaList) -> Result<String> {
    let kw = l
        .path
        .get_ident()
        .map(|i| i.to_string())
        .unwrap_or_default();
    let metas = l.parse_args_with(Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated)?;
    let mut found = None;
    for m in &metas {
        let syn::Meta::NameValue(nv) = m else {
            bail!(
                "`{kw}(...)`: the only option a representation keyword takes is \
                 `dart_identifier = \"…\"`, the Dart class name for the half it \
                 names"
            );
        };
        if !nv.path.is_ident("dart_identifier") {
            bail!(
                "unknown `{kw}(...)` option `{}`; the only one is \
                 `dart_identifier = \"…\"`",
                quote_meta(m)
            );
        }
        let syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(s),
            ..
        }) = &nv.value
        else {
            bail!("`{kw}(dart_identifier = ...)` expects a string: `{kw}(dart_identifier = \"DocHandle\")`");
        };
        if found.replace(s.value()).is_some() {
            bail!("`{kw}(...)`: `dart_identifier` given twice");
        }
    }
    found.with_context(|| {
        format!(
            "`{kw}()` claims nothing. Write `{kw}` on its own, or \
             `{kw}(dart_identifier = \"…\")`"
        )
    })
}

/// Records a concurrency-model keyword
/// (`confined`/`resident`/`frozen`/`locked`/`actor`)
/// onto `out`, catching the way it can conflict with what is already there: a
/// *different* model already set (FR0060 — a type cannot simultaneously be
/// single-owner and shared-lock, say, so this pair is mutually exclusive by
/// construction rather than by a choice not yet made).
///
/// `data` alongside a model is not a conflict: a declaration naming both
/// crosses as two Dart classes. Which options that combination admits is the
/// declaration's rule, checked in the struct arm.
fn set_model(out: &mut BridgeAttr, model: Model) -> Result<()> {
    if out.model.is_some() {
        bail!(
            "FR0060: a bridged type declares at most one concurrency model — \
             confined, resident, frozen, locked and actor describe mutually \
             exclusive object shapes (single-owner, single-owner-and-thread-bound, \
             immutable-shared, lock-shared, message-passing), so two on the same \
             #[bridge(...)] can never both hold"
        );
    }
    out.model = Some(model);
    Ok(())
}

/// FR0061 — `data` on a bridged trait. A trait has no fields to copy, so
/// `data` (crosses by value) has nothing to act on; a bridged trait always
/// crosses as a handle, under one of the five concurrency models.
fn reject_data_on_trait(data: bool, name: &str) -> Result<()> {
    if data {
        bail!(
            "FR0061: trait `{name}`: `data` is only valid on a struct or enum — \
             a trait has no fields to copy. A bridged trait crosses as a handle; \
             declare its concurrency model instead: confined, resident, frozen, \
             locked, or actor"
        );
    }
    Ok(())
}

/// Reject `#[bridge(no_eq)]` where it has no meaning. Value equality is only
/// generated for data structs and data enums, so `no_eq` (its opt-out) is a
/// mistake on any other bridged item — fail loudly naming the item rather than
/// silently ignore a flag the author expected to do something.
/// Reject `#[bridge(native_only)]` where it cannot bite. The flag removes a
/// member or a handle type from the web surface; a data struct, data enum or
/// `bytes(...)` external crosses by value on every target and has no web
/// presence to remove — so the flag there is a contract the author expected to
/// mean something. Fail loudly naming the item, the way `no_eq` does.
/// Reject a `#[cfg]` on a bridged **data** type. Same hazard as FR0034 (codegen
/// never evaluates cfg predicates, so the type is emitted into both surfaces
/// and the generated codec names it unconditionally — `E0425` on whichever
/// target the gate excludes), but it is caught here rather than by the checker
/// because the remedy is different: a data type has no `native_only` to
/// declare, since it crosses by value on every target. The only fix is to stop
/// gating it, so the message says that instead of offering a flag that would be
/// rejected.
fn reject_cfg_gated_data_type(cfg_gated: bool, kind: &str, name: &str) -> Result<()> {
    if cfg_gated {
        bail!(
            "{kind} `{name}`: a bridged data type cannot be `#[cfg]`-gated. Codegen \
             never evaluates cfg predicates, so `{name}` is emitted into both Dart \
             surfaces and the generated codec names it unconditionally — the build \
             fails inside generated code (E0425) on any target the gate excludes. \
             Remove the `#[cfg]`, or stop bridging `{name}`; unlike a member or an \
             opaque type it has no `native_only` to declare, because a data type \
             crosses by value on every target"
        );
    }
    Ok(())
}

// Takes the flag rather than the whole `BridgeAttr`: one caller sits after
// `attr.bytes` has been moved out, and a disjoint field read still borrows.
fn reject_native_only(native_only: bool, kind: &str, name: &str, why: &str) -> Result<()> {
    if native_only {
        bail!(
            "{kind} `{name}`: native_only is only valid on a bridged function, an \
             `impl`/`trait` block, or a handle type \
             (confined/resident/frozen/locked/actor) \
             — {why}. If a *member* that uses this type cannot build for wasm32, \
             declare native_only on that member instead"
        );
    }
    Ok(())
}

/// `no_block` claims something about a *body*, so it is meaningless on a
/// declaration that has none. Rejected rather than ignored: a claim that binds
/// nothing reads as coverage the author does not have, and the whole value of
/// the annotation is that a reader can trust what it covers.
fn reject_no_block(no_block: bool, kind: &str, name: &str) -> Result<()> {
    if no_block {
        bail!(
            "{kind} `{name}`: no_block is only valid on a bridged function or \
             method (or an impl/trait block of them, or a whole file via \
             `frustrate::bridge_file!(no_block);`). It claims a body never \
             waits, and a data type has no body"
        );
    }
    Ok(())
}

fn reject_no_eq(attr: &BridgeAttr, kind: &str, name: &str) -> Result<()> {
    if attr.no_eq {
        let named = if name.is_empty() {
            String::new()
        } else {
            format!(" `{name}`")
        };
        bail!(
            "{kind}{named}: no_eq is only valid on a data struct or data enum \
             (value equality is generated only for those)"
        );
    }
    Ok(())
}

/// Reject `#[bridge(dart_interface)]` where it cannot bite. The option replaces
/// the generated Dart **class** of a data struct with an interface to implement,
/// so anything with no such class — a function, an enum, an opaque type, a
/// `bytes(...)` external, an impl or trait block — has nothing for it to
/// replace. Fail loudly naming the item, the way `no_eq` does; the alternative
/// is a flag the author expected to change the surface and that silently did
/// not.
///
/// The *shape* of a legal one (all fields closure mirrors, at least one, no
/// method colliding with `Object`) is FR0050–FR0052 in the checker, where the
/// types are resolved. Here only the position is judged.
/// Reject a representation keyword where there is nothing to represent.
///
/// `data` and the four model keywords name how a *type* crosses. A function
/// and an `impl` block have no representation of their own — the type they
/// belong to already named one — so the keyword there describes nothing.
/// Accepting and ignoring it is the outcome the charter forbids: an
/// annotation the author wrote, that codegen silently drops. The short
/// spellings make it a plausible slip, e.g. reaching for `#[bridge(locked)]`
/// on a method meaning "this one touches the lock".
fn reject_representation(data: bool, model: Option<Model>, kind: &str, name: &str) -> Result<()> {
    let Some(word) = (match (data, model) {
        (true, _) => Some("data".to_string()),
        (_, Some(m)) => Some(format!("{m:?}").to_lowercase()),
        _ => None,
    }) else {
        return Ok(());
    };
    let named = if name.is_empty() {
        String::new()
    } else {
        format!(" `{name}`")
    };
    bail!(
        "FR0063: {kind}{named}: `{word}` names how a *type* crosses the bridge, and a \
         {kind} does not cross — the type it belongs to declares its own \
         representation. Drop it. Which generated Dart class a member belongs on is \
         said by the *impl block's self type*, through the marker the type already \
         understands — `#[bridge] impl Locked<Doc>` rather than `impl Doc` — not by \
         repeating the keyword on the member"
    );
}

fn reject_dart_interface(dart_interface: bool, kind: &str, name: &str) -> Result<()> {
    if dart_interface {
        let named = if name.is_empty() {
            String::new()
        } else {
            format!(" `{name}`")
        };
        bail!(
            "{kind}{named}: dart_interface is only valid on a data struct whose \
             fields are all DartCallback/DartFunction (it replaces that struct's \
             generated Dart class with an `abstract interface class` the caller \
             implements)"
        );
    }
    Ok(())
}

/// Reject `#[bridge(inbound)]` where it names no direction.
///
/// The option restricts the positions a **data** declaration may appear in, so
/// it needs a generated value class to restrict: a function, an `impl`/`trait`
/// block and a handle-only struct have none, and a `bytes(...)` external's Dart
/// peer is a class the app supplies rather than one this generates. `why` says
/// which of those this is. Fail loudly naming the item, the way `no_eq` does;
/// the alternative is a flag the author wrote to change a direction and that
/// silently changed nothing.
fn reject_inbound(inbound: bool, kind: &str, name: &str, why: &str) -> Result<()> {
    if inbound {
        let named = if name.is_empty() {
            String::new()
        } else {
            format!(" `{name}`")
        };
        bail!(
            "{kind}{named}: inbound is only valid on a data struct — it says that \
             struct's generated Dart class crosses Dart → Rust only, so a handle \
             field of it is a `Consumed<…>` the caller hands over. {why}"
        );
    }
    Ok(())
}

fn quote_meta(meta: &syn::Meta) -> String {
    meta.path()
        .segments
        .iter()
        .map(|s| s.ident.to_string())
        .collect::<Vec<_>>()
        .join("::")
}

/// The item's type and const parameter names, in order. Lifetimes are not
/// generics the bridge cares about — a borrowed `&'a str` is fine and the
/// lifetime is dropped — so they are not recorded. Recorded rather than
/// refused: a function's parameters are what FR0056's message names, and a
/// data declaration's are what `check`'s expansion binds.
fn generic_params(generics: &syn::Generics) -> Vec<String> {
    let g = generic_decl(generics);
    g.types.into_iter().chain(g.consts).collect()
}

/// The three facts about a declaration's generics the checker needs apart:
/// which parameters can be bound by an instantiation, which cannot, and which
/// would let a use site leave an argument out.
#[derive(Default)]
struct GenericDecl {
    /// Type parameter names, in order.
    types: Vec<String>,
    /// Const parameter names, in order.
    consts: Vec<String>,
    /// Names — type or const — that declare a default.
    defaulted: Vec<String>,
}

/// FR0056 for the two representations a type parameter cannot reach.
///
/// A `#[bridge(data)]` declaration may be generic — `check` expands every
/// fully-applied use into a non-generic declaration and the Dart side is one
/// generic class. Neither move is available to a **handle** or a
/// `bytes(...)` **external** type, for reasons `why` states per kind. Refused
/// where the declaration is read, because both `OpaqueDecl` and `ExternDecl`
/// record a bare name: without this the parameters were dropped in silence and
/// the generated Rust named `crate::api::Store` for a `Store<T>` (E0107, inside
/// generated code).
fn reject_generic_representation(
    generics: &syn::Generics,
    kind: &str,
    name: &str,
    why: &str,
) -> Result<()> {
    let params = generic_params(generics);
    if params.is_empty() {
        return Ok(());
    }
    bail!(
        "FR0056: {kind} `{name}`: `<{}>` — {why}. Declare a non-generic newtype per \
         instantiation (`struct {name}OfI64({name}<i64>);`) and bridge that",
        params.join(", ")
    )
}

fn generic_decl(generics: &syn::Generics) -> GenericDecl {
    let mut out = GenericDecl::default();
    for p in &generics.params {
        match p {
            syn::GenericParam::Type(t) => {
                out.types.push(t.ident.to_string());
                if t.default.is_some() {
                    out.defaulted.push(t.ident.to_string());
                }
            }
            syn::GenericParam::Const(c) => {
                out.consts.push(c.ident.to_string());
                if c.default.is_some() {
                    out.defaulted.push(c.ident.to_string());
                }
            }
            syn::GenericParam::Lifetime(_) => {}
        }
    }
    out
}

/// Rustdoc lines (`///` → `#[doc = "..."]`) on an item, in source order, with
/// their leading-space intact so re-emitting `///{line}` reproduces the text.
/// Non-doc attributes are ignored. Doc comments would otherwise be dropped
/// entirely; this preserves them onto the Dart surface.
/// Reject a `#[bridge(...)]` attribute in a position that does not carry one.
///
/// An unsupported `#[bridge(...)]` option on a field or variant is rejected
/// rather than ignored: generating Dart from a declaration whose annotation
/// codegen never read is the silent outcome the contract ("everything not
/// supported is rejected loudly at codegen with a named diagnostic") forbids,
/// and rustc only objects to such an attribute in some positions.
///
/// The one `#[bridge(...)]` option a field or a variant may carry:
/// `dart_identifier`. Every other option belongs on the item.
///
/// A field and a variant each mint a Dart name of their own, so each is a
/// place two Rust items can collide (FR0002) — which makes each a place the
/// author has to be able to say which one moves. Nothing else here has a
/// meaning at this level, so everything else is still FR0041.
fn field_dart_identifier(
    attrs: &[syn::Attribute],
    what: &str,
    name: &str,
) -> Result<Option<String>> {
    let mut found = None;
    for attr in attrs {
        if !is_bridge_attr(attr) {
            continue;
        }
        let mut out = BridgeAttr::default();
        if let syn::Meta::List(l) = &attr.meta {
            for m in l.parse_args_with(Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated)? {
                parse_bridge_option(&m, &mut out)?;
            }
        }
        let ident = out.dart_identifier.take();
        // Compared against a default with the one accepted option cleared, so a
        // new option cannot quietly become legal here by being forgotten.
        if out != BridgeAttr::default() || ident.is_none() {
            bail!(
                "FR0041: `{name}`: the only `#[bridge(...)]` option on {what} is \
                 `dart_identifier = \"…\"`, which renames what it lands under in Dart. \
                 Every other option belongs on the item — the struct, enum, trait, impl \
                 or method — not on its {what}."
            );
        }
        found = ident;
    }
    Ok(found)
}

fn doc_lines(attrs: &[syn::Attribute]) -> Vec<String> {
    let mut out = vec![];
    for attr in attrs {
        if !attr.path().is_ident("doc") {
            continue;
        }
        if let syn::Meta::NameValue(nv) = &attr.meta {
            if let syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(s),
                ..
            }) = &nv.value
            {
                out.push(s.value());
            }
        }
    }
    out
}

fn parse_struct(
    s: &syn::ItemStruct,
    module_path: &str,
    no_eq: bool,
    dart_interface: bool,
    inbound: bool,
    dart_identifier: Option<String>,
) -> Result<StructDecl> {
    let generics = generic_decl(&s.generics);
    // The shape is read straight from the syntax and recorded — the emitters
    // read it, never the field names. Tuple fields get the same synthesized
    // `field{i}` names a tuple variant does.
    let (shape, syn_fields): (StructShape, Vec<&syn::Field>) = match &s.fields {
        syn::Fields::Named(named) => (StructShape::Named, named.named.iter().collect()),
        syn::Fields::Unnamed(unnamed) => (StructShape::Tuple, unnamed.unnamed.iter().collect()),
        syn::Fields::Unit => (StructShape::Unit, vec![]),
    };
    let mut fields = vec![];
    for (i, f) in syn_fields.into_iter().enumerate() {
        let fname = match &f.ident {
            Some(id) => id.to_string(),
            None => format!("field{i}"),
        };
        let dart_identifier = field_dart_identifier(&f.attrs, "a field", &fname)?;
        let mut ty = parse_type(&f.ty)
            .with_context(|| format!("struct `{}`, field `{fname}`", s.ident))?;
        // `Self` in a field names the declaring type, exactly as it does in a
        // member signature — `struct Tree { children: Vec<Self> }` is the
        // idiomatic recursive spelling and compiles in Rust.
        substitute_self(
            &mut ty,
            Some((&s.ident.to_string(), None)),
            &format!("struct `{}`, field `{fname}`: `Self`", s.ident),
        )?;
        apply_own_params(&mut ty, &s.ident.to_string(), &generics.types);
        fields.push(Field {
            ty,
            name: fname,
            dart_identifier,
            docs: doc_lines(&f.attrs),
        });
    }
    Ok(StructDecl {
        name: s.ident.to_string(),
        module_path: module_path.to_string(),
        fields,
        shape,
        generics: generics.types,
        const_generics: generics.consts,
        defaulted_generics: generics.defaulted,
        instance: None,
        no_eq,
        dart_interface,
        inbound,
        dart_identifier,
        docs: doc_lines(&s.attrs),
    })
}

/// Every variant's discriminant, or `None` when the enum writes none.
///
/// Reads the source and never evaluates it. A discriminant is an **integer
/// literal**, optionally negated (`= 1`, `= -3`, `= 0x10`); `= FOO`,
/// `= 1 << 3` and `= OTHER as i64` are refused (FR0040) rather than guessed
/// at, because working them out means evaluating Rust — const items, `use`
/// paths, arithmetic — which this parser does not do and must not pretend to.
///
/// A variant that writes none takes Rust's own rule, one more than the
/// previous (`enum E { A = 5, B }` gives `B = 6`), starting at 0. That is a
/// property of the declaration, not an expression, so reading it needs no
/// evaluator; overflowing `i64` on the way is refused rather than wrapped.
///
/// Two shapes are refused whatever they are written with:
///
/// - **a fielded enum**, because a Dart sealed class has no enum value to hang
///   `discriminant` on. (rustc allows this only under a `#[repr]`, so most
///   authors meet rustc's own refusal first.)
/// - **a literal outside `i64`**, because Dart's `int` is a 64-bit signed
///   integer on every target this bridge emits for, so there is nothing on the
///   far side that could hold it. Negative discriminants carry fine and are
///   accepted.
fn discriminants(e: &syn::ItemEnum) -> Result<Option<Vec<i64>>> {
    if !e.variants.iter().any(|v| v.discriminant.is_some()) {
        return Ok(None);
    }
    if let Some(v) = e.variants.iter().find(|v| !v.fields.is_empty()) {
        bail!(
            "FR0040: `{}::{}` carries fields, and `{}` declares an explicit \
             discriminant. A fielded data enum lands in Dart as a sealed class, \
             whose variants are classes rather than enum values, so there is \
             nothing for the `discriminant` getter to be declared on. Drop the \
             discriminants, or split the tags into a unit-only enum beside the \
             fielded one",
            e.ident,
            v.ident,
            e.ident,
        );
    }
    let mut out = Vec::with_capacity(e.variants.len());
    let mut next: Option<i64> = Some(0);
    for (i, v) in e.variants.iter().enumerate() {
        let value = match &v.discriminant {
            // The successor of the previous variant. `None` there means the
            // previous one was `i64::MAX` and there is no successor — asked
            // only here, so a *last* variant at `i64::MAX` is fine: nothing
            // needs the number after it.
            None => next.ok_or_else(|| {
                anyhow::anyhow!(
                    "FR0040: `{}::{}` writes no discriminant, so it is one more than \
                     `{}` — Rust's own rule — and that does not fit in an `i64`. Dart's \
                     `int` is a 64-bit signed integer, so there is nothing on the far \
                     side to hold it. Write the number out",
                    e.ident,
                    v.ident,
                    e.variants[i - 1].ident,
                )
            })?,
            Some((_, expr)) => {
                let Some(written) = literal_discriminant(expr) else {
                    bail!(
                        "FR0040: `{}::{}` declares a discriminant that is not an integer \
                         literal. frustrate reads the discriminant out of the source and \
                         never evaluates it — working out `= FOO`, `= 1 << 3` or \
                         `= OTHER as i64` means evaluating Rust, which this is not — so \
                         a value it cannot read is refused rather than guessed at. Write \
                         the number, or drop the discriminants and expose the mapping as \
                         a bridged method on the enum, e.g. `#[bridge] impl {2} {{ \
                         #[bridge(sync)] pub fn value(&self) -> i64 {{ .. }} }}`",
                        e.ident,
                        v.ident,
                        e.ident,
                    );
                };
                i64::try_from(written).map_err(|_| {
                    anyhow::anyhow!(
                        "FR0040: `{}::{}` declares the discriminant `{written}`, which \
                         does not fit in an `i64`. Dart's `int` is a 64-bit signed \
                         integer, so there is nothing on the far side that could hold \
                         it. Every value in `i64` carries, negatives included",
                        e.ident,
                        v.ident,
                    )
                })?
            }
        };
        out.push(value);
        next = value.checked_add(1);
    }
    Ok(Some(out))
}

/// An integer-literal discriminant, `-` included, as an `i128` so that the
/// range check is a separate answer from "this is not a literal". `None` for
/// anything the parser will not evaluate.
fn literal_discriminant(expr: &syn::Expr) -> Option<i128> {
    match expr {
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Int(i),
            ..
        }) => i.base10_parse::<i128>().ok(),
        syn::Expr::Unary(syn::ExprUnary {
            op: syn::UnOp::Neg(_),
            expr,
            ..
        }) => literal_discriminant(expr).map(|v| -v),
        syn::Expr::Group(g) => literal_discriminant(&g.expr),
        syn::Expr::Paren(p) => literal_discriminant(&p.expr),
        _ => None,
    }
}

fn parse_enum(
    e: &syn::ItemEnum,
    module_path: &str,
    no_eq: bool,
    dart_identifier: Option<String>,
) -> Result<EnumDecl> {
    let generics = generic_decl(&e.generics);
    let mut variants = vec![];
    // Explicit discriminants are **carried**, as a `discriminant` getter on the
    // generated Dart enum, and the frustrate wire stays positional. A number an
    // author writes on an enum is usually the contract — a wire code, a C ABI
    // value, a database column — so Dart can read it without a bridged free
    // function beside the type. See [`discriminants`] for what is read and
    // what FR0040 still refuses.
    let discriminants = discriminants(e)?;
    for v in &e.variants {
        // Tuple-vs-named is read straight from the syntax here — the one place
        // the truth exists — and recorded on the IR. Emitters must never
        // re-derive it from the synthesized `field0` names (a real named field
        // called `field0` would fool that inference).
        let (fields, tuple) = match &v.fields {
            syn::Fields::Unit => (vec![], false),
            syn::Fields::Named(named) => (
                named
                    .named
                    .iter()
                    .map(|f| {
                        let fname = f.ident.as_ref().unwrap().to_string();
                        let dart_identifier =
                            field_dart_identifier(&f.attrs, "a field", &fname)?;
                        Ok(Field {
                            name: fname,
                            ty: parse_type(&f.ty)?,
                            dart_identifier,
                            docs: doc_lines(&f.attrs),
                        })
                    })
                    .collect::<Result<Vec<_>>>()?,
                false,
            ),
            syn::Fields::Unnamed(unnamed) => (
                unnamed
                    .unnamed
                    .iter()
                    .enumerate()
                    .map(|(i, f)| {
                        let dart_identifier = field_dart_identifier(
                            &f.attrs,
                            "a field",
                            &format!("field{i}"),
                        )?;
                        Ok(Field {
                            name: format!("field{i}"),
                            ty: parse_type(&f.ty)?,
                            dart_identifier,
                            docs: doc_lines(&f.attrs),
                        })
                    })
                    .collect::<Result<Vec<_>>>()?,
                true,
            ),
        };
        let dart_identifier =
            field_dart_identifier(&v.attrs, "a variant", &v.ident.to_string())?;
        // `Self` in a variant's field names the declaring enum, the same rule
        // a struct field takes.
        let mut fields = fields;
        for f in &mut fields {
            substitute_self(
                &mut f.ty,
                Some((&e.ident.to_string(), None)),
                &format!(
                    "enum `{}`, variant `{}`, field `{}`: `Self`",
                    e.ident, v.ident, f.name
                ),
            )?;
            apply_own_params(&mut f.ty, &e.ident.to_string(), &generics.types);
        }
        variants.push(Variant {
            name: v.ident.to_string(),
            fields,
            tuple,
            discriminant: discriminants.as_ref().map(|d| d[variants.len()]),
            dart_identifier,
            docs: doc_lines(&v.attrs),
        });
    }
    Ok(EnumDecl {
        name: e.ident.to_string(),
        module_path: module_path.to_string(),
        variants,
        generics: generics.types,
        const_generics: generics.consts,
        defaulted_generics: generics.defaulted,
        instance: None,
        no_eq,
        dart_identifier,
        docs: doc_lines(&e.attrs),
    })
}

/// An `impl` block's self type as a bare name, the representation claim if it
/// was written through a marker wrapper (`impl Locked<Point>` rather than
/// `impl Point`), and the type arguments if it was written applied
/// (`impl Page<Item>`, `impl<T> Page<T>`).
///
/// The one position this can't fall out of ordinary type resolution:
/// `Function::parent` is stored as a bare `String`, not a [`Type`], because
/// it identifies the impl's target rather than crossing the wire — so it
/// never reaches `check::resolve_type`, and both the wrapper and the argument
/// list have to be recognised here instead. The arguments ride on
/// [`Function::parent_args`] until `check`'s expansion binds them; dropping
/// them is what this used to do, and it made `impl Store<i64>` emit
/// `crate::api::Store::new()`.
///
/// Whether the head is a generic data template at all is `check`'s question
/// (FR0073/FR0056): the parser cannot see the declarations.
fn self_type_and_claim(tp: &syn::TypePath) -> Result<(String, Option<Claim>, Vec<Type>)> {
    let last = tp.path.segments.last().unwrap();
    let claim = match last.ident.to_string().as_str() {
        "Data" => Some(Claim::Data),
        "Confined" => Some(Claim::Model(Model::Confined)),
        "Resident" => Some(Claim::Model(Model::Resident)),
        "Frozen" => Some(Claim::Model(Model::Frozen)),
        "Locked" => Some(Claim::Model(Model::Locked)),
        "Actor" => Some(Claim::Model(Model::Actor)),
        _ => None,
    };
    let Some(claim) = claim else {
        let args = match &last.arguments {
            syn::PathArguments::None => vec![],
            syn::PathArguments::AngleBracketed(ab) => ab
                .args
                .iter()
                .filter_map(|a| match a {
                    syn::GenericArgument::Type(t) => Some(parse_type(t)),
                    // A lifetime argument is dropped exactly as a lifetime
                    // parameter is; a const argument reaches `check`'s
                    // FR0073 as an arity mismatch, which is what it is.
                    _ => None,
                })
                .collect::<Result<Vec<_>>>()?,
            syn::PathArguments::Parenthesized(_) => bail!(
                "#[bridge] impl {}(...): a parenthesized path is a `Fn` trait \
                 sugar, not a bridged self type",
                last.ident
            ),
        };
        return Ok((last.ident.to_string(), None, args));
    };
    let syn::PathArguments::AngleBracketed(ab) = &last.arguments else {
        bail!(
            "#[bridge] impl {}<...>: a representation marker takes exactly one type \
             argument, the bridged type it names",
            last.ident
        );
    };
    let mut types = ab.args.iter().filter_map(|a| match a {
        syn::GenericArgument::Type(t) => Some(t),
        _ => None,
    });
    let (Some(inner), None) = (types.next(), types.next()) else {
        bail!(
            "#[bridge] impl {}<...>: a representation marker takes exactly one type \
             argument, the bridged type it names",
            last.ident
        );
    };
    let syn::Type::Path(inner_tp) = inner else {
        bail!(
            "#[bridge] impl {}<...>: the wrapped type must be a bare name, the \
             bridged type it names",
            last.ident
        );
    };
    let inner_last = inner_tp.path.segments.last().unwrap();
    if !matches!(inner_last.arguments, syn::PathArguments::None) {
        bail!(
            "#[bridge] impl {}<...>: the wrapped type must be a bare name, the \
             bridged type it names — not `{}<...>`",
            last.ident,
            inner_last.ident
        );
    }
    Ok((inner_last.ident.to_string(), Some(claim), vec![]))
}

fn parse_impl(
    im: &syn::ItemImpl,
    impl_attr: &BridgeAttr,
    module_path: &str,
    iface: &mut Interface,
) -> Result<()> {
    let syn::Type::Path(tp) = im.self_ty.as_ref() else {
        bail!("#[bridge] impl: unsupported self type");
    };
    let (parent, parent_claim, parent_args) = self_type_and_claim(tp)?;
    // The block's own parameters. `impl<T> Page<T>` writes its members in
    // terms of `T`, and `check`'s expansion binds it per instantiation — so
    // they are recorded, like a data declaration's, rather than refused.
    // A const parameter is refused here for the reason a generic
    // declaration's is (`expand_generics`): Dart's generics carry types only.
    // The block's own parameters. `impl<T> Page<T>` writes its members in
    // terms of `T`, and `check`'s expansion binds it per instantiation — so
    // they are recorded, like a data declaration's, rather than refused.
    //
    // Three shapes rustc rejects on the author's own source get no rule here,
    // because a codegen rule for them would be a second voice saying the same
    // thing later: a **const** block parameter and a **defaulted** one (E0747,
    // and "defaults for generic parameters are not allowed here"), and a block
    // whose self type is written unapplied (E0107). The last also has a
    // frustrate-specific answer already — `check_impl_targets` says a bare
    // template name is not a type — and that one is worth saying, because it
    // names the two spellings that do work.
    let block = generic_decl(&im.generics);
    // A **const** block parameter does get one, for frustrate's own reason
    // rather than rustc's: the arguments an instantiation is formed from are
    // types (`Instance::args`), so a const written on the self type names no
    // instantiation at all and is dropped where the self type is read. Without
    // this it reached `check` as an unknown *type* named `N`, and FR0003 then
    // told the author to declare it with `#[bridge(data)]`.
    if !block.consts.is_empty() {
        bail!(
            "FR0056: #[bridge] impl<const {0}> {parent}<…>: a const parameter on a \
             bridged impl block is not supported. An instantiation is formed from type \
             arguments only — the Dart class every instantiation is a type argument list \
             on carries types, and a const would have nowhere to go — so a member here \
             would be generated onto instantiations that cannot mention `{0}`. Declare a \
             non-generic newtype per length and bridge that",
            block.consts.join(", const ")
        );
    }
    // `#[bridge] impl Trait for Type`: the written methods bridge onto the
    // concrete opaque `Type` with static dispatch. The generated module
    // cannot assume the trait is in scope,
    // so the invocation is fully-qualified UFCS — which needs a path that
    // resolves from anywhere in the crate.
    let trait_path = match &im.trait_ {
        None => None,
        Some((Some(_), path, _)) => bail!(
            "#[bridge] impl !{} for {parent}: negative impls have no methods to bridge",
            path.segments.last().unwrap().ident
        ),
        Some((None, path, _)) => {
            if path.segments.iter().any(|s| !s.arguments.is_none()) {
                bail!(
                    "#[bridge] impl for `{parent}`: generic trait impls cannot be \
                     bridged (there is no single method surface to generate); wrap \
                     the calls you need in inherent methods"
                );
            }
            if !parent_args.is_empty() {
                bail!(
                    "#[bridge] impl {} for {parent}<...>: a trait impl on an \
                     instantiation of a generic type is not decided. An inherent \
                     `impl {parent}<...>` block bridges its members onto that \
                     instantiation; what a *trait* impl should mean there — one Dart \
                     interface per instantiation, or one on the template — has not \
                     been settled, so it is refused rather than guessed. Wrap the \
                     calls you need in inherent methods",
                    path.segments.last().unwrap().ident
                );
            }
            let segs: Vec<String> =
                path.segments.iter().map(|s| s.ident.to_string()).collect();
            match segs[0].as_str() {
                "self" | "super" => bail!(
                    "#[bridge] impl {} for {parent}: self/super-relative trait paths \
                     do not resolve from generated code; name the trait bare (same \
                     module) or through a crate::-rooted / external path",
                    segs.join("::")
                ),
                _ if segs.len() == 1 => Some(format!("{module_path}::{}", segs[0])),
                _ => Some(segs.join("::")),
            }
        }
    };
    for item in &im.items {
        let syn::ImplItem::Fn(m) = item else { continue };
        // `#[bridge(skip)]` on a method excludes just that method from an
        // annotated block. Checked before the inherit below, which would
        // otherwise hand it the block's options and bridge it anyway — an
        // annotation that does nothing, the defect FR0041 exists to prevent.
        if declares_skip(&m.attrs)? {
            continue;
        }
        // Methods inherit the impl-level attribute; a method-level
        // #[bridge(...)] overrides it. Unannotated methods in an annotated
        // impl block are bridged with defaults.
        let mut attr = bridge_attr(&m.attrs)?.unwrap_or(BridgeAttr {
            sync: impl_attr.sync,
            data: false,
            model: None,
            on_contention: impl_attr.on_contention,
            bytes: None,
            no_eq: false,
            // Neither is inherited: `dart_identifier` names one item, and
            // `getter` is a per-member shape (most members take parameters).
            getter: false,
            dart_identifier: None,
            // Both name a *type's* Dart class; a method has no half to name.
            data_dart_identifier: None,
            model_dart_identifier: None,
            dart_interface: false,
            // A direction belongs to a type declaration, and a method is not
            // one: `reject_inbound` refuses it on the block above.
            inbound: false,
            skip: false,
            web_runtime_fail: impl_attr.web_runtime_fail,
            native_only: false,
            no_block: false,
            cfg_gated: false,
        });
        // ...except these three, which are OR-merged rather than replaced.
        // Every other option is a policy the author may reasonably want to
        // vary per method; these are facts about whether the code exists
        // at all for a target, or about whether it may wait, and a method
        // cannot be more portable — or more blocking — than the block
        // containing it. Under replace semantics
        // `#[bridge(native_only)] impl Node { #[bridge(sync)] fn x(..) }`
        // would silently make `x` portable and put a call to a wasm-absent
        // item back into the web surface — the exact failure this flag is for.
        // `no_block` is the same shape: a method-level `#[bridge(sync)]` must
        // not silently shed the block's claim, or the claim would cover less
        // than it reads as covering.
        // A representation keyword on the *method* was the one place this rule
        // did not reach: `parse_fn` never reads `attr.model`, so it parsed,
        // was accepted, and vanished — the silent drop the rule exists to
        // prevent, one call site short.
        reject_representation(attr.data, attr.model, "method", &m.sig.ident.to_string())?;
        attr.native_only |= impl_attr.native_only;
        attr.no_block |= impl_attr.no_block;
        attr.cfg_gated |= impl_attr.cfg_gated;
        let mut f = parse_fn(
            &m.sig,
            &attr,
            module_path,
            Some(parent.clone()),
            parent_claim,
            doc_lines(&m.attrs),
        )?;
        f.trait_impl = trait_path.clone();
        // `Self` has landed as the bare name (`substitute_self`); on an applied
        // block it is the self type *as written*, which is that name applied to
        // the block's arguments — the same move `apply_own_params` makes for a
        // generic declaration's own fields, and for the same reason: inside
        // `impl<T> Page<T>` the bare `Page` is not a type in Rust either
        // (E0107), so nothing else can have meant it.
        if !parent_args.is_empty() {
            for ty in f.signature_types_mut() {
                apply_self_args(ty, &parent, &parent_args);
            }
        }
        f.parent_args = parent_args.clone();
        f.parent_generics = block.types.clone();
        iface.functions.push(f);
    }
    Ok(())
}

/// Rewrite every `Named(name)` into `name` applied to `args` — the `impl`
/// block's self type as written. [`apply_own_params`] is the same rewrite with
/// the arguments being the declaration's own parameters; see its doc for why
/// the bare name cannot have meant anything else.
fn apply_self_args(ty: &mut Type, name: &str, args: &[Type]) {
    if args.is_empty() {
        return;
    }
    match ty {
        Type::Named(n) if n == name => *ty = Type::App(name.to_string(), args.to_vec()),
        Type::Claimed(_, t)
        | Type::List(t, _)
        | Type::Set(t, _)
        | Type::Option(t)
        | Type::Array(t, _)
        | Type::Boxed(t)
        | Type::Ref { inner: t, .. } => apply_self_args(t, name, args),
        Type::Map(k, v, _) => {
            apply_self_args(k, name, args);
            apply_self_args(v, name, args);
        }
        Type::Tuple(ts) | Type::App(_, ts) => {
            for t in ts {
                apply_self_args(t, name, args);
            }
        }
        Type::DartObject(spec) => {
            for t in [spec.item.as_mut(), spec.ret.as_mut(), spec.err.as_mut()]
                .into_iter()
                .flatten()
            {
                apply_self_args(t, name, args);
            }
        }
        _ => {}
    }
}

/// A bridged trait declaration: the trait's own
/// method signatures are the bridged surface; values cross as
/// `Box<dyn Trait>` handles. Nothing about concrete impls is parsed.
fn parse_trait(
    t: &syn::ItemTrait,
    attr: &BridgeAttr,
    module_path: &str,
    iface: &mut Interface,
) -> Result<()> {
    let name = t.ident.to_string();
    if attr.bytes.is_some() {
        bail!("trait `{name}`: bytes(...) is only supported on structs");
    }
    // Guaranteed `Some` here: the `Item::Trait` arm already called
    // `require_representation` (a representation is present) and
    // `reject_data_on_trait` (it is not `data`), so the only representation
    // left is a concurrency model.
    let model = attr
        .model
        .expect("caller already required exactly one concurrency model");
    if !t.generics.params.is_empty() {
        bail!(
            "trait `{name}`: generic traits cannot be bridged (there is no single \
             `dyn {name}` to hand to Dart); bridge a concrete instantiation behind \
             a newtype instead"
        );
    }
    let supertraits = t
        .supertraits
        .iter()
        .filter_map(|b| match b {
            syn::TypeParamBound::Trait(tb) => {
                Some(tb.path.segments.last().unwrap().ident.to_string())
            }
            _ => None,
        })
        .collect();
    for item in &t.items {
        let m = match item {
            syn::TraitItem::Fn(m) => m,
            syn::TraitItem::Type(a) => bail!(
                "trait `{name}`: associated type `{}` — traits with associated \
                 types are not dyn-compatible and cannot be bridged",
                a.ident
            ),
            syn::TraitItem::Const(c) => bail!(
                "trait `{name}`: associated const `{}` is not a callable surface; \
                 expose it through a method",
                c.ident
            ),
            _ => continue,
        };
        // `#[bridge(skip)]` excludes one method from the bridged surface —
        // checked first, and before the syntactic rules below, because a
        // method the author has declared out of the bridge should not be held
        // to the bridge's dyn-compatibility rules. Same reasoning as
        // `parse_impl`: without this, `bridge_attr`'s `None` would make the
        // method inherit the trait's options and be bridged regardless.
        if declares_skip(&m.attrs)? {
            continue;
        }
        // Dyn-compatibility, enforced where the source facts live (the
        // checker owns the IR-visible rules — receiver-less fns, Self
        // positions it can see; these two are purely syntactic).
        if !m.sig.generics.params.is_empty() {
            bail!(
                "trait `{name}`, method `{}`: generic methods are not \
                 dyn-dispatchable; a bridged trait is always called through \
                 `dyn {name}`",
                m.sig.ident
            );
        }
        if sig_mentions_self_type(&m.sig) {
            bail!(
                "trait `{name}`, method `{}`: `Self` in a bridged trait method is \
                 not dyn-dispatchable. To return a trait object, write \
                 `Box<dyn {name}>` explicitly",
                m.sig.ident
            );
        }
        // Methods inherit the trait-level attribute; a method-level
        // #[bridge(...)] overrides it — the same rule as impl blocks.
        let mut mattr = bridge_attr(&m.attrs)?.unwrap_or(BridgeAttr {
            sync: attr.sync,
            data: false,
            model: None,
            on_contention: attr.on_contention,
            bytes: None,
            no_eq: false,
            // Not inherited — see the impl-block literal.
            getter: false,
            dart_identifier: None,
            // Both name a *type's* Dart class; a method has no half to name.
            data_dart_identifier: None,
            model_dart_identifier: None,
            dart_interface: false,
            // A direction belongs to a type declaration, and a method is not
            // one: `reject_inbound` refuses it on the block above.
            inbound: false,
            skip: false,
            web_runtime_fail: attr.web_runtime_fail,
            native_only: false,
            no_block: false,
            cfg_gated: false,
        });
        // See `parse_impl`: the keyword is refused on the member, not dropped.
        reject_representation(mattr.data, mattr.model, "method", &m.sig.ident.to_string())?;
        // OR-merged, not replaced — see `parse_impl` for why.
        mattr.native_only |= attr.native_only;
        mattr.no_block |= attr.no_block;
        mattr.cfg_gated |= attr.cfg_gated;
        // Traits have no self-type wrapper position (they are always a
        // model, never `data` — FR0061 already refuses that), so this is
        // never `Some`.
        let f = parse_fn(
            &m.sig,
            &mattr,
            module_path,
            Some(name.clone()),
            None,
            doc_lines(&m.attrs),
        )?;
        iface.functions.push(f);
    }
    let (_, model_id) = half_dart_identifiers(attr, "trait", &name)?;
    iface.opaques.push(OpaqueDecl {
        name,
        module_path: module_path.to_string(),
        model,
        dyn_trait: true,
        supertraits,
        native_only: attr.native_only,
        cfg_gated: attr.cfg_gated,
        dart_identifier: model_id,
        docs: doc_lines(&t.attrs),
    });
    Ok(())
}

/// True when the signature mentions the `Self` type in any parameter or the
/// return type (receivers excluded — `&self` is the point of a method).
fn sig_mentions_self_type(sig: &syn::Signature) -> bool {
    sig.inputs
        .iter()
        .filter_map(|i| match i {
            syn::FnArg::Typed(pt) => Some(pt.ty.as_ref()),
            syn::FnArg::Receiver(_) => None,
        })
        .any(ty_mentions_self_type)
        || match &sig.output {
            syn::ReturnType::Type(_, t) => ty_mentions_self_type(t),
            syn::ReturnType::Default => false,
        }
}

/// True when `ty` names `Self` anywhere inside it.
fn ty_mentions_self_type(ty: &syn::Type) -> bool {
    fn ty_mentions(ty: &syn::Type) -> bool {
        match ty {
            syn::Type::Path(tp) => tp.path.segments.iter().any(|seg| {
                seg.ident == "Self"
                    || match &seg.arguments {
                        syn::PathArguments::AngleBracketed(ab) => ab.args.iter().any(|a| {
                            matches!(a, syn::GenericArgument::Type(t) if ty_mentions(t))
                        }),
                        _ => false,
                    }
            }),
            syn::Type::Reference(r) => ty_mentions(&r.elem),
            syn::Type::Paren(p) => ty_mentions(&p.elem),
            syn::Type::Group(g) => ty_mentions(&g.elem),
            syn::Type::Tuple(t) => t.elems.iter().any(ty_mentions),
            syn::Type::Array(a) => ty_mentions(&a.elem),
            syn::Type::Slice(s) => ty_mentions(&s.elem),
            syn::Type::TraitObject(_) => false,
            _ => false,
        }
    }
    ty_mentions(ty)
}

/// Whether an explicitly typed receiver is written `self: Box<Self>`.
///
/// Syntactic, and deliberately so: this is asking what the author wrote, not
/// what the type resolves to. `Box` under an alias, or a `Box` shadowed by
/// something else in the user's crate, reads as `Typed` here and is refused —
/// the strict side, and the only one this pass can defend.
fn is_box_self(ty: &syn::Type) -> bool {
    let syn::Type::Path(p) = ty else { return false };
    let Some(seg) = p.path.segments.last() else { return false };
    if seg.ident != "Box" {
        return false;
    }
    let syn::PathArguments::AngleBracketed(args) = &seg.arguments else {
        return false;
    };
    matches!(
        args.args.iter().collect::<Vec<_>>().as_slice(),
        [syn::GenericArgument::Type(syn::Type::Path(inner))] if inner.path.is_ident("Self")
    )
}

fn parse_fn(
    sig: &syn::Signature,
    attr: &BridgeAttr,
    module_path: &str,
    parent: Option<String>,
    parent_claim: Option<Claim>,
    docs: Vec<String>,
) -> Result<Function> {
    if attr.bytes.is_some() {
        bail!(
            "fn `{}`: bytes(...) applies to type declarations, not functions",
            sig.ident
        );
    }
    // Rust `async fn` is accepted: the body evaluates to a Future that the
    // generated glue hands to the cooperative executor. Never
    // `#[bridge(sync)]` — a sync member runs on the caller and returns a plain
    // value, so it cannot await — and never `requires_native`: the executor
    // runs on every config.
    let rust_async = sig.asyncness.is_some();
    if rust_async && attr.sync {
        bail!(
            "fn `{}`: `#[bridge(sync)]` cannot be combined with `async fn` — a sync \
             member runs on the caller and returns a plain value, so it cannot await. \
             Remove `sync`; an async fn runs on the frustrate pool and Dart awaits the \
             Future (its future is driven to completion there).",
            sig.ident
        );
    }
    let generics = generic_params(&sig.generics);
    let mut receiver = None;
    let mut params = vec![];
    for input in &sig.inputs {
        match input {
            syn::FnArg::Receiver(r) => {
                // Recorded, not judged. `&self`/`&mut self` are the two
                // borrowed shorthands and `self`/`mut self` the consuming one.
                //
                // An explicitly typed receiver is read exactly far enough to
                // tell `self: Box<Self>` — a consume, and a trait's only
                // object-safe one — from every other spelling, which stays
                // `Typed` and is refused (FR0057). Reading further would mean
                // deciding what `self: Rc<Self>` or `self: Pin<&mut Self>`
                // means to the crossing, and neither is decided.
                //
                // `syn` gives a shorthand receiver `colon_token: None` and a
                // `ty` it synthesized, so the type alone cannot tell the two
                // apart; the token is the fact.
                receiver = Some(match (&r.reference, r.colon_token.is_some()) {
                    (Some(_), _) if r.mutability.is_some() => Receiver::RefMut,
                    (Some(_), _) => Receiver::Ref,
                    (None, false) => Receiver::Value,
                    (None, true) if is_box_self(&r.ty) => Receiver::Boxed,
                    (None, true) => Receiver::Typed,
                });
            }
            syn::FnArg::Typed(pt) => {
                let syn::Pat::Ident(pi) = pt.pat.as_ref() else {
                    bail!("fn `{}`: unsupported parameter pattern", sig.ident);
                };
                // Dart-object handles are ordinary parameters now: they are
                // data (an id) on the wire, so they need no hoisting and no
                // separate positional bookkeeping.
                let (borrow, ty, unsized_borrow) = match pt.ty.as_ref() {
                    syn::Type::Reference(r) => {
                        let inner = parse_type(&r.elem)?;
                        if inner.as_dart_object().is_some() {
                            bail!(
                                "fn `{}`, parameter `{}`: Dart-object handles are passed \
                                 by value (the function owns its end of the channel)",
                                sig.ident,
                                pi.ident
                            );
                        }
                        let b = if r.mutability.is_some() {
                            Borrow::RefMut
                        } else {
                            Borrow::Ref
                        };
                        // `&[u8]` and `&str` are the unsized spellings; `&Vec<u8>`
                        // and `&String` reach the same `Type` through a path.
                        // Recorded here because this is the last point at which
                        // the two are distinguishable (see `Param`).
                        let unsized_borrow = is_unsized_borrow(&r.elem);
                        (b, inner, unsized_borrow)
                    }
                    other => (Borrow::Value, parse_type(other)?, false),
                };
                // The lifetime is not dropped any more; the checker rejects it
                // (FR0044), so it has to survive parsing to be named. Read from
                // the whole parameter type rather than from a top-level `&`,
                // because a nested borrow conjures its reference the same way:
                // `Vec<&'static Doc>` would unify `'static` with the unbound
                // lifetime `handle::confined_ref` hands back.
                let ref_lifetime = named_lifetime(pt.ty.as_ref());
                params.push(Param {
                    name: pi.ident.to_string(),
                    ty,
                    borrow,
                    unsized_borrow,
                    ref_lifetime,
                });
            }
        }
    }
    // `Self` in a **parameter**, at any depth, is the block's self type as
    // written — exactly what it is in a return. One reading, every position.
    let scope = self_scope(parent.as_deref(), parent_claim);
    for p in &mut params {
        substitute_self(
            &mut p.ty,
            scope,
            &format!("fn `{}`, parameter `{}`: `Self`", sig.ident, p.name),
        )?;
    }
    let mut ret_borrow = false;
    let (ret, fallible, err, deferred) =
        parse_return(&sig.output, &sig.ident, scope, &mut ret_borrow)?;
    Ok(Function {
        fn_id: 0,
        name: sig.ident.to_string(),
        module_path: module_path.to_string(),
        parent,
        parent_claim,
        // Set by `parse_impl`, which is where the block's self type is read.
        parent_args: vec![],
        parent_generics: vec![],
        // Assigned by `check`, from the declaration and the claim above.
        parent_repr: None,
        trait_impl: None,
        receiver,
        generics,
        params,
        ret,
        err,
        fallible,
        exec: if attr.sync { Exec::Sync } else { Exec::Async },
        on_contention: attr.on_contention,
        // Decided by the checker, not here: recognising a constructor means
        // comparing the return type against the parent, and at parse time the
        // return may still be a representation marker (`-> Actor<Job>`) that
        // only `resolve_type` erases. Matching the bare spelling alone made the
        // marker form lose constructor-ness silently.
        is_constructor: false,
        is_actor_drop: false,
        requires_native: false,
        native_only: attr.native_only,
        no_block: attr.no_block,
        cfg_gated: attr.cfg_gated,
        web_runtime_fail: attr.web_runtime_fail,
        rust_async,
        deferred,
        ret_borrow,
        getter: attr.getter,
        dart_identifier: attr.dart_identifier.clone(),
        docs,
    })
}

/// `true` when `ty` is the unit tuple `()`.
fn is_unit(ty: &syn::Type) -> bool {
    matches!(ty, syn::Type::Tuple(t) if t.elems.is_empty())
}

fn parse_return(
    output: &syn::ReturnType,
    fn_name: &syn::Ident,
    parent: SelfScope<'_>,
    borrow: &mut bool,
) -> Result<(Option<Type>, bool, Option<Type>, bool)> {
    let syn::ReturnType::Type(_, ty) = output else {
        return Ok((None, false, None, false));
    };
    // Deferred<T>: the actor hand-off wrapper, outermost only — like Result,
    // whose position rule it shares.
    // Unwrapped here so `ret`/`fallible`/`err` describe the inner type and
    // every downstream value rule sees through it; whether this *member* may
    // be deferred is the checker's question (FR0038/FR0039), not the
    // parser's. A nested `Deferred` inside the unwrapped type falls through
    // to `parse_type`'s rejection arm.
    if let syn::Type::Path(tp) = ty.as_ref() {
        let last = tp.path.segments.last().unwrap();
        if last.ident == "Deferred" {
            let bad_args = || {
                anyhow::anyhow!(
                    "fn `{fn_name}`: Deferred takes exactly one type argument \
                     (what the completion resolves to — `Deferred<T>` or \
                     `Deferred<Result<T, E>>`)"
                )
            };
            let syn::PathArguments::AngleBracketed(args) = &last.arguments else {
                return Err(bad_args());
            };
            let mut tys = args.args.iter().filter_map(|a| match a {
                syn::GenericArgument::Type(t) => Some(t),
                _ => None,
            });
            let (inner, extra) = (tys.next(), tys.next());
            let (Some(inner), None) = (inner, extra) else {
                return Err(bad_args());
            };
            let (ret, fallible, err) = parse_return_ty(inner, fn_name, parent, borrow)?;
            return Ok((ret, fallible, err, true));
        }
    }
    let (ret, fallible, err) = parse_return_ty(ty, fn_name, parent, borrow)?;
    Ok((ret, fallible, err, false))
}

/// The body of [`parse_return`] under any `Deferred` wrapper:
/// `Result<T> / Result<T, E> / anyhow::Result<T>`, `Self`, or a plain type.
fn parse_return_ty(
    ty: &syn::Type,
    fn_name: &syn::Ident,
    parent: SelfScope<'_>,
    borrow: &mut bool,
) -> Result<(Option<Type>, bool, Option<Type>)> {
    if let syn::Type::Path(tp) = ty {
        let last = tp.path.segments.last().unwrap();
        if last.ident == "Result" {
            let syn::PathArguments::AngleBracketed(args) = &last.arguments else {
                bail!("fn `{fn_name}`: Result must have type arguments");
            };
            let Some(syn::GenericArgument::Type(ok_ty)) = args.args.first() else {
                bail!("fn `{fn_name}`: Result must have a type as its first argument");
            };
            let (ok_ty, ok_borrow) = peel_ret_borrow(ok_ty, fn_name)?;
            *borrow = ok_borrow;
            let mut ok = parse_ret_type(ok_ty)?;
            let mut err = parse_error_type(args.args.iter().nth(1), fn_name)?;
            let ctx = format!("fn `{fn_name}`: a Self return");
            substitute_self_opt(ok.as_mut(), parent, &ctx)?;
            substitute_self_opt(err.as_mut(), parent, &ctx)?;
            return Ok((ok, true, err));
        }
    }
    let (ty, ret_borrow) = peel_ret_borrow(ty, fn_name)?;
    *borrow = ret_borrow;
    let mut ret = parse_ret_type(ty)?;
    substitute_self_opt(ret.as_mut(), parent, &format!("fn `{fn_name}`: a Self return"))?;
    Ok((ret, false, None))
}

/// [`substitute_self`] over a type that may not be there — a unit return, an
/// untyped error, a mirror with no item type.
fn substitute_self_opt(ty: Option<&mut Type>, scope: SelfScope<'_>, ctx: &str) -> Result<()> {
    match ty {
        Some(ty) => substitute_self(ty, scope, ctx),
        None => Ok(()),
    }
}

/// What `Self` names inside an item: the type's name, and the representation
/// marker the `impl` block wrote around it (`None` on a declaration's own
/// fields, and on a block that wrote a bare name).
///
/// `None` altogether only for a free function, where `Self` names nothing.
type SelfScope<'a> = Option<(&'a str, Option<Claim>)>;

/// The [`SelfScope`] of an `impl` block, from the two halves `parse_impl`
/// already read off its self type.
fn self_scope<'a>(parent: Option<&'a str>, claim: Option<Claim>) -> SelfScope<'a> {
    parent.map(|p| (p, claim))
}

/// Replace every `Self` in `ty` with the type the enclosing item is about, at
/// any depth.
///
/// rustc reads `Self` as that type everywhere it may be written — the impl's
/// type in a member signature, the declaring type in a struct or enum field
/// (measured: `struct Tree { children: Vec<Self> }` compiles). This makes the
/// parsed IR agree. Before it, only a **root** `-> Self` and
/// `-> Result<Self, E>` were substituted, so `-> Option<Self>`, `-> Vec<Self>`
/// and every `Self` in a field reached the checker as an unknown type
/// literally named `Self` (FR0003). Nothing had decided that the depth should
/// matter, so it was a gap rather than a rule.
///
/// **The impl block's self type as written, marker included.** In
/// `impl Locked<Doc>` a `Self` is the handle half and in `impl Data<Doc>` the
/// value half; on a type with one representation it is the type. That is sound
/// because every marker is an identity alias (`pub type Locked<T> = T;`), so
/// rustc and frustrate agree about the *Rust* type and differ only about which
/// Dart class — which the block has already declared. Every position takes this
/// reading: a parameter at any depth, a return at any depth, and a `Self` in a
/// declaration's own field (which writes no marker, so a dual type's field
/// names both halves and is FR0067, as the bare name is).
///
/// Consequences fall out of the rules that already exist rather than being
/// special-cased: a by-value `Self` parameter on a handle half is a consume
/// (`Consumed<Doc>` in Dart), exactly as writing the type name is, and on a
/// data half it is an ordinary by-value data parameter.
///
/// `Self` inside a bridged **trait** method keeps its own refusal
/// (`parse_trait`): a bridged trait is called through `dyn Trait`, and a `Self`
/// position is not dyn-compatible. That mirrors rustc rather than adding a rule.
/// `Self` inside a **generic** declaration's own field means the declaration
/// applied to its own parameters — `struct Chain<T> { next: Option<Box<Self>> }`
/// is `Chain<T>`, not `Chain`, which is not a type at all (rustc's E0107).
///
/// Runs after [`substitute_self`], which has already turned every `Self` into
/// the declaration's bare name; this applies the parameters to it. A field that
/// writes the bare name outright reaches the same place, which is right: inside
/// its own generic declaration the bare name is not valid Rust either.
fn apply_own_params(ty: &mut Type, name: &str, params: &[String]) {
    let args: Vec<Type> = params.iter().map(|p| Type::Named(p.clone())).collect();
    apply_self_args(ty, name, &args);
}

fn substitute_self(ty: &mut Type, scope: SelfScope<'_>, ctx: &str) -> Result<()> {
    match ty {
        Type::Named(n) if n == "Self" => {
            let Some((name, claim)) = scope else {
                bail!("{ctx} outside an impl block names nothing — write the type");
            };
            *ty = match claim {
                Some(c) => Type::Claimed(c, Box::new(Type::Named(name.to_string()))),
                None => Type::Named(name.to_string()),
            };
        }
        Type::Claimed(_, t)
        | Type::List(t, _)
        | Type::Set(t, _)
        | Type::Option(t)
        | Type::Array(t, _)
        | Type::Boxed(t)
        // A nested borrow's referent is an ordinary type, so `Vec<&Self>` and
        // `Option<&Self>` name the impl's type there like every other depth.
        | Type::Ref { inner: t, .. } => {
            substitute_self(t, scope, ctx)?;
        }
        Type::Map(k, v, _) => {
            substitute_self(k, scope, ctx)?;
            substitute_self(v, scope, ctx)?;
        }
        // A type argument is an ordinary type, so `-> Page<Self>` names the
        // impl's own type there, at every depth, like every other position.
        Type::Tuple(ts) | Type::App(_, ts) => {
            for t in ts {
                substitute_self(t, scope, ctx)?;
            }
        }
        // A mirror's item and result types are ordinary types, so a `Self`
        // written in one means the impl's type there too. (Such a return is
        // FR0031 later — a Dart-object handle is argument-only — but the
        // substitution is about what `Self` names, not about where the type
        // may sit.)
        Type::DartObject(spec) => {
            substitute_self_opt(spec.item.as_mut(), scope, ctx)?;
            substitute_self_opt(spec.ret.as_mut(), scope, ctx)?;
            substitute_self_opt(spec.err.as_mut(), scope, ctx)?;
        }
        _ => {}
    }
    Ok(())
}

/// The `E` of a `Result<T, E>`, when it is one that crosses as a value.
///
/// Four shapes stay on the message path and return `None`:
///
/// - **absent** — `anyhow::Result<T>`, whose `E` is `anyhow::Error`;
/// - **`String`** — the declared untyped escape hatch;
/// - **`anyhow::Error` written out** — the same type as the first case, so it
///   would be strange for the spelling to change the contract;
/// - **`Box<dyn Error>`** — Rust's other erased error, which offers a caller
///   exactly what anyhow's does (see [`is_message_error`]).
///
/// Anything else is taken as a claim that the error is a value. Whether it is
/// one the interface can carry is the checker's question (FR0035), not the
/// parser's: the parser cannot see the type declarations.
fn parse_error_type(arg: Option<&syn::GenericArgument>, fn_name: &syn::Ident) -> Result<Option<Type>> {
    let Some(arg) = arg else { return Ok(None) };
    let syn::GenericArgument::Type(ty) = arg else {
        bail!("fn `{fn_name}`: the error of a Result must be a type");
    };
    if is_message_error(ty) {
        return Ok(None);
    }
    Ok(Some(parse_type(ty).with_context(|| {
        format!(
            "fn `{fn_name}`: unsupported error type in Result. A typed error must be a \
             bridged struct or enum; to cross as a message instead, use \
             `Result<T, String>` or `anyhow::Result<T>`"
        )
    })?))
}

/// A return type written as a top-level borrow (`-> &Point`, `-> &str`).
///
/// Legal because the value is **copied into the response**: the reference is
/// read while it is still alive — the encode runs inside the same scope as the
/// call, with the receiver's lock guard or `Arc` clone still held — and nothing
/// borrowed survives it. A borrow anywhere deeper is still refused by
/// `parse_type`: only the root has a scope the emitter can reason about.
fn peel_ret_borrow<'a>(ty: &'a syn::Type, fn_name: &syn::Ident) -> Result<(&'a syn::Type, bool)> {
    let syn::Type::Reference(r) = ty else {
        return Ok((ty, false));
    };
    if r.mutability.is_some() {
        bail!(
            "fn `{fn_name}`: `&mut` in return position has no meaning across the \
             bridge — the value is copied into the response, so nothing can be \
             written back through it. Return the value, or `&T`"
        );
    }
    Ok((&r.elem, true))
}

/// A return type, where the empty tuple is the unit return rather than a
/// value. `Self` is left as the ordinary [`Type::Named`] the path parser
/// produces; [`substitute_self`] resolves it against the impl, at every depth.
fn parse_ret_type(ty: &syn::Type) -> Result<Option<Type>> {
    if let syn::Type::Tuple(t) = ty {
        if t.elems.is_empty() {
            return Ok(None);
        }
    }
    Ok(Some(parse_type(ty)?))
}

/// Whether `elem` — the referent of a `&` — is one of the **unsized**
/// spellings, `[T]` or `str`, rather than an owned container reached through a
/// path (`Vec<T>`, `String`).
///
/// Both spellings parse to the same [`Type`], so this is the last point at
/// which they are distinguishable; see [`Param::unsized_borrow`] and
/// [`Type::Ref`], which record it at their two levels.
fn is_unsized_borrow(elem: &syn::Type) -> bool {
    match elem {
        syn::Type::Slice(_) => true,
        syn::Type::Path(tp) => {
            tp.qself.is_none() && tp.path.segments.last().is_some_and(|s| s.ident == "str")
        }
        _ => false,
    }
}

/// The first lifetime the author wrote anywhere in `ty`, elided ones excluded.
///
/// A borrow the glue conjures — from the request buffer, or from
/// `handle::confined_ref` and its siblings — has an unbound lifetime, so a
/// name the author chose would unify with it and compile a dangling reference.
/// The rule is FR0044's and it is stated there; this is only where the name is
/// found, and it looks at the whole parameter type because a nested borrow
/// (`Vec<&'static Doc>`) is conjured exactly as a top-level one is.
///
/// `'_` counts: it is written, it is not an elision, and there is no lifetime
/// here a caller could usefully name.
fn named_lifetime(ty: &syn::Type) -> Option<String> {
    match ty {
        syn::Type::Reference(r) => r
            .lifetime
            .as_ref()
            .map(|l| l.to_string())
            .or_else(|| named_lifetime(&r.elem)),
        syn::Type::Slice(s) => named_lifetime(&s.elem),
        syn::Type::Array(a) => named_lifetime(&a.elem),
        syn::Type::Paren(p) => named_lifetime(&p.elem),
        syn::Type::Tuple(t) => t.elems.iter().find_map(named_lifetime),
        syn::Type::Path(tp) => tp.path.segments.iter().find_map(|seg| {
            let syn::PathArguments::AngleBracketed(ab) = &seg.arguments else {
                return None;
            };
            ab.args.iter().find_map(|a| match a {
                syn::GenericArgument::Lifetime(l) => Some(l.to_string()),
                syn::GenericArgument::Type(t) => named_lifetime(t),
                _ => None,
            })
        }),
        _ => None,
    }
}

fn parse_type(ty: &syn::Type) -> Result<Type> {
    match ty {
        syn::Type::Path(tp) => parse_path_type(tp),
        // `[T; N]`. The length has to be a literal because it is part of the
        // wire: a const expression would need evaluating, and codegen reads
        // syntax. `[u8; N]` keeps its own IR variant and its memcpy codec; see
        // [`Type::ByteArray`]. Both write N raw elements with no prefix.
        syn::Type::Array(arr) => {
            let elem = parse_type(&arr.elem)?;
            let syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Int(n),
                ..
            }) = &arr.len
            else {
                bail!(
                    "array length must be an integer literal — it is part of the wire, \
                     and codegen reads the signature rather than evaluating it"
                );
            };
            let n: usize = n.base10_parse()?;
            Ok(if elem == Type::U8 {
                Type::ByteArray(n)
            } else {
                Type::Array(Box::new(elem), n)
            })
        }
        // A reference **inside** a type — `Vec<&Doc>`, `Option<&str>`,
        // `(&Doc, i64)`. Recorded, not judged: where a borrow may sit is the
        // checker's rule (FR0077), and the lifetime the checker names comes
        // from `Param::ref_lifetime`, which `parse_fn` fills from the whole
        // parameter type rather than from the top-level `&` alone.
        syn::Type::Reference(r) => Ok(Type::Ref {
            inner: Box::new(parse_type(&r.elem)?),
            mutable: r.mutability.is_some(),
            unsized_borrow: is_unsized_borrow(&r.elem),
        }),
        // `&dyn Trait` params reach here through the Reference arm above the
        // call site; a trait object anywhere else needs its Box.
        syn::Type::TraitObject(to) => trait_object_name(to),
        // `&(dyn Trait + Send)` — the multi-bound form parses parenthesized.
        syn::Type::Paren(p) => parse_type(&p.elem),
        // A slice, only ever reachable through the top-level reference (`&[T]`)
        // — it is unsized elsewhere and would not compile in the user crate.
        //
        // `&[u8]` is the byte borrow, and on a sync member it borrows the
        // request buffer literally (`read_bytes_borrowed`), which is why
        // `Param::unsized_borrow` records this spelling: `&Vec<u8>` and
        // `&String` reach the same `Type` but need an owned container to point
        // at. Every other element type reaches the ordinary list codec, which
        // has to decode element by element into a `Vec` the glue owns — so the
        // slice is a borrow of that local, and `&Vec<T>` coerces to it.
        syn::Type::Slice(s) => {
            let elem = parse_type(&s.elem)?;
            if elem == Type::U8 {
                Ok(Type::Bytes)
            } else {
                Ok(Type::List(Box::new(elem), SeqKind::Vec))
            }
        }
        // A tuple `(A, B, …)` maps to a Dart positional record. The empty
        // tuple `()` is the unit return, handled in `parse_ret_type` before we
        // ever reach here; a 1-tuple is not a distinct Rust type worth its own
        // record, so tuples start at arity 2.
        syn::Type::Tuple(t) if t.elems.len() == 1 => bail!(
            "1-tuples `({},)` are not supported; drop the tuple and pass the value directly",
            type_to_string(&t.elems[0])
        ),
        syn::Type::Tuple(t) if !t.elems.is_empty() => {
            let elems = t.elems.iter().map(parse_type).collect::<Result<Vec<_>>>()?;
            Ok(Type::Tuple(elems))
        }
        other => bail!("unsupported type: {}", type_to_string(other)),
    }
}

/// `dyn Trait` (with optional marker bounds) as a named type. The name
/// resolves like any other: to a declared bridged trait or an FR0003.
fn trait_object_name(to: &syn::TypeTraitObject) -> Result<Type> {
    let mut traits = to.bounds.iter().filter_map(|b| match b {
        syn::TypeParamBound::Trait(tb) => Some(tb.path.segments.last().unwrap().ident.clone()),
        _ => None,
    });
    let first = traits
        .next()
        .context("dyn type with no trait bound")?
        .to_string();
    // Marker supertraits repeated at the use site (`dyn Store + Send`) are
    // legal Rust but redundant here — the declaration carries the bounds.
    if let Some(extra) = traits.find(|t| t != "Send" && t != "Sync") {
        bail!(
            "`dyn {first} + {extra}`: multi-trait objects are not supported; \
             declare one bridged trait that carries the full surface (marker \
             bounds Send/Sync are allowed and ignored — declare them as \
             supertraits instead)"
        );
    }
    Ok(Type::Named(first))
}

fn parse_path_type(tp: &syn::TypePath) -> Result<Type> {
    let last = tp.path.segments.last().unwrap();
    let ident = last.ident.to_string();
    let args = generic_args(last)?;
    let t = match (ident.as_str(), args.as_slice()) {
        ("bool", []) => Type::Bool,
        ("i8", []) => Type::I8,
        ("i16", []) => Type::I16,
        ("i32", []) => Type::I32,
        ("i64", []) => Type::I64,
        ("u8", []) => Type::U8,
        ("u16", []) => Type::U16,
        ("u32", []) => Type::U32,
        ("u64", []) => Type::U64,
        ("f32", []) => Type::F32,
        ("f64", []) => Type::F64,
        ("usize", []) => Type::Usize,
        ("isize", []) => Type::Isize,
        // i128/u128 cross as Dart BigInt on the same big-integer codec as u64,
        // extended to 16 bytes.
        ("i128", []) => Type::I128,
        ("u128", []) => Type::U128,
        // Rust `char` is a Unicode scalar value; it crosses as a one-character
        // Dart String over the u32 codepoint wire.
        ("char", []) => Type::Char,
        ("String", []) | ("str", []) => Type::String,
        // Time types cross as i64 microseconds — a span for `Duration`, micros
        // since the Unix epoch (UTC) for an instant. One wire and one Dart type
        // per shape; the *Rust peer* is whichever the author wrote, so a project
        // on chrono or `time` is not pushed through `#[bridge(bytes(...))]`.
        //
        // `Duration` is the one ambiguous spelling — std, chrono and `time` all
        // have one, and only std's is unsigned — so it is resolved by the path's
        // *root*, not by its last segment: `std::time::Duration` roots at `std`,
        // `time::Duration` at `time`. A bare `Duration` stays std, which is what
        // it has always meant and what every bridge file in this repo relies on.
        // `TimeDelta`/`OffsetDateTime`/`SystemTime` are each unique to one
        // crate, so they need no qualification to be unambiguous.
        ("Duration", []) => match path_root(tp).as_deref() {
            None | Some("std") | Some("core") => Type::Duration(DurationPeer::Std),
            Some("chrono") => Type::Duration(DurationPeer::ChronoTimeDelta),
            Some("time") => Type::Duration(DurationPeer::Time),
            // Any other root — `crate::util::Duration`, `tokio::time::Duration`,
            // a renamed dependency — is a `Duration` this parser cannot place.
            // Defaulting it to std would silently give it std's *unsigned*
            // contract, which is the surprise `is_dart_path` exists to prevent,
            // applied to a semantic difference rather than just a name.
            Some(other) => bail!(
                "FR0045: `{other}::…::Duration` is not a recognized peer type. The \
                 bridge cannot tell which `Duration` this is, and the three it knows \
                 disagree about sign (`std::time::Duration` is unsigned; \
                 `chrono::Duration` and `time::Duration` are signed). If `{other}` \
                 re-exports one of them — `tokio::time::Duration` is `std`'s — write \
                 the original path in the signature; the re-export is a name, and \
                 codegen has only the name to go on. Otherwise wrap the type and \
                 cross it with #[bridge(bytes(...))]."
            ),
        },
        // Unique to one crate each, so no qualification is needed to be
        // unambiguous — but a *foreign* root still means some other type of
        // that name, which falls through to the ordinary named-type path
        // (and so to FR0003) rather than being captured here.
        ("TimeDelta", []) if time_root(tp, &["chrono"]) => {
            Type::Duration(DurationPeer::ChronoTimeDelta)
        }
        ("SystemTime", []) if time_root(tp, &["std", "core"]) => {
            Type::SystemTime(InstantPeer::Std)
        }
        ("OffsetDateTime", []) if time_root(tp, &["time"]) => {
            Type::SystemTime(InstantPeer::TimeOffsetDateTime)
        }
        // `chrono::DateTime<Tz>`. Accepted unqualified because the type
        // argument already makes the spelling unmistakable — and because a
        // *bare* `DateTime` with no argument is left alone (it falls through to
        // `Type::Named`, so a user's own bridged `DateTime` still resolves to
        // their type). Only `Utc` is a peer: the wire carries an instant and no
        // zone, so a zone-carrying peer could not be reconstructed faithfully.
        ("DateTime", [tz]) => match tz_name(tz).as_deref() {
            Some("Utc") => Type::SystemTime(InstantPeer::ChronoUtc),
            // Rejected, not deferred. The wire carries an instant and no zone,
            // so `{other}` could not be reconstructed on the far side — and
            // chrono's `Local` in particular is *silently wrong* on this
            // project's web target: without wasm-bindgen (which the charter
            // forbids) chrono falls back to a stub that answers UTC+0 for
            // every query (chrono-0.4/src/offset/local/mod.rs, the
            // `not(unix), not(windows), not(wasmbind)` inner module). So the
            // fix named here is the Dart display edge, where a real tz
            // database exists on every platform — never a Rust-side
            // `with_timezone`, which would compile everywhere and lie on web.
            Some(other) => bail!(
                "FR0045: `DateTime<{other}>` is not a bridged peer type — the wire \
                 carries an instant (i64 microseconds since the Unix epoch, UTC) and \
                 no time zone. Declare `chrono::DateTime<chrono::Utc>` and convert \
                 for display in Dart (`.toLocal()`), or cross the value with \
                 #[bridge(bytes(...))] if the zone itself is the data. Converting on \
                 the Rust side instead is not the answer: chrono's `Local` has no \
                 time-zone source on wasm32 without wasm-bindgen and silently \
                 answers UTC."
            ),
            None => bail!(
                "FR0045: `DateTime<_>` takes a concrete time-zone argument; \
                 `chrono::DateTime<chrono::Utc>` is the bridged peer type"
            ),
        },
        // The recognized-but-unmappable spellings. Every one is root-gated the
        // same way the peers above are, and for the same reason: the rejection
        // messages below are written *about* chrono's and `time`'s types, so
        // firing them at `mycrate::UtcDateTime` would refuse a stranger's type
        // with advice that does not apply to it. A foreign root falls through
        // to `Type::Named` and is judged as the ordinary named type it is.
        //
        // Named here at all — rather than left to FR0003 — because none of the
        // three shapes FR0003 offers fits any of them: a zone-less or monotonic
        // reading has nothing an instant wire can carry, whether it is wrapped
        // as a handle, encoded as bytes, or restated field by field.
        //
        // Wall-clock readings: a date and a time with no zone, so they do not
        // denote an instant at all. `PlainDateTime` is what `time` 0.3.5x
        // renamed `PrimitiveDateTime` to (`pub type PrimitiveDateTime =
        // PlainDateTime`, time-0.3.55 lib.rs:164), so a user on current `time`
        // writes the new name and must reach the same explanation.
        ("NaiveDateTime", []) if time_root(tp, &["chrono"]) => bail!(
            "FR0045: `{ident}` is a wall-clock reading with no time zone, so it does \
             not denote an instant and cannot cross a wire that carries one (i64 \
             microseconds since the Unix epoch, UTC). Declare \
             `chrono::DateTime<chrono::Utc>` — or cross it with #[bridge(bytes(...))] \
             if the un-zoned reading is itself the data."
        ),
        ("PrimitiveDateTime" | "PlainDateTime", []) if time_root(tp, &["time"]) => bail!(
            "FR0045: `{ident}` is a wall-clock reading with no time zone, so it does \
             not denote an instant and cannot cross a wire that carries one (i64 \
             microseconds since the Unix epoch, UTC). Declare `time::OffsetDateTime` \
             — or cross it with #[bridge(bytes(...))] if the un-zoned reading is \
             itself the data."
        ),
        ("UtcDateTime", []) if time_root(tp, &["time"]) => bail!(
            "FR0045: `time::UtcDateTime` is not a bridged peer type; declare \
             `time::OffsetDateTime` (the same instant, and the spelling that exists on \
             every `time` 0.3 release) and call `.to_utc()` where your own code wants \
             the `UtcDateTime` form."
        ),
        // Monotonic clocks. Not a mapping question: an `Instant` is a reading
        // whose zero is private to one process, so no encoding of it means
        // anything on the other side of the bridge. `time` has one too, and it
        // is monotonic for the same reason, so both roots reach this message.
        ("Instant", []) if time_root(tp, &["std", "core", "time"]) => bail!(
            "FR0045: `Instant` has no epoch — it is a monotonic reading whose zero is \
             private to one process, so it cannot cross the bridge in any encoding. \
             Send a `Duration` measured from a start both sides agree on, or a \
             `SystemTime` (wall clock) if you need an absolute point in time."
        ),
        ("Vec", [t]) => {
            let inner = parse_type(t)?;
            if inner == Type::U8 {
                Type::Bytes
            } else {
                Type::List(Box::new(inner), SeqKind::Vec)
            }
        }
        // VecDeque shares Vec's list wire codec and Dart `List`; only the Rust
        // reconstruct target differs (no `Vec<u8>`→`Bytes` fast path — the
        // Bytes codec decodes to `Vec<u8>`, which is not a `VecDeque`).
        ("VecDeque", [t]) => Type::List(Box::new(parse_type(t)?), SeqKind::VecDeque),
        ("Option", [t]) => Type::Option(Box::new(parse_type(t)?)),
        // A trait object crosses as a handle, and the `Box` there is how
        // ownership of an unsized value is spelled rather than a node in the
        // type — so it resolves at the name, not through `Type::Boxed`.
        //
        // Every other `Box<T>` is the indirection a recursive data type needs
        // (`struct Node { next: Option<Box<Self>> }`), and is transparent: the
        // wire and the Dart surface are the inner type's, and only the
        // generated Rust knows there is a `Box` at all.
        //
        // The three unsized spellings are refused rather than half-understood.
        // `parse_type` maps `[T]`/`[u8]`/`str` onto the *owned* containers
        // (`List`/`Bytes`/`String`), because a slice only ever reaches it
        // through a top-level `&`, so `Box<[T]>` would reconstruct as
        // `Box::new(Vec<T>)` — a type error inside generated code, about a
        // shape the IR has no way to say. Refused with the owned spelling
        // named; a boxed slice would be a container *kind* (like `VecDeque`),
        // not a special case here.
        ("Box", [t]) => match t {
            syn::Type::TraitObject(to) => trait_object_name(to)?,
            syn::Type::Slice(_) => bail!(
                "`Box<[T]>` is not supported: the bridge maps a slice to its owned \
                 container, so this would reconstruct as `Box<Vec<T>>`. Write \
                 `Vec<T>` — the wire is identical"
            ),
            syn::Type::Path(tp2)
                if tp2.qself.is_none()
                    && tp2.path.segments.last().is_some_and(|s| s.ident == "str") =>
            {
                bail!(
                    "`Box<str>` is not supported: the bridge maps `str` to `String`, so \
                     this would reconstruct as `Box<String>`. Write `String` — the wire \
                     is identical"
                )
            }
            other => Type::Boxed(Box::new(parse_type(other)?)),
        },
        // The six representation-marker wrappers (`runtime/rust/src/lib.rs`):
        // zero-cost aliases (`pub type Locked<T> = T;`) a use site may name
        // explicitly wherever a bridged type name may appear. `check::
        // resolve_type` verifies the claim against what the inner type
        // actually declared (FR0062) and erases the wrapper — a matching
        // `Locked<Point>` and a bare `Point` produce the identical resolved
        // `Type`. Bare `Data`/`Confined`/… with no argument falls through to
        // the ordinary named-type arm below, so a user's own type by one of
        // these names is untouched; only the one-argument generic form is
        // reserved, the same rule that already reserves `Vec`/`Box`/`Option`.
        ("Data", [t]) => Type::Claimed(Claim::Data, Box::new(parse_type(t)?)),
        ("Confined", [t]) => {
            Type::Claimed(Claim::Model(Model::Confined), Box::new(parse_type(t)?))
        }
        ("Resident", [t]) => {
            Type::Claimed(Claim::Model(Model::Resident), Box::new(parse_type(t)?))
        }
        ("Frozen", [t]) => {
            Type::Claimed(Claim::Model(Model::Frozen), Box::new(parse_type(t)?))
        }
        ("Locked", [t]) => {
            Type::Claimed(Claim::Model(Model::Locked), Box::new(parse_type(t)?))
        }
        ("Actor", [t]) => {
            Type::Claimed(Claim::Model(Model::Actor), Box::new(parse_type(t)?))
        }
        ("HashSet", [t]) => Type::Set(Box::new(parse_type(t)?), MapKind::Hash),
        // BTreeSet shares HashSet's list wire codec and Dart `Set`; it decodes
        // into a `BTreeSet` (sorted). The formerly-rejected "use HashSet" path
        // is gone.
        ("BTreeSet", [t]) => Type::Set(Box::new(parse_type(t)?), MapKind::BTree),
        ("HashMap", [k, v]) => {
            Type::Map(Box::new(parse_type(k)?), Box::new(parse_type(v)?), MapKind::Hash)
        }
        // BTreeMap shares HashMap's wire codec and Dart `Map`; it encodes in
        // sorted key order (its natural iteration) and decodes into a
        // `BTreeMap`. Dart's insertion-ordered `Map` preserves that order.
        ("BTreeMap", [k, v]) => {
            Type::Map(Box::new(parse_type(k)?), Box::new(parse_type(v)?), MapKind::BTree)
        }
        ("Result", _) => bail!("Result is only supported as the outermost return type"),
        // Same position rule as Result, same reason: `Deferred` is not a
        // value — it is how an actor method hands off its completion — so it
        // never crosses as data (no parameters, no fields, no nesting).
        ("Deferred", _) => bail!(
            "Deferred is only supported as the outermost return type of an \
             actor method"
        ),
        // Dart-object handles. Parsed here, in the ordinary recursive type
        // position, which is what lets them compose: nested in a struct, in a
        // Vec, several per function. (They were previously hoisted out of the
        // parameter list and rejected everywhere else.) Direction is enforced
        // by the checker, not here — FR0031.
        ("StreamSink", [t]) => dart_object(DartMirror::StreamController, Some(parse_type(t)?), None),
        ("StreamSink", _) => {
            bail!("StreamSink takes exactly one type argument (the item type)")
        }
        ("DartCallback", [t]) => dart_object(DartMirror::Callback, parse_mirror_arg(t)?, None),
        ("DartCallback", _) => bail!(
            "DartCallback takes exactly one type argument (the argument type; \
             use DartCallback<()> for no arguments)"
        ),
        ("DartFunction", [t, r]) => {
            let (ret, err) = parse_function_result(r)?;
            if ret.is_none() && err.is_none() {
                bail!(
                    "DartFunction<_, ()> returns nothing — use DartCallback \
                     (fire-and-forget, and portable to web)"
                );
            }
            dart_object_err(DartMirror::Function, parse_mirror_arg(t)?, ret, err)
        }
        ("DartFunction", _) => {
            bail!("DartFunction takes exactly two type arguments (argument and result)")
        }
        // The `frustrate::dart::*` mirrors. Unlike the three names above,
        // these must be written through their `dart::` path — `Sink` and
        // `StreamController` are plausible user type names, and silently
        // capturing one would be exactly the kind of surprise the bridge
        // must not spring.
        ("Sink", [t]) if is_dart_path(tp) => {
            dart_object(DartMirror::Sink, Some(parse_type(t)?), None)
        }
        ("EventSink", [t]) if is_dart_path(tp) => {
            dart_object(DartMirror::EventSink, Some(parse_type(t)?), None)
        }
        ("StreamController", [t]) if is_dart_path(tp) => {
            dart_object(DartMirror::StreamController, Some(parse_type(t)?), None)
        }
        ("Sink" | "EventSink" | "StreamController", _) if is_dart_path(tp) => bail!(
            "{ident} takes exactly one type argument (the item type)"
        ),
        (_, []) => Type::Named(ident),
        // A path with type arguments whose head is none of the spellings above
        // is a use of a **generic data type** the interface declares — the one
        // remaining meaning the arm can have, since every container, marker and
        // mirror this bridge knows is claimed before here.
        //
        // The parser cannot tell whether `Page` is declared, or generic, or
        // generic with this arity: it sees one file at a time and no
        // declarations. So it records the application and `check` decides —
        // FR0003 for an undeclared head, FR0073 for a non-generic one or the
        // wrong number of arguments. Const arguments (`Cache<4>`) do not reach
        // here at all: `generic_args` yields only type arguments, so `Cache<4>`
        // parses as a zero-argument `Named` and meets the declaration's own
        // FR0056 for having a const parameter.
        (_, args) => Type::App(
            ident,
            args.iter().map(|a| parse_type(a)).collect::<Result<Vec<_>>>()?,
        ),
    };
    Ok(t)
}

fn dart_object(mirror: DartMirror, item: Option<Type>, ret: Option<Type>) -> Type {
    dart_object_err(mirror, item, ret, None)
}

fn dart_object_err(
    mirror: DartMirror,
    item: Option<Type>,
    ret: Option<Type>,
    err: Option<Type>,
) -> Type {
    Type::DartObject(Box::new(DartObjectSpec {
        mirror,
        item,
        ret,
        err,
    }))
}

/// The result half of a `DartFunction<T, R>`, as `(ret, err)`.
///
/// Two shapes. A plain `R` is the original: the closure returns a value, and a
/// throw is the enclosing bridge call's panic. `Result<R, E>` is the fallible
/// one: the Dart
/// side may fail *as a value* the Rust body handles. `Result<(), E>` is legal —
/// "do this; you may refuse" still has a reply frame — which is why
/// [`DartObjectSpec::is_returning`] and not `ret.is_some()` is the question
/// every downstream rule asks.
///
/// **The message tiers of a bridged return do not mirror.** There, the `Err`
/// string is one Rust *authored*; here nothing authors it. Accepting
/// `Result<R, String>` or `anyhow::Result<R>` would make every Dart bug — a
/// `NoSuchMethodError`, a type error, a typo — arrive as a plausible business
/// value. So the error must be declared, and everything else stays the loud path.
fn parse_function_result(ty: &syn::Type) -> Result<(Option<Type>, Option<Type>)> {
    if let syn::Type::Path(tp) = ty {
        let last = tp.path.segments.last().unwrap();
        if last.ident == "Result" {
            let args = generic_args(last)?;
            let (ok_ty, err_ty) = match args.as_slice() {
                // `Result<R>` is anyhow's alias: no declared error at all.
                [_] => bail!(UNTYPED_DART_FUNCTION_ERROR),
                [ok, e] => (*ok, *e),
                _ => bail!(
                    "FR0043: a DartFunction result written as `Result` takes exactly two \
                     type arguments (the value and the declared error)"
                ),
            };
            if is_message_error(err_ty) {
                bail!(UNTYPED_DART_FUNCTION_ERROR);
            }
            let ret = if is_unit(ok_ty) {
                None
            } else {
                Some(parse_type(ok_ty)?)
            };
            return Ok((ret, Some(parse_type(err_ty)?)));
        }
    }
    if is_unit(ty) {
        return Ok((None, None));
    }
    Ok((Some(parse_type(ty)?), None))
}

/// FR0043. Stated once so the three rejected spellings cannot drift apart.
const UNTYPED_DART_FUNCTION_ERROR: &str =
    "FR0043: a fallible DartFunction needs a typed error — the `E` of \
     `DartFunction<T, Result<R, E>>` must be a bridged struct or enum. The message \
     tiers (`Result<R, String>`, `anyhow::Result<R>`, `Result<R, anyhow::Error>`) \
     do not mirror in this direction: an `Err` Rust \
     returns is one Rust authored, but a Dart throw is not, so accepting any throw \
     as a message would turn every Dart *bug* into a plausible business value. \
     Declare the failures the closure may report — Dart throws the generated \
     `EException` and Rust receives `Err(E)` — or write `DartFunction<T, R>` and \
     let a throw be this call's panic";

/// True when the `E` of a `Result<_, E>` is one of the *message* spellings —
/// `String`, `anyhow::Error`, or `Box<dyn Error>`. Shared by the return path
/// (where they are the declared untyped tier) and the DartFunction path (where
/// they are rejected), so the two cannot disagree about which spellings those
/// are.
///
/// `Box<dyn Error>` is here because it offers a Rust caller exactly what
/// `anyhow::Error` offers — an erased error with `Display`, `source()` and
/// `downcast` — and one of the two already crossed as its message. A
/// **concrete** unbridged error (`std::io::Error`, a local `thiserror` enum)
/// stays FR0003: it offers the caller its *type*, and crossing it as a message
/// would drop that silently.
fn is_message_error(ty: &syn::Type) -> bool {
    if is_boxed_erased_error(ty) {
        return true;
    }
    let syn::Type::Path(tp) = ty else {
        return false;
    };
    let last = tp.path.segments.last().unwrap();
    last.ident == "String"
        || (last.ident == "Error" && tp.path.segments.iter().any(|s| s.ident == "anyhow"))
}

/// `Box<dyn Error>` — std's erased error — with any of the marker bounds the
/// common spelling carries (`Box<dyn Error + Send + Sync + 'static>`).
///
/// **Root-gated**, the rule [`time_root`] already states for an ambiguous bare
/// name: no root, or `std`/`core`. A foreign root (`mycrate::Error`) is a
/// stranger's trait and falls through to the ordinary trait-object path, where
/// it is judged as the type it is. The bare spelling is admitted — where
/// `anyhow::Error` above demands its segment — because the two names are
/// written differently in practice: `use std::error::Error;` then a bare
/// `dyn Error` is the idiom, while `anyhow::Error` is essentially never bare
/// (the bare form is `anyhow::Result<T>`, which has no `E` at all).
///
/// Admitting the bare name can shadow two things, both of them a trait called
/// `Error` that is not std's: one **bridged** in this interface, and one
/// imported (`use mycrate::Error;`) and used as a `Box<dyn Error>`. Neither
/// works today. The bridged one is FR0035 (measured: "a typed error must be a
/// struct or enum, which crosses by value") and the refusal's own advice is to
/// cross it as a message, which is what this now does; the imported one is
/// FR0003, and afterwards it crosses as a message if the trait has `Display`
/// and fails inside generated code — loudly, at rustc — if it does not. So
/// what changes is a rejection, in one case into the thing the rejection
/// recommended, never a working meaning into a different one.
fn is_boxed_erased_error(ty: &syn::Type) -> bool {
    let syn::Type::Path(tp) = ty else {
        return false;
    };
    let last = tp.path.segments.last().unwrap();
    if last.ident != "Box" {
        return false;
    }
    let syn::PathArguments::AngleBracketed(args) = &last.arguments else {
        return false;
    };
    let mut tys = args.args.iter().filter_map(|a| match a {
        syn::GenericArgument::Type(t) => Some(t),
        _ => None,
    });
    let (Some(inner), None) = (tys.next(), tys.next()) else {
        return false;
    };
    // `Box<(dyn Error + Send + Sync)>` — the parenthesized multi-bound form.
    let inner = match inner {
        syn::Type::Paren(p) => p.elem.as_ref(),
        other => other,
    };
    let syn::Type::TraitObject(to) = inner else {
        return false;
    };
    let mut traits = to.bounds.iter().filter_map(|b| match b {
        syn::TypeParamBound::Trait(tb) => Some(&tb.path),
        _ => None,
    });
    let Some(first) = traits.next() else {
        return false;
    };
    // Marker supertraits ride along on the common spelling and say nothing
    // about which error this is; anything else makes it a different type.
    if traits.any(|p| !p.is_ident("Send") && !p.is_ident("Sync")) {
        return false;
    }
    let root = (first.segments.len() > 1).then(|| first.segments[0].ident.to_string());
    first.segments.last().is_some_and(|s| s.ident == "Error")
        && matches!(root.as_deref(), None | Some("std") | Some("core"))
}

/// A mirror's argument type, where `()` means "no argument"
/// (`DartCallback<()>`).
fn parse_mirror_arg(t: &syn::Type) -> Result<Option<Type>> {
    if is_unit(t) {
        Ok(None)
    } else {
        parse_type(t).map(Some)
    }
}

/// True when the path names one of the `frustrate::dart::*` mirrors through
/// its `dart` module — `frustrate::dart::core::Sink`, `dart::r#async::
/// EventSink`. Requiring the segment keeps a user's own `Sink` from being
/// silently captured as a channel endpoint.
fn is_dart_path(tp: &syn::TypePath) -> bool {
    tp.path.segments.iter().any(|s| s.ident == "dart")
}

/// The first segment of a path, when there is more than one.
///
/// The *root*, not "any segment" (as [`is_dart_path`] asks), because the one
/// name this disambiguates — `Duration` — appears under a `time` segment in
/// both `std::time::Duration` and `time::Duration`. Only the root tells them
/// apart. `None` for a bare name, which therefore keeps its historical
/// meaning rather than acquiring one from a coincidental segment.
fn path_root(tp: &syn::TypePath) -> Option<String> {
    let segs = &tp.path.segments;
    (segs.len() > 1).then(|| segs[0].ident.to_string())
}

/// True when a time spelling's path root is one this parser may speak for: an
/// unqualified name (which keeps the meaning it has always had) or one of the
/// crates that actually owns the name.
///
/// Every time arm is gated on this, mapped and rejected alike, so the rule is
/// one rule: **a name under a foreign root is a stranger's type.** It falls
/// through to `Type::Named` and is judged as one, rather than being mapped to
/// a peer whose contract it does not have — or refused with advice written
/// about somebody else's crate.
fn time_root(tp: &syn::TypePath, owners: &[&str]) -> bool {
    match path_root(tp) {
        None => true,
        Some(root) => owners.contains(&root.as_str()),
    }
}

/// The last segment of a `chrono::DateTime`'s type argument (`Utc`, `Local`,
/// `FixedOffset`), or `None` when the argument is not a plain path.
fn tz_name(ty: &syn::Type) -> Option<String> {
    match ty {
        syn::Type::Path(tp) => Some(tp.path.segments.last()?.ident.to_string()),
        _ => None,
    }
}

fn generic_args(seg: &syn::PathSegment) -> Result<Vec<&syn::Type>> {
    match &seg.arguments {
        syn::PathArguments::None => Ok(vec![]),
        syn::PathArguments::AngleBracketed(ab) => Ok(ab
            .args
            .iter()
            .filter_map(|a| match a {
                syn::GenericArgument::Type(t) => Some(t),
                _ => None,
            })
            .collect()),
        syn::PathArguments::Parenthesized(_) => bail!("unsupported path arguments"),
    }
}

fn type_to_string(ty: &syn::Type) -> String {
    format!("{ty:?}").chars().take(80).collect()
}

#[cfg(test)]
mod tests {

    /// The bulk form: one line at the top claims every bridged member in the
    /// file, including methods reached through an `impl` block, so a file of a
    /// hundred members needs no per-item annotation.
    #[test]
    fn bridge_file_claims_every_member_in_the_file() {
        let iface = parse_source(
            r#"
            frustrate::bridge_file!(no_block);
            #[bridge(sync)] pub fn a(x: i64) -> i64 { x }
            #[bridge] pub async fn b() {}
            #[bridge(confined)] pub struct D { x: i64 }
            #[bridge] impl D {
                #[bridge(sync)] pub fn n(&self) -> i64 { 0 }
                pub fn touch(&self) {}
            }
            "#,
            "crate::api",
        )
        .unwrap();
        assert!(
            iface.functions.iter().all(|f| f.no_block),
            "unclaimed: {:?}",
            iface
                .functions
                .iter()
                .filter(|f| !f.no_block)
                .map(|f| &f.name)
                .collect::<Vec<_>>()
        );
        assert!(iface.functions.len() >= 4, "{:?}", iface.functions.len());
    }

    /// A per-item annotation inside a claimed file cannot opt out. Claims only
    /// strengthen — otherwise the one line at the top would read as covering
    /// the file while silently excluding whichever members carry their own
    /// `#[bridge(...)]`, which is every interesting one.
    #[test]
    fn a_per_item_attribute_cannot_shed_a_file_level_claim() {
        let iface = parse_source(
            r#"
            frustrate::bridge_file!(no_block);
            #[bridge(sync)] pub fn a(x: i64) -> i64 { x }
            "#,
            "crate::api",
        )
        .unwrap();
        assert!(iface.functions[0].no_block);
    }

    /// The three ways a file-level claim is malformed, each refused loudly.
    #[test]
    fn a_malformed_bridge_file_claim_is_refused() {
        for (src, wanted) in [
            (
                "frustrate::bridge_file!(no_block);\n\
                 frustrate::bridge_file!(no_block);\n\
                 #[bridge(sync)] pub fn a() {}",
                "more than one",
            ),
            (
                "frustrate::bridge_file!();\n#[bridge(sync)] pub fn a() {}",
                "claims nothing",
            ),
            (
                "frustrate::bridge_file!(sync);\n#[bridge(sync)] pub fn a() {}",
                "not a file-level option",
            ),
            (
                "frustrate::bridge_file!(no_block);\n#[bridge(data)] pub struct S { x: i64 }",
                "bind nothing",
            ),
        ] {
            let err = parse_source(src, "crate::api")
                .expect_err("must be refused: {src}");
            assert!(
                format!("{err}").contains(wanted),
                "wanted {wanted:?} in: {err}"
            );
        }
    }

    /// `no_block` claims a body never waits, so it is refused where there is
    /// no body. Ignoring it instead would be the FR0041-shaped defect: an
    /// annotation that reads as coverage and delivers none.
    ///
    /// All three declaration kinds get a case, because a struct reaches the
    /// rejection before its model is even known and an enum reaches it on a
    /// separate path.
    #[test]
    fn no_block_is_rejected_on_a_declaration_with_no_body() {
        for src in [
            "#[bridge(no_block)] pub struct S { x: i64 }",
            "#[bridge(no_block, confined)] pub struct D { x: i64 }",
            "#[bridge(no_block)] pub enum E { A, B }",
        ] {
            let err = parse_source(src, "crate::api")
                .expect_err("no_block on a data declaration must be refused");
            let msg = format!("{err}");
            assert!(
                msg.contains("no_block") && msg.contains("no body"),
                "the diagnostic must say what is wrong and why: {msg}"
            );
        }
    }

    /// The other half of the same rule: it *is* valid on the things that do
    /// have bodies. A rejection that also caught these would be worse than none.
    #[test]
    fn no_block_is_accepted_on_functions_impls_and_traits() {
        for src in [
            "#[bridge(sync, no_block)] pub fn f(x: i64) -> i64 { x }",
            "#[bridge(confined)] pub struct D { x: i64 } \
             #[bridge(no_block)] impl D { pub fn n(&self) -> i64 { 0 } }",
        ] {
            parse_source(src, "crate::api")
                .unwrap_or_else(|e| panic!("no_block must be valid here: {src}\n{e}"));
        }
    }

    /// A `#[bridge(...)]` attribute on a field or a variant is rejected.
    ///
    /// Ignoring it would let a declaration carry an annotation codegen never
    /// read and still generate Dart.
    ///
    /// All three positions get a case because they are three separate call
    /// sites in `parse_struct`/`parse_enum` and fixing one would leave the
    /// others silent.
    #[test]
    fn a_bridge_attribute_on_a_field_or_variant_is_rejected() {
        for (src, wanted) in [
            (
                "#[bridge(data)] pub struct S { #[bridge(no_eq)] pub x: i32 }",
                "a field",
            ),
            (
                "#[bridge(data)] pub enum E { #[bridge(no_eq)] A, B }",
                "a variant",
            ),
            (
                "#[bridge(data)] pub enum E { A(#[bridge(no_eq)] i32), B }",
                "a field",
            ),
        ] {
            let err = parse_source(src, "crate::api").unwrap_err().to_string();
            assert!(err.contains("FR0041"), "{src}: {err}");
            assert!(err.contains(wanted), "{src}: {err}");
        }
    }

    /// ...and ordinary doc comments in those positions are still fine, so the
    /// check did not just reject every attribute it saw.
    #[test]
    fn docs_on_fields_and_variants_still_parse() {
        let iface = parse(
            "#[bridge(data)] pub struct S { /// the x\n pub x: i32 }\n             #[bridge(data)] pub enum E { /// the a\n A, B }",
        );
        // `doc_lines` keeps the space after `///`, so compare on content.
        assert_eq!(iface.structs[0].fields[0].docs.len(), 1);
        assert!(iface.structs[0].fields[0].docs[0].contains("the x"));
        assert_eq!(iface.enums[0].variants[0].docs.len(), 1);
        assert!(iface.enums[0].variants[0].docs[0].contains("the a"));
    }

    /// An explicit discriminant is **carried**: the number the author wrote,
    /// per variant, with Rust's own successor rule filling the variants that
    /// write none. The frustrate wire is unaffected — it is the position.
    #[test]
    fn an_explicit_enum_discriminant_is_recorded_per_variant() {
        let iface = parse("#[bridge(data)] pub enum Code { Ok = 100, Next, Missing = 404, Neg = -1 }");
        let ds: Vec<Option<i64>> =
            iface.enums[0].variants.iter().map(|v| v.discriminant).collect();
        assert_eq!(ds, vec![Some(100), Some(101), Some(404), Some(-1)]);
        // A first variant that writes none starts at 0, as Rust does.
        let iface = parse("#[bridge(data)] pub enum E { A, B = 7, C }");
        let ds: Vec<Option<i64>> =
            iface.enums[0].variants.iter().map(|v| v.discriminant).collect();
        assert_eq!(ds, vec![Some(0), Some(7), Some(8)]);
        // Hex and other literal bases are literals too.
        let iface = parse("#[bridge(data)] pub enum H { A = 0x10 }");
        assert_eq!(iface.enums[0].variants[0].discriminant, Some(16));
        // A **last** variant at `i64::MAX` is fine: nothing needs a successor,
        // so the successor rule must not be asked about it.
        let iface = parse("#[bridge(data)] pub enum M { A = 0, B = 9223372036854775807 }");
        assert_eq!(iface.enums[0].variants[1].discriminant, Some(i64::MAX));
    }

    /// ...and an enum that writes none records none, so the getter is emitted
    /// only where the author asked for it.
    #[test]
    fn an_enum_without_discriminants_records_none() {
        let iface = parse("#[bridge(data)] pub enum Code { Ok, Missing }");
        assert_eq!(iface.enums.len(), 1);
        assert!(iface.enums[0].variants.iter().all(|v| v.discriminant.is_none()));
    }

    /// What FR0040 still refuses, and why each is refused rather than guessed.
    #[test]
    fn a_discriminant_frustrate_cannot_read_or_carry_is_refused() {
        for (src, needle) in [
            // Not a literal: reading it means evaluating Rust.
            ("#[bridge(data)] pub enum E { A = FOO }", "not an integer literal"),
            ("#[bridge(data)] pub enum E { A = 1 << 3 }", "not an integer literal"),
            ("#[bridge(data)] pub enum E { A = OTHER as i64 }", "not an integer literal"),
            // A fielded enum is a Dart sealed class: no enum value to hang a
            // getter on. (rustc allows this only under a `#[repr]`.)
            (
                "#[bridge(data)] #[repr(i64)] pub enum E { A(i64) = 1, B = 2 }",
                "sealed class",
            ),
            // Outside `i64`: Dart's `int` could not hold it.
            (
                "#[bridge(data)] pub enum E { A = 99999999999999999999 }",
                "does not fit in an `i64`",
            ),
            // The implicit successor overflows, and the message names the
            // variant that has no number rather than the one that does.
            (
                "#[bridge(data)] pub enum E { A = 9223372036854775807, B }",
                "`E::B` writes no discriminant",
            ),
        ] {
            let err = parse_source(src, "crate::api").unwrap_err().to_string();
            assert!(err.contains("FR0040"), "{src}: {err}");
            assert!(err.contains(needle), "{src}: {err}");
        }
        // The suggested fix must be one the rest of the checker accepts. An
        // earlier version of this message proposed a free function because
        // FR0005 then refused an impl block on a data type; a data type takes
        // an impl block now (a data receiver is an ordinary member, and
        // `check` pins that a data enum takes one), so the direct spelling —
        // a method on the enum — is the one named, on the enum it goes on.
        let err = parse_source("#[bridge(data)] pub enum Code { Ok = FOO }", "crate::api")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("#[bridge] impl Code {") && err.contains("fn value(&self) -> i64"),
            "the fix must be a method on the enum, and named: {err}"
        );
    }
    // Typed-error parsing: which spellings of `Result` carry a value and which
    // stay on the message path. This is the whole of the strict-addition
    // promise, so it is pinned per spelling rather than in aggregate.
    #[test]
    fn result_error_spellings_that_stay_untyped() {
        for src in [
            "#[bridge(sync)] pub fn f() -> anyhow::Result<i64> { Ok(0) }",
            "#[bridge(sync)] pub fn f() -> Result<i64> { Ok(0) }",
            "#[bridge(sync)] pub fn f() -> Result<i64, String> { Ok(0) }",
            "#[bridge(sync)] pub fn f() -> Result<i64, anyhow::Error> { Ok(0) }",
            // Rust's other erased error, in every spelling that names std's
            // trait: bare after a `use`, qualified, `core`'s, and with the
            // marker bounds the common form carries.
            "#[bridge(sync)] pub fn f() -> Result<i64, Box<dyn Error>> { Ok(0) }",
            "#[bridge(sync)] pub fn f() -> Result<i64, Box<dyn std::error::Error>> { Ok(0) }",
            "#[bridge(sync)] pub fn f() -> Result<i64, Box<dyn core::error::Error>> { Ok(0) }",
            "#[bridge(sync)] pub fn f() -> Result<i64, Box<dyn std::error::Error + Send + Sync>> \
             { Ok(0) }",
            "#[bridge(sync)] pub fn f() -> Result<i64, Box<(dyn Error + Send + Sync + 'static)>> \
             { Ok(0) }",
        ] {
            let iface = super::parse_source(src, "crate::api").unwrap();
            let f = &iface.functions[0];
            assert!(f.fallible, "{src}");
            assert!(f.err.is_none(), "{src} should stay on the message path");
        }
    }

    /// The boxed tier is `dyn Error` and nothing near it. A **concrete**
    /// unbridged error offers a Rust caller its type, and crossing it as a
    /// message would drop that silently — so it stays FR0003. A trait object
    /// under a foreign root is a stranger's trait, judged as the type it is.
    #[test]
    fn a_concrete_or_foreign_error_is_not_the_boxed_tier() {
        for (src, want) in [
            (
                "#[bridge(sync)] pub fn f() -> Result<i64, std::io::Error> { Ok(0) }",
                Type::Named("Error".into()),
            ),
            (
                "#[bridge(sync)] pub fn f() -> Result<i64, Box<dyn mycrate::Error>> { Ok(0) }",
                Type::Named("Error".into()),
            ),
            (
                "#[bridge(sync)] pub fn f() -> Result<i64, Box<dyn Failure>> { Ok(0) }",
                Type::Named("Failure".into()),
            ),
        ] {
            let iface = super::parse_source(src, "crate::api").unwrap();
            assert_eq!(iface.functions[0].err, Some(want), "{src}");
        }
        // …and each of those is then an FR0003 about a type nobody declared,
        // which is the loud refusal the tier split exists to keep.
        let ds = crate::check::check(
            super::parse_source(
                "#[bridge(sync)] pub fn f() -> Result<i64, std::io::Error> { Ok(0) }",
                "crate::api",
            )
            .unwrap(),
        )
        .unwrap_err();
        assert!(ds.iter().any(|d| d.code == "FR0003"), "{ds:?}");
    }

    #[test]
    fn a_named_error_type_is_carried_into_the_ir() {
        let iface = super::parse_source(
            "#[bridge(sync)] pub fn f() -> Result<i64, SendError> { Ok(0) }",
            "crate::api",
        )
        .unwrap();
        let f = &iface.functions[0];
        assert!(f.fallible);
        assert_eq!(
            f.err,
            Some(crate::ir::Type::Named("SendError".into())),
            "the parser cannot know if it is bridged; that is the checker's job"
        );
    }

    #[test]
    fn an_infallible_member_has_no_error_type() {
        let iface =
            super::parse_source("#[bridge(sync)] pub fn f() -> i64 { 0 }", "crate::api").unwrap();
        assert!(!iface.functions[0].fallible);
        assert!(iface.functions[0].err.is_none());
    }

    use super::*;
    use pretty_assertions::assert_eq;

    fn parse(src: &str) -> Interface {
        parse_source(src, "crate::api").unwrap()
    }

    #[test]
    fn ignores_unannotated_items() {
        let iface = parse("pub fn not_bridged() {}\npub struct NotBridged { x: i32 }");
        assert!(iface.functions.is_empty());
        assert!(iface.structs.is_empty());
    }

    // ---------------------------------------------------------------------
    // FR0042: the omission warning.
    //
    // Skipping an unannotated item (above) is correct and stays correct — a
    // bridge file is full of helpers, private types and foreign trait impls.
    // It must not be *silent*, though: a forgotten `#[bridge]` would otherwise
    // produce no diagnostic at all. These tests pin what is reported and,
    // just as importantly, what is not.
    // ---------------------------------------------------------------------

    fn warnings(src: &str) -> Vec<String> {
        parse_source_reporting(src, "crate::api")
            .unwrap()
            .1
            .iter()
            .map(|w| w.to_string())
            .collect()
    }

    /// The exact shape F-00 describes: a `pub fn` whose signature is entirely
    /// bridgeable, sitting in a declared bridge source with no `#[bridge]`.
    #[test]
    fn warns_on_an_unannotated_pub_fn_with_a_bridgeable_signature() {
        let w = warnings(
            "#[bridge] pub fn bridged(a: i32) -> i32 { a }\n\
             pub fn forgotten(a: i32, b: String) -> Vec<u8> { vec![] }",
        );
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("FR0042"), "{}", w[0]);
        assert!(w[0].contains("warning["), "not an error: {}", w[0]);
        assert!(w[0].contains("forgotten"), "names the item: {}", w[0]);
        assert!(w[0].contains("line 2"), "names the line: {}", w[0]);
        // Both exits, per the repo's diagnostic convention.
        assert!(w[0].contains("#[bridge]"), "{}", w[0]);
        assert!(w[0].contains("#[bridge(skip)]"), "{}", w[0]);
    }

    /// Visibility is the first filter: only a fully-`pub` item can have been
    /// meant for the Dart surface. `pub(crate)`/`pub(super)`/private are
    /// deliberate Rust-internal helpers.
    #[test]
    fn no_warning_on_a_private_or_restricted_fn() {
        assert!(warnings("fn helper(a: i32) -> i32 { a }").is_empty());
        assert!(warnings("pub(crate) fn helper(a: i32) -> i32 { a }").is_empty());
        assert!(warnings("pub(super) fn helper(a: i32) -> i32 { a }").is_empty());
    }

    /// The signature is the second filter. A `pub fn` that names a type the
    /// file does not bridge is a helper by strong implication — and if a
    /// bridged member actually needed that type, FR0003 already says so
    /// loudly at the use site.
    #[test]
    fn no_warning_on_a_pub_fn_naming_an_unbridged_type() {
        assert!(warnings("pub fn helper(x: InternalThing) -> i32 { 0 }").is_empty());
        assert!(warnings("pub fn helper() -> InternalThing { todo!() }").is_empty());
        // ...nested, too: the whole type graph of the signature must resolve.
        assert!(warnings("pub fn helper(x: Vec<Option<InternalThing>>) {}").is_empty());
        // ...and a signature the bridge could not express at all is silent.
        assert!(warnings("pub fn helper<T>(x: T) {}").is_empty());
        assert!(warnings("pub fn helper(x: &mut &str) {}").is_empty());
    }

    /// ...but a type bridged in the same file does resolve, so the warning
    /// still fires through it.
    #[test]
    fn warns_through_a_type_bridged_in_the_same_file() {
        let w = warnings(
            "#[bridge(data)] pub struct P { pub x: i32 }\n\
             pub fn forgotten(p: P) -> Vec<P> { vec![p] }",
        );
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("forgotten"), "{}", w[0]);
    }

    /// `#[bridge(skip)]` is the declared omission: the item stays unbridged
    /// (exactly as before) and the warning goes away.
    #[test]
    fn bridge_skip_silences_the_warning_and_bridges_nothing() {
        let (iface, warns) = parse_source_reporting(
            "#[bridge(skip)] pub fn deliberate(a: i32) -> i32 { a }",
            "crate::api",
        )
        .unwrap();
        assert!(iface.functions.is_empty(), "skip must not bridge the item");
        assert!(warns.is_empty(), "{warns:?}");
    }

    /// A `#[cfg]`-gated item is a deliberate conditional, and `#[bridge]` is
    /// the wrong fix for it (FR0034 rejects a cfg-gated bridged member).
    /// Never suggest a fix another diagnostic refuses.
    #[test]
    fn no_warning_on_a_cfg_gated_pub_fn() {
        assert!(warnings("#[cfg(test)] pub fn helper(a: i32) -> i32 { a }").is_empty());
    }

    /// The same rule for handles: a signature can parse and still be refused by
    /// the checker for *where* it puts one. Warning there would prescribe
    /// `#[bridge]` and have a different diagnostic reject it.
    #[test]
    fn no_warning_where_a_handle_sits_somewhere_the_checker_refuses() {
        const DOC: &str = "#[bridge(locked)] pub struct Doc { text: String }\n";
        for sig in [
            // An opaque crosses as a handle, so a parameter borrows it.
            "pub fn f(d: Doc) {}",
            // ...and never nested in a container (FR0004).
            "pub fn f(d: Vec<Doc>) {}",
            "pub fn f(d: &Option<Doc>) {}",
            "pub fn f() -> Vec<Doc> { vec![] }",
            // A Dart object cannot be returned — Rust cannot mint one (FR0031).
            "pub fn f() -> StreamSink<i64> { todo!() }",
            // An error crosses by value, so it is never a handle (FR0035).
            "pub fn f() -> Result<i64, Doc> { Ok(0) }",
        ] {
            let src = format!("{DOC}{sig}");
            assert!(warnings(&src).is_empty(), "{sig} should be silent");
        }
    }

    /// ...but the positions the checker *does* allow still warn, so the rule
    /// above did not just silence every function that mentions a handle.
    #[test]
    fn still_warns_where_a_handle_sits_legally() {
        const DOC: &str = "#[bridge(locked)] pub struct Doc { text: String }\n";
        for sig in [
            "pub fn f(d: &Doc) -> i64 { 0 }",
            "pub fn f(d: &mut Doc) {}",
            "pub fn f(s: StreamSink<i64>) {}",
            "pub fn f() -> Doc { todo!() }",
            "pub fn f() -> Option<Doc> { None }",
        ] {
            let src = format!("{DOC}{sig}");
            let w = warnings(&src);
            assert_eq!(w.len(), 1, "{sig} should warn: {w:?}");
        }
    }

    /// A representation-marker wrapper (`Locked<Doc>`) must not hide a handle
    /// from the two rules above: `signature_is_bridgeable` erases it
    /// (`Type::erase_claims`) before running the FR0004/FR0031/FR0035 shape
    /// tests, so every position warns or stays silent exactly as the bare
    /// spelling does. Before that erasure this was live: `Locked<Doc>` by
    /// value read as an ordinary, resolvable type — neither `Named` nor
    /// counted as an opaque — so the checker-refused shape warned "add
    /// `#[bridge]`" and the checker then refused the handle the warning had
    /// just prescribed exposing it as.
    #[test]
    fn wrapper_parity_with_the_bare_spelling() {
        const DOC: &str = "#[bridge(locked)] pub struct Doc { text: String }\n";
        for sig in [
            // By value is a consume, which the omission heuristic deliberately
            // does not prescribe (under-approximating is the safe direction
            // for a warning) — silent wrapped exactly as it is bare.
            "pub fn f(d: Locked<Doc>) {}",
            "pub fn f(d: Vec<Locked<Doc>>) {}",
            // Every container is a return position now; the heuristic still
            // stops at `Option`, and stays silent wrapped exactly as bare.
            "pub fn f() -> Vec<Locked<Doc>> { vec![] }",
        ] {
            let src = format!("{DOC}{sig}");
            assert!(warnings(&src).is_empty(), "{sig} should be silent");
        }
        for sig in [
            // The legal positions still warn, wrapped or not.
            "pub fn f(d: &Locked<Doc>) -> i64 { 0 }",
            "pub fn f(d: &mut Locked<Doc>) {}",
            "pub fn f() -> Locked<Doc> { todo!() }",
            "pub fn f() -> Option<Locked<Doc>> { None }",
        ] {
            let src = format!("{DOC}{sig}");
            let w = warnings(&src);
            assert_eq!(w.len(), 1, "{sig} should warn: {w:?}");
        }
    }

    /// Second silent shape: the type is bridged, its inherent `impl` block is
    /// not, so the Dart class comes out with no methods and nothing is said.
    #[test]
    fn warns_on_an_unannotated_inherent_impl_of_a_bridged_type() {
        let w = warnings(
            "#[bridge(locked)] pub struct Doc { text: String }\n\
             impl Doc {\n    pub fn len(&self) -> i64 { 0 }\n}",
        );
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("FR0042"), "{}", w[0]);
        assert!(w[0].contains("Doc"), "names the type: {}", w[0]);
        assert!(w[0].contains("len"), "names the method it dropped: {}", w[0]);
        assert!(w[0].contains("impl"), "points at the impl block: {}", w[0]);
        assert!(w[0].contains("handle type"), "says which: {}", w[0]);
        // A data type drops methods exactly the same way, so it is reported
        // the same way — with the representation it actually has.
        let w = warnings(
            "#[bridge(data)] pub struct P { pub x: i32 }\n\
             impl P {\n    pub fn go(&self) -> i64 { 0 }\n}",
        );
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("FR0042") && w[0].contains("data type"), "{}", w[0]);
        // A **concrete** impl on a generic data type is bridgeable now, so an
        // unannotated one drops methods the same way — and the report names
        // the block as written, `impl Page<…>`, because the bare `impl Page`
        // is a different block and is refused.
        let w = warnings(
            "#[bridge(data)] pub struct I { pub id: i64 }\n\
             #[bridge(data)] pub struct Page<T> { pub items: Vec<T> }\n\
             impl Page<I> {\n    pub fn go(&self) -> i64 { 0 }\n}",
        );
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("impl Page<…>"), "{}", w[0]);
        // A **generic** one is not reported, and the reason is that this pass
        // cannot decide it: whether a member is bridgeable depends on the
        // instantiation, which the expansion has not computed yet.
        let w = warnings(
            "#[bridge(data)] pub struct Page<T> { pub items: Vec<T> }\n\
             impl<T> Page<T> {\n    pub fn go(&self) -> i64 { 0 }\n}",
        );
        assert!(w.is_empty(), "{w:?}");
    }

    /// The impl rule is narrow on purpose. Every unannotated `impl` in this
    /// repo's own bridge sources is one of these, and none of them is a
    /// forgotten annotation.
    #[test]
    fn no_warning_on_impls_that_were_never_bridgeable() {
        // A foreign/internal trait impl — `#[bridge] impl Trait for T` has its
        // own rules and is not what a migrator forgot here.
        assert!(warnings(
            "#[bridge(locked)] pub struct Doc { text: String }\n\
             impl std::fmt::Debug for Doc {\n    \
             fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result { Ok(()) }\n}"
        )
        .is_empty());
        // An impl on a type that is not bridged at all.
        assert!(warnings("struct Helper;\nimpl Helper {\n    pub fn go(&self) -> i64 { 0 }\n}")
            .is_empty());
        // An impl on a bridged data struct whose only public method is one
        // the checker refuses anyway: `&mut self` mutates a decoded copy
        // (FR0013), so suggesting `#[bridge]` would point at a wall.
        assert!(warnings(
            "#[bridge(data)] pub struct P { pub x: i32 }\n\
             impl P {\n    pub fn bump(&mut self) { self.x += 1; }\n}"
        )
        .is_empty());
        // Same wall, one rule over: a data type that owns a handle is
        // return-only, so a receiver on it is FR0004.
        assert!(warnings(
            "#[bridge(frozen)] pub struct Doc { t: i64 }\n\
             #[bridge(data)] pub struct Holder { pub doc: Doc }\n\
             impl Holder {\n    pub fn go(&self) -> i64 { 0 }\n}"
        )
        .is_empty());
        // And a borrowed return that reaches a handle cannot mint.
        assert!(warnings(
            "#[bridge(frozen)] pub struct Doc { t: i64 }\n\
             #[bridge(confined)] pub struct C { d: Doc }\n\
             impl C {\n    pub fn doc(&self) -> &Doc { todo!() }\n}"
        )
        .is_empty());
        // An impl block with no public, bridgeable method.
        assert!(warnings(
            "#[bridge(locked)] pub struct Doc { text: String }\n\
             impl Doc {\n    fn private(&self) -> i64 { 0 }\n}"
        )
        .is_empty());
        // ...and the declared omission works here too.
        assert!(warnings(
            "#[bridge(locked)] pub struct Doc { text: String }\n\
             #[bridge(skip)] impl Doc {\n    pub fn go(&self) -> i64 { 0 }\n}"
        )
        .is_empty());
    }

    /// Inside an annotated block an unannotated method is bridged by
    /// inheritance, so `#[bridge(skip)]` has to be honoured there or it would
    /// be an annotation that does nothing — the exact defect FR0041 exists to
    /// prevent.
    #[test]
    fn bridge_skip_excludes_a_method_from_an_annotated_impl_block() {
        let iface = parse(
            "#[bridge(locked)] pub struct Doc { text: String }\n\
             #[bridge] impl Doc {\n    \
             pub fn kept(&self) -> i64 { 0 }\n    \
             #[bridge(skip)] pub fn dropped(&self) -> i64 { 0 }\n}",
        );
        let names: Vec<&str> = iface.functions.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["kept"], "{names:?}");
    }

    /// ...and identically in a bridged trait, which has the same inherit path.
    #[test]
    fn bridge_skip_excludes_a_method_from_an_annotated_trait() {
        let iface = parse(
            "#[bridge(frozen)] pub trait Store: Send + Sync {\n    \
             fn kept(&self) -> i64;\n    \
             #[bridge(skip)] fn dropped(&self) -> i64;\n}",
        );
        let names: Vec<&str> = iface.functions.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["kept"], "{names:?}");
    }

    /// `skip` declares an item deliberately unbridged; combining it with an
    /// option that configures bridging is a contradiction, and the repo's
    /// convention is to say so rather than pick a winner.
    #[test]
    fn skip_cannot_be_combined_with_bridging_options() {
        for src in [
            "#[bridge(skip, sync)] pub fn f() {}",
            "#[bridge(skip, native_only)] pub fn f() {}",
            "#[bridge(skip, locked)] pub struct S { x: i32 }",
        ] {
            let err = parse_source(src, "crate::api").unwrap_err().to_string();
            assert!(err.contains("skip"), "{src}: {err}");
        }
    }

    #[test]
    fn parses_free_function_async_by_default() {
        let iface = parse(
            r#"
            #[frustrate::bridge]
            pub fn add(a: i32, b: i32) -> i32 { a + b }
            "#,
        );
        let f = &iface.functions[0];
        assert_eq!(f.name, "add");
        assert_eq!(f.exec, Exec::Async);
        assert_eq!(f.params.len(), 2);
        assert_eq!(f.ret, Some(Type::I32));
        assert!(!f.fallible);
    }

    #[test]
    fn parses_sync_and_result() {
        let iface = parse(
            r#"
            #[bridge(sync)]
            pub fn parse_num(s: String) -> anyhow::Result<i64> { todo!() }
            "#,
        );
        let f = &iface.functions[0];
        assert_eq!(f.exec, Exec::Sync);
        assert!(f.fallible);
        assert_eq!(f.ret, Some(Type::I64));
    }

    #[test]
    fn parses_collections_and_bytes() {
        let iface = parse(
            r#"
            #[bridge]
            pub fn f(a: Vec<u8>, b: Vec<String>, c: Option<f64>, d: std::collections::HashMap<String, i32>, e: [u8; 32]) {}
            "#,
        );
        let p = &iface.functions[0].params;
        assert_eq!(p[0].ty, Type::Bytes);
        assert_eq!(p[1].ty, Type::List(Box::new(Type::String), SeqKind::Vec));
        assert_eq!(p[2].ty, Type::Option(Box::new(Type::F64)));
        assert_eq!(
            p[3].ty,
            Type::Map(Box::new(Type::String), Box::new(Type::I32), MapKind::Hash)
        );
        assert_eq!(p[4].ty, Type::ByteArray(32));
    }

    /// `[T; N]` for every element type. `[u8; N]` keeps its own IR variant —
    /// the wire is identical, and the split is what keeps the schema
    /// fingerprint of an interface that already crosses one where it is.
    #[test]
    fn parses_fixed_arrays_of_every_element_type() {
        let iface = parse(
            "#[bridge] pub fn f(a: [f32; 16], b: [i32; 4], c: [String; 3], \
               d: [u8; 32], e: [[i32; 2]; 3], g: [f64; 0]) {}",
        );
        let p = &iface.functions[0].params;
        assert_eq!(p[0].ty, Type::Array(Box::new(Type::F32), 16));
        assert_eq!(p[1].ty, Type::Array(Box::new(Type::I32), 4));
        assert_eq!(p[2].ty, Type::Array(Box::new(Type::String), 3));
        assert_eq!(p[3].ty, Type::ByteArray(32));
        assert_eq!(
            p[4].ty,
            Type::Array(Box::new(Type::Array(Box::new(Type::I32), 2)), 3)
        );
        assert_eq!(p[5].ty, Type::Array(Box::new(Type::F64), 0));
        // The length is part of the wire, so it has to be a literal codegen
        // can read rather than an expression it would have to evaluate.
        let err = parse_source(
            "const N: usize = 4; #[bridge] pub fn f(a: [i32; N]) {}",
            "crate::api",
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("array length must be an integer literal"),
            "{err:#}"
        );
    }

    #[test]
    fn parses_hashset() {
        let iface = parse(
            "#[bridge(sync)] pub fn f(s: std::collections::HashSet<i64>) -> HashSet<String> { todo!() }",
        );
        assert_eq!(
            iface.functions[0].params[0].ty,
            Type::Set(Box::new(Type::I64), MapKind::Hash)
        );
        assert_eq!(
            iface.functions[0].ret,
            Some(Type::Set(Box::new(Type::String), MapKind::Hash))
        );
    }

    #[test]
    fn parses_btree_and_vecdeque_containers() {
        // BTreeMap/BTreeSet/VecDeque parse into the shared collection nodes,
        // tagged with the concrete Rust container kind.
        let iface = parse(
            r#"
            #[bridge(sync)] pub fn f(
                m: std::collections::BTreeMap<String, i64>,
                s: std::collections::BTreeSet<i64>,
                d: std::collections::VecDeque<i64>,
            ) {}
            "#,
        );
        let p = &iface.functions[0].params;
        assert_eq!(
            p[0].ty,
            Type::Map(Box::new(Type::String), Box::new(Type::I64), MapKind::BTree)
        );
        assert_eq!(p[1].ty, Type::Set(Box::new(Type::I64), MapKind::BTree));
        assert_eq!(p[2].ty, Type::List(Box::new(Type::I64), SeqKind::VecDeque));
        // The Dart type and wire are shared with the Hash/Vec forms — only the
        // kind tag differs.
        assert_ne!(
            p[0].ty,
            Type::Map(Box::new(Type::String), Box::new(Type::I64), MapKind::Hash)
        );
    }

    #[test]
    fn parses_tuples_into_records() {
        let iface = parse(
            r#"
            #[bridge(sync)] pub fn f(a: (i32, String), b: Vec<(i32, i32)>) -> ((i32, i32), i32) { todo!() }
            "#,
        );
        let p = &iface.functions[0].params;
        assert_eq!(p[0].ty, Type::Tuple(vec![Type::I32, Type::String]));
        assert_eq!(
            p[1].ty,
            Type::List(Box::new(Type::Tuple(vec![Type::I32, Type::I32])), SeqKind::Vec)
        );
        assert_eq!(
            iface.functions[0].ret,
            Some(Type::Tuple(vec![
                Type::Tuple(vec![Type::I32, Type::I32]),
                Type::I32
            ]))
        );
        // `()` unit return is not a tuple — it stays `ret: None`.
        let unit = parse("#[bridge(sync)] pub fn g() {}");
        assert_eq!(unit.functions[0].ret, None);
        // A 1-tuple is rejected loudly (no distinct Dart record).
        let err = parse_source("#[bridge] pub fn h(x: (i32,)) {}", "crate").unwrap_err();
        assert!(err.to_string().contains("1-tuples"), "{err}");
    }

    #[test]
    fn parses_std_time_types() {
        let iface = parse(
            "#[bridge(sync)] pub fn f(d: std::time::Duration, t: std::time::SystemTime) {}",
        );
        let p = &iface.functions[0].params;
        assert_eq!(p[0].ty, Type::Duration(DurationPeer::Std));
        assert_eq!(p[1].ty, Type::SystemTime(InstantPeer::Std));
        // Bare names (via `use`) resolve on the last segment too, and keep
        // meaning std — every bridge file in this repo writes them that way.
        let iface = parse("#[bridge(sync)] pub fn g(d: Duration) -> SystemTime { todo!() }");
        assert_eq!(
            iface.functions[0].params[0].ty,
            Type::Duration(DurationPeer::Std)
        );
        assert_eq!(
            iface.functions[0].ret,
            Some(Type::SystemTime(InstantPeer::Std))
        );
        // `core::time::Duration` is the same type by another path.
        let iface = parse("#[bridge(sync)] pub fn h(d: core::time::Duration) {}");
        assert_eq!(
            iface.functions[0].params[0].ty,
            Type::Duration(DurationPeer::Std)
        );
    }

    /// The peer type is selected by the path the author wrote — the whole
    /// point being that a chrono or `time` project is not pushed through
    /// `#[bridge(bytes(...))]`.
    #[test]
    fn selects_the_declared_time_peer() {
        let iface = parse(
            "#[bridge(sync)] pub fn f(a: chrono::Duration, b: chrono::TimeDelta, \
             c: time::Duration, d: chrono::DateTime<chrono::Utc>, e: time::OffsetDateTime) {}",
        );
        let p = &iface.functions[0].params;
        assert_eq!(p[0].ty, Type::Duration(DurationPeer::ChronoTimeDelta));
        assert_eq!(p[1].ty, Type::Duration(DurationPeer::ChronoTimeDelta));
        assert_eq!(p[2].ty, Type::Duration(DurationPeer::Time));
        assert_eq!(p[3].ty, Type::SystemTime(InstantPeer::ChronoUtc));
        assert_eq!(p[4].ty, Type::SystemTime(InstantPeer::TimeOffsetDateTime));

        // `DateTime<Utc>` and `TimeDelta`/`OffsetDateTime` are each unique to
        // one crate, so the unqualified spelling (the idiomatic one, under
        // `use chrono::{DateTime, Utc};`) resolves identically.
        let iface = parse(
            "#[bridge(sync)] pub fn g(a: DateTime<Utc>, b: TimeDelta, c: OffsetDateTime) {}",
        );
        let p = &iface.functions[0].params;
        assert_eq!(p[0].ty, Type::SystemTime(InstantPeer::ChronoUtc));
        assert_eq!(p[1].ty, Type::Duration(DurationPeer::ChronoTimeDelta));
        assert_eq!(p[2].ty, Type::SystemTime(InstantPeer::TimeOffsetDateTime));

        // Peers compose like any other value type.
        let iface = parse(
            "#[bridge(sync)] pub fn h(v: Vec<chrono::Duration>, o: Option<DateTime<Utc>>) {}",
        );
        let p = &iface.functions[0].params;
        assert_eq!(
            p[0].ty,
            Type::List(
                Box::new(Type::Duration(DurationPeer::ChronoTimeDelta)),
                SeqKind::Vec
            )
        );
        assert_eq!(
            p[1].ty,
            Type::Option(Box::new(Type::SystemTime(InstantPeer::ChronoUtc)))
        );
    }

    /// FR0045 — a time type we recognize but cannot map. Each message must
    /// name what to declare instead, because none of the three shapes FR0003
    /// offers fits any of these: a zone-less or monotonic reading still has
    /// nothing to put on the wire, wrapped or restated.
    #[test]
    fn rejects_unmappable_time_types() {
        for (src, needle) in [
            (
                "#[bridge(sync)] pub fn f(t: chrono::DateTime<chrono::Local>) {}",
                "DateTime<Local>",
            ),
            (
                "#[bridge(sync)] pub fn f(t: DateTime<FixedOffset>) {}",
                "DateTime<FixedOffset>",
            ),
            (
                "#[bridge(sync)] pub fn f(t: chrono::NaiveDateTime) {}",
                "wall-clock reading",
            ),
            (
                "#[bridge(sync)] pub fn f(t: time::PrimitiveDateTime) {}",
                "wall-clock reading",
            ),
            (
                "#[bridge(sync)] pub fn f(t: time::UtcDateTime) {}",
                "time::OffsetDateTime",
            ),
            (
                "#[bridge(sync)] pub fn f(t: std::time::Instant) {}",
                "has no epoch",
            ),
        ] {
            let err = parse_source(src, "crate").unwrap_err().to_string();
            assert!(err.contains("FR0045"), "{src}: {err}");
            assert!(err.contains(needle), "{src}: {err}");
        }
        // A bare `DateTime` with no type argument is NOT captured here: only
        // `DateTime<Tz>` is, so the parser hands it on as an ordinary named
        // type. (The *checker* still refuses to declare one, because the
        // generated Dart class would shadow `dart:core`'s `DateTime` — a
        // different rule, in a different pass, for a different reason:
        // FR0046's shadow arm.)
        let iface = parse(
            "#[bridge(data)] pub struct DateTime { pub x: i64 } \
             #[bridge(sync)] pub fn f(t: DateTime) -> DateTime { t }",
        );
        assert_eq!(
            iface.functions[0].params[0].ty,
            Type::Named("DateTime".into())
        );
    }

    /// A `Duration` under a root the parser does not know is not quietly given
    /// std's *unsigned* contract. The three it knows disagree about sign, so
    /// guessing is the one thing it must not do.
    #[test]
    fn an_unplaceable_duration_root_is_rejected() {
        let err = parse_source(
            "#[bridge(sync)] pub fn f(d: tokio::time::Duration) {}",
            "crate",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("FR0045"), "{err}");
        assert!(err.contains("disagree about sign"), "{err}");

        // The message names tokio because tokio's `Duration` *is* std's,
        // re-exported — the commonest way to arrive here, and a case where
        // "wrap it in bytes" would be absurd advice on its own.
        assert!(err.contains("tokio::time::Duration"), "{err}");

        // A foreign root on a name unique to one crate is not captured at all
        // — it is somebody else's type, and resolves (and fails) as one. This
        // holds for the *rejected* spellings too, which is the half the review
        // caught: they were matched on the last segment alone, so a stranger's
        // `UtcDateTime` was refused with advice about the `time` crate's.
        for src in [
            "#[bridge(sync)] pub fn f(d: crate::model::TimeDelta) {}",
            "#[bridge(sync)] pub fn f(d: crate::model::UtcDateTime) {}",
            "#[bridge(sync)] pub fn f(d: quanta::Instant) {}",
            "#[bridge(sync)] pub fn f(d: mycrate::NaiveDateTime) {}",
        ] {
            let ds = crate::check::check(parse(src)).unwrap_err();
            assert_eq!(
                ds.iter().map(|d| d.code).collect::<Vec<_>>(),
                vec!["FR0003"],
                "{src}"
            );
        }
    }

    /// FR0047, the bare-name residue: `use chrono::Duration;` makes a bare
    /// `Duration` mean chrono's to *rustc* and std's to the parser. Reported
    /// here, before the type error lands inside generated code — as a warning,
    /// because the import may serve helper code the bridge never looks at.
    #[test]
    fn warns_when_a_use_rebinds_a_bare_time_name() {
        let w = warnings(
            "use chrono::Duration;\n#[bridge(sync)] pub fn f(d: Duration) -> Duration { d }",
        );
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("FR0047"), "{w:?}");
        assert!(w[0].contains("use chrono::Duration;"), "{w:?}");
        assert!(w[0].contains("read as `std::time::Duration`"), "{w:?}");
        // Grouped imports are read through, and the owning crate is silent.
        assert_eq!(
            warnings("use std::time::{Duration, SystemTime};\n#[bridge(sync)] pub fn f() {}").len(),
            0
        );
        assert_eq!(
            warnings("use chrono::{TimeDelta, Utc};\n#[bridge(sync)] pub fn f() {}").len(),
            0
        );
        // `time::Duration` diverges from std's meaning; `time::OffsetDateTime`
        // does not, so exactly one of the two is reported.
        assert_eq!(
            warnings("use time::{Duration, OffsetDateTime};\n#[bridge(sync)] pub fn f() {}").len(),
            1
        );
        // The names owned by a foreign crate diverge under any other root.
        let w = warnings("use crate::model::TimeDelta;\n#[bridge(sync)] pub fn f() {}");
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("read as `chrono::TimeDelta`"), "{w:?}");
        // A rename *away from* a watched name leaves the bare spelling meaning
        // what it meant, so there is nothing to warn about...
        assert_eq!(
            warnings("use chrono::Duration as Delta;\n#[bridge(sync)] pub fn f() {}").len(),
            0
        );
        // ...but a rename *onto* one is the same hazard as importing it
        // directly: the bound name is what a signature writes. The rule is
        // about the name a `use` binds, never the path's last segment.
        let w = warnings("use chrono::TimeDelta as Duration;\n#[bridge(sync)] pub fn f() {}");
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("FR0047"), "{w:?}");
        assert!(w[0].contains("chrono::TimeDelta as Duration"), "{w:?}");
        assert!(w[0].contains("read as `std::time::Duration`"), "{w:?}");
    }

    #[test]
    fn parses_struct_and_enum() {
        let iface = parse(
            r#"
            #[bridge(data)]
            pub struct Point { pub x: f64, pub y: f64 }

            #[bridge(data)]
            pub enum Patch {
                Delete { index: usize, length: usize },
                Insert(String),
                Clear,
            }
            "#,
        );
        assert_eq!(iface.structs[0].name, "Point");
        assert_eq!(iface.structs[0].fields.len(), 2);
        let e = &iface.enums[0];
        assert_eq!(e.variants[0].fields.len(), 2);
        assert_eq!(e.variants[1].fields[0].name, "field0");
        assert!(e.variants[2].fields.is_empty());
        // Tuple-ness is recorded from the syntax, not sniffed from field names.
        assert!(!e.variants[0].tuple, "Delete is a named variant");
        assert!(e.variants[1].tuple, "Insert is a tuple variant");
        assert!(!e.variants[2].tuple, "Clear is a unit variant");
    }

    /// FR0058: a bridged type declaration with no representation at all —
    /// bare `#[bridge]`, or `#[bridge(...)]` carrying only sibling flags — is
    /// refused on every kind of type declaration, naming all six keywords.
    #[test]
    fn a_type_declaration_with_no_representation_is_rejected() {
        for src in [
            "#[bridge] pub struct S { x: i64 }",
            "#[bridge(no_eq)] pub struct S { x: i64 }",
            "#[bridge] pub enum E { A }",
            "#[bridge] pub trait T { fn f(&self); }",
        ] {
            let err = parse_source(src, "crate::api").unwrap_err();
            let msg = err.root_cause().to_string();
            assert!(msg.contains("FR0058"), "{src}: {msg}");
            assert!(msg.contains("must name its representation"), "{src}: {msg}");
            for kw in ["data", "confined", "resident", "frozen", "locked", "actor"] {
                assert!(msg.contains(kw), "{src}: {msg} (missing `{kw}`)");
            }
            assert!(
                msg.contains("Functions and impl blocks are unaffected"),
                "{src}: {msg}"
            );
        }
        // `bytes(...)` is its own representation and needs no `data` beside
        // it — unaffected by FR0058.
        parse_source(
            r#"#[bridge(bytes(dart = "X", import = "x.dart"))] pub struct B { x: i64 }"#,
            "crate::api",
        )
        .unwrap();
    }

    /// `data` and a concurrency model on the same declaration: one Rust
    /// struct, two declarations, in either write order.
    #[test]
    fn data_and_a_model_together_declare_both_halves() {
        for src in [
            "#[bridge(data, locked)] pub struct S { x: i64 }",
            "#[bridge(locked, data)] pub struct S { x: i64 }",
        ] {
            let iface = parse_source(src, "crate::api").unwrap();
            assert_eq!(iface.structs.len(), 1, "{src}");
            assert_eq!(iface.opaques.len(), 1, "{src}");
            assert_eq!(iface.structs[0].name, "S", "{src}");
            assert_eq!(iface.opaques[0].name, "S", "{src}");
            assert_eq!(iface.opaques[0].model, Model::Locked, "{src}");
            // Same Rust item, so the same module path — which is what makes
            // the pair one declaration to FR0002 rather than a duplicate.
            assert_eq!(
                iface.structs[0].module_path, iface.opaques[0].module_path,
                "{src}"
            );
        }
    }

    /// FR0060: two concurrency models on the same declaration — contradictory
    /// object shapes, and unlike `data` alongside a model they cannot both
    /// hold of one type at once.
    #[test]
    fn two_models_together_are_rejected() {
        let err = parse_source("#[bridge(confined, locked)] pub struct S { x: i64 }", "crate::api")
            .unwrap_err();
        let msg = err.root_cause().to_string();
        assert!(msg.contains("FR0060"), "{msg}");
        assert!(msg.contains("at most one concurrency model"), "{msg}");
    }

    /// FR0061: `data` on a trait — a trait has no fields to copy, so only a
    /// concurrency model is a legal representation for one.
    #[test]
    fn data_on_a_trait_is_rejected() {
        let err = parse_source("#[bridge(data)] pub trait T { fn f(&self); }", "crate::api")
            .unwrap_err();
        let msg = err.root_cause().to_string();
        assert!(msg.contains("FR0061"), "{msg}");
        assert!(msg.contains("no fields to copy"), "{msg}");
    }

    /// `bytes(...)` stays mutually exclusive with every representation
    /// keyword, `data` included — the pre-existing bytes/opaque exclusivity
    /// rule extended to cover the new keyword rather than a fresh code.
    #[test]
    fn bytes_and_data_together_are_rejected() {
        let err = parse_source(
            r#"#[bridge(bytes(dart = "X", import = "x.dart"), data)] pub struct B { x: i64 }"#,
            "crate::api",
        )
        .unwrap_err();
        let msg = err.root_cause().to_string();
        assert!(msg.contains("mutually exclusive"), "{msg}");
    }

    #[test]
    fn parse_error_reports_line_and_column() {
        // A syntax error on line 4 (the unmatched `(`). The diagnostic must
        // name the location, not just say "failed to parse".
        let src = "#[bridge(sync)]\npub fn ok() {}\n\npub fn bad( -> i64 { 0 }\n";
        let err = parse_source(src, "crate::api").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("line 4"), "{msg}");
        assert!(msg.contains("column"), "{msg}");
    }

    #[test]
    fn named_variant_field_called_field0_is_recorded_as_named() {
        // A real named field literally called `field0` must not be mistaken for
        // a synthesized tuple-field name: the variant is *named*, tuple false.
        let iface = parse(
            r#"
            #[bridge(data)]
            pub enum E {
                V { field0: i64 },
            }
            "#,
        );
        let v = &iface.enums[0].variants[0];
        assert_eq!(v.fields[0].name, "field0");
        assert!(!v.tuple, "V {{ field0: i64 }} is a named variant, not a tuple");
    }

    /// The keyword, the marker type and the `impl` self type — resident is
    /// spelled everywhere its four siblings are, and resolves to the same
    /// `Type` a bare name does (FR0062 checks the claim, then erases it).
    #[test]
    fn resident_is_spelled_wherever_a_model_is() {
        let iface = parse(
            r#"
            #[bridge(resident)]
            pub struct Scene { nodes: std::rc::Rc<Vec<String>> }

            #[bridge]
            impl Resident<Scene> {
                #[bridge(sync)]
                pub fn count(&self) -> i64 { todo!() }
            }

            #[bridge(sync)]
            pub fn peek(s: &Resident<Scene>) -> i64 { todo!() }
            "#,
        );
        assert_eq!(iface.opaques[0].model, Model::Resident);
        assert_eq!(
            iface.functions[0].parent_claim,
            Some(Claim::Model(Model::Resident)),
            "the impl block's self type carries the claim"
        );
    }

    /// `resident` is a model like the others, so two models is still FR0060
    /// and the message names it among the mutually exclusive set.
    #[test]
    fn resident_beside_another_model_is_refused() {
        let err = parse_source(
            "#[bridge(resident, locked)] pub struct S { x: i64 }",
            "crate::api",
        )
        .unwrap_err();
        let msg = err.root_cause().to_string();
        assert!(msg.contains("FR0060"), "{msg}");
        assert!(msg.contains("resident"), "{msg}");
    }

    /// A bare `Resident` with no argument is the user's own type, not the
    /// marker — the same rule that keeps `Vec`/`Box`/`Option` reserved only in
    /// their one-argument generic form.
    #[test]
    fn a_bare_resident_name_is_not_the_marker() {
        let iface = parse(
            r#"
            #[bridge(data)]
            pub struct Resident { pub n: i64 }

            #[bridge(sync)]
            pub fn take_it(r: Resident) -> i64 { todo!() }
            "#,
        );
        assert_eq!(iface.functions[0].params[0].ty, Type::Named("Resident".into()));
    }

    #[test]
    fn parses_opaque_with_methods() {
        let iface = parse(
            r#"
            #[bridge(confined)]
            pub struct Doc { inner: automerge::Automerge }

            #[bridge(sync)]
            impl Doc {
                pub fn new() -> Self { todo!() }
                pub fn len(&self) -> usize { todo!() }
                pub fn splice(&mut self, pos: usize, del: isize, text: String) -> anyhow::Result<()> { todo!() }
            }
            "#,
        );
        assert_eq!(iface.opaques[0].model, Model::Confined);
        let ctor = &iface.functions[0];
        // `is_constructor` is the checker's call now — the comparison is
        // against the *resolved* return type, which a representation marker
        // hides until `resolve_type` erases it. The parser records the shape.
        assert!(!ctor.is_constructor);
        assert_eq!(ctor.ret, Some(Type::Named("Doc".into())));
        assert!(ctor.receiver.is_none());
        assert_eq!(iface.functions[1].receiver, Some(Receiver::Ref));
        let splice = &iface.functions[2];
        assert_eq!(splice.receiver, Some(Receiver::RefMut));
        assert!(splice.fallible);
        assert_eq!(splice.ret, None);
        // sync inherited from the impl block
        assert!(iface.functions.iter().all(|f| f.exec == Exec::Sync));
    }

    #[test]
    fn parses_on_contention() {
        let iface = parse(
            r#"
            #[bridge(locked)]
            pub struct Cache { m: std::collections::HashMap<String, String> }

            impl Cache {
                #[bridge(sync, on_contention = "error")]
                pub fn get(&self, k: String) -> Option<String> { todo!() }
            }
            "#,
        );
        // Note: un-annotated impl blocks are not scanned; this impl block has
        // no #[bridge] so nothing is collected.
        assert!(iface.functions.is_empty());
    }

    #[test]
    fn parses_method_level_attr_inside_bridged_impl() {
        let iface = parse(
            r#"
            #[bridge(locked)]
            pub struct Cache { x: i32 }

            #[bridge]
            impl Cache {
                #[bridge(sync, on_contention = "error")]
                pub fn get(&self, k: String) -> Option<String> { todo!() }
                pub fn put(&mut self, k: String, v: String) {}
            }
            "#,
        );
        let get = &iface.functions[0];
        assert_eq!(get.exec, Exec::Sync);
        assert_eq!(get.on_contention, Some(OnContention::Error));
        let put = &iface.functions[1];
        assert_eq!(put.exec, Exec::Async);
        assert_eq!(put.on_contention, None);
    }

    #[test]
    fn parses_u64() {
        let iface = parse("#[bridge] pub fn f(x: u64) -> u64 { x }");
        let f = &iface.functions[0];
        assert_eq!(f.params[0].ty, Type::U64);
        assert_eq!(f.ret, Some(Type::U64));
    }

    #[test]
    fn parses_char() {
        let iface = parse("#[bridge] pub fn f(c: char) -> char { c }");
        let f = &iface.functions[0];
        assert_eq!(f.params[0].ty, Type::Char);
        assert_eq!(f.ret, Some(Type::Char));
    }

    #[test]
    fn parses_i128_u128() {
        let iface = parse("#[bridge] pub fn f(x: i128) -> u128 { x as u128 }");
        let f = &iface.functions[0];
        assert_eq!(f.params[0].ty, Type::I128);
        assert_eq!(f.ret, Some(Type::U128));
    }

    #[test]
    fn accepts_rust_async_fn_as_pool_executed() {
        // `async fn` is accepted: recorded as `rust_async`, always
        // Exec::Async (pool-executed; Dart awaits the Future).
        let iface = parse("#[bridge] pub async fn f(x: i64) -> i64 { x }");
        let f = &iface.functions[0];
        assert!(f.rust_async);
        assert_eq!(f.exec, Exec::Async);
        // A plain (non-async) fn is not rust_async.
        let iface = parse("#[bridge] pub fn g() {}");
        assert!(!iface.functions[0].rust_async);
    }

    #[test]
    fn parses_native_only_and_or_merges_it_from_the_block() {
        let iface = parse("#[bridge(sync, native_only)] pub fn dial() -> i64 { 0 }");
        assert!(iface.functions[0].native_only);
        assert!(!parse("#[bridge(sync)] pub fn g() {}").functions[0].native_only);

        // OR-merged, not replaced: `f` carries its own #[bridge(...)], which
        // for every *other* option discards the block-level attribute.
        let iface = parse(
            r#"
            #[bridge(frozen)] pub struct C { x: i32 }
            #[bridge(native_only)] impl C {
                #[bridge(sync)] pub fn f(&self) -> i64 { 0 }
                pub fn g(&self) -> i64 { 0 }
            }
            "#,
        );
        for name in ["f", "g"] {
            let m = iface.functions.iter().find(|f| f.name == name).unwrap();
            assert!(m.native_only, "{name}");
        }
        // Same rule through a trait declaration.
        let iface = parse(
            "#[bridge(frozen, native_only)] pub trait T: Send + Sync { \
             #[bridge(sync)] fn f(&self) -> i64; }",
        );
        assert!(iface.functions[0].native_only);
        assert!(iface.opaques[0].native_only, "recorded on the type too");
    }

    #[test]
    fn a_cfg_gated_data_type_is_rejected_naming_the_only_fix_it_has() {
        // The same hazard FR0034 catches for members and opaque types, caught
        // here instead because the remedy differs: a data type has no
        // `native_only` to declare, so the message must say "remove the cfg"
        // rather than offer a flag the parser would reject.
        for (src, kind) in [
            ("#[cfg(unix)]\n#[bridge(data)] pub struct P { x: i32 }", "struct"),
            ("#[cfg(unix)]\n#[bridge(data)] pub enum E { A, B }", "enum"),
            (
                "#[cfg(unix)]\n#[bridge(bytes(dart = \"M\", import = \"pkg:m/m.dart\"))] \
                 pub struct M { x: i32 }",
                "struct",
            ),
        ] {
            let err = parse_source(src, "crate::api").unwrap_err().to_string();
            assert!(err.contains("cannot be `#[cfg]`-gated"), "{kind}: {err}");
            assert!(
                err.contains("no `native_only` to declare"),
                "must not offer a flag that is rejected here: {err}"
            );
        }
        // Ungated data types are unaffected, and `cfg_attr` is not a gate.
        assert!(parse_source("#[bridge(data)] pub struct P { x: i32 }", "crate::api").is_ok());
        assert!(parse_source(
            "#[cfg_attr(test, derive(Default))]\n#[bridge(data)] pub struct P { x: i32 }",
            "crate::api"
        )
        .is_ok());
    }

    #[test]
    fn native_only_is_rejected_where_it_cannot_bite() {
        // A value type crosses by value on every target: there is no web
        // presence to remove, so the flag is a contract that cannot fire.
        for src in [
            "#[bridge(data)] pub struct P { x: i32 }",
            "#[bridge(data)] pub enum E { A, B }",
            "#[bridge(bytes(dart = \"Msg\", import = \"pkg:m/m.dart\"))] pub struct M { x: i32 }",
        ] {
            let with_flag = src.replacen("#[bridge(", "#[bridge(native_only, ", 1);
            let with_flag = if with_flag == src {
                src.replace("#[bridge]", "#[bridge(native_only)]")
            } else {
                with_flag
            };
            let err = parse_source(&with_flag, "crate::api").unwrap_err().to_string();
            assert!(err.contains("native_only is only valid on"), "{with_flag}: {err}");
        }
    }

    #[test]
    fn parses_web_runtime_fail_opt_in() {
        let iface = parse(
            r#"
            #[bridge(locked)] pub struct Cache { x: i32 }
            #[bridge] impl Cache {
                #[bridge(sync, on_contention = "block", web = "runtime_fail")]
                pub fn blocking_read(&self) -> i32 { 0 }
            }
            "#,
        );
        let f = iface.functions.iter().find(|f| f.name == "blocking_read").unwrap();
        assert!(f.web_runtime_fail);
        assert_eq!(f.on_contention, Some(OnContention::Block));
        // Default: absent attribute leaves the flag off.
        let iface = parse("#[bridge(sync)] pub fn g() {}");
        assert!(!iface.functions[0].web_runtime_fail);
    }

    #[test]
    fn impl_level_web_policy_is_inherited_by_methods() {
        // Like `sync`/`on_contention`, an impl-level `web` propagates to
        // methods that carry no own #[bridge].
        let iface = parse(
            r#"
            #[bridge(locked)] pub struct Cache { x: i32 }
            #[bridge(web = "runtime_fail")]
            impl Cache {
                pub fn refine(&self, f: DartFunction<i64, i64>) -> i64 { 0 }
            }
            "#,
        );
        assert!(iface.functions.iter().find(|f| f.name == "refine").unwrap().web_runtime_fail);
    }

    /// A representation keyword on a function or an impl block described
    /// nothing and was silently dropped. The short spellings make it easy to
    /// reach for — `#[bridge(locked)] pub fn` reads like "this one takes the
    /// lock" — so it has to say no rather than quietly agree.
    #[test]
    fn rejects_a_representation_keyword_where_nothing_crosses() {
        for src in [
            "#[bridge(locked)] pub fn f() {}",
            "#[bridge(data)] pub fn f() {}",
            "#[bridge(frozen, sync)] pub fn f() -> i64 { 0 }",
        ] {
            let err = parse_source(src, "crate").unwrap_err().to_string();
            assert!(err.contains("FR0063"), "{src}: {err}");
            assert!(err.contains("does not cross"), "{src}: {err}");
        }
        // ...and the same keywords on the type declaration are untouched.
        assert!(parse_source("#[bridge(locked)] pub struct D { x: i32 }", "crate").is_ok());
        assert!(parse_source("#[bridge(data)] pub struct P { pub x: i32 }", "crate").is_ok());
        assert!(parse_source("#[bridge(sync)] pub fn f() {}", "crate").is_ok());
    }

    #[test]
    fn rejects_unknown_web_policy() {
        let err =
            parse_source("#[bridge(web = \"omit\")] pub fn f() {}", "crate").unwrap_err();
        assert!(err.to_string().contains("unknown web policy"), "{err}");
        // A non-string value is also loud.
        let err =
            parse_source("#[bridge(web = 1)] pub fn f() {}", "crate").unwrap_err();
        assert!(err.to_string().contains("web expects a string"), "{err}");
    }

    #[test]
    fn rejects_sync_async_fn() {
        // A sync member runs on the caller and returns a plain value, so it
        // cannot await — `#[bridge(sync)] async fn` is a contradiction.
        let err =
            parse_source("#[bridge(sync)] pub async fn f() {}", "crate").unwrap_err();
        assert!(err.root_cause().to_string().contains("cannot be combined"), "{err}");
        assert!(err.root_cause().to_string().contains("async fn"), "{err}");
    }

    /// A borrowed return is recorded, not refused: the value is copied into
    /// the response, so the reference is read where it still points at
    /// something. `&mut` has nothing to mean there and is refused.
    #[test]
    fn a_borrowed_return_is_recorded() {
        let iface = parse(
            "#[bridge(frozen)] pub struct S { w: Vec<String> } \
             #[bridge] impl S { #[bridge(sync)] pub fn first(&self) -> &str { \"\" } }",
        );
        let f = &iface.functions[0];
        assert!(f.ret_borrow);
        assert_eq!(f.ret, Some(Type::String));
        // Through a Result, which is the other root.
        let iface = parse(
            "#[bridge(frozen)] pub struct S { w: Vec<String> } \
             #[bridge] impl S { #[bridge(sync)] pub fn first(&self) -> Result<&str> { todo!() } }",
        );
        assert!(iface.functions[0].ret_borrow && iface.functions[0].fallible);
        // By value, unchanged.
        let iface = parse("#[bridge(sync)] pub fn f() -> String { String::new() }");
        assert!(!iface.functions[0].ret_borrow);
        // `&mut` has nothing to write back through.
        let err = parse_source(
            "#[bridge(frozen)] pub struct S { w: Vec<String> } \
             #[bridge] impl S { #[bridge(sync)] pub fn f(&self) -> &mut Vec<String> { todo!() } }",
            "crate::api",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("copied into the response"), "{err}");
    }

    /// FR0063 reached a free function and an impl block but not the methods
    /// *inside* one — and `parse_fn` never reads `attr.model`, so the keyword
    /// parsed, was accepted, and vanished. That is the silent drop the rule
    /// exists to prevent, one call site short of covering it.
    #[test]
    fn a_representation_keyword_on_a_method_is_refused() {
        for src in [
            "#[bridge(locked)] pub struct D { x: i64 } \
             #[bridge] impl D { #[bridge(locked, sync)] pub fn n(&self) -> i64 { 0 } }",
            "#[bridge(data)] pub struct P { pub x: i64 } \
             #[bridge] impl P { #[bridge(data, sync)] pub fn n(&self) -> i64 { 0 } }",
            "#[bridge(confined)] pub trait T: Send { \
               #[bridge(confined, sync)] fn n(&self) -> i64; }",
        ] {
            let err = parse_source(src, "crate::api").unwrap_err().to_string();
            assert!(err.contains("FR0063"), "{src}: {err}");
            // The message names the spelling that *does* say which generated
            // class a member lands on, rather than deferring the question.
            assert!(err.contains("impl Locked<Doc>"), "{src}: {err}");
        }
        // The keyword on the type, and nowhere else, stays fine.
        assert!(parse_source(
            "#[bridge(locked)] pub struct D { x: i64 } #[bridge] impl D { \
               #[bridge(sync, on_contention = \"error\")] pub fn n(&self) -> i64 { 0 } }",
            "crate::api",
        )
        .is_ok());
    }

    /// `Self` in a **parameter** is the impl block's self type, at every
    /// depth — the same reading the return takes, so the two agree.
    #[test]
    fn self_as_a_parameter_type_is_the_impl_type_at_every_depth() {
        let p = || Box::new(Type::Named("P".into()));
        for (param, want) in [
            ("Self", Type::Named("P".into())),
            ("&Self", Type::Named("P".into())),
            ("Vec<Self>", Type::List(p(), SeqKind::Vec)),
            ("Option<Self>", Type::Option(p())),
            (
                "Vec<Option<Self>>",
                Type::List(Box::new(Type::Option(p())), SeqKind::Vec),
            ),
            // A nested borrow's referent is an ordinary type at that depth.
            (
                "Vec<&Self>",
                Type::List(
                    Box::new(Type::Ref {
                        inner: p(),
                        mutable: false,
                        unsized_borrow: false,
                    }),
                    SeqKind::Vec,
                ),
            ),
        ] {
            let iface = parse(&format!(
                "#[bridge(data)] pub struct P {{ pub x: i64 }} \
                 #[bridge] impl P {{ #[bridge(sync)] pub fn f(&self, o: {param}) {{}} }}"
            ));
            assert_eq!(iface.functions[0].params[0].ty, want, "{param}");
        }
        // The return position keeps its substitution, which is what makes a
        // constructor recognisable at all.
        let iface = parse(
            "#[bridge(confined)] pub struct D { x: i64 } \
             #[bridge] impl D { #[bridge(sync)] pub fn new() -> Self { todo!() } }",
        );
        assert_eq!(iface.functions[0].ret, Some(Type::Named("D".into())));
    }

    /// `Self` is the self type **as written**, marker included: in
    /// `impl Locked<Doc>` it is the handle half and in `impl Data<Doc>` the
    /// value half. Sound because every marker is an identity alias
    /// (`pub type Locked<T> = T;`), so rustc and frustrate agree about the Rust
    /// type and differ only about the Dart class the block already declared.
    #[test]
    fn self_carries_the_impl_blocks_marker() {
        let dual = "#[bridge(data(dart_identifier = \"DocValue\"), locked)] \
                    pub struct Doc { pub id: i64 } ";
        let iface = parse(&format!(
            "{dual} #[bridge] impl Data<Doc> {{ \
                 #[bridge(sync)] pub fn f(&self, o: Vec<Self>) -> Self {{ todo!() }} }}"
        ));
        let claimed =
            |t: Type| Type::Claimed(Claim::Data, Box::new(t));
        assert_eq!(
            iface.functions[0].params[0].ty,
            Type::List(Box::new(claimed(Type::Named("Doc".into()))), SeqKind::Vec)
        );
        assert_eq!(iface.functions[0].ret, Some(claimed(Type::Named("Doc".into()))));
        let iface = parse(&format!(
            "{dual} #[bridge] impl Locked<Doc> {{ \
                 #[bridge(sync, on_contention = \"error\")] pub fn f(&self, o: Self) {{}} }}"
        ));
        assert_eq!(
            iface.functions[0].params[0].ty,
            Type::Claimed(Claim::Model(Model::Locked), Box::new(Type::Named("Doc".into())))
        );
        // A block that wrote no marker leaves `Self` bare, which is what
        // FR0067 then reports (the block is unplaced anyway).
        let iface = parse(
            "#[bridge(confined)] pub struct D { x: i64 } \
             #[bridge] impl D { #[bridge(sync)] pub fn f(&self, o: Self) {} }",
        );
        assert_eq!(iface.functions[0].params[0].ty, Type::Named("D".into()));
    }

    /// `Self` in a return names the impl's type wherever it is written, not
    /// only at the root. rustc reads it that way, and the substitution used to
    /// stop at the top level for no stated reason — so `-> Option<Self>`
    /// reached the checker as an unknown type literally called `Self`.
    #[test]
    fn self_in_a_return_substitutes_at_every_depth() {
        let d = || Box::new(Type::Named("D".into()));
        for (ret, want) in [
            ("Self", Type::Named("D".into())),
            ("Option<Self>", Type::Option(d())),
            ("Vec<Self>", Type::List(d(), SeqKind::Vec)),
            ("Result<Option<Self>, String>", Type::Option(d())),
            (
                "Vec<Option<Self>>",
                Type::List(Box::new(Type::Option(d())), SeqKind::Vec),
            ),
            (
                "(Self, i64)",
                Type::Tuple(vec![Type::Named("D".into()), Type::I64]),
            ),
            (
                "HashMap<String, Self>",
                Type::Map(Box::new(Type::String), d(), MapKind::Hash),
            ),
        ] {
            let iface = parse(&format!(
                "#[bridge(confined)] pub struct D {{ x: i64 }} \
                 #[bridge] impl D {{ #[bridge(sync)] pub fn f() -> {ret} {{ todo!() }} }}"
            ));
            assert_eq!(iface.functions[0].ret.as_ref(), Some(&want), "-> {ret}");
        }
        // A member with no impl to substitute against still says so, at every
        // depth — the name would otherwise reach the checker as an FR0003
        // about a type nobody declared.
        for ret in ["Self", "Option<Self>", "Vec<Self>"] {
            let err = parse_source(
                &format!("#[bridge(sync)] pub fn f() -> {ret} {{ todo!() }}"),
                "crate::api",
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("Self return outside an impl block"), "{ret}: {err}");
        }
    }

    /// A field's `Self` is the declaring type, the same rule and the same
    /// substitution — `struct Tree { children: Vec<Self> }` is the idiomatic
    /// recursive spelling and compiles in Rust, but reached the checker as an
    /// unknown type called `Self`.
    #[test]
    fn self_in_a_field_names_the_declaring_type() {
        let iface = parse(
            "#[bridge(data)] pub struct Tree { pub children: Vec<Self>, pub v: i64 } \
             #[bridge(data)] pub enum Expr { Lit(i64), Add(Vec<Self>) } \
             #[bridge(sync)] pub fn f(t: Tree, e: Expr) {}",
        );
        assert_eq!(
            iface.structs[0].fields[0].ty,
            Type::List(Box::new(Type::Named("Tree".into())), SeqKind::Vec)
        );
        assert_eq!(
            iface.enums[0].variants[1].fields[0].ty,
            Type::List(Box::new(Type::Named("Expr".into())), SeqKind::Vec)
        );
    }

    /// The struct shape is read from the syntax, not re-derived from the
    /// field names: a tuple's are synthesized `field{i}` like a tuple
    /// variant's, and a real named field called `field0` must not be mistaken
    /// for one.
    #[test]
    fn struct_shape_is_read_from_the_syntax() {
        let iface = parse(
            "#[bridge(data)] pub struct N { x: i32 } #[bridge(data)] pub struct T(i32, String); \
             #[bridge(data)] pub struct U; #[bridge(data)] pub struct E {}",
        );
        let shapes: Vec<(StructShape, Vec<&str>)> = iface
            .structs
            .iter()
            .map(|s| (s.shape, s.fields.iter().map(|f| f.name.as_str()).collect()))
            .collect();
        assert_eq!(
            shapes,
            vec![
                (StructShape::Named, vec!["x"]),
                (StructShape::Tuple, vec!["field0", "field1"]),
                (StructShape::Unit, vec![]),
                (StructShape::Named, vec![]),
            ]
        );
    }

    /// A receiver written without the `&` shorthand — by value or explicitly
    /// typed — is recorded as `Receiver::Value`, never as a constructor, and
    /// left for the checker to refuse (FR0057; see `check::tests`).
    #[test]
    fn a_non_borrow_receiver_is_read_far_enough_to_tell_box_self_apart() {
        let iface = parse(
            "#[bridge(frozen)] pub struct O; \
             #[bridge] impl O { \
                 pub fn a(self) -> O { self } \
                 pub fn b(mut self) {} \
                 pub fn c(self: Box<Self>) {} \
                 pub fn d(self: &Self) {} \
                 pub fn e(self: std::rc::Rc<Self>) {} \
                 pub fn f(self: Box<Other>) {} \
             }",
        );
        let recv = |n: &str| {
            iface
                .functions
                .iter()
                .find(|f| f.name == n)
                .unwrap_or_else(|| panic!("{n}"))
                .receiver
        };
        assert_eq!(recv("a"), Some(Receiver::Value));
        assert_eq!(recv("b"), Some(Receiver::Value));
        assert_eq!(recv("c"), Some(Receiver::Boxed));
        // Everything else is `Typed`: the parser does not read the type, so
        // the checker refuses it rather than half-understanding it. A `Box`
        // of something that is not `Self` is one of those.
        for n in ["d", "e", "f"] {
            assert_eq!(recv(n), Some(Receiver::Typed), "{n}");
        }
        for f in &iface.functions {
            assert!(!f.is_constructor, "{}", f.name);
        }
    }

    /// Generic parameters are recorded, not judged, here — type and const
    /// names in order, lifetimes dropped, and the two kinds kept apart because
    /// only a type parameter can be bound by an instantiation.
    ///
    /// A parameterized data declaration is a **template** and lands in
    /// `generic_structs`/`generic_enums`, not in `structs`/`enums`: it has no
    /// fields the wire can describe until `check` binds its parameters.
    #[test]
    fn generic_params_are_recorded_without_lifetimes() {
        let iface = parse(
            "#[bridge(data)] pub struct Cache<'a, T, const N: usize> { x: i32 } \
             #[bridge(data)] pub enum E<T> { A } \
             #[bridge(sync)] pub fn f<'a, T>(x: &'a str) {}",
        );
        assert!(iface.structs.is_empty());
        assert!(iface.enums.is_empty());
        assert_eq!(iface.generic_structs[0].generics, vec!["T"]);
        assert_eq!(iface.generic_structs[0].const_generics, vec!["N"]);
        assert_eq!(iface.generic_enums[0].generics, vec!["T"]);
        assert_eq!(iface.functions[0].generics, vec!["T"]);
        // A default is recorded too — it is what makes a bare `Cache` mean
        // something the bridge cannot honour (FR0056).
        let iface = parse("#[bridge(data)] pub struct D<T = i64> { x: i32 }");
        assert_eq!(iface.generic_structs[0].defaulted_generics, vec!["T"]);
    }

    /// A generic **handle** or `bytes(...)` declaration is refused where it is
    /// read, because both record a bare name: before this the parameters were
    /// dropped in silence and the generated Rust named `crate::api::Store` for
    /// a `Store<T>` (E0107, inside generated code).
    #[test]
    fn a_generic_handle_or_extern_declaration_is_refused() {
        for src in [
            "#[bridge(confined)] pub struct Store<T> { v: Vec<T> }",
            "#[bridge(frozen)] pub struct Store<T> { v: Vec<T> }",
            "#[bridge(bytes(dart = \"B\", import = \"package:x/x.dart\"))] pub struct W<T>(pub T);",
        ] {
            let e = parse_source(src, "crate::api").unwrap_err().to_string();
            assert!(e.contains("FR0056"), "{src}: {e}");
            assert!(e.contains("<T>"), "{src}: {e}");
        }
    }

    /// An `impl` on a generic **data** type is read, not refused: the self
    /// type's arguments ride on `parent_args` and `check`'s expansion binds
    /// them. `parent` stays the bare head, which is what the expansion looks
    /// the template up by.
    #[test]
    fn an_impl_on_a_generic_data_type_records_its_self_type_arguments() {
        let iface = parse_source(
            "#[bridge(data)] pub struct P<T> { x: T } \
             #[bridge] impl P<i64> { #[bridge(sync)] pub fn m(&self) {} }",
            "crate::api",
        )
        .unwrap();
        let f = &iface.functions[0];
        assert_eq!(f.parent.as_deref(), Some("P"));
        assert_eq!(f.parent_args, vec![Type::I64]);
        assert!(f.parent_generics.is_empty());

        let iface = parse_source(
            "#[bridge(data)] pub struct P<T> { x: T } \
             #[bridge] impl<T> P<T> { #[bridge(sync)] pub fn m(&self) -> Self { todo!() } }",
            "crate::api",
        )
        .unwrap();
        let f = &iface.functions[0];
        assert_eq!(f.parent_args, vec![Type::Named("T".into())]);
        assert_eq!(f.parent_generics, vec!["T"]);
        // A **const** block parameter is refused where the self type is read,
        // for frustrate's own reason: an instantiation is formed from type
        // arguments, so a const on the self type names none and is dropped
        // here. Without it, `check` met an unknown *type* named `N`.
        let e = parse_source(
            "#[bridge(data)] pub struct P<T> { x: T } \
             #[bridge] impl<const N: usize> P<N> { #[bridge(sync)] pub fn m(&self) {} }",
            "crate::api",
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("FR0056") && e.contains("const N"), "{e}");
        // A **defaulted** block parameter gets none: rustc refuses the impl on
        // the author's own source ("defaults for generic parameters are not
        // allowed here"), so a codegen rule would only say it later.
        assert!(parse_source(
            "#[bridge(data)] pub struct P<T> { x: T } \
             #[bridge] impl<T = i64> P<T> { #[bridge(sync)] pub fn m(&self) {} }",
            "crate::api",
        )
        .is_ok());
        // `Self` is the self type **as written**, so it carries the block's
        // parameter rather than naming a bare `P` (which is not a type).
        assert_eq!(
            f.ret,
            Some(Type::App("P".into(), vec![Type::Named("T".into())]))
        );
    }

    /// A member the checker refuses outright is not a forgotten annotation:
    /// warning about it would prescribe `#[bridge]` and then have FR0056 or
    /// FR0057 reject it. Each negative sits beside the positive control that
    /// differs only in the refused shape.
    #[test]
    fn omission_warning_skips_members_the_checker_would_refuse() {
        let control = warnings("pub fn f(x: i32) -> i32 { x }");
        assert_eq!(control.len(), 1, "{control:?}");
        assert!(warnings("pub fn f<T>(x: i32) -> i32 { x }").is_empty());

        let opaque = "#[bridge(frozen)] pub struct O;";
        let control = warnings(&format!("{opaque} impl O {{ pub fn m(&self) -> i32 {{ 1 }} }}"));
        assert_eq!(control.len(), 1, "{control:?}");
        for refused in ["m<T>(&self)", "m(self: &Self)"] {
            let w = warnings(&format!("{opaque} impl O {{ pub fn {refused} -> i32 {{ 1 }} }}"));
            assert!(w.is_empty(), "{refused}: {w:?}");
        }
        // A consuming receiver on a *handle* is bridgeable now, so the
        // omission is worth reporting — prescribing `#[bridge]` here leads
        // somewhere.
        for accepted in ["m(self)", "m(self: Box<Self>)"] {
            let w = warnings(&format!("{opaque} impl O {{ pub fn {accepted} -> i32 {{ 1 }} }}"));
            assert_eq!(w.len(), 1, "{accepted}: {w:?}");
        }
        // On a data type a by-value receiver is bridgeable too, so it is
        // reported like any other omission; `&mut self` is FR0013 and stays
        // skipped.
        let data = "#[bridge(data)] pub struct P { pub x: i32 }";
        let control = warnings(&format!("{data} impl P {{ pub fn m(&self) -> i32 {{ 1 }} }}"));
        assert_eq!(control.len(), 1, "{control:?}");
        for accepted in ["m(self)", "m(self: Box<Self>)"] {
            let w = warnings(&format!("{data} impl P {{ pub fn {accepted} -> i32 {{ 1 }} }}"));
            assert_eq!(w.len(), 1, "{accepted}: {w:?}");
        }
        let w = warnings(&format!("{data} impl P {{ pub fn m(&mut self) -> i32 {{ 1 }} }}"));
        assert!(w.is_empty(), "{w:?}");
    }

    #[test]
    fn byte_slice_param_is_first_class_immutable_bytes() {
        // `&[u8]` is supported: it parses to Bytes with an immutable borrow,
        // exactly like `&Vec<u8>`.
        let iface =
            parse("#[bridge(sync)] pub fn f(x: &[u8]) -> usize { x.len() }");
        assert_eq!(iface.functions[0].params[0].ty, Type::Bytes);
        assert_eq!(iface.functions[0].params[0].borrow, Borrow::Ref);
    }

    /// A typed slice is the list codec's type with a borrow on it. `&[u8]` is
    /// the one that also carries the buffer-borrow spelling (`unsized_borrow`);
    /// every other element type keeps the owning decode and lends that local.
    #[test]
    fn a_typed_slice_is_a_borrowed_list() {
        let iface = parse("#[bridge(sync)] pub fn f(x: &[i32]) {}");
        let p = &iface.functions[0].params[0];
        assert_eq!(p.ty, Type::List(Box::new(Type::I32), SeqKind::Vec));
        assert_eq!(p.borrow, Borrow::Ref);
        // `&Vec<i32>` reaches the same type; only the spelling differs.
        let owned = parse("#[bridge(sync)] pub fn f(x: &Vec<i32>) {}");
        assert_eq!(owned.functions[0].params[0].ty, p.ty);
        assert!(p.unsized_borrow && !owned.functions[0].params[0].unsized_borrow);
        // Multi-element tuples parse (→ Dart records); only 1-tuples reject.
        assert!(
            parse_source("#[bridge(sync)] pub fn g(x: (i32, i32)) {}", "crate").is_ok()
        );
    }

    #[test]
    fn sink_param_becomes_sink_spec() {
        let iface = parse(
            r#"
            #[bridge]
            pub fn watch(n: i64, sink: frustrate::StreamSink<Patch>, tag: String) {}
            "#,
        );
        let f = &iface.functions[0];
        // A handle is an ordinary parameter in its declared position — it is
        // data (an id) on the wire, so nothing is hoisted.
        assert_eq!(f.params.len(), 3);
        assert_eq!(f.params[0].name, "n");
        assert_eq!(f.params[1].name, "sink");
        assert_eq!(f.params[2].name, "tag");
        let spec = f.params[1].ty.as_dart_object().expect("a handle");
        assert_eq!(spec.mirror, DartMirror::StreamController);
        assert_eq!(spec.item, Some(Type::Named("Patch".into())));
        assert_eq!(spec.ret, None);
    }

    #[test]
    fn sinks_compose_in_any_position() {
        // The composition unlock: what used to be a parse error is now the
        // point. Direction is the checker's job (FR0031), not the parser's.
        let iface = parse("#[bridge] pub fn f(s: Vec<StreamSink<i64>>) {}");
        let Type::List(inner, _) = &iface.functions[0].params[0].ty else {
            panic!("expected a list");
        };
        assert_eq!(
            inner.as_dart_object().map(|s| s.mirror),
            Some(DartMirror::StreamController)
        );

        let iface = parse("#[bridge(data)] pub struct Holder { s: StreamSink<i64> }");
        assert!(iface.structs[0].fields[0].ty.as_dart_object().is_some());

        // Still by value: the function owns its end of the channel.
        let err =
            parse_source("#[bridge] pub fn f(s: &StreamSink<i64>) {}", "crate").unwrap_err();
        assert!(err.to_string().contains("passed by value"), "{err}");
    }

    #[test]
    fn dart_mirrors_need_their_dart_path() {
        // `Sink` is a plausible user type name, so capturing a bare one as a
        // channel endpoint would be a nasty surprise.
        let iface = parse(
            "#[bridge] pub fn f(a: frustrate::dart::core::Sink<i64>, \
             b: dart::r#async::EventSink<i64>) {}",
        );
        let p = &iface.functions[0].params;
        assert_eq!(
            p[0].ty.as_dart_object().map(|s| s.mirror),
            Some(DartMirror::Sink)
        );
        assert_eq!(
            p[1].ty.as_dart_object().map(|s| s.mirror),
            Some(DartMirror::EventSink)
        );
        // A bare `Sink<T>` is just an unknown named type, not a mirror.
        let iface = parse("#[bridge] pub fn f(s: Sink) {}");
        assert_eq!(iface.functions[0].params[0].ty, Type::Named("Sink".into()));
    }

    #[test]
    fn bytes_struct_becomes_extern_decl_with_protobuf_defaults() {
        let iface = parse(
            r#"
            #[bridge(bytes(dart = "Plan", import = "package:app/plan.pb.dart"))]
            pub struct PlanMsg { pub title: String }

            #[bridge(bytes(dart = "Custom", import = "pkg.dart", encode = "toWire", decode = "Custom.parse"))]
            pub struct CustomMsg { pub x: i64 }
            "#,
        );
        assert!(iface.structs.is_empty(), "externs are not data structs");
        let plan = &iface.externs[0];
        assert_eq!(plan.name, "PlanMsg");
        assert_eq!(plan.dart_type, "Plan");
        assert_eq!(plan.dart_import, "package:app/plan.pb.dart");
        assert_eq!(plan.dart_encode, "writeToBuffer");
        assert_eq!(plan.dart_decode, "Plan.fromBuffer");
        let custom = &iface.externs[1];
        assert_eq!(custom.dart_encode, "toWire");
        assert_eq!(custom.dart_decode, "Custom.parse");
    }

    #[test]
    fn bytes_attr_misuse_is_rejected() {
        for (src, expected) in [
            (
                r#"#[bridge(bytes(dart = "X", import = "x.dart"))] pub enum E { A }"#,
                "only supported on structs",
            ),
            (
                r#"#[bridge(bytes(import = "x.dart"))] pub struct S { x: i64 }"#,
                "requires dart",
            ),
            (
                r#"#[bridge(bytes(dart = "X"))] pub struct S { x: i64 }"#,
                "requires import",
            ),
            (
                r#"#[bridge(bytes(dart = "X", import = "x.dart"), frozen)] pub struct S { x: i64 }"#,
                "mutually exclusive",
            ),
            (
                r#"#[bridge(bytes(dart = "X", import = "x.dart"))] pub fn f() {}"#,
                "type declarations",
            ),
        ] {
            let err = parse_source(src, "crate").unwrap_err();
            assert!(err.root_cause().to_string().contains(expected), "{src}: {err}");
        }
    }

    #[test]
    fn no_eq_parses_on_data_types_and_is_rejected_elsewhere() {
        // Accepted on a data struct and a data enum.
        let iface = parse(
            r#"
            #[bridge(data, no_eq)] pub struct S { x: i64 }
            #[bridge(data, no_eq)] pub enum E { A, B(i64) }
            "#,
        );
        assert!(iface.structs[0].no_eq);
        assert!(iface.enums[0].no_eq);
        // Rejected loudly on any non-data item.
        for (src, expected) in [
            (
                r#"#[bridge(no_eq, sync)] pub fn f() {}"#,
                "only valid on a data struct or data enum",
            ),
            (
                r#"#[bridge(no_eq, frozen)] pub struct H { x: i64 }"#,
                "not a handle type",
            ),
            (
                r#"#[bridge(no_eq, bytes(dart = "X", import = "x.dart"))] pub struct B { x: i64 }"#,
                "not a bytes(...) external type",
            ),
            (
                r#"#[bridge(no_eq)] impl S { #[bridge] pub fn m(&self) {} }"#,
                "only valid on a data struct or data enum",
            ),
            (
                r#"#[bridge(no_eq)] pub trait T { fn m(&self); }"#,
                "only valid on a data struct or data enum",
            ),
        ] {
            let err = parse_source(src, "crate").unwrap_err();
            assert!(err.root_cause().to_string().contains(expected), "{src}: {err}");
        }
    }

    #[test]
    fn callback_params_become_callback_specs() {
        let iface = parse(
            r#"
            #[bridge]
            pub fn f(cb: DartCallback<i64>, n: i64, done: frustrate::DartCallback<()>) {}
            #[bridge]
            pub fn g(x: i64, t: DartFunction<String, i64>) {}
            "#,
        );
        let f = &iface.functions[0];
        assert_eq!(f.params.len(), 3, "handles stay in declaration order");
        let cb = f.params[0].ty.as_dart_object().expect("a handle");
        assert_eq!(cb.mirror, DartMirror::Callback);
        assert_eq!(cb.item, Some(Type::I64));
        assert_eq!(cb.ret, None);
        assert_eq!(f.params[1].name, "n");
        let done = f.params[2].ty.as_dart_object().expect("a handle");
        assert_eq!(done.item, None, "() means no argument");

        let g = &iface.functions[1];
        let t = g.params[1].ty.as_dart_object().expect("a handle");
        assert_eq!(t.mirror, DartMirror::Function);
        assert_eq!(t.item, Some(Type::String));
        assert_eq!(t.ret, Some(Type::I64));
    }

    /// A Dart closure whose failure Rust handles as a value:
    /// `DartFunction<T, Result<R, E>>`. `err` is what tells the two shapes apart
    /// everywhere downstream — the wire, the Dart type, the fingerprint.
    #[test]
    fn a_fallible_dart_function_carries_its_error_type() {
        let iface = parse(
            r#"
            #[bridge(data)]
            pub enum RefusalError { Busy }
            #[bridge]
            pub async fn ask(f: DartFunction<i64, Result<i64, RefusalError>>) -> i64 { 0 }
            #[bridge]
            pub async fn ask_void(f: DartFunction<i64, Result<(), RefusalError>>) {}
            "#,
        );
        let spec = iface.functions[0].params[0]
            .ty
            .as_dart_object()
            .expect("a handle");
        assert_eq!(spec.mirror, DartMirror::Function);
        assert_eq!(spec.item, Some(Type::I64));
        assert_eq!(spec.ret, Some(Type::I64));
        assert_eq!(spec.err, Some(Type::Named("RefusalError".into())));
        // `Result<(), E>` is "do this, you may refuse": no value, a declared
        // failure. It is still a RETURNING mirror — it has a reply frame.
        let void = iface.functions[1].params[0]
            .ty
            .as_dart_object()
            .expect("a handle");
        assert_eq!(void.ret, None);
        assert_eq!(void.err, Some(Type::Named("RefusalError".into())));
        assert!(void.is_returning(), "a reply frame, so not fire-and-forget");
    }

    /// The message tiers do NOT mirror. On the return path Rust *authored*
    /// the string; here nothing did, so `Result<R, String>`
    /// would turn every Dart bug into a plausible business value.
    #[test]
    fn an_untyped_error_on_a_dart_function_is_rejected() {
        for src in [
            "#[bridge] pub async fn f(c: DartFunction<i64, Result<i64, String>>) {}",
            "#[bridge] pub async fn f(c: DartFunction<i64, anyhow::Result<i64>>) {}",
            "#[bridge] pub async fn f(c: DartFunction<i64, Result<i64, anyhow::Error>>) {}",
            // The boxed tier is a message tier, so it is rejected here for the
            // same reason the other three are: nothing authors a Dart throw.
            "#[bridge] pub async fn f(c: DartFunction<i64, Result<i64, Box<dyn Error>>>) {}",
        ] {
            let err = parse_source(src, "crate").unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains("FR0043"), "{src}: {msg}");
            assert!(
                msg.contains("bridged struct or enum"),
                "the message must name the fix: {src}: {msg}"
            );
        }
    }

    #[test]
    fn callback_bad_shapes_are_rejected() {
        let err = parse_source("#[bridge] pub fn f(c: &DartCallback<i64>) {}", "crate")
            .unwrap_err();
        assert!(err.to_string().contains("passed by value"), "{err}");
        let err = parse_source("#[bridge] pub fn f(c: DartFunction<i64, ()>) {}", "crate")
            .unwrap_err();
        assert!(
            err.root_cause().to_string().contains("use DartCallback"),
            "{err}"
        );
    }

    #[test]
    fn parses_trait_declaration() {
        let iface = parse(
            r#"
            #[bridge(frozen)]
            pub trait Store: Send + Sync {
                fn get(&self, key: String) -> Option<String>;
                #[bridge(sync)]
                fn len(&self) -> usize { 0 }
            }

            #[bridge]
            pub fn open_store(kind: String) -> anyhow::Result<Box<dyn Store>> { todo!() }

            #[bridge(sync)]
            pub fn report(store: &dyn Store) -> String { todo!() }
            "#,
        );
        let o = &iface.opaques[0];
        assert!(o.dyn_trait);
        assert_eq!(o.model, Model::Frozen);
        assert_eq!(o.supertraits, vec!["Send", "Sync"]);
        let get = &iface.functions[0];
        assert_eq!(get.parent.as_deref(), Some("Store"));
        assert_eq!(get.receiver, Some(Receiver::Ref));
        assert_eq!(get.exec, Exec::Async);
        // default-bodied methods bridge like required ones; method-level
        // attr overrides
        let len = &iface.functions[1];
        assert_eq!(len.exec, Exec::Sync);
        let open = &iface.functions[2];
        assert_eq!(open.ret, Some(Type::Named("Store".into())));
        assert!(open.fallible);
        assert!(!open.is_constructor);
        let report = &iface.functions[3];
        assert_eq!(report.params[0].ty, Type::Named("Store".into()));
        assert_eq!(report.params[0].borrow, Borrow::Ref);
    }

    #[test]
    fn trait_impl_blocks_bridge_written_methods_statically() {
        let iface = parse(
            r#"
            #[bridge(confined)]
            pub struct Doc { x: i64 }

            #[bridge(sync)]
            impl Renderable for Doc {
                fn render(&self) -> String { todo!() }
            }

            #[bridge(sync)]
            impl automerge::ReadDoc for Doc {
                fn heads(&self) -> Vec<u8> { todo!() }
            }
            "#,
        );
        let render = &iface.functions[0];
        assert_eq!(render.parent.as_deref(), Some("Doc"));
        // bare trait name resolves to the file's module
        assert_eq!(render.trait_impl.as_deref(), Some("crate::api::Renderable"));
        assert_eq!(render.exec, Exec::Sync);
        // multi-segment paths pass through verbatim (external crates)
        assert_eq!(
            iface.functions[1].trait_impl.as_deref(),
            Some("automerge::ReadDoc")
        );
    }

    #[test]
    fn trait_impl_misuse_is_rejected() {
        for (src, expected) in [
            (
                r#"#[bridge(confined)] pub struct D { x: i64 }
                   #[bridge] impl super::T for D { fn f(&self) {} }"#,
                "do not resolve from generated code",
            ),
            (
                r#"#[bridge(confined)] pub struct D { x: i64 }
                   #[bridge] impl From<i64> for D { fn from(_: i64) -> D { todo!() } }"#,
                "generic trait impls cannot be bridged",
            ),
        ] {
            let err = parse_source(src, "crate::api").unwrap_err();
            assert!(err.root_cause().to_string().contains(expected), "{src}: {err}");
        }
    }

    #[test]
    fn trait_level_attr_is_method_default() {
        let iface = parse(
            r#"
            #[bridge(confined, sync)]
            pub trait Counter {
                fn bump(&mut self);
            }
            "#,
        );
        assert_eq!(iface.functions[0].exec, Exec::Sync);
        assert_eq!(iface.functions[0].receiver, Some(Receiver::RefMut));
    }

    #[test]
    fn trait_misuse_is_rejected() {
        for (src, expected) in [
            (
                "#[bridge] pub trait T { fn f(&self); }",
                "must name its representation",
            ),
            (
                "#[bridge(confined)] pub trait T<X> { fn f(&self); }",
                "generic traits cannot be bridged",
            ),
            (
                "#[bridge(confined)] pub trait T { fn f<X>(&self); }",
                "generic methods are not dyn-dispatchable",
            ),
            (
                "#[bridge(confined)] pub trait T { fn f(&self) -> Self; }",
                "not dyn-dispatchable",
            ),
            (
                "#[bridge(confined)] pub trait T { fn f(&self) -> Result<Self>; }",
                "not dyn-dispatchable",
            ),
            (
                "#[bridge(confined)] pub trait T { type Item; fn f(&self); }",
                "associated type",
            ),
            (
                "#[bridge(confined)] pub trait T { const N: i64; fn f(&self); }",
                "associated const",
            ),
            // The two `Box` spellings the bridge cannot reconstruct: it maps a
            // slice and `str` onto their owned containers, so the IR has no way
            // to say "boxed slice" and the decode would build a `Box<Vec<_>>`.
            (
                "#[bridge] pub fn f() -> Box<[i32]> { todo!() }",
                "Write `Vec<T>`",
            ),
            (
                "#[bridge] pub fn f() -> Box<str> { todo!() }",
                "Write `String`",
            ),
            (
                "#[bridge] pub fn f(x: &(dyn A + B)) {}",
                "multi-trait objects are not supported",
            ),
        ] {
            let err = parse_source(src, "crate").unwrap_err();
            assert!(err.root_cause().to_string().contains(expected), "{src}: {err}");
        }
        // Marker bounds at the use site are tolerated.
        let iface = parse(
            r#"
            #[bridge(frozen)]
            pub trait Store: Send + Sync { fn get(&self) -> i64; }
            #[bridge]
            pub fn f(x: &(dyn Store + Send)) -> Box<dyn Store + Send + Sync> { todo!() }
            "#,
        );
        assert_eq!(iface.functions[1].params[0].ty, Type::Named("Store".into()));
        assert_eq!(iface.functions[1].ret, Some(Type::Named("Store".into())));
    }

    #[test]
    fn borrowed_params() {
        let iface = parse(
            r#"
            #[bridge(sync)]
            pub fn doc_len(doc: &Doc) -> usize { todo!() }
            #[bridge(sync)]
            pub fn doc_clear(doc: &mut Doc) {}
            "#,
        );
        assert_eq!(iface.functions[0].params[0].borrow, Borrow::Ref);
        assert_eq!(iface.functions[1].params[0].borrow, Borrow::RefMut);
    }

    #[test]
    fn deferred_return_unwraps_to_the_inner_type() {
        let iface = parse(
            r#"
            #[bridge(actor)] pub struct A { x: i64 }
            #[bridge] impl A {
                pub fn new() -> Self { A { x: 0 } }
                pub fn slow(&mut self) -> Deferred<i64> { unimplemented!() }
                pub fn slow_fallible(&self) -> Deferred<Result<String, String>> { unimplemented!() }
            }
            "#,
        );
        // `ret`/`fallible` describe the *inner* type — the wrapper is a flag,
        // so every downstream value rule (and the Dart signature) sees
        // through it.
        let slow = iface.functions.iter().find(|f| f.name == "slow").unwrap();
        assert!(slow.deferred);
        assert_eq!(slow.ret, Some(Type::I64));
        assert!(!slow.fallible);
        let sf = iface.functions.iter().find(|f| f.name == "slow_fallible").unwrap();
        assert!(sf.deferred && sf.fallible);
        assert_eq!(sf.ret, Some(Type::String));
        assert!(!iface.functions.iter().find(|f| f.name == "new").unwrap().deferred);
    }

    #[test]
    fn deferred_outside_the_outermost_return_is_rejected() {
        // Same position rule as Result: `Deferred` is not a value, so it
        // never appears as a parameter, a field, or nested in another type.
        for src in [
            "#[bridge] pub fn f(x: Deferred<i64>) {}",
            "#[bridge(data)] pub struct S { d: Deferred<i64> }",
            "#[bridge] pub fn g() -> Vec<Deferred<i64>> { unimplemented!() }",
            "#[bridge] pub fn h() -> Deferred<Deferred<i64>> { unimplemented!() }",
        ] {
            let err = parse_source(src, "crate").unwrap_err();
            assert!(
                err.root_cause().to_string().contains("outermost return type"),
                "{src}: {err}"
            );
        }
        let err = parse_source("#[bridge] pub fn f() -> Deferred { unimplemented!() }", "crate")
            .unwrap_err();
        assert!(err.root_cause().to_string().contains("exactly one type argument"), "{err}");
    }

    /// The declared-mirror form's attribute reaches the IR, and only there.
    #[test]
    fn dart_interface_marks_the_struct() {
        let iface = parse_source(
            "#[bridge(data, dart_interface)] pub struct W { pub on_change: DartCallback<i64> } \
             #[bridge(data)] pub struct P { pub x: i64 }",
            "crate::api",
        )
        .unwrap();
        assert!(iface.structs[0].dart_interface, "the annotated struct");
        assert!(!iface.structs[1].dart_interface, "and nothing else");
    }

    /// `dart_interface` replaces a data struct's generated Dart *class*. Every
    /// item with no such class to replace rejects it by name rather than
    /// ignoring a flag the author expected to change the surface.
    #[test]
    fn dart_interface_is_rejected_where_it_cannot_bite() {
        for src in [
            "#[bridge(dart_interface, sync)] pub fn f() {}",
            "#[bridge(data, dart_interface)] pub enum E { A }",
            "#[bridge(confined, dart_interface)] pub struct D { x: i32 }",
            "#[bridge(dart_interface, bytes(dart = \"P\", import = \"p.dart\"))] pub struct B { x: i32 }",
            "#[bridge(confined)] pub struct D { x: i32 } \
             #[bridge(dart_interface)] impl D { #[bridge(sync)] pub fn f(&self) {} }",
            "#[bridge(confined, dart_interface)] pub trait T { fn f(&self); }",
        ] {
            let err = parse_source(src, "crate::api").unwrap_err();
            assert!(
                err.root_cause().to_string().contains("dart_interface is only valid"),
                "{src}: {err}"
            );
        }
    }

    /// A representation keyword takes the Dart class name for the half it
    /// names. Accepted on a declaration with one representation too — one
    /// grammar, one meaning — where it is the flat form written longhand.
    #[test]
    fn a_representation_keyword_carries_its_halfs_dart_name() {
        let iface = parse_source(
            r#"#[bridge(data(dart_identifier = "DocValue"), locked(dart_identifier = "DocHandle"))]
               pub struct Doc { pub title: String }"#,
            "crate::api",
        )
        .unwrap();
        assert_eq!(iface.structs[0].dart_identifier.as_deref(), Some("DocValue"));
        assert_eq!(iface.opaques[0].dart_identifier.as_deref(), Some("DocHandle"));
        // One representation: the nested and flat spellings agree.
        for src in [
            r#"#[bridge(confined(dart_identifier = "Document"))] pub struct Doc { t: String }"#,
            r#"#[bridge(confined, dart_identifier = "Document")] pub struct Doc { t: String }"#,
        ] {
            let iface = parse_source(src, "crate::api").unwrap();
            assert_eq!(
                iface.opaques[0].dart_identifier.as_deref(),
                Some("Document"),
                "{src}"
            );
        }
        for src in [
            r#"#[bridge(data(dart_identifier = "Vec2"))] pub struct Point { x: f64 }"#,
            r#"#[bridge(data, dart_identifier = "Vec2")] pub struct Point { x: f64 }"#,
        ] {
            let iface = parse_source(src, "crate::api").unwrap();
            assert_eq!(iface.structs[0].dart_identifier.as_deref(), Some("Vec2"), "{src}");
        }
    }

    /// FR0068 — a bare `dart_identifier` on a declaration that mints two Dart
    /// classes does not say which one it renames, and neither does a nested
    /// name written alongside a bare one.
    #[test]
    fn a_bare_dart_identifier_on_a_two_class_declaration_is_refused() {
        for src in [
            r#"#[bridge(data, locked, dart_identifier = "X")] pub struct Doc { t: String }"#,
            r#"#[bridge(data(dart_identifier = "V"), dart_identifier = "X")]
               pub struct Doc { t: String }"#,
            r#"#[bridge(locked(dart_identifier = "H"), dart_identifier = "X")]
               pub struct Doc { t: String }"#,
        ] {
            let err = parse_source(src, "crate::api").unwrap_err();
            let msg = err.root_cause().to_string();
            assert!(msg.contains("FR0068"), "{src}: {msg}");
            assert!(msg.contains("dart_identifier"), "{src}: {msg}");
        }
    }

    /// `dart_identifier` is the only option a representation keyword nests,
    /// and the grammar says so rather than dropping what it does not read.
    #[test]
    fn a_representation_keyword_takes_no_other_nested_option() {
        for (src, needle) in [
            (
                r#"#[bridge(locked(native_only))] pub struct Doc { t: String }"#,
                "the only option",
            ),
            (
                r#"#[bridge(data(other = "x"))] pub struct Doc { t: String }"#,
                "unknown",
            ),
            (
                r#"#[bridge(data())] pub struct Doc { t: String }"#,
                "claims nothing",
            ),
        ] {
            let err = parse_source(src, "crate::api").unwrap_err();
            let msg = err.root_cause().to_string();
            assert!(msg.contains(needle), "{src}: {msg}");
        }
    }

    /// The options that are about a *whole type* rather than about one of its
    /// Dart classes. Each is read against the type's name, with no per-half
    /// reader, so a declaration with two halves has no way to say which it
    /// means — and the value half is on every target by construction.
    #[test]
    fn a_two_representation_declaration_refuses_the_whole_type_options() {
        for (src, needle) in [
            (
                "#[bridge(data, locked, native_only)] pub struct Doc { t: String }",
                "native_only",
            ),
            (
                "#[bridge(data, locked, dart_interface)] pub struct Doc { t: String }",
                "dart_interface",
            ),
            (
                "#[cfg(unix)] #[bridge(data, locked)] pub struct Doc { t: String }",
                "cfg",
            ),
        ] {
            let err = parse_source(src, "crate::api").unwrap_err();
            let msg = err.root_cause().to_string();
            assert!(msg.contains(needle), "{src}: {msg}");
        }
        // `no_eq` is about the *value* half's generated equality, which a
        // declaration naming `data` has exactly one of — so it needs no
        // selector and stays flat.
        let iface = parse_source(
            "#[bridge(data, locked, no_eq)] pub struct Doc { t: String }",
            "crate::api",
        )
        .unwrap();
        assert!(iface.structs[0].no_eq);
    }
}
