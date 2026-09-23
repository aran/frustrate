//! Native actor hosts: one dedicated thread per actor instance.
//!
//! An actor host is a FIFO of jobs drained by its own OS thread — the
//! native analog of the web's worker-hosted wasm instance. The actor
//! object (a raw Box, see the generated `actor_N_*` glue) is created,
//! used, and dropped exclusively on this thread, so actor types need no
//! `Send`/`Sync` and no locks: serialization is the executor itself.
//!
//! Not compiled for wasm: there the host is a Worker driving its own
//! instance through the ordinary exports.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Mutex, OnceLock};

/// One dispatched call body. `Some(reply)` is the response envelope the loop
/// posts — the framed writer, so the ledger of what the reply minted travels
/// with it (`codec::Minted`); `None` means the job **detached** its completion
/// — a deferred method's prefix ran and handed the rest to the cooperative
/// executor (`deferred::spawn_deferred`), which now owns answering the call.
type Job = Box<dyn FnOnce() -> Option<crate::codec::FramedWriter> + Send>;

enum Msg {
    Call { call_id: u64, job: Job },
    Stop,
    /// Drop the object *and* stop — the reclaim path for a handle nobody
    /// disposed. [`Msg::Stop`] cannot do this job: it releases the executor
    /// while leaving the object alive, which is correct only because an
    /// orderly `dispose()` has already dropped it through the dispatched drop.
    Reap,
}

thread_local! {
    /// How this host thread drops its object, or `None` once something has.
    ///
    /// The object's pointer lives nowhere else on this side — `hosts()` maps an
    /// id to a `Sender`, and the Dart heap holds the pointer as `int? _raw`,
    /// which a hot restart discards. So the executor has to remember how to
    /// free what it owns, or nothing can.
    ///
    /// Thread-local rather than a field in `hosts()` for two reasons: the object
    /// is constructed *later* than the host (the constructor body is itself a
    /// job), so there is nothing to record at spawn; and construction, drop and
    /// reap all run on this thread, so a thread-local needs no lock and imposes
    /// no `Send` bound on a closure that captures a raw `Box` pointer — which is
    /// what keeps the module's "used and dropped exclusively on this thread"
    /// invariant true.
    ///
    /// One slot per thread is enough: the checker confines actor handles to
    /// actor constructors, so one instance is one executor.
    static TEARDOWN: Cell<Option<Box<dyn FnOnce()>>> = const { Cell::new(None) };

    /// The id of the host this thread IS (0 elsewhere). Set once at thread
    /// birth; read by `deferred::spawn_deferred`, which runs only inside a
    /// dispatched job and needs to know which host's registry to record the
    /// call in — the job closure itself does not carry the host id.
    static CURRENT_HOST: Cell<u64> = const { Cell::new(0) };
}

/// The host id of the executor thread this is called on. Panics off a host
/// thread — only `deferred::spawn_deferred` calls it, and generated glue only
/// reaches that from a dispatched actor job.
pub(crate) fn current_host() -> u64 {
    let id = CURRENT_HOST.with(|c| c.get());
    assert!(
        id != 0,
        "frustrate: spawn_deferred called outside an actor executor — bridge bug"
    );
    id
}

/// Record how to drop this host's object. Called on the host thread by the
/// generated constructor, through `handle::actor_new`.
pub fn arm_teardown(f: impl FnOnce() + 'static) {
    TEARDOWN.with(|slot| slot.set(Some(Box::new(f))));
}

/// Forget the teardown because the object is being dropped by the orderly
/// path. Called on the host thread by the generated dispatched drop, *before*
/// it drops, so that a [`Msg::Reap`] racing behind it finds the slot empty
/// rather than dropping a second time.
pub fn disarm_teardown() {
    TEARDOWN.with(|slot| slot.take());
}

/// A live executor, and the isolate that owns it.
///
/// `isolate` is learned rather than declared: `frustrate_actor_spawn` takes no
/// arguments, and giving it one would be an ABI change for a fact the very next
/// call carries anyway. Every call id is stamped with its isolate
/// ([`crate::post::ISOLATE_SHIFT`]), and an actor's *constructor* is dispatched
/// on its own host, so a host is attributed by the time it owns anything worth
/// reaping. `None` means "spawned, never used" — a window in which there is
/// also nothing to leak.
struct Host {
    tx: Sender<Msg>,
    isolate: Option<u32>,
}

/// Both registries in this file are a plain `std::sync::Mutex`, which is the
/// odd one out and is deliberate. The sibling registries switch to a spin lock
/// under threaded wasm — `stream.rs` (`SpinLock`), `executor.rs` (`Lock<T>`),
/// `pool.rs` (`SpinMutex`) — because the browser main thread can take those,
/// and a *contended* futex park traps there ("Atomics.wait cannot be called in
/// this context"). These two never face that, for reasons that are enforced
/// rather than incidental:
///
/// * **Native** wants a real parking lock: these guard OS threads, and a spin
///   here would spin for the length of whatever holds it.
/// * **Web** never contends them. An actor host is a Worker running its *own*
///   wasm instance (`runtime_web.dart`, `_WebActorHost`), so these statics are
///   per-instance rather than shared, and that instance is single-threaded —
///   an actor instance has no pool, which the glue enforces with a throwing
///   `spawn_worker` import (`executor::ACTOR_INSTANCE`). `spawn_host`'s
///   `thread::spawn` is not merely unsupported on wasm but unreachable:
///   `frustrate_actor_spawn` is looked up only by the native runtime
///   (`runtime_native.dart`).
/// * `deferred()` is narrower still — `Deferred<T>` is rejected anywhere but an
///   actor method (FR0038, `codegen/src/check.rs`), so nothing running on the
///   main threaded-wasm instance registers here at all.
///
/// So: do not "fix" these into spin locks, and do not add a caller that reaches
/// them from the main instance without revisiting all three points.
fn hosts() -> &'static Mutex<HashMap<u64, Host>> {
    static HOSTS: OnceLock<Mutex<HashMap<u64, Host>>> = OnceLock::new();
    HOSTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Outstanding deferred completions by owning host. What `dispose()` and the
