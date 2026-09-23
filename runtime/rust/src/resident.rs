//! The thread each live `#[bridge(resident)]` object was born on, and the
//! isolate that owns it — so that a use from the wrong thread is refused, and
//! an isolate's exit can say what it took with it.
//!
//! A resident is `Box<T>` with **no `Send` bound** ([`crate::handle::
//! resident_new`]). That is the model's point and its whole cost: no thread
//! but the one that made the object may touch it or run its `Drop`.
//!
//! # A Dart isolate is not a thread, and that is why this file exists
//!
//! "Born on the caller, used on the caller, freed on the caller" would be
//! enough if a caller were a thread. It is not. **Measured** on this machine
//! with a `dart:ffi` probe over a per-OS-thread tag: the standalone VM's root
//! isolate ran 200 event-loop turns, with real suspension between them, on one
//! OS thread — while six `Isolate.spawn`ed isolates each ran their turns
//! across six different pool threads. Within one turn the thread is stable;
//! across turns, in a spawned isolate, it is not.
//!
//! So an object built in a spawned isolate and used one `await` later is
//! reached from a different thread. That is precisely the transfer
//! `crate::handle`'s module doc says `Send` exists to license, and resident
//! has no `Send`. The model's contract is therefore about **threads, not
//! isolates**: one thread builds the object and only that thread may touch it.
//! Which isolates satisfy that is a property of how each is scheduled, not
//! something this file can enumerate — an isolate that migrates between turns
//! breaks it, and `Isolate.spawn`ed isolates measurably do.
//!
//! A contract nothing checks is a contract that gets violated silently, in
//! this case into undefined behaviour. So it is checked: every acquisition
//! compares the calling thread against the object's birth thread, and a
//! mismatch is refused rather than served.
//!
//! # What this reasons over, and what it does when it cannot decide
//!
//! Thread identity only. It compares a per-OS-thread tag recorded when the
//! object was minted against the tag of the thread now asking. It says nothing
//! about whether the handle is live, or of the right type — those are
//! `crate::handle`'s `unsafe` preconditions and are upheld by generated code.
//!
//! **A handle with no entry is refused**, not allowed. Every mint records one
//! and only a reclaim removes it, so a missing entry means the object is gone
//! — already freed, or drained by an isolate's exit — and serving a reference
//! into it is the worse of the two mistakes available. A check that cannot
//! vouch for a handle says no.
//!
//! For that to be a *complete* argument the registry has to be complete, and
//! it is keyed by the handle, which is an address. Every zero-sized `Box`
//! shares one dangling address ([`crate::handle::alias_entry`] says why), so
//! two zero-sized residents would collide on one entry and retiring either
//! would unregister both. Rather than reason about that, resident refuses a
//! zero-sized type outright, at compile time, in
//! [`crate::handle::resident_new`]. The restriction costs nothing real: a
//! thread-affine *context* — the thing this model exists for — holds
//! something.
//!
//! # Native-only, and not an omission
//!
//! Not because web has one thread — under `wasm-threads` it does not: pool
//! workers instantiate the same module on the *same shared linear memory*
//! (`crate::pool`), so an address is reachable from a worker in principle.
//! The reason is that no resident acquisition is ever emitted onto one. Every
//! resident member derives `sync` and FR0079 refuses a resident anywhere in an
//! async member's signature, so every acquisition sits on a sync arm; a sync
//! arm runs inline on its caller, which on web is the one Dart thread, and so
//! do the mint and both drop exports. Nothing reaches a worker to check.
//!
//! There is also no death to attribute: web has no isolates, and the page
//! cannot go away underneath a module that is still running. So web pays for
//! none of this — `resident_new` and `resident_drop` compile to the bare `Box`
//! operations there.
//!
//! # What one resident costs on native
//!
//! One uncontended write lock at mint, one at reclaim, one at
//! [`frustrate_resident_attach`], and one uncontended **read** lock per
//! acquisition — a receiver or a parameter, so most calls take one. That read
//! is small against what a bridge call already pays around it (an FFI
//! crossing, a `catch_unwind`, the codec), which is why the check is
//! affordable on every call rather than sampled.
//!
//! The registry, rather than a header in the box, is where the birth thread
//! lives, for two reasons. It is keyed by handle and so is type-agnostic,
//! which matters because a `dyn Trait` acquisition resolves a different
//! concrete type per impl tag and a header would need a cell type per
//! implementor in the emitted match. And it leaves the container alone, so a
//! consuming `self: Box<Self>` still hands back the original allocation
//! ([`crate::handle::confined_adopt`]) instead of moving the value into a
//! fresh one.
//!
//! # Why the isolate arrives from Dart
//!
//! Rust cannot work it out. A resident is minted on a **sync** call, and
//! `frustrate_call_sync` carries no `call_id` — so, unlike every async
//! completion, there is no isolate id in the request (`crate::post`,
//! `ISOLATE_SHIFT`). The transport, on the other hand, knows its own id: it
//! was handed one by `frustrate_init_dl`, and it stamps it in when it
//! registers the handle's finalizer.

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{OnceLock, PoisonError, RwLock};

