//! Inert attributes for frustrate bridge declarations.
//!
//! `#[frustrate::bridge]` (re-exported from this crate by the `frustrate-runtime`
//! crate as `frustrate::bridge`) marks items for the frustrate code generator.
//! The generator reads the *source* of declared bridge files with `syn`; it never
//! expands macros. This attribute therefore does nothing at compile time except
//! re-emit the item — it exists so that rustc accepts the annotation.
//!
//! `docs/ANNOTATIONS.md` is the contract reference: for every option below
//! that lets you assert something codegen cannot check for itself, it states
//! what correct usage requires, why it isn't statically checkable, and what
//! you are establishing by writing it. This comment stays the terse list.
//!
//! Accepted forms — the complete set; the generator's `parse_bridge_option`
//! (codegen/src/parse.rs) is the authority, and rejects anything else:
//!   #[bridge]                       function or impl block (no representation
//!                                   to name — see below for struct/enum/trait)
//!   #[bridge(sync)]                 sync function/method (runs on the caller)
//!   #[bridge(data)]                 struct or enum: crosses by value
//!   #[bridge(confined)]             struct or trait: opaque handle, Confined model
//!   #[bridge(resident)]             struct or trait: opaque handle, Resident model
//!                                   (no Send bound; one thread, sync members)
//!   #[bridge(frozen)]               struct or trait: opaque handle, Frozen model
//!   #[bridge(locked)]               struct or trait: opaque handle, Locked model
//!   #[bridge(actor)]                struct or trait: opaque handle, Actor model —
//!                                   the instance lives on its own executor (a
//!                                   thread on native, a Worker on web), so its
//!                                   methods are async by construction
//!   #[bridge(data(dart_identifier = "…"))]     the Dart class name for one
//!   #[bridge(locked(dart_identifier = "…"))]   half of a declaration. Both
//!                                   halves derive the same name, so a
//!                                   declaration naming two representations
//!                                   renames one of them (FR0068 refuses a
//!                                   bare `dart_identifier` there, which does
//!                                   not say which class it means)
//!
//!   A struct, enum or trait declaration must name at least one of the six
//!   representations above (FR0058); `data` is only valid on a struct or enum
//!   (FR0061), and at most one concurrency model may be named (FR0060). A
//!   struct may name `data` *and* a model: it then crosses as both a value
//!   class and a handle class, and each use site and `impl` block says which
//!   half it means through the markers below. A function or impl block has no
//!   representation to name, so `#[bridge]`/`#[bridge(sync)]` on those are
//!   unaffected by this list.
//!   #[bridge(sync, on_contention = "error")]   contract-marked sync on Locked:
//!                                   try-lock, contended -> ContentionException.
//!                                   On every target, the browser main thread
//!                                   included. The other value is "block"
//!                                   (blocking acquisition), which waits to
//!                                   acquire and is native-only
//!   #[bridge(bytes(dart = "T", import = "<uri>"))]
//!                                   bridge-external value type: one
//!                                   length-prefixed byte payload converted by
//!                                   `frustrate::BytesCodec` on the Rust side
//!                                   and by `encode`/`decode` on the Dart side
//!                                   (both default to the protobuf conventions,
//!                                   both overridable as further bytes options)
//!   #[bridge(no_eq)]                data struct or data enum: identity
//!                                   ==/hashCode instead of the generated deep
//!                                   value equality (copyWith/toString stay)
//!   #[bridge(dart_interface)]       data struct whose fields are all
//!                                   DartCallback/DartFunction: its Dart side
//!                                   is an `abstract interface class` the
//!                                   caller implements, one method per field,
//!                                   instead of a class of `required` fields.
//!                                   Rust and the wire are unchanged
//!   #[bridge(inbound)]              data struct that crosses Dart -> Rust only:
//!                                   its handle fields are `Consumed<T>` tokens
//!                                   on the Dart side, moved into Rust
//!   #[bridge(getter)]              member with no parameters and a return:
//!                                   emitted as a Dart *property* (`d.name`,
//!                                   not `d.name()`). Dart-surface only — same
//!                                   call, same wire, same dispatch id — and
//!                                   each model's own rules still decide
//!                                   sync-ness. A parameter, a `()` return, or
//!                                   a signature carrying a cancel token (any
//!                                   `async fn`, any `Deferred`) has nowhere to
//!                                   go on a property and is FR0066
//!   #[bridge(dart_identifier = "…")]
//!                                   function, method, field, variant or type
//!                                   declaration: the Dart name it lands under,
//!                                   replacing the one derived from the Rust
//!                                   name. The derivation is not injective —
//!                                   it lower-camels a name, and joins compound
//!                                   names (`{Enum}{Variant}`, the fake's
//!                                   parent + member) by concatenation — so two
//!                                   items can want one Dart name. That is
//!                                   FR0002, naming both, and this says which
//!                                   moves. Dart-surface only: the wire keys on
//!                                   fn_id and field order, never a name
//!   #[bridge(web = "runtime_fail")] opt a native-only member back INTO the web
//!                                   surface as a stub that throws loudly if it
//!                                   is actually called, so portable code that
//!                                   names it still compiles
//!   #[bridge(native_only)]          member, impl/trait block, or handle type
//!                                   that does not build for wasm32: omitted
//!                                   from the web surface and the wasm dispatch
//!   #[bridge(skip)]                 deliberately NOT bridged. Changes nothing
//!                                   about the generated interface — an
//!                                   unannotated item is skipped either way —
//!                                   it declares that the omission is intended,
//!                                   which silences the FR0042 warning and
//!                                   excludes a method from an annotated
//!                                   impl/trait block. Takes no other options.
//!   #[bridge(no_block)]             member, impl block, or trait: a claim that
//!                                   main-thread Dart calling this cannot be
//!                                   stalled — nothing the caller runs reaches
//!                                   a wait instruction. Proven at link time by
//!                                   bazel/wasm_block_check, or settled by
//!                                   placement; contradictions that codegen can
//!                                   already see are FR0048 and FR0049. Not valid on a data type,
//!                                   which has no body. For a whole file, see
//!                                   `bridge_file!` below.
//!
//! File-level form:
//!   frustrate::bridge_file!(no_block);
//!                                   one line at the top of a bridge file,
//!                                   claiming every bridged item in it. Only
//!                                   `no_block` is accepted today.
//!
//! A use site — a parameter, a return, a struct field, a `Vec`/`Option`/
//! `HashMap` element, or an `impl` block's self type — may name a
//! representation explicitly through one of six zero-cost marker types
//! (`frustrate::{Data,Confined,Resident,Frozen,Locked,Actor}`, `runtime/rust/src/
//! lib.rs`): `fn f(doc: &Locked<Doc>)`. Each is `T` with an identity mapping
//! (`pub type Locked<T> = T;`). It is optional; checked against the
//! declaration (FR0062 on a mismatch) and then erased to the plain type it
//! names.
//!
//! Optional where a type declares one representation; required where it
//! declares two, which is the case the marker exists for. A bare name with
//! two declarations resolves to neither (FR0067), so each position — and
//! each `#[bridge] impl` block's self type, which is what puts a member on
//! one of the two generated Dart classes — says which half it means.
//!
//! There is **no attribute for consuming a handle**, and there is not going to
//! be one. A Rust signature that takes `self`, `self: Box<Self>`, or a handle
//! by value already says the call consumes the object; what needed saying was
//! on the other side, where "nothing uses this handle after the call" is a
//! fact about the *Dart* program. So the opt-in is written there, as a type:
//! the handle's generated `take()` yields a `Consumed<T>`, which is the only
//! thing such a member accepts. An attribute the Rust author writes could not
//! have attested it.

