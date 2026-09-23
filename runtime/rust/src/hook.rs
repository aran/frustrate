//! The user-registration slot: one process-global `Arc<T>` an embedder may
//! install, replace, or drop, read from a hot path whose answer is almost
//! always "nobody registered anything".
//!
//! It is the shape of every hook this runtime hands an embedder — the
//! async-runtime context ([`crate::runtime`]), the panic listener
//! ([`crate::panic`]) and the `log` forwarder ([`crate::logging`]) are three —
//! and the parts that are easy to get subtly wrong are the same every time:
//! the ordering that lets a reader skip the lock, dropping displaced **user
//! code** outside the critical section, and letting a second registration
//! replace the first.
//!
//! # Why it is one type
//!
//! `spin.rs` records what the second copy costs. Three hand-written copies of
//! one concurrency algorithm had already drifted: two released with a trailing
//! store and were unsound under unwinding, and only the third carried the
//! regression test. Nothing about a registration slot makes it a safer thing
//! to hand-copy — the drop-outside rule below is invisible until it hangs, and
//! a copy that inlines its own `Mutex<Option<_>>` looks correct at every
//! glance. So it is written once, tested once, and *called* from each hook.
//!
//! # The fast path
//!
//! [`Hook::get`] reads a presence flag before it goes near the lock, so a
//! reader that finds nothing registered pays one acquire load — not a lock
//! acquisition per read, across every pool worker reading in parallel. That is
//! the whole reason the flag exists; it holds no information the slot does not.
//!
//! The flag is stored `Release` **inside** the critical section, after the slot
//! is written, and read `Acquire`, so a reader that observes `true` also
//! observes the value under the lock. The two ways to observe something stale
//! are both benign and neither is silent:
//!
//! * a stale `false`, while a [`set`](Hook::set) is in flight — the reader
//!   behaves as it would have a moment earlier, which is why registration
//!   belongs at init, before the first call that reads the hook;
//! * a stale `true`, after a [`clear`](Hook::clear) — the reader takes the lock
//!   and finds `None`, so `get` answers `None` either way.
//!
//! # The displaced value dies outside the critical section
//!
//! [`set`](Hook::set) and [`clear`](Hook::clear) lift the old `Arc` out under
//! the lock and drop it after releasing. This is load-bearing, not tidiness,
//! because what is being dropped is the embedder's value and its captures, and
//! its `Drop` can do anything:
//!
//! * The realistic capture is an `Arc<tokio::runtime::Runtime>` — the natural
//!   way to keep a runtime alive for a registered context. On a Flutter hot
//!   restart this can be its last reference, and `Runtime::drop` **blocks**
//!   until the runtime has shut down. Dropped inside the section, every reader
//!   in the process spins for the length of that shutdown.
//! * A capture whose `Drop` calls back into the same hook — plausible for a
//!   teardown that unregisters itself — would spin forever against a
//!   non-reentrant lock, on one thread, with no second thread involved.
//!
//! Hence `Option::replace`/`Option::take` rather than assigning over the slot:
//! `*slot = Some(value)` drops the displaced value where it must not be
//! dropped, and the difference is one character to miss.
//!
//! # Why [`crate::spin::SpinLock`], unconditionally
//!
//! Same stance as `stream::REGISTRY` and `callback::INVOCATIONS`, and for the
//! same reason: a hook is read wherever the thing it hooks happens, and on
//! threaded web that includes the browser main thread — a bridge call enters
//! there — where a futex-parking lock traps rather than waits, "Atomics.wait
//! cannot be called in this context". The critical section here is smaller
//! than theirs: `take`/`replace` an `Option`, or clone an `Arc`. Never an
//! allocation, never user code, never a poll.
//!
//! `executor::Lock`'s per-target split — `std::sync::Mutex` off `+atomics`
//! wasm, `SpinLock` on it — was considered and is wrong *here*, for a reason
//! specific to this module rather than a preference. That split leaves the two
//! arms exercised by disjoint configurations: the host suite and Miri only ever
//! see the `Mutex` arm, and the spin arm is covered by nothing until a wasm
//! consumer exists. One implementation on every target is exactly what
//! `spin.rs` says its own existence is for, and it is what puts this algorithm
//! under `cargo miri test -p frustrate --lib hook::`.
//!
//! Two lock-free shapes were also considered. An `AtomicPtr` swap cannot hold
//! an `Arc<dyn Fn…>` (a fat pointer), and boxing to make it thin turns every
//! read into a use-after-free race with a concurrent `set` unless something
//! defers reclamation — which is what `arc-swap` is, and a dependency this
//! crate will not take for one slot (four dependencies in total, all of which
//! enter the Bazel crate_universe hub every build resolves). `OnceLock` cannot
//! express replacement at all, and parks on its cold path besides.
//!
//! `post`'s isolate routing is deliberately **not** this type, and should not
//! be folded into it: its targets are `Copy` (a port id, a raw `extern "C" fn`)
//! with no `Arc` and no destructor to keep out of a critical section, and its
//! `ROUTED` flag selects a routing *mode* rather than reporting presence.