/// A stable identifier for the current OS thread, allocated on first sight.
///
/// A counter and a `Cell` rather than `std::thread::current().id()`: the
/// latter clones an `Arc`, which is an atomic read-modify-write on a path that
/// runs on every acquisition. This is a thread-local load and, once per
/// thread, one `fetch_add`.
///
/// Tags start at 1 so that 0 can mean "never asked", and they are never
/// reused. A thread that exits takes its tag with it, so a later thread cannot
/// impersonate it and satisfy a check it should fail.
fn thread_tag() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    thread_local! {
        static TAG: Cell<u64> = const { Cell::new(0) };
    }
    TAG.with(|t| {
        if t.get() == 0 {
            t.set(NEXT.fetch_add(1, Ordering::Relaxed));
        }
        t.get()
    })
}

/// One live resident.
struct Entry {
    /// The Rust type name, as codegen wrote it into the `resident_new` call.
    name: &'static str,
    /// The thread that built it, and the only one that may touch it.
    born: u64,
    /// The owning isolate, filled in by [`frustrate_resident_attach`] when the
    /// Dart handle registers its finalizer. `None` for the window between
    /// minting and that call — an object in that window is attributed to no
    /// isolate, and so is never reported as one's leak.
    isolate: Option<u32>,
}

fn live() -> &'static RwLock<HashMap<u64, Entry>> {
    static LIVE: OnceLock<RwLock<HashMap<u64, Entry>>> = OnceLock::new();
    LIVE.get_or_init(|| RwLock::new(HashMap::new()))
}

/// What has already been lost, by type name. Bounded by the number of resident
/// types in the program rather than by the number of objects, so it can
/// accumulate for the life of the process without growing without bound.
fn lost() -> &'static RwLock<BTreeMap<&'static str, usize>> {
    static LOST: OnceLock<RwLock<BTreeMap<&'static str, usize>>> = OnceLock::new();
    LOST.get_or_init(|| RwLock::new(BTreeMap::new()))
}

/// Record a freshly minted resident. Called by [`crate::handle::resident_new`].
pub(crate) fn record(handle: u64, name: &'static str) {
    live()
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(handle, Entry { name, born: thread_tag(), isolate: None });
}

/// Retire a resident that is being freed, answering its type name. `None` if
/// there was no entry — a handle nothing minted here.
pub(crate) fn retire(handle: u64) -> Option<&'static str> {
    live()
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&handle)
        .map(|e| e.name)
}

/// Why an acquisition cannot be served.
///
/// Two different facts, kept apart because they have different fixes and a
/// reader told the wrong one looks in the wrong place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refused {
    /// The object is there; the caller is not its thread. Carries the type
    /// name, which is what the report and the panic are built from.
    OtherThread(&'static str),
    /// No entry. Every mint records one and only a reclaim removes it, so this
    /// is an object that is already gone — freed, or drained by an isolate's
    /// exit — and there is no type name to give because there is no object.
    Gone,
}

/// Whether the calling thread is the one that built `handle`.
///
/// See the module doc on why a handle with no entry is refused rather than
/// waved through.
pub(crate) fn on_owning_thread(handle: u64) -> Result<(), Refused> {
    let map = live().read().unwrap_or_else(PoisonError::into_inner);
    match map.get(&handle) {
        Some(e) if e.born == thread_tag() => Ok(()),
        Some(e) => Err(Refused::OtherThread(e.name)),
        None => Err(Refused::Gone),
    }
}

