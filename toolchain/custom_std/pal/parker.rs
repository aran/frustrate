//! Thread parking for a std with no threads.
//!
//! std's own parker for this configuration is empty: `park` returns at once, so
//! a body that parks waiting for an `unpark` spins the only thread forever and
//! nothing says so. This parker holds a wake token instead, which is the whole
//! contract `thread::park` documents: `unpark` makes the next `park` return at
//! once, and a `park` with no token has nothing left to wait for.
//!
//! With one thread, a `park` that finds no token can never be woken, so it
//! panics rather than blocking. `park_timeout` consumes the token or returns,
//! which is what waiting out a timeout on a single thread amounts to.

use crate::cell::Cell;
use crate::pin::Pin;
use crate::time::Duration;

pub struct Parker {
    // This configuration has no threads, so a Cell is enough.
    token: Cell<bool>,
}

unsafe impl Send for Parker {}
unsafe impl Sync for Parker {} // no threads on this platform

impl Parker {
    pub unsafe fn new_in_place(parker: *mut Parker) {
        unsafe { parker.write(Parker { token: Cell::new(false) }) }
    }

    pub unsafe fn park(self: Pin<&Self>) {
        if !self.token.replace(false) {
            panic!("park on single-threaded wasm: nothing can unpark this thread");
        }
    }

    pub unsafe fn park_timeout(self: Pin<&Self>, _dur: Duration) {
        self.token.set(false);
    }

    pub fn unpark(self: Pin<&Self>) {
        self.token.set(true);
    }
}
