//! Handle representation for opaque types.
//!
//! A handle is the integer value of a raw pointer, owned by the Dart side
//! (NativeFinalizer or explicit dispose calls the generated drop). The
//! concurrency model decides the container, and with it the thread bound the
//! model's own plumbing needs:
//!
//!   Confined -> Box<T>          `T: Send`         (single owner, serialized use)
//!   Resident -> Box<T>          no bound          (single owner, one thread)
//!   Frozen   -> Arc<T>          `T: Send + Sync`  (immutable; share freely)
//!   Locked   -> Arc<RwLock<T>>  `T: Send + Sync`  (shared mutable; lock to touch)
//!   Actor    -> Box<T>          no bound          (thread-affine by construction)
//!
//! The bounds live on the `*_new` constructors because minting is the only way
//! a handle comes to exist. They are not redundant with the contracts:
//!
//! * A **dispatched** constructor runs its body on a pool worker and turns the
//!   value into a pointer *inside* the pool closure, so the value's type never
//!   appears in the closure and rustc never sees the transfer. The object is
//!   born on a worker and then used from the calling isolate. Only `Send` on
//!   `T` states that this is legal.
//! * The transport's happens-before edge, the exclusive ownership a handle
//!   gives, and the one-isolate contract are the *preconditions* under which
//!   `Send` licenses that move — not substitutes for it. None of them says the
//!   type has no thread-affine internals (an `Rc` whose other clones live in a
//!   worker's TLS, a thread-bound FFI context, a `JsValue` indexing a
//!   per-thread JS heap table under threaded wasm).
//! * The one-isolate contract governs *use*, not birthplace. No discipline on
//!   the Dart side can honour it when the glue itself breaks affinity at
//!   construction.
//!
//! Two models are exempt, and each is genuinely thread-affine end to end.
//! **Actor** owns a thread: its decode, body and drop all run on its own
//! executor, so it can host a `!Send` type and still be reached from anywhere,
//! asynchronously. **Resident** owns none: it removes the two mechanisms above
//! rather than accommodating them (a dispatched constructor is refused, and
//! the GC drop is a `dart:core` `Finalizer` on the owning isolate), so a
//! `!Send` type is reached synchronously and for free — at the price that an
//! isolate which exits holding one leaks it, since no other thread may run its
//! `Drop`. See `docs/CHARTER.md` and [`resident_new`].
//!
//! Safety: all `unsafe` here relies on the generated code upholding the
//! frustrate contracts — handles are created by the matching `*_new`, used
//! only while the Dart handle is alive (Finalizable keeps it alive across
//! calls), and dropped exactly once.

use std::sync::Arc;
// The runtime's own lock, not `std::sync`'s: the `locked` model's guards must
// be `Send` (a generated future holds one across the user body) and its
// acquisition must be able to `.await` (a thread cannot wait on
// single-threaded web).
use crate::rwlock::RwLock;

/// The `locked` model's lock and guards, re-exported so **generated glue names
/// one module**.
///
/// A bridge crate depends on `frustrate`, not on frustrate's internals, and the
/// glue is `include!`d into it — so anything the emitter spells has to resolve
/// from here. Going through this module also keeps the choice swappable:
/// replacing the lock is a change to [`crate::rwlock`], not to codegen and not
/// to anyone's `Cargo.toml`.
pub use crate::rwlock::RwLock as LockedCell;
pub use crate::rwlock::{RwLockReadGuard, RwLockWriteGuard, TryLockError};

/// **Where a locked guard is taken and dropped, per configuration.**
///
///   - **Dispatched members — every model, every target.** The acquisition is
///     inside the future, and the future is polled wherever
///     `executor::arrange_drain` routes it: a pool thread natively, a pool
///     worker on threaded web, a microtask on single-threaded web. A cancelled
///     call's guard drops there too — `async_task` drops the future inside
///     `runnable.run()`.
///   - **Sync members — the calling thread, on every target**, which on web is
///     the browser main thread. That is legal because [`LockedCell`] is
///     frustrate's own: `try_read`/`try_write` are one compare-exchange, and
///     the release is one more unless a waiter is queued on the object, in
///     which case it takes a waiter list that is a `SpinLock` on threaded
///     wasm. No path reaches a wait instruction, so `on_contention = "error"`
///     is on every surface. `on_contention = "block"` is the exception and
///     stays native-only: it waits to *acquire*, by declaration (FR0008), and
///     `blocking_read`/`blocking_write` are compiled off wasm.
///   - **Freeing a handle takes no lock at all.** The lock has no `Drop` impl,
///     so `locked_drop` just drops the `T`.
const _WHERE_A_LOCKED_GUARD_LIVES: () = ();

// ---- Model bounds, as named diagnostics ----
//
// The `*_new` bounds are the backstop: correct, but they fail at a
// `handle::confined_new::<T>` call buried in generated glue, and the message
// is a bare `Send` obligation with no idea what a "confined opaque" is. So
// codegen also emits, per non-actor opaque, one line near the top of the
// generated file:
//
//     const _: fn() = handle::assert_confined::<crate::api::TextDoc>;
//
// which fails against the marker trait below and prints the model, the type
// and the reason. `#[diagnostic::do_not_recommend]` on the blanket impl is
// what stops rustc from drilling past the custom message to the raw `Send`
// obligation underneath.
//
// Codegen emits these for *every* non-actor opaque, including one no bridged
// function returns — such a type has no `*_new` call site to carry the bound,
// and the requirement does not depend on having one: `frozen_ref` hands `&T`
// to callers in other isolates concurrently, and the last `Arc` clone drops
// wherever it happens to die.

