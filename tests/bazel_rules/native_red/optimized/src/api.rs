use frustrate::bridge;
use std::sync::Mutex;

static TOTAL: Mutex<i64> = Mutex::new(0);

/// Claims `no_block` and takes a `Mutex`, in a crate built at opt-level 2.
/// The member takes an argument, so an optimizer handed an empty request
/// could prove its decode fails and delete the body.
#[bridge(sync, no_block)]
pub fn add(by: i64) -> i64 {
    let mut total = TOTAL.lock().unwrap();
    *total += by;
    *total
}
