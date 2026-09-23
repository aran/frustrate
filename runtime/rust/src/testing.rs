//! Test doubles for bridged surfaces, so application code can be unit-tested.
//!
//! # Why this is always compiled
//!
//! Not behind `cfg(test)`: that does not cross crates, and frustrate is a
//! *dependency* of the crate under test — even under `rust_test(crate = …)`,
//! which is the only form a bridge crate can use at all. Not behind a cargo
//! feature either: a test-only feature is not separable under
//! `crate.from_cargo`, measured at one line in `[dev-dependencies]` taking a
//! lockfile from 379 to 427 packages, into the manifest that produces an iPhone
//! dylib. So this ships, always, on every platform, like `tokio::sync`'s test
//! utilities. Using it outside a test is not unsound — it is a buffer nobody
//! reads.
//!
//! # What it is for
//!
//! A `StreamSink` could not be constructed outside a live Dart isolate:
//! `post::deliver` panics for an id nothing registered, and drop-retire posts
//! too, so even building one and letting it fall out of scope panicked. Every
//! frustrate app that streams therefore invented the same substitute — an
//! internal event enum with a `#[cfg(test)]` channel variant — which leaves the
//! one code path that actually calls `sink.add` as the one path never covered.
//!
//! [`stream`] hands out a real [`StreamSink`] whose events are recorded instead
//! of posted. The sink is the same type the bridge hands your code, and
//! everything except delivery is the production implementation: `live()`
//! gating, once-only terminals, the pause flag, the waker set, `send()`'s
//! parking.

use std::any::Any;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use crate::spin::SpinLock;
use crate::stream::{Inner, StreamSink};

/// One non-terminal event a producer emitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent<T> {
    /// `sink.add(item)`.
    Item(T),
    /// `sink.add_error(msg)` — non-terminal; the stream stays open. The
    /// *terminal* error is [`Terminal::Error`], and keeping them apart here is
    /// deliberate: conflating them is the mistake this distinction exists to
    /// prevent.
    Error(String),
}

/// How the channel ended, if it has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Terminal {
    /// `close()`, `poll_close`, or the last clone dropping. Drop-closes-the-
    /// channel is observable here because it is load-bearing in the real
    /// contract.
    Closed,
    /// The terminal `error(self, msg)`.
    Error(String),
    /// Dropped inside a `frustrate_finalize_*` export: a Dart handle was
    /// collected without `dispose()`, so the channel was abandoned rather than
    /// closed. Reachable from a test only by driving your own type's finalize
    /// path; most tests never see it.
    Leaked(&'static str),
}

/// The recording behind a captured channel end.
///
/// Type-erased because [`Inner`] is not generic over the item type and must not
/// become so — it is shared by every stream in the process. `Send + Sync` on
/// the erased box is load-bearing: `Inner` is `Arc`-shared across threads, and
/// `dyn Any + Send` alone would cost it `Sync`.
///
/// # Why these are [`SpinLock`]s and not `Mutex`es
///
/// Not for contention — a captured end has one producer and one probe. It is
/// that these locks are *linked into every build*, including the throwaway
/// module `tools/check_block.dart` scans. `Inner::capture` is `None` in every
/// stream a generated binding builds, so this branch never runs in a shipped
/// app; but lld cannot know that, and a `#[bridge(no_block)]` body that calls
/// `sink.add` links it. With a parking `Mutex` here the gate read
/// `emit_two -> StreamSink::add -> CaptureState::push -> Mutex::lock ->
/// lock_contended -> futex_wait -> memory.atomic.wait32` and went red — so no
/// stream-emitting member could carry the claim, in any crate, for a reason
/// that had nothing to do with the user's code.
///
/// Gating the branch out under `cfg(frustrate_block_check)` would also have
/// been green, and would have made the artifact a worse witness: the scanned
/// module would no longer contain the `add` the shipped one does. Spinning
/// keeps the whole path in the artifact and removes the wait instead.
///
/// Both sections obey spin.rs's three rules: one container op, nothing dropped
/// inside (see [`set_terminal`](Self::set_terminal)), release by guard. The
/// same reasoning that already makes `Inner::wakers` a `SpinLock`.
pub(crate) struct CaptureState {
    events: Box<dyn Any + Send + Sync>,
    terminal: SpinLock<Option<Terminal>>,
}

