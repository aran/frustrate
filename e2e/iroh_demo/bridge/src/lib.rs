//! The bridge crate. `api.rs` is the surface; everything else is behind it.
//!
//! The two implementation modules are mutually exclusive and never both
//! compiled: `api.rs` picks one as `imp` on the same `cfg`. Only `api.rs` is in
//! `//bridge:codegen`'s `srcs`, so frustrate parses nothing here.

pub mod api;

#[cfg(not(target_family = "wasm"))]
mod node;
#[cfg(target_family = "wasm")]
mod stub;

// Written by frustrate codegen as a declared Bazel action.
mod frustrate_generated;
