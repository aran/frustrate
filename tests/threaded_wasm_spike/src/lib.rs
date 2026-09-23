//! Threaded wasm spike: real shared-memory threads in a wasm module, no
//! wasm-bindgen. One shared WebAssembly.Memory across N instances, per-worker
//! stack + TLS init done by hand in JS glue, a `frustrate.spawn_worker`
//! import carrying boxed entry closures, std atomics AND std::sync::Mutex
//! (futex path) exercised off the main thread.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

#[link(wasm_import_module = "frustrate")]
extern "C" {
    /// JS glue: create a Worker, instantiate this module on the shared
    /// memory, init stack+TLS, then call `worker_entry(entry)`.
    fn spawn_worker(entry: u32);
}

static COUNTER: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicU32 = AtomicU32::new(0);
/// Exercises std's futex-backed Mutex across real threads.
static LOCKED_SUM: Mutex<u64> = Mutex::new(0);

type Job = Box<dyn FnOnce() + Send>;

fn spawn(job: Job) {
    let entry = Box::into_raw(Box::new(job)) as u32;
    unsafe { spawn_worker(entry) };
}

/// Entry point the worker glue calls after stack+TLS init.
///
/// # Safety
/// `entry` must be a pointer produced by `spawn` on the same shared memory,
/// used exactly once.
#[no_mangle]
pub unsafe extern "C" fn worker_entry(entry: u32) {
    let job = *Box::from_raw(entry as *mut Job);
    job();
    DONE.fetch_add(1, Ordering::SeqCst);
}

/// Spawn `workers` threads, each bumping the shared counter `iters` times
/// and folding `iters` into the mutex-guarded sum.
#[no_mangle]
pub extern "C" fn start_counters(workers: u32, iters: u32) {
    for _ in 0..workers {
        spawn(Box::new(move || {
            for _ in 0..iters {
                COUNTER.fetch_add(1, Ordering::Relaxed);
            }
            *LOCKED_SUM.lock().unwrap() += iters as u64;
        }));
    }
}

/// Spawn `workers` threads each computing the nth prime — the parallel leg
/// of the speedup measurement.
#[no_mangle]
pub extern "C" fn start_work(workers: u32, n: u32) {
    for _ in 0..workers {
        spawn(Box::new(move || {
            std::hint::black_box(nth_prime(n));
        }));
    }
}

/// The serial baseline: the same total work on the calling thread.
#[no_mangle]
pub extern "C" fn work_serial(times: u32, n: u32) {
    for _ in 0..times {
        std::hint::black_box(nth_prime(n));
    }
}

#[no_mangle]
pub extern "C" fn done_count() -> u32 {
    DONE.load(Ordering::SeqCst)
}

#[no_mangle]
pub extern "C" fn reset_done() {
    DONE.store(0, Ordering::SeqCst);
}

#[no_mangle]
pub extern "C" fn counter() -> u32 {
    COUNTER.load(Ordering::SeqCst)
}

/// Only sound to call once all workers are done (uncontended lock: futex
/// wait would trap on the main thread).
#[no_mangle]
pub extern "C" fn locked_sum() -> u64 {
    *LOCKED_SUM.lock().unwrap()
}

/// Shared (thread-safe, spinlocked under +atomics) allocator, used by the
/// glue to carve out worker stacks and TLS blocks.
#[no_mangle]
pub extern "C" fn wasm_alloc(size: u32, align: u32) -> u32 {
    let layout = std::alloc::Layout::from_size_align(size as usize, align as usize)
        .expect("bad layout");
    unsafe { std::alloc::alloc(layout) as u32 }
}

fn nth_prime(n: u32) -> u64 {
    let mut count = 0u32;
    let mut candidate = 1u64;
    while count < n {
        candidate += 1;
        if is_prime(candidate) {
            count += 1;
        }
    }
    candidate
}

fn is_prime(x: u64) -> bool {
    if x < 2 {
        return false;
    }
    let mut d = 2u64;
    while d * d <= x {
        if x % d == 0 {
            return false;
        }
        d += 1;
    }
    true
}
