//! The `locked` model's read/write lock: async acquisition, and a release the
//! browser main thread may execute.
//!
//! # Why the runtime owns this
//!
//! Any async lock keeps a queue of waiters, and that queue needs a lock of its
//! own. Every off-the-shelf async `RwLock` reaches for `std::sync::Mutex`
//! there, on the *release* edge as well as the acquire one — tokio's
//! `RwLockReadGuard::drop` -> `Semaphore::release` ->
//! `add_permits_locked(added, self.waiters.lock())` is unconditional, and
//! `async_lock`'s `Event::notify` takes a `std::sync::MutexGuard` for every
//! writer and for the last reader. On `wasm32 + atomics` that mutex is std's
//! futex backend, so a *contended* release executes `memory.atomic.wait32`,
//! which the browser main thread may not execute — it traps. Swapping crates
//! does not fix a shape they share.
//!
//! So the waiter list is guarded by the primitive frustrate already uses
//! wherever the main thread may take a lock: [`crate::spin::SpinLock`] on
//! `+atomics` wasm, `std::sync::Mutex` everywhere else — the choice
//! [`crate::spin::Lock`] makes, with the reasoning for the `cfg` key there.
//! Owning the lock is what makes that choice reachable at all, and it turns
//! three prose arguments about a third party's internals into tests over this
//! file.
//!
//! # What the main thread can reach
//!
//! `try_read`/`try_write` are a single compare-exchange on one `AtomicUsize`
//! and touch no queue. Releasing a guard is one read-modify-write on that same
//! word, and takes the waiter list **only** when the word says a waiter is
//! queued on this object — where the list is the spin lock, which cannot
//! execute a wait instruction, and waking a waiter happens after the section
//! is released. So on threaded web no path from a synchronous member reaches
//! `memory.atomic.wait32`, and `on_contention = "error"` is portable.
//!
//! State the property that narrowly: the uncontended path takes no lock, and a
//! release takes the waiter list only when this object has a waiter
//! (`an_uncontended_release_never_enters_the_waiter_list` is that claim, with
//! its positive control). It is not "the main thread never spins" — an
//! ordinary async bridge call already spins boundedly on the executor's
//! registry and on the pool's job queue.
//!
//! `blocking_read`/`blocking_write` are the exception, and are compiled off
//! wasm only: they exist for `on_contention = "block"`, which waits to
//! *acquire* by declaration and stays native-only (FR0008).
//!
//! # Fairness, which is observable
//!
//! The queue is FIFO and a queued writer blocks later readers, matching
//! tokio's `RwLock`: a waiter that cannot be satisfied absorbs the available
//! permits before queuing (`batch_semaphore.rs`'s `poll_acquire` drives
//! `permits` to 0 and queues for the remainder), so a `try_read` behind a
//! queued writer fails there too. "Contended" therefore means *held **or**
//! queued behind a writer* under `on_contention = "error"`, on every target,
//! and that is not an implementation detail: it is how often a Dart caller
//! sees `ContentionException`. The alternative — let `try_read` barge past a
//! queued writer — throws less often and lets a stream of readers starve a
//! writer indefinitely, which is also the reason `handle::lock_plan` refuses
//! a shared/shared duplicate rather than re-entering a reader.
//!
//! # Invariants
//!
//! 1. `state` is the whole of "who holds this lock": the `WRITER` bit, a
//!    reader count, and a `WAITERS` bit that is set **exactly while the queue
//!    holds an ungranted waiter**. `WAITERS` is written only under the waiter
//!    lock; `WRITER` and the count are also written by the lock-free
//!    fast paths, so every write is a compare-exchange or a bit-masked RMW
//!    that leaves the other fields alone.
//! 2. The queue is **sorted by waiter id ascending**: ids are minted in push
//!    order under the lock, grants pop the front, and a cancellation removes
//!    one element. So "am I still queued" is `front().id <= mine` — O(1) — and
//!    finding my own node is a binary search.
//! 3. A waiter leaves the queue in exactly two ways: it is **granted** (the
//!    granting thread has already moved `state` on its behalf), or its own
//!    future is dropped. Since a future is never polled and dropped at the
//!    same time, a poll that finds its id gone knows it was granted.
//! 4. Therefore a granted waiter whose future is dropped before it observes
//!    the grant **must hand the grant back** — release the permit exactly as a
//!    guard's `Drop` would, and grant whatever that frees to the next waiter.
//!    That is the wake-then-cancel race, and it is reachable in production:
//!    `executor` cancels a call by dropping its task, which drops a pending
//!    acquire future.
//! 5. **No wakeup is lost, although `WAITERS` is read outside the lock.** A
//!    release moves `state` and then grants only if the value it displaced had
//!    `WAITERS` set. A waiter that queued after that read is not served by
//!    that grant — and does not need to be, because the last thing its own
//!    enqueue section does is run the same grant loop, which re-reads `state`
//!    and finds it free. So for any release/enqueue pair, at least one of the
//!    two grants sees both halves. This is the argument to check first if a
//!    locked object is ever observed wedged.
//!
//! The one thing this split costs is that "the queue head is not grantable" is
//! not a section-local invariant — a `try_read` may take a reader between two
//! sections — so it cannot be asserted on the way out of one. What stands in
//! for it is the fixed point: the randomised driver below drops every guard
//! and every future and asserts that `state` is back to exactly zero with an
//! empty queue, which is conservation checked where it is decidable.
//!
//! [`crate::spin`]'s rules shape the code and are load-bearing here. Nothing is
//! woken inside a critical section — a `Waker` reaches the executor's own
//! registry, which is not a leaf — and nothing that owns anything is dropped
//! inside one, because dropping an `async_task` waker can call the schedule
//! callback synchronously. Wakers and removed nodes are therefore carried out
//! of the section and disposed of outside it, and two tests fence each half by
//! re-entering the lock from a waker's `wake` and from its `Drop`.
//!
//! These sections are at the loose end of the bound: the grant loop pops as
//! many waiters as the lock can satisfy, and a cancellation searches the
//! queue. Both are bounded by the calls in flight on one object and touch only
//! containers this runtime owns, which is what the bound requires; neither is
//! the single container operation the common case is.
//!
//! The lock has **no `Drop` impl**, and neither guard's release touches the
//! value. `handle::locked_take` and `handle::locked_drop` rest on that: they
//! are the one synchronous path to a locked object that acquires nothing.