/// The message a refusal carries. One text per reason, because the panic a
/// member call raises and the log line a reclaim writes are the same fact and
/// must not drift apart.
pub(crate) fn refusal(why: Refused, what: &str) -> String {
    match why {
        Refused::OtherThread(name) => wrong_thread(name, what),
        Refused::Gone => format!(
            "frustrate: a resident handle was {what} after the object behind it \
             was released. Every resident is registered when it is built and \
             deregistered when it is freed, so this handle names nothing — it \
             was disposed, consumed, or lost with an isolate that exited \
             holding it. This is a use-after-free the runtime refused rather \
             than served"
        ),
    }
}

/// The message a wrong-thread acquisition is refused with. One text, because
/// the panic a member call raises and the log line a reclaim writes are the
/// same fact and must not drift apart.
pub(crate) fn wrong_thread(name: &str, what: &str) -> String {
    format!(
        "frustrate: resident `{name}` was {what} from a thread other than the one \
         that built it. A resident object has no `Send` bound, so no other thread \
         may touch it. A Dart isolate is not pinned to an OS thread — it can run \
         each event-loop turn on a different one, which an `Isolate.spawn`ed \
         isolate routinely does — so an object minted in one turn and reached in \
         the next may be reached from elsewhere. Use `{name}` from an isolate that \
         does not migrate, or declare it `confined` (which requires `Send` and may \
         then be built and freed anywhere) or `actor` (which owns a thread of its \
         own and may be reached from any isolate)"
    )
}

/// Number of live residents. The counterpart of `actor::host_count`, and used
/// the same way: a leak pin measures it across a construct/reclaim cycle.
pub fn live_count() -> usize {
    live().read().unwrap_or_else(PoisonError::into_inner).len()
}

/// Everything lost so far, by type name, with a count. Two things reach here:
/// an isolate that exited holding residents, and a reclaim that arrived on the
/// wrong thread and so could not be served.
///
/// Durable rather than drained: the isolate that could have consumed the
/// report is usually the one that died, so the reader is somebody else — the
/// isolate a hot restart brought up, or a test.
pub fn leak_report() -> Vec<(String, usize)> {
    lost()
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .map(|(name, n)| ((*name).to_string(), *n))
        .collect()
}

/// Add to the tally. Takes `&'static str`s so the map's keys cost nothing.
fn tally(by_type: &BTreeMap<&'static str, usize>) {
    let mut lost = lost().write().unwrap_or_else(PoisonError::into_inner);
    for (name, n) in by_type {
        *lost.entry(name).or_default() += n;
    }
}

/// Say something, without letting the saying break the caller.
///
/// `catch_unwind` for the reason `crate::panic`'s listener call has one: a
/// record goes through the embedder's own `map` closure, and a panic there
/// would unwind out of an `extern "C"` frame.
fn warn(message: String) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        log::warn!(target: "frustrate", "{message}");
    }));
}

/// Record a resident that cannot be reclaimed because the reclaim arrived on
/// the wrong thread. Called by [`crate::handle::resident_drop`], which then
/// does *not* free the object: running a `!Send` value's `Drop` on a thread
/// that may not touch it is exactly the undefined behaviour this model is
/// arranged to avoid, and leaking is the only other answer.
pub(crate) fn strand(name: &'static str) {
    tally(&BTreeMap::from([(name, 1)]));
    warn(wrong_thread(name, "reclaimed"));
}

