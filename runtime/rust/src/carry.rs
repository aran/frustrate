//! A value of the embedder's that follows bridge work from the thread that
//! queues it to every thread that runs it.
//!
//! [`set`] names the value by two functions over the embedder's own
//! per-thread state: `current` reads it, and `swap` makes a value current and
//! returns the one it replaced. Where frustrate queues work, it captures
//! `current()`. Around each run of that work, wherever it runs, it swaps the
//! captured value in and the thread's own value back after, on unwind too.
//!
//! The work carried is:
//!
//! - an `async fn` call's task, around each of its polls and its drop;
//! - a [`crate::runtime::spawn`] task, the same way;
//! - a plain call's job on the pool;
//! - a call to an actor, and a closure an actor posts to itself.
//!
//! A sync call runs on the calling thread, so the value is already current
//! there. Work queued from inside carried work captures the carried value,
//! so a task a call spawns carries the call's. A change the work makes to its
//! value is swapped out with it and swapped back in for its next poll.
//!
//! One value crosses no memory boundary: on web an actor runs in its own wasm
//! instance, so a call from Dart into it captures that instance's value,
//! not the caller's.
//!
//! With nothing set, capturing is one atomic load, and running is a
//! branch.

use crate::hook::Hook;
use std::any::Any;
use std::future::Future;
use std::mem::ManuallyDrop;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

trait Carrier: Send + Sync {
    fn capture(&self) -> Box<dyn Any + Send>;
    fn exchange(&self, held: &mut (dyn Any + Send));
}

struct Typed<T> {
    current: fn() -> T,
    swap: fn(T) -> T,
}

impl<T: Send + 'static> Carrier for Typed<T> {
    fn capture(&self) -> Box<dyn Any + Send> {
        Box::new(Some((self.current)()))
    }

    fn exchange(&self, held: &mut (dyn Any + Send)) {
        let slot = held
            .downcast_mut::<Option<T>>()
            .expect("captured by this carrier");
        let value = slot.take().expect("exchanged in pairs");
        *slot = Some((self.swap)(value));
    }
}

static CARRIER: Hook<dyn Carrier> = Hook::new();

/// Carry the value `current` reads from where bridge work is queued to where
/// it runs (see the module docs).
///
/// `swap(v)` makes `v` current on the calling thread and returns the value it
/// replaced; it must not panic. Set it at init, before the first bridge
/// call: work queued before then carries nothing. A second `set`
/// replaces the first, for work queued after it.
pub fn set<T: Send + 'static>(current: fn() -> T, swap: fn(T) -> T) {
    CARRIER.set(Arc::new(Typed { current, swap }));
}

/// Stop carrying. Work already queued still carries what it captured.
pub fn clear() {
    CARRIER.clear();
}

/// The carried value as it is where work is being queued.
pub(crate) struct Captured(Option<(Arc<dyn Carrier>, Box<dyn Any + Send>)>);

impl Captured {
    pub(crate) fn here() -> Self {
        Captured(CARRIER.get().map(|carrier| {
            let value = carrier.capture();
            (carrier, value)
        }))
    }

    /// Run `f` with the captured value current, and the thread's own after.
    pub(crate) fn run<R>(&mut self, f: impl FnOnce() -> R) -> R {
        let Some((carrier, value)) = &mut self.0 else {
            return f();
        };
        struct Back<'a>(&'a dyn Carrier, &'a mut (dyn Any + Send));
        impl Drop for Back<'_> {
            fn drop(&mut self) {
                self.0.exchange(self.1);
            }
        }
        carrier.exchange(&mut **value);
        let _back = Back(&**carrier, &mut **value);
        f()
    }
}

/// `future`, run with the value captured where it was made.
pub(crate) struct Carried<F> {
    captured: Captured,
    future: ManuallyDrop<F>,
}

impl<F> Carried<F> {
    pub(crate) fn new(future: F) -> Self {
        Carried {
            captured: Captured::here(),
            future: ManuallyDrop::new(future),
        }
    }
}

impl<F: Future> Future for Carried<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        // Safety: `future` is pinned with `self`. It is never moved out, and
        // `drop` below drops it in place; `captured` is not pinned.
        let this = unsafe { self.get_unchecked_mut() };
        let future = &mut this.future;
        this.captured
            .run(|| unsafe { Pin::new_unchecked(&mut **future) }.poll(cx))
    }
}

