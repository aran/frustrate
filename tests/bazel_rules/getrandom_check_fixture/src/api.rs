//! A `no_block` claim over a body that draws entropy from `getrandom`.

use frustrate::bridge;

/// `getrandom::fill` under the custom backend is a call out to frustrate's host
/// import, which waits for nothing; the claim is true.
#[bridge(sync, no_block)]
pub fn entropy_byte() -> u8 {
    let mut byte = [0u8; 1];
    let _ = getrandom::fill(&mut byte);
    byte[0]
}
