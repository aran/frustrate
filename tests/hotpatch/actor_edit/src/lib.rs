//! The hot patch test's bridge crate. Each sibling package holds one edit of
//! `src/api.rs`; this file is the same in all of them.

pub mod api;

mod frustrate_generated;
