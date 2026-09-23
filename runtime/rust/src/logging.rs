//! Rust's `log` facade → a Dart `Stream`.
//!
//! One `log::Log` implementation, whose sink is a [`Hook`] an embedder
//! registers into. A `log::info!` becomes an item on a `StreamController<T>`
//! the Dart caller owns — including one raised by a dependency that has never
//! heard of frustrate, which is the case this module exists for.
//!
//! It hears exactly the crates that linked **the same `log` rlib as this one**,
//! because `log`'s logger is a crate `static`. Under cargo that is every crate
//! in the lockfile and there is nothing to decide. Under Bazel each
//! `crate_universe` hub resolves a `log` of its own, so a consuming module
//! points `@frustrate//runtime/rust:log` — a build setting, not a fixed
//! dependency — at the hub its own crates use, and this runtime compiles
//! against that one. Nothing here is nameable as a `log` dependency, so a
//! bridge crate takes `log` from its own hub, and a mismatch between that one
//! and this runtime's makes [`install`]'s signature unsatisfiable, naming both
//! hubs. What that error does *not* cover is a **second** hub linked into the
//! same bridge: its crates log into a `log` this never installed into, so
//! their records go nowhere and [`dropped`] does not count them.
//!
//! ```
//! use frustrate::{bridge, StreamSink};
//!
//! // In the bridge crate. `LogRecord` is the *user's* bridged struct: the
//! // wire shape is theirs to choose, and codegen only ever sees their crate.
//! #[bridge]
//! pub struct LogRecord { pub level: String, pub target: String, pub message: String }
//!
//! // Sync, because installing is a registration and not work — an async
//! // member would be dispatched to a pool worker to do the same two stores.
//! #[bridge(sync)]
//! pub fn install_logging(sink: StreamSink<LogRecord>) -> Result<(), String> {
//!     frustrate::logging::install(log::LevelFilter::Info, sink, |r| LogRecord {
//!         level: r.level().to_string(),
//!         target: r.target().to_string(),
//!         message: r.args().to_string(),
//!     })
//!     .map_err(|e| e.to_string())
//! }
//! ```
//!
//! ```dart
//! final logs = StreamController<LogRecord>();
//! logs.stream.listen((r) => debugPrint('[${r.level}] ${r.target}: ${r.message}'));
//! installLogging(logs);
//! ```
//!
//! # Why this is in the runtime and not twenty lines in the user's crate
//!
//! "Hold a sink, implement `log::Log`, push records" is genuinely short, and
//! the short version is wrong in one specific way that only shows up on the
//! second run. `log::set_logger` succeeds **once per process** and takes a
//! `&'static dyn Log`, but a Flutter hot restart re-runs Dart `main()` in the
//! same native process — the same fact [`crate::runtime::register`] exists to
//! accommodate. So the naive version either panics on the second install or
//! keeps its first sink, which now points at a dead isolate, and every record
//! after the restart vanishes. The correct version needs a *replaceable* slot
//! behind a non-parking lock, with the displaced sink dropped outside the
//! critical section — which is [`Hook`], and which `hook.rs` documents as the
//! thing that had already drifted three times when it was hand-copied.
//!
//! # Nothing here can deadlock or re-enter the bridge
//!
//! Three separate properties, each structural rather than argued:
//!
//! * **A record delivered while a record is being delivered is dropped, on
//!   that thread.** `map` is user code and may itself call `log::warn!` (a
//!   `Display` impl that logs is the realistic way in). [`IN_LOGGER`] is
//!   checked and set before anything else runs and cleared by a `Drop` guard,
//!   so the inner record is counted and discarded rather than recursing. The
//!   guard is a guard rather than a set/reset pair because `map` may panic and
//!   unwind through here; a leaked `true` would make that thread go dark for
//!   the life of the process.
//! * **No lock is held while user code runs.** [`Hook::get`] clones the
//!   registration *out* of its critical section (hook.rs), and
//!   [`StreamSink::add`]'s path takes no lock at all: two atomic loads, the
//!   generated encoder, and `post::deliver` — a registry read and
//!   `Dart_PostCObject` natively, a direct import call on web.
//! * **No Dart code runs on the Rust stack.** `Dart_PostCObject` enqueues on
//!   the isolate's port; on web `StreamRouter` defers every delivery to a
//!   microtask (`tests/dart_integration/test/sink_reentrancy_test.dart` pins
//!   this on every config). So a `log!` inside a bridge call cannot re-enter
//!   that call's own object.
//!
//! # What is dropped, and how you find out
//!
//! [`dropped`] counts every record this logger accepted and did not deliver.
//! There are two sources and they are deliberately one number, because a
//! consumer's question is the same either way — "how much did I not see":
//!
//! * a re-entrant record, as above;
//! * a record that arrived after the Dart consumer went away. `add` answers
//!   `false` once the subscription is cancelled or the owning isolate is gone
//!   and the logger then retires itself, so that
//!   first record is counted and so is every one after it.
//!
//! Records are **not** dropped for backpressure: `add` is the unbounded
//! fire-and-forget primitive, so a paused subscription buffers on the Dart
//! side. A logger that awaited [`StreamSink::send`] instead would need an
//! async context, and `log::Log::log` has none — it is a synchronous trait
//! method called from anywhere, which is the whole reason `log` exists.

