//! The async execution pool. Fixed-width once built, std-only.
//!
//! Three implementations:
//!
//! - **native**: a fixed pool of OS threads.
//! - **wasm, single-threaded** (default): no pool — `spawn` runs inline on
//!   the caller. Async bridge calls on web therefore do not provide
//!   parallelism — they complete during the call that issued them, and any
//!   deferral is the Dart transport's scheduling decision. Parallelism on
//!   web is the Actor model's job (worker-placed instances).
//! - **wasm, threaded** (`wasm-threads`): real worker threads sharing this
//!   instance's linear memory (the module must be built with `+atomics` and
//!   an imported shared memory). Thread creation
//!   is an embedder hook: the `frustrate.spawn_worker` import asks the JS
//!   glue to spawn a Worker, instantiate this same module on the shared
//!   memory, initialize the thread's stack and TLS, and enter
//!   `frustrate_worker_entry`.
//!
//! # The global allocator is std's, and the runtime replaces nothing
//!
//! Said here because this is where a reader goes looking for a swap: there is
//! no `#[global_allocator]` anywhere in the runtime, on any target. Threaded
//! wasm needs an allocator the browser main thread may enter, and std's already
//! is one — `library/std/src/sys/alloc/wasm.rs` locks an `AtomicI32` in a spin
//! loop and carries a long comment saying it deliberately does *not* execute
//! `i32.atomic.wait`, because the main thread cannot block. The `+atomics`
//! block-check artifact links that allocator and contains exactly one
//! wait-containing function, `std::sys::pal::wasm::futex::futex_wait`, which no
//! allocation path reaches.
//!
//! A spin-locked `dlmalloc` of our own would be like-for-like — our spin around
//! `dlmalloc` for std's spin around the same `dlmalloc` — while costing a
//! dependency, a Bazel `select()`, and a second `dlmalloc` instantiation (~126
//! functions, ~40 KiB) in every threaded module. Reintroducing one needs a
//! measurement, not an argument from first principles.
//!
//! # The width is fixed once — declared, or probed
//!
//! The width is a single word, settled the first time anything
//! commits to it and never changed after. Two things settle it: an embedder
//! calling [`declare_width`], and the pool's own construction, which falls back
//! to this build's default (`available_parallelism`, 4 when the probe fails; a
//! constant 4 on threaded wasm, which cannot probe). Both go through **one**
//! compare-exchange on that word, so whichever lands first decides and the
//! loser is told the value it lost to. There is deliberately no window in which
//! a declaration returns `Ok` and the pool is then built at some other width —
//! which is the whole hazard, because the pool is built lazily on first use and
//! a late declaration would otherwise be an unobservable no-op.
//!
//! [`width`] does *not* settle it. Before the pool exists it forecasts (the
//! declared value, else the default); a diagnostic that fixed the width would
//! make reading it a configuration point, and `test_api::pool_width` is read
//! from tests that have no business narrowing anything.
//!
//! **Ordering, stated for the caller.** A declaration has to run before the
//! first async bridge call, because that call builds the pool. Reached from
//! Dart, that means a `#[bridge(sync)]` fn — an async one is dispatched
//! *through* `spawn_call`, so the pool is already built by the time its body
//! runs and it can only ever declare the width it already has. A Flutter hot
//! restart re-enters Dart `main()` in the same native process and does **not**
//! rebuild the pool, which is why re-declaring the same width succeeds rather
//! than erroring: the second run's declaration is true, and only a *changed*
//! value needs a process restart.
//!
//! On web the declaration is per wasm instance, because the statics are: the
//! main instance's says nothing to an actor instance, and an actor instance has
//! no pool for one to reach (its drains ride the worker's microtask queue —
//! `executor::arrange_drain`).
//!
//! The width is fixed once the pool is built; a request to change it is
//! refused with the value it is fixed at.
//!
//! Unlike [`crate::runtime::register`], which is compile-absent on wasm, this
//! exists on every target, so app code stays identical across platforms
//! (charter §3). The target that cannot honour it (single-threaded wasm)
//! refuses at runtime rather than by absence.

