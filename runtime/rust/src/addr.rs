//! `Addr<A>`: an actor's address for messages to itself.
//!
//! An actor's executor runs one message at a time. A method that must do long
//! work can do a slice of it and [`post`](Addr::post) the rest as a closure:
//! the closure is a new message, so calls that arrive meanwhile run between
//! slices (see Ordering for how firmly). A future running *off* the
//! executor (a `Deferred` completion, a `runtime::spawn` task) uses
//! [`call`](Addr::call) to apply a result to the actor and get an answer back.
//!
//! An actor receives its `Addr` as a context parameter: a constructor or
//! method of the actor declares a parameter of type `Addr<Self>`, and the
//! executor supplies it. It is not part of the Dart signature.
//!
//! ```ignore
//! #[bridge] impl Indexer {
//!     pub fn new(me: Addr<Indexer>) -> Indexer { Indexer { me, done: 0 } }
//!
//!     pub fn reindex(&mut self) -> Deferred<Result<u64, Gone>> {
//!         let (tx, rx) = ...; // any oneshot
//!         self.me.post(move |ix| ix.slice(tx));
//!         Deferred::new(async move { rx.await.map_err(|_| Gone) })
//!     }
//! }
//! ```
//!
//! # Ordering
//!
//! A posted closure is a new message: it runs after the message that posted
//! it has finished, never inside it, and posts run in post order.
//!
//! - **Native:** the closure is pushed on the host's own queue (actor.rs),
//!   the same queue that holds the calls from Dart. It runs after every
//!   message already there and before every later one.
//! - **Web:** the closure waits in this instance's own queue, and the worker
//!   posts itself a token on a channel of its own; each token, a task of its
//!   own on the worker's event loop, runs the oldest waiting closure. The
//!   browser picks between that channel and the one the calls arrive on, so a
//!   call that has reached the worker runs between two slices in practice
//!   (tests/dart_integration/test/addr_test.dart measures it), but its order
//!   against a given post is not fixed.
//!
//! # Teardown
//!
//! A posted closure runs on the actor's root object. If the root is gone when
//! the closure's turn comes (disposed, consumed, or never built because its
//! constructor panicked) the closure is dropped without running. Natively a
//! post to a stopped executor, and every post still queued behind the stop, is
//! dropped too; on web the worker's termination takes the queue with it. A
//! dropped closure drops what it captured, so a channel it held tells its
//! other end, and [`call`](Addr::call) resolves to `Err(Gone)`.
//!
//! # Panics
//!
//! A posted closure that panics answers no call, so in Dart it is an uncaught
//! error, a `BridgePanicException` in the zone the actor was built in. It also
//! reaches the panic listener. A [`call`](Addr::call) on that closure resolves
//! to `Err(Gone)`. Then the actor carries on with its next message: natively
//! the panic unwinds, and on web the trap leaves the instance running, as a
//! trap in any of its calls does.
//!
//! Awaiting [`call`](Addr::call) on the executor's own thread, from anywhere
//! but frustrate's cooperative executor (a `block_on` in a method body, say),
//! panics at once: the answer needs the executor, which is busy running the
//! waiter.

use crate::bounded::{Lock, Outside};
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

/// A message to the executor, erased: it finds the root and runs on it, or
/// is dropped without running if the root is gone.
pub(crate) type Posted = Box<dyn FnOnce() + Send>;

/// The address of an actor, for messages to itself. See the module docs.
///
/// `Clone + Send + Sync` whatever `A` is: an `Addr` carries no `A`, only the
/// way to its executor, and the closures it posts are `Send`.
pub struct Addr<A: 'static> {
    #[cfg(not(target_family = "wasm"))]
    host: Arc<crate::actor::Host>,
    _root: PhantomData<fn() -> A>,
}

impl<A: 'static> Clone for Addr<A> {
    fn clone(&self) -> Self {
        Addr {
            #[cfg(not(target_family = "wasm"))]
            host: Arc::clone(&self.host),
            _root: PhantomData,
        }
    }
}

impl<A: 'static> std::fmt::Debug for Addr<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Addr<{}>", std::any::type_name::<A>())
    }
}

/// The actor an [`Addr::call`] was sent to is gone: it was disposed or
/// consumed before the closure's turn, or its executor stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gone;

impl std::fmt::Display for Gone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the actor is gone")
    }
}

impl std::error::Error for Gone {}