use crate::spin::SpinLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// A slot holding at most one embedder-registered `Arc<T>`.
///
/// `const`-constructible, so a `static` has no initialiser to get stuck in —
/// the property `spin.rs` explains at length and `pool::QUEUE` paid for.
pub(crate) struct Hook<T: ?Sized> {
    /// Whether the slot holds something. See the module docs: this is the
    /// fast path, and it is redundant with `slot` on purpose.
    present: AtomicBool,
    slot: SpinLock<Option<Arc<T>>>,
}

/// The module is compiled on every target rather than cfg'd out, because a
/// primitive that exists on one target is a primitive whose next consumer
/// writes a second copy — and because compiling it everywhere is what puts it
/// under this crate's tests and Miri on every host. It needs no wasm dead-code
/// allowance: `crate::panic` and `crate::logging` both compile to wasm and
/// both call in.
impl<T: ?Sized> Hook<T> {
    /// An empty hook, in a `const` context.
    pub(crate) const fn new() -> Self {
        Hook {
            present: AtomicBool::new(false),
            slot: SpinLock::new(None),
        }
    }

    /// The registered value, or `None`.
    ///
    /// The empty case is one acquire load and no lock — the fast path the
    /// module docs describe. The `Arc` is cloned **out** of the section, so a
    /// caller runs the registered value with no lock held: user code is
    /// arbitrarily long, and a value that re-enters this hook while it runs
    /// must not find a lock its own thread is holding.
    pub(crate) fn get(&self) -> Option<Arc<T>> {
        if !self.present.load(Ordering::Acquire) {
            return None;
        }
        self.slot.with(|slot| slot.clone())
    }

    /// Register `value`, replacing whatever was there.
    ///
    /// Last-wins rather than first-wins or a panic; which of those a caller
    /// wants is the caller's argument to make, and [`crate::runtime::register`]
    /// makes it (Flutter hot restart re-runs Dart `main()` in a live process,
    /// so a second registration is a legitimate call and refusing it would pin
    /// a handle the restart has already invalidated).
    pub(crate) fn set(&self, value: Arc<T>) {
        let displaced = self.slot.with(|slot| {
            // `replace`, not `*slot = Some(value)`: assignment drops the
            // displaced value *here*, inside the section, and that value is
            // the embedder's.
            let displaced = slot.replace(value);
            // Release after the slot is written, so a reader that sees `true`
            // finds something under the lock.
            self.present.store(true, Ordering::Release);
            displaced
        });
        // Outside the section — see the module docs. Explicit rather than
        // left to the end of the function, because the whole point is *where*
        // it happens.
        drop(displaced);
    }

    /// Drop the registration; [`get`](Self::get) answers `None` again.
    ///
    /// A caller already holding an `Arc` from an earlier `get` keeps it, so a
    /// value being used right now is not yanked out from under its user.
    pub(crate) fn clear(&self) {
        let displaced = self.slot.with(|slot| {
            let displaced = slot.take();
            self.present.store(false, Ordering::Release);
            displaced
        });
        drop(displaced);
    }

