//! The idle barrier: how much work the runtime has queued or running, and a
//! post to a Dart port when that reaches none.
//!
//! For a test harness that needs to know nothing is running, rather than wait
//! out a stretch of stillness. The work counted is every job on the native pool
//! — a plain async call, and every executor drain — and every call sent to an
//! actor host, from when it is queued until it is answered or dropped. A
//! thread the bridge's own code starts is not frustrate's to count, and
//! neither is a task suspended on something outside the runtime (a Dart timer,
//! an answer from Dart): a suspended task is off every queue until its waker
//! puts it back, and that wake is a pool job again.
//!
//! # What zero means
//!
//! Each job posts its answer before it stops being counted, so when the count
//! reaches zero every answer posted before that is already in its isolate's
//! message queue. The notice goes into the same isolate's queue after them,
//! and the VM delivers one isolate's messages in the order they were posted,
//! whichever of its ports each was sent to. So by the time the harness handles
//! the notice, it has handled every one of those answers.
//!
//! Zero is a moment, not a state: the notice says the count was zero when it
//! was posted. The [generation](Idle::generation) goes up on every job, so a
//! harness that reads the same generation and a zero count back on handling the
//! notice knows no job has started since.
//!
//! Present only under `--cfg frustrate_idle_barrier` (`//bazel:idle_barrier`,
//! `frustrate_idle_barrier_library`). Every other build gets the [`Pending`]
//! below that is a zero-sized type with no `Drop`, so the pool and the actor
//! hosts carry no count at all.

#[cfg(all(frustrate_idle_barrier, target_family = "wasm"))]
compile_error!("frustrate_idle_barrier is native-only: a wasm build has no pool to count");

// Under the manual scheduler a runnable task waits on the run queue for the
// host to drain it, with no pool job standing for it, so a zero here would
// say "idle" with work still to do.
#[cfg(all(frustrate_idle_barrier, frustrate_manual_scheduler))]
compile_error!(
    "frustrate_idle_barrier does not count the manual scheduler's run queue; \
     a harness that drains it already knows when nothing is runnable"
);

#[cfg(frustrate_idle_barrier)]
pub(crate) use barrier::Pending;

/// One job's share of the count. Here, nothing.
///
/// Native only: the callers are the native pool and the actor hosts, and a
/// wasm build has neither, so there it would be dead code.
#[cfg(all(not(frustrate_idle_barrier), not(target_family = "wasm")))]
pub(crate) struct Pending;

#[cfg(all(not(frustrate_idle_barrier), not(target_family = "wasm")))]
impl Pending {
    #[inline(always)]
    pub(crate) fn begin() -> Self {
        Pending
    }
}

#[cfg(frustrate_idle_barrier)]
mod barrier {
    use std::sync::{Mutex, MutexGuard, PoisonError};

    /// The count, the generation and the ports waiting for zero.
    ///
    /// One lock over all three, and the notices are posted while holding it,
    /// so each operation below is one indivisible step. That is what lets the
    /// tests check every order a set of operations can run in by running them
    /// in each order, and it costs nothing a production build pays.
    pub(crate) struct Idle {
        state: Mutex<State>,
    }

    struct State {
        pending: u64,
        generation: u64,
        waiters: Vec<i64>,
    }

    impl Idle {
        pub(crate) const fn new() -> Self {
            Idle {
                state: Mutex::new(State {
                    pending: 0,
                    generation: 0,
                    waiters: Vec::new(),
                }),
            }
        }

        // A panic while the lock is held is a panic inside `post`, after the
        // state is already consistent, so a poisoned lock is still right.
        fn lock(&self) -> MutexGuard<'_, State> {
            self.state.lock().unwrap_or_else(PoisonError::into_inner)
        }

        pub(crate) fn begin(&self) {
            let mut state = self.lock();
            state.pending += 1;
            state.generation += 1;
        }

        /// One job is finished. At zero, post the generation to every
        /// waiting port.
        pub(crate) fn end(&self, mut post: impl FnMut(i64, u64)) {
            let mut state = self.lock();
            state.pending -= 1;
            if state.pending == 0 {
                let generation = state.generation;
                for port in std::mem::take(&mut state.waiters) {
                    post(port, generation);
                }
            }
        }

        /// Post the generation to `port` the next time the count is zero:
        /// now, if it is.
        pub(crate) fn notify(&self, port: i64, mut post: impl FnMut(i64, u64)) {
            let mut state = self.lock();
            if state.pending == 0 {
                post(port, state.generation);
            } else {
                state.waiters.push(port);
            }
        }

        pub(crate) fn pending(&self) -> u64 {
            self.lock().pending
        }

