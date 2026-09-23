//! Fence for `//bazel:wasm_build`.
//!
//! The BUILD file turns the setting into one of two `--cfg`s. This file
//! compiles only if exactly one of them arrived, so a setting that never
//! matched, or matched everywhere, is a build failure rather than a silently
//! wrong `select()`.

#[cfg(all(expect_wasm, expect_native))]
compile_error!("both branches of the select applied");

#[cfg(not(any(expect_wasm, expect_native)))]
compile_error!("//bazel:wasm_build resolved to neither branch");

/// Under the wasm transition, and only there.
#[cfg(expect_wasm)]
#[no_mangle]
pub extern "C" fn probe_is_wasm() -> i32 {
    const {
        assert!(cfg!(target_family = "wasm"), "wasm branch on a non-wasm target");
    }
    1
}

/// Untransitioned, and only there. The negative half: a setting that matched
/// every configuration would take the wasm branch here too, and this would
/// never be compiled.
#[cfg(expect_native)]
#[no_mangle]
pub extern "C" fn probe_is_native() -> i32 {
    const {
        assert!(!cfg!(target_family = "wasm"), "default branch on a wasm target");
    }
    0
}
