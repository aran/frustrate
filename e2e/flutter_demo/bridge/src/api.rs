//! The demo bridge API: a small gallery of the frustrate feature set, each
//! item backed by real UI in `lib/main.dart`.
//!
//! The shapes here mirror the exhaustive integration fixture
//! (tests/test_api/src/api.rs) under gallery-friendly names. The crate is
//! web-capability-safe, and it shows both fates a native-only member can take.
//! `transform_sum` (a plain pool fn taking a value-returning `DartFunction`)
//! is compile-time absent from the wasm surface — the shared `main.dart`
//! reaches it only through the `native_features.dart` conditional-export seam,
//! so the web build still compiles. `Ledger::blocking_read` instead opts INTO
//! the web surface with `#[bridge(web = "runtime_fail")]`: present so portable
//! code compiles, but throwing a loud `UnsupportedError` if actually called
//! there. `Ledger::blocking_get` is the un-opted default (compile-time absent).

use frustrate::{bridge, BytesCodec, DartCallback, DartFunction, StreamSink};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

// ================================================================ basics ==

#[bridge(sync)]
pub fn greet(name: String) -> String {
    // The first of the `log::info!` calls scattered through this file. They are
    // ordinary `log` — no frustrate type appears — and they reach Dart because
    // `install_logging` below put a `StreamSink` behind `log`'s facade. See
    // "watching the bridge" at the foot of this file.
    log::info!("greeting {name}");
    format!("Hello, {name} — from Rust \u{1F980}")
}

/// The counter state. Confined: a single owner on the UI isolate, sync
/// methods running on the caller.
#[bridge(confined)]
pub struct Counter {
    value: i64,
}

#[bridge]
impl Counter {
    #[bridge(sync)]
    pub fn new() -> Self {
        Counter { value: 0 }
    }

    #[bridge(sync)]
    pub fn increment(&mut self) -> i64 {
        self.value += 1;
        self.value
    }

    #[bridge(sync)]
    pub fn value(&self) -> i64 {
        self.value
    }
}

/// Deliberately brute-force CPU work, to give the async path something
/// visible to chew on. Note this is a *sync* body dispatched asynchronously
/// (it runs straight through on a pool thread) — contrast `cooperative_yield`
/// below, whose body actually `.await`s.
#[bridge]
pub fn nth_prime(n: i64) -> i64 {
    // From wherever the body runs — a pool worker on macOS, the calling thread
    // on single-threaded web. One logger serves both, because it is a `static`
    // of the instance the call was dispatched from.
    log::info!("nth_prime({n})");
    brute_force_nth_prime(n)
}

/// A real Rust `async fn` — its body `.await`s, so it *suspends and resumes*
/// on the cooperative executor (`frustrate::executor`) instead of running
/// straight through. This is the capability that works on **every** config:
/// native and threaded web drive the future on pool worker threads;
/// single-threaded web drives it on a JS microtask, yielding to the event loop
/// between polls and never blocking the main thread.
///
/// It `.await`s [`YieldOnce`] `rounds` times — each yield returns `Pending` and
/// wakes itself, so the executor must poll it `rounds + 1` times — then returns
/// `x * 2`. Because it suspends, thousands of these can be in flight at once on
/// a single thread (a suspended future is heap data, not a parked thread); the
/// demo fires a batch concurrently to show exactly that. No external async
/// runtime is involved — the future arranges its own wakeups.
#[bridge]
pub async fn cooperative_yield(x: i64, rounds: i64) -> i64 {
    for _ in 0..rounds.max(0) {
        YieldOnce::pending_once().await;
    }
    x * 2
}

/// A minimal self-driving future: `Pending` on the first poll (waking itself
/// via the waker), `Ready` on the second. Enough to make an `async fn` body
/// actually suspend and re-poll with no external reactor. Mirrors the
/// integration fixture's `YieldOnce` (tests/test_api/src/api.rs).
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
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    }
}

// ==================================================== values & data types ==
//
// A type-interop gallery earns a tour of its type surface: the scalars that a
// Dart `int` can't hold, the collections that pick their Dart container by
// element type and iteration order, a Rust tuple as a Dart record, and the
// data classes codegen mints with value equality and `copyWith`.

/// A `char` is a single Unicode scalar, not a byte: the crab is U+1F980, an
/// astral scalar Dart reconstructs from a surrogate pair. Crosses as a
/// one-character Dart `String`.
#[bridge(sync)]
pub fn crab() -> char {
    '\u{1F980}'
}

