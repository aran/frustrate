//! A declared width is the width the pool is *built* at — measured by how many
//! jobs actually run at once, not by asking `pool::width()` what it thinks.
//!
//! Its own test binary, and that is the whole reason this file exists rather
//! than another `#[test]` in `pool.rs`: the width is fixed once per process, so
//! a declaration that lands first is only observable in a process where nothing
//! else has touched the pool. Cargo gives each `tests/*.rs` its own binary, and
//! `//runtime/rust:declared_pool_width_test` gives Bazel the same. One `#[test]`
//! per file for the same reason — cargo runs a binary's tests on parallel
//! threads, and a second test here would race this one for the declaration.
//!
//! The width asked for is one *wider* than this build would have chosen, so the
//! measurement cannot pass by accident on a machine whose core count happens to
//! match: the default pool could never muster it.

use frustrate::pool;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Jobs that have arrived at the current rendezvous.
static ARRIVED: AtomicUsize = AtomicUsize::new(0);

/// Whether `jobs` jobs handed to the pool all ran **at the same time**: each
/// arrives, then waits for every other to arrive before returning. On a pool
/// narrower than `jobs` the ones that fit time out and say so, then release
/// their workers so the rest can run and time out in turn — so this answers,
/// rather than hanging, in either direction.
///
/// A width measurement, not a timing heuristic: the only way every job reports
/// `true` is `jobs` workers running concurrently.
fn all_at_once(jobs: usize, budget: Duration) -> bool {
    ARRIVED.store(0, Ordering::SeqCst);
    let (tx, rx) = mpsc::channel();
    for _ in 0..jobs {
        let tx = tx.clone();
        pool::spawn(move || {
            ARRIVED.fetch_add(1, Ordering::SeqCst);
            let deadline = Instant::now() + budget;
            // Sleep rather than spin: on a one-core machine two pool threads
            // reach the rendezvous only by yielding to each other.
            while ARRIVED.load(Ordering::SeqCst) < jobs && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            tx.send(ARRIVED.load(Ordering::SeqCst) >= jobs).unwrap();
        });
    }
    drop(tx);
    // Every verdict is collected before any of them is judged. Short-circuiting
    // on the first `false` would drop the receiver while jobs are still queued —
    // and a job's `send` then fails, panicking *on a pool worker*, which has no
    // `catch_unwind` and would die there with its message swallowed by libtest's
    // output capture.
    let verdicts: Vec<bool> = rx.iter().take(jobs).collect();
    verdicts.into_iter().all(|together| together)
}

#[test]
fn the_pool_is_built_at_the_declared_width() {
    // `width()` before anything is fixed is a forecast — reading it here must
    // not commit the pool to it, or the declaration below could not be made.
    let declared = pool::width() + 1;
    assert_eq!(
        pool::declare_width(NonZeroUsize::new(declared).unwrap()),
        Ok(())
    );
    assert_eq!(pool::width(), declared, "the declaration is what width reports");

    assert!(
        all_at_once(declared, Duration::from_secs(10)),
        "{declared} jobs must run at once — one more than this build's default, \
         so only the declaration can have produced them"
    );
    assert!(
        !all_at_once(declared + 1, Duration::from_secs(2)),
        "the pool must not be wider than it was declared"
    );

    assert_eq!(pool::width(), declared, "and it stayed there");
    assert_eq!(
        pool::declare_width(NonZeroUsize::new(declared + 1).unwrap()),
        Err(pool::WidthError::AlreadyFixed { width: declared }),
        "the ordering hazard from the other side: a declaration after the pool \
         is built is refused, never a silent no-op"
    );
}
