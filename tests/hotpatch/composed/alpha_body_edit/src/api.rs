//! Both bodies edited.

use frustrate::bridge;
use std::sync::atomic::{AtomicI64, Ordering};

static CALLS: AtomicI64 = AtomicI64::new(0);

const STEP: i64 = 10;

#[bridge(sync)]
pub fn add(a: i64, b: i64) -> i64 {
    a + b + 1000
}

/// Counts calls in a static that a patch must share with the launched code.
#[bridge(sync)]
pub fn count() -> i64 {
    CALLS.fetch_add(1, Ordering::SeqCst) + 100
}

/// Bumps the counter both bridges reach.
#[bridge(sync)]
pub fn bump() -> i64 {
    hotpatch_composed_shared::add(STEP)
}
