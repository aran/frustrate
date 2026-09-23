//! Rust → Dart streams: the producer half.
//!
//! A `StreamSink<T>` is a channel endpoint decoded from a bridge request
//! (the stream id is allocated by Dart and travels inline, wherever the sink
//! sits in the arguments). Items post through the same completion channel as
//! call responses — `post::respond(stream_id, envelope)` with the stream
//! statuses — so every transport that can complete a call can carry a
//! stream, including actor executors.
//!
//! Lifecycle: the stream ends when the last sink clone drops (or on
//! `close`/`error`). Cancellation is a cooperative flag set by the
//! `frustrate_stream_cancel` export: `add` starts returning `false` and all
//! further posts (including the end event) are suppressed — the Dart side
//! already tore its end down.
//!
//! Locking: the cancel registry uses [`crate::spin::SpinLock`], not
//! `std::sync::Mutex` — on threaded wasm the browser main thread may call the
//! cancel export while a worker holds the lock, and a parking lock would trap
//! there ("Atomics.wait cannot be called in this context"). Critical sections
//! are single map operations, so spinning is bounded and tiny. See that
//! module for the full argument and for why the release is a `Drop` guard.

use crate::codec::{ByteWriter, FramedWriter};
use crate::envelope::{
    string_envelope, STATUS_ERROR, STATUS_LEAKED, STATUS_STREAM_END, STATUS_STREAM_ITEM,
};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use crate::spin::SpinLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::task::{Context, Poll, Waker};

/// Live sinks by stream id, for the cancel export. Weak: the sink's own
/// clones keep it alive; a cancel for an already-ended stream is a no-op.
static REGISTRY: SpinLock<Option<HashMap<u64, Weak<Inner>>>> = SpinLock::new(None);

fn with_registry<R>(f: impl FnOnce(&mut HashMap<u64, Weak<Inner>>) -> R) -> R {
    REGISTRY.with(|m| f(m.get_or_insert_with(HashMap::new)))
}

std::thread_local! {
    /// The type name of the handle currently being finalized on this thread,
    /// or `None` outside a finalizer. Set by [`finalize_scope`] and read by
    /// `Inner::drop` to decide which terminal a dying channel posts.
    static FINALIZING: std::cell::Cell<Option<&'static str>> =
        const { std::cell::Cell::new(None) };
}

