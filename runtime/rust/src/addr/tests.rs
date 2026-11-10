//! `Addr` on the native host: ordering against calls, the self-call panic,
//! and what teardown does to posts.
//!
//! Each test runs its actor on a fresh host thread, whose object table and
//! root start empty, and holds `post::test_lock` because the host answers
//! every call through the process-global completion callback.

use super::*;
use crate::actor::{spawn_host, Host};
use crate::codec::FramedWriter;
use crate::test_wait::Count;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Mutex;
use std::time::Duration;

/// Answers go nowhere: these tests observe the actor, not the replies.
fn drain_completions() {
    extern "C" fn drain(_call_id: u64, ptr: *mut u8, len: u64, cap: u64) {
        drop(unsafe { Vec::from_raw_parts(ptr, len as usize, cap as usize) });
    }
    crate::post::init(drain);
}

/// Call ids no other test uses.
fn next_call() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(7_000_000);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Run `f` as a call on `host`.
fn on_host(host: &Host, f: impl FnOnce() + Send + 'static) {
    host.submit(next_call(), move || {
        f();
        Some(FramedWriter::from_bytes(vec![crate::envelope::STATUS_OK]))
    });
}

/// The test actor: a log of what ran on it, in order.
struct Log {
    me: Addr<Log>,
    log: Arc<Mutex<Vec<String>>>,
}

impl Log {
    fn note(&mut self, what: impl Into<String>) {
        self.log.lock().unwrap().push(what.into());
    }
}

/// The root, from a call on its own host.
fn root() -> &'static mut Log {
    unsafe { &mut *crate::objects::root::<Log>().expect("the root is built") }
}

/// Spawn a host and build its `Log` root there, as a constructor call would.
fn spawn_log() -> (Arc<Host>, Arc<Mutex<Vec<String>>>) {
    spawn_log_on(spawn_host(0))
}

/// [`spawn_log`] on a given host.
fn spawn_log_on(host: Arc<Host>) -> (Arc<Host>, Arc<Mutex<Vec<String>>>) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let theirs = Arc::clone(&log);
    on_host(&host, move || {
        crate::__private::handle::actor_new(
            Log {
                me: Addr::__here(),
                log: theirs,
            },
            "Log",
        );
    });
    (host, log)
}

fn recv<T>(rx: &Receiver<T>, what: &str) -> T {
    rx.recv_timeout(Duration::from_secs(10))
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

/// Poll `fut` to completion on this thread, parking between polls.
fn block_on<F: Future>(fut: F) -> F::Output {
    struct Unpark(std::thread::Thread);
    impl std::task::Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut fut = std::pin::pin!(fut);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
            return v;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the future never resolved"
        );
        std::thread::park_timeout(Duration::from_millis(50));
    }
}

/// A post runs after every message queued before it and before every message
/// queued after it: P1 is posted before C3 and C4 arrive, P2 after.
#[test]
fn a_post_runs_in_arrival_order_against_calls() {
    let _serial = crate::post::test_lock();
    drain_completions();
    let (host, log) = spawn_log();
    let (posted_tx, posted_rx) = channel::<()>();
    let (go_tx, go_rx) = channel::<()>();
    let (done_tx, done_rx) = channel::<()>();
    on_host(&host, move || {
        let r = root();
        r.note("A");
        r.me.post(|r| r.note("P1"));
        posted_tx.send(()).unwrap();
        // C3 and C4 are queued while this message is still running.
        recv(&go_rx, "C3 and C4 to be queued");
        r.me.post(move |r| {
            r.note("P2");
            done_tx.send(()).unwrap();
        });
    });
    recv(&posted_rx, "P1 to be posted");
    on_host(&host, || root().note("C3"));
    on_host(&host, || root().note("C4"));
    go_tx.send(()).unwrap();
    recv(&done_rx, "P2 to run");
    assert_eq!(*log.lock().unwrap(), ["A", "P1", "C3", "C4", "P2"]);
    host.request_stop();
}

