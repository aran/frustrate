//! The async-runtime hook: lend the cooperative executor an embedder-supplied
//! runtime's context, so a bridged `async fn` can `.await` a **tokio /
//! async-std leaf directly**.
//!
//! The cooperative executor ([`crate::executor`]) has no reactor: it re-polls
//! a task only when something arranged a wake, and nothing here connects that
//! to a foreign runtime's reactor. A registration is what does.
//!
//! ```ignore
//! // once, at init — see `register` for the hot-restart contract
//! frustrate::runtime::register({
//!     let handle = RT.handle().clone();
//!     move |poll| {
//!         let _guard = handle.enter();   // tokio's thread-local context
//!         poll.run();                    // ...for the duration of one poll
//!     }
//! });
//! ```
//!
//! [`register`] is compile-absent on wasm; `enter` is the identity there, so
//! the executor's drive loop stays one source line on every config.

#[cfg(not(target_family = "wasm"))]
pub use native::PollOnce;

/// The registered wrapper. `Fn`, not `FnOnce`: it runs around **every** poll.
#[cfg(not(target_family = "wasm"))]
type Context = dyn for<'a> Fn(PollOnce<'a>) + Send + Sync + 'static;

#[cfg(not(target_family = "wasm"))]
mod native {
    use super::Context;
    use crate::hook::Hook;
    use std::sync::Arc;