/// Run `f` — the drop of a `type_name` handle — as a *finalization*, so any
/// channel it still holds terminates as leaked rather than as closed.
///
/// Used only by the generated `frustrate_finalize_*` exports, which a Dart
/// `NativeFinalizer` reaches exactly when the handle was collected without
/// `dispose()` (`dispose()` detaches the finalizer before dropping, so it
/// cannot). Being here is therefore proof of a missing `dispose()`, and
/// `type_name` is the only identity available to say whose.
///
/// Posting from here is legal and lands: measured in
/// `//tests/dart_integration:gc_finalizer_test`. At real isolate shutdown the
/// post is a defined no-op (post.rs returns false), which is what makes this
/// safe — it was undefined behavior when delivery was a `NativeCallable`, and
/// that hazard is what the predecessor of this function existed to dodge.
pub fn finalize_scope<R>(type_name: &'static str, f: impl FnOnce() -> R) -> R {
    // Restore on unwind too: a panicking finalizer must not leave every later
    // drop on this thread reporting itself as leaked.
    struct Reset(Option<&'static str>);
    impl Drop for Reset {
        fn drop(&mut self) {
            FINALIZING.with(|s| s.set(self.0));
        }
    }
    let _reset = Reset(FINALIZING.with(|s| s.replace(Some(type_name))));
    f()
}

/// One Rust-held end of a Dart-registered channel (a stream's sink or a
/// callback's handle — callback.rs reuses this whole lifecycle): the id,
/// the cooperative cancel flag, and the once-only terminal accounting.
/// Dropping the last Arc retires the Dart registration via the end event.
pub(crate) struct Inner {
    pub(crate) id: u64,
    cancelled: AtomicBool,
    /// Set by `close`/`error` (and checked by `Drop`) so the terminal event
    /// posts exactly once even with clones alive.
    closed: AtomicBool,
    /// Backpressure state, driven from Dart by `frustrate_stream_pause` /
    /// `_resume` (wired to `StreamController.onPause`/`onResume`). Level, not
    /// edge: a producer awaiting [`StreamSink::send`] parks while this is set
    /// and wakes on resume. `add` ignores it — `add` is the fire-and-forget,
    /// no-backpressure primitive.
    paused: AtomicBool,
    /// Wakers of tasks parked in `send()`/`poll_ready` on this stream, one
    /// entry per parked task (deduped by `Waker::will_wake`; each `Ready`
    /// future deregisters its own on drop). A spin lock — not a parking lock —
    /// for the same reason as the cancel registry: `frustrate_stream_resume`
    /// may be entered from the browser main thread — the page instance is
    /// dispatched to from there — and `Atomics.wait` traps on that thread under
    /// threaded wasm. *May* is the whole requirement: one reachable route is
    /// enough to rule a parking lock out. Critical sections are a small `Vec`
    /// op (push/take/retain, plus a
    /// waker clone in `register`), bounded by the fan-in count; the actual
    /// wakes run *outside* the lock.
    wakers: SpinLock<Vec<Waker>>,
    /// Set only by [`crate::testing::stream`]. `None` in every stream a
    /// generated binding ever builds, so production reads one already-hot
    /// pointer beside the atomics `live()` just loaded, and takes a branch
    /// that is never taken.
    ///
    /// It lives on `Inner` rather than on `StreamSink<T>` because the sink is
    /// not the whole surface: drop-retire, `close_now` and `error` post from
    /// here, below the typed layer, and a test-built sink reaching those would
    /// hit `post::deliver`'s unregistered-id panic on the ordinary end of every
    /// test. Diverting here covers all of them at once — and keeps
    /// `StreamSink<T>` the same size, and `Send + Sync` for every `T`, which a
    /// typed buffer on the sink would have made conditional.
    pub(crate) capture: Option<Arc<crate::testing::CaptureState>>,
}

impl Inner {
    pub(crate) fn live(&self) -> bool {
        !self.cancelled.load(Ordering::Acquire) && !self.closed.load(Ordering::Acquire)
    }

    /// Mark closed; true if this call did the closing (terminal not yet
    /// sent, stream not cancelled).
    fn take_terminal(&self) -> bool {
        !self.closed.swap(true, Ordering::AcqRel) && !self.cancelled.load(Ordering::Acquire)
    }

    /// Post the retire event now, if this call took the terminal. `Drop`
    /// runs the same check, so an explicit `close()` followed by the last
    /// clone dropping posts exactly one terminal — never two, which on the
    /// Dart side would be a double-close `StateError`.
    pub(crate) fn close_now(&self) {
        if self.take_terminal() {
            match &self.capture {
                Some(c) => c.set_terminal(crate::testing::Terminal::Closed),
                None => crate::post::respond(self.id, FramedWriter::status(STATUS_STREAM_END)),
            }
        }
        // A terminal makes `live()` false, so any parked `send()` is now ready
        // (to observe the close and return false). Wake even if we did not take
        // the terminal — an idempotent no-op then (wakers already drained).
        self.wake_waiters();
    }

    /// The Dart subscription cancelled. Shared by `frustrate_stream_cancel`
    /// and the test probe so the two cannot drift.
    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.wake_waiters();
    }

    /// The consumer paused (`StreamController.onPause`). Store only; nothing
    /// to wake on a pause.
    pub(crate) fn pause(&self) {
        self.paused.store(true, Ordering::Release);
    }

    /// The consumer resumed. The store precedes the wake so a re-polling
    /// parked task observes `paused == false`.
    pub(crate) fn resume(&self) {
        self.paused.store(false, Ordering::Release);
        self.wake_waiters();
    }

    /// Whether a `send()` may proceed: the consumer is not backpressuring, or
    /// the stream is no longer live (so the parked send should wake to observe
    /// the cancel/close and return false rather than hang).
    fn ready_now(&self) -> bool {
        !self.paused.load(Ordering::Acquire) || !self.live()
    }

    /// Wake — *outside* the lock — every task parked in `send()`. Draining
    /// under the lock then waking after it is load-bearing: `Waker::wake` runs
    /// the executor's schedule (its own lock / the drain import), so waking
    /// inside this spin lock would nest `Inner`-lock → executor-lock and blow
    /// the tiny-critical-section budget that makes the spin lock legal.
    fn wake_waiters(&self) {
        let woken = self.wakers.with(std::mem::take);
        for w in woken {
            w.wake();
        }
    }

    /// Register `w` to be woken on the next resume/terminal. Deduped by
    /// `will_wake` so a re-polled task holds at most one entry.
    fn register(&self, w: &Waker) {
        self.wakers.with(|v| {
            if v.iter().any(|e| e.will_wake(w)) {
                return;
            }
            v.push(w.clone());
        });
    }

    /// Drop `w`'s registration — called from `Ready::drop` so a `send()` future
    /// that is dropped while parked (its task cancelled) leaves no stale waker.
    ///
    /// Removes *every* `will_wake`-equal entry, so if one task held two waiters
    /// on this `Inner` (a `send()` and a raw `Sink::poll_ready` park) that
    /// deduped to a single shared entry, dropping the first deregisters the
    /// other's entry too. Benign for the standard combinators (`join!`
    /// re-polls every child each wake; `FuturesUnordered` gives each child a
    /// distinct waker, so `will_wake` is false and there is no sharing); a
    /// slab-keyed registration would close it if raw-`Sink` fan-in on one task
    /// ever becomes real.
    fn deregister(&self, w: &Waker) {
        self.wakers.with(|v| v.retain(|e| !e.will_wake(w)));
    }

    /// A future that resolves once [`ready_now`](Self::ready_now) holds.
    fn ready(&self) -> Ready<'_> {
        Ready {
            inner: self,
            registered: None,
        }
    }
}

/// Readiness gate for [`StreamSink::send`]: `Pending` while the consumer is
/// backpressuring, `Ready` on resume or terminal. Register-then-recheck closes
/// the lost-wakeup window; deregister-on-drop keeps the waker set from growing
/// when a parked send is cancelled.
struct Ready<'a> {
    inner: &'a Inner,
    registered: Option<Waker>,
}

