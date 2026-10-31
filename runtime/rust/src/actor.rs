//! Native actor hosts: one dedicated thread per actor instance.
//!
//! An actor host is a FIFO of messages drained by its own OS thread, the
//! native analog of the web's worker-hosted wasm instance. The objects the
//! actor hosts live in that thread's object table (objects.rs) and are
//! created, used and dropped exclusively on it, so actor types need no
//! `Send`/`Sync` and no locks: serialization is the executor itself.
//!
//! # Routing takes no lock on the caller's thread
//!
//! Every actor call is issued from a Dart isolate's thread, and for most apps
//! that is the main isolate. So the path from a call to its executor is built
//! to take no lock at all:
//!
//! - **The Dart side holds the executor.** `frustrate_actor_spawn` returns a
//!   counted pointer to the [`Host`], and the Dart host object passes it on
//!   every call. There is no registry lookup between a call and its queue.
//!   The pointer stays valid for as long as the Dart object that holds it:
//!   that object owns one strong count and releases it from a
//!   `NativeFinalizer` (`frustrate_actor_release`). The GC backstop owns a
//!   count of its own (`frustrate_actor_retain`/`frustrate_actor_reap`), so
//!   the two finalizers may run in either order.
//! - **The queue is lock-free.** [`Inbox`] is a Treiber stack that producers
//!   push with one compare-and-swap and the host thread takes whole. A failed
//!   CAS means another producer succeeded, so no producer ever waits on
//!   another. The wake-up is `Thread::unpark`. (`std::sync::mpsc` is not used
//!   because a send to a receiver that is parked takes the channel's waker
//!   mutex, which the receiver also takes.)
//! - **Stopping is a flag and a message.** `dispose()` and the GC backstop set
//!   [`Host::stopping`] and push [`Msg::Stop`]. Everything else stopping a host
//!   does (answering what was queued behind the stop, sweeping deferred
//!   completions, leaving the registry, draining the object table) runs on the
//!   host thread.
//!
//! The one registry here, [`hosts`], exists for the hot-restart path, which
//! must find every host a dead isolate owned. The host thread enters itself
//! there and leaves on exit, and [`reap_isolate`] reads it from the VM's
//! exit-notice thread. No caller thread touches it.
//!
//! What a call still does on the caller's thread is reserve its id with the
//! cooperative executor (`executor::reserve`), the answer-once gate shared by
//! every async call; that registry's lock is executor.rs's.
//!
//! Not compiled for wasm: there the host is a Worker driving its own
//! instance through the ordinary exports, and the Dart side posts to it.

use crate::bounded::{Lock, Outside};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::c_void;
use std::ptr::{null_mut, NonNull};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::Thread;

/// One dispatched call body. `Some(reply)` is the response envelope the loop
/// posts — the framed writer, so the ledger of what the reply minted travels
/// with it (`codec::Minted`); `None` means the job **detached** its completion
/// — a deferred method's prefix ran and handed the rest to the cooperative
/// executor (`deferred::spawn_deferred`), which now owns answering the call.
type Job = Box<dyn FnOnce() -> Option<crate::codec::FramedWriter> + Send>;

enum Msg {
    /// `pending` is the call's share of the idle barrier's count, held until
    /// it is answered or dropped unrun (idle.rs).
    Call {
        call_id: u64,
        job: Job,
        pending: crate::idle::Pending,
    },
    /// A closure the actor posted to itself (`Addr::post`). It runs on the
    /// root, or is dropped unrun if the root is gone. `pending` as for a call:
    /// posted work is work the idle barrier counts.
    Posted {
        job: crate::addr::Posted,
        pending: crate::idle::Pending,
    },
    /// The executor's last message. Everything queued before it runs first;
    /// then the host drops every object still in its table and exits. What
    /// arrives after it is refused.
    ///
    /// One message for the orderly `dispose()` and for the reap of a handle
    /// nobody disposed: after an orderly dispose the table is already empty
    /// (the dispatched drop emptied it), so draining it is the reap's work and
    /// a no-op for the dispose.
    Stop,
}

/// A multi-producer, single-consumer queue whose push takes no lock.
///
/// A Treiber stack: [`push`](Inbox::push) links a node in front with one
/// compare-and-swap, and the host thread takes the whole stack with one swap
/// and reverses it, which restores arrival order. Taking the whole stack is
/// what makes this sound without hazard pointers: the consumer never pops a
/// node another thread can still be looking at, because producers never read
/// a node after publishing it.
///
/// [`close`](Inbox::close) swaps in a sentinel that refuses every later push,
/// handing the message back to its producer.
pub struct Inbox {
    head: AtomicPtr<Node>,
}

struct Node {
    msg: Msg,
    next: *mut Node,
}

impl Inbox {
    fn new() -> Self {
        Inbox { head: AtomicPtr::new(null_mut()) }
    }

    /// The closed sentinel: well aligned, never an allocation, never
    /// dereferenced.
    fn closed() -> *mut Node {
        NonNull::dangling().as_ptr()
    }

    /// Queue `msg`, or hand it back if the inbox is closed.
    fn push(&self, msg: Msg) -> Result<(), Msg> {
        let node = Box::into_raw(Box::new(Node { msg, next: null_mut() }));
        let mut head = self.head.load(Ordering::Relaxed);
        loop {
            if head == Self::closed() {
                return Err(unsafe { Box::from_raw(node) }.msg);
            }
            // The node is still private to this thread.
            unsafe { (*node).next = head };
            match self
                .head
                .compare_exchange_weak(head, node, Ordering::Release, Ordering::Relaxed)
            {
                Ok(_) => return Ok(()),
                Err(now) => head = now,
            }
        }
    }

