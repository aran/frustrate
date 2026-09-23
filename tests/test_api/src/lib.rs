pub mod api;
/// The deferred-cancel fixture. A second bridge source rather than more of
/// `api`, because it is the one fixture whose evidence is a `Drop` — keeping
/// the sensor, the future that holds it and the actor method that reads it in
/// one file is what makes it readable, and it exercises the multi-source
/// bridge (`--src path:module` twice) that nothing else in the repo does.
pub mod cancel_api;
/// Generated shape coverage: one member per emitted code path that nothing
/// else in this crate instantiates
/// (`cargo run -p shape-coverage -- --write-fixture`). Regenerate, never edit.
pub mod shapes;

// Written by frustrate codegen: build.rs under cargo, a declared action under
// Bazel. Gitignored in the cargo flow.
mod frustrate_generated;

/// Hostile requests, derived from the interface and driven at the generated
/// dispatch under Miri. Test-only, and cargo-only: it reads the IR JSON that
/// this crate's `build.rs` writes, which the Bazel flow does not produce and
/// (building no test target here) never asks for.
#[cfg(test)]
mod hostile;
