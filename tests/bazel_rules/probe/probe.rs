//! A crate that reaches std facilities `wasm32-unknown-unknown` cannot serve.
//!
//! The red fence for `//bazel/wasm_std_check`. Its unit tests prove the
//! scanner's *logic* against hand-built modules; this proves the thing those
//! modules stand in for — that a real toolchain, building real Rust, leaves the
//! symbols the scanner looks for, at the optimization levels this repo builds
//! at. Without it, a rustc that started inlining `SystemTime::now` away would
//! turn the check into a silent no-op with every test still green.
//!
//! Deliberately not the `fixture_shared` crate next door: that one carries the
//! `-Dwarnings` lint-fence contract documented in //tests/lint_clean, and
//! reaching a facility that traps has no business inside it. This crate bridges
//! nothing and needs no codegen — the scanner reads a compiled module, not an
//! API surface.
//!
//! `#[no_mangle] pub extern "C"` on each export is load-bearing twice over: it
//! makes the function a gc root so `--gc-sections` cannot delete the call, and
//! the returned value is used by the caller so the body cannot fold to a
//! constant.

/// Reaches `std::time::SystemTime::now`, which on this platform is
/// `panic!("time not implemented on this platform")`.
#[no_mangle]
pub extern "C" fn probe_system_time() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(-1)
}

/// Reaches `std::io::stdio::_print`, which on this platform accepts the write
/// and discards it.
#[no_mangle]
pub extern "C" fn probe_println(n: i32) {
    println!("probe_println {n}");
}

/// The same for stderr (`std::io::stdio::_eprint`). Separate because the two
/// are separate symbols, and a facility the scanner claims to cover but no
/// fixture reaches is a claim nothing checks.
#[no_mangle]
pub extern "C" fn probe_eprintln(n: i32) {
    eprintln!("probe_eprintln {n}");
}
