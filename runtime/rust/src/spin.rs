//! The non-parking primitives: a lock and a one-time initialisation, used
//! everywhere either must be legal on the browser main thread.
//!
//! # Why this exists at all
//!
//! On threaded wasm the main thread takes the runtime's registries — the
//! stream cancel map, the executor's task registry, the pool's job queue — and
//! a futex-parking lock traps there under contention:
//! `RuntimeError: Atomics.wait cannot be called in this context`. That is not
//! hypothetical. `pool::QUEUE` records the case where a wedged `Once` on this
//! exact path took down every async call on the page. `std::sync::Mutex`,
//! `RwLock`, `Once` and `OnceLock` all park; spinning is legal on every thread.
//!
//! # The trade, stated plainly
//!
//! A spin lock cannot trap, but it also cannot yield. Where a parking lock
//! fails loudly, this one hangs: a holder that never releases is an app freeze,
//! not an error. Three rules follow, and each is load-bearing rather than
//! stylistic — every one of them has a real bug behind it:
//!
//! * **Critical sections are bounded by a container, and nest only into a
//!   leaf.** Never a poll, never user code, and never a wake — a `Waker`
//!   reaches the executor's own registry, which is not a leaf. Usually one
//!   container operation; the honest upper bound is "work proportional to a
//!   container this runtime owns" — `rwlock`'s grant loop pops as many waiters
//!   as the lock can satisfy, which is bounded by the calls in flight on one
//!   object. What the bound must never depend on is anything outside the
//!   runtime. Each such section may also nest into the allocator, because a
//!   growing `HashMap`/`Vec`/`VecDeque` allocates and on threaded wasm the global
//!   allocator is itself a spin lock (std's, in
//!   `library/std/src/sys/alloc/wasm.rs`, which spins for the same reason this
//!   module does). That one nesting is sanctioned because the allocator is a
//!   strict leaf — dlmalloc calls nothing back — so the order is always
//!   {registry, wakers, executor, queue} -> allocator and never the reverse,
//!   and no cycle exists. Nesting into anything that is *not* a leaf is a
//!   deadlock, not a slow path.
//! * **Nothing may drop inside the section.** Bind whatever a container hands
//!   back and let it die outside. This is load-bearing, not tidiness:
//!   `HashMap::insert` returns the entry it displaced, and in `executor::spawn`
//!   that entry is an `async_task::Task` whose `Drop` synchronously calls the
//!   schedule callback — which takes the very lock being held. A drop inside
//!   the section would spin forever with no user code involved.
//! * **The release is a `Drop` guard, never a trailing store.** An unwinding
//!   critical section must still free the lock. This was a real bug
//!   (`a_panicking_critical_section_releases_the_registry_lock` in stream.rs):
//!   a panic left `locked` set, and the next `frustrate_stream_cancel` — from
//!   the Dart main thread — spun on it forever.
//!
//!   Note precisely what that guard does *not* cover. Under wasm's
//!   `panic = "abort"` nothing unwinds, so on the configuration this module
//!   exists for the guard is inert: a **trap** inside a critical section leaves
//!   the lock held for the life of the page, and the next acquisition — from
//!   the browser main thread, in `frustrate_stream_cancel` or a spawn — spins
//!   forever. A worker's trap is caught at the JS boundary, outside every Rust
//!   frame, so whatever it held stays held (`pool::frustrate_pool_replenish`).
//!
//!   The realistic trigger is the one thing these sections do that can fail:
//!   allocation, since every *mutating* section grows a container. That is
//!   survivable only because it is already page-fatal — a wasm module at its
//!   declared maximum memory cannot continue either way
//!   (`frustrate_alloc` says so in as many words). So this changes how the page
//!   dies, from a loud attributable panic to a frozen tab, not whether. Worth
//!   knowing before trusting a spin lock further than that.
//!
//! # What checks this, and what deliberately does not
//!
//! The unit tests below run on every host build, and `cargo miri test -p
//! frustrate --lib spin` runs them under Miri with leak checking on — which is
//! the point of this module being compiled on every target rather than only
//! where a `cfg` selects it. `tools/analyze.dart` runs that pass.
//!
//! # The second primitive: initialising once without a `Once`
//!
//! `OnceLock`/`Once` park for the same reason a `Mutex` does, so the same ban
//! applies to them. Wherever the value can be `const`-built the answer is a
//! plain `static` with no initialiser to get stuck in, which is what
//! `pool::QUEUE`, `stream::REGISTRY` and `callback::INVOCATIONS` all are.
//! [`RaceOnce`] is for the one value that cannot be.
//!
//! # Why it is one type
//!
//! There were three copies of this algorithm, and they had already drifted:
//! two released with a trailing store and one with a guard, so two of them were
//! unsound under unwinding and only the third had the regression test. Sharing
//! one implementation makes that divergence impossible, collapses three
//! `unsafe impl Sync` to one, and — because this module is compiled on every
//! target rather than behind `cfg(all(target_family = "wasm", ...))` — puts the
//! algorithm where Miri and the host test suite can actually reach it. The
//! per-target choice of *whether* to use it stays at each call site.