/// Echo a `char` so any single-character Dart `String` round-trips as a scalar.
#[bridge(sync)]
pub fn echo_char(c: char) -> char {
    c
}

/// The 128-bit extremes minted Rust-side: values past 2^63 that a Dart `int`
/// cannot represent, so they arrive as `BigInt` over the 16-byte big-integer
/// codec.
#[bridge(data)]
pub struct BigIntExtremes {
    pub i128_min: i128,
    pub i128_max: i128,
    pub u128_max: u128,
}

#[bridge(sync)]
pub fn big_int_extremes() -> BigIntExtremes {
    BigIntExtremes {
        i128_min: i128::MIN,
        i128_max: i128::MAX,
        u128_max: u128::MAX,
    }
}

/// A letter-frequency histogram: `Vec<i32>` maps to a Dart `Int32List`, a
/// typed list whose element type is fixed at 32-bit rather than a boxed
/// `List<int>`. Twenty-six buckets, a-z.
#[bridge(sync)]
pub fn letter_histogram(text: String) -> Vec<i32> {
    let mut counts = vec![0i32; 26];
    for c in text.chars() {
        let lower = c.to_ascii_lowercase();
        if lower.is_ascii_lowercase() {
            counts[(lower as u8 - b'a') as usize] += 1;
        }
    }
    counts
}

/// value → count in a `BTreeMap`: it iterates in sorted key order, and Dart's
/// insertion-ordered `Map` preserves that, so the pairs arrive sorted — a
/// `HashMap` would hand them back scrambled.
#[bridge(sync)]
pub fn tally_sorted(xs: Vec<i64>) -> BTreeMap<i64, i64> {
    let mut m = BTreeMap::new();
    for x in xs {
        *m.entry(x).or_insert(0) += 1;
    }
    m
}

/// distinct values in a `BTreeSet` — sorted and deduplicated (Dart `Set<int>`).
#[bridge(sync)]
pub fn distinct_sorted(xs: Vec<i64>) -> BTreeSet<i64> {
    xs.into_iter().collect()
}

/// rotate-left into a `VecDeque`: the deque shares the `Int64List` wire with
/// `Vec`, only the reconstructed Rust container differs.
#[bridge(sync)]
pub fn rotate_left(xs: Vec<i64>, by: i64) -> VecDeque<i64> {
    let mut d: VecDeque<i64> = xs.into();
    if !d.is_empty() {
        let by = by.rem_euclid(d.len() as i64) as usize;
        d.rotate_left(by);
    }
    d
}

/// A Rust tuple becomes a Dart record. Nested here — `(text, (char count,
/// byte count))` → `(String, (int, int))`, read as `.$1` and `.$2.$1`/`.$2.$2`.
#[bridge(sync)]
pub fn measure(text: String) -> (String, (i64, i64)) {
    let chars = text.chars().count() as i64;
    let bytes = text.len() as i64;
    (text, (chars, bytes))
}

/// A data class: `final` fields, generated value equality (two equal-valued
/// `Passport`s are `==` and collapse as `Set`/`Map` keys) and `copyWith`
/// (override one field, or null the nullable `holder`).
#[bridge(data)]
pub struct Passport {
    pub id: i64,
    pub holder: Option<String>,
    pub tags: Vec<String>,
}

#[bridge(sync)]
pub fn echo_passport(p: Passport) -> Passport {
    p
}

/// The `#[bridge(no_eq)]` sibling: identity equality instead of value equality,
/// so two equal-valued `Ticket`s are `!=` and never deduplicate in a `Set`.
/// `copyWith` and `toString` still generate (they don't depend on equality).
#[bridge(data, no_eq)]
pub struct Ticket {
    pub code: i64,
    pub note: Option<String>,
}

#[bridge(sync)]
pub fn echo_ticket(t: Ticket) -> Ticket {
    t
}

// ==================================================== concurrency models ==

/// Frozen: an immutable snapshot shared behind an Arc. Sync reads run on the
/// caller; the async method runs on the pool over the same shared handle.
#[bridge(frozen)]
pub struct Snapshot {
    words: Vec<String>,
}

#[bridge]
impl Snapshot {
    #[bridge(sync)]
    pub fn build(words: Vec<String>) -> Self {
        Snapshot { words }
    }

    #[bridge(sync)]
    pub fn word_count(&self) -> usize {
        self.words.len()
    }