    /// The poll a registered context is asked to wrap. Call [`run`](Self::run)
    /// inside your runtime's guard.
    ///
    /// It consumes `self`, so "polled twice" is a **compile** error rather
    /// than a contract you could break at runtime:
    ///
    /// ```compile_fail,E0382
    /// frustrate::runtime::register(|poll| {
    ///     poll.run();
    ///     poll.run();   // error[E0382]: use of moved value
    /// });
    /// ```
    ///
    /// The error code is pinned, not just "it fails": without it, a future
    /// inference or signature regression that broke this snippet for some
    /// unrelated reason would keep the test green while the property it
    /// guards was gone.
    ///
    /// The other half — dropping it without running it — cannot be made a
    /// compile error (a `Drop` type can always be dropped), so it is a loud
    /// panic; see [`register`](super::register).
    pub struct PollOnce<'a>(&'a mut dyn FnMut());

    impl PollOnce<'_> {
        /// Poll the task once, here, on this thread.
        pub fn run(self) {
            (self.0)();
        }
    }

    /// The one slot.
    ///
    /// A [`Hook`] rather than a slot spelled out here, and that module carries
    /// the mechanism this shares with every other hook an embedder registers
    /// into: the presence flag that keeps the empty case to one acquire load
    /// per poll rather than a lock, and the rule that a displaced value —
    /// here, the caller's closure and whatever it captured — dies outside the
    /// critical section.
    ///
    /// What is this hook's own, and stated on [`register`] under
    /// "Re-registration", is *why* the slot must be replaceable at all rather
    /// than a `OnceLock`: Flutter hot restart re-runs Dart `main()` in this
    /// same native process.
    static CONTEXT: Hook<Context> = Hook::new();

    /// Register the process's async-runtime context: a wrapper that installs
    /// the runtime's thread-local context, calls `poll`, and restores it.
    ///
    /// ```ignore
    /// // Native only — `register` does not exist on wasm, so a bridge that
    /// // also builds for web gates both this call and the tokio dependency:
    /// //   [target.'cfg(not(target_family = "wasm"))'.dependencies]
    /// //   tokio = { version = "1", features = ["rt-multi-thread", "time"] }
    /// // (worked example: e2e/iroh_demo/bridge/Cargo.toml)
    /// #[cfg(not(target_family = "wasm"))]
    /// frustrate::runtime::register({
    ///     let handle = RT.handle().clone();
    ///     move |poll| {
    ///         let _guard = handle.enter();
    ///         poll.run();
    ///     }
    /// });
    /// ```
    ///
    /// # The contract you are taking on
    ///
    /// - **Run the poll.** Returning without calling [`PollOnce::run`] would
    ///   strand the task with no answer and no error, so it is a loud panic
    ///   attributed to that call instead — at the cost of leaking the task,
    ///   because dropping an un-run poll would drop the future *outside* your
    ///   runtime's context, and async-task aborts the process on a panic in a
    ///   future's destructor (`raw.rs:447`). Running it twice is a compile
    ///   error; `PollOnce::run` consumes.
    /// - **Let the poll's panic through.** A wrapper that `catch_unwind`s the
    ///   poll swallows the future's real panic; `enter` notices and answers
    ///   the call, but with a message about the wrapper rather than the bug.
    ///   And a wrapper that panics *after* a poll which already completed its
    ///   call is discarded entirely — the answer-once gate has given that call
    ///   its real answer, so there is nothing left to attribute the wrapper's
    ///   panic to. Keep the wrapper to `enter`, `poll.run()`, and nothing.
    /// - **The runtime must be independently driven.** A `Handle` to a
    ///   `new_current_thread()` runtime that nobody is `block_on`-ing has a
    ///   timer wheel nothing turns: the leaf registers and is never woken.
    ///   Register a multi-thread runtime, or a current-thread one that a
    ///   thread of yours is driving.
    ///
    /// Only the first is frustrate's to enforce, and the rest of the picture
    /// is measured rather than assumed (`tests/tokio_hook`). Of the ways to
    /// get the context wrong, **tokio makes most of them loud**:
    ///
    /// | mistake | what happens |
    /// |---|---|
    /// | nothing registered | panic at leaf construction, "there is no reactor running" — attributed to the call |
    /// | handle to a dropped `Runtime` | panic, "A Tokio 1.x context was found, but it is being shutdown" |
    /// | wrapper never runs the poll | panic naming the wrapper (above) |
    /// | wrapper runs the poll twice | compile error — `PollOnce::run` consumes |
    /// | **undriven `current_thread` runtime** | **silent — the task simply never wakes** |
    /// | **`block_on` inside a bridged `async fn`** | **silent — the drain thread blocks** (see Scope) |
    ///
    /// The two bold rows are real gaps against this project's "contracts loud"
    /// bar, and both are ceilings rather than unfinished edges. For the
    /// undriven runtime the only signal available is "this task has not been
    /// woken", which is exactly what a task legitimately awaiting Dart looks
    /// like, and asking tokio would mean depending on tokio. For `block_on`
    /// the body is one you wrote and codegen never sees — the same boundary
    /// as "codegen cannot infer *this body blocks*". Both
    /// are stated here and pinned by tests, in the same spirit as pool.rs's
    /// note on what worker replenishment cannot make loud.
    ///
    /// # Scope
    ///
    /// The context wraps the **cooperative executor's** polls: bridged
    /// `async fn` bodies and actor [`Deferred`](crate::Deferred) completions.
    /// It does *not* wrap a plain `#[bridge] fn` body running on the pool
    /// (`pool::spawn_call`), so the bring-your-own-`block_on` pattern is
    /// untouched by a registration.
    ///
    /// The corollary is a trap worth naming: calling your runtime's own
    /// `block_on` *inside a bridged `async fn`* is not rejected. Measured —
    /// `Handle::enter()` installs the runtime context but not tokio's
    /// blocking-region guard, so "Cannot start a runtime from within a
    /// runtime" does not fire, for either a second runtime or the entered one.
    /// It simply blocks the drain thread until it finishes, which is the
    /// parked-thread cost this hook exists to avoid, arrived at silently.
    /// `.await` the leaf; do not `block_on` it.
    ///
    /// # Re-registration
    ///
    /// Last registration wins, and the alternatives are worse for a specific,
    /// measured reason. Flutter hot restart re-runs Dart `main()` in this same
    /// native process while Rust statics persist (actor.rs records the
    /// consequence: "tokio workers 16 → 64" from runtimes rebuilt per
    /// restart), so `register` *will* be called again in a live process. And a
    /// handle to the dropped previous `Runtime` does not go quiet — it panics
    /// "A Tokio 1.x context was found, but it is being shutdown" on every
    /// poll. So:
    ///
    /// - **Panic on re-registration** would break the hot-restart loop
    ///   outright, and this project's Dart side already refuses that stance
    ///   (`runtime_core.dart`: init is an *ensure*; throwing "buys nothing and
    ///   breaks working code").
    /// - **Keep-first** would pin the *dead* handle for the life of the
    ///   process, turning every async call after a restart into that shutdown
    ///   panic — with the app's fresh, correct registration sitting there
    ///   ignored.
    /// - **Last-wins** makes the restart simply work, and nothing is lost
    ///   silently: a poll already inside the old wrapper keeps it (the slot
    ///   holds an `Arc`), and the wrapper being replaced is the one the caller
    ///   is deliberately replacing.
    ///
    /// Registration is process-global and has no per-`fn` selection.
    pub fn register(context: impl for<'a> Fn(PollOnce<'a>) + Send + Sync + 'static) {
        // The wrapper being replaced is dropped outside the slot's critical
        // section, by `Hook::set`. That matters most on exactly the path this
        // last-wins slot exists for: on a hot restart the displaced closure's
        // captured `Arc<Runtime>` can be the runtime's last reference, and
        // `Runtime::drop` blocks until the shutdown completes.
        CONTEXT.set(Arc::new(context));
    }

    /// Drop the registration; polls revert to plain cooperative behaviour.
    ///
    /// The counterpart of [`register`] for a runtime being torn down. Leaving
    /// a handle to a dropped `Runtime` registered is not silent — tokio panics
    /// "…being shutdown" on every poll (see the table on `register`) — so this
    /// is about choosing *which* loud failure a task that outlives the runtime
    /// gets: "there is no reactor running" at its own construction, rather
    /// than a shutdown panic from a handle nobody meant to keep.
    ///
    /// A poll already running under the old wrapper keeps it (`Arc`), and the
    /// slot's lock is never held across a poll, so this is safe to call from
    /// anywhere — including a task's own body.
    pub fn unregister() {
        CONTEXT.clear();
    }

    /// Run `poll` under the registered context, if there is one.
    ///
    /// The caller wraps the whole of `Runnable::run()`, not "the poll", and
    /// the difference is load-bearing. `run()` is also where async-task drops
    /// a **cancelled** future, and a teardown that reaches for the runtime —
    /// `Handle::current()` to arm a timeout or spawn a cleanup task, ordinary
    /// in exactly the bodies this hook is for — panics without a context. A
    /// panic in `Drop` during cleanup is a **process abort**, not a catchable
    /// error, so the cheap wrap is the difference between a working
    /// cancellation and a dead process. Measured both ways in
    /// `tests/tokio_hook` (tokio's *own* leaf drops fine without a context —
    /// it owns its `scheduler::Handle` — so this is about user teardown, not
    /// about tokio's internals).
    pub(crate) fn enter<R>(poll: impl FnOnce() -> R) -> R {
        // One acquire load when nothing is registered, which is the common
        // case on every drain of every pool worker; the `Arc` is cloned out of
        // the slot's section, so the poll below runs with no lock held. Both
        // properties belong to `Hook`, and are tested there.
        let Some(context) = CONTEXT.get() else {
            return poll();
        };

        // `PollOnce::run` consumes, so a wrapper cannot poll twice. What it
        // can still do is *not* poll — return early, or panic on the way to
        // the call — and that is the case this dance exists for.
        let mut pending = Unrun(Some(poll));
        let mut out = None;
        let mut run = || {
            if let Some(poll) = pending.0.take() {
                out = Some(poll());
            }
        };
        context(PollOnce(&mut run));
        // `run`'s borrow of `pending`/`out` ends here, at its last use, so both
        // are readable again without an explicit drop.
        if let Some(out) = out {
            return out;
        }
        // Two different bugs reach here, and they are distinguishable — the
        // slot says which. Telling them apart matters because the second one
        // is a wrapper that did everything right except swallow the answer,
        // and blaming it for "not polling" sends the reader to the wrong line.
        if pending.0.is_none() {
            panic!(
                "frustrate: the registered async-runtime context ran the poll \
                 and then swallowed its result — it caught the unwind from a \
                 panicking future (a `catch_unwind` inside the wrapper?) and \
                 returned normally. The panic that was swallowed is gone; this \
                 call is answered with this message instead. A wrapper must \
                 let a poll's panic propagate: `drain_one` catches it and \
                 attributes it to the right call."
            );
        }
        panic!(
            "frustrate: the registered async-runtime context returned without \
             calling `PollOnce::run`. The task's call would never be answered \
             and its Dart future never settle. The task is leaked rather than \
             dropped — see runtime.rs — so this panic is the whole cost; fix \
             the wrapper to `{{ let _guard = handle.enter(); poll.run(); }}`"
        );
    }

    /// A poll that has not been run yet, which must **never be dropped**.
    ///
    /// It owns the task's `Runnable`, and dropping that drops the future — on
    /// a thread where the registered wrapper has already returned or unwound,
    /// so the runtime's context is gone. A teardown that reaches for the
    /// runtime then panics inside a destructor, and async-task wraps the
    /// future's drop in `abort_on_panic` (async-task 4.7.1 `raw.rs:447-449`):
    /// the process dies, unattributably, on a path whose whole purpose was to
    /// report a broken wrapper.
    ///
    /// So leak it instead. The leak is bounded by how many times the wrapper
    /// misbehaves, it is a bug path already ending in a loud panic, and a
    /// leaked task is recoverable where an aborted process is not.
    struct Unrun<F>(Option<F>);

    impl<F> Drop for Unrun<F> {
        fn drop(&mut self) {
            if let Some(unrun) = self.0.take() {
                std::mem::forget(unrun);
            }
        }
    }

    /// Serializes the tests that touch the global slot, which every drain in
    /// the process reads. Same stance as `post::test_lock`: unit tests run in
    /// one process, in parallel, and this is shared state.
    #[cfg(test)]
    pub(crate) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(not(target_family = "wasm"))]
pub use native::{register, unregister};

#[cfg(not(target_family = "wasm"))]
pub(crate) use native::enter;