use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};

use crate::spin::Lock;

/// A writer holds the lock.
const WRITER: usize = 0b01;
/// The queue holds at least one waiter that has not been granted. Set and
/// cleared only under the waiter lock; read by the lock-free fast paths, which
/// refuse while it is set so that a queued writer is not barged past.
const WAITERS: usize = 0b10;
/// One reader. The reader count occupies everything above the two flag bits,
/// so it saturates at `usize::MAX / 4` — an unreachable number of concurrent
/// calls on one object, and unguarded for that reason rather than by omission.
/// Every arithmetic on it adds or subtracts exactly this, and a holder always
/// owns at least one unit, so no add or subtract can carry into the flags.
const READER: usize = 0b100;

/// A try-lock found the object contended. Unit, because which object and which
/// member are the caller's own facts — the generated glue has both and puts
/// them in the `ContentionError` it returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TryLockError;

impl fmt::Display for TryLockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the lock is held, or a waiter is queued ahead of this acquisition")
    }
}

impl std::error::Error for TryLockError {}

/// What a test can see of a lock's whole state at one instant.
#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
struct Probe {
    readers: usize,
    queued: usize,
    writer: bool,
    /// `None` when nothing is queued. Whether the head wants the *write* lock
    /// is what says the queue is settled: a reader head with no writer holding
    /// is one `hand_over` should already have granted.
    head_writes: Option<bool>,
}

#[cfg(test)]
impl Probe {
    /// Nothing held, nothing queued.
    const IDLE: Probe = Probe {
        readers: 0,
        queued: 0,
        writer: false,
        head_writes: None,
    };

    const fn of(readers: usize, queued: usize, writer: bool, head_writes: Option<bool>) -> Probe {
        Probe { readers, queued, writer, head_writes }
    }
}

/// One queued acquisition.
struct Waiter {
    /// Minted under the waiter lock, in push order — so the queue is sorted by
    /// it (invariant 2).
    id: u64,
    /// Whether this waiter wants the write lock.
    write: bool,
    /// `None` between the push and the end of the first poll: a waiter granted
    /// inside its own enqueue has no one to wake.
    waker: Option<Waker>,
}

#[derive(Default)]
struct Waiters {
    queue: VecDeque<Waiter>,
    next_id: u64,
}

/// An async read/write lock whose guards are `Send`.
///
/// Read the module docs before changing anything here; the interesting part is
/// which operations may take the waiter list and what may happen inside it.
pub struct RwLock<T> {
    state: AtomicUsize,
    waiters: Lock<Waiters>,
    value: UnsafeCell<T>,
    /// How many times a *release* has entered the waiter list. The claim the
    /// whole synchronous contract rests on is that an uncontended one never
    /// does, and this is what turns that from prose into a test. Test-only, so
    /// release builds carry neither the field nor the increment.
    #[cfg(test)]
    sections: AtomicUsize,
}

// Safety: `value` is reached only through a guard, and `state` licenses the
// guards — one writer or any number of readers, never both. `T: Send` because
// a write guard hands `&mut T` to whichever thread holds it; `T: Sync` because
// read guards hand out `&T` on several at once. This is the bound `handle`'s
// `locked_new` states and the one that makes `Arc<RwLock<T>>: Send`.
unsafe impl<T: Send + Sync> Sync for RwLock<T> {}

impl<T> RwLock<T> {
    pub fn new(value: T) -> Self {
        RwLock {
            state: AtomicUsize::new(0),
            waiters: Lock::new(Waiters::default()),
            value: UnsafeCell::new(value),
            #[cfg(test)]
            sections: AtomicUsize::new(0),
        }
    }

    /// Take the value back out. Acquires nothing: there is no guard, no queue
    /// operation and no `Drop` impl to run, which is what makes
    /// `handle::locked_take` portable.
    ///
    /// `handle::locked_take` reaches this only through a successful
    /// `Arc::try_unwrap`, so nothing else holds the lock and nothing can be
    /// queued on it. The `debug_assert` is what checks that rather than
    /// assuming it, and costs release builds nothing.
    pub fn into_inner(self) -> T {
        debug_assert_eq!(
            self.state.load(Ordering::Relaxed),
            0,
            "into_inner with the lock held or a waiter queued"
        );
        self.value.into_inner()
    }

    /// `&mut self` is exclusive access already, so no lock is involved.
    pub fn get_mut(&mut self) -> &mut T {
        self.value.get_mut()
    }

    /// Whether an acquisition is queued on this lock right now — one relaxed
    /// load, and a snapshot by nature.
    ///
    /// Public because it is the only way to *observe* the state the release
    /// path branches on. A release enters the waiter list exactly when this is
    /// true, so a test that cannot see it cannot show that it reached that
    /// path rather than the two-compare-exchange one beside it
    /// (`tests/test_api`'s `LockProbe`, and the browser case it drives).
    pub fn has_waiter(&self) -> bool {
        self.state.load(Ordering::Relaxed) & WAITERS != 0
    }

