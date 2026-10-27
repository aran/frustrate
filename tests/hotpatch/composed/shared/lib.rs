//! A counter both bridges of the composed fixture reach.

use std::sync::atomic::{AtomicI64, Ordering};

static SHARED: AtomicI64 = AtomicI64::new(0);

/// Add `n` and answer the new value.
pub fn add(n: i64) -> i64 {
    SHARED.fetch_add(n, Ordering::SeqCst) + n
}

pub fn get() -> i64 {
    SHARED.load(Ordering::SeqCst)
}
