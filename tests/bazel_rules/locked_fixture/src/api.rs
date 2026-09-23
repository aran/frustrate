//! The **synchronous locked** half of the block-check surface: a
//! `#[bridge(no_block)]` claim whose body takes and releases a lock guard in
//! the caller's own frame, so the check must scan that release.
//!
//! # Why this is a crate of its own
//!
//! `//tests/bazel_rules`'s fixture is deliberately handle-free — that is what
//! makes it the hardest shape for the generated glue's own lint-clean promise,
//! since `frustrate::handle` and `ContentionError` are emitted there and go
//! unused. A locked handle would use them and quietly retire that fence. And
//! two bridge crates cannot share a Bazel package, because `frustrate_bridge`
//! declares its glue at exactly `src/frustrate_generated.rs` (bazel/defs.bzl).
//!
//! # What it proves that the others do not
//!
//! `fixture_block_check` roots a body that reaches no synchronisation at all;
//! `async_fixture` roots what a *dispatched* member leaves on the caller;
//! `placement_fixture` roots nothing. None of them scans a lock.
//!
//! This one does, and it is the shape the claim used to be refused for. A sync
//! `on_contention = "error"` member acquires with a compare-exchange and
//! releases with another — except when a waiter is queued, where it takes the
//! lock's waiter list, which is a `spin::SpinLock` on this artifact's
//! configuration and cannot execute a wait instruction. That is an argument
//! about `runtime/rust/src/rwlock.rs`; the root below turns it into a scan of
//! the linked module, which is the only form of it that cannot go stale.

use frustrate::bridge;

/// Shared mutable state, so the member below really does take a guard.
#[bridge(locked)]
pub struct Tally {
    count: i64,
}

#[bridge]
impl Tally {
    #[bridge(sync)]
    pub fn new() -> Self {
        Tally { count: 0 }
    }

    /// The claim and the contract together. `no_block` says main-thread Dart
    /// calling this is never stalled; `on_contention = "error"` is what makes
    /// that true rather than hoped for — a contended call returns
    /// `ContentionError` instead of waiting, and the release that follows an
    /// uncontended one reaches no wait instruction either.
    #[bridge(sync, on_contention = "error", no_block)]
    pub fn peek(&self) -> i64 {
        self.count
    }

    /// The dispatched sibling, unclaimed: it exists so the type has a way to
    /// change, and so the check has something to *not* root.
    pub fn bump(&mut self, by: i64) -> i64 {
        self.count = self.count.wrapping_add(by);
        self.count
    }
}