    /// Everything queued so far, oldest first. Consumer only.
    fn take(&self) -> Vec<Msg> {
        let head = self.head.swap(null_mut(), Ordering::Acquire);
        debug_assert!(head != Self::closed(), "take after close");
        Self::collect(head)
    }

    /// Refuse every later push, and return what was queued, oldest first.
    /// Consumer only.
    fn close(&self) -> Vec<Msg> {
        let head = self.head.swap(Self::closed(), Ordering::Acquire);
        if head == Self::closed() {
            return Vec::new();
        }
        Self::collect(head)
    }

    /// Unlink a taken stack and put it in arrival order.
    fn collect(mut p: *mut Node) -> Vec<Msg> {
        let mut out = Vec::new();
        while !p.is_null() {
            // Taken by the swap: no producer reads a node after publishing it,
            // so this thread owns every node on the list.
            let node = unsafe { Box::from_raw(p) };
            p = node.next;
            out.push(node.msg);
        }
        out.reverse();
        out
    }
}

impl Drop for Inbox {
    fn drop(&mut self) {
        let head = *self.head.get_mut();
        if head != Self::closed() {
            drop(Self::collect(head));
        }
    }
}

/// A live executor: its queue, its thread, and the isolate that owns it.
///
/// Shared by the host thread, the Dart host object (one strong count, through
/// the pointer `frustrate_actor_spawn` returns), the GC backstop (another), and
/// the [`hosts`] registry while the thread runs.
pub struct Host {
    id: u64,
    /// The isolate that spawned this host. An actor belongs to the isolate
    /// that built it: a handle is not `SendPort`-able, so it cannot migrate.
    isolate: u32,
    /// Who hears a panic on this executor that answers no call: the id Dart
    /// filed this actor's zone under (uncaught.rs). Zero in unit tests.
    owner: u64,
    inbox: Inbox,
    thread: Thread,
    /// Set once, by the first request to stop. Makes stopping idempotent, and
    /// uncounts the host in [`host_count`] at the moment of the request.
    stopping: AtomicBool,
}

impl Host {
    /// Queue `msg` and wake the host thread, or hand `msg` back if the host
    /// has stopped.
    fn post(&self, msg: Msg) -> Result<(), Msg> {
        self.inbox.push(msg)?;
        self.thread.unpark();
        Ok(())
    }

    /// This host's id.
    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    /// Queue a closure the actor posted to itself, behind everything already
    /// queued. Hands it back if the host has stopped.
    pub(crate) fn post_posted(
        &self,
        job: crate::addr::Posted,
    ) -> Result<(), crate::addr::Posted> {
        let msg = Msg::Posted {
            job,
            pending: crate::idle::Pending::begin(),
        };
        self.post(msg).map_err(|msg| match msg {
            Msg::Posted { job, .. } => job,
            _ => unreachable!("post hands back the message it was given"),
        })
    }

    /// Ask the host to stop after what is already queued. Idempotent.
    pub(crate) fn request_stop(&self) {
        if !self.stopping.swap(true, Ordering::AcqRel) {
            // Only the host thread closes the inbox, and only after this Stop,
            // so this push cannot be refused.
            let _ = self.post(Msg::Stop);
        }
    }

    /// Enqueue one call. See [`submit`].
    pub fn submit(
        &self,
        call_id: u64,
        job: impl FnOnce() -> Option<crate::codec::FramedWriter> + Send + 'static,
    ) {
        // Before the push, not after: the host thread may dequeue the moment
        // it arrives, and a job that reaches `executor::holds` before the
        // reservation exists would be dropped as if it had been cancelled.
        crate::executor::reserve(call_id);
        // This frame may answer the call itself (a stopped host, below), after
        // the message holding the call's own share has been dropped.
        let _answering = crate::idle::Pending::begin();
        let msg = Msg::Call {
            call_id,
            job: Box::new(job),
            pending: crate::idle::Pending::begin(),
        };
        if let Err(msg) = self.post(msg) {
            drop(msg);
            refuse(self.id, call_id);
        }
    }
}

thread_local! {
    /// The host this thread IS (empty elsewhere). Set once at thread birth.
    /// Read by `deferred::spawn_deferred`, which records the call in its
    /// host's registry, and by `Addr::__here`, which needs the host to post
    /// to; neither is handed the host by the job it runs in.
    static CURRENT_HOST: std::cell::OnceCell<Arc<Host>> = const { std::cell::OnceCell::new() };
}

/// The host id of the executor thread this is called on. Panics off a host
/// thread — only `deferred::spawn_deferred` calls it, and generated glue only
/// reaches that from a dispatched actor job.
pub(crate) fn current_host() -> u64 {
    current_host_id()
        .expect("frustrate: spawn_deferred called outside an actor executor — bridge bug")
}

/// The id of the host this thread is, if it is one.
pub(crate) fn current_host_id() -> Option<u64> {
    CURRENT_HOST.with(|c| c.get().map(|h| h.id))
}

/// The host this thread is. Panics off a host thread: an `Addr` is built only
/// by generated glue running on its actor's executor.
pub(crate) fn current() -> Arc<Host> {
    CURRENT_HOST.with(|c| c.get().map(Arc::clone)).expect(
        "frustrate: an Addr can only be made on its actor's executor — bridge bug",
    )
}

static NEXT_HOST: AtomicU64 = AtomicU64::new(1);

