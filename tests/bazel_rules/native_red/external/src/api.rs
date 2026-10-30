use frustrate::bridge;

extern "C" {
    /// From `step.c`: a C function the scanner has never classified.
    fn native_red_step(x: i32) -> i32;
}

/// Claims `no_block` and calls into C. The native check cannot see into the
/// call, and it is on no list of functions known not to wait, so the claim
/// cannot be verified.
#[bridge(sync, no_block)]
pub fn step(x: i32) -> i32 {
    unsafe { native_red_step(x) }
}
