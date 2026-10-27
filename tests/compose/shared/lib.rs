//! One value, written by one bridge and read by the other.

use std::sync::atomic::{AtomicI64, Ordering};

static VALUE: AtomicI64 = AtomicI64::new(0);

pub fn put(v: i64) {
    VALUE.store(v, Ordering::SeqCst);
}

pub fn get() -> i64 {
    VALUE.load(Ordering::SeqCst)
}