// A `#[bridge(no_block)]` claim on a dispatched member is settled by *where the
// body runs*, and that argument has two legs: on threaded web the body is on
// a pool worker, and on single-threaded web it is inline on the caller but
// the module contains no
// `memory.atomic.wait32` for it to execute. Both legs are needed, and together
// they cover every configuration only if the two questions line up —
//
//   "is the body dispatched off the caller?"  →  `feature = "wasm-threads"`
//   "does the wait instruction exist?"        →  `target_feature = "atomics"`
//
// — which they do in every build that ships, and do not in the pairing below.
// +atomics with the feature off gives an inline body in a module that *has* the
// instruction: neither leg holds, and every dispatched claim in it would be
// green for a reason that is not true. Nothing about the platform prevents that
// pairing, so this does.
//
// The one legitimate instance of it is the block-check artifact, which is built
// exactly this way — the feature off so a claimed body is not boxed out of the
// direct-call graph, +atomics so std's futex backends are present to find. That
// module is scanned and thrown away, never instantiated, so no body ever runs on
// any thread in it. Hence the carve-out rather than a weaker predicate.
#[cfg(all(
    target_family = "wasm",
    target_feature = "atomics",
    not(feature = "wasm-threads"),
    not(frustrate_block_check)
))]
compile_error!(
    "frustrate: this is a +atomics wasm build with the `wasm-threads` feature off. \
     A bridged async member would run its body inline on the calling thread in a \
     module that contains `memory.atomic.wait32`, which makes every \
     `#[bridge(no_block)]` claim on a dispatched member unsound. Enable the \
     `wasm-threads` feature for a threaded build (under Bazel it rides the \
     platform; see //bazel:wasm_threads_feature_build), or drop +atomics for the \
     single-threaded one."
);

/// Why `declare_width` could not deliver the width it was asked for.
///
/// A `Result` rather than a panic, and the reason is that every variant names
/// something the caller can act on: the ordering is fixable at the call site,
/// and a bridged wrapper decides how the refusal reaches Dart (`Result<_,
/// String>` is a `BridgeException` there). Ignoring it
/// is not a silent no-op either: `Result` is `#[must_use]`, so dropping the
/// answer is a rustc warning rather than a knob that quietly did nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WidthError {
    /// The width was already fixed at `width` — by the first async bridge call,
    /// which built the pool, or by an earlier declaration of a different value.
    AlreadyFixed {
        /// What the width is, and will stay, for this process.
        width: usize,
    },
    /// This build has no pool to size, so no width but 1 can be honoured.
    Unsupported,
}

impl std::fmt::Display for WidthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WidthError::AlreadyFixed { width } => write!(
                f,
                "frustrate: the pool's width is already fixed at {width}, either by an \
                 earlier declaration or by the first async bridge call, which is what \
                 builds the pool. Declare it before that call — from a \
                 `#[bridge(sync)]` fn, or from Rust that runs before any async one — \
                 and only once, since the width cannot change afterwards. A Flutter \
                 hot restart re-enters Dart main() in the same process and does not \
                 rebuild the pool, so a changed width needs a full process restart"
            ),
            WidthError::Unsupported => write!(
                f,
                "frustrate: this build has no pool to size. On single-threaded wasm an \
                 async body runs inline on the caller, so the width is 1 and no other \
                 value can be honoured; web parallelism is the Actor model"
            ),
        }
    }
}

impl std::error::Error for WidthError {}

#[cfg(any(not(target_family = "wasm"), feature = "wasm-threads"))]
use width::Width;

/// The one word the module docs describe, and the compare-exchange that keeps
/// "this declaration was accepted" and "the pool was built at that width" from
/// ever being two different answers.
///
/// Its own module so the atomics imports do not have to be `cfg`'d alongside
/// it: single-threaded wasm has no pool and answers from the shape of the build
/// instead, so nothing there constructs one.
#[cfg(any(not(target_family = "wasm"), feature = "wasm-threads"))]
mod width {
    use super::WidthError;
    use std::num::NonZeroUsize;
    use std::sync::atomic::{AtomicUsize, Ordering};

    pub(super) struct Width(AtomicUsize);

    impl Width {
        /// Not yet fixed. A width of 0 is unrepresentable in the public API
        /// (`NonZeroUsize`), so this sentinel cannot collide with a real value
        /// and there is no "was it set?" flag to keep in step with it.
        const UNFIXED: usize = 0;

        /// `const`-constructible, so the static holding it has no initialiser
        /// to get stuck in — the property `spin.rs` explains and `pool::QUEUE`
        /// paid for.
        pub(super) const fn new() -> Self {
            Width(AtomicUsize::new(Self::UNFIXED))
        }