use std::cell::UnsafeCell;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

/// A spin lock over `T`.
pub(crate) struct SpinLock<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

// Safety: every access to `value` happens under `locked`.
unsafe impl<T: Send> Sync for SpinLock<T> {}

impl<T> SpinLock<T> {
    /// Build one, in a `const` context if you like.
    ///
    /// The `const` is load-bearing, not a convenience. `pool::QUEUE` is a
    /// `static` built by this function precisely so it has *no initializer to
    /// get stuck in*: the page-killing bug it replaced was a `OnceLock` whose
    /// fallible initializer unwound through wasm under `panic = "abort"`,
    /// leaving the `Once` in RUNNING for the life of the page so that every
    /// later caller futex-waited on it. Keep this `const` and that failure
    /// mode cannot come back by someone reaching for `OnceLock`.
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
    pub(crate) fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            std::hint::spin_loop();
        }
        // Release through a guard rather than a trailing store, so an
        // unwinding `f` still frees the lock. See the module docs: the
        // trailing-store version shipped, and wedged the lock forever.
        let _guard = Release(&self.locked);
        f(unsafe { &mut *self.value.get() })
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

struct Release<'a>(&'a AtomicBool);

impl Drop for Release<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

// ------------------------------------------------- the per-target choice --

/// A lock the browser main thread may take: a plain `std::sync::Mutex`
/// wherever the target has no wait instruction, [`SpinLock`] on wasm with
/// `+atomics`, where a futex-parking lock would trap there under contention.
///
/// Its critical sections are bound by the three rules above, because on one
/// configuration they really are a spin lock's.
///
/// # Why `target_feature = "atomics"` and not `feature = "wasm-threads"`
///
/// The hazard is "does a wait instruction exist on this target", which is a
/// property of **atomics**: `+atomics` is what makes std select its futex
/// backends and what makes `memory.atomic.wait32` exist at all. Whether
/// frustrate runs a worker pool is a different question, and the cargo feature
/// is the right key for *that* (`executor::arrange_drain`, and all of pool.rs).
///
/// The two spellings agree on every configuration that ships — threaded web
/// sets both, single-threaded web and native set neither — but they disagree
/// on a fourth that is built: `+atomics` with the feature **off** is the
/// `--cfg frustrate_block_check` artifact (lib.rs, "block-check export
/// gating"). Keyed on the feature, that artifact took the `Mutex` arm while
/// std's futex backends were live, so every [`crate::executor::Executor::spawn`]
/// reached `memory.atomic.wait32` and a `#[bridge(no_block)]` claim on an
/// **async** member was unsatisfiable — on a path present in neither
/// production build. Keyed on atomics, the check artifact and threaded web
/// make the same choice, which is what makes the artifact speak for production
/// at all.
///
/// Both arms are a thin wrapper over a primitive that is tested on its own:
/// the `Mutex` by std, the spin lock by this module's unit tests, which run on
/// every host build rather than only where this `cfg` selects them. So the
/// host tests exercise the `Mutex` arm; what carries the spin arm is that the
/// critical sections are the same source.
///
/// One type rather than one per call site, for the reason the module docs give
/// for `SpinLock`: the `cfg`-key argument is subtle enough that a second copy
/// would drift.
#[cfg(not(all(target_family = "wasm", target_feature = "atomics")))]
pub(crate) struct Lock<T>(std::sync::Mutex<T>);

#[cfg(not(all(target_family = "wasm", target_feature = "atomics")))]
impl<T> Lock<T> {
    pub(crate) fn new(value: T) -> Self {
        Lock(std::sync::Mutex::new(value))
    }
    pub(crate) fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        // Poisoning is recovered rather than propagated. Every holder either
        // catches the panic before the guarded state can go logically wrong
        // (the executor's `drain_one`) or is a `Drop` that must not panic
        // itself (a lock guard's release).
        let mut guard = self.0.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut guard)
    }
}