/// A long job in slices: a call that arrives while a slice runs is served
/// before the next slice.
#[test]
fn a_call_arriving_mid_job_runs_before_the_next_slice() {
    let _serial = crate::post::test_lock();
    drain_completions();
    let (host, log) = spawn_log();
    let (mid_tx, mid_rx) = channel::<()>();
    let (go_tx, go_rx) = channel::<()>();
    let (done_tx, done_rx) = channel::<()>();
    struct Job {
        slice: u32,
        last: u32,
        mid: Sender<()>,
        go: Receiver<()>,
        done: Sender<()>,
    }
    fn step(r: &mut Log, mut job: Job) {
        job.slice += 1;
        r.note(format!("s{}", job.slice));
        if job.slice == 2 {
            // Hold this slice until the test has queued its call.
            job.mid.send(()).unwrap();
            recv(&job.go, "the call to be queued");
        }
        if job.slice == job.last {
            job.done.send(()).unwrap();
        } else {
            r.me.post(move |r| step(r, job));
        }
    }
    on_host(&host, move || {
        let job = Job {
            slice: 0,
            last: 4,
            mid: mid_tx,
            go: go_rx,
            done: done_tx,
        };
        root().me.post(move |r| step(r, job));
    });
    recv(&mid_rx, "slice 2");
    on_host(&host, || root().note("call"));
    go_tx.send(()).unwrap();
    recv(&done_rx, "the last slice");
    assert_eq!(*log.lock().unwrap(), ["s1", "s2", "call", "s3", "s4"]);
    host.request_stop();
}

/// Awaiting `call` on the actor's own executor panics at the first poll,
/// rather than waiting forever for a message that cannot run.
#[test]
fn awaiting_a_call_on_its_own_executor_panics_at_once() {
    let _serial = crate::post::test_lock();
    drain_completions();
    let (host, log) = spawn_log();
    let (tx, rx) = channel::<String>();
    on_host(&host, move || {
        let call = root().me.call(|r| r.note("ran"));
        let msg = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut call = std::pin::pin!(call);
            let _ = call.as_mut().poll(&mut Context::from_waker(Waker::noop()));
        }))
        .map(|_| String::from("no panic"))
        .unwrap_or_else(|p| crate::panic::message(&*p).to_string());
        tx.send(msg).unwrap();
    });
    let msg = recv(&rx, "the poll");
    assert!(msg.contains("awaited on its own actor's executor"), "{msg}");
    // Off the executor the same call is fine.
    let addr = {
        let (tx, rx) = channel();
        on_host(&host, move || tx.send(root().me.clone()).unwrap());
        recv(&rx, "the address")
    };
    assert_eq!(block_on(addr.call(|r| r.log.lock().unwrap().len())), Ok(1));
    assert_eq!(*log.lock().unwrap(), ["ran"]);
    host.request_stop();
}

/// Counts runs and drops of a posted closure's capture.
struct Tracked {
    drops: Arc<Count>,
}
impl Drop for Tracked {
    fn drop(&mut self) {
        self.drops.add();
    }
}