impl CaptureState {
    fn new<T: Send + 'static>(events: Arc<SpinLock<Vec<StreamEvent<T>>>>) -> Self {
        CaptureState {
            events: Box::new(events),
            terminal: SpinLock::new(None),
        }
    }

    /// Record one event. The downcast cannot fail: the only constructor pairs
    /// the erased box with the sink's own `T`, and neither escapes.
    pub(crate) fn push<T: Send + 'static>(&self, e: StreamEvent<T>) {
        self.log::<T>().with(|v| v.push(e));
    }

    fn log<T: Send + 'static>(&self) -> &Arc<SpinLock<Vec<StreamEvent<T>>>> {
        self.events
            .downcast_ref()
            .expect("frustrate::testing: capture built for a different item type")
    }

    /// Record the terminal. Callers have already passed `take_terminal`, so
    /// once-only accounting is the production code's, not a second copy.
    pub(crate) fn set_terminal(&self, t: Terminal) {
        // `Option::replace` rather than an assignment, because an assignment
        // drops the displaced `Terminal` — and its `String` — *inside* the
        // critical section. Handing it back lets it die after the guard has
        // released (spin.rs, "Nothing may drop inside the section").
        let _displaced = self.terminal.with(|g| g.replace(t));
    }
}

/// Ids for captured ends. A private counter, not the Dart-allocated space:
/// nothing routes these, and nothing else may collide with them.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// A [`StreamSink`] that records, and the probe that reads it.
///
/// ```no_run
/// # use frustrate::testing::{stream, StreamEvent::Item};
/// let (sink, probe) = stream::<i64>();
/// sink.add(1);
/// sink.add(2);
/// assert_eq!(probe.take_events(), vec![Item(1), Item(2)]);
/// ```
///
/// Hand `sink` to the code under test exactly as a bridged member would
/// receive it. The probe holds only a [`Weak`] reference to the channel, so
/// dropping the last sink still runs drop-retire — which is what lets a test
/// assert that its own teardown closes the channel.
pub fn stream<T: Send + 'static>() -> (StreamSink<T>, StreamProbe<T>) {
    let events: Arc<SpinLock<Vec<StreamEvent<T>>>> = Arc::new(SpinLock::new(Vec::new()));
    let capture = Arc::new(CaptureState::new(events.clone()));
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let inner = crate::stream::captured_end(id, capture.clone());
    let probe = StreamProbe {
        inner: Arc::downgrade(&inner),
        capture,
        events,
    };
    (crate::stream::sink_from(inner), probe)
}

/// Reads what a producer emitted, and plays the part of the Dart consumer.
pub struct StreamProbe<T> {
    inner: Weak<Inner>,
    capture: Arc<CaptureState>,
    events: Arc<SpinLock<Vec<StreamEvent<T>>>>,
}