/// Marker for `#[bridge(confined)]` types. Blanket-implemented for
/// every `Send` type; exists only to carry the diagnostic.
#[diagnostic::on_unimplemented(
    message = "confined opaque `{Self}` must be `Send`",
    label = "not `Send`",
    note = "a constructor that is not `#[bridge(sync)]` builds the value on a pool worker, and the handle is then used from the calling isolate",
    note = "a type with thread-affine internals — `Rc`, a thread-bound FFI context, a wasm-bindgen `JsValue` — belongs in `#[bridge(resident)]` (sync members on the caller, no thread bound, at the price that an isolate which exits leaks what it held) or `#[bridge(actor)]` (its own executor, async members, reachable from any isolate)"
)]
pub trait ConfinedSafe {}

#[diagnostic::do_not_recommend]
impl<T: Send> ConfinedSafe for T {}

/// Marker for `#[bridge(frozen)]` types.
#[diagnostic::on_unimplemented(
    message = "frozen opaque `{Self}` must be `Send + Sync`",
    label = "not `Send + Sync`",
    note = "`Arc<{Self}>` crosses to the pool, `frozen_ref` hands out `&{Self}` to callers in other isolates concurrently, and the last clone can drop on a pool thread",
    note = "a type with thread-affine internals — `Rc`, a thread-bound FFI context, a wasm-bindgen `JsValue` — belongs in `#[bridge(resident)]` (sync members on the caller, no thread bound, at the price that an isolate which exits leaks what it held) or `#[bridge(actor)]` (its own executor, async members, reachable from any isolate)"
)]
pub trait FrozenSafe {}

#[diagnostic::do_not_recommend]
impl<T: Send + Sync> FrozenSafe for T {}

/// Marker for `#[bridge(locked)]` types.
#[diagnostic::on_unimplemented(
    message = "locked opaque `{Self}` must be `Send + Sync`",
    label = "not `Send + Sync`",
    note = "`Arc<RwLock<{Self}>>` crosses to the pool — and `RwLock<{Self}>` is itself `Sync` only when `{Self}` is `Send + Sync`, because concurrent readers each get a `&{Self}`",
    note = "a type with thread-affine internals — `Rc`, a thread-bound FFI context, a wasm-bindgen `JsValue` — belongs in `#[bridge(resident)]` (sync members on the caller, no thread bound, at the price that an isolate which exits leaks what it held) or `#[bridge(actor)]` (its own executor, async members, reachable from any isolate)"
)]
pub trait LockedSafe {}

#[diagnostic::do_not_recommend]
impl<T: Send + Sync> LockedSafe for T {}

/// The confined thread bound, as a named error. Called only by the
/// `const _: fn() = …` line codegen emits per confined opaque.
///
/// ```compile_fail,E0277
/// frustrate::handle::assert_confined::<std::rc::Rc<i32>>();
/// ```
pub fn assert_confined<T: ConfinedSafe>() {}

/// The frozen thread bound, as a named error. See [`assert_confined`].
///
/// ```compile_fail,E0277
/// frustrate::handle::assert_frozen::<std::cell::Cell<i32>>();
/// ```
pub fn assert_frozen<T: FrozenSafe>() {}

/// The locked thread bound, as a named error. See [`assert_confined`].
///
/// ```compile_fail,E0277
/// frustrate::handle::assert_locked::<std::cell::Cell<i32>>();
/// ```
pub fn assert_locked<T: LockedSafe>() {}

// ---- Confined: Box<T> ----

/// `Send`, and deliberately not `Sync`: a confined object has one owner and
/// serialized use, so it never needs to hand `&T` to two threads at once — but
/// it can be born on a pool worker and used from the calling isolate.
///
/// ```compile_fail,E0277
/// frustrate::handle::confined_new(std::rc::Rc::new(1i32));
/// ```
pub fn confined_new<T: Send + 'static>(value: T) -> u64 {
    Box::into_raw(Box::new(value)) as u64
}

/// # Safety
/// `h` must be a live handle created by `confined_new::<T>`. **Not** a
/// resident handle: resident has its own accessors because an acquisition
/// there also checks the calling thread, and routing one here would skip that.
pub unsafe fn confined_ref<'a, T>(h: u64) -> &'a T {
    &*(h as *const T)
}

/// # Safety
/// `h` must be a live handle created by `confined_new::<T>`, and the caller
/// must have exclusive access (the Confined contract: one isolate, sync calls
/// only). See [`confined_ref`] on why a resident handle does not belong here.
#[allow(clippy::mut_from_ref)]
pub unsafe fn confined_mut<'a, T>(h: u64) -> &'a mut T {
    &mut *(h as *mut T)
}

/// # Safety
/// `h` must be a live handle created by `confined_new::<T>`; never used after.
pub unsafe fn confined_drop<T>(h: u64) {
    drop(Box::from_raw(h as *mut T));
}

// ---- Resident: Box<T>, no thread bound ----

