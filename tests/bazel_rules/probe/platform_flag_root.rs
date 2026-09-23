//! The cdylib that pulls `platform_flag_probe` into a wasm module build.
//!
//! Deliberately trivial and deliberately does not *call* the dependency: the
//! fence is a `compile_error!`, which fires when the dep's rlib is compiled,
//! and compiling it is what depending on it already guarantees. Calling it
//! would only add a way for the linker's dead-code elimination to become part
//! of what this test asserts.

#[no_mangle]
pub extern "C" fn platform_flag_probe_root() -> i32 {
    0
}