impl<T: Send + 'static> StreamProbe<T> {
    /// Everything emitted since the last call, in emission order. Drains, so
    /// successive assertions read successive spans rather than a growing
    /// prefix.
    pub fn take_events(&self) -> Vec<StreamEvent<T>> {
        // The drained `Vec` leaves the section rather than dying in it.
        self.events.with(std::mem::take)
    }

    /// How the channel ended, or `None` if it is still open.
    ///
    /// A channel the consumer **cancelled** ends quiet — `None` — because
    /// `take_terminal` says so in production. That is the real contract, not a
    /// tidied one.
    pub fn terminal(&self) -> Option<Terminal> {
        self.capture.terminal.with(|t| t.clone())
    }

    /// What the Dart subscription's `cancel()` does. `add` returns `false` from
    /// here on, which is the signal a well-behaved producer is supposed to act
    /// on.
    pub fn cancel(&self) {
        if let Some(inner) = self.inner.upgrade() {
            inner.cancel();
        }
    }

    /// The consumer is backpressuring (`StreamController.onPause`). A producer
    /// awaiting `send()` parks; `add` is unaffected, by design.
    pub fn pause(&self) {
        if let Some(inner) = self.inner.upgrade() {
            inner.pause();
        }
    }

    /// Backpressure off, and wake anything parked in `send()`.
    pub fn resume(&self) {
        if let Some(inner) = self.inner.upgrade() {
            inner.resume();
        }
    }

    /// Whether the channel is still open — neither cancelled nor ended. False
    /// once the producer dropped its last sink, which is the ordinary way a
    /// test observes teardown.
    pub fn is_open(&self) -> bool {
        self.inner.upgrade().is_some_and(|i| i.live())
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use super::*;
    use crate::stream::StreamSink;

    /// The headline fence. Every line of this panicked before capture existed:
    /// `add` reached `post::deliver` with an id nothing registered, and so did
    /// drop-retire when the sink fell out of scope at the end of the test.
    ///
    /// No `post::test_lock` anywhere in this module, deliberately — a captured
    /// end touches no global state, so consumer tests parallelize. That is
    /// half the point.
    #[test]
    fn a_producer_can_be_driven_with_no_dart_isolate_anywhere() {
        let (sink, probe) = stream::<i64>();
        assert!(sink.add(1));
        assert!(sink.add(2));
        assert_eq!(
            probe.take_events(),
            vec![StreamEvent::Item(1), StreamEvent::Item(2)]
        );
        // Drains: a second read sees the next span, not the same prefix again.
        assert_eq!(probe.take_events(), vec![]);
    }

    #[test]
    fn add_error_is_recorded_in_order_and_is_not_a_terminal() {
        let (sink, probe) = stream::<i64>();
        sink.add(1);
        assert!(sink.add_error("bad frame"));
        sink.add(2);
        assert_eq!(
            probe.take_events(),
            vec![
                StreamEvent::Item(1),
                StreamEvent::Error("bad frame".into()),
                StreamEvent::Item(2),
            ]
        );
        assert_eq!(probe.terminal(), None, "add_error must not end the stream");
        assert!(probe.is_open());
    }

    /// The contract a producer is coded against: after the consumer goes away,
    /// `add` reports false so the producer can stop. Capture honours it by
    /// running the same `live()` gate, not by reimplementing it.
    #[test]
    fn a_cancelled_stream_refuses_adds_and_records_nothing() {
        let (sink, probe) = stream::<i64>();
        sink.add(1);
        probe.cancel();
        assert!(!sink.add(2), "add after cancel must report false");
        assert!(!sink.add_error("x"));
        assert_eq!(probe.take_events(), vec![StreamEvent::Item(1)]);
        assert!(!probe.is_open());
    }

    /// Drop-closes-the-channel is observable, because it is load-bearing in
    /// the real contract — an app asserting "my teardown closes my channels"
    /// is exactly the thing that could not be tested before.
    #[test]
    fn dropping_the_last_sink_closes_the_channel() {
        let (sink, probe) = stream::<i64>();
        let clone = sink.clone();
        drop(sink);
        assert_eq!(probe.terminal(), None, "a clone still holds it open");
        assert!(probe.is_open());
        drop(clone);
        assert_eq!(probe.terminal(), Some(Terminal::Closed));
    }

    #[test]
    fn the_terminal_error_is_distinct_from_add_error() {
        let (sink, probe) = stream::<i64>();
        sink.add_error("non-terminal");
        sink.error("terminal");
        assert_eq!(probe.terminal(), Some(Terminal::Error("terminal".into())));
        assert_eq!(
            probe.take_events(),
            vec![StreamEvent::Error("non-terminal".into())]
        );
    }

    #[test]
    fn an_explicit_close_then_a_drop_reports_one_terminal() {
        let (sink, probe) = stream::<i64>();
        let clone = sink.clone();
        sink.close();
        assert_eq!(probe.terminal(), Some(Terminal::Closed));
        drop(clone); // must not overwrite or double-report
        assert_eq!(probe.terminal(), Some(Terminal::Closed));
    }

    /// A cancelled channel ends *quiet*. That is `take_terminal`'s production
    /// behaviour and the probe inherits it rather than tidying it up: a test
    /// that expected a terminal here would be learning the wrong contract.
    #[test]
    fn a_cancelled_channel_reports_no_terminal_when_dropped() {
        let (sink, probe) = stream::<i64>();
        probe.cancel();
        drop(sink);
        assert_eq!(probe.terminal(), None);
    }

    /// Backpressure is the real flag and the real waker set; only delivery is
    /// faked. `add` ignores pause by design — that is the documented
    /// difference between `add` and `send`.
    #[test]
    fn pause_and_resume_drive_the_real_backpressure_state() {
        let (sink, probe) = stream::<i64>();
        assert!(!sink.is_paused());
        probe.pause();
        assert!(sink.is_paused());
        assert!(sink.add(1), "add is unbuffered and ignores pause");
        probe.resume();
        assert!(!sink.is_paused());
        assert_eq!(probe.take_events(), vec![StreamEvent::Item(1)]);
    }

    #[test]
    fn is_cancelled_tracks_the_probe() {
        let (sink, probe) = stream::<i64>();
        assert!(!sink.is_cancelled());
        probe.cancel();
        assert!(sink.is_cancelled());
    }

    /// A captured end must never reach the transport. Without this, a capture
    /// that fell through to `post_method` would still pass every assertion
    /// above — the events would simply also be posted.
    #[test]
    fn a_captured_end_delivers_nothing_to_the_post_layer() {
        let _serial = crate::post::test_lock();
        static SEEN: std::sync::Mutex<Vec<u64>> = std::sync::Mutex::new(Vec::new());
        extern "C" fn record(call_id: u64, ptr: *mut u8, len: u64, cap: u64) {
            drop(unsafe { Vec::from_raw_parts(ptr, len as usize, cap as usize) });
            SEEN.lock().unwrap().push(call_id);
        }
        crate::post::init(record);
        SEEN.lock().unwrap().clear();

        let (sink, probe) = stream::<i64>();
        sink.add(7);
        sink.add_error("e");
        sink.error("terminal");
        drop(probe);

        assert!(
            SEEN.lock().unwrap().is_empty(),
            "a captured end posted to the transport: {:?}",
            SEEN.lock().unwrap()
        );
    }

    /// The backpressured path through the probe: `send()` parks under pause
    /// and wakes on resume, driving the real waker set. Hand-polled because v1
    /// ships no executor, and a bridge crate cannot add one just for its tests:
    /// `crate.from_cargo` resolves one feature set per crate for the whole
    /// module, so a dev-dependency lands in the manifest that builds the
    /// shipped library. The doc therefore carries this exact snippet.
    #[test]
    fn send_parks_under_probe_pause_and_wakes_on_resume() {
        use std::future::Future;
        use std::sync::atomic::AtomicUsize;
        use std::task::{Context, Poll, Wake, Waker};

        struct Counting(AtomicUsize);
        impl Wake for Counting {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
            fn wake_by_ref(self: &Arc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let counter = Arc::new(Counting(AtomicUsize::new(0)));
        let waker = Waker::from(counter.clone());
        let mut cx = Context::from_waker(&waker);

        let (sink, probe) = stream::<i64>();
        probe.pause();
        let fut = sink.send(9);
        let mut fut = std::pin::pin!(fut);
        assert!(
            fut.as_mut().poll(&mut cx).is_pending(),
            "a paused consumer must park the producer"
        );
        assert_eq!(probe.take_events(), vec![], "nothing emitted while parked");

        probe.resume();
        assert!(counter.0.load(Ordering::SeqCst) >= 1, "resume must wake");
        assert_eq!(fut.as_mut().poll(&mut cx), Poll::Ready(true));
        assert_eq!(probe.take_events(), vec![StreamEvent::Item(9)]);
    }

    /// A parked producer must also wake on cancel — and return false, not hang
    /// on a stream nobody is reading.
    #[test]
    fn send_parked_under_pause_unparks_false_on_cancel() {
        use std::future::Future;
        use std::task::{Context, Poll, Waker};

        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        let (sink, probe) = stream::<i64>();
        probe.pause();
        let mut fut = std::pin::pin!(sink.send(1));
        assert!(fut.as_mut().poll(&mut cx).is_pending());
        probe.cancel();
        assert_eq!(fut.as_mut().poll(&mut cx), Poll::Ready(false));
        assert_eq!(probe.take_events(), vec![]);
    }

    /// Compile-time fences on what this change must NOT have cost.
    ///
    /// The capture lives on `Inner` behind `dyn Any + Send + Sync` precisely so
    /// these hold. Moving the typed log onto `StreamSink<T>` would make its
    /// `Send`/`Sync` conditional on `T`, and erasing with `dyn Any + Send`
    /// alone would cost `Inner` its `Sync`.
    #[test]
    fn the_shared_types_kept_their_auto_traits() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<StreamSink<i64>>();
        assert_send_sync::<crate::stream::Inner>();
        assert_send_sync::<CaptureState>();
        assert_send_sync::<StreamProbe<i64>>();
    }
}
