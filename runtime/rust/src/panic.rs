//! The panic listener: one embedder-registered callback told about every Rust
//! panic this runtime observes, so a crash reporter can live on the Rust side
//! of the bridge.
//!
//! Without it the Rust half of an app is unreportable. A Dart `catch` sees
//! only what survived encoding into the response envelope — a message string,
//! no location, no backtrace — and nothing at all for a panic that killed a
//! thread instead of answering a call.
//!
//! ```
//! // once, at init — re-registrable; see `register`
//! frustrate::panic::register(|report| {
//!     let at = match report.location() {
//!         Some(l) => l.to_string(),
//!         None => "<no location>".to_string(),
//!     };
//!     eprintln!("rust panic: {} at {at}", report.message());
//!     if let Some(bt) = report.backtrace() {
//!         eprintln!("{bt}");
//!     }
//! });
//! ```
//!
//! # The contract
//!
//! The listener is called **once per panic that this runtime observes**, on
//! the panicking thread, after the runtime knows what became of the panic and
//! before the answer — if there is one — leaves for Dart. "Observes" is a
//! closed list, and it is closed because every entry funnels through one of
//! three places:
//!
//! | where | what reaches it |
//! |---|---|
//! | [`crate::envelope::panic_envelope`] | every panic that becomes a `STATUS_PANIC` answer: a sync body, a pool job, a cooperative-executor poll, and the two prefix catches generated glue wraps its async and actor dispatch in |
//! | [`crate::actor`]'s `Reap` arm | a user `Drop` that panics during actor teardown, which answers no call and kills the host thread |
//! | `frustrate_web_init`'s panic hook | **every** panic on web, where `panic=abort` means none of the above run |
//!
//! `panic_envelope` is the whole of the first row because it is the only
//! constructor of a `STATUS_PANIC` envelope: a dispatch shape cannot report a
//! panic to Dart without going through it, so a new shape is covered the day
//! it is written rather than the day someone remembers this module.
//!
//! # Where it stops
//!
//! - **A panic that reaches an `extern "C"` frame.** The generated
//!   `frustrate_drop_*` / `frustrate_finalize_*` exports call a user `Drop`
//!   with no catch between it and the boundary, and unwinding out of
//!   `extern "C"` aborts the process. Nothing of ours runs after that, so
//!   there is no frame to report from. Covering the finalizer half alone would
//!   be easy — `stream::finalize_scope` already wraps it — and is deliberately
//!   not done: half-coverage of one hazard is the failure mode this module
//!   exists to avoid.
//! - **Panics on threads the embedder owns**, outside any bridge frame. They
//!   are not the bridge's to see.
//! - **A double panic** — a `Drop` that panics while unwinding — aborts inside
//!   `std` before any funnel is reached.
//!
//! For those, a `std::panic::set_hook` of the embedder's own still fires: the
//! recording hook below **chains** to whatever was installed when the first
//! [`register`] ran, rather than replacing it.
//!
//! # Why this is not just `std::panic::set_hook`
//!
//! A team with Sentry already has a `std` hook, and it fires for every panic
//! in the process — strictly more than the table above. Three reasons it is
//! not the answer here:
//!
//! 1. **On web the runtime owns the hook.** `frustrate_web_init` installs the
//!    panic-attribution shim that ships the message through the
//!    `frustrate.panic` import, and that import is the only thing keeping a
//!    trapping call attributable as a `BridgePanicException` (the trap reaches
//!    the host as a payload-less `RuntimeError`). An embedder who calls
//!    `set_hook` displaces it and silently loses Dart-side attribution. This
//!    listener is how web reporting is expressed *without* that trade.
//! 2. **User code inside a `std` hook cannot be made safe.** A panic raised
//!    inside a hook is not catchable — `std` aborts with "thread panicked
//!    while processing panic. aborting.", and a `catch_unwind` *inside* the
//!    hook does not intercept it. A listener called from a hook could
//!    therefore kill the process, which is exactly what "a slow or panicking
//!    listener must not break the bridge" forbids. Called from a funnel it
//!    runs on an ordinary stack, where `catch_unwind` works.
//! 3. **A hook fires before anyone knows the disposition.** The funnel fires
//!    once the runtime has decided whether this panic became a call's answer
//!    or killed a thread.
//!
//! The hook is still the only source of a *location*, which is why one is
//! installed — to **record**, never to call user code. See
//! [`install_recorder`].
//!
//! # What registering nothing costs
//!
//! Nothing here sits on a path a non-panicking call takes. [`observed`] is
//! reached only from code that already has a panic in hand, and its first act
//! is [`Hook::get`], which for an empty slot is one acquire load and no lock
//! (`hook.rs`, `an_empty_hook_answers_without_the_lock`). No `std` panic hook
//! is installed until the first [`register`], so a process that never
//! registers pays nothing at all, not even a hook call.
//!
//! After an [`unregister`] the recorder stays installed — restoring the
//! previous hook would restore it over whatever the embedder has set since —
//! so a panic then pays one acquire load and one chained call, and captures
//! nothing.