/// Account for everything `isolate` was still holding, and say so.
///
/// Called from `post::mark_gone`, the single entry point for isolate death,
/// which reaches it from an `extern "C"` frame and therefore **must not block
/// and must not panic**. Both hold: the locks are taken one at a time and
/// released before anything else runs, they are poison-tolerant, and the one
/// thing here that runs code somebody else wrote — the log record — is caught.
///
/// The entries are removed rather than marked. They name memory nothing will
/// ever free, so keeping them would make [`live_count`] permanently wrong
/// about what is *reclaimable*, which is the only question it is asked.
pub(crate) fn reap_isolate(isolate: u32) {
    let doomed: Vec<&'static str> = {
        let mut map = live().write().unwrap_or_else(PoisonError::into_inner);
        let ids: Vec<u64> = map
            .iter()
            .filter(|(_, e)| e.isolate == Some(isolate))
            .map(|(h, _)| *h)
            .collect();
        ids.iter().filter_map(|h| map.remove(h)).map(|e| e.name).collect()
    };
    if doomed.is_empty() {
        return;
    }
    let mut by_type: BTreeMap<&'static str, usize> = BTreeMap::new();
    for name in doomed {
        *by_type.entry(name).or_default() += 1;
    }
    tally(&by_type);
    let listed = by_type
        .iter()
        .map(|(name, n)| format!("{n} × {name}"))
        .collect::<Vec<_>>()
        .join(", ");
    warn(format!(
        "isolate {isolate} exited holding {listed}. A resident object is freed by \
         the thread that built it and by nothing else, so these are leaked for the \
         life of the process. Call dispose() before the isolate ends — a Flutter \
         hot restart is the usual way to reach this, because it tears the isolate \
         down without running any Dart teardown"
    ));
}

/// How many resident objects this process has lost, all-time.
///
/// The count [`leak_report`] details. Read by the Dart transport for
/// `FrustrateRuntime.residentLeakCount`, which is the only way the loss is
/// visible to an app that has installed no `log` sink.
#[no_mangle]
pub extern "C" fn frustrate_resident_leaked() -> u64 {
    lost()
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .values()
        .sum::<usize>() as u64
}

/// Whether the calling thread may reclaim `handle`: 1 yes, 0 no.
///
/// Asked by the native transport immediately before an eager `dispose()`, so
/// that a reclaim it cannot serve is a thrown `StateError` on the caller's own
/// stack rather than a line in a log nobody installed. The drop path itself
/// cannot say so — it is an `extern "C"` export, where an unwind aborts.
///
/// Deliberately not consulted on the GC path: a `Finalizer` callback has no
/// caller to tell, and an error thrown from one reaches the isolate's error
/// handler.
#[no_mangle]
pub extern "C" fn frustrate_resident_reclaimable(handle: u64) -> u8 {
    u8::from(on_owning_thread(handle).is_ok())
}