    /// Drop the registration **only if it is still `expected`**; report whether
    /// this call did the clearing.
    ///
    /// The operation a reader needs to retire its *own* registration. A reader
    /// gets its value from [`get`](Self::get) and may act on it much later —
    /// long enough for a second [`set`](Self::set) to have landed in between —
    /// so a plain `clear` from that reader would silently throw away a
    /// registration it has never seen. [`crate::logging`] is the case that
    /// forces it: a pool worker discovers mid-record that the Dart consumer is
    /// gone and retires the logger, concurrently with a Flutter hot restart
    /// installing a live one. Last-wins is right for `set` and wrong here, and
    /// the difference is which side observed the other.
    ///
    /// `Arc::ptr_eq` compares the allocation, so identity is the registration
    /// event rather than the value: two `set`s of equal values are still two
    /// registrations, and clearing the second on behalf of the first would be
    /// the same bug.
    ///
    /// Like `clear`, the displaced value dies outside the critical section —
    /// see the module docs, and note that here the *caller* also holds an
    /// `Arc` to it, so this drop is virtually never the last one; the rule
    /// still applies because "virtually never" is not "never".
    pub(crate) fn clear_if(&self, expected: &Arc<T>) -> bool {
        let displaced = self.slot.with(|slot| {
            if !slot.as_ref().is_some_and(|held| Arc::ptr_eq(held, expected)) {
                return None;
            }
            let displaced = slot.take();
            self.present.store(false, Ordering::Release);
            displaced
        });
        let cleared = displaced.is_some();
        drop(displaced);
        cleared
    }

    /// Hold the slot's lock for the duration of `f`.
    ///
    /// Test-only, and it deliberately breaks the rule every real critical
    /// section here keeps (`f` blocks on a channel). It exists so a test can
    /// ask what [`get`](Self::get) does while the lock is *unavailable*, which
    /// is the only way to observe that the empty fast path never asks for it.
    #[cfg(test)]
    pub(crate) fn hold<R>(&self, f: impl FnOnce() -> R) -> R {
        self.slot.with(|_| f())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    /// A `static` built in a `const` context, which is the property a hook's
    /// real home depends on. If `new` stops being `const` this stops
    /// compiling, which is the point.
    static EMPTY: Hook<u32> = Hook::new();

    #[test]
    fn an_unregistered_hook_is_const_constructible_and_answers_none() {
        assert!(EMPTY.get().is_none());
    }

    #[test]
    fn a_registered_value_comes_back() {
        let hook: Hook<u32> = Hook::new();
        hook.set(Arc::new(7));
        assert_eq!(hook.get().map(|v| *v), Some(7));
    }

    #[test]
    fn the_last_registration_wins() {
        let hook: Hook<u32> = Hook::new();
        let first = Arc::new(1);
        let second = Arc::new(2);
        hook.set(first);
        hook.set(second.clone());
        assert!(
            hook.get().is_some_and(|v| Arc::ptr_eq(&v, &second)),
            "a second registration must replace the first, not be dropped on \
             the floor: a hot restart's whole purpose is to install a working \
             value over a dead one"
        );
    }

    #[test]
    fn clearing_empties_the_slot() {
        let hook: Hook<u32> = Hook::new();
        hook.set(Arc::new(7));
        hook.clear();
        assert!(hook.get().is_none());
    }

    /// `clear_if` retires the registration the caller was actually given, and
    /// leaves any later one alone.
    ///
    /// The sequence is a real one: a reader holds a value from `get`, acts on
    /// it, and decides it is dead — while a hot restart has already installed
    /// a live replacement. A plain `clear` there takes the app's registration
    /// away for good, and nothing would report it.
    #[test]
    fn clear_if_retires_only_the_registration_it_was_handed() {
        let hook: Hook<u32> = Hook::new();
        let stale = Arc::new(1);
        hook.set(stale.clone());
        let fresh = Arc::new(2);
        hook.set(fresh.clone());

        assert!(
            !hook.clear_if(&stale),
            "a stale registration must not report itself as the one cleared"
        );
        assert!(
            hook.get().is_some_and(|v| Arc::ptr_eq(&v, &fresh)),
            "the newer registration must survive a stale holder's retirement"
        );

        assert!(hook.clear_if(&fresh), "the current registration clears");
        assert!(hook.get().is_none());
        assert!(
            !hook.clear_if(&fresh),
            "an empty slot has nothing to clear, and says so"
        );
    }

    /// Identity is the *registration*, not the value: two `set`s of equal
    /// values are two registrations, and the first holder must not be able to
    /// retire the second on the strength of them comparing equal.
    #[test]
    fn clear_if_compares_allocations_and_not_values() {
        let hook: Hook<u32> = Hook::new();
        let first = Arc::new(7);
        hook.set(first.clone());
        hook.set(Arc::new(7));

        assert!(!hook.clear_if(&first));
        assert!(hook.get().is_some(), "the second registration is still live");
    }

    #[test]
    fn a_value_already_taken_out_survives_a_clear() {
        let hook: Hook<u32> = Hook::new();
        hook.set(Arc::new(7));
        let held = hook.get().expect("registered");
        hook.clear();
        assert_eq!(*held, 7, "the caller's Arc keeps the value alive");
    }

    /// The displaced value must drop **outside** the critical section.
    ///
    /// The value is the embedder's, so its `Drop` can re-enter this hook — a
    /// teardown that unregisters itself is the ordinary way to write one.
    /// Dropped inside the section, that re-entry spins forever against a
    /// non-reentrant lock, on this one thread, with no second thread involved.
    ///
    /// Bounded on purpose, like `executor.rs`'s
    /// `a_displaced_task_drops_outside_the_registry_lock`: the regression is a
    /// *hang*, so it runs on its own thread and is asserted on a timeout
    /// rather than wedging the suite.
    #[test]
    fn a_displaced_value_drops_outside_the_critical_section() {
        static HOOK: Hook<Unregisters> = Hook::new();

        struct Unregisters;
        impl Drop for Unregisters {
            fn drop(&mut self) {
                HOOK.clear();
            }
        }

        let (tx, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            HOOK.set(Arc::new(Unregisters));
            // Displaces the first, whose `Drop` re-enters `clear` — which in
            // turn displaces this second one, whose `Drop` clears an already
            // empty slot and stops. So the chain terminates, and it terminates
            // with the hook *empty*.
            HOOK.set(Arc::new(Unregisters));
            let _ = tx.send(());
        });

        let arrived = rx.recv_timeout(Duration::from_secs(5));
        assert!(
            arrived.is_ok(),
            "registration spun forever: the displaced value was dropped inside \
             the critical section and its `Drop` re-entered the same \
             non-reentrant lock"
        );
        worker.join().unwrap();
        assert!(
            HOOK.get().is_none(),
            "the re-entrant clear ran, and ran to completion"
        );
    }

