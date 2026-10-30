use frustrate::bridge;
use std::sync::{Mutex, MutexGuard};

static TOTAL: Mutex<i64> = Mutex::new(0);

#[frustrate::no_block(allow)]
fn lock_uncontended_here() -> MutexGuard<'static, i64> {
    TOTAL.lock().unwrap()
}

/// Claims `no_block` and takes a `Mutex`, through a function that says the
/// lock is fine there.
#[bridge(sync, no_block)]
pub fn add(by: i64) -> i64 {
    let mut total = lock_uncontended_here();
    *total += by;
    *total
}