    /// Read without waiting. `Err` while a writer holds the lock **or** a
    /// waiter is queued ahead (see "Fairness" above).
    pub fn try_read(&self) -> Result<RwLockReadGuard<'_, T>, TryLockError> {
        let mut s = self.state.load(Ordering::Relaxed);
        loop {
            if s & (WRITER | WAITERS) != 0 {
                return Err(TryLockError);
            }
            match self.state.compare_exchange_weak(
                s,
                s + READER,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Ok(RwLockReadGuard { lock: self }),
                Err(actual) => s = actual,
            }
        }
    }

    /// Write without waiting. `Err` unless the lock is completely free and
    /// nothing is queued.
    pub fn try_write(&self) -> Result<RwLockWriteGuard<'_, T>, TryLockError> {
        match self
            .state
            .compare_exchange(0, WRITER, Ordering::Acquire, Ordering::Relaxed)
        {
            Ok(_) => Ok(RwLockWriteGuard { lock: self }),
            Err(_) => Err(TryLockError),
        }
    }

    /// Acquire a read guard, waiting cooperatively. Dropping the returned
    /// future before it resolves cancels the acquisition (invariants 3, 4).
    pub fn read(&self) -> Read<'_, T> {
        Read {
            acquire: Acquire::new(self, false),
        }
    }

    /// Acquire a write guard, waiting cooperatively.
    pub fn write(&self) -> Write<'_, T> {
        Write {
            acquire: Acquire::new(self, true),
        }
    }

    /// Acquire a read guard by parking this thread.
    ///
    /// For `on_contention = "block"`, which is native-only (FR0008): waiting
    /// to acquire is what the contract declares, and a thread that parks on
    /// the browser main thread traps under `+atomics` and spins forever
    /// without it. Compiled off wasm for that reason, so the wasm module holds
    /// no `std::thread::park` reached from here at all — the same property
    /// `tools/check_no_park.dart` gates for the whole single-threaded module.
    #[cfg(not(target_family = "wasm"))]
    pub fn blocking_read(&self) -> RwLockReadGuard<'_, T> {
        crate::pool::block_on(self.read())
    }

    /// [`Self::blocking_read`] for the write lock.
    #[cfg(not(target_family = "wasm"))]
    pub fn blocking_write(&self) -> RwLockWriteGuard<'_, T> {
        crate::pool::block_on(self.write())
    }

    // ---------------------------------------------------------- releasing --

    /// Give back one reader. Takes the waiter list only when the word says
    /// someone is queued.
    fn release_read(&self) {
        let prev = self.state.fetch_sub(READER, Ordering::Release);
        if prev & WAITERS != 0 {
            self.grant();
        }
    }

    /// Give back the write lock.
    fn release_write(&self) {
        let prev = self.state.fetch_and(!WRITER, Ordering::Release);
        if prev & WAITERS != 0 {
            self.grant();
        }
    }

    /// Hand the lock to whoever is at the head of the queue, as far as it will
    /// go, and wake them **outside** the critical section.
    fn grant(&self) {
        #[cfg(test)]
        self.sections.fetch_add(1, Ordering::Relaxed);
        // Built here rather than inside: growing it is the one allocation the
        // section makes, and `spin`'s rules sanction nesting into the
        // allocator (a strict leaf) and nothing else. Empty `Vec`s do not
        // allocate, so an idle grant allocates nothing at all.
        let mut woken = Vec::new();
        self.waiters.with(|w| self.grant_locked(w, &mut woken));
        for waker in woken {
            waker.wake();
        }
    }

    /// The grant loop. Runs under the waiter lock; every wake it decides on
    /// goes into `woken` for the caller to deliver afterwards.
    ///
    /// Each iteration is one compare-exchange and one `pop_front`. It stops at
    /// the first waiter the lock cannot satisfy, which is what keeps the order
    /// FIFO: a queued writer is never stepped over.
    fn grant_locked(&self, w: &mut Waiters, woken: &mut Vec<Waker>) {
        while let Some(front) = w.queue.front() {
            let want_write = front.write;
            // What `WAITERS` must say once this one is off the queue.
            let others = w.queue.len() - 1;
            if !self.hand_over(want_write, others) {
                break;
            }
            // The waiter is now off the queue and the word says it holds the
            // lock; its own poll learns that by not finding its id (invariant
            // 3). What is left of the node is an id, a flag and a `None` — no
            // destructor — so letting it die here breaks no `spin` rule.
            let mut granted = w.queue.pop_front().expect("front() just returned one");
            if let Some(waker) = granted.waker.take() {
                woken.push(waker);
            }
            if want_write {
                // Exclusive: nothing behind it can be granted beside it.
                break;
            }
        }
        // `WAITERS` is written only under this lock, so this reads its own
        // section's value and is exact. Both directions bite: set over an
        // empty queue is a `ContentionException` nothing earned, and clear
        // over a waiting one is an object nobody will ever wake.
        debug_assert_eq!(
            self.state.load(Ordering::Relaxed) & WAITERS != 0,
            !w.queue.is_empty(),
            "WAITERS must say exactly whether an ungranted waiter is queued"
        );
    }

    /// Count one granted waiter into the word and settle `WAITERS` in the
    /// **same** compare-exchange.
    ///
    /// One, never two, and this is a memory-safety rule rather than a
    /// tidiness one: with "clear `WAITERS`, then add the reader" as two
    /// read-modify-writes, a `try_write` fits between them and takes the lock
    /// beside a reader this was about to count. That is two incompatible
    /// holders of `value`, not a fairness blip.
    ///
    /// Grantability is recomputed from the fresh word on every retry. A losing
    /// compare-exchange here can only have lost to a holder *releasing* —
    /// nothing may acquire while `WAITERS` is set — so a retry can find more
    /// room, never less.
    fn hand_over(&self, want_write: bool, others: usize) -> bool {
        let mut s = self.state.load(Ordering::Relaxed);
        loop {
            // A writer needs the lock entirely free; a reader needs only that
            // no writer holds it.
            let grantable = if want_write {
                s & !WAITERS == 0
            } else {
                s & WRITER == 0
            };
            if !grantable {
                return false;
            }
            let held = if want_write {
                WRITER
            } else {
                (s & !WAITERS) + READER
            };
            let next = held | if others > 0 { WAITERS } else { 0 };
            match self
                .state
                .compare_exchange_weak(s, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return true,
                Err(actual) => s = actual,
            }
        }
    }

    /// The word, for tests that check conservation and the settled property
    /// after every operation. Test-only, so release builds carry nothing.
    #[cfg(test)]
    fn probe(&self) -> Probe {
        let s = self.state.load(Ordering::SeqCst);
        let (queued, head_writes) = self
            .waiters
            .with(|w| (w.queue.len(), w.queue.front().map(|n| n.write)));
        Probe {
            readers: s / READER,
            queued,
            writer: s & WRITER != 0,
            head_writes,
        }
    }
}

impl<T: fmt::Debug> fmt::Debug for RwLock<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut d = f.debug_struct("RwLock");
        match self.try_read() {
            Ok(g) => d.field("value", &&*g),
            Err(_) => d.field("value", &"<locked>"),
        }
        .finish()
    }
}

// ------------------------------------------------------------------ guards --

