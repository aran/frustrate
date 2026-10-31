//! Compiled unchanged into both crates of //tests/compose/twin: the same type
//! and member names in each, so only the crate name in the generated exports
//! tells the two bridges apart once they are linked into one library.

use frustrate::bridge;
use std::sync::atomic::{AtomicI64, Ordering};

static VALUE: AtomicI64 = AtomicI64::new(0);
static DROPS: AtomicI64 = AtomicI64::new(0);

#[bridge(sync)]
pub fn put(v: i64) {
    VALUE.store(v, Ordering::SeqCst);
}

#[bridge(sync)]
pub fn get() -> i64 {
    VALUE.load(Ordering::SeqCst)
}

#[bridge]
pub async fn twice(x: i64) -> i64 {
    x * 2
}

#[bridge(sync)]
pub fn drops() -> i64 {
    DROPS.load(Ordering::SeqCst)
}

#[bridge(confined)]
pub struct Twin {
    value: i64,
}

#[bridge]
impl Twin {
    #[bridge(sync)]
    pub fn new(value: i64) -> Self {
        Twin { value }
    }

    #[bridge(sync)]
    pub fn value(&self) -> i64 {
        self.value
    }
}

impl Drop for Twin {
    fn drop(&mut self) {
        DROPS.fetch_add(1, Ordering::SeqCst);
    }
}
