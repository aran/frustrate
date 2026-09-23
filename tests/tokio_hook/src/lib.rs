//! The async-runtime hook, against a real tokio.
//!
//! Two layers, and the split is the point.
//!
//! **The gate** (`mod gate`) involves no frustrate at all: does a tokio
//! reactor drive a leaf polled under `Handle::enter()` from a *foreign*
//! thread, with the wake landing on an
//! `async_task` waker? It is the record of **every tokio
//! behaviour the design rests on** — including the three that are silent or
//! surprising (an undriven `current_thread` runtime, a `block_on` under
//! `enter`, and the fact that tokio's own leaf drops fine without a context).
//!
//! **The recipe** (`mod recipe`) is the same question one layer up: the exact
//! `register(...)` call [`frustrate::runtime::register`]'s rustdoc hands an
//! app, driving a real `tokio::time::sleep` on a real
//! [`frustrate::executor::Executor`].
//!
//! Every assertion here is about a **third party's** behaviour, which is why
//! they exist at all — none of it is ours to assert from memory, and a tokio
//! upgrade that changes any of it should land here first. Each failure message
//! therefore names the behaviour it invalidates, rather than just reporting a
//! mismatch.
//!
//! What is *not* here is the hook's own contract — the un-run-poll leak,
//! re-registration, cancel-drops-under-the-context, panic attribution. That is
//! in `runtime/rust/src/runtime.rs`'s unit tests, over a hand-rolled reactor,
//! because those properties are frustrate's and testing them must not put
//! tokio in the crate hub (see Cargo.toml).

