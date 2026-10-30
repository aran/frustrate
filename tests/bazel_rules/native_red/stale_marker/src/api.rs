use frustrate::bridge;

#[frustrate::no_block(allow)]
fn plus_one(x: i64) -> i64 {
    x + 1
}

/// Claims `no_block`, reaches nothing the check cannot verify, and goes
/// through a marked function anyway: one that silences nothing is an error.
#[bridge(sync, no_block)]
pub fn add(x: i64) -> i64 {
    plus_one(x)
}
