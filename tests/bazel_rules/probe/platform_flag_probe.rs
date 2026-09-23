//! Fence for "a consumer platform's `flags` reach a *dependency*'s rustc".
//!
//! This crate stands in for `getrandom` in the recipe
//! `ext/getrandom` gives for entropy on plain wasm32: a hub
//! crate that changes behaviour on a `--cfg` the app must deliver, in the wasm
//! configuration only. The lever is the `platform()`'s own `flags` attribute —
//! `frustrate_wasm_module`'s transition sets `--command_line_option:platforms`
//! and nothing else, precisely so a consumer can carry their own settings that
//! way (bazel/defs.bzl, `_wasm_platform_transition_impl`).
//!
//! What makes this a fence rather than a demonstration is that it is a
//! **dependency**, not the top-level crate. A build setting that reached only
//! the target named on the command line would satisfy a probe compiled at the
//! root and still fail the real case, where the crate that needs the flag is
//! several edges down the graph.
//!
//! The BUILD file builds it twice: once under a platform carrying the flag and
//! once under the stock `//bazel:wasm32`. Each half compiles only if the flag
//! is where it is supposed to be, so "never arrived" and "arrived everywhere"
//! are both build failures rather than a doc that has quietly gone wrong.

#[cfg(all(expect_flag, expect_no_flag))]
compile_error!("both halves of the fence were selected");

#[cfg(not(any(expect_flag, expect_no_flag)))]
compile_error!("neither half of the fence was selected");

#[cfg(expect_flag)]
#[cfg(not(getrandom_backend = "custom"))]
compile_error!(
    "the platform's extra_rustc_flags did not reach this dependency — the \
     entropy recipe in ext/getrandom is broken"
);

#[cfg(expect_no_flag)]
#[cfg(getrandom_backend = "custom")]
compile_error!(
    "a flag from the probe platform reached the stock //bazel:wasm32 build — \
     it is supposed to be scoped to the platform that declares it"
);

/// Something to compile. The fences above are the test; this is only here so
/// the crate is not empty.
pub const FENCED: bool = true;
