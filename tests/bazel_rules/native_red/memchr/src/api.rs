use frustrate::bridge;

/// Claims `no_block` and searches with memchr's routines, which on x86_64
/// pick their CPU's implementation at run time through a function pointer
/// kept in a static.
#[bridge(sync, no_block)]
pub fn find(hay: String) -> i64 {
    let b = hay.as_bytes();
    let first = memchr::memchr2(b'"', b'\\', b).map_or(-1, |i| i as i64);
    let last = memchr::memrchr(b'\n', b).map_or(-1, |i| i as i64);
    first + last + memchr::memchr_iter(b'\n', b).count() as i64
}
