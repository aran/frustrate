use frustrate::bridge;
use std::collections::HashMap;

/// Claims `no_block` and builds a `HashMap` with std's `RandomState`, whose
/// first use on a thread draws its keys from the kernel.
#[bridge(sync, no_block)]
pub fn count(words: Vec<String>) -> u32 {
    let mut seen: HashMap<String, u32> = HashMap::new();
    for w in words {
        *seen.entry(w).or_default() += 1;
    }
    seen.len() as u32
}
