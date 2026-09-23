pub mod api;

// Written by frustrate codegen (the frustrate_bridge action under Bazel);
// rules_rust unifies it with this checked-in src/ by path, so the crate root
// can `mod frustrate_generated;`.
mod frustrate_generated;

// The dispatch ids, for a consumer calling the exported ABI directly rather
// than through generated bindings — here `tests/exported_abi.rs`, which sees
// this crate the way any dependent does. Dispatch ids are derived from each
// member's wire facts, so a caller cannot name a member without this; the
// dispatcher itself is reached as a `#[no_mangle]` symbol, which needs no
// re-export.
pub use frustrate_generated::FRUSTRATE_FN_IDS;
