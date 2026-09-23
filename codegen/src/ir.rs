//! The frustrate interface representation (IR).
//!
//! Produced by `parse`, checked and finalized by `check`, consumed by the
//! emitters. Serializable so that build systems can treat the interface
//! description as a declared artifact between extraction and emission.
//!
//! The IR deliberately represents more than the codegen accepts, so that a
//! new feature extends it rather than breaking it. What the checker rejects
//! is a decision the checker states; the IR is not where a restriction lives.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Interface {
    /// Rust crate name of the bridge crate (used in generated code headers).
    pub crate_name: String,
    /// One name may appear in **both** `structs` and `opaques`: a
    /// `#[bridge(data, locked)]` struct declares two representations of one
    /// Rust item and crosses as two Dart classes. See
    /// [`Interface::is_dual`] and [`Function::parent_repr`]; every other
    /// repeated type name is FR0002.
    pub structs: Vec<StructDecl>,
    pub enums: Vec<EnumDecl>,
    /// Generic data **templates** — `#[bridge(data)] struct Page<T>` — kept
    /// apart from [`Interface::structs`] because a template is not a type: it
    /// has no fields the wire can describe until its parameters are bound.
    ///
    /// `check` expands every fully-applied use into a synthetic non-generic
    /// [`StructDecl`] in `structs` carrying an [`Instance`], and those are the
    /// wire truth — so this is `#[serde(skip)]` and out of the schema
    /// fingerprint, while the expansions it produced are in it.
    ///
    /// The Dart surface is the other way round: ONE generic class comes from
    /// the template, and every instantiation is a *type* (`Page<Item>`) rather
    /// than a name, so the emitters read this for the class and `structs` for
    /// the codecs.
    #[serde(skip, default)]
    pub generic_structs: Vec<StructDecl>,
    /// Generic data enum templates — see [`Interface::generic_structs`].
    #[serde(skip, default)]
    pub generic_enums: Vec<EnumDecl>,
    pub opaques: Vec<OpaqueDecl>,
    /// Bridge-external types (`#[bridge(bytes(...))]`): cross as bytes via
    /// user codecs on both sides.
    #[serde(default)]
    pub externs: Vec<ExternDecl>,
    /// All bridged functions: free functions and methods, in declaration
    /// order. `fn_id` is assigned by `check::finalize` and is the dispatch
    /// key shared by the generated Rust and Dart.
    pub functions: Vec<Function>,
}

/// How a struct declares its fields. Read straight from the syntax at parse
/// time — the one place the truth exists — and carried here because the
/// emitters need it: a tuple struct is reached positionally (`v.0`, and a
/// positional Dart constructor) where a named one is reached by name.
///
/// Three-way rather than a `tuple` flag like [`Variant::tuple`], because
/// `struct N {}` and `struct U;` both have an empty field list and are
/// different Rust: one is rebuilt as `N {}`, the other as a bare `U`.
///
/// Not wire-relevant — a tuple struct's fields would encode positionally in
/// declaration order exactly as named ones do — so `#[serde(skip)]`, out of
/// the schema fingerprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StructShape {
    /// `struct S { a: A, b: B }`.
    #[default]
    Named,
    /// `struct S(A, B);` — fields carry synthesized names `field0`,
    /// `field1`, …, like a tuple [`Variant`].
    Tuple,
    /// `struct S;` — no fields at all.
    Unit,
}