    /// Async method on a Frozen object (runs on the pool over the shared Arc).
    pub fn join(&self, sep: String) -> String {
        self.words.join(&sep)
    }
}

/// Locked: shared mutable state behind an RwLock. Async methods take the lock
/// on the pool; the contract-marked sync reads surface contention loudly.
#[bridge(locked)]
pub struct Ledger {
    balance: i64,
}

#[bridge]
impl Ledger {
    #[bridge(sync)]
    pub fn new() -> Self {
        Ledger { balance: 0 }
    }

    /// Async default: the write lock is acquired on the pool thread.
    pub fn add(&mut self, delta: i64) -> i64 {
        self.balance += delta;
        log::info!("ledger balance is now {}", self.balance);
        self.balance
    }

    pub fn get(&self) -> i64 {
        self.balance
    }

    /// Contract-marked sync read: contended -> ContentionException in Dart.
    ///
    /// On every target, including the browser main thread. The acquisition is
    /// a compare-exchange and the release reaches no wait instruction, so the
    /// try-lock is the one synchronous way to touch shared state directly from
    /// the main isolate. `add` and `hold_write` below are the dispatched shape
    /// beside it, which waits for the lock instead of refusing.
    #[bridge(sync, on_contention = "error")]
    pub fn try_get(&self) -> i64 {
        self.balance
    }

    /// Contract-marked blocking sync read — the un-opted default fate: a
    /// blocking read parks the calling thread, which is main-thread-fatal on
    /// web, so codegen drops it from the wasm surface entirely.
    #[bridge(sync, on_contention = "block")]
    pub fn blocking_get(&self) -> i64 {
        self.balance
    }

    /// The same blocking read, opted INTO the web surface with
    /// `#[bridge(web = "runtime_fail")]`: present on web so portable code that
    /// names it still compiles, its web body throwing a loud, attributable
    /// `UnsupportedError` if actually called there. On native it returns the
    /// balance like `blocking_get`. The informed opt-in against the default
    /// compile-time absence above.
    #[bridge(sync, on_contention = "block", web = "runtime_fail")]
    pub fn blocking_read(&self) -> i64 {
        self.balance
    }

    /// Holds the write lock for `millis` so the UI can observe contention
    /// deterministically.
    ///
    /// **Not an unconditional `thread::sleep`, and the cfg is load-bearing.**
    /// This crate builds for the web, and on `wasm32-unknown-unknown` without
    /// `target_feature=atomics` std's `thread::sleep` is the `unsupported`
    /// stub whose entire body is `panic!("can't sleep")` — under wasm's
    /// `panic=abort` that is not a slow call, it is a dead module. The demo's
    /// "hold write 800ms" button is not platform-gated, and `e2e/flutter_demo/web`
    /// is exactly that build, so the ungated version took the app down on the
    /// press. Same pattern, and the same reasoning, as `pace_producer` in
    /// tests/test_api.
    ///
    /// The single-threaded arm **returns immediately**, and that is the honest
    /// behaviour rather than a shortcut. Holding the lock there demonstrates
    /// nothing: a bridged `async fn` runs *inline on the one thread*, so the
    /// body completes before `hold_write` returns and no `try_get` can be
    /// issued while it is held. There is nothing to contend with. Burning the
    /// time anyway would freeze the page for `millis` and still show the
    /// caller a successful read — a worse demo than no pause at all, and it
    /// would need an iteration-counted busy loop (no clock: `Instant::now` is
    /// unsupported on wasm) whose calibration is a guess that drifts with the
    /// optimizer.
    ///
    /// The card's subtitle already scopes the claim (contention is observable
    /// on native, the only target where `try_get` exists), and `main.dart`
    /// disables the button where it cannot work, so the no-op is unreachable
    /// from the UI and exists to keep the *bridge* honest for a programmatic
    /// caller.
    pub fn hold_write(&mut self, millis: i64) {
        #[cfg(any(not(target_family = "wasm"), target_feature = "atomics"))]
        std::thread::sleep(std::time::Duration::from_millis(millis as u64));

        #[cfg(all(target_family = "wasm", not(target_feature = "atomics")))]
        let _ = millis;
    }
}

/// An actor: one instance owns one executor — a dedicated thread on macOS, a
/// Worker-hosted wasm instance on the web. Fanning the same CPU work across
/// several of them runs genuinely in parallel, in the browser too.
#[bridge(actor)]
pub struct Prospector {
    digs: i64,
}