impl Future for Ready<'_> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let inner = self.inner;
        if inner.ready_now() {
            // Resume/terminal already drained our waker; nothing to deregister.
            self.registered = None;
            return Poll::Ready(());
        }
        // Register *before* the second check: a resume between the two checks
        // then either drains the waker we just pushed (and we re-check ready),
        // or has already flipped the flag that the re-check observes. Either
        // way the park cannot outlive the resume.
        inner.register(cx.waker());
        self.registered = Some(cx.waker().clone());
        if inner.ready_now() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

impl Drop for Ready<'_> {
    fn drop(&mut self) {
        if let Some(w) = self.registered.take() {
            self.inner.deregister(&w);
        }
    }
}

/// Create and register a channel end for a Dart-allocated id.
pub(crate) fn end(id: u64) -> Arc<Inner> {
    let inner = Arc::new(Inner {
        id,
        cancelled: AtomicBool::new(false),
        closed: AtomicBool::new(false),
        paused: AtomicBool::new(false),
        wakers: SpinLock::new(Vec::new()),
        capture: None,
    });
    with_registry(|m| m.insert(id, Arc::downgrade(&inner)));
    inner
}

/// A channel end that records instead of posting ([`crate::testing`]).
///
/// Deliberately **not** registered in the cancel registry: the probe drives
/// cancel/pause/resume directly, so nothing global is shared and consumer
/// tests parallelize. `Inner::drop`'s `registry.remove` is then a no-op miss,
/// which it already tolerates.
pub(crate) fn captured_end(id: u64, capture: Arc<crate::testing::CaptureState>) -> Arc<Inner> {
    Arc::new(Inner {
        id,
        cancelled: AtomicBool::new(false),
        closed: AtomicBool::new(false),
        paused: AtomicBool::new(false),
        wakers: SpinLock::new(Vec::new()),
        capture: Some(capture),
    })
}

impl Drop for Inner {
    fn drop(&mut self) {
        with_registry(|m| m.remove(&self.id));
        // Drop-retire runs even while unwinding: a producer panic drops the
        // sink on the way out, and posting the end here is what closes the
        // Dart stream cleanly (`onDone`) rather than stranding a consumer that
        // waits forever. The panic itself is attributed to the *call*, not the
        // stream. A clone handed off before the panic (stored in `self`, moved
        // to another thread) is a legitimate survivor: refcount keeps `Inner`
        // alive, so this drop does not fire and the stream correctly stays
        // open. Web (`panic=abort`) runs no destructor at all, so it cannot
        // reach this path; a producer panic there leaves the stream open by
        // design (the panic is still reported on the call). A future `panic=unwind` web build would reach this drop and
        // inherit the native behavior with no change here.
        // No wake here: a *live* parked waiter borrows the sink (via `Ready`)
        // or owns it (a `Sink` combinator), so it holds an `Arc` clone and
        // `Inner` cannot be dropping while one exists. A stale waker may remain
        // — a `Sink::poll_ready` that parked and was then dropped does not
        // deregister (only `send()`'s `Ready` wrapper does) — but it is dropped
        // here with the `Vec`, unwoken, which is correct and harmless.
        // Both terminals go through `take_terminal`, so a channel the consumer
        // already cancelled stays quiet even when its holder is later collected:
        // that handle leaked memory, not a channel, and reclaiming memory is
        // what the collector is for. Only a still-live channel reports.
        if !self.take_terminal() {
            return;
        }
        let finalizing = FINALIZING.with(|s| s.get());
        if let Some(c) = &self.capture {
            c.set_terminal(match finalizing {
                Some(type_name) => crate::testing::Terminal::Leaked(type_name),
                None => crate::testing::Terminal::Closed,
            });
            return;
        }
        match finalizing {
            // Dropping inside a `frustrate_finalize_*` export: the Dart handle
            // was collected without `dispose()`, so this channel is not closing,
            // it is being abandoned. Say which, and by what.
            Some(type_name) => {
                crate::post::respond(self.id, string_envelope(STATUS_LEAKED, type_name))
            }
            None => crate::post::respond(self.id, FramedWriter::status(STATUS_STREAM_END)),
        }
    }
}

/// The producer endpoint of one Rust → Dart stream.
///
/// Storable (`'static`), cloneable (fan-in), and callable from any thread
/// that may run bridge code. Adds ordered by happens-before arrive in that
/// order; concurrent adds interleave at item granularity, never mid-item.
pub struct StreamSink<T> {
    inner: Arc<Inner>,
    /// **By value, not by reference.** An item that reaches an opaque handle
    /// is *minted* by its encoder (`handle::confined_new` and its siblings
    /// take the object), so the encoder has to own what it writes. A
    /// handle-free item is encoded from the owned local exactly as it was
    /// from a borrow of it, so the two readings stay one wire.
    encode: fn(T, &mut ByteWriter),
}

impl<T> Clone for StreamSink<T> {
    fn clone(&self) -> Self {
        StreamSink {
            inner: Arc::clone(&self.inner),
            encode: self.encode,
        }
    }
}