        /// The width the pool has, or would be built at if it were built now.
        /// Commits nothing; see the module docs.
        pub(super) fn forecast(&self, default: impl FnOnce() -> usize) -> usize {
            match self.0.load(Ordering::Acquire) {
                Self::UNFIXED => default(),
                fixed => fixed,
            }
        }

        /// Fix the width — at `default` unless a declaration got here first —
        /// and answer what it settled at. The pool's construction calls this,
        /// and nothing else does: the value it returns is the number of workers
        /// that exist, so a second caller would be a second pool.
        pub(super) fn fix(&self, default: impl FnOnce() -> usize) -> usize {
            let fixed = self.0.load(Ordering::Acquire);
            if fixed != Self::UNFIXED {
                return fixed;
            }
            let n = default();
            match self
                .0
                .compare_exchange(Self::UNFIXED, n, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => n,
                // A declaration landed between the load and the exchange. It
                // wins — it asked for a width, and this only had a default.
                Err(declared) => declared,
            }
        }

        /// An embedder's declaration.
        ///
        /// Idempotent rather than once-only, because Flutter hot restart makes
        /// the repeat legal: the second run of `main()` declares the width the
        /// pool already has, which is a true statement about this process. A
        /// *different* value is the only thing refused, and the error carries
        /// the width it is refused in favour of.
        ///
        /// The orderings sequence agreement on one word; there is no data
        /// behind it to publish.
        pub(super) fn declare(&self, width: NonZeroUsize) -> Result<(), WidthError> {
            match self.0.compare_exchange(
                Self::UNFIXED,
                width.get(),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => Ok(()),
                Err(fixed) if fixed == width.get() => Ok(()),
                Err(fixed) => Err(WidthError::AlreadyFixed { width: fixed }),
            }
        }
    }
}

/// Run the job for async call `call_id` on the pool and deliver its
/// envelope via `post::respond`. On threaded wasm the call id is additionally
/// recorded in the worker's TLS while the job runs, so a panic (a trap,
/// under wasm's panic=abort) stays attributable: the worker glue reads
/// `frustrate_current_call` after catching the trap, fails exactly the
/// right future, and replenishes the pool (`frustrate_pool_replenish`) —
/// pool width is an invariant, though state the panicking job touched — a
/// `locked` object whose guard never dropped, say — stays wedged.
pub fn spawn_call(
    call_id: u64,
    job: impl FnOnce() -> crate::codec::FramedWriter + Send + 'static,
) {
    spawn(move || {
        #[cfg(all(target_family = "wasm", feature = "wasm-threads"))]
        threaded::set_current_call(call_id);
        crate::post::respond(call_id, job());
        #[cfg(all(target_family = "wasm", feature = "wasm-threads"))]
        threaded::set_current_call(0);
    });
}

/// Drive `future` to completion on the current thread, returning its output.
///
/// A minimal, dependency-free blocking executor — a poll loop with a
/// thread-parking `Waker`. This is NOT the path a bridged Rust `async fn`
/// takes: those run on the cooperative multiplexing executor
/// (`frustrate::executor`), which suspends a `Pending` future as heap data
/// rather than parking a thread (and so works on single-threaded web, where
/// this cannot). `block_on` is retained for the worker/native CPU-drive path —
/// blocking one owned thread on a self-driving future — and is std-only:
///
///   - poll the future;
///   - on `Ready`, return the value;
///   - on `Pending`, park the current thread until the waker signals
///     (`Thread::unpark`), then poll again.
///
/// std only: the `Waker` is built from `std::task::Wake` over the running
/// thread's handle, so waking simply unparks this thread. (`unpark` is
/// sticky — a wake that races ahead of the `park` makes the next `park`
/// return immediately — so no wakeup is lost.)
///
/// # No embedded reactor
///
/// This executor does NOT provide a timer wheel, an IO reactor, or any other
/// source of external wakeups. The future must make progress on its own: a
/// leaf future that returns `Pending` without ever arranging a wake (through
/// its own timer thread, another runtime, etc.) parks this pool thread
/// forever. A body that needs tokio/async-std should bring its own runtime
/// and block on it inside the fn (`rt.block_on(...)`) — native-first, and it
/// parks one pool thread per call; an `async fn` here is for composing
/// already-self-driving futures.
///
/// The [async-runtime hook](crate::runtime) does **not** change this, and the
/// asymmetry is deliberate: it wraps the *cooperative executor's* polls, not
/// this function, which is a utility a caller invokes explicitly rather than a
/// path the generated glue takes. A registration therefore leaves the
/// bring-your-own-`block_on` pattern exactly as it was, and a tokio leaf
/// reached through `block_on` with no context of its own still fails loudly at
/// construction ("there is no reactor running") rather than hanging. To get
/// cancellation and an unparked thread, bridge an `async fn` and `.await` the
/// leaf.
pub fn block_on<F: std::future::Future>(future: F) -> F::Output {
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};

    /// Waking unparks the thread that is blocked in the poll loop below.
    struct ThreadWaker(std::thread::Thread);
    impl Wake for ThreadWaker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.unpark();
        }
    }

    let mut future = std::pin::pin!(future);
    let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return value,
            // Spurious unparks only cost an extra poll, which is correct.
            Poll::Pending => std::thread::park(),
        }
    }
}

