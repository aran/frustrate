//! Resolution, model checking, and finalization.
//!
//! This is where the frustrate safety property is enforced:
//!   - safe by default: the unannotated path can never reach a concurrency
//!     hazard at runtime;
//!   - everything expressible: every rejected combination names the explicit,
//!     contract-bearing opt-in that permits it;
//!   - contracts are loud: opted-in failure modes are deterministic and
//!     attributable (see frustrate-runtime ContentionError).

use crate::ir::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt;

/// Names `parse::parse_path_type` claims on **every** path, so a declared
/// bridged type of that name can never be reached from a signature (FR0046).
///
/// Only the spellings claimed without generic arguments and without a root
/// guard. `Vec`, `Option`, `HashMap`, `DateTime` and friends are claimed only
/// in their generic form, and a bridged type cannot be generic, so a declared
/// `Vec` resolves to the user's type exactly as they intended.
/// `StreamSink`/`DartCallback`/`DartFunction` *are* here, because their
/// wrong-arity arms reject instead of falling through — a bare one never
/// reaches `Type::Named` either. `Sink`/`EventSink`/`StreamController` are
/// deliberately absent: those arms are `dart::`-path-gated for exactly this
/// reason, so a user's own type of that name is already safe.
const PARSER_CLAIMED_TYPE_NAMES: &[&str] = &[
    "String",
    "str",
    // Every path either maps this to a peer or refuses it (FR0045); none
    // reaches a declaration.
    "Duration",
    // Position-restricted, but still claimed at parse before resolution.
    "Result",
    "Deferred",
    "StreamSink",
    "DartCallback",
    "DartFunction",
];

/// Names the parser claims for their **bare** spelling only — a qualified path
/// under a foreign root falls through to `Type::Named` and does reach a
/// declaration.
///
/// Still rejected, but for a different reason than
/// [`PARSER_CLAIMED_TYPE_NAMES`], and the message says so: the declaration is
/// reachable, just not by the name anyone will actually write.
const PARSER_CLAIMED_BARE_TYPE_NAMES: &[&str] = &[
    "SystemTime",
    "TimeDelta",
    "OffsetDateTime",
    "Instant",
    "NaiveDateTime",
    "PrimitiveDateTime",
    "PlainDateTime",
    "UtcDateTime",
];

/// Dart types the emitters name in generated code. A bridged type of one of
/// these names is emitted as `class <name>` into the same library, which
/// shadows the real one for every generated reference to it (FR0046).
///
/// A different rule from the two above — it is about the *Dart* surface, and
/// it catches the cases those miss: `DateTime` is only claimed by the parser
/// as `DateTime<Tz>`, so a declared `DateTime` resolves fine and then collides
/// on the way out. `BigInt`/`Uint8List`/`List` have the same hole and predate
/// the time work entirely.
const DART_SHADOWED_TYPE_NAMES: &[&str] = &[
    "String", "int", "double", "bool", "BigInt", "Duration", "DateTime", "Uint8List", "List",
    "Map", "Set", "Object", "Record", "Function", "Future", "Stream", "Iterable", "Null",
];

/// Facts about a target platform. Not a build-time choice: every `check`
/// invocation verifies the full surface under native facts and the web
/// subset (native-only members omitted) under web facts, so both generated
/// surfaces are valid by construction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capabilities {
    /// May generated code block the calling thread (contended lock with
    /// `on_contention = "block"`)? True for native targets; false on web.
    ///
    /// The web answer is about *reachability*, not about sync-ness: the page
    /// instance is dispatched to from the browser main thread, so a member on
    /// it may be entered there, and blocking there is fatal. This holds under
    /// both wasm builds, for different reasons — threaded wasm traps on
    /// `memory.atomic.wait32`, single-threaded wasm has no wait instruction
    /// and spins instead.
    ///
    /// Note what this is *not*: a claim that sync members run on the main
    /// thread and async ones do not. Thread placement is a property of the
    /// instance and the Dart-side dispatch route, not of a member's
    /// sync/async-ness — an actor's methods run on that actor's worker
    /// whatever their exec kind, and on single-threaded web every body runs
    /// on the caller. The capability is false on web because *some* route
    /// reaches the main thread, which is all a refusal needs.
    pub blocking_allowed: bool,
}

impl Capabilities {
    pub fn native() -> Self {
        Capabilities {
            blocking_allowed: true,
        }
    }
    pub fn web() -> Self {
        Capabilities {
            blocking_allowed: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub code: &'static str,
    pub message: String,
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "error[{}]: {}", self.code, self.message)
    }
}

/// A non-fatal finding: codegen succeeded, but something in the source looks
/// like a mistake the author would want to know about.
///
/// A separate type from [`Diagnostic`] rather than a severity field on it,
/// deliberately. Every rule the checker enforces is a fact it can prove, and
/// those must stay rejections; a warning is by nature a *heuristic*, and the
/// type system should not let one be returned where a proof was expected. The
/// distinction is visible in the rendering too — `warning[FR….]`, never
/// `error[FR….]`.
///
/// Carries its own source location, because the thing it reports is an item
/// codegen did NOT take into the interface: there is no IR node to hang it off
/// and no later stage that could reconstruct where it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    pub code: &'static str,
    /// The bridge source this came from. Empty until `generate` fills it in:
    /// the parser is handed text, not a path.
    pub file: String,
    /// 1-based line of the item the warning is about; 0 when unknown.
    pub line: usize,
    pub message: String,
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.file.is_empty(), self.line) {
            (false, 0) => write!(f, "{}: ", self.file)?,
            (false, n) => write!(f, "{}:{n}: ", self.file)?,
            (true, 0) => {}
            (true, n) => write!(f, "line {n}: ")?,
        }
        write!(f, "warning[{}]: {}", self.code, self.message)
    }
}

/// Resolve named types, enforce the model rules against both platforms'
/// facts, derive per-member `requires_native`, and assign dispatch ids.
pub fn check(iface: Interface) -> Result<Interface, Vec<Diagnostic>> {
    // Pass 1: the full surface must hold under native facts.
    let mut iface = check_pass(iface, &Capabilities::native())?;

    // Derive the native-only fact. One source is the author's own declaration
    // (`#[bridge(native_only)]`, on the member or on an opaque type it names);
    // the other two are structural, and share a theme — work the browser main
    // thread cannot do, either because it must BLOCK against the caller's event
    // loop or because completing it reaches a wait instruction:
    //
    //   - `on_contention = "block"`, which declares that the acquisition itself
    //     waits. Its sibling `"error"` does not: the try-lock is one
    //     compare-exchange and the release reaches no wait instruction
    //     (`frustrate::handle::LockedCell`), so a synchronous try-lock is on
    //     every surface and only the blocking contract is derived here; or
    //   - a value-returning `DartFunction` invoked via the BLOCKING
    //     `DartFunction::call` — which parks the
    //     invoking worker while the application thread runs the closure.
    //
    //
    // But `DartFunction` also has a portable primitive, `call_async`, awaited
    // cooperatively on the executor — a `Pending` poll yields to the event
    // loop instead of parking a thread. A member can only take that path if
    // its body can `.await`, i.e. it is a Rust `async fn` (`rust_async`).
    // Actor methods cannot be `async fn` (check_function rejects it — an actor
    // already owns its executor), and a plain pool member is not a future, so
    // both must use blocking `call` and stay native-only. Hence a returning
    // callback forces native ONLY when the member is not a Rust `async fn`.
    // (A Rust `async fn` with NO returning callback was already portable.)
    //
    // A returning method may sit anywhere a handle can — including inside a
    // struct parameter — so this rides the interface-aware type-graph walk
    // rather than a top-level scan. The declared source rides the same walk,
    // for the same reason: a native-only opaque may be nested as deeply as any
    // other type, and a shallow check would let a member that names one into
    // the web subset, whose glue would then fail to resolve. Computed first,
    // then assigned, because the walk borrows the interface the loop would be
    // mutating.
    // Two vectors, because they answer different questions. `declared` is the
    // author's fact — written on the member, or inherited from a native-only
    // opaque the member names — and it is written back so everything
    // downstream (the omission breadcrumb's reason, FR0032) can distinguish a
    // declaration from a derivation without re-walking the graph.
    let declared: Vec<bool> = iface
        .functions
        .iter()
        .map(|f| f.native_only || native_only_type_in_scope(&iface, f).is_some())
        .collect();
    let native: Vec<bool> = iface
        .functions
        .iter()
        .zip(&declared)
        .map(|(f, &d)| {
            d || f.on_contention == Some(OnContention::Block)
                || (!f.rust_async
                    && f.params.iter().any(|p| {
                        // `is_returning`, not `ret.is_some()`: a
                        // `DartFunction<T, Result<(), E>>` returns no value but
                        // still round-trips, so it parks the invoking worker
                        // exactly as a value-returning one does.
                        reachable_handles(&iface, &p.ty)
                            .iter()
                            .any(|spec| spec.is_returning())
                    }))
        })
        .collect();
    for ((f, n), d) in iface.functions.iter_mut().zip(native).zip(declared) {
        f.requires_native = n;
        f.native_only = d;
    }

    // The `#[bridge(web = "runtime_fail")]` opt-in only means something on a
    // native-only member: it trades that member's compile-time absence for a
    // runtime-throwing web stub. On a member that
    // already runs everywhere it is a no-op the author expected to matter — so
    // reject it loudly rather than silently ignore, the way the whole checker
    // treats a contract that cannot bite.
    {
        let mut diags = vec![];
        for f in &iface.functions {
            let ctx = match &f.parent {
                Some(p) => format!("`{}::{}`", p, f.name),
                None => format!("`{}`", f.name),
            };
            if f.web_runtime_fail && !f.requires_native {
                diags.push(Diagnostic {
                    code: "FR0030",
                    message: format!(
                        "{ctx}: `#[bridge(web = \"runtime_fail\")]` is only meaningful on a \
                         native-only member — one that cannot run on web: \
                         `on_contention = \"block\"`, a value-returning DartFunction on a \
                         non-async member, or a `#[bridge(native_only)]` declaration. \
                         This member already runs on web, so the opt-in would do \
                         nothing. Remove it — or, if this member cannot build for wasm32 \
                         (a native-only dependency), declare that with \
                         `#[bridge(native_only)]` and keep the opt-in"
                    ),
                });
            }
            // A stub cannot reference a class the web surface does not emit —
            // and a member of a native-only type has nowhere to be declared.
            if f.web_runtime_fail && f.requires_native {
                if let Some(ty) = native_only_type_in_scope(&iface, f) {
                    let how = if f.parent.as_deref() == Some(ty.as_str()) {
                        "it is a member of"
                    } else {
                        "its signature names"
                    };
                    diags.push(Diagnostic {
                        code: "FR0032",
                        message: format!(
                            "{ctx}: `#[bridge(web = \"runtime_fail\")]` cannot stub this \
                             member on web — {how} `{ty}`, which is declared with a \
                             concurrency model and `native_only` (e.g. \
                             `#[bridge(confined, native_only)]`) and has no class on the \
                             web surface. Remove the opt-in (this member is compile-time \
                             absent on web, with the rest of `{ty}`), or move `native_only` \
                             off `{ty}` onto individual members if `{ty}` itself must stay \
                             visible on web"
                        ),
                    });
                }
            }
            // The undeclared hazard the charter asks to be named: codegen
            // cannot evaluate a cfg predicate, so it emits the member into both
            // surfaces regardless, and the build dies inside generated code
            // (E0425) on whichever target the gate excluded.
            // Reported per member, except when the whole type carries the cfg
            // — there the type-level diagnostic below says it once instead of
            // once per method, and both name the same single fix.
            let parent_gated = iface.member_handle(f).is_some_and(|o| o.cfg_gated);
            if f.cfg_gated && !f.requires_native && !parent_gated {
                diags.push(Diagnostic {
                    code: "FR0034",
                    message: format!(
                        "{ctx}: this bridged item is `#[cfg]`-gated, but codegen never \
                         evaluates cfg predicates — the member is emitted into the web \
                         Dart surface and the wasm dispatch table either way, and the \
                         build fails inside generated code (E0425) on any target the \
                         gate excludes. If the gate means \"native only\", declare it: \
                         `#[bridge(native_only)]`. If it selects among native targets, \
                         move the cfg inside the function body so the bridged item \
                         always exists"
                    ),
                });
            }
        }
        // The same hazard one level up. A cfg-gated opaque type still gets
        // `frustrate_drop_T`/`frustrate_finalize_T` exports naming
        // `crate::…::T` unconditionally, so the gate excluding the target
        // being built fails inside generated code exactly as a member's would.
        //
        // Data structs, data enums and `bytes(...)` externs are not covered:
        // they are equally unevaluatable, but `native_only` is rejected on them
        // (it has no web presence to remove), so FR0034's prescribed fix does
        // not apply and firing it would name a remedy that cannot be taken.
        for o in &iface.opaques {
            if o.cfg_gated && !o.native_only {
                let kind = if o.dyn_trait { "trait" } else { "type" };
                diags.push(Diagnostic {
                    code: "FR0034",
                    message: format!(
                        "`{}`: this bridged {kind} is `#[cfg]`-gated, but codegen never \
                         evaluates cfg predicates — the {kind} is emitted into the web \
                         Dart surface, and its drop and finalize exports name the Rust \
                         {kind} either way, so the build fails inside generated code \
                         (E0425) on any target the gate excludes. If the gate means \
                         \"native only\", declare it on the concurrency model instead: \
                         `#[bridge(confined, native_only)]` (or frozen/locked/actor)",
                        o.name
                    ),
                });
            }
        }
        if !diags.is_empty() {
            return Err(diags);
        }
    }

    // Pass 2: the web surface — native-only members omitted — must hold
    // under web facts. Structural today (the subset excludes everything
    // web facts reject), load-bearing the day a new fact or rule lands.
    let web_subset = Interface {
        functions: iface
            .functions
            .iter()
            .filter(|f| !f.requires_native)
            .cloned()
            .collect(),
        ..iface.clone()
    };
    check_pass(web_subset, &Capabilities::web())?;

    finalize(iface)
}

/// Resolve named types and enforce the model rules under one fact set.
fn check_pass(mut iface: Interface, caps: &Capabilities) -> Result<Interface, Vec<Diagnostic>> {
    let mut diags = vec![];

    // Generic data types, before anything else reads a declaration: every
    // fully-applied use becomes a synthetic non-generic declaration, so every
    // rule below — direction, native-only propagation, name minting, the
    // fingerprint — sees ordinary declarations and needs no notion of a
    // parameter. See [`expand_generics`].
    expand_generics(&mut iface, &mut diags);
    if !diags.is_empty() {
        // Bail rather than accumulate: everything below reasons about resolved
        // declarations, and an interface whose expansion failed does not have
        // them. A second batch of diagnostics derived from a broken expansion
        // would name types the author never wrote.
        return Err(diags);
    }

    let templates: HashSet<String> = iface
        .generic_structs
        .iter()
        .map(|s| s.name.clone())
        .chain(iface.generic_enums.iter().map(|e| e.name.clone()))
        .collect();
    let structs: HashSet<String> = iface.structs.iter().map(|s| s.name.clone()).collect();
    let enums: HashSet<String> = iface.enums.iter().map(|e| e.name.clone()).collect();
    let opaques: HashMap<String, Model> = iface
        .opaques
        .iter()
        .map(|o| (o.name.clone(), o.model))
        .collect();
    let externs: HashSet<String> = iface.externs.iter().map(|e| e.name.clone()).collect();
    let dyn_traits: HashSet<String> = iface
        .opaques
        .iter()
        .filter(|o| o.dyn_trait)
        .map(|o| o.name.clone())
        .collect();
    // Names declared under two representations. Computed once, from the
    // declaration fact itself, because "is this name dual" is asked from six
    // places and a second spelling of it could disagree with the first.
    let duals: HashSet<String> = iface
        .opaques
        .iter()
        .filter(|o| iface.is_dual(&o.name))
        .map(|o| o.name.clone())
        .collect();
    // Every name with a value declaration *and* a handle one, dual or not.
    // The ones that are not dual are two different Rust items sharing a name
    // — a struct beside a same-named trait, or two modules colliding — which
    // the duplicate-name rule below reports. They are collected here only so
    // that their members can be dropped alongside the genuinely unplaced
    // ones, because "which half" has no answer for them either.
    let two_declarations: HashSet<String> = iface
        .opaques
        .iter()
        .filter(|o| iface.struct_decl(&o.name).is_some() || iface.enum_decl(&o.name).is_some())
        .map(|o| o.name.clone())
        .collect();

    // Which half of a dual type each member lands on. Every rule after this
    // asks `member_repr`, so this has to run before all of them — and three
    // consumers run ahead of `check_function` (the trait-impl model check and
    // the native-only agreement below, and the FR0027 surface collision), so
    // "before all of them" means here, immediately after the declared names.
    // It cannot be the parser's job: `merge` concatenates the `--src` files
    // with no per-file identity, so the file holding an `impl` block need not
    // be the one holding the declaration.
    //
    // A member with no half is dropped from the interface rather than left in
    // it. Every downstream rule is written for a member that is on one half or
    // the other, and each would answer a half-question about it wrongly and
    // loudly: FR0026 would say `Doc` "crosses by value", FR0027 would test it
    // against the data surface, FR0005 would say the type declares no
    // representation. FR0067 has already said the true thing; the rest is
    // noise the author has to read past. The cost is the member's own
    // signature going unresolved this run — the same staging the minted-name
    // rule takes, where the author fixes the cause and sees the rest next run.
    {
        // Keyed by *block*, not by parent: `impl Doc` and
        // `impl Store for Doc` are two blocks with one fix each, and the IR
        // has no block identity other than the pair.
        let bridged_trait_paths: HashSet<String> = iface
            .opaques
            .iter()
            .filter(|o| o.dyn_trait)
            .map(|o| o.full_path())
            .collect();
        let mut unplaced: HashSet<(String, Option<String>)> = HashSet::new();
        iface.functions.retain_mut(|f| {
            let Some(parent) = f.parent.clone() else {
                return true;
            };
            // Already placed: leave it. That guard is what makes a *valid*
            // dual interface survive the second pass — `check_function`
            // erases `parent_claim` in pass 1, so pass 2 sees members whose
            // half is recorded and whose claim is gone, and without it every
            // one of them would be reported as unplaced. (Pass 2 runs only
            // after pass 1 returned `Ok`, so it never meets one that is
            // genuinely unplaced.)
            if !two_declarations.contains(&parent) || f.parent_repr.is_some() {
                return true;
            }
            // Two declarations that are not one type: the duplicate-name rule
            // below has already named the collision, and it is the only thing
            // true to say. Dropping the members keeps every later rule from
            // adding a second, false sentence about them — `#[bridge] impl S`
            // on a struct beside a same-named trait would otherwise be told
            // that `S` declares no representation.
            if !duals.contains(&parent) {
                return false;
            }
            // From the `impl` block's own self type, never from the member:
            // a representation names how a *type* crosses, and FR0063 refuses
            // the keyword on a member for that reason.
            match f.parent_claim {
                Some(Claim::Data) if structs.contains(&parent) => {
                    f.parent_repr = Some(Repr::Data);
                    true
                }
                Some(Claim::Model(m)) if opaques.get(&parent) == Some(&m) => {
                    f.parent_repr = Some(Repr::Handle);
                    true
                }
                other => {
                    // One diagnostic per block, not per member: the fix is one
                    // edit to the block's self type.
                    let tr = f.trait_impl.clone();
                    if unplaced.insert((parent.clone(), tr.clone())) {
                        // A *bridged* trait is implemented by handles only
                        // (FR0026), so offering the value marker there would
                        // prescribe a rejection. A foreign trait has no such
                        // rule and keeps both.
                        let handle_only = tr
                            .as_deref()
                            .is_some_and(|p| bridged_trait_paths.contains(p));
                        diags.push(unplaced_member_diagnostic(
                            &parent,
                            other,
                            tr.as_deref(),
                            handle_only,
                            &opaques,
                        ));
                    }
                    false
                }
            }
        });
    }

    // Trait declarations. Confined, frozen and locked each require a thread
    // bound of their handle type (runtime/rust/src/handle.rs): Send for
    // confined, Send + Sync for frozen and locked. A concrete opaque states
    // that bound as a fact about itself; `dyn T` erases the type, so
    // `Box<dyn T>` can only inherit it from the trait's own supertraits. Hence
    // the requirement is on the declaration. Resident requires nothing, which
    // is not an omission: it has no thread bound to inherit, so a resident
    // trait carries exactly the bounds its author wrote. Actor traits are
    // deferred: actor spawn is anchored on concrete-type constructors (the
    // extension is recorded in the design).
    for o in &iface.opaques {
        if !o.dyn_trait {
            continue;
        }
        match o.model {
            Model::Confined | Model::Frozen | Model::Locked => {
                let has = |b: &str| o.supertraits.iter().any(|s| s == b);
                // Confined is Send-only on purpose: the model is one owner and
                // serialized use, so a confined trait object is never shared
                // between threads at one time — it is only *born* on one and
                // used on another.
                let confined = o.model == Model::Confined;
                if !(has("Send") && (confined || has("Sync"))) {
                    let bound = if confined { "Send" } else { "Send + Sync" };
                    diags.push(Diagnostic {
                        code: "FR0021",
                        message: format!(
                            "trait `{0}`: a {1} trait must declare `{bound}` \
                             supertrait{2} (`pub trait {0}: {bound}`) — the erased \
                             type cannot prove the bound the way a concrete opaque \
                             does, and `Box<dyn {0}>` is what the handle holds",
                            o.name,
                            match o.model {
                                Model::Confined => "confined",
                                Model::Frozen => "frozen",
                                _ => "locked",
                            },
                            if confined { "" } else { "s" },
                        ),
                    });
                }
            }
            Model::Resident => {}
            Model::Actor => diags.push(Diagnostic {
                code: "FR0023",
                message: format!(
                    "trait `{}`: an actor trait is not supported — actor spawn is \
                     anchored on concrete-type constructors, and a trait has none. \
                     Declare the trait confined, resident, frozen, or locked — or \
                     declare a concrete actor type",
                    o.name
                ),
            }),
        }
    }

    // FR0056: a generic **function or method**. A data declaration may be
    // generic — `expand_generics` above turned every fully-applied use of one
    // into a non-generic declaration — but a bridged fn cannot, and that is
    // what makes the expansion possible: the instantiations are exactly what
    // the signatures write, and a signature that was itself generic would have
    // none to write. Nothing else could supply them: a call from Dart carries
    // its arguments' bytes, not their Rust types.
    //
    // Kept ahead of resolution so it leads the batch: a parameter used as a
    // parameter type is deliberately NOT reported as an unknown type below
    // (`resolve_type`), or the author would go looking for a declaration that
    // cannot exist.
    for f in &iface.functions {
        if f.generics.is_empty() {
            continue;
        }
        let ctx = match &f.parent {
            Some(p) => format!("fn `{}::{}`", p, f.name),
            None => format!("fn `{}`", f.name),
        };
        diags.push(Diagnostic {
            code: "FR0056",
            message: format!(
                "{ctx}: a generic bridged function is not supported. `<{}>` would have to \
                 be chosen by the caller, and a Dart call carries its arguments' bytes, \
                 not their Rust types — so there is no instantiation for codegen to emit. \
                 Write one bridged fn per instantiation, or take the argument as a \
                 declared type: a `#[bridge(data)]` struct or enum (which may itself be \
                 generic — `Page<Item>` in a signature is fine), a handle, or a \
                 `#[bridge(bytes(...))]` value",
                f.generics.join(", ")
            ),
        });
    }

    // The declared-mirror form. Three rules are what make that emission
    // possible at all, checked here rather than discovered as a failure inside generated
    // Dart.
    for s in &iface.structs {
        if !s.dart_interface {
            continue;
        }
        if s.fields.is_empty() {
            diags.push(Diagnostic {
                code: "FR0051",
                message: format!(
                    "struct `{}`: a dart_interface declares no methods. Its Dart \
                     side would be an interface with nothing to implement and its \
                     Rust side a struct with nothing to call. Add at least one \
                     DartCallback/DartFunction field, or drop `dart_interface`",
                    s.name
                ),
            });
        }
        for f in &s.fields {
            let ok = matches!(&f.ty, Type::DartObject(spec) if spec.mirror.is_closure());
            if !ok {
                diags.push(Diagnostic {
                    code: "FR0050",
                    message: format!(
                        "struct `{}`, field `{}`: every field of a dart_interface is one \
                         method of the generated Dart interface, so it must be a \
                         `DartCallback<T>` (a void method) or a `DartFunction<T, R>` (a \
                         returning one) written directly — not `{}`. A sink mirror is a \
                         Dart object with methods of its own, not a method; a data field \
                         is state, and an interface has no constructor to supply it; and a \
                         container or `Option` around a mirror has no method to be. Keep \
                         those as a plain `#[bridge]` struct of handles, which crosses \
                         exactly the same way",
                        s.name,
                        f.name,
                        error_type_label(&f.ty)
                    ),
                });
            }
            // The emitter's own conversion, not the Rust name: that is where
            // the collision would otherwise land, as an "can't override
            // Object.toString" inside generated code naming nothing the author
            // wrote. Same hazard and answer as FR0027's `dispose`.
            let method = crate::emit_dart::dart_name(&f.name);
            if matches!(
                method.as_str(),
                "toString" | "hashCode" | "runtimeType" | "noSuchMethod"
            ) {
                diags.push(Diagnostic {
                    code: "FR0052",
                    message: format!(
                        "struct `{}`, field `{}`: the generated Dart method would be \
                         `{method}`, which every Dart class already inherits from \
                         `Object` with a different signature — the interface would not \
                         compile. Rename the field",
                        s.name, f.name
                    ),
                });
            }
        }
    }

    // FR0036: a typed error `E` generates the Dart class `EException`. If the
    // interface already declares a type by that name, the generated file has
    // two classes with one name and does not compile — a failure inside
    // generated code, naming nothing the author wrote. The four runtime
    // exceptions are re-exported by every surface, so they collide too.
    {
        // The names a generated exception class can collide with: every Dart
        // class this interface declares. That is `class_structs`/`class_enums`
        // — the templates in, the expansions out — for the reason those exist.
        let declared: HashSet<&str> = iface
            .class_structs()
            .map(|s| s.name.as_str())
            .chain(iface.class_enums().map(|e| e.name.as_str()))
            .chain(opaques.keys().map(|s| s.as_str()))
            .chain(externs.iter().map(|s| s.as_str()))
            .collect();
        // Two generated names, because a typed error can be declared in two
        // directions now. `EException` comes from either; `EFallible` — the
        // closure-parameter alias that makes fallibility visible in the Dart
        // signature — only from a `DartFunction<T, Result<R, E>>`.
        let mut seen: Vec<String> = Vec::new();
        let mut names: Vec<(String, String)> = Vec::new();
        for e in typed_error_names(&iface) {
            // A generic error has ONE exception class, named after the
            // template: `Refusal<i64>` and `Refusal<String>` both generate
            // `RefusalException<T>`, because `Refusal<i64>Exception` is not a
            // Dart identifier. So the name that can collide is the template's.
            let stem = match iface.instance_of(e.name) {
                Some(i) => i.template.clone(),
                None => e.name.to_string(),
            };
            if seen.contains(&stem) {
                continue;
            }
            seen.push(stem.clone());
            names.push((stem.clone(), format!("{stem}Exception")));
            if e.on_a_closure {
                names.push((stem.clone(), format!("{stem}Fallible")));
            }
        }
        for (e, generated) in names {
            let clash = declared.contains(generated.as_str());
            let runtime = matches!(
                generated.as_str(),
                "BridgeException"
                    | "BridgePanicException"
                    | "ContentionException"
                    | "LeakedChannelError"
            );
            if clash || runtime {
                let whose = if runtime {
                    "one of the runtime exceptions every generated surface re-exports"
                } else {
                    "a type this interface already declares"
                };
                diags.push(Diagnostic {
                    code: "FR0036",
                    message: format!(
                        "typed error `{e}` would generate the Dart name `{generated}`, \
                         which collides with {whose}. Two declarations of one name do not \
                         compile, and the error would be inside generated code. Rename \
                         `{e}`"
                    ),
                });
            }
        }
    }

    // Resolve Named types everywhere. `generics` is the enclosing
    // declaration's own parameters, which resolve to nothing and say nothing
    // (FR0056 has spoken above).
    let resolve = |ty: &mut Type, generics: &[String], ctx: &str, diags: &mut Vec<Diagnostic>| {
        let scope = Scope {
            structs: &structs,
            enums: &enums,
            opaques: &opaques,
            externs: &externs,
            duals: &duals,
            generics,
            templates: &templates,
            self_name: None,
        };
        resolve_type(ty, &scope, ctx, diags)
    };
    // A declaration's own fields are the one position where the parser
    // substitutes `Self` for a **bare** name, so they are the one position
    // where a name the author never typed can appear. See [`Scope::self_name`].
    let resolve_field = |ty: &mut Type,
                         generics: &[String],
                         self_name: Option<&str>,
                         ctx: &str,
                         diags: &mut Vec<Diagnostic>| {
        let scope = Scope {
            structs: &structs,
            enums: &enums,
            opaques: &opaques,
            externs: &externs,
            duals: &duals,
            generics,
            templates: &templates,
            self_name,
        };
        resolve_type(ty, &scope, ctx, diags)
    };
    // A field may name a handle. Which *direction* the type it belongs to may
    // then travel is a use-site question (FR0004 below), not a declaration
    // one: returning transfers each handle exactly once, and only decoding a
    // handle *from* Dart would be the consume this used to refuse here.
    //
    // The exception is a type that reaches **both** kinds, which is refused at
    // the declaration because the two directions are opposite and neither is
    // available: a Dart-object handle makes the type argument-only (FR0031,
    // Rust cannot mint a Dart object) and an opaque makes it return-only
    // (FR0004, Dart cannot give up ownership). No position could carry it, so
    // every use site would report, and the emitters would be asked for a codec
    // that has no direction to be written in.
    let mut structs_vec = std::mem::take(&mut iface.structs);
    for s in &mut structs_vec {
        for f in &mut s.fields {
            let ctx = format!("struct `{}`, field `{}`", s.name, f.name);
            resolve_field(&mut f.ty, &s.generics, Some(&s.name), &ctx, &mut diags);
            field_takes_no_borrow(&f.ty, &ctx, &mut diags);
        }
    }
    iface.structs = structs_vec;
    // Templates are resolved too, though no wire form of theirs is ever
    // emitted: the generic Dart class comes from the template, so `dart_type`
    // has to meet resolved names there. It is also the only thing that judges a
    // template nobody instantiates — which would otherwise emit a class naming
    // a type the interface does not declare.
    let mut generic_structs = std::mem::take(&mut iface.generic_structs);
    for s in &mut generic_structs {
        for f in &mut s.fields {
            let ctx = format!("struct `{}<{}>`, field `{}`", s.name, s.generics.join(", "), f.name);
            resolve_field(&mut f.ty, &[], Some(&s.name), &ctx, &mut diags);
        }
    }
    iface.generic_structs = generic_structs;
    let mut generic_enums = std::mem::take(&mut iface.generic_enums);
    for e in &mut generic_enums {
        for v in &mut e.variants {
            for f in &mut v.fields {
                let ctx = format!(
                    "enum `{}<{}>`, variant `{}`, field `{}`",
                    e.name,
                    e.generics.join(", "),
                    v.name,
                    f.name
                );
                resolve_field(&mut f.ty, &[], Some(&e.name), &ctx, &mut diags);
            }
        }
    }
    iface.generic_enums = generic_enums;
    let mut enums_vec = std::mem::take(&mut iface.enums);
    for e in &mut enums_vec {
        for v in &mut e.variants {
            for f in &mut v.fields {
                let ctx = format!("enum `{}`, variant `{}`, field `{}`", e.name, v.name, f.name);
                resolve_field(&mut f.ty, &e.generics, Some(&e.name), &ctx, &mut diags);
                field_takes_no_borrow(&f.ty, &ctx, &mut diags);
            }
        }
    }
    iface.enums = enums_vec;
    {
        let names: Vec<(&str, Type)> = iface
            .structs
            .iter()
            .map(|s| (s.name.as_str(), Type::Struct(s.name.clone())))
            .chain(
                iface
                    .enums
                    .iter()
                    .map(|e| (e.name.as_str(), Type::Enum(e.name.clone()))),
            )
            .collect();
        for (name, ty) in names {
            let mut dart_object = false;
            walk_type_graph(&iface, &ty, &mut |t| {
                if t.as_dart_object().is_some() {
                    dart_object = true;
                }
            });
            // An **inbound** declaration is exempt, and not by special case:
            // the conflict below is that the two handle kinds want opposite
            // directions, and on an inbound class they want the same one. A
            // Dart-object handle travels Dart → Rust, and so does a handle
            // field there — as the `Consumed<…>` the caller minted. The
            // generated codecs agree: both kinds put this declaration on the
            // encoder-and-no-decoder side.
            let inbound = inbound_struct(&iface, name).is_some();
            if dart_object && reaches_opaque(&iface, &ty) && !inbound {
                diags.push(Diagnostic {
                    code: "FR0004",
                    message: format!(
                        "`{name}`: this type reaches a Dart-object handle *and* an \
                         opaque handle, and the two pull it in opposite directions. A \
                         Dart-object handle can only travel Dart → Rust (FR0031: Rust \
                         cannot mint a Dart object); an opaque handle in a *plain* data \
                         type can only travel Rust → Dart, that class being \
                         return-shaped — its handle fields are `Doc`, not the \
                         `Consumed<Doc>` handing one over needs. So no parameter, return \
                         or field could carry `{name}` as declared. Declare it \
                         `#[bridge(data, inbound)]`, which makes both kinds travel the \
                         one way, or split it in two — the channels go in as a \
                         parameter, the handles come back as the return"
                    ),
                });
            }
        }
    }

    check_inbound_declarations(&mut iface, &mut diags);

    // Bridged impls of bridged traits. Contract checks first (on the written methods), then synthesis: any
    // trait method the impl block does not write — required methods written
    // elsewhere are impossible in Rust, so in practice the trait's
    // default-bodied methods — is cloned onto the concrete type. UFCS
    // dispatch needs only the signature, and the trait declaration carries
    // it. Idempotent (pass 2 sees the clones as written methods).
    {
        let mut new_fns: Vec<Function> = vec![];
        for f in &iface.functions {
            let (Some(parent), Some(tr)) = (&f.parent, &f.trait_impl) else {
                continue;
            };
            let Some(t) = iface.dyn_trait_by_path(tr) else {
                continue; // foreign trait: static dispatch only
            };
            let ctx = format!("`{}::{}` (impl {} for {})", parent, f.name, t.name, parent);
            match iface.member_handle(f).map(|c| c.model) {
                // A data type is not a handle, so it cannot be one of the
                // trait's implementors — and the failure is silent rather than
                // loud without this. The methods would land on the generated
                // data class and work, but the class would not `implements`
                // the trait's Dart interface (that interface re-declares
                // `dispose`/`handleValue`/`isDisposed`, which only a handle
                // has), so the one thing `impl Trait for T` is written for —
                // passing a `T` where a `&dyn Trait` is expected — would not
                // compile in Dart, with nothing here having said why.
                None => diags.push(Diagnostic {
                    code: "FR0026",
                    message: format!(
                        "{ctx}: a bridged trait's implementors are handles, and `{parent}` \
                         crosses by value. The generated Dart class cannot implement \
                         `{}` — that interface carries the handle surface \
                         (`dispose`/`handleValue`/`isDisposed`) — so `{parent}` could \
                         never be passed where a `&dyn {}` is expected. Declare \
                         `{parent}` with a concurrency model matching the trait's \
                         ({:?}), or keep this impl unbridged and expose the operation \
                         as an inherent `#[bridge] impl {parent}` method",
                        t.name, t.name, t.model
                    ),
                }),
                Some(Model::Actor) => diags.push(Diagnostic {
                    code: "FR0025",
                    message: format!(
                        "{ctx}: an actor cannot implement a bridged trait — actor \
                         methods run only on their own executor, which trait-typed \
                         dispatch cannot reach. Implement the trait on a non-actor \
                         type, or keep the impl unbridged"
                    ),
                }),
                Some(model) if model != t.model => diags.push(Diagnostic {
                    code: "FR0026",
                    message: format!(
                        "{ctx}: the concrete type is `{model:?}` but trait `{}` is \
                         declared `{:?}` — a handle crossing at a `&dyn {}` \
                         parameter is acquired under the trait's model, so every \
                         implementor must declare the same model",
                        t.name, t.model, t.name
                    ),
                }),
                _ => {}
            }
            // Native-only is a property of whether the code exists for the
            // target, so a trait and its implementors must agree. Symmetric,
            // like FR0026, because both directions break the web surface: a
            // native-only implementor of a portable trait leaves `_implTag`
            // testing `v is Concrete` against a class that is not emitted,
            // and a native-only trait with a portable implementor leaves that
            // implementor's `implements Trait` naming an absent interface.
            if let Some(c) = iface.member_handle(f) {
                if c.native_only != t.native_only {
                    let (yes, no) = if c.native_only {
                        (parent.as_str(), t.name.as_str())
                    } else {
                        (t.name.as_str(), parent.as_str())
                    };
                    diags.push(Diagnostic {
                        code: "FR0033",
                        message: format!(
                            "{ctx}: `{yes}` is declared `native_only` but `{no}` is not — \
                             the generated web surface would name a type that does not \
                             exist there. Declare `native_only` on both the trait and \
                             every bridged implementor, or on neither — or keep this impl \
                             unbridged"
                        ),
                    });
                }
            }
            if let Some(tm) = iface
                .functions
                .iter()
                .find(|m| m.parent.as_deref() == Some(t.name.as_str()) && m.name == f.name)
            {
                if f.exec != tm.exec || f.on_contention != tm.on_contention {
                    diags.push(Diagnostic {
                        code: "FR0024",
                        message: format!(
                            "{ctx}: execution contract differs from the trait's \
                             declaration ({:?}/{:?} vs {:?}/{:?}). The generated \
                             class implements the trait's Dart interface, so the \
                             member signatures must agree — annotate the impl \
                             method to match `{}::{}`",
                            f.exec, f.on_contention, tm.exec, tm.on_contention, t.name, f.name
                        ),
                    });
                }
            }
        }
        // Synthesis: (concrete, trait) links → missing trait methods.
        let links: HashSet<(String, String)> = iface
            .functions
            .iter()
            .filter_map(|f| match (&f.parent, &f.trait_impl) {
                (Some(p), Some(tr)) => iface
                    .dyn_trait_by_path(tr)
                    .map(|t| (p.clone(), t.name.clone())),
                _ => None,
            })
            .collect();
        let mut links: Vec<_> = links.into_iter().collect();
        links.sort();
        for (concrete, trait_name) in links {
            let trait_path = iface.opaque(&trait_name).unwrap().full_path();
            for tm in &iface.functions {
                if tm.parent.as_deref() != Some(trait_name.as_str()) || tm.is_actor_drop {
                    continue;
                }
                // On the *handle* half specifically. A type declaring two
                // representations may carry an inherent member of the same
                // name on its value half — legal Rust, since E0592 is
                // inherent-vs-inherent — and reading that as "the impl block
                // wrote it" leaves the handle class implementing the trait's
                // Dart interface without one of its members.
                let written = iface.functions.iter().chain(new_fns.iter()).any(|f| {
                    f.parent.as_deref() == Some(concrete.as_str())
                        && f.name == tm.name
                        && iface.member_repr(f) == Some(Repr::Handle)
                });
                if !written {
                    new_fns.push(Function {
                        parent: Some(concrete.clone()),
                        trait_impl: Some(trait_path.clone()),
                        fn_id: 0,
                        // Constructed here rather than parsed, so it has to
                        // carry its own half. Only a handle implements a
                        // bridged trait (FR0026 above), so a dual concrete
                        // type's synthesized method is on the handle half.
                        // Without this it would belong to neither class and
                        // simply not be emitted, leaving the Dart class
                        // missing a member of the interface it implements.
                        parent_repr: duals.contains(&concrete).then_some(Repr::Handle),
                        ..tm.clone()
                    });
                }
            }
        }
        iface.functions.extend(new_fns);
    }

    // Type names must be unique across every bridge file: the emitters resolve
    // a type by name and ignore module_path, so two same-named types in
    // different modules would silently emit duplicate/wrong codec functions.
    {
        // The handle declarations that are *not* exempt from the duplicate
        // rule. Exactly one opaque per dual name is: a second declaration of
        // that name is a third item and still a duplicate, and without the
        // count two `--src` files claiming one module path would exempt them
        // all and the name would never be reported at all.
        let exempt_one_per_dual: Vec<&str> = {
            let mut exempted: HashSet<&str> = HashSet::new();
            iface
                .opaques
                .iter()
                .filter(|d| !(iface.is_dual(&d.name) && exempted.insert(d.name.as_str())))
                .map(|d| d.name.as_str())
                .collect()
        };
        let mut seen_types: HashSet<&str> = HashSet::new();
        // The expansions are deliberately absent and the **templates** are
        // deliberately present: `Page<Item>` is not a name an author could
        // duplicate or shadow with (it is the Rust type, and unique for that
        // reason), while `Page` is, and a template's Dart class collides with
        // `dart:core` exactly as any other declaration's would.
        let type_names = iface
            .class_structs()
            .map(|d| d.name.as_str())
            .chain(iface.class_enums().map(|d| d.name.as_str()))
            // A dual type's handle declaration repeats its value
            // declaration's name on purpose: one `#[bridge(data, locked)]`
            // struct, two declarations. It is exempt from the duplicate rule
            // and from nothing else — including the Dart-name collision the
            // two halves then have, which the minted-name rule below reports
            // like any other. rustc's own E0428 forbids two items of one name
            // in one module, so a pair sharing a module path is necessarily
            // one item; a pair whose module paths differ is two, and falls
            // through to the duplicate report.
            .chain(exempt_one_per_dual.iter().copied())
            .chain(iface.externs.iter().map(|d| d.name.as_str()));
        for name in type_names {
            if !seen_types.insert(name) {
                diags.push(Diagnostic {
                    code: "FR0002",
                    message: format!(
                        "duplicate bridged type name `{name}`; type names must be unique \
                         across all bridge files (the generated codec resolves a type by \
                         name, not by module path)"
                    ),
                });
            }
            // FR0046, three ways a declared name can be a lie. Not a
            // time-mapping rule: `String` has had the first hole and `BigInt`
            // the third since the beginning. They are fixed here because
            // selectable peers add names to the first two sets, and a rule
            // covering only the new ones would have to be re-derived the next
            // time one is added.
            //
            // Each arm states the reason that actually applies to it — an
            // earlier draft used the first message for all of them and
            // overstated two thirds of the cases, which is the sort of wrong
            // that a diagnostic can least afford.
            if PARSER_CLAIMED_TYPE_NAMES.contains(&name) {
                // Claimed on every path: no spelling reaches the declaration.
                diags.push(Diagnostic {
                    code: "FR0046",
                    message: format!(
                        "bridged type `{name}`: the parser resolves that name to a \
                         built-in mapping on every path, so this declaration could \
                         never be reached from a signature. Rename it (e.g. `My{name}`)"
                    ),
                });
            } else if PARSER_CLAIMED_BARE_TYPE_NAMES.contains(&name) {
                // Claimed bare only. The declaration is reachable through a
                // qualified path — but the bare spelling everyone writes
                // silently means the built-in mapping instead, which is the
                // capture this refuses.
                diags.push(Diagnostic {
                    code: "FR0046",
                    message: format!(
                        "bridged type `{name}`: a bare `{name}` in a signature is read \
                         as the built-in time mapping, not as this type, so the \
                         declaration is reachable only through a fully qualified path \
                         and any ordinary use of it would silently mean something else. \
                         Rename it (e.g. `My{name}`)"
                    ),
                });
            } else if DART_SHADOWED_TYPE_NAMES.contains(&name) {
                // Reachable, correctly resolved, and still broken — on the far
                // side. `class DateTime` lands in the same generated library
                // as `DateTime.fromMicrosecondsSinceEpoch(…)`, and Dart
                // resolves the local declaration.
                diags.push(Diagnostic {
                    code: "FR0046",
                    message: format!(
                        "bridged type `{name}`: the generated Dart class would shadow \
                         `{name}` from `dart:core`/`dart:typed_data` for the whole \
                         generated library, including the code the emitters write \
                         against it. Rename it (e.g. `My{name}`)"
                    ),
                });
            }
        }
    }

    let mut functions = std::mem::take(&mut iface.functions);
    // Keyed by the *class* a member lands on, which is the parent and the
    // half — not the parent alone. Two members of one name on one class are a
    // duplicate whatever declared them (rustc allows an inherent method beside
    // a trait method of that name; one Dart class cannot). Two on the two
    // halves of one type are two classes, and neither shadows the other.
    let mut seen_names: HashSet<(Option<String>, Option<Repr>, String)> = HashSet::new();
    let names = DeclaredNames {
        structs: &structs,
        enums: &enums,
        opaques: &opaques,
        dyn_traits: &dyn_traits,
        duals: &duals,
    };
    for f in &mut functions {
        let ctx = match &f.parent {
            Some(p) => format!("`{}::{}`", p, f.name),
            None => format!("`{}`", f.name),
        };
        if !seen_names.insert((f.parent.clone(), iface.member_repr(f), f.name.clone())) {
            diags.push(Diagnostic {
                code: "FR0002",
                message: format!("{ctx}: duplicate bridged function name"),
            });
        }
        // A member whose Dart name collides with the surface its own generated
        // class already has silently shadows it — a Rust `dispose` overrides
        // the generated `dispose()`, so the object is never freed; a Rust
        // `copy_with` overrides `copyWith`, so the one route to a changed
        // value stops working. Reject loudly, and name the surface that
        // actually applies rather than the union of both.
        if let Some(parent) = &f.parent {
            let collision = if iface.member_handle(f).is_some() {
                // `take` is reserved on **every** handle class, not only the
                // ones that emit it. Reserving it conditionally would mean
                // that adding a consuming member to a Rust type turned an
                // unrelated Dart member named `take` — on an app that already
                // compiled — into an error, which is the bridge's own
                // evolution breaking a caller. Reservation costs nothing;
                // emission is what stays conditional.
                // The **Dart** name, not the Rust one: `dart_identifier` is
                // the other way a member lands on `take`, and a collision is
                // a property of the name that is emitted.
                matches!(
                    crate::emit_dart::member_name(f).as_str(),
                    "dispose" | "isDisposed" | "handleValue" | "take"
                )
                .then_some((
                    "the generated handle surface (`dispose`/`isDisposed`/`handleValue`, \
                     and `take`, which every handle class reserves so that a consuming \
                     member added later cannot break an app that already compiled)",
                    "it would silently shadow it (e.g. a `dispose` that never frees the \
                     object)",
                ))
            } else if iface.instance_of(parent).is_some() && f.receiver.is_none() {
                // A `static` on the extension is reached through the
                // extension's own name (`Page$i64.copyWith()`), which nothing
                // on the class can shadow and which is not a member of the
                // Dart type at all. Measured: `dart analyze` accepts it.
                None
            } else {
                data_surface_clash(&iface, parent, &f.name)
            };
            if let Some((what, on_class)) = collision {
                // A member of an **instantiation** does not land on the class:
                // it lands on an `extension` over the instantiation's Dart
                // type, and the consequence there is one of two, neither of
                // them the class's. Both measured with `dart analyze`.
                let consequence = if iface.instance_of(parent).is_none() {
                    on_class
                } else if matches!(
                    crate::emit_dart::dart_name(&f.name).as_str(),
                    "toString" | "hashCode" | "runtimeType" | "noSuchMethod"
                ) {
                    "this member lands on an `extension`, and an extension may not \
                     declare a member `Object` already has at all — the generated Dart \
                     would not compile (`extension_declares_member_of_object`)"
                } else {
                    "this member lands on an `extension` over the instantiation's Dart \
                     type, and Dart resolves a class member in preference to an \
                     extension member, so it would compile and never be called"
                };
                diags.push(Diagnostic {
                    code: "FR0027",
                    message: format!(
                        "{ctx}: method name collides with {what}; {consequence}. Rename \
                         the method"
                    ),
                });
            }
        }
        for p in &mut f.params {
            let pctx = format!("{ctx}, parameter `{}`", p.name);
            resolve(&mut p.ty, &f.generics, &pctx, &mut diags);
        }
        if let Some(ret) = &mut f.ret {
            let rctx = format!("{ctx}, return type");
            resolve(ret, &f.generics, &rctx, &mut diags);
        }
        if let Some(err) = &mut f.err {
            let ectx = format!("{ctx}, error type");
            resolve(err, &f.generics, &ectx, &mut diags);
            // FR0035: an error crosses by value, so it must be a value type.
            // An opaque has a lifecycle — a handle the receiver must dispose —
            // and handing one out on the failure path would make every `catch`
            // a resource-management obligation. An extern is excluded too, to
            // keep the generated exception story to two shapes.
            //
            // A `native_only` error type cannot arise: `native_only` is
            // rejected on data types at parse time, and everything else is
            // already rejected right here.
            match err {
                // A struct or enum, but only one that reaches no handle. A
                // data type *may* hold one (FR0004 is a use-site rule now), and
                // an error payload is the one return path this diagnostic
                // exists to keep handles off: minting on the failure path is
                // exactly the resource-management obligation it refuses.
                Type::Struct(_) | Type::Enum(_) if reaches_opaque(&iface, err) => {
                    diags.push(Diagnostic {
                        code: "FR0035",
                        message: format!(
                            "{ctx}: `{}` cannot be a typed error — it reaches an opaque \
                             type through its fields, so failing would mint a handle the \
                             `catch` block must dispose. That is a resource-management \
                             obligation on the one path a caller is least likely to get \
                             right, which is why an error crosses as plain data. Return \
                             the handle from the success path and keep the error to \
                             values",
                            error_type_label(err)
                        ),
                    });
                }
                Type::Struct(_) | Type::Enum(_) => {}
                // FR0003 (unknown) or FR0056 (a generic parameter) already
                // fired; saying it twice helps nobody.
                Type::Named(_) => {}
                Type::Opaque(n) => diags.push(Diagnostic {
                    code: "FR0035",
                    message: format!(
                        "{ctx}: `{n}` cannot be a typed error. It is an opaque type, so \
                         it crosses as a handle its receiver must dispose — which would \
                         make every `catch` a resource-management obligation, on the one \
                         path a caller is least likely to get right. A typed error must \
                         be a struct or enum, which crosses by value. To cross as a \
                         message instead, return `Result<T, String>` or \
                         `anyhow::Result<T>`"
                    ),
                }),
                other => diags.push(Diagnostic {
                    code: "FR0035",
                    message: format!(
                        "{ctx}: `{}` cannot be a typed error. The error of a bridged \
                         `Result` crosses by value, so it must be a struct or enum \
                         declared in this interface. To cross as a message instead, \
                         return `Result<T, String>` or `anyhow::Result<T>`",
                        error_type_label(other)
                    ),
                }),
            }
        }
        // `iface.functions` is taken out for this loop, but the walk only
        // needs `structs`/`enums`, which are already restored.
        check_function(f, &ctx, &iface, &names, caps, &mut diags);
    }
    iface.functions = functions;

    // FR0065 — a `dart_identifier` has to be a Dart identifier. It is written
    // as a free string, so nothing but this stops `"2fast"` or `"class"` or
    // `""` reaching the emitter and failing inside generated code, which is the
    // failure the annotation exists to move upstream in the first place.
    {
        let mut named: Vec<(&str, &Option<String>)> = vec![];
        for d in iface.class_structs() {
            named.push((&d.name, &d.dart_identifier));
            for f in &d.fields {
                named.push((&f.name, &f.dart_identifier));
            }
        }
        for d in iface.class_enums() {
            named.push((&d.name, &d.dart_identifier));
            for v in &d.variants {
                named.push((&v.name, &v.dart_identifier));
                for f in &v.fields {
                    named.push((&f.name, &f.dart_identifier));
                }
            }
        }
        for d in &iface.opaques {
            named.push((&d.name, &d.dart_identifier));
        }
        for f in &iface.functions {
            named.push((&f.name, &f.dart_identifier));
        }
        for (rust, ident) in named {
            let Some(ident) = ident else { continue };
            let ok = !ident.is_empty()
                && !ident.starts_with(|c: char| c.is_ascii_digit())
                && ident.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !DART_RESERVED.contains(&ident.as_str());
            if !ok {
                diags.push(Diagnostic {
                    code: "FR0065",
                    message: format!(
                        "`{rust}`: `dart_identifier = \"{ident}\"` is not a name this \
                         bridge will mint. It must be non-empty, start with a letter or \
                         `_`, contain only those and digits, and not be a Dart reserved \
                         word. Dart allows `$` in an identifier and this does not: the \
                         generated stems own it — an instantiation's members land on \
                         `extension Page$Item on Page<Item>`, whose name is prefix \
                         notation over the type arguments — so a written `$` could be \
                         spelled like one of those and collide inside generated code"
                    ),
                });
            }
        }
    }

    // FR0002 — two Rust items that mint one Dart name.
    //
    // The derivation cannot avoid this and should not try: `to_lower_camel_case`
    // collapses `word_count` and `wordCount`, and every compound name
    // (`{Enum}{Variant}`, the fake's `parent` + `Member`) loses the boundary
    // between its halves. Which of two colliding items should move, and to what,
    // is information only the author has — so this reports both and
    // `dart_identifier` carries the decision.
    //
    // Read from `emit_dart::minted_names`, which lives beside the writer sites:
    // re-deriving the names here would drift from the emitters on the next
    // declaration kind, and the drift would be silent — this would pass and the
    // generated Dart would not compile.
    //
    // Staged behind everything above rather than batched with it, because a
    // name is only worth comparing once the thing that mints it is known to be
    // legal: an error type this pass has already refused has no Dart class, and
    // asking for its name would reach an `unreachable!` in the emitter. The
    // author fixes those first and sees any collision on the next run.
    if diags.is_empty() {
        let minted = crate::emit_dart::minted_names(&iface);
        // Two class *names* that collide make one Dart class out of two, and
        // every member of both then shares that scope — so their members
        // collide as a consequence, not on their own. Report the cause; the one
        // rename that fixes it fixes them all.
        let merged: HashSet<String> = {
            let mut by_name: HashMap<&str, usize> = HashMap::new();
            for m in &minted {
                if m.scope == crate::emit_dart::Scope::Library {
                    *by_name.entry(m.name.as_str()).or_default() += 1;
                }
            }
            by_name
                .into_iter()
                .filter(|(_, n)| *n > 1)
                .map(|(name, _)| name.to_string())
                .collect()
        };
        // The Dart types two or more instantiations of one template collapse
        // onto, and which instantiations those are. `dart_type` is not
        // injective — nine Rust integers are `int`, both floats are `double`,
        // `char` and `String` are `String`, each time peer shares its Dart
        // peer — so `Page<i32>` and `Page<i64>` are two instantiations, two
        // codecs and one `Page<int>`. That is harmless everywhere the codec is
        // chosen by a written position, and it is not harmless for a member:
        // an extension member is resolved from the receiver's static type, so
        // two of them under one name leave every call site ambiguous. The
        // members are minted into that scope (`emit_dart::member_class`), so
        // the collision is FR0002's; this only supplies the true reason.
        let collapsed: HashMap<String, Vec<String>> = {
            let mut by_type: HashMap<String, Vec<String>> = HashMap::new();
            for name in iface
                .structs
                .iter()
                .map(|s| &s.name)
                .chain(iface.enums.iter().map(|e| &e.name))
            {
                if iface.instance_of(name).is_some() {
                    by_type
                        .entry(crate::emit_dart::data_dart_type(&iface, name))
                        .or_default()
                        .push(name.clone());
                }
            }
            by_type.retain(|_, v| v.len() > 1);
            by_type
        };
        // Dart names that two **instantiations of one template** both mint —
        // the extension each one's members land on, and the crate fake's
        // answering method. The stem is prefix notation over the type
        // arguments, and its built-in tokens (`List`, `Set`, `Opt`, `Bytes`,
        // `Tup2`, …) are not reserved against an author's own class names, so
        // a struct called `Bytes` gives `Page<Bytes>` and `Page<Vec<u8>>` one
        // stem. Computed with the emitters' own helpers, never re-derived, for
        // the reason `minted_names` states.
        let stem_clash: HashMap<String, Vec<String>> = {
            let mut by_name: HashMap<String, Vec<String>> = HashMap::new();
            let instances = || {
                iface
                    .structs
                    .iter()
                    .map(|s| &s.name)
                    .chain(iface.enums.iter().map(|e| &e.name))
                    .filter(|n| iface.instance_of(n).is_some())
            };
            for name in instances() {
                if iface.members(name, Repr::Data).next().is_none() {
                    continue;
                }
                by_name
                    .entry(crate::emit_dart::instance_extension(&iface, name))
                    .or_default()
                    .push(name.clone());
            }
            for f in &iface.functions {
                let Some(parent) = f.parent.as_deref() else { continue };
                if iface.instance_of(parent).is_none() {
                    continue;
                }
                by_name
                    .entry(crate::emit_dart_fake::free_name(&iface, f))
                    .or_default()
                    .push(parent.to_string());
            }
            for v in by_name.values_mut() {
                v.sort_unstable();
                v.dedup();
            }
            by_name.retain(|_, v| v.len() > 1);
            by_name
        };
        let mut seen: HashMap<(crate::emit_dart::Scope, String), (String, Option<String>)> =
            HashMap::new();
        // And one *pair* is one diagnostic however many scopes it meets in: two
        // free functions collapse in the library scope and again on the crate
        // fake, and the author renames once either way.
        let mut reported: HashSet<(String, String)> = HashSet::new();
        for m in minted {
            if matches!(&m.scope, crate::emit_dart::Scope::Class(c) if merged.contains(c.as_str()))
            {
                continue;
            }
            let key = (m.scope.clone(), m.name.clone());
            match seen.get(&key) {
                Some((first, _)) if !reported.insert(pair_key(first, &m.origin)) => {}
                Some((first, first_decl)) => {
                    // The two halves of one type meeting on one class name is
                    // not two Rust names colliding: the names are the same
                    // name, on purpose, and the flat `dart_identifier` the
                    // general message prescribes is FR0068 here. Say which
                    // half to name instead.
                    // Keyed on the Rust type both names were minted from, not
                    // on the name they collided at: the two halves may have
                    // been given the *same* explicit name, which is still one
                    // type needing one of them moved.
                    let same_decl = matches!((first_decl, &m.decl), (Some(a), Some(b)) if a == b);
                    // Two instantiations meeting on one Dart type. Not a
                    // naming accident, so `dart_identifier` is not the answer:
                    // under `impl<T>` the two members are ONE Rust item, and
                    // there is nothing to rename.
                    let instances = match &m.scope {
                        crate::emit_dart::Scope::Class(c) => collapsed.get(c.as_str()),
                        crate::emit_dart::Scope::Library => None,
                    };
                    let how = if let Some(insts) = stem_clash.get(&m.name) {
                        format!(
                            "`{}` are one Dart name here. The name an instantiation's \
                             members land under is prefix notation over its type \
                             arguments (`Page$List$Item` = `Page<Vec<Item>>`), and its \
                             built-in tokens — `List`, `Set`, `Map`, `Opt`, `Bytes`, \
                             `Tup{{n}}`, `Array{{n}}`, the primitives — are not reserved \
                             against the Dart class names your own declarations mint, so \
                             one of yours is spelled like one of them. Rename the \
                             offending declaration's Dart class with \
                             `#[bridge(dart_identifier = \"…\")]` on IT, not on the \
                             instantiation: the stem is built from the class names, so it \
                             follows",
                            insts.join("` and `")
                        )
                    } else if let Some(insts) = instances {
                        format!(
                            "`{}` are one Dart type: the map from a type argument to a \
                             Dart type is not injective (nine Rust integers are `int`, \
                             both floats are `double`, `char` and `String` are `String`, \
                             each time peer shares its Dart peer). Each instantiation's \
                             members land on their own `extension`, Dart resolves an \
                             extension member from the receiver's static type, and here \
                             that type does not say which instantiation — so neither is \
                             more specific and every call is refused. A \
                             `dart_identifier` cannot settle it: under `impl<T>` these \
                             are one Rust item. Write one `impl` per instantiation and \
                             give the members different names, bind the parameter to \
                             types Dart tells apart, or write a free function per \
                             instantiation",
                            insts.join("` and `")
                        )
                    } else if same_decl && m.decl.as_deref().is_some_and(|d| iface.is_dual(d)) {
                        "One Rust type, two Dart classes: name the one that moves, \
                         inside the keyword whose class it is — \
                         `#[bridge(data(dart_identifier = \"…\"), locked)]`, or the \
                         same on the handle keyword"
                            .to_string()
                    } else {
                        "The Rust names are fine; the mapping to Dart is not injective \
                         — it lower-camels a name and joins compound names by \
                         concatenation, and neither can be undone. Add \
                         `#[bridge(dart_identifier = \"…\")]` to either one to say \
                         which moves, and where"
                            .to_string()
                    };
                    diags.push(Diagnostic {
                        code: "FR0002",
                        message: format!(
                            "{} and {first} both land in Dart as `{}`{}. {how}",
                            m.origin,
                            m.name,
                            match &m.scope {
                                crate::emit_dart::Scope::Library => String::new(),
                                crate::emit_dart::Scope::Class(c) => format!(" on `{c}`"),
                            }
                        ),
                    })
                }
                None => {
                    seen.insert(key, (m.origin, m.decl));
                }
            }
        }
    }

    if !diags.is_empty() {
        return Err(diags);
    }
    Ok(iface)
}

// ------------------------------------------------------ generic expansion --

/// Turn every fully-applied use of a generic data template into a synthetic
/// non-generic declaration, and rewrite the use to name it.
///
/// **Why expansion rather than generic codecs.** A generic codec parameterized
/// on its element codecs is the obvious alternative, and the tree makes it the
/// expensive one: almost every rule the checker enforces is a fact about an
/// *instantiation*, not about a template. `Page<Doc>` is return-only and
/// `Page<i64>` is not (FR0004); `Page<NativeOnlyThing>` is absent from the web
/// surface and `Page<i64>` is not; one gets an owned consuming encoder and no
/// decoder, the other a borrowing encoder and a decoder. So the substitution
/// has to happen before the rules run — or every rule, and `walk_type_graph`
/// with them, would have to carry a substitution environment. Expansion does it
/// once, here, and every rule below reads ordinary declarations.
///
/// The Dart side does not monomorphize: Dart has generics, so ONE
/// `class Page<T>` comes from the template and `Page<Item>` is a type rather
/// than a name. Only the codecs are per-instantiation, which is what the two
/// sides disagreeing about handles and typed lists requires anyway.
///
/// **Termination.** An instantiation is registered under its name *before* its
/// fields are expanded, so an ordinary recursive generic (`struct Node<T> {
/// next: Option<Box<Node<T>>> }`) closes on the memo. What does not terminate
/// is polymorphic recursion, and [`check_instantiation_growth`] decides that
/// before anything is registered (FR0075).
fn expand_generics(iface: &mut Interface, diags: &mut Vec<Diagnostic>) {
    // Refusals on the templates themselves, before anything is instantiated:
    // an expansion of a declaration that cannot cross would report the same
    // fact once per use site instead of once.
    for (kind, name, consts, defaulted, dart_interface) in iface
        .generic_structs
        .iter()
        .map(|s| ("struct", &s.name, &s.const_generics, &s.defaulted_generics, s.dart_interface))
        .chain(iface.generic_enums.iter().map(|e| {
            ("enum", &e.name, &e.const_generics, &e.defaulted_generics, false)
        }))
    {
        if !consts.is_empty() {
            diags.push(Diagnostic {
                code: "FR0056",
                message: format!(
                    "{kind} `{name}`: `<const {}>` — a const parameter is not supported. \
                     A const shapes the wire (it is a length, a capacity, an array bound), \
                     and the Dart class one generic declaration emits carries type \
                     arguments only, so an instantiation could not be written on the far \
                     side. Declare a non-generic struct per length, or take the length as \
                     a field",
                    consts.join(", const ")
                ),
            });
        }
        if !defaulted.is_empty() {
            diags.push(Diagnostic {
                code: "FR0056",
                message: format!(
                    "{kind} `{name}`: `<{}>` declares a default. The instantiations this \
                     bridge emits are exactly the ones the signatures write out, so a \
                     bare `{name}` in a signature would have to mean the default — and it \
                     does not: it is an unapplied template (FR0073). Drop the default and \
                     write the argument at each use",
                    defaulted.join(", ")
                ),
            });
        }
        if dart_interface {
            diags.push(Diagnostic {
                code: "FR0076",
                message: format!(
                    "struct `{name}`: `dart_interface` and type parameters do not \
                     combine. Every field of a dart_interface is one *method* of the \
                     generated Dart interface, and a top-level tuple argument spreads into \
                     positional parameters (`DartFunction<(i64, String), bool>` binds \
                     `t(a, b)`) — so the method's arity would depend on the argument bound \
                     to the parameter, while one generic interface declares one signature. \
                     Keep it as a plain `#[bridge(data)]` struct of handle fields, which \
                     crosses identically, or declare a non-generic interface per \
                     instantiation"
                ),
            });
        }
    }
    check_impl_targets(iface, diags);
    if !diags.is_empty() {
        return;
    }

    // A template's own fields name its parameters. Convert those to
    // `Type::Param` once — idempotently, so the second `check_pass` is a no-op
    // — which is what tells the substitution and the Dart emitter apart from an
    // unknown type name.
    let mut templates_s = std::mem::take(&mut iface.generic_structs);
    for t in &mut templates_s {
        let params = t.generics.clone();
        for f in &mut t.fields {
            bind_params(&mut f.ty, &params);
        }
    }
    iface.generic_structs = templates_s;
    let mut templates_e = std::mem::take(&mut iface.generic_enums);
    for t in &mut templates_e {
        let params = t.generics.clone();
        for v in &mut t.variants {
            for f in &mut v.fields {
                bind_params(&mut f.ty, &params);
            }
        }
    }
    iface.generic_enums = templates_e;
    // The block parameters a member of a generic `impl` is written in terms
    // of, before anything reads its signature — the same move the templates
    // got above, and idempotent for the same reason.
    let mut functions = std::mem::take(&mut iface.functions);
    for f in &mut functions {
        if f.parent_generics.is_empty() {
            continue;
        }
        let params = f.parent_generics.clone();
        for a in &mut f.parent_args {
            bind_params(a, &params);
        }
        for ty in f.signature_types_mut() {
            bind_params(ty, &params);
        }
    }
    iface.functions = functions;

    // Whether there is a finite surface to emit at all, before anything is
    // registered — the expansion below has no way to find out on its own
    // without running forever.
    check_instantiation_growth(iface, diags);
    if !diags.is_empty() {
        return;
    }

    let mut ex = Expansion {
        struct_templates: iface.generic_structs.clone(),
        enum_templates: iface.generic_enums.clone(),
        structs: iface.structs.iter().map(|s| s.name.clone()).collect(),
        enums: iface.enums.iter().map(|e| e.name.clone()).collect(),
        models: iface
            .opaques
            .iter()
            .map(|o| (o.name.clone(), o.model))
            .collect(),
        externs: iface.externs.iter().map(|e| e.name.clone()).collect(),
        done: Vec::new(),
        budget: EXPANSION_BUDGET,
        out_structs: Vec::new(),
        out_enums: Vec::new(),
    };

    // Every position a *closed* application can be written. A template's own
    // fields are deliberately absent: an application there is open (it names
    // the template's parameters) and is reached by substitution instead.
    let mut structs = std::mem::take(&mut iface.structs);
    for s in &mut structs {
        for f in &mut s.fields {
            let ctx = format!("struct `{}`, field `{}`", s.name, f.name);
            ex.rewrite(&mut f.ty, &ctx, diags);
        }
    }
    iface.structs = structs;
    let mut enums = std::mem::take(&mut iface.enums);
    for e in &mut enums {
        for v in &mut e.variants {
            for f in &mut v.fields {
                let ctx = format!("enum `{}`, variant `{}`, field `{}`", e.name, v.name, f.name);
                ex.rewrite(&mut f.ty, &ctx, diags);
            }
        }
    }
    iface.enums = enums;
    let mut functions = std::mem::take(&mut iface.functions);
    for f in &mut functions {
        // A member of a **generic** block has an open signature: its
        // applications name the block's parameters, and are reached by
        // substitution below rather than registered here. Its concrete
        // siblings are ordinary: the self type is one more closed application,
        // registered like any other, and the member's `parent` becomes the
        // synthetic declaration's name.
        if !f.parent_generics.is_empty() {
            continue;
        }
        if !f.parent_args.is_empty() {
            let parent = f.parent.clone().expect("arguments imply a self type");
            let ctx = format!("`impl {parent}<…>`, member `{}`", f.name);
            let mut ty = Type::App(parent, std::mem::take(&mut f.parent_args));
            ex.rewrite(&mut ty, &ctx, diags);
            if let Type::Named(display) = ty {
                f.parent = Some(display);
            }
        }
        let who = match &f.parent {
            Some(p) => format!("`{}::{}`", p, f.name),
            None => format!("`{}`", f.name),
        };
        for p in &mut f.params {
            let ctx = format!("{who}, parameter `{}`", p.name);
            ex.rewrite(&mut p.ty, &ctx, diags);
        }
        if let Some(r) = &mut f.ret {
            let ctx = format!("{who}, return type");
            ex.rewrite(r, &ctx, diags);
        }
        if let Some(e) = &mut f.err {
            let ctx = format!("{who}, error type");
            ex.rewrite(e, &ctx, diags);
        }
    }
    expand_impl_members(&mut functions, &mut ex, diags);
    iface.functions = functions;

    iface.structs.extend(ex.out_structs);
    iface.enums.extend(ex.out_enums);
}

/// Materialize each member of a generic `impl` block onto every instantiation
/// of its template, in place of the open member the author wrote.
///
/// One `Function` — and so one `fn_id` — per (member, instantiation). That is
/// what makes the Dart side possible at all: the member lands on an
/// `extension` over the instantiation's Dart *type*, which Dart resolves
/// statically, so each call site already knows which id it is dispatching.
///
/// A **fixpoint**, because a materialized signature is a closed position like
/// any other and may name an instantiation nothing else did — including one of
/// its own template, which is then materialized in its turn. It terminates
/// because [`check_impl_targets`] has already refused the shapes that would
/// grow an argument: every argument of every instantiation reached from here
/// is either a type written closed in the source or an argument of an
/// instantiation the closed expansion already found, and both sets are finite
/// while the templates and their arities are fixed.
fn expand_impl_members(
    functions: &mut Vec<Function>,
    ex: &mut Expansion,
    diags: &mut Vec<Diagnostic>,
) {
    if functions.iter().all(|f| f.parent_generics.is_empty()) {
        return;
    }
    // Source index → the members it expanded to, in instantiation-registration
    // order. Order is the whole point: `finalize` assigns `fn_id` by position,
    // so it has to be a function of the interface and nothing else.
    let mut expanded: Vec<(usize, Vec<Function>)> = functions
        .iter()
        .enumerate()
        .filter(|(_, f)| !f.parent_generics.is_empty())
        .map(|(i, _)| (i, vec![]))
        .collect();
    let mut covered: HashSet<(usize, String)> = HashSet::new();
    loop {
        let mut grew = false;
        // A snapshot: `rewrite` below appends to `ex.done`, and anything it
        // adds is picked up on the next round.
        let insts = ex.done.clone();
        for (i, out) in &mut expanded {
            let src = &functions[*i];
            let template = src.parent.clone().expect("a member has a parent");
            for display in &insts {
                let Some(args) = ex.instance_args(display, &template) else {
                    continue;
                };
                if !covered.insert((*i, display.clone())) {
                    continue;
                }
                // Positional: `check_impl_targets` has already required the
                // self type to be the block's parameters, one each.
                let bindings: Vec<(String, Type)> = src
                    .parent_args
                    .iter()
                    .zip(&args)
                    .map(|(p, a)| match p {
                        Type::Param(n) => (n.clone(), a.clone()),
                        _ => unreachable!("a generic block's self type is its parameters"),
                    })
                    .collect();
                let mut g = src.clone();
                g.parent = Some(display.clone());
                g.parent_args = vec![];
                g.parent_generics = vec![];
                let who = format!("`{display}::{}`", g.name);
                for ty in g.signature_types_mut() {
                    *ty = substitute(ty, &bindings);
                }
                for p in &mut g.params {
                    let ctx = format!("{who}, parameter `{}`", p.name);
                    ex.rewrite(&mut p.ty, &ctx, diags);
                }
                if let Some(r) = &mut g.ret {
                    ex.rewrite(r, &format!("{who}, return type"), diags);
                }
                if let Some(e) = &mut g.err {
                    ex.rewrite(e, &format!("{who}, error type"), diags);
                }
                out.push(g);
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    for (i, out) in &expanded {
        if !out.is_empty() {
            continue;
        }
        let f = &functions[*i];
        let template = f.parent.as_deref().unwrap_or("");
        diags.push(Diagnostic {
            code: "FR0056",
            message: format!(
                "`impl<{0}> {template}<{0}>`, member `{1}`: `{template}` has no \
                 instantiation in this interface, so this block bridges nothing. A \
                 member of a generic impl is generated onto every instantiation the \
                 signatures derive, and the set is empty — the annotation would emit no \
                 Dart and no dispatch id, which is the silent omission `#[bridge]` \
                 exists to prevent. Write a signature that names `{template}<…>`, or \
                 drop the `#[bridge]`",
                f.parent_generics.join(", "),
                f.name
            ),
        });
    }
    // Splice, so declaration order is preserved and each open member is
    // replaced exactly where it stood.
    let mut by_index: HashMap<usize, Vec<Function>> = expanded.into_iter().collect();
    let mut out: Vec<Function> = Vec::with_capacity(functions.len());
    for (i, f) in std::mem::take(functions).into_iter().enumerate() {
        match by_index.remove(&i) {
            Some(fs) => out.extend(fs),
            None => out.push(f),
        }
    }
    *functions = out;
}

/// One node of [`check_instantiation_growth`]'s graph: a template's name and
/// one of its parameter positions.
type GrowthNode = (String, usize);

/// The number of instantiations [`Expansion`] will register before it gives
/// up. See [`Expansion::budget`] — a liveness backstop, not a limit.
const EXPANSION_BUDGET: usize = 4096;

/// FR0075 — decide, before anything is expanded, whether the set of
/// instantiations this interface asks for is finite.
///
/// **The graph.** One node per (template, parameter position). An application
/// `Q<a0, …, an>` written inside `P`'s own fields adds an edge
/// `(P, i) -> (Q, j)` for every parameter `Ti` of `P` that occurs in `aj`: the
/// argument bound to `(Q, j)` is built from the one bound to `(P, i)`. The
/// edge **grows** when `Ti` occurs strictly below the top of `aj` — `Vec<Ti>`,
/// `(Ti, X)`, `R<Ti>` — because then it is built *larger*. A closed argument
/// (`Q<i64>`) is built from nothing and adds no edge at all.
///
/// A generic `impl` block's member signatures are edges of exactly the same
/// kind, and are read here for that reason: every instantiation of `P` carries
/// every member, so `impl<T> P<T> { fn f(&self) -> Q<Vec<T>> }` asks for
/// `Q<Vec<A>>` at every `P<A>` precisely as a field would. rustc does not
/// catch that one — a generic fn is monomorphized on demand — so it is the
/// bridge's own claim and the bridge's own rule.
///
/// **The criterion.** The set is infinite iff some cycle contains a growing
/// edge: following that cycle once returns to a node with a strictly larger
/// argument, so the walk never closes. A cycle with no growing edge permutes a
/// bounded set of arguments and terminates, which is why
/// `struct Chain<T> { next: Option<Box<Chain<T>>> }` and
/// `impl<A, B> Pair<A, B> { fn flip(self) -> Pair<B, A> }` are ordinary.
///
/// It reasons over the **declarations**, not over the order a walk reaches
/// them, so it answers the same for a source file whose items are reordered.
/// When it cannot see a head at all — an unknown template, or the wrong arity
/// — it adds no edge and says nothing: that shape has its own diagnostic
/// (FR0073) from the expansion, which registers nothing for it.
fn check_instantiation_growth(iface: &Interface, diags: &mut Vec<Diagnostic>) {
    struct Edge {
        from: GrowthNode,
        to: GrowthNode,
        grows: bool,
        ctx: String,
        wrote: String,
    }
    let arity = |name: &str| iface.template_params(name).map(|p| p.len());
    let mut edges: Vec<Edge> = vec![];

    // Every position a template's parameters can be written into an
    // application: its own fields, and the members of a generic `impl` on it.
    let mut sites: Vec<(&str, Vec<String>, String, Vec<&Type>)> = vec![];
    for s in &iface.generic_structs {
        for f in &s.fields {
            sites.push((
                &s.name,
                s.generics.clone(),
                format!("struct `{}`, field `{}`", s.name, f.name),
                vec![&f.ty],
            ));
        }
    }
    for e in &iface.generic_enums {
        for v in &e.variants {
            for f in &v.fields {
                sites.push((
                    &e.name,
                    e.generics.clone(),
                    format!("enum `{}`, variant `{}`, field `{}`", e.name, v.name, f.name),
                    vec![&f.ty],
                ));
            }
        }
    }
    for f in &iface.functions {
        if f.parent_generics.is_empty() {
            continue;
        }
        let Some(parent) = f.parent.as_deref() else {
            continue;
        };
        // The block's parameter names in the template's positional order,
        // which `check_impl_targets` has already required the self type to be.
        let names: Vec<String> = f
            .parent_args
            .iter()
            .map(|a| match a {
                Type::Param(n) | Type::Named(n) => n.clone(),
                _ => String::new(),
            })
            .collect();
        sites.push((
            parent,
            names,
            format!("`impl<…> {parent}<…>`, member `{}`", f.name),
            f.signature_types().collect(),
        ));
    }

    for (owner, params, ctx, tys) in &sites {
        for ty in tys {
            let mut apps: Vec<(&String, &Vec<Type>)> = vec![];
            ty.walk(&mut |t| {
                if let Type::App(head, args) = t {
                    apps.push((head, args));
                }
            });
            for (head, args) in apps {
                if arity(head) != Some(args.len()) {
                    continue;
                }
                for (j, arg) in args.iter().enumerate() {
                    // `Box` and a representation marker are erased before an
                    // instantiation's identity is formed, so an argument
                    // written through either is the type it names.
                    let mut top = arg;
                    while let Type::Boxed(t) | Type::Claimed(_, t) = top {
                        top = t;
                    }
                    for (i, param) in params.iter().enumerate() {
                        if param.is_empty() || !mentions_param(arg, param) {
                            continue;
                        }
                        let bare = matches!(top, Type::Param(n) | Type::Named(n) if n == param);
                        edges.push(Edge {
                            from: (owner.to_string(), i),
                            to: (head.clone(), j),
                            grows: !bare,
                            ctx: ctx.clone(),
                            wrote: Type::App(head.clone(), args.clone()).rust_display(),
                        });
                    }
                }
            }
        }
    }

    // A growing edge whose head can reach its tail again closes a growing
    // cycle. Reported once — the first in declaration order — because the
    // author's fix is to break the cycle, and every node on it names the same
    // cycle.
    let adjacency: Vec<(GrowthNode, GrowthNode)> =
        edges.iter().map(|e| (e.from.clone(), e.to.clone())).collect();
    for e in edges.iter().filter(|e| e.grows) {
        if !reaches(&adjacency, &e.to, &e.from) {
            continue;
        }
        diags.push(Diagnostic {
            code: "FR0075",
            message: format!(
                "{}: `{}` writes `{}` under a constructor, and `{}` leads back to \
                 `{}` — so each instantiation asks for one with a strictly larger \
                 argument and the set is infinite, each member of it a distinct wire \
                 shape and a distinct codec. Bind the argument to a closed type here, \
                 or break the cycle. (A generic that recurs at the *same* argument — \
                 `struct N<T> {{ next: Option<Box<N<T>>> }}` — is finite and is the \
                 ordinary recursive shape.)",
                e.ctx,
                e.from.0,
                e.wrote,
                e.to.0,
                e.from.0,
            ),
        });
        return;
    }
}

/// True when `param` is named anywhere inside `ty`. Both spellings, because
/// this runs where a template's own fields are already bound to
/// [`Type::Param`] and a member's signature may still be `Type::Named`.
fn mentions_param(ty: &Type, param: &str) -> bool {
    let mut found = false;
    ty.walk(&mut |t| {
        if matches!(t, Type::Param(n) | Type::Named(n) if n == param) {
            found = true;
        }
    });
    found
}

/// Reachability over an adjacency list. Growth is not part of it: a cycle is a
/// cycle whether or not its other edges grow, and the growing edge is chosen
/// by the caller.
fn reaches(adjacency: &[(GrowthNode, GrowthNode)], from: &GrowthNode, to: &GrowthNode) -> bool {
    let mut seen: HashSet<&GrowthNode> = HashSet::new();
    let mut queue = vec![from];
    while let Some(n) = queue.pop() {
        if n == to {
            return true;
        }
        if !seen.insert(n) {
            continue;
        }
        queue.extend(adjacency.iter().filter(|(t, _)| t == n).map(|(_, h)| h));
    }
    false
}

/// What a `#[bridge] impl` may name as its self type, once generic data
/// templates are in the picture.
///
/// Three shapes are accepted and one of them is new. `impl Point` is the
/// ordinary block. `impl Page<Item>` bridges its members onto that one
/// instantiation, and registers it. `impl<T> Page<T>` bridges each member onto
/// **every** instantiation of `Page` the signatures derive — one `Function`,
/// and so one dispatch id, per (member, instantiation), because that is what
/// the wire has: a distinct receiver codec and a distinct Rust
/// monomorphization each.
///
/// A **bound** on a block parameter (`impl<T: Display> Page<T>`) is read and
/// dropped, not refused. The bridge cannot evaluate a trait bound, and a
/// template may be declared with one (`struct Page<T: Clone>`), which every
/// instantiation satisfies by construction — the author's own signatures
/// compiled. So the member is generated for every instantiation and rustc
/// rejects the glue for one that does not satisfy the bound, which is
/// compile-time and sound. What is unusual, and worth knowing when that error
/// arrives, is that the claim rustc is rejecting was made by the bridge and
/// not by the author; the remedy is a per-instantiation `impl`.
fn check_impl_targets(iface: &Interface, diags: &mut Vec<Diagnostic>) {
    for f in &iface.functions {
        let Some(parent) = f.parent.as_deref() else {
            continue;
        };
        let params = iface.template_params(parent);
        // `#[bridge] impl Page { … }` — the bare template name. Not a
        // dispatch question: an unapplied template is not a type in Rust
        // either, which is FR0073's reason one position over.
        if params.is_some() && f.parent_args.is_empty() {
            diags.push(Diagnostic {
                code: "FR0056",
                message: format!(
                    "`{parent}::{}`: `{parent}` is a generic data type, and a bare \
                     `{parent}` is not a type — it is a template with parameters still \
                     to bind, so there is nothing for an `impl` block to be about. Write \
                     the self type applied: `impl {parent}<Item>` bridges its members \
                     onto that instantiation, `impl<T> {parent}<T>` onto every \
                     instantiation the signatures derive",
                    f.name
                ),
            });
            continue;
        }
        if f.parent_args.is_empty() {
            continue;
        }
        // Arguments on something that takes none. The message names what the
        // head actually is, because that is the whole of the answer.
        let Some(params) = params else {
            let what = if iface.opaque(parent).is_some() {
                "a handle type, which cannot be generic (FR0056): its drop and finalize \
                 exports name the Rust type absolutely"
            } else if iface.extern_decl(parent).is_some() {
                "a `bytes(...)` external type, which cannot be generic (FR0056): its \
                 Dart peer is one named class from one import"
            } else {
                "declared with no type parameters"
            };
            diags.push(Diagnostic {
                code: "FR0056",
                message: format!(
                    "`#[bridge] impl {parent}<…>`, member `{}`: `{parent}` is {what}. \
                     Write `impl {parent}`",
                    f.name
                ),
            });
            continue;
        };
        if f.parent_generics.is_empty() {
            // Concrete: the self type is one more closed application, and
            // `expand_generics` registers it exactly where it registers the
            // ones in signatures — arity and unknown names included (FR0073).
            continue;
        }
        // Generic. The self type must be the template applied to the block's
        // own parameters, one each — `impl<T> Page<T>`, `impl<A, B> Pair<B, A>`.
        //
        // A self type that *matches* a subset (`impl<T> Page<Vec<T>>`,
        // `impl<T> Pair<T, i64>`) is legal Rust and decidable, and is refused
        // because which instantiations it selects is a unification the
        // expansion does not do. That is a "not implemented", not a soundness
        // claim, and it is said as one.
        let mut bound: Vec<&str> = vec![];
        let plain = f.parent_args.iter().all(|a| match a {
            Type::Named(n) if f.parent_generics.iter().any(|p| p == n) => {
                let fresh = !bound.contains(&n.as_str());
                bound.push(n);
                fresh
            }
            _ => false,
        });
        // A parameter the self type does not write gets no rule: rustc refuses
        // that impl on the author's own source (E0207, an unconstrained type
        // parameter), and a second voice saying it later would only be later.
        if !plain {
            diags.push(Diagnostic {
                code: "FR0056",
                message: format!(
                    "`#[bridge] impl<{0}> {parent}<{1}>`, member `{2}`: the self type of \
                     a generic bridged impl must be `{parent}` applied to the block's own \
                     parameters, one each — `impl<{0}> {parent}<{0}>`, or any permutation \
                     of them. Selecting a *subset* of the instantiations by matching — a \
                     nested argument (`{parent}<Vec<T>>`), a concrete one \
                     (`{parent}<T, i64>`), a parameter written twice — is legal Rust and \
                     is not implemented here: the expansion binds the block's parameters \
                     positionally and does not unify. Write one `impl {parent}<…>` per \
                     instantiation you mean",
                    f.parent_generics.join(", "),
                    f.parent_args
                        .iter()
                        .map(|a| a.rust_display())
                        .collect::<Vec<_>>()
                        .join(", "),
                    f.name
                ),
            });
            continue;
        }
        if f.parent_args.len() != params.len() {
            diags.push(Diagnostic {
                code: "FR0073",
                message: format!(
                    "`#[bridge] impl<{0}> {parent}<{0}>`, member `{1}`: `{parent}<…>` \
                     takes {2} type argument{3}, `<{4}>`, and {5} {6} written",
                    f.parent_generics.join(", "),
                    f.name,
                    params.len(),
                    if params.len() == 1 { "" } else { "s" },
                    params.join(", "),
                    f.parent_args.len(),
                    if f.parent_args.len() == 1 { "was" } else { "were" },
                ),
            });
            continue;
        }
    }
}

/// Rewrite each `Named(p)` that names one of `params` into a [`Type::Param`].
fn bind_params(ty: &mut Type, params: &[String]) {
    match ty {
        Type::Named(n) if params.iter().any(|p| p == n) => *ty = Type::Param(n.clone()),
        Type::Claimed(_, t)
        | Type::List(t, _)
        | Type::Set(t, _)
        | Type::Option(t)
        | Type::Array(t, _)
        | Type::Boxed(t)
        | Type::Ref { inner: t, .. } => bind_params(t, params),
        Type::Map(k, v, _) => {
            bind_params(k, params);
            bind_params(v, params);
        }
        Type::Tuple(ts) | Type::App(_, ts) => {
            for t in ts {
                bind_params(t, params);
            }
        }
        Type::DartObject(spec) => {
            for t in [spec.item.as_mut(), spec.ret.as_mut(), spec.err.as_mut()]
                .into_iter()
                .flatten()
            {
                bind_params(t, params);
            }
        }
        _ => {}
    }
}

/// Substitute `bindings` (parameter name → argument) into a template's type.
fn substitute(ty: &Type, bindings: &[(String, Type)]) -> Type {
    let sub = |t: &Type| Box::new(substitute(t, bindings));
    match ty {
        Type::Param(p) => bindings
            .iter()
            .find(|(name, _)| name == p)
            .map(|(_, arg)| arg.clone())
            // Unreachable: arity is checked before this runs and a template's
            // `Param` nodes come from its own parameter list.
            .unwrap_or_else(|| ty.clone()),
        Type::List(t, k) => Type::List(sub(t), *k),
        Type::Set(t, k) => Type::Set(sub(t), *k),
        Type::Map(k, v, kind) => Type::Map(sub(k), sub(v), *kind),
        Type::Option(t) => Type::Option(sub(t)),
        Type::Array(t, n) => Type::Array(sub(t), *n),
        Type::Boxed(t) => Type::Boxed(sub(t)),
        Type::Ref { inner, mutable, unsized_borrow } => Type::Ref {
            inner: sub(inner),
            mutable: *mutable,
            unsized_borrow: *unsized_borrow,
        },
        Type::Claimed(c, t) => Type::Claimed(*c, sub(t)),
        Type::Tuple(ts) => Type::Tuple(ts.iter().map(|t| substitute(t, bindings)).collect()),
        Type::App(n, ts) => Type::App(
            n.clone(),
            ts.iter().map(|t| substitute(t, bindings)).collect(),
        ),
        Type::DartObject(spec) => {
            let mut spec = spec.clone();
            for t in [spec.item.as_mut(), spec.ret.as_mut(), spec.err.as_mut()]
                .into_iter()
                .flatten()
            {
                *t = substitute(t, bindings);
            }
            Type::DartObject(spec)
        }
        other => other.clone(),
    }
}

/// The state of one expansion run.
struct Expansion {
    struct_templates: Vec<StructDecl>,
    enum_templates: Vec<EnumDecl>,
    /// Declared struct names — the ones the author wrote, plus every struct
    /// instantiation registered so far. Type arguments resolve against this and
    /// the three below, which is why they grow: an argument that is itself an
    /// instantiation is registered before the head that names it.
    structs: HashSet<String>,
    enums: HashSet<String>,
    /// Declared handle types and their models.
    models: HashMap<String, Model>,
    /// Declared `bytes(...)` external type names.
    externs: HashSet<String>,
    /// Instantiation display names already registered, in registration order —
    /// which is the order the synthetic declarations are appended in, and so
    /// the order the schema fingerprint sees. A `Vec` rather than a set because
    /// that order has to be deterministic.
    done: Vec<String>,
    /// A liveness backstop, and nothing more: the number of instantiations
    /// [`check_instantiation_growth`] has already proved finite, above which
    /// the expansion reports instead of running forever.
    ///
    /// There is **no principled value** for it. It exists because a defect in
    /// that analysis would otherwise hang the build rather than fail it, and a
    /// hang is the one failure nobody can act on. Anything under it is
    /// accepted, so no legal interface is bounded by it.
    budget: usize,
    out_structs: Vec<StructDecl>,
    out_enums: Vec<EnumDecl>,
}

impl Expansion {
    /// Replace every application inside `ty`, innermost first, with a `Named`
    /// naming the synthetic declaration it instantiated.
    fn rewrite(&mut self, ty: &mut Type, ctx: &str, diags: &mut Vec<Diagnostic>) {
        match ty {
            Type::Claimed(_, t)
            | Type::List(t, _)
            | Type::Set(t, _)
            | Type::Option(t)
            | Type::Array(t, _)
            | Type::Boxed(t)
            | Type::Ref { inner: t, .. } => self.rewrite(t, ctx, diags),
            Type::Map(k, v, _) => {
                self.rewrite(k, ctx, diags);
                self.rewrite(v, ctx, diags);
            }
            Type::Tuple(ts) => {
                for t in ts {
                    self.rewrite(t, ctx, diags);
                }
            }
            Type::DartObject(spec) => {
                for t in [spec.item.as_mut(), spec.ret.as_mut(), spec.err.as_mut()]
                    .into_iter()
                    .flatten()
                {
                    self.rewrite(t, ctx, diags);
                }
            }
            Type::App(..) => {
                // Innermost first: an argument that is itself an application is
                // a registered declaration by the time the head is registered,
                // so the head's own identity is written in terms of it.
                let Type::App(name, args) = std::mem::replace(ty, Type::Bool) else {
                    unreachable!("matched App")
                };
                let mut args = args;
                for a in &mut args {
                    self.rewrite(a, ctx, diags);
                }
                *ty = match self.apply(&name, args, ctx, diags) {
                    Some(display) => Type::Named(display),
                    // Refused; `check_pass` returns before resolution reads
                    // this, so what it is left as is never observed.
                    None => Type::Named(name),
                };
            }
            _ => {}
        }
    }

    /// Register `name<args>` if it is a legal application, and answer with the
    /// synthetic declaration's name.
    fn apply(
        &mut self,
        name: &str,
        args: Vec<Type>,
        ctx: &str,
        diags: &mut Vec<Diagnostic>,
    ) -> Option<String> {
        let is_enum = self.enum_templates.iter().any(|e| e.name == name);
        let params: Vec<String> = if is_enum {
            self.enum_templates
                .iter()
                .find(|e| e.name == name)
                .map(|e| e.generics.clone())?
        } else {
            match self.struct_templates.iter().find(|s| s.name == name) {
                Some(s) => s.generics.clone(),
                None => {
                    let why = if self.is_declared(name) {
                        format!(
                            "`{name}` is declared, but it has no type parameters — write \
                             `{name}`"
                        )
                    } else {
                        format!(
                            "no bridged generic data type named `{name}` is declared. A \
                             type argument list is only accepted on a `#[bridge(data)]` \
                             struct or enum that declares parameters; a handle type and a \
                             `bytes(...)` type cannot be generic (FR0056)"
                        )
                    };
                    diags.push(Diagnostic {
                        code: "FR0073",
                        message: format!("{ctx}: `{name}<…>`: {why}"),
                    });
                    return None;
                }
            }
        };
        if params.len() != args.len() {
            diags.push(Diagnostic {
                code: "FR0073",
                message: format!(
                    "{ctx}: `{name}<…>` takes {} type argument{}, `<{}>`, and {} {} \
                     written. Every parameter is written out at every use: the \
                     instantiations codegen emits are exactly the ones the signatures name",
                    params.len(),
                    if params.len() == 1 { "" } else { "s" },
                    params.join(", "),
                    args.len(),
                    if args.len() == 1 { "was" } else { "were" },
                ),
            });
            return None;
        }

        // A representation marker inside a type argument is checked and erased
        // here rather than at resolution, because the *identity* of the
        // instantiation is formed from the arguments: a matching `Data<Item>`
        // and a bare `Item` have to name one instantiation and one schema
        // fingerprint, which is the property every other position already has.
        let mut args = args;
        for a in &mut args {
            self.check_and_erase_claims(a, ctx, diags);
            self.resolve_arg(a, ctx, diags);
        }

        let display = Type::App(name.to_string(), args.clone()).rust_display();
        if self.done.iter().any(|d| d == &display) {
            return Some(display);
        }
        if self.budget == 0 {
            diags.push(Diagnostic {
                code: "FR0075",
                message: format!(
                    "{ctx}: registering `{display}` passed {EXPANSION_BUDGET} \
                     instantiations. `check_instantiation_growth` proved this interface's \
                     set finite before the expansion ran, so this is a defect in that \
                     analysis and not something to fix in the source — please report it \
                     with this declaration. (The budget exists only so the build fails \
                     instead of hanging.)"
                ),
            });
            return None;
        }
        self.budget -= 1;

        let bindings: Vec<(String, Type)> = params.iter().cloned().zip(args.clone()).collect();
        self.done.push(display.clone());
        if is_enum {
            self.enums.insert(display.clone());
        } else {
            self.structs.insert(display.clone());
        }

        let stem = instance_stem(&display);
        let instance = Some(Instance {
            template: name.to_string(),
            args,
            stem,
        });
        if is_enum {
            let template = self
                .enum_templates
                .iter()
                .find(|e| e.name == name)
                .expect("looked up above")
                .clone();
            let mut decl = EnumDecl {
                name: display.clone(),
                generics: vec![],
                const_generics: vec![],
                defaulted_generics: vec![],
                instance,
                ..template.clone()
            };
            for (v, tv) in decl.variants.iter_mut().zip(&template.variants) {
                for (f, tf) in v.fields.iter_mut().zip(&tv.fields) {
                    let fctx = format!("`{display}`, variant `{}`, field `{}`", v.name, f.name);
                    Expansion::check_dart_form(&tf.ty, false, &bindings, name, &fctx, diags);
                    f.ty = substitute(&tf.ty, &bindings);
                    self.rewrite(&mut f.ty, &fctx, diags);
                }
            }
            self.out_enums.push(decl);
        } else {
            let template = self
                .struct_templates
                .iter()
                .find(|s| s.name == name)
                .expect("looked up above")
                .clone();
            let mut decl = StructDecl {
                name: display.clone(),
                generics: vec![],
                const_generics: vec![],
                defaulted_generics: vec![],
                instance,
                ..template.clone()
            };
            for (f, tf) in decl.fields.iter_mut().zip(&template.fields) {
                let fctx = format!("`{display}`, field `{}`", f.name);
                Expansion::check_dart_form(&tf.ty, false, &bindings, name, &fctx, diags);
                f.ty = substitute(&tf.ty, &bindings);
                self.rewrite(&mut f.ty, &fctx, diags);
            }
            self.out_structs.push(decl);
        }
        Some(display)
    }

    /// FR0074 — the one binding the generic Dart class cannot express.
    ///
    /// The class declares each field with the **template's** type, so
    /// `Option<T>` is `T?`. Dart collapses `T?` where `T` is itself nullable
    /// into one `T?`, while the wire keeps `Some(None)` and `None` apart and
    /// the codec for the substituted type therefore uses the faithful
    /// `FrOption` form (`emit_dart::dart_type_opt`). One class cannot declare
    /// both, so an `Option` argument bound to a parameter that sits under an
    /// `Option` is refused rather than silently collapsed.
    ///
    /// `under_option` is reset by every other container, because every other
    /// container renders compositionally: `Option<Vec<T>>` is `List<T>?`, and
    /// `T = Option<i64>` gives `List<int?>?` from both directions.
    fn check_dart_form(
        ty: &Type,
        under_option: bool,
        bindings: &[(String, Type)],
        template: &str,
        ctx: &str,
        diags: &mut Vec<Diagnostic>,
    ) {
        match ty {
            Type::Param(p) if under_option => {
                let Some((_, arg)) = bindings.iter().find(|(name, _)| name == p) else {
                    return;
                };
                if matches!(unbox(arg), Type::Option(_)) {
                    diags.push(Diagnostic {
                        code: "FR0074",
                        message: format!(
                            "{ctx}: `{p}` is written under an `Option` in `{template}`, so \
                             the generated Dart class declares this field `{p}?` — and \
                             Dart has no nullable-of-nullable, so `{p} = {}` would collapse \
                             `Some(None)` and `None` into one value the class cannot tell \
                             apart. Bind `{p}` to a non-optional type and move the \
                             optionality into `{template}`'s own field, or declare a \
                             non-generic struct for this instantiation",
                            arg.rust_display()
                        ),
                    });
                }
            }
            // Transparent to Dart, so it does not reset the nesting. A borrow
            // is transparent the same way: `Option<&T>` is the same `T?`.
            Type::Boxed(t) | Type::Ref { inner: t, .. } => {
                Expansion::check_dart_form(t, under_option, bindings, template, ctx, diags)
            }
            Type::Option(t) => Expansion::check_dart_form(t, true, bindings, template, ctx, diags),
            Type::List(t, _) | Type::Set(t, _) | Type::Array(t, _) | Type::Claimed(_, t) => {
                Expansion::check_dart_form(t, false, bindings, template, ctx, diags)
            }
            Type::Map(k, v, _) => {
                Expansion::check_dart_form(k, false, bindings, template, ctx, diags);
                Expansion::check_dart_form(v, false, bindings, template, ctx, diags);
            }
            Type::Tuple(ts) | Type::App(_, ts) => {
                for t in ts {
                    Expansion::check_dart_form(t, false, bindings, template, ctx, diags);
                }
            }
            Type::DartObject(spec) => {
                for t in [spec.item.as_ref(), spec.ret.as_ref(), spec.err.as_ref()]
                    .into_iter()
                    .flatten()
                {
                    Expansion::check_dart_form(t, false, bindings, template, ctx, diags);
                }
            }
            _ => {}
        }
    }

    /// FR0062 inside a type argument, then erasure — see the call site.
    fn check_and_erase_claims(&self, ty: &mut Type, ctx: &str, diags: &mut Vec<Diagnostic>) {
        match ty {
            Type::Claimed(claim, inner) => {
                let claim = *claim;
                self.check_and_erase_claims(inner, ctx, diags);
                // The declaration's own representation, read off the same name
                // sets `resolve_type` reads. An argument that is itself an
                // instantiation has already been rewritten to a `Named` naming
                // a synthetic *data* declaration, so it answers `data` here
                // exactly as the template does.
                if let Type::Named(n) = inner.as_ref() {
                    let actual = if self.structs.contains(n) || self.enums.contains(n) {
                        Some(Claim::Data)
                    } else {
                        self.models.get(n).copied().map(Claim::Model)
                    };
                    // No arm for an unresolved name or an extern: FR0003 and
                    // FR0062's extern message are `resolve_type`'s, and they
                    // reach the same argument where it was substituted into the
                    // synthetic declaration's field.
                    if let Some(actual) = actual {
                        check_representation_claim(claim, actual, n, ctx, diags);
                    }
                }
                *ty = (**inner).clone();
            }
            Type::List(t, _)
            | Type::Set(t, _)
            | Type::Option(t)
            | Type::Array(t, _)
            | Type::Boxed(t)
            | Type::Ref { inner: t, .. } => self.check_and_erase_claims(t, ctx, diags),
            Type::Map(k, v, _) => {
                self.check_and_erase_claims(k, ctx, diags);
                self.check_and_erase_claims(v, ctx, diags);
            }
            Type::Tuple(ts) | Type::App(_, ts) => {
                for t in ts {
                    self.check_and_erase_claims(t, ctx, diags);
                }
            }
            Type::DartObject(spec) => {
                for t in [spec.item.as_mut(), spec.ret.as_mut(), spec.err.as_mut()]
                    .into_iter()
                    .flatten()
                {
                    self.check_and_erase_claims(t, ctx, diags);
                }
            }
            _ => {}
        }
    }

    /// The type arguments of the registered instantiation `display`, when it
    /// is one of `template`'s. `None` for every other registered name — which
    /// is what `expand_impl_members` filters on.
    fn instance_args(&self, display: &str, template: &str) -> Option<Vec<Type>> {
        let inst = self
            .out_structs
            .iter()
            .find(|s| s.name == display)
            .and_then(|s| s.instance.as_ref())
            .or_else(|| {
                self.out_enums
                    .iter()
                    .find(|e| e.name == display)
                    .and_then(|e| e.instance.as_ref())
            })?;
        (inst.template == template).then(|| inst.args.clone())
    }

    /// True when some declaration of this interface already answers to `name`.
    fn is_declared(&self, name: &str) -> bool {
        self.structs.contains(name)
            || self.enums.contains(name)
            || self.models.contains_key(name)
            || self.externs.contains(name)
    }

    /// Resolve the names inside a type **argument**, here rather than in the
    /// ordinary resolution pass.
    ///
    /// Two reasons, both about the fact that an argument is not only written in
    /// a field. It is also stored on the [`Instance`], where the emitters read
    /// it to spell the Rust path (`crate::api::Page<crate::api::Item>`) and the
    /// Dart type (`Page<Item>`) — so it has to be resolved, and resolving it
    /// twice would report an unknown name twice. And a template may not use
    /// every parameter, so an argument that reaches no field would otherwise
    /// reach no diagnostic either and arrive at an emitter unresolved.
    fn resolve_arg(&self, ty: &mut Type, ctx: &str, diags: &mut Vec<Diagnostic>) {
        match ty {
            Type::Named(n) => {
                let name = n.clone();
                *ty = if self.structs.contains(&name) {
                    Type::Struct(name)
                } else if self.enums.contains(&name) {
                    Type::Enum(name)
                } else if self.models.contains_key(&name) {
                    Type::Opaque(name)
                } else if self.externs.contains(&name) {
                    Type::Extern(name)
                } else {
                    diags.push(unknown_type(ctx, &name));
                    return;
                };
            }
            Type::List(t, _)
            | Type::Set(t, _)
            | Type::Option(t)
            | Type::Array(t, _)
            | Type::Boxed(t)
            | Type::Ref { inner: t, .. }
            | Type::Claimed(_, t) => self.resolve_arg(t, ctx, diags),
            Type::Map(k, v, _) => {
                self.resolve_arg(k, ctx, diags);
                self.resolve_arg(v, ctx, diags);
            }
            Type::Tuple(ts) | Type::App(_, ts) => {
                for t in ts {
                    self.resolve_arg(t, ctx, diags);
                }
            }
            Type::DartObject(spec) => {
                for t in [spec.item.as_mut(), spec.ret.as_mut(), spec.err.as_mut()]
                    .into_iter()
                    .flatten()
                {
                    self.resolve_arg(t, ctx, diags);
                }
            }
            _ => {}
        }
    }
}

/// The identifier form of an instantiation's name.
///
/// `Page<Item>` is the Rust type and so a unique *name*, but it is not an
/// identifier, and the codec functions on both sides are named after it
/// (`enc_…`, `_enc…`). This escapes it into `[A-Za-z0-9_]` and prefixes the
/// escaped length.
///
/// **It cannot collide.** The escape is injective (every character outside
/// `[A-Za-z0-9]`, `_` included, becomes `_` plus two lowercase hex digits, so no
/// two spellings share an encoding), and the leading length digit means no stem
/// is ever a Rust identifier — which is what every non-generic declaration's
/// stem is. So an instantiation can collide neither with another instantiation
/// nor with a declared type.
pub(crate) fn instance_stem(display: &str) -> String {
    let mut escaped = String::new();
    for c in display.chars() {
        if c.is_ascii_alphanumeric() {
            escaped.push(c);
        } else {
            // Non-ASCII is possible in a Rust type name; encode its scalar
            // value so the escape stays total and injective.
            escaped.push_str(&format!("_{:x}_", c as u32));
        }
    }
    format!("{}{escaped}", escaped.len())
}

/// Append synthetic functions and assign dispatch ids. Runs once, after
/// both capability passes; ids therefore cover the full surface and are
/// identical on every platform (web omits members, never renumbers them).
fn finalize(mut iface: Interface) -> Result<Interface, Vec<Diagnostic>> {
    // Actor objects must drop on their own executor, after queued calls
    // drain — so disposal is a dispatched call, not a `frustrate_drop_*`
    // export. Append one synthetic drop function per actor type.
    let dual: HashSet<String> = iface
        .opaques
        .iter()
        .filter(|o| iface.is_dual(&o.name))
        .map(|o| o.name.clone())
        .collect();
    for o in &iface.opaques {
        if o.model == Model::Actor {
            iface.functions.push(Function {
                fn_id: 0,
                name: format!("__frustrate_drop_{}", o.name),
                module_path: o.module_path.clone(),
                parent: Some(o.name.clone()),
                parent_claim: None,
                parent_args: vec![],
                parent_generics: vec![],
                // Constructed here rather than parsed, so it carries its own
                // half: disposal is the handle's, and a type declaring two
                // representations has a value half that does not dispose.
                // `emit_dart::emit_actor` looks this member up among the
                // handle half's, and finding none is a panic.
                parent_repr: dual.contains(&o.name).then_some(Repr::Handle),
                trait_impl: None,
                receiver: None,
                generics: vec![],
                params: vec![],
                ret: None,
                // A synthetic drop returns nothing and cannot fail.
                err: None,
                ret_borrow: false,
                getter: false,
                // Synthesized, so there is no author to have named it.
                dart_identifier: None,
                fallible: false,
                exec: Exec::Async,
                on_contention: None,
                is_constructor: false,
                is_actor_drop: true,
                // Appended *after* the derivation above, so it inherits the
                // type's declaration directly. A native-only actor whose drop
                // stayed portable would put a dispatch arm naming a
                // wasm-absent type back into the web build.
                requires_native: o.native_only,
                native_only: o.native_only,
                // Never claimed. A synthetic drop runs the type's `Drop` impl,
                // which codegen never sees and the author never annotated, so
                // there is no claim to inherit — and inventing one would be a
                // proof about code nobody asserted anything about.
                no_block: false,
                cfg_gated: false,
                web_runtime_fail: false,
                rust_async: false,
                deferred: false,
                docs: vec![],
            });
        }
    }

    // Assign dispatch ids from each member's own wire-relevant facts, not from
    // its position (`hash::member_ids`, which carries the derivation and the
    // collision rule). Both emitters consume this same finalized IR, so the
    // table is consistent by construction.
    //
    // This was ordinal until hot patching landed. The comment here used to
    // decline hashing because "the only way the two halves can skew is a
    // hand-edited generated file", and listed what would change that: "a
    // reason to let a caller built against a *different* id table through".
    // That is now an ordinary occurrence rather than a hypothetical. A patch
    // replaces the running library's dispatch tables, and the Dart generated
    // beside it arrives separately — later, or never, since a reload that
    // fails to compile leaves the old Dart calling the patched image. With
    // positional ids a member declared after an insertion inherits its
    // neighbour's slot, so that stale caller decodes its arguments in a body
    // expecting a different signature. Content-derived ids turn the same
    // situation into an absent id and the `unknown fn_id` panic every entry
    // point already has.
    let ids = crate::hash::member_ids(&iface.functions);
    for (f, id) in iface.functions.iter_mut().zip(ids) {
        f.fn_id = id;
    }
    Ok(iface)
}

/// One typed error the interface declares, and whether any of its declarations
/// is on a `DartFunction` — which generates a second Dart name, the `EFallible`
/// closure alias, and so a second way to collide.
pub(crate) struct TypedError<'a> {
    pub name: &'a str,
    pub on_a_closure: bool,
}

/// `t` with any [`Type::Claimed`] wrapper stripped, without resolving it — an
/// error type may be written `Data<MyError>` like any other use site, and
/// this runs before resolution (see [`typed_error_names`]), so the wrapper is
/// still there to strip.
fn peel_claim(t: &Type) -> &Type {
    match t {
        Type::Claimed(_, inner) => peel_claim(inner),
        other => other,
    }
}

/// Every typed error name in the interface, deduplicated, with `on_a_closure`
/// OR-ed across occurrences.
///
/// Matches on `Type::Named`, because this runs **before** resolution — FR0036
/// has to fire before a bad name reaches the emitters. For the same reason it
/// walks the declarations directly instead of following `Named` links: a handle
/// nested in a struct field is found because the struct's own fields are walked,
/// not because the walk descends through the reference to it.
pub(crate) fn typed_error_names(iface: &Interface) -> Vec<TypedError<'_>> {
    fn add<'a>(out: &mut Vec<TypedError<'a>>, name: &'a str, on_a_closure: bool) {
        match out.iter_mut().find(|e| e.name == name) {
            Some(e) => e.on_a_closure |= on_a_closure,
            None => out.push(TypedError { name, on_a_closure }),
        }
    }
    let mut out: Vec<TypedError<'_>> = Vec::new();
    for f in &iface.functions {
        if let Some(Type::Named(e)) = f.err.as_ref().map(peel_claim) {
            add(&mut out, e.as_str(), false);
        }
    }
    let mut on_closures: Vec<&str> = Vec::new();
    walk_every_type(iface, &mut |t| {
        if let Type::DartObject(spec) = t {
            if let Some(Type::Named(e)) = spec.err.as_ref().map(peel_claim) {
                on_closures.push(e.as_str());
            }
        }
    });
    for e in on_closures {
        add(&mut out, e, true);
    }
    out
}

/// Every type written anywhere in the interface, at every depth, without
/// following `Named` links (see [`typed_error_names`] for why that matters).
fn walk_every_type<'a>(iface: &'a Interface, f: &mut dyn FnMut(&'a Type)) {
    for s in &iface.structs {
        for field in &s.fields {
            field.ty.walk(f);
        }
    }
    for e in &iface.enums {
        for v in &e.variants {
            for field in &v.fields {
                field.ty.walk(f);
            }
        }
    }
    for fun in &iface.functions {
        for p in &fun.params {
            p.ty.walk(f);
        }
        if let Some(r) = &fun.ret {
            r.walk(f);
        }
        if let Some(e) = &fun.err {
            e.walk(f);
        }
    }
}

/// Walk `ty` and everything reachable from it, **descending into declared
/// structs and enums** — unlike [`Type::walk`], which stops at a `Struct`
/// name because the bare IR type has no way to look one up.
///
/// This is load-bearing rather than convenience: every handle rule
/// (FR0018, FR0020, FR0031, and the `requires_native` derivation) has to see
/// a handle no matter how deeply a struct nests it. A shallow check lets a
/// native-only member into the web subset, which is latent UB.
///
/// Cycle-guarded by declaration name, so a recursive type terminates. The
/// guard is on declarations, not on `Type` values, because only a named
/// declaration can close a cycle.
pub(crate) fn walk_type_graph<'a>(
    iface: &'a Interface,
    ty: &'a Type,
    f: &mut dyn FnMut(&'a Type),
) {
    walk_type_graph_mode(iface, ty, true, f)
}

/// [`walk_type_graph`], with the choice of stopping at a [`Type::Ref`].
///
/// `through_refs` is what tells the two questions apart. Almost every rule
/// wants `true` — whether a native-only type is named, whether a mirror with a
/// reply frame is reachable, whether a handle appears at all, none of which a
/// borrow changes. The rule that wants `false` is FR0004, which is about
/// *transfer*: `Vec<&Doc>` reaches no handle it takes, `Vec<Doc>` does, and
/// that is the whole difference between them.
fn walk_type_graph_mode<'a>(
    iface: &'a Interface,
    ty: &'a Type,
    through_refs: bool,
    f: &mut dyn FnMut(&'a Type),
) {
    walk_type_graph_full(iface, ty, through_refs, true, f)
}

/// [`walk_type_graph`] with the choice of stopping at a channel endpoint too.
///
/// `through_channels: false` answers "what does **this value's own bytes**
/// carry". A `StreamSink<Doc>` field puts `Doc` in the type graph, but not in
/// this type's envelope: the item is a separate payload, encoded by the
/// channel's own encoder and delivered later. A rule about direction, about
/// which codec a declaration gets, or about what a position transfers must not
/// see through one — a struct holding a `StreamSink<Doc>` travels Dart → Rust
/// perfectly well and hands over nothing.
fn walk_type_graph_full<'a>(
    iface: &'a Interface,
    ty: &'a Type,
    through_refs: bool,
    through_channels: bool,
    f: &mut dyn FnMut(&'a Type),
) {
    #[allow(clippy::too_many_arguments)]
    fn go<'a>(
        iface: &'a Interface,
        ty: &'a Type,
        through_refs: bool,
        through_channels: bool,
        seen: &mut HashSet<&'a str>,
        f: &mut dyn FnMut(&'a Type),
    ) {
        f(ty);
        match ty {
            Type::Ref { inner, .. } => {
                if through_refs {
                    go(iface, inner, through_refs, through_channels, seen, f);
                }
            }
            Type::List(t, _)
            | Type::Set(t, _)
            | Type::Option(t)
            | Type::Array(t, _)
            | Type::Boxed(t) => go(iface, t, through_refs, through_channels, seen, f),
            Type::Map(k, v, _) => {
                go(iface, k, through_refs, through_channels, seen, f);
                go(iface, v, through_refs, through_channels, seen, f);
            }
            Type::Tuple(ts) => {
                for t in ts {
                    go(iface, t, through_refs, through_channels, seen, f);
                }
            }
            Type::DartObject(spec) => {
                if !through_channels {
                    return;
                }
                if let Some(t) = &spec.item {
                    go(iface, t, through_refs, through_channels, seen, f);
                }
                if let Some(t) = &spec.ret {
                    go(iface, t, through_refs, through_channels, seen, f);
                }
                if let Some(t) = &spec.err {
                    go(iface, t, through_refs, through_channels, seen, f);
                }
            }
            Type::Struct(name) => {
                if !seen.insert(name.as_str()) {
                    return;
                }
                if let Some(s) = iface.structs.iter().find(|s| &s.name == name) {
                    for field in &s.fields {
                        go(iface, &field.ty, through_refs, through_channels, seen, f);
                    }
                }
            }
            Type::Enum(name) => {
                if !seen.insert(name.as_str()) {
                    return;
                }
                if let Some(e) = iface.enums.iter().find(|e| &e.name == name) {
                    for v in &e.variants {
                        for field in &v.fields {
                            go(iface, &field.ty, through_refs, through_channels, seen, f);
                        }
                    }
                }
            }
            // The leaves, spelled out rather than left to a `_ => {}`. This is
            // the one type walk whose blindness would be **silent**: a new
            // container variant that this stopped at would hide a handle from
            // `reachable_handles`, and the two rules that read it — FR0020 (a
            // Dart method with a reply frame cannot be invoked from a sync
            // member) and the `requires_native` derivation — would then say
            // nothing while both halves still compiled, which is a deadlock or
            // a member emitted into a web surface whose glue names an item the
            // wasm build does not have. Every other walk over `Type` fails
            // loudly instead (an `unreachable!` in an emitter, an FR0003, or a
            // size hint that is a lower bound by contract), so they keep their
            // wildcard. Adding a variant must fail *here*, at rustc.
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
            | Type::Named(_)
            // `Claimed` and `App` are erased before the rules that read this
            // walk ever run — `resolve_type` for the marker, the expansion pass
            // for the application — so there is nothing under them to reach.
            | Type::Claimed(..)
            | Type::App(..)
            // A `Param` lives only inside a template, which no rule that reads
            // this walk ever asks about: every rule asks about a declaration in
            // `structs`/`enums`, and the expansion substituted them all away.
            | Type::Param(_)
            | Type::Extern(_)
            | Type::Opaque(_) => {}
        }
    }
    go(iface, ty, through_refs, through_channels, &mut HashSet::new(), f);
}

/// [`walk_type_graph`], stopping at a [`Type::Ref`].
///
/// The one question that changes across a borrow is *transfer*: everything
/// under a reference is lent, and a rule about who ends up owning a value has
/// nothing to say about it. `Vec<&Doc>` therefore reaches no handle here,
/// while `Vec<Doc>` reaches one — which is the difference FR0004 is asking
/// about and the graph walk deliberately erases.
fn lending_aware_walk<'a>(iface: &'a Interface, ty: &'a Type, f: &mut dyn FnMut(&'a Type)) {
    // Nor through a channel endpoint, for `reaches_opaque`'s reason: what a
    // `StreamSink<Doc>` parameter transfers is the endpoint, not the items. The
    // items are the channel's own envelope, minted when the producer sends and
    // reclaimed by the channel if they are never delivered.
    walk_type_graph_full(iface, ty, false, false, f)
}

/// `true` when `name` is a declared handle type (a concurrency model) that
/// also carries `native_only` — e.g. `#[bridge(confined, native_only)]`.
fn opaque_is_native_only(iface: &Interface, name: &str) -> bool {
    iface
        .opaques
        .iter()
        .any(|o| o.name == name && o.native_only)
}

/// The native-only opaque `f` names anywhere the wasm build would have to
/// resolve it: as its parent type (a method, or a constructor), in a parameter,
/// or in the return — at any depth, since an opaque may be nested inside a
/// struct, a `Vec`, an `Option`, or a callback's signature. `None` when there
/// is none.
///
/// This is what makes the type-level declaration worth having over marking
/// every member by hand: a free function taking `&NativeOnlyType` is native-only
/// whether or not its author remembered to say so, and forgetting would
/// otherwise emit web glue naming an item the wasm build does not have.
pub(crate) fn native_only_type_in_scope(iface: &Interface, f: &Function) -> Option<String> {
    if let Some(p) = f.parent.as_deref() {
        if opaque_is_native_only(iface, p) {
            return Some(p.to_string());
        }
    }
    f.params
        .iter()
        .map(|p| &p.ty)
        .chain(f.ret.iter())
        .find_map(|ty| native_only_in_type(iface, ty))
}

/// The native-only opaque reachable from `ty` through the declaration graph,
/// if any.
///
/// A **declared type** needs this as much as a function does. A data type may
/// hold a handle (FR0004 is a direction rule), so a struct with a native-only
/// field is itself absent from the wasm build — its Dart class would declare a
/// field of a type the web surface does not emit, and its decoder would call
/// that type's private constructor. The type-level answer is the same as the
/// member-level one: omit it, with a breadcrumb saying where it went.
pub(crate) fn native_only_in_type(iface: &Interface, ty: &Type) -> Option<String> {
    let mut found = None;
    walk_type_graph(iface, ty, &mut |t| {
        if let Type::Opaque(n) = t {
            if found.is_none() && opaque_is_native_only(iface, n) {
                found = Some(n.clone());
            }
        }
    });
    found
}

/// [`native_only_in_type`] for a declared struct or enum, by name.
pub(crate) fn decl_native_only(iface: &Interface, name: &str) -> Option<String> {
    native_only_in_type(iface, &Type::Struct(name.into()))
        .or_else(|| native_only_in_type(iface, &Type::Enum(name.into())))
}

/// Whether any opaque is reachable through a channel endpoint's **item** —
/// the payload a `StreamSink`/`DartCallback` carries Rust → Dart.
///
/// The endpoints are found structurally ([`Type::walk`], matching how the actor
/// rules read a parameter), but each item is walked through the **declaration
/// graph**: an item is a value the producer builds, so a handle in a field of it
/// crosses exactly as a bare one does. That distinction is load-bearing — while
/// FR0018 refused every handle in an item, FR0004's declaration walk covered
/// this; now that items may carry handles, nothing else does.
pub(crate) fn opaque_in_a_channel_item(
    iface: &Interface,
    ty: &Type,
    is_hit: &dyn Fn(&Type) -> bool,
) -> bool {
    let mut found = false;
    ty.walk(&mut |t| {
        if let Type::DartObject(spec) = t {
            for item in spec.item.iter() {
                walk_type_graph(iface, item, &mut |t| found |= is_hit(t));
            }
        }
    });
    found
}

/// Every Dart-object handle reachable from `ty`, structs and enums included.
pub(crate) fn reachable_handles<'a>(
    iface: &'a Interface,
    ty: &'a Type,
) -> Vec<&'a DartObjectSpec> {
    let mut out = vec![];
    walk_type_graph(iface, ty, &mut |t| {
        if let Some(spec) = t.as_dart_object() {
            out.push(spec);
        }
    });
    out
}

/// A readable name for a type in a diagnostic. Named kinds print their name;
/// containers and primitives fall back to the IR spelling, which is ugly but
/// unambiguous — and both are already rare enough here to be worth neither a
/// pretty-printer nor a wrong guess.
/// What a data type's member name would collide with on the generated Dart
/// side, or `None` if it collides with nothing (FR0027).
///
/// A data class is not a blank slate the way a handle class is: it already
/// declares every field as a getter, `copyWith`, `==`, `hashCode` and
/// `toString`, and a *unit-only* enum lands as a Dart `enum`, which comes with
/// `index`/`name`/`values`/`compareTo` on top of `Object`. A member that
/// reuses one of those names does not fail here; it fails inside generated
/// code, as a duplicate declaration or an invalid override naming nothing the
/// author wrote — FR0052's reason, one position over.
fn data_surface_clash(
    iface: &Interface,
    parent: &str,
    member: &str,
) -> Option<(&'static str, &'static str)> {
    let dart = crate::emit_dart::dart_name(member);
    if let Some(s) = iface.struct_decl(parent) {
        if s.fields.iter().any(|f| crate::emit_dart::dart_name(&f.name) == dart) {
            return Some((
                "a field of the same type",
                "the generated class would declare a method and a field under one name \
                 and would not compile",
            ));
        }
    }
    // A fielded enum's fields live on its variant subclasses, so only the
    // shared surface can clash there. A unit-only enum has Dart's own on top.
    let decl = iface.enums.iter().find(|e| e.name == parent);
    let unit_only = decl.is_some_and(|e| e.variants.iter().all(|v| v.fields.is_empty()));
    if unit_only && matches!(dart.as_str(), "index" | "name" | "values" | "compareTo") {
        return Some((
            "a member every Dart `enum` already has (`index`/`name`/`values`/`compareTo`); \
             a unit-only enum lands as one",
            "it would be an invalid override of something the author never wrote",
        ));
    }
    // `discriminant` is reserved where it is emitted — on an enum whose own
    // declaration writes discriminants — and not on every unit enum, unlike
    // `take`, which every handle class reserves whether or not it emits one.
    // The difference is who can cause the collision: `take` appears because
    // something *elsewhere* in the interface consumes the type, so reserving it
    // conditionally would let a stranger's edit turn an already-compiling app
    // into an error. A discriminant is written on this declaration, so the
    // author who causes the clash is the author who reads this.
    if unit_only
        && dart == "discriminant"
        && decl.is_some_and(|e| e.variants.iter().any(|v| v.discriminant.is_some()))
    {
        return Some((
            "the `discriminant` getter this enum's own explicit discriminants generate",
            "it would silently shadow the number the Rust declaration writes",
        ));
    }
    matches!(
        dart.as_str(),
        "copyWith" | "toString" | "hashCode" | "runtimeType" | "noSuchMethod"
    )
    .then_some((
        "the generated data-class surface \
         (`copyWith`/`toString`/`hashCode`/`runtimeType`/`noSuchMethod`)",
        "it would silently shadow it (e.g. a `copy_with` that is not the one route to a \
         changed value)",
    ))
}

fn error_type_label(t: &Type) -> String {
    match t {
        Type::Opaque(n) | Type::Extern(n) | Type::Struct(n) | Type::Enum(n) | Type::Named(n) => {
            n.clone()
        }
        other => format!("{other:?}"),
    }
}

/// What a `Type::Named` resolves against: the interface's declared type names,
/// plus the generic parameters of the declaration whose types are being
/// resolved.
struct Scope<'a> {
    structs: &'a HashSet<String>,
    enums: &'a HashSet<String>,
    opaques: &'a HashMap<String, Model>,
    externs: &'a HashSet<String>,
    /// Names declared under two representations — see
    /// [`crate::ir::Interface::is_dual`]. A bare one of these resolves to
    /// neither declaration (FR0067); a marker selects the half.
    duals: &'a HashSet<String>,
    /// The enclosing **declaration's** own name, set only where the parser
    /// substitutes `Self` for a bare name — a struct or enum field. A bare
    /// name equal to it may therefore be one the author never typed, and a
    /// diagnostic naming it has to say so or they go looking for a `Doc` that
    /// is not in their source.
    ///
    /// `None` everywhere else, a member signature included: an `impl` block's
    /// `Self` carries the block's marker, so on a dual type it resolves rather
    /// than arriving bare.
    self_name: Option<&'a str>,
    /// The enclosing declaration's own type/const parameter names. A `Named`
    /// that matches one is a parameter, not an unknown type: it is left as it
    /// is with no FR0003, because FR0056 has already refused the declaration
    /// and an unknown-type message would send the author looking for a
    /// declaration that cannot exist.
    generics: &'a [String],
    /// The generic data templates. A `Named` that matches one is a template
    /// written with no arguments, which is not a type — every use writes them
    /// out — so it gets FR0073's own message rather than FR0003's, which would
    /// say the declaration does not exist when it does.
    templates: &'a HashSet<String>,
}

/// The two representations a type declaring both crosses under, in the order
/// the author writes them.
fn dual_claims(opaques: &HashMap<String, Model>, name: &str) -> [Claim; 2] {
    [
        Claim::Data,
        Claim::Model(*opaques.get(name).expect("a dual type has a handle half")),
    ]
}

/// The marker type that names one representation at a use site or on an
/// `impl` block's self type: `Data<Doc>`, `Locked<Doc>`.
fn claim_marker(c: Claim) -> &'static str {
    match c {
        Claim::Data => "Data",
        Claim::Model(Model::Confined) => "Confined",
        Claim::Model(Model::Resident) => "Resident",
        Claim::Model(Model::Frozen) => "Frozen",
        Claim::Model(Model::Locked) => "Locked",
        Claim::Model(Model::Actor) => "Actor",
    }
}

/// FR0067 — an `impl` block on a type declaring two representations whose
/// self type does not say which half the members are for, or claims a third.
///
/// A member's half is not a detail of presentation: it decides whether the
/// receiver is decoded out of the request or read as a handle id, which rule
/// set the signature is checked under, and which Dart class the member lands
/// on. Nothing in a bare `impl Doc` chooses between them, and picking one
/// would be picking for the author.
fn unplaced_member_diagnostic(
    parent: &str,
    claim: Option<Claim>,
    trait_impl: Option<&str>,
    handle_only: bool,
    opaques: &HashMap<String, Model>,
) -> Diagnostic {
    let [a, b] = dual_claims(opaques, parent);
    // Spelled as the block the author wrote, trait and all, so the diagnostic
    // names a line that is in their file.
    let tr = match trait_impl {
        Some(path) => format!("{} for ", path.rsplit("::").next().unwrap_or(path)),
        None => String::new(),
    };
    let how = if handle_only {
        format!(
            "`#[bridge] impl {tr}{}<{parent}>` — a bridged trait is implemented by \
             handles, so the value half is not an option here (FR0026)",
            claim_marker(b)
        )
    } else {
        format!(
            "`#[bridge] impl {tr}{}<{parent}>` or `#[bridge] impl {tr}{}<{parent}>`",
            claim_marker(a),
            claim_marker(b)
        )
    };
    match claim {
        Some(c) => Diagnostic {
            code: "FR0062",
            message: format!(
                "`impl {tr}{}<{parent}>`: `{parent}` is declared `{}` and `{}`, not `{}` — \
                 this marker claims a representation `{parent}` does not have. Write {how}",
                claim_marker(c),
                describe_claim(a),
                describe_claim(b),
                describe_claim(c),
            ),
        },
        None => Diagnostic {
            code: "FR0067",
            message: format!(
                "`impl {tr}{parent}`: `{parent}` is declared `{}` and `{}`, so it crosses \
                 as two Dart classes, and this block does not say which one its members \
                 are on. The two are checked under different rules and reach `self` in \
                 different ways, so there is no default to pick. Write {how}",
                describe_claim(a),
                describe_claim(b),
            ),
        },
    }
}

/// FR0067 — the bare name of a type declaring two representations, written
/// where a type may appear.
///
/// The rule: a name with two declarations is disambiguated only by a marker
/// written **at that position**. Every position carries its own claim
/// ([`Type::Claimed`] for a use site, [`Function::parent_claim`] for an
/// `impl` block's self type) and nothing flows between them, so a position
/// with no marker names two declarations and resolves to neither. A type with
/// one declaration has nothing to disambiguate, which is why a bare name
/// stays legal there and the marker stays optional.
///
/// A **member's** `Self` never reaches here: it is substituted for the `impl`
/// block's self type as written, marker included, so `Self` in
/// `impl Locked<Doc>` resolves exactly as `Locked<Doc>` does. A **field's**
/// does, because a declaration writes no marker of its own — and a recursive
/// field of a dual type is genuinely two readings (a nested value, or a handle
/// to a sub-object), which is the bare-name rule and not an exception to it.
fn dual_use_site_diagnostic(
    name: &str,
    opaques: &HashMap<String, Model>,
    ctx: &str,
    may_be_self: bool,
) -> Diagnostic {
    let [a, b] = dual_claims(opaques, name);
    // Named rather than described, because the author may not have written
    // `{name}` at all — a field written `Self` arrives here as `Doc`.
    let written = if may_be_self {
        " If this position is written `Self`: a declaration names its own type, and \
         writes no representation marker, so `Self` arrives here as the bare name — \
         which names both halves. Write the marker."
    } else {
        ""
    };
    Diagnostic {
        code: "FR0067",
        message: format!(
            "{ctx}: `{name}` is declared `{}` and `{}`, so it crosses as two Dart \
             classes and this position does not say which. Write `{}<{name}>` or \
             `{}<{name}>`.{written}",
            describe_claim(a),
            describe_claim(b),
            claim_marker(a),
            claim_marker(b),
        ),
    }
}

/// One of the six representation keywords, for an FR0062 message.
fn describe_claim(c: Claim) -> &'static str {
    match c {
        Claim::Data => "data",
        Claim::Model(Model::Confined) => "confined",
        Claim::Model(Model::Resident) => "resident",
        Claim::Model(Model::Frozen) => "frozen",
        Claim::Model(Model::Locked) => "locked",
        Claim::Model(Model::Actor) => "actor",
    }
}

/// FR0062 — a representation-marker wrapper (`Locked<Point>` in a type
/// position, or `impl Locked<Point>`'s self type) claims a representation
/// that does not match what `name` actually declared. Shared by
/// `resolve_type`'s [`Type::Claimed`] arm and `check_function`'s impl
/// self-type check — the same rule, two call sites, because the self type
/// is a bare name that never reaches `resolve_type` (see
/// `parse::self_type_and_claim`).
///
/// No-op when `claim == actual`: the ordinary, expected case — the marker
/// is optional, and agreement produces no diagnostic.
fn check_representation_claim(
    claim: Claim,
    actual: Claim,
    name: &str,
    ctx: &str,
    diags: &mut Vec<Diagnostic>,
) {
    if claim == actual {
        return;
    }
    diags.push(Diagnostic {
        code: "FR0062",
        message: format!(
            "{ctx}: `{name}` is declared `{}`, not `{}` — this marker claims a \
             representation `{name}` does not have. It is optional; either drop \
             it or fix the mismatch",
            describe_claim(actual),
            describe_claim(claim),
        ),
    });
}

/// FR0003 — a name no declaration of this interface answers to.
///
/// One function because there are two places a name is resolved: the ordinary
/// pass below, and the type arguments of a generic instantiation
/// ([`Expansion::resolve_arg`]), which are resolved earlier because the
/// instantiation's identity is formed from them.
fn unknown_type(ctx: &str, name: &str) -> Diagnostic {
    Diagnostic {
        code: "FR0003",
        message: format!(
            "{ctx}: unknown type `{name}`. Bridged types must be declared \
             with #[bridge(data)] (data) or a handle representation — \
             #[bridge(confined)] / #[bridge(frozen)] / #[bridge(locked)] / \
             #[bridge(actor)] — in a declared bridge file. A type from \
             another crate crosses in the shape you want it to have in \
             Dart: as a handle, a newtype around it declared with one of \
             those four, with a #[bridge] \
             impl delegating the methods you need; as an encoded value, that \
             same newtype under `#[bridge(bytes(...))]` with a `BytesCodec`; \
             as data, a #[bridge(data)] struct restating the fields you \
             want, converted with `From` — including a tuple struct, which \
             crosses positionally, but its field type still has to be one \
             this interface bridges."
        ),
    }
}

fn resolve_type(ty: &mut Type, scope: &Scope<'_>, ctx: &str, diags: &mut Vec<Diagnostic>) {
    let Scope {
        structs,
        enums,
        opaques,
        externs,
        duals,
        generics,
        self_name,
        templates,
    } = *scope;
    match ty {
        Type::Named(n) if duals.contains(n.as_str()) => {
            let as_self = self_name == Some(n.as_str());
            diags.push(dual_use_site_diagnostic(n, opaques, ctx, as_self));
        }
        Type::Named(n) => {
            let name = n.clone();
            *ty = if structs.contains(&name) {
                Type::Struct(name)
            } else if enums.contains(&name) {
                Type::Enum(name)
            } else if opaques.contains_key(&name) {
                Type::Opaque(name)
            } else if externs.contains(&name) {
                Type::Extern(name)
            } else if generics.contains(&name) {
                return;
            } else if templates.contains(&name) {
                diags.push(Diagnostic {
                    code: "FR0073",
                    message: format!(
                        "{ctx}: `{name}` is generic and is written here with no type \
                         arguments. A template is not a type — it has no fields the wire \
                         can describe until its parameters are bound — so every use writes \
                         them out (`{name}<i64>`), and the instantiations codegen emits are \
                         exactly the ones the signatures name"
                    ),
                });
                return;
            } else {
                diags.push(unknown_type(ctx, &name));
                return;
            };
        }
        // A representation-marker wrapper: resolve the inner type first (so
        // an unresolved name still gets its FR0003), then compare the claim
        // against what the resolved inner type actually is, and erase the
        // wrapper either way — `Locked<Point>` and a bare `Point` must
        // produce the identical resolved `Type`, matching or not, so an
        // emitter downstream never has to know a marker was ever written.
        // A marker on a type declaring two representations *selects* one, so
        // the claim has to be read before the inner name is resolved — the
        // ordinary path below resolves first and would refuse the bare inner
        // (the rule above) before ever reading the claim.
        Type::Claimed(claim, inner)
            if matches!(inner.as_ref(), Type::Named(n) if duals.contains(n.as_str())) =>
        {
            let claim = *claim;
            let Type::Named(name) = inner.as_ref() else {
                unreachable!("the guard matched a `Named` inner")
            };
            let name = name.clone();
            let [a, b] = dual_claims(opaques, &name);
            *ty = if claim == a {
                Type::Struct(name)
            } else if claim == b {
                Type::Opaque(name)
            } else {
                diags.push(Diagnostic {
                    code: "FR0062",
                    message: format!(
                        "{ctx}: `{name}` is declared `{}` and `{}`, not `{}` — this marker \
                         claims a representation `{name}` does not have. Write \
                         `{}<{name}>` or `{}<{name}>`",
                        describe_claim(a),
                        describe_claim(b),
                        describe_claim(claim),
                        claim_marker(a),
                        claim_marker(b),
                    ),
                });
                // Neither half, so nothing here can pick one. Left as the
                // written name, which every later rule treats as unresolved.
                Type::Named(name)
            };
        }
        Type::Claimed(claim, inner) => {
            let claim = *claim;
            resolve_type(inner, scope, ctx, diags);
            match inner.as_ref() {
                Type::Struct(name) | Type::Enum(name) => {
                    check_representation_claim(claim, Claim::Data, name, ctx, diags);
                }
                Type::Opaque(name) => {
                    if let Some(model) = opaques.get(name) {
                        check_representation_claim(claim, Claim::Model(*model), name, ctx, diags);
                    }
                }
                // The inner name did not resolve; FR0003 already named it,
                // and a second diagnostic about the marker would only repeat
                // that with less information.
                Type::Named(_) => {}
                Type::Extern(name) => {
                    diags.push(Diagnostic {
                        code: "FR0062",
                        message: format!(
                            "{ctx}: `{name}` is a `#[bridge(bytes(...))]` external type, \
                             which is its own representation — none of data, confined, \
                             frozen, locked, or actor apply to it. Write `{name}` \
                             directly, with no marker"
                        ),
                    });
                }
                _ => {
                    diags.push(Diagnostic {
                        code: "FR0062",
                        message: format!(
                            "{ctx}: a representation marker names a bridged type \
                             directly (`Locked<Point>`), never a container or a \
                             composed type — this one wraps something else entirely"
                        ),
                    });
                }
            }
            *ty = (**inner).clone();
        }
        Type::List(t, _) | Type::Set(t, _) | Type::Option(t) | Type::Array(t, _)
        | Type::Boxed(t) | Type::Ref { inner: t, .. } => resolve_type(t, scope, ctx, diags),
        Type::Map(k, v, _) => {
            resolve_type(k, scope, ctx, diags);
            resolve_type(v, scope, ctx, diags);
        }
        // A handle's item and result types are ordinary types and resolve
        // like any other — this is what lets a mirror carry a bridged struct.
        Type::DartObject(spec) => {
            if let Some(t) = &mut spec.item {
                resolve_type(t, scope, ctx, diags);
            }
            if let Some(t) = &mut spec.ret {
                resolve_type(t, scope, ctx, diags);
            }
            // A fallible closure's declared error resolves here too, so a
            // mirror-only error type reaches the emitters as a `Struct`/`Enum`
            // rather than an unresolved `Named` (which they treat as
            // unreachable).
            if let Some(t) = &mut spec.err {
                resolve_type(t, scope, ctx, diags);
            }
        }
        // An application survives resolution only inside a **template**, where
        // it is open (`struct Wrapper<T> { page: Page<T> }`) — every closed one
        // was replaced by `expand_generics`. Its arguments are ordinary types
        // and the generic Dart class renders them, so they resolve like any
        // other; the head is a template name and resolves to nothing.
        Type::Tuple(ts) | Type::App(_, ts) => {
            for t in ts {
                resolve_type(t, scope, ctx, diags);
            }
        }
        _ => {}
    }
}

/// FR0004 — a handle reachable from `ty`, in a position where the crossing
/// would go Dart → Rust.
///
/// Minting is exactly-once and safe; **consuming** is what has no spelling.
/// Returning a value that holds a handle transfers each one once, and the Dart
/// wrapper that must dispose it is built by the same decode. Decoding one *out
/// of* a value Dart passed is the opposite: the handle's owner is still Dart,
/// the object's life still ends at `dispose()`, and reading its id here would
/// hand Rust a second owner for it. So this walks the whole type **graph** —
/// through struct and enum fields, not only through containers — because a
/// handle buried three declarations deep is the same violation as a bare one.
///
/// A handle behind a [`Type::Ref`] is not one of them, and the walk stops
/// there: this rule is about *transfer*, and `&Doc` inside a `Vec` transfers
/// exactly as much as `&Doc` at the top level does, which is nothing. Where a
/// borrow may sit is FR0077's rule, not this one.
fn forbid_opaque_inside(iface: &Interface, ty: &Type, ctx: &str, diags: &mut Vec<Diagnostic>) {
    lending_aware_walk(iface, ty, &mut |t| {
        if let Type::Opaque(name) = t {
            diags.push(Diagnostic {
                code: "FR0004",
                message: format!(
                    "{ctx}: opaque type `{name}` cannot be reached from here. A handle \
                     owns a Rust object that exactly one Dart object must dispose, and \
                     nothing here can hand that ownership over: this position either \
                     transfers nothing (a borrow, a data receiver) or has no decode \
                     that could mint or take one (a `bytes(...)` codec, a fixed \
                     array). What does work: **return** it — \
                     as the whole return value, inside an `Option`, a `Vec`, a set, a \
                     map (as the key, the value, or both), or a tuple, or as a field of \
                     a returned struct or enum, at any depth. Inbound, hand it over \
                     from a **by-value** parameter, which the Dart caller spells \
                     `take()`; or take it **by reference** (`&{name}`), which borrows \
                     without transferring. For several handles from one call, a \
                     returned struct carries them even when their types differ"
                ),
            });
        }
    });
}

/// FR0004 for a handle behind a reference in the **return**, wherever the
/// reference sits: the whole return (`-> &Doc`, [`Function::ret_borrow`]) or
/// inside one (`-> Option<&Doc>`). One sentence, because it is one rule.
fn borrowed_return_carries_handle(ctx: &str) -> Diagnostic {
    Diagnostic {
        code: "FR0004",
        message: format!(
            "{ctx}: a borrowed return cannot carry a handle. Returning one \
             transfers ownership to Dart, which disposes it — and a `&T` is a \
             view of something this Rust code still owns. Return the value by \
             value (the handle is minted from it), or return the data the \
             caller actually needs"
        ),
    }
}

/// FR0077 — where a reference **inside** a type may sit, and FR0013 for a
/// nested `&mut` of a value.
///
/// `at` is `None` where a borrow is legal and `Some(reason)` where it is not,
/// carrying that position's own sentence: the reasons genuinely differ, and one
/// message covering all of them would have to be vague enough to be false
/// somewhere. A borrow is read out of an `Option`, a list or a tuple at any
/// depth, because those are exactly the containers whose decode builds one
/// owned local the glue can lend a view of.
pub(crate) fn check_borrow_positions(
    ty: &Type,
    ctx: &str,
    at: Option<&'static str>,
    is_handle: &dyn Fn(&Type) -> bool,
    diags: &mut Vec<Diagnostic>,
) {
    match ty {
        Type::Ref { inner, mutable, .. } => {
            // A **handle** borrow under a set or a map is legal: it points at
            // the object a handle names, not into the container the decode
            // built, so there is nothing here for it to outlive. Judged per
            // reference rather than per element, so `HashSet<(&Doc, &str)>`
            // reports the one that is really wrong.
            let exempt =
                at == Some(borrow_at::IN_SET_MAP) && is_handle(unbox(inner));
            if let (Some(reason), false) = (at, exempt) {
                diags.push(Diagnostic {
                    code: "FR0077",
                    message: format!(
                        "{ctx}: a reference cannot sit here — {reason}. A borrow is read \
                         out of an `Option`, a list or a tuple at any depth — and, for a \
                         **handle**, out of a set or a map as well; write the owned type \
                         here"
                    ),
                });
            }
            // A `&mut` reaches through to the object only for a handle. For a
            // value the glue decoded a local and the mutation dies with it,
            // which is FR0013's rule and its reason, one level in.
            if *mutable && !is_handle(unbox(inner)) {
                diags.push(Diagnostic {
                    code: "FR0013",
                    message: format!(
                        "{ctx}: `&mut` on a value type has no effect across the bridge — \
                         the value is decoded into an owned local, so any mutation is \
                         discarded. Take it by value and return the new value"
                    ),
                });
            }
            check_borrow_positions(inner, ctx, Some(borrow_at::IN_REF), is_handle, diags);
        }
        Type::Option(t) | Type::List(t, _) => check_borrow_positions(t, ctx, at, is_handle, diags),
        Type::Tuple(ts) => {
            for t in ts {
                check_borrow_positions(t, ctx, at, is_handle, diags);
            }
        }
        // A set or a map lends a **handle** exactly as a list does; a borrowed
        // *value* in one is what stays refused. The `Ref` arm above is where
        // the two are told apart, so the reason travels down unchanged and
        // only the reference it is about is named.
        Type::Set(t, _) => {
            check_borrow_positions(t, ctx, Some(borrow_at::IN_SET_MAP), is_handle, diags)
        }
        Type::Map(k, v, _) => {
            check_borrow_positions(k, ctx, Some(borrow_at::IN_SET_MAP), is_handle, diags);
            check_borrow_positions(v, ctx, Some(borrow_at::IN_SET_MAP), is_handle, diags);
        }
        Type::Array(t, _) => check_borrow_positions(t, ctx, Some(borrow_at::IN_ARRAY), is_handle, diags),
        Type::Boxed(t) => check_borrow_positions(t, ctx, Some(borrow_at::IN_BOX), is_handle, diags),
        Type::DartObject(spec) => {
            for t in spec.item.iter().chain(spec.ret.iter()).chain(spec.err.iter()) {
                check_borrow_positions(t, ctx, Some(borrow_at::IN_MIRROR), is_handle, diags);
            }
        }
        // A declared type's own fields are checked once, at the declaration —
        // reaching them from every use site would report the same field as
        // many times as it is named.
        Type::Struct(_) | Type::Enum(_) => {}
        _ => {}
    }
}

/// The handle test [`check_borrow_positions`] uses once the IR is resolved.
///
/// A predicate rather than a match inside the walk, because the rule has a
/// second reader: the `#[bridge]`-omission heuristic in `parse.rs` asks it of
/// a signature that has not been through name resolution yet, where a handle
/// is still a [`Type::Named`]. One walk, two vocabularies.
pub(crate) fn is_bridged_handle(ty: &Type) -> bool {
    matches!(ty, Type::Opaque(_))
}

/// FR0077 at a declaration: a struct or enum field cannot hold a reference at
/// any depth. Checked once here rather than at every use site, so the field is
/// named once however many members mention its type.
fn field_takes_no_borrow(ty: &Type, ctx: &str, diags: &mut Vec<Diagnostic>) {
    check_borrow_positions(ty, ctx, Some(borrow_at::IN_FIELD), &is_bridged_handle, diags);
}

/// The reasons FR0077 gives, one per position that owns what it decodes.
/// Named constants because two of them are used from more than one call site
/// and all of them have to survive a reader asking "is that actually true".
mod borrow_at {
    pub(super) const IN_REF: &str = "a reference inside another reference has \
        nothing to point at, the intermediate one being a temporary this call builds";
    pub(super) const IN_SET_MAP: &str = "a set or a map decodes into an owned \
        container, so a borrowed **value** element would be a second container of \
        references built over the first, which is not built — a list of borrows \
        already says the same thing with one. (A borrowed *handle* is accepted \
        here: it points at the object, not into the decoded container)";
    pub(super) const IN_ARRAY: &str = "a fixed array decodes into an owned array, \
        and a borrowed element is not built";
    pub(super) const IN_BOX: &str = "a `Box` is put back on decode and owns what \
        it holds";
    pub(super) const IN_FIELD: &str = "a data class is reconstructed as an owned \
        value and the bridge drops lifetimes, so a field has nothing that outlives \
        the call to point at";
    pub(super) const IN_MIRROR: &str = "a Dart-object handle carries its value in \
        an envelope that outlives the call, so a borrow of this call's locals cannot \
        go in one";
}

/// FR0004 on the **consume** path: a handle a by-value parameter hands over
/// may sit only where the Dart surface can name one token per handle.
///
/// The accepted positions are the return path's ownable ones ([`forbid_unownable_opaque`])
/// **minus a struct or enum field the declaration did not claim**, and the
/// reason for the subtraction is the Dart side, not the wire: a generated data
/// class gives each field the Dart type a *returned* value of that Rust type
/// has, and this direction needs the type a *parameter* has — `Consumed<Doc>`
/// where the returning one needs `Doc`. One class cannot be both shapes, so the
/// declaration picks: `#[bridge(data, inbound)]` mints the parameter-shaped
/// class and gives up the return path in exchange, which is what
/// [`inbound_struct`] answers here.
///
/// A set and a map are accepted, as key, as value or both. They stage flat —
/// a `Vec` of elements, a `Vec` of pairs — so every raw is adopted before the
/// container is rebuilt, and the container's own `Hash`/`Eq` then collapses
/// what it collapses on the *objects*, which is Rust's semantics and frees
/// what it drops. Staging the set itself would have collapsed on the raw
/// instead, and two zero-sized objects share one raw.
///
/// A `bytes(...)` codec stays refused for the reason it is on the return path:
/// its wire form is one opaque payload the user's own codec writes, with
/// nowhere for a handle to be minted or taken.
fn forbid_unconsumable_opaque(
    iface: &Interface,
    ty: &Type,
    ctx: &str,
    diags: &mut Vec<Diagnostic>,
) {
    match ty {
        Type::Opaque(_) => {}
        // A borrowed element hands nothing over, so the consume rule stops
        // here. `Vec<&Doc>` is a by-value `Vec` of borrows: the container is
        // the call's, the objects are not.
        Type::Ref { .. } => {}
        Type::Option(inner) | Type::List(inner, _) | Type::Set(inner, _) => {
            forbid_unconsumable_opaque(iface, inner, ctx, diags)
        }
        Type::Map(k, v, _) => {
            forbid_unconsumable_opaque(iface, k, ctx, diags);
            forbid_unconsumable_opaque(iface, v, ctx, diags);
        }
        Type::Tuple(ts) => {
            for t in ts {
                forbid_unconsumable_opaque(iface, t, ctx, diags);
            }
        }
        // An inbound struct IS a consume position: its Dart class is
        // parameter-shaped, so each handle field is already a `Consumed<…>` the
        // caller minted. Its own fields are not walked again — what they may
        // hold is a property of the declaration, judged once at the
        // declaration ([`check_inbound_declarations`]), not once per use.
        Type::Struct(name) if inbound_struct(iface, name).is_some() => {}
        Type::Struct(name) | Type::Enum(name) if reaches_opaque(iface, ty) => {
            // The advice names the shapes that work rather than only the
            // flattening one: a tuple already carries several handles plus
            // data with no declaration at all, and `inbound` is the spelling
            // that keeps the field names.
            let declare = if matches!(ty, Type::Struct(_)) {
                format!(
                    "declare it `#[bridge(data, inbound)]`, which makes its Dart class \
                     parameter-shaped and gives up returning `{name}` in exchange"
                )
            } else {
                "put the handle fields in a struct declared `#[bridge(data, inbound)]` \
                 and give this enum a variant carrying that struct — an inbound *enum* \
                 is not built, its Dart side being one class per variant"
                    .to_string()
            };
            diags.push(Diagnostic {
                code: "FR0004",
                message: format!(
                    "{ctx}: `{name}` reaches an opaque handle through its fields, and a \
                     handle can only be given up from a position the Dart surface can \
                     put a `Consumed<…>` in. A generated data class gives every field \
                     the Dart type a *returned* value of it has — `Doc` — and handing \
                     one over needs the type a *parameter* has, `Consumed<Doc>`. One \
                     class cannot be both shapes, and inferring which from the members \
                     that name `{name}` would let an unrelated member added elsewhere \
                     change this class. So the declaration says: {declare}. Otherwise \
                     pass the handles as their own parameters (an `Option`, a `Vec`, a \
                     set, a map and a tuple of them all work), or take `{name}` by \
                     reference, which transfers nothing"
                ),
            });
        }
        other => forbid_opaque_inside(iface, other, ctx, diags),
    }
}

/// What a `#[bridge(data, inbound)]` declaration must be for the shape it
/// claims to be buildable — three rules, all FR0004, each reported once at the
/// declaration rather than at every use of it.
///
/// A declaration refused for reaching **no handle** has its flag cleared, so
/// everything downstream reads it as the plain struct it already is. Without
/// that it would report once here and again at every position the shape rule
/// guards, burying the one diagnostic that says what to fix under N that
/// restate it. The other refusals leave the flag alone: those declarations do
/// mean what they say, and reading them as plain would make the use sites
/// advise adding an `inbound` the author has already written.
fn check_inbound_declarations(iface: &mut Interface, diags: &mut Vec<Diagnostic>) {
    let mut cleared: Vec<String> = vec![];
    // Declarations the two rules below have already refused. Their fields are
    // not judged again: a self-referential struct reaches itself through some
    // field, so the field walk would report that field for a shape reason when
    // the real problem is the cycle the first rule already named.
    let mut refused: Vec<String> = vec![];
    for s in iface.structs.iter().filter(|s| s.inbound) {
        let ty = Type::Struct(s.name.clone());
        let name = &s.name;
        // `reaches_opaque`, which follows the declaration graph, rather than
        // `taken_handles`, which follows only the inbound part of it: a handle
        // behind a *plain* field is genuinely reached, and saying "reaches no
        // handle" of such a struct would be false. That case is the field rule
        // below.
        if !reaches_opaque(iface, &ty) {
            diags.push(Diagnostic {
                code: "FR0004",
                message: format!(
                    "`{name}`: `inbound` says this struct's Dart class is \
                     parameter-shaped — every handle it reaches typed as the \
                     `Consumed<…>` a caller hands over rather than the plain handle a \
                     return carries. `{name}` reaches no handle, so the two shapes are \
                     the same class and `inbound` changes nothing about it except to \
                     forbid returning it, for no reason a reader of `{name}` could \
                     reconstruct. Drop `inbound`, or give it the handle field it was \
                     written for"
                ),
            });
            cleared.push(name.clone());
            refused.push(name.clone());
            continue;
        }
        // Self-reference. The staged decode of an inbound struct is a value
        // shaped like the struct with each handle left as its bare id, built
        // as a tuple of its fields' staged forms (`emit_rust::decode_staged`),
        // and a struct that reaches itself has no finite such shape — the
        // tuple would nest for ever. `Option<Box<Self>>` is the spelling that
        // makes a *plain* recursive data struct work, and it is exactly what
        // has no staged form here.
        let mut reaches_self = false;
        let mut seen: HashSet<&str> = HashSet::new();
        seen.insert(name.as_str());
        for f in &s.fields {
            let mut stack: Vec<&Type> = vec![&f.ty];
            while let Some(t) = stack.pop() {
                match t {
                    Type::Struct(n) if n == name => reaches_self = true,
                    Type::Struct(n) => {
                        if let Some(inner) = inbound_struct(iface, n) {
                            if seen.insert(inner.name.as_str()) {
                                stack.extend(inner.fields.iter().map(|f| &f.ty));
                            }
                        }
                    }
                    Type::Option(t) | Type::List(t, _) | Type::Set(t, _) | Type::Array(t, _)
                    | Type::Boxed(t) | Type::Ref { inner: t, .. } => stack.push(t),
                    Type::Map(k, v, _) => {
                        stack.push(k);
                        stack.push(v);
                    }
                    Type::Tuple(ts) => stack.extend(ts.iter()),
                    _ => {}
                }
            }
        }
        if reaches_self {
            diags.push(Diagnostic {
                code: "FR0004",
                message: format!(
                    "`{name}`: an `inbound` struct cannot reach itself. Its request-side \
                     form is the struct with every handle left as the bare id it arrived \
                     as, built out of its fields' own such forms — and a struct that \
                     contains itself has no finite one to build. A *plain* recursive \
                     data struct is fine, handles and all, because nothing stages it: \
                     the decode reconstructs it directly. Hold the recursive part in a \
                     plain struct and keep the handles in this one"
                ),
            });
            refused.push(name.clone());
        }
    }
    // Every field, judged by the **use-site** consume rule — the same walk a
    // by-value parameter gets, because a field of a parameter-shaped class is a
    // consume position and the two must not come to disagree about which shapes
    // can stage. It answers all three ways a field can be wrong, each with the
    // reason that position already has: a handle reached through a declaration
    // that is not itself inbound (that class is return-shaped), a handle
    // reached through a container with no staged form at all (a `Box`, a fixed
    // array, a `bytes(...)` codec, a channel envelope), and a nested inbound
    // struct, which is accepted.
    //
    // Here rather than at the use site, so a bad field reports once however
    // many members name the struct — which is why
    // [`forbid_unconsumable_opaque`]'s own inbound arm does not walk these
    // fields again.
    for s in iface.structs.iter().filter(|s| s.inbound && !refused.contains(&s.name)) {
        for f in &s.fields {
            let ctx = format!("struct `{}`, field `{}`", s.name, f.name);
            forbid_unconsumable_opaque(iface, &f.ty, &ctx, diags);
        }
    }
    for s in iface.structs.iter_mut() {
        if cleared.contains(&s.name) {
            s.inbound = false;
        }
    }
    // A **plain** declaration's field, and an enum variant's, cannot hold an
    // inbound one: that field would be parameter-shaped inside a class that is
    // return-shaped. Judged here, once per field, rather than at every use of
    // the outer type.
    let mut field_diags: Vec<Diagnostic> = vec![];
    for s in iface.structs.iter().filter(|s| !s.inbound) {
        for f in &s.fields {
            let ctx = format!("struct `{}`, field `{}`", s.name, f.name);
            forbid_inbound_shape(iface, &f.ty, &ctx, inbound_at::IN_FIELD, &mut field_diags);
        }
    }
    for e in &iface.enums {
        for v in &e.variants {
            for f in &v.fields {
                let ctx = format!("enum `{}`, variant `{}`, field `{}`", e.name, v.name, f.name);
                forbid_inbound_shape(iface, &f.ty, &ctx, inbound_at::IN_VARIANT, &mut field_diags);
            }
        }
    }
    diags.append(&mut field_diags);
}

/// FR0004 for an **inbound** declaration in a position that reads the other
/// shape.
///
/// `#[bridge(data, inbound)]` gives a data class the Dart field types a
/// *parameter* of each field's Rust type has; a plain one gets the types a
/// *return* has. The two differ exactly where a handle is reached
/// (`Consumed<Doc>` against `Doc`), so a class built in one shape cannot stand
/// where the other is read. `at` carries the position's own sentence, because
/// the reasons genuinely differ and one message covering both would have to be
/// vague enough to be false somewhere.
///
/// The walk goes through containers, tuples and a channel envelope's types, and
/// **stops at a declaration**: a plain declaration's own fields are judged
/// where they are declared, so a `Wrapper { b: Bundle }` returned from ten
/// members reports once rather than ten times.
fn forbid_inbound_shape(
    iface: &Interface,
    ty: &Type,
    ctx: &str,
    at: &str,
    diags: &mut Vec<Diagnostic>,
) {
    match ty {
        Type::Struct(name) if inbound_struct(iface, name).is_some() => {
            diags.push(Diagnostic {
                code: "FR0004",
                message: format!(
                    "{ctx}: `{name}` is declared `#[bridge(data, inbound)]`, which \
                     asks for a parameter-shaped Dart class — one that types the \
                     handles it reaches as the `Consumed<…>` a caller mints with \
                     `take()` and a call spends, rather than as the plain handle a \
                     return carries. {at}"
                ),
            });
        }
        Type::Ref { inner, .. } | Type::Option(inner) | Type::List(inner, _)
        | Type::Set(inner, _) | Type::Array(inner, _) | Type::Boxed(inner) => {
            forbid_inbound_shape(iface, inner, ctx, at, diags)
        }
        Type::Map(k, v, _) => {
            forbid_inbound_shape(iface, k, ctx, at, diags);
            forbid_inbound_shape(iface, v, ctx, at, diags);
        }
        Type::Tuple(ts) => {
            for t in ts {
                forbid_inbound_shape(iface, t, ctx, at, diags);
            }
        }
        // A channel envelope is left to [`forbid_opaque_inside`], which walks
        // the declaration graph and refuses the handle the inbound struct
        // reaches — with the reason that is actually about a channel. An
        // inbound declaration always reaches one (that is the rule that makes
        // it legal at all), so nothing gets through here unreported.
        _ => {}
    }
}

/// The sentences [`forbid_inbound_shape`] gives, one per position that reads a
/// return-shaped class. Named constants for the reason [`borrow_at`]'s are:
/// each has to survive a reader asking whether it is actually true.
mod inbound_at {
    pub(super) const IN_RETURN: &str = "A return reads the other shape, and a \
        token is not something to hand back: it is spent by the call it is passed \
        to rather than disposed, so nothing on the far side would have a \
        `dispose()` to reach the object by. Return the handles themselves, or a \
        plain data struct holding them, which is return-shaped and works at any \
        depth";
    pub(super) const IN_FIELD: &str = "This declaration is not inbound, so its own \
        class is return-shaped, and one class cannot be read both ways. Declare it \
        `inbound` as well — an inbound struct may hold another";
    pub(super) const IN_VARIANT: &str = "An enum has no `inbound` of its own — its \
        Dart side is one class per variant, and which of those would carry a token \
        is not decided — so this variant's class is return-shaped. Pass the inbound \
        struct as a parameter of its own";
}

/// The two origins of a collision, order-independent, so one pair met in two
/// scopes is recognised as the same pair.
fn pair_key(a: &str, b: &str) -> (String, String) {
    if a <= b {
        (a.to_string(), b.to_string())
    } else {
        (b.to_string(), a.to_string())
    }
}

/// Dart's reserved words. A `dart_identifier` may not be one (FR0065); the
/// derived path escapes them with a trailing `_` instead (`dart_name`), which
/// an author writing the name by hand can do too.
const DART_RESERVED: &[&str] = &[
    "assert", "break", "case", "catch", "class", "const", "continue", "default", "do", "else",
    "enum", "extends", "false", "final", "finally", "for", "if", "in", "is", "new", "null",
    "rethrow", "return", "super", "switch", "this", "throw", "true", "try", "var", "void",
    "while", "with",
];

/// The type under every `Box` — the type the crossing actually sees, since a
/// `Box` is transparent on the wire and to Dart (see [`Type::Boxed`]). A rule
/// about *what a member hands back* reads through it; a rule about what the
/// generated Rust must spell does not.
pub(crate) fn unbox(ty: &Type) -> &Type {
    match ty {
        Type::Boxed(t) => unbox(t),
        other => other,
    }
}

/// Whether a handle is reachable from `ty` through the declaration graph.
///
/// The predicate both emitters read to choose between an owned encode (which
/// mints) and a borrowing one, so it has to mean exactly what FR0004 means.
/// [`walk_type_graph`] that stops at a channel endpoint: what *this value's
/// bytes* reach. See [`reaches_opaque`], which is this with the "any opaque"
/// predicate.
pub(crate) fn walk_type_graph_no_channels<'a>(
    iface: &'a Interface,
    ty: &'a Type,
    f: &mut dyn FnMut(&'a Type),
) {
    walk_type_graph_full(iface, ty, true, false, f)
}

pub(crate) fn reaches_opaque(iface: &Interface, ty: &Type) -> bool {
    let mut found = false;
    // **Not through a channel endpoint.** Every question this answers — which
    // direction a type may travel, which codec a declaration gets, whether a
    // position transfers a handle — is about the bytes of *this* value. A
    // `StreamSink<Doc>` field carries no `Doc`: the items are a separate
    // envelope with their own encoder, minted later and reclaimed by the
    // channel's own machinery. Looking through one would refuse a struct that
    // bundles a handle-carrying sink, for a reason untrue of it.
    walk_type_graph_full(iface, ty, true, false, &mut |t| {
        if matches!(t, Type::Opaque(_)) {
            found = true;
        }
    });
    found
}

/// Whether a handle reachable from `ty` is **handed over** — reached without
/// passing through a [`Type::Ref`].
///
/// [`reaches_opaque`] cannot answer this: `Vec<&Doc>` and `Vec<Doc>` both reach
/// `Doc`, and the whole difference between them is the borrow. Every question
/// about *taking* — which parameters consume, which Dart classes get `take()`,
/// which positions the take walker visits — asks this one instead.
///
/// The walk is structural except at one declaration: an **inbound** struct
/// (`#[bridge(data, inbound)]`, [`StructDecl::inbound`]), whose fields it
/// enters. That is not an exception to the rule but the rule read literally —
/// an inbound struct's handle field IS handed over, which is what declaring the
/// direction bought. Every other declaration is opaque to this walk, because a
/// handle behind a plain struct field cannot be consumed at all (FR0004) and a
/// reference cannot sit in one (FR0077).
pub fn takes_opaque(iface: &Interface, ty: &Type) -> bool {
    !taken_handles(iface, ty).is_empty()
}

/// Whether a handle reachable from `ty` is **lent** — reached through a
/// [`Type::Ref`]. The other half of [`takes_opaque`]; a container refused by
/// A container may be both — `Vec<(&Doc, Doc)>` — and the emitter handles it
/// in one walk; what FR0078 refuses is a borrowed *value* beside a take.
pub fn lends_opaque(ty: &Type) -> bool {
    !lent_handles(ty).is_empty()
}

/// Whether a type borrows a **value** anywhere — a reference whose pointee is
/// not a handle, and so lives in the local the decode built rather than behind
/// a handle. The distinction FR0078 turns on.
pub(crate) fn borrows_a_value(ty: &Type) -> bool {
    let mut found = false;
    ty.walk(&mut |t| {
        if let Type::Ref { inner, .. } = t {
            found |= !matches!(unbox(inner), Type::Opaque(_));
        }
    });
    found
}

/// Every bridged type a value **hands over**: [`takes_opaque`]'s walk with the
/// names kept. The taking twin of [`lent_handles`], and written beside it so
/// the two cannot come to disagree about which positions transfer.
///
/// An **inbound** struct is entered, in field order, so a handle it carries is
/// named here exactly as a handle in a tuple is — which is what the whole
/// declaration means (see [`inbound_struct`]). The recursion is guarded by
/// declaration name for the reason [`walk_type_graph`]'s is: only a named
/// declaration can close a cycle, and an inbound struct may hold another.
pub(crate) fn taken_handles<'a>(iface: &'a Interface, ty: &'a Type) -> Vec<&'a str> {
    fn go<'a>(
        iface: &'a Interface,
        ty: &'a Type,
        seen: &mut HashSet<&'a str>,
        out: &mut Vec<&'a str>,
    ) {
        match ty {
            Type::Opaque(n) => out.push(n.as_str()),
            Type::Ref { .. } => {}
            Type::Option(t) | Type::List(t, _) | Type::Set(t, _) | Type::Array(t, _)
            | Type::Boxed(t) => go(iface, t, seen, out),
            Type::Map(k, v, _) => {
                go(iface, k, seen, out);
                go(iface, v, seen, out);
            }
            Type::Tuple(ts) => {
                for t in ts {
                    go(iface, t, seen, out);
                }
            }
            Type::Struct(n) => {
                let Some(s) = inbound_struct(iface, n) else { return };
                if !seen.insert(n.as_str()) {
                    return;
                }
                for f in &s.fields {
                    go(iface, &f.ty, seen, out);
                }
            }
            _ => {}
        }
    }
    let mut out = vec![];
    go(iface, ty, &mut HashSet::new(), &mut out);
    out
}

/// The declaration behind `name` when it is an **inbound** data struct, and
/// `None` otherwise — including for a name that is an enum, an opaque, or a
/// plain data struct.
///
/// One lookup, because "is this an inbound struct" is asked by the predicates
/// above, by both emitters and by the checker's own shape rule, and a second
/// spelling of it is how they would come to disagree about a declaration.
pub fn inbound_struct<'a>(iface: &'a Interface, name: &str) -> Option<&'a StructDecl> {
    iface.struct_decl(name).filter(|s| s.inbound)
}

/// Every handle position a type lends, in wire order: the bridged type name
/// and whether the borrow is `&mut`.
///
/// One walk, because the rules that read it must agree about which positions
/// exist — the checker's (FR0006 on a frozen `&mut`, FR0012 on a borrowed
/// confined handle in an async member) and the emitter's, which acquires
/// exactly these and no others.
pub(crate) fn lent_handles(ty: &Type) -> Vec<(&str, bool)> {
    fn go<'a>(ty: &'a Type, out: &mut Vec<(&'a str, bool)>) {
        match ty {
            Type::Ref { inner, mutable, .. } => {
                if let Type::Opaque(n) = unbox(inner) {
                    out.push((n.as_str(), *mutable));
                }
            }
            Type::Option(t) | Type::List(t, _) | Type::Set(t, _) => go(t, out),
            Type::Map(k, v, _) => {
                go(k, out);
                go(v, out);
            }
            Type::Tuple(ts) => {
                for t in ts {
                    go(t, out);
                }
            }
            _ => {}
        }
    }
    let mut out = vec![];
    go(ty, &mut out);
    out
}

/// FR0004 on the **return** path: a handle may sit only where an owned encode
/// exists for it.
///
/// Mirrors `emit_rust::encode_owned` position for position — the root, an
/// `Option`, a `Vec`, a set element, a map key or value, a tuple element, and a
/// struct or enum field, recursively. Every one of those consumes the container
/// it walks, so each handle is minted exactly once, which is the whole rule.
///
/// The `other` arm is the safe direction on purpose: a `Type` variant nobody
/// has taught this function about is refused rather than admitted, so a new
/// container that grows an owned encode has to be named here to be accepted.
/// What it catches today is a handle reached through a shape that has no owned
/// encode at all — which, since maps and sets grew one, is only a mirror's
/// inner types (refused again, and more specifically, by FR0031).
fn forbid_unownable_opaque(iface: &Interface, ty: &Type, ctx: &str, diags: &mut Vec<Diagnostic>) {
    fn go<'a>(
        iface: &'a Interface,
        ty: &'a Type,
        ctx: &str,
        seen: &mut HashSet<&'a str>,
        diags: &mut Vec<Diagnostic>,
    ) {
        match ty {
            Type::Opaque(_) => {}
            // A borrowed element has no owned encode — the same fact
            // `Function::ret_borrow` already states at the root, one level in.
            // The value inside is copied into the response through the
            // reference, which works for data and cannot work for a handle:
            // minting takes the object, and the body still owns it.
            Type::Ref { inner, .. } => {
                if reaches_opaque(iface, inner) {
                    diags.push(borrowed_return_carries_handle(ctx));
                }
            }
            // A fixed array is consumed by value like a `Vec`: `for x in arr`
            // desugars to `IntoIterator::into_iter(arr)`, and the by-value impl
            // for `[T; N]` has existed in every edition since 1.53 (measured:
            // it compiles under `--edition 2015`; only the `.into_iter()`
            // *method call* on an array resolves differently before 2021). So
            // the elements are owned and each handle is minted once. Admitted
            // for that reason and not by omission: nothing distinguishes
            // `[Doc; 3]` from `Vec<Doc>` here except where the length lives.
            // A `Box` is transparent: it changes neither the wire nor who owns
            // the value, so it neither creates nor removes an owned encode.
            Type::Option(inner)
            | Type::List(inner, _)
            | Type::Array(inner, _)
            | Type::Boxed(inner) => go(iface, inner, ctx, seen, diags),
            // A set and a map are consumed by value on the way out, exactly as
            // a `Vec` is, so each handle inside is minted once and reaches Dart
            // as one wrapper. A handle **key** is admitted with the rest: Dart
            // equality on a handle is identity, so a returned `Map<Doc, V>` is
            // identity-keyed — which is the same divergence a returned
            // `List<Doc>` already has on `contains`/`indexOf`, and a line drawn
            // at keys alone would have no reason behind it.
            Type::Set(inner, _) => go(iface, inner, ctx, seen, diags),
            Type::Map(k, v, _) => {
                go(iface, k, ctx, seen, diags);
                go(iface, v, ctx, seen, diags);
            }
            // A tuple is a fixed, positional shape — the generated encode
            // destructures it and owns each element, and the Dart side decodes
            // a record. It is the only *anonymous* ownable position, which is
            // what makes it the answer for two handles of different types
            // without declaring a struct for them.
            Type::Tuple(ts) => {
                for t in ts {
                    go(iface, t, ctx, seen, diags);
                }
            }
            Type::Struct(name) => {
                if !seen.insert(name.as_str()) {
                    return;
                }
                if let Some(s) = iface.struct_decl(name) {
                    for f in &s.fields {
                        go(iface, &f.ty, ctx, seen, diags);
                    }
                }
            }
            Type::Enum(name) => {
                if !seen.insert(name.as_str()) {
                    return;
                }
                if let Some(e) = iface.enums.iter().find(|e| &e.name == name) {
                    for v in &e.variants {
                        for f in &v.fields {
                            go(iface, &f.ty, ctx, seen, diags);
                        }
                    }
                }
            }
            other => forbid_opaque_inside(iface, other, ctx, diags),
        }
    }
    go(iface, ty, ctx, &mut HashSet::new(), diags);
}

/// The declared-name lookup tables `check_function` needs, bundled to keep
/// its parameter count under clippy's `too_many_arguments` threshold rather
/// than because the four belong together conceptually.
struct DeclaredNames<'a> {
    structs: &'a HashSet<String>,
    enums: &'a HashSet<String>,
    opaques: &'a HashMap<String, Model>,
    dyn_traits: &'a HashSet<String>,
    duals: &'a HashSet<String>,
}

fn check_function(
    f: &mut Function,
    ctx: &str,
    iface: &Interface,
    names: &DeclaredNames<'_>,
    caps: &Capabilities,
    diags: &mut Vec<Diagnostic>,
) {
    let DeclaredNames { structs, enums, opaques, dyn_traits, duals } = *names;
    // FR0062 — an `impl` self-type wrapper (`impl Locked<Point>`) claims a
    // representation for `f.parent`; compare it against what `parent`
    // actually declared. Independent of FR0005 below and checked first: a
    // wrong claim is wrong whether or not the named type even qualifies as a
    // handle: `impl Locked<Point>` over a *data* `Point` is FR0062, because
    // the claim disagrees with the declaration, even though the impl block
    // itself is fine — a data type carries members too.
    // On a parent declaring two representations the claim is not a
    // restatement to compare — it is what *selected* the half, and the
    // assignment pass in `check_pass` already compared it and reported.
    let parent_is_dual = f.parent.as_deref().is_some_and(|p| duals.contains(p));
    if let (Some(parent), Some(claim), false) = (&f.parent, f.parent_claim, parent_is_dual) {
        let actual = if structs.contains(parent) || enums.contains(parent) {
            Some(Claim::Data)
        } else {
            opaques.get(parent).map(|m| Claim::Model(*m))
        };
        // `None` (parent is unknown, or an extern `bytes(...)` type): no
        // FR0062 here. FR0005 below already refuses any impl block whose
        // self type has no representation of its own, unknown or extern
        // included, and it is the more specific diagnostic for "this cannot
        // be an impl target at all" — this check only ever compares a claim
        // against a *known* representation.
        if let Some(actual) = actual {
            check_representation_claim(claim, actual, parent, ctx, diags);
        }
    }
    // Erased either way, matching `Type::Claimed` — checked once, then gone.
    // A matching `impl Locked<Doc>` must produce the identical `Function` a
    // bare `impl Doc` would, so nothing downstream (including this struct's
    // own `PartialEq`, which a "wrapper is zero-cost" test relies on) can
    // tell the two apart.
    f.parent_claim = None;

    let parent_model = iface.member_handle(f).map(|o| o.model);
    // A data type carries members too. There is no model to apply and nothing
    // to keep alive: the receiver is decoded from the request like any other
    // value, the body runs against that copy, and the copy dies with the call.
    // What such a member may *do* with the copy is narrower than a handle
    // method — see the receiver rules below.
    let parent_is_data = iface.member_repr(f) == Some(Repr::Data);
    if f.parent.is_some() && parent_model.is_none() && !parent_is_data {
        diags.push(Diagnostic {
            code: "FR0005",
            message: format!(
                "{ctx}: #[bridge] impl blocks are only supported on a type this \
                 interface declares a representation for — `#[bridge(data)]`, or a \
                 handle (`#[bridge(confined)]` / `#[bridge(frozen)]` / \
                 `#[bridge(locked)]` / `#[bridge(actor)]`). A `#[bridge(bytes(...))]` \
                 type crosses through its codec and has no generated Dart class to \
                 hang members on"
            ),
        });
        return;
    }
    // A `dart_interface` struct's Dart side is an interface the *caller*
    // implements, one method per field (FR0050). A generated member there
    // would be a concrete method on a class every implementor must reimplement
    // — so the caller would have to write the body of a method the bridge is
    // supposed to answer, and whichever one they wrote would win.
    if f.parent.as_deref().is_some_and(|p| iface.is_dart_interface(p)) {
        diags.push(Diagnostic {
            code: "FR0050",
            message: format!(
                "{ctx}: a `dart_interface` type carries no bridged members. Its Dart \
                 side is an interface the caller implements — one method per field — so \
                 a generated method would be one more thing every implementor has to \
                 write, answered by them rather than by this body. Move the member to a \
                 free function taking the interface as a parameter, or drop \
                 `dart_interface` and keep it a plain data struct"
            ),
        });
        return;
    }
    // A **receiver** is decoded from the request exactly as a parameter is, so
    // it inherits the parameter rule (FR0004). A *receiverless* member is fine
    // either way: nothing decodes the parent.
    //
    // Both directions land here and the reason differs, so both are written.
    // A plain data type reaching a handle travels Rust → Dart only, and without
    // this the two features compose into generated code that cannot compile —
    // it has no decoder on the Rust side and no encoder on the Dart side, and
    // the receiver path calls both. An **inbound** one has exactly those, so
    // the refusal is about what the call would do rather than what it could
    // emit: every receiver kind is reconstructed as an owned value, `&self`
    // included, so any of them would spend the tokens the class holds — and
    // this tree spends only through `take()`, which a data class does not have.
    if parent_is_data && f.receiver.is_some() {
        if let Some(parent) = f.parent.as_deref() {
            let inbound = inbound_struct(iface, parent).is_some();
            let owns = reaches_opaque(iface, &Type::Struct(parent.to_string()))
                || reaches_opaque(iface, &Type::Enum(parent.to_string()));
            if owns {
                let why = if inbound {
                    format!(
                        "`{parent}` is declared `inbound`, so its Dart class holds a \
                         `Consumed<…>` token per handle — and a receiver is \
                         reconstructed as an owned value whatever it is written as, \
                         `&self` included, so this call would spend those tokens. \
                         Spending is spelled `take()` in this tree, and a data class \
                         has none, so the caller would write `x.{}()` and lose the \
                         objects with nothing at the call site saying so",
                        f.name
                    )
                } else {
                    format!(
                        "`{parent}` reaches an opaque handle, so it travels Rust → Dart \
                         only — and a receiver is decoded the other way, out of the \
                         request, exactly as a parameter is"
                    )
                };
                diags.push(Diagnostic {
                    code: "FR0004",
                    message: format!(
                        "{ctx}: {why}. Make this a free function that takes what it \
                         needs as a parameter, or a receiverless member on `{parent}`, \
                         which decodes nothing"
                    ),
                });
                return;
            }
        }
    }
    // A constructor is a receiverless member returning its own parent — and
    // only on a handle, where minting one is exactly what the factory is for.
    // A data type's receiverless member is a `static`: the generated class
    // already has its own `const` constructor over the fields, and a second
    // one under the Rust name would collide with it.
    //
    // Decided here rather than at parse time because the comparison is against
    // the *resolved* return type. A `-> Actor<Job>` is a representation marker
    // until `resolve_type` erases it, so the parse-time test — which matched
    // the bare `Type::Named` only — silently called the marker form a plain
    // member, and the actor rules then refused it (FR0053, FR0015) for having
    // no receiver and returning a handle.
    //
    // Read through a `Box`, for the same reason the marker is erased first: a
    // wrapper that changes nothing about the crossing does not change what
    // kind of member this is. `-> Box<Self>` hands Dart exactly what
    // `-> Self` does.
    f.is_constructor = parent_model.is_some()
        && f.receiver.is_none()
        && matches!(f.ret.as_ref().map(unbox), Some(Type::Opaque(n)) if Some(n.as_str()) == f.parent.as_deref());

    // Bridged trait methods are always called through `dyn Trait`; an
    // associated function without a receiver is not dyn-dispatchable (and
    // has no handle to be called on). The syntactic dyn-compatibility rules
    // (generic methods, Self positions) are rejected at parse time.
    let parent_is_dyn = f
        .parent
        .as_ref()
        .is_some_and(|p| dyn_traits.contains(p));
    if parent_is_dyn && f.receiver.is_none() && !f.is_actor_drop {
        diags.push(Diagnostic {
            code: "FR0022",
            message: format!(
                "{ctx}: an associated function without a receiver is not \
                 dyn-dispatchable and cannot be bridged on a trait. Construct \
                 through a free factory function returning `Box<dyn {}>`",
                f.parent.as_deref().unwrap_or_default()
            ),
        });
        // Parse flags receiver-less parent functions returning the parent as
        // constructors; that path is meaningless for traits.
        f.is_constructor = false;
    }

    // FR0057, and what is left of it: an explicitly typed receiver that is not
    // `self: Box<Self>`.
    //
    // A **handle** consumes: `self`, `mut self` and `self: Box<Self>` all say
    // the call takes the object, the Dart side gives it up through `take()`,
    // and the crossing is faithful. A `Consumed<T>` parameter is not an
    // ordinary call site, which is the whole of what the shape needed.
    //
    // A **data** type takes a by-value receiver too. Decoding the value out of
    // the request is the implicit clone every by-value data parameter already
    // performs, and a receiver is a parameter here — so `self` on a data type
    // asks for nothing the crossing was not already doing for the argument
    // beside it. `self: Box<Self>` reaches the body as `Box::new` of that same
    // local, on the rule that already makes a `Box` around a value type
    // transparent to the crossing and visible only in the generated Rust.
    //
    // An explicitly typed receiver other than `Box<Self>` is refused on either
    // representation, because the parser reads the type exactly far enough to
    // recognise that one and no further: what `self: Rc<Self>` or
    // `self: Pin<&mut Self>` should mean to the crossing is not decided, and a
    // guess would be silent.
    if f.receiver == Some(Receiver::Typed) {
        diags.push(Diagnostic {
            code: "FR0057",
            message: format!(
                "{ctx}: an explicitly typed receiver is read only far enough to \
                 recognise `self: Box<Self>`, and this is not that one. What \
                 `self: Rc<Self>` or `self: Pin<&mut Self>` should mean to the \
                 crossing is not decided, so it is refused rather than guessed at. \
                 Write `&self`, `&mut self`, `self`, or `self: Box<Self>`"
            ),
        });
    }
    // A consuming receiver on a bridged trait needs no rule of its own —
    // `self: Box<Self>`, that is.
    //
    // The hazard it used to be refused for is real and is answered rather than
    // avoided: the Dart member lives on an extension over `Consumed<T>`, Dart
    // resolves an extension from the *static* type, and `Consumed<Impl>` is
    // assignable to `Consumed<T>` — so a token holding a concrete implementor
    // does reach the trait's own glue. It arrives carrying the impl tag a
    // borrowed `&dyn T` parameter already writes, and the glue takes from the
    // registry the tag names (`emit_rust::take_trait_param`).
    //
    // `self` by value is the one that cannot, and rustc does not say so
    // anywhere the author would look. The trait stays dyn compatible —
    // `fn m(self)` is simply not dispatchable, so it is left out of the vtable
    // and `Box<dyn T>` still compiles, factory and all. What does not compile
    // is the *call*: the glue holds the `Box<dyn T>` the registry gave it, and
    // a `Box<dyn T>` is not a `T`, so `T::m(this)` is E0277 inside generated
    // code with nothing pointing back at the declaration. There is no other
    // emission to reach for either — a `dyn T` cannot be moved out of its box
    // (E0161) — so this is refused at the declaration rather than left to a
    // compiler error the author cannot place.
    if parent_is_dyn && f.receiver == Some(Receiver::Value) {
        let t = f.parent.as_deref().unwrap_or_default();
        diags.push(Diagnostic {
            code: "FR0022",
            message: format!(
                "{ctx}: a by-value `self` receiver is not dyn-dispatchable, and a \
                 bridged trait's members are reached through `Box<dyn {t}>` — which is \
                 not itself a `{t}`, so the generated glue cannot call this member \
                 (rustc: the trait bound `Box<dyn {t}>: {t}` is not satisfied). Write \
                 `self: Box<Self>`: it consumes the object exactly as `self` does and \
                 crosses as the same single handle, and it is the receiver a trait \
                 object can be moved through"
            ),
        });
    }

    // `&mut self` on a data type. Not unsafe — meaningless, and for exactly
    // the reason `&mut Point` as a *parameter* is (FR0013, below): the
    // receiver is decoded into a local the generated glue owns, so the
    // mutation lands on that local and is dropped with it. The Dart side
    // cannot help either — a generated data class's fields are all `final`,
    // so there is nowhere for a changed value to go but a new instance.
    if parent_is_data && f.receiver == Some(Receiver::RefMut) {
        diags.push(Diagnostic {
            code: "FR0013",
            message: format!(
                "{ctx}: `&mut self` on a data type has no effect across the bridge — the \
                 receiver is decoded into an owned local, so any mutation is discarded, \
                 and the generated Dart class's fields are `final` with `copyWith` as \
                 the only route to a changed value. Take `&self` and return the new \
                 value, or declare the type `#[bridge(confined)]`/`#[bridge(locked)]` if \
                 it should be an object Dart mutates in place"
            ),
        });
    }

    // Methods on Confined types derive Sync: they run on the caller, which is
    // the whole point of the model. (There is no `async` opt-out.)
    if f.receiver.is_some() && parent_model == Some(Model::Confined) {
        f.exec = Exec::Sync;
    }

    // Resident derives Sync for the same reason and one more: confined's
    // methods run on the caller because that is the model, while a resident's
    // *must* — the object has no `Send` bound, so no other thread may so much
    // as hold a reference to it. A constructor is not covered here (it has no
    // receiver); FR0079 below refuses a dispatched one rather than silently
    // making it sync, because a constructor's dispatch is the one the author
    // chose and the birthplace it decides is the whole difference between this
    // model and confined.
    if f.receiver.is_some() && parent_model == Some(Model::Resident) {
        f.exec = Exec::Sync;
    }

    // A Rust `async fn` body goes to the cooperative executor, which runs on
    // every config — so this is not a portability question. It is legal
    // wherever dispatch is the executor's: free functions and frozen/locked
    // methods. The three models with their own execution reject it: confined
    // and resident (caller-thread, sync — cannot await) and actor (its own
    // executor runs bodies one at a time, and that serialization is the model;
    // `Deferred<T>` is the opt-out).
    if f.rust_async {
        if parent_model == Some(Model::Confined) {
            diags.push(Diagnostic {
                code: "FR0029",
                message: format!(
                    "{ctx}: a Rust `async fn` cannot run on a confined type — confined \
                     methods run synchronously on the caller (one isolate) and cannot \
                     await. Use a plain `fn`, or declare the type `frozen`/`locked` \
                     (both run an `async fn` on the cooperative executor) if the body \
                     must await"
                ),
            });
        } else if parent_model == Some(Model::Resident) {
            diags.push(Diagnostic {
                code: "FR0029",
                message: format!(
                    "{ctx}: a Rust `async fn` cannot run on a resident type — resident \
                     methods run synchronously on the caller and cannot await, and the \
                     cooperative executor that would run the body polls it on whatever \
                     thread drives it, which a resident object may never be touched \
                     from. Use a plain `fn`. `frozen`/`locked` run an `async fn`, but \
                     both require `Send + Sync`; a body that must await while holding a \
                     thread-affine object belongs in `#[bridge(actor)]`"
                ),
            });
        } else if parent_model == Some(Model::Actor) {
            diags.push(Diagnostic {
                code: "FR0029",
                message: format!(
                    "{ctx}: a Rust `async fn` is not supported on an actor method — an \
                     actor already runs its bodies on its own dedicated executor, one at \
                     a time, and that serialization is the model. Write a plain `fn`; it \
                     runs on the actor's executor. \
                     For a method that must await slow work without holding the \
                     instance, return `Deferred<T>`: the body runs serialized on the \
                     executor, and the wrapped future completes the call later"
                ),
            });
        }
    }

    // FR0066 — what a Dart property can be.
    //
    // The flag changes the header and nothing else: same call, same wire, same
    // dispatch id. So the rules are exactly the shapes a property has nowhere
    // to put something. A parameter has no syntax on a getter. A `void` getter
    // reads nothing, which is what a getter is for. A cancel token rides the
    // signature (`takes_cancel_token`) and is derived from `async fn` or
    // `Deferred`, not written by the author — a getter would drop it silently,
    // so those are refused rather than quietly losing cancellation.
    //
    // Everything else passes through untouched, and each model's own rules
    // still decide the rest: a getter on a locked type is sync only with a
    // contention contract (FR0007), async on an actor (FR0014). `&mut self`
    // and a fallible getter are legal Dart and legal here.
    if f.getter {
        let bad = if !f.params.is_empty() {
            Some(
                "a Dart property takes no arguments. Drop `getter`, or move the \
                 parameters into the receiver's state",
            )
        } else if f.ret.is_none() {
            Some(
                "a Dart property must read something, and this returns `()`. Drop \
                 `getter`",
            )
        } else if f.rust_async || f.deferred {
            Some(
                "this member's Dart signature carries a `cancel:` token — every \
                 `async fn`, and every `Deferred`, is cancellable — and a property has \
                 nowhere to put it. Emitting one would drop cancellation silently. \
                 Drop `getter`, or make the body a plain `fn` (it still runs off the \
                 caller)",
            )
        } else if matches!(f.receiver, Some(Receiver::Value | Receiver::Boxed))
            && iface.receiver_handle(f).is_some()
        {
            // A property is expected to be idempotent — Effective Dart says so
            // outright, and every reader assumes it. This one destroys its
            // receiver, so the second read throws. `d.take().report()` is
            // available and says what happens. Asked of a **handle** receiver
            // only: a by-value data receiver is a decoded copy per call, so
            // reading it twice works and there is nothing to warn about.
            Some(
                "this member consumes its receiver, and a Dart property must be \
                 readable twice — reading this one again would throw, because the \
                 object is gone. Drop `getter`; the member is still reached as \
                 `x.take().name()`",
            )
        } else {
            None
        };
        if let Some(why) = bad {
            diags.push(Diagnostic {
                code: "FR0066",
                message: format!("{ctx}: {why}"),
            });
        }
    }

    // `Deferred<T>` is the actor opt-out of serialized completion: the body
    // runs on the actor's executor only for its synchronous prefix, and the
    // wrapped future answers the call later. It is meaningful nowhere else — everywhere else, an `async fn`
    // already releases everything while it waits.
    if f.deferred {
        if f.rust_async {
            diags.push(Diagnostic {
                code: "FR0038",
                message: format!(
                    "{ctx}: an `async fn` cannot return `Deferred` — an async fn's \
                     whole body already runs off the caller, so there is nothing left \
                     to defer. Return the inner type directly"
                ),
            });
        } else if parent_model != Some(Model::Actor) {
            diags.push(Diagnostic {
                code: "FR0038",
                message: format!(
                    "{ctx}: `Deferred` is the actor opt-out of serialized completion \
                     and is only supported on actor methods. Outside an actor, write an \
                     `async fn` instead — on a free function or a `frozen` type it holds \
                     nothing while it awaits, and on a `locked` type it holds that \
                     object's lock for the whole body, which is what `&mut self` across \
                     an await means. On an actor, declare the type \
                     `#[bridge(actor)]`"
                ),
            });
        } else if f.is_constructor {
            diags.push(Diagnostic {
                code: "FR0039",
                message: format!(
                    "{ctx}: an actor constructor cannot return `Deferred` — the \
                     constructor materializes the instance (and arms its teardown) on \
                     the executor before any other message can run, so there is no \
                     instance to release while it waits. Construct fast, then do slow \
                     setup in a deferred method after `spawn`"
                ),
            });
        }
    }

    // Actors are async-only by construction: a sync call into an actor would
    // block the caller on a cross-executor round trip — the jank the model
    // exists to avoid.
    if parent_model == Some(Model::Actor) && f.exec == Exec::Sync {
        diags.push(Diagnostic {
            code: "FR0014",
            message: format!(
                "{ctx}: actor methods are async-only by construction (a sync call \
                 would block the caller while the actor's executor does the work). \
                 Remove `sync`, or use model `confined` if you want caller-thread \
                 execution"
            ),
        });
    }

    // An actor member with no receiver, that is not a constructor, has nowhere
    // to run. Every actor call is dispatched to *that instance's* executor, so
    // the member needs an instance to name one; a constructor is the exception
    // because it creates the executor it then runs on.
    //
    // Nothing else catches this. rustc is content — the Rust is fine — and the
    // failure lands in generated *Dart*, where the member becomes a `static`
    // method whose body reaches for the instance field `_host`:
    //
    //     static Future<int> probe() async {
    //       final r = await _host.call(772, 0, (w) {});
    //                       ^^^^^ undefined: `_host` is per instance
    //
    // which fails at load time for the whole suite, naming a generated file.
    // Refused here so the author is told at the declaration instead.
    if parent_model == Some(Model::Actor) && f.receiver.is_none() && !f.is_constructor {
        diags.push(Diagnostic {
            code: "FR0053",
            message: format!(
                "{ctx}: an actor member with no receiver has no executor to run \
                 on — every actor call is dispatched to the instance's own \
                 executor, and only a constructor may have none (it creates the \
                 executor it runs on). Take `&self`, or make it a free function"
            ),
        });
    }

    // Actor boundaries carry value types only. An actor's objects live in
    // its own executor (on web: a separate wasm instance with separate
    // memory), so handles cannot cross between an actor and anything else.
    if parent_model == Some(Model::Actor) {
        for p in &f.params {
            // Structural, not just the root: a handle a parameter lends
            // (`Vec<&Doc>`) or hands over (`Vec<Doc>`) crosses the boundary
            // exactly as a bare one does.
            //
            // Two positions, opposite directions, and their reasons are
            // opposite too — so they are told apart rather than sharing one
            // sentence. A handle the parameter *carries* comes from outside and
            // the actor cannot reach it. A handle a **channel item** carries
            // goes the other way: the actor would mint it, in its own instance,
            // and the Dart wrapper's `dispose()` calls the *main* instance's
            // `frustrate_drop_<T>` — a different registry in a different linear
            // memory. That is a wrong-object free, and no reclaim compensates
            // for it; it is why this rule, not FR0018, is what keeps a channel
            // handle sound.
            //
            // The inbound half stops at a channel and the outbound half is
            // *only* channels, so the two partition the handles a parameter
            // reaches and each message is true of what it names. Both walk the
            // declaration graph: a handle behind a struct field crosses the
            // boundary exactly as a bare one does, in either direction.
            let inbound = reaches_opaque(iface, &p.ty);
            let outbound = opaque_in_a_channel_item(iface, &p.ty, &|t| {
                matches!(t, Type::Opaque(_))
            });
            if outbound {
                diags.push(Diagnostic {
                    code: "FR0015",
                    message: format!(
                        "{ctx}, parameter `{}`: an actor's channel cannot carry an \
                         opaque handle out. The actor would mint it in its own \
                         instance — on web, a separate wasm module with its own linear \
                         memory and its own handle registry — and the Dart wrapper \
                         that disposes it calls the *main* instance's \
                         `frustrate_drop_<T>`, freeing an unrelated address. Send the \
                         data by value, and mint handles from a free function or a \
                         non-actor type",
                        p.name
                    ),
                });
            }
            if inbound {
                diags.push(Diagnostic {
                    code: "FR0015",
                    message: format!(
                        "{ctx}, parameter `{}`: opaque handles cannot cross an actor \
                         boundary — an actor's executor (on web, a separate wasm \
                         instance) cannot reach objects owned elsewhere. Pass the \
                         data by value, or construct it inside the actor",
                        p.name
                    ),
                });
            }
        }
        if !f.is_constructor {
            if let Some(ret) = &f.ret {
                // The declaration graph, not the structure: a handle in a
                // field of a returned struct is bound to this executor exactly
                // as a bare one is.
                walk_type_graph(iface, ret, &mut |t| {
                    if matches!(t, Type::Opaque(_)) {
                        diags.push(Diagnostic {
                            code: "FR0015",
                            message: format!(
                                "{ctx}: an actor method returns value types only — a \
                                 returned handle would be bound to this actor's \
                                 executor, and a handle value does not record which \
                                 executor owns it. Return a value type; mint handles \
                                 from a free function or a non-actor type"
                            ),
                        });
                    }
                });
            }
        }
    } else {
        // Actor handles never appear on non-actor functions either: the
        // global runtime cannot route to an actor's executor.
        for p in &f.params {
            // The two directions, split as in the actor-parent arm above and
            // walking the declaration graph for the same reason: an actor
            // instance *travelling in* is one the callee cannot route to; one a
            // channel would hand *out* is one nothing could have created here,
            // because a spawn is a constructor. Say which.
            let is_actor =
                |t: &Type| matches!(t, Type::Opaque(n) if opaques.get(n.as_str()) == Some(&Model::Actor));
            let mut inbound = false;
            walk_type_graph_no_channels(iface, &p.ty, &mut |t| inbound |= is_actor(t));
            let outbound = opaque_in_a_channel_item(iface, &p.ty, &is_actor);
            if outbound {
                diags.push(Diagnostic {
                    code: "FR0015",
                    message: format!(
                        "{ctx}, parameter `{}`: an actor instance cannot be sent out \
                         through a channel. Each one is created by its own constructor \
                         — that is what spawns the executor it lives on — so there is \
                         nothing here to send, and a handle value does not record which \
                         executor owns it",
                        p.name
                    ),
                });
            }
            if inbound {
                diags.push(Diagnostic {
                    code: "FR0015",
                    message: format!(
                        "{ctx}, parameter `{}`: an actor handle is bound to its own \
                         executor and cannot be passed to other bridge functions",
                        p.name
                    ),
                });
            }
        }
        if let Some(ret) = &f.ret {
            walk_type_graph(iface, ret, &mut |t| {
                if matches!(t, Type::Opaque(n) if opaques.get(n.as_str()) == Some(&Model::Actor))
                {
                    diags.push(Diagnostic {
                        code: "FR0015",
                        message: format!(
                            "{ctx}: actor instances are created only by their own \
                             constructors (each spawn creates an executor); other \
                             functions cannot return them"
                        ),
                    });
                }
            });
        }
    }

    // A resident object never leaves the thread that made it. Both halves of
    // that are this one rule, because they are one fact about the model:
    // `handle::resident_new` has no `Send` bound (runtime/rust/src/handle.rs),
    // so nothing but the calling thread may hold the value, hold a reference
    // to it, or run its `Drop`.
    //
    // A dispatched member — `Exec::Async` — runs its body on a pool worker, so
    // it is refused on both sides of the signature:
    //
    //   * a **parameter** would lend or move the object onto that worker.
    //     Stronger than FR0012, which lets a *consumed* confined handle through
    //     because `T: Send` licenses the move into the future. Resident has no
    //     such licence, so a consume is refused with the borrows.
    //   * a **return** would build the object there. This is the birthplace
    //     rule, and it is what a resident constructor meets: write
    //     `#[bridge(sync)]`.
    //
    // Structural, and through the declaration graph on the return, for the
    // reasons FR0015 above is: a handle inside a `Vec` or behind a returned
    // struct's field crosses the same thread boundary a bare one does.
    if f.exec == Exec::Async {
        let is_resident =
            |n: &str| opaques.get(n) == Some(&Model::Resident);
        for p in &f.params {
            let mut hit = false;
            p.ty.walk(&mut |t| hit |= matches!(t, Type::Opaque(n) if is_resident(n)));
            if hit {
                diags.push(Diagnostic {
                    code: "FR0079",
                    message: format!(
                        "{ctx}, parameter `{}`: a resident handle cannot reach an async \
                         member — the body runs on a pool worker, and a resident object \
                         has no `Send` bound, so no other thread may hold it or a \
                         reference to it. Mark the member `#[bridge(sync)]` so it runs \
                         on the caller. If the object really must be reached off the \
                         caller's thread, `resident` is the wrong model for it: \
                         `#[bridge(actor)]` owns a thread and needs no bound, while \
                         `frozen`/`locked` need `Send + Sync`",
                        p.name
                    ),
                });
            }
        }
        if let Some(ret) = &f.ret {
            let mut hit = false;
            walk_type_graph(iface, ret, &mut |t| {
                hit |= matches!(t, Type::Opaque(n) if is_resident(n));
            });
            if hit {
                diags.push(Diagnostic {
                    code: "FR0079",
                    message: format!(
                        "{ctx}: a resident object is born on the caller, so the member \
                         that mints one must run there — this one is dispatched, so its \
                         body runs off the caller (a pool worker, or the cooperative \
                         executor for an `async fn`) and would build the value there \
                         before handing it back. That transfer is exactly what \
                         `confined` requires `Send` for, and resident has no `Send`. \
                         Mark it `#[bridge(sync)]`"
                    ),
                });
            }
        }
    }

    // The birthplace rule's third position, and it holds on **every** member,
    // not only a dispatched one: a channel item.
    //
    // A `StreamSink`/`DartCallback` is `Send + Sync + 'static` and storable, so
    // the thread that encodes an item — and therefore mints a handle in it — is
    // not a property of this declaration at all. A sink handed to a sync member
    // may be parked and fed from a pool thread later, which is the whole point
    // of the stored-sink pattern. A resident minted there could then only be
    // freed there, and Dart's `dispose()` runs on the isolate's own thread:
    // `resident_drop` refuses a foreign thread and *strands* the object rather
    // than freeing it. So there is no thread this can be right on, and the
    // refusal is unconditional.
    {
        let is_resident = |t: &Type| {
            matches!(t, Type::Opaque(n) if opaques.get(n.as_str()) == Some(&Model::Resident))
        };
        for p in &f.params {
            if opaque_in_a_channel_item(iface, &p.ty, &is_resident) {
                diags.push(Diagnostic {
                    code: "FR0079",
                    message: format!(
                        "{ctx}, parameter `{}`: a resident handle cannot cross a \
                         channel. A resident object is built and freed by one thread, \
                         and a channel endpoint is `Send + Sync` and outlives the call \
                         — a stored sink is fed from wherever its producer runs — so \
                         which thread mints an item is not something this declaration \
                         says. Freeing one from another thread does not free it: the \
                         object is stranded. Send a value type, or pick a model that \
                         crosses threads: `frozen`/`locked` (which need `Send + Sync`) \
                         or `actor` (which owns a thread)",
                        p.name
                    ),
                });
            }
        }
    }

    // Frozen types are immutable after construction.
    if f.receiver == Some(Receiver::RefMut) && parent_model == Some(Model::Frozen) {
        diags.push(Diagnostic {
            code: "FR0006",
            message: format!(
                "{ctx}: `&mut self` on a frozen type. Frozen means immutable after \
                 construction; use model `confined` (single-owner mutation) or \
                 `locked` (shared mutation) instead"
            ),
        });
    }

    // Locked sync access is the contract-marked opt-in — for an access that
    // takes a *guard*. A consuming receiver takes none: `Arc::try_unwrap` is a
    // compare-exchange and `into_inner` moves the value out of a cell, so
    // nothing is acquired and nothing is released (`handle::locked_take`).
    // There is therefore no contention to have a policy about, and an
    // `on_contention` written on one anyway is FR0010, as on any other member
    // that acquires nothing.
    let locked_sync_receiver = matches!(f.receiver, Some(Receiver::Ref | Receiver::RefMut))
        && parent_model == Some(Model::Locked)
        && f.exec == Exec::Sync;
    // A locked handle lent from inside a container takes a guard exactly as a
    // top-level one does — `Vec<&Doc>` takes as many as the caller sent — so it
    // is the same contract and the same rule.
    let locked_sync_params = f.params.iter().any(|p| {
        (matches!(&p.ty, Type::Opaque(n) if opaques.get(n) == Some(&Model::Locked))
            && p.borrow != Borrow::Value)
            || lent_handles(&p.ty)
                .iter()
                .any(|(n, _)| opaques.get(*n) == Some(&Model::Locked))
    }) && f.exec == Exec::Sync;
    if (locked_sync_receiver || locked_sync_params) && f.on_contention.is_none() {
        diags.push(Diagnostic {
            code: "FR0007",
            message: format!(
                "{ctx}: synchronous access to a locked type requires an explicit \
                 contention contract. Add `on_contention = \"error\"` (try-lock; a \
                 contended call throws ContentionError naming this method) or \
                 `on_contention = \"block\"` (blocking lock; native targets only), \
                 or make the call async (the default), which awaits the lock safely \
                 on every platform"
            ),
        });
    }
    if f.on_contention == Some(OnContention::Block) && !caps.blocking_allowed {
        diags.push(Diagnostic {
            code: "FR0008",
            message: format!(
                "{ctx}: `on_contention = \"block\"` is not available on this target \
                 (blocking the calling thread is fatal on the web main thread). Make \
                 the call async, which acquires and releases the lock on a worker"
            ),
        });
    }
    // The declared half of `no_block`. What a body actually calls is past
    // codegen's ceiling and is proven at link time instead; these two catch the cases where the source
    // already says, in frustrate's own vocabulary, that the claim is false.
    //
    // **Only `block`.** The claim's own words are contradicted by an
    // acquisition that waits, on every target, and this is the only check such
    // a member gets: it is `requires_native`, so it is cfg'd out of the wasm
    // build and `emit_block_checks` emits no root for it. Not hypothetical —
    // before this rule existed, `bazel/wasm_block_check` reported the guard
    // release reaching `futex_wait` from a claimed sync locked member, a true
    // finding delivered as a wasm call trace.
    //
    // The try-lock is **not** refused, and the reason is what `no_block`
    // actually means in this tree. There is one operative definition, not two:
    // it is what `bazel/wasm_block_check` verifies on wasm — no wait
    // instruction reachable from what the caller runs — and what placement
    // settles everywhere else. Members whose `executor::spawn` and
    // `pool::spawn` take an internal `std::sync::Mutex` in the caller's own
    // frame are already settled green under it (`emit_rust::claims`). "Never
    // waits on any platform" was a description of that definition written when
    // every accepted member happened to satisfy it, not the definition itself.
    //
    // A sync `on_contention = "error"` member conforms: its acquisition is a
    // compare-exchange and its release reaches no wait instruction
    // (`frustrate::handle::LockedCell`). Refusing it also foreclosed the one
    // *mechanical* proof of that, because the member is in the wasm build now
    // — `Dispatch::Caller`, so `claims` settles it `Artifact` and a root
    // reaches the whole body, release included.
    // `tests/bazel_rules/locked_fixture` is that root.
    if f.no_block && f.on_contention == Some(OnContention::Block) {
        diags.push(Diagnostic {
            code: "FR0048",
            message: format!(
                "{ctx}: `no_block` claims the calling thread is never stalled here, but \
                 `on_contention = \"block\"` declares that the acquisition waits for the \
                 lock — which is that claim contradicted in its own words. Pick one: drop \
                 the claim, take `on_contention = \"error\"` (which refuses instead of \
                 waiting, and keeps the claim provable), or make the call async, which \
                 acquires and releases the lock where waiting is legal"
            ),
        });
    }
    // A value-returning `DartFunction` parameter is answered by Dart, and the
    // invoking thread waits for that answer — the same fact that already makes
    // such a member `requires_native`. An `async fn` is exempt: it awaits the
    // reply cooperatively (`call_async`) rather than parking.
    if f.no_block
        && !f.rust_async
        && f.params.iter().any(|p| {
            reachable_handles(iface, &p.ty)
                .iter()
                .any(|spec| spec.is_returning())
        })
    {
        diags.push(Diagnostic {
            code: "FR0049",
            message: format!(
                "{ctx}: `no_block` claims this body never waits, but it takes a \
                 value-returning Dart callback, and invoking one from a \
                 non-`async fn` body blocks the calling thread until Dart answers. \
                 Make it an `async fn` (which awaits the reply instead of parking), \
                 or drop the claim"
            ),
        });
    }
    if f.on_contention.is_some() && f.exec == Exec::Async {
        diags.push(Diagnostic {
            code: "FR0009",
            message: format!(
                "{ctx}: `on_contention` has no meaning on an async call (async \
                 acquisition always waits safely); remove it or add `sync`"
            ),
        });
    }
    if f.on_contention.is_some()
        && !(locked_sync_receiver || locked_sync_params)
        && f.exec == Exec::Sync
    {
        diags.push(Diagnostic {
            code: "FR0010",
            message: format!(
                "{ctx}: `on_contention` only applies to calls that access a \
                 locked type"
            ),
        });
    }

    for p in &f.params {
        let pctx = format!("{ctx}, parameter `{}`", p.name);
        // Where a reference inside the type may sit (FR0077), and a nested
        // `&mut` of a value (FR0013). A borrow is legal at the top of a
        // parameter type, so the walk starts admitting one.
        check_borrow_positions(&p.ty, &pctx, None, &is_bridged_handle, diags);
        // FR0078 — a borrowed **value** inside a container the call also takes
        // from.
        //
        // A container that lends a *handle* beside a take is built: the
        // reference points at the object the handle names, so the argument walk
        // can consume the container it is reading. A borrowed **value** cannot
        // be, and the difference is where its pointee lives — inside that very
        // container, which the walk moves the taken objects out of. The
        // reference would point at a local the walk has already given away.
        //
        // Split into two parameters (one borrowing, one taking), or take the
        // value by value: it is decoded into a local either way.
        if p.borrow == Borrow::Value && takes_opaque(iface, &p.ty) && borrows_a_value(&p.ty) {
            diags.push(Diagnostic {
                code: "FR0078",
                message: format!(
                    "{pctx}: this parameter takes a handle and borrows a value from the \
                     same container. The borrowed value lives *in* that container, and \
                     the argument is built by moving the taken objects out of it — so the \
                     reference would point at something the same walk has given away. (A \
                     borrowed **handle** beside a take is fine: it points at the object, \
                     not into the container.) Take the value by value, which decodes into \
                     a local either way, or split them into two parameters"
                ),
            });
        }
        // The model rules for a handle lent from inside a container are the
        // ones a top-level borrow gets, position for position — the glue takes
        // the same reference through the same accessor, and where it sits in the
        // parameter's type changes nothing about what the model allows.
        for (name, mutable) in lent_handles(&p.ty) {
            let Some(model) = opaques.get(name).copied() else {
                continue;
            };
            if model == Model::Frozen && mutable {
                diags.push(Diagnostic {
                    code: "FR0006",
                    message: format!(
                        "{pctx}: `&mut {name}` on a frozen type; frozen is immutable \
                         after construction"
                    ),
                });
            }
            if model == Model::Confined && f.exec == Exec::Async {
                diags.push(Diagnostic {
                    code: "FR0012",
                    message: format!(
                        "{pctx}: a confined type cannot be borrowed by an async \
                         function — the call would run on a pool thread while the owner \
                         keeps using the object (confined = one isolate, caller-thread \
                         execution). Mark the function #[bridge(sync)], or declare \
                         `{name}` as locked (shared, async-safe)"
                    ),
                });
            }
        }
        // An explicitly named lifetime on a borrowed parameter. The parser
        // dropped these silently for as long as every borrowed value type was
        // decoded into an owned local — nothing could go wrong because nothing
        // was actually borrowed from the wire. A sync `&[u8]`/`&str` now IS
        // borrowed from the request buffer, and the dispatch's own slice comes
        // from `request_slice`, whose `'a` is unbound (it is conjured from a
        // raw pointer). A user writing `&'static [u8]` would unify `'a` with
        // `'static` and get a dangling reference that compiles.
        //
        // Rejected rather than "handled", because there is no lifetime a user
        // could name that would mean anything here: the request buffer lives
        // for the call and for nothing longer. Rejected for `&mut` and for
        // opaque handles too — the same silence was there, and a rule that
        // fires on only the currently-dangerous spelling is a rule someone has
        // to re-derive the next time a decode changes shape.
        if let Some(lt) = &p.ref_lifetime {
            diags.push(Diagnostic {
                code: "FR0044",
                message: format!(
                    "{pctx}: remove the explicit lifetime `{lt}` — a borrowed parameter \
                     points at something that lives for exactly this call: the request \
                     buffer, a local the decode built, or the object behind a handle. A \
                     named lifetime cannot be honoured (`{lt}` would unify with the \
                     unbound lifetime the dispatch conjures its references with, and \
                     compile a dangling reference); write the elided form"
                ),
            });
        }
        // A param whose name equals a generated wire-protocol local can shadow
        // it — `call_id` in particular type-checks and completes the *wrong*
        // future with no error. Reject; renaming is trivial.
        if matches!(
            p.name.as_str(),
            "r" | "w" | "call_id" | "ret" | "this" | "req" | "resp"
        ) {
            diags.push(Diagnostic {
                code: "FR0028",
                message: format!(
                    "{pctx}: parameter name `{}` collides with a generated wire-protocol \
                     local (it could silently complete the wrong call). Rename it — e.g. \
                     `{}_`",
                    p.name, p.name
                ),
            });
        }
        match &p.ty {
            Type::Opaque(name) => {
                let model = opaques[name];
                // A by-value trait-typed parameter (`Box<dyn T>`) needs no rule
                // of its own: the tag it already carries names the registry,
                // and the take behind each tag is generated
                // (`emit_rust::take_trait_param`).
                if model == Model::Frozen && p.borrow == Borrow::RefMut {
                    diags.push(Diagnostic {
                        code: "FR0006",
                        message: format!(
                            "{pctx}: `&mut {name}` on a frozen type; frozen is \
                             immutable after construction"
                        ),
                    });
                }
                // FR0012 is about a **borrow**: the call would run on a pool
                // thread while the owner keeps using the object. A *consumed*
                // confined handle is exempt, and not by exception — there is
                // no owner left to race with. The glue takes the `Box` on the
                // calling thread and moves the value into the future, which
                // `T: Send` (the confined bound) already licenses.
                if model == Model::Confined
                    && f.exec == Exec::Async
                    && p.borrow != Borrow::Value
                {
                    diags.push(Diagnostic {
                        code: "FR0012",
                        message: format!(
                            "{pctx}: a confined type cannot be borrowed by an async \
                             function — the call would run on a pool thread while the \
                             owner keeps using the object (confined = one isolate, \
                             caller-thread execution). Mark the function \
                             #[bridge(sync)], take the handle by value (`{name}`), \
                             which consumes it and leaves no owner to race with, or \
                             declare `{name}` as locked (shared, async-safe)"
                        ),
                    });
                }
            }
            // Dart-object handles are judged by the handle rules below
            // (FR0018/FR0020/FR0031), which speak about a channel rather than
            // about data. Falling through to the value-type checks would
            // report an opaque *inside* a handle as a plain opaque-in-a-param
            // (FR0004), burying the specific diagnostic under a generic one.
            Type::DartObject(_) => {}
            _ => {
                // An *immutable* borrow of a value type is free: the glue
                // decodes into a local it owns and lends that local for the
                // length of the call, so nothing can dangle and the body reads
                // exactly what it would have read by value. (`&str` and
                // `&[u8]` go further on a sync arm and borrow the request
                // buffer itself; see `build_call_parts`.)
                //
                // `&mut` is refused: the mutation lands on that local and is
                // dropped with it, which is the footgun this exists for — and
                // the same rule `&mut self` on a data type gets.
                if p.borrow == Borrow::RefMut {
                    diags.push(Diagnostic {
                        code: "FR0013",
                        message: format!(
                            "{pctx}: `&mut` on a value type has no effect across the \
                             bridge — the value is decoded into an owned local, so any \
                             mutation is discarded. Take it by value and return the \
                             new value"
                        ),
                    });
                }
                // A **by-value** parameter that reaches a handle consumes it,
                // and the positions that can carry one are the positions with
                // an owned decode — the mirror of the return path's rule.
                // A borrowed one transfers nothing and stays refused
                // everywhere.
                if p.borrow == Borrow::Value {
                    forbid_unconsumable_opaque(iface, &p.ty, &pctx, diags);
                } else {
                    forbid_opaque_inside(iface, &p.ty, &pctx, diags);
                }
            }
        }
    }

    // Dart-object handles. All of these ride the
    // interface-aware type-graph walk, so a handle buried in a struct field
    // is judged exactly like a top-level one — the point of modelling
    // endpoints as data. (FR0016 "at most one sink" and FR0017 "a sink
    // forbids a return value" are gone: those restrictions existed only
    // because endpoints could not compose.)
    for p in &f.params {
        let pctx = format!("{ctx}, parameter `{}`", p.name);
        for spec in reachable_handles(iface, &p.ty) {
            // A channel's **item** may carry an opaque handle: that direction
            // is Rust → Dart, the same one a member's return travels, and the
            // droppability of the envelope is paid for rather than refused —
            // the producer does not mint for a channel it knows is dead
            // (`StreamSink::add` tests liveness before encoding), and an item
            // that is encoded and then undeliverable is freed on whichever side
            // dropped it: from the writer's mint ledger (`codec::Minted`) where
            // Rust refused the post, and by the router's tombstone reclaim
            // where Dart absorbed the item.
            //
            // What keeps that sound on web is FR0015 and FR0079, not this rule:
            // an actor mints into its own wasm instance, and a resident must be
            // freed by the thread that built it. Both refuse a channel item.
            //
            // The **reply** half of a `DartFunction` — its `R`, and the `E` of
            // a declared failure — stays refused, and not for the envelope's
            // sake. A handle there travels Dart → Rust, which is a *transfer
            // out of a Dart wrapper*: the object has an owner already, and
            // giving it up is the `Consumed<…>` opt-in on the Dart side plus
            // the staged decode and duplicate check on the Rust side (FR0004's
            // machinery). A closure's reply frame has neither, so a handle in
            // one would be freed twice or never.
            for ty in spec.ret.iter().chain(spec.err.iter()) {
                walk_type_graph(iface, ty, &mut |t| {
                    if let Type::Opaque(name) = t {
                        diags.push(Diagnostic {
                            code: "FR0018",
                            message: format!(
                                "{pctx}: a Dart method's result cannot carry the opaque \
                                 type `{name}`. A result travels Dart → Rust, so the \
                                 handle would be given up by the Dart object holding \
                                 it — and a handle is only given up from a position \
                                 the Dart surface can put a `Consumed<{name}>` in, \
                                 against a decode that stages every id and rejects \
                                 duplicates before taking any. A closure's reply frame \
                                 has neither. Return a value type; the closure's \
                                 *argument* may carry `{name}` (Rust → Dart, as a \
                                 stream item may), and a member's own return still \
                                 hands handles out"
                            ),
                        });
                    }
                });
            }
            // A value-returning method blocks the invoking worker unless the
            // body can `.await` it (`call_async`). A sync member's body runs
            // on the very application thread that must run the Dart closure,
            // so invoking it there is a deadlock, not a wait.
            // A fallible closure is a returning one — `Result<(), E>` included:
            // it has a reply frame, so it parks the caller just the same.
            if spec.is_returning() && f.exec == Exec::Sync {
                diags.push(Diagnostic {
                    code: "FR0020",
                    message: format!(
                        "{pctx}: a value-returning Dart method cannot be reached from \
                         a sync member — its body runs on the application thread, and \
                         invoking the method there would deadlock (the Dart side needs \
                         that thread). Make the member an `async fn` and await it via \
                         `call_async` (the portable path — runs on native and web), or \
                         use a DartCallback or another fire-and-forget mirror"
                    ),
                });
            }
            // FR0035, the mirror direction. The declared error becomes an
            // `EException` the Dart closure throws and an `Err(E)` the Rust body
            // matches on, so it must be something both sides can hold by value —
            // the same fact, and the same code, as a member's own typed error.
            if let Some(err) = &spec.err {
                match err {
                    Type::Struct(_) | Type::Enum(_) => {}
                    // FR0003 (unknown) or FR0056 (a generic parameter) already
                    // fired for the name.
                    Type::Named(_) => {}
                    other => diags.push(Diagnostic {
                        code: "FR0035",
                        message: format!(
                            "{pctx}: `{}` cannot be the declared error of a DartFunction. \
                             It crosses by value — Dart throws the generated exception \
                             carrying it and Rust receives `Err` — so it must be a struct \
                             or enum declared in this interface. To let any throw be this \
                             call's panic instead, write `DartFunction<T, R>`",
                            error_type_label(other)
                        ),
                    }),
                }
            }
        }
    }
    // (No web-facts backstop for a returning method: `call_async` is
    // portable, and a member that can only use the blocking `call` — a
    // non-`async fn` — is already `requires_native`, so the web pass never
    // sees it. The former FR0019 would now wrongly reject a portable
    // `async fn` member, so it is gone.)

    // FR0031 — directionality. Rust cannot mint a Dart object, so a handle
    // can only travel Dart → Rust. Enforcing it keeps the Rust-encode and
    // Dart-decode arms *provably* dead rather than merely untested, and it
    // is what lets the emitters omit them entirely.
    let mut forbid_handle = |ty: &Type, what: &str, how: &str| {
        for _ in reachable_handles(iface, ty) {
            diags.push(Diagnostic {
                code: "FR0031",
                message: format!(
                    "{ctx}: a Dart-object handle cannot be reached from {what} — a \
                     handle is a Rust-held reference to a Dart object, and Rust cannot \
                     create one. {how}"
                ),
            });
        }
    };
    if let Some(ret) = &f.ret {
        forbid_handle(
            ret,
            "a return type",
            "Take the handle as a parameter instead; the caller supplies the object.",
        );
    }
    for p in &f.params {
        // A handle reachable from *another* handle's item or result is the
        // same violation one level in: Rust would have to hand Dart an
        // object it cannot make.
        for spec in reachable_handles(iface, &p.ty) {
            if let Some(item) = &spec.item {
                forbid_handle(
                    item,
                    "a stream item or method argument of another handle",
                    "Pass the handles as separate parameters — they compose freely \
                     side by side.",
                );
            }
            if let Some(ret) = &spec.ret {
                forbid_handle(
                    ret,
                    "the result of a Dart method",
                    "Return a value type; pass any further channels as parameters.",
                );
            }
            if let Some(err) = &spec.err {
                forbid_handle(
                    err,
                    "the declared error of a Dart method",
                    "Declare a value-typed error; pass any further channels as parameters.",
                );
            }
        }
    }

    // Returning a handle transfers ownership to Dart, exactly once per handle
    // encoded — so the question here is not *whether* but *where*: only the
    // positions with an owned encode.
    if let Some(ret) = &f.ret {
        // A borrow nested in the return is read where it still points at
        // something — the encode runs in the same scope as the call — so the
        // positions are the parameter's (FR0077).
        check_borrow_positions(ret, &format!("{ctx}, return type"), None, &is_bridged_handle, diags);
        // An **inbound** declaration is refused here whatever it holds: the
        // question is the shape of its Dart class, not whether this particular
        // position could encode the handles behind it.
        forbid_inbound_shape(
            iface,
            ret,
            &format!("{ctx}, return type"),
            inbound_at::IN_RETURN,
            diags,
        );
        // A **borrowed** return has no owned encode anywhere in it: the value
        // is copied out through the reference, and minting a handle needs the
        // object itself. That is not a gap to fill either — a returned handle
        // is one Dart owns and disposes, and a `&Doc` the author still owns
        // cannot become one without deciding what happens to the original.
        if f.ret_borrow && reaches_opaque(iface, ret) {
            diags.push(borrowed_return_carries_handle(ctx));
        } else {
            forbid_unownable_opaque(iface, ret, &format!("{ctx}, return type"), diags);
        }
    }
}

#[cfg(test)]
mod tests {
    /// A caveat says what the web body means *less*. A member the web surface
    /// never emits has no web body, and one that throws has no meaning at all —
    /// so in both cases the caveat is a claim about something that is not
    /// there, which is a mistake rather than a no-op.
    /// The positive control: an ordinary portable member takes one.
    /// A typed error must be a value. An opaque crosses as a handle its
    /// receiver must dispose, and handing one out on the failure path makes
    /// every `catch` a resource-management obligation.
    #[test]
    fn an_opaque_error_type_is_rejected_saying_why_it_cannot_be_one() {
        let e = check(
            crate::parse::parse_source(
                "#[bridge(frozen)] pub struct Conn { pub x: i32 }\n                 #[bridge(sync)] pub fn f() -> Result<i64, Conn> { Ok(0) }",
                "crate::api",
            )
            .unwrap(),
        )
        .unwrap_err();
        let msg = format!("{e:?}");
        assert!(msg.contains("FR0035"), "{msg}");
        assert!(msg.contains("dispose"), "names the lifecycle problem: {msg}");
        assert!(msg.contains("anyhow::Result"), "names the way out: {msg}");
    }

    #[test]
    fn a_primitive_error_type_is_rejected() {
        let e = check(
            crate::parse::parse_source(
                "#[bridge(sync)] pub fn f() -> Result<i64, i32> { Ok(0) }",
                "crate::api",
            )
            .unwrap(),
        )
        .unwrap_err();
        assert!(format!("{e:?}").contains("FR0035"));
    }

    /// The positive controls: both kinds of bridged value type are accepted.
    #[test]
    fn a_bridged_enum_or_struct_error_type_is_accepted() {
        for decl in [
            "#[bridge(data)] pub enum SendError { NoSuchPeer, Closed }",
            "#[bridge(data)] pub struct SendError { pub code: i32 }",
        ] {
            let iface = check(
                crate::parse::parse_source(
                    &format!("{decl}\n#[bridge(sync)] pub fn f() -> Result<i64, SendError> {{ Ok(0) }}"),
                    "crate::api",
                )
                .unwrap(),
            )
            .expect(decl);
            let f = iface.functions.iter().find(|f| f.name == "f").unwrap();
            assert!(matches!(
                f.err,
                Some(crate::ir::Type::Enum(_)) | Some(crate::ir::Type::Struct(_))
            ));
        }
    }

    /// The generated class name is a name in the same file as the interface's
    /// own types, so a collision is a generated file that does not compile.
    #[test]
    fn an_error_type_whose_exception_class_collides_is_rejected() {
        let e = check(
            crate::parse::parse_source(
                "#[bridge(data)] pub enum Send { A }\n                 #[bridge(data)] pub struct SendException { pub x: i32 }\n                 #[bridge(sync)] pub fn f() -> Result<i64, Send> { Ok(0) }",
                "crate::api",
            )
            .unwrap(),
        )
        .unwrap_err();
        let msg = format!("{e:?}");
        assert!(msg.contains("FR0036"), "{msg}");
        assert!(msg.contains("SendException"), "{msg}");
    }

    /// `typed_error_names` runs before resolution and matches directly on
    /// `Type::Named`, because `Type::Claimed` did not exist when it was
    /// written. A claimed error type (`Result<T, Data<Send>>`) must still be
    /// found through the wrapper — `peel_claim` is what makes that so —
    /// or a collision like this one would reach the emitters undetected.
    #[test]
    fn a_wrapped_error_type_still_collides() {
        let e = check(
            crate::parse::parse_source(
                "#[bridge(data)] pub enum Send { A }\n                 #[bridge(data)] pub struct SendException { pub x: i32 }\n                 #[bridge(sync)] pub fn f() -> Result<i64, Data<Send>> { Ok(0) }",
                "crate::api",
            )
            .unwrap(),
        )
        .unwrap_err();
        let msg = format!("{e:?}");
        assert!(msg.contains("FR0036"), "{msg}");
        assert!(msg.contains("SendException"), "{msg}");
    }

    /// The runtime exceptions are re-exported by every surface, so they are
    /// just as unavailable as a locally declared name.
    #[test]
    fn an_error_type_colliding_with_a_runtime_exception_is_rejected() {
        let e = check(
            crate::parse::parse_source(
                "#[bridge(data)] pub enum Contention { A }\n                 #[bridge(sync)] pub fn f() -> Result<i64, Contention> { Ok(0) }",
                "crate::api",
            )
            .unwrap(),
        )
        .unwrap_err();
        let msg = format!("{e:?}");
        assert!(msg.contains("FR0036"), "{msg}");
        assert!(msg.contains("re-export"), "{msg}");
    }

    use super::*;
    use crate::parse::parse_source;

    fn check_src(src: &str) -> Result<Interface, Vec<Diagnostic>> {
        check(parse_source(src, "crate::api").unwrap())
    }

    fn codes(r: Result<Interface, Vec<Diagnostic>>) -> Vec<&'static str> {
        r.err()
            .map(|ds| ds.into_iter().map(|d| d.code).collect())
            .unwrap_or_default()
    }

    const SCENE: &str = "#[bridge(resident)] pub struct Scene { n: std::rc::Rc<i64> }\n";

    /// FR0079's **birthplace** half. A resident object is built by whoever
    /// calls the member, so a dispatched one would build it on a pool worker
    /// — the transfer confined pays for with `Send` and resident does not
    /// have. It fires on a constructor and on any other member that mints one,
    /// at any depth of the return, because `encode_owned` mints wherever the
    /// handle sits.
    #[test]
    fn a_dispatched_member_may_not_mint_a_resident() {
        for ret in ["Scene", "Vec<Scene>", "Option<Scene>", "anyhow::Result<Scene>"] {
            let src = format!("{SCENE}#[bridge] pub fn make() -> {ret} {{ todo!() }}");
            assert!(
                codes(check_src(&src)).contains(&"FR0079"),
                "{ret}: {:?}",
                codes(check_src(&src))
            );
            // The same shape on the caller is exactly what the model wants.
            let ok = format!("{SCENE}#[bridge(sync)] pub fn make() -> {ret} {{ todo!() }}");
            assert!(codes(check_src(&ok)).is_empty(), "{ret}: {:?}", codes(check_src(&ok)));
        }
    }

    /// FR0079's **use** half, and the way it is stronger than FR0012. Confined
    /// exempts a *consumed* handle on an async member because `T: Send`
    /// licenses moving the value into the future. Resident has no `Send`, so
    /// the consume is refused with the borrows.
    #[test]
    fn a_resident_reaches_no_async_member_borrowed_or_consumed() {
        for param in ["&Scene", "&mut Scene", "Scene", "Vec<&Scene>", "Vec<Scene>"] {
            let src = format!("{SCENE}#[bridge] pub fn f(s: {param}) -> i64 {{ todo!() }}");
            assert!(
                codes(check_src(&src)).contains(&"FR0079"),
                "{param}: {:?}",
                codes(check_src(&src))
            );
        }
        // The confined twin, for contrast: the consume is *allowed* there.
        let confined = "#[bridge(confined)] pub struct D { n: i64 }\n                        #[bridge] pub fn f(d: D) -> i64 { todo!() }";
        assert!(codes(check_src(confined)).is_empty(), "{:?}", codes(check_src(confined)));
    }

    /// Members derive sync, like confined's, so a plain `#[bridge]` method is
    /// not a refusal — it is a caller-thread call.
    #[test]
    fn resident_members_derive_sync() {
        let src = format!("{SCENE}#[bridge] impl Scene {{ #[bridge] pub fn n(&self) -> i64 {{ 0 }} }}");
        let iface = check_src(&src).expect("accepted");
        let m = iface.functions.iter().find(|f| f.name == "n").expect("the member");
        assert_eq!(m.exec, Exec::Sync);
    }

    /// A Rust `async fn` body has nowhere to run on a resident type, and the
    /// refusal says why rather than pointing at confined's reason.
    #[test]
    fn a_rust_async_fn_on_a_resident_type_is_refused() {
        let src = format!(
            "{SCENE}#[bridge] impl Scene {{ #[bridge] pub async fn n(&self) -> i64 {{ 0 }} }}"
        );
        let ds = check_src(&src).unwrap_err();
        let d = ds.iter().find(|d| d.code == "FR0029").expect("FR0029");
        assert!(d.message.contains("resident"), "{}", d.message);
        assert!(d.message.contains("actor"), "names where such a body belongs: {}", d.message);
    }

    /// A resident `dyn` trait needs no supertrait: there is no thread bound to
    /// inherit. FR0021 is confined/frozen/locked's rule and must not spread.
    #[test]
    fn a_resident_trait_needs_no_send_supertrait() {
        let src = "#[bridge(resident)] pub trait Sink { #[bridge(sync)] fn push(&self, n: i64); }";
        assert!(codes(check_src(src)).is_empty(), "{:?}", codes(check_src(src)));
        // The confined twin still demands one.
        let confined = "#[bridge(confined)] pub trait Sink { #[bridge(sync)] fn push(&self, n: i64); }";
        assert!(codes(check_src(confined)).contains(&"FR0021"));
    }

    /// A resident handle crossing an actor boundary is refused by FR0015,
    /// which already speaks about every handle rather than about a model — so
    /// resident needed no extension there, and this pins that it did not.
    #[test]
    fn a_resident_may_not_cross_into_an_actor() {
        let src = format!(
            "{SCENE}#[bridge(actor)] pub struct Job {{ n: i64 }}\n             #[bridge] impl Job {{ pub fn new() -> Self {{ todo!() }}              pub fn run(&self, s: &Scene) -> i64 {{ todo!() }} }}"
        );
        assert!(codes(check_src(&src)).contains(&"FR0015"), "{:?}", codes(check_src(&src)));
    }

    /// `#[bridge(data, inbound)]` — every position it is admitted in, and
    /// every one it is not, with the code each refusal carries.
    ///
    /// The rule is one sentence — an inbound class is parameter-shaped where a
    /// plain one is return-shaped — so the matrix is what says the sentence was
    /// applied everywhere rather than at the one position it was written for.
    #[test]
    fn an_inbound_struct_travels_dart_to_rust_and_nowhere_else() {
        let decl = "#[bridge(confined)] pub struct Doc { x: i64 } \
                    #[bridge(data, inbound)] pub struct B { pub d: Doc }";
        // Admitted: the bare parameter, and every container a bare handle is
        // consumable from. Nothing here is special-cased per container — the
        // struct is a consume position, so it composes where one does.
        for sig in [
            "pub fn f(b: B) {}",
            "pub fn f(b: Vec<B>) {}",
            "pub fn f(b: Option<B>) {}",
            "pub fn f(b: (B, i64)) {}",
            "pub fn f(b: Vec<Option<B>>) {}",
        ] {
            let src = format!("{decl} #[bridge(sync)] {sig}");
            assert!(check_src(&src).is_ok(), "{sig}: {:?}", check_src(&src).err());
        }
        // Refused, each with the reason for that position.
        for (sig, needle) in [
            ("pub fn f() -> B { todo!() }", "A return reads the other shape"),
            ("pub fn f() -> Vec<B> { todo!() }", "A return reads the other shape"),
            ("pub fn f() -> Option<B> { todo!() }", "A return reads the other shape"),
            (
                "pub fn f(b: &B) {}",
                "opaque type `Doc` cannot be reached from here",
            ),
        ] {
            let src = format!("{decl} #[bridge(sync)] {sig}");
            let ds = check_src(&src).unwrap_err();
            let d = ds.iter().find(|d| d.code == "FR0004").unwrap_or_else(|| panic!("{sig}: {ds:?}"));
            assert!(d.message.contains(needle), "{sig}: {}", d.message);
        }
        // A field of a plain struct, and a variant of an enum: refused at the
        // declaration, once, rather than at every use of the outer type.
        let plain = format!("{decl} #[bridge(data)] pub struct W {{ pub b: B }} \
                             #[bridge(sync)] pub fn f(w: W) {{}} \
                             #[bridge(sync)] pub fn g(w: W) {{}}");
        let ds = check_src(&plain).unwrap_err();
        let field: Vec<_> = ds
            .iter()
            .filter(|d| d.message.contains("This declaration is not inbound"))
            .collect();
        assert_eq!(field.len(), 1, "reported once per field, not per use: {ds:?}");
        assert!(field[0].message.contains("struct `W`, field `b`"), "{:?}", field[0]);

        let en = format!("{decl} #[bridge(data)] pub enum E {{ V(B) }} \
                          #[bridge(sync)] pub fn f(e: E) {{}}");
        let ds = check_src(&en).unwrap_err();
        assert!(
            ds.iter().any(|d| d.message.contains("An enum has no `inbound` of its own")),
            "{ds:?}"
        );
    }

    /// The two declarations `inbound` may not describe, each refused where the
    /// author wrote it rather than where someone later used it.
    #[test]
    fn an_inbound_declaration_must_have_a_direction_to_mean() {
        // Nothing to point: the two shapes coincide, so the keyword changes
        // nothing about the class and only forbids a position.
        let ds = check_src(
            "#[bridge(data, inbound)] pub struct B { pub n: i64 } \
             #[bridge(sync)] pub fn f(b: B) {}",
        )
        .unwrap_err();
        assert_eq!(ds.len(), 1, "{ds:?}");
        assert_eq!(ds[0].code, "FR0004");
        assert!(ds[0].message.contains("reaches no handle"), "{}", ds[0].message);

        // Self-referential: the staged form is the struct's fields with each
        // handle left as an id, and that has no finite spelling here.
        let ds = check_src(
            "#[bridge(confined)] pub struct Doc { x: i64 } \
             #[bridge(data, inbound)] pub struct B { pub d: Doc, pub next: Option<Box<B>> } \
             #[bridge(sync)] pub fn f(b: B) {}",
        )
        .unwrap_err();
        assert_eq!(ds.len(), 1, "one diagnostic, at the declaration: {ds:?}");
        assert!(ds[0].message.contains("cannot reach itself"), "{}", ds[0].message);
        // The plain twin is still fine, handles and all — nothing stages it.
        assert!(check_src(
            "#[bridge(confined)] pub struct Doc { x: i64 } \
             #[bridge(data)] pub struct B { pub d: Doc, pub next: Option<Box<B>> } \
             #[bridge(sync)] pub fn f() -> B { todo!() }",
        )
        .is_ok());

        // A field is judged by the same consume rule a by-value parameter is,
        // so every way one can be wrong reports at the declaration, once, with
        // that position's own reason. The `Box` and the fixed array are the
        // ones the emitter cannot stage: both were accepted here until the
        // walk was shared, and codegen panicked rather than refusing them.
        for (field, needle) in [
            // Reached through a declaration that is not itself inbound: that
            // class is return-shaped inside a parameter-shaped one.
            ("pub p: P", "declare it `#[bridge(data, inbound)]`"),
            // Through an enum, which has no `inbound` of its own.
            ("pub e: E", "an inbound *enum* is not built"),
            // Through a shape with no staged form at all.
            ("pub d: Box<Doc>", "cannot be reached from here"),
            ("pub d: [Doc; 2]", "cannot be reached from here"),
            ("pub d: Option<Box<Doc>>", "cannot be reached from here"),
        ] {
            let src = format!(
                "#[bridge(confined)] pub struct Doc {{ x: i64 }} \
                 #[bridge(data)] pub struct P {{ pub d: Doc }} \
                 #[bridge(data)] pub enum E {{ V(Doc) }} \
                 #[bridge(data, inbound)] pub struct B {{ {field} }} \
                 #[bridge(sync)] pub fn f(b: B) {{}}"
            );
            let ds = check_src(&src).unwrap_err();
            let d = ds
                .iter()
                .find(|d| d.code == "FR0004" && d.message.contains("struct `B`, field"))
                .unwrap_or_else(|| panic!("{field}: {ds:?}"));
            assert!(d.message.contains(needle), "{field}: {}", d.message);
        }
        // And the two a field may be: a handle, and another inbound struct.
        assert!(check_src(
            "#[bridge(confined)] pub struct Doc { x: i64 } \
             #[bridge(data, inbound)] pub struct I { pub d: Doc } \
             #[bridge(data, inbound)] pub struct B { pub d: Doc, pub inner: I, pub many: Vec<I> } \
             #[bridge(sync)] pub fn f(b: B) {}",
        )
        .is_ok());

        // A **channel endpoint** field is not one of those cases, and the
        // reason is what the struct's own bytes carry. A `DartCallback<Doc>`
        // field puts no `Doc` in `B`: the items are a separate envelope, minted
        // when the producer sends and reclaimed by the channel if they are never
        // delivered. So the field is legal — it is legal on a plain `data`
        // struct too — and what `inbound` has to say about `B` is nothing,
        // which is the struct-level refusal rather than a field one.
        let ds = check_src(
            "#[bridge(confined)] pub struct Doc { x: i64 } \
             #[bridge] impl Doc { #[bridge(sync)] pub fn x(&self) -> i64 { 0 } } \
             #[bridge(sync)] pub fn make() -> Doc { todo!() } \
             #[bridge(data, inbound)] pub struct B { pub cb: frustrate::DartCallback<Doc> } \
             #[bridge(sync)] pub fn f(b: B) {}",
        )
        .unwrap_err();
        assert!(
            ds.iter().any(|d| d.code == "FR0004"
                && d.message.contains("`B` reaches no handle")),
            "{ds:?}"
        );
        assert!(
            !ds.iter().any(|d| d.message.contains("struct `B`, field")),
            "the field itself is fine: {ds:?}"
        );
        // Without `inbound` the same struct is accepted outright.
        assert!(check_src(
            "#[bridge(confined)] pub struct Doc { x: i64 } \
             #[bridge] impl Doc { #[bridge(sync)] pub fn x(&self) -> i64 { 0 } } \
             #[bridge(sync)] pub fn make() -> Doc { todo!() } \
             #[bridge(data)] pub struct B { pub cb: frustrate::DartCallback<Doc> } \
             #[bridge(sync)] pub fn f(b: B) {}",
        )
        .is_ok());
    }

    /// A receiver on an inbound struct, in all three spellings. The reason is
    /// the same for each and is about the receiver rather than the wire: the
    /// value is reconstructed as owned whatever `self` is written as.
    #[test]
    fn an_inbound_struct_carries_no_receiver() {
        for recv in ["&self", "&mut self", "self"] {
            let src = format!(
                "#[bridge(confined)] pub struct Doc {{ x: i64 }} \
                 #[bridge(data, inbound)] pub struct B {{ pub d: Doc }} \
                 #[bridge] impl B {{ #[bridge(sync)] pub fn m({recv}) -> i64 {{ 0 }} }} \
                 #[bridge(sync)] pub fn f(b: B) {{}}"
            );
            let ds = check_src(&src).unwrap_err();
            let d = ds.iter().find(|d| d.code == "FR0004").unwrap_or_else(|| panic!("{recv}: {ds:?}"));
            assert!(d.message.contains("would spend those tokens"), "{recv}: {}", d.message);
        }
        // A receiverless member is fine: nothing decodes the parent.
        assert!(check_src(
            "#[bridge(confined)] pub struct Doc { x: i64 } \
             #[bridge(data, inbound)] pub struct B { pub d: Doc } \
             #[bridge] impl B { #[bridge(sync)] pub fn m() -> i64 { 0 } } \
             #[bridge(sync)] pub fn f(b: B) {}",
        )
        .is_ok());
    }

    /// A **channel** field beside a handle field. On a plain data type the two
    /// handle kinds pull in opposite directions and the declaration is refused;
    /// on an inbound one they pull the same way, so it is not.
    #[test]
    fn an_inbound_struct_may_hold_both_handle_kinds() {
        let ds = check_src(
            "#[bridge(confined)] pub struct Doc { x: i64 } \
             #[bridge(data)] pub struct S { pub d: Doc, pub sink: frustrate::StreamSink<i64> } \
             #[bridge(sync)] pub fn f(s: S) {}",
        )
        .unwrap_err();
        assert!(
            ds.iter().any(|d| d.message.contains("opposite directions")),
            "the plain declaration is still refused: {ds:?}"
        );
        assert!(
            check_src(
                "#[bridge(confined)] pub struct Doc { x: i64 } \
                 #[bridge(data, inbound)] pub struct S { pub d: Doc, pub sink: frustrate::StreamSink<i64> } \
                 #[bridge(sync)] pub fn f(s: S) {}",
            )
            .is_ok(),
            "an inbound declaration sends both kinds the one way"
        );
    }

    /// FR0046 — the three ways a declared type name can be a lie, each with
    /// the reason that actually applies to it. `String` and `BigInt` prove
    /// this is not a time-mapping rule: both holes predate it.
    #[test]
    fn a_declared_type_may_not_take_a_claimed_or_shadowing_name() {
        // Claimed on every path: unreachable.
        for name in ["Duration", "String", "StreamSink", "Result"] {
            let src = format!(
                "#[bridge(data)] pub struct {name} {{ pub x: i64 }} \
                 #[bridge(sync)] pub fn f() {{}}"
            );
            let ds = check_src(&src).unwrap_err();
            assert!(codes(Err(ds.clone())).contains(&"FR0046"), "{name} accepted");
            assert!(
                ds.iter().any(|d| d.message.contains("on every path")),
                "{name}: {ds:?}"
            );
        }
        // Claimed bare only: reachable through a qualified path, but the bare
        // spelling everyone writes means the mapping.
        for name in ["SystemTime", "TimeDelta", "Instant", "UtcDateTime"] {
            let src = format!(
                "#[bridge(data)] pub struct {name} {{ pub x: i64 }} \
                 #[bridge(sync)] pub fn f() {{}}"
            );
            let ds = check_src(&src).unwrap_err();
            assert!(
                ds.iter().any(|d| d.code == "FR0046"
                    && d.message.contains("silently mean something else")),
                "{name}: {ds:?}"
            );
        }
        // Resolves correctly and still breaks, on the Dart side. `DateTime` is
        // the one the time work made reachable; `BigInt`/`Uint8List`/`List`
        // have had it all along.
        for name in ["DateTime", "BigInt", "Uint8List", "List"] {
            let src = format!(
                "#[bridge(data)] pub struct {name} {{ pub x: i64 }} \
                 #[bridge(sync)] pub fn f(v: {name}) -> {name} {{ v }}"
            );
            let ds = check_src(&src).unwrap_err();
            assert!(
                ds.iter()
                    .any(|d| d.code == "FR0046" && d.message.contains("would shadow")),
                "{name}: {ds:?}"
            );
        }
        // Names nothing claims and nothing shadows stay the user's own.
        for name in ["Vec", "Option", "Sink", "StreamController", "Cache"] {
            let src = format!(
                "#[bridge(data)] pub struct {name} {{ pub x: i64 }} \
                 #[bridge(sync)] pub fn f(v: {name}) -> {name} {{ v }}"
            );
            assert!(
                !codes(check_src(&src)).contains(&"FR0046"),
                "{name} was rejected"
            );
        }
    }

    /// Every non-actor model needs its handle type's thread bound
    /// (runtime/rust/src/handle.rs), and an erased `Box<dyn T>` can only get
    /// it from the trait's supertraits.
    #[test]
    fn traits_must_declare_their_models_thread_bound() {
        for model in ["frozen", "locked"] {
            let ds = codes(check_src(&format!(
                "#[bridge({model})] pub trait T {{ fn f(&self) -> i64; }}"
            )));
            assert_eq!(ds, vec!["FR0021"], "{model}");
            let ds = codes(check_src(&format!(
                "#[bridge({model})] pub trait T: Send {{ fn f(&self) -> i64; }}"
            )));
            assert_eq!(ds, vec!["FR0021"], "{model}: Send alone is not enough");
            assert!(
                check_src(&format!(
                    "#[bridge({model})] pub trait T: Send + Sync {{ fn f(&self) -> i64; }}"
                ))
                .is_ok(),
                "{model}"
            );
        }
        // Confined is Send-only: one owner, serialized use, so the object is
        // born on one thread and used on another but never shared at one time.
        // A confined trait object still crosses that birth, so `Send` is not
        // optional — the one-isolate contract governs use, not birthplace.
        let ds = codes(check_src(
            "#[bridge(confined)] pub trait T { fn f(&mut self) -> i64; }",
        ));
        assert_eq!(ds, vec!["FR0021"]);
        assert!(check_src(
            "#[bridge(confined)] pub trait T: Send { fn f(&mut self) -> i64; }"
        )
        .is_ok());
        // ...and `Sync` is not demanded of it.
        assert!(check_src(
            "#[bridge(confined)] pub trait T: Send + Sync { fn f(&mut self) -> i64; }"
        )
        .is_ok());
    }

    #[test]
    fn actor_traits_are_rejected() {
        let ds = codes(check_src(
            "#[bridge(actor)] pub trait T { fn f(&self) -> i64; }",
        ));
        assert!(ds.contains(&"FR0023"), "{ds:?}");
    }

    #[test]
    fn receiverless_trait_fns_are_rejected() {
        let ds = codes(check_src(
            r#"
            #[bridge(confined)]
            pub trait T: Send { fn make() -> Box<dyn T>; }
            "#,
        ));
        assert_eq!(ds, vec!["FR0022"]);
    }

    #[test]
    fn rejects_mut_ref_value_types() {
        assert_eq!(
            codes(check_src("#[bridge(sync)] pub fn f(s: &mut String) {}")),
            vec!["FR0013"]
        );
        // An *immutable* borrow of String/Bytes is still allowed.
        assert!(check_src("#[bridge(sync)] pub fn g(s: &String) -> usize { s.len() }").is_ok());
    }

    #[test]
    fn byte_slice_borrow_immutable_ok_mutable_rejected() {
        // `&[u8]` is a first-class immutable byte borrow (parses to Bytes).
        assert!(check_src("#[bridge(sync)] pub fn f(x: &[u8]) -> usize { x.len() }").is_ok());
        // `&mut [u8]` stays rejected: the value is decoded into an owned local,
        // so a mutation would be silently discarded (FR0013).
        assert_eq!(
            codes(check_src("#[bridge(sync)] pub fn g(x: &mut [u8]) {}")),
            vec!["FR0013"]
        );
    }

    /// A named lifetime on a borrowed parameter is rejected, because the
    /// parser has always dropped it silently and a borrowed decode makes that
    /// silence dangerous. `request_slice` hands the dispatch a `&'a [u8]` with
    /// an unbound `'a` (it is built from a raw pointer), so a user writing
    /// `data: &'static [u8]` would unify `'a` with `'static` and get a
    /// dangling reference that compiles. There is nothing a user lifetime
    /// could usefully mean here: the request buffer lives for the call and
    /// only for the call.
    #[test]
    fn rejects_an_explicit_lifetime_on_a_borrowed_param() {
        assert_eq!(
            codes(check_src(
                "#[bridge(sync)] pub fn f(data: &'static [u8]) -> usize { data.len() }"
            )),
            vec!["FR0044"]
        );
        assert_eq!(
            codes(check_src(
                "#[bridge(sync)] pub fn f<'a>(s: &'a str) -> usize { s.len() }"
            )),
            vec!["FR0044"]
        );
        // The elided form — the only one that ever meant anything — is fine.
        assert!(check_src("#[bridge(sync)] pub fn f(data: &[u8]) -> usize { data.len() }").is_ok());
    }

    #[test]
    fn rejects_handle_method_name_collision() {
        let ds = codes(check_src(
            "#[bridge(confined)] pub struct D { x: i32 } \
             #[bridge] impl D { #[bridge(sync)] pub fn dispose(&self) {} }",
        ));
        assert!(ds.contains(&"FR0027"), "{ds:?}");
    }

    #[test]
    fn rejects_reserved_param_name() {
        assert!(codes(check_src("#[bridge(sync)] pub fn f(call_id: u64) {}")).contains(&"FR0028"));
        // An ordinary `id` param is fine (not a generated protocol local).
        assert!(check_src("#[bridge(sync)] pub fn g(id: i64) {}").is_ok());
    }

    /// FR0005 — an impl block needs a self type this interface declares a
    /// representation for. `data` is one of them: a method on a data type
    /// decodes a copy of the receiver, runs against it and drops it, so there
    /// is nothing a handle would have supplied. What is left for FR0005 is a
    /// self type with no generated class at all — a `bytes(...)` extern, whose
    /// only Dart surface is the caller's own codec.
    #[test]
    fn rejects_impl_on_a_type_with_no_generated_class() {
        let ds = codes(check_src(
            "#[bridge(bytes(dart = \"Blob\", import = \"pkg/blob.dart\"))] pub struct B; \
             #[bridge] impl B { #[bridge(sync)] pub fn f(&self) -> i64 { 0 } }",
        ));
        assert!(ds.contains(&"FR0005"), "{ds:?}");
        // A data type carries members like any other declared representation.
        assert!(check_src(
            "#[bridge(data)] pub struct D { pub x: i32 } \
             #[bridge] impl D { #[bridge(sync)] pub fn f(&self) -> i64 { 0 } }",
        )
        .is_ok());
    }

    /// The receiver rules a **data** type gets, which are not the handle
    /// ones. `&mut self` mutates a copy nobody will look at (FR0013); every
    /// other spelling of the receiver is a shape the crossing already
    /// performs on an ordinary parameter, so it is accepted.
    #[test]
    fn a_data_type_refuses_only_a_mutable_receiver() {
        let decl = "#[bridge(data)] pub struct P { pub x: i64 }";
        assert!(check_src(&format!(
            "{decl} #[bridge] impl P {{ #[bridge(sync)] pub fn n(&self) -> i64 {{ 0 }} }}"
        ))
        .is_ok());

        let ds = check_src(&format!(
            "{decl} #[bridge] impl P {{ #[bridge(sync)] pub fn bump(&mut self) {{}} }}"
        ))
        .unwrap_err();
        assert_eq!(ds.iter().map(|d| d.code).collect::<Vec<_>>(), vec!["FR0013"]);
        assert!(ds[0].message.contains("discarded"), "{}", ds[0].message);
        assert!(ds[0].message.contains("final"), "{}", ds[0].message);

        // By value, on a type with no `Copy` in sight: the receiver is decoded
        // out of the request like the parameter beside it, so `self` asks for
        // nothing the crossing was not already doing. `self: Box<Self>` is the
        // same local in a `Box`.
        for recv in ["self", "mut self", "self: Box<Self>"] {
            let src = format!(
                "{decl} #[bridge] impl P {{ #[bridge(sync)] pub fn take({recv}) -> i64 {{ 0 }} }}"
            );
            assert!(check_src(&src).is_ok(), "{src}: {:?}", check_src(&src).err());
        }

        // The handle arm is gone: `self` on a handle consumes, and the Dart
        // caller says so with `take()`.
        assert!(check_src(
            "#[bridge(confined)] pub struct D { x: i64 } \
             #[bridge] impl D { #[bridge(sync)] pub fn into_x(self) -> i64 { 0 } }",
        )
        .is_ok());
    }

    /// `take` is reserved on every handle class, including one nothing
    /// consumes — so adding a consuming member later cannot turn an app that
    /// already compiled into an error. A data class is unaffected: it has no
    /// `take()` to shadow.
    #[test]
    fn take_is_reserved_on_every_handle_class_and_on_no_data_class() {
        let ds = check_src(
            "#[bridge(confined)] pub struct D { x: i64 } \
             #[bridge] impl D { #[bridge(sync)] pub fn take(&self) -> i64 { 0 } }",
        )
        .unwrap_err();
        let d = ds.iter().find(|d| d.code == "FR0027").expect("reserved");
        assert!(d.message.contains("take"), "{}", d.message);
        assert!(check_src(
            "#[bridge(data)] pub struct P { pub x: i64 } \
             #[bridge] impl P { #[bridge(sync)] pub fn take(&self) -> i64 { 0 } }",
        )
        .is_ok());
        // `dart_identifier` is the other way onto the name, and the collision
        // is a property of the name that is *emitted*.
        let ds = check_src(
            "#[bridge(confined)] pub struct D { x: i64 } \
             #[bridge] impl D { \
               #[bridge(sync, dart_identifier = \"take\")] pub fn drain(&self) -> i64 { 0 } }",
        )
        .unwrap_err();
        assert!(ds.iter().any(|d| d.code == "FR0027"), "{ds:?}");
    }

    /// A representation marker in a constructor's return used to lose it its
    /// constructor-ness. The parse-time test compared the return against the
    /// parent by matching the *bare* name, so `-> Actor<Job>` did not match
    /// `Job` — and the actor rules then refused the member for having no
    /// receiver (FR0053) and returning a handle (FR0015). The marker is
    /// supposed to be sugar: `impl Actor<Job>` must produce what `impl Job`
    /// produces, byte for byte.
    #[test]
    fn a_marker_in_a_constructors_return_keeps_it_a_constructor() {
        let bare = check_src(
            "#[bridge(actor)] pub struct Job { n: i64 } \
             #[bridge] impl Job { pub fn new() -> Job { todo!() } }",
        )
        .unwrap();
        let marked = check_src(
            "#[bridge(actor)] pub struct Job { n: i64 } \
             #[bridge] impl Actor<Job> { pub fn new() -> Actor<Job> { todo!() } }",
        )
        .unwrap();
        assert_eq!(bare, marked);
        assert!(bare.functions.iter().any(|f| f.name == "new" && f.is_constructor));
        // A data type's receiverless member returning the parent is a static,
        // not a constructor: the class already has its own `const` one.
        let iface = check_src(
            "#[bridge(data)] pub struct P { pub x: i64 } \
             #[bridge] impl P { #[bridge(sync)] pub fn origin() -> P { todo!() } }",
        )
        .unwrap();
        assert!(!iface.functions[0].is_constructor);
    }

    /// A receiverless member returning `Option<Self>` is a **static**, not a
    /// constructor: `is_constructor` compares the return against the bare
    /// parent, and an `Option` is not it. The Dart surface is a static
    /// returning a nullable handle, which is the faithful reading — a factory
    /// that may decline has no constructor spelling in Dart either.
    #[test]
    fn a_member_returning_an_optional_self_is_a_static_not_a_constructor() {
        let iface = check_src(
            "#[bridge(confined)] pub struct D { x: i64 } \
             #[bridge] impl D { #[bridge(sync)] pub fn new() -> Self { todo!() } \
               #[bridge(sync)] pub fn open(p: String) -> Option<Self> { todo!() } \
               #[bridge(sync)] pub fn many() -> Vec<Self> { todo!() } }",
        )
        .unwrap();
        let f = |n: &str| iface.functions.iter().find(|f| f.name == n).unwrap();
        assert!(f("new").is_constructor);
        assert!(!f("open").is_constructor);
        assert!(!f("many").is_constructor);
        assert_eq!(
            f("open").ret,
            Some(Type::Option(Box::new(Type::Opaque("D".into()))))
        );
    }

    /// `-> Box<Self>` **is** a constructor. A `Box` is transparent on the wire
    /// and to Dart, so the caller receives exactly what `-> Self` gives it —
    /// the same reason a representation marker in the return keeps a
    /// constructor a constructor. Decided here rather than left to fall out of
    /// a `matches!`, which would have called it a static.
    #[test]
    fn a_box_in_a_constructors_return_keeps_it_a_constructor() {
        let boxed = check_src(
            "#[bridge(confined)] pub struct D { x: i64 } \
             #[bridge] impl D { #[bridge(sync)] pub fn new() -> Box<Self> { todo!() } }",
        )
        .unwrap();
        let bare = check_src(
            "#[bridge(confined)] pub struct D { x: i64 } \
             #[bridge] impl D { #[bridge(sync)] pub fn new() -> D { todo!() } }",
        )
        .unwrap();
        assert!(boxed.functions[0].is_constructor);
        // The IR *does* differ — the generated Rust has to spell the `Box` the
        // user wrote, so unlike a representation marker this is not erased.
        // What must not differ is anything the crossing can see, which is the
        // schema fingerprint.
        assert_eq!(boxed.functions[0].ret, Some(Type::Boxed(Box::new(Type::Opaque("D".into())))));
        assert_eq!(crate::hash::schema_hash(&boxed), crate::hash::schema_hash(&bare));
    }

    /// A by-value receiver on a data type needs no annotation, and `Copy` is
    /// not consulted: the receiver rides the request as a value and the glue
    /// decodes its own local, which is the implicit clone every by-value data
    /// parameter already performs. Nothing about the Dart value the caller
    /// holds changes, so `o.m(); o.m()` works and says nothing untrue.
    #[test]
    fn a_data_type_takes_self_by_value_with_no_annotation() {
        for decl in [
            "#[bridge(data)] pub struct P { pub x: i64 }",
            // Not `Copy`, and holding a field that could not be.
            "#[bridge(data)] pub struct P { pub x: Vec<String> }",
            "#[bridge(data)] pub enum P { A(String) }",
        ] {
            let src = format!(
                "{decl} #[bridge] impl P {{ #[bridge(sync)] pub fn take(self) -> i64 {{ 0 }} }}"
            );
            assert!(check_src(&src).is_ok(), "{src}: {:?}", check_src(&src).err());
        }
        // A handle's by-value receiver is a different thing and keeps its own
        // rules: the object *is* handed over, through the Dart `take()`.
        assert!(check_src(
            "#[bridge(confined)] pub struct D { x: i64 } \
             #[bridge] impl D { #[bridge(sync)] pub fn into_x(self) -> i64 { 0 } }",
        )
        .is_ok());
    }

    /// FR0066 — the three shapes a Dart property has nowhere to put something.
    /// Everything else passes through, and each model's own rules still decide
    /// the rest.
    #[test]
    fn a_getter_is_refused_where_a_property_has_no_room() {
        let decl = "#[bridge(frozen)] pub struct S { x: i64 }";
        for (member, needle) in [
            ("#[bridge(sync, getter)] pub fn a(&self, k: i64) -> i64 { 0 }", "takes no arguments"),
            ("#[bridge(sync, getter)] pub fn b(&self) {}", "must read something"),
            ("#[bridge(getter)] pub async fn c(&self) -> i64 { 0 }", "`cancel:` token"),
        ] {
            let ds = check_src(&format!("{decl} #[bridge] impl S {{ {member} }}")).unwrap_err();
            let d = ds
                .iter()
                .find(|d| d.code == "FR0066")
                .unwrap_or_else(|| panic!("{member}: {ds:?}"));
            assert!(d.message.contains(needle), "{member}: {}", d.message);
        }
        // Accepted: sync and async, fallible, `&mut self`, and receiverless.
        for member in [
            "#[bridge(sync, getter)] pub fn n(&self) -> i64 { 0 }",
            "#[bridge(getter)] pub fn n(&self) -> i64 { 0 }",
            "#[bridge(sync, getter)] pub fn n(&self) -> Result<i64> { todo!() }",
            "#[bridge(sync, getter)] pub fn n() -> i64 { 0 }",
        ] {
            let src = format!("{decl} #[bridge] impl S {{ {member} }}");
            assert!(check_src(&src).is_ok(), "{member}: {:?}", check_src(&src).err());
        }
        // A `locked` type's own rules still decide sync-ness: a sync getter
        // without a contention contract is FR0007, exactly as a method is.
        assert!(codes(check_src(
            "#[bridge(locked)] pub struct L { x: i64 } \
             #[bridge] impl L { #[bridge(sync, getter)] pub fn n(&self) -> i64 { 0 } }",
        ))
        .contains(&"FR0007"));
    }

    /// A receiverless member of a data type is a static, not a constructor:
    /// the generated class already has its own `const` constructor over the
    /// fields, and a second one under the Rust name would collide with it.
    #[test]
    fn a_data_types_receiverless_member_is_a_static() {
        let iface = check_src(
            "#[bridge(data)] pub struct P { pub x: i64 } \
             #[bridge] impl P { #[bridge(sync)] pub fn origin() -> P { P { x: 0 } } }",
        )
        .unwrap();
        let f = iface.functions.iter().find(|f| f.name == "origin").unwrap();
        assert!(!f.is_constructor);
        // The same shape on a handle *is* a constructor — that is the contrast.
        let iface = check_src(
            "#[bridge(confined)] pub struct D { x: i64 } \
             #[bridge] impl D { #[bridge(sync)] pub fn new() -> D { D { x: 0 } } }",
        )
        .unwrap();
        assert!(iface.functions.iter().find(|f| f.name == "new").unwrap().is_constructor);
    }

    /// FR0027 — a member whose Dart name is already taken by the class it
    /// lands on. The handle surface and the data surface are different sets,
    /// and the message must name the one that applies.
    #[test]
    fn a_member_may_not_shadow_its_own_generated_surface() {
        let cases = [
            (
                "#[bridge(data)] pub struct P { pub x: i64 } \
                 #[bridge] impl P { #[bridge(sync)] pub fn copy_with() -> i64 { 0 } }",
                "copyWith",
            ),
            (
                "#[bridge(data)] pub struct P { pub x: i64 } \
                 #[bridge] impl P { #[bridge(sync)] pub fn x(&self) -> i64 { 0 } }",
                "field",
            ),
            (
                "#[bridge(data)] pub enum E { A, B } \
                 #[bridge] impl E { #[bridge(sync)] pub fn index(&self) -> i64 { 0 } }",
                "every Dart `enum`",
            ),
        ];
        for (src, needle) in cases {
            let ds = check_src(src).unwrap_err();
            let d = ds.iter().find(|d| d.code == "FR0027").unwrap();
            assert!(d.message.contains(needle), "{src}: {}", d.message);
        }
        // A *fielded* enum's fields live on its variant subclasses, so a
        // member sharing a field name is fine there.
        assert!(check_src(
            "#[bridge(data)] pub enum E { A { a: i64 } } \
             #[bridge] impl E { #[bridge(sync)] pub fn a(&self) -> i64 { 0 } }",
        )
        .is_ok());
        // `discriminant` is reserved on the enums that emit it, and only
        // those: it appears because *this* declaration writes discriminants,
        // so the author who causes the clash is the author who reads the
        // diagnostic. (Contrast `take`, which every handle class reserves
        // unconditionally, because something elsewhere in the interface can
        // make it appear.)
        let ds = check_src(
            "#[bridge(data)] pub enum E { A = 1, B = 2 } \
             #[bridge] impl E { #[bridge(sync)] pub fn discriminant(&self) -> i64 { 0 } }",
        )
        .unwrap_err();
        let d = ds.iter().find(|d| d.code == "FR0027").unwrap();
        assert!(d.message.contains("`discriminant` getter"), "{}", d.message);
        assert!(check_src(
            "#[bridge(data)] pub enum E { A, B } \
             #[bridge] impl E { #[bridge(sync)] pub fn discriminant(&self) -> i64 { 0 } }",
        )
        .is_ok());
    }

    /// FR0050 — a `dart_interface` type is implemented by the caller, so it
    /// has no room for a member the bridge answers.
    #[test]
    fn a_dart_interface_carries_no_bridged_members() {
        let ds = check_src(
            "#[bridge(data, dart_interface)] pub struct Ops { pub on_change: DartCallback<i64> } \
             #[bridge] impl Ops { #[bridge(sync)] pub fn go(&self) -> i64 { 0 } }",
        )
        .unwrap_err();
        let d = ds.iter().find(|d| d.code == "FR0050").unwrap();
        assert!(d.message.contains("the caller implements"), "{}", d.message);
    }

    /// A representation-marker wrapper (`Locked<Doc>`) that matches the
    /// wrapped type's actual declaration is pure sugar: the finalized
    /// interface is byte-identical to the bare spelling, in every position
    /// the marker can appear. This is the "zero-cost" claim's proof.
    #[test]
    fn a_matching_wrapper_is_identical_to_the_bare_type_everywhere() {
        let decl = "#[bridge(locked)] pub struct Doc { x: i64 }";
        let cases = [
            // Parameter.
            ("fn f(d: &Doc)", "fn f(d: &Locked<Doc>)"),
            // Return.
            ("fn g() -> Doc", "fn g() -> Locked<Doc>"),
            // Nested in a container. (Return position: an opaque nested in a
            // `Vec`/`Option` by *value* is only legal there — as a
            // parameter it would mean Rust owning N handles by value, which
            // FR0004 refuses regardless of any wrapper.)
            ("fn h() -> Vec<Doc>", "fn h() -> Vec<Locked<Doc>>"),
            ("fn i() -> Option<Doc>", "fn i() -> Option<Locked<Doc>>"),
        ];
        for (bare_sig, wrapped_sig) in cases {
            // Plain `#[bridge]` (async), not `sync` — a sync free function
            // taking a borrowed locked param is FR0007 (needs an explicit
            // contention contract), which is an orthogonal rule this test
            // has no interest in exercising.
            let bare = format!("{decl} #[bridge] pub {bare_sig} {{ todo!() }}");
            let wrapped = format!("{decl} #[bridge] pub {wrapped_sig} {{ todo!() }}");
            assert_eq!(
                check_src(&bare).unwrap(),
                check_src(&wrapped).unwrap(),
                "bare vs {wrapped_sig}"
            );
        }
        // A data struct through `Data<T>`.
        let data_decl = "#[bridge(data)] pub struct Point { pub x: i64 }";
        let bare = format!("{data_decl} #[bridge(sync)] pub fn f() -> Point {{ todo!() }}");
        let wrapped =
            format!("{data_decl} #[bridge(sync)] pub fn f() -> Data<Point> {{ todo!() }}");
        assert_eq!(check_src(&bare).unwrap(), check_src(&wrapped).unwrap());
        // A struct field. A handle may sit in one now (FR0004 is a direction
        // rule), so this compares finalized IR like the other positions rather
        // than settling for two identical refusals.
        let bare = format!(
            "{decl} #[bridge(data)] pub struct Holder {{ pub d: Doc }} \
             #[bridge(sync)] pub fn make() -> Holder {{ todo!() }}"
        );
        let wrapped = format!(
            "{decl} #[bridge(data)] pub struct Holder {{ pub d: Locked<Doc> }} \
             #[bridge(sync)] pub fn make() -> Holder {{ todo!() }}"
        );
        assert_eq!(check_src(&bare).unwrap(), check_src(&wrapped).unwrap());
    }

    /// FR0062, in each of the positions a marker can appear.
    #[test]
    fn a_wrapper_that_disagrees_with_the_declaration_is_rejected() {
        let decl = "#[bridge(frozen)] pub struct Doc { x: i64 }";
        for (sig, expected_msg) in [
            (
                format!("{decl} #[bridge(sync)] pub fn f(d: &Locked<Doc>) {{}}"),
                "`Doc` is declared `frozen`, not `locked`",
            ),
            (
                format!("{decl} #[bridge(sync)] pub fn f() -> Confined<Doc> {{ todo!() }}"),
                "`Doc` is declared `frozen`, not `confined`",
            ),
            (
                format!("{decl} #[bridge(sync)] pub fn f(d: Vec<Actor<Doc>>) {{}}"),
                "`Doc` is declared `frozen`, not `actor`",
            ),
        ] {
            let ds = check_src(&sig).unwrap_err();
            assert!(ds.iter().any(|d| d.code == "FR0062"), "{sig}: {ds:?}");
            assert!(
                ds.iter().any(|d| d.message.contains(expected_msg)),
                "{sig}: {ds:?}"
            );
        }
        // `data` wrapping a declared handle.
        let ds = check_src(&format!(
            "{decl} #[bridge(sync)] pub fn f() -> Data<Doc> {{ todo!() }}"
        ))
        .unwrap_err();
        assert!(ds.iter().any(|d| d.code == "FR0062"), "{ds:?}");
        // `Locked<...>` wrapping a declared data struct.
        let ds = check_src(
            "#[bridge(data)] pub struct Point { pub x: i64 } \
             #[bridge(sync)] pub fn f() -> Locked<Point> { todo!() }",
        )
        .unwrap_err();
        assert!(ds.iter().any(|d| d.code == "FR0062"), "{ds:?}");
        assert!(
            ds.iter().any(|d| d.message.contains("declared `data`, not `locked`")),
            "{ds:?}"
        );
    }

    /// `Data<T>` wrapping a `#[bridge(bytes(...))]` external type: bytes is
    /// its own representation, not one of the five markers can name.
    #[test]
    fn a_wrapper_around_a_bytes_extern_type_is_rejected() {
        let ds = check_src(
            r#"#[bridge(bytes(dart = "P", import = "p.dart"))] pub struct P { x: i64 }
               #[bridge(sync)] pub fn f() -> Data<P> { todo!() }"#,
        )
        .unwrap_err();
        assert!(ds.iter().any(|d| d.code == "FR0062"), "{ds:?}");
        assert!(
            ds.iter().any(|d| d.message.contains("its own representation")),
            "{ds:?}"
        );
    }

    /// A marker wraps a bridged type name directly, never a container.
    #[test]
    fn a_wrapper_around_a_container_is_rejected() {
        let ds = check_src(
            "#[bridge(locked)] pub struct Doc { x: i64 } \
             #[bridge(sync)] pub fn f() -> Locked<Vec<Doc>> { todo!() }",
        )
        .unwrap_err();
        assert!(ds.iter().any(|d| d.code == "FR0062"), "{ds:?}");
    }

    /// The impl self-type position: `impl Locked<Doc>` where `Doc` really is
    /// locked is pure sugar (same methods as the bare form); where it
    /// disagrees, FR0062 fires on the claim alone — the impl block itself is
    /// fine, because a data type carries members too, so `impl Data<Point>`
    /// over a data `Point` is simply accepted.
    #[test]
    fn impl_self_type_wrapper_is_checked_against_the_declaration() {
        let matching = check_src(
            "#[bridge(locked)] pub struct Doc { x: i64 } \
             #[bridge] impl Locked<Doc> { #[bridge] pub fn f(&self) -> i64 { 0 } }",
        )
        .unwrap();
        let bare = check_src(
            "#[bridge(locked)] pub struct Doc { x: i64 } \
             #[bridge] impl Doc { #[bridge] pub fn f(&self) -> i64 { 0 } }",
        )
        .unwrap();
        assert_eq!(matching, bare);

        let mismatched = codes(check_src(
            "#[bridge(frozen)] pub struct Doc { x: i64 } \
             #[bridge] impl Locked<Doc> { #[bridge(sync)] pub fn f(&self) -> i64 { 0 } }",
        ));
        assert!(mismatched.contains(&"FR0062"), "{mismatched:?}");

        let data_impl = codes(check_src(
            "#[bridge(data)] pub struct Point { pub x: i64 } \
             #[bridge] impl Locked<Point> { #[bridge(sync)] pub fn f(&self) -> i64 { 0 } }",
        ));
        assert_eq!(data_impl, vec!["FR0062"]);

        // The claim agrees (`Point` really is `data`), so nothing fires: the
        // marker is sugar here exactly as it is over a handle.
        assert!(check_src(
            "#[bridge(data)] pub struct Point { pub x: i64 } \
             #[bridge] impl Data<Point> { #[bridge(sync)] pub fn f(&self) -> i64 { 0 } }",
        )
        .is_ok());
    }

    /// Malformed wrapper syntax on an impl self type: wrong arity, or a
    /// non-bare inner type. Uncoded, matching `"#[bridge] impl: unsupported
    /// self type"`'s style — a parse-time syntax rejection, not a checker
    /// rule.
    #[test]
    fn malformed_impl_self_type_wrapper_is_rejected() {
        for src in [
            "#[bridge] impl Locked { #[bridge(sync)] pub fn f(&self) {} }",
            "#[bridge] impl Locked<Doc, Doc> { #[bridge(sync)] pub fn f(&self) {} }",
            "#[bridge] impl Locked<Vec<Doc>> { #[bridge(sync)] pub fn f(&self) {} }",
        ] {
            let err = crate::parse::parse_source(src, "crate::api").unwrap_err();
            assert!(
                err.to_string().contains("the bridged type it names"),
                "{src}: {err}"
            );
        }
    }

    /// A user's own type literally named `Data`/`Confined`/… is untouched as
    /// long as it is never applied to a generic argument — only the
    /// one-argument generic form is a reserved marker, the same rule that
    /// already reserves `Vec`/`Box`/`Option`: FR0046's own
    /// `a_declared_type_may_not_take_a_claimed_or_shadowing_name` proves a
    /// `struct Vec` declaration is accepted, not refused, so the analogy
    /// this test leans on is real, not asserted.
    #[test]
    fn a_bare_marker_name_with_no_argument_is_an_ordinary_type() {
        let iface = check_src(
            "#[bridge(data)] pub struct Data { pub x: i64 } \
             #[bridge(sync)] pub fn f() -> Data { todo!() }",
        )
        .unwrap();
        assert_eq!(iface.structs[0].name, "Data");
    }

    #[test]
    fn rejects_on_contention_without_a_locked_type() {
        // FR0010: `on_contention` only means something for a sync call that
        // accesses a Locked type. On a plain free function it is meaningless.
        let ds = codes(check_src(
            "#[bridge(sync, on_contention = \"error\")] pub fn f() {}",
        ));
        assert!(ds.contains(&"FR0010"), "{ds:?}");
    }

    #[test]
    fn rejects_duplicate_type_names() {
        let ds = codes(check_src(
            "#[bridge(data)] pub struct P { x: i32 } #[bridge(data)] pub struct P { y: i32 }",
        ));
        assert!(ds.contains(&"FR0002"), "{ds:?}");
    }

    /// A bridged trait **by value** is a consume like any other: the impl tag
    /// the parameter already carries names the registry the take comes out of.
    #[test]
    fn a_by_value_trait_param_is_a_consume() {
        assert!(
            check_src(
                r#"
            #[bridge(confined)]
            pub trait T: Send { fn f(&self) -> i64; }
            #[bridge(sync)]
            pub fn g(x: Box<dyn T>) {}
            "#,
            )
            .is_ok()
        );
    }

    #[test]
    fn trait_returns_compose_like_opaques() {
        let iface = check_src(
            r#"
            #[bridge(frozen)]
            pub trait Store: Send + Sync {
                fn get(&self, key: String) -> Option<String>;
                fn snapshot(&self) -> Box<dyn Store>;
            }
            #[bridge]
            pub fn open(kind: String) -> anyhow::Result<Box<dyn Store>> { todo!() }
            #[bridge]
            pub fn open_all() -> Vec<Box<dyn Store>> { todo!() }
            #[bridge]
            pub fn maybe() -> Option<Box<dyn Store>> { todo!() }
            "#,
        )
        .unwrap();
        assert_eq!(iface.functions[1].ret, Some(Type::Opaque("Store".into())));
        assert_eq!(
            iface.functions[3].ret,
            Some(Type::List(Box::new(Type::Opaque("Store".into())), SeqKind::Vec))
        );
        assert_eq!(
            iface.functions[4].ret,
            Some(Type::Option(Box::new(Type::Opaque("Store".into()))))
        );
    }

    #[test]
    fn trait_handles_respect_actor_boundaries() {
        // A trait handle is still instance-local: it cannot cross into an
        // actor (FR0015, same as concrete opaques).
        let ds = codes(check_src(
            r#"
            #[bridge(confined)]
            pub trait T: Send { fn f(&self) -> i64; }
            #[bridge(actor)]
            pub struct A { x: i64 }
            #[bridge]
            impl A {
                pub fn new() -> Self { todo!() }
                pub fn use_t(&self, t: &dyn T) -> i64 { todo!() }
            }
            "#,
        ));
        assert!(ds.contains(&"FR0015"), "{ds:?}");
    }

    #[test]
    fn bridged_impl_contract_rules() {
        // FR0024: exec must match the trait's declaration (the class
        // implements the trait's Dart interface).
        let ds = codes(check_src(
            r#"
            #[bridge(confined)]
            pub trait Tally: Send { fn bump(&mut self) -> i64; }
            #[bridge(confined)]
            pub struct Abacus { n: i64 }
            #[bridge(sync)]
            impl Tally for Abacus { fn bump(&mut self) -> i64 { todo!() } }
            "#,
        ));
        // Confined trait methods derive sync, so annotate the trait's
        // method model instead: here the impl says sync but the trait's
        // method... both derive sync via Confined. Use frozen to diverge.
        assert!(ds.is_empty() || ds == vec!["FR0024"], "{ds:?}");
        let ds = codes(check_src(
            r#"
            #[bridge(frozen)]
            pub trait Greeter: Send + Sync { fn greet(&self) -> String; }
            #[bridge(frozen)]
            pub struct Robot { x: i64 }
            #[bridge(sync)]
            impl Greeter for Robot { fn greet(&self) -> String { todo!() } }
            "#,
        ));
        assert_eq!(ds, vec!["FR0024"]);
        // FR0025: actors cannot implement bridged traits.
        let ds = codes(check_src(
            r#"
            #[bridge(confined)]
            pub trait Tally: Send { fn bump(&mut self) -> i64; }
            #[bridge(actor)]
            pub struct Robot { x: i64 }
            #[bridge]
            impl Tally for Robot { fn bump(&mut self) -> i64 { todo!() } }
            "#,
        ));
        assert!(ds.contains(&"FR0025"), "{ds:?}");
        // FR0026: implementor model must match the trait's model.
        let ds = codes(check_src(
            r#"
            #[bridge(frozen)]
            pub trait Greeter: Send + Sync { fn greet(&self) -> String; }
            #[bridge(locked)]
            pub struct Robot { x: i64 }
            #[bridge]
            impl Greeter for Robot { fn greet(&self) -> String { todo!() } }
            "#,
        ));
        assert!(ds.contains(&"FR0026"), "{ds:?}");
        // Foreign traits are exempt from all of it: static dispatch only.
        assert!(check_src(
            r#"
            #[bridge(confined)]
            pub struct Doc { x: i64 }
            #[bridge(sync)]
            impl serde_like::Pretty for Doc { fn pretty(&self) -> String { todo!() } }
            "#,
        )
        .is_ok());
    }

    #[test]
    fn unwritten_trait_methods_are_synthesized_onto_implementors() {
        let iface = check_src(
            r#"
            #[bridge(confined)]
            pub trait Tally: Send {
                fn bump(&mut self) -> i64;
                fn describe(&self) -> String { String::new() }
            }
            #[bridge(confined)]
            pub struct Abacus { n: i64 }
            #[bridge]
            impl Tally for Abacus {
                fn bump(&mut self) -> i64 { todo!() }
            }
            "#,
        )
        .unwrap();
        let synth: Vec<&Function> = iface
            .functions
            .iter()
            .filter(|f| f.parent.as_deref() == Some("Abacus"))
            .collect();
        assert_eq!(synth.len(), 2, "bump written + describe synthesized");
        let describe = synth.iter().find(|f| f.name == "describe").unwrap();
        assert_eq!(describe.trait_impl.as_deref(), Some("crate::api::Tally"));
        assert_eq!(describe.exec, Exec::Sync, "confined derivation applies");
        // fn_ids are unique and dense across the synthesized surface.
        let mut ids: Vec<u32> = iface.functions.iter().map(|f| f.fn_id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), iface.functions.len());
    }

    #[test]
    fn async_fn_is_accepted_and_portable() {
        // A Rust `async fn` runs on the cooperative executor, which works on
        // every platform: accepted, Exec::Async, and NOT requires_native — so
        // it is present on the web surface too.
        let iface = check_src("#[bridge] pub async fn slow(x: i64) -> i64 { x }").unwrap();
        let f = &iface.functions[0];
        assert!(f.rust_async);
        assert_eq!(f.exec, Exec::Async);
        assert!(!f.requires_native, "async fns run on the executor everywhere");
        // Frozen AND Locked methods may be async fns (executor execution), also
        // portable. Both are checked: this comment used to claim the pair while
        // only exercising `frozen`, and for as long as it did, a locked
        // `async fn` was accepted here and then failed to compile in the
        // generated glue.
        for model in ["frozen", "locked"] {
            let iface = check_src(&format!(
                "#[bridge({model})] pub struct S {{ x: i64 }}\n\
                 #[bridge] impl S {{ pub async fn f(&self) -> i64 {{ self.x }} }}"
            ))
            .unwrap_or_else(|d| panic!("{model} should accept an async fn: {d:?}"));
            let m = iface.functions.iter().find(|f| f.name == "f").unwrap();
            assert!(m.rust_async && !m.requires_native, "{model}");
        }
    }

    /// A receiverless, non-constructor actor member. Accepted until the shape
    /// was generated mechanically, at which point it emitted Dart naming the
    /// instance field `_host` from a `static` method — a load-time failure for
    /// the whole suite, attributed to a generated file.
    #[test]
    fn a_receiverless_actor_member_is_refused() {
        let ds = codes(check_src(
            r#"
            #[bridge(actor)] pub struct A { x: i64 }
            #[bridge] impl A {
                pub fn make() -> Self { A { x: 0 } }
                pub fn stray() -> i64 { 0 }
            }
            "#,
        ));
        assert!(ds.contains(&"FR0053"), "{ds:?}");
        // The constructor beside it is fine: it creates the executor it runs on.
        assert_eq!(ds.iter().filter(|c| **c == "FR0053").count(), 1, "{ds:?}");
    }

    #[test]
    fn async_fn_rejected_on_confined_and_actor() {
        // Confined: caller-thread sync execution cannot await.
        let ds = codes(check_src(
            r#"
            #[bridge(confined)] pub struct D { x: i64 }
            #[bridge] impl D { pub async fn f(&self) -> i64 { self.x } }
            "#,
        ));
        assert!(ds.contains(&"FR0029"), "{ds:?}");
        // Actor: its own executor already runs bodies; web actor is a wasm
        // worker instance where async Rust cannot run.
        let ds = codes(check_src(
            r#"
            #[bridge(actor)] pub struct A { x: i64 }
            #[bridge] impl A {
                pub fn new() -> Self { todo!() }
                pub async fn f(&self) -> i64 { self.x }
            }
            "#,
        ));
        assert!(ds.contains(&"FR0029"), "{ds:?}");
    }

    #[test]
    fn web_subset_includes_async_fns() {
        // A Rust `async fn` is a normal web member now (executor-driven): the
        // web pass sees it (not filtered by requires_native) and it stays on
        // the web surface.
        let iface = check_src(
            r#"
            #[bridge(sync)] pub fn plain() -> i64 { 0 }
            #[bridge] pub async fn slow() -> i64 { 0 }
            "#,
        )
        .unwrap();
        assert_eq!(iface.functions.iter().filter(|f| f.rust_async).count(), 1);
        assert!(!iface
            .functions
            .iter()
            .find(|f| f.name == "slow")
            .unwrap()
            .requires_native);
    }

    #[test]
    fn assigns_every_member_its_content_derived_id() {
        let iface = check_src(
            r#"
            #[bridge] pub fn a() {}
            #[bridge] pub fn b() {}
            "#,
        )
        .unwrap();
        // finalize is the only place ids are set, and it defers the derivation
        // to one function — so what this pins is that every member got that
        // function's answer, not a position. The derivation's own properties
        // (stability under insertion, movement on a signature change, collision
        // resolution) are tested in `hash.rs`.
        let ids: Vec<u32> = iface.functions.iter().map(|f| f.fn_id).collect();
        assert_eq!(ids, crate::hash::member_ids(&iface.functions));
        assert_ne!(ids[0], ids[1]);
    }

    /// FR0057 — a receiver is one of five spellings: `&self`, `&mut self`,
    /// `self`, `mut self`, `self: Box<Self>`. Every *other* typed receiver is
    /// refused, because the parser reads the type only far enough to recognise
    /// `Box<Self>` and what the rest would mean to the crossing is not
    /// decided. What each accepted spelling *means* is the representation's
    /// own question, not this rule's.
    #[test]
    fn refuses_a_typed_receiver_it_does_not_read() {
        let opaque = "#[bridge(confined)] pub struct O;";
        for recv in [
            "self: &Self",
            "self: &mut Self",
            "self: std::pin::Pin<&mut Self>",
            "self: std::rc::Rc<Self>",
        ] {
            let src = format!("{opaque} #[bridge] impl O {{ pub fn m({recv}) {{}} }}");
            let ds = check_src(&src).err().unwrap();
            assert_eq!(ds.iter().map(|d| d.code).collect::<Vec<_>>(), vec!["FR0057"], "{src}");
            assert!(ds[0].message.starts_with("`O::m`: "), "{src}: {}", ds[0].message);
            assert!(ds[0].message.contains("not decided"), "{src}: {}", ds[0].message);
        }
        // The four accepted spellings on a handle.
        for recv in ["&self", "&mut self", "self", "mut self", "self: Box<Self>"] {
            let src = format!("{opaque} #[bridge] impl O {{ pub fn m({recv}) {{}} }}");
            assert!(check_src(&src).is_ok(), "{src}: {:?}", check_src(&src).err());
        }
        // The rule is about the *spelling*, not the representation: a data
        // type refuses the same four and accepts the same five.
        let data = "#[bridge(data)] pub struct P { pub x: i64 }";
        let ds = check_src(&format!(
            "{data} #[bridge] impl P {{ #[bridge(sync)] pub fn m(self: std::rc::Rc<Self>) {{}} }}"
        ))
        .unwrap_err();
        assert_eq!(ds.iter().map(|d| d.code).collect::<Vec<_>>(), vec!["FR0057"]);
        assert!(ds[0].message.contains("not decided"), "{}", ds[0].message);
        assert!(check_src(&format!(
            "{data} #[bridge] impl P {{ #[bridge(sync)] pub fn m(self: Box<Self>) -> i64 {{ 0 }} }}"
        ))
        .is_ok());
    }

    /// A **consuming** member on a locked type takes no guard at all:
    /// `try_unwrap` is a compare-exchange and `into_inner` moves the value out
    /// of a cell. So there is nothing to contend for, no contract to name
    /// (FR0007 does not apply), and nothing for FR0010 to be about either.
    /// `handle::locked_take` states the argument.
    #[test]
    fn a_sync_consuming_member_on_a_locked_type_is_portable_and_contract_free() {
        let decl = "#[bridge(locked)] pub struct L { n: i64 }";
        for recv in ["self", "self: Box<Self>"] {
            let iface = check_src(&format!(
                "{decl} #[bridge] impl L {{ #[bridge(sync)] pub fn into_n({recv}) -> i64 {{ 0 }} }}"
            ))
            .unwrap_or_else(|d| panic!("{recv}: {d:?}"));
            let f = iface.functions.iter().find(|f| f.name == "into_n").unwrap();
            assert!(!f.requires_native, "{recv}");
            assert!(f.on_contention.is_none(), "{recv}");
        }
        // A *borrowed* sync receiver still needs one — it takes a guard.
        assert_eq!(
            codes(check_src(&format!(
                "{decl} #[bridge] impl L {{ #[bridge(sync)] pub fn n(&self) -> i64 {{ 0 }} }}"
            ))),
            vec!["FR0007"]
        );
        // And `on_contention` on a member that acquires nothing is FR0010, as
        // it is anywhere else.
        assert_eq!(
            codes(check_src(&format!(
                "{decl} #[bridge] impl L {{ \
                 #[bridge(sync, on_contention = \"error\")] pub fn into_n(self) -> i64 {{ 0 }} }}"
            ))),
            vec!["FR0010"]
        );
    }

    /// FR0012 is about a **borrow**: an async call would run on a pool thread
    /// while the owner kept using the object. A consumed confined handle is
    /// exempt because there is no owner left — the glue takes the `Box` on the
    /// calling thread and moves the value into the future, which the confined
    /// model's own `Send` bound already licenses.
    #[test]
    fn a_consumed_confined_handle_is_exempt_from_the_async_borrow_rule() {
        let decl = "#[bridge(confined)] pub struct C { n: i64 }";
        assert!(check_src(&format!("{decl} #[bridge] pub async fn eat(c: C) -> i64 {{ 0 }}")).is_ok());
        assert!(check_src(&format!("{decl} #[bridge] pub async fn eat(c: Vec<C>) -> i64 {{ 0 }}")).is_ok());
        let ds = check_src(&format!("{decl} #[bridge] pub async fn peek(c: &C) -> i64 {{ 0 }}"))
            .unwrap_err();
        assert_eq!(ds.iter().map(|d| d.code).collect::<Vec<_>>(), vec!["FR0012"]);
        // The message points at the way out that now exists.
        assert!(ds[0].message.contains("by value"), "{}", ds[0].message);
    }

    /// FR0066 — a consuming getter. A Dart property is expected to be readable
    /// twice; this one destroys its receiver, so the second read throws.
    #[test]
    fn refuses_a_consuming_getter() {
        let ds = check_src(
            "#[bridge(confined)] pub struct D { n: i64 } \
             #[bridge] impl D { #[bridge(sync, getter)] pub fn total(self) -> i64 { 0 } }",
        )
        .unwrap_err();
        let d = ds.iter().find(|d| d.code == "FR0066").expect("FR0066");
        assert!(d.message.contains("readable twice"), "{}", d.message);
        // The borrowing spelling is a property, as it always was.
        assert!(check_src(
            "#[bridge(confined)] pub struct D { n: i64 } \
             #[bridge] impl D { #[bridge(sync, getter)] pub fn total(&self) -> i64 { 0 } }",
        )
        .is_ok());
    }

    /// A consuming receiver on a bridged trait is `self: Box<Self>`.
    ///
    /// Its Dart side is an extension on `Consumed<T>`, and Dart picks an
    /// extension from the *static* type while `Consumed<Impl>` is assignable
    /// to `Consumed<T>` — so a token holding a concrete implementor does reach
    /// the trait's glue. The impl tag it carries is what says which registry
    /// the take comes out of, so nothing here refuses it.
    ///
    /// `fn m(self)` is FR0022, and the reason is not the one it looks like:
    /// the trait stays dyn compatible and `Box<dyn T>` compiles. The glue's
    /// call does not, because a `Box<dyn T>` is not a `T`.
    #[test]
    fn a_consuming_receiver_on_a_bridged_trait_is_the_boxed_one() {
        assert!(
            check_src("#[bridge(frozen)] pub trait T: Send + Sync { fn m(self: Box<Self>); }")
                .is_ok()
        );
        for recv in ["self", "mut self"] {
            let src =
                format!("#[bridge(frozen)] pub trait T: Send + Sync {{ fn m({recv}); }}");
            let ds = check_src(&src).unwrap_err();
            let d = ds.iter().find(|d| d.code == "FR0022").expect("FR0022");
            assert!(d.message.contains("self: Box<Self>"), "{}", d.message);
        }
        // The refusal is about the *trait* position and nothing else: the same
        // spelling on a concrete handle of the same model is accepted, and so
        // is a borrowed receiver on the trait.
        assert!(check_src(
            "#[bridge(frozen)] pub struct C { pub v: i64 } \
             #[bridge] impl C { #[bridge(sync)] pub fn m(self) -> i64 { 0 } }",
        )
        .is_ok());
        assert!(
            check_src("#[bridge(frozen)] pub trait T: Send + Sync { fn m(&self); }").is_ok()
        );
        // A concrete type implementing the trait declares its own consuming
        // member alongside; that one dispatches through its own fn_id.
        assert!(check_src(
            "#[bridge(frozen)] pub trait T: Send + Sync { fn m(&self); } \
             #[bridge(frozen)] pub struct C; \
             #[bridge] impl T for C { fn m(&self) {} } \
             #[bridge] impl C { #[bridge(sync)] pub fn into_x(self) -> i64 { 0 } }",
        )
        .is_ok());
    }

    /// FR0056 — what stays refused now that a generic **data** declaration is
    /// expanded rather than rejected.
    ///
    /// A parameter used as a field type is a parameter, not an unknown type —
    /// FR0003 stays silent for it, or the author would go looking for a
    /// declaration that cannot exist.
    #[test]
    fn rejects_generic_items() {
        for (src, expected) in [
            // A generic *function*: nothing could supply the instantiation,
            // because a Dart call carries bytes and not Rust types.
            ("#[bridge(sync)] pub fn f<T>(x: T) -> T { x }", vec!["FR0056"]),
            ("#[bridge(sync)] pub fn f<T>(x: i32) {}", vec!["FR0056"]),
            (
                "#[bridge(frozen)] pub struct O; \
                 #[bridge] impl O { pub fn m<T>(&self, x: i32) {} }",
                vec!["FR0056"],
            ),
            // A const parameter and a defaulted one, on a data declaration
            // that would otherwise be expanded.
            (
                "#[bridge(data)] pub struct Cache<const N: usize> { x: i32 } \
                 #[bridge(sync)] pub fn f(c: Cache) -> i64 { 0 }",
                vec!["FR0056"],
            ),
            (
                "#[bridge(data)] pub struct P<T = i64> { x: T } \
                 #[bridge(sync)] pub fn f(p: P<i64>) -> i64 { 0 }",
                vec!["FR0056"],
            ),
            // An impl on a template, written with the bare name.
            (
                "#[bridge(data)] pub struct P<T> { x: T } \
                 #[bridge] impl P { #[bridge(sync)] pub fn m(&self) {} }",
                vec!["FR0056"],
            ),
        ] {
            let ds = check_src(src).err().unwrap();
            let got: Vec<&str> = ds.iter().map(|d| d.code).collect();
            assert_eq!(got, expected, "{src}");
        }
        // The parameter names are quoted, and a method names its type.
        let ds = check_src(
            "#[bridge(frozen)] pub struct O; \
             #[bridge] impl O { pub fn m<T, const N: usize>(&self) {} }",
        )
        .err()
        .unwrap();
        assert!(ds[0].message.starts_with("fn `O::m`: "), "{}", ds[0].message);
        assert!(ds[0].message.contains("`<T, N>`"), "{}", ds[0].message);
        // Lifetimes are not generics here: a borrowed `&str` and a struct with
        // a lifetime parameter both stay accepted.
        assert!(check_src("#[bridge(sync)] pub fn g(s: &str) -> String { s.into() }").is_ok());
        assert!(check_src("#[bridge(data)] pub struct Cache<'a> { x: i32 }").is_ok());
        // A parameter is never reported as an unknown type — not in the
        // template, and not in an expansion, where it is gone.
        let iface = check_src(
            "#[bridge(data)] pub struct Cache<T> { x: T } \
             #[bridge(sync)] pub fn f(c: Cache<i64>) -> i64 { 0 }",
        )
        .unwrap();
        assert_eq!(iface.generic_structs[0].fields[0].ty, Type::Param("T".into()));
        assert_eq!(iface.struct_decl("Cache<i64>").unwrap().fields[0].ty, Type::I64);
        // …but a field type the interface does not declare still is, reported
        // against the template rather than against a use site.
        let ds = check_src("#[bridge(data)] pub struct W<T>(pub semver::Version, pub T);")
            .err()
            .unwrap();
        assert_eq!(ds.iter().map(|d| d.code).collect::<Vec<_>>(), vec!["FR0003"]);
        assert!(ds[0].message.contains("struct `W<T>`"), "{}", ds[0].message);
    }

    /// A bridged `impl` on a generic data type: what it produces, and what
    /// stays refused now that it is not refused wholesale.
    #[test]
    fn an_impl_on_a_generic_data_type_expands_per_instantiation() {
        const DECL: &str = "#[bridge(data)] pub struct Item { pub id: i64 } \
                            #[bridge(data)] pub struct Page<T> { pub items: Vec<T> } ";
        // A generic block reaches every instantiation the signatures derive,
        // one `Function` and one `fn_id` each; a concrete block reaches its
        // own and ADDS it to the set.
        let iface = check_src(&format!(
            "{DECL} \
             #[bridge] impl<T> Page<T> {{ #[bridge(sync)] pub fn n(&self) -> i64 {{ 0 }} }} \
             #[bridge] impl Page<String> {{ #[bridge(sync)] pub fn s(&self) -> i64 {{ 0 }} }} \
             #[bridge(sync)] pub fn a() -> Page<Item> {{ todo!() }}"
        ))
        .unwrap();
        let members: Vec<(&str, &str)> = iface
            .functions
            .iter()
            .filter_map(|f| Some((f.parent.as_deref()?, f.name.as_str())))
            .collect();
        // In instantiation-**registration** order, which is declaration order
        // over the closed positions: the concrete block's own self type is
        // registered where the block stands, ahead of the free function below
        // it. Deterministic is the requirement — `finalize` assigns ids by
        // position — and this is the order that is a function of the source.
        assert_eq!(
            members,
            vec![
                ("Page<String>", "n"),
                ("Page<Item>", "n"),
                ("Page<String>", "s"),
            ],
            "{members:?}"
        );
        // Each expansion is its own member with its own id: the same method on
        // two instantiations must not share one, or a call to `Page<String>.n`
        // would reach `Page<Item>.n`. Distinctness is the claim — the ids come
        // from each expansion's own facts (`hash::member_ids`), which differ in
        // `parent_repr`.
        let ids: std::collections::HashSet<u32> =
            iface.functions.iter().map(|f| f.fn_id).collect();
        assert_eq!(ids.len(), iface.functions.len(), "{ids:?}");
        // A parameter in the signature is bound per instantiation.
        let iface = check_src(&format!(
            "{DECL} \
             #[bridge] impl<T> Page<T> {{ #[bridge(sync)] pub fn head(&self) -> T {{ todo!() }} }} \
             #[bridge(sync)] pub fn a() -> Page<Item> {{ todo!() }} \
             #[bridge(sync)] pub fn b() -> Page<String> {{ todo!() }}"
        ))
        .unwrap();
        let rets: Vec<Option<&Type>> = iface
            .functions
            .iter()
            .filter(|f| f.name == "head")
            .map(|f| f.ret.as_ref())
            .collect();
        assert_eq!(
            rets,
            vec![Some(&Type::Struct("Item".into())), Some(&Type::String)],
            "{rets:?}"
        );

        for (src, expected) in [
            // Two instantiations, one Dart type, and one member name: an
            // extension member resolves from the receiver's static type, and
            // that type does not say which.
            (
                "#[bridge(data)] pub struct P<T> { pub x: T }                  #[bridge] impl<T> P<T> { #[bridge(sync)] pub fn n(&self) -> i64 { 0 } }                  #[bridge(sync)] pub fn a(p: P<i32>) -> i64 { 0 }                  #[bridge(sync)] pub fn b(p: P<i64>) -> i64 { 0 }",
                "FR0002",
            ),
            // Selecting a subset by matching.
            (
                "#[bridge(data)] pub struct P<T> { pub x: T }                  #[bridge] impl<T> P<Vec<T>> { #[bridge(sync)] pub fn n(&self) -> i64 { 0 } }                  #[bridge(sync)] pub fn a(p: P<Vec<i64>>) -> i64 { 0 }",
                "FR0056",
            ),
            // Applied to something with no parameters.
            (
                "#[bridge(frozen)] pub struct S;                  #[bridge] impl S<i64> { #[bridge(sync)] pub fn n(&self) -> i64 { 0 } }",
                "FR0056",
            ),
            // A generic block on a template nothing instantiates bridges
            // nothing at all.
            (
                "#[bridge(data)] pub struct P<T> { pub x: T }                  #[bridge] impl<T> P<T> { #[bridge(sync)] pub fn n(&self) -> i64 { 0 } }",
                "FR0056",
            ),
            // Polymorphic recursion through a member: the same infinite set
            // FR0075 refuses through a field, and rustc does NOT catch this
            // one — a generic fn is monomorphized on demand.
            (
                "#[bridge(data)] pub struct P<T> { pub x: T }                  #[bridge] impl<T> P<T> {                    #[bridge(sync)] pub fn g(&self) -> P<Vec<T>> { todo!() } }                  #[bridge(sync)] pub fn a(p: P<i64>) -> i64 { 0 }",
                "FR0075",
            ),
            // An instantiation whose receiver reaches a handle is decoded Dart
            // to Rust, which FR0004 refuses — judged per instantiation, which
            // is what expanding before the rules buys.
            (
                "#[bridge(frozen)] pub struct D;                  #[bridge(data)] pub struct P<T> { pub x: T }                  #[bridge] impl<T> P<T> { #[bridge(sync)] pub fn n(&self) -> i64 { 0 } }                  #[bridge(sync)] pub fn a() -> P<D> { todo!() }",
                "FR0004",
            ),
        ] {
            let ds = check_src(src).err().unwrap_or_else(|| panic!("accepted: {src}"));
            assert!(
                ds.iter().any(|d| d.code == expected),
                "{src}: {:?}",
                ds.iter().map(|d| d.code).collect::<Vec<_>>()
            );
        }

        // FR0027 on an extension has two consequences and neither is the
        // class's, both measured with `dart analyze`: a member `Object`
        // already has cannot be declared on an extension at all, and anything
        // else is simply outranked by the class member.
        for (member, needle) in [
            ("to_string", "would not compile"),
            ("x", "never be called"),
        ] {
            let ds = check_src(&format!(
                "#[bridge(data)] pub struct P<T> {{ pub x: T }} \
                 #[bridge] impl<T> P<T> {{ \
                   #[bridge(sync)] pub fn {member}(&self) -> i64 {{ 0 }} }} \
                 #[bridge(sync)] pub fn a(p: P<i64>) -> i64 {{ 0 }}"
            ))
            .err()
            .unwrap();
            let d = ds.iter().find(|d| d.code == "FR0027").unwrap();
            assert!(d.message.contains(needle), "{member}: {}", d.message);
        }
        // …and a `static` collides with neither: it is reached through the
        // extension's own name, which nothing on the class can shadow.
        assert!(check_src(
            "#[bridge(data)] pub struct P<T> { pub x: T } \
             #[bridge] impl<T> P<T> { \
               #[bridge(sync)] pub fn copy_with() -> P<T> { todo!() } } \
             #[bridge(sync)] pub fn a(p: P<i64>) -> i64 { 0 }"
        )
        .is_ok());

        // A block parameter the self type does not write gets NO rule here:
        // rustc refuses that impl on the author's own source (E0207), and a
        // codegen rule would be a second voice saying it later.
        assert!(check_src(
            "#[bridge(data)] pub struct P<T> { pub x: T } #[bridge] impl<T, U> P<T> { #[bridge(sync)] pub fn n(&self) -> i64 { 0 } } #[bridge(sync)] pub fn a(p: P<i64>) -> i64 { 0 }"
        )
        .is_ok());
        // A permutation of the block's parameters is ordinary, not a subset.
        assert!(check_src(
            "#[bridge(data)] pub struct Pair<A, B> { pub a: A, pub b: B } #[bridge] impl<A, B> Pair<B, A> { #[bridge(sync)] pub fn n(&self) -> i64 { 0 } } #[bridge(sync)] pub fn f(p: Pair<i64, String>) -> i64 { 0 }"
        )
        .is_ok());
        // Two concrete blocks on instantiations that share a Dart type are
        // fine while their member names differ: each call site's codec is
        // written on it, exactly as a free function's is. Two `static`s of one
        // name are fine even so — a static is reached through the extension's
        // own name, which cannot be ambiguous.
        assert!(check_src(
            "#[bridge(data)] pub struct P<T> { pub x: T }              #[bridge] impl P<i32> {                #[bridge(sync)] pub fn small(&self) -> i64 { 0 }                #[bridge(sync)] pub fn zero() -> Self { todo!() } }              #[bridge] impl P<i64> {                #[bridge(sync)] pub fn big(&self) -> i64 { 0 }                #[bridge(sync)] pub fn zero() -> Self { todo!() } }",
        )
        .is_ok());
    }

    /// The extension one instantiation's members land on, and the fake method
    /// that answers them, are named by prefix notation over the type
    /// constructors — injective because every token's arity is fixed.
    #[test]
    fn an_instantiations_dart_surface_is_named_by_prefix_notation() {
        let iface = check_src(
            "#[bridge(data)] pub struct Item { pub id: i64 }              #[bridge(data)] pub struct Page<T> { pub items: Vec<T> }              #[bridge] impl<T> Page<T> { #[bridge(sync)] pub fn n(&self) -> i64 { 0 } }              #[bridge(sync)] pub fn a() -> Page<Item> { todo!() }              #[bridge(sync)] pub fn b(p: Page<Vec<Item>>) -> i64 { 0 }              #[bridge(sync)] pub fn c(p: Page<Option<(i64, String)>>) -> i64 { 0 }",
        )
        .unwrap();
        for (decl, ext) in [
            ("Page<Item>", "Page$Item"),
            ("Page<Vec<Item>>", "Page$List$Item"),
            ("Page<Option<(i64, String)>>", "Page$Opt$Tup2$i64$String"),
        ] {
            assert_eq!(crate::emit_dart::instance_extension(&iface, decl), ext);
        }
        let fakes: Vec<String> = iface
            .functions
            .iter()
            .filter(|f| f.name == "n")
            .map(|f| crate::emit_dart_fake::free_name(&iface, f))
            .collect();
        assert_eq!(
            fakes,
            vec!["page$ItemN", "page$List$ItemN", "page$Opt$Tup2$i64$StringN"],
            "{fakes:?}"
        );
        // The tokens are ordinary spellings, not reserved words, so an
        // author's own class can be spelled like one — reported, with the
        // remedy that works: rename the *declaration* whose class name is in
        // the way, and the stem follows.
        const CLASH: &str = "#[bridge(data)] pub struct Bytes { pub n: i64 } \
             #[bridge(data)] pub struct Page<T> { pub items: Vec<T> } \
             #[bridge] impl<T> Page<T> { #[bridge(sync)] pub fn n(&self) -> i64 { 0 } } \
             #[bridge(sync)] pub fn a(p: Page<Bytes>) -> i64 { 0 } \
             #[bridge(sync)] pub fn b(p: Page<Vec<u8>>) -> i64 { 0 }";
        let ds = check_src(CLASH).err().unwrap();
        let d = ds.iter().find(|d| d.code == "FR0002").unwrap();
        assert!(d.message.contains("prefix notation"), "{}", d.message);
        assert!(d.message.contains("`Page<Bytes>` and `Page<Vec<u8>>`"), "{}", d.message);
        let iface = check_src(&CLASH.replace(
            "#[bridge(data)] pub struct Bytes",
            "#[bridge(data, dart_identifier = \"ByteCount\")] pub struct Bytes",
        ))
        .unwrap();
        assert_eq!(
            crate::emit_dart::instance_extension(&iface, "Page<Bytes>"),
            "Page$ByteCount"
        );

        // `dart_identifier` on the template renames the class, and the
        // extension follows it.
        let iface = check_src(
            "#[bridge(data, dart_identifier = \"Sheet\")] pub struct Page<T> { pub x: T }              #[bridge] impl<T> Page<T> { #[bridge(sync)] pub fn n(&self) -> i64 { 0 } }              #[bridge(sync)] pub fn a(p: Page<i64>) -> i64 { 0 }",
        )
        .unwrap();
        assert_eq!(
            crate::emit_dart::instance_extension(&iface, "Page<i64>"),
            "Sheet$i64"
        );
    }

    /// A generic data type is expanded once per fully-applied use, and every
    /// rule below runs over the expansions — which is the whole reason for the
    /// pass, and the property this pins.
    #[test]
    fn a_generic_data_type_expands_per_instantiation() {
        let iface = check_src(
            "#[bridge(data)] pub struct Item { pub id: i64 } \
             #[bridge(data)] pub struct Page<T> { pub items: Vec<T>, pub total: i64 } \
             #[bridge(data)] pub struct Wrapper<T> { pub page: Page<T> } \
             #[bridge(sync)] pub fn a() -> Page<Item> { todo!() } \
             #[bridge(sync)] pub fn b(x: Wrapper<i64>) -> i64 { 0 }",
        )
        .unwrap();
        // The templates stay out of `structs`: they have no wire form.
        let names: Vec<&str> = iface.structs.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["Item", "Page<Item>", "Page<i64>", "Wrapper<i64>"],
            "{names:?}"
        );
        // The fixpoint reached `Page<i64>` through `Wrapper<i64>`'s field, and
        // that field now names it.
        assert_eq!(
            iface.struct_decl("Wrapper<i64>").unwrap().fields[0].ty,
            Type::Struct("Page<i64>".into())
        );
        // Each expansion knows the template and the arguments it came from.
        let inst = iface.instance_of("Page<Item>").unwrap();
        assert_eq!(inst.template, "Page");
        assert_eq!(inst.args, vec![Type::Struct("Item".into())]);
        // A `Box` around an argument is transparent to the crossing, so it is
        // transparent to the identity: one instantiation, not two.
        let iface = check_src(
            "#[bridge(data)] pub struct P<T> { pub x: T } \
             #[bridge(sync)] pub fn a(p: P<i64>) -> i64 { 0 } \
             #[bridge(sync)] pub fn b(p: P<Box<i64>>) -> i64 { 0 }",
        )
        .unwrap();
        assert_eq!(iface.structs.len(), 1, "{:?}", iface.structs);
    }

    /// A recursive generic closes on the memo; polymorphic recursion — the
    /// template instantiating *itself* at a different argument — asks for
    /// infinitely many wire shapes and is refused (FR0075).
    #[test]
    fn polymorphic_recursion_is_refused_and_ordinary_recursion_is_not() {
        assert!(check_src(
            "#[bridge(data)] pub struct Node<T> { pub v: T, pub next: Option<Box<Node<T>>> } \
             #[bridge(sync)] pub fn f(n: Node<i64>) -> i64 { 0 }"
        )
        .is_ok());
        let ds = check_src(
            "#[bridge(data)] pub struct L<T> { pub v: T, pub next: Option<Box<L<Vec<T>>>> } \
             #[bridge(sync)] pub fn f(l: L<i64>) -> i64 { 0 }",
        )
        .err()
        .unwrap();
        assert_eq!(ds[0].code, "FR0075", "{ds:?}");
        // Mutual polymorphic recursion is the same fact through two hops.
        let ds = check_src(
            "#[bridge(data)] pub struct A<T> { pub b: Option<Box<B<Vec<T>>>> } \
             #[bridge(data)] pub struct B<U> { pub a: Option<Box<A<U>>> } \
             #[bridge(sync)] pub fn f(a: A<i64>) -> i64 { 0 }",
        )
        .err()
        .unwrap();
        assert_eq!(ds[0].code, "FR0075", "{ds:?}");
        // An argument that is itself an instantiation of the same template is
        // NOT recursion: it is registered before the head that names it.
        assert!(check_src(
            "#[bridge(data)] pub struct P<T> { pub x: T } \
             #[bridge(sync)] pub fn f(p: P<P<i64>>) -> i64 { 0 }"
        )
        .is_ok());
        // A **closed** argument builds from nothing, so it is not growth
        // however the source is ordered. Reading a walk's order instead of the
        // declarations answered these two differently.
        for src in [
            "#[bridge(data)] pub struct I { pub id: i64 } \
             #[bridge(data)] pub struct P<T> { pub x: T, pub p: Option<Box<P<i64>>> } \
             #[bridge(sync)] pub fn f(v: P<I>) -> i64 { 0 } \
             #[bridge(sync)] pub fn g(v: P<i64>) -> i64 { 0 }",
            "#[bridge(data)] pub struct I { pub id: i64 } \
             #[bridge(data)] pub struct P<T> { pub x: T, pub p: Option<Box<P<i64>>> } \
             #[bridge(sync)] pub fn g(v: P<i64>) -> i64 { 0 } \
             #[bridge(sync)] pub fn f(v: P<I>) -> i64 { 0 }",
        ] {
            assert!(check_src(src).is_ok(), "{src}");
        }
        // A generic `impl`'s member signatures are edges of the same graph:
        // every instantiation carries the member, so this asks for
        // `A<i64>` → `B<i64>` → `A<Vec<i64>>` → … and never closes. It reaches
        // the growth through another template's FIELD, which is why the rule
        // cannot be a local test on the signature — and rustc does not catch
        // it, because a generic fn is monomorphized on demand.
        let ds = check_src(
            "#[bridge(data)] pub struct A<T> { pub y: T } \
             #[bridge(data)] pub struct B<T> { pub x: A<Vec<T>> } \
             #[bridge] impl<T> A<T> { #[bridge(sync)] pub fn f(&self) -> B<T> { todo!() } } \
             #[bridge(sync)] pub fn a(p: A<i64>) -> i64 { 0 }",
        )
        .err()
        .unwrap();
        assert_eq!(ds[0].code, "FR0075", "{ds:?}");
        // Growth that leads nowhere is finite and is accepted: `Wrapper` never
        // names `Page` back, so the set is {Page<i64>, Wrapper<Vec<i64>>}.
        assert!(check_src(
            "#[bridge(data)] pub struct Wrapper<U> { pub x: U } \
             #[bridge(data)] pub struct Page<T> { pub items: Vec<T> } \
             #[bridge] impl<T> Page<T> { \
               #[bridge(sync)] pub fn wrap(&self) -> Wrapper<Vec<T>> { todo!() } } \
             #[bridge(sync)] pub fn a(p: Page<i64>) -> i64 { 0 }"
        )
        .is_ok());
    }

    /// FR0073 — an application whose head is not a generic data template, and
    /// the wrong arity.
    #[test]
    fn a_type_application_names_a_generic_data_type_at_its_own_arity() {
        for (src, needle) in [
            (
                "#[bridge(data)] pub struct P { x: i64 } \
                 #[bridge(sync)] pub fn f(a: P<i64>) {}",
                "has no type parameters",
            ),
            (
                "#[bridge(sync)] pub fn f(a: Missing<i64>) {}",
                "no bridged generic data type",
            ),
            (
                "#[bridge(data)] pub struct P<T> { x: T } \
                 #[bridge(sync)] pub fn f(a: P<i64, i64>) {}",
                "takes 1 type argument",
            ),
            (
                "#[bridge(data)] pub enum E<L, R> { A(L), B(R) } \
                 #[bridge(sync)] pub fn f(a: E<i64>) {}",
                "takes 2 type arguments",
            ),
        ] {
            let ds = check_src(src).err().unwrap();
            assert_eq!(ds[0].code, "FR0073", "{src}: {ds:?}");
            assert!(ds[0].message.contains(needle), "{src}: {}", ds[0].message);
        }
        // A template named with no arguments at all is the same fact, and gets
        // its own message: the declaration exists, which is what FR0003 would
        // have denied.
        let ds = check_src(
            "#[bridge(data)] pub struct P<T> { x: T } #[bridge(sync)] pub fn f(a: P) {}",
        )
        .err()
        .unwrap();
        assert_eq!(ds[0].code, "FR0073", "{ds:?}");
        assert!(ds[0].message.contains("is generic"), "{}", ds[0].message);
    }

    /// `Self` in a generic declaration's own field means the declaration
    /// **applied to its own parameters** — `Chain<T>`, not `Chain`, which is
    /// not a type at all. Before this it substituted the bare name and the
    /// field became an unknown type, reporting that a declaration the author
    /// had written did not exist.
    #[test]
    fn self_in_a_generic_declaration_carries_its_parameters() {
        let iface = check_src(
            "#[bridge(data)] pub struct Chain<T> { pub v: T, pub next: Option<Box<Self>> }              #[bridge(sync)] pub fn f(c: Chain<i64>) -> i64 { 0 }",
        )
        .unwrap();
        // The template's own field is the OPEN application, which is what the
        // generic Dart class declares; the instantiation's is the closed one,
        // and it closed on the memo rather than recurring forever.
        assert_eq!(
            iface.generic_structs[0].fields[1].ty,
            Type::Option(Box::new(Type::Boxed(Box::new(Type::App(
                "Chain".into(),
                vec![Type::Param("T".into())]
            )))))
        );
        assert_eq!(
            iface.struct_decl("Chain<i64>").unwrap().fields[1].ty,
            Type::Option(Box::new(Type::Boxed(Box::new(Type::Struct(
                "Chain<i64>".into()
            )))))
        );
        // The same in an enum variant.
        let iface = check_src(
            "#[bridge(data)] pub enum Tree<T> { Leaf(T), Pair(Box<Self>, Box<Self>) }              #[bridge(sync)] pub fn f(t: Tree<i64>) -> i64 { 0 }",
        )
        .unwrap();
        assert_eq!(
            iface.enum_decl("Tree<i64>").unwrap().variants[1].fields[0].ty,
            Type::Boxed(Box::new(Type::Enum("Tree<i64>".into())))
        );
    }

    /// FR0074 — an `Option` argument bound to a parameter the template writes
    /// under an `Option`. The generic class declares `T?`, Dart has no
    /// nullable-of-nullable, and the wire keeps `Some(None)` and `None` apart.
    #[test]
    fn an_option_argument_under_an_option_is_refused() {
        let ds = check_src(
            "#[bridge(data)] pub struct P<T> { pub x: Option<T> } \
             #[bridge(sync)] pub fn f(p: P<Option<i64>>) -> i64 { 0 }",
        )
        .err()
        .unwrap();
        assert_eq!(ds[0].code, "FR0074", "{ds:?}");
        // Every other container resets the nesting, because every other
        // container renders compositionally.
        assert!(check_src(
            "#[bridge(data)] pub struct P<T> { pub x: Option<Vec<T>> } \
             #[bridge(sync)] pub fn f(p: P<Option<i64>>) -> i64 { 0 }"
        )
        .is_ok());
        // And a non-optional argument under an `Option` is the ordinary case.
        assert!(check_src(
            "#[bridge(data)] pub struct P<T> { pub x: Option<T> } \
             #[bridge(sync)] pub fn f(p: P<i64>) -> i64 { 0 }"
        )
        .is_ok());
    }

    /// A representation marker inside a type argument is checked (FR0062) and
    /// then erased, so `Page<Data<Item>>` and `Page<Item>` are ONE
    /// instantiation and one schema fingerprint — the property a marker has
    /// everywhere else.
    #[test]
    fn a_marker_in_a_type_argument_is_checked_and_erased() {
        let iface = check_src(
            "#[bridge(data)] pub struct Item { pub id: i64 } \
             #[bridge(data)] pub struct P<T> { pub x: T } \
             #[bridge(sync)] pub fn a(p: P<Item>) -> i64 { 0 } \
             #[bridge(sync)] pub fn b(p: P<Data<Item>>) -> i64 { 0 }",
        )
        .unwrap();
        assert_eq!(iface.structs.len(), 2, "{:?}", iface.structs);
        assert!(iface.struct_decl("P<Item>").is_some());
        let ds = check_src(
            "#[bridge(data)] pub struct Item { pub id: i64 } \
             #[bridge(data)] pub struct P<T> { pub x: T } \
             #[bridge(sync)] pub fn a(p: P<Locked<Item>>) -> i64 { 0 }",
        )
        .err()
        .unwrap();
        assert_eq!(ds[0].code, "FR0062", "{ds:?}");
    }

    /// An instantiation is a declaration like any other, so the direction rule
    /// reads it like any other: `Page<Doc>` reaches a handle and is
    /// return-only, `Page<i64>` is not. That is the whole reason expansion
    /// happens before the rules rather than inside them.
    #[test]
    fn a_handle_argument_makes_only_that_instantiation_return_only() {
        let src = "#[bridge(frozen)] pub struct Doc { pub id: i64 } \
                   #[bridge(data)] pub struct P<T> { pub x: T } \
                   #[bridge(sync)] pub fn out() -> P<Doc> { todo!() } \
                   #[bridge(sync)] pub fn plain(p: P<i64>) -> i64 { 0 }";
        assert!(check_src(src).is_ok(), "{:?}", check_src(src).err());
        let ds = check_src(
            "#[bridge(frozen)] pub struct Doc { pub id: i64 } \
             #[bridge(data)] pub struct P<T> { pub x: T } \
             #[bridge(sync)] pub fn takes(p: P<Doc>) {}",
        )
        .err()
        .unwrap();
        assert_eq!(ds[0].code, "FR0004", "{ds:?}");
    }

    /// The identifier the codec functions are named after. `Page<Item>` is the
    /// declaration's name and is not an identifier; the stem is, it is
    /// injective, and its leading length digit is what makes it unable to
    /// collide with a declared type's stem — no Rust identifier starts with a
    /// digit.
    #[test]
    fn an_instantiation_stem_is_an_identifier_that_cannot_collide() {
        let a = instance_stem("Page<Item>");
        assert!(a.starts_with(|c: char| c.is_ascii_digit()), "{a}");
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'), "{a}");
        // Injective where the naive `Page_Item` spelling is not.
        assert_ne!(instance_stem("A<B_C>"), instance_stem("A_B<C>"));
        assert_ne!(instance_stem("P<Vec<i64>>"), instance_stem("P<Vec<i64> >"));
    }

    /// All three struct shapes cross as data. A tuple struct's fields carry
    /// synthesized names (`field0`, …) and land positionally, exactly as a
    /// tuple *enum variant*'s already do; a unit struct is the zero-field case
    /// of that, a singleton whose instances are all equal.
    ///
    /// The shape decides nothing about what a field may *be*: a newtype over a
    /// type this interface does not bridge is still FR0003, and the shape no
    /// longer changes the advice.
    #[test]
    fn every_struct_shape_crosses_as_data() {
        for src in [
            "#[bridge(data)] pub struct T(pub i32, pub String);",
            "#[bridge(data)] pub struct U;",
            "#[bridge(data)] pub struct N {}",
            "#[bridge(frozen)] pub struct W(pub i32);",
        ] {
            assert!(check_src(src).is_ok(), "{src}");
        }
        assert_eq!(
            codes(check_src("#[bridge(data)] pub struct W(pub semver::Version);")),
            vec!["FR0003"]
        );
        // The synthesized names reach the IR in declaration order, which is
        // wire order — the emitters read the shape, never these names.
        let iface = check_src("#[bridge(data)] pub struct T(pub i32, pub String);").unwrap();
        let t = iface.struct_decl("T").unwrap();
        assert_eq!(t.shape, StructShape::Tuple);
        assert_eq!(
            t.fields.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
            ["field0", "field1"]
        );
    }

    /// FR0003 — and the three shapes, because one word ("newtype") covered
    /// two of them and left the third with no way back. The message is the
    /// how-to for a foreign type; asserting only the code lets it rot into
    /// advice that does not work.
    #[test]
    fn unknown_type_is_actionable() {
        let ds = check_src("#[bridge] pub fn f(x: Mystery) {}").err().unwrap();
        assert_eq!(ds[0].code, "FR0003");
        for needle in ["confined", "bytes(...)", "#[bridge(data)] struct", "From"] {
            assert!(ds[0].message.contains(needle), "{}", ds[0].message);
        }
    }

    /// FR0048 — the claim and a blocking contract cannot both be true.
    ///
    /// Asserted as the *whole* code list, not `ds[0]`: an
    /// `on_contention = "block"` member is `requires_native`, so it is absent
    /// from the web subset and the diagnostic must fire exactly once. A second
    /// copy would mean the web pass had somehow kept it.
    #[test]
    fn no_block_contradicts_a_blocking_contention_contract() {
        let ds = check_src(
            r#"
            #[bridge(locked)] pub struct Doc { x: i32 }
            #[bridge] impl Doc {
                #[bridge(sync, no_block, on_contention = "block")]
                pub fn bump(&self) -> i32 { 0 }
            }
            "#,
        )
        .err()
        .unwrap();
        assert_eq!(
            ds.iter().map(|d| d.code).collect::<Vec<_>>(),
            vec!["FR0048"]
        );
        assert!(ds[0].message.contains("no_block"), "{}", ds[0].message);
        assert!(
            ds[0].message.contains("on_contention"),
            "{}",
            ds[0].message
        );
    }

    /// FR0049 — invoking a value-returning Dart callback waits for Dart's
    /// answer, which the claim forbids.
    #[test]
    fn no_block_contradicts_a_returning_dart_callback() {
        let ds = check_src(
            r#"
            #[bridge(no_block)]
            pub fn ask(f: DartFunction<i64, i64>) -> i64 { 0 }
            "#,
        );
        assert!(
            codes(ds).contains(&"FR0049"),
            "a returning callback on a claimed non-async member must be refused"
        );
    }

    /// The exemption in FR0049 is load-bearing, not an oversight: an
    /// `async fn` awaits the reply through `call_async` instead of parking, so
    /// the claim survives.
    #[test]
    fn no_block_allows_a_returning_callback_on_an_async_fn() {
        let iface = check_src(
            r#"
            #[bridge(no_block)]
            pub async fn ask(f: DartFunction<i64, i64>) -> i64 { 0 }
            "#,
        )
        .unwrap();
        assert!(iface.functions[0].no_block);
    }

    /// A block-level claim strengthens its methods and a method-level
    /// `#[bridge(...)]` cannot shed it. Replace semantics here would let
    /// `#[bridge(sync)]` on one method silently drop out of the proof while
    /// the block still read as covering it.
    #[test]
    fn no_block_is_inherited_by_every_method_of_a_claimed_impl() {
        let iface = check_src(
            r#"
            #[bridge(confined)] pub struct Doc { x: i32 }
            #[bridge(no_block)] impl Doc {
                #[bridge(sync)] pub fn len(&self) -> usize { 0 }
                pub fn touch(&self) {}
            }
            "#,
        )
        .unwrap();
        let claimed: Vec<_> = iface
            .functions
            .iter()
            .filter(|f| !f.is_actor_drop)
            .map(|f| (f.name.as_str(), f.no_block))
            .collect();
        assert!(
            claimed.iter().all(|&(_, c)| c),
            "every method of a claimed impl must carry the claim: {claimed:?}"
        );
    }

    #[test]
    fn confined_methods_become_sync() {
        let iface = check_src(
            r#"
            #[bridge(confined)] pub struct Doc { x: i32 }
            #[bridge] impl Doc {
                pub fn len(&self) -> usize { 0 }
            }
            "#,
        )
        .unwrap();
        assert_eq!(iface.functions[0].exec, Exec::Sync);
    }

    #[test]
    fn confined_borrow_in_async_free_fn_is_rejected_with_optin_named() {
        let ds = check_src(
            r#"
            #[bridge(confined)] pub struct Doc { x: i32 }
            #[bridge] pub fn doc_len(doc: &Doc) -> usize { 0 }
            "#,
        )
        .err()
        .unwrap();
        assert_eq!(ds[0].code, "FR0012");
        assert!(ds[0].message.contains("#[bridge(sync)]"), "{}", ds[0].message);
    }

    #[test]
    fn locked_sync_requires_contention_contract() {
        let ds = check_src(
            r#"
            #[bridge(locked)] pub struct Cache { x: i32 }
            #[bridge] impl Cache {
                #[bridge(sync)]
                pub fn get(&self) -> i32 { 0 }
            }
            "#,
        )
        .err()
        .unwrap();
        assert_eq!(ds[0].code, "FR0007");
        assert!(ds[0].message.contains("on_contention"), "{}", ds[0].message);
        assert!(ds[0].message.contains("ContentionError"), "{}", ds[0].message);
    }

    #[test]
    fn locked_sync_with_error_contract_is_accepted() {
        let iface = check_src(
            r#"
            #[bridge(locked)] pub struct Cache { x: i32 }
            #[bridge] impl Cache {
                #[bridge(sync, on_contention = "error")]
                pub fn get(&self) -> i32 { 0 }
            }
            "#,
        )
        .unwrap();
        assert_eq!(iface.functions[0].on_contention, Some(OnContention::Error));
    }

    /// Both contracts are accepted, and only one of them is native-only. The
    /// difference is what each does when the lock is taken: `block` waits, by
    /// declaration, which the browser main thread may not do; `error` refuses,
    /// and its release reaches no wait instruction either, so it is on the web
    /// surface with everything else.
    ///
    /// The async sibling is the control: a dispatched locked member acquires
    /// and releases wherever the executor runs it, and was always portable.
    #[test]
    fn the_blocking_contract_is_native_only_and_the_try_lock_is_not() {
        let iface = check_src(
            r#"
            #[bridge(locked)] pub struct Cache { x: i32 }
            #[bridge] impl Cache {
                #[bridge(sync, on_contention = "block")]
                pub fn get(&self) -> i32 { 0 }
                #[bridge(sync, on_contention = "error")]
                pub fn try_get(&self) -> i32 { 0 }
                pub fn read(&self) -> i32 { 0 }
            }
            "#,
        )
        .unwrap();
        let f = |n: &str| iface.functions.iter().find(|f| f.name == n).unwrap();
        assert!(f("get").requires_native, "block waits to acquire");
        assert!(!f("try_get").requires_native, "the try-lock never waits");
        assert!(!f("read").requires_native, "a dispatched locked member is portable");
    }

    /// The other half of the same fact, asserted where it bites: the web pass
    /// now *sees* a sync try-lock member, and accepts it. Nothing else about
    /// the member changes — it is still `#[bridge(sync)]` and still throws
    /// `ContentionException` when the object is taken.
    #[test]
    fn web_facts_accept_a_try_lock_contract() {
        let iface = parse_source(
            r#"
            #[bridge(locked)] pub struct Cache { x: i32 }
            #[bridge] impl Cache {
                #[bridge(sync, on_contention = "error")]
                pub fn try_get(&self) -> i32 { 0 }
            }
            "#,
            "crate::api",
        )
        .unwrap();
        let out = check_pass(iface, &Capabilities::web()).expect("accepted under web facts");
        assert_eq!(
            out.functions[0].on_contention,
            Some(OnContention::Error),
            "and it is still the try-lock, not a rewritten member"
        );
    }

    /// `no_block` refuses the blocking contract and accepts the try-lock, and
    /// the difference is the whole of what the claim means here: `block`
    /// declares that the acquisition waits, while `error` refuses instead and
    /// its release reaches no wait instruction. The accepted one is not merely
    /// tolerated — it is settled by an artifact, which the settlement assert
    /// below is the codegen half of.
    #[test]
    fn no_block_refuses_the_blocking_contract_and_settles_the_try_lock() {
        let ds = codes(check_src(
            r#"
            #[bridge(locked)] pub struct Cache { x: i32 }
            #[bridge] impl Cache {
                #[bridge(sync, on_contention = "block", no_block)]
                pub fn get(&self) -> i32 { 0 }
            }
            "#,
        ));
        assert!(ds.contains(&"FR0048"), "{ds:?}");

        let iface = check_src(
            r#"
            #[bridge(locked)] pub struct Cache { x: i32 }
            #[bridge] impl Cache {
                #[bridge(sync, on_contention = "error", no_block)]
                pub fn try_get(&self) -> i32 { 0 }
                #[bridge(no_block)]
                pub fn read(&self) -> i32 { 0 }
            }
            "#,
        )
        .expect("the try-lock keeps the claim");
        let f = |n: &str| iface.functions.iter().find(|f| f.name == n).unwrap();
        assert!(!f("try_get").requires_native, "and stays on the web surface");
        // The claim is settled by placement on the dispatched sibling and by an
        // artifact on the synchronous one — the two halves of `no_block`'s one
        // definition, and the reason refusing the try-lock cost a proof.
        let settled: Vec<_> = crate::emit_rust::claims(&iface)
            .into_iter()
            .map(|(s, f)| (f.name.clone(), s.token()))
            .collect();
        assert!(
            settled.contains(&("try_get".to_string(), "artifact")),
            "{settled:?}"
        );
        assert!(
            settled.contains(&("read".to_string(), "placement-dispatch")),
            "{settled:?}"
        );
    }

    #[test]
    fn a_declaration_is_enough_to_be_native_only() {
        // The point of the whole flag. Before it, `requires_native` could only
        // be reached by an `on_contention = "block"` contract or a
        // value-returning DartFunction — so a member whose only problem was
        // that its dependency does not build for wasm32 had to grow a *phantom*
        // returning callback it never called, purely to trip the derivation.
        // This function has neither construct and is native-only anyway.
        let iface = check_src(
            r#"
            #[bridge(sync, native_only)] pub fn dial(ticket: String) -> i64 { 0 }
            #[bridge(sync)] pub fn portable(x: i64) -> i64 { x }
            "#,
        )
        .unwrap();
        let dial = iface.functions.iter().find(|f| f.name == "dial").unwrap();
        assert!(dial.requires_native && dial.native_only);
        assert!(dial.on_contention.is_none(), "no blocking contract");
        assert!(dial.params.iter().all(|p| !matches!(p.ty, Type::DartObject(_))));
        let p = iface.functions.iter().find(|f| f.name == "portable").unwrap();
        assert!(!p.requires_native, "an undeclared sibling stays portable");
    }

    #[test]
    fn native_only_propagates_from_a_type_to_everything_naming_it() {
        // The type-level declaration is not just shorthand for marking each
        // member: a free function that merely *names* the type is native-only
        // too, at any depth. Without that its glue would name a Rust item the
        // wasm build does not have — the E0425 this flag exists to prevent.
        let iface = check_src(
            r#"
            #[bridge(frozen, native_only)] pub struct Node { x: i32 }
            #[bridge] impl Node {
                #[bridge(sync)] pub fn id(&self) -> i64 { 0 }
            }
            #[bridge(sync)] pub fn describe(n: &Node) -> String { String::new() }
            #[bridge(sync)] pub fn maybe() -> Option<Node> { None }
            #[bridge(sync)] pub fn unrelated(x: i64) -> i64 { x }
            "#,
        )
        .unwrap();
        for name in ["id", "describe", "maybe"] {
            let f = iface.functions.iter().find(|f| f.name == name).unwrap();
            assert!(f.requires_native, "{name} names a native-only type");
            assert!(f.native_only, "{name}: inherited, so it reads as declared");
        }
        let u = iface.functions.iter().find(|f| f.name == "unrelated").unwrap();
        assert!(!u.requires_native);
    }

    #[test]
    fn a_native_only_actors_synthetic_drop_is_native_only_too() {
        // finalize() appends the drop *after* the derivation runs, so it takes
        // the fact from the type directly. A portable drop would put a dispatch
        // arm naming a wasm-absent type back into the web build.
        let iface = check_src(
            r#"
            #[bridge(actor, native_only)] pub struct Node { x: i32 }
            #[bridge] impl Node {
                pub fn poke(&mut self) {}
            }
            "#,
        )
        .unwrap();
        let drop = iface
            .functions
            .iter()
            .find(|f| f.is_actor_drop)
            .expect("synthetic drop");
        assert!(drop.requires_native && drop.native_only);
    }

    #[test]
    fn native_only_and_the_runtime_fail_opt_in_compose() {
        // FR0030's condition is `!requires_native`, and a declared member now
        // satisfies it — so the opt-in is finally usable for the reason it was
        // written: a member absent on web that portable Dart still names.
        let iface = check_src(
            "#[bridge(sync, native_only, web = \"runtime_fail\")] pub fn dial() -> i64 { 0 }",
        )
        .unwrap();
        let f = &iface.functions[0];
        assert!(f.requires_native && f.native_only && f.web_runtime_fail);
    }

    #[test]
    fn a_stub_cannot_reference_a_type_the_web_surface_omits() {
        // FR0032. A member of a native-only type has no class to live in, and
        // one merely naming it would reference an absent class in its signature.
        let ds = codes(check_src(
            r#"
            #[bridge(frozen, native_only)] pub struct Node { x: i32 }
            #[bridge] impl Node {
                #[bridge(sync, web = "runtime_fail")] pub fn id(&self) -> i64 { 0 }
            }
            "#,
        ));
        assert_eq!(ds, vec!["FR0032"]);
        let ds = codes(check_src(
            r#"
            #[bridge(frozen, native_only)] pub struct Node { x: i32 }
            #[bridge(sync, web = "runtime_fail")] pub fn describe(n: &Node) -> String { String::new() }
            "#,
        ));
        assert_eq!(ds, vec!["FR0032"]);
    }

    #[test]
    fn a_trait_and_its_implementors_must_agree_on_native_only() {
        // FR0033, symmetric like FR0026 — both directions break the web
        // surface, so neither is the "safe" one to allow.
        let native_only_impl = r#"
            #[bridge(frozen)] pub trait Store: Send + Sync { fn get(&self) -> i64; }
            #[bridge(frozen, native_only)] pub struct Disk { x: i32 }
            #[bridge] impl Store for Disk { fn get(&self) -> i64 { 0 } }
        "#;
        assert!(codes(check_src(native_only_impl)).contains(&"FR0033"));
        let native_only_trait = r#"
            #[bridge(frozen, native_only)] pub trait Store: Send + Sync { fn get(&self) -> i64; }
            #[bridge(frozen)] pub struct Disk { x: i32 }
            #[bridge] impl Store for Disk { fn get(&self) -> i64 { 0 } }
        "#;
        assert!(codes(check_src(native_only_trait)).contains(&"FR0033"));
        let agreed = r#"
            #[bridge(frozen, native_only)] pub trait Store: Send + Sync { fn get(&self) -> i64; }
            #[bridge(frozen, native_only)] pub struct Disk { x: i32 }
            #[bridge] impl Store for Disk { fn get(&self) -> i64 { 0 } }
        "#;
        assert!(!codes(check_src(agreed)).contains(&"FR0033"));
    }

    #[test]
    fn a_cfg_gated_bridged_item_must_declare_what_the_cfg_means() {
        // FR0034 — the charter's "undeclared hazard" case. Codegen never
        // evaluates cfg predicates, so before this the member was emitted into
        // both surfaces regardless and the wasm build died at E0425 *inside
        // generated code*, naming nothing the author wrote.
        let ds = codes(check_src(
            "#[cfg(not(target_family = \"wasm\"))]\n\
             #[bridge(sync)] pub fn dial() -> i64 { 0 }",
        ));
        assert_eq!(ds, vec!["FR0034"]);
        // Declaring what the gate means is exactly the fix the message names.
        assert!(check_src(
            "#[cfg(not(target_family = \"wasm\"))]\n\
             #[bridge(sync, native_only)] pub fn dial() -> i64 { 0 }"
        )
        .is_ok());
        // A cfg on the impl block reaches its methods, and so does the
        // declaration that answers it.
        assert_eq!(
            codes(check_src(
                "#[bridge(frozen)] pub struct C { x: i32 }\n\
                 #[cfg(unix)]\n#[bridge] impl C { #[bridge(sync)] pub fn f(&self) -> i64 { 0 } }"
            )),
            vec!["FR0034"]
        );
        assert!(check_src(
            "#[bridge(frozen)] pub struct C { x: i32 }\n\
             #[cfg(unix)]\n#[bridge(native_only)] impl C { #[bridge(sync)] pub fn f(&self) -> i64 { 0 } }"
        )
        .is_ok());
    }

    #[test]
    fn a_cfg_gated_type_is_caught_too_and_cfg_attr_is_not() {
        // FR0034 at type level: the drop/finalize exports name `crate::…::T`
        // unconditionally, so a cfg-gated type is the same silent E0425.
        assert_eq!(
            codes(check_src(
                "#[cfg(not(target_family = \"wasm\"))]\n\
                 #[bridge(frozen)] pub struct Disk { x: i32 }"
            )),
            vec!["FR0034"]
        );
        assert!(check_src(
            "#[cfg(not(target_family = \"wasm\"))]\n\
             #[bridge(frozen, native_only)] pub struct Disk { x: i32 }"
        )
        .is_ok());
        // Same for a bridged trait declaration.
        assert_eq!(
            codes(check_src(
                "#[cfg(unix)]\n\
                 #[bridge(frozen)] pub trait S: Send + Sync { fn g(&self) -> i64; }"
            )),
            vec!["FR0034"]
        );
        // `cfg_attr` conditions another attribute; it never removes the item,
        // so it is no hazard — and declaring `native_only` to satisfy a
        // spurious diagnostic would wrongly drop a portable member from web.
        assert!(check_src(
            "#[cfg_attr(test, allow(dead_code))]\n\
             #[bridge(sync)] pub fn f() -> i64 { 0 }"
        )
        .is_ok());
    }

    #[test]
    fn an_impl_level_native_only_cannot_be_dropped_by_a_method_attribute() {
        // The one place `native_only` deviates from the inherit-or-replace rule
        // every other option follows. Under replace semantics the method below
        // would come out portable, and its glue would name a wasm-absent item —
        // silently, which is the failure this flag exists to make loud.
        let iface = check_src(
            r#"
            #[bridge(frozen)] pub struct C { x: i32 }
            #[bridge(native_only)] impl C {
                #[bridge(sync)] pub fn f(&self) -> i64 { 0 }
                pub fn g(&self) -> i64 { 0 }
            }
            "#,
        )
        .unwrap();
        for name in ["f", "g"] {
            let m = iface.functions.iter().find(|f| f.name == name).unwrap();
            assert!(m.requires_native, "{name} cannot be more portable than its impl");
        }
    }

    #[test]
    fn web_runtime_fail_opt_in_accepted_on_native_only_members() {
        // The opt-in rides the same requires_native derivation: an opted member
        // is still native-only (omitted from the web *capability* check pass),
        // it just carries the flag the emitter reads to make a throwing web stub
        // instead of a compile-time omission. Both a blocking contract and a
        // value-returning DartFunction can carry it; an un-opted sibling stays
        // the default (native-only, flag off).
        let iface = check_src(
            r#"
            #[bridge(locked)] pub struct Cache { x: i32 }
            #[bridge] impl Cache {
                #[bridge(sync, on_contention = "block", web = "runtime_fail")]
                pub fn blocking_read(&self) -> i32 { 0 }
                #[bridge(sync, on_contention = "block")]
                pub fn blocking_get(&self) -> i32 { 0 }
            }
            #[bridge(web = "runtime_fail")]
            pub fn transform(f: DartFunction<i64, i64>) -> i64 { 0 }
            "#,
        )
        .unwrap();
        let br = iface.functions.iter().find(|f| f.name == "blocking_read").unwrap();
        assert!(br.requires_native && br.web_runtime_fail);
        let bg = iface.functions.iter().find(|f| f.name == "blocking_get").unwrap();
        assert!(bg.requires_native && !bg.web_runtime_fail, "un-opted stays default");
        let tr = iface.functions.iter().find(|f| f.name == "transform").unwrap();
        assert!(tr.requires_native && tr.web_runtime_fail);
    }

    #[test]
    fn web_runtime_fail_on_a_portable_member_is_rejected() {
        // FR0030: the opt-in is meaningless on a member that already runs on
        // web (nothing to fail at runtime). A plain free function:
        let ds = codes(check_src(
            "#[bridge(sync, web = \"runtime_fail\")] pub fn f() {}",
        ));
        assert_eq!(ds, vec!["FR0030"]);
        // A void (fire-and-forget) DartCallback is portable too — still rejected.
        let ds = codes(check_src(
            "#[bridge(web = \"runtime_fail\")] pub fn g(cb: DartCallback<i64>) {}",
        ));
        assert_eq!(ds, vec!["FR0030"]);
        // A genuinely native-only returning callback (a plain pool member,
        // which must use the blocking `call`) accepts it.
        assert!(check_src(
            "#[bridge(web = \"runtime_fail\")] pub fn t(f: DartFunction<i64, i64>) -> i64 { 0 }"
        )
        .is_ok());
        // But an `async fn` returning callback is portable (awaits call_async),
        // so the opt-in is now meaningless on it — FR0030 rejects it.
        let ds = codes(check_src(
            "#[bridge(web = \"runtime_fail\")] pub async fn transform(f: DartFunction<i64, i64>) -> i64 { 0 }"
        ));
        assert_eq!(ds, vec!["FR0030"]);
    }

    #[test]
    fn web_facts_still_reject_blocking_members() {
        // The web pass excludes native-only members by construction; FR0008
        // remains the loud backstop should one ever reach it.
        let iface = parse_source(
            r#"
            #[bridge(locked)] pub struct Cache { x: i32 }
            #[bridge] impl Cache {
                #[bridge(sync, on_contention = "block")]
                pub fn get(&self) -> i32 { 0 }
            }
            "#,
            "crate::api",
        )
        .unwrap();
        let ds = check_pass(iface, &Capabilities::web()).err().unwrap();
        assert_eq!(ds[0].code, "FR0008");
    }

    #[test]
    fn frozen_mut_rejected() {
        assert_eq!(
            codes(check_src(
                r#"
                #[bridge(frozen)] pub struct Snapshot { x: i32 }
                #[bridge] impl Snapshot {
                    pub fn set(&mut self, v: i32) {}
                }
                "#,
            )),
            vec!["FR0006"]
        );
    }

    /// A concrete handle by value is a **consume**: the call takes the
    /// object, and the Dart caller says so with `take()`. Accepted at the
    /// root, in an `Option`, in a list and in a tuple — the return path's
    /// ownable positions minus a struct field.
    #[test]
    fn a_handle_by_value_is_consumed_in_every_ownable_position() {
        for ty in [
            "Snapshot",
            "Option<Snapshot>",
            "Vec<Snapshot>",
            "std::collections::VecDeque<Snapshot>",
            "(Snapshot, i64)",
            "Option<Vec<(Snapshot, i64)>>",
        ] {
            let src = format!(
                "#[bridge(frozen)] pub struct Snapshot {{ x: i32 }}\n\
                 #[bridge(sync)] pub fn consume(s: {ty}) {{}}"
            );
            assert!(check_src(&src).is_ok(), "{ty}: {:?}", check_src(&src).err());
        }
    }

    /// A set and a map are consume positions like any other container — key,
    /// value or both, at any depth.
    #[test]
    fn a_handle_is_consumed_from_a_map_or_a_set() {
        for ty in [
            "std::collections::HashMap<i64, Snapshot>",
            "std::collections::HashSet<Snapshot>",
            "std::collections::BTreeMap<Snapshot, i64>",
            "std::collections::BTreeSet<Snapshot>",
            "std::collections::HashMap<Snapshot, Snapshot>",
            "Vec<std::collections::HashMap<String, Snapshot>>",
            "std::collections::HashMap<String, &Snapshot>",
            "std::collections::HashSet<&Snapshot>",
        ] {
            let src = format!(
                "#[bridge(frozen)] pub struct Snapshot {{ x: i32 }}\n\
                 #[bridge(sync)] pub fn consume(s: {ty}) {{}}"
            );
            assert!(check_src(&src).is_ok(), "{ty}: {:?}", check_src(&src).err());
        }
    }

    /// A borrowed **value** in a set or a map stays refused: it would be a
    /// second container of references built over the decoded one. A borrowed
    /// handle is not that shape, and the message says so.
    #[test]
    fn a_borrowed_value_in_a_set_or_a_map_is_refused() {
        for ty in [
            "std::collections::HashSet<&str>",
            "std::collections::HashMap<&str, i64>",
            "std::collections::BTreeMap<i64, &String>",
        ] {
            let src = format!("#[bridge(sync)] pub fn f(s: {ty}) {{}}");
            let ds = check_src(&src).unwrap_err();
            let d = ds
                .iter()
                .find(|d| d.code == "FR0077")
                .unwrap_or_else(|| panic!("{ty}: {ds:?}"));
            assert!(d.message.contains("borrowed **value**"), "{}", d.message);
        }
        // Judged per reference: a set holding both names only the value one.
        let ds = check_src(
            "#[bridge(frozen)] pub struct S { x: i32 } \
             #[bridge(sync)] pub fn f(s: std::collections::HashSet<(&S, &str)>) {}",
        )
        .unwrap_err();
        assert_eq!(ds.iter().filter(|d| d.code == "FR0077").count(), 1, "{ds:?}");
        // A handle borrow under a set survives a container in between.
        assert!(check_src(
            "#[bridge(frozen)] pub struct S { x: i32 } \
             #[bridge(sync)] pub fn f(s: std::collections::HashSet<Vec<&S>>) {}",
        )
        .is_ok());
    }

    /// FR0004 is a **use-site** rule: which direction a value that holds a
    /// handle may travel, not whether it may hold one. Declaring the field is
    /// fine; returning it transfers each handle once; taking it as a parameter
    /// would need Dart to give up an ownership it has no syntax to give up.
    #[test]
    fn a_handle_in_a_data_type_may_be_returned_and_not_passed() {
        let decls = r#"
            #[bridge(frozen)] pub struct Snapshot { x: i32 }
            #[bridge(data)] pub struct Holder { pub snap: Snapshot, pub n: i64 }
        "#;
        // Declared alone: nothing to judge yet.
        assert!(check_src(decls).is_ok());
        // Returned, bare and through a container.
        for ret in ["Holder", "Option<Holder>", "Vec<Holder>"] {
            let src = format!("{decls} #[bridge] pub fn make() -> {ret} {{ todo!() }}");
            assert!(check_src(&src).is_ok(), "{ret}");
        }
        // Passed: refused, and the message has to say which direction works.
        let ds = check_src(&format!(
            "{decls} #[bridge] pub fn eat(h: Holder) {{}}"
        ))
        .unwrap_err();
        assert_eq!(ds.iter().map(|d| d.code).collect::<Vec<_>>(), vec!["FR0004"]);
        // A by-value data struct is the *consume* path, and it is refused for
        // a Dart-surface reason: one class, two field types.
        assert!(ds[0].message.contains("Consumed<"), "{}", ds[0].message);
        assert!(ds[0].message.contains("by reference"), "{}", ds[0].message);
        // Borrowed, the old reason still stands: nothing is transferred, so
        // there is nowhere for the handle to come from.
        let ds = check_src(&format!(
            "{decls} #[bridge] pub fn peek(h: &Holder) {{}}"
        ))
        .unwrap_err();
        assert_eq!(ds.iter().map(|d| d.code).collect::<Vec<_>>(), vec!["FR0004"]);
        assert!(ds[0].message.contains("transfers nothing"), "{}", ds[0].message);
        // Nested one declaration deeper, in both directions.
        let deep = format!("{decls} #[bridge(data)] pub struct Outer {{ pub h: Holder }}");
        assert!(check_src(&format!("{deep} #[bridge] pub fn make() -> Outer {{ todo!() }}")).is_ok());
        assert_eq!(
            codes(check_src(&format!("{deep} #[bridge] pub fn eat(o: Outer) {{}}"))),
            vec!["FR0004"]
        );
    }

    /// FR0026 — who may implement a bridged trait. A data type may not: its
    /// generated class cannot carry the handle surface the trait's Dart
    /// interface declares, so it could never stand where a `&dyn Trait` does.
    ///
    /// Silent without this, and the silence is the point: the methods would
    /// land on the data class and work, while the one thing the impl was
    /// written for would not compile in Dart with nothing having said why.
    #[test]
    fn a_bridged_traits_implementors_are_handles() {
        let decls = "#[bridge(confined)] pub trait Tally: Send { #[bridge(sync)] fn total(&self) -> i64; }";
        let ds = check_src(&format!(
            "{decls} #[bridge(data)] pub struct P {{ pub x: i64 }} \
             #[bridge] impl Tally for P {{ #[bridge(sync)] fn total(&self) -> i64 {{ 0 }} }}"
        ))
        .unwrap_err();
        let d = ds.iter().find(|d| d.code == "FR0026").unwrap();
        assert!(d.message.contains("crosses by value"), "{}", d.message);
        assert!(d.message.contains("inherent"), "{}", d.message);
        // A handle of the trait's own model is accepted, which is the contrast.
        assert!(check_src(&format!(
            "{decls} #[bridge(confined)] pub struct C {{ x: i64 }} \
             #[bridge] impl Tally for C {{ #[bridge(sync)] fn total(&self) -> i64 {{ 0 }} }}"
        ))
        .is_ok());
    }

    /// A data type reaching a `native_only` handle is native-only itself, at
    /// any depth. Both halves matter: the *members* that name it are omitted
    /// from the web surface (this), and so is the generated class
    /// (`emit_dart`), because its field type has no class there.
    #[test]
    fn a_data_type_inherits_native_only_from_a_handle_it_carries() {
        let iface = check_src(
            r#"
            #[bridge(confined, native_only)] pub struct NativeThing { n: i64 }
            #[bridge(data)] pub struct Holder { pub thing: NativeThing }
            #[bridge(data)] pub struct Outer { pub inner: Holder }
            #[bridge(sync)] pub fn outer() -> Outer { todo!() }
            "#,
        )
        .unwrap();
        let f = iface.functions.iter().find(|f| f.name == "outer").unwrap();
        assert!(f.requires_native, "a member returning it is web-absent");
        assert_eq!(
            super::decl_native_only(&iface, "Outer").as_deref(),
            Some("NativeThing"),
            "and so is the class, two declarations up"
        );
    }

    /// The one declaration-site refusal left in FR0004: a type pulled in both
    /// directions at once has no position at all.
    #[test]
    fn a_type_reaching_both_handle_kinds_is_refused_at_the_declaration() {
        let ds = check_src(
            r#"
            #[bridge(frozen)] pub struct Snapshot { x: i32 }
            #[bridge(data)] pub struct Both { pub snap: Snapshot, pub sink: StreamSink<i64> }
            "#,
        )
        .unwrap_err();
        let d = ds.iter().find(|d| d.code == "FR0004").unwrap();
        assert!(d.message.contains("opposite directions"), "{}", d.message);
        // Either alone is fine to declare.
        assert!(check_src(
            r#"
            #[bridge(frozen)] pub struct Snapshot { x: i32 }
            #[bridge(data)] pub struct Out { pub snap: Snapshot }
            #[bridge(data)] pub struct In { pub sink: StreamSink<i64> }
            "#,
        )
        .is_ok());
    }

    /// FR0002 — two Rust items minting one Dart name, in each scope a name is
    /// minted, and `dart_identifier` past each one.
    ///
    /// The mapping cannot be made injective: `to_lower_camel_case` collapses
    /// `word_count` and `wordCount`, and every compound name loses the boundary
    /// between its halves. Which item moves is the author's call.
    #[test]
    fn two_items_that_mint_one_dart_name_are_reported_and_renameable() {
        // (1) a member and a free function, through the fake's compound name.
        // (2) two compounds whose halves split differently.
        // (3) `to_lower_camel_case`, on two members of one class.
        // (4) two fields of one class.
        // (5) a variant class, which is `{Enum}{Variant}`.
        // (6) a unit-only enum's values share its class scope with its members.
        let cases: [(&str, &str); 6] = [
            (
                "#[bridge(data)] pub struct Point { pub x: i64 } \
                 #[bridge] impl Point { #[bridge(sync)] pub fn norm(&self) -> i64 { 0 } } \
                 #[bridge(sync)] pub fn point_norm(p: Point) -> i64 { 0 }",
                "pointNorm",
            ),
            (
                "#[bridge(data)] pub struct PointNorm { pub v: i64 } \
                 #[bridge] impl PointNorm { #[bridge(sync)] pub fn x(&self) -> i64 { 0 } } \
                 #[bridge(data)] pub struct Point { pub x: i64 } \
                 #[bridge] impl Point { #[bridge(sync)] pub fn norm_x(&self) -> i64 { 0 } }",
                "pointNormX",
            ),
            (
                "#[bridge(confined)] pub struct Doc { t: i64 } \
                 #[bridge] impl Doc { \
                   #[bridge(sync)] pub fn word_count(&self) -> i64 { 0 } \
                   #[allow(non_snake_case)] #[bridge(sync)] pub fn wordCount(&self) -> i64 { 1 } }",
                "wordCount",
            ),
            (
                "#[bridge(data)] pub struct P { pub word_count: i64, \
                   #[allow(non_snake_case)] pub wordCount: i64 }",
                "wordCount",
            ),
            (
                "#[bridge(data)] pub enum Shape { AB { n: i64 } } \
                 #[bridge(data)] pub enum ShapeA { B { n: i64 } } \
                 #[bridge(sync)] pub fn f(a: Shape, b: ShapeA) -> i64 { 0 }",
                "ShapeAB",
            ),
            (
                "#[bridge(data)] pub enum Color { Red, Green } \
                 #[bridge] impl Color { #[bridge(sync)] pub fn red(&self) -> bool { true } }",
                "red",
            ),
        ];
        for (src, name) in cases {
            let ds = check_src(src).unwrap_err();
            let hits: Vec<_> = ds.iter().filter(|d| d.code == "FR0002").collect();
            // One pair, one diagnostic — two free functions collide in the
            // library scope *and* on the crate fake, and saying so twice would
            // report one rename as two.
            assert_eq!(hits.len(), 1, "{src}: {ds:?}");
            assert!(hits[0].message.contains(name), "{src}: {}", hits[0].message);
            assert!(
                hits[0].message.contains("dart_identifier"),
                "{src}: {}",
                hits[0].message
            );
        }

        // And the way past each one. A rename on either side resolves it.
        for src in [
            "#[bridge(data)] pub struct Point { pub x: i64 } \
             #[bridge] impl Point { #[bridge(sync)] pub fn norm(&self) -> i64 { 0 } } \
             #[bridge(sync, dart_identifier = \"pointNormOf\")] pub fn point_norm(p: Point) -> i64 { 0 }",
            "#[bridge(confined)] pub struct Doc { t: i64 } \
             #[bridge] impl Doc { \
               #[bridge(sync)] pub fn word_count(&self) -> i64 { 0 } \
               #[allow(non_snake_case)] #[bridge(sync, dart_identifier = \"wordCountRaw\")] \
               pub fn wordCount(&self) -> i64 { 1 } }",
            "#[bridge(data)] pub struct P { pub word_count: i64, \
               #[allow(non_snake_case)] #[bridge(dart_identifier = \"wordCountRaw\")] pub wordCount: i64 }",
            "#[bridge(data)] pub enum Shape { AB { n: i64 } } \
             #[bridge(data)] pub enum ShapeA { #[bridge(dart_identifier = \"ShapeAOnlyB\")] B { n: i64 } } \
             #[bridge(sync)] pub fn f(a: Shape, b: ShapeA) -> i64 { 0 }",
            "#[bridge(data)] pub enum Color { Red, Green } \
             #[bridge] impl Color { #[bridge(sync, dart_identifier = \"isRed\")] pub fn red(&self) -> bool { true } }",
        ] {
            assert!(check_src(src).is_ok(), "{src}: {:?}", check_src(src).err());
        }
    }

    /// FR0065 — the annotation is a free string, so it is the one place a
    /// non-identifier could reach the emitter and fail inside generated code,
    /// which is what it exists to prevent.
    #[test]
    fn a_dart_identifier_must_be_a_dart_identifier() {
        // `$` is legal in a Dart identifier and is refused here anyway: the
        // generated stems own it (an instantiation's members land on
        // `extension Page$Item on Page<Item>`), so a written `$` could be
        // spelled like one and collide inside generated code.
        for bad in ["", "2fast", "has space", "class", "a-b", "Page$Item", "a$b"] {
            let src = format!(
                "#[bridge(sync, dart_identifier = \"{bad}\")] pub fn f() -> i64 {{ 0 }}"
            );
            let ds = check_src(&src).unwrap_err();
            assert!(
                ds.iter().any(|d| d.code == "FR0065"),
                "{bad} accepted: {ds:?}"
            );
        }
        for good in ["wordCount", "_private", "a1", "class_"] {
            let src = format!(
                "#[bridge(sync, dart_identifier = \"{good}\")] pub fn f() -> i64 {{ 0 }}"
            );
            assert!(check_src(&src).is_ok(), "{good} refused");
        }
    }

    /// The two features compose, and this is where they would not: a data type
    /// that owns handles is return-only, and a **receiver** is decoded out of
    /// the request. Without the rule the generated code calls a Rust decoder
    /// and a Dart encoder that a return-only type does not have.
    #[test]
    fn a_receiver_on_a_handle_owning_data_type_is_refused() {
        let decls = r#"
            #[bridge(frozen)] pub struct Doc { t: i64 }
            #[bridge(data)] pub struct Holder { pub doc: Doc, pub n: i64 }
        "#;
        let ds = check_src(&format!(
            "{decls} #[bridge] impl Holder {{ #[bridge(sync)] pub fn size(&self) -> i64 {{ 0 }} }}"
        ))
        .unwrap_err();
        let d = ds.iter().find(|d| d.code == "FR0004").unwrap();
        assert!(d.message.contains("a receiver is decoded"), "{}", d.message);
        // A receiverless member decodes nothing, so it is fine.
        assert!(check_src(&format!(
            "{decls} #[bridge] impl Holder {{ #[bridge(sync)] pub fn zero() -> i64 {{ 0 }} }}"
        ))
        .is_ok());
        // And a data type that owns no handle takes a receiver as before.
        assert!(check_src(
            "#[bridge(data)] pub struct P { pub n: i64 } \
             #[bridge] impl P { #[bridge(sync)] pub fn size(&self) -> i64 { 0 } }",
        )
        .is_ok());
    }

    /// Every rule that reads `walk_type_graph` has to see through a **new**
    /// container variant, and the two that fail *silently* if it does not are
    /// the ones pinned here.
    ///
    /// `reachable_handles` feeds FR0020 (a Dart method with a reply frame
    /// cannot be invoked from a sync member: the application thread would wait
    /// on the Dart event loop it is standing on) and the `requires_native`
    /// derivation (a native-only opaque anywhere in a signature keeps that
    /// member out of the web surface). A blind walk under `[T; N]` produces no
    /// diagnostic at all, and both halves still compile — so the failure is a
    /// deadlock, or a member emitted into a web surface whose Rust glue names
    /// an item the wasm build does not have. The other walks' blindness is
    /// loud (an `unreachable!` or an FR0003), which is why they are not here.
    #[test]
    fn a_fixed_array_is_not_a_blind_spot_for_the_handle_walks() {
        let mirror = "#[bridge(data)] pub struct W { pub ask: DartFunction<i64, bool> }";
        for shape in [
            "[W; 2]",
            "Vec<[W; 2]>",
            "Option<[W; 2]>",
            "Box<W>",
            "Option<Box<W>>",
            "Vec<Box<W>>",
        ] {
            let ds = codes(check_src(&format!(
                "{mirror} #[bridge(sync)] pub fn f(a: {shape}) {{ let _ = a; }}"
            )));
            assert!(ds.contains(&"FR0020"), "{shape}: {ds:?}");
        }
        // `requires_native` propagates out of an array the same way: the
        // opaque is reachable only through it.
        let iface = check_src(
            "#[bridge(confined, native_only)] pub struct Nat { x: i32 } \
             #[bridge(sync)] pub fn f() -> [Nat; 2] { todo!() } \
             #[bridge(sync)] pub fn h() -> Box<Nat> { todo!() } \
             #[bridge(sync)] pub fn g() -> [i64; 2] { [0, 0] }",
        )
        .unwrap();
        let f = |n: &str| iface.functions.iter().find(|f| f.name == n).unwrap().requires_native;
        assert!(f("f"), "a native-only opaque inside an array is still in scope");
        assert!(f("h"), "…and inside a Box");
        assert!(!f("g"), "an array of plain data is portable");
    }

    /// The return path's own rule: a handle may sit only where an owned encode
    /// exists for it — and every container that is consumed on the way out has
    /// one. A tuple is destructured and each element owned; a map and a set are
    /// consumed by value, so each handle inside is minted exactly once.
    #[test]
    fn a_returned_handle_needs_a_position_that_can_own_it() {
        assert!(check_src(
            r#"
            #[bridge(frozen)] pub struct Snapshot { x: i32 }
            #[bridge(confined)] pub struct Cache { x: i32 }
            #[bridge] pub fn pair() -> (Snapshot, Cache) { todo!() }
            #[bridge] pub fn beside() -> (String, Snapshot) { todo!() }
            #[bridge] pub fn nested() -> Vec<(Snapshot, i64)> { todo!() }
            "#,
        )
        .is_ok());
        // A map value, a map key, both at once, a set element, and each of
        // those reached through a declared type and through another container.
        assert!(check_src(
            r#"
            #[bridge(frozen)] pub struct Snapshot { x: i32 }
            #[bridge(data)] pub struct Keyed { pub by_name: std::collections::HashMap<String, Snapshot> }
            #[bridge] pub fn by_name() -> std::collections::HashMap<String, Snapshot> { todo!() }
            #[bridge] pub fn keyed() -> std::collections::HashMap<Snapshot, i64> { todo!() }
            #[bridge] pub fn both() -> std::collections::BTreeMap<Snapshot, Snapshot> { todo!() }
            #[bridge] pub fn set() -> std::collections::HashSet<Snapshot> { todo!() }
            #[bridge] pub fn btree_set() -> std::collections::BTreeSet<Snapshot> { todo!() }
            #[bridge] pub fn deep() -> Vec<std::collections::HashMap<String, Option<Snapshot>>> { todo!() }
            #[bridge] pub fn through_decl() -> Keyed { todo!() }
            "#,
        )
        .is_ok());
        // Inbound, the same containers are a **consume**: the map stages flat
        // and is rebuilt once every handle has an owner.
        assert!(check_src(
            r#"
                #[bridge(frozen)] pub struct Snapshot { x: i32 }
                #[bridge] pub fn take(m: std::collections::HashMap<String, Snapshot>) {}
                "#,
        )
        .is_ok());
    }

    /// A borrowed return has no owned encode in it, so it cannot carry a
    /// handle — and that is not a gap to fill: a returned handle is one Dart
    /// owns and disposes, and a `&Doc` is a view of something Rust still owns.
    #[test]
    fn a_borrowed_return_cannot_carry_a_handle() {
        let ds = check_src(
            r#"
            #[bridge(frozen)] pub struct Snapshot { x: i32 }
            #[bridge(confined)] pub struct Holder { s: Snapshot }
            #[bridge] impl Holder { #[bridge(sync)] pub fn snap(&self) -> &Snapshot { todo!() } }
            "#,
        )
        .unwrap_err();
        let d = ds.iter().find(|d| d.code == "FR0004").unwrap();
        assert!(d.message.contains("borrowed return"), "{}", d.message);
        // A borrowed *value* is fine, at any shape the codec already handles.
        assert!(check_src(
            r#"
            #[bridge(data)] pub struct P { pub x: i64 }
            #[bridge(frozen)] pub struct S { p: P, w: Vec<String> }
            #[bridge] impl S {
                #[bridge(sync)] pub fn p(&self) -> &P { todo!() }
                #[bridge(sync)] pub fn w(&self) -> &Vec<String> { todo!() }
                #[bridge(sync)] pub fn n(&self) -> &i64 { todo!() }
            }
            "#,
        )
        .is_ok());
    }

    /// A typed error whose payload reaches a handle is refused by FR0035 —
    /// the error position is the one return path that must stay handle-free,
    /// because minting there hands a `catch` block something to dispose.
    #[test]
    fn a_typed_error_carrying_a_handle_is_refused() {
        let ds = check_src(
            r#"
            #[bridge(frozen)] pub struct Snapshot { x: i32 }
            #[bridge(data)] pub struct Failure { pub snap: Snapshot }
            #[bridge] pub fn f() -> Result<(), Failure> { Ok(()) }
            "#,
        )
        .unwrap_err();
        assert_eq!(ds.iter().map(|d| d.code).collect::<Vec<_>>(), vec!["FR0035"]);
        assert!(ds[0].message.contains("`catch`"), "{}", ds[0].message);
    }

    #[test]
    fn option_opaque_return_allowed() {
        let iface = check_src(
            r#"
            #[bridge(frozen)] pub struct Snapshot { x: i32 }
            #[bridge] pub fn find() -> Option<Snapshot> { None }
            "#,
        )
        .unwrap();
        assert!(matches!(
            iface.functions[0].ret,
            Some(Type::Option(ref t)) if matches!(t.as_ref(), Type::Opaque(_))
        ));
    }

    #[test]
    fn on_contention_on_async_rejected() {
        let ds = check_src(
            r#"
            #[bridge(locked)] pub struct Cache { x: i32 }
            #[bridge] impl Cache {
                #[bridge(on_contention = "error")]
                pub fn get(&self) -> i32 { 0 }
            }
            "#,
        )
        .err()
        .unwrap();
        assert_eq!(ds[0].code, "FR0009");
    }

    #[test]
    fn actor_sync_method_rejected() {
        let ds = check_src(
            r#"
            #[bridge(actor)] pub struct A { x: i64 }
            #[bridge] impl A {
                pub fn new() -> Self { todo!() }
                #[bridge(sync)] pub fn peek(&self) -> i64 { self.x }
            }
            "#,
        )
        .unwrap_err();
        assert_eq!(ds[0].code, "FR0014");
        assert!(ds[0].message.contains("async-only"), "{}", ds[0].message);
    }

    #[test]
    fn actor_opaque_param_rejected() {
        let ds = check_src(
            r#"
            #[bridge(confined)] pub struct Doc { t: String }
            #[bridge(actor)] pub struct A { x: i64 }
            #[bridge] impl A {
                pub fn new() -> Self { todo!() }
                pub fn read(&self, d: &Doc) -> i64 { 0 }
            }
            "#,
        )
        .unwrap_err();
        assert!(ds.iter().any(|d| d.code == "FR0015"), "{ds:?}");
    }

    #[test]
    fn actor_handle_cannot_cross_to_other_functions() {
        let ds = check_src(
            r#"
            #[bridge(actor)] pub struct A { x: i64 }
            #[bridge] impl A { pub fn new() -> Self { todo!() } }
            #[bridge(sync)] pub fn poke(a: &A) -> i64 { 0 }
            "#,
        )
        .unwrap_err();
        assert!(ds.iter().any(|d| d.code == "FR0015"), "{ds:?}");
    }

    #[test]
    fn sink_item_types_resolve_and_streams_pass_both_capability_passes() {
        let iface = check_src(
            r#"
            #[bridge(data)] pub enum Patch { Clear }
            #[bridge] pub fn watch(sink: StreamSink<Patch>) {}
            "#,
        )
        .unwrap();
        let spec = iface.functions[0].params[0]
            .ty
            .as_dart_object()
            .expect("a handle");
        assert_eq!(spec.item, Some(Type::Enum("Patch".into())));
        assert!(!iface.functions[0].requires_native, "streams are web-legal");
    }

    #[test]
    fn handles_compose_where_the_old_endpoint_rules_forbade_it() {
        // The three shapes FR0016/FR0017 used to reject, and the one the
        // parser used to reject. All legal now that a handle is data: this
        // is the behaviour change the whole design exists to deliver.
        for src in [
            "#[bridge] pub fn f(a: StreamSink<i64>, b: StreamSink<i64>) {}",
            "#[bridge] pub fn f(s: StreamSink<i64>) -> i64 { 0 }",
            "#[bridge] pub fn f(s: Vec<StreamSink<i64>>) {}",
            "#[bridge(data)] pub struct Pair { a: StreamSink<i64>, b: StreamSink<i64> }\n\
             #[bridge] pub fn f(p: Pair) {}",
        ] {
            check_src(src).unwrap_or_else(|ds| panic!("{src}: {ds:?}"));
        }
        // And a struct of two sinks really is two sinks — the equivalence
        // that failed before.
        let iface = check_src(
            "#[bridge(data)] pub struct Pair { a: StreamSink<i64>, b: StreamSink<i64> }\n\
             #[bridge] pub fn f(p: Pair) {}",
        )
        .unwrap();
        let handles = reachable_handles(&iface, &iface.functions[0].params[0].ty);
        assert_eq!(handles.len(), 2, "both reached through the struct");
    }

    #[test]
    fn handles_are_argument_only() {
        // FR0031: Rust cannot mint a Dart object, so a handle never travels
        // Rust → Dart. Each of these would need it to.
        for src in [
            "#[bridge] pub fn f() -> StreamSink<i64> { todo!() }",
            "#[bridge] pub fn f(s: StreamSink<StreamSink<i64>>) {}",
            "#[bridge] pub fn f(t: DartFunction<i64, DartCallback<i64>>) {}",
            "#[bridge(data)] pub struct H { s: StreamSink<i64> }\n\
             #[bridge] pub fn f() -> H { todo!() }",
        ] {
            let ds = check_src(src).unwrap_err();
            assert!(
                ds.iter().any(|d| d.code == "FR0031"),
                "{src}: {ds:?}"
            );
        }
    }

    /// A channel item is a Rust → Dart position, so it carries handles: the
    /// producer mints, the router's item handler builds the wrapper that
    /// disposes. Wherever the item type puts one — the root, a container, a
    /// declared type's field — because the item is walked as a type graph and
    /// not matched against a shape.
    #[test]
    fn opaque_stream_items_and_callback_arguments_are_accepted() {
        for item in [
            "Snap",
            "Vec<Snap>",
            "Option<Snap>",
            "std::collections::HashMap<String, Snap>",
            "Vec<Option<Snap>>",
            "Holder",
        ] {
            for chan in ["StreamSink", "DartCallback"] {
                let src = format!(
                    r#"
                    #[bridge(frozen)] pub struct Snap {{ x: i32 }}
                    #[bridge(data)] pub struct Holder {{ pub s: Snap }}
                    #[bridge] pub fn f(s: {chan}<{item}>) {{}}
                    "#
                );
                assert!(
                    check_src(&src).is_ok(),
                    "{chan}<{item}> must be accepted, got {:?}",
                    check_src(&src).unwrap_err()
                );
            }
        }
    }

    /// The reply half is the other direction and stays refused: a handle in a
    /// Dart method's result would be given up by the Dart object holding it,
    /// which is the `Consumed<…>` seam (FR0004's machinery) that a closure's
    /// reply frame does not have.
    #[test]
    fn an_opaque_in_a_dart_methods_result_is_refused() {
        for ret in ["Snap", "Vec<Snap>", "Result<Snap, Refusal>"] {
            let ds = check_src(&format!(
                r#"
                #[bridge(frozen)] pub struct Snap {{ x: i32 }}
                #[bridge(data)] pub enum Refusal {{ No }}
                #[bridge] pub async fn f(g: DartFunction<i64, {ret}>) {{}}
                "#
            ))
            .unwrap_err();
            assert!(
                ds.iter().any(|d| d.code == "FR0018"),
                "a handle in `{ret}` must be refused, got {:?}",
                ds.iter().map(|d| d.code).collect::<Vec<_>>()
            );
        }
        // The declared error is the same direction: Dart throws it, Rust
        // receives it by value.
        let ds = check_src(
            r#"
            #[bridge(frozen)] pub struct Snap { x: i32 }
            #[bridge(data)] pub struct Refusal { pub s: Snap }
            #[bridge] pub async fn f(g: DartFunction<i64, Result<i64, Refusal>>) {}
            "#,
        )
        .unwrap_err();
        assert!(ds.iter().any(|d| d.code == "FR0018"), "{ds:?}");
    }

    /// **The rule that keeps the lifted case sound on web**, and it is not
    /// FR0018's. A handle minted inside an actor is an index into that actor's
    /// own wasm instance; the Dart wrapper's `dispose()` calls the *main*
    /// instance's `frustrate_drop_<T>`, a different registry in a different
    /// linear memory. That is a wrong-object free, which no reclaim hook
    /// compensates for.
    ///
    /// FR0015 refuses it, in both directions, because its parameter walk is
    /// `Type::walk`, which descends a `DartObject`'s item. Pinned here rather
    /// than left implicit: FR0018 used to fire beside it on every one of these,
    /// so nothing proved FR0015 alone was enough.
    #[test]
    fn a_handle_cannot_cross_an_actor_boundary_through_a_channel() {
        // An actor member opening a channel of some other type's handles.
        let ds = check_src(
            r#"
            #[bridge(frozen)] pub struct Snap { x: i32 }
            #[bridge(actor)] pub struct Mine { n: i64 }
            #[bridge] impl Mine {
                pub fn new() -> Mine { todo!() }
                pub fn watch(&self, s: StreamSink<Snap>) {}
            }
            "#,
        )
        .unwrap_err();
        assert!(ds.iter().any(|d| d.code == "FR0015"), "{ds:?}");
        assert!(!ds.iter().any(|d| d.code == "FR0018"), "{ds:?}");

        // And an *actor instance* as a channel item on an ordinary member.
        let ds = check_src(
            r#"
            #[bridge(actor)] pub struct Mine { n: i64 }
            #[bridge] impl Mine {
                pub fn new() -> Mine { todo!() }
                pub fn n(&self) -> i64 { self.n }
            }
            #[bridge] pub fn f(s: StreamSink<Mine>) {}
            "#,
        )
        .unwrap_err();
        assert!(ds.iter().any(|d| d.code == "FR0015"), "{ds:?}");
        assert!(!ds.iter().any(|d| d.code == "FR0018"), "{ds:?}");
    }

    /// **One struct field deep, and the rule still has to hold.** FR0015 reads
    /// a parameter structurally, which was enough while FR0018 refused every
    /// handle in an item and FR0004's declaration walk covered the rest. Now
    /// that items carry handles, this is the only thing standing between an
    /// actor and a wrong-instance free, so the item is walked through the
    /// declaration graph and that is pinned here.
    #[test]
    fn an_actor_channel_item_is_refused_through_a_struct_field() {
        // The actor would mint `Snap` in its own instance; the main instance's
        // `frustrate_drop_Snap` would free an unrelated address.
        for item in ["Holder", "Vec<Holder>", "Option<Holder>"] {
            let ds = check_src(&format!(
                r#"
                #[bridge(frozen)] pub struct Snap {{ x: i32 }}
                #[bridge] impl Snap {{ #[bridge(sync)] pub fn x(&self) -> i32 {{ 0 }} }}
                #[bridge(sync)] pub fn make() -> Snap {{ todo!() }}
                #[bridge(data)] pub struct Holder {{ pub s: Snap }}
                #[bridge(actor)] pub struct Mine {{ n: i64 }}
                #[bridge] impl Mine {{
                    pub fn new() -> Mine {{ todo!() }}
                    pub fn watch(&self, s: StreamSink<{item}>) {{}}
                }}
                "#
            ))
            .unwrap_err();
            assert!(
                ds.iter().any(|d| d.code == "FR0015"),
                "a handle behind `{item}` must be refused, got {:?}",
                ds.iter().map(|d| d.code).collect::<Vec<_>>()
            );
        }

        // The mirror: an actor *instance* behind a field, on any member.
        let ds = check_src(
            r#"
            #[bridge(actor)] pub struct Mine { n: i64 }
            #[bridge] impl Mine {
                pub fn new() -> Mine { todo!() }
                pub fn n(&self) -> i64 { self.n }
            }
            #[bridge(data)] pub struct AH { pub a: Mine }
            #[bridge] pub fn f(s: StreamSink<AH>) {}
            "#,
        )
        .unwrap_err();
        assert!(ds.iter().any(|d| d.code == "FR0015"), "{ds:?}");
    }

    /// A **resident** cannot cross a channel at all, on any member. It is the
    /// birthplace rule at a third position: a sink is `Send + Sync` and
    /// outlives the call, so which thread mints an item is not something the
    /// declaration says — and freeing a resident from another thread strands it
    /// rather than freeing it.
    #[test]
    fn a_resident_cannot_cross_a_channel() {
        for item in ["Scene", "Vec<Scene>", "Held", "Option<Held>"] {
            for chan in ["StreamSink", "DartCallback"] {
                let src = format!(
                    r#"
                    #[bridge(resident)] pub struct Scene {{ n: std::rc::Rc<i64> }}
                    #[bridge] impl Scene {{ #[bridge(sync)] pub fn new() -> Self {{ todo!() }} }}
                    #[bridge(data)] pub struct Held {{ pub s: Scene }}
                    #[bridge(sync)] pub fn f(c: {chan}<{item}>) {{}}
                    "#
                );
                let ds = check_src(&src).unwrap_err();
                assert!(
                    ds.iter().any(|d| d.code == "FR0079"),
                    "{chan}<{item}>: {:?}",
                    ds.iter().map(|d| d.code).collect::<Vec<_>>()
                );
            }
        }
    }

    #[test]
    fn externs_resolve_and_go_anywhere_data_goes() {
        let iface = check_src(
            r#"
            #[bridge(bytes(dart = "Plan", import = "package:app/plan.pb.dart"))]
            pub struct PlanMsg { pub title: String }
            #[bridge(data)] pub struct Meeting { pub plan: PlanMsg }
            #[bridge(sync)] pub fn bump(p: PlanMsg) -> PlanMsg { todo!() }
            #[bridge] pub fn watch_plans(sink: StreamSink<PlanMsg>) {}
            #[bridge] pub fn on_plan(cb: DartCallback<Vec<PlanMsg>>) {}
            "#,
        )
        .unwrap();
        // Resolved everywhere a value type is legal: fields, params,
        // returns, stream items, callback args, collections.
        assert_eq!(
            iface.structs[0].fields[0].ty,
            Type::Extern("PlanMsg".into())
        );
        assert_eq!(iface.functions[0].ret, Some(Type::Extern("PlanMsg".into())));
        assert_eq!(
            iface.functions[1].params[0]
                .ty
                .as_dart_object()
                .and_then(|s| s.item.clone()),
            Some(Type::Extern("PlanMsg".into()))
        );
        assert!(!iface.functions[0].requires_native, "externs are web-legal");
    }

    #[test]
    fn void_callbacks_are_portable_returning_callbacks_are_native_only() {
        let iface = check_src(
            r#"
            #[bridge(data)] pub enum Patch { Clear }
            #[bridge(sync)] pub fn on_change(cb: DartCallback<Patch>) {}
            #[bridge] pub fn transform(x: i64, f: DartFunction<i64, i64>) -> i64 { 0 }
            "#,
        )
        .unwrap();
        let void_cb = &iface.functions[0];
        assert!(!void_cb.requires_native, "fire-and-forget is web-legal");
        assert_eq!(
            void_cb.params[0].ty.as_dart_object().and_then(|s| s.item.clone()),
            Some(Type::Enum("Patch".into()))
        );
        let returning = &iface.functions[1];
        assert!(
            returning.requires_native,
            "a returning callback blocks the invoking worker — native-only, \
             through the same machinery as on_contention = \"block\""
        );
    }

    /// A **fallible** closure is a returning one everywhere it matters, and
    /// `Result<(), E>` is the case that would silently misclassify if any rule
    /// asked `ret.is_some()` instead of `is_returning()`: it has no value but
    /// still round-trips, so it parks the invoking worker just the same.
    #[test]
    fn a_fallible_closure_is_classified_as_returning_even_with_no_value() {
        let iface = check_src(
            r#"
            #[bridge(data)] pub enum Refusal { No }
            #[bridge] pub async fn ask(f: DartFunction<i64, Result<i64, Refusal>>) -> i64 { 0 }
            #[bridge] pub async fn tell(f: DartFunction<i64, Result<(), Refusal>>) {}
            #[bridge] pub fn blocking(f: DartFunction<i64, Result<i64, Refusal>>) -> i64 { 0 }
            #[bridge] pub fn blocking_void(f: DartFunction<i64, Result<(), Refusal>>) {}
            "#,
        )
        .unwrap();
        // An `async fn` awaits `call_async`, so it stays portable — fallible or
        // not, with a value or not.
        assert!(!iface.functions[0].requires_native);
        assert!(!iface.functions[1].requires_native);
        // A non-`async fn` must park a worker, so it is native-only — and the
        // NO-VALUE case must be classified with it, not left portable emitting
        // blocking glue for web.
        assert!(iface.functions[2].requires_native);
        assert!(
            iface.functions[3].requires_native,
            "`Result<(), E>` has a reply frame, so it blocks exactly as a \
             value-returning closure does"
        );
        // The error type resolved on the way through, so the emitters never
        // meet an unresolved `Named`.
        let spec = iface.functions[1].params[0].ty.as_dart_object().unwrap();
        assert_eq!(spec.err, Some(Type::Enum("Refusal".into())));
        assert_eq!(spec.ret, None);
    }

    /// FR0020 covers the fallible shape too, including the no-value one.
    #[test]
    fn a_fallible_closure_on_a_sync_member_is_still_a_deadlock_error() {
        for src in [
            "#[bridge(data)] pub enum R { No } \
             #[bridge(sync)] pub fn f(t: DartFunction<i64, Result<i64, R>>) {}",
            "#[bridge(data)] pub enum R { No } \
             #[bridge(sync)] pub fn f(t: DartFunction<i64, Result<(), R>>) {}",
        ] {
            let ds = check_src(src).unwrap_err();
            assert!(ds.iter().any(|d| d.code == "FR0020"), "{src}: {ds:?}");
        }
    }

    /// FR0035, the mirror direction: the declared error crosses by value, so an
    /// opaque cannot be one — the same fact, and the same code, as on a
    /// member's own `Result`.
    #[test]
    fn a_dart_functions_declared_error_must_be_a_value_type() {
        let ds = check_src(
            r#"
            #[bridge(locked)] pub struct Conn { x: i64 }
            #[bridge] pub async fn ask(f: DartFunction<i64, Result<i64, Conn>>) -> i64 { 0 }
            "#,
        )
        .unwrap_err();
        assert!(ds.iter().any(|d| d.code == "FR0035"), "{ds:?}");
        assert!(
            ds.iter().any(|d| d.message.contains("DartFunction")),
            "the message must name where the error was declared: {ds:?}"
        );
    }

    /// FR0036 covers the second generated name a closure error mints. Without
    /// this, `RefusalFallible` could silently shadow a user's own type and the
    /// failure would be inside generated code.
    #[test]
    fn the_fallible_alias_is_covered_by_the_name_collision_check() {
        let ds = check_src(
            r#"
            #[bridge(data)] pub enum Refusal { No }
            #[bridge(data)] pub struct RefusalFallible { x: i64 }
            #[bridge] pub async fn ask(f: DartFunction<i64, Result<i64, Refusal>>) -> i64 { 0 }
            #[bridge(sync)] pub fn mk() -> RefusalFallible { todo!() }
            "#,
        )
        .unwrap_err();
        assert!(ds.iter().any(|d| d.code == "FR0036"), "{ds:?}");
        assert!(
            ds.iter().any(|d| d.message.contains("RefusalFallible")),
            "{ds:?}"
        );
    }

    #[test]
    fn returning_callback_on_sync_member_is_a_static_deadlock_error() {
        let ds = check_src(
            "#[bridge(sync)] pub fn f(t: DartFunction<i64, i64>) {}",
        )
        .unwrap_err();
        assert_eq!(ds[0].code, "FR0020");
        assert!(ds[0].message.contains("deadlock"), "{}", ds[0].message);
        assert!(ds[0].message.contains("DartCallback"), "{}", ds[0].message);
    }

    #[test]
    fn async_fn_returning_callback_is_portable() {
        // The awaitable path: a Rust `async fn` taking a DartFunction awaits
        // `call_async` on the cooperative executor, so it runs on every
        // platform — NOT native-only, and it must pass the web facts (the old
        // FR0019 backstop, which would have rejected it, is gone).
        let iface = check_src(
            "#[bridge] pub async fn transform(x: i64, f: DartFunction<i64, i64>) -> i64 { 0 }",
        )
        .unwrap();
        let f = &iface.functions[0];
        assert!(f.rust_async);
        assert!(
            !f.requires_native,
            "an `async fn` DartFunction member awaits call_async — portable"
        );
        // The web pass sees it (not filtered by requires_native) and accepts it.
        let web = parse_source(
            "#[bridge] pub async fn transform(x: i64, f: DartFunction<i64, i64>) -> i64 { 0 }",
            "crate::api",
        )
        .unwrap();
        assert!(check_pass(web, &Capabilities::web()).is_ok());
    }

    #[test]
    fn non_async_fn_returning_callback_stays_native_only() {
        // A plain pool member (or actor method) cannot `.await`, so it must use
        // the blocking `call` — which parks a worker and cannot hold on
        // single-threaded web. Still native-only, through the same machinery
        // as on_contention = "block".
        let iface = check_src(
            "#[bridge] pub fn transform_sum(x: i64, f: DartFunction<i64, i64>) -> i64 { 0 }",
        )
        .unwrap();
        let f = &iface.functions[0];
        assert!(!f.rust_async);
        assert!(f.requires_native, "a non-`async fn` returning callback is native-only");
    }


    #[test]
    fn actor_gets_a_synthetic_drop_appended_last() {
        let iface = check_src(
            r#"
            #[bridge(actor)] pub struct A { x: i64 }
            #[bridge] impl A {
                pub fn new() -> Self { todo!() }
                pub fn get(&self) -> i64 { self.x }
            }
            "#,
        )
        .unwrap();
        let drop = iface.functions.iter().find(|f| f.is_actor_drop).unwrap();
        assert_eq!(drop.parent.as_deref(), Some("A"));
        // Appended after the declared members, which is what "synthetic" means
        // here: it is not in the source, so it has no declared position of its
        // own. Its *id* says nothing about position — ids are derived from what
        // a member is (`hash::member_ids`) — so the claim is about the list.
        assert!(iface.functions.last().unwrap().is_actor_drop);
        assert_eq!(iface.functions.len(), 3);
    }

    /// FR0038/FR0039: `Deferred` is the actor opt-out of serialized
    /// completion, and nothing else — everywhere else an `async fn` already
    /// releases everything, and a constructor has no instance to release.
    #[test]
    fn deferred_is_actor_methods_only() {
        // Free fn, non-actor method, async fn: FR0038.
        assert_eq!(
            codes(check_src("#[bridge] pub fn f() -> Deferred<i64> { unimplemented!() }")),
            vec!["FR0038"]
        );
        assert_eq!(
            codes(check_src(
                "#[bridge(frozen)] pub struct F { x: i64 }\n\
                 #[bridge] impl F { pub fn slow(&self) -> Deferred<i64> { unimplemented!() } }"
            )),
            vec!["FR0038"]
        );
        assert_eq!(
            codes(check_src("#[bridge] pub async fn g() -> Deferred<i64> { unimplemented!() }")),
            vec!["FR0038"]
        );
        // An actor constructor: FR0039.
        let e = check_src(
            "#[bridge(actor)] pub struct A { x: i64 }\n\
             #[bridge] impl A { pub fn open() -> Deferred<Self> { unimplemented!() } }",
        )
        .unwrap_err();
        let msg = format!("{e:?}");
        assert!(msg.contains("FR0039"), "{msg}");
        assert!(msg.contains("no instance to release"), "{msg}");
        // The one supported shape checks clean.
        assert!(check_src(
            "#[bridge(actor)] pub struct B { x: i64 }\n\
             #[bridge] impl B {\n\
                 pub fn new() -> Self { B { x: 0 } }\n\
                 pub fn slow(&mut self) -> Deferred<Result<i64, String>> { unimplemented!() }\n\
             }"
        )
        .is_ok());
    }

    /// The charter line "the build error for an undeclared hazard names the
    /// opt-in and its contract": the error a user hits first — `async fn` on
    /// an actor method — must point at `Deferred`.
    #[test]
    fn the_actor_async_fn_rejection_names_the_deferred_opt_in() {
        let e = check_src(
            "#[bridge(actor)] pub struct A { x: i64 }\n\
             #[bridge] impl A {\n\
                 pub fn new() -> Self { A { x: 0 } }\n\
                 pub async fn slow(&self) -> i64 { 0 }\n\
             }",
        )
        .unwrap_err();
        let msg = format!("{e:?}");
        assert!(msg.contains("FR0029"), "{msg}");
        assert!(msg.contains("Deferred<T>"), "{msg}");
    }

    // ---- the declared Dart interface ------------------------------------

    /// FR0050: every field of a `dart_interface` is one method of the
    /// generated Dart interface, so it has to be something a method can be.
    #[test]
    fn a_dart_interface_field_must_be_a_closure_mirror() {
        for src in [
            // Data: an interface has no constructor to supply state through.
            "#[bridge(data, dart_interface)] pub struct W { pub on_change: DartCallback<i64>, pub n: i64 }",
            // A sink mirror is a Dart *object* with methods of its own.
            "#[bridge(data, dart_interface)] pub struct W { pub items: StreamSink<i64> }",
            "#[bridge(data, dart_interface)] pub struct W { pub items: frustrate::dart::core::Sink<i64> }",
            // A container or Option around a mirror has no method to be.
            "#[bridge(data, dart_interface)] pub struct W { pub cb: Option<DartCallback<i64>> }",
            "#[bridge(data, dart_interface)] pub struct W { pub cb: Vec<DartCallback<i64>> }",
        ] {
            let ds = codes(check_src(src));
            assert!(ds.contains(&"FR0050"), "{src}: {ds:?}");
        }
        // The legal shapes: both closure mirrors, void and returning, with and
        // without an argument.
        assert!(check_src(
            "#[bridge(data, dart_interface)] pub struct W { \
                 pub ping: DartCallback<()>, \
                 pub on_change: DartCallback<i64>, \
                 pub rename: DartFunction<(i64, String), bool> } \
             #[bridge] pub async fn f(a: W) { a.ping.call(()); }"
        )
        .is_ok());
    }

    /// FR0051: an interface with no methods is a contract that cannot bite —
    /// nothing to implement, nothing to call, no handle on the wire.
    #[test]
    fn an_empty_dart_interface_is_rejected() {
        let ds = codes(check_src("#[bridge(data, dart_interface)] pub struct W {}"));
        assert!(ds.contains(&"FR0051"), "{ds:?}");
        // An empty *ordinary* struct is still fine; the rule is about the form.
        assert!(check_src("#[bridge(data)] pub struct W {}").is_ok());
    }

    /// FR0052 asks about the **Dart** name, which is where the collision is:
    /// `to_string` becomes `toString`, and a field already spelled `toString`
    /// maps to itself. Checking the Rust spelling would miss the second.
    #[test]
    fn a_dart_interface_method_may_not_collide_with_object() {
        for field in ["to_string", "toString", "hash_code", "runtime_type", "no_such_method"] {
            let src = format!(
                "#[bridge(data, dart_interface)] pub struct W {{ pub {field}: DartCallback<i64> }}"
            );
            let ds = codes(check_src(&src));
            assert!(ds.contains(&"FR0052"), "{field}: {ds:?}");
        }
        // A Dart *keyword* is not a collision: `dart_name` escapes it to
        // `class_`, which no member of Object is called.
        assert!(check_src(
            "#[bridge(data, dart_interface)] pub struct W { pub class: DartCallback<i64> } \
             #[bridge] pub fn f(a: W) { a.class.call(1); }"
        )
        .is_ok());
    }

    /// The classification is per-**interface**, not per-method, and that is a
    /// decision rather than an oversight: the checker cannot see which methods
    /// a body calls, so one returning method makes every member taking the
    /// interface one that may have to wait for Dart.
    ///
    /// It falls out with no new code because `walk_type_graph` already descends
    /// a declared struct's fields — which is exactly why this is pinned. A
    /// shallow walk would let such a member into the web subset, whose glue
    /// would then park a pool worker against the browser event loop.
    #[test]
    fn one_returning_method_classifies_the_whole_interface() {
        let iface = "#[bridge(data, dart_interface)] pub struct W { \
                         pub log: DartCallback<String>, \
                         pub ask: DartFunction<i64, bool> }";
        // FR0020: a sync member cannot invoke it at all.
        let ds = codes(check_src(&format!(
            "{iface} #[bridge(sync)] pub fn f(a: W) {{ a.log.call(String::new()); }}"
        )));
        assert!(ds.contains(&"FR0020"), "{ds:?}");
        // Nested one struct deeper, and behind a Vec: same answer.
        let ds = codes(check_src(&format!(
            "{iface} #[bridge(data)] pub struct Run {{ pub ws: Vec<W> }} \
             #[bridge(sync)] pub fn f(r: Run) {{ let _ = r; }}"
        )));
        assert!(ds.contains(&"FR0020"), "{ds:?}");
        // A non-`async fn` async member is legal but native-only: invoking the
        // returning method parks a pool worker.
        let ok = check_src(&format!("{iface} #[bridge] pub fn f(a: W) {{ let _ = a; }}")).unwrap();
        assert!(ok.functions[0].requires_native, "{:?}", ok.functions[0]);
        // An `async fn` awaits the reply instead, so it stays portable.
        let ok =
            check_src(&format!("{iface} #[bridge] pub async fn f(a: W) {{ let _ = a; }}")).unwrap();
        assert!(!ok.functions[0].requires_native, "{:?}", ok.functions[0]);
        // FR0049: and `no_block` contradicts the wait.
        let ds = codes(check_src(&format!(
            "{iface} #[bridge(no_block)] pub fn f(a: W) {{ let _ = a; }}"
        )));
        assert!(ds.contains(&"FR0049"), "{ds:?}");
        // A void-only interface is none of the above.
        let ok = check_src(
            "#[bridge(data, dart_interface)] pub struct V { pub log: DartCallback<String> } \
             #[bridge(sync)] pub fn f(a: V) { a.log.call(String::new()); }",
        )
        .unwrap();
        assert!(!ok.functions[0].requires_native);
    }

    /// FR0031 is inherited whole: an interface is handle-bearing, so it is
    /// argument-only like any other handle-bearing type.
    #[test]
    fn a_dart_interface_cannot_be_returned() {
        let ds = codes(check_src(
            "#[bridge(data, dart_interface)] pub struct W { pub log: DartCallback<String> } \
             #[bridge] pub fn f() -> W { unimplemented!() }",
        ));
        assert!(ds.contains(&"FR0031"), "{ds:?}");
    }

    // ------------------------------------------ two representations, one type --

    /// A `#[bridge(data, locked)]` declaration, with one member on each half.
    /// The value half is renamed, because both halves derive `Doc` and two
    /// Dart classes cannot share a name (FR0002).
    const DUAL: &str = r#"
        #[bridge(data(dart_identifier = "DocValue"), locked)]
        pub struct Doc { pub title: String }
    "#;

    /// Both halves are declared, and each member lands on the one its `impl`
    /// block named — which is what every later rule reads, and what the
    /// generated Rust and Dart split on.
    #[test]
    fn a_type_may_declare_two_representations() {
        let ok = check_src(&format!(
            "{DUAL}
             #[bridge] impl Data<Doc> {{ #[bridge(sync)] pub fn len(&self) -> i64 {{ 0 }} }}
             #[bridge] impl Locked<Doc> {{
                 #[bridge(sync)] pub fn new(t: String) -> Locked<Doc> {{ unimplemented!() }}
                 pub fn set(&mut self, t: String) {{}}
                 pub fn snapshot(&self) -> Data<Doc> {{ unimplemented!() }}
             }}"
        ))
        .unwrap();
        assert!(ok.is_dual("Doc"), "both halves declared");
        let half = |name: &str| {
            ok.functions
                .iter()
                .find(|f| f.name == name)
                .map(|f| ok.member_repr(f))
                .unwrap()
        };
        assert_eq!(half("len"), Some(Repr::Data));
        assert_eq!(half("new"), Some(Repr::Handle));
        assert_eq!(half("set"), Some(Repr::Handle));
        assert_eq!(half("snapshot"), Some(Repr::Handle));
        // Receiverless and returning its own parent's handle half: the factory
        // a handle class is for. The same shape on the value half is a static.
        let ctor = ok.functions.iter().find(|f| f.name == "new").unwrap();
        assert!(ctor.is_constructor);
        // A cross-half return resolves to the *other* declaration.
        let snap = ok.functions.iter().find(|f| f.name == "snapshot").unwrap();
        assert_eq!(snap.ret, Some(Type::Struct("Doc".into())));
    }

    /// The two halves are checked under their own rules and nothing else's.
    /// Each row is one signature that one half accepts and the other refuses —
    /// which is the only thing that can catch a rule reading the *name* where
    /// it should read the member's half, because such a rule stays silent.
    #[test]
    fn each_half_is_checked_under_its_own_rules() {
        let rows: &[(&str, &str, &str)] = &[
            // `&mut self` mutates a decoded copy on a value (FR0013); it is
            // the write lock on a locked handle.
            ("pub fn m(&mut self) {}", "FR0013", ""),
            // Sync access to a locked object needs a contention contract
            // (FR0007); the same option on a value member is meaningless
            // (FR0010).
            ("#[bridge(sync)] pub fn m(&self) -> i64 { 0 }", "", "FR0007"),
            (
                "#[bridge(sync, on_contention = \"error\")] pub fn m(&self) -> i64 { 0 }",
                "FR0010",
                "",
            ),
            // Each half's generated Dart surface is different, so the names a
            // member may not shadow are different (FR0027). Left async, which
            // both halves accept, so the only difference is the surface.
            ("pub fn copy_with(&self) -> i64 { 0 }", "FR0027", ""),
            ("pub fn dispose(&self) -> i64 { 0 }", "", "FR0027"),
        ];
        for (member, on_data, on_handle) in rows {
            for (block, want) in [("Data", on_data), ("Locked", on_handle)] {
                let ds = codes(check_src(&format!(
                    "{DUAL} #[bridge] impl {block}<Doc> {{ {member} }}"
                )));
                if want.is_empty() {
                    assert!(ds.is_empty(), "impl {block}<Doc> {member}: {ds:?}");
                } else {
                    assert!(ds.contains(want), "impl {block}<Doc> {member}: {ds:?}");
                }
            }
        }
    }

    /// FR0067 — a bare name is refused wherever a type may appear, because a
    /// name with two declarations is disambiguated only by a marker written at
    /// that position, and nothing flows between positions.
    #[test]
    fn the_bare_name_of_a_dual_type_is_refused_at_every_position() {
        for src in [
            "#[bridge(sync)] pub fn f(d: Doc) -> i64 { 0 }",
            "#[bridge(sync)] pub fn f() -> Doc { unimplemented!() }",
            "#[bridge(sync)] pub fn f() -> Vec<Doc> { vec![] }",
            "#[bridge(sync)] pub fn f() -> Option<Doc> { None }",
            "#[bridge(data)] pub struct P { pub d: Doc }",
            "#[bridge(sync)] pub fn f() -> Result<i64, Doc> { Ok(0) }",
        ] {
            let ds = codes(check_src(&format!("{DUAL} {src}")));
            assert!(ds.contains(&"FR0067"), "{src}: {ds:?}");
        }
        // And a marker at the position resolves it, on either half.
        for src in [
            "#[bridge(sync)] pub fn f(d: Data<Doc>) -> i64 { 0 }",
            "#[bridge(sync)] pub fn f() -> Vec<Locked<Doc>> { vec![] }",
            "#[bridge(sync)] pub fn f() -> Option<Locked<Doc>> { None }",
            "#[bridge(data)] pub struct P { pub d: Data<Doc> }",
            "#[bridge(sync)] pub fn f() -> Result<i64, Data<Doc>> { Ok(0) }",
        ] {
            check_src(&format!("{DUAL} {src}")).unwrap_or_else(|d| panic!("{src}: {d:?}"));
        }
    }

    /// `Self` in a member is the impl block's self type **as written**, so on
    /// a dual type it resolves — the marker is on the block, and every
    /// position inside it reads the same one. Parameter and return alike, at
    /// every depth.
    #[test]
    fn self_in_a_member_carries_the_blocks_marker() {
        check_src(&format!(
            "{DUAL} #[bridge] impl Locked<Doc> {{ \
                 #[bridge(sync, on_contention = \"error\")] pub fn cmp(&self, o: &Self) -> i64 {{ 0 }} \
                 #[bridge(sync)] pub fn n() -> Self {{ unimplemented!() }} }}"
        ))
        .unwrap_or_else(|d| panic!("{d:?}"));
        check_src(&format!(
            "{DUAL} #[bridge] impl Data<Doc> {{ \
                 #[bridge(sync)] pub fn merge(&self, o: Vec<Self>) -> Self {{ unimplemented!() }} }}"
        ))
        .unwrap_or_else(|d| panic!("{d:?}"));
    }

    /// A **declaration** writes no marker, so a field written `Self` is the
    /// bare name and FR0067 reports it — with the note that says where the
    /// name came from, because the author never typed `Doc`. A recursive field
    /// of a dual type genuinely has two readings (a nested value, or a handle
    /// to a sub-object), so this is the bare-name rule and not an exception.
    #[test]
    fn self_in_a_dual_declarations_field_is_the_bare_name() {
        let ds = check_src(
            "#[bridge(data(dart_identifier = \"DocValue\"), locked)]
             pub struct Doc { pub title: String, pub next: Option<Box<Self>> }",
        )
        .unwrap_err();
        let d = ds.iter().find(|d| d.code == "FR0067").expect("FR0067");
        assert!(d.message.contains("field `next`"), "{}", d.message);
        assert!(d.message.contains("written `Self`"), "{}", d.message);
        // A bare name at a position `Self` cannot reach gets the plain message.
        let ds = check_src(&format!("{DUAL} #[bridge(sync)] pub fn f(d: Doc) -> i64 {{ 0 }}"))
            .unwrap_err();
        assert!(
            ds.iter().all(|d| !d.message.contains("written `Self`")),
            "a parameter is never `Self`: {ds:?}"
        );
    }

    /// An `impl` block with no marker names no half, so its members have no
    /// rules to be checked under. One diagnostic for the block, not one per
    /// member — the fix is one edit — and the members are dropped rather than
    /// checked as whichever half happened to be looked up first.
    #[test]
    fn an_unplaced_impl_block_reports_once_and_stops() {
        let ds = check_src(&format!(
            "{DUAL} #[bridge] impl Doc {{
                 #[bridge(sync)] pub fn a(&self) -> i64 {{ 0 }}
                 #[bridge(sync)] pub fn b(&mut self) -> i64 {{ 0 }}
             }}"
        ))
        .unwrap_err();
        assert_eq!(
            ds.iter().filter(|d| d.code == "FR0067").count(),
            1,
            "one block, one diagnostic: {ds:?}"
        );
        // Nothing else fires about those members: FR0005 ("declares no
        // representation"), FR0013 (`&mut self` on a value) and FR0027 would
        // each be false of the tree.
        assert!(ds.iter().all(|d| d.code == "FR0067"), "{ds:?}");
    }

    /// Two blocks on one type are two fixes, and each diagnostic names the
    /// block the author wrote — including the trait, without which the
    /// message points at an `impl Doc` line that is not in their file.
    #[test]
    fn each_unplaced_block_is_reported_as_the_block_it_is() {
        let ds = check_src(&format!(
            "{DUAL}
             #[bridge(locked)] pub trait Store: Send + Sync {{ fn put(&self, k: String); }}
             #[bridge] impl Doc {{ #[bridge(sync)] pub fn a(&self) -> i64 {{ 0 }} }}
             #[bridge] impl Store for Doc {{ fn put(&self, k: String) {{}} }}"
        ))
        .unwrap_err();
        let msgs: Vec<&str> = ds
            .iter()
            .filter(|d| d.code == "FR0067")
            .map(|d| d.message.as_str())
            .collect();
        assert_eq!(msgs.len(), 2, "one per block: {ds:?}");
        assert!(
            msgs.iter().any(|m| m.starts_with("`impl Doc`")),
            "the inherent block: {msgs:?}"
        );
        assert!(
            msgs.iter().any(|m| m.starts_with("`impl Store for Doc`")),
            "the trait block: {msgs:?}"
        );
    }

    /// A struct beside a same-named *trait* is not one type declaring two
    /// representations — it is two Rust items that cannot coexist (E0428), and
    /// the duplicate-name rule is the whole of what is true about it. Nothing
    /// else may add a second sentence: `S` does declare a representation, so
    /// FR0005 saying otherwise would be false of the tree.
    #[test]
    fn a_struct_beside_a_same_named_trait_is_a_duplicate_and_nothing_more() {
        let ds = check_src(
            "#[bridge(data)] pub struct S { pub x: i64 }
             #[bridge(locked)] pub trait S: Send + Sync { fn go(&self); }",
        )
        .unwrap_err();
        assert!(
            ds.iter().all(|d| d.code == "FR0002"),
            "one cause, one diagnostic: {ds:?}"
        );
    }

    /// FR0062 on a marker that names a third representation, listing the two
    /// the type actually has — at a use site and on an impl block alike.
    #[test]
    fn a_marker_naming_neither_half_lists_both() {
        for src in [
            "#[bridge] impl Frozen<Doc> { #[bridge(sync)] pub fn a(&self) -> i64 { 0 } }",
            "#[bridge(sync)] pub fn f(d: Frozen<Doc>) -> i64 { 0 }",
        ] {
            let ds = check_src(&format!("{DUAL} {src}")).unwrap_err();
            let d = ds.iter().find(|d| d.code == "FR0062").expect("FR0062");
            assert!(d.message.contains("`data` and `locked`"), "{}", d.message);
        }
    }

    /// One member name may sit on each half — they are two Dart classes —
    /// and two on one half may not, whichever kind of `impl` declared them.
    #[test]
    fn a_member_name_is_unique_per_class_not_per_type() {
        let iface = "#[bridge(data(dart_identifier = \"DocValue\"), locked)]
             pub struct Doc { pub title: String }
             #[bridge(locked)] pub trait Store: Send + Sync { fn put(&self, k: String); }";
        // One on each half: two classes, no collision.
        check_src(&format!(
            "{iface}
             #[bridge] impl Store for Locked<Doc> {{ fn put(&self, k: String) {{}} }}
             #[bridge] impl Data<Doc> {{ pub fn put(&self, k: String) {{}} }}"
        ))
        .unwrap();
        // Two on one half: one Dart class, and the second would shadow the
        // first. rustc allows an inherent method beside a trait method of that
        // name, so only this rule catches it.
        let ds = codes(check_src(&format!(
            "{iface}
             #[bridge] impl Store for Locked<Doc> {{ fn put(&self, k: String) {{}} }}
             #[bridge] impl Locked<Doc> {{ pub fn put(&self, k: String) {{}} }}"
        )));
        assert!(ds.contains(&"FR0002"), "{ds:?}");
    }

    /// A bare `impl Trait for Doc` is offered the handle marker only: a
    /// bridged trait is implemented by handles (FR0026), so the value half is
    /// not a fix.
    #[test]
    fn an_unplaced_trait_block_is_offered_only_the_half_that_can_take_it() {
        let ds = check_src(&format!(
            "{DUAL}
             #[bridge(locked)] pub trait Store: Send + Sync {{ fn put(&self, k: String); }}
             #[bridge] impl Store for Doc {{ fn put(&self, k: String) {{}} }}"
        ))
        .unwrap_err();
        let d = ds.iter().find(|d| d.code == "FR0067").expect("FR0067");
        assert!(d.message.contains("impl Store for Locked<Doc>"), "{}", d.message);
        assert!(
            !d.message.contains("impl Store for Data<Doc>"),
            "the value half is FR0026 here: {}",
            d.message
        );
    }

    /// The pair of declarations is one Rust item, so it is not a duplicate
    /// name (FR0002's type-name rule) — but the two Dart classes it mints are
    /// still two, and that collision is reported like any other, with the
    /// spelling that can actually fix it.
    #[test]
    fn the_two_halves_collide_on_one_dart_name_and_the_fix_is_named() {
        let ds = check_src(
            "#[bridge(data, locked)] pub struct Doc { pub title: String }
             #[bridge(sync)] pub fn f(d: Data<Doc>) -> i64 { 0 }",
        )
        .unwrap_err();
        let d = ds.iter().find(|d| d.code == "FR0002").expect("FR0002");
        assert!(d.message.contains("One Rust type, two Dart classes"), "{}", d.message);
        assert!(d.message.contains("data(dart_identifier"), "{}", d.message);
        // …and not the duplicate-*type*-name message, which has no fix here.
        assert!(!d.message.contains("must be unique"), "{}", d.message);
        // The same when both halves were named, and named the same: still one
        // Rust type needing one of its two classes moved. Keyed on the type
        // the names were minted from, not on the name they met at.
        let ds = check_src(
            "#[bridge(data(dart_identifier = \"X\"), locked(dart_identifier = \"X\"))]
             pub struct Doc { pub title: String }
             #[bridge(sync)] pub fn f(d: Data<Doc>) -> i64 { 0 }",
        )
        .unwrap_err();
        let d = ds.iter().find(|d| d.code == "FR0002").expect("FR0002");
        assert!(d.message.contains("One Rust type, two Dart classes"), "{}", d.message);
    }

    /// A bridged trait is implemented by handles (FR0026), so on a dual type
    /// the impl goes on the handle half — and the methods the impl block does
    /// not write are synthesized onto that half, not into neither.
    #[test]
    fn a_bridged_trait_is_implemented_by_the_handle_half() {
        let iface = "#[bridge(data(dart_identifier = \"DocValue\"), locked)]
             pub struct Doc { pub title: String }
             #[bridge(locked)] pub trait Store: Send + Sync {
                 fn put(&self, k: String);
                 fn hint(&self) -> i64 { 0 }
             }";
        let ok = check_src(&format!(
            "{iface} #[bridge] impl Store for Locked<Doc> {{ fn put(&self, k: String) {{}} }}"
        ))
        .unwrap();
        // The written method and the synthesized default both land on the
        // handle half; a member on neither half is emitted into no class at
        // all, which Dart reports far from the cause.
        for name in ["put", "hint"] {
            let f = ok
                .functions
                .iter()
                .find(|f| f.parent.as_deref() == Some("Doc") && f.name == name)
                .unwrap_or_else(|| panic!("`{name}` on Doc"));
            assert_eq!(ok.member_repr(f), Some(Repr::Handle), "{name}");
        }
        // The value half cannot implement it: its Dart class carries no
        // handle surface for the trait's interface to require.
        let ds = codes(check_src(&format!(
            "{iface} #[bridge] impl Store for Data<Doc> {{ fn put(&self, k: String) {{}} }}"
        )));
        assert!(ds.contains(&"FR0026"), "{ds:?}");
    }

    /// An actor's synthetic drop is a member of its handle half. Without that
    /// the Dart actor emitter looks for it among the handle half's members and
    /// finds none.
    #[test]
    fn a_dual_actors_drop_is_on_the_handle_half() {
        let ok = check_src(
            "#[bridge(data(dart_identifier = \"JobValue\"), actor)] pub struct Job { pub n: i64 }
             #[bridge] impl Data<Job> { #[bridge(sync)] pub fn doubled(&self) -> i64 { 0 } }
             #[bridge] impl Actor<Job> { pub fn spawn() -> Actor<Job> { unimplemented!() } }",
        )
        .unwrap();
        let drop = ok
            .functions
            .iter()
            .find(|f| f.is_actor_drop)
            .expect("finalize appends an actor drop");
        assert_eq!(ok.member_repr(drop), Some(Repr::Handle));
        assert_eq!(ok.members("Job", Repr::Handle).count(), 2);
        assert_eq!(ok.members("Job", Repr::Data).count(), 1);
    }

    /// `#[derive(Copy)]` is on the value declaration, so it says nothing about
    /// the handle half: `self` by value there is a **consume** of an object
    /// Dart holds a handle to (through `take()`), not the free copy the value
    /// half's `self` is — so it lands on the handle class, takes no lock, and
    /// refuses the contention contract a consuming member has no use for
    /// (FR0010).
    #[test]
    fn copy_licenses_a_by_value_receiver_on_the_value_half_only() {
        let iface = "#[bridge(data(dart_identifier = \"PtValue\"), locked)]
             #[derive(Clone, Copy)] pub struct Pt { pub x: i64 }";
        let value = check_src(&format!(
            "{iface} #[bridge] impl Data<Pt> {{ #[bridge(sync)] pub fn n(self) -> i64 {{ 0 }} }}"
        ))
        .unwrap();
        let n = value.functions.iter().find(|f| f.name == "n").unwrap();
        assert_eq!(value.member_repr(n), Some(Repr::Data));
        let handle = check_src(&format!(
            "{iface} #[bridge] impl Locked<Pt> {{ #[bridge(sync)] pub fn n(self) -> i64 {{ 0 }} }}"
        ))
        .unwrap();
        let n = handle.functions.iter().find(|f| f.name == "n").unwrap();
        assert_eq!(handle.member_repr(n), Some(Repr::Handle));
        assert!(!n.requires_native, "a consuming member takes no guard");
        let ds = codes(check_src(&format!(
            "{iface} #[bridge] impl Locked<Pt> {{ #[bridge(sync, on_contention = \"error\")] pub fn n(self) -> i64 {{ 0 }} }}"
        )));
        assert_eq!(ds, vec!["FR0010"], "{ds:?}");
    }
    // ------------------------------------------ a borrow inside a type --

    const BORROW_DECL: &str = "#[bridge(data)] pub struct P { pub x: i64 } \
         #[bridge(frozen)] pub struct D { n: i64 } \
         #[bridge] impl D { #[bridge(sync)] pub fn new() -> Self { todo!() } } \
         #[bridge(confined)] pub struct C { n: i64 } \
         #[bridge] impl C { #[bridge(sync)] pub fn new() -> Self { todo!() } }";

    /// The positions a borrow may sit in: `Option`, a list and a tuple, over a
    /// handle or a value, nested among themselves to any depth.
    #[test]
    fn a_borrow_is_accepted_in_a_container_position() {
        for ty in [
            "Vec<&D>",
            "Option<&D>",
            "(&D, i64)",
            "Vec<Option<&D>>",
            "Option<Vec<&D>>",
            "Vec<(&D, i64)>",
            "Vec<&mut C>",
            "Vec<&str>",
            "Option<&str>",
            "Option<&[u8]>",
            "Vec<&P>",
            "Vec<&Vec<i64>>",
        ] {
            let src = format!("{BORROW_DECL} #[bridge(sync)] pub fn f(x: {ty}) {{ let _ = x; }}");
            assert_eq!(codes(check_src(&src)), Vec::<&str>::new(), "{ty}");
        }
        // A top-level `&[&D]` is the slice spelling of the same thing.
        let src = format!("{BORROW_DECL} #[bridge(sync)] pub fn f(x: &[&D]) {{ let _ = x; }}");
        assert_eq!(codes(check_src(&src)), Vec::<&str>::new());
    }

    /// A borrow of a **generic instantiation** inside a container. The
    /// expansion has to walk through the reference to find the application,
    /// or the emitter meets an unexpanded `App` in a value position — which is
    /// exactly what happened when the two features first met.
    #[test]
    fn a_borrow_of_an_instantiation_is_expanded() {
        let ok = check_src(
            "#[bridge(data)] pub struct Item { pub id: i64 } \
             #[bridge(data)] pub struct Page<T> { pub items: Vec<T>, pub total: i64 } \
             #[bridge(sync)] pub fn f(ps: Vec<&Page<Item>>) -> i64 { 0 } \
             #[bridge(sync)] pub fn g(p: Option<&Page<Item>>) -> i64 { 0 }",
        )
        .unwrap();
        assert!(ok.struct_decl("Page<Item>").is_some(), "the lent use was expanded");
        for name in ["f", "g"] {
            let f = ok.functions.iter().find(|f| f.name == name).unwrap();
            let mut lent = false;
            f.params[0].ty.walk(&mut |t| {
                if let Type::Ref { inner, .. } = t {
                    lent |= **inner == Type::Struct("Page<Item>".into());
                }
            });
            assert!(lent, "{name}: the borrow's inner is the expansion, not an `App`");
        }
    }

    /// FR0077 — the positions that own what they decode, each refused with its
    /// own reason rather than one sentence that would have to be vague enough
    /// to be false somewhere.
    #[test]
    fn a_borrow_is_refused_where_the_decode_owns_what_it_builds() {
        for (ty, reason) in [
            ("std::collections::HashSet<&P>", "set or a map"),
            ("std::collections::HashMap<i64, &P>", "set or a map"),
            ("[&P; 2]", "fixed array"),
            ("Vec<&&P>", "reference inside another reference"),
            ("Box<&P>", "`Box` is put back"),
        ] {
            let src = format!("{BORROW_DECL} #[bridge(sync)] pub fn f(x: {ty}) {{ let _ = x; }}");
            let ds = check_src(&src).unwrap_err();
            let d = ds.iter().find(|d| d.code == "FR0077").unwrap_or_else(|| panic!("{ty}: {ds:?}"));
            assert!(d.message.contains(reason), "{ty}: {}", d.message);
        }
        // A struct or enum field, refused once at the declaration.
        let ds = check_src("#[bridge(data)] pub struct S { pub name: &'static str }").unwrap_err();
        let d = ds.iter().find(|d| d.code == "FR0077").unwrap_or_else(|| panic!("{ds:?}"));
        assert!(d.message.contains("data class is reconstructed"), "{}", d.message);
        // A mirror's item type.
        let ds = check_src(
            "#[bridge(data)] pub struct P { pub x: i64 } \
             #[bridge(sync)] pub fn f(c: DartCallback<Vec<&P>>) { let _ = c; }",
        )
        .unwrap_err();
        let d = ds.iter().find(|d| d.code == "FR0077").unwrap_or_else(|| panic!("{ds:?}"));
        assert!(d.message.contains("envelope"), "{}", d.message);
    }

    /// FR0013 reaches a nested `&mut` for the reason it reaches a top-level
    /// one: the mutation lands on a local the glue decoded and dies with it.
    /// A `&mut` of a *handle* is a real exclusive borrow and stays accepted.
    #[test]
    fn a_nested_mutable_borrow_of_a_value_is_refused() {
        let src = format!("{BORROW_DECL} #[bridge(sync)] pub fn f(x: Vec<&mut P>) {{ let _ = x; }}");
        assert_eq!(codes(check_src(&src)), vec!["FR0013"]);
        let src = format!("{BORROW_DECL} #[bridge(sync)] pub fn f(x: Vec<&mut C>) {{ let _ = x; }}");
        assert_eq!(codes(check_src(&src)), Vec::<&str>::new());
    }

    /// One container lends a **handle** beside a take — the reference points at
    /// the object, so the walk that builds the argument may consume the
    /// container it reads.
    #[test]
    fn a_container_lends_a_handle_beside_a_take() {
        for ty in ["Vec<(&D, D)>", "Option<(&C, C)>", "Vec<(&mut C, C)>"] {
            let src = format!("{BORROW_DECL} #[bridge(sync)] pub fn f(x: {ty}) {{ let _ = x; }}");
            assert!(check_src(&src).is_ok(), "{ty}: {:?}", check_src(&src).err());
        }
    }

    /// FR0078 — a borrowed **value** beside a take in one container. Its
    /// pointee lives in that container, which the argument walk moves the taken
    /// objects out of.
    #[test]
    fn a_borrowed_value_beside_a_take_is_refused() {
        for ty in ["Vec<(&str, D)>", "Vec<(&P, D)>", "Option<(&String, C)>"] {
            let src = format!("{BORROW_DECL} #[bridge(sync)] pub fn f(x: {ty}) {{ let _ = x; }}");
            let ds = check_src(&src).unwrap_err();
            let d = ds
                .iter()
                .find(|d| d.code == "FR0078")
                .unwrap_or_else(|| panic!("{ty}: {ds:?}"));
            assert!(d.message.contains("borrows a value"), "{}", d.message);
        }
    }

    /// A bridged trait inside a container carries a **per-element** impl tag,
    /// lent or taken. The model rules reach it there like any other handle.
    #[test]
    fn a_bridged_trait_crosses_from_inside_a_container() {
        for ty in ["Vec<&dyn S>", "Option<&dyn S>", "Vec<Box<dyn S>>"] {
            assert!(
                check_src(&format!(
                    "#[bridge(frozen)] pub trait S: Send + Sync {{ fn n(&self) -> i64; }} \
                     #[bridge(sync)] pub fn f(xs: {ty}) -> i64 {{ 0 }}"
                ))
                .is_ok(),
                "{ty}"
            );
        }
        // FR0006 still reaches a frozen `&mut` element, trait or not.
        let ds = check_src(
            "#[bridge(frozen)] pub trait S: Send + Sync { fn n(&self) -> i64; } \
             #[bridge(sync)] pub fn f(xs: Vec<&mut dyn S>) -> i64 { 0 }",
        )
        .unwrap_err();
        assert!(ds.iter().any(|d| d.code == "FR0006"), "{ds:?}");
    }

    /// The model rules reach a lent handle wherever it sits: a frozen `&mut`
    /// is FR0006 and a borrowed confined handle in an async member is FR0012,
    /// exactly as at the top of a parameter.
    #[test]
    fn the_model_rules_reach_a_lent_handle() {
        let ds = check_src(
            "#[bridge(frozen)] pub struct D { n: i64 } \
             #[bridge] impl D { #[bridge(sync)] pub fn new() -> Self { todo!() } } \
             #[bridge(sync)] pub fn f(xs: Vec<&mut D>) { let _ = xs; }",
        )
        .unwrap_err();
        assert!(ds.iter().any(|d| d.code == "FR0006"), "{ds:?}");
        let ds = check_src(
            "#[bridge(confined)] pub struct C { n: i64 } \
             #[bridge] impl C { #[bridge(sync)] pub fn new() -> Self { todo!() } } \
             #[bridge] pub async fn f(xs: Vec<&C>) { let _ = xs; }",
        )
        .unwrap_err();
        assert!(ds.iter().any(|d| d.code == "FR0012"), "{ds:?}");
    }

    /// A lent **locked** handle takes a guard exactly as a top-level one does,
    /// so a sync member holding one needs the same contention contract.
    #[test]
    fn a_lent_locked_handle_needs_a_contention_contract() {
        let decl = "#[bridge(locked)] pub struct V { n: i64 } \
                    #[bridge] impl V { #[bridge(sync)] pub fn new() -> Self { todo!() } }";
        let ds = codes(check_src(&format!(
            "{decl} #[bridge(sync)] pub fn f(xs: Vec<&V>) {{ let _ = xs; }}"
        )));
        assert_eq!(ds, vec!["FR0007"]);
        check_src(&format!(
            "{decl} #[bridge(sync, on_contention = \"error\")] pub fn f(xs: Vec<&V>) {{ let _ = xs; }}"
        ))
        .unwrap();
    }

    /// FR0015 reaches a handle a parameter lends or takes from inside a
    /// container, not only a bare one: an actor's objects live in its own
    /// executor however deeply the signature buries the handle.
    #[test]
    fn an_actor_handle_cannot_cross_inside_a_container() {
        let decl = "#[bridge(actor)] pub struct A { n: i64 } \
                    #[bridge] impl A { pub fn new() -> Self { todo!() } }";
        for ty in ["Vec<&A>", "Vec<A>", "Option<&A>"] {
            let ds = codes(check_src(&format!(
                "{decl} #[bridge] pub async fn f(xs: {ty}) {{ let _ = xs; }}"
            )));
            assert!(ds.contains(&"FR0015"), "{ty}: {ds:?}");
        }
    }

    /// FR0004 on the return path reaches a nested borrow: the value is copied
    /// out through the reference, and a handle cannot be minted from a `&T`
    /// the body still owns.
    #[test]
    fn a_lent_handle_in_the_return_is_refused() {
        let src = format!("{BORROW_DECL} #[bridge(sync)] pub fn f() -> Option<&'static D> {{ None }}");
        let ds = check_src(&src).unwrap_err();
        let d = ds.iter().find(|d| d.code == "FR0004").unwrap_or_else(|| panic!("{ds:?}"));
        assert!(d.message.contains("borrowed return"), "{}", d.message);
    }

    /// FR0044 names a lifetime wherever the author wrote it. The referent of a
    /// nested borrow is not the request buffer, but the hazard is the same one:
    /// the glue conjures its references with an unbound lifetime.
    #[test]
    fn a_named_lifetime_is_refused_at_any_depth() {
        let src = format!("{BORROW_DECL} #[bridge(sync)] pub fn f(xs: Vec<&'static D>) {{ let _ = xs; }}");
        let ds = check_src(&src).unwrap_err();
        assert!(ds.iter().any(|d| d.code == "FR0044"), "{ds:?}");
    }

}
