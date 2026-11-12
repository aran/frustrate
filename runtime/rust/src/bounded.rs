//! The bounded lock: the one lock type frustrate's registries take, and the
//! only lock a `#[bridge(no_block)]` member may reach.
//!
//! A registry the Dart main thread can touch needs a lock, and a lock can make
//! its taker wait for as long as another thread holds it. That wait is bounded
//! only if every critical section under the lock is: a section that sleeps,
//! takes a parking lock, or calls code nobody can see stalls every thread
//! queued behind it. So the bound is a property of all of the lock's sections
//! at once, and this type is shaped so that property can be checked:
//!
//! - **Sections are closures.** [`Lock::with`] is the only way in; there is no
//!   guard to hold across arbitrary code, so a section is exactly one call to
//!   the closure.
//! - **The acquire has an identity.** It is one non-generic function, private
//!   to this module, so every section in the program is a caller of it, and
//!   its path (`frustrate::bounded::Raw::acquire`) is its name in the
//!   no_block check. `bazel/no_block_check`'s native analysis does two things
//!   with it: it
//!   does not count the acquire's own wait against a root, and it checks every
//!   caller of the acquire in the program — every section — reaches no wait, no
//!   slow function, no external it cannot vouch for, and no acquire. That is
//!   the proof that the wait the acquire stands for is bounded; the analysis
//!   refuses the exemption when it cannot complete it.
//!
//! What the analysis cannot prove, and what the sections keep by convention:
//! that a section's own loops terminate quickly. Every section here is a
//! container operation on a container this runtime owns.
//!
//! # Per target
//!
//! A `std::sync::Mutex` wherever the target has no wait instruction, and
//! [`crate::spin::SpinLock`] on wasm with `+atomics`, where a parking lock
//! traps on the browser main thread under contention (`spin.rs` has the rules
//! a spin lock's sections keep).
//!
//! The key is `target_feature = "atomics"`, not `feature = "wasm-threads"`:
//! the hazard is whether a wait instruction exists, which `+atomics` decides.
//! The two agree on every configuration that ships, and disagree on the
//! threaded no_block check build (`+atomics` with the feature off), which
//! must check the lock the threaded build runs.
//!
//! Poisoning is recovered rather than propagated: no section leaves its
//! container torn when it unwinds (each is one container operation), and the
//! callers include `Drop`s and `extern "C"` frames that must not panic.

use std::cell::UnsafeCell;

/// A lock over `T` whose critical sections are closures.
pub(crate) struct Lock<T> {
    raw: Raw,
    value: UnsafeCell<T>,
}

// Safety: `value` is reached only inside `with`, while `raw` is held, so at
// most one thread has it at a time; `T: Send` because that thread may be any.
unsafe impl<T: Send> Sync for Lock<T> {}

impl<T> Lock<T> {
    /// Build one, in a `const` context, so a `static` has no initialiser to
    /// get stuck in (`spin.rs`, on initialising once).
    pub(crate) const fn new(value: T) -> Self {
        Lock {
            raw: Raw::new(),
            value: UnsafeCell::new(value),
        }
    }

    /// Run `f` under the lock.
    ///
    /// `f` is a critical section: keep it to a container operation on a
    /// container this runtime owns. It may not wait, call user code, call an
    /// external function the no_block check cannot vouch for, take a lock
    /// (this one included, which would deadlock), or drop anything whose
    /// `Drop` does: bind what a container hands back and let it die after
    /// `with` returns. The native check build proves the first four for every
    /// section in the program.
    pub(crate) fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        let _held = self.raw.acquire();
        // Safety: `_held` is the lock, held until this frame ends, unwinding
        // included.
        f(unsafe { &mut *self.value.get() })
    }

    /// Whether the lock is held right now. Test-only: a test asserting that
    /// an unwinding section released the lock reads this rather than
    /// acquiring, which would hang on the bug it tests for.
    #[cfg(test)]
    pub(crate) fn is_held(&self) -> bool {
        self.raw.is_held()
    }
}