/// Every host whose thread is running, by id: what [`reap_isolate`] searches.
///
/// Entered and left by the host thread itself ([`run`]), and read by
/// `reap_isolate` on the VM thread that delivers isolate-exit notices. No
/// caller thread takes this lock: a call reaches its host through the pointer
/// the Dart side holds.
///
/// The bounded lock, because `reap_isolate` reaches it from `post::mark_gone`,
/// which a post from the main isolate's thread can run. Entries are
/// [`Outside`]: the last `Arc` to a host drops it, which must not happen
/// under the lock.
static HOSTS: Lock<BTreeMap<u64, Outside<Arc<Host>>>> = Lock::new(BTreeMap::new());

/// Outstanding deferred completions by owning host: what a stopping host
/// cancels, the calls whose futures are live on the cooperative executor
/// after their prefixes ran on it.
///
/// Registered by `deferred::spawn_deferred` (on the host thread, during the
/// prefix's job) and unregistered by its drop guard when the future is
/// dropped — completion, cancellation, and a panicking poll all end there.
/// Swept by the host thread when it stops ([`cancel_deferred_for`]).
///
/// The bounded lock: its takers are the host thread and whichever thread
/// drops a deferred future, which under the manual scheduler can be the main
/// isolate's.
static DEFERRED: Lock<BTreeMap<u64, BTreeSet<u64>>> = Lock::new(BTreeMap::new());

pub(crate) fn register_deferred(host: u64, call_id: u64) {
    DEFERRED.with(|map| {
        map.entry(host).or_default().insert(call_id);
    });
}

pub(crate) fn unregister_deferred(host: u64, call_id: u64) {
    DEFERRED.with(|map| {
        if let Some(set) = map.get_mut(&host) {
            set.remove(&call_id);
            if set.is_empty() {
                map.remove(&host);
            }
        }
    });
}

/// Cancel every outstanding deferred completion of `host`, when it stops.
///
/// The backstop for the paths where no Dart-side `dispose()` enumerates them:
/// a reaped handle and a dead isolate. After an orderly `dispose()`, which has
/// already cancelled each one through `frustrate_call_cancel`, this finds
/// claimed ids and `executor::cancel` refuses them. Nobody is waiting on these
/// calls, so no answer is owed; the point is dropping the futures and what
/// they captured.
///
/// Runs on the host thread after its last call, so it also catches a deferred
/// prefix that was still queued when the stop was requested.
fn cancel_deferred_for(host: u64) {
    let ids: Vec<u64> = DEFERRED
        .with(|map| map.remove(&host))
        .map(|set| set.into_iter().collect())
        .unwrap_or_default();
    for id in ids {
        crate::executor::cancel(id);
    }
}

/// Answer `call_id` as refused by a stopped host, unless a cancel owns it.
///
/// Takes the call back through the executor's answer-once gate before
/// posting, so a cancel that got there first still wins.
fn refuse(host: u64, call_id: u64) {
    if crate::executor::cancel(call_id) {
        crate::post::respond(
            call_id,
            crate::envelope::run(|| panic!("frustrate: actor host {host} is stopped")),
        );
    }
}

/// Start an executor owned by `isolate`.
///
/// Takes no lock on the calling thread. The host enters the [`hosts`]
/// registry from its own thread, and then checks whether its isolate has
/// already died, which closes the window in which an isolate's exit notice
/// could miss a host it had just spawned.
pub fn spawn_host(isolate: u32) -> Arc<Host> {
    spawn_owned(isolate, 0)
}

/// [`spawn_host`], with the owner a panic that answers no call is reported to.
pub(crate) fn spawn_owned(isolate: u32, owner: u64) -> Arc<Host> {
    let id = NEXT_HOST.fetch_add(1, Ordering::Relaxed);
    // The thread needs its `Host`, and the `Host` needs the thread's handle
    // to wake it. So the thread starts first and waits for its `Host` here.
    let handoff = Arc::new(AtomicPtr::<Host>::new(null_mut()));
    let theirs = Arc::clone(&handoff);
    let thread = std::thread::Builder::new()
        .name(format!("frustrate-actor-{id}"))
        .spawn(move || {
            let host = loop {
                let p = theirs.swap(null_mut(), Ordering::Acquire);
                if !p.is_null() {
                    break unsafe { Arc::from_raw(p) };
                }
                std::thread::park();
            };
            run(host);
        })
        .expect("frustrate: failed to spawn actor host thread")
        .thread()
        .clone();
    let host = Arc::new(Host {
        id,
        isolate,
        owner,
        inbox: Inbox::new(),
        thread,
        stopping: AtomicBool::new(false),
    });
    handoff.store(Arc::into_raw(Arc::clone(&host)) as *mut Host, Ordering::Release);
    host.thread.unpark();
    host
}

