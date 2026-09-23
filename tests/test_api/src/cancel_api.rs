// `new_without_default` is off for the same reason it is off in `api.rs`:
// `Sensor::new` is a *bridged constructor* Dart calls across the ABI, and a
// `Default` impl would be Rust-side dead code added only to satisfy the lint.
#![allow(clippy::new_without_default)]

//! The deferred-cancel fixture: an actor whose `Deferred` completion holds a
//! **drop sensor** across its suspension point.
//!
//! Cancelling a call here means the Rust future is dropped, and that is the
//! claim a test has to be able to check. A test that only watches the Dart
//! future would pass against a runtime that stopped listening and leaked the
//! task — the same trap `awaits_until_cancelled` in [`crate::api`] exists to
//! close for a plain bridged `async fn`.
//!
//! The sensor is read through an **actor method** and lives per instance: on
//! web an actor is a Worker running its own wasm instance, so a `static` in the
//! page instance is a different `static`; only a call into *this* actor can see
//! what its own future did. A fresh actor is therefore a fresh sensor.

use frustrate::{bridge, Deferred};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// What a parked deferred completion waits on, and what a later message on the
/// same actor opens.
///
/// Hand-rolled (a mutex and a waker list) for the reason `Gate` in
/// [`crate::api`] is: a `Deferred` future runs on frustrate's cooperative
/// executor, which provides no async runtime unless the app registered one —
/// so the portable form, and the only one on web, is a plain future over an
/// explicitly shared value.
#[derive(Default)]
struct Latch {
    state: Mutex<(Option<i64>, Vec<std::task::Waker>)>,
}

impl Latch {
    fn open(&self, value: i64) {
        let wakers = {
            let mut s = self.state.lock().unwrap();
            s.0 = Some(value);
            std::mem::take(&mut s.1)
        };
        for w in wakers {
            w.wake();
        }
    }

    fn wait(self: &Arc<Self>) -> LatchWait {
        LatchWait(self.clone())
    }
}

/// Resolves to the latch's value once it opens, parking (and registering its
/// waker) until then. The suspension point the drop sensor is held across.
struct LatchWait(Arc<Latch>);

impl std::future::Future for LatchWait {
    type Output = i64;
    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<i64> {
        let mut s = self.0.state.lock().unwrap();
        match s.0 {
            Some(v) => std::task::Poll::Ready(v),
            None => {
                s.1.push(cx.waker().clone());
                std::task::Poll::Pending
            }
        }
    }
}

/// Records that the future holding it was dropped.
///
/// Moved *into* the `async` block rather than captured by the prefix, so it is
/// owned by the future itself and lives exactly as long as it does — a cancel
/// (the task claimed out of the executor and dropped on a later drain), a
/// completion, and a panicking poll all end here.
struct DropSensor(Arc<AtomicBool>);

impl Drop for DropSensor {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// An actor with one deferred completion and a window onto whether that
/// completion's future has been dropped.
#[bridge(actor)]
pub struct Sensor {
    latch: Arc<Latch>,
    dropped: Arc<AtomicBool>,
    /// How many deferred completions this instance has started. Lets a test
    /// say "the prefix ran" without a clock, which is what makes the *pre*-
    /// cancel `future_was_dropped()` assertion meaningful.
    started: i64,
}

#[bridge]
impl Sensor {
    pub fn new() -> Self {
        Sensor {
            latch: Arc::default(),
            dropped: Arc::new(AtomicBool::new(false)),
            started: 0,
        }
    }

    /// A `Deferred` completion that parks until [`Sensor::release`] — or until
    /// its future is dropped, which is what cancelling the call does.
    ///
    /// `token` rides into the answer (`latch value + token`) so a test that
    /// lets one of these complete can prove the `await` got *this* call's
    /// answer.
    pub fn deferred_until_cancelled(&mut self, token: i64) -> Deferred<i64> {
        self.started += 1;
        let latch = self.latch.clone();
        let sensor = DropSensor(self.dropped.clone());
        Deferred::new(async move {
            let _sensor = sensor;
            latch.wait().await + token
        })
    }

    /// A deferred completion that is already resolved when it is handed over:
    /// the control for "a cancel that arrives too late does nothing".
    pub fn deferred_at_once(&mut self, value: i64) -> Deferred<i64> {
        self.started += 1;
        Deferred::new(async move { value + 1 })
    }

    /// Whether the future of a [`Sensor::deferred_until_cancelled`] call has
    /// been dropped. An actor method because the flag lives in this actor's
    /// own memory — on web, in its worker's wasm instance.
    pub fn future_was_dropped(&self) -> bool {
        self.dropped.load(Ordering::SeqCst)
    }

    /// How many deferred prefixes have run on this instance.
    pub fn started_count(&self) -> i64 {
        self.started
    }

    /// Open the latch: every parked completion resolves with `value`.
    pub fn release(&mut self, value: i64) {
        self.latch.open(value);
    }
}