/// reap paths cancel: the calls whose futures are live on the cooperative
/// executor after their prefixes ran here.
///
/// Registered by `deferred::spawn_deferred` (on the host thread, during the
/// prefix's job) and unregistered by its drop guard when the future is
/// dropped — completion, cancellation, and a panicking poll all end there.
/// The unregister is asynchronous to the cancel (the future drops on a later
/// drain), so a sweep may see ids whose tasks are already claimed;
/// `executor::cancel` refuses those, which is what makes the sweep idempotent
/// with the per-call cancels `dispose()` already did.
fn deferred() -> &'static Mutex<HashMap<u64, HashSet<u64>>> {
    static DEFERRED: OnceLock<Mutex<HashMap<u64, HashSet<u64>>>> = OnceLock::new();
    DEFERRED.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn register_deferred(host: u64, call_id: u64) {
    deferred()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(host)
        .or_default()
        .insert(call_id);
}

pub(crate) fn unregister_deferred(host: u64, call_id: u64) {
    let mut map = deferred()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(set) = map.get_mut(&host) {
        set.remove(&call_id);
        if set.is_empty() {
            map.remove(&host);
        }
    }
}

/// Cancel one deferred completion, for the Dart-side `dispose()`.
///
/// Returns whether the cancel **claimed** the call (`executor::cancel`'s
/// contract): `true` means its completion will never be delivered and the
/// caller now owns failing the Dart future; `false` means the call already
/// answered — or is answering right now, race-free under the executor's
/// registry lock — and must be left alone. This claim protocol is what makes
/// the disposed-deferred error exactly-once on native: a cancelled id never
/// responds, an unclaimed id's response delivers normally.
pub fn cancel_deferred(host: u64, call_id: u64) -> bool {
    unregister_deferred(host, call_id);
    crate::executor::cancel(call_id)
}

/// Cancel every outstanding deferred completion of `host` — the backstop
/// sweep for the paths where no Dart-side `dispose()` enumerates them: a
/// reaped handle, a dead isolate, and (as an idempotent belt) an orderly
/// shutdown. Nobody is waiting on these calls, so no answer is owed; the
/// point is dropping the futures and what they captured.
///
/// **Runs no user code on this thread**: `executor::cancel` only claims the
/// registry entry — the future's drop lands on a later drain, inside
/// `drain_one`'s catch_unwind — so this is safe from `mark_gone`'s
/// `extern "C"` frame (must not panic, must not block).
fn cancel_deferred_for(host: u64) {
    let ids: Vec<u64> = {
        let mut map = deferred()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.remove(&host)
            .map(|set| set.into_iter().collect())
            .unwrap_or_default()
    };
    for id in ids {
        crate::executor::cancel(id);
    }
}

static NEXT_HOST: AtomicU64 = AtomicU64::new(1);

pub fn spawn_host() -> u64 {
    let id = NEXT_HOST.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = channel::<Msg>();
    hosts().lock().unwrap().insert(
        id,
        Host {
            tx,
            isolate: None,
        },
    );
    std::thread::Builder::new()
        .name(format!("frustrate-actor-{id}"))
        .spawn(move || {
            // Actor host threads may block on Dart (DartFunction::call).
            crate::callback::enter_worker_context();
            // So a deferred prefix dispatched here can name its own host
            // (`current_host`) without the job carrying the id.
            CURRENT_HOST.with(|c| c.set(id));
            while let Ok(msg) = rx.recv() {
                match msg {
                    // The job is the generated envelope::run body: panics are
                    // already converted to envelopes inside. Answering the call
                    // can still run user code — freeing a reply nobody took
                    // runs its handles' `Drop` — so both answer paths below
                    // catch, and this loop never unwinds out of an answer. (The
                    // `Reap` arm below deliberately does, which is its own
                    // contract.) `None` means the job detached its completion (a
                    // deferred method) — the executor task it spawned owns the
                    // answer now.
                    Msg::Call { call_id, job } => {
                        // A cancel that reached this call while it was queued
                        // claimed the reservation `submit` made, and owns the
                        // answer (`executor::holds` — a peek, sound because an
                        // id never comes back). The job must not run at all:
                        // its prefix is work the caller cancelled. Dropping it
                        // here releases the request bytes and everything the
                        // closure captured.
                        if !crate::executor::holds(call_id) {
                            continue;
                        }
                        if let Some(reply) = job() {
                            // The job answered inline — a plain method, or a
                            // deferred prefix that panicked before it could
                            // spawn. Post only if the entry is still ours: the
                            // same check-and-remove every other answer path
                            // makes (executor.rs, the answer-once gate), which
                            // is what keeps a cancel landing *during* the job
                            // from producing a second answer.
                            if crate::executor::cancel(call_id) {
                                crate::post::respond(call_id, reply);
                            } else {
                                // A cancel owns the answer, so this reply goes
                                // nowhere. Its encode has already registered
                                // any handle it returns, and the Dart wrapper
                                // that would dispose one is never built.
                                //
                                // Caught for `post::respond`'s reason, which
                                // applies verbatim: freeing runs a user `Drop`,
                                // and a panic escaping here kills this host
                                // thread and wedges every call still to come.
                                if let Err(payload) = std::panic::catch_unwind(
                                    std::panic::AssertUnwindSafe(|| {
                                        reply.into_parts().1.reclaim();
                                    }),
                                ) {
                                    crate::panic::observed(crate::panic::message(&*payload));
                                }
                            }
                        }
                    }
                    Msg::Stop => break,
                    Msg::Reap => {
                        // An empty slot is the ordinary already-dropped case,
                        // not an error: the dispatched drop disarmed it. Break
                        // either way — the point of a Reap is that this thread
                        // stops existing.
                        //
                        // The `break` is the mechanism only when something
                        // still holds a `Sender`. In the ordinary path `reap`
                        // has already removed the map's — the last one — so
                        // `rx.recv()` would end this loop by itself; the tests
                        // hold a clone precisely so the two cannot be confused.
                        //
                        // Caught only to report it, then resumed: a panicking
                        // user `Drop` still unwinds out of this closure and
                        // kills this thread, which is exactly what was being
                        // asked for. `resume_unwind` does not re-run the panic
                        // hook, so nothing is printed twice, and the thread
                        // dies on the same payload it would have. The
                        // `frustrate_finalize_*` exports cannot even do this —
                        // a panic there crosses `extern "C"`.
                        //
                        // Without the catch this is the one panic in the
                        // runtime that answers no call at all, so it reached
                        // neither side of the bridge: a crash reporter would
                        // see a clean process while an actor's teardown was
                        // blowing up.
                        if let Some(teardown) = TEARDOWN.with(|slot| slot.take()) {
                            if let Err(payload) =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(teardown))
                            {
                                crate::panic::observed(crate::panic::message(&*payload));
                                std::panic::resume_unwind(payload);
                            }
                        }
                        break;
                    }
                }
            }
        })
        .expect("frustrate: failed to spawn actor host thread");
    id
}