/// Confined's `Box`, minus the `Send` — because the two mechanisms that
/// needed it are gone, not because the requirement was waived.
///
/// [`confined_new`]'s doc names both: a dispatched constructor builds the
/// value on a pool worker, and a `NativeFinalizer` frees it on an arbitrary
/// VM thread. Resident removes each at its source. A member or constructor
/// that would be dispatched is refused at codegen time (FR0079), so the value
/// is built by the caller; and the Dart handle's GC path is a `dart:core`
/// `Finalizer`, whose callback runs on the isolate that attached it, so the
/// value is freed by the caller too. Between those two the object never leaves
/// one thread, and `Send` has nothing left to license.
///
/// `name` is the Rust type name, and it is here for one reason: an isolate
/// that exits leaves its live residents unreclaimable — nothing else may run a
/// `!Send` value's `Drop` — and [`crate::resident`] has to be able to say what
/// was lost. That module also states what the bookkeeping costs.
///
/// ```
/// // The `Rc` that `confined_new` refuses.
/// let h = frustrate::handle::resident_new(std::rc::Rc::new(1i32), "Probe");
/// unsafe { frustrate::handle::resident_drop::<std::rc::Rc<i32>>(h) };
/// ```
///
/// The zero-sized refusal, as a compile error rather than as a promise:
///
/// ```compile_fail,E0080
/// frustrate::handle::resident_new((), "Zst");
/// ```
///
/// # A resident type may not be zero-sized
///
/// Refused at compile time, by the assertion below. `Box::new` of a zero-sized
/// value is a dangling aligned address rather than an allocation, so **every**
/// zero-sized object shares one handle ([`alias_entry`] says why that is
/// harmless for confined). The resident registry is keyed by that handle, so
/// two such objects would collide on one entry and retiring either would
/// unregister both — taking the thread check and the isolate attribution with
/// it. The restriction costs nothing real: a thread-affine context, which is
/// what this model is for, holds something.
pub fn resident_new<T: 'static>(value: T, name: &'static str) -> u64 {
    const {
        assert!(
            std::mem::size_of::<T>() != 0,
            "a #[bridge(resident)] type must not be zero-sized: every zero-sized \
             Box shares one address, so the runtime cannot tell two of them apart \
             and cannot enforce the thread rule the model rests on. Give the type \
             a field, or declare it #[bridge(confined)]"
        );
    }
    let h = Box::into_raw(Box::new(value)) as u64;
    #[cfg(not(target_family = "wasm"))]
    crate::resident::record(h, name);
    #[cfg(target_family = "wasm")]
    let _ = name;
    h
}

/// Refuse an acquisition from a thread that did not build the object.
///
/// **A Dart isolate is not a thread**, which is the whole reason this exists:
/// an isolate may run each event-loop turn on a different OS thread, and a
/// spawned one measurably does (see [`crate::resident`]). Without this, the
/// model's contract would be prose that an ordinary multi-isolate program
/// violates silently, into undefined behaviour.
///
/// Panics rather than returns: every sync dispatch arm runs inside
/// `envelope::run`, so the panic becomes a `statusPanic` envelope and reaches
/// Dart as a `BridgePanicException` naming the type — the same route a
/// panicking user body takes. A `Result` here would have to be threaded
/// through every generated accessor site to say the same thing later.
///
/// A no-op on web: one wasm instance is one thread, and a web worker is a
/// separate instance with separate memory, so nothing can reach here from
/// elsewhere.
#[inline]
fn assert_owning_thread(h: u64, what: &str) {
    #[cfg(not(target_family = "wasm"))]
    if let Err(why) = crate::resident::on_owning_thread(h) {
        panic!("{}", crate::resident::refusal(why, what));
    }
    #[cfg(target_family = "wasm")]
    let _ = (h, what);
}

/// # Safety
/// `h` must be a live handle created by `resident_new::<T>`.
///
/// # Panics
/// If the calling thread is not the one that built the object.
pub unsafe fn resident_ref<'a, T>(h: u64) -> &'a T {
    assert_owning_thread(h, "read");
    &*(h as *const T)
}

/// # Safety
/// `h` must be a live handle created by `resident_new::<T>`, and the caller
/// must have exclusive access (one isolate, sync calls only).
///
/// # Panics
/// If the calling thread is not the one that built the object.
#[allow(clippy::mut_from_ref)]
pub unsafe fn resident_mut<'a, T>(h: u64) -> &'a mut T {
    assert_owning_thread(h, "mutated");
    &mut *(h as *mut T)
}

/// Take the object, for a consuming member. The `Box` is the one
/// [`resident_new`] made, so `self: Box<Self>` gets the original allocation.
///
/// The registry entry goes with it: the object is the caller's from here, and
/// a value that has left is not one an isolate can still leak.
///
/// # Safety
/// `h` must be a live handle created by `resident_new::<T>` whose Dart side
/// has given it up; never used after.
///
/// # Panics
/// If the calling thread is not the one that built the object.
pub unsafe fn resident_adopt<T>(h: u64) -> Box<T> {
    assert_owning_thread(h, "consumed");
    #[cfg(not(target_family = "wasm"))]
    crate::resident::retire(h);
    Box::from_raw(h as *mut T)
}

/// Free a resident object — the `dispose()` path and the `dart:core`
/// `Finalizer` path both land here.
///
/// **A reclaim on the wrong thread leaks instead of dropping**, and says so
/// through the same report an isolate exit writes. Running a `!Send` value's
/// `Drop` from a thread that may not touch it is exactly the undefined
/// behaviour the model is arranged to avoid, so the only two answers are "not
/// yet" and "never"; there is no later, because nothing schedules work back
/// onto a Dart thread. It does not panic here for the reason it panics
/// everywhere else: this is called straight from an `extern "C"` export, where
/// an unwind aborts the process. The eager `dispose()` path *is* told — the
/// native transport asks [`crate::resident::frustrate_resident_reclaimable`]
/// first and throws — because there is a caller there to tell.
///
/// One consequence to know: a stranded object's `Drop` never runs, so any
/// stream or callback it still holds is never ended, and an open registration
/// keeps its isolate alive. That follows from the leak rather than adding to
/// it — the object is unreachable either way — but it is why `dispose()` on
/// the owning thread, not collection, is the discipline this model asks for.
///
/// # Safety
/// `h` must be a live handle created by `resident_new::<T>`; never used after.
pub unsafe fn resident_drop<T>(h: u64) {
    #[cfg(not(target_family = "wasm"))]
    {
        match crate::resident::on_owning_thread(h) {
            // Another thread. The object is stranded: it is retired from the
            // registry (nothing will ever reclaim it) and counted, and the
            // `Box` is deliberately not freed.
            Err(crate::resident::Refused::OtherThread(name)) => {
                crate::resident::retire(h);
                crate::resident::strand(name);
                return;
            }
            // No entry, so nothing to free and nothing to count: this handle
            // was already reclaimed. Returning is the whole point — freeing
            // again would be the double free.
            Err(crate::resident::Refused::Gone) => return,
            Ok(()) => {
                crate::resident::retire(h);
            }
        }
    }
    drop(Box::from_raw(h as *mut T));
}