    /// Ask `hook.get()` from another thread while this one holds the slot's
    /// lock, and report whether an answer arrived.
    ///
    /// `Err` means the probe went for the lock. The release is sent **before
    /// the caller asserts** — a probe that did take the lock is spinning right
    /// now, and only this unsticks it, so a regression is a failed assertion
    /// rather than a test binary that hangs and takes the scope's join with it.
    fn get_while_the_lock_is_held(
        hook: &Hook<u32>,
    ) -> Result<Option<Arc<u32>>, mpsc::RecvTimeoutError> {
        let (held_tx, held_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (answer_tx, answer_rx) = mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(move || {
                hook.hold(|| {
                    let _ = held_tx.send(());
                    let _ = release_rx.recv();
                });
            });
            held_rx.recv().expect("the holder never took the lock");
            scope.spawn(move || {
                let _ = answer_tx.send(hook.get());
            });
            let answer = answer_rx.recv_timeout(Duration::from_secs(2));
            let _ = release_tx.send(());
            answer
        })
    }

    /// What "the fast path is a load, not a lock" means, asked semantically:
    /// with the lock unavailable, an empty hook still answers.
    ///
    /// Both empty states, because they arrive differently — a hook nobody ever
    /// touched, and one whose registration was cleared. The second is what
    /// makes `clear` reset the fast path a *behaviour* rather than something a
    /// reader has to confirm by peeking at the flag.
    #[test]
    fn an_empty_hook_answers_without_the_lock() {
        let never_set: Hook<u32> = Hook::new();
        let answer = get_while_the_lock_is_held(&never_set);
        assert!(
            matches!(answer, Ok(None)),
            "a hook nobody registered against took the lock to say so, so \
             every read in the process now serialises: {answer:?}"
        );

        let cleared: Hook<u32> = Hook::new();
        cleared.set(Arc::new(7));
        cleared.clear();
        let answer = get_while_the_lock_is_held(&cleared);
        assert!(
            matches!(answer, Ok(None)),
            "`clear` left the fast path claiming a registration: {answer:?}"
        );
    }

    /// Proves the probe above can actually detect a lock acquisition. Without
    /// this, `Ok(None)` would also be what a `hold` that held nothing produced,
    /// and the fast-path test would pass for a `get` that locks on every call.
    #[test]
    fn the_probe_notices_a_get_that_does_take_the_lock() {
        let registered: Hook<u32> = Hook::new();
        registered.set(Arc::new(7));
        let answer = get_while_the_lock_is_held(&registered);
        assert!(
            answer.is_err(),
            "a `get` that must read the slot got past a held lock, so the \
             probe proves nothing about the empty case either: {answer:?}"
        );
    }
}
