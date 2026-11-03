//! The spin lock under [`crate::bounded::Lock`] on wasm with `+atomics`.
//!
//! On threaded wasm the browser main thread takes the runtime's registries,
//! and a futex-parking lock (`std::sync::Mutex`, `RwLock`, `Once`, `OnceLock`)
//! traps there under contention: `RuntimeError: Atomics.wait cannot be called
//! in this context`. Spinning is legal on every thread.
//!
//! # The trade
//!
//! A spin lock cannot trap, but it also cannot yield. Where a parking lock
//! fails loudly, this one hangs: a holder that never releases is an app freeze,
//! not an error. So the sections under it keep these rules:
//!
//! * **Critical sections are bounded by a container, and nest only into a
//!   leaf.** Never a poll, never user code, and never a wake — a `Waker`
//!   reaches the executor's own registry, which is not a leaf. What the bound
//!   depends on is a container this runtime owns, never anything outside it.
//!   A section may nest into the allocator, because a growing
//!   `HashMap`/`Vec`/`VecDeque` allocates and on threaded wasm the global
//!   allocator is itself a spin lock (std's, in
//!   `library/std/src/sys/alloc/wasm.rs`). The allocator is a strict leaf —
//!   dlmalloc calls nothing back — so the order is always
//!   {registry, wakers, executor, queue} -> allocator and no cycle exists.
//!   Nesting into anything that is *not* a leaf is a deadlock.
//! * **Nothing may drop inside the section.** Bind whatever a container hands
//!   back and let it die outside. `HashMap::insert` returns the entry it
//!   displaced, and in `executor::spawn` that entry is an `async_task::Task`
//!   whose `Drop` synchronously calls the schedule callback — which takes the
//!   very lock being held.
//! * **The release is a `Drop` guard, never a trailing store**, so an
//!   unwinding critical section still frees the lock.
//!
//!   That guard does *not* cover a trap. Under wasm's `panic = "abort"`
//!   nothing unwinds, so a trap inside a critical section leaves the lock held
//!   for the life of the page, and the next acquisition — from the browser
//!   main thread, in `frustrate_stream_cancel` or a spawn — spins forever. A
//!   worker's trap is caught at the JS boundary, outside every Rust frame, so
//!   whatever it held stays held (`pool::frustrate_pool_replenish`).
//!
//!   The one thing these sections do that can fail is allocation, since every
//!   *mutating* section grows a container. That is already page-fatal — a wasm
//!   module at its declared maximum memory cannot continue either way
//!   (`frustrate_alloc` says so in as many words) — so the spin lock changes
//!   how the page dies, from a loud panic to a frozen tab, not whether.
//!
//! # Tests
//!
//! The unit tests run on every host build, and `cargo miri test -p frustrate
//! --lib spin` runs them under Miri with leak checking on (`tools/analyze.dart`
//! runs that pass). The module is compiled on every target, not only where a
//! `cfg` selects it, so Miri and the host tests reach it.
//!
//! # Initialising once without a `Once`
//!
//! `OnceLock`/`Once` park for the same reason a `Mutex` does. Wherever the
//! value can be `const`-built the answer is a plain `static` with no
//! initialiser to get stuck in, which is what `pool::QUEUE`, `stream::REGISTRY`
//! and `callback::INVOCATIONS` are. The one value that cannot be, the global
//! executor, lives in `once_cell::race::OnceBox` (executor.rs).

use std::cell::UnsafeCell;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, Ordering};

/// A spin lock over `T`: the bounded lock's (bounded.rs) arm on wasm with
/// `+atomics`. Compiled on every target so its tests and Miri reach it.
#[cfg_attr(
    not(all(target_family = "wasm", target_feature = "atomics")),
    allow(dead_code)
)]
pub(crate) struct SpinLock<T> {
    locked: AtomicBool,
    // Read only by `with`, which only tests call: the runtime's one lock
    // guards `()` and takes its guard.
    #[cfg_attr(not(test), allow(dead_code))]
    value: UnsafeCell<T>,
}

