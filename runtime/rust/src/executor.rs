//! Cooperative async executor: multiplexes many futures on few threads.
//!
//! This is the shared core that lets a Rust `async fn` run on *every*
//! configuration — native, single-threaded web, and threaded web — replacing
//! the old native-only `pool::block_on`-per-call approach (which parked one
//! thread per suspended future and so could not exist on single-threaded web
//! at all). A suspended task here is heap data — a pinned future — not a
//! parked thread. The executor polls ready tasks and lets `Pending` ones sit
//! until their waker re-enqueues them.
//!
//! Two kinds ride the one run queue. A **call's** task is held in a
//! `call_id`-keyed registry, because something on the Dart side is waiting for
//! exactly one answer and that registry is the gate deciding who gives it. A
//! **detached** task ([`Executor::spawn_detached`], public as
//! [`crate::runtime::spawn`]) answers no call, so it has no entry and no cancel
//! surface: it is background work that outlives the call which started it.
//!
//! # DRY layering
//!
//! One [`Executor`] core is shared across all configs. Only three things vary,
//! and they are injected, not branched-in:
//!
//!   - the **Scheduler**: `schedule` arranges for a drain to happen. On native
//!     it hands a job to the thread pool (so ready tasks poll in parallel); on
//!     single-threaded web it schedules a `queueMicrotask(frustrate_drain)`;
//!     on threaded web it uses the pool like native. A native build carrying
//!     `--cfg frustrate_manual_scheduler` arranges nothing and waits for the
//!     host to call `frustrate_manual_drain` — see [`arrange_drain`]. The flag
//!     defaults off and that arm is absent from production builds entirely.
//!   - the **completion hook**: `complete` takes a finished task's framed
//!     reply — the writer, so the ledger of what its encode minted reaches the
//!     transport that learns whether it was taken (`codec::Minted`). In
//!     production this is [`crate::post::respond`] (already native/web
//!     abstracted); tests record what a delivery would have carried.
//!   - the **runtime context**: [`crate::runtime::enter`] wraps each
//!     `Runnable::run`, so an embedder-supplied tokio/async-std runtime can
//!     drive the leaves a task awaits. Not a per-config branch but a per-*process* one — unregistered, it is one
//!     acquire load and a direct call; on wasm it is the identity.
//!
//! The generated glue for an `async fn` is therefore byte-identical on every
//! platform: `executor::spawn(call_id, async { encode(the_fn(args).await) })`.
//!
//! # Task lifecycle
//!
//! Rather than hand-roll the waker refcounting, notified-during-run race, and
//! cancel-drops-the-future semantics (all genuinely subtle once more than one
//! thread drains the run queue), each task is an [`async_task`] task: a
//! scheduler-agnostic, dependency-light (no wasm-bindgen, `std`-only, and its
//! atomics degrade to single-threaded ops on non-atomics wasm) primitive built
//! for exactly this "bring your own `schedule`" pattern. `async_task::spawn`
//! returns a [`Runnable`] (poll it once with `run()`) and a [`Task`] join
//! handle (drop it to cancel — which drops the future). We keep the `Task` in
//! the registry for cancellation and to keep the task alive; the `Runnable`
//! rides the run queue. A detached task's handle is `detach`ed instead of
//! kept, which gives up the join and the cancel but not the task.

use crate::codec::FramedWriter;
use crate::envelope::{self, Outcome};
use async_task::{Runnable, Task};
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::Arc;

// The registry lock is `spin::Lock`, because the browser main thread may take
// it (a bridge call spawns from the main thread) and a futex-parking lock
// would trap there under contention — the same stance the pool takes
// (pool.rs). Critical sections here are tiny (push/pop a queue entry), so
// spinning is bounded by another holder's queue op, never by a poll. The
// per-target choice, and the argument for keying it on `target_feature =
// "atomics"` rather than the cargo feature, live at the type.
use crate::spin::Lock;

/// The run-queue tag a [detached](Executor::spawn_detached) task rides under.
///
/// Zero is already "no call" everywhere this id is read: it is
/// [`CURRENT_DRAIN_CALL`]'s idle value, and the host reads a zero back from
/// `frustrate_current_drain_call` as "a trap in executor plumbing rather than a
/// user poll" (`runtime_web.dart`, `_drainExecutor`). Nothing ever inserts it
/// into the registry, so the answer-once check-and-remove in `drain_one` finds
/// nothing to claim and answers nobody — which is the whole contract of a task
/// with no call behind it.
const DETACHED: u64 = 0;

/// What the registry holds for one call id.
///
/// Two states, because a call can be claimable *before* its future exists. A
/// native actor call is [`Executor::reserve`]d by `actor::submit` while it
/// sits in its host's FIFO, and becomes a task only when its deferred prefix
/// runs (actor.rs, deferred.rs). Both states are the same claim — whoever
/// removes the entry owns answering the call — which is what puts the queued
/// phase *inside* the answer-once gate instead of outside it.
enum Entry {
    /// The call is queued on an actor host; its future does not exist yet.
    /// Dropping this drops nothing: the queued job (and the request bytes it
    /// captured) is dropped by its host, which skips a call whose reservation
    /// is gone.
    Reserved,
    /// The task. Never read — held so that *dropping* it drops the future,
    /// which is async_task's cancellation and the only thing this handle is
    /// for here (nothing joins on it: a task's output is the envelope it
    /// already delivered).
    Live(#[allow(dead_code)] Task<()>),
}

/// The executor's mutable state.
struct Inner {
    /// Live calls by call id. Holding the `Task` keeps the future alive and
    /// gives us cancellation (dropping it drops the future). An entry is
    /// removed by whichever answer path claims it first — completion, panic
    /// attribution, a cancel, or the actor host loop answering a call inline;
    /// that check-and-remove IS the answer-once gate (see `spawn`).
    tasks: HashMap<u64, Entry>,
    /// Runnables waiting to be polled, each tagged with its call id so a
    /// finished task can be reaped and a panicking poll attributed.
    ready: VecDeque<(u64, Runnable)>,
}

struct Shared {
    inner: Lock<Inner>,
    /// Arrange for a drain to happen (the per-config Scheduler seam).
    schedule: Box<dyn Fn() + Send + Sync>,
    /// Deliver a finished task's envelope to the host (`post::respond` in
    /// production). Takes the framed writer rather than its bytes, so the
    /// ledger of what the reply minted reaches the transport, which is the only
    /// frame that learns whether the answer was taken (`codec::Minted`).
    complete: Box<dyn Fn(u64, FramedWriter) + Send + Sync>,
}

/// A cooperative executor. Cheap to clone the handle (`Arc` inside); the
/// production instance is a process/page global, but tests make their own.
pub struct Executor {
    shared: Arc<Shared>,
}

impl Executor {
    /// Build an executor with a `schedule` seam and a `complete` hook.
    pub fn new(
        schedule: impl Fn() + Send + Sync + 'static,
        complete: impl Fn(u64, FramedWriter) + Send + Sync + 'static,
    ) -> Self {
        Executor {
            shared: Arc::new(Shared {
                inner: Lock::new(Inner {
                    tasks: HashMap::new(),
                    ready: VecDeque::new(),
                }),
                schedule: Box::new(schedule),
                complete: Box::new(complete),
            }),
        }
    }

    /// Spawn `body` (an `async fn` body, evaluated to a future) under `call_id`.
    /// When it resolves to an [`Outcome`], its encoded envelope is delivered
    /// through the completion hook; a panic while polling becomes a panic
    /// envelope for the same call (native only — under wasm's panic=abort a
    /// trap is caught by the host JS frame instead, exactly as elsewhere).
    ///
    /// The `Send` bound is about crossing to a pool worker, and nothing else.
    /// It used to double as an accidental refusal of every `async fn` on a
    /// `locked` type — a generated wrapper takes its guard and then awaits the
    /// user body, and `std`'s guards are `!Send`, so the future was too. That
    /// was never a design: it rejected even a shared read with no `.await` in
    /// its body, because the generated block's own await was enough. The
    /// `locked` model now uses a lock whose guards are `Send` and whose
    /// acquisition awaits (`handle::LockedCell`), so the bound refuses only
    /// what it is for.
    pub fn spawn(&self, call_id: u64, body: impl Future<Output = Outcome> + Send + 'static) {
        self.spawn_inner(call_id, body, false);
    }