/// What one acquisition does with the object it names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Use {
    /// `&T`.
    Shared,
    /// `&mut T`.
    Mut,
    /// The object itself: the call takes it and the Dart handle is spent.
    Consume,
}

impl Use {
    /// Whether this access excludes every other one. `&mut` does by Rust's own
    /// rule; a **consume** does more strongly still, because it ends the
    /// object rather than merely borrowing it exclusively.
    fn exclusive(self) -> bool {
        !matches!(self, Use::Shared)
    }
}

/// One `Box`-held acquisition (confined, resident or actor), for
/// [`alias_check`]: the
/// handle, what the call does with it, and whether it addresses any bytes.
///
/// `None` means "cannot alias". Every zero-sized value shares one dangling
/// address (`Box::new(Zst)` is that address, not an allocation), so two
/// *distinct* ZST objects carry the same handle — and having no bytes, they
/// cannot alias anyway, nor can two takes of them free anything twice.
/// Recording the emptiness here rather than at the call site keeps the
/// generated code from having to know it.
///
/// Only the `Box` containers collapse this way. An `Arc` allocates a header —
/// two counters — even for a zero-sized `T`, so two frozen objects have
/// distinct handle ids however small they are, and forgiving a duplicate there
/// would let two takes free one `ArcInner` twice. [`arc_alias_entry`] is their
/// constructor and it forgives nothing.
pub fn alias_entry<T>(h: u64, usage: Use) -> (Option<u64>, Use) {
    let addresses_bytes = std::mem::size_of::<T>() != 0;
    (addresses_bytes.then_some(h), usage)
}

/// [`alias_entry`] for an `Arc`-held object (frozen). No ZST collapse: see
/// [`alias_entry`].
///
/// Not generic, because the size of `T` was the only thing the type parameter
/// decided.
pub fn arc_alias_entry(h: u64, usage: Use) -> (Option<u64>, Use) {
    (Some(h), usage)
}

/// A duplicate one call cannot be served: the two positions, and the clause
/// the panic is built from.
///
/// `why` lives here rather than in the emitter because the rule that decides
/// it lives here: a pair's verdict and the sentence explaining it are the same
/// fact, and splitting them is how a message comes to describe a rule that has
/// since moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Aliased {
    pub a: u32,
    pub b: u32,
    /// Reads as "`{method}` {why}, as `{a}` and `{b}`".
    pub why: &'static str,
}

/// Refuse a `Box`- or `Arc`-held handle that one call acquires twice, where
/// either acquisition is **exclusive** ([`Use::exclusive`]).
///
/// `entries` is one [`alias_entry`]/[`arc_alias_entry`] per acquisition, in
/// the member's own order (the receiver first, where there is one) — one entry
/// per *element* where a parameter is a list of handles, so the count is a
/// runtime one, and the caller maps a position back to a name.
///
/// **Why exclusive-only, where the locked refusal takes every duplicate.** One
/// principle, three containers: refuse exactly the duplicates the container
/// cannot serve. An `RwLock` can serve none — a re-entrant `read` may deadlock
/// against a queued writer, and one guard cannot feed two argument positions,
/// which is why [`lock_plan`] refuses a repeat outright. A bare `Box` and an
/// `Arc` both serve a shared pair perfectly well: two `&T` to one object is
/// ordinary Rust and says something (`compare(&d, &d)`), and for `Arc` it is
/// the whole point of the model. What neither can serve is a pair including an
/// exclusive access. Two references claiming exclusive and shared access to
/// one allocation is undefined behaviour, not a hang; and a take frees the
/// object under whatever else the call was going to do with it, or twice if
/// the other acquisition is a take as well. [`frozen_ref`] is what makes the
/// frozen case invisible downstream: it hands out a `&T` without touching the
/// count, so `Arc::try_unwrap` would see nothing and succeed.
///
/// So this refuses those pairs and nothing else. A caller who learned the
/// locked rule and guesses wrong here is merely surprised that something
/// works; one who learned this rule and guesses wrong there meets a panic
/// naming both parameters.
pub fn alias_check(entries: &[(Option<u64>, Use)]) -> Result<(), Aliased> {
    for (i, (a, a_use)) in entries.iter().enumerate() {
        let Some(a) = a else { continue };
        for (j, (b, b_use)) in entries.iter().enumerate().skip(i + 1) {
            if Some(a) != b.as_ref() || !(a_use.exclusive() || b_use.exclusive()) {
                continue;
            }
            let why = if *a_use == Use::Consume || *b_use == Use::Consume {
                "was given one handle twice and consumes it in one of them"
            } else {
                "borrows one handle twice, and at least one of them mutably"
            };
            return Err(Aliased { a: i as u32, b: j as u32, why });
        }
    }
    Ok(())
}

// ---- Actor: Box<T>, owned by an executor ----
//
// The same raw Box as Confined — an actor body has exclusive access by
// construction, because its executor is the only place actor bodies run. What
// differs is who can free it. Every other model's handle is reachable from the
// Dart object that owns it, so a `NativeFinalizer` can pass the pointer back.
// An actor's pointer reaches Rust only inside a *call*, and a handle nobody
// disposed makes no more calls. So the executor is told, at construction, how
// to drop what it holds; the finalizer then only needs the host id.

