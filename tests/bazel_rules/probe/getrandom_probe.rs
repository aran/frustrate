//! The whole `ext/getrandom` chain, end to end, as a module that must link.
//!
//! The sibling `platform_flag_probe.rs` fences the *mechanism* — that a
//! platform's `flags` reach a dependency's rustc, and only under that platform.
//! This one fences the **product**: `//ext/getrandom:wasm32` delivers the cfg,
//! getrandom takes its custom backend, `frustrate-getrandom` defines the symbol
//! that backend calls, and `frustrate::random` puts a real host import behind
//! it. Any link in that chain missing is a build failure here.
//!
//! Linking is the assertion. There is no browser in a `build_test`, so what
//! this cannot say — that the bytes are random — is said by
//! `//tests/dart_integration:host_random_web_test`, which drives the same
//! import in Chrome.

// The crate is one `#[no_mangle]` symbol and no callable item, so nothing here
// refers to it and rustc would not link an `--extern` crate it considers
// unused. Whether the dependency edge alone is enough is exactly the sort of
// thing that changes under a compiler upgrade, so the documented spelling is
// used here too rather than relying on the answer.
use frustrate_getrandom as _;

#[no_mangle]
pub extern "C" fn probe_random_u32() -> u32 {
    let mut bytes = [0u8; 4];
    getrandom::fill(&mut bytes).expect("the custom backend is infallible");
    u32::from_ne_bytes(bytes)
}