    /// Reserve `call_id` before its future exists.
    ///
    /// The answer-once claim for a call that is still queued somewhere else:
    /// `actor::submit` reserves before it enqueues, so a cancel arriving while
    /// the call waits in its host's FIFO claims it exactly as it would claim a
    /// suspended task — and the host, finding the reservation gone, drops the
    /// job instead of running it. [`spawn_reserved`](Self::spawn_reserved) is
    /// the other end: it turns the reservation into the task under this same
    /// lock, so there is no instant in which neither holds the call.
    pub fn reserve(&self, call_id: u64) {
        // Displaced outside the lock for the reason `spawn` spells out: a
        // displaced `Entry::Live`'s drop re-enters this lock through the
        // schedule callback. Nothing legitimate displaces here (ids are never
        // reused), which is what the debug_assert says.
        let displaced = self
            .shared
            .inner
            .with(|inner| inner.tasks.insert(call_id, Entry::Reserved));
        debug_assert!(
            displaced.is_none(),
            "call id {call_id} was already live: the answer-once gate is \
             broken upstream of the executor"
        );
        drop(displaced);
    }

    /// Whether the registry still holds `call_id` — a **peek, not a claim**.
    ///
    /// Sound as a skip test only because an entry never comes back: ids are
    /// never reused and nothing re-inserts one, so `false` means "claimed, and
    /// permanently". The actor host loop reads it to decide that a queued job
    /// must not run at all (actor.rs).
    pub fn holds(&self, call_id: u64) -> bool {
        self.shared
            .inner
            .with(|inner| inner.tasks.contains_key(&call_id))
    }

    /// Spawn `body` **only if `call_id`'s reservation is still there**, and
    /// return whether it was.
    ///
    /// The hand-off a deferred actor prefix makes (deferred.rs): the
    /// reservation [`reserve`](Self::reserve) took at submit becomes this
    /// task, swapped under one registry lock so a concurrent cancel either
    /// claims the reservation (and this refuses to spawn) or claims the task
    /// (and the completion hook never fires) — never neither.
    ///
    /// `false` means a cancel claimed the call while the prefix was running.
    /// The body is then dropped **un-polled**, here, on the caller's thread:
    /// unlike a cancelled task's future — which drops on a later drain — this
    /// one was never handed to the executor, so there is no drain to defer it
    /// to. The caller is an actor host thread inside the arm's `catch_unwind`,
    /// which is where a panicking `Drop` in the body's captures belongs.
    pub fn spawn_reserved(
        &self,
        call_id: u64,
        body: impl Future<Output = Outcome> + Send + 'static,
    ) -> bool {
        self.spawn_inner(call_id, body, true)
    }

    fn spawn_inner(
        &self,
        call_id: u64,
        body: impl Future<Output = Outcome> + Send + 'static,
        reserved: bool,
    ) -> bool {
        // Fold completion into the task: its output is the delivered side
        // effect, not a value we join on (which would need a second future to
        // drive). A panic before this point is caught in `drain_one`.
        let for_complete = self.shared.clone();
        let future = async move {
            let outcome = body.await;
            // The answer-once gate. Every path that answers this call —
            // this completion, `drain_one`'s panic attribution, and
            // [`Executor::cancel`] — does a check-and-remove of the same
            // registry entry under the same lock, so exactly one of them ever
            // owns the answer. The case this decides: a `cancel` racing a
            // completion mid-poll (the canceller then owns failing the call,
            // and this completion must NOT also respond). Claiming here is
            // also what reaps a finished task from the registry.
            //
            // `claimed` is this task's own join handle; dropping it while the
            // task is running is a flag-only operation in async_task (the
            // poll in progress finishes), and it happens outside the lock.
            let claimed = for_complete.inner.with(|inner| inner.tasks.remove(&call_id));
            if claimed.is_some() {
                (for_complete.complete)(call_id, envelope::encode(outcome));
            } else {
                // The canceller owns the answer, so this reply goes nowhere —
                // but the body already ran, so any handle it returns is already
                // registered and no Dart wrapper will ever be built to dispose
                // it. Give those objects back here; nothing else can.
                //
                // No catch, unlike the actor host's twin: freeing runs a user
                // `Drop`, but this future is polled inside `drain_one`'s
                // `catch_unwind` below, which attributes such a panic (finding
                // the entry already claimed) and leaves the thread alive.
                outcome.reclaim();
            }
        };
        let for_schedule = self.shared.clone();
        let schedule = move |runnable: Runnable| {
            for_schedule
                .inner
                .with(|inner| inner.ready.push_back((call_id, runnable)));
            (for_schedule.schedule)();
        };
        let (runnable, task) = async_task::spawn(future, schedule);
        // Bind what `insert` displaces and let it die OUTSIDE the lock. This is
        // not defensive style; dropping it here would hang the thread. For a
        // task that is neither SCHEDULED nor RUNNING, `Task::drop` calls the
        // schedule callback *synchronously* so the executor drops the future
        // (async-task 4.7.1, task.rs:213-215) — and the `schedule` closure
        // built just above takes this same lock, which does not re-enter. A
        // displaced entry dropped inside the critical section would spin
        // forever, on this thread, with no user code involved.
        //
        // `reserved` makes the insert conditional, and the condition is read
        // under the same lock as the insert — that atomicity is the whole
        // point of `spawn_reserved`. `held` carries the task out of the
        // closure when it did not go in.
        let mut held = Some(task);
        let displaced = self.shared.inner.with(|inner| {
            if reserved && !inner.tasks.contains_key(&call_id) {
                return None;
            }
            inner
                .tasks
                .insert(call_id, Entry::Live(held.take().expect("inserted once")))
        });
        let spawned = held.is_none();
        debug_assert!(
            reserved || displaced.is_none(),
            "call id {call_id} was already live: the answer-once gate is \
             broken upstream of the executor"
        );
        drop(displaced);
        if !spawned {
            // A cancel claimed the reservation while the prefix was running,
            // so it owns the answer and this future must never be polled.
            // Dropping the `Runnable` drops the future in place and schedules
            // nothing (async-task 4.7.1, runnable.rs:919); the join handle
            // then finds the task CLOSED and takes no branch of its own
            // (task.rs:193). Outside the lock, exactly like `displaced`.
            drop(runnable);
            drop(held);
            return false;
        }
        // Enqueue the first poll (through `schedule`, so the Scheduler wakes).
        runnable.schedule();
        true
    }

    /// Spawn a task that answers **no call**: background work that outlives
    /// the bridge call which started it (or that no bridge call started).
    ///
    /// The difference from [`spawn`](Self::spawn) is the whole of it. A call's
    /// task is registered under its `call_id`, because something on the Dart
    /// side is waiting for exactly one answer and the registry is the gate
    /// that decides who gives it. A detached task has no answer to give, so it
    /// has no entry, no cancel surface, and nothing to reap: it is handed to
    /// async_task with the join handle immediately detached, and it lives
    /// until its future returns `Ready` or until nothing holds a waker for it.
    ///
    /// It shares the run queue — and therefore the Scheduler — with every
    /// call's task. On native its polls land on pool workers, on
    /// single-threaded web on the microtask drain, so a detached task is
    /// multiplexed exactly like a call's and parks no thread while it waits.
    ///
    /// **A panic is reported, not delivered.** `drain_one` finds no registry
    /// entry for [`DETACHED`] and so posts nothing, but it still builds the
    /// panic envelope, which is what tells the process panic listener
    /// ([`crate::panic`]). There is no future to reject, so being told is the
    /// only outcome available; silence is the one that would be wrong.
    ///
    /// Callable from any thread that may run bridge code, including the
    /// browser main thread inside a `#[bridge(sync)]` body: it takes the
    /// registry lock for one queue push and then asks the Scheduler, neither
    /// of which waits.
    pub fn spawn_detached(&self, body: impl Future<Output = ()> + Send + 'static) {
        let for_schedule = self.shared.clone();
        let schedule = move |runnable: Runnable| {
            for_schedule
                .inner
                .with(|inner| inner.ready.push_back((DETACHED, runnable)));
            (for_schedule.schedule)();
        };
        let (runnable, task) = async_task::spawn(body, schedule);
        // Enqueue first, then detach.
        //
        // `detach` is not `drop`. Dropping a `Task` *cancels*, and cancelling a
        // task that is neither SCHEDULED nor RUNNING calls the schedule
        // callback synchronously so the executor drops the future
        // (async-task 4.7.1, `Task::set_canceled`, task.rs:207-213) — the
        // re-entrancy `spawn_inner` above has to bound its lock against.
        // `set_detached` (task.rs:230-…) schedules nothing on any path; it
        // only gives up the handle's reference, so a detached task keeps
        // running and frees itself when it completes.
        //
        // This order is async-task's own fast path: `set_detached` opens with a
        // single compare-exchange against `SCHEDULED | TASK | REFERENCE`,
        // "optimistically assume the `Task` is being detached just after
        // creating the task". Detaching first is sound too, and takes the loop.
        runnable.schedule();
        task.detach();
    }