/// Box `value` and arm its executor's teardown, so an undisposed handle is
/// still reclaimed when the host is reaped.
///
/// `type_name` is the actor type, used only for the leak diagnostic — reaching
/// the teardown at all proves `dispose()` was never called, and the type name
/// is the only identity available to say whose.
///
/// Runs on the host thread: the generated constructor body is itself a job on
/// that executor.
pub fn actor_new<T: 'static>(value: T, type_name: &'static str) -> u64 {
    let h = Box::into_raw(Box::new(value)) as u64;
    #[cfg(not(target_family = "wasm"))]
    crate::actor::arm_teardown(move || {
        // `finalize_scope`, not a bare drop: this path runs only when nobody
        // disposed the handle, so any channel the object still holds must
        // terminate as *leaked* naming `type_name`, the way an undisposed
        // opaque handle's does. A bare drop would close them silently, which
        // reads to the app exactly like an orderly shutdown.
        crate::stream::finalize_scope(type_name, || unsafe { actor_drop::<T>(h) })
    });
    #[cfg(target_family = "wasm")]
    let _ = type_name; // the web executor is a Worker; termination reclaims it
    h
}

/// # Safety
/// `h` must be a live handle created by `actor_new::<T>`; never used after.
///
/// Disarms the executor's teardown first, so a `Reap` arriving behind an
/// orderly drop finds an empty slot instead of freeing the same Box twice.
pub unsafe fn actor_drop<T>(h: u64) {
    #[cfg(not(target_family = "wasm"))]
    crate::actor::disarm_teardown();
    drop(Box::from_raw(h as *mut T));
}

// ---- Frozen: Arc<T> ----

/// `Send + Sync`: `Arc<T>: Send` needs both, `&T` reaches concurrent callers
/// through [`frozen_ref`], and the last clone can drop on a pool thread.
///
/// ```compile_fail,E0277
/// frustrate::handle::frozen_new(std::cell::Cell::new(1i32));
/// ```
pub fn frozen_new<T: Send + Sync + 'static>(value: T) -> u64 {
    Arc::into_raw(Arc::new(value)) as u64
}

/// # Safety
/// `h` must be a live handle created by `frozen_new::<T>`.
pub unsafe fn frozen_ref<'a, T>(h: u64) -> &'a T {
    &*(h as *const T)
}

/// Clone the Arc behind a frozen handle (for async execution, where the
/// borrow must outlive the dispatch call).
///
/// # Safety
/// `h` must be a live handle created by `frozen_new::<T>`.
pub unsafe fn frozen_arc<T>(h: u64) -> Arc<T> {
    let raw = h as *const T;
    Arc::increment_strong_count(raw);
    Arc::from_raw(raw)
}

/// # Safety
/// `h` must be a live handle created by `frozen_new::<T>`; never used after.
pub unsafe fn frozen_drop<T>(h: u64) {
    drop(Arc::from_raw(h as *const T));
}

// ---- Locked: Arc<RwLock<T>> ----

/// `Send + Sync`: `RwLock<T>` is `Sync` only when `T` is `Send + Sync`
/// (concurrent readers each hold a `&T`), which is what `Arc<RwLock<T>>: Send`
/// needs to cross to the pool.
///
/// ```compile_fail,E0277
/// frustrate::handle::locked_new(std::cell::Cell::new(1i32));
/// ```
pub fn locked_new<T: Send + Sync + 'static>(value: T) -> u64 {
    Arc::into_raw(Arc::new(RwLock::new(value))) as u64
}

/// # Safety
/// `h` must be a live handle created by `locked_new::<T>`.
pub unsafe fn locked_arc<T>(h: u64) -> Arc<RwLock<T>> {
    let raw = h as *const RwLock<T>;
    Arc::increment_strong_count(raw);
    Arc::from_raw(raw)
}

/// Borrow the lock behind a locked handle for the duration of a sync call.
///
/// Carries no platform bound, because [`LockedCell`]'s release cannot wait:
/// the try-lock contract is on every target, and the only native-only
/// synchronous path is the blocking one. What keeps *that* off wasm is
/// structural rather than a rule two codegen passes must keep agreeing on —
/// `blocking_read` and `blocking_write` do not exist there, so a `block`
/// member emitted into a wasm build fails to compile, naming the method.
/// FR0008 is the checker's backstop ahead of it.
///
/// # Safety
/// `h` must be a live handle created by `locked_new::<T>`.
pub unsafe fn locked_ref<'a, T>(h: u64) -> &'a RwLock<T> {
    &*(h as *const RwLock<T>)
}