use crate::hook::Hook;
use std::any::Any;
use std::sync::Arc;

/// The registered listener. `Fn`, not `FnOnce`: it runs for every panic.
type Listener = dyn Fn(&PanicReport) + Send + Sync + 'static;

/// The one slot.
///
/// A [`Hook`] rather than a slot spelled out here: that module carries the
/// presence flag that keeps the empty case to one acquire load, and the rule
/// that a displaced listener — the embedder's closure and its captures — dies
/// outside the critical section.
static LISTENER: Hook<Listener> = Hook::new();

/// Where a panic was raised, as `file:line:column`.
///
/// Owned rather than the borrowed [`std::panic::Location`] a panic hook is
/// handed: that borrow ends when the hook returns, and this has to outlive it
/// to reach the funnel. [`Display`](std::fmt::Display) renders it exactly as
/// `Location` does, so it can be pasted anywhere one would be.
pub struct PanicLocation {
    file: String,
    line: u32,
    column: u32,
}

impl PanicLocation {
    /// The source file, as `file!()` would give it.
    pub fn file(&self) -> &str {
        &self.file
    }

    /// The 1-based line.
    pub fn line(&self) -> u32 {
        self.line
    }

    /// The 1-based column.
    pub fn column(&self) -> u32 {
        self.column
    }
}

impl std::fmt::Display for PanicLocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}:{}", self.file, self.line, self.column)
    }
}

/// What a listener is told about one panic.
///
/// Three things, and deliberately no fourth saying *what became of* the panic
/// (answered a call / killed a host thread). A reporter cannot act on that
/// differently — it files a crash either way — and the backtrace already names
/// the frame it happened in.
///
/// Owned rather than borrowed from the panic payload, so the type has no
/// lifetime parameter: `Fn(&PanicReport)` is a shape closure inference handles
/// without a written-out `for<'a>` at every call site, and one `String` on a
/// path that has already unwound is not a cost worth that.
pub struct PanicReport {
    message: String,
    location: Option<PanicLocation>,
    backtrace: Option<std::backtrace::Backtrace>,
}

impl PanicReport {
    /// The panic message: the payload of a `panic!("…")`, and a fixed
    /// placeholder for a payload that is neither `&str` nor `String`
    /// (`std::panic::panic_any` with some other type).
    ///
    /// The same string the call's `STATUS_PANIC` envelope carries, so a report
    /// and the `BridgePanicException` Dart saw name the same failure.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Where the panic was raised, when a recording hook was installed in time
    /// to see it.
    ///
    /// `None` in exactly two cases, both stated on [`register`]: the panic
    /// happened before the first `register`, or the embedder installed their
    /// own `std::panic::set_hook` *after* it and displaced the recorder.
    pub fn location(&self) -> Option<&PanicLocation> {
        self.location.as_ref()
    }

