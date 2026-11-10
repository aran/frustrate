//! Tiny self-contained bridge fixture for the Bazel rule tests.
//!
//! Two `#[bridge(sync)]` functions are enough to exercise the whole pipeline
//! the rules wire up — codegen scans this file, emits the Rust glue and Dart
//! bindings, and the generated glue compiles into the wasm module. Kept
//! deliberately minimal (no async, no streams, no handles) so these tests
//! stay about the *rules*, not the surface, and never depend on
//! tests/test_api.

use frustrate::bridge;

/// `no_block` is here for `//tests/bazel_rules:fixture_no_block_check`, and the
/// claim is true rather than convenient: `wrapping_add` reaches no
/// synchronisation primitive on any platform.
///
/// It costs the other fixtures nothing. The field is `#[serde(skip)]` in the
/// IR, so the wire fingerprint does not move, and the root it emits is
/// `#[cfg(frustrate_no_block_check)]`, which no shipped build sets — measured:
/// `fixture.wasm`, `fixture_wasi.wasm` and `fixture_threaded.wasm` are
/// byte-identical across adding it.
#[bridge(sync, no_block)]
pub fn add(a: i32, b: i32) -> i32 {
    a.wrapping_add(b)
}

/// Not claimed, and the one caller of [`slow`], so that the marker
/// `//tests/bazel_rules:fixture_no_block_check` names exists in the native
/// program: the native check refuses a `slow` path that names no function.
#[bridge(sync)]
pub fn sub(a: i32, b: i32) -> i32 {
    slow();
    a.wrapping_sub(b)
}

/// `//tests/bazel_rules:fixture_no_block_check`'s `slow` marker.
#[bridge(skip)]
#[inline(never)]
pub fn slow() {}

/// The unit-test half of `frustrate_bridge_library`'s contract, and the
/// contrast that makes the integration test in `tests/exported_abi.rs` mean
/// something. This one runs *inside* the crate (`rust_test(crate = …)`), so it
/// can name the generated module by path — `mod frustrate_generated` is private
/// to the crate root. The integration test is a separate crate and can reach
/// the very same function only as an exported symbol.
#[cfg(test)]
mod tests {
    #[test]
    fn the_generated_module_is_reachable_from_inside_the_crate() {
        assert_eq!(super::add(2, 3), 5);
        // Private to the crate, so this line is what an integration test
        // cannot write — it has to go through the C ABI instead.
        assert_ne!(
            crate::frustrate_generated::frustrate_rule_test_fixture_schema_hash(),
            0
        );
    }
}
