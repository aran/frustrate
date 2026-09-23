//! A bridge whose implementation genuinely does not exist on wasm.
//!
//! This fixture exists to prove *absence*, which no unit test over emitted
//! strings can. Every item below that is `#[bridge(native_only)]` is also
//! `#[cfg(not(target_family = "wasm"))]`-gated, so for wasm32 those Rust items
//! are not compiled at all. The wasm `build_test` in
//! tests/native_only/BUILD.bazel therefore *cannot* pass unless codegen left
//! the wasm module with no reference to any of them — not in the dispatch
//! table, not in a glue fn, not in a drop or finalize export. A comment-out, or
//! a gate applied to the fn but not its dispatch arm, fails to resolve and the
//! build dies at `E0425` pointing into generated code — the original failure
//! this attribute was added to prevent.
//!
//! Codegen never evaluates the cfgs — it parses this file once and emits both
//! surfaces — so the `#[bridge(native_only)]` declarations are what carry the
//! fact. That is the whole point: the cfg is for rustc, the declaration is for
//! codegen, and before `native_only` existed there was no way to write the
//! second one — a `#[cfg]`-gated member was emitted into both surfaces
//! regardless, and the wasm build died inside generated code.
//!
//! The gates stand in for the real case (a dependency that does not build for
//! wasm32) without needing one. A native-only dependency is a build-graph fact;
//! what codegen has to get right is identical either way.

use frustrate::bridge;

/// The portable half. Something must survive into the wasm module, or the
/// build would prove nothing about what was removed.
#[bridge(sync)]
pub fn double(x: i64) -> i64 {
    x * 2
}

/// Declared native-only as a member, with no structural reason to be — no
/// blocking contract, no value-returning `DartFunction`. Before
/// `#[bridge(native_only)]` this could not be said at all, and the only way to
/// get the same absence was a phantom returning callback the member never
/// called.
#[cfg(not(target_family = "wasm"))]
#[bridge(sync, native_only)]
pub fn platform_tag() -> String {
    std::env::consts::OS.to_string()
}

/// Native-only as a *type*: the class, its members and both drop exports go
/// with it.
#[cfg(not(target_family = "wasm"))]
#[bridge(frozen, native_only)]
pub struct NativeThing {
    n: i64,
}

#[cfg(not(target_family = "wasm"))]
#[bridge(native_only)]
impl NativeThing {
    #[bridge(sync)]
    pub fn open(n: i64) -> Self {
        NativeThing { n }
    }

    /// Carries its own `#[bridge(...)]`, which for every other option would
    /// discard the block-level attribute. `native_only` is OR-merged instead,
    /// so this method cannot come out more portable than the impl around it.
    #[bridge(sync)]
    pub fn get(&self) -> i64 {
        self.n
    }
}

/// A free function that only *names* the native-only type, and says nothing
/// itself. It is native-only because `NativeThing` is — the propagation that
/// keeps an unannotated function from emitting web glue for a type the wasm
/// module does not have. It is also why FR0034 does not fire on it despite the
/// `#[cfg]`: it is already native-only, so the gate is declared, just not here.
#[cfg(not(target_family = "wasm"))]
#[bridge(sync)]
pub fn describe(t: &NativeThing) -> i64 {
    t.get()
}