// Safety: every access to `value` happens under `locked`.
unsafe impl<T: Send> Sync for SpinLock<T> {}

#[cfg_attr(
    not(all(target_family = "wasm", target_feature = "atomics")),
    allow(dead_code)
)]
impl<T> SpinLock<T> {
    /// Build one, in a `const` context, so a `static` holding it has no
    /// initialiser to get stuck in. Keep it `const`.
    pub(crate) const fn new(value: T) -> Self {
        SpinLock {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    /// Run `f` under the lock.
    ///
    /// Keep `f` bounded by a container this runtime owns, with no wake, no
    /// poll, no user code and no destructor-bearing drop inside it — see the
    /// module docs on why that bound is what makes spinning legal.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        let _held = self.acquire();
        f(unsafe { &mut *self.value.get() })
    }

    /// Take the lock, released when the answer drops.
    ///
    /// Released through a guard rather than a trailing store, so an unwinding
    /// section still frees the lock.
    pub(crate) fn acquire(&self) -> Held<'_, T> {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            std::hint::spin_loop();
        }
        Held(&self.locked, PhantomData)
    }

    /// Whether the lock is held right now.
    #[cfg(test)]
    pub(crate) fn is_held(&self) -> bool {
        self.locked.load(Ordering::Acquire)
    }

    /// Whether the lock is held right now — **and** release it if it is.
    ///
    /// Test-only, and the read-and-clear is the point rather than a shortcut:
    /// a test asserting that an unwinding critical section released the lock
    /// must not leave a wedged lock behind when it fails, or the next
    /// acquisition anywhere in the same test binary spins forever instead of
    /// letting the failure be reported. Asserting by simply re-acquiring
    /// would hang on exactly the bug being tested.
    #[cfg(test)]
    pub(crate) fn take_held(&self) -> bool {
        self.locked.swap(false, Ordering::AcqRel)
    }
}

/// A held [`SpinLock`]; dropping it releases the lock.
pub(crate) struct Held<'a, T>(&'a AtomicBool, PhantomData<&'a T>);

impl<T> Drop for Held<'_, T> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `static` built in a `const` context — the property `pool::QUEUE`
    /// depends on. If `new` ever stops being `const` this stops compiling,
    /// which is the point.
    static CONST_BUILT: SpinLock<u32> = SpinLock::new(7);

    #[test]
    fn it_is_const_constructible_as_a_static() {
        assert_eq!(CONST_BUILT.with(|v| *v), 7);
    }

    #[test]
    fn with_hands_out_a_mutable_borrow() {
        let lock = SpinLock::new(vec![1, 2]);
        lock.with(|v| v.push(3));
        assert_eq!(lock.with(|v| v.clone()), vec![1, 2, 3]);
    }

    /// A trailing-store release leaves `locked` set when `f` unwinds, and the next acquisition
    /// spins forever — on the browser main thread, an app freeze.
    #[test]
    fn a_panicking_critical_section_releases_the_lock() {
        // Under `post::test_lock`: a deliberate panic reaches whatever
        // `std` hook stands at the time, and `panic`'s tests install the
        // web funnel as that hook — which would report this panic into
        // their log and fail their "exactly one report" assertions.
        let _serial = crate::post::test_lock();
        let lock = SpinLock::new(0u32);
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            lock.with(|v| {
                *v = 1;
                panic!("critical section blew up");
            })
        }));
        assert!(unwound.is_err(), "the panic must propagate");
        // Read-and-clear, never a re-acquisition: `with` would spin forever on
        // precisely the bug under test rather than reporting it.
        assert!(
            !lock.take_held(),
            "the lock was still held after its critical section unwound"
        );
        assert_eq!(lock.with(|v| *v), 1, "the write before the panic stands");
    }

    #[test]
    fn it_serialises_concurrent_writers() {
        static COUNTER: SpinLock<u64> = SpinLock::new(0);
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| {
                    for _ in 0..500 {
                        COUNTER.with(|v| *v += 1);
                    }
                });
            }
        });
        assert_eq!(COUNTER.with(|v| *v), 2000, "no update was lost");
    }
}
