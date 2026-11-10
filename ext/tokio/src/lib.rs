//! tokio's runtime, lent to frustrate's cooperative executor.
//!
//! A bridged `async fn` runs on frustrate's executor, which has no reactor, so
//! a tokio leaf (`sleep`, a socket, a channel) panics at construction unless
//! the executor is lent a runtime's context. [`set_runtime`] lends it, around
//! every poll.
//!
//! ```ignore
//! // once, at init; native only
//! frustrate_tokio::set_runtime(RT.handle().clone());
//! ```
//!
//! # What it buys over `Handle::enter`
//!
//! The recipe `frustrate::runtime::set_context` documents for any runtime is
//! `let _guard = handle.enter(); poll.run();`. For tokio that sets the
//! context but does not mark the thread as inside a runtime, so a
//! `block_on` or `blocking_recv` in a bridged `async fn` blocks the
//! executor's drain and nothing reports it.
//!
//! This crate wraps each poll as `handle.block_on(async { poll.run() })`
//! instead, which does mark it. Then a nested `block_on` panics ("Cannot
//! start a runtime from within a runtime"), `blocking_recv` panics ("Cannot
//! block the current thread from within a runtime"), and the panic is
//! attributed to the call like any other. Leaf futures work as before.
//!
//! Costs: a poll costs more than under `enter`, and tokio's cooperative
//! budget applies inside a poll.
//!
//! # Using it
//!
//! Depend on this crate and point it at your own tokio (`BUILD.bazel` beside
//! this file). The runtime must be driven independently: set a
//! multi-thread runtime's handle, or a current-thread one that a thread of
//! yours is running. The setting is process-global and the last one wins.
//!
//! Off native this crate is empty: wasm has no tokio reactor to lend, and
//! `frustrate::runtime::set_context` does not exist there.

#[cfg(not(target_family = "wasm"))]
pub use native::set_runtime;

#[cfg(not(target_family = "wasm"))]
mod native {
    use tokio::runtime::Handle;

    /// Set `handle`'s runtime as the context of every poll on frustrate's
    /// executor. Replaces any earlier one.
    pub fn set_runtime(handle: Handle) {
        frustrate::runtime::set_context(move |poll| handle.block_on(async move { poll.run() }));
    }
}
