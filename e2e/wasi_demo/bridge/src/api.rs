//! An event log: every entry is a v4 UUID, a wall-clock timestamp, and an
//! elapsed time from a monotonic clock.
//!
//! Every member deliberately reaches a `std` facility that plain
//! `wasm32-unknown-unknown` does not have — entropy, both clocks, `println!` —
//! which is what puts this crate on the wasi platform. It must not acquire a
//! wasm-bindgen dependency: that combination does not build.

use frustrate::bridge;
use std::time::{Duration, Instant, SystemTime};
use uuid::Uuid;

/// One entry. Carries a value from each of the three clocks-and-entropy
/// facilities so the UI (and the browser test) can check all of them.
#[derive(Clone)]
#[bridge(data)]
pub struct Entry {
    /// A v4 UUID — 122 bits from `random_get`.
    pub id: String,
    pub label: String,
    /// Wall clock, `clock_time_get(REALTIME)`. Crosses to Dart as a `DateTime`.
    pub at: SystemTime,
    /// Monotonic, `clock_time_get(MONOTONIC)`. Crosses as a `Duration`.
    pub since_start: Duration,
}

/// The log itself. Confined: one owner on the UI isolate, sync methods running
/// on the caller — the model has nothing to do with the platform choice, it is
/// just the simplest thing that holds state.
#[bridge(confined)]
pub struct EventLog {
    started: Instant,
    entries: Vec<Entry>,
}

#[bridge]
impl EventLog {
    #[bridge(sync)]
    pub fn new() -> Self {
        EventLog {
            // If MONOTONIC were missing this would abort here, at construction,
            // before any entry existed — which is the loud version.
            started: Instant::now(),
            entries: Vec::new(),
        }
    }

    #[bridge(sync)]
    pub fn append(&mut self, label: String) -> Entry {
        let entry = Entry {
            id: Uuid::new_v4().to_string(),
            label,
            at: SystemTime::now(),
            since_start: self.started.elapsed(),
        };
        // Reaches `fd_write`. On web this is a console line, which is what the
        // Playwright spec asserts on — the only way to see stdout from a wasm
        // module, and proof that std's own output path is wired rather than
        // silently discarded.
        println!("[event-log] {} {}", entry.id, entry.label);
        self.entries.push(entry.clone());
        entry
    }

    #[bridge(sync)]
    pub fn entries(&self) -> Vec<Entry> {
        self.entries.clone()
    }

    #[bridge(sync)]
    pub fn len(&self) -> i64 {
        self.entries.len() as i64
    }
}

/// The verdict of [`entropy_report`].
///
/// A report rather than a bool because the interesting failure is not "it threw"
/// — it is "it succeeded and the bytes are wrong", and a caller that only sees a
/// bool cannot tell you *how* wrong.
#[bridge(data)]
pub struct EntropyReport {
    pub samples: i64,
    /// How many of the sampled UUIDs were distinct. Anything below `samples` on
    /// a 122-bit id means the source is not random.
    pub distinct: i64,
    /// Whether any sample came back as the nil UUID. A `random_get` that
    /// returns success without writing produces exactly this, forever.
    pub any_nil: bool,
    /// Population count over every random byte sampled, and what a fair coin
    /// would give. Catches a source that is non-zero but badly biased — a
    /// stuck-bit or a repeating block that `distinct` alone would miss only if
    /// it also repeated, which it need not.
    pub set_bits: i64,
    pub total_bits: i64,
    /// `set_bits` within [35%, 65%] of `total_bits`, `distinct == samples`, and
    /// no nil. The one call a UI or a test needs.
    pub healthy: bool,
}

/// Draw `samples` UUIDs and report on the entropy behind them.
///
/// **This is the assertion this example exists to make.** Every other failure
/// on this platform is loud: a missing import throws, a missing clock aborts. A
/// `random_get` that reports success *without filling the buffer* raises
/// nothing anywhere, and neither the type system nor the ABI can catch it. Only
/// sampling the output can.
#[bridge(sync)]
pub fn entropy_report(samples: i64) -> EntropyReport {
    let n = samples.clamp(1, 4096) as usize;
    let mut ids = Vec::with_capacity(n);
    let mut set_bits = 0i64;
    let mut any_nil = false;

    for _ in 0..n {
        let id = Uuid::new_v4();
        if id.is_nil() {
            any_nil = true;
        }
        set_bits += id
            .as_bytes()
            .iter()
            .map(|b| b.count_ones() as i64)
            .sum::<i64>();
        ids.push(id);
    }

    // 6 of the 128 bits are the version and variant, which are constants — they
    // are counted above, so the expectation is not exactly half. Close enough
    // that a 35–65% band is a wide margin around it and a narrow one around
    // anything broken (all-zero scores 4 of 128; all-ones scores 126 of 128).
    let total_bits = (n * 128) as i64;
    let distinct = {
        let mut sorted: Vec<_> = ids.iter().map(|u| u.as_u128()).collect();
        sorted.sort_unstable();
        sorted.dedup();
        sorted.len() as i64
    };

    let healthy = !any_nil
        && distinct == n as i64
        && set_bits * 100 >= total_bits * 35
        && set_bits * 100 <= total_bits * 65;

    EntropyReport {
        samples: n as i64,
        distinct,
        any_nil,
        set_bits,
        total_bits,
        healthy,
    }
}

/// The host environment, as std sees it.
///
/// `environ_sizes_get`/`environ_get` are the two imports this platform needs
/// that have no interesting implementation — frustrate reports an empty
/// environment. This exists so that "empty" is an *observed* answer rather than
/// an assumed one: std reaching an unimplemented `environ_get` would throw, and
/// a caller would otherwise never know the difference between "no variables"
/// and "never asked".
#[bridge(sync)]
pub fn env_var_count() -> i64 {
    std::env::vars().count() as i64
}