    /// Poll one ready task, if any. Returns whether a task was run — so
    /// [`drain_all`](Self::drain_all) knows when the queue is empty.
    pub fn drain_one(&self) -> bool {
        let popped = self.shared.inner.with(|inner| inner.ready.pop_front());
        let Some((call_id, runnable)) = popped else {
            return false;
        };
        // Record the call being polled so a panic stays attributable even when
        // `catch_unwind` cannot run: on wasm (`panic=abort`) the poll traps
        // instead of unwinding, the `Err` branch below is skipped, and the trap
        // escapes to the host's `frustrate_drain` frame — which reads
        // `frustrate_current_drain_call` to reject exactly this call's future
        // (the single-threaded-web analogue of the pool's `frustrate_current_call`,
        // pool.rs). Native/threaded web unwind and never read it; the write is
        // uniform and harmless there.
        CURRENT_DRAIN_CALL.with(|c| c.set(call_id));
        // `run()` polls the future once. It re-schedules itself internally if
        // woken during the poll (the notified-during-run race async_task owns),
        // so we never lose a wake. A panic in the user future propagates out of
        // `run()`; catch it and attribute it to this call.
        //
        // [`crate::runtime::enter`] wraps the whole `run()` and not just the
        // poll: `run()` also drops a cancelled future, whose teardown needs
        // the context too (`runtime::tests::
        // cancelling_a_suspended_leaf_drops_it_under_the_context`).
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::runtime::enter(|| runnable.run())
        })) {
            // Nothing to reap on the ordinary path: a completing poll claimed
            // its own registry entry (the answer-once gate in `spawn`), and a
            // cancelled task's entry was claimed by the cancel.
            Ok(_woken) => {}
            Err(panic) => {
                // Same gate as completion: attribute the panic only if the
                // call is still ours to answer. A concurrent cancel that
                // claimed the entry owns the answer — and its claim is also
                // what keeps this envelope from double-responding.
                let claimed = self.shared.inner.with(|inner| inner.tasks.remove(&call_id));
                // Enveloped on **both** branches, posted only on the claimed
                // one. `panic_envelope` is where the panic listener is told
                // and a panic whose call somebody else already claimed is still a panic: dropping it
                // here was the one place this runtime caught a panic and then
                // said nothing about it anywhere. A future that cancels its own
                // call and then panics reaches it deterministically. The cost
                // on that branch is an envelope nobody reads, on a path that
                // has already panicked. It mints nothing — a panic envelope is
                // a string — so there is no ledger to discharge when it is
                // dropped unposted.
                let reply = envelope::panic_envelope(panic);
                if claimed.is_some() {
                    (self.shared.complete)(call_id, reply);
                }
            }
        }
        // Cleared on every path that unwinds (native/threaded web), including a
        // caught panic. On a wasm trap this line is never reached, leaving the
        // id set for the host to read after catching the trap; the next
        // `drain_one` overwrites it before its own poll, so the stale window is
        // exactly "between the trap and its attribution".
        CURRENT_DRAIN_CALL.with(|c| c.set(0));
        true
    }

    /// Poll ready tasks until the run queue is empty. This is the web
    /// microtask entry (`frustrate_drain`): self-driving futures complete
    /// within one drain; a future awaiting an external event returns `Pending`
    /// without re-enqueuing, so the drain ends and the JS event loop runs —
    /// the task resumes on a later microtask when its waker fires.
    ///
    /// Returns whether anything ran. False means the queue was already empty —
    /// "quiet", the only signal a host driving its own drains has that the Rust
    /// side is not merely between wakes. The web entry discards it, since a
    /// microtask that finds nothing has nothing to decide.
    pub fn drain_all(&self) -> bool {
        let mut ran = false;
        while self.drain_one() {
            ran = true;
        }
        ran
    }

    /// Cancel a live call. A `Runnable` for it still sitting in the run queue
    /// becomes an inert no-op when drained.
    ///
    /// Returns whether the entry was still there to claim. `true` means this
    /// cancel owns the call's answer — the task's completion hook will never
    /// run (the answer-once gate in [`spawn`](Self::spawn) finds the entry
    /// gone), even if the future was mid-poll on another thread when the claim
    /// landed. `false` means the call was already answered (or already
    /// cancelled) and the caller must leave it alone.
    ///
    /// This is the gate's one check-and-remove, so it is also what the actor
    /// host loop calls to take ownership of an answer it is about to post
    /// (actor.rs) — the claimer is not always a canceller.
    ///
    /// What the claim releases depends on which state it took. A live task's
    /// future is dropped by async_task on the next drain of its final runnable
    /// — a pool thread on native, inside `drain_one`'s `catch_unwind` — never
    /// on the canceller's thread, which is what makes this safe to call from a
    /// finalizer or an `extern "C"` frame. An [`Entry::Reserved`] has no future
    /// to drop yet; its queued job is dropped by its actor host, which skips a
    /// call whose reservation is gone.
    pub fn cancel(&self, call_id: u64) -> bool {
        let claimed = self.shared.inner.with(|inner| inner.tasks.remove(&call_id));
        claimed.is_some()
    }

    /// Number of live calls — unfinished, unclaimed tasks, plus reservations
    /// for actor calls still queued. Test/introspection support: a leak check
    /// after a drain expects zero.
    pub fn task_count(&self) -> usize {
        self.shared.inner.with(|inner| inner.tasks.len())
    }
}

// -------------------------------------------------- global + Scheduler seam --

// Manual scheduling is native-only, and setting it on a wasm build is a mistake
// worth a compile error rather than a silent no-op: on web the host *already*
// drives the drain — single-threaded web through
// `queueMicrotask(frustrate_drain)`, an actor instance through its worker pump —
// so the determinism the flag buys on native is what web already has. A
// silently-ignored build flag is the failure mode this repo does not ship.
#[cfg(all(target_family = "wasm", frustrate_manual_scheduler))]
compile_error!(
    "frustrate_manual_scheduler is native-only. On web the host already drives \
     the drain (queueMicrotask(frustrate_drain), or an actor's worker pump), so \
     task order is already the host's to determine. Drop the --cfg from wasm \
     builds."
);

/// Arrange for a drain to happen. This is the *only* per-config code in the
/// executor — the Scheduler seam. The core above is byte-identical everywhere.
///
///   - **native**: hand a single-task drain to the thread pool, so ready tasks
///     poll in parallel across worker threads. Multiplexing *and* parallelism,
///     reusing the existing pool (no second thread farm).
///   - **threaded web**: the same — pool workers drain in parallel.
///   - **single-threaded web**: the only thread is the caller's, and it must
///     never synchronously block against the JS event loop. Ask the host to
///     `queueMicrotask(frustrate_drain)` through the `frustrate.schedule_drain`
///     import; the task resumes on that microtask, having yielded to the loop.
///   - **native under `--cfg frustrate_manual_scheduler`**: arrange nothing and
///     wait for the host — see the manual arm below.
#[cfg(all(not(target_family = "wasm"), not(frustrate_manual_scheduler)))]
fn arrange_drain() {
    crate::pool::spawn(|| {
        global().drain_one();
    });
}

/// The third Scheduler: enqueue only, and let the host say when to poll.
///
/// # Manual scheduling
///
/// Under `--cfg frustrate_manual_scheduler` a spawn or a wake puts the task on
/// the run queue and stops there — no pool job, so no worker picks it up. The
/// queue moves only when the host calls [`frustrate_manual_drain`], which polls
/// on the *calling* thread. That makes the interleaving of Rust tasks with host
/// events the host's to choose rather than the pool scheduler's, which is what
/// a seeded, replayable simulation needs.
///
/// **This exists only in a build that asked for it.** The flag defaults off and
/// this arm is absent from every production configuration, so the cost is not
/// "one predictable branch" — it is nothing at all, and the export below is
/// likewise not in the table. That is the standing requirement for a test-only
/// affordance here, and a compile-time seam is the only way to actually meet it:
/// `arrange_drain` runs on every spawn and every wake, so a runtime switch would
/// be a load on the hottest path in the executor, paid forever, to serve a test.
///
/// **`DartFunction::call` is forbidden inside a manual drain, and already
/// refuses.** It blocks the calling thread until Dart answers (callback.rs), so
/// a drain running on the Dart thread would deadlock against the closure it is
/// waiting for. The assert there — `IS_WORKER`, false on any thread that is not
/// a pool worker or an actor — fires first and names the deadlock. Host I/O
/// reached from a manually drained task must therefore be request-out /
/// answer-in: a `DartCallback`, or a `StreamSink` event answered by a later
/// bridged method call. Nothing about this arm weakens that contract; it is why
/// the arm does not try to.
///
/// **What this does not cover: actors.** A native actor host is its own OS
/// thread draining its own FIFO (actor.rs), not a client of this Scheduler, so
/// an actor call's timing is unaffected by the flag and a build that uses
/// actors is not thereby deterministic. What *is* affected is the executor
/// work an actor's deferred method spawns — that rides this run queue like any
/// other task, so its future's drop lands on a host drain rather than promptly.
/// The tests asserting the prompt form are gated to the pool build and named
/// there.
#[cfg(all(not(target_family = "wasm"), frustrate_manual_scheduler))]
fn arrange_drain() {}

