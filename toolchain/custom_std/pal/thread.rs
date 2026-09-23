//! `available_parallelism` and `thread::sleep` for wasm32-unknown-unknown.
//!
//! Installed by toolchain/custom_std as `sys/thread/frustrate.rs`.
//!
//! **These two ship together, and that is a limitation rather than a
//! preference.** Both are selected by one `cfg_select!` arm in
//! `sys/thread/mod.rs`, and that arm is a single block: two facilities editing
//! it independently would each insert an arm, the first would win the match,
//! and the second would be silently dead code. Rather than let selection order
//! decide which one works, they are one facility. Splitting them means teaching
//! the patcher that several facilities can contribute to one arm; nothing needs
//! that yet.
//!
//! ## available_parallelism
//!
//! The stub returns `Err(UNKNOWN_THREAD_COUNT)`, which is honest but useless:
//! crates that size a pool from it fall back to 1. The host answers from
//! `navigator.hardwareConcurrency`, the same figure frustrate's Dart runtime
//! exposes as `hardwareParallelism` and uses to size an `ActorPool` — one
//! number, so a bridged crate's own pool and a Dart-side pool cannot disagree
//! about how wide the machine is.
//!
//! frustrate's *call* pool is not sized from this: on wasm its width is a
//! declared value or a constant default, because the stub above is what a plain
//! build sees (runtime/rust/src/pool.rs). This facility widens what a
//! *dependency* can learn, not what the runtime asks.
//!
//! **It reports the machine, not permission to use it.** On a single-threaded
//! build `std::thread::spawn` still fails, and a crate that sizes a pool from
//! this and then spawns gets its error from the spawn, where it belongs.
//! Reporting 1 would be a lie about the hardware and would also mis-size the
//! Actor pool, which *can* use the width.
//!
//! ## sleep
//!
//! Only reached on the **stock** build. With `+atomics`, std has a real
//! futex-backed `sleep` and the arm keeps it; this is for the build that has no
//! wait instruction at all, where the stub is `panic!`.
//!
//! `thread::sleep` blocks — including inside an `async fn`, where it does *not*
//! yield and no other future on that thread runs. That is the correct semantics
//! for a blocking call, so the only question is where blocking is legal: a
//! worker may wait, the main thread may not.
//!
//! So the host busy-waits on a worker and **throws on the main thread**. The
//! throw crosses back as a trap and `$frustrateCall` attributes it — the same
//! loud, attributable outcome std gives today, kept rather than traded for a
//! frozen page. An Actor instance is a worker, which is where this is meant to
//! be used.
//!
//! The busy-wait burns that worker's core for the duration. There is nothing to
//! block on without atomics, so this is the honest cost rather than a shortcut;
//! a caller that wants to yield instead should be an `async fn` awaiting a
//! timer, not calling `thread::sleep`.

use crate::io;
use crate::num::NonZero;
use crate::time::Duration;

#[link(wasm_import_module = "frustrate")]
unsafe extern "C" {
    /// `navigator.hardwareConcurrency`, or 0 when the host cannot say.
    safe fn hardware_concurrency() -> u32;
    /// Block for `ns`. Throws on the main thread, where blocking is illegal.
    safe fn sleep_ns(ns: u64);
}

pub fn available_parallelism() -> io::Result<NonZero<usize>> {
    match NonZero::new(hardware_concurrency() as usize) {
        Some(n) => Ok(n),
        // The host could not answer. `UNKNOWN_THREAD_COUNT` is what the stub
        // returns and what callers already handle; inventing a 1 here would
        // report a machine width nobody measured.
        None => Err(io::Error::UNKNOWN_THREAD_COUNT),
    }
}

pub fn sleep(dur: Duration) {
    // Saturating: a Duration can exceed u64 nanoseconds, and a sleep that long
    // is not going to be waited out anyway. Clamping is better than wrapping to
    // a short sleep, which would be a silently wrong duration.
    sleep_ns(u64::try_from(dur.as_nanos()).unwrap_or(u64::MAX));
}