        pub(crate) fn generation(&self) -> u64 {
            self.lock().generation
        }
    }

    static IDLE: Idle = Idle::new();

    fn post(port: i64, generation: u64) {
        // A closed port is a harness that stopped waiting; nothing to do.
        let _ = crate::post::post_int(port, generation as i64);
    }

    /// One job's share of the count, from [`begin`](Pending::begin) until it
    /// drops. Carried inside the job, so a job that is dropped unrun — a
    /// stopped actor host's queue — stops being counted too.
    pub(crate) struct Pending(());

    impl Pending {
        pub(crate) fn begin() -> Self {
            IDLE.begin();
            Pending(())
        }
    }

    impl Drop for Pending {
        fn drop(&mut self) {
            IDLE.end(post);
        }
    }

    /// Post the generation to `port` once nothing is queued or running.
    #[unsafe(no_mangle)]
    pub extern "C" fn frustrate_idle_notify(port: i64) {
        IDLE.notify(port, post);
    }

    /// How many jobs are queued or running.
    #[unsafe(no_mangle)]
    pub extern "C" fn frustrate_idle_pending() -> u64 {
        IDLE.pending()
    }

    /// How many jobs have ever been queued.
    #[unsafe(no_mangle)]
    pub extern "C" fn frustrate_idle_generation() -> u64 {
        IDLE.generation()
    }

    #[cfg(test)]
    mod tests {
        use super::Idle;
        use std::cell::RefCell;

        /// What lands in one isolate's message queue, in order.
        #[derive(Clone, Copy, Debug, PartialEq)]
        enum Message {
            Answer(usize),
            Idle(i64),
        }

        /// One indivisible step. A job is `Begin`, `Answer`, `End`; a
        /// harness's wait is one `Notify`.
        #[derive(Clone, Copy, Debug)]
        enum Step {
            Begin,
            Answer(usize),
            End,
            Notify(i64),
        }

        /// Every way to interleave `threads`, keeping each one's own order.
        fn interleavings(threads: &[Vec<Step>]) -> Vec<Vec<Step>> {
            fn go(
                threads: &[Vec<Step>],
                at: &mut [usize],
                run: &mut Vec<Step>,
                out: &mut Vec<Vec<Step>>,
            ) {
                let mut moved = false;
                for t in 0..threads.len() {
                    if at[t] < threads[t].len() {
                        moved = true;
                        run.push(threads[t][at[t]]);
                        at[t] += 1;
                        go(threads, at, run, out);
                        at[t] -= 1;
                        run.pop();
                    }
                }
                if !moved {
                    out.push(run.clone());
                }
            }
            let mut out = Vec::new();
            go(
                threads,
                &mut vec![0; threads.len()],
                &mut Vec::new(),
                &mut out,
            );
            out
        }

        /// Run `steps` against a fresh [`Idle`] and return the queue.
        fn run(started: usize, steps: &[Step]) -> Vec<Message> {
            let idle = Idle::new();
            for _ in 0..started {
                idle.begin();
            }
            let queue = RefCell::new(Vec::new());
            let post = |port: i64, _generation: u64| queue.borrow_mut().push(Message::Idle(port));
            for &step in steps {
                match step {
                    Step::Begin => idle.begin(),
                    Step::Answer(job) => queue.borrow_mut().push(Message::Answer(job)),
                    Step::End => idle.end(post),
                    Step::Notify(port) => idle.notify(port, post),
                }
            }
            assert_eq!(idle.pending(), 0, "every job ended");
            queue.into_inner()
        }

        /// The barrier's claim, in every order: each harness hears exactly
        /// once, and only after every answer from a job that started before
        /// it asked.
        ///
        /// Jobs 0 and 1 are already queued when the harnesses ask, as an
        /// answer the harness is waiting for would be; job 2 starts at any
        /// point, before, between or after them, and is owed to a harness
        /// that asked after it started. Two harnesses ask, so one
        /// end can owe several notices.
        #[test]
        fn a_notice_follows_every_answer_owed_before_it_in_every_order() {
            let threads = [
                vec![Step::Answer(0), Step::End],
                vec![Step::Answer(1), Step::End],
                vec![Step::Begin, Step::Answer(2), Step::End],
                vec![Step::Notify(10)],
                vec![Step::Notify(11)],
            ];
            let orders = interleavings(&threads);
            assert_eq!(
                orders.len(),
                15120,
                "9 steps in runs of 2, 2, 3, 1, 1: 9!/(2!·2!·3!)"
            );
            for order in &orders {
                let queue = run(2, order);
                for port in [10, 11] {
                    let notices: Vec<usize> = (0..queue.len())
                        .filter(|&i| queue[i] == Message::Idle(port))
                        .collect();
                    assert_eq!(
                        notices.len(),
                        1,
                        "port {port} heard once in {order:?}: {queue:?}"
                    );
                    // Job 2 is owed only when it began before this harness asked.
                    let asked = order
                        .iter()
                        .position(|s| matches!(s, Step::Notify(p) if *p == port));
                    let began = order.iter().position(|s| matches!(s, Step::Begin));
                    let owed: &[usize] = if began < asked { &[0, 1, 2] } else { &[0, 1] };
                    for &job in owed {
                        let answer = queue.iter().position(|&m| m == Message::Answer(job));
                        assert!(
                            answer.is_some_and(|a| a < notices[0]),
                            "job {job}'s answer precedes port {port}'s notice in {order:?}: {queue:?}"
                        );
                    }
                }
            }
        }

        /// The generation a notice carries counts every job begun before it,
        /// so a job that began and ended unseen still moves it.
        #[test]
        fn the_generation_moves_on_every_job() {
            let idle = Idle::new();
            let heard = RefCell::new(Vec::new());
            let post = |_: i64, generation: u64| heard.borrow_mut().push(generation);
            idle.notify(1, post);
            idle.begin();
            idle.end(post);
            idle.notify(1, post);
            assert_eq!(heard.into_inner(), [0, 1]);
            assert_eq!(idle.generation(), 1);
        }
    }
}
