use frustrate::bridge;

#[inline(never)]
extern "C" fn waiter() {
    std::thread::sleep(std::time::Duration::from_millis(1));
}

/// Claims `no_block` and calls a function that sleeps from inline asm whose
/// template holds a `;`.
#[bridge(sync, no_block)]
pub fn jump() {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!("nop; bl {f}", f = sym waiter, clobber_abi("C"))
    };
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!("nop; call {f}", f = sym waiter, clobber_abi("C"))
    };
}