/// The host thread's loop.
fn run(host: Arc<Host>) {
    // Actor host threads may block on Dart (DartFunction::call).
    crate::callback::enter_worker_context();
    // So a deferred prefix dispatched here can name its own host
    // (`current_host`) without the job carrying the id.
    CURRENT_HOST.with(|c| {
        let _ = c.set(Arc::clone(&host));
    });
    // What this thread queues, and what panics here with no call, is the
    // actor's.
    crate::uncaught::enter(host.owner);
    let entry = Outside::new(Arc::clone(&host));
    let displaced = HOSTS.with(|map| map.insert(host.id, entry));
    drop(displaced.map(Outside::into_inner));
    // After entering the registry, not before: an exit notice either finds
    // this host there, or was delivered before it entered, and then the
    // isolate is already marked gone here. Both orders hold because the
    // registry lock orders this insert against `reap_isolate`'s scan, and
    // `mark_gone` marks the isolate before it scans.
    if crate::post::isolate_gone(host.isolate) {
        host.request_stop();
    }
    loop {
        let batch = host.inbox.take();
        if batch.is_empty() {
            // A wake-up that raced this check is not lost: `unpark` before
            // `park` makes the park return at once.
            std::thread::park();
            continue;
        }
        let mut batch = batch.into_iter();
        while let Some(msg) = batch.next() {
            match msg {
                Msg::Call { call_id, job, pending: _pending } => serve(call_id, job),
                Msg::Posted { job, pending: _pending } => crate::addr::run(job, host.owner),
                Msg::Stop => {
                    stop(&host, batch);
                    return;
                }
            }
        }
    }
}

/// Run one call and answer it.
fn serve(call_id: u64, job: Job) {
    // A cancel that reached this call while it was queued claimed the
    // reservation `submit` made, and owns the answer (`executor::holds` — a
    // peek, sound because an id never comes back). The job must not run at
    // all: its prefix is work the caller cancelled. Dropping it here releases
    // the request bytes and everything the closure captured.
    if !crate::executor::holds(call_id) {
        return;
    }
    // The job is the generated envelope::run body: panics are already
    // converted to envelopes inside. Answering the call can still run user
    // code — freeing a reply nobody took runs its handles' `Drop` — so the
    // reclaim below is caught, and this loop never unwinds out of an answer.
    // `None` means the job detached its completion (a deferred method) — the
    // executor task it spawned owns the answer now.
    let Some(reply) = job() else { return };
    // The job answered inline — a plain method, or a deferred prefix that
    // panicked before it could spawn. Post only if the entry is still ours:
    // the same check-and-remove every other answer path makes (executor.rs,
    // the answer-once gate), which is what keeps a cancel landing *during* the
    // job from producing a second answer.
    if crate::executor::cancel(call_id) {
        crate::post::respond(call_id, reply);
    } else {
        // A cancel owns the answer, so this reply goes nowhere. Its encode has
        // already registered any handle it returns, and the Dart wrapper that
        // would dispose one is never built.
        //
        // Caught for `post::respond`'s reason, which applies verbatim: freeing
        // runs a user `Drop`, and a panic escaping here kills this host thread
        // and wedges every call still to come.
        if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            reply.into_parts().1.reclaim();
        })) {
            crate::panic::observed(crate::panic::message(&*payload));
        }
    }
}

/// The host's last act: refuse what came after the stop, release its deferred
/// completions, leave the registry, and drop every object still in its table.
fn stop(host: &Host, behind: impl Iterator<Item = Msg>) {
    // Closing first means nothing posted from here on, by anyone, can land
    // in a queue nobody will drain: it is handed back to its sender instead.
    for msg in behind.chain(host.inbox.close()) {
        match msg {
            Msg::Call { call_id, .. } => refuse(host.id, call_id),
            // Behind the stop, so the actor is disposed or about to be
            // drained: dropped unrun, like a post that finds its root gone.
            Msg::Posted { job, pending: _pending } => crate::addr::discard(job),
            Msg::Stop => {}
        }
    }
    // Claim-only on this thread: the futures drop on later executor drains.
    cancel_deferred_for(host.id);
    let removed = HOSTS.with(|map| map.remove(&host.id));
    drop(removed.map(Outside::into_inner));
    // Last, because it runs user `Drop`s. After an orderly dispose the table
    // is empty; otherwise each object drops as a leak, naming its type, and a
    // panicking `Drop` is reported without stranding the rest.
    crate::objects::drain();
}

/// Enqueue one call on the host behind `host`.
///
/// Takes no lock: the host is the pointer the Dart side holds, and the queue
/// is lock-free. A stopped host answers the call with a loud panic envelope
/// rather than dropping it.
///
/// **Reserves the call on the cooperative executor first**, which is what
/// makes a queued call cancellable at all: a `Deferred` method's completion
/// only becomes a task when its prefix runs, so between here and there the
/// answer-once gate would otherwise hold nothing and a `FrustrateCancelToken`
/// would claim nothing. The reservation is resolved by whoever answers — the
/// host loop, the prefix's hand-off (`executor::spawn_reserved`), a refusal by
/// a stopped host, or a cancel. The one path that resolves none of those is a
/// host thread that dies with messages still behind it, and the host loop
/// unwinds out of nothing but a job, which catches its own panics.
///
/// # Safety
/// `host` must be a pointer `frustrate_actor_spawn` returned whose count the
/// caller still holds.
pub unsafe fn submit(
    host: *const c_void,
    call_id: u64,
    job: impl FnOnce() -> Option<crate::codec::FramedWriter> + Send + 'static,
) {
    unsafe { &*(host as *const Host) }.submit(call_id, job)
}

/// Number of hosts in the registry not yet asked to stop. A host counts once
/// its thread has entered the registry, which it does before serving its
/// first call. Test support: executor-leak pins measure this around
/// spawn/dispose (a failed spawn must strand nothing). Derived from the
/// registry on each call, so it costs nothing until asked.
pub fn host_count() -> usize {
    HOSTS.with(|map| {
        map.values()
            .filter(|host| !host.stopping.load(Ordering::Acquire))
            .count()
    })
}