/// Method selectors for a Dart-object handle.
///
/// A void method call posts `[STATUS_STREAM_ITEM, selector, payload…]`; the
/// Dart router looks the selector up in the object's dispatch table. Selector
/// 0 is the primary item method of every mirror (`Sink.add`, and the
/// invocation of a `DartCallback`), so the single-method endpoints are simply
/// the "selector 0 only" case.
///
/// The terminal is deliberately NOT a selector: `STATUS_STREAM_END` retires
/// the registration for *every* mirror, including ones that declare no
/// `close` method, so lifecycle stays uniform across method sets.
pub mod selector {
    /// The mirror's primary method: `Sink.add(T)` on a sink-shaped mirror,
    /// or the single invocation of a callback/function mirror.
    pub const ADD: u8 = 0;
    /// Alias for [`ADD`] at a returning-method call site, where "add" would
    /// misname the same slot.
    pub const CALL: u8 = ADD;
    /// `EventSink.addError(Object)` — non-terminal, per Dart's contract that
    /// a stream may carry errors and continue. Distinct from
    /// [`super::StreamSink::error`], which IS terminal.
    pub const ADD_ERROR: u8 = 1;
}

/// A writer framed for one selector-tagged void method call on a Dart-object
/// handle. The single place that names [`STATUS_STREAM_ITEM`] for this shape,
/// so no caller can pair a selector with the wrong status; encode the payload
/// into it and hand it to [`post_method`].
///
/// An item may be encoded and then not delivered — the consumer cancelled, or
/// its isolate is gone — and an item that reaches an opaque has *minted* those
/// objects by then. The ledger every writer keeps is what frees them
/// ([`crate::codec::Minted`]).
pub(crate) fn item_writer(sel: u8) -> FramedWriter {
    FramedWriter::event(STATUS_STREAM_ITEM, sel)
}

/// Post one selector-tagged void method call on a Dart-object handle, framed
/// by [`item_writer`]. Returns false — without sending — once the channel is
/// cancelled or closed, which is what makes a stale handle inert rather than a
/// panic.
///
/// **Both refusals reclaim.** The payload is already encoded, so any handle it
/// carries is already registered; nothing downstream will decode it, and the
/// Dart wrapper that would dispose it is never built. `Minted::reclaim` frees
/// exactly what this encode minted — a no-op for the handle-free items that
/// are the overwhelming majority. Only the *local* refusal is spelled here;
/// the one a dead consumer makes belongs to `post::deliver`, which is where it
/// is discovered.
pub(crate) fn post_method(inner: &Inner, w: FramedWriter) -> bool {
    if !inner.live() {
        w.into_parts().1.reclaim();
        return false;
    }
    if crate::post::deliver(inner.id, w) {
        return true;
    }
    // The consumer isolate is gone (it exited, was killed, or the app hot
    // restarted) and will never take another event. Retire this end as if Dart
    // had cancelled: `add` returns false from here on, so a producer that
    // checks — the documented contract — stops on its own. Nothing else can
    // deliver the news; the isolate that would have told us is the one that
    // died. Always `true` on web, where the consumer is the page (post.rs).
    inner.cancelled.store(true, Ordering::Release);
    inner.wake_waiters();
    false
}