#[cfg(test)]
mod gate {
    use async_task::{Runnable, Task};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};

    /// A cooperative executor that is emphatically **not** tokio: a queue of
    /// `Runnable`s drained one poll at a time on a plain std thread. This is
    /// the shape of `Executor::drain_one`, reduced to nothing but the property
    /// under test.
    #[derive(Clone, Default)]
    pub(super) struct Coop {
        q: Arc<(Mutex<Vec<Runnable>>, Condvar)>,
        pub(super) polls: Arc<AtomicUsize>,
    }

    impl Coop {
        pub(super) fn spawn<F>(&self, fut: F) -> Task<F::Output>
        where
            F: std::future::Future + Send + 'static,
            F::Output: Send + 'static,
        {
            let q = self.q.clone();
            let (runnable, task) = async_task::spawn(fut, move |r: Runnable| {
                q.0.lock().unwrap().push(r);
                q.1.notify_all();
            });
            runnable.schedule();
            task
        }

        pub(super) fn pop(&self, timeout: Duration) -> Option<Runnable> {
            let mut g = self.q.0.lock().unwrap();
            let deadline = Instant::now() + timeout;
            loop {
                if let Some(item) = g.pop() {
                    return Some(item);
                }
                let left = deadline.checked_duration_since(Instant::now())?;
                g = self.q.1.wait_timeout(g, left).unwrap().0;
            }
        }

        pub(super) fn drain_until(
            &self,
            done: &AtomicBool,
            enter: impl Fn(&mut dyn FnMut()),
            timeout: Duration,
        ) {
            let deadline = Instant::now() + timeout;
            while !done.load(Ordering::SeqCst) && Instant::now() < deadline {
                let Some(runnable) = self.pop(Duration::from_millis(20)) else {
                    continue;
                };
                self.polls.fetch_add(1, Ordering::SeqCst);
                let mut slot = Some(runnable);
                enter(&mut || {
                    slot.take().unwrap().run();
                });
            }
        }
    }

    /// **The gate.** A `tokio::time::sleep` polled on a plain std thread under
    /// `Handle::enter()` must reach `Ready` — woken by tokio's own timer wheel
    /// through *our* waker — after real wall-clock time, in a handful of polls.
    ///
    /// The poll count is half the assertion: a future that completed by being
    /// hammered would prove nothing about suspension, which is the whole
    /// resource argument for the hook over bring-your-own-`block_on`.
    #[test]
    fn a_tokio_leaf_is_driven_under_enter_from_a_foreign_thread() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let handle = rt.handle().clone();
        let coop = Coop::default();
        let done = Arc::new(AtomicBool::new(false));

        let flag = done.clone();
        let _task = coop.spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            flag.store(true, Ordering::SeqCst);
        });

        let started = Instant::now();
        coop.drain_until(
            &done,
            |poll| {
                let _guard = handle.enter();
                poll();
            },
            Duration::from_secs(10),
        );
        let elapsed = started.elapsed();

        assert!(
            done.load(Ordering::SeqCst),
            "the tokio timer never woke our waker: tokio no longer drives a leaf \
             polled under `Handle::enter()` from a foreign thread, which is the \
             premise the whole hook rests on"
        );
        assert!(
            elapsed >= Duration::from_millis(140),
            "completed in {elapsed:?}: the sleep did not actually elapse"
        );
        let polls = coop.polls.load(Ordering::SeqCst);
        assert!(
            polls <= 8,
            "{polls} polls across 150ms is a busy loop, not a suspension"
        );
    }

    /// What `Runnable::run()` does *besides* poll, measured — because the hook
    /// wraps the whole of `run()` and that decision needs a reason that is
    /// true.
    ///
    /// `run()` is also where async-task **drops a cancelled future**. Two
    /// separate questions, and they have different answers:
    ///
    ///   - **tokio's own leaf does not need the context to drop.** A
    ///     `TimerEntry` owns an `Arc`'d `scheduler::Handle` captured at
    ///     construction (tokio-1.53.1 `runtime/time/entry.rs:287-304`), so
    ///     `PinnedDrop` deregisters through that, not through the
    ///     thread-local. Asserted here so the claim stays measured: an earlier
    ///     draft of this work asserted the opposite from a misread panic
    ///     location.
    ///   - **A user's `Drop` very much can.** Anything reaching for
    ///     `Handle::current()` — arming a `sleep`/`timeout`, spawning a
    ///     cleanup task — panics without a context. Measured, and it is worse
    ///     than a panic: running that drop outside `enter()` aborts the
    ///     process ("panic in a destructor during cleanup ... thread caused
    ///     non-unwinding panic"), so `catch_unwind` cannot even observe it.
    ///     That is why the positive case is pinned in
    ///     `recipe::a_cancelled_calls_teardown_still_has_the_runtime` rather
    ///     than the negative one here — the negative one takes the test binary
    ///     with it.
    ///
    /// The second is what justifies wrapping `run()` rather than "the poll":
    /// it costs nothing, and the alternative makes a cancelled call's teardown
    /// a different environment from its body's, with a process abort as the
    /// penalty for noticing.
    #[test]
    fn tokios_own_leaf_drops_without_a_context() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let handle = rt.handle().clone();
        let coop = Coop::default();

        let task = coop.spawn(async {
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        {
            let _guard = handle.enter();
            coop.pop(Duration::from_secs(5)).unwrap().run();
        }
        drop(task);
        let mut slot = Some(coop.pop(Duration::from_secs(5)).unwrap());
        let bare = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            slot.take().unwrap().run();
        }));
        assert!(
            bare.is_ok(),
            "tokio's TimerEntry deregisters through the `scheduler::Handle` it \
             owns, not the thread-local. If that changed, `enter`'s doc \
             comment in runtime.rs gains a second reason it does not currently \
             claim: {:?}",
            bare.err().map(panic_message)
        );

        // `sleep()` builds its TimerEntry eagerly, so it has to be constructed
        // *inside* the context, not as an argument evaluated outside it — the
        // same eagerness that makes `timeout` in a `Drop` an abort.
        rt.block_on(async { tokio::time::sleep(Duration::from_millis(1)).await });
    }

    /// The control for the gate: without `enter()`, a tokio leaf fails at
    /// **construction**, loudly. So the passing gate above is `enter()` doing
    /// the work, and an app that forgets to register gets an attributable
    /// panic on its own call rather than a hang.
    #[test]
    fn without_enter_a_tokio_leaf_panics_at_construction() {
        let _rt = tokio::runtime::Runtime::new().unwrap();
        let coop = Coop::default();
        let task = coop.spawn(async {
            tokio::time::sleep(Duration::from_millis(10)).await;
        });
        let runnable = coop.pop(Duration::from_secs(5)).unwrap();
        let mut slot = Some(runnable);
        let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            slot.take().unwrap().run()
        }))
        .unwrap_err();
        assert!(panic_message(err).contains("no reactor running"));
        drop(task);
    }

    /// The hazard `frustrate::runtime::register`'s docs name, pinned so it
    /// stays true rather than remembered: a `Handle` to a `current_thread`
    /// runtime nobody drives registers the leaf and **never wakes it**. No
    /// panic, no error, no timeout — which is exactly why the contract is
    /// documented instead of detected. frustrate cannot tell this apart from a
    /// task legitimately awaiting Dart, and tokio exposes nothing to ask.
    #[test]
    fn a_current_thread_handle_nobody_drives_stalls_silently() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let handle = rt.handle().clone();
        let coop = Coop::default();
        let done = Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        let _task = coop.spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            flag.store(true, Ordering::SeqCst);
        });

        coop.drain_until(
            &done,
            |poll| {
                let _guard = handle.enter();
                poll();
            },
            Duration::from_millis(400),
        );

        assert!(
            !done.load(Ordering::SeqCst),
            "an undriven current_thread runtime fired a timer — if tokio grew \
             a way to make this work, `register`'s documented contract is \
             stricter than it needs to be and should be relaxed on purpose"
        );
        assert_eq!(
            coop.polls.load(Ordering::SeqCst),
            1,
            "and it stalls rather than spinning: one poll, then nothing"
        );
    }

    /// **What decides `register`'s re-registration rule.** A `Handle` outlives
    /// the `Runtime` it came from, and hot restart is exactly the case where
    /// an app rebuilds its runtime and the old one is dropped. So: is a stale
    /// handle loud or silent?
    ///
    /// Measured: **loud.** tokio panics "A Tokio 1.x context was found, but it
    /// is being shutdown" — from `TimerEntry::poll_elapsed`
    /// (tokio-1.53.1 `runtime/time/entry.rs:535-543`), so at the first **poll**
    /// rather than at construction. This test cannot tell the two apart, since
    /// construction and the first poll share one `run()`; the distinction comes
    /// from the tokio source and is recorded so the claim is not overstated.
    /// Two consequences, and they point the same way:
    ///
    ///   - It is not a hang, so "the runtime must outlive the registration" is
    ///     enforced by tokio rather than resting on `register`'s documentation.
    ///   - It is why the slot is **last-wins**. A keep-first slot would pin
    ///     the dead handle for the life of the process, so every async call
    ///     after a hot restart would panic — with the app's fresh, correct
    ///     registration sitting there ignored. Last-wins makes the restart
    ///     work; keep-first would make this loud panic the normal dev loop.
    #[test]
    fn a_handle_to_a_dropped_runtime_panics_loudly() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let handle = rt.handle().clone();
        drop(rt);

        let coop = Coop::default();
        let done = Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        let _task = coop.spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            flag.store(true, Ordering::SeqCst);
        });

        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            coop.drain_until(
                &done,
                |poll| {
                    let _guard = handle.enter();
                    poll();
                },
                Duration::from_millis(400),
            );
        }));

        // Either answer is acceptable for the *design*; what is not acceptable
        // is not knowing. Assert the one tokio actually has, so a change shows
        // up here rather than in an app.
        let msg = panic_message(outcome.expect_err(
            "a stale handle stopped failing. If tokio now keeps the driver \
             alive behind a `Handle`, `register` need not say the runtime must \
             outlive the registration; if it went quiet instead, that \
             requirement became a *silent* hazard and belongs in the table on \
             `register` beside the undriven current_thread case",
        ));
        assert!(
            msg.contains("being shutdown"),
            "expected tokio's shutdown-context panic, got {msg:?}"
        );
        assert!(
            !done.load(Ordering::SeqCst),
            "and the timer certainly did not fire"
        );
    }

    /// Does registering a context break the **existing**
    /// bring-your-own-`block_on` pattern? Only the cooperative executor's
    /// polls are wrapped, so a plain `#[bridge] fn` on `pool::spawn_call` is
    /// untouched by construction. The question this settles is the other one:
    /// what happens if a `block_on` runs *inside* an entered context anyway —
    /// which is what a bridged `async fn` body doing `RT.block_on(...)` would
    /// be.
    ///
    /// Measured: **it works, and that is the trap.** `Handle::enter()` installs
    /// the runtime context but not tokio's blocking-region guard, so the
    /// "Cannot start a runtime from within a runtime" check does not fire —
    /// neither for a second runtime nor for the entered one itself. The
    /// `block_on` simply blocks the drain thread until it finishes, which is
    /// the parked-thread cost the hook exists to avoid, arrived at silently.
    /// Documented on `register` rather than guarded, because it is a body the
    /// user wrote and frustrate never sees.
    #[test]
    fn block_on_inside_an_entered_context_silently_blocks_the_drain() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let handle = rt.handle().clone();
        let other = tokio::runtime::Runtime::new().unwrap();

        // (a) a *different* runtime's block_on, inside the entered context.
        let nested = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = handle.enter();
            other.block_on(async { tokio::time::sleep(Duration::from_millis(5)).await });
        }));
        assert!(
            nested.is_ok(),
            "tokio grew a guard against this ({:?}) — good news, and \
             `register`'s Scope note should now say it fails loudly instead of \
             blocking silently",
            nested.err().map(panic_message)
        );

        // (b) the entered runtime's own block_on, from this foreign thread.
        // Also permitted: `enter()` is not a blocking region.
        let same = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = handle.enter();
            rt.block_on(async { tokio::time::sleep(Duration::from_millis(5)).await });
        }));
        assert!(
            same.is_ok(),
            "got {:?}",
            same.err().map(panic_message)
        );
    }

    pub(super) fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
        payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_string()))
            .unwrap_or_else(|| "<non-string panic>".to_string())
    }
}

