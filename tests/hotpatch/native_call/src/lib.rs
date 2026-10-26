//! The hot patch test's bridge crate. Each sibling package holds one edit of
//! `src/api.rs`; this file is the same in all of them.

pub mod api;

mod frustrate_generated;

unsafe extern "C" {
    /// From `//tests/hotpatch:native_dep`, a C archive the library links.
    pub fn hotpatch_native_dep(x: i32) -> i32;
}

/// Keeps the C function in the launch image, as a bridge's own calls into a
/// wrapped C library do.
pub fn native_dep(x: i32) -> i32 {
    unsafe { hotpatch_native_dep(x) }
}
