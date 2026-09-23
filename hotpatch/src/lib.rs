//! frustrate-hotpatch: build patches for a running bridge library.
//!
//! A library compiled with `--cfg=frustrate_hot_patch` routes its generated
//! entry points through slots a loaded patch can redirect. This crate builds
//! such a patch from the LLVM IR of an edited compilation: it finds what
//! changed against the launch build, decides whether the change can be
//! patched at all, reduces the module to what must be replaced, binds
//! everything else to the running image, and links the result. The CLI in
//! `main.rs` is what a `*.hot_patch.json` manifest runs.

pub mod contract;
pub mod debuginfo;
pub mod image;
pub mod ir;
pub mod link;
pub mod patch;
pub mod state;