impl<T: Send + 'static> StreamSink<T> {
    /// Post one item. Returns `false` — without sending — once the Dart
    /// side cancelled or the stream was closed; producers should stop (and
    /// holders of stored sinks should prune them).
    pub fn add(&self, item: T) -> bool {
        if let Some(c) = &self.inner.capture {
            // `live()` first, so the `bool` contract is byte-for-byte the
            // production one: after cancel or close this returns false and
            // records nothing, which is what a producer under test is being
            // asked to notice.
            if !self.inner.live() {
                return false;
            }
            c.push(crate::testing::StreamEvent::Item(item));
            return true;
        }
        // Liveness is tested **before** the encode, not only inside
        // `post_method`. Encoding an item that reaches an opaque handle *mints*
        // it, and a cancelled or closed stream sends nothing — so encoding
        // first would register the object with the Dart wrapper that was going
        // to dispose it never built. Tested here, `item` is instead dropped
        // whole and its own `Drop` runs, which is a better reclaim than any
        // walk over bytes: it frees the object the producer built rather than
        // the handles a copy of it minted, and it needs no generated code. It
        // costs a handle-free stream nothing — the same load `post_method`
        // was about to make, one step earlier.
        //
        // The window this leaves — the consumer cancels between here and the
        // post — is what `post_method`'s reclaim covers.
        if !self.inner.live() {
            return false;
        }
        let mut w = item_writer(selector::ADD);
        (self.encode)(item, &mut w);
        post_method(&self.inner, w)
    }

    /// Post one item *with backpressure*: await until the Dart consumer is not
    /// paused, then `add`. Returns the same `bool` as [`add`](Self::add) —
    /// `false` once the stream is cancelled or closed (including if that
    /// happens while parked). This is the idiomatic backpressured producer path
    /// (a bounded-channel `send().await` / a `futures::Sink`); it parks the
    /// *task*, never a thread, so it needs an async context — a sync producer
    /// stays on `add` (unbounded) and may poll [`is_paused`](Self::is_paused).
    /// Readiness is driven by `StreamController.onPause`/`onResume`; a `Sink`/
    /// `EventSink` argument has no pause seam, so a stream fed one never parks.
    ///
    /// Note this inherent `send` (returning `bool`) shadows
    /// [`futures::SinkExt::send`] (returning `Result<(), StreamClosed>`) at a
    /// call site that imports both — inherent methods win resolution. Use the
    /// `Sink` API through combinators (`forward`, `send_all`); reach for this
    /// `send` for a direct one-item post.
    #[must_use = "send() posts nothing until awaited; await it, or use add() for fire-and-forget"]
    pub async fn send(&self, item: T) -> bool {
        self.inner.ready().await;
        self.add(item)
    }

    /// Deliver a non-terminal error to the Dart side (`EventSink.addError`).
    /// The stream stays open — Dart's contract is that a stream may carry
    /// errors and continue. For the terminal form, see [`Self::error`].
    pub fn add_error(&self, msg: impl std::fmt::Display) -> bool {
        if let Some(c) = &self.inner.capture {
            if !self.inner.live() {
                return false;
            }
            c.push::<T>(crate::testing::StreamEvent::Error(msg.to_string()));
            return true;
        }
        let mut w = item_writer(selector::ADD_ERROR);
        w.write_string(&msg.to_string());
        post_method(&self.inner, w)
    }

    /// Whether the Dart subscription cancelled. For producers that want to
    /// stop without having an item ready to `add`.
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }

    /// Whether the Dart consumer is currently backpressuring (paused its
    /// subscription). The non-blocking lever for a *sync* producer, which
    /// cannot `.await` [`send`](Self::send): poll this and drop or sample
    /// instead of overrunning an unbounded buffer. False for a stream fed a
    /// `Sink`/`EventSink` (no pause seam).
    pub fn is_paused(&self) -> bool {
        self.inner.paused.load(Ordering::Acquire)
    }

    /// Terminal error: the Dart stream receives it as a BridgeException and
    /// closes. Clones of this sink stop adding.
    pub fn error(self, msg: impl std::fmt::Display) {
        if self.inner.take_terminal() {
            match &self.inner.capture {
                Some(c) => {
                    c.set_terminal(crate::testing::Terminal::Error(msg.to_string()))
                }
                None => crate::post::respond(
                    self.inner.id,
                    string_envelope(STATUS_ERROR, &msg.to_string()),
                ),
            }
        }
        // Terminal → `live()` false → any parked sibling `send()` is now ready
        // to observe the close and return false.
        self.inner.wake_waiters();
    }

    /// Close the stream now, even if clones remain. Equivalent to dropping
    /// the last clone.
    pub fn close(self) {
        self.inner.close_now();
    }
}

/// The [`futures_sink::Sink`] error for a [`StreamSink`]: the Dart consumer
/// cancelled or closed the stream, so a `forward`/`send_all` producer should
/// stop. The plain [`StreamSink::add`]/[`StreamSink::send`] API reports the
/// same condition as a `false` return instead of an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamClosed;

impl std::fmt::Display for StreamClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the Dart stream was cancelled or closed")
    }
}

impl std::error::Error for StreamClosed {}

/// `futures::Sink` for the StreamController-shaped producer: `poll_ready` is the
/// backpressure gate (the same paused-check `send` awaits), `start_send` posts,
/// and a cancelled/closed stream surfaces as [`StreamClosed`] so ecosystem
/// combinators stop. Only this mirror gets the impl — `Sink`/`EventSink` have
/// no pause seam.
impl<T: Send + 'static> futures_sink::Sink<T> for StreamSink<T> {
    type Error = StreamClosed;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), StreamClosed>> {
        let inner = &self.inner;
        if !inner.live() {
            return Poll::Ready(Err(StreamClosed));
        }
        if !inner.paused.load(Ordering::Acquire) {
            return Poll::Ready(Ok(()));
        }
        // Register-then-recheck, as in `Ready::poll` — a resume/terminal
        // between the checks cannot leave us parked past it.
        inner.register(cx.waker());
        if !inner.live() {
            Poll::Ready(Err(StreamClosed))
        } else if !inner.paused.load(Ordering::Acquire) {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    fn start_send(self: Pin<&mut Self>, item: T) -> Result<(), StreamClosed> {
        if self.add(item) {
            Ok(())
        } else {
            Err(StreamClosed)
        }
    }

    /// Nothing is buffered in the sink — `start_send` posts eagerly — so flush
    /// is always ready.
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), StreamClosed>> {
        Poll::Ready(Ok(()))
    }

    /// Closing the sink ends the Dart stream (drop-retire's explicit form).
    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), StreamClosed>> {
        self.inner.close_now();
        Poll::Ready(Ok(()))
    }
}

/// Construct the sink for a decoded stream id. Called by generated glue,
/// which supplies the item encoder (codecs live in the generated file).
/// Wrap an already-built end. Used by [`crate::testing::stream`], whose
/// `Inner` is captured rather than registered; the encoder is never called on
/// that path, so it can be one that writes nothing.
pub(crate) fn sink_from<T>(inner: Arc<Inner>) -> StreamSink<T> {
    StreamSink {
        inner,
        encode: |_, _| {
            unreachable!("frustrate::testing: a captured sink never encodes")
        },
    }
}

pub fn sink<T>(id: u64, encode: fn(T, &mut ByteWriter)) -> StreamSink<T> {
    StreamSink {
        inner: end(id),
        encode,
    }
}