#[cfg(all(target_family = "wasm", target_feature = "atomics"))]
pub(crate) struct Lock<T>(SpinLock<T>);

#[cfg(all(target_family = "wasm", target_feature = "atomics"))]
impl<T> Lock<T> {
    pub(crate) fn new(value: T) -> Self {
        Lock(SpinLock::new(value))
    }
    pub(crate) fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        self.0.with(f)
    }
}

// -------------------------------------------------------- one-time init --

/// A `T` published exactly once, and never by parking.
///
/// [`std::sync::OnceLock`] is the obvious type and is the wrong one here for
/// the same reason `Mutex` is: `get_or_init` parks the loser of an
/// initialisation race in `Once`'s futex, and on `+atomics` wasm that is
/// `memory.atomic.wait32` — a trap on the browser main thread rather than a
/// wait. `pool::QUEUE` records the page that died to it. The one value it is
/// for is the global [`crate::executor::Executor`], which owns boxed closures
/// and so cannot be `const`-built.
///
/// # The trade
///
/// The initialiser may run **more than once**. Threads that arrive at an
/// unpublished cell together each build a value, one compare-exchange decides
/// which is published, and the losers drop theirs on the spot. So this is not
/// a `Once` in the "runs exactly once" sense, and it is only correct where
/// construction is side-effect-free and cheap — building an `Executor` is
/// three allocations and no observable effect. The fast path is exactly
/// `OnceLock`'s, one acquire load, and no path waits on any thread.
///
/// Compiled on every target although only `+atomics` wasm selects it
/// (`executor::ExecCell`), for the reason the module docs give for `SpinLock`:
/// that is what puts the algorithm under these unit tests and under
/// `cargo miri test -p frustrate --lib spin`. Hence the conditional `dead_code`
/// allowance, conditional so dead-code checking stays live where it is used.
#[cfg_attr(
    not(all(target_family = "wasm", target_feature = "atomics")),
    allow(dead_code)
)]
pub(crate) struct RaceOnce<T> {
    /// Null until some thread publishes; after that, a `Box<T>` this cell owns
    /// and never replaces.
    ptr: AtomicPtr<T>,
    /// Suppresses the auto `Send`/`Sync`, which `AtomicPtr<T>` would otherwise
    /// hand out unconditionally, so the two below are the whole story.
    owned: PhantomData<*mut T>,
}

// Safety: `Sync` needs **both** bounds, and the second is the easy one to miss.
// `T: Sync` because every thread that asks gets a `&T` to the published value.
// `T: Send` because the value does cross threads: any thread holding only a
// `&RaceOnce` can be the one that builds and publishes, and the value is then
// dropped by whoever owns the cell. This is the bound `std`'s `OnceLock`
// carries for the same reason.
unsafe impl<T: Send + Sync> Sync for RaceOnce<T> {}

// Safety: moving the cell moves the value it owns, and nothing else.
unsafe impl<T: Send> Send for RaceOnce<T> {}

#[cfg_attr(
    not(all(target_family = "wasm", target_feature = "atomics")),
    allow(dead_code)
)]
impl<T> RaceOnce<T> {
    /// An empty cell, in a `const` context — so a `static` needs no
    /// initialiser, the property that keeps `pool::QUEUE` unwedgeable.
    pub(crate) const fn new() -> Self {
        RaceOnce {
            ptr: AtomicPtr::new(std::ptr::null_mut()),
            owned: PhantomData,
        }
    }

    /// The published value, initialising the cell if nothing has yet.
    ///
    /// `init` runs **outside** any critical section — there is none — so it may
    /// allocate and take other locks freely, unlike an `f` handed to
    /// [`SpinLock::with`].
    pub(crate) fn get_or_init(&self, init: impl FnOnce() -> T) -> &T {
        let published = self.ptr.load(Ordering::Acquire);
        if !published.is_null() {
            // Safety: non-null means some thread published a `Box` this cell
            // owns. It is never replaced, and only freed by `Drop`, which
            // needs `&mut self` and so cannot run while this `&self` borrow is
            // alive. The `Acquire` pairs with the publisher's `Release` below,
            // so the pointee is fully initialised as seen from this thread.
            return unsafe { &*published };
        }
        let mine = Box::into_raw(Box::new(init()));
        match self.ptr.compare_exchange(
            std::ptr::null_mut(),
            mine,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            // Safety: this thread published `mine` and nothing ever replaces a
            // published pointer.
            Ok(_) => unsafe { &*mine },
            Err(winner) => {
                // Lost the race. `mine` was never published, so no other thread
                // can have seen it; dropping it here is the whole cost of the
                // trade, and it happens on this thread with no lock held.
                // Safety: `mine` came from `Box::into_raw` just above and is
                // reclaimed exactly once.
                drop(unsafe { Box::from_raw(mine) });
                // Safety: as the load above.
                unsafe { &*winner }
            }
        }
    }
}