    /// The stack at the panic site, captured before unwinding began.
    ///
    /// `None` where the platform has no backtrace support (wasm) or the
    /// capture came back empty. Present regardless of `RUST_BACKTRACE`: a
    /// Flutter app has no environment for anyone to set it in, so the capture
    /// is forced.
    pub fn backtrace(&self) -> Option<&std::backtrace::Backtrace> {
        self.backtrace.as_ref()
    }
}

/// Register the process's panic listener, replacing whatever was there.
///
/// # The contract you are taking on
///
/// - **The listener may be called from any thread**, concurrently, which is
///   why it is `Send + Sync`. Bridge calls run on pool workers, actor host
///   threads, and the calling thread.
/// - **A panicking listener is swallowed.** It cannot become the call's
///   answer: the envelope is already built when the listener runs, and it
///   carries the *original* panic. The listener's own panic is loud where
///   panics are loud — it goes through the `std` hook chain, so it prints —
///   but it changes nothing on the wire.
/// - **A slow listener delays the answer it is reporting on.** The panicking
///   call's response cannot leave until the listener returns; on the sync path
///   that is the `frustrate_call_sync` frame the caller is blocked in. No
///   thread is spawned and no timeout imposed, because either would be a
///   mechanism whose only job is to tolerate a bug in code the embedder
///   controls. Report asynchronously if reporting is slow.
///
/// # Location and the `std` hook
///
/// The first `register` on an unwinding target installs a `std` panic hook
/// that records the panic's location and backtrace into a thread-local, then
/// calls the hook that was installed before it. It never calls the listener —
/// see the module docs, reason 2.
///
/// The consequence is an ordering the embedder controls: a
/// `std::panic::set_hook` of your own **after** `register` displaces the
/// recorder, and [`PanicReport::location`] goes `None` from then on. Chain to
/// the previous hook yourself if you want both, or install yours first.
///
/// # Re-registration
///
/// Last-wins, for the reason [`crate::runtime::register`] sets out at length:
/// Flutter hot restart re-runs Dart `main()` in this same native process while
/// Rust statics persist, so a second `register` is an ordinary call from a
/// working app. Refusing it would pin the first run's listener — and its
/// captures, which may reference an isolate that no longer exists — for the
/// life of the process. The recorder is installed once regardless.
pub fn register(listener: impl Fn(&PanicReport) + Send + Sync + 'static) {
    // The displaced listener is dropped outside the slot's critical section by
    // `Hook::set`; it is the embedder's closure, and its `Drop` is theirs too.
    LISTENER.set(Arc::new(listener));
    install_recorder();
}

/// Drop the registration; panics stop being reported.
///
/// A listener already running keeps running — the slot holds an `Arc` and
/// [`observed`] clones it out before calling — so this never yanks a report
/// out from under itself. The recording hook stays installed; the module docs
/// say what that costs.
pub fn unregister() {
    LISTENER.clear();
}

/// The message a panic payload carries.
///
/// One extraction, shared by the wire envelope
/// ([`crate::envelope::panic_envelope`]) and by the report, so the two can
/// never disagree about what the panic said.
pub(crate) fn message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("non-string panic payload")
}

/// Tell the listener about a panic this runtime just observed.
///
/// Callers are the rows of the module docs' table, and nothing else should
/// become one without extending that table.
pub(crate) fn observed(message: &str) {
    let Some(listener) = LISTENER.get() else {
        return;
    };
    // `take`, not a read: the record is consumed so a panic a user body caught
    // for itself cannot lend its location to the *next* one that reaches a
    // funnel. Taken before the listener runs, because the listener's own panic
    // would otherwise overwrite it under us.
    let site = SITE.with(|s| s.borrow_mut().take()).unwrap_or(Site {
        location: None,
        backtrace: None,
    });
    deliver(&*listener, message, site);
}