/// Look one live `Inner` up by id, or `None` if unknown/already-ended. The
/// shared prelude of every Dart→Rust stream signal (cancel/pause/resume); an
/// unknown id is always a legal race with the stream ending.
fn lookup(id: u64) -> Option<Arc<Inner>> {
    with_registry(|m| m.get(&id).and_then(Weak::upgrade))
}

/// Cooperative cancel from the Dart side. Safe for any id: unknown or
/// already-ended streams are a no-op (the cancel/end race is legal).
// `frustrate_block_check` gating, here and on the two signal exports below:
// see the "block-check export gating" note in lib.rs.
#[cfg(not(frustrate_block_check))]
#[no_mangle]
pub extern "C" fn frustrate_stream_cancel(id: u64) {
    if let Some(inner) = lookup(id) {
        // A cancel makes `live()` false: wake any parked `send()` so it returns
        // false rather than hanging on a stream the consumer abandoned.
        inner.cancel();
    }
}

/// Backpressure from the Dart side: the consumer paused its subscription
/// (`StreamController.onPause`). A producer awaiting [`StreamSink::send`] parks
/// until resume. `add` is unaffected — it never blocks. Store only; nothing to
/// wake on a pause. Safe for any id.
#[cfg(not(frustrate_block_check))]
#[no_mangle]
pub extern "C" fn frustrate_stream_pause(id: u64) {
    if let Some(inner) = lookup(id) {
        inner.pause();
    }
}

