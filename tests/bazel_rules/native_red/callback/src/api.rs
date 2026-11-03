use frustrate::bridge;
use std::sync::OnceLock;

static CALLBACK: OnceLock<Box<dyn Fn(i32) -> i32 + Send + Sync>> = OnceLock::new();

/// Installs the callback. Unclaimed: nothing a claimed member runs.
#[bridge(sync)]
pub fn install() {
    let _ = CALLBACK.set(Box::new(|x| x + 1));
}

/// Claims `no_block` and calls whatever `install` put behind the static.
#[bridge(sync, no_block)]
pub fn call(x: i32) -> i32 {
    match CALLBACK.get() {
        Some(f) => f(x),
        None => x,
    }
}