/// Enqueue one call on `host`. A bad host id is a bridge bug (the Dart
/// handle wrapper prevents use-after-dispose), reported loudly through the
/// normal completion path rather than UB or silence.
///
/// **Reserves the call on the cooperative executor first**, which is what
/// makes a queued call cancellable at all: a `Deferred` method's completion
/// only becomes a task when its prefix runs, so between here and there the
/// answer-once gate would otherwise hold nothing and a `FrustrateCancelToken`
/// would claim nothing. The reservation is resolved by whoever answers — the host
/// loop above, the prefix's hand-off (`executor::spawn_reserved`), or a
/// cancel. The one path that resolves none of those is a host thread that dies
/// with messages still behind it (a panicking user `Drop` during teardown;
/// `Stop`/`Reap` both ride *behind* the queue, so every ordinary teardown
/// still dequeues), and that leaks one map entry per stranded call.
pub fn submit(
    host: u64,
    call_id: u64,
    job: impl FnOnce() -> Option<crate::codec::FramedWriter> + Send + 'static,
) {
    // Before the send, not after: the host thread may dequeue the moment it
    // arrives, and a job that reaches `executor::holds` before the reservation
    // exists would be dropped as if it had been cancelled.
    crate::executor::reserve(call_id);
    let tx = {
        let mut map = hosts().lock().unwrap();
        match map.get_mut(&host) {
            Some(h) => {
                // First call attributes the host. Later calls cannot change it:
                // one actor belongs to the isolate that built it, and a handle
                // does not migrate between isolates (it is not `SendPort`-able).
                h.isolate
                    .get_or_insert((call_id >> crate::post::ISOLATE_SHIFT) as u32);
                Some(h.tx.clone())
            }
            None => None,
        }
    };
    let dead = match tx {
        Some(tx) => tx
            .send(Msg::Call {
                call_id,
                job: Box::new(job),
            })
            .is_err(),
        None => true,
    };
    if dead {
        // Nothing will ever dequeue this call, so this frame owes the answer —
        // and takes the reservation back through the same gate before posting
        // one, so a cancel that beat it here still wins.
        if crate::executor::cancel(call_id) {
            crate::post::respond(
                call_id,
                crate::envelope::run(|| panic!("frustrate: actor host {host} is stopped")),
            );
        }
    }
}

/// Number of live hosts. Test support: executor-leak pins measure this
/// around spawn/dispose (a failed spawn must strand nothing).
pub fn host_count() -> usize {
    hosts().lock().unwrap().len()
}

/// Stop the host after currently queued calls drain (FIFO). Idempotent.
///
/// Also sweeps the host's deferred registry — a belt: an orderly `dispose()`
/// has already cancelled each outstanding deferred through
/// [`cancel_deferred`] (and failed those futures on the Dart side), so the
/// sweep finds already-claimed ids and `executor::cancel` refuses them. It
/// exists for a `shutdown` that is not dispose-shaped.
pub fn shutdown(host: u64) {
    if let Some(Host { tx, .. }) = hosts().lock().unwrap().remove(&host) {
        // The host thread exits at the Stop marker; if it already died the
        // send error is irrelevant.
        let _ = tx.send(Msg::Stop);
    }
    cancel_deferred_for(host);
}

/// Drop the object and release the executor, for a handle nobody disposed.
/// Idempotent, and a no-op for an id that was already reaped or shut down.
///
/// **Called from a `NativeFinalizer` callback**, which runs on an arbitrary
/// thread with no current isolate and may not re-enter the VM: a map lock and
/// an mpsc send, and the drop itself happens later on the host thread.
///
/// **A stale id cannot reap a live host.** `NEXT_HOST` only ever increments, so
/// ids are never reused and a token held by a handle from before a hot restart
/// can only miss. That is what makes it safe for a finalizer to fire long after
/// the id it captured stopped meaning anything.
///
/// Removing the entry is not bookkeeping — it is the linearization point this
/// shares with [`shutdown`], and it is also what makes [`host_count`] fall, so
/// a leak pin can see the difference.
pub fn reap(host: u64) {
    if let Some(Host { tx, .. }) = hosts().lock().unwrap().remove(&host) {
        let _ = tx.send(Msg::Reap);
    }
    // Nobody disposed this handle, so nothing enumerated its deferred
    // completions either — drop their futures (and everything they captured).
    // Claim-only on this thread; see `cancel_deferred_for` for why that makes
    // it legal from a finalizer.
    cancel_deferred_for(host);
}