#[bridge]
impl Prospector {
    pub fn new() -> Self {
        Prospector { digs: 0 }
    }

    /// Brute-force the nth prime on this prospector's executor.
    pub fn dig(&mut self, n: i64) -> i64 {
        self.digs += 1;
        brute_force_nth_prime(n)
    }

    pub fn digs(&self) -> i64 {
        self.digs
    }

    /// `std::thread::sleep` on this actor's executor, returning the millis it
    /// observed elapsing, or -1 where the facility is absent.
    ///
    /// **Here rather than as a free function, because where it runs is the
    /// whole point.** `thread::sleep` blocks its thread — including inside an
    /// `async fn`, where it does not yield and nothing else on that thread
    /// runs. A worker may wait; the browser main thread may not. An actor
    /// instance *is* a worker. Called on the UI isolate the host would refuse
    /// it, loudly, rather than freeze the page.
    ///
    /// Without atomics there is nothing to wait on, so the host busy-waits and
    /// burns this worker's core for the duration. That is the honest cost of
    /// the facility, not a shortcut around it.
    pub fn nap(&mut self, millis: i64) -> i64 {
        #[cfg(any(not(target_family = "wasm"), feature = "std-facilities"))]
        {
            let start = std::time::Instant::now();
            std::thread::sleep(std::time::Duration::from_millis(millis.max(0) as u64));
            start.elapsed().as_millis() as i64
        }
        #[cfg(all(target_family = "wasm", not(feature = "std-facilities")))]
        {
            let _ = millis;
            -1
        }
    }

    /// Progress stream computed on this actor's executor: each prime relays
    /// out while the method is still running — real-time streaming during
    /// long CPU work, on the web too.
    pub fn mine_progress(&mut self, rounds: i64, n: i64, sink: StreamSink<i64>) {
        for _ in 0..rounds {
            self.digs += 1;
            if !sink.add(brute_force_nth_prime(n)) {
                return;
            }
        }
    }
}

// ==================================================== streams & callbacks ==

/// The document's patch stream element — a data-carrying enum, mirrored as a
/// sealed Dart hierarchy the UI pattern-matches.
#[bridge(data)]
#[derive(Clone)]
pub enum TextPatch {
    Splice { index: usize, text: String },
    Delete { index: usize, length: usize },
    Mark(String, i64),
    Clear,
}

/// Round-trips a constructed patch list — lets the UI build every variant in
/// Dart, cross the bridge, and switch over the sealed result.
#[bridge(sync)]
pub fn echo_patches(ps: Vec<TextPatch>) -> Vec<TextPatch> {
    ps
}

/// A Confined mutable document. `watch` is the automerge-style pattern: stored
/// sinks pushed by later splices, pruned once cancelled; `on_change` is the
/// closure flavor, fired with the new length after each splice.
#[bridge(confined)]
pub struct TextDoc {
    content: String,
    watchers: Vec<StreamSink<TextPatch>>,
    change_callbacks: Vec<DartCallback<usize>>,
}

#[bridge]
impl TextDoc {
    #[bridge(sync)]
    pub fn new() -> Self {
        TextDoc {
            content: String::new(),
            watchers: vec![],
            change_callbacks: vec![],
        }
    }

    /// Subscribe to this document's patches. The sink outlives this call
    /// (stored); the stream ends when the document drops, or on cancel.
    pub fn watch(&mut self, sink: StreamSink<TextPatch>) {
        self.watchers.push(sink);
    }

    /// Closure flavor: fires with the new char count after each splice.
    pub fn on_change(&mut self, cb: DartCallback<usize>) {
        self.change_callbacks.push(cb);
    }

    pub fn text(&self) -> String {
        self.content.clone()
    }

    pub fn len_chars(&self) -> usize {
        self.content.chars().count()
    }

    pub fn splice(
        &mut self,
        index: usize,
        delete: usize,
        insert: String,
    ) -> Result<Vec<TextPatch>, String> {
        let chars: Vec<char> = self.content.chars().collect();
        if index + delete > chars.len() {
            return Err(format!(
                "splice out of bounds: index {index} + delete {delete} > length {}",
                chars.len()
            ));
        }
        let mut next: String = chars[..index].iter().collect();
        next.push_str(&insert);
        next.extend(&chars[index + delete..]);
        self.content = next;
        let mut patches = vec![];
        if delete > 0 {
            patches.push(TextPatch::Delete {
                index,
                length: delete,
            });
        }
        if !insert.is_empty() {
            patches.push(TextPatch::Splice {
                index,
                text: insert,
            });
        }
        // Push to live watchers; prune the ones whose stream is gone.
        self.watchers
            .retain(|w| patches.iter().all(|p| w.add(p.clone())));
        let len = self.content.chars().count();
        for cb in &self.change_callbacks {
            cb.call(len);
        }
        Ok(patches)
    }
}