/// Shared access to the value. `Send` when `T: Send + Sync`, because it is a
/// borrow of a lock that is `Sync` under exactly that bound — which is what
/// lets a generated future hold one across an `.await` and still satisfy
/// `executor::spawn`.
pub struct RwLockReadGuard<'a, T> {
    lock: &'a RwLock<T>,
}

impl<T> Deref for RwLockReadGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // Safety: this guard is one of the readers `state` counts, so no
        // writer holds the lock for as long as it lives.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> Drop for RwLockReadGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.release_read();
    }
}

impl<T: fmt::Debug> fmt::Debug for RwLockReadGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

/// Exclusive access to the value.
pub struct RwLockWriteGuard<'a, T> {
    lock: &'a RwLock<T>,
}

impl<T> Deref for RwLockWriteGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // Safety: `state`'s `WRITER` bit is this guard, and it excludes every
        // reader and every other writer.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for RwLockWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // Safety: as `deref`, and this is the only live reference.
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for RwLockWriteGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.release_write();
    }
}

impl<T: fmt::Debug> fmt::Debug for RwLockWriteGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

// ---------------------------------------------------------------- futures --

/// Where one acquisition has got to.
enum Step {
    /// Not yet polled: the fast path has not been tried.
    Fresh,
    /// Queued under this id, not yet granted — or granted and not yet
    /// observed, which is the same thing from here and is why dropping in this
    /// state has to consult the queue.
    Queued(u64),
    /// Resolved. Nothing left to undo.
    Done,
}

/// The acquisition itself, shared by [`Read`] and [`Write`].
struct Acquire<'a, T> {
    lock: &'a RwLock<T>,
    write: bool,
    step: Step,
}

impl<'a, T> Acquire<'a, T> {
    fn new(lock: &'a RwLock<T>, write: bool) -> Self {
        Acquire {
            lock,
            write,
            step: Step::Fresh,
        }
    }

    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        match self.step {
            Step::Done => panic!("frustrate: a lock acquisition was polled after it resolved"),
            Step::Fresh => {
                if self.take_uncontended() {
                    self.step = Step::Done;
                    return Poll::Ready(());
                }
                // Cloned before the section, never inside one: the rule has no
                // exceptions, so there is nothing to check when it is read.
                let waker = cx.waker().clone();
                let mut woken = Vec::new();
                // Carried out and dropped here — see `Drop` below on why a
                // waker may not die inside a section.
                let (id, queued, _unused) = self.lock.waiters.with(|w| {
                    let id = w.next_id;
                    w.next_id += 1;
                    // Pushed with no waker: a waiter granted inside its own
                    // enqueue has nobody to wake, and self-waking a task that
                    // is about to return `Ready` is pure waste.
                    w.queue.push_back(Waiter {
                        id,
                        write: self.write,
                        waker: None,
                    });
                    // Before the grant attempt, so a release racing this
                    // section sees a queue to serve.
                    self.lock.state.fetch_or(WAITERS, Ordering::Relaxed);
                    self.lock.grant_locked(w, &mut woken);
                    // That sweep can grant *this* waiter — the release that
                    // freed the lock may have read `WAITERS` as clear a moment
                    // before the push and skipped its own sweep. So the
                    // membership test comes after it, in the same section, and
                    // a self-grant resolves here rather than parking on a
                    // wake that nobody owes.
                    //
                    // Pushed at the back, and grants come off the front, so
                    // "still queued" is exactly "the back is still me".
                    match w.queue.back_mut() {
                        Some(n) if n.id == id => {
                            n.waker = Some(waker);
                            (id, true, None)
                        }
                        _ => (id, false, Some(waker)),
                    }
                });
                for waker in woken {
                    waker.wake();
                }
                if queued {
                    self.step = Step::Queued(id);
                    Poll::Pending
                } else {
                    self.step = Step::Done;
                    Poll::Ready(())
                }
            }
            Step::Queued(id) => {
                let waker = cx.waker().clone();
                // The displaced waker leaves the section before it is dropped:
                // an `async_task` waker's `Drop` can call the schedule
                // callback, which takes the executor's own lock.
                let (queued, _replaced) = self.lock.waiters.with(|w| {
                    match w.queue.binary_search_by_key(&id, |n| n.id) {
                        Ok(i) => (true, w.queue[i].waker.replace(waker)),
                        // Gone from the queue means granted (invariant 3).
                        Err(_) => (false, Some(waker)),
                    }
                });
                if queued {
                    Poll::Pending
                } else {
                    self.step = Step::Done;
                    Poll::Ready(())
                }
            }
        }
    }

    /// The lock-free fast path. Deliberately not `try_read()`/`try_write()`:
    /// those hand back a guard, and dropping it here to keep only the boolean
    /// would run a release the acquisition has not finished with. The caller
    /// mints the guard once the whole acquisition resolves.
    fn take_uncontended(&self) -> bool {
        if self.write {
            self.lock
                .state
                .compare_exchange(0, WRITER, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
        } else {
            let mut s = self.lock.state.load(Ordering::Relaxed);
            loop {
                if s & (WRITER | WAITERS) != 0 {
                    return false;
                }
                match self.lock.state.compare_exchange_weak(
                    s,
                    s + READER,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return true,
                    Err(actual) => s = actual,
                }
            }
        }
    }
}

impl<T> Drop for Acquire<'_, T> {
    fn drop(&mut self) {
        let Step::Queued(id) = self.step else { return };
        let write = self.write;
        let mut woken = Vec::new();
        // Both of these leave the section before they are dropped: a removed
        // node still owns its waker, and dropping a waker can re-enter the
        // executor.
        let _removed = self.lock.waiters.with(|w| {
            let removed = match w.queue.binary_search_by_key(&id, |n| n.id) {
                Ok(i) => w.queue.remove(i),
                Err(_) => None,
            };
            if removed.is_some() {
                // Still queued: just leave. Taking the head away can unblock
                // what was behind it, so re-run the grant.
                if w.queue.is_empty() {
                    self.lock.state.fetch_and(!WAITERS, Ordering::Relaxed);
                }
            } else if write {
                // Granted and never observed (invariant 4). Hand it back
                // exactly as a guard's `Drop` would, but without re-entering
                // this lock, which is already held.
                self.lock.state.fetch_and(!WRITER, Ordering::Release);
            } else {
                self.lock.state.fetch_sub(READER, Ordering::Release);
            }
            self.lock.grant_locked(w, &mut woken);
            removed
        });
        for waker in woken {
            waker.wake();
        }
    }
}