impl<T> Drop for RaceOnce<T> {
    fn drop(&mut self) {
        let published = *self.ptr.get_mut();
        if !published.is_null() {
            // Safety: `&mut self` proves no `&T` handed out by `get_or_init` is
            // still alive, and a published pointer is reclaimed exactly once.
            drop(unsafe { Box::from_raw(published) });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicI32;

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

    /// The regression that motivates the `Drop` guard. A trailing-store
    /// release leaves `locked` set when `f` unwinds, and the next acquisition
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

    // ---- RaceOnce ----

    /// `new` in a `const` context — what lets `executor::global`'s `static`
    /// have no initialiser to get stuck in. An anonymous `const` rather than a
    /// `static`: a `static` that published would never drop, and these tests
    /// run under Miri with leak checking on.
    const _: RaceOnce<u32> = RaceOnce::new();

    #[test]
    fn it_publishes_one_value_and_every_later_caller_sees_it() {
        let calls = AtomicI32::new(0);
        let cell: RaceOnce<u32> = RaceOnce::new();
        let first = cell.get_or_init(|| {
            calls.fetch_add(1, Ordering::SeqCst);
            7
        });
        let second = cell.get_or_init(|| {
            calls.fetch_add(1, Ordering::SeqCst);
            9
        });
        assert_eq!(*first, 7);
        assert_eq!(*second, 7, "a published value is never replaced");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a published cell does not run the initialiser again"
        );
        assert!(
            std::ptr::eq(first, second),
            "and it hands out the one value, not a copy"
        );
    }

    /// The cell may *build* more than one value — that is the documented trade
    /// — but it must **publish** exactly one, and every thread must come away
    /// with that one.
    #[test]
    fn racing_initialisers_all_come_away_with_the_same_value() {
        let cell: RaceOnce<u64> = RaceOnce::new();
        let seen: SpinLock<Vec<usize>> = SpinLock::new(Vec::new());
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    let v = cell.get_or_init(|| 0x00C0_FFEE);
                    assert_eq!(*v, 0x00C0_FFEE);
                    seen.with(|log| log.push(v as *const u64 as usize));
                });
            }
        });
        let addrs = seen.with(|log| log.clone());
        assert_eq!(addrs.len(), 8);
        assert!(
            addrs.iter().all(|a| *a == addrs[0]),
            "every thread must observe the single published value"
        );
    }

    /// Live instances of `Counted`, so a value built and thrown away is
    /// visible. The borrow keeps each test's count its own.
    struct Counted<'a>(&'a AtomicI32);

    impl<'a> Counted<'a> {
        fn new(live: &'a AtomicI32) -> Self {
            live.fetch_add(1, Ordering::SeqCst);
            Counted(live)
        }
    }

    impl Drop for Counted<'_> {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// Neither side of the race leaks: the loser drops the value it built, and
    /// dropping the cell frees the one it published. Both are what keeps
    /// `cargo miri test -p frustrate --lib spin` clean.
    ///
    /// The race is *forced*, not hoped for: both threads are held inside their
    /// initialiser until the other has also passed the empty load, so both
    /// really do build a value and exactly one of them has to be discarded.
    #[test]
    fn the_losing_racer_drops_its_value_and_the_cell_frees_the_published_one() {
        let live = AtomicI32::new(0);
        let gate = std::sync::Barrier::new(2);
        {
            let cell: RaceOnce<Counted> = RaceOnce::new();
            std::thread::scope(|s| {
                for _ in 0..2 {
                    s.spawn(|| {
                        cell.get_or_init(|| {
                            gate.wait();
                            Counted::new(&live)
                        });
                    });
                }
            });
            assert_eq!(
                live.load(Ordering::SeqCst),
                1,
                "two values were built; the loser's must have been dropped"
            );
        }
        assert_eq!(
            live.load(Ordering::SeqCst),
            0,
            "dropping the cell frees what it published"
        );
    }
}