/// Value-returning callback via the PORTABLE awaited path: an `async fn` that
/// awaits `call_async` on the cooperative executor, so it runs on native VM,
/// single-threaded web, AND threaded web — the web-portable counterpart to
/// `transform_sum`'s blocking `call`. A closure that throws surfaces as this
/// call's attributable `BridgePanicException`.
#[bridge]
pub async fn transform(x: i64, f: DartFunction<i64, i64>) -> i64 {
    f.call_async(x).await
}

/// Value-returning callback on a plain pool fn. A plain fn cannot `.await`, so
/// it uses the blocking `call`, which parks the pool worker per item while the
/// Dart closure maps it. Parking a worker against the web event loop is
/// main-thread-fatal, so this shape is native-only — compile-time absent from
/// the wasm surface, reached from the shared UI through native_features.dart.
/// The awaited `transform` above is the portable counterpart.
#[bridge]
pub fn transform_sum(values: Vec<i64>, f: DartFunction<i64, i64>) -> i64 {
    values.into_iter().map(|v| f.call(v)).sum()
}

// ================================================================ traits ==

/// A Frozen trait bridged as `Box<dyn Greeter>`: one Dart interface, several
/// Rust implementations chosen at the factory, `louder` minting fresh handles.
#[bridge(frozen)]
pub trait Greeter: Send + Sync {
    #[bridge(sync)]
    fn greet(&self, name: String) -> String;
    /// Async: runs on the pool over the shared Arc'd trait object.
    fn greet_many(&self, names: Vec<String>) -> Vec<String>;
    /// A trait method minting a new trait-object handle.
    fn louder(&self) -> Box<dyn Greeter>;
}

struct PlainGreeter {
    excitement: usize,
}

impl Greeter for PlainGreeter {
    fn greet(&self, name: String) -> String {
        format!("hello {name}{}", "!".repeat(self.excitement))
    }
    fn greet_many(&self, names: Vec<String>) -> Vec<String> {
        names.into_iter().map(|n| self.greet(n)).collect()
    }
    fn louder(&self) -> Box<dyn Greeter> {
        Box::new(PlainGreeter {
            excitement: self.excitement + 1,
        })
    }
}

struct PirateGreeter;

impl Greeter for PirateGreeter {
    fn greet(&self, name: String) -> String {
        format!("ahoy {name}")
    }
    fn greet_many(&self, names: Vec<String>) -> Vec<String> {
        names.into_iter().map(|n| self.greet(n)).collect()
    }
    fn louder(&self) -> Box<dyn Greeter> {
        Box::new(PirateGreeter)
    }
}

struct RobotGreeter {
    unit: i64,
}

impl Greeter for RobotGreeter {
    fn greet(&self, name: String) -> String {
        format!("BEEP {name} [unit {}]", self.unit)
    }
    fn greet_many(&self, names: Vec<String>) -> Vec<String> {
        names.into_iter().map(|n| self.greet(n)).collect()
    }
    fn louder(&self) -> Box<dyn Greeter> {
        Box::new(RobotGreeter {
            unit: self.unit + 1,
        })
    }
}

#[bridge(sync)]
pub fn new_greeter(kind: String) -> Box<dyn Greeter> {
    log::info!("building a {kind} greeter");
    match kind.as_str() {
        "pirate" => Box::new(PirateGreeter),
        "robot" => Box::new(RobotGreeter { unit: 1 }),
        _ => Box::new(PlainGreeter { excitement: 0 }),
    }
}

// ======================================================== external types ==

/// Stands in for a prost-generated protobuf message: crosses as bytes via
/// user codecs on both sides. Wire form: 8-byte LE revision, then the UTF-8
/// title — mirrored by FakePlan in bridge/lib/fake_plan.dart.
#[bridge(bytes(dart = "FakePlan", import = "package:demo_bridge/fake_plan.dart"))]
pub struct FakePlanMsg {
    pub title: String,
    pub revision: i64,
}