impl<A: 'static> Addr<A> {
    /// The address of the executor this runs on. For generated glue, which
    /// passes it as a context parameter; `A` is the actor type the glue names.
    ///
    /// Natively, panics off an actor's executor. On web it checks nothing: the
    /// address names the instance it is made in, which for generated glue is
    /// the actor's own.
    #[doc(hidden)]
    pub fn __here() -> Self {
        Addr {
            #[cfg(not(target_family = "wasm"))]
            host: crate::actor::current(),
            _root: PhantomData,
        }
    }

    /// Queue `f` as a message to this actor. It runs with `&mut` the actor
    /// after the message running now, if any, and every earlier post (see the
    /// module docs' Ordering), or is dropped without running if the actor is
    /// gone by then. Fire-and-forget.
    pub fn post(&self, f: impl FnOnce(&mut A) + Send + 'static) {
        self.post_waited(f, None);
    }

    /// [`post`](Self::post), naming the [`Call`] waiting on the closure, if
    /// any, so a web trap in it can still answer that call.
    fn post_waited(&self, f: impl FnOnce(&mut A) + Send + 'static, waiter: Option<Waiter>) {
        let job: Posted = Box::new(move || match crate::objects::root::<A>() {
            // Safety: this closure runs as its own message on the executor
            // that owns the root, so no other borrow of it is live (the
            // argument `handle::actor_mut` rests on), and the borrow ends
            // when `f` returns.
            Some(root) => f(unsafe { &mut *root }),
            None => drop(f),
        });
        self.send(job, waiter);
    }

    /// Natively a panic unwinds through the closure and drops its [`Answer`],
    /// so the waiter needs no record.
    #[cfg(not(target_family = "wasm"))]
    fn send(&self, job: Posted, _waiter: Option<Waiter>) {
        // Refused by a stopped host: dropped here, on the posting thread,
        // which is why a posted closure must be `Send`.
        if let Err(job) = self.host.post_posted(job) {
            drop(job);
        }
    }

    #[cfg(target_family = "wasm")]
    fn send(&self, job: Posted, waiter: Option<Waiter>) {
        web::post(job, waiter);
    }

    /// Run `f` on this actor as a message, and resolve to its result.
    ///
    /// For code off the executor that must apply something to the actor: a
    /// `Deferred` future that finished slow I/O, or a `runtime::spawn` task.
    /// Resolves to `Err(Gone)` if the closure is dropped without running.
    ///
    /// Polling the returned future on this actor's own executor, other than
    /// from frustrate's cooperative executor, panics: it could only complete
    /// once the poller let the executor go.
    pub fn call<R: Send + 'static>(&self, f: impl FnOnce(&mut A) -> R + Send + 'static) -> Call<R> {
        let slot = Arc::new(Lock::new(Slot {
            value: None,
            gone: false,
            waker: None,
        }));
        let answer = Answer {
            slot: Some(Arc::clone(&slot)),
        };
        #[cfg(target_family = "wasm")]
        let waiter: Option<Waiter> = Some(Arc::clone(&slot) as Waiter);
        #[cfg(not(target_family = "wasm"))]
        let waiter: Option<Waiter> = None;
        self.post_waited(move |root| answer.send(f(root)), waiter);
        Call {
            slot,
            #[cfg(not(target_family = "wasm"))]
            host: self.host.id(),
        }
    }
}

struct Slot<R> {
    value: Option<R>,
    /// The answer will never come: the closure was dropped unrun, or trapped.
    gone: bool,
    waker: Option<Waker>,
}

/// A [`Call`]'s slot, without its answer type: what can tell the waiter the
/// answer will never come.
trait Abandon {
    /// Resolve the call to `Err(Gone)` unless it has its answer, and wake it.
    fn abandon(&self);
}

impl<R> Abandon for Lock<Slot<R>> {
    fn abandon(&self) {
        let waker = self.with(|s| {
            s.gone = true;
            s.waker.take().map(Outside::new)
        });
        if let Some(w) = waker {
            w.into_inner().wake();
        }
    }
}

/// The waiter on a posted closure, kept beside it on web only.
#[cfg_attr(not(target_family = "wasm"), allow(dead_code))]
type Waiter = Arc<dyn Abandon>;

/// The sending half of a [`Call`], moved into the posted closure. Dropping it
/// unsent (the closure was dropped without running) resolves the call to
/// `Err(Gone)`.
struct Answer<R> {
    /// Taken by [`send`](Answer::send), so a drop after sending finds nothing.
    slot: Option<Arc<Lock<Slot<R>>>>,
}

impl<R> Answer<R> {
    fn send(mut self, value: R) {
        let slot = self.slot.take().expect("an Answer sends once");
        let waker = slot.with(|s| {
            s.value = Some(value);
            s.waker.take().map(Outside::new)
        });
        if let Some(w) = waker {
            w.into_inner().wake();
        }
    }
}

impl<R> Drop for Answer<R> {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.take() {
            slot.abandon();
        }
    }
}

/// The answer to an [`Addr::call`].
#[must_use = "a Call does nothing to the actor unless awaited; the closure runs either way"]
pub struct Call<R> {
    slot: Arc<Lock<Slot<R>>>,
    #[cfg(not(target_family = "wasm"))]
    host: u64,
}