/// Order the locked handles one call is about to acquire, and refuse a
/// duplicate.
///
/// `entries` is `(handle id, position)`, one per locked acquisition in the
/// member's own order — one entry per *element* where a parameter is a list of
/// handles, so the count is a runtime one. On `Ok` the slice is sorted by
/// handle id ascending and the caller acquires guards in that order. On
/// `Err((i, j))` two entries named the same object; the pair is the *original*
/// positions, so the caller can name them.
///
/// A **consumed** locked handle is not here at all: it takes no guard, so it
/// has no place in an acquisition order, and the duplicate it could make is
/// refused by [`alias_check`], which every consuming member joins. Counting
/// one here would be a second answer to the arity this function decides.
///
/// **Why ascending handle id is a sound global order.** A locked handle id is
/// the address of the `Arc<RwLock<T>>` allocation (`locked_new`), so distinct
/// live objects have distinct ids and the numeric order over them is total.
/// Every acquisition of two or more locks goes through this function, so two
/// calls that share any locks acquire the shared ones in the same relative
/// order — no cycle can form, and no pair of calls can hold one another's
/// next lock. The order is stable for as long as it matters: each caller has
/// already cloned an `Arc` for every entry (`locked_arc`), so every object in
/// the plan stays allocated, at that address, until the call releases it. An
/// id reused by a later allocation cannot reorder anything, because the two
/// calls never hold that address at the same time.
///
/// Duplicates are refused rather than deduplicated. Two `&mut` parameters
/// naming one object would alias; two shared ones would re-enter a reader on a
/// thread that may already have a writer queued, which std documents as a
/// possible deadlock; and handing both parameters one guard would mean
/// choosing argument expressions at runtime, which the generated code cannot
/// do. Refusing is the honest answer, and relaxing the shared/shared case
/// later is a compatible change.
pub fn lock_plan(entries: &mut [(u64, u32)]) -> Result<(), (u32, u32)> {
    entries.sort_unstable_by_key(|(id, _)| *id);
    for pair in entries.windows(2) {
        if pair[0].0 == pair[1].0 {
            let (a, b) = (pair[0].1, pair[1].1);
            return Err(if a <= b { (a, b) } else { (b, a) });
        }
    }
    Ok(())
}

/// # Safety
/// `h` must be a live handle created by `locked_new::<T>`; never used after.
pub unsafe fn locked_drop<T>(h: u64) {
    drop(Arc::from_raw(h as *const RwLock<T>));
}

// ---- Taking: a call that consumes the object behind a handle ----
//
// The Dart side gives the object up *before* the call is issued (`take()`
// mints a `Consumed<T>`, and the generated encoder spends it), so nothing
// there will free it and nothing there can name it again. What is left is to
// move it out of its container, which each container does differently and only
// `Box` does unconditionally.
//
// **Own first, unwrap second, so a refused call leaks nothing.** The
// generated glue adopts every consumed handle in the call — [`confined_adopt`]
// or one of the two `*_adopt`s, all infallible, all taking what Dart held —
// and only then unwraps the two that can refuse. A failure part-way therefore
// leaves every object in this frame's locals, and they are released when it
// returns. The alternative order leaks: a raw nobody has adopted is a pointer
// Dart has already forgotten.
//
// **A refused call releases what it was given, and that is the contract, not
// a compromise.** Spending has to happen at issue rather than at response,
// because on the async path a call made in between would otherwise clone an
// `Arc` out of an allocation the take had already freed. So a spent token's
// object is gone whether the call ran or not, and handing it back on the
// failure path is not available: a returned handle is one Dart must dispose,
// which is what FR0035 refuses on every other failure path too.
//
// **Why `try_unwrap` failing means what it says.** Every concurrent holder of
// a frozen or locked object is a clone made by [`frozen_arc`]/[`locked_arc`]
// in some dispatch prelude, so `Err` is exactly "a call on this object has not
// finished". The uncounted [`frozen_ref`]/[`locked_ref`] derefs cannot hide
// one: those live only inside a sync body, and a sync body cannot overlap
// another call in its isolate — Dart never runs on the Rust stack (callback
// invocation asserts a pool or actor context, and sink deliveries are
// deferred) — while handles do not cross isolates at all, since a
// `Finalizable` cannot be sent. `Ok` therefore also implies the lock's waiter
// queue is empty: a waiter would be inside a call holding a clone.

/// Adopt the object behind a confined or actor handle: this frame owns the
/// `Box`, and the object with it.
///
/// Returns the `Box` rather than the value, so that `self: Box<Self>` has
/// something to hand the body — and so that a *dispatched* call captures an
/// owner rather than a `u64` and cannot leak one it never polls. `self` reads
/// through it (`*box_x`), which costs nothing.
///
/// Unlike the other two, nothing can refuse: a confined object has one isolate
/// and synchronous calls, and an actor's consuming call queues behind
/// everything already sent. There is no `*_take` beside this one. Resident's
/// consume is [`resident_adopt`], which can refuse — see [`confined_ref`].
///
/// # Safety
/// `h` must be a live handle created by `confined_new::<T>`/`actor_new::<T>`
/// whose Dart side has given it up; never used after.
pub unsafe fn confined_adopt<T>(h: u64) -> Box<T> {
    Box::from_raw(h as *mut T)
}

/// Disarm the current actor executor's teardown, for a consuming member.
///
/// An actor's teardown is armed at construction ([`actor_new`]) so that an
/// undisposed handle is still reclaimed when the host is reaped. A consuming
/// member takes the object out from under it, so the slot has to be emptied
/// first or a `Reap` arriving behind the call frees the same `Box` twice —
/// the same ordering [`actor_drop`] keeps, split out because a take is not a
/// drop.
pub fn actor_disarm() {
    #[cfg(not(target_family = "wasm"))]
    crate::actor::disarm_teardown();
}

/// Adopt the `Arc` behind a frozen handle: takes the strong count Dart held,
/// so this frame owns the object and cannot leak it however the call ends.
/// The `Arc` half of [`confined_adopt`].
///
/// **No increment**, unlike [`frozen_arc`] — the Dart wrapper is spent and
/// will never drop its own count.
///
/// # Safety
/// `h` must be a live handle created by `frozen_new::<T>` whose Dart side has
/// given it up; never used after.
pub unsafe fn frozen_adopt<T>(h: u64) -> Arc<T> {
    Arc::from_raw(h as *const T)
}

/// [`frozen_adopt`] for a locked handle.
///
/// # Safety
/// As [`frozen_adopt`], with `locked_new`.
pub unsafe fn locked_adopt<T>(h: u64) -> Arc<RwLock<T>> {
    Arc::from_raw(h as *const RwLock<T>)
}