/// One fully-applied use of a generic data template: what a synthetic
/// declaration was expanded from.
///
/// The declaration's own `name` is the Rust spelling (`Page<Item>`), which is
/// unique because it *is* the Rust type. That spelling is not an identifier,
/// so [`Instance::stem`] carries the identifier form the codec functions are
/// named with.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Instance {
    /// The template's Rust name (`Page`). Looked up in
    /// [`Interface::generic_structs`] / [`Interface::generic_enums`], which is
    /// where the Dart class name and its parameter list come from.
    pub template: String,
    /// The type arguments, resolved and with representation markers erased, in
    /// declaration order. `dart_type` renders the Dart type arguments from
    /// these and `rust_type` the Rust ones.
    pub args: Vec<Type>,
    /// An identifier form of `name`, unique across the interface and unable to
    /// collide with any Rust type name — see `check::instance_stem` for the
    /// encoding and the argument that it cannot collide.
    pub stem: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StructDecl {
    pub name: String,
    pub module_path: String,
    pub fields: Vec<Field>,
    /// See [`StructShape`]. Use this — never the field names — to tell tuple
    /// from named, so a named field literally called `field0` is not misread
    /// as positional.
    #[serde(skip, default)]
    pub shape: StructShape,
    /// The declaration's **type** parameter names, in order (`struct
    /// Page<T, U>` → `["T", "U"]`). Lifetimes are not recorded: the bridge
    /// drops them, and a `struct S<'a>` crosses like any other. Const
    /// parameters are [`StructDecl::const_generics`].
    ///
    /// Non-empty means this declaration is a **template** and lives in
    /// [`Interface::generic_structs`]: a field whose type is one of these names
    /// is a parameter, and `check`'s expansion binds it per instantiation.
    ///
    /// Never emitted, hence `#[serde(skip)]`: a template has no wire form of
    /// its own — the expansions do — so the schema fingerprint does not move.
    #[serde(skip, default)]
    pub generics: Vec<String>,
    /// The declaration's **const** parameter names, in order (`struct
    /// Cache<const N: usize>` → `["N"]`). Refused (FR0056): a const shapes the
    /// wire — it is a length, a capacity, an array bound — and Dart's generics
    /// carry types only, so an instantiation could not be spelled on the far
    /// side. Recorded rather than dropped so the refusal can name it.
    #[serde(skip, default)]
    pub const_generics: Vec<String>,
    /// Parameter names that declare a default (`struct Page<T = i64>`).
    /// Refused (FR0056): a bare `Page` in a signature would then mean
    /// `Page<i64>`, and the bridge derives its instantiations from what the
    /// signatures write. Recorded so the refusal can name the parameter.
    #[serde(skip, default)]
    pub defaulted_generics: Vec<String>,
    /// Set on the synthetic declarations `check`'s expansion mints, one per
    /// fully-applied use of a template. `None` on everything an author wrote.
    ///
    /// `#[serde(skip)]`: the expansion's `name`, `module_path` and `fields` are
    /// serialized and are the whole of its wire form; this says only which
    /// template and arguments it came from, which the *emitters* need (one
    /// generic Dart class, a Rust path spelling the arguments) and the wire
    /// does not.
    #[serde(skip, default)]
    pub instance: Option<Instance>,
    /// `#[bridge(no_eq)]`: opt OUT of generated value equality, restoring Dart's
    /// identity `==`/`hashCode`. Dart-surface
    /// only — the wire is unchanged — so `#[serde(skip)]` keeps it out of the
    /// schema fingerprint, like [`StructDecl::docs`].
    #[serde(skip, default)]
    pub no_eq: bool,
    /// `#[bridge(dart_interface)]`: the Dart side of this struct is an
    /// `abstract interface class` the caller **implements**, one method per
    /// field, rather than a class of `required` named fields.
    ///
    /// Dart-surface only — every field is a closure mirror (FR0050), so the
    /// Rust struct and the wire are unchanged and only the encoder differs,
    /// reaching each handle through a method tear-off (`v.onChange`) instead of
    /// a field read.
    ///
    /// So `#[serde(skip)]` keeps it out of the schema
    /// fingerprint exactly like [`StructDecl::no_eq`]: two halves that disagree
    /// about the *shape of the Dart class* still speak the same wire, and field
    /// order — which is method order, and is wire-relevant — is serialized
    /// already.
    #[serde(skip, default)]
    pub dart_interface: bool,
    /// `#[bridge(inbound)]`: this struct crosses **Dart → Rust only**, so a
    /// handle field of it is one the caller hands over — `Consumed<T>` on the
    /// Dart side, the plain owned `T` on the Rust side, adopted out of the
    /// request like any other consumed handle.
    ///
    /// Declared rather than inferred from the members that name the type: a
    /// class whose field types followed interface-wide usage would change shape
    /// when an unrelated member was added elsewhere, and the generated Dart
    /// class stays a function of its own declaration.
    ///
    /// **Not** `#[serde(skip)]`, unlike every other Dart-surface flag above:
    /// this one says which way the handle behind a field travels, and that is
    /// the difference between the glue minting a handle and adopting one. A
    /// stale peer that disagreed about it would read a request-side id as a
    /// response-side mint straight into an `unsafe` deref, which is exactly
    /// what the fingerprint exists to refuse ([`crate::hash`]).
    #[serde(default)]
    pub inbound: bool,
    /// `#[bridge(dart_identifier = "…")]`: the Dart name this lands under,
    /// replacing the one derived from the Rust name.
    ///
    /// The derivation is not injective — `to_lower_camel_case` collapses
    /// `word_count` and `wordCount`, and a compound like `{Enum}{Variant}`
    /// loses the boundary between its halves — so two distinct Rust items can
    /// want one Dart name. Which of them should move is information only the
    /// author has, so the collision is an error (FR0002) and this carries the
    /// decision past it.
    ///
    /// Dart-surface only: the wire keys on `fn_id` and field order, never on a
    /// name, so `#[serde(skip)]` keeps it out of the schema fingerprint.
    #[serde(skip, default)]
    pub dart_identifier: Option<String>,
    /// Rustdoc lines (`///`) on the declaration, in order, preserved onto the
    /// generated Dart surface so they are not silently lost. Cosmetic, not
    /// wire-relevant, so `#[serde(skip)]` — out of the schema fingerprint.
    #[serde(skip, default)]
    pub docs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Field {
    pub name: String,
    pub ty: Type,
    /// `#[bridge(dart_identifier = "…")]`: the Dart name this lands under,
    /// replacing the one derived from the Rust name.
    ///
    /// The derivation is not injective — `to_lower_camel_case` collapses
    /// `word_count` and `wordCount`, and a compound like `{Enum}{Variant}`
    /// loses the boundary between its halves — so two distinct Rust items can
    /// want one Dart name. Which of them should move is information only the
    /// author has, so the collision is an error (FR0002) and this carries the
    /// decision past it.
    ///
    /// Dart-surface only: the wire keys on `fn_id` and field order, never on a
    /// name, so `#[serde(skip)]` keeps it out of the schema fingerprint.
    #[serde(skip, default)]
    pub dart_identifier: Option<String>,
    /// See [`StructDecl::docs`].
    #[serde(skip, default)]
    pub docs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnumDecl {
    pub name: String,
    pub module_path: String,
    pub variants: Vec<Variant>,
    /// See [`StructDecl::generics`].
    #[serde(skip, default)]
    pub generics: Vec<String>,
    /// See [`StructDecl::const_generics`].
    #[serde(skip, default)]
    pub const_generics: Vec<String>,
    /// See [`StructDecl::defaulted_generics`].
    #[serde(skip, default)]
    pub defaulted_generics: Vec<String>,
    /// See [`StructDecl::instance`].
    #[serde(skip, default)]
    pub instance: Option<Instance>,
    /// `#[bridge(no_eq)]`: opt OUT of generated value equality (see
    /// [`StructDecl::no_eq`]). Affects only the sealed-class emission; a
    /// unit-only enum emits a Dart `enum`, which has no generated equality to
    /// suppress. Dart-surface only, so `#[serde(skip)]`.
    #[serde(skip, default)]
    pub no_eq: bool,
    /// `#[bridge(dart_identifier = "…")]` — see [`StructDecl::dart_identifier`].
    #[serde(skip, default)]
    pub dart_identifier: Option<String>,
    /// See [`StructDecl::docs`].
    #[serde(skip, default)]
    pub docs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Variant {
    pub name: String,
    /// `#[bridge(dart_identifier = "…")]`: the Dart name this lands under,
    /// replacing the one derived from the Rust name.
    ///
    /// The derivation is not injective — `to_lower_camel_case` collapses
    /// `word_count` and `wordCount`, and a compound like `{Enum}{Variant}`
    /// loses the boundary between its halves — so two distinct Rust items can
    /// want one Dart name. Which of them should move is information only the
    /// author has, so the collision is an error (FR0002) and this carries the
    /// decision past it.
    ///
    /// Dart-surface only: the wire keys on `fn_id` and field order, never on a
    /// name, so `#[serde(skip)]` keeps it out of the schema fingerprint.
    #[serde(skip, default)]
    pub dart_identifier: Option<String>,

    /// Empty for unit variants. Tuple variants get synthesized names
    /// `field0`, `field1`, ...; use [`Variant::tuple`] — never the field name —
    /// to tell tuple from named, so a named field literally called `field0` is
    /// not misread as positional.
    pub fields: Vec<Field>,
    /// True when the variant was declared with unnamed (tuple) fields. The
    /// single source of truth for tuple-vs-named, set once at parse time from
    /// the actual syntax; both emitters read it instead of sniffing synthesized
    /// field names. Not wire-relevant — tuple and named variants encode
    /// identically (positionally, in declaration order) — so it is
    /// `#[serde(skip)]` and stays out of the schema fingerprint.
    #[serde(skip)]
    pub tuple: bool,
    /// The variant's Rust discriminant, on an enum where the author wrote at
    /// least one (`enum Level { Low = 1, Mid, High = 9 }`). `None` on every
    /// variant of an enum that writes none.
    ///
    /// A number an author writes on an enum usually **is** the contract — a
    /// wire code, a C ABI value, a database column — so it crosses, as
    /// `int get discriminant` on the generated Dart enum. What does not change
    /// is the frustrate wire, which stays the variant's **position**: the
    /// discriminant is a value the enum carries, not the tag it travels under.
    ///
    /// Read from the source, never evaluated: only an integer literal (with an
    /// optional `-`) is a discriminant here, and anything else is FR0040.
    /// A variant that writes none takes Rust's own rule, one more than the
    /// previous — a property of the declaration rather than an expression to
    /// evaluate.
    ///
    /// Dart-surface only: the wire is positional either way, so `#[serde(skip)]`
    /// keeps it out of the schema fingerprint like [`Variant::tuple`].
    #[serde(skip, default)]
    pub discriminant: Option<i64>,
    /// See [`StructDecl::docs`].
    #[serde(skip, default)]
    pub docs: Vec<String>,
}

/// Concurrency model of an opaque type. See docs/CHARTER.md.
///
/// Confined, Frozen and Locked each fix a thread bound on the Rust type,
/// enforced by the handle constructor they use and restated per opaque by
/// [`crate::emit_rust`]. Actor and Resident carry none: both are thread-affine
/// end to end, by different means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Model {
    /// Single owner, one isolate. Sync methods only; they run on the caller.
    /// Requires `Send` — a dispatched constructor builds the value on a pool
    /// worker, and the calling isolate then uses it.
    Confined,
    /// Confined's thread bound removed, by removing the two mechanisms that
    /// needed it: the object is born on the caller (constructors are sync, and
    /// a dispatched or `async fn` one is refused) and it is freed on the
    /// caller (a `dart:core` `Finalizer`, never a `NativeFinalizer`). `Box<T>`
    /// with `'static` only. The price is that an isolate that exits leaks its
    /// live residents — nothing else may run their `Drop` — which the runtime
    /// reports by type name.
    Resident,
    /// Immutable after construction (`Arc<T>`). Sync and async methods.
    /// Requires `Send + Sync`.
    Frozen,
    /// Shared mutable (`Arc<RwLock<T>>`). Async by default; sync is a
    /// contract-marked opt-in via `on_contention`. Requires `Send + Sync`.
    Locked,
    /// Message passing; async-only by construction. One instance = one
    /// dedicated executor (a thread on native, a Worker-hosted wasm
    /// instance on web). Load-bearing on web: wasm handles are
    /// instance-local and a web worker is a separate instance, so Actor
    /// is the only web parallelism story. Long CPU work belongs here.
    Actor,
}

/// A representation named by a marker-type wrapper at a **use site** —
/// `Locked<Point>` in a signature, or `impl Locked<Point>`'s self type — one
/// of the six zero-cost aliases in `runtime/rust/src/lib.rs`
/// (`Data`/`Confined`/`Frozen`/`Locked`/`Actor`). Parsers emit it
/// (`Type::Claimed`, `Function::parent_claim`); the checker compares it
/// against what the named type actually declared and reports a mismatch
/// (FR0062), then erases it — `Type::Claimed` never survives past
/// `check::resolve_type`, so emitters and the wire fingerprint never see it.
/// Optional; checked against the declaration (FR0062).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Claim {
    /// `Data<T>`: `T` must be declared `#[bridge(data)]` (a struct or enum).
    Data,
    /// `Confined<T>` / `Frozen<T>` / `Locked<T>` / `Actor<T>`: `T` must be a
    /// handle declared under the named model.
    Model(Model),
}

/// Which half of a declared type a member is generated onto — the value class
/// built from its fields, or the handle class over the live Rust object.
///
/// Two-valued rather than a [`Claim`], although a claim is what the author
/// writes, because the handle's concurrency model already lives on its
/// [`OpaqueDecl`]. A `Claim::Model(m)` here could only restate that model or
/// contradict it, and a contradiction is not a state this should be able to
/// hold: `check` compares the written claim against the declaration (FR0062)
/// and records the *half* it selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Repr {
    /// The value class: the member's receiver is decoded out of the request
    /// like a parameter, and dies with the call.
    Data,
    /// The handle class: the member's receiver is a live Rust object reached
    /// through a handle id, under the [`OpaqueDecl`]'s model.
    Handle,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpaqueDecl {
    pub name: String,
    pub module_path: String,
    pub model: Model,
    /// True when the declaration is a trait, not a struct: values cross as
    /// `Box<dyn Name>` behind the same thin-pointer handles (the Box is the
    /// Sized type the handle registry stores). The
    /// Dart surface is identical to a concrete opaque's.
    #[serde(default)]
    pub dyn_trait: bool,
    /// Supertrait bound names as written (`trait Store: Send + Sync` →
    /// ["Send", "Sync"]). Only meaningful for traits; the checker requires
    /// Send + Sync on Frozen/Locked traits (type erasure hides the bounds
    /// monomorphization would otherwise prove).
    #[serde(default)]
    pub supertraits: Vec<String>,
    /// A handle type declared with `native_only` alongside its concurrency
    /// model (e.g. `#[bridge(confined, native_only)]`): this whole type is
    /// absent from the web surface — no class, no members, no drop export —
    /// because its implementation does not build for wasm32.
    ///
    /// Type level rather than member level because it also *propagates*: any
    /// bridged function that names this type anywhere in its signature becomes
    /// native-only too (see `check`). Without that, a free function taking
    /// `&NativeOnlyType` would still be emitted portable, and its glue would
    /// name a Rust item the wasm build does not have — the same `E0425` inside
    /// generated code that the declaration exists to prevent.
    #[serde(default)]
    pub native_only: bool,
    /// See [`Function::cfg_gated`]. A cfg-gated opaque is the same hazard one
    /// level up: its drop/finalize exports name `crate::…::T` unconditionally.
    #[serde(skip, default)]
    pub cfg_gated: bool,
    /// `#[bridge(dart_identifier = "…")]` — see [`StructDecl::dart_identifier`].
    #[serde(skip, default)]
    pub dart_identifier: Option<String>,
    /// See [`StructDecl::docs`].
    #[serde(skip, default)]
    pub docs: Vec<String>,
}

/// A bridge-external type: a value type whose wire form is a byte payload
/// produced by user codecs — `frustrate::BytesCodec` on the Rust side, the
/// declared Dart methods on the other. Built for the protobuf pattern
/// (both sides already own generated codecs); usable anywhere a value type
/// is (params, returns, fields, collections, stream items, callback args).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExternDecl {
    pub name: String,
    pub module_path: String,
    /// The Dart-side type name (e.g. the protoc-generated class).
    pub dart_type: String,
    /// Import URI that provides [`Self::dart_type`], emitted into the
    /// generated surfaces.
    pub dart_import: String,
    /// Instance method on the Dart type returning the bytes
    /// (protobuf default: `writeToBuffer`).
    pub dart_encode: String,
    /// Static/factory expression taking the bytes
    /// (protobuf default: `<dart_type>.fromBuffer`).
    pub dart_decode: String,
}

