//! `Deferred<T>`: an actor method that hands off its completion.
//!
//! An actor processes one message at a time, so a method body that awaits
//! slow work holds the whole instance for the duration. `Deferred` is the
//! opt-out: the method runs on the actor's
//! executor only long enough to capture what the slow work needs — the
//! **prefix** — and returns a `Deferred` wrapping the rest. The generated glue
//! hands the wrapped future to the cooperative executor under the call's own
//! id; the caller's `await` completes when it resolves. The instance is
//! released the moment the prefix returns.
//!
//! # The capture/await split is the borrow checker's
//!
//! [`Deferred::new`] requires `Send + 'static`, so the future **cannot borrow
//! `self`** — clone what you need before the `async move` block. That bound is
//! the entire soundness story: everything that touches `&self`/`&mut self`
//! runs serialized on the executor; everything after runs detached, touching
//! only values it owns or explicitly shares (an `Arc`, a channel, a handle).
//!
//! # Runtime context: only if you registered one
//!
//! The future runs on frustrate's cooperative executor, which provides no
//! tokio (or other runtime) context **by default**: a tokio timer or IO type
//! constructed inside it panics with "there is no reactor running". Two ways
//! out, and the second is native-only.
//!
//! **Without a registration**, spawn the work **on the runtime** and await the
//! result through a plain future — a `JoinHandle`, a oneshot receiver. This is
//! portable (it is what a web actor does too, where there is no runtime to
//! register):
//!
//! ```ignore
//! pub fn connect(&self, ticket: String) -> Deferred<Result<PeerInfo, String>> {
//!     let handle = self.rt_handle.clone(); // tokio::runtime::Handle
//!     Deferred::new(async move {
//!         handle.spawn(dial(ticket)).await.map_err(|e| e.to_string())?
//!     })
//! }
//! ```
//!
//! **With [`frustrate::runtime::register`](crate::runtime::register)** (native
//! only), deferred bodies drain on the same global executor as every other
//! task, so they get the registered context and can `.await` the leaf
//! directly — no `spawn`, no `JoinHandle`, and the work is cancelled by the
//! same `dispose()`/reap path that already cancels the `Deferred`:
//!
//! ```ignore
//! Deferred::new(async move { dial(ticket).await })
//! ```
//!
//! **Construct the leaf inside the `async` block, not as an argument.** The
//! prefix — everything up to and including the expression handed to
//! `Deferred::new` — runs on the **actor's own thread**, not on the executor,
//! so it gets no registered context. tokio builds a leaf eagerly, so
//! `Deferred::new(tokio::time::sleep(d))` panics "there is no reactor running"
//! even in a process that registered one, while
//! `Deferred::new(async move { tokio::time::sleep(d).await })` is fine: the
//! construction moves inside the future and happens on the first poll, under
//! the context. The example above is the safe shape for this reason and not
//! only for the borrow checker's.

use crate::envelope::Outcome;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

/// A completion an actor method handed off. Only meaningful as the outermost
/// return type of an actor method (the checker rejects every other position);
/// the Dart signature is unchanged — the caller still just `await`s.
pub struct Deferred<T> {
    fut: Pin<Box<dyn Future<Output = T> + Send + 'static>>,
}

impl<T> Deferred<T> {
    /// Wrap the slow part of an actor method.
    ///
    /// `Send + 'static` means the future cannot borrow `self` — clone what
    /// you need first. See the module docs for the tokio-context rule.
    pub fn new(fut: impl Future<Output = T> + Send + 'static) -> Self {
        Deferred { fut: Box::pin(fut) }
    }
}

impl<T> Future for Deferred<T> {
    type Output = T;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        self.fut.as_mut().poll(cx)
    }
}

/// Spawn a deferred actor completion under `call_id`. Called by the generated
/// glue, from the deferred method's prefix — i.e. **on the actor's executor**.
///
/// Native: records the call in the owning host's deferred registry
/// ([`crate::actor`]) so `dispose()` can cancel it, and unregisters when the
/// future is dropped — completion, cancellation, and a panicking poll all end
/// with the future dropped, so a drop guard covers every exit.
///
/// The hand-off itself goes through [`crate::executor::spawn_reserved`], not
/// `spawn`: the call has been reserved on the executor since `actor::submit`
/// enqueued it, and swapping that reservation for this task under one registry
/// lock is what leaves no instant in which a cancel would claim neither. A
/// cancel that landed while this prefix ran therefore refuses the spawn, and
/// `body` is dropped here, un-polled — the guard below rides it out.
///
/// Web: the actor's executor IS its worker-hosted instance, and cancellation
/// is the worker's `terminate()` — there is no registry to keep, so this is
/// exactly [`crate::executor::spawn`].
#[cfg(not(target_family = "wasm"))]
pub fn spawn_deferred(call_id: u64, body: impl Future<Output = Outcome> + Send + 'static) {
    let host = crate::actor::current_host();
    crate::actor::register_deferred(host, call_id);
    /// Unregisters when the wrapped future is dropped — on completion, on
    /// cancellation, and after a panicking poll alike.
    struct Unregister {
        host: u64,
        call_id: u64,
    }
    impl Drop for Unregister {
        fn drop(&mut self) {
            crate::actor::unregister_deferred(self.host, self.call_id);
        }
    }
    let guard = Unregister { host, call_id };
    crate::executor::spawn_reserved(call_id, async move {
        let _guard = guard;
        body.await
    });
}

#[cfg(target_family = "wasm")]
pub fn spawn_deferred(call_id: u64, body: impl Future<Output = Outcome> + Send + 'static) {
    crate::executor::spawn(call_id, body);
}