/// The future [`RwLock::read`] returns.
pub struct Read<'a, T> {
    acquire: Acquire<'a, T>,
}

impl<'a, T> Future for Read<'a, T> {
    type Output = RwLockReadGuard<'a, T>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // `Unpin`, so no `unsafe` projection: the acquisition holds a
        // reference, an id and a flag, and nothing points into it.
        let this = self.get_mut();
        match this.acquire.poll(cx) {
            Poll::Ready(()) => Poll::Ready(RwLockReadGuard {
                lock: this.acquire.lock,
            }),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// The future [`RwLock::write`] returns.
pub struct Write<'a, T> {
    acquire: Acquire<'a, T>,
}

impl<'a, T> Future for Write<'a, T> {
    type Output = RwLockWriteGuard<'a, T>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // As `Read::poll`.
        let this = self.get_mut();
        match this.acquire.poll(cx) {
            Poll::Ready(()) => Poll::Ready(RwLockWriteGuard {
                lock: this.acquire.lock,
            }),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize};
    use std::sync::Arc;
    use std::task::Wake;

    /// A waker that counts, so a test can say "this waiter was woken" rather
    /// than infer it from the waiter having made progress.
    struct Counting(AtomicUsize);

    impl Wake for Counting {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn counting() -> (Arc<Counting>, Waker) {
        let c = Arc::new(Counting(AtomicUsize::new(0)));
        (c.clone(), Waker::from(c))
    }

    /// Poll a future once by hand. Every queueing test below drives futures
    /// this way rather than through an executor, because what is under test is
    /// the order the lock hands itself out in — an executor would decide that
    /// order and hide it.
    fn poll_once<P>(f: &mut Pin<P>, waker: &Waker) -> Poll<<P::Target as Future>::Output>
    where
        P: std::ops::DerefMut,
        P::Target: Future,
    {
        f.as_mut().poll(&mut Context::from_waker(waker))
    }

    /// `std::pin::pin!` yields a `Pin<&mut F>`, and dropping *that* drops a
    /// pointer — the future itself lives to the end of the block. Every test
    /// below that cancels an acquisition on purpose therefore boxes it, so
    /// `drop` really is the cancellation it is written to be.
    fn boxed<F: Future>(f: F) -> Pin<Box<F>> {
        Box::pin(f)
    }

    /// One in-flight acquisition in the randomised driver. Erased, so reads
    /// and writes ride one `Vec`; boxed, so a `remove` really cancels.
    type Pending<'a> = Pin<Box<dyn Future<Output = ()> + 'a>>;

    // ------------------------------------------------------- uncontended --

    #[test]
    fn try_paths_succeed_on_a_free_lock_and_leave_no_trace() {
        let lock = RwLock::new(7i32);
        {
            let g = lock.try_read().expect("free");
            assert_eq!(*g, 7);
            let g2 = lock.try_read().expect("readers share");
            assert_eq!(*g2, 7);
            assert_eq!(lock.probe(), Probe::of(2, 0, false, None));
        }
        assert_eq!(lock.probe(), Probe::IDLE, "both readers gave their count back");
        {
            let mut g = lock.try_write().expect("free again");
            *g = 9;
        }
        assert_eq!(lock.probe(), Probe::IDLE);
        assert_eq!(lock.into_inner(), 9);
    }

    /// The property the portability argument rests on, as a test rather than a
    /// sentence: with nothing queued on the object, neither acquiring nor
    /// releasing a guard enters the waiter list at all. That is what makes a
    /// synchronous locked member on the browser main thread one
    /// compare-exchange each way and nothing else.
    #[test]
    fn an_uncontended_release_never_enters_the_waiter_list() {
        let lock = RwLock::new(0i32);
        for _ in 0..100 {
            drop(lock.try_read().expect("free"));
            let mut w = lock.try_write().expect("free");
            *w += 1;
            drop(w);
        }
        assert_eq!(
            lock.sections.load(Ordering::SeqCst),
            0,
            "an uncontended release took the waiter list"
        );
        // And the positive control: one queued waiter, and it does.
        let held = lock.try_read().expect("free");
        let (_c, waker) = counting();
        let mut w = boxed(lock.write());
        assert!(poll_once(&mut w, &waker).is_pending());
        drop(held);
        assert!(
            lock.sections.load(Ordering::SeqCst) > 0,
            "a release with a waiter queued must grant it"
        );
        drop(w);
    }

    #[test]
    fn a_writer_excludes_readers_and_a_reader_excludes_writers() {
        let lock = RwLock::new(0i32);
        let w = lock.try_write().expect("free");
        assert_eq!(lock.try_read().unwrap_err(), TryLockError);
        assert_eq!(lock.try_write().unwrap_err(), TryLockError);
        drop(w);
        let r = lock.try_read().expect("released");
        assert_eq!(lock.try_write().unwrap_err(), TryLockError);
        drop(r);
    }

    #[test]
    fn an_uncontended_await_never_queues() {
        let lock = RwLock::new(1i32);
        let (count, waker) = counting();
        let mut fut = std::pin::pin!(lock.write());
        match poll_once(&mut fut, &waker) {
            Poll::Ready(mut g) => *g += 1,
            Poll::Pending => panic!("a free lock must resolve on the first poll"),
        }
        assert_eq!(count.0.load(Ordering::SeqCst), 0, "nothing to wake");
        assert_eq!(lock.probe(), Probe::IDLE);
    }

    // ------------------------------------------------------------ order --

    /// The fairness rule this lock commits to, and the one that decides how
    /// often Dart sees a `ContentionException`: a queued writer stops later
    /// readers, so "contended" means held **or** queued behind a writer.
    /// Without it a stream of readers starves a writer indefinitely. tokio's
    /// `RwLock` answers the same way, a waiter absorbing the available permits
    /// before it queues.
    #[test]
    fn a_queued_writer_refuses_a_later_try_read() {
        let lock = RwLock::new(0i32);
        let held = lock.try_read().expect("free");
        let (_c, waker) = counting();
        let mut w = std::pin::pin!(lock.write());
        assert!(poll_once(&mut w, &waker).is_pending(), "a reader holds it");
        assert_eq!(
            lock.try_read().unwrap_err(),
            TryLockError,
            "the queued writer is ahead of this reader"
        );
        drop(held);
        assert!(poll_once(&mut w, &waker).is_ready(), "the writer's turn");
    }

