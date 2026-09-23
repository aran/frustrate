//! A deliberately small but representative FRB surface.
//!
//! Small enough that the generated code is readable and cheap to check in;
//! representative enough that the generated Rust exercises the parts of the
//! FRB runtime that matter — the sync codec, the async handler, a struct
//! mirror, an opaque-free `Vec<u8>`, and a stream sink.

use flutter_rust_bridge::frb;

// NOT `flutter_rust_bridge::StreamSink` — that path does not exist. FRB's
// stream sink is re-exported by the *generated* module, so this `use` line
// makes the hand-written api source depend on codegen's output. The cycle
// (codegen needs the crate to expand; the crate needs codegen's output to
// compile) is why `mod frb_generated;` can name a file that is not there yet:
// FRB expands under `--cfg frb_expand`, which the generated module is written
// to tolerate.
use crate::frb_generated::StreamSink;

/// Sync, scalars only. The cheapest thing FRB can generate.
#[frb(sync)]
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

/// Sync, String in and out.
#[frb(sync)]
pub fn greet(name: String) -> String {
    format!("Hello, {name}!")
}

/// Sync, a Vec<u8> argument (the wire-heavy case).
#[frb(sync)]
pub fn sum_bytes(data: Vec<u8>) -> u64 {
    data.iter().map(|b| *b as u64).sum()
}

/// A mirrored struct, so the generated Dart carries a class and the
/// generated Rust carries a CST encode/decode pair.
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[frb(sync)]
pub fn translate(p: Point, dx: f64, dy: f64) -> Point {
    Point {
        x: p.x + dx,
        y: p.y + dy,
    }
}

/// Async (FRB's default), so the generated code carries the handler path.
pub fn double_slowly(x: i32) -> i32 {
    x * 2
}

/// A stream, so the generated code carries a StreamSink.
pub fn tick(sink: StreamSink<i32>, n: i32) {
    for i in 0..n {
        let _ = sink.add(i);
    }
}

/// A typed error, so the generated code carries the anyhow/Result path.
pub fn checked_div(a: i32, b: i32) -> Result<i32, String> {
    if b == 0 {
        Err("division by zero".to_owned())
    } else {
        Ok(a / b)
    }
}