impl BytesCodec for FakePlanMsg {
    fn to_bytes(&self) -> Vec<u8> {
        let mut out = self.revision.to_le_bytes().to_vec();
        out.extend_from_slice(self.title.as_bytes());
        out
    }

    fn from_bytes(bytes: &[u8]) -> Self {
        let revision = i64::from_le_bytes(bytes[..8].try_into().expect("FakePlan: short buffer"));
        let title = String::from_utf8(bytes[8..].to_vec()).expect("FakePlan: invalid utf8");
        FakePlanMsg { title, revision }
    }
}

#[bridge(sync)]
pub fn bump_plan(p: FakePlanMsg) -> FakePlanMsg {
    FakePlanMsg {
        title: p.title,
        revision: p.revision + 1,
    }
}

// ============================================== the standard library on web ==
//
// Five things ordinary Rust does that wasm32-unknown-unknown cannot: read a
// clock, get entropy, print, count cores, sleep. std links the unsupported PAL
// for all of them, so a crate you did not write and cannot change either
// panics or silently does nothing. Building against a std from
// toolchain/custom_std replaces those stubs with host calls, and then these
// work — through std's own APIs, from Rust that does not know it is on wasm.
//
// **The cfg is inside each body, never around the item.** Codegen is syn over
// this file and never evaluates cfg, so the bridged surface must be identical
// on every target; only the returned *value* says whether std could do the
// work. `-1` is the sentinel for "this std has no such facility". That is what
// lets one lib/main.dart render the same card everywhere and report honestly:
// real numbers on macOS and on a facility web build, sentinels on the default
// one, with the card itself explaining the difference.
//
// The `std-facilities` feature is what distinguishes the two web builds, and it
// has to be a feature rather than a target check: stock and patched std are
// both wasm32-unknown-unknown. bridge/BUILD.bazel sets it from the platform's
// wasm_std constraint, so it cannot disagree with the std being linked.

/// Whether this build can do any of the below — so the UI can say why the
/// numbers are sentinels instead of just showing -1.
#[bridge(sync)]
pub fn std_facilities_present() -> bool {
    cfg!(any(not(target_family = "wasm"), feature = "std-facilities"))
}

/// Micros since the Unix epoch from `SystemTime::now()`, or -1.
#[bridge(sync)]
pub fn std_wall_micros() -> i64 {
    #[cfg(any(not(target_family = "wasm"), feature = "std-facilities"))]
    {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_micros() as i64)
            .unwrap_or(-1)
    }
    #[cfg(all(target_family = "wasm", not(feature = "std-facilities")))]
    {
        -1
    }
}

/// Nanos `Instant` observed elapsing across a fixed slice of real work, or -1.
///
/// Real work rather than two adjacent reads on purpose: `performance.now()` is
/// coarsened to 100 µs unless the page is cross-origin isolated, so a tight
/// pair legitimately reads zero. The demo's own pages send COOP/COEP and get
/// 5 µs; a page that does not still shows a sensible number here.
#[bridge(sync)]
pub fn std_monotonic_nanos() -> i64 {
    #[cfg(any(not(target_family = "wasm"), feature = "std-facilities"))]
    {
        let start = std::time::Instant::now();
        std::hint::black_box(brute_force_nth_prime(200));
        start.elapsed().as_nanos() as i64
    }
    #[cfg(all(target_family = "wasm", not(feature = "std-facilities")))]
    {
        -1
    }
}

/// A hash of a fixed key under a fresh `RandomState`, folded positive, or -1.
///
/// `RandomState` is the public surface over `hashmap_random_keys`, which is
/// where a stock wasm std is not merely absent but quietly weak: it derives the
/// seed from *allocation addresses*, under std's own note that this "isn't
/// particularly secure, but there isn't really an alternative". In a
/// deterministic module those are close to predictable.
///
/// Within one page load this only shows the path is reachable — std caches the
/// keys per thread, so reading twice here returns the same number by
/// construction. That the seed differs *between* loads is the property that
/// matters, and it takes two page loads: playwright/std_facilities.spec.js.
///
/// Deliberately not `getrandom`: it refuses to compile for
/// wasm32-unknown-unknown at all, which would take the default web build with
/// it. Going through std is also the more honest demonstration — this is about
/// the std a crate already calls.
#[bridge(sync)]
pub fn std_random_u64() -> i64 {
    #[cfg(any(not(target_family = "wasm"), feature = "std-facilities"))]
    {
        use std::collections::hash_map::RandomState;
        use std::hash::{BuildHasher, Hasher};
        let mut h = RandomState::new().build_hasher();
        h.write_u64(0x5EED);
        // Fold to a positive i64: -1 is the sentinel, so the real path must
        // never be able to produce it.
        (h.finish() >> 1) as i64
    }
    #[cfg(all(target_family = "wasm", not(feature = "std-facilities")))]
    {
        -1
    }
}