    #[test]
    fn a_released_writer_wakes_the_whole_leading_run_of_readers() {
        let lock = RwLock::new(0i32);
        let held = lock.try_write().expect("free");
        let (c1, w1) = counting();
        let (c2, w2) = counting();
        let (c3, w3) = counting();
        let mut r1 = std::pin::pin!(lock.read());
        let mut r2 = std::pin::pin!(lock.read());
        let mut w = std::pin::pin!(lock.write());
        assert!(poll_once(&mut r1, &w1).is_pending());
        assert!(poll_once(&mut r2, &w2).is_pending());
        assert!(poll_once(&mut w, &w3).is_pending());
        drop(held);
        assert_eq!(c1.0.load(Ordering::SeqCst), 1, "first reader woken");
        assert_eq!(c2.0.load(Ordering::SeqCst), 1, "and the one behind it");
        assert_eq!(c3.0.load(Ordering::SeqCst), 0, "the writer waits its turn");
        let g1 = match poll_once(&mut r1, &w1) {
            Poll::Ready(g) => g,
            Poll::Pending => panic!("granted"),
        };
        let g2 = match poll_once(&mut r2, &w2) {
            Poll::Ready(g) => g,
            Poll::Pending => panic!("granted"),
        };
        assert_eq!(lock.probe(), Probe::of(2, 1, false, Some(true)),
            "two readers, the writer queued");
        drop((g1, g2));
        assert_eq!(c3.0.load(Ordering::SeqCst), 1, "the last reader out wakes it");
        assert!(poll_once(&mut w, &w3).is_ready());
    }

    // ----------------------------------------------------- cancellation --

    #[test]
    fn dropping_a_queued_waiter_removes_it_and_frees_the_way() {
        let lock = RwLock::new(0i32);
        let held = lock.try_read().expect("free");
        let (_c, waker) = counting();
        let mut w = boxed(lock.write());
        assert!(poll_once(&mut w, &waker).is_pending());
        assert_eq!(lock.probe(), Probe::of(1, 1, false, Some(true)));
        drop(w);
        assert_eq!(lock.probe(), Probe::of(1, 0, false, None), "the queue is empty again");
        lock.try_read()
            .expect("with nothing queued a later reader may join");
        drop(held);
    }

    /// Cancelling the **head** of the queue must hand what it was blocking to
    /// whoever was behind it — the sweep-on-every-removal rule.
    #[test]
    fn cancelling_a_queued_head_grants_the_waiter_behind_it() {
        let lock = RwLock::new(0i32);
        let held = lock.try_read().expect("free");
        let (cw, waker_w) = counting();
        let (cr, waker_r) = counting();
        let mut w = boxed(lock.write());
        let mut r = boxed(lock.read());
        assert!(poll_once(&mut w, &waker_w).is_pending());
        assert!(poll_once(&mut r, &waker_r).is_pending(), "behind the writer");
        drop(held);
        assert_eq!(cw.0.load(Ordering::SeqCst), 1);
        assert_eq!(cr.0.load(Ordering::SeqCst), 0, "still behind the writer");
        // The writer is granted but has not observed it. Cancel it.
        drop(w);
        assert_eq!(cr.0.load(Ordering::SeqCst), 1, "the reader is now the head");
        assert!(poll_once(&mut r, &waker_r).is_ready());
    }

    /// The wake-then-cancel race, and the reason invariant 4 exists: the
    /// executor cancels a call by dropping its task, which drops a pending
    /// acquire future — and that future may already have been handed the lock.
    /// Getting this wrong locks the object for the life of the process.
    #[test]
    fn a_granted_waiter_dropped_before_it_looks_hands_the_lock_back() {
        let lock = RwLock::new(0i32);
        let held = lock.try_read().expect("free");
        let (count, waker) = counting();
        let mut w = boxed(lock.write());
        assert!(poll_once(&mut w, &waker).is_pending());
        drop(held);
        assert_eq!(count.0.load(Ordering::SeqCst), 1, "granted and woken");
        assert_eq!(lock.probe(), Probe::of(0, 0, true, None), "the writer bit is its permit");
        // Never polled again: the grant is still unobserved when this drops.
        drop(w);
        assert_eq!(lock.probe(), Probe::IDLE, "the permit came back");
        assert_eq!(*lock.try_read().expect("free again"), 0);
    }

    #[test]
    fn a_granted_reader_dropped_before_it_looks_gives_its_count_back() {
        let lock = RwLock::new(0i32);
        let held = lock.try_write().expect("free");
        let (_c, waker) = counting();
        let mut r = boxed(lock.read());
        assert!(poll_once(&mut r, &waker).is_pending());
        drop(held);
        assert_eq!(lock.probe(), Probe::of(1, 0, false, None), "granted: one reader counted");
        drop(r);
        assert_eq!(lock.probe(), Probe::IDLE);
        lock.try_write().expect("free again");
    }

    #[test]
    fn a_future_that_was_never_polled_touches_nothing() {
        let lock = RwLock::new(0i32);
        let held = lock.try_write().expect("free");
        drop(lock.read());
        drop(lock.write());
        assert_eq!(lock.probe(), Probe::of(0, 0, true, None), "only the guard is accounted");
        drop(held);
        assert_eq!(lock.probe(), Probe::IDLE);
    }

    #[test]
    fn a_re_polled_waiter_takes_the_newest_waker() {
        let lock = RwLock::new(0i32);
        let held = lock.try_write().expect("free");
        let (first, w_first) = counting();
        let (second, w_second) = counting();
        let mut r = std::pin::pin!(lock.read());
        assert!(poll_once(&mut r, &w_first).is_pending());
        assert!(poll_once(&mut r, &w_second).is_pending());
        drop(held);
        assert_eq!(first.0.load(Ordering::SeqCst), 0, "the stale waker is not used");
        assert_eq!(second.0.load(Ordering::SeqCst), 1);
        assert!(poll_once(&mut r, &w_second).is_ready());
    }

