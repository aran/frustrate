//! Entropy for wasm32-unknown-unknown, answered by the host's CSPRNG.
//!
//! Installed by toolchain/custom_std as `sys/random/frustrate.rs`. It replaces
//! `sys/random/unsupported.rs`, which does two different unhelpful things:
//!
//! * `fill_bytes` is `panic!("this target does not support random data
//!   generation")` — loud, and the reason std's own entropy (the `HashMap`
//!   seed below, and anything else routed through `sys::random`) is unusable
//!   on this target.
//!
//!   Not the reason the **`getrandom` crate** is: getrandom never asks std, it
//!   picks a backend of its own and has none for `wasm32-unknown-unknown`
//!   without either wasm-bindgen or a custom backend. This facility does not
//!   change that, and installing it in the hope that it will is the mistake
//!   worth naming here. `ext/getrandom` is the backend that does serve it —
//!   built on the same `fill_random` import, which the
//!   glue supplies whether or not a module was built against this std.
//! * `hashmap_random_keys` derives `HashMap`'s seed from **allocation
//!   addresses**, under std's own comment that this "isn't particularly secure,
//!   but there isn't really an alternative". In a deterministic wasm module
//!   those addresses are close to predictable, so every `HashMap` in the
//!   process is seeded from a guessable value — quietly. In a browser there
//!   *is* an alternative, which is the more interesting half of this facility:
//!   it fixes a silent weakness rather than lifting a panic.
//!
//! The host serves this from `crypto.getRandomValues`, a CSPRNG. It caps at
//! 65536 bytes per call, so the host side chunks; that is invisible here.

#[link(wasm_import_module = "frustrate")]
unsafe extern "C" {
    /// Fill `len` bytes at `ptr` from the host CSPRNG. Infallible: the host
    /// aborts rather than return short, because a partially filled buffer
    /// silently weakens every caller.
    safe fn fill_random(ptr: *mut u8, len: usize);
}

pub fn fill_bytes(bytes: &mut [u8]) {
    if bytes.is_empty() {
        return;
    }
    fill_random(bytes.as_mut_ptr(), bytes.len());
}

pub fn hashmap_random_keys() -> (u64, u64) {
    let mut buf = [0u8; 16];
    fill_bytes(&mut buf);
    let k1 = u64::from_ne_bytes(buf[..8].try_into().unwrap());
    let k2 = u64::from_ne_bytes(buf[8..].try_into().unwrap());
    (k1, k2)
}
