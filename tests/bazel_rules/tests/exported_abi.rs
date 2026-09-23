//! An ordinary Rust integration test over a bridge crate — the thing a bridge
//! crate could not have until `frustrate_bridge_library` existed.
//!
//! This file is its own crate. It is not `mod`-ed into the fixture, it carries
//! no `#[path]`, and it cannot see anything the fixture does not make public —
//! which is the point: the surface it tests is the surface a *consumer* has.
//! Before the rlib half was declared, the only compilable form was
//! `rust_test(crate = …)`, which recompiles the crate's own sources with
//! `--test` and may not add `srcs`, so a file under `tests/` had to be smuggled
//! into some module of the crate behind `#[cfg(test)]`.
//!
//! The second test is the load-bearing one. `mod frustrate_generated` is
//! private, so from out here the generated dispatcher exists only as an
//! exported symbol — exactly the way the Dart transport sees it. Declaring it
//! `extern "C"` and calling it proves three things a build_test could not: the
//! rlib links, the generated glue is inside it, and its `#[no_mangle]` exports
//! survive into a linked binary rather than being dropped as unreferenced
//! archive members.

use frustrate::codec::{ByteReader, ByteWriter};
use frustrate::envelope::{STATUS_OK, STATUS_PANIC};

/// The dispatch id of a member, by the name the bridge declares it under.
/// Ids are derived from each member's wire facts, so this is a lookup rather
/// than a literal — a literal here would be a copy of the hash, wrong the
/// first time anyone changed the signature.
fn id_of(name: &str) -> u32 {
    rule_test_fixture::FRUSTRATE_FN_IDS
        .iter()
        .find(|(n, _)| *n == name)
        .unwrap_or_else(|| panic!("no bridged member named {name}"))
        .1
}

// The generated glue's ABI, declared here the way any foreign consumer would.
//
// `frustrate_call_sync` comes from the fixture crate's generated module;
// `frustrate_buffer_free` comes from `//runtime/rust:frustrate`, one rlib
// further down. Both are reached the same way, so this also covers the
// transitive case — the one every shipped module already depends on, since
// nearly all of frustrate's exports live in the runtime rather than in the
// bridge.
//
// Plain comments, not doc comments: rustdoc does not document extern blocks,
// and `unused_doc_comments` is a warning this package builds with `-Dwarnings`.
extern "C" {
    fn frustrate_call_sync(
        fn_id: u32,
        req: *const u8,
        req_len: u64,
        out: *mut u8,
        out_cap: u64,
    ) -> i32;
    fn frustrate_buffer_free(ptr: *mut u8, len: u64, cap: u64);
}

/// The crate's public Rust API, called from another crate. `#[bridge]` is an
/// attribute on an ordinary `pub fn`, so this is the plain-Rust half — the
/// half `use my_bridge::…` is supposed to give anyone, and did not.
#[test]
fn the_bridged_function_is_callable_as_ordinary_rust() {
    assert_eq!(rule_test_fixture::api::add(2, 3), 5);
}

/// One whole bridge call, driven the way Dart drives it: encode the arguments
/// with the runtime's codec, hand the dispatcher a response buffer, decode the
/// envelope it answered into.
///
/// `5` is the answer to `add(2, 3)`, not a recorded constant, so this cannot
/// pass by matching a stale expectation.
#[test]
fn a_bridge_call_round_trips_through_the_exported_c_abi() {
    let mut out = [0u8; 64];
    let n = unsafe { call_add(2, 3, out.as_mut_ptr(), out.len() as u64) };
    assert!(n >= 0, "a 5-byte envelope must fit a 64-byte buffer, got {n}");

    let mut r = ByteReader::new(&out[..n as usize]);
    assert_eq!(r.read_u8(), STATUS_OK, "response envelope status");
    assert_eq!(r.read_i32(), 5, "add(2, 3)");
    r.assert_consumed();
}

/// The other half of the same ABI: a response too large for the buffer is
/// leased instead — a `(ptr, len, cap)` triple in its first 24 bytes, released
/// with `frustrate_buffer_free`.
///
/// Driven with an unknown `fn_id`, which is the one response this fixture can
/// make longer than the smallest legal buffer: the dispatcher's own
/// "unknown sync fn_id" panic, caught and enveloped. So this pins two contracts
/// at once — a forged `fn_id` is loud rather than a wrong body, and the answer
/// still comes back correctly when it does not fit.
#[test]
fn a_response_too_large_for_the_buffer_is_leased_back() {
    let mut out = [0u8; 24];
    let n = unsafe {
        frustrate_call_sync(4242, std::ptr::null(), 0, out.as_mut_ptr(), out.len() as u64)
    };
    assert_eq!(n, -1, "the panic message must not have fitted 24 bytes");

    let mut triple = [0u64; 3];
    unsafe { std::ptr::copy_nonoverlapping(out.as_ptr(), triple.as_mut_ptr() as *mut u8, 24) };
    let (ptr, len, cap) = (triple[0] as *mut u8, triple[1], triple[2]);
    assert!(!ptr.is_null(), "the dispatcher leased no response buffer");

    let resp = unsafe { std::slice::from_raw_parts(ptr, len as usize) };
    let mut r = ByteReader::new(resp);
    assert_eq!(r.read_u8(), STATUS_PANIC, "response envelope status");
    let msg = r.read_string();
    assert!(msg.contains("unknown sync fn_id 4242"), "{msg}");
    r.assert_consumed();

    unsafe { frustrate_buffer_free(ptr, len, cap) };
}

/// # Safety
/// `out` must be valid for writes of `out_cap` bytes, and `out_cap >= 24`.
unsafe fn call_add(a: i32, b: i32, out: *mut u8, out_cap: u64) -> i32 {
    let mut w = ByteWriter::new();
    w.write_i32(a);
    w.write_i32(b);
    let req = w.take();
    unsafe {
        frustrate_call_sync(
            id_of("add"),
            req.as_ptr(),
            req.len() as u64,
            out,
            out_cap,
        )
    }
}
