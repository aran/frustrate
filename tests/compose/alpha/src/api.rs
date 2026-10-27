//! One of two bridges composed into one library (//tests/compose).

use frustrate::{bridge, StreamSink};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};

static DROPS: AtomicI64 = AtomicI64::new(0);

/// Write the value both bridges share.
#[bridge(sync)]
pub fn alpha_put(v: i64) {
    compose_shared::put(v)
}

/// Read the value both bridges share.
#[bridge(sync)]
pub fn alpha_get() -> i64 {
    compose_shared::get()
}

/// An async call: its completion travels the transport's port.
#[bridge]
pub async fn alpha_twice(x: i64) -> i64 {
    x * 2
}

/// A stream: 1..=n, then the end.
#[bridge]
pub fn alpha_count(n: i64, sink: StreamSink<i64>) {
    for i in 1..=n {
        if !sink.add(i) {
            return;
        }
    }
}

/// How many [AlphaBox]es have been dropped.
#[bridge(sync)]
pub fn alpha_drops() -> i64 {
    DROPS.load(Ordering::SeqCst)
}

/// An opaque handle, freed through this bridge's own drop export.
#[bridge(confined)]
pub struct AlphaBox {
    value: i64,
}

#[bridge]
impl AlphaBox {
    #[bridge(sync)]
    pub fn new(value: i64) -> Self {
        AlphaBox { value }
    }

    #[bridge(sync)]
    pub fn value(&self) -> i64 {
        self.value
    }
}

impl Drop for AlphaBox {
    fn drop(&mut self) {
        DROPS.fetch_add(1, Ordering::SeqCst);
    }
}

static HELD: Mutex<Option<StreamSink<i64>>> = Mutex::new(None);

/// Open a stream that stays open until [alpha_end_held].
#[bridge]
pub fn alpha_hold(sink: StreamSink<i64>) {
    *HELD.lock().unwrap() = Some(sink);
}

/// End the stream [alpha_hold] opened.
#[bridge(sync)]
pub fn alpha_end_held() {
    HELD.lock().unwrap().take();
}

static OPEN: AtomicBool = AtomicBool::new(false);
static WAITER: Mutex<Option<Waker>> = Mutex::new(None);

/// A future that is ready once the gate opens. Wakes through the waker it
/// was last polled with, so it suspends the cooperative executor rather than
/// blocking a thread, which single-threaded web has only one of.
struct Gate;

impl Future for Gate {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if OPEN.load(Ordering::SeqCst) {
            return Poll::Ready(());
        }
        *WAITER.lock().unwrap() = Some(cx.waker().clone());
        if OPEN.load(Ordering::SeqCst) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

/// Close the gate [alpha_wait] waits on.
#[bridge(sync)]
pub fn alpha_close_gate() {
    OPEN.store(false, Ordering::SeqCst);
}

/// A call that stays in flight until [alpha_open_gate].
#[bridge]
pub async fn alpha_wait() -> i64 {
    Gate.await;
    1
}

/// Open the gate, completing every [alpha_wait].
#[bridge(sync)]
pub fn alpha_open_gate() {
    OPEN.store(true, Ordering::SeqCst);
    if let Some(waker) = WAITER.lock().unwrap().take() {
        waker.wake();
    }
}