#[cfg(all(target_family = "wasm", not(feature = "wasm-threads")))]
/// Run `job` inline: a single-threaded wasm instance has no worker threads.
pub fn spawn(job: impl FnOnce() + Send + 'static) {
    job();
}

/// The pool's width: how many jobs can run concurrently. 1 on
/// single-threaded wasm (jobs run inline on the caller).
#[cfg(all(target_family = "wasm", not(feature = "wasm-threads")))]
pub fn width() -> usize {
    1
}

/// Declare the pool's width. Here there is no pool, so 1 is the only width
/// this build can honour and every other value is refused rather than
/// accepted and ignored — see the module docs and [`WidthError::Unsupported`].
#[cfg(all(target_family = "wasm", not(feature = "wasm-threads")))]
pub fn declare_width(width: std::num::NonZeroUsize) -> Result<(), WidthError> {
    if width.get() == 1 {
        Ok(())
    } else {
        Err(WidthError::Unsupported)
    }
}

#[cfg(not(target_family = "wasm"))]
pub use native::{declare_width, spawn, width};

#[cfg(all(target_family = "wasm", feature = "wasm-threads"))]
pub use threaded::{declare_width, spawn, width};

#[cfg(not(target_family = "wasm"))]
mod native {
    use super::{Width, WidthError};
    use std::num::NonZeroUsize;
    use std::sync::mpsc;
    use std::sync::{Mutex, OnceLock};
    use std::thread;

    type Job = Box<dyn FnOnce() + Send + 'static>;

    struct Pool {
        sender: Mutex<mpsc::Sender<Job>>,
    }

    static POOL: OnceLock<Pool> = OnceLock::new();

    static WIDTH: Width = Width::new();

    /// This build's default: one worker per available core, 4 when the probe
    /// fails. `available_parallelism` rather than the processor count, so a
    /// cgroup quota is respected — which is also why it disagrees with Dart's
    /// `Platform.numberOfProcessors` under Bazel.
    fn probe() -> usize {
        thread::available_parallelism().map(|n| n.get()).unwrap_or(4)
    }

    /// The pool's width: what it was built at, or — before the first job —
    /// what it would be built at now.
    pub fn width() -> usize {
        WIDTH.forecast(probe)
    }

    /// Declare the pool's width, in place of the probe above. Must run before
    /// the first async bridge call builds the pool; the module docs carry the
    /// ordering contract and the hot-restart case, and [`WidthError`] carries
    /// what a late call is told.
    pub fn declare_width(width: NonZeroUsize) -> Result<(), WidthError> {
        WIDTH.declare(width)
    }

    fn pool() -> &'static Pool {
        POOL.get_or_init(|| {
            let (sender, receiver) = mpsc::channel::<Job>();
            let receiver = std::sync::Arc::new(Mutex::new(receiver));
            let workers = WIDTH.fix(probe);
            for i in 0..workers {
                let receiver = receiver.clone();
                thread::Builder::new()
                    .name(format!("frustrate-{i}"))
                    .spawn(move || {
                        // Pool threads may block on Dart (DartFunction::call).
                        crate::callback::enter_worker_context();
                        loop {
                            let job = {
                                let guard = receiver.lock().unwrap();
                                guard.recv()
                            };
                            match job {
                                Ok(job) => job(),
                                Err(_) => return,
                            }
                        }
                    })
                    .expect("failed to spawn frustrate worker");
            }
            Pool {
                sender: Mutex::new(sender),
            }
        })
    }

    /// Run `job` on the frustrate pool.
    pub fn spawn(job: impl FnOnce() + Send + 'static) {
        pool()
            .sender
            .lock()
            .unwrap()
            .send(Box::new(job))
            .expect("frustrate pool is gone");
    }
}

