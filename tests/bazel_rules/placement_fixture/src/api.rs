//! The **placement** half of the block-check surface: a bridge whose every
//! `#[bridge(no_block)]` claim is settled by where the body runs, so the check
//! builds no wasm module at all.
//!
//! # Why this is a crate of its own
//!
//! Being all-placement is a property of the whole claim set, so it cannot be
//! one more member in a crate that also claims a free function — and two bridge
//! crates cannot share a Bazel package, because `frustrate_bridge` declares its
//! glue at exactly `src/frustrate_generated.rs` (bazel/defs.bzl).
//!
//! # Why it is the only block-check fixture in `bazel test //...`
//!
//! The other two need the locally built +atomics std and are `manual`. This one
//! needs no wasm toolchain, because the shape it exercises is exactly the one
//! that builds nothing: the census says every claim is placed, and
//! `wasm_block_check --claims-only` says so back. That makes the claims-only
//! path the *only* part of this gate a wildcard-only machine ever runs — which
//! is the point, since it is also the path a bridge like `e2e/iroh_demo` (whose
//! check artifact cannot be built at all) depends on entirely.
//!
//! The red direction cannot be a Bazel target — Bazel has no way to assert that
//! an action fails — so it is covered by //bazel/wasm_block_check's own unit
//! tests: a claims-only target handed a claim that needs a root refuses by
//! name, telling the author to add `crate =`.

use frustrate::bridge;

/// An actor, so its methods run on its own executor: a dedicated thread
/// natively, a dedicated Worker on web.
#[bridge(actor)]
pub struct Digger {
    depth: i64,
}

/// The claim, on every method at once. It is true by *placement* rather than by
/// what the bodies do — `no_block` says main-thread Dart calling these cannot
/// be stalled, and main-thread Dart only serializes bytes and posts them: the
/// wait is on the actor's own executor, where a wait is legal. Codegen emits no
/// check root for any of them, which is what makes this crate's whole claim
/// set artifact-free.
#[bridge(no_block)]
impl Digger {
    pub fn new() -> Self {
        Digger { depth: 0 }
    }

    pub fn dig(&mut self, by: i64) -> i64 {
        self.depth = self.depth.wrapping_add(by);
        self.depth
    }
}
