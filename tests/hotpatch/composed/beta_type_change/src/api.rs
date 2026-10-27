//! A bridged type gains a field: its members keep their ids while its
//! encoding moves.

use frustrate::bridge;
use std::sync::atomic::{AtomicI64, Ordering};

static CALLS: AtomicI64 = AtomicI64::new(0);

#[bridge(data)]
pub struct Pair {
    pub a: i64,
    pub b: i64,
}

#[bridge(sync)]
pub fn mul(a: i64, b: i64) -> i64 {
    a * b
}

/// Counts calls in a static of its own, apart from bridge A's.
#[bridge(sync)]
pub fn count() -> i64 {
    CALLS.fetch_add(1, Ordering::SeqCst) + 1
}

#[bridge(sync)]
pub fn first(p: Pair) -> i64 {
    p.a
}

/// The counter bridge A bumps, read through this bridge.
#[bridge(sync)]
pub fn shared() -> i64 {
    hotpatch_composed_shared::get()
}
