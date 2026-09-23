//! Entropy from the host's CSPRNG — **wasm only**, and deliberately so.
//!
//! On `wasm32-unknown-unknown` there is no OS behind the module, so nothing in
//! a stock std can produce a random byte: `sys::random::fill_bytes` panics, and
//! the `getrandom` crate (and therefore `uuid`, `rand`, `ahash`, `ring`)
//! refuses to compile at all without either wasm-bindgen or a backend supplied
//! from outside. This is the byte source frustrate already has, made reachable
//! from Rust so a bridge does not have to hand-declare a host import to get at
//! it.
//!
//! [`fill_bytes`] is `crypto.getRandomValues`, through the `frustrate`
//! namespace. **The glue serves it unconditionally at every instantiation site**
//! — main instance, pool worker, actor worker — whatever std the module was
//! built against and whichever wasm platform produced it
//! (`runtime/dart/lib/src/js/frustrate.js`, `stdFacilityImports`, and the three
//! call sites in `runtime_web.dart` / that file). So this needs no custom std,
//! no wasm-bindgen, no `platform` change and no configuration; a module that
//! never calls it declares no import and pays nothing.
//!
//! # There is no native counterpart, and that is the point
//!
//! Native already has a working answer — `getrandom` uses the OS there — and
//! frustrate has none to offer that would be better. Providing one would mean
//! either taking a dependency this crate does not have or writing the first
//! OS-specific code in the runtime (`getentropy`, `BCryptGenRandom`), to
//! reimplement what the caller's existing crate already does correctly. So
//! this module is compile-absent off wasm, in the same spirit as
//! [`crate::runtime::register`] being absent *on* it: the gap is where the
//! platform actually differs.
//!
//! A caller who wants `getrandom` (and so `rand`, `uuid`, `ahash`, `ring`)
//! rather than raw bytes does not build on this directly: `frustrate-getrandom`
//! in [`ext/`](../../../docs/ext.md) is that join, and it is where the `cfg`
//! and the platform that carries it are worked out.

#[link(wasm_import_module = "frustrate")]
extern "C" {
    /// Fill `len` bytes at `ptr` from the host CSPRNG. Infallible: the host
    /// aborts rather than return short, because a partially filled buffer
    /// silently weakens every caller. It chunks at 65536 bytes a call
    /// (`crypto.getRandomValues`' own cap), which is invisible here.
    fn fill_random(ptr: *mut u8, len: usize);
}

/// Fill `bytes` from the host CSPRNG.
///
/// ```ignore
/// let mut key = [0u8; 32];
/// frustrate::random::fill_bytes(&mut key);
/// ```
///
/// Infallible, and never short: the host's contract is that it fills the whole
/// buffer or aborts. Callable from anywhere — it is a synchronous import with
/// no lock and no wait, so a `#[bridge(sync)]` body on the browser main thread
/// may use it.
pub fn fill_bytes(bytes: &mut [u8]) {
    // The empty case never reaches the host. `as_mut_ptr` on an empty slice is
    // a dangling-but-aligned pointer, and the host would build a typed-array
    // view over it — a zero-length view at an out-of-bounds offset is a
    // `RangeError`, not a no-op. Nothing is lost: there are no bytes to fill.
    if bytes.is_empty() {
        return;
    }
    // SAFETY: `ptr`/`len` describe a live, exclusively borrowed slice of this
    // module's own linear memory for the duration of the call. The host writes
    // exactly `len` bytes there and reads nothing.
    unsafe { fill_random(bytes.as_mut_ptr(), bytes.len()) };
}