/// Which concrete Rust sequence container backs a [`Type::List`]. Both map to
/// Dart `List` and share the list wire codec; only the decode reconstruct
/// target differs (`Vec` vs `VecDeque`). Not wire-relevant — see [`Type::List`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeqKind {
    #[default]
    Vec,
    VecDeque,
}

/// Whether a [`Type::Set`]/[`Type::Map`] is hashed (`HashSet`/`HashMap`) or
/// B-tree ordered (`BTreeSet`/`BTreeMap`). Both map to the same Dart
/// `Set`/`Map` and share the wire codec; only the decode reconstruct target
/// differs. A BTree encodes in its natural sorted iteration order, which
/// Dart's insertion-ordered `Set`/`Map` then preserves. Not wire-relevant in
/// the fingerprint sense — see [`Type::List`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MapKind {
    #[default]
    Hash,
    BTree,
}

/// Which Rust type backs a [`Type::Duration`] — the *span* peer the author
/// declared in the signature.
///
/// One Dart type (`Duration`) and one wire form (i64 µs) for all three; the
/// peer only says which Rust value the decode reconstructs and which crate's
/// accessor the encode calls. Selected by the written path, never by an
/// attribute: the generated glue calls the user's own `fn`, so the emitted
/// peer *has* to be the one in the signature — an attribute could only agree
/// with it or contradict it.
///
/// The signed/unsigned split is the reason this is not cosmetic. Dart's
/// `Duration` is signed and `std::time::Duration` is not, so a negative value
/// is a contract violation for [`DurationPeer::Std`] (rejected loudly on both
/// sides) and ordinary data for the other two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DurationPeer {
    /// `std::time::Duration` (`core::time::Duration`). The default, and the
    /// only **unsigned** peer. Its representable range is also the narrowest
    /// on wasm32, where std's `SystemTime` is an unsigned offset from the
    /// epoch and cannot hold a pre-epoch instant at all.
    #[default]
    Std,
    /// `chrono::TimeDelta`, which `chrono::Duration` is an alias for. Signed.
    ChronoTimeDelta,
    /// `time::Duration` (a `time::SignedDuration` alias since 0.3.5x). Signed.
    Time,
}

/// Which Rust type backs a [`Type::SystemTime`] — the *instant* peer the
/// author declared. See [`DurationPeer`]; same rules, same wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstantPeer {
    /// `std::time::SystemTime`. The default.
    #[default]
    Std,
    /// `chrono::DateTime<chrono::Utc>`.
    ChronoUtc,
    /// `time::OffsetDateTime`.
    TimeOffsetDateTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Exec {
    /// Runs on the calling thread; the Dart call returns a plain value.
    Sync,
    /// Runs on the runtime thread pool; the Dart call returns a Future.
    Async,
}

/// Contract-marked behavior for sync access to a Locked object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnContention {
    /// try-lock; throws ContentionError naming the object and method.
    Error,
    /// Blocking lock. Only legal where the target allows blocking (native).
    Block,
}