/// Stop every executor owned by a dead isolate.
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
/// Each host is stopped exactly as a finalizer would stop it: its objects drop
/// on its own executor, running the user's `Drop`, and the thread exits. Hosts
/// of *other* isolates are untouched, which is why a host records its isolate.
/// The two strong counts the dead isolate's Dart objects held are never
/// released, so each such host leaks its small `Host` struct; its thread,
/// queue contents and objects are all released.
///
/// **Must not block and must not panic**: it runs from `mark_gone`, on a VM
/// thread inside an `extern "C"` frame. One short lock, poison-tolerant, then
/// lock-free posts.
pub fn reap_isolate(isolate: u32) {
    let doomed: Vec<Outside<Arc<Host>>> = HOSTS.with(|map| {
        map.values()
            .filter(|h| h.isolate == isolate)
            .map(|h| Outside::new(Arc::clone(h)))
            .collect()
    });
    for host in doomed.into_iter().map(Outside::into_inner) {
        host.request_stop();
    }
}

/// Start an executor and return the Dart side's counted pointer to it.
/// Release it with [`frustrate_actor_release`].
///
/// `owner` is the id Dart filed the actor's zone under, for a panic on this
/// executor that answers no call (uncaught.rs). Like a call id it carries the
/// owning isolate in its high bits.
#[unsafe(no_mangle)]
pub extern "C" fn frustrate_actor_spawn(owner: u64) -> *const c_void {
    let isolate = (owner >> crate::post::ISOLATE_SHIFT) as u32;
    Arc::into_raw(spawn_owned(isolate, owner)) as *const c_void
}

/// Stop the host after currently queued calls drain (FIFO). Idempotent.
///
/// # Safety
/// `host` must be a counted pointer from [`frustrate_actor_spawn`] or
/// [`frustrate_actor_retain`] that the caller has not released.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn frustrate_actor_shutdown(host: *const c_void) {
    unsafe { &*(host as *const Host) }.request_stop();
}

/// Add a strong count to `host` and return it, as a token for the GC
/// backstop ([`frustrate_actor_reap`]), which releases it.
///
/// # Safety
/// As [`frustrate_actor_shutdown`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn frustrate_actor_retain(host: *const c_void) -> *const c_void {
    unsafe { Arc::increment_strong_count(host as *const Host) };
    host
}

/// Release one strong count on a host.
///
/// Takes a pointer so the symbol is directly usable as a Dart
/// `NativeFinalizer` callback: it is the finalizer of the Dart host object,
/// and the release of the backstop's token when `dispose()` detaches it. Only
/// frees memory; it neither stops the host nor runs user code.
///
/// # Safety
/// `token` must be a counted pointer from [`frustrate_actor_spawn`] or
/// [`frustrate_actor_retain`], released exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn frustrate_actor_release(token: *mut c_void) {
    drop(unsafe { Arc::from_raw(token as *const Host) });
}

