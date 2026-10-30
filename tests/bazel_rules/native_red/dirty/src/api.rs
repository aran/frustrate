use frustrate::bridge;

/// Claims `no_block` and takes the runtime's resident registry lock, whose
/// own section is clean. The build turns on a second section of the same lock
/// type elsewhere in the runtime that sleeps, so this member's wait on the
/// lock is unbounded and the claim cannot be verified.
#[bridge(sync, no_block)]
pub fn live() -> i64 {
    frustrate::resident::live_count() as i64
}