#[cfg(all(target_family = "wasm", feature = "wasm-threads"))]
mod threaded {
    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    type Job = Box<dyn FnOnce() + Send + 'static>;
    /// Thread entries handed to the embedder: double-boxed so the raw
    /// pointer crossing the import boundary is thin.
    type Entry = Box<dyn FnOnce() + Send + 'static>;

    #[link(wasm_import_module = "frustrate")]
    extern "C" {
        /// Embedder hook: spawn a Worker, instantiate this module on the
        /// shared memory, init the new thread's stack + TLS, then call
        /// `frustrate_worker_entry(entry)` on it.
        fn spawn_worker(entry: u32);
    }

    /// The default pool width. wasm32-unknown-unknown cannot observe hardware
    /// parallelism (`available_parallelism` is unsupported), so the *default*
    /// is a documented constant rather than a probe. An embedder that knows
    /// the machine says so with [`declare_width`]: not being able to measure
    /// the width is no reason to refuse a declared one.
    const DEFAULT_WORKERS: usize = 4;

    static WIDTH: super::Width = super::Width::new();

    /// The only lock kind the main thread may take: a spin lock. Any
    /// futex-parking lock (std Mutex, std mpsc's internal waker lock) traps
    /// on the browser main thread when contended.
    use crate::spin::SpinLock;

    /// Job queue whose push side never waits, so the main thread may push:
    /// a spin-locked deque plus a sequence counter workers futex-wait on.
    /// memory.atomic.notify is legal on every thread; only the *wait* is
    /// main-thread-illegal, and only workers wait. (std::sync::mpsc is not
    /// usable here: its send path locks the receiver-waker Mutex, which
    /// parks with memory.atomic.wait32 under contention — a trap on main.)
    struct Queue {
        jobs: SpinLock<VecDeque<Job>>,
        /// Bumped on every push; workers wait for it to move.
        seq: AtomicU32,
    }

    impl Queue {
        /// Wait-free from the pusher's side, and that is the other half of what
        /// settles a dispatched `#[bridge(no_block)]` claim: the main thread
        /// reaches this function on every such call, so "the body runs on a
        /// worker" is only useful if getting it there cannot itself wait.
        /// `SpinLock` never executes the wait instruction and
        /// `memory_atomic_notify` is legal on any thread. A parking lock here —
        /// or any wait added on this path — would stall the page on a claim that
        /// says it cannot, and no check artifact would see it: a dispatched
        /// member's root is its caller-side residue, which does not include this.
        fn push(&self, job: Job) {
            self.jobs.with(|q| q.push_back(job));
            self.seq.fetch_add(1, Ordering::SeqCst);
            unsafe {
                core::arch::wasm32::memory_atomic_notify(self.seq.as_ptr() as *mut i32, 1);
            }
        }

        fn run_worker(&self) {
            loop {
                let observed = self.seq.load(Ordering::SeqCst);
                if let Some(job) = self.jobs.with(|q| q.pop_front()) {
                    job();
                    continue;
                }
                // Blocking is legal here (worker thread). A push that raced
                // us already moved `seq`, so the wait returns immediately.
                unsafe {
                    core::arch::wasm32::memory_atomic_wait32(
                        self.seq.as_ptr() as *mut i32,
                        observed as i32,
                        -1,
                    );
                }
            }
        }
    }

    /// The queue, const-constructed as a static rather than built inside a
    /// `OnceLock::get_or_init`.
    ///
    /// This is a safety property, not a style choice. The embedder's
    /// `spawn_worker` hook can fail, and under panic=abort its JS exception
    /// unwinds through wasm frames running no `Drop` — so a `Once` around the
    /// queue would be left RUNNING for the life of the page, and the next
    /// `pool()` would futex-wait on it from the browser main thread. A const
    /// static has no initializer to get stuck in.
    static QUEUE: Queue = Queue {
        jobs: SpinLock::new(VecDeque::new()),
        seq: AtomicU32::new(0),
    };