/// Tell the runtime which isolate owns a resident handle.
///
/// Called by the native transport's resident `HandleDrop.attach`, once per
/// handle, when the Dart object registers its finalizer — the first moment
/// anything knows both facts at once. A handle this is never called for stays
/// unattributed and is never reported as anyone's leak.
#[no_mangle]
pub extern "C" fn frustrate_resident_attach(handle: u64, isolate: u32) {
    if let Some(e) = live()
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .get_mut(&handle)
    {
        e.isolate = Some(isolate);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handle::{resident_drop, resident_new};

    fn leaked(name: &str) -> Option<usize> {
        leak_report().into_iter().find(|(n, _)| n == name).map(|(_, n)| n)
    }

    /// `live_count` is one number for the process, so a test that measures a
    /// delta across it must not run beside a test that mints — and the harness
    /// runs them on several threads by default. Every test here takes this, so
    /// the registry each one sees holds only what that test put in it.
    ///
    /// It is [`crate::serialize_globals`], the same lock the logging tests
    /// take, because the interference runs both ways: those tests install a
    /// sink, and [`reap_isolate`] and [`strand`] write into it.
    fn exclusive() -> std::sync::MutexGuard<'static, ()> {
        crate::serialize_globals()
    }

    /// The ordinary life: mint, attach, free. Nothing is left behind, and the
    /// isolate that owned it can exit reporting nothing.
    #[test]
    fn a_disposed_resident_is_not_an_isolate_s_leak() {
        let _x = exclusive();
        let before = live_count();
        let h = resident_new(String::from("hi"), "ProbeDisposed");
        frustrate_resident_attach(h, 7);
        assert_eq!(live_count(), before + 1);
        unsafe { resident_drop::<String>(h) };
        assert_eq!(live_count(), before);
        reap_isolate(7);
        assert_eq!(leaked("ProbeDisposed"), None);
    }

    /// The isolate died holding one. The object is unreachable and stays
    /// allocated — the point is that the *report* names it, since the memory
    /// is beyond reach by construction.
    #[test]
    fn an_exiting_isolate_reports_what_it_held_by_name() {
        let _x = exclusive();
        let h = resident_new(String::from("held"), "ProbeAbandoned");
        frustrate_resident_attach(h, 4242);
        reap_isolate(4242);
        assert_eq!(leaked("ProbeAbandoned"), Some(1));
        // Reaping again finds nothing: the entry was removed, not marked, so a
        // second exit notice for the same isolate cannot double-count.
        reap_isolate(4242);
        assert_eq!(leaked("ProbeAbandoned"), Some(1));
    }

    /// Attribution is what makes the sweep safe to run at all: one isolate's
    /// exit must not touch another's objects.
    #[test]
    fn another_isolate_s_residents_survive_the_sweep() {
        let _x = exclusive();
        let mine = resident_new(1i64, "ProbeMine");
        let theirs = resident_new(2i64, "ProbeTheirs");
        frustrate_resident_attach(mine, 11);
        frustrate_resident_attach(theirs, 12);
        reap_isolate(11);
        assert_eq!(leaked("ProbeMine"), Some(1));
        assert_eq!(leaked("ProbeTheirs"), None);
        unsafe { resident_drop::<i64>(theirs) };
    }

    /// A handle nothing attached belongs to no isolate, so no isolate's exit
    /// may claim it. Reachable for real: the mint and the attach are two
    /// calls, and an isolate can die between them.
    #[test]
    fn an_unattached_resident_is_nobody_s_leak() {
        let _x = exclusive();
        let h = resident_new(0u8, "ProbeUnattached");
        reap_isolate(99);
        assert_eq!(leaked("ProbeUnattached"), None);
        unsafe { resident_drop::<u8>(h) };
    }

    /// The soundness check. The object is minted here and reached from a
    /// second thread, which is what a spawned isolate's next event-loop turn
    /// does — measured, see the module doc.
    #[test]
    fn a_second_thread_is_refused() {
        let _x = exclusive();
        let h = resident_new(std::rc::Rc::new(5i32), "ProbeAffine");
        assert!(on_owning_thread(h).is_ok(), "the minting thread may touch it");
        let verdict = std::thread::spawn(move || on_owning_thread(h))
            .join()
            .expect("the probe thread does not panic");
        assert_eq!(
            verdict,
            Err(Refused::OtherThread("ProbeAffine")),
            "another thread may not"
        );
        unsafe { resident_drop::<std::rc::Rc<i32>>(h) };
    }

    /// A reclaim that arrives on the wrong thread leaks rather than dropping,
    /// and says so. Dropping there would run the value's `Drop` on a thread
    /// that may not touch it.
    #[test]
    fn a_wrong_thread_reclaim_leaks_instead_of_dropping() {
        let _x = exclusive();
        let h = resident_new(String::from("stranded"), "ProbeStranded");
        let live_before = live_count();
        std::thread::spawn(move || unsafe { resident_drop::<String>(h) })
            .join()
            .expect("the reclaim does not panic");
        assert_eq!(leaked("ProbeStranded"), Some(1));
        assert_eq!(
            live_count(),
            live_before - 1,
            "the entry is retired: nothing will ever reclaim this object"
        );
    }

    /// A handle with no entry is refused, and refused **as a use-after-free**
    /// rather than as a thread mistake. Every mint records one and only a
    /// reclaim removes it, so a missing entry means the object is already
    /// gone; telling the reader to check their threads would send them looking
    /// in the wrong place.
    #[test]
    fn an_unknown_handle_is_refused_as_a_released_object() {
        let _x = exclusive();
        assert_eq!(on_owning_thread(0xdead_beef), Err(Refused::Gone));
        let msg = refusal(Refused::Gone, "read");
        assert!(msg.contains("was released"), "{msg}");
        assert!(!msg.contains("thread other than"), "{msg}");
    }

    /// A retired handle is an unknown handle: use-after-free is refused by the
    /// same rule, on the thread that owned it.
    #[test]
    fn a_reclaimed_handle_is_refused_on_its_own_thread() {
        let _x = exclusive();
        let h = resident_new(7i64, "ProbeGone");
        unsafe { resident_drop::<i64>(h) };
        assert_eq!(on_owning_thread(h), Err(Refused::Gone));
    }
}