#[cfg(test)]
mod recipe {
    use frustrate::codec::FramedWriter;
    use frustrate::envelope::{Outcome, STATUS_OK};
    use frustrate::executor::Executor;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    type Delivered = Arc<Mutex<Vec<(u64, Vec<u8>)>>>;

    /// A test executor with the same two seams the production global uses:
    /// `schedule` (here a no-op — the test drives the drain) and `complete`.
    fn recording() -> (Executor, Delivered) {
        let log: Delivered = Arc::new(Mutex::new(Vec::new()));
        let sink = log.clone();
        let exec = Executor::new(
            || {},
            move |id, bytes| sink.lock().unwrap().push((id, bytes)),
        );
        (exec, log)
    }

    /// Exactly what [`frustrate::runtime::register`]'s rustdoc tells an app to
    /// write — plus the serialization these tests need and an app does not.
    ///
    /// Registration is **process-global** by design (one runtime per process,
    /// no per-`fn` selection), and `cargo test` runs these in one process in
    /// parallel. So a test holds the slot for its whole body and hands it back
    /// on the way out, panic or not. An app registers once and never
    /// unregisters; the guard is a property of the harness, not of the API.
    #[must_use]
    fn register_tokio(handle: tokio::runtime::Handle) -> Registered {
        static SERIAL: Mutex<()> = Mutex::new(());
        let held = SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        frustrate::runtime::register(move |poll| {
            let _guard = handle.enter();
            poll.run();
        });
        Registered(held)
    }