    /// Whether the one-time worker spawn has been *attempted*, claimed as a
    /// whole before the first hook call. A spawn that throws never comes
    /// back, so anything finer would either re-attempt a spawn that already
    /// failed (a no-progress storm — the same script, module and memory) or
    /// poison the next few calls with the same failure. Claiming it whole
    /// means exactly one call ever pays for a failed spawn, and the pool runs
    /// at whatever width came up: the throw also abandons the loop's remaining
    /// spawns, so the degraded report the Dart side raises states the real
    /// width, and replenishment (below) is the only thing that adds to it.
    static SPAWN_STARTED: AtomicBool = AtomicBool::new(false);

    /// The pool's width: what it was built at — the declared width, else
    /// [`DEFAULT_WORKERS`] — or, before the first job, what it would be built
    /// at now. Width is an invariant: a trapped worker is replaced
    /// (`frustrate_pool_replenish`).
    pub fn width() -> usize {
        WIDTH.forecast(|| DEFAULT_WORKERS)
    }

    /// Declare the pool's width, in place of [`DEFAULT_WORKERS`]. Must run
    /// before the first async bridge call builds the pool, and applies to
    /// *this* wasm instance only — the module docs carry both, and
    /// [`WidthError`](super::WidthError) carries what a late call is told.
    pub fn declare_width(width: std::num::NonZeroUsize) -> Result<(), super::WidthError> {
        WIDTH.declare(width)
    }

    /// Spawn one pool worker attached to the queue. Workers need no
    /// registration beyond this: they participate purely by popping.
    fn spawn_one() {
        let entry: Entry = Box::new(|| QUEUE.run_worker());
        unsafe { spawn_worker(Box::into_raw(Box::new(entry)) as u32) };
    }