use crate::hook::Hook;
use crate::StreamSink;
use log::{LevelFilter, Log, Metadata, Record};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

/// The registered forwarder: map one record and post it, reporting whether the
/// Dart side took it.
///
/// Type-erased over the item type on purpose. The record type is the *user's*
/// bridged struct — it has to be, because codegen parses only their crate — so
/// nothing in this module can name it, and the `Hook` would otherwise have to
/// be generic, which a `static` cannot be.
type Emit = dyn for<'a> Fn(&Record<'a>) -> bool + Send + Sync + 'static;

/// The one slot. See [`Hook`] for the mechanism it shares with
/// [`crate::runtime`]'s async-context hook.
static HOOK: Hook<Emit> = Hook::new();

/// Whether *this* module won `log::set_logger`.
///
/// Not a pointer comparison against `log::logger()`: [`Forwarder`] and `log`'s
/// own `NopLogger` are both zero-sized statics, and Rust does not promise
/// distinct addresses for distinct ZST statics, so that comparison can report
/// "ours" when nothing is installed at all.
///
/// Two concurrent *first* [`install`]s are not a supported sequence — the loser
/// would be told a foreign logger holds the slot, which is misleading rather
/// than unsound. Only the first is restricted: once this is `true`, an
/// [`install`] from any thread is safe, which is what the per-actor recipe
/// relies on natively (the actor's install runs on the actor's own thread).
static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Records accepted and not delivered. Monotonic for the life of the process,
/// so a consumer that cares reads it twice and subtracts.
static DROPPED: AtomicU64 = AtomicU64::new(0);

std::thread_local! {
    /// Whether this thread is inside [`Forwarder::log`] right now.
    static IN_LOGGER: Cell<bool> = const { Cell::new(false) };
}

/// Held for the duration of one delivery on one thread.
struct Reentrancy;

impl Reentrancy {
    /// `None` if this thread is already delivering a record — the caller must
    /// then drop the record rather than recurse.
    ///
    /// `try_with`, not `with`: a TLS destructor that logs runs after this
    /// thread's `IN_LOGGER` has been destroyed, and `with` panics there. A
    /// panic out of `log::Log::log` is worse than a dropped record from a
    /// thread that is going away, so the inaccessible case takes the same
    /// branch as the re-entrant one.
    fn enter() -> Option<Reentrancy> {
        match IN_LOGGER.try_with(|flag| flag.replace(true)) {
            Ok(false) => Some(Reentrancy),
            Ok(true) | Err(_) => None,
        }
    }
}

impl Drop for Reentrancy {
    fn drop(&mut self) {
        let _ = IN_LOGGER.try_with(|flag| flag.set(false));
    }
}

/// The process's `log::Log`, installed at most once and never replaced. What
/// *is* replaceable is [`HOOK`], which is the whole point: `log` gives out its
/// one slot for the life of the process, and a hot restart needs a new sink.
struct Forwarder;

static FORWARDER: Forwarder = Forwarder;