/// True when this wasm instance is an actor's executor — the worker pump
/// marks it (`frustrate_mark_actor_instance`) before dispatching anything.
///
/// The threaded-web Scheduler has to branch on it: an actor instance has no
/// pool of its own — `spawn_worker` from one is a bridge bug the glue
/// enforces with a throwing import — so its drains must ride the worker's
/// microtask queue exactly as single-threaded web's do. Before deferred
/// actor methods nothing inside an actor instance ever spawned on the
/// executor, and the wrong Scheduler was unreachable.
#[cfg(target_family = "wasm")]
static ACTOR_INSTANCE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Mark this wasm instance as an actor's executor. Called once by the worker
/// pump, between instantiation and the first dispatched call.
// `frustrate_block_check` gating, here and on the three exports below: see the
// "block-check export gating" note in lib.rs.
#[cfg(target_family = "wasm")]
#[cfg(not(frustrate_block_check))]
#[no_mangle]
pub extern "C" fn frustrate_mark_actor_instance() {
    ACTOR_INSTANCE.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Host hook: `queueMicrotask(() => frustrate_drain())`. Legal on the
/// browser main thread (no `Atomics.wait`), and provided by every
/// instantiation site — the main transport and the actor pump alike.
#[cfg(target_family = "wasm")]
fn schedule_drain_microtask() {
    #[link(wasm_import_module = "frustrate")]
    extern "C" {
        fn schedule_drain();
    }
    unsafe { schedule_drain() };
}

/// **The two arms are exhaustive, and a `#[bridge(no_block)]` claim on a
/// non-actor `async fn` rests on which one it takes.** Such a claim is settled
/// by placement: `spawn`
/// constructs the future on the caller and hands its *first* poll here, so
/// every poll of the body — and the drop of a cancelled one, which async_task
/// also performs inside `runnable.run()` — happens wherever this routes. On a
/// non-actor threaded instance that is the pool, only, with no fallback.
///
/// The main-thread microtask arm is reachable **only** for an actor instance,
/// where waiting is legal, and the host never drains the threaded main
/// instance at all (`runtime_web.dart`, `_drain` — the export is installed for
/// the single-threaded runtime and for actor pumps, never for this one). Route
/// a non-actor threaded drain through `schedule_drain_microtask` and every such
/// claim becomes a false green in one line, with nothing to catch it: no check
/// root reaches a dispatched body.
#[cfg(all(target_family = "wasm", feature = "wasm-threads"))]
fn arrange_drain() {
    // Main (threaded) instance: ready tasks poll in parallel on the pool.
    // Actor instance: same compiled module, no pool — microtask drains.
    if ACTOR_INSTANCE.load(std::sync::atomic::Ordering::Relaxed) {
        schedule_drain_microtask();
    } else {
        crate::pool::spawn(|| {
            global().drain_one();
        });
    }
}

#[cfg(all(target_family = "wasm", not(feature = "wasm-threads")))]
fn arrange_drain() {
    schedule_drain_microtask();
}

/// Settle the global executor on the calling thread.
///
/// Called by `frustrate_web_init`, which the main thread runs as the last step
/// of module setup (`runtime_web.dart`) — before the first bridged call, and
/// therefore before any pool worker exists, since a worker is only ever spawned
/// from `pool::spawn`. That makes "the main thread initialises this" structural
/// instead of a property to re-derive from the call graph.
///
/// Why it is worth a line: it means the initialisation race in [`ExecCell`]
/// never runs. `RaceOnce`'s trade is that threads arriving together each build
/// an `Executor` and all but one throw theirs away; a cell already published
/// before a second thread exists has no race to lose. (This used to be load
/// bearing for a stronger reason — the cell was an `OnceLock`, whose cold path
/// could put the browser main thread into `Once`'s futex, the failure
/// `pool::QUEUE` records. That hazard is gone from the primitive itself.)
///
/// Idempotent, and safe to call from an actor instance's pump too: that
/// instance has its own memory, so it is initialising its own `EXEC`.
#[cfg(target_family = "wasm")]
pub(crate) fn init_global() {
    let _ = global();
}

/// The cell the global executor lives in.
///
/// `OnceLock` wherever the target has no wait instruction; [`crate::spin::RaceOnce`]
/// on wasm with `+atomics`. Same seam and same key as [`Lock`], for the same
/// reason: `OnceLock::get_or_init` parks on its cold path, and where parking
/// lowers to `memory.atomic.wait32` the browser main thread traps instead of
/// waiting. The generalisation of `callback::INVOCATIONS`, which is const-built
/// rather than behind a `OnceLock` and says so; an `Executor` owns boxed
/// closures and cannot be const-built, so it needs a cell that initialises
/// without waiting rather than no cell at all.
///
/// Where parking is legal, `OnceLock` is the better primitive and stays: it
/// really does run its initialiser once, whereas `RaceOnce` may build a value
/// per racing thread and publish one. That cost is worth paying only to buy
/// something, and off `+atomics` there is nothing to buy.
///
/// A type alias rather than two `global()` bodies — the per-config choice is
/// which primitive, never what the function does.
#[cfg(not(all(target_family = "wasm", target_feature = "atomics")))]
type ExecCell = std::sync::OnceLock<Executor>;

#[cfg(all(target_family = "wasm", target_feature = "atomics"))]
type ExecCell = crate::spin::RaceOnce<Executor>;

/// The process/page-global executor. Its completion hook is
/// [`crate::post::respond`] (already native/web abstracted); its Scheduler is
/// [`arrange_drain`].
fn global() -> &'static Executor {
    static EXEC: ExecCell = ExecCell::new();
    EXEC.get_or_init(|| {
        Executor::new(arrange_drain, |call_id, bytes| {
            crate::post::respond(call_id, bytes)
        })
    })
}

/// Spawn an `async fn` body on the global executor under `call_id`. The
/// generated glue for a Rust `async fn` calls exactly this, byte-identically
/// on every platform: `executor::spawn(call_id, async { encode(fn(a).await) })`.
pub fn spawn(call_id: u64, body: impl Future<Output = Outcome> + Send + 'static) {
    global().spawn(call_id, body);
}

/// Spawn background work on the global executor under no call. See
/// [`Executor::spawn_detached`]; [`crate::runtime::spawn`] is the public name.
pub fn spawn_detached(body: impl Future<Output = ()> + Send + 'static) {
    global().spawn_detached(body);
}

/// Reserve `call_id` on the global executor before its future exists. The
/// native actor host calls this as it enqueues a call; see
/// [`Executor::reserve`].
pub fn reserve(call_id: u64) {
    global().reserve(call_id);
}

/// Whether the global executor still holds `call_id` — a peek, not a claim.
/// See [`Executor::holds`].
pub fn holds(call_id: u64) -> bool {
    global().holds(call_id)
}

/// Turn `call_id`'s reservation into its task on the global executor, or
/// refuse if a cancel already claimed it. See [`Executor::spawn_reserved`];
/// the deferred prefix's hand-off (deferred.rs) is the only caller.
pub fn spawn_reserved(call_id: u64, body: impl Future<Output = Outcome> + Send + 'static) -> bool {
    global().spawn_reserved(call_id, body)
}

/// Claim a call on the global executor. See [`Executor::cancel`] for the claim
/// contract (`true` = the caller now owns answering the call) and for what the
/// claim releases in each state. Used by the native actor runtime to cancel
/// deferred completions at `dispose()`/reap and to gate the answers its host
/// loop posts (actor.rs), and by [`frustrate_call_cancel`] for the Dart-side
/// token.
pub fn cancel(call_id: u64) -> bool {
    global().cancel(call_id)
}

/// Cancel one in-flight bridged `async fn` call from the Dart side — the
/// transport half of `FrustrateCancelToken` (runtime/dart).
///
/// Returns `1` when this cancel **claimed** the call and `0` when it did not —
/// [`Executor::cancel`]'s contract verbatim. Safe for any id: an unknown or
/// already-answered call is `0`, so a cancel racing a completion is a legal race
/// rather than an error.
///
/// Nothing user-written runs on the caller's thread — the future is dropped on a
/// later drain — which is what makes this callable from the browser main thread,
/// where the registry lock is already the non-parking `spin::SpinLock` (see
/// [`Lock`]) for exactly the reason `frustrate_stream_cancel` is (stream.rs).
///
/// Both claimable shapes reach it here: a Rust `async fn` body, which is a task,
/// and a native actor call, which is [`Executor::reserve`]d from the moment it
/// is enqueued (actor.rs). A plain `#[bridge]` fn dispatched async is neither —
/// it runs to completion on a pool worker — so codegen emits no cancel surface
/// for one and this export would simply answer `0`.
#[cfg(not(frustrate_block_check))]
#[no_mangle]
pub extern "C" fn frustrate_call_cancel(call_id: u64) -> u8 {
    cancel(call_id) as u8
}

