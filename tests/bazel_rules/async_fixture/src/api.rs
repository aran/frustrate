//! The **async** half of the block-check fixture surface: one
//! `#[bridge(no_block)]` `async fn`, and nothing else.
//!
//! # Why this is a crate of its own rather than a second member next door
//!
//! `//tests/bazel_rules:fixture_block_check` proves the claim on a *sync*
//! member, and its crate cannot simply grow an async one. `fixture_shared`'s
//! shape — sync-only, handle-free — is half of //tests/lint_clean's contract,
//! and the half nothing else covers: it is the only fixture in which
//! `frustrate_call_async`'s `ByteReader` is dead on **wasm** as well as on
//! native. That is anyone's first bridge, and the shape the generated file's
//! unconditional `#[allow]`s have to keep warning-free under `-Dwarnings`. One
//! `async fn` over there makes that reader live and the coverage disappears
//! without anything going red.
//!
//! Two bridge crates also cannot share a Bazel package: `frustrate_bridge`
//! declares its glue at exactly `src/frustrate_generated.rs`, one per package
//! by construction (bazel/defs.bzl).
//!
//! What this package covers that the sync fixture cannot is the settlement of a
//! **dispatched** member: `tick`'s claim is settled by placement with no root
//! at all, and `stamp`'s by a *residue* root — one holding the caller-side
//! slice of the member and nothing of its body. Both rows in one census also
//! make this the only Bazel target whose report has to word two different
//! greens.

use frustrate::bridge;

/// A claimed `async fn` with nothing left on the caller: scalar in, scalar out,
/// no handle handed back. Its census row is `placement-dispatch` and it gets no
/// check root — `executor::spawn` constructs the future on the calling thread
/// and hands every poll to the pool on threaded web, and single-threaded web
/// has no wait instruction to execute.
///
/// The `.await` is load-bearing rather than decoration: suspending is what
/// makes the *later* polls, and the drop of a cancelled future, happen on a
/// drain rather than in the call — which is the half of dispatch placement that
/// a straight-line body would not exercise at all.
#[bridge(no_block)]
pub async fn tick(n: i64) -> i64 {
    YieldOnce::pending_once().await;
    n.wrapping_add(1)
}

/// Stands in for a prost-generated message: crosses as bytes through a codec
/// the bridge author wrote.
#[bridge(bytes(dart = "Plan", import = "package:async_claim_fixture/plan.dart"))]
pub struct PlanMsg {
    pub revision: i64,
}

impl frustrate::BytesCodec for PlanMsg {
    fn to_bytes(&self) -> Vec<u8> {
        self.revision.to_le_bytes().to_vec()
    }

    /// The residue this fixture exists for, and the reason it is *user* code
    /// that matters: this runs on whichever thread called the member, before
    /// anything reaches the pool. A lock taken here stalls that thread.
    fn from_bytes(bytes: &[u8]) -> Self {
        PlanMsg {
            revision: i64::from_le_bytes(bytes[..8].try_into().unwrap_or_default()),
        }
    }
}

/// The same claim with a **residue**: the body is dispatched exactly as
/// `tick`'s is, but `spawn_N_stamp`'s prelude runs `PlanMsg::from_bytes` on the
/// calling thread first. So this one is rooted — at that decode edge, and at
/// nothing else. BUILD.bazel has the sabotage and the chain it produces.
#[bridge(no_block)]
pub async fn stamp(p: PlanMsg) -> i64 {
    YieldOnce::pending_once().await;
    p.revision.wrapping_add(1)
}

/// Returns `Pending` on its first poll (waking itself) and `Ready` on the
/// second, so `tick` really does suspend.
///
/// Written out here rather than shared with `tests/test_api`: this crate
/// deliberately depends on nothing but `frustrate`, so the fixture stays about
/// the rule and cannot be perturbed by the integration surface.
struct YieldOnce {
    yielded: bool,
}

impl YieldOnce {
    fn pending_once() -> Self {
        YieldOnce { yielded: false }
    }
}

impl std::future::Future for YieldOnce {
    type Output = ();
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        if self.yielded {
            std::task::Poll::Ready(())
        } else {
            self.yielded = true;
            // Self-driving: arrange the next poll before yielding.
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    }
}
