//! `getrandom`'s custom backend, served by frustrate's host CSPRNG.
//!
//! `getrandom` has no backend for `wasm32-unknown-unknown`. It is not a gap it
//! could fill: with no OS and no wasm-bindgen, there is nothing for it to ask.
//! So it offers a seam instead — build it with
//! `--cfg getrandom_backend="custom"` and it calls a symbol you define — and
//! this crate defines that symbol over [`frustrate::random::fill_bytes`], which
//! is `crypto.getRandomValues` through an import frustrate's glue already
//! serves.
//!
//! That unblocks `rand`, `uuid`, `ahash`, `ring` and everything else that
//! generates an id or a key, on the plain-`wasm32` column, with no custom std
//! and no wasm-bindgen.
//!
//! # Using it
//!
//! Depend on this crate and name it once, then select the platform that carries
//! the `cfg`:
//!
//! ```ignore
//! // in your bridge crate's lib.rs — see "Why the `use`" below
//! use frustrate_getrandom as _;
//! ```
//!
//! ```python
//! frustrate_wasm_module(
//!     name = "my_rust.wasm",
//!     crate = "//bridge:my_rust",
//!     platform = "@frustrate//ext/getrandom:wasm32",   # or :wasm32_threads
//! )
//! ```
//!
//! Off wasm this crate is empty, so the dependency needs no `cfg` in your
//! manifest and the native build keeps `getrandom`'s own OS backend — which is
//! the one you want there, and the reason the `cfg` belongs to a *platform*
//! rather than to a crate annotation (`BUILD.bazel` beside this crate).
//!
//! Forget the platform and `getrandom` stops the build itself, with advice that
//! is right for the ecosystem and wrong for you: *"you may need to enable the
//! `wasm_js` configuration flag"* is the wasm-bindgen route, which is the
//! thing this crate exists to avoid. Select the platform instead.
//!
//! # One definition covers both getrandom majors
//!
//! A real graph carries two: `rand` is on 0.3 while `automerge` is on 0.4, and
//! cargo keeps them side by side. They resolve the custom backend to the same
//! symbol on purpose, so this one definition satisfies both — it is named
//! `__getrandom_v03_custom` in 0.4 as well, for exactly that reason. Which
//! major this crate compiles *against* decides only whose `Error` type spells
//! the signature.
//!
//! # Why the `use`
//!
//! The whole crate is one `#[no_mangle]` symbol and no callable item, so
//! nothing in your code refers to it — and rustc links an `--extern` crate only
//! when it is used. A dependency edge alone can therefore leave the symbol out
//! and the link fails on `undefined symbol: __getrandom_v03_custom`, naming a
//! crate you did in fact depend on. `use frustrate_getrandom as _;` is the
//! ecosystem's spelling for "link this, I have no name for it".
//!
//! Loud rather than silent, at least: the failure is a link error, not entropy
//! that quietly returns zero.

// Nothing off wasm, deliberately: `getrandom` has a working OS backend there
// and this crate has no business displacing it. Empty rather than absent so a
// consumer's manifest needs no `cfg` of its own.
#[cfg(target_family = "wasm")]
mod wasm {
    /// The seam `getrandom` calls under `--cfg getrandom_backend="custom"`.
    ///
    /// # Safety
    ///
    /// `getrandom`'s contract: `dest` is writable for `len` bytes. The host
    /// fills exactly that many or aborts — it never returns short, which is why
    /// this is infallible and why `Ok(())` is not optimism.
    #[no_mangle]
    unsafe extern "Rust" fn __getrandom_v03_custom(
        dest: *mut u8,
        len: usize,
    ) -> Result<(), getrandom::Error> {
        frustrate::random::fill_bytes(std::slice::from_raw_parts_mut(dest, len));
        Ok(())
    }
}