#[cfg(all(test, not(target_family = "wasm")))]
pub(crate) use native::test_lock;

/// No context to lend on wasm: [`register`](self) is compile-absent there
/// (there is no tokio reactor to enter), so this is the identity and the
/// executor's core stays one source line on every config — the same
/// injected-seam discipline as `executor::arrange_drain`, not a `cfg` inside
/// the core.
#[cfg(target_family = "wasm")]
#[inline(always)]
pub(crate) fn enter<R>(poll: impl FnOnce() -> R) -> R {
    poll()
}

// ------------------------------------------------------- background work --

/// Run `future` to completion on frustrate's executor, tied to no bridge call.
///
/// The portable answer to "my Rust needs somewhere to put background work":
/// a per-connection loop, a retry with backoff, a subscription that outlives
/// the call that opened it. Available on **every** platform frustrate targets,
/// with one implementation — native, single-threaded web, threaded web — for
/// the reason [`crate::executor`] exists at all: a suspended task here is heap
/// data on a shared run queue, not a parked thread, so it costs the same on a
/// browser main thread as it does on a pool worker.
///
/// ```ignore
/// #[bridge]
/// impl Session {
///     #[bridge(sync)]
///     pub fn start(&self) {
///         let inbox = self.inbox.clone();
///         frustrate::runtime::spawn(async move {
///             while let Some(msg) = inbox.next().await {
///                 handle(msg);
///             }
///         });
///     }
/// }
/// ```
///
/// Callable from anywhere bridge code runs, a `#[bridge(sync)]` body on the
/// browser main thread included: it pushes one queue entry and asks the
/// Scheduler, and neither waits.
///
/// # What you give up by detaching
///
/// - **No handle, so no cancellation and no join.** Nothing can stop this task
///   from outside; give it its own stop signal (a channel, an `AtomicBool`, a
///   dropped sender) if it needs one. In particular a task spawned before a
///   Flutter **hot restart** keeps running: hot restart re-runs Dart `main()`
///   in the same process, and nothing here is re-initialised by that.
/// - **`Send`, always** — the task may be polled on any pool worker. On
///   single-threaded web, where there are no workers to be polled on, the
///   bound can be dropped: see [`spawn_local`].
/// - **A panic is reported, not delivered.** No call is waiting, so there is no
///   future to reject; the panic reaches the process panic listener
///   ([`crate::panic`]) and nothing else. On wasm (`panic = "abort"`) it traps
///   out of the drain, where the host has no call to attribute it to and
///   rethrows it into the page.
///
/// # Awaiting anything
///
/// The executor is a poll loop with no reactor of its own, so what a detached
/// task may `.await` is exactly what a bridged `async fn` may:
///
/// - [`sleep`] — the portable "wait a bit", which is what a retry loop wants;
/// - another frustrate future — a [`crate::DartFunction::call_async`] round
///   trip ([`crate::callback`]) reaches a host facility Rust has none of, so
///   long as Dart can *compute* the answer; a `locked` acquisition; a stream
///   send;
/// - on **native only**, a tokio/async-std leaf, once the embedder has lent
///   its context with [`register`](self);
/// - any future woken by a waker your own code holds, from any thread.
pub fn spawn(future: impl std::future::Future<Output = ()> + Send + 'static) {
    crate::executor::spawn_detached(future);
}

/// The installed timer: what [`sleep`] awaits.
type Timer = dyn Fn(std::time::Duration) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = ()> + Send>,
    > + Send
    + Sync
    + 'static;

static TIMER: crate::hook::Hook<Timer> = crate::hook::Hook::new();

/// Install the process's timer — what [`sleep`] is.
///
/// A timer is a **clock**, and frustrate has none: the executor is a poll loop
/// with no reactor, and on `wasm32-unknown-unknown` the module has no host to
/// ask. So this is a slot rather than an implementation, filled from wherever
/// the answer actually lives — the same shape as
/// [`crate::logging::install`] and [`crate::panic::register`], and installed
/// the same way, from a `#[bridge(sync)]` member your app calls once at
/// startup.
///
/// **Dart is the portable filling**, and the reason the slot is worth having is
/// that it is the *only* one a caller needs to know about: a crate three edges
/// down your graph can `frustrate::runtime::sleep(d).await` without a handle
/// threaded to it.
///
/// ```ignore
/// #[bridge(sync)]
/// pub fn install_timer(f: DartFunction<i64, i64>) {
///     frustrate::runtime::install_timer(move |d| {
///         let f = f.clone();
///         Box::pin(async move {
///             f.call_async(d.as_millis() as i64).await;
///         })
///     });
/// }
/// ```
/// ```dart
/// installTimer(f: (ms) async {
///   await Future.delayed(Duration(milliseconds: ms));
///   return ms;
/// });
/// ```
///
/// On native you may prefer a runtime you already have —
/// `move |d| Box::pin(tokio::time::sleep(d))` — which keeps timers off the
/// Dart isolate. Nothing here prefers one for you.
///
/// # What you are taking on
///
/// - **The future must actually resolve**, after roughly `d`. Nothing checks
///   it; a timer that never fires is a task that never wakes, which is what a
///   task legitimately awaiting Dart also looks like.
/// - **A Dart-backed timer fires only while that isolate is pumping.** Browsers
///   throttle background tabs hard — for a backoff loop that is arguably
///   correct, and for a deadline it is not. Choose accordingly, per platform;
///   this slot is where that choice goes.
/// - **Re-installing replaces.** Flutter hot restart re-runs Dart `main()` in
///   the same native process, so the install runs again against a fresh
///   isolate; the displaced closure is dropped outside the slot's critical
///   section, exactly as [`crate::panic::register`]'s is.
pub fn install_timer(
    timer: impl Fn(std::time::Duration) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = ()> + Send>,
        > + Send
        + Sync
        + 'static,
) {
    TIMER.set(std::sync::Arc::new(timer));
}

/// Drop the timer registration. [`sleep`] panics again until one is installed.
pub fn uninstall_timer() {
    TIMER.clear();
}