/// A take found a call on the object still in flight. A named type rather
/// than `()`, so the `Result` says what its failure means — and a unit one,
/// because there is nothing to carry: which object and which member are the
/// caller's own facts, and the generated glue has both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StillInUse;

/// Unwrap an adopted frozen `Arc`. `Err` means a call on this object is still
/// in flight; the object is released here, because this frame held the last
/// count Dart had.
pub fn frozen_take<T>(arc: Arc<T>) -> Result<T, StillInUse> {
    Arc::try_unwrap(arc).map_err(|_| StillInUse)
}

/// Unwrap an adopted locked `Arc`, out of its lock.
///
/// **The one synchronous path to a locked object that acquires nothing at
/// all**, which is why it needs no contention contract: `try_unwrap` is a
/// compare-exchange, [`LockedCell::into_inner`] moves the value out of an
/// `UnsafeCell`, and the lock has no `Drop` impl — the same fact
/// [`locked_drop`] rests on. There is no guard to take and none to release, so
/// there is no contention to have a policy about (FR0007 does not apply, and
/// an `on_contention` written here is FR0010).
pub fn locked_take<T>(arc: Arc<RwLock<T>>) -> Result<T, StillInUse> {
    frozen_take(arc).map(RwLock::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ascending id, with each entry's original parameter index following it.
    #[test]
    fn lock_plan_sorts_by_handle_id() {
        let mut plan = [(9u64, 0u32), (3, 1), (7, 2)];
        assert_eq!(lock_plan(&mut plan), Ok(()));
        assert_eq!(plan, [(3, 1), (7, 2), (9, 0)]);
    }

    /// A duplicate that is not adjacent in the caller's order still has to be
    /// found — which is what sorting first buys. The reported pair is the two
    /// *original* parameter indices, because those are what the panic names;
    /// returning sorted positions would name the wrong parameters.
    #[test]
    fn lock_plan_reports_original_indices_of_a_duplicate() {
        let mut plan = [(5u64, 0u32), (9, 1), (5, 2)];
        assert_eq!(lock_plan(&mut plan), Err((0, 2)));
    }

    #[test]
    fn lock_plan_accepts_all_distinct() {
        let mut plan = [(1u64, 0u32), (2, 1)];
        assert_eq!(lock_plan(&mut plan), Ok(()));
    }

    /// Fewer than two entries can neither invert nor alias.
    #[test]
    fn lock_plan_is_trivial_below_two_entries() {
        assert_eq!(lock_plan(&mut []), Ok(()));
        assert_eq!(lock_plan(&mut [(4u64, 0u32)]), Ok(()));
    }

    #[test]
    fn alias_check_allows_distinct_handles() {
        let e = [alias_entry::<i32>(1, Use::Mut), alias_entry::<i32>(2, Use::Shared)];
        assert_eq!(alias_check(&e), Ok(()));
    }

    /// Two shared borrows of one object are ordinary Rust and stay legal.
    #[test]
    fn alias_check_allows_a_shared_duplicate() {
        let e = [alias_entry::<i32>(7, Use::Shared), alias_entry::<i32>(7, Use::Shared)];
        assert_eq!(alias_check(&e), Ok(()));
    }

    #[test]
    fn alias_check_refuses_a_duplicate_involving_mut() {
        for (a, b) in [
            (Use::Mut, Use::Shared),
            (Use::Shared, Use::Mut),
            (Use::Mut, Use::Mut),
        ] {
            let e = [alias_entry::<i32>(7, a), alias_entry::<i32>(7, b)];
            let d = alias_check(&e).expect_err("{a:?} beside {b:?}");
            assert_eq!((d.a, d.b), (0, 1));
            assert!(d.why.contains("mutably"), "{}", d.why);
        }
    }

    /// A shared duplicate is fine even when an unrelated `&mut` is present:
    /// the mut has to be one of the *duplicated* pair.
    #[test]
    fn alias_check_pairs_the_mut_with_the_duplicate() {
        let e = [
            alias_entry::<i32>(7, Use::Shared),
            alias_entry::<i32>(7, Use::Shared),
            alias_entry::<i32>(9, Use::Mut),
        ];
        assert_eq!(alias_check(&e), Ok(()));
    }

    /// Every zero-sized value shares one dangling address, so two *distinct*
    /// ZST objects carry the same handle. They have no bytes to alias, so the
    /// check must not read that collision as a duplicate.
    #[test]
    fn alias_check_ignores_zero_sized_types() {
        struct Zst;
        let a = confined_new(Zst);
        let b = confined_new(Zst);
        assert_eq!(a, b, "the premise: distinct ZST handles collide");
        let e = [alias_entry::<Zst>(a, Use::Consume), alias_entry::<Zst>(b, Use::Consume)];
        assert_eq!(alias_check(&e), Ok(()));
        unsafe {
            confined_drop::<Zst>(a);
            confined_drop::<Zst>(b);
        }
    }

    #[test]
    fn confined_lifecycle() {
        let h = confined_new(String::from("hi"));
        unsafe {
            assert_eq!(confined_ref::<String>(h), "hi");
            confined_mut::<String>(h).push('!');
            assert_eq!(confined_ref::<String>(h), "hi!");
            confined_drop::<String>(h);
        }
    }

    /// The Actor exemption, at the signature: `actor_new` takes no `Send`
    /// bound, so a deliberately thread-affine type still mints a handle. An
    /// actor's decode, body and drop all run on its own executor, so there is
    /// no transfer to license. `Rc<Cell<i64>>` defeats both `Send` and `Sync`,
    /// so either bound leaking onto this path fails here.
    #[test]
    fn actor_new_takes_a_thread_affine_type() {
        struct Affine(std::rc::Rc<std::cell::Cell<i64>>);
        let h = actor_new(Affine(std::rc::Rc::new(std::cell::Cell::new(7))), "Affine");
        unsafe {
            assert_eq!(confined_ref::<Affine>(h).0.get(), 7);
            actor_drop::<Affine>(h);
        }
    }

    #[test]
    fn frozen_arc_keeps_value_alive_after_drop() {
        let h = frozen_new(vec![1, 2, 3]);
        let arc = unsafe { frozen_arc::<Vec<i32>>(h) };
        unsafe { frozen_drop::<Vec<i32>>(h) };
        assert_eq!(*arc, vec![1, 2, 3]);
    }

    /// `try_*` rather than `write().await`, so the test needs no executor. It
    /// is also the honest shape for this lock now: acquisition suspends a task,
    /// and there is no task here — a `block_on` in a unit test would be
    /// measuring the harness.
    #[test]
    fn locked_lifecycle() {
        let h = locked_new(0i64);
        {
            let lock = unsafe { locked_ref::<i64>(h) };
            *lock.try_write().expect("uncontended") += 5;
            assert_eq!(*lock.try_read().expect("uncontended"), 5);
        }
        unsafe { locked_drop::<i64>(h) };
    }

    /// A consume is an exclusive access, so a duplicate involving one is
    /// refused however the other half is spelled — and two consumes of one
    /// handle, which would free it twice, most of all.
    #[test]
    fn alias_check_refuses_a_duplicate_involving_a_consume() {
        for other in [Use::Shared, Use::Mut, Use::Consume] {
            let e = [alias_entry::<i32>(7, Use::Consume), alias_entry::<i32>(7, other)];
            let d = alias_check(&e).expect_err("consume beside {other:?}");
            assert_eq!((d.a, d.b), (0, 1));
            assert!(d.why.contains("consumes"), "{}", d.why);
        }
    }

    /// An `Arc` allocates a header even for a zero-sized `T`, so two frozen
    /// ZST objects have distinct ids and there is nothing to forgive. The
    /// premise is measured, not assumed: if it ever stopped holding, this
    /// fails rather than silently letting a real duplicate through.
    #[test]
    fn arc_entries_do_not_collapse_zero_sized_types() {
        struct Zst;
        let a = frozen_new(Zst);
        let b = frozen_new(Zst);
        assert_ne!(a, b, "the premise: distinct frozen ZST handles differ");
        let e = [arc_alias_entry(a, Use::Consume), arc_alias_entry(b, Use::Consume)];
        assert_eq!(alias_check(&e), Ok(()));
        unsafe {
            frozen_drop::<Zst>(a);
            frozen_drop::<Zst>(b);
        }
    }

    /// Adopting a confined handle hands back the `Box`, so the caller owns
    /// the object outright — `self` reads through it and `self: Box<Self>`
    /// takes it as it is.
    #[test]
    fn confined_adopt_hands_back_the_box() {
        let h = confined_new(String::from("hi"));
        let b: Box<String> = unsafe { confined_adopt::<String>(h) };
        assert_eq!(*b, "hi");
    }

    /// Adopting takes Dart's count rather than adding one, so an uncontended
    /// take succeeds and the object is the caller's.
    #[test]
    fn frozen_take_succeeds_when_nothing_else_holds_a_clone() {
        let h = frozen_new(vec![1i32, 2, 3]);
        let arc = unsafe { frozen_adopt::<Vec<i32>>(h) };
        assert_eq!(frozen_take(arc), Ok(vec![1, 2, 3]));
    }

    /// A call still in flight holds a clone (`frozen_arc`), and that is
    /// exactly what the take refuses on — deterministically, with the object
    /// released here and surviving until the in-flight call lets go.
    #[test]
    fn frozen_take_refuses_while_a_clone_is_live_and_releases_the_object() {
        let h = frozen_new(vec![1i32, 2, 3]);
        let in_flight = unsafe { frozen_arc::<Vec<i32>>(h) };
        let arc = unsafe { frozen_adopt::<Vec<i32>>(h) };
        assert_eq!(frozen_take(arc), Err(StillInUse));
        // The refusal dropped this frame's count; the in-flight call still
        // reads the object, and it dies with that clone.
        assert_eq!(*in_flight, vec![1, 2, 3]);
        assert_eq!(Arc::strong_count(&in_flight), 1);
    }

    /// `into_inner` after the unwrap, and no guard anywhere: this is the
    /// portable path a consuming member on a locked type uses.
    #[test]
    fn locked_take_unwraps_the_lock_without_a_guard() {
        let h = locked_new(41i64);
        let arc = unsafe { locked_adopt::<i64>(h) };
        assert_eq!(locked_take(arc), Ok(41));
    }

    #[test]
    fn locked_take_refuses_while_a_clone_is_live() {
        let h = locked_new(41i64);
        let in_flight = unsafe { locked_arc::<i64>(h) };
        let arc = unsafe { locked_adopt::<i64>(h) };
        assert_eq!(locked_take(arc), Err(StillInUse));
        assert_eq!(*in_flight.try_read().expect("uncontended"), 41);
    }

    /// The property the whole `locked` model rests on after the swap: a guard
    /// can cross a thread, so a future holding one is `Send` and
    /// `executor::spawn` accepts it. `std`'s guards are `!Send`, which is what
    /// used to refuse every `async fn` on a locked type.
    #[test]
    fn locked_guards_are_send() {
        fn assert_send<T: Send>() {}
        assert_send::<RwLockReadGuard<'static, i64>>();
        assert_send::<RwLockWriteGuard<'static, i64>>();
    }
}
