// `write_with_newline` is off for this file, for the reason `emit_dart.rs`
// gives: the format strings mirror the generated Dart line for line, trailing
// newline included.
#![allow(clippy::write_with_newline)]

//! The typed fake harness, emitted into the bottom of each Dart surface.
//!
//! ## What it is
//!
//! A mirror of the generated client, pointing the other way. For every member
//! the surface emits, this emits an *answer*: decode the request with the same
//! walkers the client encodes it with ([`crate::emit_dart::decode_expr_fake`]),
//! call one method on a user-supplied fake, and encode the result as the
//! response envelope a real library would have sent. Nothing downstream knows
//! the difference — the client's own decoders and `decodeEnvelope` run
//! unchanged.
//!
//! Three families come out of it, per crate:
//!
//!   * `Fake<Crate>Bridge` — the dispatch. Extends `FakeBridge`, and is what a
//!     `FakeRuntime` is built on.
//!   * `Fake<Crate>` — the free functions, the static members and every
//!     constructor.
//!   * `Fake<Type>` — one per opaque, actor and trait; plus one per struct or
//!     enum that reaches a Dart-object handle, whose fields are the sinks and
//!     closures the caller passed.
//!
//! ## Why the members throw by default
//!
//! Every generated fake method's body is a throw. A fake declares only the
//! members its test exercises, and a member nobody expected to be called
//! arrives as a panic envelope *naming it* rather than as a plausible zero.
//! An `abstract interface class` would be the tighter declaration and is the
//! wrong one: the project's own fixture emits hundreds of these members, and an
//! interface would make any fake of it unwritable.
//!
//! ## Where it sits
//!
//! Inline in the surface, not in a `part` or a sibling library, because it
//! needs what only that library has: the private handle constructors
//! (`TextDoc._`) and the private codecs (`_encPoint`, `_decWidget`). The cost
//! of that is an import every build pays — which is why the harness names only
//! `package:frustrate/fake_contract.dart`, where nothing implements
//! `FrustrateRuntime`. The fake transport itself is a separate library a test
//! imports.

use std::fmt::Write;

use crate::emit_dart::{
    dart_name, dart_type, dart_type_opt, decl_has_handle, decl_owns_handles, decode_expr,
    fake_class_header, fake_data_class, fake_handle_class, fake_variant_class, field_name, member_name,
    decode_expr_fake,
    emit_doc_comment, encode_stmts_fake, error_class_stem, error_exception_type,
    has_handle, web_fate,
    SizeHint, WebFate,
};
use crate::ir::{
    DartMirror, DartObjectSpec, EnumDecl, Exec, Function, Interface, Model, OpaqueDecl, Receiver,
    Repr,
    StructDecl, StructShape, Type,
};

/// Upper-camel of the crate name — `test_api` becomes `TestApi` — used as the
/// stem of the two crate-level classes.
fn crate_stem(iface: &Interface) -> String {
    use heck::ToUpperCamelCase;
    iface.crate_name.to_upper_camel_case()
}

/// The name of a fake method on `Fake<Crate>` for a member the fake reaches
/// through no `Fake<Type>` object of its own.
///
/// A free function keeps its own name. A constructor or a static member is
/// flattened onto the crate fake with its parent in front — `TextDoc::new`
/// becomes `textDocNew` — because that is the only place a receiverless
/// member of a type can live: the caller overrides a fake by extending it, and
/// a Dart `static` cannot be overridden. A **data** type's members flatten the
/// same way, receiver or not: there is no handle to resolve and so no
/// `FakePoint` to hang them on, and the receiver arrives as the fake method's
/// first argument instead.
///
/// Concatenating two camel-cased halves is not injective — `Point::norm` and a
/// free `point_norm` both reach `pointNorm` — and that is left as a collision
/// for FR0002 to report rather than dodged with a separator. Which of the two
/// should move, and to what, is the author's call, and `dart_identifier`
/// carries it; a separator would buy the rare case at the cost of an
/// `_` in every compound name anyone ever overrides.
///
/// A member of an **instantiation** flattens the same way, but its parent is
/// `Page<Item>`, which is not an identifier. It takes the instantiation's own
/// stem instead — `page$ItemLen` — injective for the reason
/// [`crate::emit_dart::instance_extension`] gives, so two instantiations of one
/// template never answer on one fake method even where their Dart types agree.
pub(crate) fn free_name(iface: &Interface, f: &Function) -> String {
    use heck::ToUpperCamelCase;
    let member = member_name(f).to_upper_camel_case();
    match &f.parent {
        None => member_name(f),
        Some(p) if iface.instance_of(p).is_some() => {
            let stem = crate::emit_dart::instance_extension(iface, p);
            let mut cs = stem.chars();
            let head: String = cs.next().map(|c| c.to_ascii_lowercase()).into_iter().collect();
            format!("{head}{}{member}", cs.as_str())
        }
        Some(p) => format!("{}{member}", dart_name(p)),
    }
}

/// The `Type` node for `f`'s **value** receiver, as the right variant —
/// `Struct` and `Enum` are distinct nodes and the codec walkers look the
/// declaration up by the one they are handed. `None` when `f` has no receiver
/// or its receiver is a handle.
fn data_receiver_type(iface: &Interface, f: &Function) -> Option<Type> {
    let parent = iface.data_receiver(f)?;
    if iface.struct_decl(parent).is_some() {
        return Some(Type::Struct(parent.to_string()));
    }
    iface
        .enum_decl(parent)
        .map(|_| Type::Enum(parent.to_string()))
}

/// Whether `f` is answered by a method on `Fake<Crate>` rather than by one on
/// a `Fake<Type>` object. True for everything with no receiver, and for a
/// **data** type's methods: the fake resolves a receiver by looking a handle
/// up in its registry, and a data receiver is a value on the wire with no
/// handle to look up.
pub(crate) fn on_crate_fake(iface: &Interface, f: &Function) -> bool {
    f.receiver.is_none() || iface.receiver_handle(f).is_none()
}

// ------------------------------------------------------------ fake types --

/// True when `ty`'s fake-side Dart type differs from its client-side one — it
/// reaches an opaque handle (which becomes a `Fake<Type>`) or a Dart-object
/// handle (which becomes a sink or a closure).
fn differs(iface: &Interface, ty: &Type) -> bool {
    let mut found = false;
    crate::check::walk_type_graph(iface, ty, &mut |t| {
        if t.as_dart_object().is_some() || matches!(t, Type::Opaque(_)) {
            found = true;
        }
    });
    found
}

/// The Dart type a fake's method sees where the client sees `dart_type`.
pub(crate) fn fake_type(iface: &Interface, ty: &Type) -> String {
    fake_type_opt(iface, ty, false)
}