/// Reap every executor owned by a dead isolate.
///
/// **This is the hot-restart path.** Flutter's hot restart tears down
/// the isolate without running `State.dispose` or `AppLifecycleState.detached`,
/// so no application code ever gets the chance to call `dispose()` — measured
/// on macOS across three restarts: OS threads 29 -> 81, tokio workers
/// 16 -> 64, bound UDP sockets 2 -> 8, one whole actor per restart. The app had
/// done everything available to it.
///
/// The Dart-side finalizer cannot cover this: it belongs to the isolate that
/// just died, so it will never run. What does survive is the exit-port notice
/// ([`crate::post::handle_exit`]), which is where this is called from.
///
/// Each host is sent `Reap`, exactly as a finalizer would — the object drops on
/// its own executor, running the user's `Drop`, and the thread exits. Hosts of
/// *other* isolates are untouched, which is why the attribution in [`Host`]
/// has to exist at all rather than this reaping everything.
///
/// **Must not block and must not panic**: it runs from `mark_gone`, on a VM
/// thread inside an `extern "C"` frame. One short lock, then sends; the drops
/// happen later, on the host threads.
pub fn reap_isolate(isolate: u32) {
    let (ids, doomed): (Vec<u64>, Vec<Sender<Msg>>) = {
        let mut map = hosts()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ids: Vec<u64> = map
            .iter()
            .filter(|(_, h)| h.isolate == Some(isolate))
            .map(|(id, _)| *id)
            .collect();
        let doomed = ids.iter().filter_map(|id| map.remove(id)).map(|h| h.tx).collect();
        (ids, doomed)
    };
    for id in &ids {
        // Same reasoning as `reap`: the isolate's finalizers died with it, so
        // this is the only path left that can release its outstanding
        // deferred futures. Claim-only on this thread (no user code, no
        // blocking — the drops land on pool drains), which is what keeps
        // `mark_gone`'s must-not-panic contract.
        cancel_deferred_for(*id);
    }
    for tx in doomed {
        let _ = tx.send(Msg::Reap);
    }
}

#[no_mangle]
pub extern "C" fn frustrate_actor_spawn() -> u64 {
    spawn_host()
}

#[no_mangle]
pub extern "C" fn frustrate_actor_shutdown(host: u64) {
    shutdown(host);
}

/// Cancel one deferred completion. Called by the Dart host's `dispose()`,
/// once per outstanding deferred call.
///
/// Returns 1 if the cancel claimed the call (Dart now owns failing that
/// future with the disposed-deferred `StateError`), 0 if the call already
/// answered (its response is delivered or in flight — Dart must leave it to
/// complete normally).
#[no_mangle]
pub extern "C" fn frustrate_actor_cancel_deferred(host: u64, call_id: u64) -> u8 {
    cancel_deferred(host, call_id) as u8
}