/// Tell the listener about a panic seen from **inside** a `std` panic hook —
/// the web arm, where `panic=abort` means no funnel will ever run.
///
/// Safe here only because the process is already dying: a listener that panics
/// under `panic=abort` traps, and that trap is indistinguishable from the one
/// the original panic was about to cause anyway. On an unwinding target this
/// would be the abort the module docs describe, which is why the recorder
/// records and this is called from nowhere else.
#[cfg(any(panic = "abort", test))]
pub(crate) fn observed_in_hook(info: &std::panic::PanicHookInfo<'_>) {
    let Some(listener) = LISTENER.get() else {
        return;
    };
    let site = Site {
        location: info.location().map(owned_location),
        // Nothing to capture where this arm runs: `panic=abort` is wasm today,
        // and `force_capture` there is `Unsupported`.
        backtrace: None,
    };
    deliver(&*listener, message(info.payload()), site);
}

/// Build the report and hand it over, absorbing the listener's own panic.
///
/// The `catch_unwind` is the "a panicking listener must not break the bridge"
/// guarantee **on an unwinding target**, and it is one arm rather than two
/// cfg'd ones because under `panic=abort` it compiles to a plain call.
///
/// It is not a guarantee on today's web build, and nothing here pretends
/// otherwise: the only caller there is [`observed_in_hook`], where a listener's
/// panic aborts before any catch could see it. That arm carries its own reason
/// for being acceptable, and it is about the process already dying — not about
/// this line.
fn deliver(listener: &Listener, message: &str, site: Site) {
    let report = PanicReport {
        message: message.to_owned(),
        location: site.location,
        backtrace: site.backtrace,
    };
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| listener(&report)));
}

/// What the recording hook stashes for a funnel to pick up.
struct Site {
    location: Option<PanicLocation>,
    backtrace: Option<std::backtrace::Backtrace>,
}

std::thread_local! {
    /// The site of the most recent panic on this thread, or `None`.
    ///
    /// Written by the recording hook, `take`n by [`observed`]. Overwritten
    /// rather than accumulated: a panic a user body caught for itself leaves a
    /// record nobody consumes, and the next panic on this thread must not
    /// inherit it. Nothing can panic between the hook and the funnel on the
    /// same thread — a `Drop` that panics while unwinding is a double panic,
    /// which aborts — so the record a funnel takes is the panic it is
    /// reporting on.
    static SITE: std::cell::RefCell<Option<Site>> = const { std::cell::RefCell::new(None) };
}

/// Copy a hook's borrowed location out, so it can outlive the hook call.
fn owned_location(l: &std::panic::Location<'_>) -> PanicLocation {
    PanicLocation {
        file: l.file().to_owned(),
        line: l.line(),
        column: l.column(),
    }
}

/// Install the location recorder, once per process.
///
/// Gated on `panic = "unwind"` rather than on the target family, so a future
/// unwinding web build records locations the day it exists without anyone
/// editing a cfg — the complement of `frustrate_web_init`'s `panic = "abort"`
/// shim, which is where the listener is called from on today's web build.
///
/// It records and chains; it never calls the listener. The presence check
/// comes first, so a process that registered and then unregistered pays one
/// acquire load per panic and captures nothing.
#[cfg(panic = "unwind")]
fn install_recorder() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if LISTENER.get().is_some() {
                // Forced, not `capture()`: `capture()` obeys `RUST_BACKTRACE`,
                // and a Flutter app has no environment in which anyone set it.
                let backtrace = std::backtrace::Backtrace::force_capture();
                let captured =
                    backtrace.status() == std::backtrace::BacktraceStatus::Captured;
                let site = Site {
                    location: info.location().map(owned_location),
                    backtrace: captured.then_some(backtrace),
                };
                SITE.with(|s| *s.borrow_mut() = Some(site));
            }
            previous(info);
        }));
    });
}

/// No recorder where nothing unwinds: on `panic=abort` the hook that would
/// record is the hook the listener is called *from* (`frustrate_web_init`), so
/// a second one would have nothing to hand it.
#[cfg(not(panic = "unwind"))]
fn install_recorder() {}