/// A value a registry holds whose `Drop` may do more than free memory (a
/// channel end, a waker, an `Arc` of something that holds one). Dropping one
/// does nothing: a section that unwinds while one is in its hands leaks it
/// rather than running its destructor under the lock, and a section that takes
/// one out of its container hands it back to be [`Outside::into_inner`]ed
/// after `with` returns. That is the "nothing drops inside a section" rule
/// kept on the unwind path too, where no `let` binding can reach.
#[must_use = "an `Outside` dropped without `into_inner` leaks its value"]
pub(crate) struct Outside<T>(std::mem::ManuallyDrop<T>);

impl<T> Outside<T> {
    pub(crate) fn new(value: T) -> Self {
        Outside(std::mem::ManuallyDrop::new(value))
    }

    /// The value, to use or drop where no lock is held.
    pub(crate) fn into_inner(self) -> T {
        std::mem::ManuallyDrop::into_inner(self.0)
    }
}

impl<T> std::ops::Deref for Outside<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

#[cfg(not(all(target_family = "wasm", target_feature = "atomics")))]
struct Raw(std::sync::Mutex<()>);

#[cfg(not(all(target_family = "wasm", target_feature = "atomics")))]
impl Raw {
    const fn new() -> Self {
        Raw(std::sync::Mutex::new(()))
    }

    /// Every section's entry. Never inlined, so each section is a call to it
    /// at any opt-level; the native analysis knows it by its path.
    #[inline(never)]
    fn acquire(&self) -> std::sync::MutexGuard<'_, ()> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[cfg(test)]
    fn is_held(&self) -> bool {
        matches!(self.0.try_lock(), Err(std::sync::TryLockError::WouldBlock))
    }
}

#[cfg(all(target_family = "wasm", target_feature = "atomics"))]
struct Raw(crate::spin::SpinLock<()>);

#[cfg(all(target_family = "wasm", target_feature = "atomics"))]
impl Raw {
    const fn new() -> Self {
        Raw(crate::spin::SpinLock::new(()))
    }

    #[inline(never)]
    fn acquire(&self) -> crate::spin::Held<'_, ()> {
        self.0.acquire()
    }

    #[cfg(test)]
    fn is_held(&self) -> bool {
        self.0.is_held()
    }
}

/// A section that sleeps, compiled only into the no_block check's red fixture
/// (//tests/bazel_rules/native_red), whose build alone sets this cfg: the
/// fence that a section the member never runs still decides its verdict.
#[cfg(frustrate_dirty_section_fixture)]
#[unsafe(no_mangle)]
pub extern "C" fn frustrate_dirty_section_fixture() {
    static FIXTURE: Lock<()> = Lock::new(());
    FIXTURE.with(|_| std::thread::sleep(std::time::Duration::from_millis(1)));
}

#[cfg(test)]
mod tests {
    use super::*;

    static CONST_BUILT: Lock<u32> = Lock::new(7);

    #[test]
    fn it_is_const_constructible_as_a_static() {
        assert_eq!(CONST_BUILT.with(|v| *v), 7);
    }

    #[test]
    fn a_panicking_section_releases_the_lock() {
        let _serial = crate::post::test_lock();
        let lock = Lock::new(0u32);
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            lock.with(|v| {
                *v = 1;
                panic!("critical section blew up");
            })
        }));
        assert!(unwound.is_err());
        assert!(!lock.is_held(), "the lock outlived its unwinding section");
        assert_eq!(lock.with(|v| *v), 1, "the write before the panic stands");
    }

    #[test]
    fn it_serialises_concurrent_writers() {
        static COUNTER: Lock<u64> = Lock::new(0);
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| {
                    for _ in 0..500 {
                        COUNTER.with(|v| *v += 1);
                    }
                });
            }
        });
        assert_eq!(COUNTER.with(|v| *v), 2000);
    }
}
