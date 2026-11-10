//! The scanner's memory budget.
//!
//! Every analysis here is meant to need memory linear in its input. The
//! global allocator holds the scanner to that: it counts the bytes the heap
//! holds, and once they pass the budget for the input, the scanner prints why
//! and exits with a failure, so the check fails rather than taking the
//! machine down or passing unchecked. The budget is [`BASE`] plus
//! [`PER_INPUT_BYTE`] bytes per byte of the module.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::OnceLock;

/// The budget before any input is known, and its fixed part after.
const BASE: usize = 2 << 30;

/// The budget per byte of input. The scan holds the module's text once and a
/// parsed form of it, a few times its size.
const PER_INPUT_BYTE: usize = 8;

static USED: AtomicUsize = AtomicUsize::new(0);
static LIMIT: AtomicUsize = AtomicUsize::new(BASE);
static MESSAGE: OnceLock<String> = OnceLock::new();
/// Set once the budget is passed: what the exit allocates is not charged.
static STOPPING: AtomicBool = AtomicBool::new(false);

pub(crate) struct Budgeted;

/// The budget for an input of `len` bytes.
fn budget_for(len: u64) -> usize {
    BASE.saturating_add(usize::try_from(len).unwrap_or(usize::MAX).saturating_mul(PER_INPUT_BYTE))
}

/// Set the budget for the scan of `path`, the input `label` names.
pub(crate) fn set(path: &str, label: &str) {
    let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let limit = budget_for(len);
    let mb = |n: u64| n.div_ceil(1 << 20);
    let _ = MESSAGE.set(format!(
        "no_block_check: cannot verify {label}: the scanner passed its memory \
         budget of {} MB on this input ({} MB), so it stopped. Its analyses are \
         meant to need memory linear in the input, so this is a bug in the \
         scanner, not in the code it checks: report it to frustrate with the input.\n",
        mb(limit as u64),
        mb(len)
    ));
    LIMIT.store(limit, Ordering::Relaxed);
}

#[cold]
fn over() -> ! {
    // Writing to fd 2 directly allocates nothing; std's `stderr()` may, on a
    // thread's first use of it, and would come back here.
    use std::os::fd::FromRawFd;
    let msg = MESSAGE.get().map(String::as_str).unwrap_or(
        "no_block_check: cannot verify: the scanner passed its memory budget, so it stopped.\n",
    );
    let mut err = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(2) });
    let _ = err.write_all(msg.as_bytes());
    std::process::exit(1)
}

fn charge(n: usize) {
    let used = USED.fetch_add(n, Ordering::Relaxed).saturating_add(n);
    if used > LIMIT.load(Ordering::Relaxed) && !STOPPING.swap(true, Ordering::Relaxed) {
        over();
    }
}

unsafe impl GlobalAlloc for Budgeted {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        charge(layout.size());
        System.alloc(layout)
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        charge(layout.size());
        System.alloc_zeroed(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
        USED.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if new_size > layout.size() {
            charge(new_size - layout.size());
        } else {
            USED.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
        }
        System.realloc(ptr, layout, new_size)
    }
}