/// How a method takes `self`. Read from the signature at parse time; whether
/// a shape may cross is the checker's rule (FR0057).
///
/// The three accepted forms are wire-relevant and serialized: a consuming
/// receiver's request is a handle the call *takes*, and a binding compiled
/// against the borrowing spelling would hand out the same bytes for a call
/// that no longer gives the object back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Receiver {
    /// `&self`.
    Ref,
    /// `&mut self`.
    RefMut,
    /// `self` or `mut self`.
    ///
    /// On a **handle** the call consumes the object and the Dart side gives it
    /// up through `take()`. On a **data** type it moves the local the glue
    /// decoded out of the request — the implicit clone every by-value data
    /// parameter already performs — so the Dart value the caller holds is
    /// untouched and the member is an ordinary one on its class.
    Value,
    /// `self: Box<Self>`. Consumes on a handle, like [`Receiver::Value`], and
    /// differs only in what the glue hands the body: the registry's `Box`
    /// itself rather than the value inside it. On a data type it is
    /// `Box::new` of the decoded local, the same transparency [`Type::Boxed`]
    /// already gives a `Box` around a value type.
    ///
    /// It therefore carries one handle on the wire exactly as `self` does, and
    /// separating the two here makes the schema fingerprint **stricter than
    /// the wire requires**: swapping one spelling for the other re-fingerprints
    /// an interface that would have interoperated. Deliberate, and the cheaper
    /// of the two mistakes available — the alternative is a second
    /// `#[serde(skip)]` field beside this one, splitting one fact about the
    /// syntax across two places that then have to be kept agreeing. The cost
    /// is one rebuild after an edit nobody makes twice.
    Boxed,
    /// Any other explicitly typed receiver — `self: &Self`, `self: Rc<Self>`,
    /// `self: Pin<&mut Self>`. The parser does not read the type beyond
    /// recognising `Box<Self>`, so these are refused rather than
    /// half-understood (FR0057). Nothing constructs a `Function` with this
    /// past the checker, so it has no wire form and no emitter arm.
    Typed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Function {
    /// Dispatch id shared between generated Rust and Dart. Assigned by
    /// `check::finalize`; 0 until then.
    pub fn_id: u32,
    pub name: String,
    /// Rust module path through which the function is reachable from the
    /// bridge crate root, e.g. "crate::api".
    pub module_path: String,
    /// `Some(type name)` when this is a member of a declared type;
    /// [`Function::parent_repr`] says which of its representations.
    pub parent: Option<String>,
    /// Which of the parent's representations this member is generated onto.
    ///
    /// `Some` exactly when the parent declares two (see [`Repr`]); `None`
    /// means "the parent's only representation", so an interface with no such
    /// type serializes byte-identically to before this field existed and no
    /// existing schema fingerprint moves (the
    /// [`Function::err`]/[`Function::deferred`] precedent).
    ///
    /// Wire-relevant, so serialized rather than skipped: at one `fn_id` a data
    /// receiver decodes the value out of the request and a handle receiver
    /// reads a handle id, and two halves that disagree about which must not
    /// pass the fingerprint check.
    ///
    /// Set by `check` from the declaration and the `impl` block's claim, never
    /// from whether a marker was written — so `impl Locked<Doc>` and
    /// `impl Doc` on a single-representation `Doc` still produce equal
    /// `Function`s, exactly as [`Function::parent_claim`] guarantees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_repr: Option<Repr>,
    /// The type arguments written on the `impl` block's self type
    /// (`impl Page<Item>` → `[Item]`, `impl<T> Page<T>` → `[Param("T")]`),
    /// empty for the bare spelling every non-generic block writes.
    ///
    /// Set by `parse`, consumed by `check::expand_generics`, and empty on
    /// everything it hands downstream: an expanded member's `parent` is the
    /// synthetic declaration's own name (`Page<Item>`), which already carries
    /// the arguments as an [`Instance`]. So `#[serde(skip)]` — nothing that
    /// reaches the wire ever sees it.
    #[serde(skip, default)]
    pub parent_args: Vec<Type>,
    /// The `impl` block's own type parameter names (`impl<T> Page<T>` →
    /// `["T"]`), which [`Function::parent_args`] and the member's signature
    /// are written in terms of. Empty for a concrete block.
    ///
    /// Distinct from [`Function::generics`], the *function's* own parameter
    /// list, which stays refused (FR0056): a block parameter is bound by the
    /// instantiation the member is expanded onto, and a function parameter is
    /// bound by nothing a Dart call could supply.
    ///
    /// `#[serde(skip)]` for [`Function::parent_args`]'s reason.
    #[serde(skip, default)]
    pub parent_generics: Vec<String>,
    /// `Some(claim)` when the `#[bridge] impl` block's self type was written
    /// through a representation-marker wrapper (`impl Locked<Point>`) rather
    /// than the bare type name. `parent` above is already unwrapped to
    /// `"Point"` by the time this is set — parsing has to recognise the
    /// wrapper here because the self type is stored as a bare name, not a
    /// [`Type`], so it never passes through `check::resolve_type`.
    /// `check_function` compares it against `parent`'s actual declared
    /// representation (FR0062) the same way `resolve_type` checks
    /// [`Type::Claimed`]. Never wire-relevant — a matching wrapper produces
    /// the identical `Function` a bare self type would — so `#[serde(skip)]`.
    #[serde(skip, default)]
    pub parent_claim: Option<Claim>,
    /// `Some(full trait path)` when this method comes from a bridged
    /// `impl Trait for Type` block: the
    /// generated invocation is fully-qualified UFCS
    /// (`<Type as Trait>::method`), because the generated module cannot
    /// assume the trait is in scope. Static dispatch — the parent is the
    /// concrete type; the trait need not be bridged (foreign traits work).
    #[serde(default)]
    pub trait_impl: Option<String>,
    pub receiver: Option<Receiver>,
    /// See [`StructDecl::generics`] — the fn's own type and const parameters
    /// (`fn f<T>(x: T)` → `["T"]`), refused by FR0056.
    #[serde(skip, default)]
    pub generics: Vec<String>,
    pub params: Vec<Param>,
    /// None for `()`.
    pub ret: Option<Type>,
    /// True when the declared return type is `Result<T, _>` — of either kind.
    /// See [`Function::err`] for which.
    pub fallible: bool,
    /// The bridged error type, when the member declares one:
    /// `Result<T, E>` where `E` names a bridged struct or enum. `None` covers
    /// both the infallible case and the *untyped* fallible one
    /// (`anyhow::Result<T>`, `Result<T, String>`), where the error crosses as
    /// its `Display` message.
    ///
    /// Wire-relevant, so it is **not** `#[serde(skip)]` — a member that starts
    /// returning a typed error must break the schema fingerprint, because a
    /// binding compiled before the change would meet a status byte it has no
    /// decoder for. `skip_serializing_if` keeps the IR JSON of every interface
    /// that uses no typed error byte-identical, so existing fingerprints do
    /// not move.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub err: Option<Type>,
    pub exec: Exec,
    pub on_contention: Option<OnContention>,
    /// True for associated functions that construct the parent opaque
    /// (return type is the parent by value).
    pub is_constructor: bool,
    /// True for the synthetic per-actor drop function appended by
    /// `check::finalize`. Actor objects must drop on their own executor
    /// (after queued calls drain), so disposal is a dispatched call rather
    /// than a `frustrate_drop_*` export.
    #[serde(default)]
    pub is_actor_drop: bool,
    /// Derived by `check` from three sources: an `on_contention = "block"`
    /// contract (the calling thread may be the browser main thread, where
    /// waiting is fatal on both wasm builds — a trap with `+atomics`, an
    /// unattributable spin without); a value-returning `DartFunction` parameter on
    /// a non-`async fn` member (invoking it blocks a pool worker against the
    /// web event loop); or the author's own [`Function::native_only`]
    /// declaration. Native-only members are omitted from the generated web
    /// Dart surface and cfg-gated out of the wasm dispatch.
    ///
    /// A Rust `async fn` is *not* a source: its body is driven by the
    /// cooperative executor, which is portable (see `rust_async`).
    #[serde(default)]
    pub requires_native: bool,
    /// `#[bridge(native_only)]`: the declaration that this member cannot exist
    /// on web — overwhelmingly because its implementation, or a crate it calls,
    /// does not build for wasm32. This is an *input* to
    /// [`Function::requires_native`], not a synonym: the derived flag is the
    /// union of this and the two structural sources above.
    ///
    /// `check` writes it back, so after checking it is true either because the
    /// author wrote it on this member (or the `impl`/`trait` around it) or
    /// because the member names a native-only [`OpaqueDecl`]. Both are the same
    /// fact — this code is not built for the target — which is why they share a
    /// flag and a diagnostic.
    ///
    /// It exists because those structural sources are the two rarest reasons a
    /// member is native-only, and the commonest reason — a dependency that
    /// does not compile for the target — had no way to be said at all.
    #[serde(default)]
    pub native_only: bool,
    /// `#[bridge(no_block)]` (or a file-wide `frustrate::bridge_file!(no_block)`):
    /// the author's claim that this body, and everything it transitively
    /// reaches, never waits on a synchronization primitive
    /// primitive. FR0048/FR0049 reject the contradictions
    /// codegen can see from a signature; `bazel/wasm_block_check` settles what
    /// the body actually calls, at link time.
    ///
    /// Changes no emitted byte on any shipped target: the check roots it
    /// produces are `#[cfg(frustrate_block_check)]`, a cfg no production build
    /// sets. So like [`Function::cfg_gated`] it stays out of the schema
    /// fingerprint — two halves that disagree about a claim still interoperate.
    #[serde(skip, default)]
    pub no_block: bool,
    /// True when the bridged item (or the `impl`/`trait` block it came from)
    /// carries a `#[cfg(...)]`/`#[cfg_attr(...)]` attribute. Codegen cannot
    /// evaluate cfg predicates, so it always emits the member into both
    /// surfaces; if the gate excludes the target being built, the generated
    /// glue names an item that is not there. FR0034 rejects the combination
    /// unless the member also declares `native_only`.
    ///
    /// Diagnostic-only — it changes no emitted byte, so like
    /// [`Function::web_runtime_fail`] it stays out of the schema fingerprint.
    #[serde(skip, default)]
    pub cfg_gated: bool,
    /// `#[bridge(web = "runtime_fail")]`: the informed opt-in that INCLUDES an
    /// otherwise-native-only member (`requires_native`) in the web Dart surface
    /// so portable app code naming it still compiles for web — but its web body
    /// throws a loud, attributable `UnsupportedError` (naming the member and
    /// why) on the Dart side, before any attempt to cross (the Rust wasm
    /// dispatch arm stays `cfg`'d out and absent). Meaningless — and a loud
    /// codegen error (FR0030) — on a member that is not `requires_native`.
    /// Dart-surface only: the wire and the generated Rust are byte-identical
    /// whether or not it is set (the throw never reaches the boundary), so
    /// `#[serde(skip)]` keeps it out of the schema fingerprint, like
    /// [`StructDecl::docs`].
    #[serde(skip, default)]
    pub web_runtime_fail: bool,
    /// True when the user wrote a Rust `async fn`: the body evaluates to a
    /// `Future`, which the generated glue hands to the cooperative executor
    /// (`frustrate::executor::spawn`). That
    /// executor runs on every config, so an async-bodied fn is portable and
    /// never `requires_native`. Never `#[bridge(sync)]`: a sync member runs on
    /// the caller and returns a plain value, so it cannot await. Orthogonal to
    /// `exec`, which is the Dart-side calling convention (async fns are always
    /// `Exec::Async`).
    #[serde(default)]
    pub rust_async: bool,
    /// True when the declared return type is `Deferred<T>` — the actor
    /// opt-out of serialized completion.
    /// The method's body runs on the actor's executor only for its
    /// synchronous prefix; the wrapped future completes the call later on the
    /// cooperative executor, releasing the instance across its awaits.
    /// [`Function::ret`]/[`Function::err`] hold the *unwrapped* inner type,
    /// so every value-type rule sees through the wrapper. Actor methods only
    /// (FR0038; constructors FR0039).
    ///
    /// Wire-relevant like [`Function::err`] — the Dart host must flag the
    /// call for dispose-cancellation, so a binding compiled before a member
    /// went deferred must not pass the fingerprint check — and
    /// `skip_serializing_if` for the same reason `err` has it: interfaces
    /// with no deferred member keep byte-identical IR JSON, so existing
    /// fingerprints do not move.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deferred: bool,
    /// The return type was written as a **borrow** (`-> &Point`). The value is
    /// copied into the response either way, so the wire is identical and this
    /// is `#[serde(skip)]`: only the generated Rust differs, reading through
    /// the reference instead of consuming an owned value.
    #[serde(skip, default)]
    pub ret_borrow: bool,
    /// `#[bridge(getter)]`: this member is a Dart **property**, not a method —
    /// `d.name` rather than `d.name()`.
    ///
    /// Dart-surface only: the same call, the same wire, the same dispatch id;
    /// only the header the caller writes against differs. So `#[serde(skip)]`,
    /// like [`StructDecl::no_eq`].
    ///
    /// It is a *spelling*, not a capability — reading a value out of a handle
    /// has always been a one-line method under each model's own rules, and
    /// those rules still decide everything: a getter on a `locked` type is
    /// sync only with a contention contract, async on an actor, and so on.
    /// What the flag cannot express is a parameter (FR0066), a value to
    /// discard, or a cancel token, which has nowhere to go on a property.
    #[serde(skip, default)]
    pub getter: bool,
    /// `#[bridge(dart_identifier = "…")]` — see [`StructDecl::dart_identifier`].
    #[serde(skip, default)]
    pub dart_identifier: Option<String>,
    /// See [`StructDecl::docs`]. A trait method's lines ride along to the
    /// implementors `check` synthesizes from it, so a concrete class carries
    /// the documentation its interface declares.
    #[serde(skip, default)]
    pub docs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Param {
    pub name: String,
    pub ty: Type,
    pub borrow: Borrow,
    /// True when a borrowed param was spelled as an **unsized** borrow —
    /// `&[u8]`, `&str` — rather than as a borrow of an owned container
    /// (`&Vec<u8>`, `&String`). Both spellings parse to the same [`Type`], and
    /// the type alone therefore cannot tell them apart — but only the unsized
    /// form can be satisfied by a decode that borrows the request buffer, since
    /// `&[u8]` is what such a decode can produce and `&Vec<u8>` needs a real
    /// `Vec` to point at. Meaningless (and false) for by-value params and for
    /// opaque handles, which carry their own borrow rules.
    #[serde(default)]
    pub unsized_borrow: bool,
    /// The lifetime the source named on a borrowed param (`&'a [u8]`), if any.
    ///
    /// Carried only so the checker can reject it *by name*. The parser has
    /// always dropped lifetimes silently, which was harmless while every
    /// borrowed value type was decoded into an owned local — and is not
    /// harmless now that a sync borrow reaches the request buffer, because
    /// `request_slice` hands the dispatch a `&'a [u8]` with an unbound `'a`
    /// and a user-written `'static` would unify with it.
    #[serde(default)]
    pub ref_lifetime: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Borrow {
    Value,
    Ref,
    RefMut,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Type {
    Bool,
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    /// u64 crosses as Dart BigInt — the full unsigned range does not fit
    /// Dart's 64-bit signed int.
    U64,
    /// i128/u128 cross as Dart BigInt on the *same* big-integer codec as u64,
    /// extended to 16 bytes (two u64 halves): one big-integer story, not two.
    /// Signed for I128, unsigned for U128.
    I128,
    U128,
    F32,
    F64,
    /// usize/isize cross as 64-bit; the Dart side uses int.
    Usize,
    Isize,
    String,
    /// Rust `char` (a Unicode scalar value) ⇄ Dart one-character `String`.
    /// Crosses as a `u32` codepoint on the existing u32 wire primitive; both
    /// sides reject a non-scalar (empty/multi-char/lone-surrogate/out-of-range)
    /// loudly rather than truncating.
    Char,
    /// A **span** ⇄ Dart `Duration`. Crosses as i64 microseconds. The
    /// variant is named for its default
    /// peer, `std::time::Duration`; [`DurationPeer`] says which Rust type the
    /// author actually declared.
    ///
    /// The peer is **serialized**, unlike [`SeqKind`], and so is part of the
    /// schema fingerprint (`hash.rs`). `Vec` and `VecDeque` are skipped there
    /// because they accept the *identical* set of values; time peers do not.
    /// `std::time::Duration` is unsigned and the other two are signed, and
    /// each peer's representable range differs — so the generated Dart differs
    /// too (only the unsigned peer gets the negative-span check). Two halves
    /// that disagree about which values are legal are exactly what the
    /// fingerprint exists to refuse at init, rather than discovering it later
    /// from whichever value happens to cross first.
    ///
    /// Serde emits the default peer as `{"duration":"std"}` rather than the
    /// old `"duration"`, so a time-using interface re-fingerprints once. That
    /// is correct rather than merely tolerable: this change also rewrote the
    /// generated code on both sides of every such interface, so a binding from
    /// before it genuinely does not match. Interfaces with no time type are
    /// byte-identical and do not move.
    Duration(DurationPeer),
    /// An **instant** ⇄ Dart `DateTime` (UTC). Crosses as i64 microseconds
    /// since the Unix epoch. Named for its default peer,
    /// `std::time::SystemTime`; see [`Type::Duration`] on the peer.
    SystemTime(InstantPeer),
    /// Vec<u8>: fast path, crosses as Uint8List.
    Bytes,
    /// `[u8; N]`: fixed-length byte array (e.g. change hashes).
    ///
    /// The same wire as [`Type::Array`] with a `U8` element — N raw bytes, no
    /// length prefix — and kept as its own variant only so the schema
    /// fingerprint of every interface that already crosses a `[u8; N]` stays
    /// where it is. `[u8; N]` also keeps the memcpy codec
    /// (`write_byte_array`/`read_byte_array`) rather than the element loop.
    ByteArray(usize),
    /// `[T; N]` for any other element type.
    ///
    /// **N raw elements, no length prefix**: the length is in the type, so a
    /// prefix would carry no information the far side does not already have,
    /// and `[u8; N]` had already settled that a fixed array writes its payload
    /// bare. A wrong-length value is refused on the Dart encode, before
    /// anything crosses; Rust cannot produce one, because the array's length is
    /// its type.
    ///
    /// **Serialized**, unlike [`SeqKind`]: N decides how many elements the far
    /// side reads, so two halves that disagree about it do not speak the same
    /// wire. No existing interface names one, so no existing fingerprint moves.
    Array(Box<Type>, usize),
    /// `Vec<T>` or `VecDeque<T>`: both cross on the list wire codec (length +
    /// per-element) and map to Dart `List<T>`. `kind` records which concrete
    /// Rust container to reconstruct on decode. It is not wire-relevant — both
    /// containers encode identically — so it is `#[serde(skip)]`, kept out of
    /// the schema fingerprint exactly like [`Variant::tuple`].
    List(Box<Type>, #[serde(skip)] SeqKind),
    /// `HashSet<T>` or `BTreeSet<T>`: cross on the list wire codec (length +
    /// elements) and map to Dart `Set<T>`. See [`Type::List`] on `kind`.
    Set(Box<Type>, #[serde(skip)] MapKind),
    /// `HashMap<K,V>` or `BTreeMap<K,V>` → Dart `Map<K,V>`. See [`Type::List`]
    /// on `kind`.
    Map(Box<Type>, Box<Type>, #[serde(skip)] MapKind),
    Option(Box<Type>),
    /// A tuple `(A, B, …)` (arity ≥ 2) → a Dart positional record
    /// `(TA, TB, …)`, accessed `$1`/`$2`/…. A struct-shaped positional value:
    /// each element encodes exactly like a struct field, in declaration order,
    /// on the existing per-element codec — no new wire primitive. `()` is not
    /// a tuple here; the unit return is `Function::ret == None`.
    Tuple(Vec<Type>),
    /// Reference to a declared type; resolved by `check` into one of the
    /// kinds below. Parsers emit `Named`; emitters must never see it.
    Named(String),
    /// A **fully-applied generic data type** written at a use site —
    /// `Page<Item>`, `Either<i64, Point>`. Parsers emit it for any path with
    /// type arguments whose head is none of the spellings `parse_path_type`
    /// claims; `check`'s expansion pass replaces it with a `Named` naming the
    /// synthetic declaration it instantiated, so emitters never see it.
    ///
    /// The arguments are ordinary types, so an argument may itself be an
    /// application (`Page<Either<i64, Point>>`) — the expansion is a fixpoint
    /// and works inside out.
    App(String, Vec<Type>),
    /// A **type parameter** of the enclosing generic data template — the `T`
    /// in `struct Page<T> { items: Vec<T> }`.
    ///
    /// Lives only inside [`Interface::generic_structs`] /
    /// [`Interface::generic_enums`]: `check`'s expansion substitutes every one
    /// away when it instantiates a template, so no declaration in `structs` or
    /// `enums` — and therefore nothing on the wire — contains one. What still
    /// reads it is the **Dart class**, which is emitted once from the template
    /// and is generic: `dart_type` renders this as the Dart type-parameter
    /// name, so `Vec<T>` declares `List<T>`.
    Param(String),
    /// A representation-marker wrapper around a use-site type reference —
    /// `Locked<Point>` written where a bridged type name may appear. Parsers
    /// emit it; `check::resolve_type` verifies the [`Claim`] against the
    /// inner type's actual declared representation (FR0062 on a mismatch)
    /// and replaces the whole node with the resolved inner type, so this
    /// variant never survives to an emitter or to the hashed IR — a bare
    /// `Point` and a matching `Locked<Point>` produce byte-identical output.
    /// A user's own type literally named `Data`/`Confined`/… with **no**
    /// generic argument is untouched; only the one-argument generic form is
    /// reserved (the same rule that already reserves `Vec`/`Box`/`Option`).
    Claimed(Claim, Box<Type>),
    Struct(String),
    Enum(String),
    Opaque(String),
    /// A bridge-external type (see [`ExternDecl`]). A value type on the
    /// wire (a byte payload), so it goes anywhere data goes.
    Extern(String),
    /// A handle to a Dart object whose methods Rust can call — the dual of
    /// [`Type::Opaque`].
    ///
    /// Data on the wire: the Dart-minted id is encoded in position, so a
    /// handle composes exactly like any other type — nested in a struct, in
    /// a `Vec`, several per function, beside a real return value. That is
    /// the whole reason endpoints are modelled this way; composition falls
    /// out of the ordinary recursive codecs instead of being special-cased.
    ///
    /// Argument-only (FR0031): Rust cannot mint a Dart object.
    DartObject(Box<DartObjectSpec>),
    /// A reference **inside** a type: `Vec<&Doc>`, `Option<&str>`,
    /// `(&Doc, i64)`, `&[&Doc]`. The top-level spelling of a parameter stays
    /// [`Param::borrow`]; this variant appears only nested.
    ///
    /// **Serialized, and that is the whole reason it is a variant rather than a
    /// flag somewhere.** `Vec<&Doc>` and `Vec<Doc>` write byte-identical
    /// requests — a length and a handle id per element — but one lends the
    /// objects and the other takes them. Two halves that disagree would leave
    /// Dart spending tokens Rust never took (a leak) or holding handles Rust
    /// freed (a use-after-free), and nothing on the wire could notice. So the
    /// fingerprint has to.
    ///
    /// `mutable` is **not** serialized. `&Doc` and `&mut Doc` are the same
    /// bytes and the same Dart type, and each half's glue picks its own
    /// accessor from its own copy of the signature — there is no cross-half
    /// agreement to enforce, and unlike [`Receiver::Boxed`] there is no second
    /// field this would have to be kept consistent with. `unsized_borrow` is
    /// skipped for the reason [`Param::unsized_borrow`] gives: `&str` and
    /// `&String` are one wire and one Dart type, and only the Rust glue differs.
    Ref {
        inner: Box<Type>,
        /// `&mut T` rather than `&T`.
        #[serde(skip, default)]
        mutable: bool,
        /// Spelled as an **unsized** borrow — `&str`, `&[u8]` — rather than as
        /// a borrow of an owned container (`&String`, `&Vec<u8>`). See
        /// [`Param::unsized_borrow`], which records the same fact at the top
        /// level.
        #[serde(skip, default)]
        unsized_borrow: bool,
    },
    /// `Box<T>` around a value type — the indirection a recursive data type
    /// needs (`struct Node { next: Option<Box<Self>> }`). `Box<dyn Trait>` is
    /// not this: a trait object crosses as a handle and resolves to
    /// [`Type::Opaque`] at the name, with the `Box` being how ownership of an
    /// unsized value is spelled rather than a node in the type.
    ///
    /// **Transparent on the wire and to Dart.** The bytes are the inner type's,
    /// and so is `dart_type` — a `Box` is a Rust-side indirection with no Dart
    /// counterpart. Only the generated Rust differs: `rust_type` spells the
    /// `Box`, and the decode reconstructs one.
    ///
    /// `#[serde(untagged)]` serializes it as the bare inner type, so the schema
    /// fingerprint of `Box<T>` and `T` are the same number — the same rule
    /// [`SeqKind`] is skipped under, and for the same reason: identical value
    /// set, identical bytes, so two halves that disagree about the `Box` still
    /// interoperate and must not be refused at init. `skip_deserializing`
    /// because an untagged variant that wraps *every* other variant would
    /// otherwise be tried on any input the tagged arms reject, which for a
    /// recursive type does not terminate.
    ///
    /// Nothing needs the `Box` back. The IR JSON has one reader —
    /// `tests/test_api/src/hostile.rs`, which deserializes an `Interface` to
    /// build wire bytes for the generated dispatch — and it reasons about the
    /// wire, which is exactly the erased projection this produces. A
    /// `Box<T>` read back as `T` describes the same bytes it was written from.
    ///
    /// Last in the enum because serde requires untagged variants there.
    #[serde(untagged, skip_deserializing)]
    Boxed(Box<Type>),
}

/// Which Dart type a [`Type::DartObject`] mirrors. Fixes the method set, the
/// Dart-side parameter type, and the binding rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DartMirror {
    /// `dart:core` `Sink<T>` — `add`, `close`. Write-end only, so no
    /// cancellation back-channel.
    Sink,
    /// `dart:async` `EventSink<T>` — `Sink` plus `addError`. Still write-end
    /// only.
    EventSink,
    /// `dart:async` `StreamController<T>` — the full producer end, including
    /// the `onCancel` back-channel the generated Dart binds. The only mirror
    /// whose consumer can stop the Rust producer, and what the legacy
    /// `StreamSink<T>` names.
    StreamController,
    /// A Dart closure `void Function(T)` — what `DartCallback<T>` names.
    Callback,
    /// A Dart closure `R Function(T)` — what `DartFunction<T, R>` names.
    Function,
}

/// The shape of one [`Type::DartObject`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DartObjectSpec {
    pub mirror: DartMirror,
    /// The item/argument type (the `T`); `None` for `DartCallback<()>` /
    /// `DartFunction<(), R>`. Value types only (checked).
    pub item: Option<Type>,
    /// `Some` for a value-returning method (`DartFunction`'s `R`). Its
    /// presence is what can make a member native-only.
    pub ret: Option<Type>,
    /// `Some` when the method's result is declared `Result<R, E>`: the Dart
    /// side may fail *as a value* the Rust body handles, rather than as the
    /// enclosing call's panic. `E` is a bridged struct or enum — the same
    /// typed-error vocabulary as a bridged `Result<T, E>` return, mirrored:
    /// the value form is Rust's, the exception form is Dart's.
    ///
    /// **Wire-relevant, so NOT `#[serde(skip)]`.** A fallible closure's reply
    /// can carry `STATUS_TYPED_ERROR`, which an infallible binding has no
    /// decoder for — so two interfaces differing only in a closure's
    /// fallibility must fingerprint differently. `skip_serializing_if` keeps every
    /// existing interface's IR JSON byte-identical, so no fingerprint moves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub err: Option<Type>,
}

impl DartMirror {
    /// True when the Dart side is a closure rather than an object with named
    /// methods. Only affects the generated Dart parameter type — the wire
    /// and the dispatch table are identical.
    pub fn is_closure(self) -> bool {
        matches!(self, DartMirror::Callback | DartMirror::Function)
    }

    /// True when the mirror declares `addError` (selector 1).
    pub fn has_add_error(self) -> bool {
        matches!(self, DartMirror::EventSink | DartMirror::StreamController)
    }
}

impl DartObjectSpec {
    /// True when the mirror's method has a **reply frame** — a value, a
    /// declared failure, or both. This, never `ret.is_some()`, is the question
    /// every rule about returning mirrors is asking: `DartFunction<T,
    /// Result<(), E>>` ("do this; you may refuse") returns no value yet still
    /// round-trips, so it blocks the invoking worker exactly as a
    /// value-returning one does and must be classified with it.
    pub fn is_returning(&self) -> bool {
        self.ret.is_some() || self.err.is_some()
    }
}

impl Type {
    /// Walk this type and every type it contains.
    pub fn walk<'a>(&'a self, f: &mut dyn FnMut(&'a Type)) {
        f(self);
        match self {
            Type::Claimed(_, t) | Type::Boxed(t) | Type::Ref { inner: t, .. } => t.walk(f),
            Type::List(t, _) | Type::Set(t, _) | Type::Option(t) | Type::Array(t, _) => t.walk(f),
            Type::Map(k, v, _) => {
                k.walk(f);
                v.walk(f);
            }
            Type::Tuple(ts) | Type::App(_, ts) => {
                for t in ts {
                    t.walk(f);
                }
            }
            Type::DartObject(spec) => {
                if let Some(t) = &spec.item {
                    t.walk(f);
                }
                if let Some(t) = &spec.ret {
                    t.walk(f);
                }
                if let Some(t) = &spec.err {
                    t.walk(f);
                }
            }
            _ => {}
        }
    }

    /// Strip every [`Type::Claimed`] wrapper, at every depth, replacing each
    /// with the plain type it names — the same erasure `check::resolve_type`
    /// performs, minus the FR0062 comparison against the declaration.
    ///
    /// For code that reasons about a parsed-but-not-yet-checked signature
    /// (the `#[bridge]`-omission heuristic in `parse.rs`), a wrapper is
    /// invisible unless erased first: that heuristic matches on `Named` and
    /// counts opaques by shape, and a `Locked<Doc>` it has never heard of is
    /// neither. Erasing here restores parity with the bare spelling; the
    /// wrapper is optional and checked against the declaration elsewhere
    /// (FR0062), never here, so unconditional erasure loses nothing a sound
    /// heuristic needed.
    pub fn erase_claims(&mut self) {
        match self {
            Type::Claimed(_, inner) => {
                inner.erase_claims();
                *self = (**inner).clone();
            }
            Type::List(t, _)
            | Type::Set(t, _)
            | Type::Option(t)
            | Type::Array(t, _)
            | Type::Boxed(t)
            | Type::Ref { inner: t, .. } => t.erase_claims(),
            Type::Map(k, v, _) => {
                k.erase_claims();
                v.erase_claims();
            }
            Type::Tuple(ts) | Type::App(_, ts) => {
                for t in ts {
                    t.erase_claims();
                }
            }
            Type::DartObject(spec) => {
                if let Some(t) = &mut spec.item {
                    t.erase_claims();
                }
                if let Some(t) = &mut spec.ret {
                    t.erase_claims();
                }
                if let Some(t) = &mut spec.err {
                    t.erase_claims();
                }
            }
            _ => {}
        }
    }

    /// The Rust spelling of this type, as a signature would write it.
    ///
    /// This is the **identity** of a generic instantiation: the synthetic
    /// declaration `check` mints for `Page<Item>` is named by this, and two
    /// uses are the same instantiation exactly when their spellings agree.
    /// So it must be canonical rather than merely readable — a resolved
    /// `Struct("Item")` and an unresolved `Named("Item")` are the same type and
    /// render identically, and a container renders through its own arguments
    /// rather than through the concrete Rust container kind, which is not part
    /// of the type's identity here (`Vec` and `VecDeque` differ only in what a
    /// decode reconstructs — see [`SeqKind`]).
    pub fn rust_display(&self) -> String {
        let list = |ts: &[Type]| {
            ts.iter()
                .map(|t| t.rust_display())
                .collect::<Vec<_>>()
                .join(", ")
        };
        match self {
            Type::Bool => "bool".into(),
            Type::I8 => "i8".into(),
            Type::I16 => "i16".into(),
            Type::I32 => "i32".into(),
            Type::I64 => "i64".into(),
            Type::U8 => "u8".into(),
            Type::U16 => "u16".into(),
            Type::U32 => "u32".into(),
            Type::U64 => "u64".into(),
            Type::I128 => "i128".into(),
            Type::U128 => "u128".into(),
            Type::F32 => "f32".into(),
            Type::F64 => "f64".into(),
            Type::Usize => "usize".into(),
            Type::Isize => "isize".into(),
            Type::String => "String".into(),
            Type::Char => "char".into(),
            Type::Duration(DurationPeer::Std) => "std::time::Duration".into(),
            Type::Duration(DurationPeer::ChronoTimeDelta) => "chrono::TimeDelta".into(),
            Type::Duration(DurationPeer::Time) => "time::Duration".into(),
            Type::SystemTime(InstantPeer::Std) => "std::time::SystemTime".into(),
            Type::SystemTime(InstantPeer::ChronoUtc) => "chrono::DateTime<chrono::Utc>".into(),
            Type::SystemTime(InstantPeer::TimeOffsetDateTime) => "time::OffsetDateTime".into(),
            Type::Bytes => "Vec<u8>".into(),
            Type::ByteArray(n) => format!("[u8; {n}]"),
            Type::Array(t, n) => format!("[{}; {n}]", t.rust_display()),
            Type::List(t, _) => format!("Vec<{}>", t.rust_display()),
            Type::Set(t, _) => format!("HashSet<{}>", t.rust_display()),
            Type::Map(k, v, _) => format!("HashMap<{}, {}>", k.rust_display(), v.rust_display()),
            Type::Option(t) => format!("Option<{}>", t.rust_display()),
            Type::Tuple(ts) => format!("({})", list(ts)),
            // Transparent to the crossing, so transparent to the identity:
            // `Page<Box<i64>>` and `Page<i64>` are one instantiation, exactly
            // as `Box<T>` and `T` are one wire form (see [`Type::Boxed`]).
            Type::Boxed(t) => t.rust_display(),
            // Spelled as written. A borrow can only reach an instantiation
            // as an argument, where the expansion puts it into a field and
            // FR0077 refuses it — so this is rendered for that message.
            Type::Ref { inner, mutable, unsized_borrow } => {
                let mu = if *mutable { "mut " } else { "" };
                let inner = match (unsized_borrow, inner.as_ref()) {
                    (true, Type::String) => "str".to_string(),
                    (true, Type::Bytes) => "[u8]".to_string(),
                    (true, Type::List(t, _)) => format!("[{}]", t.rust_display()),
                    (_, t) => t.rust_display(),
                };
                format!("&{mu}{inner}")
            }
            // The marker is erased before an instantiation is formed, so this
            // is only reached from a diagnostic; render the type it names.
            Type::Claimed(_, t) => t.rust_display(),
            Type::Named(n)
            | Type::Param(n)
            | Type::Struct(n)
            | Type::Enum(n)
            | Type::Opaque(n)
            | Type::Extern(n) => n.clone(),
            Type::App(n, args) => format!("{n}<{}>", list(args)),
            Type::DartObject(spec) => {
                let item = spec
                    .item
                    .as_ref()
                    .map(|t| t.rust_display())
                    .unwrap_or_else(|| "()".into());
                match (&spec.ret, &spec.err) {
                    (ret, Some(e)) => format!(
                        "DartFunction<{item}, Result<{}, {}>>",
                        ret.as_ref()
                            .map(|r| r.rust_display())
                            .unwrap_or_else(|| "()".into()),
                        e.rust_display()
                    ),
                    (Some(r), None) => format!("DartFunction<{item}, {}>", r.rust_display()),
                    (None, None) => match spec.mirror {
                        DartMirror::Callback => format!("DartCallback<{item}>"),
                        DartMirror::Sink => format!("Sink<{item}>"),
                        DartMirror::EventSink => format!("EventSink<{item}>"),
                        _ => format!("StreamSink<{item}>"),
                    },
                }
            }
        }
    }

    /// The [`DartObjectSpec`] this type is, if it is a handle at the top
    /// level. Does not look inside containers — use [`Type::walk`] (or the
    /// checker's interface-aware walk, which also descends into declared
    /// structs) to find nested ones.
    pub fn as_dart_object(&self) -> Option<&DartObjectSpec> {
        match self {
            Type::DartObject(spec) => Some(spec),
            _ => None,
        }
    }
}

impl Function {
    /// Every type the signature names, in wire order: the parameters, the
    /// return, the declared error. The receiver is not among them — it is
    /// [`Function::parent`], a name rather than a [`Type`].
    ///
    /// One iterator so that a pass over a signature cannot silently miss a
    /// position: the three used to be written out at each call site, and the
    /// error type was the one that got left off.
    pub fn signature_types_mut(&mut self) -> impl Iterator<Item = &mut Type> {
        self.params
            .iter_mut()
            .map(|p| &mut p.ty)
            .chain(self.ret.iter_mut())
            .chain(self.err.iter_mut())
    }

    /// [`Function::signature_types_mut`], read-only.
    pub fn signature_types(&self) -> impl Iterator<Item = &Type> {
        self.params
            .iter()
            .map(|p| &p.ty)
            .chain(self.ret.iter())
            .chain(self.err.iter())
    }
}

impl OpaqueDecl {
    /// The declaration's fully qualified Rust path — the identity that
    /// `Function::trait_impl` links against.
    pub fn full_path(&self) -> String {
        format!("{}::{}", self.module_path, self.name)
    }
}

impl Interface {
    pub fn opaque(&self, name: &str) -> Option<&OpaqueDecl> {
        self.opaques.iter().find(|o| o.name == name)
    }

    pub fn struct_decl(&self, name: &str) -> Option<&StructDecl> {
        self.structs.iter().find(|s| s.name == name)
    }

    pub fn enum_decl(&self, name: &str) -> Option<&EnumDecl> {
        self.enums.iter().find(|e| e.name == name)
    }

    /// True when `name` declares **two** representations — a
    /// `#[bridge(data, locked)]` type, which produces both a [`StructDecl`]
    /// and an [`OpaqueDecl`] of the same name and crosses as two Dart classes.
    ///
    /// This is the one place two declarations of one name are legal: FR0002's
    /// type-name uniqueness rule exempts exactly this pair (rustc's own E0428
    /// forbids two items of one name in one module, so the pair is necessarily
    /// one item). For such a name "how does it cross" has no single answer —
    /// every caller has to ask about a *member* ([`Interface::member_repr`])
    /// or about a use site's marker.
    /// A struct and a **concrete** opaque only. The parser refuses a
    /// concurrency model on an enum ("an enum is always `data`"), so an enum
    ///
    /// A struct beside a trait of the same
    /// name in the same module is rustc's own E0428, so `dyn_trait` is
    /// excluded here to keep the pair meaning what it says rather than to
    /// catch anything. The module paths must agree, or the two declarations
    /// are two different Rust items sharing a name — a duplicate, which
    /// FR0002 refuses.
    pub fn is_dual(&self, name: &str) -> bool {
        match (self.struct_decl(name), self.opaque(name)) {
            (Some(s), Some(o)) => !o.dyn_trait && s.module_path == o.module_path,
            _ => false,
        }
    }

    /// The one representation `name` is declared under, or `None` when it has
    /// two declarations or none.
    ///
    /// Two declarations is `None` whether or not they are one type: where they
    /// are ([`Interface::is_dual`]) the answer belongs to the member; where
    /// they are not, they are two Rust items sharing a name and FR0002 is
    /// refusing the interface anyway.
    fn sole_repr(&self, name: &str) -> Option<Repr> {
        let data = self.struct_decl(name).is_some() || self.enum_decl(name).is_some();
        match (data, self.opaque(name).is_some()) {
            (true, false) => Some(Repr::Data),
            (false, true) => Some(Repr::Handle),
            // Both: a dual type, answered per member. Neither: not a
            // declared type (an extern, or unknown).
            _ => None,
        }
    }

    /// The half `f` is generated onto: [`Function::parent_repr`] when set,
    /// else the parent's only declaration.
    ///
    /// `None` for a free function, for a parent this interface does not
    /// declare, and for a member whose parent has two declarations and no
    /// half recorded. The checker refuses every one of those — FR0067 where
    /// the two declarations are one type, FR0002 where they are not — and
    /// drops the member, so nothing downstream is ever handed one.
    ///
    /// This, never `opaque(parent).is_some()`, is the question every rule
    /// about "is this member on a handle or on a value" is asking. The two
    /// coincide only while a type has one representation.
    pub fn member_repr(&self, f: &Function) -> Option<Repr> {
        let parent = f.parent.as_deref()?;
        f.parent_repr.or_else(|| self.sole_repr(parent))
    }

    /// The handle declaration `f` is generated onto — `None` when `f` is a
    /// free function, or a member of a type's **data** half. See
    /// [`Interface::member_repr`].
    pub fn member_handle(&self, f: &Function) -> Option<&OpaqueDecl> {
        match self.member_repr(f)? {
            Repr::Handle => self.opaque(f.parent.as_deref()?),
            Repr::Data => None,
        }
    }

    /// [`Interface::member_handle`], asked only of a member that has a
    /// receiver: `Some` iff `self` is a live Rust object reached through a
    /// handle id, rather than a value decoded out of the request.
    pub fn receiver_handle(&self, f: &Function) -> Option<&OpaqueDecl> {
        f.receiver.and(self.member_handle(f))
    }

    /// The members generated onto one half of `name`'s Dart surface, in
    /// declaration order.
    ///
    /// `parent == name` alone is not the question a class emitter is asking:
    /// a type declaring both representations has two classes that share a
    /// parent name, and its members split between them by
    /// [`Interface::member_repr`].
    pub fn members<'a>(&'a self, name: &'a str, repr: Repr) -> impl Iterator<Item = &'a Function> {
        self.functions
            .iter()
            .filter(move |f| f.parent.as_deref() == Some(name) && self.member_repr(f) == Some(repr))
    }

    /// The other side of [`Interface::receiver_handle`]: the parent's name
    /// when `f`'s receiver is a **value**, which rides the request ahead of
    /// the parameters and decodes into a local exactly as a parameter does.
    /// `None` for a free function, a receiverless member, and every member of
    /// a handle class.
    pub fn data_receiver<'a>(&self, f: &'a Function) -> Option<&'a str> {
        match (f.receiver, self.member_repr(f)) {
            (Some(_), Some(Repr::Data)) => f.parent.as_deref(),
            _ => None,
        }
    }

    /// Every struct declaration that mints a **Dart class name**: the ones the
    /// author wrote and the generic templates, never the expansions.
    ///
    /// An expansion is a type argument list on a template's class
    /// (`Page<Item>`), not a name of its own — nothing can collide with it,
    /// shadow it, or rename it — so every rule about names reads this, and
    /// every rule about the wire reads [`Interface::structs`].
    pub fn class_structs(&self) -> impl Iterator<Item = &StructDecl> {
        self.structs
            .iter()
            .filter(|s| s.instance.is_none())
            .chain(self.generic_structs.iter())
    }

    /// See [`Interface::class_structs`].
    pub fn class_enums(&self) -> impl Iterator<Item = &EnumDecl> {
        self.enums
            .iter()
            .filter(|e| e.instance.is_none())
            .chain(self.generic_enums.iter())
    }

    /// The generic struct template named `name`, if there is one.
    pub fn generic_struct(&self, name: &str) -> Option<&StructDecl> {
        self.generic_structs.iter().find(|s| s.name == name)
    }

    /// The generic enum template named `name`, if there is one.
    pub fn generic_enum(&self, name: &str) -> Option<&EnumDecl> {
        self.generic_enums.iter().find(|e| e.name == name)
    }

    /// The [`Instance`] a declared name was expanded from, if it is one of the
    /// synthetic declarations `check` mints. `None` for everything an author
    /// wrote — which is what every existing rule and emitter arm sees.
    pub fn instance_of(&self, name: &str) -> Option<&Instance> {
        self.struct_decl(name)
            .and_then(|s| s.instance.as_ref())
            .or_else(|| self.enum_decl(name).and_then(|e| e.instance.as_ref()))
    }

    /// The identifier a declaration's generated codec functions are named
    /// after (`enc_Point`, `_decPoint`).
    ///
    /// The Rust name for anything an author wrote — private helpers keep it
    /// deliberately, and it is unique because Rust type names are. A synthetic
    /// expansion is named `Page<Item>`, which is unique for the same reason and
    /// is not an identifier, so it takes [`Instance::stem`] instead.
    pub fn codec_stem(&self, name: &str) -> String {
        match self.instance_of(name) {
            Some(i) => i.stem.clone(),
            None => name.to_string(),
        }
    }

    /// The template parameter names of the generic declaration named `name`.
    pub fn template_params(&self, name: &str) -> Option<&[String]> {
        self.generic_struct(name)
            .map(|s| s.generics.as_slice())
            .or_else(|| self.generic_enum(name).map(|e| e.generics.as_slice()))
    }

    /// True when `name` is a struct declared `#[bridge(dart_interface)]` — the
    /// declared-mirror form, whose Dart surface is an interface to implement
    /// rather than a class to construct.
    pub fn is_dart_interface(&self, name: &str) -> bool {
        self.struct_decl(name).is_some_and(|s| s.dart_interface)
    }

    pub fn extern_decl(&self, name: &str) -> Option<&ExternDecl> {
        self.externs.iter().find(|e| e.name == name)
    }

    /// The bridged trait a `Function::trait_impl` path points at, if it is
    /// bridged at all (foreign traits are legal impl targets and get static
    /// dispatch only).
    pub fn dyn_trait_by_path(&self, path: &str) -> Option<&OpaqueDecl> {
        self.opaques
            .iter()
            .find(|o| o.dyn_trait && o.full_path() == path)
    }

    /// Concrete opaques with a bridged `impl <trait> for Type`, in opaque
    /// declaration order. This order IS the impl-tag assignment (tag 0 is
    /// the dyn handle; tag i+1 is implementors[i]) — deterministic from the
    /// finalized IR, so both emitters agree by construction.
    pub fn trait_implementors(&self, trait_name: &str) -> Vec<&OpaqueDecl> {
        let Some(t) = self.opaque(trait_name) else {
            return vec![];
        };
        let path = t.full_path();
        self.opaques
            .iter()
            .filter(|o| !o.dyn_trait)
            .filter(|c| {
                self.functions.iter().any(|f| {
                    f.parent.as_deref() == Some(c.name.as_str())
                        && f.trait_impl.as_deref() == Some(path.as_str())
                })
            })
            .collect()
    }

    /// The bridged traits a concrete opaque implements (for the generated
    /// Dart `implements` clause), in trait declaration order.
    pub fn implemented_traits(&self, concrete: &str) -> Vec<&OpaqueDecl> {
        self.opaques
            .iter()
            .filter(|o| o.dyn_trait)
            .filter(|t| {
                let path = t.full_path();
                self.functions.iter().any(|f| {
                    f.parent.as_deref() == Some(concrete)
                        && f.trait_impl.as_deref() == Some(path.as_str())
                })
            })
            .collect()
    }
}