impl<R> Call<R> {
    /// Whether this poll is on the executor that must run the closure, by
    /// something other than the cooperative executor. Natively that is the
    /// host's own thread, which only ever runs messages. On web the instance
    /// is the executor, and between messages it runs only the cooperative
    /// executor's drains.
    fn polled_by_its_own_executor(&self) -> bool {
        #[cfg(not(target_family = "wasm"))]
        let on_executor = crate::actor::current_host_id() == Some(self.host);
        #[cfg(target_family = "wasm")]
        let on_executor = true;
        on_executor && !crate::executor::draining()
    }
}

impl<R> Future for Call<R> {
    type Output = Result<R, Gone>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.polled_by_its_own_executor() {
            panic!(
                "frustrate: Addr::call awaited on its own actor's executor, which can \
                 never run the closure while it is waiting here; post the work with \
                 Addr::post, or await the call from a Deferred future"
            );
        }
        // Cloned before the section, and the waker it replaces dropped after.
        let waker = cx.waker().clone();
        let (poll, displaced) = self.slot.with(|s| {
            if let Some(v) = s.value.take() {
                (Poll::Ready(Ok(v)), Some(Outside::new(waker)))
            } else if s.gone {
                (Poll::Ready(Err(Gone)), Some(Outside::new(waker)))
            } else {
                (Poll::Pending, s.waker.replace(waker).map(Outside::new))
            }
        });
        drop(displaced.map(Outside::into_inner));
        poll
    }
}

/// Run one posted message. Natively a panic is caught, told to the panic
/// listener and to `owner`'s Dart zone, and the host goes on to its next
/// message.
#[cfg(not(target_family = "wasm"))]
pub(crate) fn run(job: Posted, owner: u64) {
    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)) {
        crate::uncaught::report(owner, crate::envelope::panic_envelope(payload));
    }
}

/// Drop a posted message unrun. Its captures' `Drop`s are user code, and a
/// panic there is reported rather than allowed to end the host thread.
#[cfg(not(target_family = "wasm"))]
pub(crate) fn discard(job: Posted) {
    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(job))) {
        crate::panic::observed(crate::panic::message(&*payload));
    }
}

/// The web half: the instance's own queue of closures, and the token that
/// asks the worker to run the oldest one as a task of its own.
#[cfg(target_family = "wasm")]
mod web {
    use super::{Posted, Waiter};
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;

    thread_local! {
        /// Closures waiting for their token, oldest first, each with the
        /// [`Call`](super::Call) waiting on it if any. One instance is one
        /// executor, so this is that executor's queue.
        static WAITING: RefCell<VecDeque<(Posted, Option<Waiter>)>> =
            const { RefCell::new(VecDeque::new()) };

        /// The waiter on the closure running now. Set and cleared by
        /// [`frustrate_run_posted`] around the closure, so after a trap in it
        /// this still names that closure's waiter.
        static RUNNING: Cell<Option<Waiter>> = const { Cell::new(None) };
    }

    #[link(wasm_import_module = "frustrate")]
    extern "C" {
        /// Schedule one task on this worker that calls
        /// [`frustrate_run_posted`] (the actor worker's `posted` channel in
        /// frustrate.js).
        fn self_post();
    }

    pub(super) fn post(job: Posted, waiter: Option<Waiter>) {
        WAITING.with(|q| q.borrow_mut().push_back((job, waiter)));
        unsafe { self_post() };
    }

    /// Run the oldest waiting closure: one per token. Called by the actor
    /// worker for each token on its `posted` channel.
    #[cfg(not(frustrate_block_check))]
    #[unsafe(no_mangle)]
    pub extern "C" fn frustrate_run_posted() {
        let job = WAITING.with(|q| q.borrow_mut().pop_front());
        match job {
            Some((job, waiter)) => {
                RUNNING.set(waiter);
                job();
                RUNNING.set(None);
            }
            None => panic!("frustrate: a self token arrived with no closure waiting — bridge bug"),
        }
    }

    /// The closure [`frustrate_run_posted`] was running trapped: resolve its
    /// [`Call`](super::Call), if it has one, to `Err(Gone)`, as unwinding
    /// would have natively. Called by the actor worker after it catches the
    /// trap.
    ///
    /// It touches [`RUNNING`], which only `frustrate_run_posted` writes and
    /// only outside the closure, and that call's slot, whose lock sections
    /// run no user code and so cannot have trapped while held. It does not
    /// touch the queue. Waking the waiter is ordinary executor work, the same
    /// any later call on the instance does.
    #[cfg(not(frustrate_block_check))]
    #[unsafe(no_mangle)]
    pub extern "C" fn frustrate_posted_trapped() {
        if let Some(waiter) = RUNNING.take() {
            waiter.abandon();
        }
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests;