pub(crate) fn fake_type_opt(iface: &Interface, ty: &Type, in_option: bool) -> String {
    // Identical wherever no handle is reachable, which is most of every
    // interface — one mapping, not two that can drift.
    if !differs(iface, ty) {
        return dart_type_opt(iface, ty, in_option);
    }
    match ty {
        Type::Opaque(n) => fake_handle_class(iface, n),
        // A `Box` has no Dart counterpart at all — see [`Type::Boxed`]. Nor has
        // a borrow: a lent handle is the same `Fake<Type>` a borrowed one at
        // the top of a parameter already is.
        Type::Boxed(t) | Type::Ref { inner: t, .. } => fake_type_opt(iface, t, in_option),
        Type::DartObject(spec) => mirror_type(iface, spec, false),
        Type::Struct(n) | Type::Enum(n) => fake_data_class(iface, n),
        Type::List(t, _) | Type::Array(t, _) => {
            format!("List<{}>", fake_type_opt(iface, t, false))
        }
        Type::Set(t, _) => format!("Set<{}>", fake_type_opt(iface, t, false)),
        Type::Map(k, v, _) => format!(
            "Map<{}, {}>",
            fake_type_opt(iface, k, false),
            fake_type_opt(iface, v, false)
        ),
        Type::Tuple(ts) => format!(
            "({})",
            ts.iter()
                .map(|t| fake_type_opt(iface, t, false))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Type::Option(t) => {
            let inner = fake_type_opt(iface, t, true);
            if in_option {
                format!("FrOption<{inner}>")
            } else {
                format!("{inner}?")
            }
        }
        // Unreachable: `differs` is true only for the arms above.
        _ => unreachable!("a type with no reachable handle takes the client mapping"),
    }
}

/// The fake-side type of one Dart-object mirror.
///
/// A sink-shaped mirror becomes a `FakeStreamSink<T>` — the write end, plus
/// the consumer's cancel and pause state, which is what a Rust producer sees.
/// A closure mirror stays a closure with the same argument types.
///
/// A **returning** closure becomes a `Future`, and that is a declared
/// difference from native physics rather than an accident: Rust blocks a pool
/// worker on the reply, and a fake has no worker to block.
fn mirror_type(iface: &Interface, spec: &DartObjectSpec, spread: bool) -> String {
    let params = mirror_param_types(iface, spec, spread).join(", ");
    match spec.mirror {
        DartMirror::Sink | DartMirror::EventSink | DartMirror::StreamController => {
            format!("FakeStreamSink<{}>", mirror_item(iface, spec))
        }
        DartMirror::Callback => format!("void Function({params})"),
        DartMirror::Function => {
            let ret = spec
                .ret
                .as_ref()
                .map(|t| dart_type(iface, t))
                .unwrap_or_else(|| "void".into());
            let closure = format!("Future<{ret}> Function({params})");
            match &spec.err {
                // The same alias the client's parameter wears, for the same
                // reason: Dart has no checked exceptions, and this is the one
                // place the type can say the closure may refuse. Here the fake
                // is the one that has to be ready for the throw.
                Some(e) => format!("{}Fallible<{closure}>", error_class_stem(iface, e)),
                None => closure,
            }
        }
    }
}

/// The item type a sink mirror carries, in the **fake's** spelling. Identical
/// to the client's for every value item; a handle item is the fake's stand-in
/// (`FakeDoc`), because the object the fake's producer hands over is the fake's,
/// and the wire form it mints is `_fakeWire.mintHandle`'s.
fn mirror_item(iface: &Interface, spec: &DartObjectSpec) -> String {
    spec.item
        .as_ref()
        .map(|t| fake_type(iface, t))
        .unwrap_or_else(|| "void".into())
}

/// The argument types of a closure mirror. A `dart_interface` **method** takes
/// a top-level tuple as separate positional arguments (`t(a, b)`), because a
/// method can; a closure field takes the record.
fn mirror_param_types(iface: &Interface, spec: &DartObjectSpec, spread: bool) -> Vec<String> {
    match (&spec.item, spread) {
        (None, _) => vec![],
        (Some(Type::Tuple(ts)), true) => ts.iter().map(|t| fake_type(iface, t)).collect(),
        (Some(t), _) => vec![fake_type(iface, t)],
    }
}

// ------------------------------------------------------- mirror factories --

/// Every distinct Dart-object mirror the interface can hand a fake, in a
/// stable order, paired with whether it spreads a tuple positionally.
///
/// Deduplicated, so one factory serves every member that takes the same shape;
/// keyed on the spread flag too, because a `dart_interface` method and a
/// closure field of the same spec are different Dart types.
fn mirrors(iface: &Interface) -> Vec<(DartObjectSpec, bool)> {
    let mut out: Vec<(DartObjectSpec, bool)> = Vec::new();
    let mut seen_decls: Vec<String> = Vec::new();
    for f in &iface.functions {
        for p in &f.params {
            collect(iface, &p.ty, &mut seen_decls, &mut out);
        }
    }
    out
}

fn push(out: &mut Vec<(DartObjectSpec, bool)>, spec: &DartObjectSpec, spread: bool) {
    if !out.iter().any(|(s, sp)| s == spec && *sp == spread) {
        out.push((spec.clone(), spread));
    }
}

fn collect(
    iface: &Interface,
    ty: &Type,
    seen: &mut Vec<String>,
    out: &mut Vec<(DartObjectSpec, bool)>,
) {
    match ty {
        Type::DartObject(spec) => push(out, spec, false),
        Type::List(t, _)
        | Type::Set(t, _)
        | Type::Option(t)
        | Type::Array(t, _)
        | Type::Boxed(t) => collect(iface, t, seen, out),
        Type::Map(k, v, _) => {
            collect(iface, k, seen, out);
            collect(iface, v, seen, out);
        }
        Type::Tuple(ts) => {
            for t in ts {
                collect(iface, t, seen, out);
            }
        }
        Type::Struct(n) => {
            if seen.contains(n) {
                return;
            }
            seen.push(n.clone());
            let s = iface.struct_decl(n).expect("struct in finalized IR");
            for f in &s.fields {
                // A declared interface's fields are its *methods*, and a method
                // spreads a top-level tuple where a closure field could not.
                match (s.dart_interface, f.ty.as_dart_object()) {
                    (true, Some(spec)) => push(out, spec, true),
                    _ => collect(iface, &f.ty, seen, out),
                }
            }
        }
        Type::Enum(n) => {
            if seen.contains(n) {
                return;
            }
            seen.push(n.clone());
            let e = iface.enums.iter().find(|e| &e.name == n).expect("enum in IR");
            for v in &e.variants {
                for f in &v.fields {
                    collect(iface, &f.ty, seen, out);
                }
            }
        }
        _ => {}
    }
}

/// The call that builds one mirror from the handle id sitting in the request.
///
/// A factory function rather than an inline expression because a mirror has to
/// compose in *expression* position — inside `List.generate` for a
/// `Vec<DartCallback<T>>` — where there is nowhere to put the local the closure
/// needs to capture.
pub(crate) fn mirror_call(iface: &Interface, spec: &DartObjectSpec, spread: bool) -> String {
    let i = mirrors(iface)
        .iter()
        .position(|(s, sp)| s == spec && *sp == spread)
        .expect("every reachable mirror is registered");
    format!("_fakeMirror{i}(_fakeReq, _fakeReq.track(r.readHandle()))")
}

/// How a mirror factory opens and closes, given its encoder body: an arrow
/// when the encode mints nothing, a block naming `_fakeWire` when it does.
fn fake_wire_scope(enc: &str) -> (&'static str, &'static str) {
    if enc.contains("_fakeWire") {
        ("{\n    final _fakeWire = _fakeReq.wire;\n    return ", ";\n}")
    } else {
        ("=>\n\x20   ", ";")
    }
}

fn emit_mirror_factories(out: &mut String, iface: &Interface) {
    for (i, (spec, spread)) in mirrors(iface).iter().enumerate() {
        let ty = mirror_type(iface, spec, *spread);
        let names: Vec<String> = (0..mirror_param_types(iface, spec, *spread).len())
            .map(|n| format!("a{}", n + 1))
            .collect();
        let params: Vec<String> = mirror_param_types(iface, spec, *spread)
            .iter()
            .zip(&names)
            .map(|(t, n)| format!("{t} {n}"))
            .collect();
        // The wire carries one value where the Dart side may have spread it
        // into several arguments; re-assemble in declaration order.
        let encoded: Vec<(&Type, String)> = match (&spec.item, *spread) {
            (None, _) => vec![],
            (Some(Type::Tuple(ts)), true) => ts.iter().zip(&names).map(|(t, n)| (t, n.clone())).collect(),
            (Some(t), _) => vec![(t, names[0].clone())],
        };
        let mut hint = SizeHint::default();
        for (t, n) in &encoded {
            hint.add(t, n);
        }

        match spec.mirror {
            DartMirror::Sink | DartMirror::EventSink | DartMirror::StreamController => {
                let item = mirror_item(iface, spec);
                let mut enc = String::new();
                if let Some(t) = &spec.item {
                    encode_stmts_fake(&mut enc, iface, t, "v", 4);
                }
                // A handle item is *minted* by this encode, and the fake's
                // registry is where it is minted into — so the factory becomes
                // a block that names the wire. Only where the encode reads it:
                // a `final _fakeWire = ...` nothing uses is an
                // `unused_local_variable` in the consumer's own analyze, which
                // is the same reason the dispatch entry hoists conditionally.
                let (open, close) = fake_wire_scope(&enc);
                let _ = write!(
                    out,
                    "{ty} _fakeMirror{i}(FakeRequest _fakeReq, int id) {open}\
                     FakeStreamSink<{item}>(_fakeReq, id, (w, v) {{\n\
                     {enc}\
                     \x20   }}, hasAddError: {}){close}\n\n",
                    spec.mirror.has_add_error()
                );
            }
            DartMirror::Callback => {
                let mut enc = String::new();
                for (t, n) in &encoded {
                    encode_stmts_fake(&mut enc, iface, t, n, 3);
                }
                let (open, close) = fake_wire_scope(&enc);
                // The gone-channel test comes **before** the encode, as
                // `FakeStreamSink.add` does it and for the same reason: an
                // argument that reaches a handle is minted by this encode, so
                // encoding into a retired closure would register an object
                // nothing retires. Throwing after the mint would have leaked it
                // into the fake's registry on the way out.
                let _ = write!(
                    out,
                    "{ty} _fakeMirror{i}(FakeRequest _fakeReq, int id) {open}\
                     ({}) {{\n\
                     \x20     if (!_fakeReq.wire.isChannelOpen(id)) {{\n\
                     \x20       throw StateError(_fakeClosureGone(_fakeReq.label));\n\
                     \x20     }}\n\
                     \x20     final w = {};\n\
                     {enc}\
                     \x20     if (!_fakeReq.wire.deliver(id, 0, w)) {{\n\
                     \x20       throw StateError(_fakeClosureGone(_fakeReq.label));\n\
                     \x20     }}\n\
                     \x20   }}{close}\n\n",
                    params.join(", "),
                    hint.ctor()
                );
            }
            DartMirror::Function => {
                let mut enc = String::new();
                for (t, n) in &encoded {
                    encode_stmts_fake(&mut enc, iface, t, n, 3);
                }
                // The declared refusal comes back as its encoded value, and the
                // fake receives the same exception class the client would throw
                // in the other direction — one vocabulary, mirrored.
                let typed_error = match &spec.err {
                    None => String::new(),
                    Some(err) => format!(
                        ",\n          typedError: (r) {{\n            final _e = {};\n            r.assertConsumed();\n            return {}(_e);\n          }}",
                        decode_expr(iface, err),
                        error_exception_type(iface, err)
                    ),
                };
                let ret = match &spec.ret {
                    None => "      r.assertConsumed();\n".to_string(),
                    Some(t) => format!(
                        "      final _ret = {};\n      r.assertConsumed();\n      return _ret;\n",
                        decode_expr(iface, t)
                    ),
                };
                let (open, close) = fake_wire_scope(&enc);
                let _ = write!(
                    out,
                    "{ty} _fakeMirror{i}(FakeRequest _fakeReq, int id) {open}\
                     ({}) async {{\n\
                     \x20     if (!_fakeReq.wire.isChannelOpen(id)) {{\n\
                     \x20       throw StateError(_fakeClosureGone(_fakeReq.label));\n\
                     \x20     }}\n\
                     \x20     final w = {};\n\
                     {enc}\
                     \x20     final r = decodeEnvelope(\n\
                     \x20         await _fakeReq.wire.invokeChannel(id, w){typed_error});\n\
                     {ret}\
                     \x20   }}{close}\n\n",
                    params.join(", "),
                    hint.ctor()
                );
            }
        }
    }
}

// ------------------------------------------------ handle-bearing decls --

/// Structs and enums whose client class the fake cannot use as it stands, in
/// declaration order — the ones that reach a handle of either kind.
///
/// The two kinds point opposite ways on a **plain** declaration, and both land
/// here. A Dart-object handle makes the type argument-only (FR0031), so the
/// client emits no decoder and the fake's mirror class is the only Dart form
/// the decoded value can take. An **opaque** makes it return-only (FR0004), so
/// the client emits no encoder and the mirror is what the fake's own method
/// returns, holding `Fake<Type>` objects where the client class holds real
/// handles. An **inbound** declaration lands here too and points the first way
/// whichever kinds it reaches, its class being parameter-shaped: the mirror is
/// what the fake's method *receives*, holding the objects Rust would have
/// adopted. Which codec halves a mirror gets follows from that; the class
/// itself is the same in every case.
/// `web` drops the ones that reach a `native_only` handle: their `Fake<Type>`
/// fields name a fake this surface does not emit, mirroring the client class's
/// own absence.
/// True when the declaration `name` — one the author wrote, or one of `check`'s
/// expansions — has a fake mirror on this surface.
fn mirrored(iface: &Interface, name: &str, web: bool) -> bool {
    (decl_has_handle(iface, name) || decl_owns_handles(iface, name))
        && !(web && crate::check::decl_native_only(iface, name).is_some())
}

/// True when a **template** has a mirror class: when some instantiation of it
/// has a mirror on this surface. There is no other way to ask — a template is
/// not in `structs`, so it reaches no handle of its own; and if the template's
/// own field is a handle then every instantiation carries it, so the two
/// questions have the same answer wherever both are meaningful.
fn template_mirrored(iface: &Interface, template: &str, web: bool) -> bool {
    iface
        .structs
        .iter()
        .map(|s| (&s.name, &s.instance))
        .chain(iface.enums.iter().map(|e| (&e.name, &e.instance)))
        .filter(|(_, i)| i.as_ref().is_some_and(|i| i.template == template))
        .any(|(n, _)| mirrored(iface, n, web))
}

/// The declarations whose mirror **class** this surface emits: one per thing an
/// author declared, one per template — never one per expansion, which is a type
/// argument list on a template's mirror rather than a class of its own.
fn handle_structs(iface: &Interface, web: bool) -> Vec<&StructDecl> {
    iface
        .class_structs()
        .filter(|s| {
            if s.generics.is_empty() {
                mirrored(iface, &s.name, web)
            } else {
                template_mirrored(iface, &s.name, web)
            }
        })
        .collect()
}

fn handle_enums(iface: &Interface, web: bool) -> Vec<&EnumDecl> {
    iface
        .class_enums()
        .filter(|e| {
            if e.generics.is_empty() {
                mirrored(iface, &e.name, web)
            } else {
                template_mirrored(iface, &e.name, web)
            }
        })
        .collect()
}

/// The declarations whose mirror **codecs** this surface emits: one per wire
/// form, so the expansions and never the templates.
fn codec_structs(iface: &Interface, web: bool) -> Vec<&StructDecl> {
    iface
        .structs
        .iter()
        .filter(|s| mirrored(iface, &s.name, web))
        .collect()
}

fn codec_enums(iface: &Interface, web: bool) -> Vec<&EnumDecl> {
    iface
        .enums
        .iter()
        .filter(|e| mirrored(iface, &e.name, web))
        .collect()
}

/// The mirror class of one struct — from the template when there is one, so a
/// generic data type has ONE `FakePage<T>` exactly as it has one `Page<T>`.
fn emit_handle_struct(out: &mut String, iface: &Interface, s: &StructDecl) {
    let n = &s.name;
    // `n` is the Rust name the private `_fakeDec`/`_fakeEnc` helpers key
    // on (through `codec_stem`); `fcls` is the public mirror class name, which
    // follows the type's own Dart name; `head` carries the type parameters
    // when this is a template. See `emit_dart::emit_opaque`.
    let fcls = fake_data_class(iface, n);
    let head = fake_class_header(iface, n, &s.generics);
    // Positional where the client class is positional — see `emit_struct`.
    let tuple = s.shape == StructShape::Tuple;
    let what = if s.dart_interface {
        format!(
            "the `{n}` the caller implemented — one closure per method, bound to \
             the channel that method's handle opened"
        )
    } else if s.inbound {
        format!(
            "the `{n}` the caller passed. `{n}` is declared `inbound`, so its client \
             class holds a `Consumed<…>` per handle and this one holds the object \
             behind each: the fake IS the Rust side, and what arrives here is what \
             Rust would have adopted"
        )
    } else if decl_has_handle(iface, n) {
        format!("the `{n}` the caller passed, with each handle it carries bound to its channel")
    } else {
        format!(
            "the `{n}` this fake returns. Build it from `Fake<Type>` objects; the \
             harness registers one handle per field, exactly as the real Rust does"
        )
    };
    let _ = write!(
        out,
        "/// The fake side of {what}.\n\
         class {head} {{\n"
    );
    let spread = s.dart_interface;
    for f in &s.fields {
        emit_doc_comment(out, &f.docs, "  ");
        let ty = match (spread, f.ty.as_dart_object()) {
            (true, Some(spec)) => mirror_type(iface, spec, true),
            _ => fake_type(iface, &f.ty),
        };
        let _ = write!(out, "  final {ty} {};\n", field_name(f));
    }
    let params: Vec<String> = s
        .fields
        .iter()
        .map(|f| {
            let dn = field_name(f);
            if tuple {
                format!("this.{dn}")
            } else {
                format!("required this.{dn}")
            }
        })
        .collect();
    if tuple {
        let _ = write!(out, "  const {}({});\n}}\n\n", fcls, params.join(", "));
    } else {
        let _ = write!(out, "  const {}({{{}}});\n}}\n\n", fcls, params.join(", "));
    }
}

/// The mirror codecs of one struct — one per instantiation, for the reason
/// `emit_dart::emit_struct` gives.
fn emit_handle_struct_codec(out: &mut String, iface: &Interface, s: &StructDecl) {
    let n = &s.name;
    let fcls = fake_data_class(iface, n);
    let stem = iface.codec_stem(n);
    let tuple = s.shape == StructShape::Tuple;
    let spread = s.dart_interface;
    // An **inbound** struct is a decoder and no encoder — the mirror direction
    // of the returned case below, and for the same reason read the other way:
    // the client encodes it and never decodes it, so the fake does the
    // opposite. A handle field decodes by *taking* the object out of the
    // registry the fake minted it in, which is what the real Rust's adopt does.
    if s.inbound {
        let req = if decl_has_handle(iface, n) { ", FakeRequest _fakeReq" } else { "" };
        let _ = write!(
            out,
            "{fcls} _fakeDec{stem}(BinaryReader r, FakeWire _fakeWire{req}) {{\n"
        );
        for f in &s.fields {
            let expr = crate::emit_dart::decode_expr_fake_consumed(iface, &f.ty);
            let _ = write!(out, "  final {} = {expr};\n", field_name(f));
        }
        let args: Vec<String> = s
            .fields
            .iter()
            .map(|f| {
                let dn = field_name(f);
                if tuple {
                    dn
                } else {
                    format!("{dn}: {dn}")
                }
            })
            .collect();
        let _ = write!(out, "  return {}({});\n}}\n\n", fcls, args.join(", "));
        return;
    }
    if decl_has_handle(iface, n) {
        let _ = write!(
            out,
            "{fcls} _fakeDec{stem}(BinaryReader r, FakeRequest _fakeReq) {{\n"
        );
        for f in &s.fields {
            let expr = match (spread, f.ty.as_dart_object()) {
                (true, Some(spec)) => mirror_call(iface, spec, true),
                _ => decode_expr_fake(iface, &f.ty),
            };
            let _ = write!(out, "  final {} = {expr};\n", field_name(f));
        }
        let args: Vec<String> = s
            .fields
            .iter()
            .map(|f| {
                let dn = field_name(f);
                if tuple {
                    dn
                } else {
                    format!("{dn}: {dn}")
                }
            })
            .collect();
        let _ = write!(out, "  return {}({});\n}}\n\n", fcls, args.join(", "));
    }
    // Where the real Rust mints a registry entry for each handle it returns,
    // the fake mints one in `_fakeWire` — same exactly-once, same direction.
    if decl_owns_handles(iface, n) {
        let _ = write!(
            out,
            "void _fakeEnc{stem}(BinaryWriter w, FakeWire _fakeWire, {fcls} v) {{\n"
        );
        for f in &s.fields {
            encode_stmts_fake(out, iface, &f.ty, &format!("v.{}", field_name(f)), 1);
        }
        out.push_str("}\n\n");
    }
}

fn emit_handle_enum(out: &mut String, iface: &Interface, e: &EnumDecl) {
    let n = &e.name;
    // See `emit_handle_struct`.
    let fcls = fake_data_class(iface, n);
    let head = fake_class_header(iface, n, &e.generics);
    let _ = write!(
        out,
        "/// The fake side of `{n}`, whose variants carry handles. Same shape as\n\
         /// the client's sealed hierarchy, with `Fake<Type>` in place of every\n\
         /// handle field.\n\
         sealed class {head} {{\n  const {fcls}();\n}}\n"
    );
    for v in &e.variants {
        let cls = fake_variant_class(iface, e, v);
        let vhead = if e.generics.is_empty() {
            cls.clone()
        } else {
            format!("{cls}<{}>", e.generics.join(", "))
        };
        let _ = write!(out, "final class {vhead} extends {head} {{\n");
        for f in &v.fields {
            let _ = write!(
                out,
                "  final {} {};\n",
                fake_type(iface, &f.ty),
                field_name(f)
            );
        }
        if v.fields.is_empty() {
            let _ = write!(out, "  const {cls}();\n");
        } else if v.tuple {
            let params: Vec<String> = v
                .fields
                .iter()
                .map(|f| format!("this.{}", field_name(f)))
                .collect();
            let _ = write!(out, "  const {cls}({});\n", params.join(", "));
        } else {
            let params: Vec<String> = v
                .fields
                .iter()
                .map(|f| format!("required this.{}", field_name(f)))
                .collect();
            let _ = write!(out, "  const {cls}({{{}}});\n", params.join(", "));
        }
        out.push_str("}\n");
    }
}

fn emit_handle_enum_codec(out: &mut String, iface: &Interface, e: &EnumDecl) {
    let n = &e.name;
    let fcls = fake_data_class(iface, n);
    let stem = iface.codec_stem(n);
    // A variant class of a generic enum takes the same arguments the base does.
    let vtype = |cls: &str| match iface.instance_of(n) {
        Some(i) => format!(
            "{cls}<{}>",
            i.args
                .iter()
                .map(|a| fake_type(iface, a))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        None => cls.to_string(),
    };
    if decl_has_handle(iface, n) {
        let _ = write!(
            out,
            "\n{fcls} _fakeDec{stem}(BinaryReader r, FakeRequest _fakeReq) {{\n\
             \x20 final idx = r.readU32();\n  switch (idx) {{\n"
        );
        for (idx, v) in e.variants.iter().enumerate() {
            let cls = vtype(&fake_variant_class(iface, e, v));
            if v.fields.is_empty() {
                let _ = write!(out, "    case {idx}:\n      return const {cls}();\n");
                continue;
            }
            let _ = write!(out, "    case {idx}:\n");
            for f in &v.fields {
                let _ = write!(
                    out,
                    "      final {} = {};\n",
                    field_name(f),
                    decode_expr_fake(iface, &f.ty)
                );
            }
            let args: Vec<String> = if v.tuple {
                v.fields.iter().map(field_name).collect()
            } else {
                v.fields
                    .iter()
                    .map(|f| format!("{0}: {0}", field_name(f)))
                    .collect()
            };
            let _ = write!(out, "      return {cls}({});\n", args.join(", "));
        }
        let _ = write!(
            out,
            "    default:\n      throw StateError('frustrate codec: invalid variant index $idx for {n}');\n  }}\n}}\n\n"
        );
    }
    // See `emit_handle_struct_codec`: the fake mints where Rust mints.
    if decl_owns_handles(iface, n) {
        let _ = write!(
            out,
            "\nvoid _fakeEnc{stem}(BinaryWriter w, FakeWire _fakeWire, {fcls} v) {{\n"
        );
        for (idx, v) in e.variants.iter().enumerate() {
            // Applied, for the reason `emit_dart::emit_enum_codec` gives: a raw
            // generic in an `is` test does not promote.
            let cls = vtype(&fake_variant_class(iface, e, v));
            let kw = if idx == 0 { "if" } else { "} else if" };
            let _ = write!(out, "  {kw} (v is {cls}) {{\n    w.writeU32({idx});\n");
            for f in &v.fields {
                encode_stmts_fake(out, iface, &f.ty, &format!("v.{}", field_name(f)), 2);
            }
        }
        let _ = write!(
            out,
            "  }} else {{\n    throw StateError('frustrate codec: unknown {n} subtype');\n  }}\n}}\n\n"
        );
    }
}

// --------------------------------------------------------- fake classes --

/// Whether a member is answered by the fake at all on this surface. An omitted
/// member has no client to send it; a `web = "runtime_fail"` stub throws in the
/// client before anything crosses. Either way nothing reaches the harness, so
/// neither gets a fake method or a dispatch arm.
fn answered(f: &Function, web: bool) -> bool {
    matches!(web_fate(f, web), WebFate::Emit)
}

/// One fake method declaration: the client's signature with fake-side types,
/// no cancel token (the transport consumes it; it never reaches the wire), and
/// a body that throws.
///
/// Named parameters, on the client's rule (`emit_dart::dart_params`): the fake
/// mirrors the surface, and the dispatch arm below calls it by those names.
fn emit_fake_member(out: &mut String, iface: &Interface, f: &Function, name: &str, owner: &str) {
    emit_doc_comment(out, &f.docs, "  ");
    let mut params: Vec<String> = vec![];
    // A data receiver is a value, so the fake takes it as an ordinary leading
    // argument — the same position it occupies on the wire, and `self:` at the
    // call site. `self` is not a Dart keyword (nor a private name, which a
    // named parameter may not be) and cannot collide with a user parameter,
    // which Rust would not let them name `self` either.
    if let Some(ty) = data_receiver_type(iface, f) {
        params.push(format!("required {} self", fake_type(iface, &ty)));
    }
    params.extend(f.params.iter().map(|p| {
        format!(
            "{}{} {}",
            crate::emit_dart::required_kw(&p.ty),
            fake_type(iface, &p.ty),
            dart_name(&p.name)
        )
    }));
    let ret = match &f.ret {
        Some(t) => fake_type(iface, t),
        None => "void".into(),
    };
    let ret = match f.exec {
        Exec::Sync => ret,
        Exec::Async => format!("Future<{ret}>"),
    };
    let _ = write!(
        out,
        "  {} => throw UnimplementedError(\n\
         \x20     '{owner}.{} is not implemented by this fake');\n\n",
        // Not a property where the fake took the receiver as a parameter.
        crate::emit_dart::member_header(f.getter && params.is_empty(), &ret, name, &params),
        crate::emit_dart::dart_literal(name),
    );
}

fn emit_fake_opaque(out: &mut String, iface: &Interface, o: &OpaqueDecl, web: bool) {
    let n = &o.name;
    // See `emit_handle_struct`: `n` names the Rust item in prose and in the
    // private helpers; `fcls` is the mirror's own class name.
    let fcls = fake_handle_class(iface, n);
    // A trait's implementors must be usable where the trait is, so the
    // concrete fake declares the trait fake too. The members are the same
    // `Function` IR on both sides, so the signatures match by construction.
    let implements = {
        let traits = iface.implemented_traits(n);
        if traits.is_empty() {
            String::new()
        } else {
            format!(
                " implements {}",
                traits
                    .iter()
                    .map(|t| fake_handle_class(iface, &t.name))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    };
    let kind = match (o.dyn_trait, o.model) {
        (true, _) => "trait",
        (_, Model::Actor) => "actor",
        _ => "opaque type",
    };
    let _ = write!(
        out,
        "/// The fake behind the `{n}` {kind}. Extend it and override the members\n\
         /// your test exercises; the rest throw, and a throw a member does not\n\
         /// declare crosses as a panic naming it.\n\
         class {fcls}{implements} {{\n"
    );
    for f in iface
        .members(n, Repr::Handle)
        .filter(|f| f.receiver.is_some() && !f.is_actor_drop)
        .filter(|f| answered(f, web))
        .filter(|f| !on_crate_fake(iface, f))
    {
        emit_fake_member(out, iface, f, &member_name(f), &fake_handle_class(iface, n));
    }
    out.push_str("}\n\n");
}

fn emit_fake_crate(out: &mut String, iface: &Interface, web: bool) {
    let stem = crate_stem(iface);
    let _ = write!(
        out,
        "/// The fake behind every member of `{}` the bridge reaches without a\n\
         /// handle: its free functions, its static members, every constructor,\n\
         /// and every method of a `data` type (whose receiver arrives as the\n\
         /// first argument, because a value has no handle to resolve). A\n\
         /// constructor returns the `Fake<Type>` the bridge registers a handle\n\
         /// for, so the caller's generated handle class resolves back to it.\n\
         ///\n\
         /// Extend it and override what your test exercises.\n\
         class Fake{stem} {{\n",
        iface.crate_name
    );
    for f in iface
        .functions
        .iter()
        .filter(|f| on_crate_fake(iface, f) && !f.is_actor_drop)
        .filter(|f| answered(f, web))
    {
        emit_fake_member(out, iface, f, &free_name(iface, f), &format!("Fake{stem}"));
    }
    out.push_str("}\n\n");
}

// -------------------------------------------------------------- dispatch --

/// One dispatch arm: decode the request, call the fake, encode the answer.
fn emit_arm(out: &mut String, iface: &Interface, f: &Function) {
    let id = f.fn_id;
    let _ = write!(out, "      case {id}:\n        {{\n");

    // The synthetic actor drop is the one member with no fake method: the
    // caller's `dispose()` dispatches it to free the Rust object, and here
    // that is exactly retiring the registry entry — the same thing a
    // `HandleDrop.drop` does for an opaque handle.
    if f.is_actor_drop {
        let _ = write!(
            out,
            "          final _raw = r.readHandle();\n\
             \x20         r.assertConsumed();\n\
             \x20         _fakeWire.retireHandle(_raw);\n\
             \x20         return (BinaryWriter(1)..writeU8(statusOk)).takeBytes();\n\
             \x20       }}\n"
        );
        return;
    }

    // A member whose request carries a Dart-object handle gets a scope, for the
    // mirror-image of the reason the client's encoder gets a
    // `FrustrateOpenScope`: on the real bridge those channels end when the Rust
    // body drops them, and Dart has no drop. The scope ends them when the call
    // answers, and `FakeStreamSink.retain()` is how a fake says it kept one.
    // The receiver counts too when it is a value: a data type carrying a
    // Dart-object handle decodes through `_fakeDec…`, which reads `_fakeReq`.
    let recv_opens = iface
        .data_receiver(f)
        .is_some_and(|p| decl_has_handle(iface, p));
    let opens = recv_opens || f.params.iter().any(|p| has_handle(iface, &p.ty));
    let mut depth = 5;
    if opens {
        let _ = write!(
            out,
            "          final _fakeReq = FakeRequest(_fakeWire, _fakeLabel);\n\
             \x20         try {{\n"
        );
        depth += 1;
    }
    let pad = "  ".repeat(depth);

    if f.receiver.is_some() {
        if let Some(o) = iface.receiver_handle(f) {
            // A consuming receiver takes the object, so the fake's registry
            // gives it up here exactly as the real one does — and reads the
            // impl tag first where the client writes one, which is a consuming
            // receiver on a bridged trait (`emit_dart::emit_function`).
            let read = if matches!(f.receiver, Some(Receiver::Value | Receiver::Boxed)) {
                let take = if o.dyn_trait { "_fakeTakeTagged" } else { "_fakeTake" };
                format!("{take}<{}>(_fakeWire, r)", fake_handle_class(iface, &o.name))
            } else {
                format!(
                    "_fakeWire.resolveHandle(r.readHandle()) as {}",
                    fake_handle_class(iface, &o.name)
                )
            };
            let _ = write!(out, "{pad}final _self = {read};\n");
        } else {
            // A value receiver: decoded from the request exactly as Rust
            // decodes it, and handed to the crate fake as the first argument.
            let ty = data_receiver_type(iface, f).expect("a value receiver is declared");
            let _ = write!(out, "{pad}final _self = {};\n", decode_expr_fake(iface, &ty));
        }
    }
    for p in &f.params {
        let decode = if crate::emit_dart::consumed_param(iface, p) {
            crate::emit_dart::decode_expr_fake_consumed(iface, &p.ty)
        } else {
            decode_expr_fake(iface, &p.ty)
        };
        let _ = write!(out, "{pad}final {} = {decode};\n", dart_name(&p.name));
    }
    // Trailing bytes mean this arm and the client's encoder disagree about the
    // request. Nothing else would catch that: the fake would answer plausibly
    // from values read at the wrong offsets.
    let _ = write!(out, "{pad}r.assertConsumed();\n");

    let crate_fake = on_crate_fake(iface, f);
    let target = if crate_fake { "impl" } else { "_self" };
    let name = if crate_fake {
        free_name(iface, f)
    } else {
        member_name(f)
    };
    // Named, because the fake's members are (`emit_fake_member`). The local
    // holding a decoded argument is `dart_name(p)` — the parameter's own name —
    // so every argument reads `x: x`; the flattened data receiver is the one
    // whose local and label differ.
    let mut args: Vec<String> = vec![];
    if crate_fake && f.receiver.is_some() {
        args.push("self: _self".to_string());
    }
    args.extend(f.params.iter().map(|p| {
        let n = dart_name(&p.name);
        format!("{n}: {n}")
    }));
    let await_kw = match f.exec {
        Exec::Sync => "",
        Exec::Async => "await ",
    };
    // A `#[bridge(getter)]` member is a property on the fake too, so the arm
    // reads it rather than calling it. It takes no arguments by construction
    // (FR0066) — except a data receiver, which the crate fake takes as its
    // first parameter and so cannot be a property there.
    let getter = f.getter && !(crate_fake && f.receiver.is_some());
    let call = |args: &str| {
        if getter {
            String::new()
        } else {
            format!("({args})")
        }
    };

    if f.err.is_some() {
        let _ = write!(out, "{pad}try {{\n");
        depth += 1;
    }
    let body = "  ".repeat(depth);
    let mut hint = SizeHint::default();
    hint.add_fixed(1); // the status byte
    if let Some(t) = &f.ret {
        hint.add(t, "_ret");
    }
    match &f.ret {
        None => {
            let _ = write!(
                out,
                "{body}{await_kw}{target}.{name}{};\n\
                 {body}return (BinaryWriter(1)..writeU8(statusOk)).takeBytes();\n",
                call(&args.join(", "))
            );
        }
        Some(t) => {
            let mut enc = String::new();
            encode_stmts_fake(&mut enc, iface, t, "_ret", depth);
            let _ = write!(
                out,
                "{body}final _ret = {await_kw}{target}.{name}{};\n\
                 {body}final w = {}..writeU8(statusOk);\n\
                 {enc}\
                 {body}return w.takeBytes();\n",
                call(&args.join(", ")),
                hint.ctor()
            );
        }
    }
    if let Some(err) = &f.err {
        // Only this arm knows the error's type, so only this arm can encode it
        // as a value. Everything else a fake throws is handled by the dispatch
        // wrapper and crosses as prose.
        // Applied: `_x.error` is handed to the per-instantiation encoder, and a
        // raw `RefusalException` would make it a `Refusal<dynamic>`.
        let stem = error_exception_type(iface, err);
        let mut ehint = SizeHint::default();
        ehint.add_fixed(1); // the status byte
        ehint.add(err, "_e");
        let mut enc = String::new();
        encode_stmts_fake(&mut enc, iface, err, "_e", depth);
        let _ = write!(
            out,
            "{pad}}} on {stem} catch (_x) {{\n\
             {body}final _e = _x.error;\n\
             {body}final w = {}..writeU8(statusTypedError);\n\
             {enc}\
             {body}return w.takeBytes();\n\
             {pad}}}\n",
            ehint.ctor()
        );
    }
    if opens {
        let _ = write!(
            out,
            "          }} finally {{\n\
             \x20           _fakeReq.retire();\n\
             \x20         }}\n"
        );
    }
    let _ = write!(out, "        }}\n");
}

fn emit_bridge(out: &mut String, iface: &Interface, web: bool) {
    let stem = crate_stem(iface);
    let _ = write!(
        out,
        "/// Answers bridge requests from a [Fake{stem}] and the `Fake<Type>` objects\n\
         /// it hands out. Give one to a `FakeRuntime` (package:frustrate/testing.dart)\n\
         /// and activate it:\n\
         ///\n\
         /// ```dart\n\
         /// Frustrate.activate(FakeRuntime(Fake{stem}Bridge(MyApi())));\n\
         /// addTearDown(Frustrate.reset);\n\
         /// ```\n\
         ///\n\
         /// It accepts exactly [frustrateSchemaHash], so a harness generated from\n\
         /// one interface cannot answer for another's bindings.\n\
         final class Fake{stem}Bridge extends FakeBridge {{\n\
         \x20 /// The free functions, statics and constructors. Every other member\n\
         \x20 /// is reached through the `Fake<Type>` a constructor returned.\n\
         \x20 final Fake{stem} impl;\n\n\
         \x20 Fake{stem}Bridge(this.impl) : super(frustrateSchemaHash);\n\n\
         \x20 /// The member a fn id names, or a placeholder for one this\n\
         \x20 /// interface does not have — read only for attribution, and never\n\
         \x20 /// allowed to throw inside an error path.\n\
         \x20 static String _member(int fnId) =>\n\
         \x20     frustrateMemberNames[fnId] ?? 'fn#$fnId';\n\n"
    );

    for (which, exec) in [("answerSync", Exec::Sync), ("answerAsync", Exec::Async)] {
        let (sig, async_kw) = match exec {
            Exec::Sync => ("Uint8List answerSync(int fnId, BinaryReader r)", ""),
            Exec::Async => (
                "Future<Uint8List> answerAsync(int fnId, BinaryReader r)",
                "async ",
            ),
        };
        // The arms first, because whether this method needs the wire at all is
        // a property of the arms it ends up with — a receiver to resolve, a
        // handle to mint or retire, a scope to open. An interface whose whole
        // sync (or async) half is scalar-in/scalar-out uses none of them, and
        // a `final _fakeWire = wire;` nothing reads is an `unused_local_variable`
        // warning in the consumer's own `dart analyze`.
        let mut arms = String::new();
        for f in iface
            .functions
            .iter()
            .filter(|f| f.exec == exec)
            .filter(|f| answered(f, web))
        {
            emit_arm(&mut arms, iface, f);
        }
        // Reading `wire` throws when this bridge is not installed behind a fake
        // runtime, so hoisting it makes that a loud failure on entry rather
        // than mid-decode. Only where an arm would touch it: where none does,
        // there is nothing the missing wire could have broken.
        let hoist_wire = if arms.contains("_fakeWire") {
            "    final _fakeWire = wire;\n"
        } else {
            ""
        };
        let _ = write!(
            out,
            "  @override\n  {sig} {async_kw}{{\n\
             {hoist_wire}\
             \x20   final _fakeLabel = _member(fnId);\n\
             \x20   // A bare `catch`: an unimplemented member throws\n\
             \x20   // `UnimplementedError`, which is an Error and not an Exception.\n\
             \x20   try {{\n\
             \x20     switch (fnId) {{\n"
        );
        out.push_str(&arms);
        let _ = write!(
            out,
            "        default:\n\
             \x20         throw StateError(frustrateMemberNames.containsKey(fnId)\n\
             \x20             ? 'frustrate: $_fakeLabel is not on this surface, so nothing '\n\
             \x20                 'here can have called it (this is a bridge bug)'\n\
             \x20             : 'frustrate: fn id $fnId is not a member of this interface '\n\
             \x20                 '(this is a bridge bug)');\n\
             \x20     }}\n\
             \x20   }} catch (e) {{\n\
             \x20     return fakeThrown(e, _fakeLabel);\n\
             \x20   }}\n\
             \x20 }}\n\n"
        );
        let _ = which;
    }
    out.push_str("}\n");
}

// ------------------------------------------------------------------ entry --

pub(crate) fn emit(out: &mut String, iface: &Interface, web: bool) {
    out.push_str(
        "\n// -------------------------------------------------------------- fakes --\n\
         //\n\
         // Run these bindings against a fake instead of a library: extend the\n\
         // Fake* classes below, override what your test exercises, and activate\n\
         // `FakeRuntime(Fake<Crate>Bridge(yourFake))` from\n\
         // package:frustrate/testing.dart.\n\
         //\n\
         // Nothing here is reachable from an app that never constructs one, and\n\
         // nothing here implements FrustrateRuntime — package:frustrate/\n\
         // fake_contract.dart, which this names, declares only interfaces and\n\
         // value classes. That is what keeps the transport monomorphic in a\n\
         // production build (see the library doc there).\n\n",
    );
    // A trait handle arrives as `[impl tag][handle]`. One helper per trait
    // consumes the tag in order, because a fake resolves by raw and has one
    // registry — the tag is the Rust side's question.
    for o in iface.opaques.iter().filter(|o| o.dyn_trait) {
        if web && o.native_only {
            continue;
        }
        let n = &o.name;
        let fcls = fake_handle_class(iface, n);
        let _ = write!(
            out,
            "{fcls} _fakeResolve{n}(FakeWire _fakeWire, BinaryReader r) {{\n\
             \x20 r.readU8(); // impl tag: which Rust registry holds it. A fake has one.\n\
             \x20 return _fakeWire.resolveHandle(r.readHandle()) as {fcls};\n\
             }}\n\n"
        );
    }
    // The fake's half of a consume: the registry gives the object up as the
    // handle is read, so a raw the harness has already been handed resolves to
    // nothing afterwards — a loud `StateError` where a real double-take would
    // be a double free.
    out.push_str(
        "T _fakeTake<T>(FakeWire _fakeWire, BinaryReader r) {\n\
         \x20 final raw = r.readHandle();\n\
         \x20 final o = _fakeWire.resolveHandle(raw) as T;\n\
         \x20 _fakeWire.retireHandle(raw);\n\
         \x20 return o;\n\
         }\n\n",
    );
    // The same, for a trait-typed position: `[impl tag][handle]`, and the tag
    // is consumed in order for the reason `_fakeResolve{n}` consumes it.
    out.push_str(
        "T _fakeTakeTagged<T>(FakeWire _fakeWire, BinaryReader r) {\n\
         \x20 r.readU8(); // impl tag: which Rust registry holds it. A fake has one.\n\
         \x20 return _fakeTake<T>(_fakeWire, r);\n\
         }\n\n",
    );
    // A void Dart closure has nowhere to hang "keep this open past the call"
    // the way a sink does, so the harness always ends its channel when the call
    // answers. Calling one afterwards says so rather than going nowhere.
    out.push_str(
        "String _fakeClosureGone(String member) =>\n\
         \x20   'frustrate: this Dart callback was passed to $member, that call has '\n\
         \x20   'answered, and its channel is closed. A fake cannot keep a callback '\n\
         \x20   'the way a Rust body keeps one by storing it: Dart has no Drop, and a '\n\
         \x20   'closure has nowhere to say so (a sink does — FakeStreamSink.retain). '\n\
         \x20   'Run that shape against the real library.';\n\n",
    );
    emit_mirror_factories(out, iface);
    for s in handle_structs(iface, web) {
        emit_handle_struct(out, iface, s);
    }
    for e in handle_enums(iface, web) {
        emit_handle_enum(out, iface, e);
    }
    for s in codec_structs(iface, web) {
        emit_handle_struct_codec(out, iface, s);
    }
    for e in codec_enums(iface, web) {
        emit_handle_enum_codec(out, iface, e);
    }
    for o in &iface.opaques {
        if web && o.native_only {
            continue;
        }
        emit_fake_opaque(out, iface, o, web);
    }
    emit_fake_crate(out, iface, web);
    emit_bridge(out, iface, web);
}

#[cfg(test)]
mod tests {
    use crate::check::check;
    use crate::parse::parse_source;

    fn emit_all(src: &str) -> crate::emit_dart::DartBindings {
        let mut iface = check(parse_source(src, "crate::api").unwrap()).unwrap();
        iface.crate_name = "test_api".into();
        crate::emit_dart::emit(&iface, "test_api.frustrate")
    }

    fn emit_src(src: &str) -> String {
        emit_all(src).native
    }

    /// The two shapes that make an arm safe rather than merely present: the
    /// request is asserted fully consumed before the fake is called, and the
    /// answer is one buffer whose first byte is the status.
    #[test]
    fn a_sync_arm_consumes_the_request_and_frames_its_own_status() {
        let code = emit_src("#[bridge(sync)] pub fn add(a: i32, b: i32) -> i32 { a + b }");
        assert!(code.contains("  int add({required int a, required int b}) => throw UnimplementedError("), "{code}");
        assert!(
            code.contains(
                "          final a = r.readI32();\n\
                 \x20         final b = r.readI32();\n\
                 \x20         r.assertConsumed();\n\
                 \x20         final _ret = impl.add(a: a, b: b);\n\
                 \x20         final w = BinaryWriter()..writeU8(statusOk);\n\
                 \x20         w.writeI32(_ret);\n\
                 \x20         return w.takeBytes();"
            ),
            "{code}"
        );
    }

    /// The generated bindings are analyzed by the consumer, so a local nothing
    /// reads is a build failure there and not merely untidy. `_fakeWire` is
    /// hoisted per dispatch method, and the two halves of an interface can
    /// differ: a crate whose sync members are all scalar-in/scalar-out and
    /// whose objects are async gets it in `answerAsync` only.
    #[test]
    fn the_wire_is_hoisted_only_into_the_dispatch_method_that_reads_it() {
        /// Whether the method opening with `sig` declares `_fakeWire` before
        /// its switch — the region where an unread declaration would sit.
        fn hoisted(code: &str, sig: &str) -> bool {
            let body = &code[code.find(sig).expect("the dispatch method is emitted")..];
            let switch = body.find("switch (fnId)").expect("the switch is emitted");
            body[..switch].contains("final _fakeWire = wire;")
        }

        let scalars = emit_src(
            "#[bridge(sync)] pub fn add(a: i32, b: i32) -> i32 { a + b }\n\
             #[bridge] pub async fn slow(n: i64) -> i64 { n }",
        );
        // The `_fakeTake` helper names `_fakeWire` as its own parameter and is
        // emitted unconditionally, so the question is about the *dispatch
        // methods*: neither may hoist a wire it never reads.
        assert!(!hoisted(&scalars, "Uint8List answerSync"), "{scalars}");
        assert!(!hoisted(&scalars, "Future<Uint8List> answerAsync"), "{scalars}");

        let mixed = emit_src(
            r#"
            #[bridge(sync)] pub fn add(a: i32, b: i32) -> i32 { a + b }
            #[bridge(actor)] pub struct Miner { n: i64 }
            #[bridge] impl Miner {
                pub fn new() -> Self { todo!() }
                pub fn dig(&mut self, n: i64) -> i64 { n }
            }
            "#,
        );
        assert!(
            !hoisted(&mixed, "Uint8List answerSync"),
            "the sync half only adds two ints: {mixed}"
        );
        assert!(
            hoisted(&mixed, "Future<Uint8List> answerAsync"),
            "the async half mints, resolves and retires handles: {mixed}"
        );
    }

    /// A cancel token never reaches the wire — `PendingCalls` consumes it — so
    /// the fake's method has no parameter for it, on either shape that takes
    /// one (a Rust `async fn`, and a `Deferred<T>` actor method).
    #[test]
    fn a_cancel_token_is_not_a_fake_parameter() {
        let code = emit_src(
            r#"
            #[bridge] pub async fn slow(n: i64) -> i64 { n }
            #[bridge(actor)] pub struct Miner { n: i64 }
            #[bridge] impl Miner {
                pub fn new() -> Self { todo!() }
                pub fn dig(&mut self, n: i64) -> frustrate::Deferred<i64> { todo!() }
            }
            "#,
        );
        assert!(code.contains("Future<int> slow({required int n}) => throw UnimplementedError("), "{code}");
        assert!(code.contains("Future<int> dig({required int n}) => throw UnimplementedError("), "{code}");
        assert!(!code.contains("cancel) => throw UnimplementedError"), "{code}");
        // The client's own signatures still carry it.
        assert!(code.contains("FrustrateCancelToken? cancel})"), "{code}");
    }

    /// A constructor lands on the crate fake (it has no receiver to hang off),
    /// returns the user's `Fake<Type>`, and the arm mints a raw for it — which
    /// is what makes the caller's generated handle class resolve back to it.
    #[test]
    fn a_constructor_mints_a_handle_for_the_object_the_fake_returned() {
        let code = emit_src(
            r#"
            #[bridge(confined)] pub struct TextDoc { s: String }
            #[bridge] impl TextDoc {
                #[bridge(sync)] pub fn new() -> Self { todo!() }
                #[bridge(sync)] pub fn len(&self) -> i64 { 0 }
            }
            "#,
        );
        assert!(code.contains("  FakeTextDoc textDocNew() => throw UnimplementedError("), "{code}");
        assert!(code.contains("w.writeHandle(_fakeWire.mintHandle(_ret));"), "{code}");
        // An instance member resolves its receiver out of the same registry.
        assert!(
            code.contains(
                "final _self = _fakeWire.resolveHandle(r.readHandle()) as FakeTextDoc;"
            ),
            "{code}"
        );
        assert!(code.contains("final _ret = _self.len();"), "{code}");
    }

    /// An actor's `dispose()` dispatches a synthetic drop, which for a fake is
    /// exactly retiring the registry entry — the same thing `HandleDrop.drop`
    /// does for an opaque handle. It is the one member with no fake method.
    #[test]
    fn the_synthetic_actor_drop_retires_the_entry() {
        let code = emit_src(
            r#"
            #[bridge(actor)] pub struct Miner { n: i64 }
            #[bridge] impl Miner {
                pub fn new() -> Self { todo!() }
                pub fn dig(&mut self) -> i64 { 0 }
            }
            "#,
        );
        assert!(
            code.contains(
                "          final _raw = r.readHandle();\n\
                 \x20         r.assertConsumed();\n\
                 \x20         _fakeWire.retireHandle(_raw);"
            ),
            "{code}"
        );
        assert!(!code.contains("frustrateDropMiner"), "no fake method for it: {code}");
    }

    /// A stream parameter reaches the fake as the write end plus the consumer's
    /// cancel and pause state. `hasAddError` is the mirror's own answer: only an
    /// EventSink or a StreamController has that method, and a fake that called
    /// it on a plain Sink would otherwise throw inside the router, on a
    /// microtask, with nothing to attribute it to.
    #[test]
    fn a_stream_parameter_becomes_a_sink_that_knows_its_mirror() {
        let code = emit_src(
            r#"
            #[bridge(sync)] pub fn watch(sink: frustrate::StreamSink<i64>) {}
            #[bridge(sync)] pub fn fill(sink: frustrate::dart::core::Sink<i64>) {}
            "#,
        );
        assert!(code.contains("hasAddError: true);"), "{code}");
        assert!(code.contains("hasAddError: false);"), "{code}");
        assert!(
            code.contains("  void watch({required FakeStreamSink<int> sink}) => throw UnimplementedError("),
            "{code}"
        );
        assert!(
            code.contains("final sink = _fakeMirror0(_fakeReq, _fakeReq.track(r.readHandle()));"),
            "{code}"
        );
        // The scope is what ends the channel when the call answers, standing in
        // for the drop a Rust body performs by returning.
        assert!(
            code.contains(
                "          final _fakeReq = FakeRequest(_fakeWire, _fakeLabel);\n\
                 \x20         try {"
            ),
            "{code}"
        );
        assert!(
            code.contains(
                "          } finally {\n            _fakeReq.retire();\n          }"
            ),
            "{code}"
        );
    }

    /// A void closure delivers a selector-0 event; a value-returning one rides
    /// the invocation round trip and comes back as a `Future`, because a fake
    /// has no worker to park where Rust parks one.
    #[test]
    fn closures_reach_the_fake_as_closures() {
        let code = emit_src(
            r#"
            #[bridge(sync)] pub fn notify(cb: frustrate::DartCallback<i64>) {}
            #[bridge] pub async fn transform(f: frustrate::DartFunction<i64, String>) {}
            "#,
        );
        assert!(
            code.contains("  void notify({required void Function(int) cb}) => throw UnimplementedError("),
            "{code}"
        );
        assert!(code.contains("if (!_fakeReq.wire.deliver(id, 0, w)) {"), "{code}");
        assert!(
            code.contains(
                "  Future<void> transform({required Future<String> Function(int) f}) => throw UnimplementedError("
            ),
            "{code}"
        );
        assert!(
            code.contains("final r = decodeEnvelope(\n          await _fakeReq.wire.invokeChannel(id, w));"),
            "{code}"
        );
        // One message for one fact: a closure whose call has answered says the
        // same thing whether it returns a value or not. Three sites, not two:
        // the void mirror tests the channel *before* encoding as well, because
        // an argument that reaches a handle is minted by that encode and a
        // throw afterwards would strand it.
        assert_eq!(code.matches("_fakeClosureGone(_fakeReq.label)").count(), 3, "{code}");
    }

    /// A closure that may refuse hands the fake the same exception class the
    /// caller throws in the other direction — one vocabulary, mirrored — and
    /// wears the same alias, since Dart has no checked exceptions.
    #[test]
    fn a_fallible_closure_carries_its_declared_error_both_ways() {
        let code = emit_src(
            r#"
            #[bridge(data)] pub struct Refusal { why: String }
            #[bridge]
            pub async fn ask(f: frustrate::DartFunction<i64, Result<i64, Refusal>>) {}
            "#,
        );
        assert!(
            code.contains(
                "  Future<void> ask({required RefusalFallible<Future<int> Function(int)> f}) => throw UnimplementedError("
            ),
            "{code}"
        );
        assert!(code.contains("return RefusalException(_e);"), "{code}");
    }

    /// A declared Dart interface reaches the fake as one closure per method,
    /// with a top-level tuple spread into positional arguments — the same
    /// spread the client's method tear-off produces, so the two sides read as
    /// the same interface.
    #[test]
    fn a_dart_interface_becomes_a_record_of_closures_with_the_same_spread() {
        let code = emit_src(
            r#"
            #[bridge(data, dart_interface)] pub struct Auditor {
                note: frustrate::DartCallback<String>,
                approve: frustrate::DartCallback<(i64, String)>,
            }
            #[bridge(sync)] pub fn audit(a: Auditor) {}
            "#,
        );
        assert!(code.contains("class FakeAuditor {"), "{code}");
        assert!(code.contains("  final void Function(String) note;"), "{code}");
        assert!(code.contains("  final void Function(int, String) approve;"), "{code}");
        assert!(code.contains("final a = _fakeDecAuditor(r, _fakeReq);"), "{code}");
    }

    /// A channel whose item carries a handle is the fake's to **mint**: the
    /// mirror is typed in the fake's vocabulary (`FakeDoc`, not the client's
    /// `Doc`, which a fake has no way to construct) and its encoder goes
    /// through the fake registry. The factory becomes a block so it can name
    /// the wire; one that mints nothing stays an arrow.
    #[test]
    fn a_fake_mirror_mints_its_handle_items() {
        let code = emit_src(
            r#"
            #[bridge(confined)] pub struct Doc { pub n: i64 }
            #[bridge] impl Doc { #[bridge(sync)] pub fn n(&self) -> i64 { self.n } }
            #[bridge(sync)] pub fn make() -> Doc { todo!() }
            #[bridge] pub fn watch(s: StreamSink<Doc>) {}
            #[bridge] pub fn plain(s: StreamSink<i64>) {}
            "#,
        );
        assert!(code.contains("FakeStreamSink<FakeDoc> _fakeMirror"), "{code}");
        assert!(code.contains("final _fakeWire = _fakeReq.wire;"), "{code}");
        assert!(code.contains("w.writeHandle(_fakeWire.mintHandle(v));"), "{code}");
        // The value channel is untouched: still an arrow, still no wire.
        assert!(code.contains("FakeStreamSink<int> _fakeMirror"), "{code}");
        assert!(code.contains("w.writeI64(v);"), "{code}");
        // And the fake's method signature speaks the fake's item type.
        assert!(code.contains("watch({required FakeStreamSink<FakeDoc> s})"), "{code}");
    }

    /// A member's own typed error is the one throw the dispatch wrapper cannot
    /// handle: only this arm knows how to encode the value.
    #[test]
    fn a_typed_error_is_encoded_by_the_arm_that_declares_it() {
        let code = emit_src(
            r#"
            #[bridge(data)] pub enum Denied { NoFunds, Frozen }
            #[bridge(sync)] pub fn withdraw(amount: i64) -> Result<i64, Denied> { Ok(amount) }
            "#,
        );
        assert!(code.contains("} on DeniedException catch (_x) {"), "{code}");
        assert!(code.contains("final w = BinaryWriter()..writeU8(statusTypedError);"), "{code}");
        assert!(code.contains("_encDenied(w, _e);"), "{code}");
    }

    /// A trait-typed parameter arrives as `[impl tag][handle]`. The tag says
    /// which Rust registry holds the object and a fake has one, so the helper
    /// consumes it in order and resolves by raw. The concrete fake declares the
    /// trait fake, so it is usable wherever the trait is.
    #[test]
    fn a_trait_handle_consumes_its_impl_tag_and_resolves_by_raw() {
        let code = emit_src(
            r#"
            #[bridge(frozen)] pub trait Greeter: Send + Sync {
                #[bridge(sync)] fn greet(&self) -> String;
            }
            #[bridge(frozen)] pub struct Robot { n: i32 }
            #[bridge] impl Robot {
                #[bridge(sync)] pub fn new() -> Self { todo!() }
            }
            #[bridge] impl Greeter for Robot {
                #[bridge(sync)] fn greet(&self) -> String { todo!() }
            }
            #[bridge(sync)] pub fn hail(g: &dyn Greeter) -> String { todo!() }
            "#,
        );
        assert!(code.contains("FakeGreeter _fakeResolveGreeter("), "{code}");
        assert!(code.contains("r.readU8(); // impl tag"), "{code}");
        assert!(code.contains("final g = _fakeResolveGreeter(_fakeWire, r);"), "{code}");
        assert!(code.contains("class FakeRobot implements FakeGreeter {"), "{code}");
    }

    /// A struct or an enum that reaches a Dart-object handle has no client
    /// decoder at all (FR0031 makes it argument-only), so the fake's mirror is
    /// the only Dart form the decoded value can take.
    #[test]
    fn a_handle_nested_in_a_data_type_gets_a_fake_mirror() {
        let code = emit_src(
            r#"
            #[bridge(data)] pub struct Fanout { label: String, out: frustrate::DartCallback<i64> }
            #[bridge(data)] pub enum Route { Silent, Chatty { out: frustrate::DartCallback<i64> } }
            #[bridge(sync)] pub fn fan(f: Fanout) {}
            #[bridge(sync)] pub fn route(route: Route) {}
            "#,
        );
        // The struct: fields in wire order, handles bound to their channels.
        assert!(code.contains("class FakeFanout {"), "{code}");
        assert!(code.contains("  final void Function(int) out;"), "{code}");
        assert!(
            code.contains("FakeFanout _fakeDecFanout(BinaryReader r, FakeRequest _fakeReq) {"),
            "{code}"
        );
        // The enum: the same sealed hierarchy the client has, one level over.
        assert!(code.contains("sealed class FakeRoute {"), "{code}");
        assert!(code.contains("final class FakeRouteChatty extends FakeRoute {"), "{code}");
        assert!(code.contains("      return const FakeRouteSilent();"), "{code}");
        assert!(
            code.contains("invalid variant index $idx for Route"),
            "{code}"
        );
    }

    /// A member the web surface omits has no client to send it, and a
    /// `web = "runtime_fail"` stub throws before anything crosses. Neither gets
    /// a fake method or an arm, and the default arm distinguishes "not on this
    /// surface" from "not a member of this interface at all".
    #[test]
    fn the_web_harness_omits_what_the_web_client_cannot_send() {
        let b = emit_all(
            r#"
            #[bridge(sync, native_only)] pub fn dial(t: String) -> i64 { 0 }
            #[bridge(sync)] pub fn ping() -> i64 { 0 }
            "#,
        );
        assert!(b.native.contains("  int dial({required String t}) => throw UnimplementedError("), "{}", b.native);
        assert!(!b.web.contains("int dial({required String t}) => throw"), "{}", b.web);
        assert!(b.web.contains("  int ping() => throw UnimplementedError("), "{}", b.web);
        assert!(
            b.web.contains("is not on this surface, so nothing "),
            "{}",
            b.web
        );
        assert!(
            b.web.contains("is not a member of this interface "),
            "{}",
            b.web
        );
    }

    /// The flattened fake name is a concatenation and therefore not injective:
    /// `TextDoc::new` and a free `text_doc_new` both reach `textDocNew`. That
    /// is FR0002, and `dart_identifier` is how the author says which one moves.
    #[test]
    fn a_flattened_fake_name_collision_is_reported_and_renameable() {
        let src = r#"
            #[bridge(confined)] pub struct TextDoc { s: String }
            #[bridge] impl TextDoc {
                #[bridge(sync)] pub fn new() -> Self { todo!() }
            }
            #[bridge(sync)] pub fn text_doc_new() -> i64 { 0 }
        "#;
        let ds = check(parse_source(src, "crate::api").unwrap()).unwrap_err();
        let d = ds.iter().find(|d| d.code == "FR0002").unwrap();
        assert!(d.message.contains("textDocNew"), "{}", d.message);
        assert!(d.message.contains("dart_identifier"), "{}", d.message);

        // The same source, with the free function renamed, builds — and the
        // rename reaches the emitted surface.
        let code = emit_src(
            r#"
            #[bridge(confined)] pub struct TextDoc { s: String }
            #[bridge] impl TextDoc {
                #[bridge(sync)] pub fn new() -> Self { todo!() }
            }
            #[bridge(sync, dart_identifier = "textDocCount")]
            pub fn text_doc_new() -> i64 { 0 }
            "#,
        );
        assert!(code.contains("  FakeTextDoc textDocNew()"), "{code}");
        assert!(code.contains("int textDocCount()"), "{code}");
        assert!(!code.contains("int textDocNew()"), "{code}");
    }
}