    // ------------------------------------------------------ re-entrancy --

    /// The `spin` rule that nothing is woken inside a critical section, turned
    /// into a fence. The re-entrant call is one that must **take the waiter
    /// list** — a `write()` polled while a reader holds, so it queues — rather
    /// than one the lock-free fast path answers, or the test would pass
    /// against an implementation that wakes with the section still held. On
    /// the `SpinLock` arm that would spin forever; on the host `Mutex` arm it
    /// deadlocks or panics ("cannot recursively acquire"). Either way the
    /// failure is this test hanging, not a page freezing later.
    #[test]
    fn a_waker_may_re_enter_the_lock_while_it_is_woken() {
        struct Reentrant(*const RwLock<i32>, AtomicBool);
        // Safety: the test keeps the lock alive for the waker's whole life and
        // never sends it anywhere.
        unsafe impl Send for Reentrant {}
        unsafe impl Sync for Reentrant {}
        impl Wake for Reentrant {
            fn wake(self: Arc<Self>) {
                self.wake_by_ref();
            }
            fn wake_by_ref(self: &Arc<Self>) {
                // Safety: as the impls above.
                let lock = unsafe { &*self.0 };
                let mut w = Box::pin(lock.write());
                assert!(
                    w.as_mut()
                        .poll(&mut Context::from_waker(&Waker::noop().clone()))
                        .is_pending(),
                    "a reader holds it, so this queues — which takes the waiter list"
                );
                self.1.store(true, Ordering::SeqCst);
            }
        }
        let lock = RwLock::new(0i32);
        let re = Arc::new(Reentrant(&lock as *const _, AtomicBool::new(false)));
        let waker = Waker::from(re.clone());
        let held = lock.try_write().expect("free");
        let mut r = Box::pin(lock.read());
        assert!(poll_once(&mut r, &waker).is_pending());
        drop(held);
        assert!(re.1.load(Ordering::SeqCst), "the re-entrant wake returned");
        drop(r);
    }

    /// The other half of the same rule: dropping a waker runs arbitrary code —
    /// `async_task`'s waker drop calls the schedule callback — so a displaced
    /// waker must die outside the section too. The node's clone is made the
    /// **last** reference before the re-poll, so the displacing `mem::replace`
    /// is what runs `Drop`.
    #[test]
    fn a_waker_may_re_enter_the_lock_while_it_is_dropped() {
        struct DropsIntoLock(*const RwLock<i32>, Arc<AtomicBool>);
        // Safety: as above.
        unsafe impl Send for DropsIntoLock {}
        unsafe impl Sync for DropsIntoLock {}
        impl Wake for DropsIntoLock {
            fn wake(self: Arc<Self>) {}
            fn wake_by_ref(self: &Arc<Self>) {}
        }
        impl Drop for DropsIntoLock {
            fn drop(&mut self) {
                // Safety: as the impls above.
                let lock = unsafe { &*self.0 };
                let mut w = Box::pin(lock.write());
                assert!(
                    w.as_mut()
                        .poll(&mut Context::from_waker(&Waker::noop().clone()))
                        .is_pending(),
                    "a writer holds it, so this queues — which takes the waiter list"
                );
                self.1.store(true, Ordering::SeqCst);
            }
        }
        let lock = RwLock::new(0i32);
        let ran = Arc::new(AtomicBool::new(false));
        let held = lock.try_write().expect("free");
        let mut r = Box::pin(lock.read());
        {
            let waker = Waker::from(Arc::new(DropsIntoLock(&lock as *const _, ran.clone())));
            assert!(poll_once(&mut r, &waker).is_pending());
        }
        // The queued node now holds the only reference, so re-polling with a
        // different waker is what drops it.
        assert!(!ran.load(Ordering::SeqCst), "not dropped yet");
        let (_c, other) = counting();
        assert!(poll_once(&mut r, &other).is_pending());
        assert!(ran.load(Ordering::SeqCst), "the re-entrant drop returned");
        drop(r);
        drop(held);
    }

    // ------------------------------------------------------- the blocker --

    // ---------------------------------------------------------- bounds --

    /// The property the whole `locked` model rests on: a guard can cross a
    /// thread, so a generated future holding one is `Send` and
    /// `executor::spawn` accepts it. `std`'s guards are `!Send`, which is what
    /// used to refuse every `async fn` on a locked type.
    #[test]
    fn guards_and_acquisitions_are_send() {
        fn assert_send<T: Send>() {}
        assert_send::<RwLock<i32>>();
        assert_send::<RwLockReadGuard<'static, i64>>();
        assert_send::<RwLockWriteGuard<'static, i64>>();
        assert_send::<Read<'static, i64>>();
        assert_send::<Write<'static, i64>>();
    }

    // ------------------------------------------------------ conservation --

