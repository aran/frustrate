//! One of the two bridges `//tests/bazel_rules/linked_fixture:linked` links.

use frustrate::bridge;

/// Claimed, and true on every platform: `wrapping_add` waits on nothing.
#[bridge(sync, no_block)]
pub fn add(a: i32, b: i32) -> i32 {
    a.wrapping_add(b)
}

/// Not claimed, and the one caller of [`slow`], so that the marker the check
/// target names exists in the native program (a native analysis refuses a
/// `slow` path that names no function).
#[bridge(sync)]
pub fn sub(a: i32, b: i32) -> i32 {
    slow();
    a.wrapping_sub(b)
}

/// This bridge's `slow` marker.
#[bridge(skip)]
#[inline(never)]
pub fn slow() {}
