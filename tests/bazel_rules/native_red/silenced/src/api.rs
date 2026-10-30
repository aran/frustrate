use frustrate::bridge;

extern "C" {
    /// From `step.c`: a C function the scanner has never classified.
    fn native_red_silenced_step(x: i32) -> i32;
}

/// Claims `no_block` and calls into C, unsilenced.
#[bridge(sync, no_block)]
pub fn plain(x: i32) -> i32 {
    unsafe { native_red_silenced_step(x) }
}

/// The same call, silenced on this member with its reason.
#[bridge(sync, no_block(allow(native_red_silenced_step)))]
pub fn silenced(x: i32) -> i32 {
    unsafe { native_red_silenced_step(x) }
}