/// Register a listener that appends every message it is told, and answer with
/// the log.
///
/// Shared with the sibling modules whose funnels this one cannot drive: the
/// cooperative executor and the actor host loop each own a harness, and a
/// coverage test belongs beside the harness that can reach the arm.
#[cfg(test)]
pub(crate) fn record_for_tests() -> Arc<std::sync::Mutex<Vec<String>>> {
    let log: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let sink = log.clone();
    register(move |report| {
        sink.lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(report.message().to_string())
    });
    log
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// What a test listener recorded, so an assertion can read it after the
    /// panic that produced it has been turned into bytes.
    #[derive(Default)]
    struct Recorded {
        messages: Vec<String>,
        locations: Vec<Option<String>>,
        backtraces: Vec<bool>,
    }

    /// Register a listener that records, and answer with what it recorded.
    fn recording() -> Arc<Mutex<Recorded>> {
        let log: Arc<Mutex<Recorded>> = Arc::default();
        let sink = log.clone();
        register(move |report| {
            let mut r = sink.lock().unwrap_or_else(|e| e.into_inner());
            r.messages.push(report.message().to_string());
            r.locations.push(report.location().map(|l| l.to_string()));
            r.backtraces.push(report.backtrace().is_some());
        });
        log
    }

    /// Serializes every test here against each other **and** against the
    /// other tests in this crate that swap the process `std` panic hook
    /// (`stream`, `executor`, `runtime`) or raise a panic that reaches a
    /// funnel. Registering installs the recorder for the whole binary, and a
    /// sibling swapping a hook over it while one of these asserts on a
    /// location would make it flaky.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        crate::post::test_lock()
    }

    /// The sync funnel: `envelope::run` catches, `panic_envelope` reports.
    #[test]
    fn a_panicking_sync_body_is_reported_with_its_message_and_location() {
        let _serial = serial();
        let log = recording();
        let at = line!() + 1;
        let bytes = crate::envelope::delivered_run(|| panic!("sync body exploded"));
        unregister();

        assert_eq!(bytes[0], crate::envelope::STATUS_PANIC);
        let r = log.lock().unwrap();
        assert_eq!(r.messages, ["sync body exploded"]);
        let located = r.locations[0].as_deref().expect(
            "no location: the recording panic hook was not installed, so a \
             reporter gets a message with nowhere to group it",
        );
        assert!(
            located.contains("panic.rs") && located.contains(&at.to_string()),
            "the location is not the panic site (expected line {at}): {located}"
        );
        assert!(
            r.backtraces[0],
            "no backtrace was captured on a target that has them"
        );
    }

    /// The message the listener gets and the message on the wire are the same
    /// string, because one extraction produces both.
    #[test]
    fn the_reported_message_is_the_one_the_envelope_carries() {
        let _serial = serial();
        let log = recording();
        let bytes = crate::envelope::delivered_run(|| panic!("{} {}", "formatted", 7));
        unregister();

        let mut r = crate::codec::ByteReader::new(&bytes[1..]);
        let on_the_wire = r.read_string();
        assert_eq!(log.lock().unwrap().messages, [on_the_wire]);
    }

    /// A payload that is neither `&str` nor `String` still reports something a
    /// reporter can file, rather than nothing.
    #[test]
    fn a_non_string_payload_is_still_reported() {
        let _serial = serial();
        let log = recording();
        let bytes = crate::envelope::delivered_run(|| std::panic::panic_any(7u32));
        unregister();

        assert_eq!(bytes[0], crate::envelope::STATUS_PANIC);
        assert_eq!(log.lock().unwrap().messages, ["non-string panic payload"]);
    }

    /// A listener that panics must not become the call's answer, and must not
    /// take the process with it.
    #[test]
    fn a_panicking_listener_cannot_break_the_call() {
        let _serial = serial();
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        register(move |_| {
            seen.fetch_add(1, Ordering::SeqCst);
            panic!("the listener itself is broken");
        });
        let bytes = crate::envelope::delivered_run(|| panic!("the real failure"));
        unregister();

        assert_eq!(calls.load(Ordering::SeqCst), 1, "the listener ran");
        assert_eq!(bytes[0], crate::envelope::STATUS_PANIC);
        let mut r = crate::codec::ByteReader::new(&bytes[1..]);
        assert_eq!(
            r.read_string(),
            "the real failure",
            "the listener's panic displaced the real one on the wire"
        );
    }

    /// A panic a user body caught for itself leaves a recorded site nobody
    /// consumed. The next panic that *does* reach a funnel must report its own
    /// site, not that stale one.
    #[test]
    fn a_caught_panic_does_not_lend_its_location_to_the_next_one() {
        let _serial = serial();
        let log = recording();
        let mut swallowed_at = 0;
        let mut escaping_at = 0;
        let bytes = crate::envelope::delivered_run(|| {
            swallowed_at = line!() + 1;
            let _ = std::panic::catch_unwind(|| panic!("swallowed by the body"));
            escaping_at = line!() + 1;
            panic!("the one that escapes")
        });
        unregister();

        assert_eq!(bytes[0], crate::envelope::STATUS_PANIC);
        let r = log.lock().unwrap();
        assert_eq!(
            r.messages,
            ["the one that escapes"],
            "the swallowed panic was reported: no funnel saw it, so nothing \
             should have"
        );
        let located = r.locations[0].as_deref().expect("located");
        assert!(
            located.contains(&format!(":{escaping_at}:")),
            "the report carries the swallowed panic's location (line \
             {swallowed_at}) rather than the escaping one's (line \
             {escaping_at}): {located}"
        );
    }

    /// Nothing registered is the ordinary case and must reach no user code.
    /// The empty read's cost is `hook.rs`'s to prove; what is this module's is
    /// that a cleared slot really does stop the reports.
    #[test]
    fn unregistering_stops_the_reports() {
        let _serial = serial();
        let log = recording();
        let _ = crate::envelope::delivered_run(|| panic!("while registered"));
        unregister();
        let _ = crate::envelope::delivered_run(|| panic!("after unregistering"));

        assert_eq!(log.lock().unwrap().messages, ["while registered"]);
    }

    /// The web arm, driven directly: on `panic=abort` the hook *is* the
    /// funnel, so the same report has to come out of it.
    ///
    /// `observed_in_hook` is compiled under `cfg(test)` on every target so
    /// this host run covers a path only a wasm build otherwise takes. A
    /// `PanicHookInfo` cannot be constructed outside `std`, so the arm is
    /// driven through a real hook — which displaces the recorder for the
    /// duration, exactly as the web build has no recorder at all.
    #[test]
    fn the_hook_arm_reports_the_same_report() {
        let _serial = serial();
        let log = recording();
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(observed_in_hook));
        let at = line!() + 1;
        let _ = std::panic::catch_unwind(|| panic!("web-shaped panic"));
        std::panic::set_hook(previous);
        unregister();

        // **Presence, not exclusivity**, and that is the difference between a
        // claim this test can own and one it cannot. For the length of the
        // window above, the arm under test *is* the process `std` hook, so
        // every panic anywhere in the binary is reported into this log —
        // including a `#[should_panic]` codec test that has nothing to do with
        // any of this. Asserting the log equals one entry is therefore an
        // assertion about the whole process, which no test can hold; asserting
        // that the arm produced this panic's report, with this panic's
        // location, is the claim actually being made. One-report-per-panic is
        // held by the funnel tests above, which do not displace the hook.
        let r = log.lock().unwrap();
        let i = r
            .messages
            .iter()
            .position(|m| m == "web-shaped panic")
            .expect("the hook arm reported nothing, so a web build reports nothing");
        let located = r.locations[i].as_deref().expect(
            "the hook arm dropped the location it is handed directly, which is \
             the only one web will ever have",
        );
        assert!(located.contains(&format!(":{at}:")), "{located}");
    }
}