    fn pool() -> &'static Queue {
        if !SPAWN_STARTED.swap(true, Ordering::Relaxed) {
            for _ in 0..WIDTH.fix(|| DEFAULT_WORKERS) {
                spawn_one();
            }
        }
        &QUEUE
    }

    /// Respawn one pool worker after a trap killed one (the main-thread
    /// glue calls this on a worker 'trap' message, restoring the pool to
    /// full width). Main-thread legal: one spin-locked allocation — the new
    /// thread does all the waiting.
    ///
    /// Replenishment restores *width*, never state: whatever the panicking
    /// job held stays held (no unwinding under panic=abort). For a **user
    /// `Locked` type** the object is simply dead — its guard's `Drop` never
    /// ran. An async call then awaits a release that never comes, suspending a
    /// task rather than parking a worker (the lock is async). That is the dead
    /// object telling you about the panic, not a pool defect. Natively it says
    /// so sooner, through a sync member's `on_contention = "error"` — but
    /// those members are native-only, so on web the suspension is the whole of
    /// the report.
    ///
    /// It is *not* loud for the runtime's own [`crate::spin::SpinLock`]s, which
    /// have no try-lock path to report through: one held at trap time spins
    /// every later acquisition, including from the browser main thread. The
    /// only window is an allocation inside a critical section, and it is
    /// reachable only when the module is out of memory — already page-fatal.
    /// See spin.rs, which states the same boundary from the lock's side.
    #[no_mangle]
    pub extern "C" fn frustrate_pool_replenish() {
        // A trap implies the pool initialized; an uninitialized pool is a
        // no-op, never an init from here.
        if SPAWN_STARTED.load(Ordering::Relaxed) {
            spawn_one();
        }
    }

    /// Run `job` on a worker thread.
    ///
    /// **There is no inline fallback, and a `#[bridge(no_block)]` claim rests on
    /// that.** A dispatched member's claim is settled by placement here: the
    /// body runs on a worker, so the browser main thread cannot execute anything it does. What
    /// makes that sound is that the push is *total* — a pool with no live worker
    /// leaves the job queued forever, which loses the call, and never runs it on
    /// the pusher. Adding "…and if no worker picks it up, run it here" would
    /// turn every such claim into a false green, silently, with nothing in the
    /// check artifact to notice: no root reaches a dispatched body.
    pub fn spawn(job: impl FnOnce() + Send + 'static) {
        pool().push(Box::new(job));
    }

    thread_local! {
        static CURRENT_CALL: Cell<u64> = const { Cell::new(0) };
    }

    pub fn set_current_call(id: u64) {
        CURRENT_CALL.with(|c| c.set(id));
    }

    /// Thread entry: runs the boxed entry closure (the pool worker loop).
    ///
    /// # Safety
    /// `entry` must be a pointer produced by `pool()` in this shared memory,
    /// used exactly once, after the calling thread's stack pointer and TLS
    /// block have been initialized by the glue.
    #[no_mangle]
    pub unsafe extern "C" fn frustrate_worker_entry(entry: u32) {
        let entry = *Box::from_raw(entry as *mut Entry);
        entry();
    }

    /// The call id of the async job this thread is currently running (0 if
    /// none). The worker glue reads this after catching a trap so the panic
    /// fails exactly the future it belongs to.
    #[no_mangle]
    pub extern "C" fn frustrate_current_call() -> u64 {
        CURRENT_CALL.with(|c| c.get())
    }

    /// Aligned allocation for worker stacks and TLS blocks, called by the
    /// glue (reentrantly, during `spawn_worker`). Never freed: pool workers
    /// live for the page's lifetime, and a trapped worker's stack + TLS
    /// (~1 MiB) leak deliberately when it is replaced — bounded by the
    /// number of panics, and panics are bugs.
    /// Null is not a failure code here, it is an address. The glue does not
    /// check the return value — it cannot usefully, since 0 is a legal
    /// address in linear memory — and hands it straight to a new thread as
    /// its stack base and TLS block. A silent null therefore produces a
    /// worker whose stack grows over the module's own low memory, corrupting
    /// whatever lives there with no trap, no error, and no failed spawn. Trap
    /// instead: the panic hook (`frustrate_web_init`) routes the message to
    /// JS, so an exhausted shared memory surfaces as an attributable
    /// BridgePanicException on the call that triggered the spawn.
    #[no_mangle]
    pub extern "C" fn frustrate_alloc_aligned(size: u32, align: u32) -> u32 {
        let layout = std::alloc::Layout::from_size_align(size as usize, align as usize)
            .expect("frustrate: bad worker stack/TLS layout");
        let ptr = unsafe { std::alloc::alloc(layout) } as u32;
        assert!(
            ptr != 0,
            "frustrate: out of memory reserving {size} bytes (align {align}) for a \
             pool worker's stack/TLS — the shared memory is at its maximum"
        );
        ptr
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use super::*;
    use std::num::NonZeroUsize;
    use std::sync::mpsc;

    fn nonzero(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).expect("test widths are positive")
    }

    /// Whatever settles the width, everything else agrees with it — over a
    /// `Width` of this test's own, so the process's real pool is untouched.
    ///
    /// This is the property the compare-exchange exists for, and it is written
    /// as a race rather than a sequence because a sequence would pass against
    /// a load-then-store that has a window in it. `n` threads each either
    /// declare a width of their own or build at a default; afterwards every
    /// `Ok` must name the width that survived, every `Err` must report that
    /// same width, and every builder must have been told to build at it.
    #[test]
    fn one_width_wins_and_no_caller_is_told_otherwise() {
        /// What one racing thread did and was told.
        #[derive(Debug)]
        enum Outcome {
            Declared(usize, Result<(), WidthError>),
            /// The width the pool would have been built at.
            Built(usize),
        }
        const THREADS: usize = 8;
        const DEFAULT: usize = 99;

        let word = std::sync::Arc::new(Width::new());
        let start = std::sync::Arc::new(std::sync::Barrier::new(THREADS));
        let mut handles = Vec::new();
        for i in 0..THREADS {
            let (word, start) = (word.clone(), start.clone());
            handles.push(std::thread::spawn(move || {
                start.wait();
                // Half declare, half build. The declared widths are distinct,
                // and none of them is DEFAULT, so an accepted declaration
                // cannot be two threads happening to agree.
                if i % 2 == 0 {
                    let asked = i + 1;
                    Outcome::Declared(asked, word.declare(nonzero(asked)))
                } else {
                    Outcome::Built(word.fix(|| DEFAULT))
                }
            }));
        }
        let outcomes: Vec<Outcome> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let settled = word.forecast(|| unreachable!("one of the threads fixed it"));
        for outcome in outcomes {
            match outcome {
                Outcome::Declared(asked, Ok(())) => assert_eq!(
                    asked, settled,
                    "a declaration was accepted for a width the pool did not get"
                ),
                Outcome::Declared(_, Err(e)) => {
                    assert_eq!(e, WidthError::AlreadyFixed { width: settled })
                }
                Outcome::Built(at) => assert_eq!(
                    at, settled,
                    "a builder was told to build at a width nothing else agrees with"
                ),
            }
        }
    }

    /// The two facts a caller reads off a `Width` before and after it is
    /// fixed, and the hot-restart case: re-declaring the width the pool has
    /// is `Ok`, a different one is refused and names what it is refused for.
    #[test]
    fn a_forecast_before_it_is_fixed_a_fact_after() {
        let word = Width::new();
        assert_eq!(word.forecast(|| 8), 8, "unfixed: the default is a forecast");
        assert_eq!(word.declare(nonzero(2)), Ok(()));
        assert_eq!(word.forecast(|| 8), 2, "a declaration replaces the default");
        assert_eq!(word.fix(|| 8), 2, "and the pool is built at it");
        assert_eq!(word.declare(nonzero(2)), Ok(()), "hot restart re-declares");
        assert_eq!(
            word.declare(nonzero(3)),
            Err(WidthError::AlreadyFixed { width: 2 })
        );
        assert_eq!(word.forecast(|| 8), 2, "a refused declaration changed nothing");
    }

    /// The same refusal against the process's *real* pool, which by this point
    /// exists: this test builds it first rather than racing the rest of the
    /// binary for it. Nothing here can widen or narrow the pool the other
    /// tests share — that is the point of forcing the build up front.
    #[test]
    fn declaring_a_width_the_running_pool_does_not_have_is_refused() {
        let (tx, rx) = mpsc::channel();
        spawn(move || tx.send(()).unwrap());
        rx.recv().expect("the pool runs the job that builds it");

        let running = width();
        assert_eq!(declare_width(nonzero(running)), Ok(()));
        let refused = declare_width(nonzero(running + 1)).expect_err("the pool is built");
        assert_eq!(refused, WidthError::AlreadyFixed { width: running });
        assert!(
            refused.to_string().contains(&running.to_string()),
            "the message must name the width it is fixed at: {refused}"
        );
        assert_eq!(width(), running, "a refused declaration changed nothing");
    }

    #[test]
    fn runs_jobs_concurrently_enough() {
        let (tx, rx) = mpsc::channel();
        for i in 0..32 {
            let tx = tx.clone();
            spawn(move || tx.send(i).unwrap());
        }
        let mut got: Vec<i32> = (0..32).map(|_| rx.recv().unwrap()).collect();
        got.sort();
        assert_eq!(got, (0..32).collect::<Vec<_>>());
    }

    /// A future that returns `Pending` `n` times (waking itself each time)
    /// before it is `Ready`, so `block_on` must poll it `n + 1` times and
    /// park between polls. Proves the executor is a real poll loop, not a
    /// single `now_or_never`.
    struct PendModel {
        remaining: u32,
        polls: std::rc::Rc<std::cell::Cell<u32>>,
    }

    impl std::future::Future for PendModel {
        type Output = u32;
        fn poll(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<u32> {
            self.polls.set(self.polls.get() + 1);
            if self.remaining == 0 {
                std::task::Poll::Ready(self.polls.get())
            } else {
                self.remaining -= 1;
                // Self-driving: schedule the next poll before yielding.
                cx.waker().wake_by_ref();
                std::task::Poll::Pending
            }
        }
    }

    #[test]
    fn block_on_drives_a_multi_poll_future_to_completion() {
        let polls = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let total = super::block_on(PendModel {
            remaining: 3,
            polls: polls.clone(),
        });
        // Pending 3 times, then Ready on the 4th poll.
        assert_eq!(polls.get(), 4);
        assert_eq!(total, 4);
    }

    #[test]
    fn block_on_composes_async_blocks() {
        // An `async` block that awaits an inner self-driving future — the
        // shape the generated glue drives for a bridged `async fn`.
        async fn inner(x: i64) -> i64 {
            let polls = std::rc::Rc::new(std::cell::Cell::new(0u32));
            let _ = PendModel {
                remaining: 1,
                polls,
            }
            .await;
            x * 2
        }
        assert_eq!(super::block_on(inner(21)), 42);
    }
}