use proc_macro::TokenStream;

/// No-op marker attribute. See crate docs.
#[proc_macro_attribute]
pub fn bridge(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}

/// File-level bridge claims: `frustrate::bridge_file!(no_block);`
///
/// Expands to nothing. Like [`macro@bridge`], it exists so rustc accepts the
/// line; the generator reads it out of the *source*.
///
/// **Why a macro and not `#![bridge(no_block)]`.** A file-scoped claim wants to
/// be an inner attribute, but custom inner attributes are unstable
/// (`custom_inner_attributes`), and frustrate is a stable-Rust project. An
/// item-position macro invocation is the stable spelling with the same reading
/// order — one line, at the top, binding the file.
///
/// **Why a file and not a build rule.** The claim is not surface-neutral: it
/// makes codegen emit `#[cfg(frustrate_block_check)]` check roots. A claim
/// carried in `BUILD.bazel` would therefore make the same sources generate
/// different Rust under cargo and under Bazel; the generated surface stays a
/// pure function of the source. What the build *does* choose is which check
/// artifacts get built and scanned.
///
/// Claims strengthen and never subtract: a per-item `#[bridge(...)]` inside a
/// claimed file cannot opt out. If one member must block, it does not belong in
/// a claimed file.
#[proc_macro]
pub fn bridge_file(_input: TokenStream) -> TokenStream {
    TokenStream::new()
}