/// Drain the global executor's run queue.
///
/// On single-threaded web this is the cooperative resume point: the host calls
/// it from the `queueMicrotask` that [`arrange_drain`] requested, so a task
/// that returned `Pending` (having yielded to the JS event loop) gets polled
/// again once its waker has re-enqueued it. Present on every build so the
/// export table is uniform; on native / threaded web the pool drives drains
/// and nothing calls this.
#[cfg(not(frustrate_block_check))]
#[no_mangle]
pub extern "C" fn frustrate_drain() {
    global().drain_all();
}

/// Run every runnable task to completion on the calling thread, and report
/// whether anything ran. 1 = polled at least one task, 0 = the queue was
/// already empty ("quiet").
///
/// The host's drain entry under [manual scheduling](arrange_drain); absent from
/// every other build, so a production export table is unchanged by this feature
/// existing. Detached tasks ([`Executor::spawn_detached`], public as
/// [`crate::runtime::spawn`]) drain here too — they ride the same run queue as
/// call tasks, so "everything runnable" means both without doing anything extra.
///
/// Quiet is not the same as finished: a task awaiting a host answer is not on
/// the run queue, so a drain that reports 0 means "nothing to poll *right now*",
/// which is exactly the signal a simulation loop needs to decide it should
/// advance its own clock or deliver the next queued event. A task suspended
/// forever on an answer nobody will send also reports quiet; noticing that is
/// the harness's job, not this function's.
///
/// Re-entrancy is the caller's to avoid: calling this from inside a task being
/// polled by it would poll the run queue from within a poll. Nothing here
/// detects that, because the manual arm exists for a harness that drives drains
/// from one place.
#[cfg(frustrate_manual_scheduler)]
#[no_mangle]
pub extern "C" fn frustrate_manual_drain() -> u8 {
    global().drain_all() as u8
}

