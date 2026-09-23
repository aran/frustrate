//! The `Drop` of an opaque handle edited.

use frustrate::bridge;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;

static CALLS: AtomicI64 = AtomicI64::new(0);

thread_local! {
    /// A thread-local the patch must share with the launched code rather than
    /// get a fresh copy of: on Mach-O that means binding to the running
    /// image's thread-local descriptor.
    static SEEN: std::cell::Cell<i64> = const { std::cell::Cell::new(0) };
}

struct Tally {
    hits: i64,
    last: i64,
}

static TALLY: Mutex<Tally> = Mutex::new(Tally { hits: 0, last: 0 });

#[bridge(sync)]
pub fn add(a: i64, b: i64) -> i64 {
    a + b
}

/// Counts calls in a static that a patch must share with the launched code.
#[bridge(sync)]
pub fn count() -> i64 {
    let seen = SEEN.with(|s| {
        s.set(s.get() + 1);
        s.get()
    });
    CALLS.fetch_add(1, Ordering::SeqCst) + seen
}

#[bridge(sync)]
pub fn tally(x: i64) -> i64 {
    let mut t = TALLY.lock().unwrap();
    t.hits += 1;
    t.last = x;
    t.hits
}

/// An actor: its own thread, its own state. A patch to a method here has to
/// route `frustrate_actor_call`, a different entry point from the sync one.
#[bridge(actor)]
pub struct Ledger {
    entries: i64,
}

#[bridge]
impl Ledger {
    pub fn new() -> Self {
        Ledger { entries: 0 }
    }

    pub fn record(&mut self, amount: i64) -> i64 {
        self.entries += amount;
        self.entries
    }
}

/// An `async fn`, which routes `frustrate_call_async`.
#[bridge]
pub async fn eventually(x: i64) -> i64 {
    x + 1
}

/// A confined opaque handle with a `Drop`: freeing one goes through the
/// generated `frustrate_drop_Doc` export, a different entry point again.
#[bridge(confined)]
pub struct Doc {
    text: String,
}

#[bridge]
impl Doc {
    pub fn new(text: String) -> Self {
        Doc { text }
    }

    #[bridge(sync)]
    pub fn length(&self) -> i64 {
        self.text.len() as i64
    }
}

impl Drop for Doc {
    fn drop(&mut self) {
        DROPPED.fetch_add(100, Ordering::SeqCst);
    }
}

static DROPPED: AtomicI64 = AtomicI64::new(0);

/// Nothing calls this. The library is compiled with `-Clink-dead-code`, so it
/// is in the image and in the IR all the same, and an edit to it changes
/// nothing that runs.
#[allow(dead_code)]
fn unused(x: i64) -> i64 {
    x * 2
}