    struct Registered(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);
    impl Drop for Registered {
        fn drop(&mut self) {
            frustrate::runtime::unregister();
        }
    }

    /// The payoff. A future that `.await`s a real `tokio::time::sleep` runs on
    /// frustrate's cooperative executor: it suspends as heap data, tokio's
    /// reactor wakes it, and it completes — no pool thread parked on it, and
    /// the task never leaves our registry.
    #[test]
    fn an_async_fn_awaits_a_real_tokio_leaf_on_the_cooperative_executor() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _registration = register_tokio(rt.handle().clone());
        let (exec, log) = recording();

        exec.spawn(1, async {
            tokio::time::sleep(Duration::from_millis(120)).await;
            Outcome::Ok(FramedWriter::status(STATUS_OK))
        });

        // One poll arms the timer and returns Pending. The task is now heap
        // data; this thread owns nothing.
        assert!(exec.drain_one());
        assert!(log.lock().unwrap().is_empty(), "suspended, not completed");
        assert_eq!(exec.task_count(), 1);

        let started = Instant::now();
        while !exec.drain_one() {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "tokio's reactor never re-enqueued the task"
            );
            std::thread::yield_now();
        }
        assert!(
            started.elapsed() >= Duration::from_millis(100),
            "it resumed too early to have really slept"
        );
        assert_eq!(log.lock().unwrap().len(), 1);
        assert_eq!(log.lock().unwrap()[0].0, 1);
        assert_eq!(log.lock().unwrap()[0].1[0], STATUS_OK);
        assert_eq!(exec.task_count(), 0);

    }

    /// The cancellation the hook exists to give, over `block_on`: a suspended
    /// tokio leaf is dropped by dropping *our* registry entry. tokio never
    /// owned the future, so there is no `JoinHandle::abort` lifecycle to
    /// mirror — and the drop lands under the registered context, so the timer
    /// wheel deregistration succeeds.
    #[test]
    fn cancelling_a_call_drops_a_live_tokio_leaf() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _registration = register_tokio(rt.handle().clone());
        let (exec, log) = recording();

        struct DropFlag(Arc<AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = DropFlag(dropped.clone());

        exec.spawn(2, async move {
            let _guard = guard;
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Outcome::Ok(FramedWriter::status(STATUS_OK))
        });
        assert!(exec.drain_one(), "armed on a one-hour timer, Pending");
        assert!(!dropped.load(Ordering::SeqCst));

        assert!(exec.cancel(2), "the cancel claims the answer");
        exec.drain_all();
        assert!(
            dropped.load(Ordering::SeqCst),
            "the future — and the tokio leaf inside it — is gone"
        );
        assert!(log.lock().unwrap().is_empty(), "a cancelled call never completes");
        assert_eq!(exec.task_count(), 0);

        // The runtime survives having a registration torn out from under it.
        rt.block_on(async { tokio::time::sleep(Duration::from_millis(1)).await });
    }

    /// A cancelled call's **teardown** runs with the runtime still under it.
    ///
    /// This is the reason [`crate::runtime::enter`] wraps the whole of
    /// `Runnable::run()`: that one call polls a live task *and* drops a
    /// cancelled one, and a teardown that reaches for `Handle::current()` —
    /// arming a timeout, spawning a cleanup task — is ordinary in exactly the
    /// bodies this hook exists for. Outside a context it panics inside `Drop`,
    /// and async-task wraps a future's destructor in `abort_on_panic` (4.7.1
    /// `raw.rs:447-449`), so that is a **process abort**, not a catchable
    /// error.
    ///
    /// Which is why only the positive case is asserted, here, and there is no
    /// negative twin: running the same drop outside `enter()` was measured
    /// during development and it takes the test binary with it ("panic in a
    /// destructor during cleanup ... thread caused non-unwinding panic"), so it
    /// cannot be written as a passing test. If the hook regresses to wrapping
    /// only the poll, this test does not fail — the whole binary dies. That is
    /// the loudest signal available and it is the point.
    #[test]
    fn a_cancelled_calls_teardown_still_has_the_runtime() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _registration = register_tokio(rt.handle().clone());
        let (exec, log) = recording();

        static TORN_DOWN: AtomicBool = AtomicBool::new(false);
        struct SpawnsOnDrop;
        impl Drop for SpawnsOnDrop {
            fn drop(&mut self) {
                // Panics without a context — and a panicking destructor
                // during cleanup aborts.
                tokio::runtime::Handle::current().spawn(async {});
                TORN_DOWN.store(true, Ordering::SeqCst);
            }
        }
        TORN_DOWN.store(false, Ordering::SeqCst);

        exec.spawn(9, async {
            let _teardown = SpawnsOnDrop;
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Outcome::Ok(FramedWriter::status(STATUS_OK))
        });
        assert!(exec.drain_one(), "suspended on a one-hour timer");
        assert!(exec.cancel(9));
        exec.drain_all();

        assert!(
            TORN_DOWN.load(Ordering::SeqCst),
            "the cancelled future's teardown never completed"
        );
        assert!(log.lock().unwrap().is_empty());
    }

    /// Many concurrent tokio-leaf calls multiplex on **one** thread. This is
    /// the resource claim the hook is for, stated as a number: the
    /// bring-your-own-`block_on` pattern would need 200 parked pool threads to
    /// do this, and could not cancel any of them.
    #[test]
    fn two_hundred_tokio_sleeps_multiplex_on_one_thread() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _registration = register_tokio(rt.handle().clone());
        let (exec, log) = recording();

        for id in 0..200 {
            exec.spawn(id, async {
                tokio::time::sleep(Duration::from_millis(50)).await;
                Outcome::Ok(FramedWriter::status(STATUS_OK))
            });
        }
        let started = Instant::now();
        while log.lock().unwrap().len() < 200 {
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "only {} of 200 completed",
                log.lock().unwrap().len()
            );
            if !exec.drain_one() {
                std::thread::yield_now();
            }
        }
        assert_eq!(exec.task_count(), 0, "no leaked tasks");
        // 200 × 50ms serialized would be 10s; concurrent is ~50ms. The bound
        // is loose because CI machines are not fast, but it is nowhere near
        // serial.
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the sleeps ran serially ({:?}) — they are not multiplexing",
            started.elapsed()
        );

    }
}