impl Log for Forwarder {
    /// `log`'s own level filter, and nothing else. Per-target filtering belongs
    /// to the caller — either through `log::set_max_level`, or by having `map`
    /// read `record.target()` — rather than in a second filter here that would
    /// have to be configured through an API of its own.
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &Record) {
        // Declared first, so it is dropped **last**. `emit` below drops before
        // it, and dropping the last clone of the registration runs the user's
        // closure destructor — which drops the `StreamSink`, which posts the
        // stream's end event. That must happen while this thread is still
        // marked in-logger: a `Drop` that logs would otherwise recurse into a
        // registration that is halfway gone.
        let Some(_guard) = Reentrancy::enter() else {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let Some(emit) = HOOK.get() else {
            // No registration. Reachable in two ways, both worth counting: a
            // record racing an `uninstall`, and every record after the logger
            // retired itself below — which is exactly how "the consumer went
            // away and you have been logging into nothing since" becomes a
            // number instead of silence.
            DROPPED.fetch_add(1, Ordering::Relaxed);
            return;
        };
        if !emit(record) {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            // The Dart consumer cancelled, or its isolate is gone. Retire, or
            // every later record pays `map` and a full encode to be refused.
            //
            // `clear_if`, not `clear`: this thread may have been holding
            // `emit` across a hot restart that already installed a live
            // registration, and clearing *that* would take the app's logging
            // away for good. The max level is deliberately left alone for the
            // same reason — a `set_max_level(Off)` here would land after the
            // restart's `set_max_level(Info)` and silently filter everything.
            HOOK.clear_if(&emit);
        }
    }

    /// Nothing is buffered here: `add` posts eagerly, and what happens after
    /// the post is the Dart consumer's business. There is nothing to flush.
    fn flush(&self) {}
}

/// A logger that is not ours already holds `log`'s one global slot.
///
/// Returned rather than panicked, because both sides of this are legitimate:
/// an app that wants `env_logger` on native and this on web is making a real
/// choice, and so is one that treats the collision as a bug. `log` has no
/// uninstall, so once this is returned it is returned forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForeignLogger;

impl std::fmt::Display for ForeignLogger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "frustrate::logging::install: another logger (env_logger, \
             fern, …) already called log::set_logger, and log allows that \
             exactly once per process. Remove the other logger's init, or \
             forward to this sink from inside it",
        )
    }
}

impl std::error::Error for ForeignLogger {}

/// Take `log`'s global slot, unless somebody else already has it.
///
/// Split out from [`install`] so the decision can be tested: the real
/// `log::set_logger` succeeds once per *process*, and `cargo test` is one
/// process, so a test of the foreign-logger branch cannot use it without
/// wrecking every other test in the binary.
fn claim(installed: &AtomicBool, set: impl FnOnce() -> bool) -> Result<(), ForeignLogger> {
    if installed.load(Ordering::Acquire) {
        return Ok(());
    }
    if set() {
        installed.store(true, Ordering::Release);
        Ok(())
    } else {
        Err(ForeignLogger)
    }
}

