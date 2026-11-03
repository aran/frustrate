//! A claimed member whose body touches a lazily initialised thread-local.

use std::cell::RefCell;

use frustrate::bridge;

thread_local! {
    static SEEN: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
}

/// Records a call on the calling thread and says how many it has seen.
#[bridge(sync, no_block)]
pub fn seen() -> u32 {
    SEEN.with_borrow_mut(|seen| {
        seen.push("seen");
        seen.len() as u32
    })
}