impl<F> Drop for Carried<F> {
    fn drop(&mut self) {
        let future = &mut self.future;
        // Safety: the one drop of `future`, in place.
        self.captured.run(|| unsafe { ManuallyDrop::drop(future) });
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use super::*;
    use std::cell::Cell;

    thread_local! {
        static VALUE: Cell<u32> = const { Cell::new(0) };
    }

    fn current() -> u32 {
        VALUE.with(Cell::get)
    }

    fn swap(v: u32) -> u32 {
        VALUE.with(|c| c.replace(v))
    }

    fn set(v: u32) {
        VALUE.with(|c| c.set(v));
    }

    /// Registers the test carrier for one test's span: the slot is process
    /// global.
    fn carrying() -> impl Drop {
        struct Clear(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);
        impl Drop for Clear {
            fn drop(&mut self) {
                clear();
            }
        }
        let guard = crate::serialize_globals();
        super::set(current, swap);
        Clear(guard)
    }

    #[test]
    fn a_job_runs_under_the_value_captured_where_it_was_made() {
        let _r = carrying();
        set(7);
        let mut captured = Captured::here();
        set(0);
        let seen = std::thread::spawn(move || {
            set(3);
            let seen = captured.run(current);
            (seen, current())
        })
        .join()
        .unwrap();
        assert_eq!(seen, (7, 3), "the job saw 7 and its thread got 3 back");
    }

    #[test]
    fn the_thread_gets_its_own_value_back_after_a_panic() {
        let _r = carrying();
        set(7);
        let mut captured = Captured::here();
        set(3);
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            captured.run(|| panic!("in the job"))
        }));
        assert!(panicked.is_err());
        assert_eq!(current(), 3);
    }

    #[test]
    fn a_task_keeps_a_change_it_makes_across_polls() {
        let _r = carrying();
        set(7);
        let mut polls = 0;
        let mut task = Box::pin(Carried::new(std::future::poll_fn(move |_| {
            polls += 1;
            if polls == 1 {
                assert_eq!(current(), 7);
                set(8);
                Poll::Pending
            } else {
                Poll::Ready(current())
            }
        })));
        set(0);
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        assert!(task.as_mut().poll(&mut cx).is_pending());
        assert_eq!(current(), 0, "the poll gave the thread its value back");
        assert_eq!(task.as_mut().poll(&mut cx), Poll::Ready(8));
    }

    #[test]
    fn a_task_is_dropped_under_its_value() {
        let _r = carrying();
        struct Probe(std::sync::Arc<std::sync::atomic::AtomicU32>);
        impl Drop for Probe {
            fn drop(&mut self) {
                self.0.store(current(), std::sync::atomic::Ordering::SeqCst);
            }
        }
        let seen = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        set(7);
        let probe = Probe(seen.clone());
        let task = Carried::new(async move {
            let _probe = probe;
            std::future::pending::<()>().await;
        });
        set(0);
        drop(task);
        assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 7);
        assert_eq!(current(), 0);
    }

    /// Through the executor: a call's task, suspended and then polled on
    /// another thread, sees the calling thread's value at every poll, and so
    /// does a task it spawns.
    #[test]
    fn a_call_s_task_and_what_it_spawns_carry_the_caller_s_value() {
        use crate::codec::FramedWriter;
        use crate::envelope::{Outcome, STATUS_OK};
        use crate::executor::Executor;
        use std::sync::{Arc, Mutex};

        let _r = carrying();
        let exec = Arc::new(Executor::new(|| {}, |_, _| {}));
        let seen = Arc::new(Mutex::new(Vec::new()));
        set(7);
        {
            let seen = seen.clone();
            let inner = exec.clone();
            exec.spawn(1, async move {
                seen.lock().unwrap().push(("first poll", current()));
                let mut yielded = false;
                std::future::poll_fn(|cx| {
                    if yielded {
                        Poll::Ready(())
                    } else {
                        yielded = true;
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                })
                .await;
                seen.lock().unwrap().push(("second poll", current()));
                let spawned = seen.clone();
                inner.spawn_detached(async move {
                    spawned.lock().unwrap().push(("spawned", current()));
                });
                Outcome::Ok(FramedWriter::status(STATUS_OK))
            });
        }
        set(0);
        let after = std::thread::spawn(move || {
            set(3);
            while exec.drain_one() {}
            current()
        })
        .join()
        .unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            [("first poll", 7), ("second poll", 7), ("spawned", 7)]
        );
        assert_eq!(after, 3, "the polling thread got its own value back");
    }

    #[test]
    fn nothing_set_carries_nothing() {
        let _guard = crate::serialize_globals();
        set(7);
        let mut captured = Captured::here();
        set(3);
        assert_eq!(captured.run(current), 3);
    }
}