/// Teardown: a post that finds its root disposed, a post queued behind the
/// stop, and a post to a stopped host are each dropped without running, and
/// their captures are dropped; a call in any of those positions is `Gone`.
#[test]
fn posts_after_dispose_and_behind_the_stop_drop_unrun() {
    let _serial = crate::post::test_lock();
    drain_completions();
    let (host, log) = spawn_log();
    let ran = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(Count::new());
    let (addr_tx, addr_rx) = channel();
    let (gone_tx, gone_rx) = channel();
    let (first_tx, first_rx) = channel();
    {
        let (ran, drops) = (Arc::clone(&ran), Arc::clone(&drops));
        on_host(&host, move || {
            let me = root().me.clone();
            addr_tx.send(me.clone()).unwrap();
            let held = Tracked {
                drops: Arc::clone(&drops),
            };
            let ran1 = Arc::clone(&ran);
            // Queued ahead of the dispose call below, so it runs on a live
            // root.
            me.post(move |r| {
                let _held = held;
                ran1.fetch_add(1, Ordering::SeqCst);
                r.note("before dispose");
            });
            first_tx.send(()).unwrap();
        });
    }
    recv(&first_rx, "the first post");
    {
        let (ran, drops) = (Arc::clone(&ran), Arc::clone(&drops));
        on_host(&host, move || {
            let me = root().me.clone();
            // The orderly dispose: the drop call removes the root.
            crate::__private::handle::actor_drop::<Log>(crate::objects::root_slot());
            // Posted after the root is gone: dropped unrun.
            let held = Tracked {
                drops: Arc::clone(&drops),
            };
            let ran2 = Arc::clone(&ran);
            me.post(move |_| {
                let _held = held;
                ran2.fetch_add(1, Ordering::SeqCst);
            });
            gone_tx.send(me.call(|_| ())).unwrap();
        });
    }
    let addr: Addr<Log> = recv(&addr_rx, "the address");
    let gone = recv(&gone_rx, "the call");
    // Dropped unrun: the posted closure ran once (the first), both captures
    // dropped, and the call made after the dispose is Gone.
    assert_eq!(block_on(gone), Err(Gone));
    drops.wait_for(2, "captures dropped");
    assert_eq!(ran.load(Ordering::SeqCst), 1);
    assert_eq!(*log.lock().unwrap(), ["before dispose"]);

    // Queued behind the stop: the stop is requested from inside a message,
    // so everything that message posts afterwards lands behind it.
    let (stopped_tx, stopped_rx) = channel();
    {
        let (ran, drops) = (Arc::clone(&ran), Arc::clone(&drops));
        let addr = addr.clone();
        on_host(&host, move || {
            crate::actor::current().request_stop();
            let held = Tracked { drops };
            addr.post(move |_| {
                let _held = held;
                ran.fetch_add(1, Ordering::SeqCst);
            });
            stopped_tx.send(addr.call(|_| ())).unwrap();
        });
    }
    let behind = recv(&stopped_rx, "the stop");
    assert_eq!(block_on(behind), Err(Gone));
    drops.wait_for(3, "the capture behind the stop dropped");

    // After the host has stopped, a post is refused and dropped by the
    // poster, and a call is Gone.
    let held = Tracked {
        drops: Arc::clone(&drops),
    };
    let ran3 = Arc::clone(&ran);
    addr.post(move |_| {
        let _held = held;
        ran3.fetch_add(1, Ordering::SeqCst);
    });
    drops.wait_for(4, "the capture of a refused post dropped");
    assert_eq!(block_on(addr.call(|_| ())), Err(Gone));
    assert_eq!(ran.load(Ordering::SeqCst), 1, "a post ran after dispose");
}

/// A posted closure that panics is reported to its actor's owner as a panic
/// with no call, a call on a closure that panics resolves to `Err(Gone)`, and
/// the host serves the next message.
#[test]
fn a_panicking_post_reaches_its_owner_and_leaves_the_host_serving() {
    let _serial = crate::post::test_lock();
    drain_completions();
    static SEEN: Mutex<Vec<(u64, Vec<u8>)>> = Mutex::new(Vec::new());
    extern "C" fn record(id: u64, ptr: *mut u8, len: u64, cap: u64) {
        let bytes = unsafe { Vec::from_raw_parts(ptr, len as usize, cap as usize) };
        SEEN.lock().unwrap().push((id, bytes));
    }
    SEEN.lock().unwrap().clear();
    let isolate = crate::post::register_callback(record);
    let owner = (isolate << crate::post::ISOLATE_SHIFT) | 9;
    let (host, log) = spawn_log_on(crate::actor::spawn_owned(isolate as u32, owner));
    let (tx, rx) = channel();
    let (call_tx, call_rx) = channel();
    on_host(&host, move || {
        root().me.post(|_| panic!("a posted closure exploded"));
        call_tx
            .send(
                root()
                    .me
                    .call(|_| -> i64 { panic!("a called closure exploded") }),
            )
            .unwrap();
        root().me.post(move |r| {
            r.note("after");
            tx.send(()).unwrap();
        });
    });
    let call = recv(&call_rx, "the call");
    assert_eq!(block_on(call), Err(Gone));
    recv(&rx, "the post after the panics");
    assert_eq!(*log.lock().unwrap(), ["after"]);
    let seen = std::mem::take(&mut *SEEN.lock().unwrap());
    let panics: Vec<_> = seen
        .iter()
        .map(|(id, bytes)| {
            assert_eq!(*id, owner, "reported under the actor's owner");
            assert_eq!(bytes[0], crate::envelope::STATUS_PANIC);
            String::from_utf8_lossy(bytes).into_owned()
        })
        .collect();
    assert_eq!(panics.len(), 2, "one report per panic: {panics:?}");
    assert!(panics[0].contains("a posted closure exploded"));
    assert!(panics[1].contains("a called closure exploded"));
    host.request_stop();
}