thread_local! {
    /// The call id [`Executor::drain_one`] is currently polling on this thread
    /// (0 when none). Set before each poll and cleared after; on a wasm trap
    /// (`panic=abort`) the clear is skipped, so it holds the trapping call for
    /// the host to read. See [`frustrate_current_drain_call`].
    static CURRENT_DRAIN_CALL: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// The call id of the task this thread is currently polling (0 if none).
///
/// The single-threaded-web host reads this after catching a poll trap (a Rust
/// panic under `panic=abort`, which bypasses the executor's `catch_unwind`) so
/// the panic rejects exactly the awaiting future — the executor analogue of the
/// pool's `frustrate_current_call` (pool.rs). Present on every build so the
/// export table is uniform; native / threaded web unwind in-band and never call
/// it.
#[cfg(not(frustrate_block_check))]
#[no_mangle]
pub extern "C" fn frustrate_current_drain_call() -> u64 {
    CURRENT_DRAIN_CALL.with(|c| c.get())
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use super::*;
    use crate::codec::FramedWriter;
    use crate::envelope::STATUS_OK;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Mutex;
    use std::task::{Context, Poll};

    /// Records completions so a test can assert what was delivered. Not
    /// scheduling anything itself — tests drive the drain explicitly, so
    /// `schedule` is a no-op.
    /// What `recording`'s completion hook appends to: `(call_id, envelope)`
    /// per delivered answer.
    type Delivered = Arc<Mutex<Vec<(u64, Vec<u8>)>>>;

    fn recording() -> (Executor, Delivered) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let sink = log.clone();
        // The hook stands in for the transport, so it discharges the ledger the
        // way a delivery does: whatever the reply minted is the far side's now.
        let exec = Executor::new(
            || {},
            move |id, reply: FramedWriter| {
                sink.lock().unwrap().push((id, reply.delivered_bytes()))
            },
        );
        (exec, log)
    }

    // Spelled out rather than `async fn` on purpose: the explicit
    // `+ Send + 'static` is a compile-time assertion that these bodies satisfy
    // what `Executor::spawn` requires, which an `async fn`'s anonymous future
    // would leave implicit.
    #[allow(clippy::manual_async_fn)]
    fn ok_body(value: i32) -> impl Future<Output = Outcome> + Send + 'static {
        async move {
            let mut w = FramedWriter::status(crate::envelope::STATUS_OK);
            w.write_i32(value);
            Outcome::Ok(w)
        }
    }

    /// A displaced registry entry must drop *outside* the lock.
    ///
    /// `HashMap::insert` returns whatever it evicted, and here that is an
    /// `async_task::Task`. For a task neither SCHEDULED nor RUNNING,
    /// `Task::drop` calls the schedule callback synchronously so the executor
    /// can drop the future (async-task 4.7.1, task.rs:213-215) — and this
    /// executor's schedule callback takes the same non-reentrant spin lock.
    /// Dropping the eviction inside the critical section therefore spins
    /// forever on this thread, with no user code anywhere in the picture.
    ///
    /// Bounded on purpose: the regression is a *hang*, not a panic, so the work
    /// runs on its own thread and this asserts on a timeout rather than wedging
    /// the whole suite. The duplicate also trips `spawn`'s `debug_assert` — a
    /// different signal, silenced here so it does not read as a failure.
    #[test]
    fn a_displaced_task_drops_outside_the_registry_lock() {
        let _serial = crate::post::test_lock();
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));

        /// `Pending` and — unlike `YieldOnce` — it never wakes, so after one
        /// poll the task is neither SCHEDULED nor RUNNING. That idle state is
        /// the whole point: it is the only one in which `Task::drop` takes
        /// async-task's synchronous-schedule branch.
        struct ParkForever;
        impl Future for ParkForever {
            type Output = Outcome;
            fn poll(self: std::pin::Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Outcome> {
                Poll::Pending
            }
        }

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let finished = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let (exec, _log) = recording();
                exec.spawn(7, ParkForever);
                // Poll it once so it parks: spawned-but-never-drained leaves it
                // SCHEDULED, and a SCHEDULED task's drop does not re-enter.
                assert!(exec.drain_one());
                // Same id: evicts the now-idle entry, whose `Task` drops here.
                exec.spawn(7, ok_body(2));
            }));
            let _ = tx.send(finished.is_ok());
        });
        let arrived = rx.recv_timeout(std::time::Duration::from_secs(5));

        std::panic::set_hook(prev);
        assert!(
            arrived.is_ok(),
            "a duplicate call id wedged the executor's spin lock: the displaced \
             Task dropped inside the critical section, and its Drop re-entered \
             that lock through the schedule callback"
        );
    }

    #[test]
    fn ready_future_completes_with_its_bytes() {
        let (exec, log) = recording();
        exec.spawn(7, ok_body(42));
        exec.drain_all();
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 1);
        let (id, bytes) = &log[0];
        assert_eq!(*id, 7);
        assert_eq!(bytes[0], STATUS_OK);
        assert_eq!(crate::codec::ByteReader::new(&bytes[1..]).read_i32(), 42);
        assert_eq!(exec.task_count(), 0, "finished task is reaped");
    }

    /// Pending on the first poll (self-waking), Ready on the second — so the
    /// executor must poll more than once, and a wake must re-enqueue the task.
    struct YieldOnce {
        yielded: bool,
    }
    impl Future for YieldOnce {
        type Output = Outcome;
        fn poll(mut self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Outcome> {
            if self.yielded {
                Poll::Ready(Outcome::Ok(FramedWriter::status(
                    crate::envelope::STATUS_OK,
                )))
            } else {
                self.yielded = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }

    #[test]
    fn pending_then_ready_completes_after_a_wake_and_repoll() {
        let (exec, log) = recording();
        exec.spawn(1, YieldOnce { yielded: false });
        // First poll: Pending, self-woke, so re-enqueued but not yet complete.
        assert!(exec.drain_one());
        assert_eq!(log.lock().unwrap().len(), 0, "still pending after one poll");
        assert_eq!(exec.task_count(), 1);
        // Second poll: Ready.
        assert!(exec.drain_one());
        assert_eq!(log.lock().unwrap().len(), 1, "completed on the re-poll");
        assert_eq!(exec.task_count(), 0);
        assert!(!exec.drain_one(), "queue is empty");
    }

    #[test]
    fn thousand_futures_multiplex_on_one_thread() {
        let (exec, log) = recording();
        // All spawned together, all suspend once (YieldOnce), all resume — on
        // this single test thread. The proof it multiplexes rather than parks
        // a thread per suspended future.
        for id in 0..1000 {
            exec.spawn(id, YieldOnce { yielded: false });
        }
        exec.drain_all();
        assert_eq!(log.lock().unwrap().len(), 1000);
        assert_eq!(exec.task_count(), 0, "no leaked tasks");
        // Every id delivered exactly once.
        let mut ids: Vec<u64> = log.lock().unwrap().iter().map(|(id, _)| *id).collect();
        ids.sort_unstable();
        assert_eq!(ids, (0..1000).collect::<Vec<_>>());
    }

    /// Parks forever (Pending, never self-wakes) and flips a flag when its
    /// storage is dropped — so a cancel that drops the future is observable.
    struct ParkForever {
        _guard: DropFlag,
    }
    struct DropFlag(Arc<AtomicBool>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    impl Future for ParkForever {
        type Output = Outcome;
        fn poll(self: std::pin::Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Outcome> {
            Poll::Pending
        }
    }

    #[test]
    fn cancelling_a_pending_task_drops_its_future() {
        let dropped = Arc::new(AtomicBool::new(false));
        let (exec, log) = recording();
        exec.spawn(
            9,
            ParkForever {
                _guard: DropFlag(dropped.clone()),
            },
        );
        exec.drain_one(); // poll once -> Pending, Runnable consumed
        assert!(!dropped.load(Ordering::SeqCst), "future still alive while parked");
        assert_eq!(exec.task_count(), 1);

        // Cancel removes the join handle. async_task then schedules one final
        // no-poll runnable (firing our schedule hook — production arranges a
        // drain); draining it drops the future. No leak.
        exec.cancel(9);
        assert_eq!(exec.task_count(), 0, "the task is gone from the registry");
        exec.drain_all();
        assert!(dropped.load(Ordering::SeqCst), "cancel dropped the future");
        assert!(log.lock().unwrap().is_empty(), "a cancelled task never completes");
    }

    /// A reservation is claimable exactly like a task, and the hand-off that
    /// consumes it is all-or-nothing: with the reservation intact
    /// `spawn_reserved` takes it and the call completes; with it claimed the
    /// spawn is refused and the body is dropped **un-polled**.
    ///
    /// The core of what closes the native actor's queued-cancel window
    /// (actor.rs); pinned here, on a hand-built executor, because the property
    /// is the registry's rather than the actor's.
    #[test]
    fn a_reservation_is_claimable_and_the_hand_off_is_all_or_nothing() {
        let (exec, log) = recording();

        // Claimed before the hand-off: the spawn is refused, and refusing it
        // drops the body without a poll (`ParkForever` would have parked).
        let dropped = Arc::new(AtomicBool::new(false));
        exec.reserve(1);
        assert!(exec.holds(1));
        assert_eq!(exec.task_count(), 1, "a reservation is a live call");
        assert!(exec.cancel(1), "a reservation is the canceller's to claim");
        assert!(!exec.holds(1), "a claim is single-shot");
        assert!(
            !exec.spawn_reserved(
                1,
                ParkForever {
                    _guard: DropFlag(dropped.clone()),
                },
            ),
            "the reservation was claimed, so the hand-off must be refused"
        );
        assert!(
            dropped.load(Ordering::SeqCst),
            "a refused hand-off drops the body then and there — it was never \
             given to the executor, so no drain will ever reach it"
        );
        assert_eq!(exec.task_count(), 0, "a refused spawn leaves nothing behind");
        exec.drain_all();
        assert!(log.lock().unwrap().is_empty(), "a claimed call never answers");

        // Reservation intact: the hand-off takes it and the call answers once.
        exec.reserve(2);
        assert!(exec.spawn_reserved(2, ok_body(5)));
        exec.drain_all();
        assert_eq!(log.lock().unwrap().len(), 1, "the handed-off body answered");
        assert_eq!(log.lock().unwrap()[0].0, 2);
        assert_eq!(exec.task_count(), 0);
    }

    /// The answer-once gate, under the one race it exists for: a cancel that
    /// lands while the future is mid-poll — after the poll started, before its
    /// completion hook could run. The claimed cancel must win: the completion
    /// hook must never fire for a call the canceller now owns answering.
    ///
    /// Red fence: with the gate deleted (completion hook called
    /// unconditionally), the log gets a completion for a call `cancel`
    /// claimed — a double answer — and both asserts below fail. Verified by
    /// making exactly that edit.
    #[test]
    fn a_claimed_cancel_beats_a_mid_poll_completion() {
        use std::sync::Barrier;
        /// First poll: rendezvous "in poll", wait for "cancel done", then
        /// complete. The completion hook therefore runs strictly after the
        /// cancel claimed (or failed to claim) the entry.
        struct BlockThenReady(Arc<Barrier>);
        impl Future for BlockThenReady {
            type Output = Outcome;
            fn poll(self: std::pin::Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Outcome> {
                self.0.wait(); // in poll
                self.0.wait(); // cancel done
                Poll::Ready(Outcome::Ok(FramedWriter::status(STATUS_OK)))
            }
        }

        let (exec, log) = recording();
        let gate = Arc::new(Barrier::new(2));
        exec.spawn(5, BlockThenReady(gate.clone()));
        let drainer = {
            let exec = Executor {
                shared: exec.shared.clone(),
            };
            std::thread::spawn(move || exec.drain_all())
        };
        gate.wait(); // the poll has started
        let claimed = exec.cancel(5);
        gate.wait(); // let the poll finish
        drainer.join().unwrap();

        assert!(claimed, "the entry was live mid-poll, so the cancel claims it");
        assert!(
            log.lock().unwrap().is_empty(),
            "a claimed cancel owns the answer: the completion hook must not \
             fire for call 5 — a double answer is exactly what the gate exists \
             to prevent"
        );
        assert_eq!(exec.task_count(), 0);
    }

    /// The same race, with a reply that **returns a handle**: losing the claim
    /// must give the object back, not drop it on the floor.
    ///
    /// The body runs to completion before the gate is consulted, so by the time
    /// the cancel is found to have won, the encode has already registered the
    /// object and the Dart wrapper that would dispose it will never be built.
    /// Nothing else in the process holds the pointer — `handle::confined_new`
    /// is a bare `Box::into_raw` — so this branch is the only thing that can
    /// free it. Modelled the way the channel tests model a mint: a `u64` and a
    /// drop fn that records rather than frees.
    #[test]
    fn a_reply_the_cancel_claimed_gives_its_handles_back() {
        use std::sync::Barrier;
        static RECLAIMED: Mutex<Vec<u64>> = Mutex::new(Vec::new());
        unsafe fn note(h: u64) {
            RECLAIMED.lock().unwrap().push(h);
        }

        struct BlockThenMint(Arc<Barrier>);
        impl Future for BlockThenMint {
            type Output = Outcome;
            fn poll(self: std::pin::Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Outcome> {
                self.0.wait(); // in poll
                self.0.wait(); // cancel done
                let mut w = FramedWriter::status(STATUS_OK);
                unsafe { w.write_minted(0x5EED, note) };
                Poll::Ready(Outcome::Ok(w))
            }
        }

        RECLAIMED.lock().unwrap().clear();
        let (exec, log) = recording();
        let gate = Arc::new(Barrier::new(2));
        exec.spawn(6, BlockThenMint(gate.clone()));
        let drainer = {
            let exec = Executor {
                shared: exec.shared.clone(),
            };
            std::thread::spawn(move || exec.drain_all())
        };
        gate.wait();
        assert!(exec.cancel(6), "the entry was live mid-poll");
        gate.wait();
        drainer.join().unwrap();

        assert!(log.lock().unwrap().is_empty(), "the cancel owns the answer");
        assert_eq!(
            *RECLAIMED.lock().unwrap(),
            vec![0x5EED],
            "and the reply it discarded gave back what it had minted"
        );
    }

    /// The control for the test above: a reply that *is* delivered must not be
    /// reclaimed, or the object would be freed under the wrapper Dart has just
    /// built from it. Without this, "always reclaim" passes the pair.
    #[test]
    fn a_delivered_reply_keeps_its_handles() {
        static RECLAIMED: Mutex<usize> = Mutex::new(0);
        unsafe fn note(_h: u64) {
            *RECLAIMED.lock().unwrap() += 1;
        }

        *RECLAIMED.lock().unwrap() = 0;
        let (exec, log) = recording();
        exec.spawn(7, async {
            let mut w = FramedWriter::status(STATUS_OK);
            unsafe { w.write_minted(0xF00D, note) };
            Outcome::Ok(w)
        });
        exec.drain_all();

        assert_eq!(log.lock().unwrap().len(), 1, "the answer was delivered");
        assert_eq!(*RECLAIMED.lock().unwrap(), 0, "so nothing was freed");
    }

    /// The Dart-facing export, over the **global** executor and its real
    /// Scheduler — the path a `FrustrateCancelToken` actually takes.
    ///
    /// Worth a test of its own beyond `cancelling_a_pending_task_drops_its_future`,
    /// which drives a hand-built executor and its own drain: here the drop
    /// lands on a *pool worker*, arranged by `arrange_drain`, with nothing on
    /// this thread driving it. That is the property the token's contract rests
    /// on — cancelling runs no user code on the caller's thread — and it is
    /// what the wait below is for rather than a flake guard.
    // Not under `--cfg frustrate_manual_scheduler`: this specifically asserts a
    // *pool worker* performs the drop with nothing on this thread driving it.
    // The manual build proves the same guarantee, on the host's drain, in
    // `a_claimed_cancel_drops_the_future_on_the_host_s_drain` below.
    #[cfg(not(frustrate_manual_scheduler))]
    #[test]
    fn the_cancel_export_claims_a_live_call_and_drops_its_future_on_a_drain() {
        let _serial = crate::post::test_lock();
        let dropped = Arc::new(AtomicBool::new(false));
        // An id from this test alone; the global registry is shared with every
        // other test in this binary and ids are never reused.
        const ID: u64 = 0x0C0F_FEE0_0001;
        super::spawn(
            ID,
            ParkForever {
                _guard: DropFlag(dropped.clone()),
            },
        );
        assert_eq!(
            frustrate_call_cancel(ID),
            1,
            "a live task is the caller's to claim"
        );
        // The future is dropped by async_task on the next drain of its final
        // runnable, which a pool worker runs. Nothing here drives it.
        let start = std::time::Instant::now();
        while !dropped.load(Ordering::SeqCst)
            && start.elapsed() < std::time::Duration::from_secs(5)
        {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            dropped.load(Ordering::SeqCst),
            "a claimed cancel must drop the future — on a drain, not here"
        );
        assert_eq!(
            frustrate_call_cancel(ID),
            0,
            "the claim is single-shot: a second cancel owns nothing"
        );
        assert_eq!(
            frustrate_call_cancel(0xDEAD_BEEF),
            0,
            "an id that was never live is a legal no-op, not an error"
        );
    }

    /// The other side of the claim contract: a call that already answered
    /// refuses the cancel, so the canceller knows NOT to fail a future that
    /// has (or will get) its real completion.
    #[test]
    fn cancel_after_completion_does_not_claim() {
        let (exec, log) = recording();
        exec.spawn(6, ok_body(1));
        exec.drain_all();
        assert_eq!(log.lock().unwrap().len(), 1, "completed normally");
        assert!(
            !exec.cancel(6),
            "an answered call must refuse the claim — the caller would \
             otherwise fail a future that already has its answer"
        );
    }

    #[test]
    fn schedule_hook_fires_on_spawn_and_on_wake() {
        // The Scheduler seam must be poked when work becomes ready: once at
        // spawn (initial enqueue) and again when a wake re-enqueues.
        let count = Arc::new(AtomicU32::new(0));
        let c = count.clone();
        let exec = Executor::new(move || { c.fetch_add(1, Ordering::SeqCst); }, |_, _| {});
        exec.spawn(1, YieldOnce { yielded: false });
        assert_eq!(count.load(Ordering::SeqCst), 1, "spawn schedules the first poll");
        exec.drain_one(); // Pending + self-wake re-enqueues -> schedule again
        assert_eq!(count.load(Ordering::SeqCst), 2, "a wake schedules a re-poll");
    }

    /// `drain_all` reports whether it polled anything. That bool is the only
    /// signal a host driving its own drains has for "the Rust side is quiet" —
    /// a simulation loop reads it to decide it should advance its clock or
    /// deliver the next queued event rather than drain again.
    ///
    /// Quiet is not "finished": a task suspended on a host answer is off the
    /// run queue and so reports quiet too. That distinction is the harness's to
    /// make, and the doc on `frustrate_manual_drain` says so.
    #[test]
    fn drain_all_reports_whether_it_ran_anything() {
        let (exec, log) = recording();
        assert!(
            !exec.drain_all(),
            "an untouched queue is quiet — a host must be able to ask before \
             spawning anything and get a straight answer"
        );
        exec.spawn(1, ok_body(7));
        assert!(exec.drain_all(), "a queued task means the drain polled something");
        assert!(!exec.drain_all(), "and the queue is quiet once emptied");
        assert_eq!(log.lock().unwrap().len(), 1, "the task actually completed");
    }

    /// Detached tasks ride the same run queue as call tasks, so a drain that
    /// claims "everything runnable" must include them — and must count them in
    /// the quiet bool, or a host would stop draining with work still pending.
    #[test]
    fn a_detached_task_drains_and_counts_as_not_quiet() {
        let ran = Arc::new(AtomicU32::new(0));
        let r = ran.clone();
        let (exec, log) = recording();
        exec.spawn_detached(async move {
            r.fetch_add(1, Ordering::SeqCst);
        });
        assert!(exec.drain_all(), "a detached task is runnable work like any other");
        assert_eq!(ran.load(Ordering::SeqCst), 1, "and it ran");
        assert!(!exec.drain_all(), "quiet once it is done");
        assert!(
            log.lock().unwrap().is_empty(),
            "a detached task answers no call, so it delivers no envelope"
        );
    }

    /// The wiring test for `--cfg frustrate_manual_scheduler`: that the build
    /// flag actually reaches the *global* executor's Scheduler. The behavioural
    /// tests above cannot cover it — `recording()` builds its own executor with
    /// a no-op `schedule`, so they would pass identically either way.
    /// The cancel contract under manual scheduling: a claimed cancel still
    /// drops the future, and still never runs user `Drop` code on the
    /// caller's thread — but the drop lands on the host's drain rather than a
    /// pool worker's. The pool-driven form of this is
    /// `the_cancel_export_claims_a_live_call_and_drops_its_future_on_a_drain`.
    ///
    /// Worth its own test rather than a gate: "a cancelled future is dropped"
    /// is the guarantee a harness leans on when it tears a simulation down,
    /// and a build where nothing drains autonomously is exactly where a
    /// forgotten drop would go unnoticed.
    #[cfg(frustrate_manual_scheduler)]
    #[test]
    fn a_claimed_cancel_drops_the_future_on_the_host_s_drain() {
        let _serial = crate::post::test_lock();
        let dropped = Arc::new(AtomicBool::new(false));
        const ID: u64 = 0x0C0F_FEE0_0002;
        super::spawn(
            ID,
            ParkForever {
                _guard: DropFlag(dropped.clone()),
            },
        );
        assert_eq!(frustrate_call_cancel(ID), 1, "a live task is the caller's to claim");
        assert!(
            !dropped.load(Ordering::SeqCst),
            "cancelling must not run the future's Drop on the caller's thread — \
             that is the token contract, and it holds under either Scheduler"
        );

        assert_eq!(
            frustrate_manual_drain(),
            1,
            "the cancelled task's final runnable is still queued work"
        );
        assert!(
            dropped.load(Ordering::SeqCst),
            "and the host's drain is what drops it"
        );
        assert_eq!(
            frustrate_call_cancel(ID),
            0,
            "the claim is single-shot regardless of who drained"
        );
    }

    #[cfg(frustrate_manual_scheduler)]
    #[test]
    fn the_manual_global_polls_nothing_until_the_host_drains() {
        let _serial = crate::post::test_lock();
        static DONE: Mutex<Vec<(u64, Vec<u8>)>> = Mutex::new(Vec::new());
        extern "C" fn record(call_id: u64, ptr: *mut u8, len: u64, cap: u64) {
            let leased = unsafe { Vec::from_raw_parts(ptr, len as usize, cap as usize) };
            DONE.lock().unwrap().push((call_id, leased.clone()));
        }
        crate::post::init(record);
        DONE.lock().unwrap().clear();

        super::spawn(4242, ok_body(99));

        // A real interval, because the claim is about something NOT happening.
        // In a production build the pool answers this within microseconds — the
        // sibling test above waits on a condvar and passes promptly — so if any
        // worker were going to pick the task up, it would have.
        std::thread::sleep(std::time::Duration::from_millis(150));
        assert!(
            DONE.lock().unwrap().is_empty(),
            "the manual Scheduler must enqueue and stop: with no host drain, \
             nothing may poll the task"
        );

        assert_eq!(
            frustrate_manual_drain(),
            1,
            "the host's drain finds the task still queued and polls it"
        );
        let done = DONE.lock().unwrap();
        assert_eq!(done.len(), 1, "one completion delivered, on the host's thread");
        assert_eq!(done[0].0, 4242);
        assert_eq!(done[0].1[0], STATUS_OK);
        assert_eq!(crate::codec::ByteReader::new(&done[0].1[1..]).read_i32(), 99);
        drop(done);

        assert_eq!(
            frustrate_manual_drain(),
            0,
            "and the next drain reports quiet rather than re-running anything"
        );
    }

    // Not under `--cfg frustrate_manual_scheduler`: this asserts the *pool*
    // delivers unprompted, and the whole point of that flag is that it does not.
    // The manual build's counterpart is
    // `the_manual_global_polls_nothing_until_the_host_drains` below.
    #[cfg(not(frustrate_manual_scheduler))]
    #[test]
    fn native_global_seam_delivers_through_the_pool_and_post() {
        // The full native Scheduler seam: spawn on the *global* executor, whose
        // arrange_drain hands drains to the real thread pool and whose
        // completion hook is post::respond. A registered post callback records
        // the delivered envelope.
        let _serial = crate::post::test_lock();
        static DONE: Mutex<Vec<(u64, Vec<u8>)>> = Mutex::new(Vec::new());
        static CV: std::sync::Condvar = std::sync::Condvar::new();
        extern "C" fn record(call_id: u64, ptr: *mut u8, len: u64, cap: u64) {
            // Take ownership of the leased buffer (frees on drop), keeping a copy.
            let leased = unsafe { Vec::from_raw_parts(ptr, len as usize, cap as usize) };
            DONE.lock().unwrap().push((call_id, leased.clone()));
            CV.notify_all();
        }
        crate::post::init(record);
        DONE.lock().unwrap().clear();

        super::spawn(4242, ok_body(99));

        let mut guard = DONE.lock().unwrap();
        let start = std::time::Instant::now();
        while guard.is_empty() && start.elapsed() < std::time::Duration::from_secs(5) {
            guard = CV
                .wait_timeout(guard, std::time::Duration::from_millis(200))
                .unwrap()
                .0;
        }
        assert_eq!(guard.len(), 1, "one completion delivered");
        assert_eq!(guard[0].0, 4242);
        assert_eq!(guard[0].1[0], STATUS_OK);
        assert_eq!(
            crate::codec::ByteReader::new(&guard[0].1[1..]).read_i32(),
            99
        );
    }

    #[test]
    fn a_panicking_poll_becomes_a_panic_envelope_for_its_call() {
        // `drain_one` funnels this panic through `envelope::panic_envelope`,
        // which tells the process-global panic listener. `post::test_lock` is
        // what the tests that register one hold; without it this report lands
        // in their log and fails their "exactly one report" assertions.
        let _serial = crate::post::test_lock();
        let (exec, log) = recording();
        exec.spawn(5, async { panic!("boom in a future") });
        exec.drain_all();
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 1);
        let (id, bytes) = &log[0];
        assert_eq!(*id, 5);
        assert_eq!(bytes[0], crate::envelope::STATUS_PANIC);
        assert!(crate::codec::ByteReader::new(&bytes[1..])
            .read_string()
            .contains("boom in a future"));
        assert_eq!(exec.task_count(), 0, "a panicked task is removed");
    }

    /// A poll that panics on a call **somebody else already claimed** answers
    /// nobody — and must still be reported.
    ///
    /// Unreported, this arm drops the payload on the floor: no envelope, no
    /// output, no trace of the panic anywhere in the process. The race that reaches it
    /// (a `FrustrateCancelToken` landing while a poll is mid-panic on another
    /// thread) is not reproducible, so it is driven deterministically instead —
    /// the future claims its own entry from inside its poll, which is the same
    /// state `drain_one` finds, arrived at on one thread.
    ///
    /// Serialized on `post::test_lock` because registering a panic listener
    /// swaps the process `std` panic hook, exactly like the sibling tests that
    /// silence it.
    #[test]
    fn a_panic_on_an_already_claimed_call_is_reported_though_nobody_is_answered() {
        let _serial = crate::post::test_lock();
        let reports = crate::panic::record_for_tests();

        let (exec, log) = recording();
        let exec = Arc::new(exec);
        let claimer = exec.clone();
        exec.spawn(11, async move {
            assert!(claimer.cancel(11), "the future must own the claim");
            panic!("panicked with the answer already claimed");
        });
        exec.drain_all();
        crate::panic::unregister();

        assert!(
            log.lock().unwrap().is_empty(),
            "the answer-once gate was broken: a claimed call was answered twice"
        );
        assert_eq!(
            reports.lock().unwrap().as_slice(),
            ["panicked with the answer already claimed"],
            "a panic nobody could be told about on the wire was told to nobody \
             at all"
        );
    }

    // ------------------------------------------------------ detached tasks --

    /// A detached task is driven by the same drain as a call's, and answers
    /// nobody: no completion, no registry entry, nothing to reap.
    #[test]
    fn a_detached_task_runs_and_answers_nobody() {
        let (exec, log) = recording();
        let ran = Arc::new(AtomicBool::new(false));
        let flag = ran.clone();
        exec.spawn_detached(async move {
            flag.store(true, Ordering::SeqCst);
        });
        assert!(
            !ran.load(Ordering::SeqCst),
            "spawning enqueues a poll; it does not run the body inline"
        );
        exec.drain_all();
        assert!(ran.load(Ordering::SeqCst), "the drain polled it to completion");
        assert!(
            log.lock().unwrap().is_empty(),
            "a detached task has no call to answer, so nothing may be delivered"
        );
        assert_eq!(
            exec.task_count(),
            0,
            "a detached task is never in the call registry, before or after"
        );
    }

    /// The property that makes this worth having at all: the task **outlives
    /// the drain that started it**. `Task::detach` gives up the join handle
    /// without cancelling, so a `Pending` poll leaves the future alive in
    /// async_task's own allocation — nothing else holds it — and its waker
    /// brings it back.
    ///
    /// Red fence: replacing `task.detach()` with `drop(task)` cancels the task
    /// instead, and `ran` stays false — the second poll never happens.
    #[test]
    fn a_detached_task_survives_a_pending_poll_and_resumes_on_its_wake() {
        struct WakeThenFinish(Arc<AtomicBool>, bool);
        impl Future for WakeThenFinish {
            type Output = ();
            fn poll(mut self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
                if self.1 {
                    self.0.store(true, Ordering::SeqCst);
                    return Poll::Ready(());
                }
                self.1 = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }

        let (exec, log) = recording();
        let ran = Arc::new(AtomicBool::new(false));
        exec.spawn_detached(WakeThenFinish(ran.clone(), false));
        assert!(exec.drain_one(), "first poll: Pending, self-woken");
        assert!(!ran.load(Ordering::SeqCst));
        assert!(exec.drain_one(), "the wake re-enqueued it");
        assert!(
            ran.load(Ordering::SeqCst),
            "the detached task was still alive to be re-polled"
        );
        assert!(!exec.drain_one(), "and the queue is empty again");
        assert!(log.lock().unwrap().is_empty());
    }

    /// Detached work is not claimable. `cancel(DETACHED)` must be `false` —
    /// not because the id is special-cased, but because nothing ever inserts
    /// it — so a `FrustrateCancelToken` for an id the executor never held
    /// cannot be told it owns an answer.
    #[test]
    fn a_detached_task_puts_nothing_claimable_in_the_registry() {
        let (exec, _log) = recording();
        exec.spawn_detached(async {});
        assert!(!exec.cancel(DETACHED), "there is nothing to claim");
        assert!(!exec.holds(DETACHED));
        exec.drain_all();
    }

    /// A panicking detached task reaches the process panic listener and
    /// delivers nothing. There is no call waiting, so being told is the only
    /// outcome available — and it is the one silence would cost.
    ///
    /// Red fence: build the envelope only on the claimed branch of `drain_one`
    /// and `reports` comes back empty while the test still "passes" its
    /// delivery assertion — which is exactly the failure this pins.
    #[test]
    fn a_panicking_detached_task_is_reported_and_delivers_nothing() {
        let _serial = crate::post::test_lock();
        let reports = crate::panic::record_for_tests();

        let (exec, log) = recording();
        exec.spawn_detached(async { panic!("boom in a detached task") });
        exec.drain_all();
        crate::panic::unregister();

        assert!(
            log.lock().unwrap().is_empty(),
            "no call is waiting, so nothing may be delivered"
        );
        assert_eq!(
            reports.lock().unwrap().as_slice(),
            ["boom in a detached task"],
            "a detached panic must reach the panic listener — it has nowhere \
             else to go"
        );
        assert_eq!(exec.task_count(), 0);
    }

    /// The public seam end to end: [`crate::runtime::spawn`] on the *global*
    /// executor, whose Scheduler is the real thread pool. Nothing is delivered
    /// and nothing is joined, so the observation is the side effect itself.
    #[test]
    fn the_global_seam_runs_detached_work_on_the_pool() {
        static DONE: Mutex<bool> = Mutex::new(false);
        static CV: std::sync::Condvar = std::sync::Condvar::new();
        *DONE.lock().unwrap() = false;

        crate::runtime::spawn(async {
            *DONE.lock().unwrap() = true;
            CV.notify_all();
        });

        let mut guard = DONE.lock().unwrap();
        let start = std::time::Instant::now();
        while !*guard && start.elapsed() < std::time::Duration::from_secs(5) {
            guard = CV
                .wait_timeout(guard, std::time::Duration::from_millis(200))
                .unwrap()
                .0;
        }
        assert!(*guard, "the pool polled the detached task to completion");
    }
}