/// # Safety
/// Always safe to call: `token` is an opaque host id, never dereferenced.
///
/// Takes a pointer rather than a `u64` so the symbol is directly usable as a
/// Dart `NativeFinalizer` callback, exactly as the `frustrate_finalize_*`
/// exports are. Host ids start at 1, so a real token is never null.
#[no_mangle]
pub extern "C" fn frustrate_actor_reap(token: *mut core::ffi::c_void) {
    reap(token as u64);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::FramedWriter;
    use std::sync::atomic::AtomicUsize;

    // One test fn, serialized with other post-using tests via
    // post::test_lock (the post callback is process-global).
    #[test]
    fn fifo_ordering_then_dead_host_loudness() {
        let _serial = crate::post::test_lock();
        static SEEN: Mutex<Vec<(u64, Vec<u8>)>> = Mutex::new(Vec::new());
        extern "C" fn record(call_id: u64, ptr: *mut u8, len: u64, cap: u64) {
            let bytes =
                unsafe { Vec::from_raw_parts(ptr, len as usize, cap as usize) };
            SEEN.lock().unwrap().push((call_id, bytes));
        }
        crate::post::init(record);

        static ORDER: AtomicUsize = AtomicUsize::new(0);
        let host = spawn_host();
        for i in 0..10u64 {
            submit(host, i, move || {
                let seq = ORDER.fetch_add(1, Ordering::SeqCst) as u64;
                assert_eq!(seq, i, "actor host must be FIFO");
                Some(FramedWriter::from_bytes(vec![0, i as u8]))
            });
        }
        shutdown(host);
        while SEEN.lock().unwrap().len() < 10 {
            std::thread::yield_now();
        }
        assert_eq!(SEEN.lock().unwrap()[9], (9, vec![0, 9]));

        // A call to a stopped/unknown host responds with a loud panic
        // envelope, never silence.
        submit(host, 11, || Some(FramedWriter::from_bytes(vec![])));
        while SEEN.lock().unwrap().len() < 11 {
            std::thread::yield_now();
        }
        let seen = SEEN.lock().unwrap();
        let (id, bytes) = &seen[10];
        assert_eq!(*id, 11);
        assert_eq!(bytes[0], crate::envelope::STATUS_PANIC);
    }

    /// Take the completion callback and throw the answers away.
    ///
    /// The reap tests assert on drops and thread exits, never on payloads —
    /// but `submit` still responds, and an unregistered target is a panic on
    /// the host thread (`post::deliver`), not a no-op. So they have to own the
    /// callback for the duration like every other post-using test.
    fn drain_completions() {
        extern "C" fn drain(_call_id: u64, ptr: *mut u8, len: u64, cap: u64) {
            drop(unsafe { Vec::from_raw_parts(ptr, len as usize, cap as usize) });
        }
        crate::post::init(drain);
    }

    /// Increments a counter when dropped.
    struct Sentinel(&'static AtomicUsize);
    impl Drop for Sentinel {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    thread_local! {
        /// Parked on the host thread so its destructor runs when that thread
        /// exits. The map entry is a poor proxy for "the thread is gone" —
        /// `reap` removes it synchronously under the caller's lock, long
        /// before the host has processed anything — and "the thread exits" is
        /// the claim under test.
        static EXIT_MARK: Cell<Option<Sentinel>> = const { Cell::new(None) };
    }

    /// Spin until `counter` reaches `want`, or fail naming what never happened.
    ///
    /// Bounded by a **deadline, not an iteration count**. An iteration bound
    /// bounds spins rather than elapsed time, and those diverge in exactly the
    /// wrong direction: the busier the machine, the less wall-clock a fixed
    /// number of `yield_now` calls buys, so load makes a false failure *more*
    /// likely rather than less — a flake that only ever appears in a full
    /// parallel run and never when the test is chased on its own. The deadline
    /// is generous because its only job is to make a genuine hang report
    /// instead of wedging the suite.
    fn await_count(counter: &AtomicUsize, want: usize, what: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if counter.load(Ordering::SeqCst) >= want {
                return;
            }
            std::thread::yield_now();
        }
        panic!(
            "{what}: expected {want}, still {}",
            counter.load(Ordering::SeqCst)
        );
    }

    /// A panicking user `Drop` during teardown still kills the host thread —
    /// and is reported to the panic listener on its way out.
    ///
    /// This is the one panic in the runtime that answers no call at all, so
    /// without the report it reaches neither side of the bridge. The arm
    /// catches only to report and then `resume_unwind`s, which is why the
    /// thread-exit assertion below is the other half of the test: it holds the
    /// deliberate "this kills the thread" behaviour that the catch could
    /// silently have taken away.
    #[test]
    fn a_panicking_teardown_is_reported_and_still_kills_the_host_thread() {
        let _serial = crate::post::test_lock();
        drain_completions();
        let reports = crate::panic::record_for_tests();
        static EXITS: AtomicUsize = AtomicUsize::new(0);
        let host = spawn_host();
        let before = host_count();

        // Armed from the host thread, like a generated constructor body.
        let (tx, rx) = channel::<()>();
        submit(host, 0, move || {
            arm_teardown(|| panic!("a user Drop exploded during teardown"));
            EXIT_MARK.with(|m| m.set(Some(Sentinel(&EXITS))));
            tx.send(()).unwrap();
            Some(FramedWriter::from_bytes(vec![]))
        });
        rx.recv().unwrap();

        reap(host);
        await_count(
            &EXITS,
            1,
            "the host thread survived a panicking teardown: the catch that \
             reports it swallowed the unwind instead of resuming it",
        );
        crate::panic::unregister();

        assert_eq!(host_count(), before - 1, "reap must free the map entry");
        assert_eq!(
            reports.lock().unwrap().as_slice(),
            ["a user Drop exploded during teardown"],
            "a panic that killed an actor host thread was reported nowhere"
        );
    }

    /// The teardown runs exactly once, and every later reap is a no-op rather
    /// than a second drop.
    #[test]
    fn reap_drops_once_and_is_idempotent() {
        // These submit jobs, and `submit` responds through the process-global
        // post callback. Nothing here asserts on posts, but a concurrent
        // post-using test would see the completions — so serialize.
        let _serial = crate::post::test_lock();
        drain_completions();
        static DROPS: AtomicUsize = AtomicUsize::new(0);
        static EXITS: AtomicUsize = AtomicUsize::new(0);
        let host = spawn_host();
        let before = host_count();

        // Arm from the host thread, which is where a generated constructor
        // body runs — arming from the test thread would set the wrong slot.
        let (tx, rx) = channel::<()>();
        submit(host, 0, move || {
            arm_teardown(|| {
                DROPS.fetch_add(1, Ordering::SeqCst);
            });
            EXIT_MARK.with(|m| m.set(Some(Sentinel(&EXITS))));
            tx.send(()).unwrap();
            Some(FramedWriter::from_bytes(vec![]))
        });
        rx.recv().unwrap();
        assert_eq!(DROPS.load(Ordering::SeqCst), 0, "arming must not drop");

        reap(host);
        await_count(&DROPS, 1, "the teardown never ran");
        await_count(&EXITS, 1, "the host thread never exited");
        assert_eq!(host_count(), before - 1, "reap must free the map entry");

        // Idempotent from both directions: a second reap and a shutdown of an
        // already-reaped host must find nothing and drop nothing.
        reap(host);
        shutdown(host);
        assert_eq!(DROPS.load(Ordering::SeqCst), 1, "teardown must not repeat");
    }

    /// A dead isolate's executors are released; another isolate's are not.
    ///
    /// This is the hot-restart fix. Flutter's hot restart runs no application
    /// teardown at all, so nothing on the Dart side can reach these hosts — the
    /// finalizer that would belongs to the isolate that just died.
    #[test]
    fn a_dead_isolate_takes_its_own_executors_and_only_its_own() {
        let _serial = crate::post::test_lock();
        let mine = spawn_host();
        let theirs = spawn_host();
        let before = host_count();

        // Attribution comes from the first call's id, so submit one to each.
        // `submit` responds through `post`, which is what `test_lock` serializes.
        let call = |iso: u64, host: u64| {
            submit(host, iso << crate::post::ISOLATE_SHIFT | 7, || {
                Some(crate::envelope::run(|| crate::envelope::Outcome::Error("x".into())))
            });
        };
        call(11, mine);
        call(22, theirs);
        // Let each host's thread pick its message up, so the attribution has
        // certainly been recorded before the reap reads it.
        for _ in 0..200 {
            if hosts().lock().unwrap().values().all(|h| h.isolate.is_some()) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }

        reap_isolate(11);
        assert_eq!(
            host_count(),
            before - 1,
            "isolate 11's executor was not released"
        );
        assert!(
            hosts().lock().unwrap().contains_key(&theirs),
            "isolate 22's executor must survive — reaping every host on any \
             isolate death would break a multi-isolate app"
        );

        // Idempotent: a second notice for the same isolate finds nothing.
        reap_isolate(11);
        assert_eq!(host_count(), before - 1);
        reap(theirs);
    }

    /// The wire, not just the mechanism: an isolate-death notice must reach
    /// the reap. Without this the whole path is one untested line — removing
    /// the call from `mark_gone` left every other test green.
    #[test]
    fn an_isolate_death_notice_releases_that_isolate_s_executors() {
        let _serial = crate::post::test_lock();
        let host = spawn_host();
        let before = host_count();
        submit(host, 31u64 << crate::post::ISOLATE_SHIFT | 1, || {
            Some(crate::envelope::run(|| crate::envelope::Outcome::Error("x".into())))
        });
        for _ in 0..200 {
            if hosts()
                .lock()
                .unwrap()
                .get(&host)
                .is_none_or(|h| h.isolate.is_some())
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }

        crate::post::mark_gone_for_test(31);
        assert_eq!(
            host_count(),
            before - 1,
            "an isolate death did not release the executor it owned"
        );
    }

    /// A host that was spawned and never called has no isolate, so no isolate
    /// death claims it. That window is real but empty: nothing has been
    /// constructed on it yet, so there is nothing to leak.
    // Not under `--cfg frustrate_manual_scheduler`. This asserts *when* a
    // future is dropped, and that timing belongs to a self-draining Scheduler:
    // the drop lands on the next executor drain, which under manual scheduling
    // is whenever the host next asks and never on its own. The guarantee is
    // unchanged there, only its schedule is, and the assertions here are
    // written against the pool's.
    #[cfg(not(frustrate_manual_scheduler))]
    #[test]
    fn an_unattributed_host_is_not_reaped_by_anyone() {
        let _serial = crate::post::test_lock();
        let host = spawn_host();
        let before = host_count();
        reap_isolate(0);
        reap_isolate(1);
        assert_eq!(host_count(), before, "an unused host was claimed");
        reap(host);
    }

    /// A reap racing behind an orderly drop finds an empty slot. The thread
    /// must still exit — "nothing to drop" is not "nothing to do".
    ///
    /// **Holds a `Sender` clone across the reap**, without which this asserts
    /// nothing: `reap` removes the map's Sender, and that alone ends
    /// `rx.recv()`, so the loop would exit alike whether or not `Msg::Reap`
    /// breaks it. Written the obvious way first, and the negative check caught
    /// it — deleting the `break` left the test green.
    #[test]
    fn reap_with_a_disarmed_slot_still_exits() {
        let _serial = crate::post::test_lock();
        drain_completions();
        static DROPS: AtomicUsize = AtomicUsize::new(0);
        static EXITS: AtomicUsize = AtomicUsize::new(0);
        let host = spawn_host();
        let before = host_count();

        let (tx, rx) = channel::<()>();
        submit(host, 0, move || {
            arm_teardown(|| {
                DROPS.fetch_add(1, Ordering::SeqCst);
            });
            // What the generated dispatched drop does before it frees the Box.
            disarm_teardown();
            EXIT_MARK.with(|m| m.set(Some(Sentinel(&EXITS))));
            tx.send(()).unwrap();
            Some(FramedWriter::from_bytes(vec![]))
        });
        rx.recv().unwrap();

        // Keep a Sender alive so the channel cannot close underneath the test.
        // Now the ONLY thing that can end that loop is `Msg::Reap` breaking it.
        let _keepalive = hosts().lock().unwrap().get(&host).map(|h| h.tx.clone()).expect("live");

        reap(host);
        await_count(&EXITS, 1, "an empty slot must not keep the thread alive");
        assert_eq!(DROPS.load(Ordering::SeqCst), 0, "the drop already ran");
        assert_eq!(host_count(), before - 1);
    }

    // ------------------------------------------------- deferred completions --

    /// Parks until dropped; the flag makes the drop observable. `Output` is
    /// an `Outcome` so it is spawnable as a deferred body.
    // The `Sentinel` is never read — it is held purely so that dropping this
    // future runs its `Drop`, which is the observation the test makes.
    struct ParkedBody(#[allow(dead_code)] Sentinel);
    impl std::future::Future for ParkedBody {
        type Output = crate::envelope::Outcome;
        fn poll(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Self::Output> {
            std::task::Poll::Pending
        }
    }

    /// Record completions like `record`, but only count — the deferred tests
    /// assert on which call ids ever answered.
    fn recording_posts() -> &'static Mutex<Vec<u64>> {
        static SEEN_IDS: Mutex<Vec<u64>> = Mutex::new(Vec::new());
        extern "C" fn record_id(call_id: u64, ptr: *mut u8, len: u64, cap: u64) {
            drop(unsafe { Vec::from_raw_parts(ptr, len as usize, cap as usize) });
            SEEN_IDS.lock().unwrap().push(call_id);
        }
        crate::post::init(record_id);
        SEEN_IDS.lock().unwrap().clear();
        &SEEN_IDS
    }

    /// The dispose-cancel protocol end to end on the real global executor:
    /// a claimed cancel drops the future and the call NEVER answers; a call
    /// that answered refuses the claim.
    ///
    /// Red fence for the claim contract: with `executor::cancel`'s claim
    /// ignored (respond unconditionally in the spawn wrapper), the cancelled
    /// id's completion arrives and the `seen` assert fails. Exercised
    /// together with executor.rs's `a_claimed_cancel_beats_a_mid_poll_completion`.
    // Not under `--cfg frustrate_manual_scheduler`. This asserts *when* a
    // future is dropped, and that timing belongs to a self-draining Scheduler:
    // the drop lands on the next executor drain, which under manual scheduling
    // is whenever the host next asks and never on its own. The guarantee is
    // unchanged there, only its schedule is, and the assertions here are
    // written against the pool's.
    #[cfg(not(frustrate_manual_scheduler))]
    #[test]
    fn a_cancelled_deferred_never_answers_and_a_completed_one_refuses_the_claim() {
        let _serial = crate::post::test_lock();
        let seen = recording_posts();
        static PARKED_DROPS: AtomicUsize = AtomicUsize::new(0);
        let host = spawn_host();

        // Two deferred calls dispatched on the host thread, exactly as the
        // generated prefix glue does: job spawns the body and returns None.
        // One `submit` each, because a prefix hands off through
        // `executor::spawn_reserved` and the reservation it consumes is the
        // one `submit` made — a second body spawned under a *borrowed* id
        // would be refused (and would be a bridge bug in real glue: one call,
        // one completion).
        submit(host, 101, move || {
            crate::deferred::spawn_deferred(101, ParkedBody(Sentinel(&PARKED_DROPS)));
            None
        });
        let (tx, rx) = channel::<()>();
        submit(host, 102, move || {
            crate::deferred::spawn_deferred(102, async {
                crate::envelope::Outcome::Error("done".into())
            });
            tx.send(()).unwrap();
            None
        });
        rx.recv().unwrap();

        // 102 completes naturally on the pool-driven executor. Deadline rather
        // than an iteration count, for the reason `await_count` spells out.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if seen.lock().unwrap().contains(&102) {
                break;
            }
            std::thread::yield_now();
        }
        assert!(seen.lock().unwrap().contains(&102), "the ready body answers");
        assert!(
            !cancel_deferred(host, 102),
            "an answered call must refuse the claim — dispose() would \
             otherwise fail a future whose real completion is delivered"
        );

        // 101 is parked; the cancel claims it, the future is dropped on a
        // drain, and no completion for it ever arrives.
        assert!(cancel_deferred(host, 101), "a live deferred is claimable");
        await_count(&PARKED_DROPS, 1, "the cancelled future was never dropped");
        assert!(
            !seen.lock().unwrap().contains(&101),
            "a claimed cancel owns the answer: the executor must not respond \
             for call 101"
        );
        assert!(!cancel_deferred(host, 101), "a claim is single-shot");
        shutdown(host);
    }

    /// A reaped host takes its outstanding deferred futures with it — the
    /// GC/hot-restart path, where no Dart `dispose()` enumerates them. The
    /// observable is the future's drop; no completion is owed or sent.
    // Not under `--cfg frustrate_manual_scheduler`: the drops it waits for land
    // on an executor drain, which under manual scheduling only a host performs.
    // Same reason as the two gated above.
    #[cfg(not(frustrate_manual_scheduler))]
    #[test]
    fn reaping_a_host_drops_its_outstanding_deferred_futures() {
        let _serial = crate::post::test_lock();
        let seen = recording_posts();
        static REAPED_DROPS: AtomicUsize = AtomicUsize::new(0);
        let host = spawn_host();

        let (tx, rx) = channel::<()>();
        submit(host, 201, move || {
            crate::deferred::spawn_deferred(201, ParkedBody(Sentinel(&REAPED_DROPS)));
            tx.send(()).unwrap();
            None
        });
        rx.recv().unwrap();

        reap(host);
        await_count(
            &REAPED_DROPS,
            1,
            "reap must release the deferred future (nothing else can)",
        );
        assert!(
            !seen.lock().unwrap().contains(&201),
            "no answer is owed to a reaped handle's call"
        );
        assert!(
            deferred().lock().unwrap().get(&host).is_none(),
            "the host's registry entry must not outlive the reap"
        );
    }

    // ------------------------------------------------------- queued cancels --

    /// A cancel that reaches a call still **queued** behind a slow one claims
    /// it, and the job is dropped without ever running.
    ///
    /// The measurement takes both observations, and neither alone would do: a
    /// job that ran would drop its captures too, and a job leaked unrun would
    /// report neither. What a caller sees — a rejected future — is not
    /// evidence of either, which is why this is asserted here rather than
    /// only through the Dart surface.
    ///
    /// Red fence: with the `executor::holds` skip removed from the host loop,
    /// the job runs (`RAN`) and answers, so both the flag and the `seen`
    /// assert fail. With the `executor::reserve` in `submit` removed, the
    /// cancel claims nothing and the first assert fails.
    ///
    /// Call 603 is the control: queued behind the same slow call, cancelled by
    /// nobody, and it answers normally.
    #[test]
    fn a_cancel_claims_a_call_that_is_still_queued() {
        let _serial = crate::post::test_lock();
        let seen = recording_posts();
        static QUEUED_DROPS: AtomicUsize = AtomicUsize::new(0);
        static RAN: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(false);
        RAN.store(false, Ordering::SeqCst);
        let host = spawn_host();

        // The fence is the FIFO, not a clock: 601 holds the host thread until
        // this test lets go, so 602 and 603 are certainly still queued below.
        let (go_tx, go_rx) = channel::<()>();
        submit(host, 601, move || {
            go_rx.recv().unwrap();
            Some(FramedWriter::from_bytes(vec![0]))
        });
        submit(host, 602, move || {
            RAN.store(true, Ordering::SeqCst);
            Some(FramedWriter::from_bytes(vec![0]))
        });
        let fence = Sentinel(&QUEUED_DROPS);
        submit(host, 603, move || {
            // Captured only to be dropped with this job; the sentinel proves
            // the *claimed* job's captures are released, and this one keeps
            // the control's shape identical to it.
            let _fence = fence;
            Some(FramedWriter::from_bytes(vec![0]))
        });

        assert!(
            crate::executor::cancel(602),
            "a call queued on a host is claimable before its job runs — the \
             reservation submit() made is what the claim takes"
        );
        assert!(!crate::executor::cancel(602), "a claim is single-shot");
        go_tx.send(()).unwrap();

        // 603's answer is FIFO-after 602's slot, so it proves the loop has
        // already passed (and dropped) the claimed job.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if seen.lock().unwrap().contains(&603) {
                break;
            }
            std::thread::yield_now();
        }
        let seen = seen.lock().unwrap();
        assert!(seen.contains(&603), "the uncancelled queued call must answer");
        assert!(
            !RAN.load(Ordering::SeqCst),
            "a claimed cancel must stop the job from running at all: a \
             deferred prefix is user code the caller cancelled"
        );
        assert_eq!(
            QUEUED_DROPS.load(Ordering::SeqCst),
            1,
            "the claimed job must be dropped, not stranded in the queue"
        );
        assert!(
            !seen.contains(&602),
            "a claimed cancel owns the answer: the host must not respond for \
             call 602"
        );
        drop(seen);
        shutdown(host);
    }

    /// A reply the host loop cannot post gives back the handles it minted.
    ///
    /// The job runs to completion — encoding, and so registering, any handle it
    /// returns — and only then does the loop ask whether the answer is still
    /// its to give. A cancel that landed *during* the job owns it, so the reply
    /// is dropped here: the actor twin of the executor's gate, and the only
    /// thing left that can free the object.
    #[test]
    fn a_reply_the_host_cannot_post_gives_its_handles_back() {
        let _serial = crate::post::test_lock();
        let seen = recording_posts();
        static RECLAIMED: Mutex<Vec<u64>> = Mutex::new(Vec::new());
        unsafe fn note(h: u64) {
            RECLAIMED.lock().unwrap().push(h);
        }
        RECLAIMED.lock().unwrap().clear();

        let host = spawn_host();
        let (started_tx, started_rx) = channel::<()>();
        let (go_tx, go_rx) = channel::<()>();
        submit(host, 701, move || {
            started_tx.send(()).unwrap();
            go_rx.recv().unwrap();
            let mut w = FramedWriter::status(crate::envelope::STATUS_OK);
            unsafe { w.write_minted(0xACE, note) };
            Some(w)
        });
        started_rx.recv().unwrap();
        assert!(
            crate::executor::cancel(701),
            "the call is live while its job runs, so the cancel claims it"
        );
        go_tx.send(()).unwrap();

        // FIFO-after 701, so its answer proves the loop has already passed the
        // claimed one. Same fence the queued-cancel test uses.
        submit(host, 702, || Some(FramedWriter::from_bytes(vec![0])));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if seen.lock().unwrap().contains(&702) {
                break;
            }
            std::thread::yield_now();
        }
        let posted = seen.lock().unwrap();
        assert!(posted.contains(&702), "the uncancelled call must answer");
        assert!(!posted.contains(&701), "a claimed cancel owns the answer");
        assert_eq!(
            *RECLAIMED.lock().unwrap(),
            vec![0xACE],
            "and the reply the loop could not post freed what it had minted"
        );
        drop(posted);
        shutdown(host);
    }

    /// The hand-off from host to executor is atomic: a cancel that lands after
    /// the prefix started but before it spawned still claims the call, and the
    /// body it was about to hand over is dropped **un-polled**.
    ///
    /// This is the window a reservation alone would leave open. Red fence,
    /// verified: with `deferred::spawn_deferred` calling `executor::spawn`
    /// instead of `spawn_reserved`, the prefix inserts its task *after* the
    /// claim — so the body is polled and parked, nothing ever drops it, and a
    /// cancel that reported the call claimed has left it live and answerable.
    /// `REFUSED_DROPS` never reaches 1.
    #[test]
    fn a_cancel_during_the_prefix_beats_the_hand_off() {
        let _serial = crate::post::test_lock();
        let seen = recording_posts();
        static REFUSED_DROPS: AtomicUsize = AtomicUsize::new(0);
        static POLLS: AtomicUsize = AtomicUsize::new(0);
        let host = spawn_host();

        /// Records its polls, and its drop. `ParkedBody` cannot say the first
        /// thing, and a refused hand-off has to be told from a body that
        /// parked once and was then dropped.
        struct WatchedBody(#[allow(dead_code)] Sentinel);
        impl std::future::Future for WatchedBody {
            type Output = crate::envelope::Outcome;
            fn poll(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Self::Output> {
                POLLS.fetch_add(1, Ordering::SeqCst);
                std::task::Poll::Pending
            }
        }

        let (started_tx, started_rx) = channel::<()>();
        let (go_tx, go_rx) = channel::<()>();
        submit(host, 701, move || {
            // The prefix is running now — and waits here, so the cancel below
            // lands strictly between dequeue and hand-off.
            started_tx.send(()).unwrap();
            go_rx.recv().unwrap();
            crate::deferred::spawn_deferred(701, WatchedBody(Sentinel(&REFUSED_DROPS)));
            None
        });
        started_rx.recv().unwrap();

        assert!(
            crate::executor::cancel(701),
            "a call whose prefix is running is still the canceller's to claim"
        );
        go_tx.send(()).unwrap();
        await_count(&REFUSED_DROPS, 1, "the refused body was never dropped");
        assert_eq!(
            POLLS.load(Ordering::SeqCst),
            0,
            "a refused hand-off must drop the body un-polled — the claim \
             happened before the executor ever saw it"
        );
        assert!(
            !seen.lock().unwrap().contains(&701),
            "the canceller owns the answer, so nothing may respond for 701"
        );
        assert!(
            deferred().lock().unwrap().get(&host).is_none(),
            "the deferred registration must be released with the refused body"
        );
        shutdown(host);
    }
}
