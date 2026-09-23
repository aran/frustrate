//! An actor-only bridge: no `#[bridge(sync)]` function anywhere, and every
//! bridged member belongs to an actor object.
//!
//! This shape exists to fence a property of the *generated* file, not of this
//! API: whether a dispatch entry point's `ByteReader` is live depends on the
//! target family, from one emitted file. Natively an actor's calls route
//! through `frustrate_actor_call`, so `frustrate_call_async`'s match has no
//! arms at all (they are `#[cfg(target_family = "wasm")]`); on wasm those same
//! arms are the transport and the reader is live. `frustrate_call_sync` has no
//! arms on either target, because nothing here is sync. See
//! tests/lint_clean/BUILD.bazel.
//!
//! Deliberately minimal — an identity and a counter, no I/O, no threads, no
//! dependencies beyond frustrate — so it stays about the emitted glue.

use frustrate::bridge;

/// An actor purely so that its members dispatch as actor calls.
#[bridge(actor)]
pub struct Counter {
    n: i64,
}

#[bridge]
impl Counter {
    /// Constructor: an actor object's `new` is itself a dispatched call.
    pub fn open() -> Self {
        Counter { n: 0 }
    }

    /// A method taking an argument, so the emitted arm really does read the
    /// request — the entry point's reader being unused must be a fact about
    /// the *target*, not about this fixture having nothing to decode.
    pub fn bump(&mut self, by: i64) -> i64 {
        self.n += by;
        self.n
    }
}
