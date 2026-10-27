//! One of the two bridges //compose links into one library.

use frustrate::bridge;
use std::sync::atomic::{AtomicI64, Ordering};

static TAPS: AtomicI64 = AtomicI64::new(0);

/// Counts taps in a static, which a hot patch keeps.
#[bridge(sync)]
pub fn ping_tap() -> i64 {
    TAPS.fetch_add(1, Ordering::SeqCst) + 1
}

/// What the app shows for this bridge.
#[bridge(sync)]
pub fn ping_label(taps: i64) -> String {
    format!("ping: {taps} taps")
}
