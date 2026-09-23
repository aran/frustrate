//! `Instant` and `SystemTime` for wasm32-unknown-unknown, answered by the host.
//!
//! Installed by toolchain/custom_std as `sys/time/frustrate.rs`, selected by a
//! wasm arm inserted ahead of the fallback in `sys/time/mod.rs` — whose
//! `unsupported` `now()` is `panic!("time not implemented on this platform")`.
//! The surface below mirrors `sys/time/unsupported.rs` exactly; only `now()`
//! differs. That mirroring is load-bearing: `mod.rs` re-exports `Instant`,
//! `SystemTime` and `UNIX_EPOCH` through an `imp` alias, so anything the
//! fallback exposes and this file does not is a compile error.
//!
//! Two clocks, because `Instant` and `SystemTime` are different promises and a
//! single source would break one of them:
//!
//! * `Instant` is **monotonic** and takes `performance.now()`. It never goes
//!   backwards, which `Date.now()` does whenever the wall clock is adjusted.
//! * `SystemTime` is the **wall clock** and takes `Date.now()`. It is allowed
//!   to jump; every user of it already has to cope with that.
//!
//! ## Resolution is a property of the page, not of this file
//!
//! `performance.now()` is deliberately coarsened by browsers unless the page is
//! cross-origin isolated: **5 µs isolated, 100 µs not**, measured in Chromium.
//! So on a page without COOP/COEP any interval shorter than 100 µs reads as
//! **zero** — `a == b` for two `Instant`s taken either side of real work.
//!
//! That is a contract, not a defect, and it is the reason the host half refuses
//! to guess: it reports what the browser gives it. Code that needs to resolve
//! finer than a tick must either run on an isolated page or measure a batch.
//! frustrate serves COOP/COEP on its own pages for exactly this reason.

use crate::time::Duration;

#[link(wasm_import_module = "frustrate")]
unsafe extern "C" {
    /// Monotonic nanoseconds. Never decreases across calls in one instance.
    safe fn now_monotonic_ns() -> u64;
    /// Wall-clock nanoseconds since the Unix epoch. May jump.
    safe fn now_wall_ns() -> u64;
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct Instant(Duration);

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct SystemTime(Duration);

pub const UNIX_EPOCH: SystemTime = SystemTime(Duration::from_secs(0));

impl Instant {
    pub fn now() -> Instant {
        Instant(Duration::from_nanos(now_monotonic_ns()))
    }

    pub fn checked_sub_instant(&self, other: &Instant) -> Option<Duration> {
        self.0.checked_sub(other.0)
    }

    pub fn checked_add_duration(&self, other: &Duration) -> Option<Instant> {
        Some(Instant(self.0.checked_add(*other)?))
    }

    pub fn checked_sub_duration(&self, other: &Duration) -> Option<Instant> {
        Some(Instant(self.0.checked_sub(*other)?))
    }
}

impl SystemTime {
    // Mirrors `sys/time/unsupported.rs`, which grew these when the selector
    // moved: `sys/time/mod.rs` re-exports the whole type through `imp`, so an
    // arm missing them fails to compile against every caller of
    // `SystemTime::MAX`/`MIN`. MIN is `Duration::ZERO`, not `Duration::MIN` —
    // a `Duration` cannot be negative.
    pub const MAX: SystemTime = SystemTime(Duration::MAX);

    pub const MIN: SystemTime = SystemTime(Duration::ZERO);

    pub fn now() -> SystemTime {
        SystemTime(Duration::from_nanos(now_wall_ns()))
    }

    pub fn sub_time(&self, other: &SystemTime) -> Result<Duration, Duration> {
        self.0.checked_sub(other.0).ok_or_else(|| other.0 - self.0)
    }

    pub fn checked_add_duration(&self, other: &Duration) -> Option<SystemTime> {
        Some(SystemTime(self.0.checked_add(*other)?))
    }

    pub fn checked_sub_duration(&self, other: &Duration) -> Option<SystemTime> {
        Some(SystemTime(self.0.checked_sub(*other)?))
    }
}
