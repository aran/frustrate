//! The other bridge `//tests/bazel_rules/linked_fixture:linked` links.

use frustrate::bridge;
use std::sync::Mutex;

static TOTAL: Mutex<i64> = Mutex::new(0);

/// Claimed, and the same member as `example`'s: a fn id comes from a member's
/// own wire facts, so both bridges number it alike.
#[bridge(sync, no_block)]
pub fn add(a: i32, b: i32) -> i32 {
    a.wrapping_add(b)
}

/// Takes the `Mutex` itself, silenced by each analysis's name for the wait. The
/// silence is this bridge's: `example` has no `Mutex`, and its members are
/// numbered from the same 0.
#[bridge(sync, no_block(allow(
    pthread_mutex_lock,
    syscall::SYS_futex::FUTEX_WAIT_BITSET,
    std::sys::sync::mutex::futex::Mutex::lock_contended,
)))]
pub fn read() -> i64 {
    *TOTAL.lock().unwrap()
}

/// Not claimed, and the one caller of [`slow`].
#[bridge(sync)]
pub fn write(v: i64) -> i64 {
    slow();
    *TOTAL.lock().unwrap() = v;
    v
}

/// This bridge's `slow` marker.
#[bridge(skip)]
#[inline(never)]
pub fn slow() {}
