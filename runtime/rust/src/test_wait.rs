//! Waiting for another thread in a test: a [`Count`] the test owns the signal
//! of, and [`wait_until`] for state it does not.
//!
//! Both are bounded by a deadline, never an iteration count. A count of
//! `yield_now` calls bounds spins rather than elapsed time, and those diverge
//! in exactly the wrong direction: the busier the machine, the less wall-clock
//! a fixed number of spins buys, so load makes a false failure more likely.
//! The deadline is generous because its only job is to make a genuine hang
//! report instead of wedging the suite.

use std::sync::{Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

const DEADLINE: Duration = Duration::from_secs(10);

/// A count that other threads raise (a `Drop` that ran, a thread that exited)
/// and the test blocks on.
pub(crate) struct Count {
    n: Mutex<usize>,
    raised: Condvar,
}

impl Count {
    pub(crate) const fn new() -> Self {
        Count { n: Mutex::new(0), raised: Condvar::new() }
    }

    pub(crate) fn add(&self) {
        *self.n.lock().unwrap_or_else(PoisonError::into_inner) += 1;
        self.raised.notify_all();
    }

    pub(crate) fn get(&self) -> usize {
        *self.n.lock().unwrap_or_else(PoisonError::into_inner)
    }

    #[cfg_attr(frustrate_manual_scheduler, allow(dead_code))]
    pub(crate) fn reset(&self) {
        *self.n.lock().unwrap_or_else(PoisonError::into_inner) = 0;
    }

    /// Block until the count reaches `want`, or fail naming what never
    /// happened.
    pub(crate) fn wait_for(&self, want: usize, what: &str) {
        let guard = self.n.lock().unwrap_or_else(PoisonError::into_inner);
        let (guard, _) = self
            .raised
            .wait_timeout_while(guard, DEADLINE, |n| *n < want)
            .unwrap_or_else(PoisonError::into_inner);
        assert!(*guard >= want, "{what}: expected {want}, still {}", *guard);
    }
}

/// Poll until `cond` holds, or fail with `what`. For state the test cannot
/// signal itself, such as another thread's registry.
pub(crate) fn wait_until(cond: impl Fn() -> bool, what: &str) {
    let deadline = Instant::now() + DEADLINE;
    while !cond() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}