/// `available_parallelism()`, or -1.
///
/// The machine, not permission to use it: this is the real core count even on
/// a single-threaded build, where `thread::spawn` still fails. Reporting 1 to
/// prevent that would be a lie about the hardware — and would mis-size an
/// `ActorPool`, which can genuinely use the width.
#[bridge(sync)]
pub fn std_cores() -> i64 {
    #[cfg(any(not(target_family = "wasm"), feature = "std-facilities"))]
    {
        std::thread::available_parallelism()
            .map(|n| n.get() as i64)
            .unwrap_or(-1)
    }
    #[cfg(all(target_family = "wasm", not(feature = "std-facilities")))]
    {
        -1
    }
}

/// `println!`, returning the bytes std reported writing, or 0 where stdout is
/// the stub that accepts a write and discards it.
///
/// On a facility build this reaches `console.log`, one entry per line — the
/// host buffers per stream until a newline, so a `println!` arrives whole
/// rather than split across writes.
#[bridge(sync)]
pub fn std_console_write(msg: String) -> i64 {
    #[cfg(any(not(target_family = "wasm"), feature = "std-facilities"))]
    {
        println!("{msg}");
        msg.len() as i64 + 1
    }
    #[cfg(all(target_family = "wasm", not(feature = "std-facilities")))]
    {
        let _ = msg;
        0
    }
}

// ==================================================== watching the bridge ==

/// One `log` record, in the shape this app chose for it.
///
/// The wire form is the *app's*, not the runtime's: `frustrate::logging` hands
/// the mapper a `&log::Record` and forwards whatever comes back, so codegen
/// only ever sees an ordinary bridged struct of this crate's own design.
#[bridge(data)]
pub struct LogLine {
    pub level: String,
    pub target: String,
    pub message: String,
}

/// Send every record `log` accepts to `sink`.
///
/// Sync, because installing is a registration and not work — the same reason
/// `TextDoc::watch` is. Dart calls it once from `main()`, and a Flutter hot
/// restart runs `main()` again in the same process: that lands on the designed
/// re-install path, which drops the displaced sink and so ends the dead
/// isolate's stream rather than leaving records going nowhere.
///
/// `Result<(), String>` so a refusal arrives in Dart as a `BridgeException`
/// carrying the runtime's own message; the demo has no `anyhow`.
#[bridge(sync)]
pub fn install_logging(sink: StreamSink<LogLine>) -> Result<(), String> {
    frustrate::logging::install(log::LevelFilter::Info, sink, |record| LogLine {
        level: record.level().to_string(),
        target: record.target().to_string(),
        message: record.args().to_string(),
    })
    .map_err(|e| e.to_string())
}

/// Log one line, from the page instance's own thread.
///
/// The only member here whose entire visible effect is a record — it exists so
/// the card has a button. Everything else that logs (`greet`, `nth_prime`,
/// `Ledger::add`, `new_greeter`) does it while doing its own job, which is the
/// shape a real app has and the reason the card fills up as the gallery is
/// used.
///
/// **Nothing on `Prospector` logs, deliberately.** An actor is a separate wasm
/// instance on web with its own `static`s, so its records would need a logger
/// installed inside it — they would appear on macOS
/// and vanish in the browser, which is a platform difference this card has no
/// way to explain and the gallery has no business hiding.
#[bridge(sync)]
pub fn emit_demo_log(message: String) {
    log::info!("{message}");
}

// =============================================================== helpers ==

fn brute_force_nth_prime(n: i64) -> i64 {
    let mut count = 0i64;
    let mut candidate = 1i64;
    while count < n {
        candidate += 1;
        if is_prime(candidate) {
            count += 1;
        }
    }
    candidate
}

fn is_prime(x: i64) -> bool {
    if x < 2 {
        return false;
    }
    let mut d = 2;
    while d * d <= x {
        if x % d == 0 {
            return false;
        }
        d += 1;
    }
    true
}