/// The consumer resumed (`StreamController.onResume`): clear the pause flag and
/// wake every producer parked in `send()`. The store precedes the wake so a
/// re-polling parked task observes `paused == false` (see `Ready::poll`). Safe
/// for any id.
#[cfg(not(frustrate_block_check))]
#[no_mangle]
pub extern "C" fn frustrate_stream_resume(id: u64) {
    if let Some(inner) = lookup(id) {
        inner.resume();
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use super::*;
    use crate::codec::ByteReader;
    use crate::envelope::{STATUS_ERROR, STATUS_STREAM_END, STATUS_STREAM_ITEM};
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex;

    fn enc_i64(v: i64, w: &mut ByteWriter) {
        w.write_i64(v);
    }

    static SEEN: Mutex<Vec<(u64, Vec<u8>)>> = Mutex::new(Vec::new());

    extern "C" fn record(call_id: u64, ptr: *mut u8, len: u64, cap: u64) {
        let bytes = unsafe { Vec::from_raw_parts(ptr, len as usize, cap as usize) };
        SEEN.lock().unwrap().push((call_id, bytes));
    }

    fn events_for(id: u64) -> Vec<Vec<u8>> {
        SEEN.lock()
            .unwrap()
            .iter()
            .filter(|(i, _)| *i == id)
            .map(|(_, b)| b.clone())
            .collect()
    }

    /// The registry lock must survive an unwinding critical section
    /// (regression guard; the defect landed fixed 2026-07-23).
    ///
    /// Before the fix, `SpinLock::with` released with a plain `store(false)`
    /// *after* `f`, so a panic inside the critical section skipped the release
    /// and left the lock held forever. That was not merely a leak:
    /// `frustrate_stream_cancel` may be entered from the browser main thread,
    /// and a held lock turns it into an unbounded spin — an app freeze rather than a loud
    /// crash. `with` now releases via a `Drop` guard, like the sibling allocator
    /// lock in pool.rs; this test holds that line.
    ///
    /// Serialized against the other registry test via `post::test_lock`: while
    /// this test is deliberately wedging a process-global lock, no other test
    /// may touch it.
    #[test]
    fn a_panicking_critical_section_releases_the_registry_lock() {
        let _serial = crate::post::test_lock();
        // Silence the deliberate panic's default backtrace print.
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let unwound = std::panic::catch_unwind(|| {
            with_registry(|_| panic!("deliberate panic inside the critical section"))
        })
        .is_err();
        std::panic::set_hook(prev);
        assert!(unwound, "the fixture must actually unwind");

        // Read-and-clear rather than a plain read: if the lock IS wedged,
        // clearing it here is what keeps the very next registry access — in
        // this test binary or the sibling test below — from spinning forever
        // instead of reporting this failure.
        let wedged = REGISTRY.take_held();
        assert!(
            !wedged,
            "the registry lock was still held after its critical section \
             unwound: frustrate_stream_cancel would spin on it forever when \
             entered from the browser main thread"
        );
    }

    // One test fn: the post callback is process-global (last-write-wins),
    // so this must not run concurrently with other post-using tests; see
    // post::test_lock.
    #[test]
    fn sink_lifecycle_cancel_and_terminals() {
        let _serial = crate::post::test_lock();
        crate::post::init(record);

        // Items then drop → items in order, then exactly one end.
        let s = sink::<i64>(9001, enc_i64);
        assert!(s.add(1));
        let clone = s.clone();
        drop(s);
        assert!(clone.add(2), "clone keeps the stream open");
        drop(clone);
        let ev = events_for(9001);
        assert_eq!(ev.len(), 3, "{ev:?}");
        // [status, selector, payload…] — `add` is selector 0 on every mirror.
        assert_eq!(ev[0][0], STATUS_STREAM_ITEM);
        assert_eq!(ev[0][1], selector::ADD);
        assert_eq!(ByteReader::new(&ev[0][2..]).read_i64(), 1);
        assert_eq!(ByteReader::new(&ev[1][2..]).read_i64(), 2);
        // The terminal carries no selector: it retires the registration for
        // every mirror, whatever its method set.
        assert_eq!(ev[2], vec![STATUS_STREAM_END]);

        // Terminal error posts once; late clones stop adding, no end event.
        let s = sink::<i64>(9002, enc_i64);
        let c = s.clone();
        s.error("boom");
        assert!(!c.add(3), "add after terminal must refuse");
        drop(c);
        let ev = events_for(9002);
        assert_eq!(ev.len(), 1, "{ev:?}");
        assert_eq!(ev[0][0], STATUS_ERROR);
        assert_eq!(ByteReader::new(&ev[0][1..]).read_string(), "boom");

        // Cancel: flag visible cross-thread, everything suppressed after.
        let s = sink::<i64>(9003, enc_i64);
        assert!(s.add(1));
        let handle = {
            let s = s.clone();
            std::thread::spawn(move || {
                while !s.is_cancelled() {
                    std::hint::spin_loop();
                }
            })
        };
        frustrate_stream_cancel(9003);
        handle.join().unwrap();
        assert!(!s.add(2));
        drop(s);
        let ev = events_for(9003);
        assert_eq!(ev.len(), 1, "post-cancel events must be suppressed: {ev:?}");

        // Cancel for an unknown / already-dead id is a no-op.
        frustrate_stream_cancel(9004);
        with_registry(|m| assert!(!m.contains_key(&9001), "registry must not leak"));

        // add_error is a *non-terminal* selector: the stream stays open and
        // keeps accepting items (Dart's EventSink contract). This is the pair
        // that is easy to conflate with `error`, which ends the stream above.
        let s = sink::<i64>(9005, enc_i64);
        assert!(s.add_error("recoverable"));
        assert!(s.add(5), "addError must not close the stream");
        drop(s);
        let ev = events_for(9005);
        assert_eq!(ev.len(), 3, "{ev:?}");
        assert_eq!(ev[0][0], STATUS_STREAM_ITEM);
        assert_eq!(ev[0][1], selector::ADD_ERROR);
        assert_eq!(ByteReader::new(&ev[0][2..]).read_string(), "recoverable");
        assert_eq!(ev[1][1], selector::ADD);
        assert_eq!(ev[2], vec![STATUS_STREAM_END], "one normal end, not an error");
    }

    /// Backpressure: `send()` parks while paused, wakes on resume, unparks with
    /// `false` on cancel, and deregisters its waker when a parked send is
    /// dropped. Driven by hand with a counting waker — no executor — so the
    /// register-then-recheck ordering and the wake set are asserted directly.
    #[test]
    fn send_parks_while_paused_and_wakes_on_resume_or_cancel() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::Arc as StdArc;
        use std::task::Wake;

        let _serial = crate::post::test_lock();
        crate::post::init(record);

        struct Counting(AtomicUsize);
        impl Wake for Counting {
            fn wake(self: StdArc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
            fn wake_by_ref(self: &StdArc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let counter = StdArc::new(Counting(AtomicUsize::new(0)));
        let waker = Waker::from(counter.clone());
        let mut cx = Context::from_waker(&waker);
        let wakes = || counter.0.load(Ordering::SeqCst);

        let s = sink::<i64>(9101, enc_i64);

        // Not paused → resolves on the first poll and posts the item.
        assert_eq!(
            std::pin::pin!(s.send(1)).as_mut().poll(&mut cx),
            Poll::Ready(true)
        );
        assert_eq!(events_for(9101).len(), 1);

        // Paused → parks without posting; resume wakes it; the re-poll posts.
        frustrate_stream_pause(9101);
        {
            let mut fut = std::pin::pin!(s.send(2));
            assert_eq!(fut.as_mut().poll(&mut cx), Poll::Pending);
            assert_eq!(events_for(9101).len(), 1, "a paused send must not post");
            assert_eq!(s.inner.wakers.with(|v| v.len()), 1, "waker registered");

            let before = wakes();
            frustrate_stream_resume(9101);
            assert!(wakes() > before, "resume must wake the parked sender");
            assert_eq!(fut.as_mut().poll(&mut cx), Poll::Ready(true));
        }
        assert_eq!(events_for(9101).len(), 2, "resumed send posts its item");

        // Cancel while paused → the parked send wakes and returns false, no post.
        frustrate_stream_pause(9101);
        {
            let mut fut = std::pin::pin!(s.send(3));
            assert_eq!(fut.as_mut().poll(&mut cx), Poll::Pending);
            frustrate_stream_cancel(9101);
            assert_eq!(
                fut.as_mut().poll(&mut cx),
                Poll::Ready(false),
                "cancel unparks a paused send with false"
            );
        }
        assert_eq!(events_for(9101).len(), 2, "a cancelled send posts nothing");
        drop(s);

        // Deregister-on-drop: a parked send dropped before resume leaves the
        // waker set empty, so a later stream drop cannot trip the Drop assert.
        let s = sink::<i64>(9102, enc_i64);
        frustrate_stream_pause(9102);
        {
            let mut fut = std::pin::pin!(s.send(9));
            assert_eq!(fut.as_mut().poll(&mut cx), Poll::Pending);
            assert_eq!(s.inner.wakers.with(|v| v.len()), 1);
        }
        assert_eq!(
            s.inner.wakers.with(|v| v.len()),
            0,
            "dropping a parked send deregisters its waker"
        );
        frustrate_stream_resume(9102); // unknown-after-drop paths are no-ops
        drop(s);
    }

    /// A channel item that **mints** is either delivered or reclaimed, and the
    /// producer does not mint at all for a channel it can already see is dead.
    ///
    /// The encoder here stands in for a generated one: it registers the object
    /// (modelled by `mem::forget`, which is what `handle::confined_new`'s
    /// `Box::into_raw` amounts to) and records the drop through
    /// `write_minted`. So "the object leaked" and "the object was reclaimed"
    /// are distinguishable — `DROPPED` counts the item's own destructor,
    /// `RECLAIMED` the ledger's.
    #[test]
    fn an_undelivered_item_gives_back_what_it_minted() {
        let _serial = crate::post::test_lock();
        crate::post::init(record);

        static DROPPED: AtomicUsize = AtomicUsize::new(0);
        static RECLAIMED: Mutex<Vec<u64>> = Mutex::new(Vec::new());

        struct Counted(u64);
        impl Drop for Counted {
            fn drop(&mut self) {
                DROPPED.fetch_add(1, Ordering::SeqCst);
            }
        }
        unsafe fn record_reclaim(h: u64) {
            RECLAIMED.lock().unwrap().push(h);
        }
        fn enc(v: Counted, w: &mut ByteWriter) {
            // The mint: the registry now owns the object, so the local must not
            // run its destructor — exactly `Box::into_raw`'s effect.
            unsafe { w.write_minted(v.0, record_reclaim) };
            std::mem::forget(v);
        }

        // Cancelled *before* the add: nothing is encoded, so nothing is minted,
        // and the item is dropped whole. This is the common shape — a producer
        // looping on `add` past a cancel — and it needs no ledger at all.
        let s = sink::<Counted>(9301, enc);
        s.inner.cancel();
        assert!(!s.add(Counted(11)));
        assert_eq!(DROPPED.load(Ordering::SeqCst), 1, "the item's own Drop ran");
        assert!(
            RECLAIMED.lock().unwrap().is_empty(),
            "a cancelled stream must not mint"
        );
        drop(s);

        // The window the ledger exists for: encoded while live, refused at the
        // post. Driven through `post_method` directly, because the race it
        // models cannot be scheduled from a single thread.
        let inner = end(9302);
        let mut w = item_writer(selector::ADD);
        enc(Counted(22), &mut w);
        assert_eq!(DROPPED.load(Ordering::SeqCst), 1, "minting consumed the item");
        inner.cancel();
        assert!(!post_method(&inner, w));
        assert_eq!(
            *RECLAIMED.lock().unwrap(),
            vec![22],
            "an undelivered item frees the handle it minted"
        );
        assert!(events_for(9302).is_empty(), "and posts nothing");

        // The delivered case does not reclaim: the wrapper on the far side owns
        // the object now.
        RECLAIMED.lock().unwrap().clear();
        let s = sink::<Counted>(9303, enc);
        assert!(s.add(Counted(33)));
        assert!(
            RECLAIMED.lock().unwrap().is_empty(),
            "a delivered item keeps its handle"
        );
        assert_eq!(events_for(9303).len(), 1);
        drop(s);
    }

    /// The `futures::Sink` impl: `poll_ready` gates on the same pause
    /// state as `send`, and a cancelled stream surfaces as `StreamClosed` so
    /// ecosystem combinators stop. Driven through the trait directly.
    #[test]
    fn sink_impl_backpressures_and_reports_closed() {
        use futures_sink::Sink;
        use std::sync::Arc as StdArc;
        use std::task::Wake;

        let _serial = crate::post::test_lock();
        crate::post::init(record);

        struct Noop;
        impl Wake for Noop {
            fn wake(self: StdArc<Self>) {}
        }
        let waker = Waker::from(StdArc::new(Noop));
        let mut cx = Context::from_waker(&waker);

        let mut s = sink::<i64>(9201, enc_i64);

        // Not paused → ready, and start_send posts the item.
        assert!(matches!(
            Pin::new(&mut s).poll_ready(&mut cx),
            Poll::Ready(Ok(()))
        ));
        assert_eq!(Pin::new(&mut s).start_send(1), Ok(()));
        assert_eq!(events_for(9201).len(), 1);

        // Paused → not ready; resume → ready again.
        frustrate_stream_pause(9201);
        assert!(matches!(Pin::new(&mut s).poll_ready(&mut cx), Poll::Pending));
        frustrate_stream_resume(9201);
        assert!(matches!(
            Pin::new(&mut s).poll_ready(&mut cx),
            Poll::Ready(Ok(()))
        ));

        // Cancelled → poll_ready and start_send both report StreamClosed.
        frustrate_stream_cancel(9201);
        assert!(matches!(
            Pin::new(&mut s).poll_ready(&mut cx),
            Poll::Ready(Err(StreamClosed))
        ));
        assert_eq!(Pin::new(&mut s).start_send(2), Err(StreamClosed));
        assert_eq!(events_for(9201).len(), 1, "no post after cancel");
        drop(s);
    }
}