/// Stop the host and release the backstop's count, for an actor handle
/// collected without `dispose()`.
///
/// **Called from a `NativeFinalizer` callback**, which runs on an arbitrary
/// thread with no current isolate and may not re-enter the VM: a flag, a
/// lock-free push and an unpark. The objects drop later, on the host thread.
///
/// # Safety
/// As [`frustrate_actor_release`], for a token from [`frustrate_actor_retain`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn frustrate_actor_reap(token: *mut c_void) {
    let host = unsafe { Arc::from_raw(token as *const Host) };
    host.request_stop();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::FramedWriter;
    use crate::test_wait::{Count, wait_until};
    use std::cell::Cell;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc::channel;

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
        let host = spawn_host(0);
        for i in 0..10u64 {
            host.submit(i, move || {
                let seq = ORDER.fetch_add(1, Ordering::SeqCst) as u64;
                assert_eq!(seq, i, "actor host must be FIFO");
                Some(FramedWriter::from_bytes(vec![0, i as u8]))
            });
        }
        host.request_stop();
        wait_until(|| SEEN.lock().unwrap().len() >= 10, "ten answers");
        assert_eq!(SEEN.lock().unwrap()[9], (9, vec![0, 9]));

        // A call to a stopped/unknown host responds with a loud panic
        // envelope, never silence.
        host.submit(11, || Some(FramedWriter::from_bytes(vec![])));
        wait_until(|| SEEN.lock().unwrap().len() >= 11, "the stopped host's answer");
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
    struct Sentinel(&'static Count);
    impl Drop for Sentinel {
        fn drop(&mut self) {
            self.0.add();
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

    /// Wait until `host`'s thread has entered the registry, which it does
    /// from its own thread after spawn returns.
    fn await_registered(host: &Arc<Host>) {
        wait_until(
            || HOSTS.with(|m| m.contains_key(&host.id)),
            "the host thread never entered the registry",
        );
    }

    /// Stop `host` the way the GC backstop does: through a token of its own,
    /// released by the reap.
    fn reap_via_backstop(host: &Arc<Host>) {
        let token = unsafe { frustrate_actor_retain(Arc::as_ptr(host) as *const c_void) };
        unsafe { frustrate_actor_reap(token as *mut c_void) };
    }

    /// Increments a counter when dropped, as an actor object would be.
    struct Counted(&'static Count);
    impl Drop for Counted {
        fn drop(&mut self) {
            self.0.add();
        }
    }

    /// A panicking user `Drop` during teardown is reported to the panic
    /// listener, the rest of the table still drops, and the host thread
    /// exits.
    ///
    /// This is the one panic in the runtime that answers no call at all, so
    /// without the report it reaches neither side of the bridge.
    #[test]
    fn a_panicking_teardown_is_reported_and_the_host_thread_still_exits() {
        let _serial = crate::post::test_lock();
        drain_completions();
        let reports = crate::panic::record_for_tests();
        static EXITS: Count = Count::new();
        static DROPS: Count = Count::new();
        struct Grenade;
        impl Drop for Grenade {
            fn drop(&mut self) {
                panic!("a user Drop exploded during teardown");
            }
        }
        let host = spawn_host(0);

        // Minted on the host thread, like a generated constructor body. The
        // grenade is newer, so it drops first: the panic must not strand what
        // drains after it.
        let (tx, rx) = channel::<()>();
        host.submit(0, move || {
            crate::handle::actor_new(Counted(&DROPS), "Counted");
            crate::objects::insert(Grenade, "Grenade");
            EXIT_MARK.with(|m| m.set(Some(Sentinel(&EXITS))));
            tx.send(()).unwrap();
            Some(FramedWriter::from_bytes(vec![]))
        });
        rx.recv().unwrap();
        // Registered: the host has served a job.
        let before = host_count();

        reap_via_backstop(&host);
        assert_eq!(host_count(), before - 1, "a reap must count at once");
        EXITS.wait_for(1, "the host thread never exited");
        crate::panic::unregister();

        assert_eq!(DROPS.get(), 1, "the drain stopped at the panic");
        assert_eq!(
            reports.lock().unwrap().as_slice(),
            ["a user Drop exploded during teardown"],
            "a panic in an actor's teardown was reported nowhere"
        );
    }

    /// A reap drops what nobody disposed exactly once, and every later stop is
    /// a no-op rather than a second drop.
    #[test]
    fn reap_drops_once_and_is_idempotent() {
        let _serial = crate::post::test_lock();
        drain_completions();
        static DROPS: Count = Count::new();
        static EXITS: Count = Count::new();
        let host = spawn_host(0);

        let (tx, rx) = channel::<()>();
        host.submit(0, move || {
            crate::handle::actor_new(Counted(&DROPS), "Counted");
            EXIT_MARK.with(|m| m.set(Some(Sentinel(&EXITS))));
            tx.send(()).unwrap();
            Some(FramedWriter::from_bytes(vec![]))
        });
        rx.recv().unwrap();
        // Registered: the host has served a job.
        let before = host_count();
        assert_eq!(DROPS.get(), 0, "minting must not drop");

        reap_via_backstop(&host);
        DROPS.wait_for(1, "the reap never dropped the object");
        EXITS.wait_for(1, "the host thread never exited");
        assert_eq!(host_count(), before - 1);

        // Idempotent from both directions: a second reap and a shutdown of an
        // already-reaped host must find nothing and drop nothing.
        reap_via_backstop(&host);
        unsafe { frustrate_actor_shutdown(Arc::as_ptr(&host) as *const c_void) };
        assert_eq!(DROPS.get(), 1, "teardown must not repeat");
        assert_eq!(host_count(), before - 1, "a host is uncounted once");
    }

    /// A dead isolate's executors are released; another isolate's are not.
    ///
    /// This is the hot-restart fix. Flutter's hot restart runs no application
    /// teardown at all, so nothing on the Dart side can reach these hosts — the
    /// finalizer that would belongs to the isolate that just died.
    #[test]
    fn a_dead_isolate_takes_its_own_executors_and_only_its_own() {
        let _serial = crate::post::test_lock();
        let mine = spawn_host(11);
        let theirs = spawn_host(22);
        // Each host enters the registry, and so the count, from its own thread.
        await_registered(&mine);
        await_registered(&theirs);
        let before = host_count();

        reap_isolate(11);
        assert_eq!(
            host_count(),
            before - 1,
            "isolate 11's executor was not released"
        );
        assert!(
            !theirs.stopping.load(Ordering::SeqCst),
            "isolate 22's executor must survive — reaping every host on any \
             isolate death would break a multi-isolate app"
        );

        // Idempotent: a second notice for the same isolate finds nothing.
        reap_isolate(11);
        assert_eq!(host_count(), before - 1);
        theirs.request_stop();
    }

    /// The wire, not just the mechanism: an isolate-death notice must reach
    /// the reap. Without this the whole path is one untested line — removing
    /// the call from `mark_gone` left every other test green.
    #[test]
    fn an_isolate_death_notice_releases_that_isolate_s_executors() {
        let _serial = crate::post::test_lock();
        let host = spawn_host(31);
        await_registered(&host);
        let before = host_count();

        crate::post::mark_gone_for_test(31);
        assert_eq!(
            host_count(),
            before - 1,
            "an isolate death did not release the executor it owned"
        );
    }

    /// A host spawned for an isolate that has already died stops itself, so
    /// an exit notice that raced the spawn cannot strand it.
    #[test]
    fn a_host_spawned_for_a_dead_isolate_stops_itself() {
        let _serial = crate::post::test_lock();
        crate::post::mark_gone_for_test(41);
        let before = host_count();
        let host = spawn_host(41);
        wait_until(
            || host.stopping.load(Ordering::SeqCst),
            "a host of a dead isolate kept running",
        );
        assert_eq!(host_count(), before, "it counted itself out");
        wait_until(
            || !HOSTS.with(|m| m.contains_key(&host.id)),
            "the stopped host never left the registry",
        );
    }

    /// An orderly dispose drops the object through the dispatched drop, and
    /// the stop behind it finds an empty table: one drop, not two. The thread
    /// exits even though the test still holds the host.
    #[test]
    fn a_disposed_object_is_not_dropped_again_by_the_stop() {
        let _serial = crate::post::test_lock();
        drain_completions();
        static DROPS: Count = Count::new();
        static EXITS: Count = Count::new();
        let host = spawn_host(0);

        let (tx, rx) = channel::<u64>();
        host.submit(0, move || {
            tx.send(crate::handle::actor_new(Counted(&DROPS), "Counted")).unwrap();
            EXIT_MARK.with(|m| m.set(Some(Sentinel(&EXITS))));
            Some(FramedWriter::from_bytes(vec![]))
        });
        let slot = rx.recv().unwrap();
        // Registered: the host has served a job.
        let before = host_count();
        // What the generated dispatched drop does.
        host.submit(1, move || {
            crate::handle::actor_drop::<Counted>(slot);
            Some(FramedWriter::from_bytes(vec![]))
        });
        host.request_stop();
        EXITS.wait_for(1, "a stop must end the thread");
        assert_eq!(DROPS.get(), 1, "dropped twice, or never");
        assert_eq!(host_count(), before - 1);
    }

    // ------------------------------------------------- deferred completions --

    /// Parks until dropped; the flag makes the drop observable. `Output` is
    /// an `Outcome` so it is spawnable as a deferred body.
    // The `Sentinel` is never read — it is held purely so that dropping this
    // future runs its `Drop`, which is the observation the test makes.
    #[cfg(not(frustrate_manual_scheduler))]
    struct ParkedBody(#[allow(dead_code)] Sentinel);
    #[cfg(not(frustrate_manual_scheduler))]
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
        static PARKED_DROPS: Count = Count::new();
        PARKED_DROPS.reset();
        let host = spawn_host(0);

        // Two deferred calls dispatched on the host thread, exactly as the
        // generated prefix glue does: job spawns the body and returns None.
        // One `submit` each, because a prefix hands off through
        // `executor::spawn_reserved` and the reservation it consumes is the
        // one `submit` made — a second body spawned under a *borrowed* id
        // would be refused (and would be a bridge bug in real glue: one call,
        // one completion).
        host.submit(101, move || {
            crate::deferred::spawn_deferred(101, ParkedBody(Sentinel(&PARKED_DROPS)));
            None
        });
        let (tx, rx) = channel::<()>();
        host.submit(102, move || {
            crate::deferred::spawn_deferred(102, async {
                crate::envelope::Outcome::Error("done".into())
            });
            tx.send(()).unwrap();
            None
        });
        rx.recv().unwrap();

        // 102 completes naturally on the pool-driven executor.
        wait_until(|| seen.lock().unwrap().contains(&102), "the fence call never answered");
        assert!(seen.lock().unwrap().contains(&102), "the ready body answers");
        assert!(
            !crate::executor::cancel(102),
            "an answered call must refuse the claim — dispose() would \
             otherwise fail a future whose real completion is delivered"
        );

        // 101 is parked; the cancel claims it, the future is dropped on a
        // drain, and no completion for it ever arrives.
        assert!(crate::executor::cancel(101), "a live deferred is claimable");
        PARKED_DROPS.wait_for(1, "the cancelled future was never dropped");
        assert!(
            !seen.lock().unwrap().contains(&101),
            "a claimed cancel owns the answer: the executor must not respond \
             for call 101"
        );
        assert!(!crate::executor::cancel(101), "a claim is single-shot");
        host.request_stop();
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
        static REAPED_DROPS: Count = Count::new();
        let host = spawn_host(0);

        let (tx, rx) = channel::<()>();
        host.submit(201, move || {
            crate::deferred::spawn_deferred(201, ParkedBody(Sentinel(&REAPED_DROPS)));
            tx.send(()).unwrap();
            None
        });
        rx.recv().unwrap();

        reap_via_backstop(&host);
        REAPED_DROPS.wait_for(1, "reap must release the deferred future (nothing else can)");
        assert!(
            !seen.lock().unwrap().contains(&201),
            "no answer is owed to a reaped handle's call"
        );
        wait_until(
            || DEFERRED.with(|m| m.get(&host.id).is_none()),
            "the host's registry entry must not outlive the reap",
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
        static QUEUED_DROPS: Count = Count::new();
        static RAN: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(false);
        RAN.store(false, Ordering::SeqCst);
        let host = spawn_host(0);

        // The fence is the FIFO, not a clock: 601 holds the host thread until
        // this test lets go, so 602 and 603 are certainly still queued below.
        let (go_tx, go_rx) = channel::<()>();
        host.submit(601, move || {
            go_rx.recv().unwrap();
            Some(FramedWriter::from_bytes(vec![0]))
        });
        host.submit(602, move || {
            RAN.store(true, Ordering::SeqCst);
            Some(FramedWriter::from_bytes(vec![0]))
        });
        let fence = Sentinel(&QUEUED_DROPS);
        host.submit(603, move || {
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
        wait_until(|| seen.lock().unwrap().contains(&603), "the fence call never answered");
        let seen = seen.lock().unwrap();
        assert!(seen.contains(&603), "the uncancelled queued call must answer");
        assert!(
            !RAN.load(Ordering::SeqCst),
            "a claimed cancel must stop the job from running at all: a \
             deferred prefix is user code the caller cancelled"
        );
        assert_eq!(
            QUEUED_DROPS.get(),
            1,
            "the claimed job must be dropped, not stranded in the queue"
        );
        assert!(
            !seen.contains(&602),
            "a claimed cancel owns the answer: the host must not respond for \
             call 602"
        );
        drop(seen);
        host.request_stop();
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

        let host = spawn_host(0);
        let (started_tx, started_rx) = channel::<()>();
        let (go_tx, go_rx) = channel::<()>();
        host.submit(701, move || {
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
        host.submit(702, || Some(FramedWriter::from_bytes(vec![0])));
        wait_until(|| seen.lock().unwrap().contains(&702), "the fence call never answered");
        let posted = seen.lock().unwrap();
        assert!(posted.contains(&702), "the uncancelled call must answer");
        assert!(!posted.contains(&701), "a claimed cancel owns the answer");
        assert_eq!(
            *RECLAIMED.lock().unwrap(),
            vec![0xACE],
            "and the reply the loop could not post freed what it had minted"
        );
        drop(posted);
        host.request_stop();
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
        static REFUSED_DROPS: Count = Count::new();
        static POLLS: AtomicUsize = AtomicUsize::new(0);
        let host = spawn_host(0);

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
        host.submit(701, move || {
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
        REFUSED_DROPS.wait_for(1, "the refused body was never dropped");
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
        wait_until(
            || DEFERRED.with(|m| m.get(&host.id).is_none()),
            "the deferred registration must be released with the refused body",
        );
        host.request_stop();
    }

    // ------------------------------------------------------------- routing --

    /// A call reaches its host without taking either registry lock: with both
    /// held by this thread, a call still runs and answers. A `submit` that
    /// took one would deadlock here, and the deadline turns that into a
    /// failure.
    #[test]
    fn a_call_takes_no_registry_lock() {
        let _serial = crate::post::test_lock();
        let seen = recording_posts();
        let host = spawn_host(0);
        await_registered(&host);
        HOSTS.with(|_| {
            DEFERRED.with(|_| {
                unsafe {
                    submit(Arc::as_ptr(&host) as *const c_void, 801, || {
                        Some(FramedWriter::from_bytes(vec![0]))
                    })
                };
                wait_until(|| seen.lock().unwrap().contains(&801), "the call never answered");
            })
        });
        host.request_stop();
    }

    /// The inbox keeps each producer's order, loses nothing under concurrent
    /// producers, and refuses every push after close, handing it back.
    ///
    /// Run it under Miri too (`cargo miri test -p frustrate --lib inbox`): the
    /// stack is raw pointers moved between threads.
    #[test]
    fn inbox_is_fifo_per_producer_and_refuses_after_close() {
        const PRODUCERS: u64 = 4;
        const EACH: u64 = if cfg!(miri) { 20 } else { 2000 };
        fn call(id: u64) -> Msg {
            Msg::Call {
                call_id: id,
                job: Box::new(|| None),
                pending: crate::idle::Pending::begin(),
            }
        }
        let inbox = Arc::new(Inbox::new());
        let producers: Vec<_> = (0..PRODUCERS)
            .map(|p| {
                let inbox = Arc::clone(&inbox);
                std::thread::spawn(move || {
                    for i in 0..EACH {
                        assert!(inbox.push(call(p << 32 | i)).is_ok());
                    }
                })
            })
            .collect();
        let mut got: Vec<u64> = Vec::new();
        while got.len() < (PRODUCERS * EACH) as usize {
            for m in inbox.take() {
                match m {
                    Msg::Call { call_id, .. } => got.push(call_id),
                    Msg::Posted { .. } | Msg::Stop => unreachable!(),
                }
            }
            std::thread::yield_now();
        }
        for p in producers {
            p.join().unwrap();
        }
        for p in 0..PRODUCERS {
            let mine: Vec<u64> = got.iter().copied().filter(|id| id >> 32 == p).collect();
            assert_eq!(mine, (0..EACH).map(|i| p << 32 | i).collect::<Vec<_>>());
        }

        assert!(inbox.push(call(7)).is_ok());
        let closed = inbox.close();
        assert_eq!(closed.len(), 1, "close returns what was still queued");
        match inbox.push(call(8)) {
            Err(Msg::Call { call_id: 8, .. }) => {}
            _ => panic!("a push after close must be handed back"),
        }
        assert!(inbox.close().is_empty(), "a second close finds nothing");
        // An open inbox frees what it still holds when dropped.
        let open = Inbox::new();
        assert!(open.push(call(9)).is_ok());
        drop(open);
    }

    /// A call queued behind the stop is answered as refused, never dropped
    /// silently, and so is a call submitted after the thread has gone.
    #[test]
    fn calls_behind_the_stop_are_refused_loudly() {
        let _serial = crate::post::test_lock();
        let seen = recording_posts();
        let host = spawn_host(0);
        let (go_tx, go_rx) = channel::<()>();
        host.submit(901, move || {
            go_rx.recv().unwrap();
            Some(FramedWriter::from_bytes(vec![0]))
        });
        host.request_stop();
        // Pushed after the Stop, so it is behind it in the queue.
        host.submit(902, || Some(FramedWriter::from_bytes(vec![0])));
        go_tx.send(()).unwrap();
        wait_until(
            || {
                let s = seen.lock().unwrap();
                s.contains(&901) && s.contains(&902)
            },
            "a call behind the stop was never answered",
        );
        wait_until(
            || !HOSTS.with(|m| m.contains_key(&host.id)),
            "the stopped host never left the registry",
        );
        // The inbox is closed now, so this one is refused on this thread.
        host.submit(903, || Some(FramedWriter::from_bytes(vec![0])));
        assert!(seen.lock().unwrap().contains(&903));
    }
}