/// Send every record `log` accepts to `sink`, shaped by `map`.
///
/// `max_level` is `log`'s own filter (`log::set_max_level`), not a second one:
/// the `log!` macros check it before they build a `Record` at all, so a level
/// this call excludes costs nothing anywhere. It is a parameter rather than
/// something this function picks because `log`'s default is
/// [`LevelFilter::Off`] — an install that did not set it would deliver
/// nothing, and an install that set it unconditionally to `Trace` would
/// silently undo a caller's earlier `set_max_level`.
///
/// # Calling it again
///
/// Legal and load-bearing. Flutter hot restart re-runs Dart `main()` in the
/// same native process, so the second call arrives with the first sink
/// pointing at an isolate that no longer exists. The old registration is
/// displaced and dropped (outside the slot's critical section — see [`Hook`]),
/// which drops the old sink, which posts its stream's end event: the previous
/// Dart `StreamController` sees `onDone` and the new one starts receiving.
///
/// # Where it is installed
///
/// The logger is a process global on native and an **instance** global on
/// web, because `log`'s slot is a `static` and a web actor is a separate wasm
/// instance with its own memory. So one install covers every thread — pool
/// workers, actor threads — on native, and covers only the page instance on
/// web. An actor whose code logs needs its own install, reached through a
/// method on the actor.
///
/// A registration holds its sink, so on native it pins the isolate the way every
/// open stream does — for an app that is the point, and for anything with an
/// end it is why [`uninstall`] exists. A test suite that installs and never
/// releases passes and then does not exit; under Bazel that is a TIMEOUT with
/// an empty log, which looks like a hang in the code under test and is not.
///
/// # Errors
///
/// [`ForeignLogger`] if something else already called `log::set_logger`.
/// Nothing is installed in that case and `max_level` is not touched, so the
/// sink is dropped on the way out and the Dart stream closes cleanly rather
/// than hanging open on a logger that will never write to it.
pub fn install<T, F>(
    max_level: LevelFilter,
    sink: StreamSink<T>,
    map: F,
) -> Result<(), ForeignLogger>
where
    T: Send + 'static,
    F: for<'a> Fn(&Record<'a>) -> T + Send + Sync + 'static,
{
    // Claim first. If this fails, `sink` and `map` die here and the caller's
    // stream closes; a hook set before the claim would have stranded them.
    claim(&INSTALLED, || log::set_logger(&FORWARDER).is_ok())?;
    HOOK.set(Arc::new(move |record: &Record<'_>| sink.add(map(record))));
    // After the hook, so a record in the gap is filtered by the macro rather
    // than reaching a logger with nowhere to put it.
    log::set_max_level(max_level);
    Ok(())
}

/// Stop forwarding: drop the registration and set `log`'s max level to
/// [`LevelFilter::Off`], so the macros stop building records.
///
/// The Rust-side release lever. Dart's is cancelling the subscription, which
/// reaches the same end by a different route — the *next* record's `add`
/// returns false and the logger retires itself — so a cancelled stream leaves
/// exactly one record's worth of work behind, and one counted drop.
///
/// Dropping the registration drops the sink, which posts the stream's end
/// event, so the Dart consumer sees `onDone`. `log::set_logger` is not undone
/// (there is no such call): a later [`install`] re-registers into the same slot
/// and works.
///
/// Not concurrent-safe against [`install`]: the two are lifecycle calls, made
/// in order. (Two *installs* racing is fine after the first one; see
/// [`INSTALLED`].)
pub fn uninstall() {
    log::set_max_level(LevelFilter::Off);
    HOOK.clear();
}

/// How many records this logger accepted and did not deliver — see the module
/// docs for the two ways that happens.
///
/// Bridge it if you want the number in Dart; it is a plain `u64`, and a `u64`
/// crosses as a `BigInt`.
pub fn dropped() -> u64 {
    DROPPED.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{self, StreamEvent, Terminal};

    /// `log`'s logger, this module's `HOOK`, and `DROPPED` are all process
    /// globals, and `cargo test` runs this binary's tests in parallel threads.
    /// Every test that installs takes this first.
    ///
    /// Crate-wide ([`crate::serialize_globals`]) rather than local to this
    /// module, because the interference is not only between logging tests: any
    /// module that logs writes into whichever sink is installed, and
    /// `crate::resident` does.
    fn serialize() -> std::sync::MutexGuard<'static, ()> {
        crate::serialize_globals()
    }

    /// Records delivered so far, as `(level, message)`.
    fn drain(probe: &testing::StreamProbe<(String, String)>) -> Vec<(String, String)> {
        probe
            .take_events()
            .into_iter()
            .filter_map(|e| match e {
                StreamEvent::Item(item) => Some(item),
                _ => None,
            })
            .collect()
    }

    fn level_and_message(record: &Record<'_>) -> (String, String) {
        (record.level().to_string(), record.args().to_string())
    }

    #[test]
    fn records_reach_the_sink_and_the_level_filter_applies() {
        let _guard = serialize();
        let (sink, probe) = testing::stream::<(String, String)>();
        install(LevelFilter::Info, sink, level_and_message).expect("no foreign logger");

        log::info!("hello");
        log::debug!("filtered out by max_level");
        log::error!("boom");

        assert_eq!(
            drain(&probe),
            vec![
                ("INFO".to_string(), "hello".to_string()),
                ("ERROR".to_string(), "boom".to_string()),
            ]
        );
        uninstall();
    }

    /// The property the whole re-entrancy guard exists for, asked by *doing*
    /// it: a `map` closure that logs. The outer record must still arrive, the
    /// inner one must be counted and discarded, and the call must return.
    #[test]
    fn a_record_logged_from_inside_the_mapper_is_dropped_not_recursed() {
        let _guard = serialize();
        let (sink, probe) = testing::stream::<(String, String)>();
        install(LevelFilter::Trace, sink, |record| {
            // Exactly what a `Display` impl that logs does to its formatter.
            log::warn!("from inside the mapper");
            level_and_message(record)
        })
        .expect("no foreign logger");

        let before = dropped();
        log::info!("outer");

        assert_eq!(
            drain(&probe),
            vec![("INFO".to_string(), "outer".to_string())],
            "the outer record must be delivered, exactly once"
        );
        assert_eq!(
            dropped() - before,
            1,
            "the re-entrant record must be counted, not delivered and not recursed"
        );
        uninstall();
    }

    /// A panic out of `map` must not leave the thread permanently marked
    /// in-logger. Without the `Drop` guard the flag stays `true` and every
    /// later record on that thread is dropped for the life of the process —
    /// counted, but the thread is dark.
    #[test]
    fn a_panicking_mapper_does_not_wedge_its_thread() {
        let _guard = serialize();
        // Also `post::test_lock`, which is the crate-wide lock for swapping the
        // *process* panic hook. `serialize()` only orders this module against
        // itself, and the swap below is global: while a silent hook stands in
        // place of `panic`'s location recorder, every sibling test that asserts
        // a `PanicReport::location` sees `None` and fails.
        let _hook = crate::post::test_lock();
        let (sink, probe) = testing::stream::<(String, String)>();
        install(LevelFilter::Trace, sink, |record| {
            if record.args().to_string() == "explode" {
                panic!("the mapper panicked");
            }
            level_and_message(record)
        })
        .expect("no foreign logger");

        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let exploded = std::panic::catch_unwind(|| log::info!("explode"));
        std::panic::set_hook(hook);
        assert!(exploded.is_err(), "the mapper's panic must propagate");

        log::info!("after");
        assert_eq!(
            drain(&probe),
            vec![("INFO".to_string(), "after".to_string())],
            "the thread must still be delivering after a mapper panic"
        );
        uninstall();
    }

    /// The hot-restart contract, at the level this module owns it: a second
    /// install replaces the first, and the first stream is closed rather than
    /// abandoned open.
    #[test]
    fn re_installing_closes_the_previous_stream_and_redirects() {
        let _guard = serialize();
        let (first_sink, first) = testing::stream::<(String, String)>();
        install(LevelFilter::Trace, first_sink, level_and_message).expect("no foreign logger");
        log::info!("before");

        let (second_sink, second) = testing::stream::<(String, String)>();
        install(LevelFilter::Trace, second_sink, level_and_message).expect("still ours");
        log::info!("after");

        assert_eq!(drain(&first), vec![("INFO".to_string(), "before".to_string())]);
        assert_eq!(
            first.terminal(),
            Some(Terminal::Closed),
            "the displaced sink must be dropped, which ends its Dart stream"
        );
        assert_eq!(drain(&second), vec![("INFO".to_string(), "after".to_string())]);
        uninstall();
    }

    /// A cancelled subscription costs one more record, then nothing: `add`
    /// reports the cancellation, the logger retires itself, and every record
    /// after that is counted rather than encoded.
    #[test]
    fn a_cancelled_stream_retires_the_logger() {
        let _guard = serialize();
        let (sink, probe) = testing::stream::<(String, String)>();
        install(LevelFilter::Trace, sink, level_and_message).expect("no foreign logger");
        probe.cancel();

        let before = dropped();
        log::info!("first after cancel");
        log::info!("second after cancel");
        assert_eq!(dropped() - before, 2);
        assert!(drain(&probe).is_empty());

        // Retired, not merely refusing: a fresh install works, which it would
        // not if the slot still held the dead registration.
        let (next_sink, next) = testing::stream::<(String, String)>();
        install(LevelFilter::Trace, next_sink, level_and_message).expect("still ours");
        log::info!("live again");
        assert_eq!(drain(&next), vec![("INFO".to_string(), "live again".to_string())]);
        uninstall();
    }

    /// After `uninstall`, `log`'s max level is `Off`, so the macros short
    /// circuit before a record is even built.
    #[test]
    fn uninstall_closes_the_stream_and_stops_the_macros() {
        let _guard = serialize();
        let (sink, probe) = testing::stream::<(String, String)>();
        install(LevelFilter::Trace, sink, level_and_message).expect("no foreign logger");
        uninstall();

        assert_eq!(probe.terminal(), Some(Terminal::Closed));
        assert_eq!(log::max_level(), LevelFilter::Off);
        let before = dropped();
        log::info!("nobody is listening");
        assert_eq!(
            dropped() - before,
            0,
            "a filtered record never reaches the logger, so it is not a drop"
        );
    }

    /// The claim decision, through its seam. The real `log::set_logger` is
    /// once per process and this binary is one process, so the foreign branch
    /// is unreachable through the real call without breaking every other test
    /// here.
    #[test]
    fn claiming_is_once_and_a_foreign_logger_is_reported() {
        let installed = AtomicBool::new(false);

        assert_eq!(claim(&installed, || false), Err(ForeignLogger));
        assert!(
            !installed.load(Ordering::Acquire),
            "a failed claim must not mark the slot as ours"
        );

        assert_eq!(claim(&installed, || true), Ok(()));
        assert!(installed.load(Ordering::Acquire));

        assert_eq!(
            claim(&installed, || panic!("set_logger must not be called twice")),
            Ok(()),
            "once it is ours, a re-install skips log::set_logger entirely"
        );
    }
}