/// Install a timer **served by Dart**, which is the portable filling and the
/// one most apps want: Dart has a timer on every platform frustrate targets,
/// and a stock wasm32 module has none at all.
///
/// Two one-way trips, because a `DartFunction` cannot do this: the Dart closure
/// behind one answers *synchronously* (`R Function(T)`, callbacks.md), so it
/// can compute but it cannot wait. `call_async` makes the **Rust** side await a
/// round trip; it does not let the Dart side delay one. A timer therefore goes
/// out on a fire-and-forget channel and comes back through a second entry
/// point.
///
/// The consumer's half is two one-line members:
///
/// ```ignore
/// #[bridge(sync)]
/// pub fn install_timer(requests: DartCallback<(i64, i64)>) {
///     frustrate::runtime::install_dart_timer(requests);
/// }
///
/// #[bridge(sync)]
/// pub fn fire_timer(id: i64) {
///     frustrate::runtime::fire_timer(id);
/// }
/// ```
/// ```dart
/// installTimer(requests: (r) {
///   final (id, millis) = r;
///   Timer(Duration(milliseconds: millis), () => fireTimer(id: id));
/// });
/// ```
///
/// Each request is `(id, milliseconds)`. **Fire every id you are given**, once,
/// after its delay: an id whose task has since been dropped is not an error —
/// [`fire_timer`] ignores it — but an id never fired is a task that never
/// wakes.
///
/// **This pins the isolate**, and for an app that is the point: the callback
/// holds a `StreamSink`, an open stream keeps its isolate alive
/// (docs/design/streams.md), and a process that can still be asked to sleep
/// should not be exiting. Anything with an end — a test suite, a worker
/// isolate you mean to retire — calls [`uninstall_timer`] when it is done, or
/// it runs to completion and then does not exit.
pub fn install_dart_timer(requests: crate::DartCallback<(i64, i64)>) {
    install_timer(move |d| {
        let millis = i64::try_from(d.as_millis()).unwrap_or(i64::MAX);
        let id = NEXT_TIMER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let slot = std::sync::Arc::new(crate::spin::SpinLock::new(TimerSlot {
            fired: false,
            waker: None,
        }));
        PENDING.with(|p| p.insert(id, std::sync::Arc::clone(&slot)));
        requests.call((id, millis));
        Box::pin(TimerFuture { id, slot })
    });
}

/// Fire the timer `id` was issued for. Unknown or already-fired ids are
/// ignored: the host fires every id it was handed, and a task that ended (or
/// was dropped) took its registration with it.
pub fn fire_timer(id: i64) {
    let Some(slot) = PENDING.with(|p| p.remove(&id)) else {
        return;
    };
    // Bound out: waking runs the executor's schedule callback, which takes the
    // run-queue lock. Doing that inside this one is the re-entrancy
    // `executor::spawn_inner` spells out, on a lock that does not re-enter.
    let waker = slot.with(|s| {
        s.fired = true;
        s.waker.take()
    });
    if let Some(w) = waker {
        w.wake();
    }
}

struct TimerSlot {
    fired: bool,
    waker: Option<std::task::Waker>,
}

/// Timers issued and not yet fired.
///
/// A [`crate::spin::SpinLock`], for the reason `callback::INVOCATIONS` is one:
/// the browser main thread takes it — a `#[bridge(sync)]` `fire_timer` runs
/// there — and a futex-parking lock traps on that thread. Critical sections
/// here are a map insert or remove.
static PENDING: TimerRegistry = TimerRegistry(crate::spin::SpinLock::new(None));
static NEXT_TIMER: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(1);

/// The map, built lazily inside the lock so the whole thing stays `const`.
struct TimerRegistry(
    crate::spin::SpinLock<Option<std::collections::HashMap<i64, std::sync::Arc<crate::spin::SpinLock<TimerSlot>>>>>,
);

impl TimerRegistry {
    fn with<R>(
        &self,
        f: impl FnOnce(&mut std::collections::HashMap<i64, std::sync::Arc<crate::spin::SpinLock<TimerSlot>>>) -> R,
    ) -> R {
        self.0.with(|slot| f(slot.get_or_insert_with(Default::default)))
    }

    fn remove(&self, id: i64) -> Option<std::sync::Arc<crate::spin::SpinLock<TimerSlot>>> {
        self.with(|m| m.remove(&id))
    }
}

// Safety: the inner `SpinLock` is `Sync` for a `Send` payload, and the payload
// here is a map of `Arc<SpinLock<TimerSlot>>`, whose own `Waker` is `Send`.
unsafe impl Sync for TimerRegistry {}

/// The future [`install_dart_timer`] hands back: pending until the host fires.
///
/// Dropping it takes the registration with it, so a cancelled sleep leaves
/// nothing for a later `fire_timer` to find — and the host firing an id nobody
/// waits for is the documented no-op rather than a leak.
struct TimerFuture {
    id: i64,
    slot: std::sync::Arc<crate::spin::SpinLock<TimerSlot>>,
}

impl std::future::Future for TimerFuture {
    type Output = ();
    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        self.slot.with(|s| {
            if s.fired {
                std::task::Poll::Ready(())
            } else {
                s.waker = Some(cx.waker().clone());
                std::task::Poll::Pending
            }
        })
    }
}

impl Drop for TimerFuture {
    fn drop(&mut self) {
        PENDING.remove(self.id);
    }
}

/// Sleep for `duration` on the installed timer, yielding the thread.
///
/// The portable "wait a bit" a retry loop wants, on every platform frustrate
/// targets, and a *future* rather than a parked thread — so a thousand of them
/// cost a thousand heap allocations and no threads, on the browser main thread
/// included. Contrast `std::thread::sleep`, which blocks its thread even inside
/// an `async fn` and is illegal on the main thread anyway
/// ([blocking.md](../../../docs/design/blocking.md)).
///
/// ```ignore
/// let mut backoff = Duration::from_millis(50);
/// while let Err(e) = connect().await {
///     frustrate::runtime::sleep(backoff).await;
///     backoff = (backoff * 2).min(Duration::from_secs(30));
/// }
/// ```
///
/// # Panics
///
/// If no timer is installed — [`install_timer`] is not optional, and this is
/// deliberately loud rather than a sleep that silently never wakes. Uniform
/// across platforms: a native default would make the *native* build work and
/// leave web to discover the gap at run time, which is the divergence worth
/// avoiding more than the convenience is worth having.
pub async fn sleep(duration: std::time::Duration) {
    let timer = TIMER.get().expect(
        "frustrate: no timer installed — frustrate::runtime::sleep needs \
         frustrate::runtime::install_timer, called once at startup. The \
         runtime has no clock of its own: on wasm32-unknown-unknown there is \
         no host to ask, so the answer comes from Dart (Future.delayed through \
         a DartFunction) or from a native runtime you already have. See \
         install_timer's docs.",
    );
    timer(duration).await
}

