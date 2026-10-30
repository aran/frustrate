use frustrate::bridge;

/// Claims `no_block`, reaches nothing the check cannot verify, and carries a
/// silence anyway: one that silences nothing is an error.
#[bridge(sync, no_block(allow(sqlite3_step)))]
pub fn plain(x: i32) -> i32 {
    x + 1
}