    /// A randomised single-threaded driver. With no concurrency the lock is a
    /// state machine, so the two invariants are exact after **every**
    /// operation rather than only at the end:
    ///
    ///   * conservation — the word's reader count and writer bit are owned by
    ///     exactly the live guards plus the granted-but-unobserved waiters;
    ///   * settled — the head of the queue is never one the lock could have
    ///     granted.
    ///
    /// The interleavings *are* the operation orders here, which is the whole
    /// reason to drive it by hand instead of through an executor.
    #[test]
    fn a_randomised_driver_conserves_the_word_and_leaves_no_grantable_head() {
        // xorshift, so the sequence is reproducible from the seed printed in
        // any failure rather than depending on a dev-dependency.
        let mut rng = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let lock = RwLock::new(0i64);
        let mut guards_r: Vec<RwLockReadGuard<'_, i64>> = Vec::new();
        let mut guards_w: Vec<RwLockWriteGuard<'_, i64>> = Vec::new();
        // Each pending acquisition, with whether it has been granted (which
        // the driver learns by polling, exactly as production does).
        let mut pending: Vec<Pending<'_>> = Vec::new();
        for step in 0..4000u32 {
            let p = lock.probe();
            let owned_r = guards_r.len();
            let owned_w = guards_w.len();
            // Conservation, both directions. Every reader unit in the word is
            // a guard this driver holds or an acquisition it has not polled
            // yet; every acquisition it has not polled can account for at most
            // one. A double count fails the upper bound, a lost unit the
            // lower.
            assert!(
                p.readers >= owned_r && p.readers <= owned_r + pending.len(),
                "step {step}: {} readers in the word, {owned_r} guards held and \
                 {} acquisitions outstanding",
                p.readers,
                pending.len()
            );
            assert!(
                !p.writer || owned_w == 1 || !pending.is_empty(),
                "step {step}: the writer bit belongs to nobody"
            );
            assert!(
                !(p.writer && p.readers > 0),
                "step {step}: a writer and {} readers at once",
                p.readers
            );
            // Settled: with no concurrency, `hand_over` runs to a fixed point
            // in every section, so a queue head the lock could satisfy is a
            // grant that did not happen. A writer head needs the lock wholly
            // free; a reader head needs only that no writer holds it — so a
            // reader head is settled *only* while a writer holds.
            match p.head_writes {
                None => assert_eq!(p.queued, 0, "step {step}: a head with no queue"),
                Some(true) => assert!(
                    p.writer || p.readers > 0,
                    "step {step}: a queued writer with the lock free"
                ),
                Some(false) => assert!(
                    p.writer,
                    "step {step}: a queued reader with no writer holding"
                ),
            }
            match next() % 6 {
                0 => {
                    if let Ok(g) = lock.try_read() {
                        guards_r.push(g);
                    }
                }
                1 => {
                    if guards_w.is_empty() {
                        if let Ok(g) = lock.try_write() {
                            guards_w.push(g);
                        }
                    }
                }
                2 => {
                    if !guards_r.is_empty() {
                        let i = (next() as usize) % guards_r.len();
                        guards_r.remove(i);
                    }
                }
                3 => {
                    guards_w.pop();
                }
                _ => {}
            }
            // Independently: start, poll or drop an acquisition. The futures
            // borrow the lock, so they are boxed to live in one `Vec`.
            match next() % 4 {
                0 if pending.len() < 8 => {
                    // Safety of the transmute-free trick: the futures and the
                    // lock share this scope, and `pending` is drained before
                    // it ends.
                    let want_write = next() % 2 == 0;
                    let fut: Pending<'_> = if want_write {
                        Box::pin(async {
                            let _g = lock.write().await;
                        })
                    } else {
                        Box::pin(async {
                            let _g = lock.read().await;
                        })
                    };
                    pending.push(fut);
                }
                1 if !pending.is_empty() => {
                    let i = (next() as usize) % pending.len();
                    let (_c, waker) = counting();
                    let mut cx = Context::from_waker(&waker);
                    if pending[i].as_mut().poll(&mut cx).is_ready() {
                        // The guard the body took was dropped inside it.
                        drop(pending.remove(i));
                    }
                }
                2 if !pending.is_empty() => {
                    let i = (next() as usize) % pending.len();
                    drop(pending.remove(i));
                }
                _ => {}
            }
        }
        // Drain: everything the driver still holds goes, and the word must be
        // exactly as it started.
        drop(guards_r);
        drop(guards_w);
        // Polling to completion rather than dropping, so the run ends by
        // exercising the grant path rather than the cancel one.
        while !pending.is_empty() {
            let (_c, waker) = counting();
            let mut cx = Context::from_waker(&waker);
            let before = pending.len();
            pending.retain_mut(|f| f.as_mut().poll(&mut cx).is_pending());
            assert!(
                pending.len() < before,
                "no waiter could make progress with the lock free — a lost wakeup"
            );
        }
        assert_eq!(
            lock.probe(),
            Probe::IDLE,
            "every acquisition gave back exactly what it took"
        );
    }

    /// Real threads, real barging, under Miri's preemption. The exclusion
    /// counter is the memory-safety check for the grant path: a write guard
    /// must never coexist with a reader, which is what `hand_over`'s single
    /// compare-exchange exists to guarantee. A lost wakeup shows up here as
    /// every thread parked, which Miri reports as a deadlock rather than a
    /// hang.
    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn concurrent_holders_never_overlap() {
        // Positive while readers hold, -1 while a writer does.
        let occupancy = AtomicI32::new(0);
        let lock = RwLock::new(0u64);
        // Shared by reference, so each thread's `move` takes the borrow rather
        // than the value.
        let (occupancy, lock) = (&occupancy, &lock);
        std::thread::scope(|s| {
            for t in 0..4u64 {
                s.spawn(move || {
                    for i in 0..40u64 {
                        if (t + i) % 3 == 0 {
                            let mut g = lock.blocking_write();
                            assert_eq!(
                                occupancy.swap(-1, Ordering::SeqCst),
                                0,
                                "a writer found the value already occupied"
                            );
                            *g += 1;
                            std::hint::black_box(&mut *g);
                            occupancy.store(0, Ordering::SeqCst);
                        } else {
                            let g = lock.blocking_read();
                            let seen = occupancy.fetch_add(1, Ordering::SeqCst);
                            assert!(seen >= 0, "a reader found a writer holding the value");
                            std::hint::black_box(&*g);
                            occupancy.fetch_sub(1, Ordering::SeqCst);
                        }
                        // The try paths barge alongside, which is what makes
                        // the grant CAS race against something.
                        if let Ok(g) = lock.try_read() {
                            let seen = occupancy.fetch_add(1, Ordering::SeqCst);
                            assert!(seen >= 0, "a try-reader found a writer");
                            std::hint::black_box(&*g);
                            occupancy.fetch_sub(1, Ordering::SeqCst);
                        }
                        // And a cancellation racing everything else, which is
                        // the production shape: the executor drops a pending
                        // acquire future when a call is cancelled. Polled once
                        // so it may queue, then dropped — so whenever a
                        // concurrent release grants it in that window, the
                        // hand-back runs against other threads' grants. This
                        // is the only place invariant 4 meets real
                        // concurrency; without it Miri only ever interleaves
                        // the deterministic cases above.
                        let mut c = Box::pin(lock.write());
                        let _ = c
                            .as_mut()
                            .poll(&mut Context::from_waker(&Waker::noop().clone()));
                        drop(c);
                    }
                });
            }
        });
        assert_eq!(lock.probe(), Probe::IDLE, "every holder gave its count back");
        assert_eq!(occupancy.load(Ordering::SeqCst), 0);
    }
}
