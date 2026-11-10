use frustrate::bridge;
use std::sync::Mutex;

static TOTAL: Mutex<i64> = Mutex::new(0);

#[frustrate::no_block(allow)]
fn add_here(by: i64) -> i64 {
    let mut total = TOTAL.lock().unwrap();
    *total += by;
    *total
}

/// Takes a `Mutex` through a function that says the lock is fine there.
#[bridge(sync, no_block)]
pub fn add(by: i64) -> i64 {
    add_here(by)
}

/// Takes the `Mutex` itself, silenced by each analysis's name for the wait.
#[bridge(
    sync,
    no_block(allow(
        pthread_mutex_lock,
        syscall::SYS_futex::FUTEX_WAIT_BITSET,
        std::sys::sync::mutex::futex::Mutex::lock_contended,
    ))
)]
pub fn read() -> i64 {
    *TOTAL.lock().unwrap()
}
