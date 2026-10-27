//! Bridge A of the composed hot patch fixture. Each sibling package named
//! `alpha*` holds one edit of `src/api.rs`; this file is the same in all of them.

pub mod api;

mod frustrate_generated;
