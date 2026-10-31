use frustrate::bridge;

/// Reaches nothing any analysis finds, and carries a silence anyway.
#[bridge(sync, no_block(allow(sqlite3_step)))]
pub fn plain(x: i32) -> i32 {
    x + 1
}

#[frustrate::no_block(allow)]
fn plus_one(x: i64) -> i64 {
    x + 1
}

/// Goes through a marked function with nothing under it.
#[bridge(sync, no_block)]
pub fn add(x: i64) -> i64 {
    plus_one(x)
}
