//! One `#[bridge(no_block)]` claim over the **iroh** crate graph, so the gate
//! runs against a graph carrying wasm-bindgen's exports rather than only
//! against frustrate's own fixtures.
//!
//! It is a crate of its own because every claim in `//bridge` is settled by
//! *placement* — its actor's methods run on the actor's Worker, its one free
//! function is handed to the pool — so none of them is rooted, no module is
//! built and nothing is scanned there. A `#[bridge(sync)]` claim is the one
//! kind whose body a root reaches, so it is what exercises the artifact path at
//! all. Nothing here ships: this crate is in no app, no plugin and no
//! platform's build.
//!
//! Two bridge crates also cannot share a Bazel package — `frustrate_bridge`
//! declares its glue at exactly `src/frustrate_generated.rs`.

use frustrate::bridge;
use iroh::SecretKey;

/// The endpoint id a 32-byte secret expands to: iroh's identity derivation, run
/// on the caller's thread.
///
/// `sync` is load-bearing, not a performance choice. It is what makes the body
/// run in the entry frame on the calling thread, which is the only member kind
/// whose whole body a check root reaches — and reaching this body over the iroh
/// graph is the entire point of the fixture. A `#[bridge]` member here would be
/// settled by placement and build no module at all.
///
/// A real call into iroh rather than arithmetic that happens to sit in a crate
/// linking it — `SecretKey::public` is an ed25519 scalar multiplication and
/// `to_z32` the pkarr encoding identities print in — so a clean scan says
/// something about iroh's code.
///
/// Every function on this path is **infallible**, and that is the claim rather
/// than a detail: iroh's error type builds an `n0_error::Meta`, whose
/// `backtrace_enabled` is a `OnceLock`, so a body that can construct one
/// reaches `Once::call`. BUILD.bazel has the witness chain.
///
/// That bounds what iroh can offer a thread that runs a body *itself*, not what
/// an app can do with iroh: waiting is legal anywhere else. `//bridge` is the
/// shape that follows — its ticket parse is a dispatched member and its
/// connection work an actor's — so this fixture's infallible body is a property
/// of the claim being `sync`, not a restriction the demo also had to live with.
///
/// The seed is an `i64` spread over the 32 bytes rather than a `Vec<u8>`, to
/// keep the scan about iroh's code and not the bridge's buffer handling.
#[bridge(sync, no_block)]
pub fn endpoint_id_for_seed(seed: i64) -> String {
    let mut bytes = [0u8; 32];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = (seed as u64).wrapping_mul(i as u64 + 1).to_le_bytes()[i % 8];
    }
    SecretKey::from_bytes(&bytes).public().to_z32()
}