/// [`spawn`], without the `Send` bound.
///
/// **Single-threaded web only, and that is not a policy — it is the whole
/// reason the bound can go.** On this target the module has no threads at all:
/// [`crate::pool`]'s wasm worker farm is behind the `wasm-threads` feature,
/// which requires the `+atomics` build this `cfg` excludes, so the executor's
/// run queue, its wakers and its drains are all one thread's. A future that
/// holds an `Rc`, a `RefCell` or a JS handle can therefore ride the same queue
/// as everything else with nothing to be raced by. Elsewhere — native, threaded
/// web — a task really is handed to a pool worker, and this function does not
/// exist rather than being a hazard you could hold wrong.
///
/// So a bridge that spawns non-`Send` work on web and `Send` work on native
/// selects between the two on the same `cfg` that already splits its
/// dependencies. Everything [`spawn`] says about detaching applies unchanged.
#[cfg(all(target_family = "wasm", not(target_feature = "atomics")))]
pub fn spawn_local(future: impl std::future::Future<Output = ()> + 'static) {
    /// The future, asserted `Send` for the run queue's sake.
    ///
    /// # Safety
    ///
    /// The assertion is that nothing ever sends it anywhere. Under this
    /// module's `cfg` the target has one thread and no way to make another:
    /// `pool::spawn` on wasm is `#[cfg(feature = "wasm-threads")]`, and that
    /// feature's worker farm needs the shared memory a `+atomics` build
    /// imports — which this `cfg` excludes — so `spawn_worker` cannot be
    /// reached and no second thread exists to observe the future, its wakers,
    /// or its output. The queue this rides is the same one, in the same
    /// instance's own linear memory; an actor's instance has a memory (and an
    /// executor) of its own and is likewise alone in it.
    struct SingleThreaded<F>(F);

    // SAFETY: see the type. No second thread exists on this target.
    unsafe impl<F> Send for SingleThreaded<F> {}

    impl<F: std::future::Future> std::future::Future for SingleThreaded<F> {
        type Output = F::Output;
        fn poll(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<F::Output> {
            // SAFETY: a structural pin projection onto the single field. This
            // wrapper is not `Unpin` (it inherits nothing), never moves out of
            // `.0`, and has no `Drop`.
            unsafe { self.map_unchecked_mut(|s| &mut s.0) }.poll(cx)
        }
    }

    crate::executor::spawn_detached(SingleThreaded(future));
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use std::task::{Context as TaskContext, Poll};
    use std::time::Duration;

    /// Unregisters on drop, so a panicking test cannot leave a wrapper
    /// installed for every other test in the process.
    struct Registered(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);
    impl Drop for Registered {
        fn drop(&mut self) {
            MISBEHAVE.with(|m| m.set(false));
            unregister();
        }
    }

    fn registered(context: impl for<'a> Fn(PollOnce<'a>) + Send + Sync + 'static) -> Registered {
        let guard = test_lock();
        register(context);
        Registered(guard)
    }

    thread_local! {
        /// Whether a *misbehaving* wrapper should misbehave on this thread.
        static MISBEHAVE: Cell<bool> = const { Cell::new(false) };
    }

    /// Register a wrapper that breaks its contract — but **only on the calling
    /// thread**.
    ///
    /// The slot is process-global by design, and `cargo test` runs the suite in
    /// one process in parallel: a globally-broken wrapper does not merely fail
    /// its own test, it skips the poll of every *other* test's drain that
    /// overlaps it. (Observed before this existed:
    /// `callback::…::call_async_throwing_closure_becomes_a_panic_envelope`
    /// failing intermittently — one test's contract violation landing in
    /// another test's envelope.) `test_lock` cannot fix that; the tests it
    /// would have to serialize against are every test that drains anything.
    ///
    /// Confining the misbehaviour to the registering thread does fix it: these
    /// tests drive their own drains synchronously, so every other thread —
    /// pool workers included — gets a plain pass-through.
    fn registered_broken(
        context: impl for<'a> Fn(PollOnce<'a>) + Send + Sync + 'static,
    ) -> Registered {
        let guard = registered(move |poll| {
            if MISBEHAVE.with(|m| m.get()) {
                context(poll);
            } else {
                poll.run();
            }
        });
        MISBEHAVE.with(|m| m.set(true));
        guard
    }

    // ------------------------------------------------------------------
    // A stand-in for tokio, faithful in the three ways that matter here.
    //
    // Not tokio itself: adding tokio even as one `[dev-dependencies]` line
    // puts it in the crate_universe hub for every triple this runtime is
    // built for, which is the cost testing.rs already measured and refused
    // (379 → 427 lock packages, into the manifest that produces an iPhone
    // dylib). The real-tokio proof lives in `tests/tokio_hook`, its own cargo
    // workspace, where that cost is nobody's.
    //
    // What it copies, because these are the properties the hook depends on:
    //   1. the context is a **thread-local**, installed by a scoped guard —
    //      like `Handle::enter`;
    //   2. a leaf **reads it at construction** and panics without one, like
    //      `tokio::time::sleep` ("there is no reactor running");
    //   3. the wake comes from a **foreign thread**, like tokio's timer wheel;
    //   4. the leaf's **Drop** reads it too. This one models a *user* teardown
    //      (`Handle::current()` to arm a timeout or spawn a cleanup task), not
    //      tokio's own `TimerEntry`, which needs no context to drop — measured
    //      in tests/tokio_hook. It records rather than panics, because a real
    //      one panicking there would abort the process.
    // ------------------------------------------------------------------

    thread_local! {
        static REACTOR: Cell<Option<&'static Reactor>> = const { Cell::new(None) };
    }

    struct Reactor {
        /// Leaves constructed, minus leaves dropped. Zero after a completion
        /// or a cancel, or the hook lost a future.
        live: AtomicU32,
        /// Set by a leaf whose `Drop` ran with no context installed. Recorded
        /// rather than asserted for the reason the property exists: a real
        /// teardown that reaches for the runtime *panics* there, and a
        /// panicking destructor during cleanup aborts the process — which
        /// would take the test binary with it instead of failing a test.
        dropped_bare: AtomicBool,
    }

    impl Reactor {
        /// Leaked because a real runtime's `Handle` is `'static`-ish in
        /// practice (a process-lived `Runtime`), and it lets the registered
        /// wrapper — which must be `'static` — name it directly.
        fn new() -> &'static Reactor {
            Box::leak(Box::new(Reactor {
                live: AtomicU32::new(0),
                dropped_bare: AtomicBool::new(false),
            }))
        }

        /// The enter-guard shape: install for a scope, restore on drop.
        fn enter(&'static self) -> EnterGuard {
            let previous = REACTOR.with(|r| r.replace(Some(self)));
            EnterGuard(previous)
        }

        /// tokio's `Handle::enter`-shaped wrapper, ready to `register`.
        fn context(&'static self) -> impl for<'a> Fn(PollOnce<'a>) + Send + Sync + 'static {
            move |poll| {
                let _guard = self.enter();
                poll.run();
            }
        }

        fn current() -> &'static Reactor {
            REACTOR
                .with(|r| r.get())
                .expect("there is no reactor running")
        }
    }

    struct EnterGuard(Option<&'static Reactor>);
    impl Drop for EnterGuard {
        fn drop(&mut self) {
            REACTOR.with(|r| r.set(self.0));
        }
    }

    /// The leaf. Like `tokio::time::sleep`, it is built eagerly — so
    /// constructing it outside a context is the loud failure, not a hang.
    struct Sleep {
        reactor: &'static Reactor,
        delay: Duration,
        armed: bool,
        fired: Arc<AtomicBool>,
    }

    impl Sleep {
        fn new(delay: Duration) -> Sleep {
            let reactor = Reactor::current();
            reactor.live.fetch_add(1, Ordering::SeqCst);
            Sleep {
                reactor,
                delay,
                armed: false,
                fired: Arc::new(AtomicBool::new(false)),
            }
        }
    }

    impl Future for Sleep {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<()> {
            // Reading the context on every poll is what makes a poll outside
            // `enter` fail rather than silently succeed.
            let _ = Reactor::current();
            if self.fired.load(Ordering::SeqCst) {
                return Poll::Ready(());
            }
            if !self.armed {
                self.armed = true;
                let fired = self.fired.clone();
                let waker = cx.waker().clone();
                let delay = self.delay;
                // The reactor's own thread — a *foreign* thread, exactly like
                // tokio's timer wheel — is what wakes our async-task waker.
                std::thread::spawn(move || {
                    std::thread::sleep(delay);
                    fired.store(true, Ordering::SeqCst);
                    waker.wake();
                });
            }
            Poll::Pending
        }
    }

    impl Drop for Sleep {
        fn drop(&mut self) {
            // Models a *user* teardown that reaches for the runtime — arming a
            // timeout, spawning a cleanup task — which is what pins the hook to
            // wrapping the whole of `run()` rather than "the poll": a cancelled
            // task's future is dropped inside that same call. (tokio's own
            // `TimerEntry` needs no context to drop; it owns its
            // `scheduler::Handle`. Measured in tests/tokio_hook.)
            if REACTOR.with(|r| r.get().is_none()) {
                self.reactor.dropped_bare.store(true, Ordering::SeqCst);
            }
            self.reactor.live.fetch_sub(1, Ordering::SeqCst);
        }
    }

    // ------------------------------------------------------------------
    // enter()'s own contract
    // ------------------------------------------------------------------

    #[test]
    fn nothing_registered_runs_the_poll_untouched() {
        let _serial = test_lock();
        unregister();
        let ran = Cell::new(0);
        let value = enter(|| {
            ran.set(ran.get() + 1);
            7
        });
        assert_eq!(value, 7);
        assert_eq!(ran.get(), 1, "the poll runs exactly once, unwrapped");
    }

    #[test]
    fn a_registered_context_wraps_the_poll_and_returns_its_value() {
        let reactor = Reactor::new();
        let _reg = registered(reactor.context());
        let seen = enter(|| REACTOR.with(|r| r.get().is_some()));
        assert!(seen, "the poll ran with the runtime's context installed");
        assert!(
            REACTOR.with(|r| r.get().is_none()),
            "and the guard restored it afterwards"
        );
    }

    fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
        payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_string()))
            .unwrap_or_else(|| "<non-string panic>".to_string())
    }

    #[test]
    fn a_context_that_never_polls_fails_loudly() {
        // `post::test_lock`, because `enter` **panics** here. The panic is
        // raised by the code under test rather than by a literal `panic!`,
        // which is exactly why it was missed: a deliberate panic is
        // process-global however it is spelled, and
        // `panic::tests::the_hook_arm_reports_the_same_report` installs the
        // web funnel as the process hook for the length of its window, so a
        // panic raised anywhere while that stands is reported into its log.
        let _hook = crate::post::test_lock();
        let _reg = registered_broken(|_poll| { /* forgets to call it */ });
        let err = std::panic::catch_unwind(|| enter(|| 1)).unwrap_err();
        let msg = panic_message(err);
        assert!(
            msg.contains("without calling `PollOnce::run`"),
            "a wrapper that never polls must name itself, not strand the call \
             silently — got {msg:?}"
        );
    }

    /// The other shape of the same bug: the wrapper panics on its way to the
    /// poll. The un-run poll unwinds through `enter`, and must be leaked there
    /// too — the abort path does not care *why* the poll never ran.
    /// A wrapper that polls but `catch_unwind`s the future's panic must be
    /// told *that*, not "you never polled". Both leave `enter` with no value
    /// to return, so the naive single message blames the wrong line — and the
    /// wrapper here did the one thing the contract asks.
    #[test]
    fn a_context_that_swallows_the_polls_panic_is_told_so() {
        let _hook = crate::post::test_lock();
        let _reg = registered_broken(|poll| {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| poll.run()));
        });
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            enter(|| panic!("the future blew up"))
        }))
        .unwrap_err();
        std::panic::set_hook(previous);
        let msg = panic_message(err);
        assert!(
            msg.contains("swallowed its result"),
            "a wrapper that polled and ate the panic must not be told it \
             never polled — got {msg:?}"
        );
    }

    #[test]
    fn a_context_that_panics_before_polling_still_leaks_rather_than_drops() {
        // Under `post::test_lock`: a deliberate panic reaches whatever
        // `std` hook stands at the time, and `panic`'s tests install the
        // web funnel as that hook — which would report this panic into
        // their log and fail their "exactly one report" assertions.
        let _serial = crate::post::test_lock();
        struct AbortsIfDropped;
        impl Drop for AbortsIfDropped {
            fn drop(&mut self) {
                panic!("dropped outside the runtime context");
            }
        }
        let _reg = registered_broken(|_poll| panic!("the wrapper itself failed"));
        let doomed = AbortsIfDropped;
        let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            enter(move || {
                let _keep = doomed;
                1
            })
        }))
        .unwrap_err();
        assert!(
            panic_message(err).contains("the wrapper itself failed"),
            "the wrapper's own panic is what reaches drain_one's catch_unwind"
        );
    }

    // "Polled twice" has no test here on purpose: `PollOnce::run` consumes,
    // so it is a compile error, pinned by the `compile_fail` doctest on
    // `PollOnce` itself (rustdoc only collects those from public items — a
    // doctest inside this `cfg(test)` module would verify nothing).

    /// The un-run poll must be **leaked, not dropped**. Dropping it would drop
    /// the task's future outside the registered context, and async-task wraps
    /// a future's destructor in `abort_on_panic` (4.7.1 `raw.rs:447-449`) — so
    /// a teardown that reaches for the runtime would abort the process on the
    /// very path whose job is to report the broken wrapper. This test would
    /// not *fail* if that regressed; the binary would die. Which is the point.
    #[test]
    fn an_un_run_poll_is_leaked_rather_than_dropped() {
        // Under `post::test_lock`: a deliberate panic reaches whatever
        // `std` hook stands at the time, and `panic`'s tests install the
        // web funnel as that hook — which would report this panic into
        // their log and fail their "exactly one report" assertions.
        let _serial = crate::post::test_lock();
        struct AbortsIfDropped;
        impl Drop for AbortsIfDropped {
            fn drop(&mut self) {
                // Stands in for `Handle::current()` in a teardown: it panics
                // when there is no context, and async-task turns a panic in a
                // future's drop into a process abort.
                panic!("dropped outside the runtime context");
            }
        }
        let _reg = registered_broken(|_poll| { /* never runs it */ });
        let doomed = AbortsIfDropped;
        let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            enter(move || {
                let _keep = doomed;
                1
            })
        }))
        .unwrap_err();
        assert!(
            panic_message(err).contains("without calling `PollOnce::run`"),
            "the broken wrapper must be what is reported"
        );
    }

    #[test]
    fn re_registering_replaces_the_wrapper() {
        // The hot-restart contract: Dart `main()` re-runs in this same
        // process, the app rebuilds its runtime, and the second registration
        // must take. Keep-first would pin the *first* handle — whose runtime
        // the restart just dropped — and tokio panics "…being shutdown" on
        // every poll through it (measured, tests/tokio_hook), so every async
        // call after a restart would fail with the correct registration
        // sitting right there, ignored.
        let first = Reactor::new();
        let second = Reactor::new();
        let _reg = registered(first.context());
        register(second.context());
        let which = enter(|| REACTOR.with(|r| r.get().unwrap() as *const Reactor));
        assert!(
            std::ptr::eq(which, second),
            "the last registration wins, so a hot restart can replace a \
             handle to a dead runtime"
        );
    }

    /// A displaced wrapper must drop **outside** the slot's lock.
    ///
    /// The end-to-end version of `hook.rs`'s
    /// `a_displaced_value_drops_outside_the_critical_section`, through this
    /// hook's own API: the value being replaced is user code — their closure
    /// and its captures — so its `Drop` can do anything, including re-enter
    /// `register`/`unregister`. Dropped inside the critical section, a capture
    /// whose `Drop` unregisters spins forever against the non-reentrant slot
    /// lock, on this thread, with no second thread involved. The realistic
    /// version is milder and worse to diagnose: a captured `Arc<Runtime>` whose
    /// last reference lands here blocks every drain in the process for the
    /// length of `Runtime::drop`'s shutdown.
    ///
    /// Bounded on purpose, exactly like `executor.rs`'s
    /// `a_displaced_task_drops_outside_the_registry_lock`: the regression is a
    /// *hang*, not a panic, so it runs on its own thread and asserts on a
    /// timeout rather than wedging the suite.
    #[test]
    fn a_displaced_context_drops_outside_the_slot_lock() {
        struct UnregistersOnDrop;
        impl Drop for UnregistersOnDrop {
            fn drop(&mut self) {
                unregister();
            }
        }

        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let _serial = test_lock();
            let captured = UnregistersOnDrop;
            register(move |poll| {
                let _keep = &captured;
                poll.run();
            });
            // Replacing it drops `captured`, whose `Drop` re-enters the slot.
            register(|poll| poll.run());
            unregister();
            let _ = tx.send(());
        });
        let arrived = rx.recv_timeout(std::time::Duration::from_secs(5));
        assert!(
            arrived.is_ok(),
            "re-registration never returned: the displaced wrapper was dropped \
             inside the slot's critical section, and its capture's Drop \
             re-entered the same non-reentrant lock"
        );
        handle.join().unwrap();
    }

    #[test]
    fn unregister_reverts_to_plain_cooperative_behaviour() {
        let reactor = Reactor::new();
        {
            let _reg = registered(reactor.context());
            assert!(enter(|| REACTOR.with(|r| r.get().is_some())));
        }
        let _serial = test_lock();
        assert!(
            enter(|| REACTOR.with(|r| r.get().is_none())),
            "after unregister the poll runs with no context at all"
        );
    }

    // ------------------------------------------------------------------
    // The executor integration — the thing the hook actually exists for
    // ------------------------------------------------------------------

    use crate::codec::FramedWriter;
    use crate::envelope::{Outcome, STATUS_OK};
    use crate::executor::Executor;

    /// Records completions; `schedule` is a no-op because these tests drive
    /// the drain explicitly (the same seam `executor.rs`'s own tests use).
    type Delivered = Arc<Mutex<Vec<(u64, Vec<u8>)>>>;

    fn recording() -> (Executor, Delivered) {
        let log: Delivered = Arc::new(Mutex::new(Vec::new()));
        let sink = log.clone();
        let exec = Executor::new(
            || {},
            move |id, reply: crate::codec::FramedWriter| {
                sink.lock().unwrap().push((id, reply.delivered_bytes()))
            },
        );
        (exec, log)
    }

    fn ok() -> Outcome {
        Outcome::Ok(FramedWriter::status(STATUS_OK))
    }

    /// The payoff, end to end on the cooperative executor: a task that awaits
    /// a leaf which *only* the registered runtime can drive suspends as heap
    /// data, is woken from a foreign thread, and completes — with no pool
    /// thread parked on it.
    #[test]
    fn a_task_awaiting_a_reactor_leaf_completes_under_a_registered_context() {
        let reactor = Reactor::new();
        let _reg = registered(reactor.context());
        let (exec, log) = recording();

        exec.spawn(1, async {
            Sleep::new(Duration::from_millis(40)).await;
            ok()
        });

        // First poll: the leaf is constructed (needs the context), arms, and
        // returns Pending. The task is now heap data — this thread is free.
        assert!(exec.drain_one());
        assert!(log.lock().unwrap().is_empty(), "suspended, not completed");
        assert_eq!(exec.task_count(), 1);

        // The reactor's thread wakes our waker, which re-enqueues the task.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !exec.drain_one() {
            assert!(
                std::time::Instant::now() < deadline,
                "the reactor's wake never reached our waker"
            );
            std::thread::yield_now();
        }
        assert_eq!(log.lock().unwrap().len(), 1, "completed on the re-poll");
        assert_eq!(log.lock().unwrap()[0].0, 1);
        assert_eq!(exec.task_count(), 0);
        assert_eq!(reactor.live.load(Ordering::SeqCst), 0, "leaf dropped");
    }

    /// Fence for `drain_one` wrapping the whole `Runnable::run()` rather than
    /// the poll: a cancelled future is dropped inside `run()` too, and its
    /// teardown needs the context.
    #[test]
    fn cancelling_a_suspended_leaf_drops_it_under_the_context() {
        let reactor = Reactor::new();
        let _reg = registered(reactor.context());
        let (exec, log) = recording();

        exec.spawn(2, async {
            Sleep::new(Duration::from_secs(30)).await;
            ok()
        });
        assert!(exec.drain_one(), "one poll: constructed, armed, Pending");
        assert_eq!(reactor.live.load(Ordering::SeqCst), 1, "leaf is live");

        // Cancel claims the answer; async-task then schedules one final,
        // no-poll runnable whose drain drops the future.
        assert!(exec.cancel(2));
        exec.drain_all();
        assert_eq!(
            reactor.live.load(Ordering::SeqCst),
            0,
            "the cancelled future was never dropped"
        );
        assert!(
            !reactor.dropped_bare.load(Ordering::SeqCst),
            "the leaf's teardown ran with no runtime context: the hook wrapped \
             the poll but not the drop, and a real teardown would have aborted \
             the process here rather than failed this assertion"
        );
        assert!(log.lock().unwrap().is_empty(), "a cancelled task never completes");
    }

    /// Proves the sensor the test above leans on actually fires. Without this,
    /// `!dropped_bare` would pass for a leaf that never checked, and the whole
    /// wrap-the-drop fence would be vacuous — the failure mode of a flag-based
    /// assertion is that the flag is simply never set.
    ///
    /// No executor here on purpose: construct under a context, drop outside
    /// one, by hand.
    #[test]
    fn the_bare_drop_sensor_fires_when_it_should() {
        let reactor = Reactor::new();
        let leaf = {
            let _guard = reactor.enter();
            Sleep::new(Duration::from_secs(1))
        };
        assert_eq!(reactor.live.load(Ordering::SeqCst), 1);
        assert!(!reactor.dropped_bare.load(Ordering::SeqCst));
        drop(leaf); // outside any context
        assert!(
            reactor.dropped_bare.load(Ordering::SeqCst),
            "the fake leaf does not notice a context-less drop, so every \
             `!dropped_bare` assertion in this file is vacuous"
        );
        assert_eq!(reactor.live.load(Ordering::SeqCst), 0);
    }

    /// The control, and the reason the hook is not merely cosmetic: with
    /// nothing registered the same task fails loudly at leaf construction and
    /// the panic is attributed to its own call — it does not hang.
    #[test]
    fn without_a_registration_the_leaf_panics_into_its_own_call() {
        // `post::test_lock` as well, and not because this test posts: it swaps
        // the **global** panic hook, and so does
        // `executor::tests::a_displaced_task_drops_outside_the_registry_lock`
        // under that lock. Two lock domains around one global means the
        // save/restore pairs can interleave and strand a silencing hook for
        // the rest of the run — every later assertion message swallowed. Take
        // both, in the one order anything here takes them (post, then runtime).
        let _hook = crate::post::test_lock();
        let _serial = test_lock();
        unregister();
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let (exec, log) = recording();
        exec.spawn(3, async {
            Sleep::new(Duration::from_millis(1)).await;
            ok()
        });
        exec.drain_all();
        std::panic::set_hook(previous);

        let log = log.lock().unwrap();
        assert_eq!(log.len(), 1, "the call is answered, not stranded");
        assert_eq!(log[0].1[0], crate::envelope::STATUS_PANIC);
        assert!(crate::codec::ByteReader::new(&log[0].1[1..])
            .read_string()
            .contains("no reactor running"));
    }

    /// A wrapper that never polls must not strand the Dart future: the panic
    /// `enter` raises is inside `drain_one`'s `catch_unwind`, so it becomes a
    /// panic envelope for exactly that call.
    #[test]
    fn a_broken_context_answers_the_call_it_broke() {
        // Both locks, post first — see the note on
        // `without_a_registration_the_leaf_panics_into_its_own_call`: the
        // global panic hook is guarded by `post::test_lock` elsewhere.
        let _hook = crate::post::test_lock();
        let _reg = registered_broken(|_poll| {});
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let (exec, log) = recording();
        exec.spawn(4, async { ok() });
        exec.drain_all();
        std::panic::set_hook(previous);

        let log = log.lock().unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].0, 4);
        assert_eq!(log[0].1[0], crate::envelope::STATUS_PANIC);
        assert!(crate::codec::ByteReader::new(&log[0].1[1..])
            .read_string()
            .contains("without calling `PollOnce::run`"));
    }

    /// The global native seam, not a test-built executor: a task spawned
    /// through `executor::spawn` drains on a **pool worker**, which is the
    /// thread that must carry the registered context.
    ///
    /// Two test locks, and **`post` before `runtime`** — the order every test
    /// in this file that takes both uses (there are four now, so this is a
    /// rule rather than a coincidence). Nothing outside this file takes
    /// `runtime::test_lock` at all, so there is no cycle to construct; keep it
    /// that way by adding new takers on this side of the order.
    // Not under `--cfg frustrate_manual_scheduler`, as the name says: with an
    // enqueue-only Scheduler there are no pool workers in the loop, and the
    // context is entered on whichever thread the host drains from. The wrapping
    // is `drain_one`'s either way — see `executor::arrange_drain`.
    #[cfg(not(frustrate_manual_scheduler))]
    #[test]
    fn the_global_executor_enters_the_context_on_its_pool_workers() {
        let _serial = crate::post::test_lock();
        let reactor = Reactor::new();
        let _reg = registered(reactor.context());

        static DONE: Mutex<Vec<(u64, Vec<u8>)>> = Mutex::new(Vec::new());
        extern "C" fn record(call_id: u64, ptr: *mut u8, len: u64, cap: u64) {
            let leased = unsafe { Vec::from_raw_parts(ptr, len as usize, cap as usize) };
            DONE.lock().unwrap().push((call_id, leased.clone()));
        }
        crate::post::init(record);
        DONE.lock().unwrap().clear();

        crate::executor::spawn(777, async {
            Sleep::new(Duration::from_millis(30)).await;
            ok()
        });

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while DONE.lock().unwrap().is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "the pool-worker drain never completed the task: the global \
                 executor is not entering the registered context"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let done = DONE.lock().unwrap();
        assert_eq!(done[0].0, 777);
        assert_eq!(done[0].1[0], STATUS_OK);
    }

    // ------------------------------------------------------------ the timer --

    /// A timer of the shape an embedder installs: a foreign thread that fires
    /// the waker, exactly like a real one (tokio's wheel, Dart's event loop).
    fn thread_timer(
    ) -> impl Fn(Duration) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync
    {
        |d: Duration| {
            Box::pin(async move {
                let fired = Arc::new(AtomicBool::new(false));
                let f = fired.clone();
                let mut armed = false;
                std::future::poll_fn(move |cx| {
                    if fired.load(Ordering::SeqCst) {
                        return Poll::Ready(());
                    }
                    if !armed {
                        armed = true;
                        let f = f.clone();
                        let waker = cx.waker().clone();
                        std::thread::spawn(move || {
                            std::thread::sleep(d);
                            f.store(true, Ordering::SeqCst);
                            waker.wake();
                        });
                    }
                    Poll::Pending
                })
                .await
            })
        }
    }

    #[test]
    fn sleep_resolves_on_the_installed_timer() {
        let _serial = test_lock();
        super::install_timer(thread_timer());
        let start = std::time::Instant::now();
        crate::pool::block_on(super::sleep(Duration::from_millis(40)));
        super::uninstall_timer();
        assert!(
            start.elapsed() >= Duration::from_millis(40),
            "sleep returned before its duration: {:?}",
            start.elapsed()
        );
    }

    /// Not installed is loud. The alternative — a `sleep` that returns
    /// `Pending` forever — is indistinguishable from a task legitimately
    /// waiting on Dart, which is the one failure nothing downstream can
    /// diagnose.
    #[test]
    fn sleeping_with_no_timer_installed_names_the_installer() {
        let _serial = test_lock();
        super::uninstall_timer();
        let panicked = std::panic::catch_unwind(|| {
            crate::pool::block_on(super::sleep(Duration::from_millis(1)))
        })
        .unwrap_err();
        let msg = panicked
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panicked.downcast_ref::<&str>().copied())
            .unwrap_or("");
        assert!(msg.contains("install_timer"), "{msg}");
    }

    /// Hot restart re-runs Dart `main()` in this same process, so the install
    /// runs again against a fresh isolate and the second one must win.
    #[test]
    fn re_installing_replaces_the_timer() {
        let _serial = test_lock();
        static WHICH: AtomicU32 = AtomicU32::new(0);
        super::install_timer(|_| {
            WHICH.store(1, Ordering::SeqCst);
            Box::pin(std::future::ready(()))
        });
        super::install_timer(|_| {
            WHICH.store(2, Ordering::SeqCst);
            Box::pin(std::future::ready(()))
        });
        crate::pool::block_on(super::sleep(Duration::from_millis(1)));
        super::uninstall_timer();
        assert_eq!(WHICH.load(Ordering::SeqCst), 2, "the first timer answered");
    }
}
