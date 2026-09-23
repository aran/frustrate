// Three lints are off for this fixture, because what they ask for would make
// it a worse test of the bridge:
//   * `too_many_arguments` — `mix_scalars` takes one parameter per scalar
//     type on purpose; that width IS the case under test.
//   * `new_without_default` — every `new` here is a *bridged constructor*
//     that Dart calls across the ABI. A `Default` impl would be Rust-side
//     dead code added only to satisfy the lint.
//   * `boxed_local` — `rebox` takes a `Box<i64>` nobody would write, which is
//     the point: the claim under test is that a `Box` around a value type is
//     invisible to the wire and to Dart, and the way to test that is to box
//     something that gains nothing from it. Clippy is right about the Rust
//     and wrong about the fixture.
#![allow(
    clippy::too_many_arguments,
    clippy::new_without_default,
    clippy::boxed_local
)]

//! The frustrate integration-test bridge API.
//!
//! Exercises the full v1 surface: every scalar, strings, bytes, fixed byte
//! arrays, collections, options, structs, data enums, Results, panics, and
//! all three concurrency models (shaped after the real automerge-style
//! usage: a Confined text document, a Frozen snapshot, a Locked counter).

use anyhow::{bail, Result};
use frustrate::{bridge, Data, DartCallback, DartFunction, Deferred, Locked, StreamSink};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

// ------------------------------------------------------------- functions --

/// Also the block-check fixture's **sync** half (`tools/check_block.dart`), and
/// the member its red fence is proven on: the simplest witness the gate can
/// produce, since the root calls `call_N_add_i32` which calls this body, so a
/// lock taken here appears as a named root-to-wait path with nothing clever in
/// between.
#[bridge(sync, no_block)]
pub fn add_i32(a: i32, b: i32) -> i32 {
    a.wrapping_add(b)
}

#[bridge(sync)]
pub fn mix_scalars(
    b: bool,
    x8: i8,
    x16: i16,
    x32: i32,
    x64: i64,
    u8v: u8,
    u16v: u16,
    u32v: u32,
    f32v: f32,
    f64v: f64,
    us: usize,
    is_: isize,
    u64v: u64,
) -> String {
    format!("{b}|{x8}|{x16}|{x32}|{x64}|{u8v}|{u16v}|{u32v}|{f32v}|{f64v}|{us}|{is_}|{u64v}")
}

/// u64 decode direction: values above i64::MAX must arrive intact as
/// BigInt.
#[bridge(sync)]
pub fn u64_extremes() -> Vec<u64> {
    vec![0, 1, 1 << 63, u64::MAX]
}

#[bridge(sync)]
pub fn name_by_id(ids: HashMap<u64, String>, key: u64) -> Option<String> {
    ids.get(&key).cloned()
}

#[bridge(sync)]
pub fn bump_u64(x: Option<u64>) -> Option<u64> {
    x.map(|v| v.wrapping_add(1))
}

/// Nested option: the inner `Option` crosses
/// as a faithful `FrOption<int>`, so outer-None, Some(None), and Some(Some)
/// stay distinct instead of all collapsing to Dart `null`.
#[bridge(sync)]
pub fn nest_opt(x: Option<Option<i64>>) -> Option<Option<i64>> {
    x
}

#[bridge]
pub fn concat_strings(parts: Vec<String>, sep: String) -> String {
    parts.join(&sep)
}

#[bridge(sync)]
pub fn rev_bytes(data: Vec<u8>) -> Vec<u8> {
    data.into_iter().rev().collect()
}

/// First-class immutable `&[u8]` borrow. On a sync member this borrows the
/// request buffer itself — the generated arm is `read_bytes_borrowed()`, so
/// no owned `Vec` exists anywhere on the path and the body reads the bytes
/// where the transport put them (pinned by
/// `tests/dart_integration/test/request_pointer_identity_test.dart`).
/// `&mut [u8]` stays rejected.
#[bridge(sync)]
pub fn sum_bytes(data: &[u8]) -> i64 {
    data.iter().map(|b| *b as i64).sum()
}

/// The REQUEST FLOOR instrument. `sum_bytes` is not one: its body is an O(n)
/// widening sum, so a `sumBytes` timing is request copies *plus* real Rust
/// work, and a change that removes a copy is measured against a number that
/// contains something it cannot move.
///
/// This body is O(1) — `len()` on a slice is a field read — so the row is
/// exactly the request copies plus the crossing floor and nothing else. It is
/// the instrument every request-path change is judged by. Keep the body O(1);
/// the moment it does per-byte work it stops being a floor.
#[bridge(sync)]
pub fn request_floor(data: &[u8]) -> i64 {
    data.len() as i64
}

/// The async twin of [`request_floor`], on the pool path — the one the
/// motivating pipelined row runs on, and the only one that ever paid a
/// request-side memset.
#[bridge]
pub fn request_floor_async(data: &[u8]) -> i64 {
    data.len() as i64
}

/// Pointer identity through the bridge: returns the address the Rust body saw
/// for its borrowed argument. A Dart test that stages the request buffer
/// itself can then assert the body read *that* buffer, which is a structural
/// proof that no copy sits between the transport's block and the body — the
/// native analogue of counting wasm entries on web.
///
/// Only meaningful under a caller that knows where it put the bytes; it is a
/// fence fixture, not an API anyone should imitate.
#[bridge(sync)]
pub fn head_ptr(data: &[u8]) -> u64 {
    data.as_ptr() as u64
}

/// The `&str` half of the borrowed decode, which nothing else exercises.
///
/// `read_str_borrowed` is the twin of `read_bytes_borrowed` and it carries an
/// extra obligation the byte one does not: UTF-8 validation, with the same
/// attributable panic `read_string` raises. Emit-text assertions and the fact
/// that it compiles say nothing about either the borrow or the check actually
/// running, so this fixture exists to make both execute. It returns the byte
/// length *and* touches the string, so a decode that produced the wrong bytes
/// could not pass.
#[bridge(sync)]
pub fn request_floor_str(s: &str) -> i64 {
    (s.len() as i64) + i64::from(s.starts_with('\u{feff}'))
}

#[bridge(sync)]
pub fn rev_hash(head: [u8; 32]) -> [u8; 32] {
    let mut out = head;
    out.reverse();
    out
}

// ------------------------------------------------ fixed arrays `[T; N]` --
//
// `[u8; N]` used to be the only fixed array a signature could name. The wire
// is the same for every element type — N elements, raw, no length prefix,
// because the length is in the *type* — so `[u8; N]` keeps its memcpy codec
// and nothing about it moved.

/// One struct covering every array shape the codecs branch on: the bulk
/// numeric path, the element loop, `[u8; N]` beside them, an array nested in
/// an array, an element type that is not fixed-width, and `N = 0`.
#[bridge(data)]
pub struct Transform {
    /// A fixed-width numeric element takes the typed list `Vec<f32>` takes and
    /// the same bulk memcpy, minus the prefix.
    pub matrix: [f32; 16],
    pub ids: [i32; 4],
    /// `i64` has a typed Dart list but no bulk codec, so this is the mixed
    /// case: Rust memcpies, Dart loops, and the bytes have to agree.
    pub longs: [i64; 2],
    /// Non-numeric elements loop on both sides, in ascending index order.
    pub corners: [String; 3],
    pub bounds: [Point; 2],
    /// Unchanged, and next to the others so a regression in either shows here.
    pub digest: [u8; 8],
    pub grid: [[i32; 2]; 3],
    pub flags: [Option<i64>; 2],
    /// An empty array is a legal Rust type and writes nothing at all.
    pub none: [f64; 0],
}

#[bridge(sync)]
pub fn echo_transform(t: Transform) -> Transform {
    t
}

/// A borrowed fixed array: the glue decodes into a local it owns and lends
/// that, exactly as `&Vec<T>` does.
#[bridge(sync)]
pub fn matrix_trace(m: &[f32; 16]) -> f32 {
    m[0] + m[5] + m[10] + m[15]
}

#[bridge(sync)]
pub fn maybe_double(x: Option<f64>) -> Option<f64> {
    x.map(|v| v * 2.0)
}

#[bridge(sync)]
pub fn invert_map(m: HashMap<String, i64>) -> HashMap<i64, String> {
    m.into_iter().map(|(k, v)| (v, k)).collect()
}

/// HashSet<T> ⇄ Dart Set<T> on the list wire codec (length + elements).
#[bridge(sync)]
pub fn union_sets(a: HashSet<i64>, b: HashSet<i64>) -> HashSet<i64> {
    a.union(&b).copied().collect()
}

#[bridge(sync)]
pub fn no_args_no_ret() {}

/// Time mapping: a span ⇄ Dart `Duration` as
/// i64 microseconds. The Rust peer is whichever the signature names —
/// `chrono::Duration` and `time::Duration` are first-class beside this one —
/// and `std::time::Duration` is the peer with the *unsigned* contract, which
/// is what the negative-Duration guard in `bridge_test.dart` exercises. The
/// fixture stays on std because adding chrono to this crate would pull a
/// dependency into the Bazel crate hub for no wire coverage: every peer emits
/// the identical i64 µs, so the cross-language half is this one.
#[bridge(sync)]
pub fn add_duration(a: std::time::Duration, b: std::time::Duration) -> std::time::Duration {
    a + b
}

/// SystemTime ⇄ Dart `DateTime` (UTC), i64 micros since the Unix epoch.
#[bridge(sync)]
pub fn advance_time(t: std::time::SystemTime, by: std::time::Duration) -> std::time::SystemTime {
    t + by
}

/// A time type nested in the containers §1 claims it composes into. Neither
/// `Duration` nor `SystemTime` is on the bulk numeric fast path (they are not
/// in `bulk_codec_stem`/`bulk_rust_prim`), so these take the generic
/// per-element codec — which is what the claim rests on and what nothing had
/// crossed the boundary to check. `Option` is here for the same reason: the
/// null-flag path is a different arm again.
#[bridge(sync)]
pub fn earliest_after(
    stamps: Vec<std::time::SystemTime>,
    spans: Vec<std::time::Duration>,
    floor: Option<std::time::SystemTime>,
) -> Option<std::time::SystemTime> {
    let total: std::time::Duration = spans.iter().sum();
    stamps
        .into_iter()
        .map(|t| t + total)
        .filter(|t| floor.is_none_or(|f| *t >= f))
        .min()
}

/// Micros since the Unix epoch from `SystemTime::now()`, or -1.
///
/// This and the two below probe the wasip1 std facilities, one signature per
/// facility, existing on every target. The `cfg` is inside the body
/// deliberately: codegen never sees it, so the emitted surface is identical
/// everywhere and no capability flag is needed — only the *value* says
/// whether std could do the work. `-1`/`0` is the sentinel for "this target
/// has no such facility", which is wasm32-unknown-unknown, where std links
/// the unsupported PAL and the real call would abort the module.
///
/// The cfg has three ways to be true, and they are three different hosts:
/// native (std works outright), wasi (the preview1 shim serves it), and the
/// `wasm-std-facilities` feature (a std from toolchain/custom_std, whose stubs
/// were replaced by host imports). The feature is needed because the *target*
/// cannot tell the last case apart from the first: stock and patched std are
/// both wasm32-unknown-unknown.
///
/// Entropy is probed through `RandomState` below rather than `getrandom`,
/// which cannot be a dependency of this crate: it fails to compile for
/// wasm32-unknown-unknown at all ("the wasm*-unknown-unknown targets are not
/// supported by default"), which would take the default web fixture with it.
#[bridge(sync)]
pub fn std_clock_micros() -> i64 {
    #[cfg(any(not(target_family = "wasm"), target_os = "wasi", feature = "wasm-std-facilities"))]
    {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_micros() as i64)
            .unwrap_or(-1)
    }
    #[cfg(all(target_family = "wasm", not(target_os = "wasi"), not(feature = "wasm-std-facilities")))]
    {
        -1
    }
}

/// A non-negative elapsed-nanos reading from two `Instant::now()` calls, or
/// -1. Monotonic clocks come from the same host call as the wall clock but a
/// different clock id, so this catches a shim that wired only one of them.
#[bridge(sync)]
pub fn std_monotonic_nanos() -> i64 {
    #[cfg(any(not(target_family = "wasm"), target_os = "wasi", feature = "wasm-std-facilities"))]
    {
        let a = std::time::Instant::now();
        let b = std::time::Instant::now();
        b.duration_since(a).as_nanos() as i64
    }
    #[cfg(all(target_family = "wasm", not(target_os = "wasi"), not(feature = "wasm-std-facilities")))]
    {
        -1
    }
}

/// Writes to stdout and returns the byte count std reported writing. Proves
/// `fd_write` is wired; on a target with no stdout this is 0.
#[bridge(sync)]
pub fn std_println(msg: String) -> i64 {
    #[cfg(any(not(target_family = "wasm"), target_os = "wasi", feature = "wasm-std-facilities"))]
    {
        println!("{msg}");
        msg.len() as i64 + 1
    }
    #[cfg(all(target_family = "wasm", not(target_os = "wasi"), not(feature = "wasm-std-facilities")))]
    {
        let _ = msg;
        0
    }
}

/// A hash of a fixed key under a fresh `RandomState`, or -1.
///
/// `RandomState` is the public surface over `hashmap_random_keys`, which is
/// what actually differs between a stock wasm std and a facility one: the stub
/// derives it from *allocation addresses*, under std's own note that this
/// "isn't particularly secure, but there isn't really an alternative". In a
/// deterministic module those are close to predictable.
///
/// Within one instance this proves only that the path does not panic. The
/// assertion that matters — that the seed differs between instantiations —
/// needs two page loads and lives in the demo's Playwright spec: std caches the
/// keys per thread, and workers share the module's memory, so neither a second
/// `RandomState` nor a worker is a discriminator here.
#[bridge(sync)]
pub fn std_hash_seed() -> i64 {
    #[cfg(any(not(target_family = "wasm"), target_os = "wasi", feature = "wasm-std-facilities"))]
    {
        use std::collections::hash_map::RandomState;
        use std::hash::{BuildHasher, Hasher};
        let mut h = RandomState::new().build_hasher();
        h.write_u64(0x5EED);
        // Fold to a positive i64: the sentinel is -1, so the real path must
        // never be able to produce it.
        (h.finish() >> 1) as i64
    }
    #[cfg(all(target_family = "wasm", not(target_os = "wasi"), not(feature = "wasm-std-facilities")))]
    {
        -1
    }
}

/// `available_parallelism()`, or -1.
///
/// Reports the machine, not permission to use it: this is expected to be the
/// real core count even on a single-threaded build, where `thread::spawn`
/// still fails. See toolchain/custom_std/pal/thread.rs.
#[bridge(sync)]
pub fn std_parallelism() -> i64 {
    #[cfg(any(not(target_family = "wasm"), target_os = "wasi", feature = "wasm-std-facilities"))]
    {
        std::thread::available_parallelism()
            .map(|n| n.get() as i64)
            .unwrap_or(-1)
    }
    #[cfg(all(target_family = "wasm", not(target_os = "wasi"), not(feature = "wasm-std-facilities")))]
    {
        -1
    }
}

/// `thread::sleep` for `millis`, returning the elapsed millis it observed, or
/// -1 when the facility is absent.
///
/// **Sync, so it runs on the caller — which on web is the main thread, where
/// this is supposed to fail.** That is the point: the main thread may never
/// wait, so the host refuses and the refusal crosses back as an
/// attributable `BridgePanicException`. The Dart side asserts the throw.
///
/// [`Miner::sleep_on_executor`] is the other half, where waiting is legal.
#[bridge(sync)]
pub fn std_sleep_millis(millis: i64) -> i64 {
    #[cfg(any(not(target_family = "wasm"), target_os = "wasi", feature = "wasm-std-facilities"))]
    {
        let start = std::time::Instant::now();
        std::thread::sleep(std::time::Duration::from_millis(millis.max(0) as u64));
        start.elapsed().as_millis() as i64
    }
    #[cfg(all(target_family = "wasm", not(target_os = "wasi"), not(feature = "wasm-std-facilities")))]
    {
        let _ = millis;
        -1
    }
}

#[bridge]
pub fn sum_squares(n: i64) -> i64 {
    (1..=n).map(|i| i * i).sum()
}

// -------------------------------------------------- adversarial values --
//
// Round-trip fixtures for the edge values that had never crossed the bridge:
// float special cases, integer extremes in both
// directions, empty collections, and Dart→Rust strings that UTF-8 cannot
// represent. Each echoes its argument unchanged so the Dart side can assert
// exact bit-for-bit preservation (or discover the honest, documented
// behaviour where exact preservation is impossible — a lone surrogate has no
// UTF-8 encoding).

/// Echo an f64 verbatim so NaN, ±Inf, and -0.0 can be checked for exact
/// preservation (the codec must move the bit pattern, not a numeric value).
#[bridge(sync)]
pub fn echo_f64(x: f64) -> f64 {
    x
}

/// Echo an f32 verbatim: proves f32 precision limits survive the narrower
/// wire slot (the point is the 32-bit slot round-trips its own bits).
#[bridge(sync)]
pub fn echo_f32(x: f32) -> f32 {
    x
}

/// Echo a Vec<f64>: the edge floats inside a collection element codec.
#[bridge(sync)]
pub fn echo_f64s(xs: Vec<f64>) -> Vec<f64> {
    xs
}

/// Echo an i64 so i64::MIN / i64::MAX can round-trip (Dart→Rust→Dart).
#[bridge(sync)]
pub fn echo_i64(x: i64) -> i64 {
    x
}

/// Echo a u64 so u64::MAX round-trips as BigInt (the u64 decode direction is
/// already golden-tested; this pins the argument direction too).
#[bridge(sync)]
pub fn echo_u64(x: u64) -> u64 {
    x
}

/// Echo a u32 so u32::MAX round-trips.
#[bridge(sync)]
pub fn echo_u32(x: u32) -> u32 {
    x
}

/// Echo an i128 so the signed 128-bit extremes (i128::MIN/MAX) round-trip as
/// BigInt over the 16-byte big-integer codec.
#[bridge(sync)]
pub fn echo_i128(x: i128) -> i128 {
    x
}

/// Echo a u128 so u128::MAX round-trips as BigInt.
#[bridge(sync)]
pub fn echo_u128(x: u128) -> u128 {
    x
}

/// The 128-bit extremes minted Rust-side (Rust→Dart direction): values Dart
/// cannot author as plain ints. Returned in one struct.
#[bridge(data)]
pub struct Int128Extremes {
    pub i128_min: i128,
    pub i128_max: i128,
    pub u128_max: u128,
}

#[bridge(sync)]
pub fn int128_extremes() -> Int128Extremes {
    Int128Extremes {
        i128_min: i128::MIN,
        i128_max: i128::MAX,
        u128_max: u128::MAX,
    }
}

/// Echo a `char` so a one-character Dart String round-trips as a Unicode
/// scalar: ASCII, non-ASCII BMP, and an astral scalar like '🦀' (U+1F980).
#[bridge(sync)]
pub fn echo_char(c: char) -> char {
    c
}

/// A `char` minted Rust-side (return direction): the crab, an astral scalar
/// Dart must reconstruct as a surrogate pair.
#[bridge(sync)]
pub fn crab() -> char {
    '🦀'
}

/// The integer extremes minted Rust-side, returned in one struct: pins the
/// Rust→Dart direction for values Dart cannot author as plain ints for the
/// unsigned slots (u64::MAX).
#[bridge(data)]
pub struct IntExtremes {
    pub i64_min: i64,
    pub i64_max: i64,
    pub u32_max: u32,
    pub u64_max: u64,
}

#[bridge(sync)]
pub fn int_extremes() -> IntExtremes {
    IntExtremes {
        i64_min: i64::MIN,
        i64_max: i64::MAX,
        u32_max: u32::MAX,
        u64_max: u64::MAX,
    }
}

/// Echo a String verbatim: exercises the empty string and — the interesting
/// case — a Dart string carrying a lone UTF-16 surrogate, which has no valid
/// UTF-8 encoding. The Rust body only sees whatever the Dart encoder produced.
#[bridge(sync)]
pub fn echo_string(s: String) -> String {
    s
}

/// The UTF-8 byte length Rust actually received for a string argument: lets
/// the Dart test observe how a lone surrogate was encoded on the wire without
/// guessing.
#[bridge(sync)]
pub fn string_utf8_len(s: String) -> i64 {
    s.len() as i64
}

/// Echo a Vec<u8>: pins the empty-`Vec<u8>` argument (distinct wire path from
/// a populated one — length 0, no bytes).
#[bridge(sync)]
pub fn echo_bytes(data: Vec<u8>) -> Vec<u8> {
    data
}

/// Echo a Vec<i64>: pins the empty-`Vec<T>` argument for a non-byte element.
/// Also a typed-list case (`Int64List`): the adversarial `i64::MIN` element
/// must survive under dart2wasm, where a typed-list surprise would hide.
#[bridge(sync)]
pub fn echo_i64s(xs: Vec<i64>) -> Vec<i64> {
    xs
}

// ---------------------------------------------- typed lists (§1) --
//
// `Vec<numeric>` maps to a Dart typed list (`Int32List`, `Float64List`, …),
// symmetric in both directions. One echo per element
// type so the full matrix runs the cross-language loop and the returned Dart
// value is asserted `isA<…>()` of the exact typed class. The wire codec is
// unchanged (length + per-element primitive) — only the Dart container type.

#[bridge(sync)]
pub fn echo_i8s(xs: Vec<i8>) -> Vec<i8> {
    xs
}

#[bridge(sync)]
pub fn echo_i16s(xs: Vec<i16>) -> Vec<i16> {
    xs
}

#[bridge(sync)]
pub fn echo_i32s(xs: Vec<i32>) -> Vec<i32> {
    xs
}

#[bridge(sync)]
pub fn echo_u16s(xs: Vec<u16>) -> Vec<u16> {
    xs
}

#[bridge(sync)]
pub fn echo_u32s(xs: Vec<u32>) -> Vec<u32> {
    xs
}

/// `Float32List`: f32 narrows some f64 values, so the round-trip is asserted
/// against the f32-rounded expectation, and NaN/±Inf must survive.
#[bridge(sync)]
pub fn echo_f32s(xs: Vec<f32>) -> Vec<f32> {
    xs
}

/// `Option<Vec<i32>>` → `Int32List?`: the typed list composes through Option.
#[bridge(sync)]
pub fn echo_opt_i32s(x: Option<Vec<i32>>) -> Option<Vec<i32>> {
    x
}

/// `HashMap<String, Vec<f64>>` → `Map<String, Float64List>`: composes through
/// a map value, with adversarial floats (NaN/±Inf) in the elements.
#[bridge(sync)]
pub fn echo_map_of_f64s(m: HashMap<String, Vec<f64>>) -> HashMap<String, Vec<f64>> {
    m
}

/// `Vec<Vec<i32>>` → `List<Int32List>`: the typed list composes as the inner
/// element of a generic outer list (the outer stays `List`, each row typed).
#[bridge(sync)]
pub fn echo_i32_grid(g: Vec<Vec<i32>>) -> Vec<Vec<i32>> {
    g
}

/// A struct with a typed-list field (`Int32List xs`): proves the typed list
/// survives the struct field codec and that generated value equality
/// (`==`/`hashCode`) treats the typed list element-wise.
#[bridge(data)]
pub struct Samples {
    pub xs: Vec<i32>,
    pub weights: Vec<f64>,
}

#[bridge(sync)]
pub fn echo_samples(s: Samples) -> Samples {
    s
}

// ------------------------------------------------ nested generics --
//
// The README's "fully composable surface" claim had
// zero runtime coverage. These exercise a representative matrix of nested
// collection/option compositions, plus structs and enums carrying them. Each
// echoes so the full loop (Dart encode → Rust decode → Rust encode → Dart
// decode) is exercised — echo re-encodes from the decoded Rust value, so both
// halves of every nesting level run; `transpose` adds a semantic transform so
// a byte-passthrough could not masquerade as correct.

#[bridge(sync)]
pub fn echo_vec_opt(xs: Vec<Option<i64>>) -> Vec<Option<i64>> {
    xs
}

#[bridge(sync)]
pub fn echo_opt_vec(x: Option<Vec<i64>>) -> Option<Vec<i64>> {
    x
}

#[bridge(sync)]
pub fn echo_map_of_vec(m: HashMap<String, Vec<i64>>) -> HashMap<String, Vec<i64>> {
    m
}

#[bridge(sync)]
pub fn echo_vec_of_vec(xss: Vec<Vec<i64>>) -> Vec<Vec<i64>> {
    xss
}

#[bridge(sync)]
pub fn echo_vec_of_map(ms: Vec<HashMap<String, i64>>) -> Vec<HashMap<String, i64>> {
    ms
}

/// A `Vec` whose element is a *nested* `Option` — exercises the `FrOption`
/// wrapper composing inside a collection (element type `List<FrOption<int>?>`
/// on the Dart side): outer None → null, Some(None) → FrNone, Some(Some) →
/// FrSome, all distinct.
#[bridge(sync)]
pub fn echo_vec_nested_opt(xs: Vec<Option<Option<i64>>>) -> Vec<Option<Option<i64>>> {
    xs
}

/// A real reshape over `Vec<Vec<T>>`: transpose a rectangular grid. Guards
/// against a decode/encode that merely relays the same bytes.
#[bridge(sync)]
pub fn transpose(grid: Vec<Vec<i64>>) -> Vec<Vec<i64>> {
    if grid.is_empty() {
        return vec![];
    }
    let cols = grid[0].len();
    (0..cols)
        .map(|c| grid.iter().map(|row| row[c]).collect())
        .collect()
}

/// A struct whose fields are nested generics: proves composition survives the
/// struct field codec, not just top-level params.
#[bridge(data)]
pub struct Nested {
    pub tags: Vec<Option<String>>,
    pub groups: HashMap<String, Vec<i64>>,
    pub grid: Vec<Vec<i64>>,
}

#[bridge(sync)]
pub fn echo_nested(n: Nested) -> Nested {
    n
}

/// A data enum whose variants carry nested generics (tuple and named shapes).
#[bridge(data)]
pub enum NestedEnum {
    Rows(Vec<Vec<i64>>),
    Named { entries: HashMap<String, Vec<String>> },
}

#[bridge(sync)]
pub fn echo_nested_enum(e: NestedEnum) -> NestedEnum {
    e
}

/// `Self` in a field names the declaring type, the way rustc reads it — so
/// the idiomatic recursive spelling is the one that works. The wire
/// terminates because the list length does: a leaf carries an empty one, and
/// the generated `enc_Category`/`dec_Category` recurse by function call.
#[bridge(data)]
pub struct Category {
    pub name: String,
    pub children: Vec<Self>,
}

#[bridge(sync)]
pub fn echo_category(c: Category) -> Category {
    c
}

/// The same in an enum variant, where `Self` is the enum.
#[bridge(data)]
pub enum Expr {
    Lit(i64),
    Sum(Vec<Self>),
}

/// Evaluated Rust-side, so the Dart test proves the tree arrived whole rather
/// than only that it echoed.
#[bridge(sync)]
pub fn eval_expr(e: Expr) -> i64 {
    match e {
        Expr::Lit(n) => n,
        Expr::Sum(xs) => xs.into_iter().map(eval_expr).sum(),
    }
}

// ------------------------------------------- `Box<T>` and recursive data --
//
// `Box<T>` around a value type is transparent: the wire and the Dart surface
// are the inner type's, and only the generated Rust puts the `Box` back. It is
// what makes the recursive shapes below expressible in Rust at all — a `Vec`
// happens to give recursion a way through, and `Box` is what the shapes that
// hold *exactly one* child need.

/// A binary tree: each side is one child, not a list of them, so `Vec<Self>`
/// cannot express it.
#[bridge(data)]
pub enum Shape {
    Leaf(i64),
    Pair(Box<Self>, Box<Self>),
}

/// Summed Rust-side, so the Dart test proves the whole tree arrived rather
/// than only that it echoed.
#[bridge(sync)]
pub fn shape_sum(s: Shape) -> i64 {
    match s {
        Shape::Leaf(n) => n,
        Shape::Pair(a, b) => shape_sum(*a) + shape_sum(*b),
    }
}

/// A linked list through `Option<Box<Self>>` — the other recursive idiom, and
/// the one where the `Option` and the `Box` have to compose.
#[bridge(data)]
pub struct Link {
    pub value: i64,
    pub next: Option<Box<Self>>,
}

#[bridge(sync)]
pub fn echo_link(l: Link) -> Link {
    l
}

/// A `Box` where nothing needs one, which is what pins the transparency: Dart
/// sees `int` and `Int32List`, and the wire is `i64`'s and `Vec<i32>`'s.
///
/// `Vec<Box<i32>>` is also the one shape where the two sides take *different*
/// fast paths on purpose — Dart bulk-writes the typed list, Rust loops,
/// because a `Vec<Box<i32>>` has no contiguous run to memcpy. The bytes have
/// to agree anyway, which is what this crosses to check.
#[bridge(sync)]
pub fn rebox(x: Box<i64>, xs: Vec<Box<i32>>) -> Vec<Box<i32>> {
    xs.into_iter().map(|b| Box::new(*b + *x as i32)).collect()
}

// ------------------------------------------------------ tuples → records --
//
// Rust tuples `(A, B, …)` map to Dart 3 positional records `(TA, TB, …)`,
// accessed `$1`/`$2`. Wire is struct-shaped: each
// element encodes positionally on the existing per-element codec — no new
// primitive. These echo functions exercise both encode and decode at every
// nesting level. `()` unit return stays first-class (see `no_args_no_ret`).

/// A 2-tuple `(i32, String)` ⇄ Dart `(int, String)`.
#[bridge(sync)]
pub fn echo_pair(t: (i32, String)) -> (i32, String) {
    t
}

/// A 3-tuple exercises arity > 2 and mixed element types.
#[bridge(sync)]
pub fn echo_triple(t: (i64, bool, String)) -> (i64, bool, String) {
    t
}

/// A tuple inside a `Vec` element: `Vec<(i32, String)>` ⇄ `List<(int, String)>`.
#[bridge(sync)]
pub fn echo_pairs(ts: Vec<(i32, i32)>) -> Vec<(i32, i32)> {
    ts
}

/// A nested tuple `((i32, i32), i32)`: the record composes inside itself.
#[bridge(sync)]
pub fn echo_nested_tuple(t: ((i32, i32), i32)) -> ((i32, i32), i32) {
    t
}

/// A tuple inside `Option` and as a `HashMap` value: the record composes in
/// every data position.
#[bridge(sync)]
pub fn echo_opt_pair(x: Option<(i32, String)>) -> Option<(i32, String)> {
    x
}

#[bridge(sync)]
pub fn echo_map_of_pair(m: HashMap<String, (i32, i64)>) -> HashMap<String, (i32, i64)> {
    m
}

/// A struct with a tuple field: proves the record survives the struct field
/// codec and that generated value equality treats a record field structurally
/// (Dart records are natively value-equal; `frDeepEquals` falls through to
/// `==` for a record, which is not a `List`/`Map`).
#[bridge(data)]
pub struct Labelled {
    pub id: i64,
    pub at: (i32, i32),
}

#[bridge(sync)]
pub fn echo_labelled(v: Labelled) -> Labelled {
    v
}

// --------------------------------------- BTreeMap/BTreeSet/VecDeque → Map/Set/List --
//
// These share the Dart type and the wire codec with HashMap/HashSet/Vec
// (length + elements); only the Rust side reconstructs the concrete ordered
// container on decode. A BTreeMap iterates — and so
// encodes — in sorted key order, which Dart's insertion-ordered `Map`
// preserves, letting the Dart side assert the round trip is sorted.

#[bridge(sync)]
pub fn echo_btree_map(m: BTreeMap<String, i64>) -> BTreeMap<String, i64> {
    m
}

#[bridge(sync)]
pub fn echo_btree_set(s: BTreeSet<i64>) -> BTreeSet<i64> {
    s
}

#[bridge(sync)]
pub fn echo_deque(d: VecDeque<i64>) -> VecDeque<i64> {
    d
}

/// Build a `BTreeMap` from entries handed in *unsorted*; it comes back sorted
/// by key because a BTreeMap iterates in order and Dart's `Map` preserves
/// insertion order. Proves the ordered-container reconstruct, not a passthrough.
#[bridge(sync)]
pub fn sorted_map(pairs: Vec<(String, i64)>) -> BTreeMap<String, i64> {
    pairs.into_iter().collect()
}

// ------------------------------------------------------------ data types --

#[bridge(data)]
pub struct Point {
    pub x: f64,
    pub y: f64,
    pub label: Option<String>,
}

#[bridge(sync)]
pub fn midpoint(a: Point, b: Point) -> Point {
    Point {
        x: (a.x + b.x) / 2.0,
        y: (a.y + b.y) / 2.0,
        label: a.label.or(b.label),
    }
}

/// The Dart name a member lands under, said explicitly.
///
/// The mapping from a Rust name is not injective — `to_lower_camel_case`
/// collapses `word_count` and `wordCount`, and every compound name loses the
/// boundary between its halves — so two items can want one Dart name. That is
/// FR0002, and this is the way past it: `Point::norm` already takes
/// `pointNorm` on the generated fake, so the free function beside it says
/// where it goes instead.
#[bridge(sync, dart_identifier = "pointNormOf")]
pub fn point_norm(p: Point) -> f64 {
    p.norm()
}

/// A value type by reference. The glue decodes into a local it owns and lends
/// that local for the length of the call, so the body reads exactly what it
/// would have read by value and nothing can dangle. `&[T]` is the same borrow
/// with the slice spelling — for every element type, not only bytes.
#[bridge(sync)]
pub fn norm_of(p: &Point) -> f64 {
    (p.x * p.x + p.y * p.y).sqrt()
}

#[bridge(sync)]
pub fn sum_x(ps: &[Point]) -> f64 {
    ps.iter().map(|p| p.x).sum()
}

#[bridge(sync)]
pub fn max_of(xs: &[i64]) -> i64 {
    xs.iter().copied().max().unwrap_or(0)
}

/// Members on a **data** type: a snapshot crosses in the request, the body
/// runs against that copy, and the copy dies with the call. No model applies
/// and nothing is kept alive, so the same three members are legal in every
/// context — which is what the Dart side asserts by calling them twice and
/// getting the same answer.
#[bridge]
impl Point {
    /// `&self`, run on the caller. The receiver is decoded ahead of the
    /// parameters, exactly where a by-value parameter would sit.
    #[bridge(sync)]
    pub fn norm(&self) -> f64 {
        (self.x * self.x + self.y * self.y).sqrt()
    }

    /// `&self` async: the receiver decodes on the caller and moves to a pool
    /// thread with the rest of the request.
    #[bridge]
    pub fn scaled(&self, k: f64) -> Point {
        Point { x: self.x * k, y: self.y * k, label: self.label.clone() }
    }

    /// Receiverless: a **static** on the generated class, not a constructor.
    /// A data class already has its own `const Point({...})`.
    #[bridge(sync)]
    pub fn origin() -> Point {
        Point { x: 0.0, y: 0.0, label: None }
    }

    /// `Self` in a parameter names the impl type, exactly as it does in a
    /// return — spelled out here, on the type whose own name is the only
    /// reading available.
    #[bridge(sync)]
    pub fn dot(&self, other: &Self) -> f64 {
        self.x * other.x + self.y * other.y
    }

    /// A borrowed return from a **data** receiver: the reference points into
    /// the decoded copy, which the same scope owns.
    #[bridge(sync)]
    pub fn x_ref(&self) -> &f64 {
        &self.x
    }

    /// Dart property syntax: `p.quadrant`, not `p.quadrant()`. The header is
    /// the only thing that differs — same call, same wire, same dispatch id.
    #[bridge(sync, getter)]
    pub fn quadrant(&self) -> i64 {
        match (self.x >= 0.0, self.y >= 0.0) {
            (true, true) => 1,
            (false, true) => 2,
            (false, false) => 3,
            (true, false) => 4,
        }
    }
}

/// A by-value receiver on a **data** type, needing no annotation and holding
/// a field that could not be `Copy`.
///
/// The receiver rides the request as a value and the glue decodes its own
/// local, which is the implicit clone every by-value data parameter already
/// performs — so the Dart value the caller holds is untouched and calling
/// twice is exactly as legal as passing the same value to two calls.
#[bridge(data)]
#[derive(Clone)]
pub struct Tick {
    pub at: i64,
    pub note: String,
}

#[bridge]
impl Tick {
    #[bridge(sync)]
    pub fn consume(self) -> i64 {
        self.at
    }

    /// `self: Box<Self>` on a data type is that same local in a `Box` — the
    /// transparency a `Box` around a value type already has, visible only in
    /// the generated Rust.
    #[bridge(sync)]
    pub fn consume_boxed(self: Box<Self>) -> String {
        self.note
    }

    /// An async property, on the same type.
    #[bridge(getter)]
    pub fn doubled(&self) -> i64 {
        self.at * 2
    }
}

/// Data-class ergonomics fixture: a mix of a required
/// scalar, a nullable field, and a collection field, so the generated Dart
/// `copyWith` is exercised across all three — including the nullable trap
/// (`copyWith(owner: null)` must null the field while omitting preserves it).
/// Fields are `final`; equality is generated (default).
#[bridge(data)]
pub struct Widget {
    pub id: i64,
    pub owner: Option<String>,
    pub tags: Vec<String>,
}

#[bridge(sync)]
pub fn echo_widget(input: Widget) -> Widget {
    input
}

/// `#[bridge(no_eq)]` opts OUT of value equality:
/// two independently-built equal-valued `Ticket`s are `!=` (identity), so a
/// `Ticket` is not usable as a value `Map`/`Set` key. `copyWith` and `toString`
/// still generate (they don't depend on equality).
#[bridge(data, no_eq)]
pub struct Ticket {
    pub code: i64,
    pub note: Option<String>,
}

#[bridge(sync)]
pub fn echo_ticket(input: Ticket) -> Ticket {
    input
}

/// The other two struct shapes. A tuple struct's fields carry synthesized
/// names (`field0`, …) and land positionally in Dart, exactly as a tuple enum
/// *variant*'s already do; a unit struct is the zero-field case — a singleton
/// whose instances are all equal.
#[bridge(data)]
pub struct Meters(pub f64);

#[bridge(data)]
pub struct Span(pub i64, pub String);

#[bridge(data)]
pub struct Origin;

#[bridge(sync)]
pub fn scale_meters(m: Meters, k: f64) -> Meters {
    Meters(m.0 * k)
}

#[bridge(sync)]
pub fn relabel_span(s: Span, label: String) -> Span {
    Span(s.0, label)
}

#[bridge(sync)]
pub fn origin() -> Origin {
    Origin
}

/// Members on the other two shapes: positional fields are reached by index in
/// the receiver's decode, and a unit struct's is empty.
#[bridge]
impl Meters {
    #[bridge(sync)]
    pub fn feet(&self) -> f64 {
        self.0 * 3.280_839_895
    }
}

#[bridge]
impl Origin {
    #[bridge(sync)]
    pub fn label(&self) -> String {
        "origin".into()
    }
}

#[bridge(data)]
pub enum Color {
    Red,
    Green,
    Blue,
}

#[bridge(sync)]
pub fn next_color(c: Color) -> Color {
    match c {
        Color::Red => Color::Green,
        Color::Green => Color::Blue,
        Color::Blue => Color::Red,
    }
}

/// A unit-only enum lands as a Dart `enum`, which takes members in a body
/// after its values — so this proves the long-form spelling compiles and
/// dispatches.
#[bridge]
impl Color {
    #[bridge(sync)]
    pub fn hex(&self) -> String {
        match self {
            Color::Red => "#ff0000".into(),
            Color::Green => "#00ff00".into(),
            Color::Blue => "#0000ff".into(),
        }
    }
}

/// Explicit Rust discriminants, carried to Dart as `int get discriminant`.
///
/// The shape that made this real friction: an enum written to mirror an
/// external numbering (an HTTP status, a C ABI value, a database column). The
/// frustrate wire is still the variant's **position** — `Status.ok.index` is 0
/// — and the number the Rust declaration writes is a value the enum carries.
/// `Redirect` writes none, so it takes Rust's own rule, one more than `Ok`.
#[bridge(data)]
pub enum Status {
    Ok = 200,
    Redirect,
    NotFound = 404,
    /// Negative and non-monotonic: an `i64` is an `i64`.
    Local = -1,
}

#[bridge(sync)]
pub fn status_of(code: i64) -> Status {
    match code {
        200 => Status::Ok,
        201 => Status::Redirect,
        404 => Status::NotFound,
        _ => Status::Local,
    }
}

/// Shaped after the automerge TextPatch enum from the reference apps.
#[bridge(data)]
#[derive(Clone)]
pub enum TextPatch {
    Splice { index: usize, text: String },
    Delete { index: usize, length: usize },
    Mark(String, i64),
    Clear,
}

#[bridge(sync)]
pub fn echo_patches(ps: Vec<TextPatch>) -> Vec<TextPatch> {
    ps
}

/// A fielded enum lands as a sealed class hierarchy, and a member goes on the
/// sealed **base**: the receiver is the whole value, and the encoder switches
/// on the variant, so one declaration covers every subclass.
#[bridge]
impl TextPatch {
    #[bridge(sync)]
    pub fn span(&self) -> usize {
        match self {
            TextPatch::Splice { text, .. } => text.len(),
            TextPatch::Delete { length, .. } => *length,
            TextPatch::Mark(_, _) | TextPatch::Clear => 0,
        }
    }
}

// -------------------------------------------------------- errors / panics --

#[bridge(sync)]
pub fn parse_number(s: String) -> Result<i64> {
    if s.is_empty() {
        bail!("empty input: nothing to parse");
    }
    Ok(s.trim().parse()?)
}

#[bridge]
pub fn parse_number_async(s: String) -> Result<i64> {
    parse_number(s)
}

/// Rust's other erased error. `Box<dyn Error>` offers a caller exactly what
/// `anyhow::Error` offers — `Display`, `source()`, `downcast` — so it crosses
/// on the same untyped tier, as the error's `Display` text. (`{e:#}` is one
/// format for every tier; anyhow honours the alternate flag and walks its
/// chain, and a plain `dyn Error` does not, so a boxed error's message is its
/// outermost `Display` and nothing more.)
#[bridge(sync)]
pub fn parse_port(s: String) -> std::result::Result<i64, Box<dyn std::error::Error>> {
    Ok(s.trim().parse::<u16>()? as i64)
}

/// The same tier on a dispatched member, with the marker bounds the common
/// spelling carries.
#[bridge]
pub fn parse_port_async(
    s: String,
) -> std::result::Result<i64, Box<dyn std::error::Error + Send + Sync>> {
    Ok(s.trim().parse::<u16>()? as i64)
}

/// A typed error: the whole point is that Dart can branch on it rather than
/// read prose. Modelled on the real case from `e2e/iroh_demo` — one variant a
/// caller retries, one it must not.
#[bridge(data)]
pub enum WithdrawError {
    /// Carries the shortfall, so the payload half is exercised too and not
    /// just the discriminant.
    Insufficient { short_by: i64 },
    AccountFrozen,
}

#[bridge(sync)]
pub fn withdraw(balance: i64, amount: i64) -> Result<i64, WithdrawError> {
    if balance < 0 {
        return Err(WithdrawError::AccountFrozen);
    }
    if amount > balance {
        return Err(WithdrawError::Insufficient {
            short_by: amount - balance,
        });
    }
    Ok(balance - amount)
}

/// The same error on the async path: a typed error must survive the pool
/// round trip and the completion post, not just the synchronous return.
#[bridge]
pub fn withdraw_async(balance: i64, amount: i64) -> Result<i64, WithdrawError> {
    withdraw(balance, amount)
}

/// And from a genuine Rust `async fn` body, which completes through the
/// cooperative executor rather than the pool. That is a third dispatch shape,
/// and until this existed nothing carried a typed error through it — the
/// fourth, an actor method, is `Miner::withdraw_from`. (An `async fn` cannot be
/// an actor method: FR0029.)
///
/// Also the block-check fixture's **`async fn`** claim, and it must stay: it is
/// the only `async fn` whose claim either driver reports, and the shape whose
/// settlement is easiest to get wrong. Nothing is built for it and nothing is
/// scanned — the census says `placement-dispatch`
/// (`tools/check_block.dart`, `_requiredDispatch`).
///
/// Why that is sound, for a body that suspends. `executor::spawn` constructs
/// the future on the calling thread and hands its **first** poll to the
/// Scheduler; on threaded web a non-actor instance routes every drain to the
/// pool (`arrange_drain` in executor.rs), and the host never calls
/// `frustrate_drain` on that instance at all (runtime_web.dart, `_drainFn`). So
/// no poll of this body runs on the browser main thread. On single-threaded web
/// the drain *is* a main-thread microtask, and the module contains no wait
/// instruction for the body to execute.
///
/// Cancellation is the case worth stating, because it is the one that looks
/// like an exception and is not. `FrustrateCancelToken.cancel()` drops a
/// partially-executed future, running the `Drop` of everything the body held
/// across its suspension point — but async_task performs that drop inside
/// `runnable.run()` on the next drain of the task's final runnable, never on
/// the canceller's thread (`Executor::cancel` in executor.rs). That is the same
/// pool worker the polls run on, so a guard whose `Drop` waits is waiting where
/// waiting is legal.
///
/// This member used to carry a `static Mutex` guard held across the `.await` as
/// a *red* witness, and the assertion was about scan mechanics rather than
/// about a hazard: it proved the check root dropped its rebuilt future, in an
/// era when the root reached this body at all. Under the contract the root is
/// gone, and so is the hazard it stood for. The `.await` stays, because the
/// suspension is what makes cancellation reachable and this member is what the
/// cancellation tests use.
#[bridge(no_block)]
pub async fn withdraw_awaiting(balance: i64, amount: i64) -> Result<i64, WithdrawError> {
    YieldOnce::pending_once().await;
    withdraw(balance, amount)
}

#[bridge(sync)]
pub fn always_panics() -> i32 {
    panic!("deliberate panic for the integration test")
}

// ------------------------------------------------------------ the listener --

/// What the registered panic listener leaves behind, for
/// `panic_listener_test.dart` to read back.
///
/// An `AtomicU64` and nothing else on the counting path, deliberately. On web
/// the listener runs **inside the panic hook**, moments before the trap, and
/// `trap_attribution_test.dart` states the standing rule: a trap leaves the
/// instance's Rust state arbitrary, so nothing may be asserted after it. A
/// relaxed store to an atomic is the one read that escapes that rule rather
/// than bending it — it completes before the trap, holds no lock, and cannot
/// be half-written. The message beside it takes a `Mutex`, which is why the
/// browser arm of that test asserts only the count.
static PANICS_SEEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static LAST_PANIC: Mutex<String> = Mutex::new(String::new());

/// Register the panic listener this fixture reports through.
///
/// Idempotent by construction — `frustrate::panic::register` is last-wins — so
/// a test file that calls it in `setUpAll` and the hostile driver that reaches
/// it as just another sync member can both do so freely.
#[bridge(sync)]
pub fn watch_panics() {
    frustrate::panic::register(|report| {
        PANICS_SEEN.fetch_add(1, Ordering::Relaxed);
        // Rendered here rather than in Dart so the location's presence is
        // observable across the wire: a report with nowhere to group it is the
        // failure this listener exists to prevent.
        let mut last = LAST_PANIC.lock().unwrap_or_else(|e| e.into_inner());
        *last = match report.location() {
            Some(at) => format!("{} @ {at}", report.message()),
            None => format!("{} @ <no location>", report.message()),
        };
    });
}

/// How many panics the listener has been told about in this instance.
#[bridge(sync)]
pub fn panics_seen() -> i64 {
    PANICS_SEEN.load(Ordering::Relaxed) as i64
}

/// The most recent report, as `message @ file:line:column`.
#[bridge(sync)]
pub fn last_panic_report() -> String {
    LAST_PANIC
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Rust `async fn` that yields once (Pending, self-waking) and then panics on
/// the RESUMED poll — so the panic lands mid-poll on a later executor drain,
/// not on the initial `frustrate_call_async`. On single-threaded web that trap
/// escapes the synchronous call frame to the `frustrate_drain` microtask, which
/// must still attribute it: the returned future rejects with a
/// BridgePanicException naming this call, never hangs (executor.rs
/// `frustrate_current_drain_call`). Native/threaded web catch it in-band.
#[bridge]
pub async fn async_panics_after_yield() -> i32 {
    YieldOnce::pending_once().await;
    panic!("deliberate async panic after yield")
}

// ------------------------------------------------------ pool replenishment --

/// Panics on the async pool. On threaded web the trap kills a worker
/// thread; the runtime must replace it (pool width is an invariant).
#[bridge]
pub fn pool_panic() -> i32 {
    panic!("deliberate pool panic")
}

/// The pool's width on this platform — a declared width if the embedder
/// declared one, else this build's default (pool.rs: `available_parallelism`
/// on native, a constant 4 on threaded wasm, 1 inline on single-threaded web).
#[bridge(sync)]
pub fn pool_width() -> i64 {
    frustrate::pool::width() as i64
}

/// Declare the Rust pool's width (`pool::declare_width`), the way an app would
/// from its own bridge.
///
/// `#[bridge(sync)]` is not incidental: an async member is dispatched *through*
/// `pool::spawn_call`, so the pool is already built by the time its body runs
/// and an async version of this could only ever declare the width it already
/// has. That is the realistic way to get this wrong, which is why the fixture
/// shows the right shape.
///
/// `Result<(), String>` so a refusal reaches Dart as a `BridgeException`
/// carrying the runtime's own message.
#[bridge(sync)]
pub fn declare_pool_width(width: i64) -> std::result::Result<(), String> {
    let requested = usize::try_from(width)
        .ok()
        .and_then(std::num::NonZeroUsize::new)
        .ok_or_else(|| format!("a pool width must be at least 1, got {width}"))?;
    frustrate::pool::declare_width(requested).map_err(|e| e.to_string())
}

static RENDEZVOUS: AtomicI64 = AtomicI64::new(0);

#[bridge(sync)]
pub fn rendezvous_reset() {
    RENDEZVOUS.store(0, Ordering::SeqCst);
}

/// Width probe: true iff `width` concurrent calls all ran at once. Each
/// call arrives, then waits for the arrival count to reach `width` — on a
/// narrowed pool the excess probes deterministically time out instead.
/// Iteration-counted (Instant::now is unsupported on wasm); thread::sleep
/// is real everywhere this runs (the test is asyncIsParallel-gated).
#[bridge]
pub fn pool_rendezvous(width: i64, max_wait_ms: i64) -> bool {
    RENDEZVOUS.fetch_add(1, Ordering::SeqCst);
    for _ in 0..max_wait_ms {
        if RENDEZVOUS.load(Ordering::SeqCst) >= width {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    false
}

// ------------------------------------------------------ Confined: TextDoc --

/// A Confined mutable document: sync methods running on the caller — the
/// model that makes Flutter text-editing pipelines work without shadow
/// caches.
#[bridge(confined)]
pub struct TextDoc {
    content: String,
    /// The watch pattern: stored sinks, pushed by
    /// later mutations, pruned once cancelled.
    watchers: Vec<StreamSink<TextPatch>>,
    /// The closure flavor of the same pattern:
    /// stored Dart closures, fired with the new length on each splice.
    change_callbacks: Vec<DartCallback<usize>>,
}

#[bridge]
impl TextDoc {
    #[bridge(sync)]
    pub fn new() -> Self {
        TextDoc {
            content: String::new(),
            watchers: vec![],
            change_callbacks: vec![],
        }
    }

    /// Async constructor: ownership transfers from the pool to Dart.
    pub fn load(initial: String) -> Self {
        TextDoc {
            content: initial,
            watchers: vec![],
            change_callbacks: vec![],
        }
    }

    /// Subscribe to this document's patches. The sink outlives this call
    /// (stored); the stream ends when the document drops, or on cancel.
    pub fn watch(&mut self, sink: StreamSink<TextPatch>) {
        self.watchers.push(sink);
    }

    /// Closure flavor: fires with the new char count after each splice.
    pub fn on_change(&mut self, cb: DartCallback<usize>) {
        self.change_callbacks.push(cb);
    }

    pub fn text(&self) -> String {
        self.content.clone()
    }

    pub fn len_chars(&self) -> usize {
        self.content.chars().count()
    }

    pub fn splice(&mut self, index: usize, delete: usize, insert: String) -> Result<Vec<TextPatch>> {
        let chars: Vec<char> = self.content.chars().collect();
        if index + delete > chars.len() {
            bail!(
                "splice out of bounds: index {index} + delete {delete} > length {}",
                chars.len()
            );
        }
        let mut next: String = chars[..index].iter().collect();
        next.push_str(&insert);
        next.extend(&chars[index + delete..]);
        self.content = next;
        let mut patches = vec![];
        if delete > 0 {
            patches.push(TextPatch::Delete {
                index,
                length: delete,
            });
        }
        if !insert.is_empty() {
            patches.push(TextPatch::Splice {
                index,
                text: insert,
            });
        }
        // Push to live watchers; prune the ones whose stream is gone
        // (add returns false after cancel).
        self.watchers
            .retain(|w| patches.iter().all(|p| w.add(p.clone())));
        let len = self.content.chars().count();
        for cb in &self.change_callbacks {
            cb.call(len);
        }
        Ok(patches)
    }
}

#[bridge(sync)]
pub fn doc_starts_with(doc: &TextDoc, prefix: String) -> bool {
    doc.content.starts_with(&prefix)
}

// ------------------------------------------------------- Frozen: Snapshot --

#[bridge(frozen)]
pub struct Snapshot {
    words: Vec<String>,
}

#[bridge]
impl Snapshot {
    #[bridge(sync)]
    pub fn build(words: Vec<String>) -> Self {
        Snapshot { words }
    }

    #[bridge(sync)]
    pub fn word_count(&self) -> usize {
        self.words.len()
    }

    /// Async method on a Frozen object (runs on the pool over the shared Arc).
    pub fn join(&self, sep: String) -> String {
        self.words.join(&sep)
    }

    /// A **borrowed** return. The value is copied into the response, read
    /// through the reference inside the same scope that holds the receiver's
    /// `Arc` clone — so nothing borrowed outlives the encode. `&str` needs
    /// the deref the same way a `&i64` does; the codec sees a place
    /// expression either way.
    #[bridge(sync)]
    pub fn first_word(&self) -> &str {
        self.words.first().map(String::as_str).unwrap_or("")
    }

    #[bridge(sync)]
    pub fn all_words(&self) -> &Vec<String> {
        &self.words
    }

    /// `Self` names the impl's own type wherever the return writes it, the
    /// way rustc reads it — not only at the root. A factory that may decline
    /// is `Option<Self>`, and it lands as a **static** returning a nullable
    /// handle, never a constructor: `is_constructor` compares the return
    /// against the bare parent, and an `Option` is not it.
    #[bridge(sync)]
    pub fn maybe_build(words: Vec<String>) -> Option<Self> {
        if words.is_empty() {
            None
        } else {
            Some(Snapshot { words })
        }
    }

    /// The same substitution inside a container that mints one handle per
    /// element.
    #[bridge(sync)]
    pub fn split_words(words: Vec<String>) -> Vec<Self> {
        words
            .into_iter()
            .map(|w| Snapshot { words: vec![w] })
            .collect()
    }

    /// Rust `async fn`, portable to every platform: its body evaluates to a
    /// Future that runs on the cooperative executor (`frustrate::executor`),
    /// which multiplexes it with every other in-flight future — native/threaded
    /// web on pool worker threads, single-threaded web via microtask. It
    /// `.await`s [`YieldOnce`], which returns `Pending` on its first poll
    /// (waking itself) and `Ready` on the second — so the executor MUST poll
    /// more than once, proving it is a real poll loop. Exercised on native VM
    /// and web in bridge_test.dart, including 1000 concurrent calls multiplexed
    /// on the one thread (the decisive single-threaded-web proof).
    pub async fn async_word_count(&self) -> usize {
        YieldOnce::pending_once().await;
        self.words.len()
    }
}

/// A minimal self-driving future: `Pending` on the first poll (scheduling
/// its own next poll via the waker), `Ready` on the second. Enough to prove
/// [`frustrate::pool::block_on`] polls more than once — no external reactor
/// is involved.
struct YieldOnce {
    yielded: bool,
}

impl YieldOnce {
    fn pending_once() -> Self {
        YieldOnce { yielded: false }
    }
}

impl std::future::Future for YieldOnce {
    type Output = ();
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        if self.yielded {
            std::task::Poll::Ready(())
        } else {
            self.yielded = true;
            // Self-driving: arrange the next poll before yielding.
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    }
}

/// Async free function borrowing a Frozen handle.
#[bridge]
pub fn snapshot_total_len(snap: &Snapshot) -> usize {
    snap.words.iter().map(|w| w.len()).sum()
}

/// The block-check fixture's **nested** handed-out handle: an `Option<T>` of a
/// Frozen opaque, from a dispatched member.
///
/// Three things differ from [`LiveProbe::new`], and each is a place the drop
/// edge could have gone missing. The handle is nested rather than the whole
/// return, so the root's edge comes from the type-graph walk instead of a
/// top-level match. The model is Frozen, so the glue is `frozen_drop` over an
/// `Arc` rather than `confined_drop` over a `Box`. And the member is
/// *dispatched*, so this is a **residue** root: the body runs on the pool and
/// is settled by placement, so the root contains the drop edge and nothing
/// else — no `spawn_*` call, no body. If the drop edge were dropped from the
/// residue this member would have no root at all, and the census would say
/// `placement-dispatch` instead of `artifact-residue`, which is what
/// `tools/check_block.dart`'s fixture guard notices.
///
/// `Snapshot` has **no** `Drop` impl, deliberately: what stays covered here is
/// the linkage — `frozen_drop::<Snapshot>` → `Arc::drop_slow` →
/// `drop_in_place::<Snapshot>` — which is what carries a user's `Drop` into the
/// scan when there is one. Adding one that takes a `static Mutex` produces
///
/// ```text
/// frustrate_check_block_103_find_snapshot
///   test_api::frustrate_generated::drop_inner_Snapshot
///     frustrate::handle::frozen_drop::test_api::api::Snapshot
///       core::mem::drop::alloc::sync::Arc::test_api::api::Snapshot
///         core::ptr::drop_in_place::alloc::sync::Arc::..::Snapshot
///           alloc::sync::Arc::..::Snapshot::core::ops::drop::Drop::drop
///             alloc::sync::Arc::test_api::api::Snapshot::drop_slow
///               core::ptr::drop_in_place::test_api::api::Snapshot
///                 test_api::api::Snapshot::core::ops::drop::Drop::drop
///                   std::sync::poison::mutex::Mutex::lock
///                     std::sys::sync::mutex::futex::Mutex::lock_contended
///                       std::sys::pal::wasm::futex::futex_wait
/// ```
///
/// ...against `memory_atomic_wait32` — the refcount branch is a direct call
/// like any other, so an `Arc` model hides nothing. Measured, then reverted:
/// the red direction is demonstrated by hand, as
/// `//tests/bazel_rules/async_fixture` explains.
#[bridge(no_block)]
pub fn find_snapshot(make: bool) -> Option<Snapshot> {
    make.then(|| Snapshot {
        words: vec!["found".into()],
    })
}

/// One process-wide `OnceLock`, read by [`once_setting`] and by nothing else.
static ONCE_SETTING: std::sync::OnceLock<i64> = std::sync::OnceLock::new();

/// A claimed body that **reaches a wait**, and is green — the shape the check
/// used to refuse, and the reason it stopped.
///
/// `OnceLock::get_or_init` parks on `memory.atomic.wait32` when a second thread
/// is already initialising, so on a `+atomics` build there is a real wait
/// instruction reachable from this body. A scan of it would be red. Nothing is
/// scanned: the body is handed to the pool, so on threaded web it runs on a
/// worker where waiting is legal, and on
/// single-threaded web it runs on the caller in a module with no wait
/// instruction in it. The census says `placement-dispatch` and no artifact is
/// built for it.
///
/// This is not a contrived shape. `e2e/iroh_demo` hit it on its first fallible
/// call: every `n0_error` asks a `OnceLock` whether backtraces are enabled, so
/// parsing a pairing ticket reaches a wait, and no artifact could ever clear
/// such a body. Keeping this member claimed is what keeps that case fenced —
/// revert the settlement rule and this claim goes red by name, here, rather
/// than in someone's app.
#[bridge(no_block)]
pub fn once_setting(v: i64) -> i64 {
    *ONCE_SETTING.get_or_init(|| v)
}

// -------------------------------------------------------- Locked: Counter --

#[bridge(locked)]
pub struct Counter {
    value: i64,
}

#[bridge]
impl Counter {
    #[bridge(sync)]
    pub fn new() -> Self {
        Counter { value: 0 }
    }

    /// Async default: lock acquired on the pool thread.
    pub fn add(&mut self, delta: i64) -> i64 {
        self.value += delta;
        self.value
    }

    pub fn get(&self) -> i64 {
        self.value
    }

    /// A Rust `async fn` on a `locked` type: the guard is held across the
    /// body's suspension, which is what an async lock is for. Rejected outright
    /// before the lock became async — `executor::spawn`'s `Send` bound refused
    /// the `std` guard.
    pub async fn add_slowly(&mut self, delta: i64) -> i64 {
        YieldOnce::pending_once().await;
        self.value += delta;
        self.value
    }

    /// The narrower case the old refusal also caught, and the one that showed
    /// it was over-broad: a shared read with **no** `.await` in the body at
    /// all. Nothing here can suspend; it was refused because the *generated*
    /// block awaits the call.
    pub async fn peek(&self) -> i64 {
        self.value
    }

    /// Contract-marked sync read: contended -> ContentionException in Dart.
    #[bridge(sync, on_contention = "error")]
    pub fn try_get(&self) -> i64 {
        self.value
    }

    /// The try-lock under `#[bridge(no_block)]`, which is a claim about this
    /// member's *caller*: main-thread Dart calling it is never stalled.
    ///
    /// Accepted, where the blocking contract beside it is FR0048. The
    /// difference is not a policy about locks but what the claim is checked
    /// against — no wait instruction reachable from what the caller runs. A
    /// blocking acquisition declares one; this refuses instead, and its
    /// release reaches none. `tests/bazel_rules/locked_fixture` is where that
    /// is proven against the artifact rather than argued; this fixture is here
    /// so the surface, the settlement census and the Dart call all see the
    /// combination too.
    #[bridge(sync, on_contention = "error", no_block)]
    pub fn try_get_unstalled(&self) -> i64 {
        self.value
    }

    /// The **write** side of the same contract, and the shape a Dart property
    /// setter takes: a synchronous `&mut self` on a locked object. It takes a
    /// write guard on the calling thread, which on web is the browser main
    /// thread, and the mutation is visible to every later call.
    #[bridge(sync, on_contention = "error")]
    pub fn try_bump(&mut self, by: i64) -> i64 {
        self.value += by;
        self.value
    }

    /// Contract-marked blocking sync read (native targets only). UN-opted: the
    /// default fate — compile-time absent from the web surface (the omission
    /// is pinned in emit_dart.rs's
    /// `web_surface_omits_the_blocking_contract_and_keeps_the_try_lock`).
    #[bridge(sync, on_contention = "block")]
    pub fn blocking_get(&self) -> i64 {
        self.value
    }

    /// The same blocking read, but opted INTO the web surface with
    /// `#[bridge(web = "runtime_fail")]`: present on web so portable code
    /// referencing it compiles, and its web body throws a loud `UnsupportedError`
    /// at runtime if actually called there (native behaves exactly like
    /// `blocking_get`). Proves the informed opt-in against the default absence.
    #[bridge(sync, on_contention = "block", web = "runtime_fail")]
    pub fn blocking_read(&self) -> i64 {
        self.value
    }

    /// Test fixture: holds the **read** guard on the calling thread, through
    /// the *generated* locked glue, for a counted number of iterations.
    ///
    /// A stress rather than a proof, and the difference matters. It gives a
    /// dispatched writer a window to queue behind a main-thread reader, which
    /// is the interleaving that makes a release take the waiter list — but the
    /// body sees `&Counter` and cannot observe whether that happened, so a run
    /// where the writer never arrived is indistinguishable from one where it
    /// did. `LockProbe` below owns its lock and answers that question
    /// deterministically; this exercises the same release through the emitted
    /// glue.
    ///
    /// Iteration-counted rather than timed: there is no clock on
    /// wasm32-unknown-unknown that does not either panic or drift with the
    /// optimizer, and `black_box` is what keeps the loop from being folded
    /// away.
    #[bridge(sync, on_contention = "error")]
    pub fn hold_read_spinning(&self, iters: i64) -> i64 {
        let mut seen = 0i64;
        for _ in 0..iters {
            seen = std::hint::black_box(seen + self.value);
        }
        std::hint::black_box(seen);
        self.value
    }

    /// Test fixture: holds the write lock for `millis` so the integration
    /// test can observe contention deterministically.
    pub fn hold_write(&mut self, millis: i64) {
        std::thread::sleep(std::time::Duration::from_millis(millis as u64));
    }

    /// A holder that suspends **on Dart** while holding the write guard.
    ///
    /// `hold_write` above is the *running* holder — a pool thread asleep with
    /// the guard. This is the *suspended* one, and since the lock became async
    /// they are different states worth telling apart: the guard is live across
    /// `call_async`, so the closure Dart supplies runs while this object is
    /// locked, on a task that is parked as heap data.
    ///
    /// It makes the mid-hold state observable without racing a yield, and it is
    /// the Rust -> Dart -> Rust shape: a sync call issued from inside that
    /// closure re-enters the bridge against a lock its own call chain holds.
    /// `on_contention = "error"` is what makes that an attributable refusal
    /// rather than a deadlock, which is the whole of why the contract exists.
    pub async fn hold_write_asking(&mut self, f: DartFunction<i64, i64>) -> i64 {
        self.value += f.call_async(self.value).await;
        self.value
    }
}

// --------------------------------------- the release path, made observable --

/// A probe that owns its lock, so a test can hold a guard on the calling
/// thread and *watch a waiter arrive behind it*.
///
/// Every other locked fixture goes through the generated glue, where the body
/// sees only `&T` and cannot ask the lock anything. That is fine for the
/// refusal cases — a contended `try_read` is one compare-exchange and touches
/// no waiter list — but it makes the one path this contract's portability
/// rests on unobservable: a release that *finds a waiter queued* and therefore
/// enters the waiter list, grants the waiter and wakes it through the executor
/// and the pool. On threaded web that whole chain runs on the browser main
/// thread, and a test that cannot tell whether it ran cannot fail when it
/// stops running.
///
/// So this holds the lock itself. `frozen`, because the probe is shared and
/// immutable — the mutability is inside its own `LockedCell`, which is not
/// codegen's business and needs no contention contract from it.
#[bridge(frozen)]
pub struct LockProbe {
    cell: Arc<frustrate::handle::LockedCell<i64>>,
}

#[bridge]
impl LockProbe {
    #[bridge(sync)]
    pub fn new() -> LockProbe {
        LockProbe {
            cell: Arc::new(frustrate::handle::LockedCell::new(0)),
        }
    }

    /// Take a read guard on the **calling** thread, spin until something
    /// queues behind it, and release. Answers whether it saw one.
    ///
    /// `true` is the interesting run: the release that follows it is the one
    /// that takes the waiter list. `false` means no waiter came — which is the
    /// honest answer on single-threaded web, where nothing can run between this
    /// acquisition and its release.
    ///
    /// `unchanged_from` is the value the caller read before dispatching its
    /// writer. A different value under the guard means that writer already
    /// ran, so none is coming and this answers `false` at once. Otherwise a
    /// writer that runs in parallel has to queue here, however long it takes
    /// to be scheduled. `max_spins` ends the call where nothing can queue, and
    /// is iteration-counted for the reason `hold_read_spinning` gives.
    #[bridge(sync)]
    pub fn read_until_contended(&self, unchanged_from: i64, max_spins: i64) -> bool {
        let Ok(guard) = self.cell.try_read() else {
            return false;
        };
        if *guard != unchanged_from {
            return false;
        }
        let mut saw = false;
        for _ in 0..max_spins {
            if self.cell.has_waiter() {
                saw = true;
                break;
            }
            std::hint::black_box(&*guard);
        }
        // The release under test: with `saw`, it enters the waiter list.
        drop(guard);
        saw
    }

    /// The waiter. Dispatched, so on threaded web it runs on a pool worker and
    /// really does queue behind the reader above.
    pub async fn bump(&self) -> i64 {
        let mut g = self.cell.write().await;
        *g += 1;
        *g
    }

    #[bridge(sync)]
    pub fn value(&self) -> i64 {
        self.cell.try_read().map(|g| *g).unwrap_or(-1)
    }
}

// ----------------------------------- Data + Locked: Note, one struct, two classes --

/// One Rust struct crossing as **both** a value class and a handle class.
///
/// A note is edited in place — which wants a handle, so Dart's edits reach the
/// one Rust object — and snapshotted as a value, which wants a class with
/// fields Dart can hold, compare and keep after the object is disposed.
/// Before `#[bridge(data, locked)]` that was two Rust types with a delegating
/// method per member.
///
/// The two halves derive the same Dart name, so the handle half is renamed
/// (FR0002 reports the collision; `locked(dart_identifier = ...)` says which
/// class moves). The value half keeps `Note`.
#[bridge(data, locked(dart_identifier = "NoteHandle"))]
#[derive(Clone)]
pub struct Note {
    pub title: String,
    pub body: String,
}

/// The **value** half. Its receiver is decoded out of the request like a
/// parameter and dies with the call, so `&self` is the only receiver it takes
/// (FR0013 refuses `&mut self`) and a receiverless member is a `static`, never
/// a constructor — the class already has its own `const` one.
#[bridge]
impl Data<Note> {
    #[bridge(sync)]
    pub fn word_count(&self) -> i64 {
        self.body.split_whitespace().count() as i64
    }

    /// Receiverless on the *value* half: a `static`, and deliberately not the
    /// constructor the identical shape is on the handle half below.
    #[bridge(sync)]
    pub fn blank(title: String) -> Data<Note> {
        Note {
            title,
            body: String::new(),
        }
    }

    /// `Self` in a **parameter**, on the value half: the block wrote
    /// `Data<Note>`, so this is the value class, exactly as writing the marker
    /// out would be. By value, because a data parameter is decoded out of the
    /// request either way.
    #[bridge(sync)]
    pub fn longer_than(&self, other: Self) -> bool {
        self.body.len() > other.body.len()
    }

    /// The same keyword one level in, and borrowed.
    #[bridge(sync)]
    pub fn total_words(&self, rest: Vec<&Self>) -> i64 {
        self.body.split_whitespace().count() as i64
            + rest.iter().map(|n| n.body.split_whitespace().count() as i64).sum::<i64>()
    }
}

/// The **handle** half. Same Rust struct, reached through a handle id under
/// the Locked model: `&mut self` is the write lock, a sync read needs a
/// contention contract, and a receiverless member returning the handle is the
/// factory.
#[bridge]
impl Locked<Note> {
    #[bridge(sync)]
    pub fn open(title: String, body: String) -> Locked<Note> {
        Note { title, body }
    }

    /// The mutation the handle exists for: it lands on the one Rust object,
    /// where the value half's would land on a decoded copy and be discarded.
    pub fn append(&mut self, more: String) -> i64 {
        self.body.push(' ');
        self.body.push_str(&more);
        self.body.split_whitespace().count() as i64
    }

    /// Contract-marked sync read, which makes this member native-only — on a
    /// type whose *other* half is on every target. The web surface omits this
    /// member and keeps both classes.
    #[bridge(sync, on_contention = "error")]
    pub fn try_word_count(&self) -> i64 {
        self.body.split_whitespace().count() as i64
    }

    /// Handle → value, written by hand because the bridge will not choose
    /// between a sync-with-a-contract read and an async one on the author's
    /// behalf. Async here, the Locked default.
    pub fn snapshot(&self) -> Data<Note> {
        self.clone()
    }

    /// `Self` in a **parameter**, on the handle half: the block wrote
    /// `Locked<Note>`, so this is the handle class — `NoteHandle` in Dart, lent
    /// rather than taken because the parameter is a reference.
    pub fn same_title_as(&self, other: &Self) -> bool {
        self.title == other.title
    }

    /// And by value, which is a consume on a handle exactly as writing the
    /// type's name is: Dart passes `other.take()`, a `Consumed<NoteHandle>`.
    pub fn absorb(&mut self, other: Self) -> i64 {
        self.body.push(' ');
        self.body.push_str(&other.body);
        self.body.split_whitespace().count() as i64
    }
}

/// The value half at a use site: crosses by value, in and out, like any data.
#[bridge(sync)]
pub fn note_headline(n: Data<Note>) -> String {
    format!("{}: {}", n.title, n.body)
}

/// The handle half at a use site: borrowed here (by value it would be a
/// consume, through `take()`), and the
/// borrow needs the same contention contract a sync member does.
#[bridge(sync, on_contention = "error")]
pub fn note_title(n: &Locked<Note>) -> String {
    n.title.clone()
}

/// Value → handle, also written by hand: minting one assumes a `Clone` the
/// bridge is not entitled to assume for the author.
#[bridge(sync)]
pub fn note_reopen(n: Data<Note>) -> Locked<Note> {
    n
}

/// Brute-force CPU work on the async pool — the pool-parallelism fixture.
/// Native and threaded web run this on real worker threads;
/// single-threaded web runs it inline (no parallelism, by design).
#[bridge]
pub fn pool_nth_prime(n: i64) -> i64 {
    let mut count = 0i64;
    let mut candidate = 1i64;
    while count < n {
        candidate += 1;
        let mut is_prime = candidate >= 2;
        let mut d = 2i64;
        while d * d <= candidate {
            if candidate % d == 0 {
                is_prime = false;
                break;
            }
            d += 1;
        }
        if is_prime {
            count += 1;
        }
    }
    candidate
}

// ---------------------------------------------------------------- streams --

static STREAM_OPENS: AtomicI64 = AtomicI64::new(0);
static CANCEL_OBSERVED: AtomicBool = AtomicBool::new(false);
static STREAM_POSTED: AtomicI64 = AtomicI64::new(0);

/// How many stream producers actually ran — pins that a never-listened
/// stream issues no call.
#[bridge(sync)]
pub fn stream_opens() -> i64 {
    STREAM_OPENS.load(Ordering::SeqCst)
}

#[bridge(sync)]
pub fn cancel_observed() -> bool {
    CANCEL_OBSERVED.load(Ordering::SeqCst)
}

/// How many items the running producer has actually handed to `add` — the
/// Rust-side counterpart of what Dart received. Lets a test tell "the
/// producer kept running" from "the items are sitting in a Dart buffer",
/// which is the whole distinction backpressure is about.
#[bridge(sync)]
pub fn stream_posted() -> i64 {
    STREAM_POSTED.load(Ordering::SeqCst)
}

/// Items then drop-without-close: the sink drop ends the stream.
#[bridge]
pub fn count_to(n: i64, sink: StreamSink<i64>) {
    STREAM_OPENS.fetch_add(1, Ordering::SeqCst);
    for i in 1..=n {
        if !sink.add(i) {
            return;
        }
    }
}

/// The block-check fixture's **stream** half (`tools/check_block.dart`), and
/// the reason it exists: every other claimed member is scalar-in, scalar-out,
/// so nothing kept `StreamSink`'s code in the gate's reachable set. It was not
/// wait-free — `add` reaches `frustrate::testing::CaptureState`, the capture
/// branch a generated binding never takes but the linker keeps, and that held a
/// parking `Mutex`. No user code was involved, so *no* stream-emitting member
/// could carry the claim, in any crate.
///
/// The shape covers both halves of a captured end in one root: `add` links the
/// event log, and letting the sink fall out of scope links drop-retire's
/// terminal. `sync` is what makes any of that scannable: a caller-run member is
/// the one kind whose body a check root reaches, so a dispatched member with the
/// same signature would be settled by placement and put none of `StreamSink`'s
/// code in front of the gate.
#[bridge(sync, no_block)]
pub fn emit_two(sink: StreamSink<i64>) {
    sink.add(1);
    sink.add(2);
}

/// Items then a terminal error.
#[bridge]
pub fn fail_after(n: i64, sink: StreamSink<i64>) {
    for i in 1..=n {
        sink.add(i);
    }
    sink.error("deliberate stream failure");
}

/// Items then a producer panic. The panic is attributed to the *call*
/// (`BridgePanicException`) on every platform, after the items that crossed. On
/// native the sink then drops on the unwind and drop-retire ends the stream
/// cleanly (`onDone`); web (`panic=abort`) runs no destructor and leaves the
/// stream open by design.
#[bridge]
pub fn panic_after(n: i64, sink: StreamSink<i64>) {
    for i in 1..=n {
        sink.add(i);
    }
    panic!("deliberate stream panic");
}

/// Async producer that adds items, yields once so the panic lands mid-poll on
/// a *resumed* executor drain, then panics. The point is the drop path: when
/// the resumed poll panics, the executor's `catch_unwind` (executor.rs) unwinds
/// `run()`, which drops the future — and with it the captured sink — *while*
/// `std::thread::panicking()` is true. On native that reaches `Drop for Inner`
/// under the same condition the deleted guard used to short-circuit, so this is
/// the async analogue of `panic_after`: the stream must still end. Web
/// (`panic=abort`) runs no destructor and leaves it open, like every panic
/// there.
#[bridge]
pub async fn panic_after_async(n: i64, sink: StreamSink<i64>) {
    for i in 1..=n {
        sink.add(i);
    }
    YieldOnce::pending_once().await;
    panic!("deliberate async stream panic");
}

/// Fallible opener: the Err rejects the call and nothing else. The sink is
/// dropped on the way out, so its end event closes the Dart stream cleanly —
/// the error lives on the call, where every other member puts it.
#[bridge]
pub fn guarded_stream(ok: bool, sink: StreamSink<i64>) -> Result<()> {
    if !ok {
        bail!("stream refused: guard was false");
    }
    sink.add(1);
    Ok(())
}

/// Two sinks on one fallible member: the shape whose outcome used to depend
/// on parameter order, because the glue routed an Err into whichever
/// StreamController it found first. Both must now be treated the same.
#[bridge]
pub fn guarded_pair(a: StreamSink<i64>, b: StreamSink<i64>) -> Result<()> {
    a.add(1);
    b.add(2);
    bail!("pair refused after one item each");
}

/// One pacing pause for [`stream_until_cancelled`]: a real millisecond of wall
/// clock on every device that fixture can actually reach.
///
/// **`thread::sleep`, not a computed pause, and that is the whole point.** The
/// pause here used to be `for j in 0..200_000 { x = x.wrapping_add(j) }` with a
/// trailing `black_box(x)`. That is a triangular sum: LLVM replaces it with its
/// closed form, and a *trailing* `black_box` pins the resulting value, not the
/// loop. At `-C opt-level=3` the pause is therefore *deleted* rather than made
/// faster — orders of magnitude, not a factor — while at `-C opt-level=0` it
/// paces as written. So under `bazel test
/// -c opt` the producer ran its entire budget out before Dart could cancel it,
/// and both tests below failed on a producer that had already returned — the
/// runtime's cancel path never implicated. Sleeping is profile-independent:
/// `-c opt` and fastbuild pace identically, which is what a *timing* fixture
/// has to do. Polling `is_cancelled()` in the pause is not the fix it looks
/// like — LLVM hoists an `AtomicBool::load` out of the loop and that collapses
/// the same way.
///
/// **Blocking is legal on every device this reaches.** Both callers are
/// `asyncIsParallel`-gated: `bridge_test`'s "cancel stops a live producer"
/// returns early without it, and `isolate_death_test` is `@TestOn('vm')`. A
/// `#[bridge]` sync fn dispatches through `pool::spawn_call`, so the producer
/// is a real OS thread natively and a Worker running `Queue::run_worker` on
/// threaded wasm — which already parks on `memory.atomic.wait32` itself.
///
/// **The non-atomics wasm arm is deliberately kept total.** The cfg below is
/// std's own predicate verbatim: `sys/thread/mod.rs` selects a real sleep for
/// `all(target_family = "wasm", target_feature = "atomics")` — the
/// `memory.atomic.wait32` loop — and everything else on wasm falls to
/// `unsupported::sleep`, whose whole body is `panic!("can't sleep")`. Under
/// wasm's panic=abort that is a dead module, not a slow test. Nothing reaches
/// it today (single-threaded web is exactly what the gates exclude, and the
/// threaded fixture is built on the pinned nightly with
/// `-C target-feature=+atomics`, which does set the cfg — stable does not,
/// since `atomics` is unstable). The arm exists so a future *ungated* caller
/// degrades to a slow test instead of taking the bridge down. Its `black_box`
/// sits **inside** the loop, so the barrier runs per iteration and the sum
/// survives `-O3` (56.7 µs/item, versus 1 ns/item for the trailing form).
fn pace_producer() {
    #[cfg(any(not(target_family = "wasm"), target_feature = "atomics"))]
    std::thread::sleep(std::time::Duration::from_millis(1));

    #[cfg(all(target_family = "wasm", not(target_feature = "atomics")))]
    {
        let mut x = 0u64;
        for j in 0..200_000u64 {
            x = std::hint::black_box(x.wrapping_add(j));
        }
    }
}

/// Cooperative cancellation fixture: pushes paced items until `add`
/// observes the cancel flag. Bounded so an unobserved cancel can never hang
/// the suite; only meaningful where the producer runs on a real worker
/// thread (asyncIsParallel).
///
/// The budget bounds the *failure* path, and with a millisecond cadence that
/// bound is 20 s: an unobserved cancel is a failed expectation at the test's
/// own 5 s poll followed by a producer that runs itself out, never an
/// unbounded wait. On the passing path the producer returns at the first
/// refused `add`, within one pause of the cancel.
#[bridge]
pub fn stream_until_cancelled(sink: StreamSink<i64>) {
    // Reset at entry so a test reads this run's counters, not a previous
    // test's leftovers.
    CANCEL_OBSERVED.store(false, Ordering::SeqCst);
    STREAM_POSTED.store(0, Ordering::SeqCst);
    for i in 0..20_000i64 {
        if !sink.add(i) {
            CANCEL_OBSERVED.store(true, Ordering::SeqCst);
            return;
        }
        STREAM_POSTED.fetch_add(1, Ordering::SeqCst);
        pace_producer();
    }
}

/// Sinks parked in a process-global slot so an isolate *other* than the one
/// that opened them can push (the "posts racing isolate death" window;
/// runtime/rust/src/post.rs).
///
/// This is what makes the isolate-death pin deterministic rather than a race.
/// The natural shape — stream into an isolate and kill it — has to catch a live
/// producer mid-flight, so it only *probably* posts into a dead consumer.
/// Parking the sink separates the two halves: the owning isolate is provably
/// gone (its exit listener fired) before a single post is attempted, and the
/// pushing isolate chooses the moment.
static PARKED_SINKS: std::sync::Mutex<Vec<StreamSink<i64>>> =
    std::sync::Mutex::new(Vec::new());

/// Hand a sink to the global slot and return; the stream outlives this call and
/// its isolate.
#[bridge(sync)]
pub fn park_sink(sink: StreamSink<i64>) {
    PARKED_SINKS.lock().unwrap().push(sink);
}

/// Push `value` to every parked sink and return how many accepted it.
///
/// A sink whose consumer is gone — cancelled, or owned by an isolate that has
/// exited — refuses, and refusing sinks are pruned, exactly as the watch pattern
/// prunes.
#[bridge(sync)]
pub fn push_parked(value: i64) -> usize {
    let mut sinks = PARKED_SINKS.lock().unwrap();
    let mut accepted = 0usize;
    sinks.retain(|s| {
        if s.add(value) {
            accepted += 1;
            true
        } else {
            false
        }
    });
    accepted
}

/// How many sinks the global slot still holds — the pruning observable.
#[bridge(sync)]
pub fn parked_sink_count() -> usize {
    PARKED_SINKS.lock().unwrap().len()
}

// ------------------------------------------- parked returning callbacks --
//
// The `park_sink` idea applied to `DartFunction`, for the blocking-`call`
// liveness pin (runtime/rust/src/post.rs `probe`).
//
// Same separation of halves, and the same reason: the defect is a *hang*, so
// the test must prove a worker is parked before anything is killed. One
// isolate registers the closure and parks it here; another isolate chooses the
// moment to invoke it, and the closure itself reports (over a `SendPort`) that
// it is running — at which point the invocation is provably registered and
// unanswered, and the parent can kill its owner with no race.
//
// A per-call SLOT INDEX rather than a single global slot, deliberately: with
// one slot the liveness check would reuse the dead isolate's function, whose
// isolate is tombstoned, and `deliver` would refuse it before the probe ever
// ran — the negative check would pass for the wrong reason.
static PARKED_FNS: std::sync::Mutex<Vec<DartFunction<i64, i64>>> =
    std::sync::Mutex::new(Vec::new());

/// Where `call_parked` is: 0 idle, 1 inside the blocking `call`, 2 returned.
/// Read through the *sync* `parked_call_state`, which answers on the Dart
/// application thread and so still works while a pool worker is parked — that
/// is what lets a timed-out expectation say which of the two failures it hit.
static CALL_STATE: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

/// Park a Dart closure where another isolate can invoke it; returns its slot.
///
/// `#[bridge]` and not `#[bridge(sync)]`: FR0020 rejects a `DartFunction`
/// parameter on a sync member (a sync body runs on the application thread,
/// which is the thread the closure needs). Native-only by derivation, like
/// every non-`async fn` member that can reach a returning callback.
#[bridge]
pub fn park_function(f: DartFunction<i64, i64>) -> i64 {
    let mut fns = PARKED_FNS.lock().unwrap();
    fns.push(f);
    (fns.len() - 1) as i64
}

/// Invoke the parked closure in `slot` and return what it answered — from a
/// pool worker, through the BLOCKING `call`.
///
/// The clone is taken and the mutex **released** before invoking: `call` parks
/// this worker for as long as the Dart closure runs, and holding the registry
/// lock across that would wedge every other fixture here rather than just this
/// call.
#[bridge]
pub fn call_parked(slot: i64, value: i64) -> i64 {
    let f = PARKED_FNS.lock().unwrap()[slot as usize].clone();
    CALL_STATE.store(1, std::sync::atomic::Ordering::SeqCst);
    let out = f.call(value);
    CALL_STATE.store(2, std::sync::atomic::Ordering::SeqCst);
    out
}

/// Where `call_parked` got to — see [`CALL_STATE`].
#[bridge(sync)]
pub fn parked_call_state() -> i64 {
    CALL_STATE.load(std::sync::atomic::Ordering::SeqCst)
}

/// Where `call_parked_async` is: 0 idle, 1 suspended inside the awaited call,
/// 2 returned.
///
/// Its OWN atomic, deliberately not [`CALL_STATE`]: that one's 0/1/2 are
/// written for the blocking path, and sharing it would make one suite's
/// assertion depend on the other suite's history (both run in one process
/// under the local `dart test` flow, where every file shares this dylib).
static ASYNC_CALL_STATE: AtomicI64 = AtomicI64::new(0);

/// Invoke the parked closure in `slot` through the AWAITED `call_async`, from a
/// bridged `async fn` on the cooperative executor.
///
/// The `call_parked` twin, and the only member in the tree that can reach the
/// awaited-invocation death case: the two existing `call_async` fixtures
/// (`transform`, `Transforms::apply_transform`) take their `DartFunction` as a
/// parameter, so its owner is the calling isolate and cannot die independently.
/// Parking separates the halves — one isolate owns the closure, another awaits
/// it — which is what lets a test kill the owner while the invocation is
/// provably delivered, accepted, and unanswered.
///
/// Same lock discipline as `call_parked`, for a sharper reason: a `MutexGuard`
/// held across an `.await` would be captured by the generated future, wedging
/// every other fixture here for as long as this task is suspended (which, for
/// the state §1 describes, is the rest of the session). The clone is taken and
/// the guard dropped at the end of the statement, before the suspension point.
///
/// NOT `requires_native` — an `async fn` is portable by derivation
/// (codegen/src/check.rs), so this compiles onto the web surface as well. It is
/// simply never called there: `park_function` takes a `DartFunction` on a
/// non-`async fn` and so is native-only, leaving no way to fill a slot on web.
#[bridge]
pub async fn call_parked_async(slot: i64, value: i64) -> i64 {
    let f = PARKED_FNS
        .lock()
        .unwrap()
        .get(slot as usize)
        .unwrap_or_else(|| panic!("call_parked_async: no parked function in slot {slot}"))
        .clone();
    ASYNC_CALL_STATE.store(1, Ordering::SeqCst);
    let out = f.call_async(value).await;
    ASYNC_CALL_STATE.store(2, Ordering::SeqCst);
    out
}

/// Where `call_parked_async` got to — see [`ASYNC_CALL_STATE`].
///
/// `#[bridge(sync)]` for the same reason as `parked_call_state`: it answers on
/// the Dart application thread, so a timed-out expectation can still say which
/// of the two failures it hit.
#[bridge(sync)]
pub fn parked_async_call_state() -> i64 {
    ASYNC_CALL_STATE.load(Ordering::SeqCst)
}

/// Drop every parked function, retiring each one's Dart registration; returns
/// how many were dropped.
///
/// Not housekeeping. An open callback registration pins its isolate, so a
/// test that parks a closure owned by the
/// *test* isolate and returns leaves a suite that passes and then never exits —
/// which under Bazel is an unexplained target TIMEOUT with no output at all,
/// since the runner only flushes when the process ends. Dropping the last Rust
/// clone posts the end event that retires the Dart side and releases the pin.
#[bridge(sync)]
pub fn clear_parked_functions() -> usize {
    let mut fns = PARKED_FNS.lock().unwrap();
    let n = fns.len();
    fns.clear();
    n
}

// (`pool_width` already exists above — the Rust-side width, which is what a
// pool-narrowing check must read: `Platform.numberOfProcessors` is
// `available_parallelism()`'s cgroup-unaware cousin and disagrees under Bazel.)

/// Backpressure fixture: the async sibling of
/// `stream_until_cancelled`. `send().await` parks while the Dart consumer is
/// paused, so STREAM_POSTED stops advancing until resume — the observable that
/// distinguishes "producer throttled" from "items buffered in Dart". Only a
/// *send*-based (async) producer can be backpressured; a `sink.add` loop is
/// unbounded by design, which is why the sync `stream_until_cancelled` cannot
/// pin this. Bounded and paced like its sibling.
#[bridge]
pub async fn stream_with_backpressure(sink: StreamSink<i64>) {
    CANCEL_OBSERVED.store(false, Ordering::SeqCst);
    STREAM_POSTED.store(0, Ordering::SeqCst);
    for i in 0..20_000i64 {
        // Parks here while paused; returns false once cancelled/closed.
        if !sink.send(i).await {
            CANCEL_OBSERVED.store(true, Ordering::SeqCst);
            return;
        }
        STREAM_POSTED.fetch_add(1, Ordering::SeqCst);
        // A computed pause, NOT `pace_producer`'s `thread::sleep`, and the
        // reason is this fixture's device rather than its gating: it is a
        // bridged `async fn`, so it runs on the cooperative executor
        // (frustrate::executor), where a blocking sleep holds a worker instead
        // of yielding it and would stall every other multiplexed call.
        //
        // `black_box` INSIDE the loop is load-bearing. Trailing, it pins the
        // value and LLVM closed-forms the triangular sum away — 1 ns/item at
        // `-C opt-level=3` against 1.01 ms at `-O0`. Per-iteration, the barrier
        // has to run each time: 56.7 µs/item at `-O3`, 1.06 ms at `-O0`
        // (rustc 1.91.1). Compressed across profiles, but never collapsed.
        let mut x = 0u64;
        for j in 0..200_000u64 {
            x = std::hint::black_box(x.wrapping_add(j));
        }
    }
}

static TICKER_POSTED: AtomicI64 = AtomicI64::new(0);
static TICKER_REFUSED: AtomicBool = AtomicBool::new(false);

/// Isolate keep-alive fixture: a detached Rust thread posts `count` items long
/// after the opening call returned — the stored-sink watch pattern. The open
/// stream keeps the isolate that opened it alive to consume them; when the thread finishes and
/// drops the sink, drop-retire closes the stream and the isolate exits.
/// Bounded (not infinite) so the driving test can observe that clean exit
/// within its timeout.
///
/// Native-shaped by construction (a detached thread), so the wasm arm is a
/// no-op; the test that drives it is VM-only.
#[bridge]
pub fn spawn_ticker(count: i64, sink: StreamSink<i64>) {
    TICKER_POSTED.store(0, Ordering::SeqCst);
    TICKER_REFUSED.store(false, Ordering::SeqCst);
    #[cfg(not(target_family = "wasm"))]
    {
        let _ = std::thread::spawn(move || {
            for i in 0..count {
                if !sink.add(i) {
                    TICKER_REFUSED.store(true, Ordering::SeqCst);
                    return;
                }
                TICKER_POSTED.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        });
    }
    #[cfg(target_family = "wasm")]
    {
        let _ = count;
        drop(sink);
    }
}

/// Whether the ticker's sink ever refused a post — the distinguishable
/// "closed" signal a producer needs to stop working when its consumer is
/// gone. False while it is still posting happily.
#[bridge(sync)]
pub fn ticker_refused() -> bool {
    TICKER_REFUSED.load(Ordering::SeqCst)
}

/// How many posts the ticker believes succeeded.
#[bridge(sync)]
pub fn ticker_posted() -> i64 {
    TICKER_POSTED.load(Ordering::SeqCst)
}

// --------------------------------------------- external types (bytes codec) --

/// Stands in for a prost-generated protobuf message: crosses the bridge as
/// bytes via user codecs on both sides. Wire
/// form here: 8-byte LE revision, then the UTF-8 title — mirrored by
/// FakePlan in tests/dart_integration/lib/fake_plan.dart.
#[bridge(bytes(dart = "FakePlan", import = "package:frustrate_integration/fake_plan.dart"))]
pub struct FakePlanMsg {
    pub title: String,
    pub revision: i64,
}

impl frustrate::BytesCodec for FakePlanMsg {
    fn to_bytes(&self) -> Vec<u8> {
        let mut out = self.revision.to_le_bytes().to_vec();
        out.extend_from_slice(self.title.as_bytes());
        out
    }

    fn from_bytes(bytes: &[u8]) -> Self {
        let revision = i64::from_le_bytes(bytes[..8].try_into().expect("FakePlan: short buffer"));
        let title = String::from_utf8(bytes[8..].to_vec()).expect("FakePlan: invalid utf8");
        FakePlanMsg { title, revision }
    }
}

#[bridge(sync)]
pub fn bump_plan(p: FakePlanMsg) -> FakePlanMsg {
    FakePlanMsg {
        title: p.title,
        revision: p.revision + 1,
    }
}

/// Externs in collections, through the async pool.
///
/// Also the block-check fixture's **decode residue**, and the only one. A
/// dispatched member's body is settled by placement, but its parameters are
/// not: `spawn_N_*`'s prelude decodes them on the calling thread, before
/// anything reaches the pool, and for a bridge-external type that decode is a
/// call into `FakePlanMsg::from_bytes` — arbitrary user Rust on the browser
/// main thread. So this member's root is a residue root carrying exactly that
/// edge, reached through the `Vec` by the parameter type-graph walk.
///
/// The red direction, demonstrated by hand and reverted: take a `static Mutex`
/// inside `from_bytes` and the scan reports
///
/// ```text
/// frustrate_check_block_N_merge_plans
///   test_api::api::FakePlanMsg::from_bytes
///     std::sync::poison::mutex::Mutex::lock
///       std::sys::sync::mutex::futex::Mutex::lock_contended
///         std::sys::pal::wasm::futex::futex_wait
/// ```
///
/// ...against `memory_atomic_wait32`. Putting the same `Mutex` in the *body*
/// changes nothing, which is the other half of what this fixture asserts.
#[bridge(no_block)]
pub fn merge_plans(plans: Vec<FakePlanMsg>) -> FakePlanMsg {
    FakePlanMsg {
        title: plans
            .iter()
            .map(|p| p.title.as_str())
            .collect::<Vec<_>>()
            .join("+"),
        revision: plans.iter().map(|p| p.revision).max().unwrap_or(0) + 1,
    }
}

/// Externs nested inside ordinary data types.
#[bridge(data)]
pub struct Meeting {
    pub room: String,
    pub plan: FakePlanMsg,
}

#[bridge(sync)]
pub fn schedule(m: Meeting) -> String {
    format!("{} rev{} in {}", m.plan.title, m.plan.revision, m.room)
}

// -------------------------------------------------------------- callbacks --

/// Fire-and-forget callback from the pool: n invocations, then the handle
/// drops (retiring the Dart-side registration).
#[bridge]
pub fn notify_n(n: i64, cb: DartCallback<i64>) {
    for i in 1..=n {
        cb.call(i);
    }
}

/// Zero-arg callback.
#[bridge]
pub fn ping(times: i64, done: DartCallback<()>) {
    for _ in 0..times {
        done.call(());
    }
}

/// Sync member firing a callback: pins that the closure runs after the
/// call returns (never during it), identically on every platform.
#[bridge(sync)]
pub fn tally(items: Vec<i64>, cb: DartCallback<i64>) -> i64 {
    let mut sum = 0;
    for i in &items {
        cb.call(*i);
        sum += *i;
    }
    sum
}

/// Value-returning callback (native-only by derivation): a plain pool member
/// cannot `.await`, so it uses the blocking `call` — the pool worker blocks
/// per item while the Dart closure maps it. Opted INTO the web surface with
/// `#[bridge(web = "runtime_fail")]` — the async/DartFunction shape of the
/// runtime-throwing stub (`Miner::refine` below stays un-opted, so an actor's
/// DartFunction member still proves the default absence on web).
#[bridge(web = "runtime_fail")]
pub fn transform_sum(values: Vec<i64>, f: DartFunction<i64, i64>) -> i64 {
    values.into_iter().map(|v| f.call(v)).sum()
}

/// Value-returning callback via the PORTABLE awaited path: an `async fn` that
/// awaits `call_async` on the cooperative executor, so it runs on native VM,
/// single-threaded web, AND threaded web — the web-portable counterpart to
/// `transform_sum`'s blocking `call`. A closure that throws surfaces as this
/// call's `BridgePanicException`, attributably.
#[bridge]
pub async fn transform(x: i64, f: DartFunction<i64, i64>) -> i64 {
    f.call_async(x).await
}

/// Background work with **no call behind it**: `runtime::spawn` hands the
/// executor a task the returning call does not own, does not join, and cannot
/// cancel (see `frustrate::runtime::spawn`, "What you give up by detaching").
///
/// Shaped to make the platform claim rather than restate the unit tests
/// (executor.rs). The call answers `x` immediately; everything observable
/// happens afterwards, in a task that:
///
///   1. **suspends and is resumed by an external wake** — `call_async` posts to
///      Dart and returns `Pending`, so the resume comes from
///      `frustrate_callback_respond` on a later drain, not from a self-wake
///      inside the drain that started it. On single-threaded web the call has
///      already returned to Dart by then;
///   2. **outlives its spawning call**, and keeps the stream open while it does
///      — the detached-producer refcount (streams.md);
///   3. **is driven by whatever this platform's Scheduler is** — pool workers
///      on native and threaded web, the microtask drain on single-threaded web.
///
/// Three round trips rather than one so the resume is a loop, not a single
/// hand-off; the stream closes when the task ends and the sink drops.
#[bridge]
pub async fn spawn_detached_echo(x: i64, f: DartFunction<i64, i64>, sink: StreamSink<i64>) -> i64 {
    frustrate::runtime::spawn(async move {
        let mut v = x;
        for _ in 0..3 {
            v = f.call_async(v).await;
            if !sink.add(v) {
                return; // the consumer went away; stop asking Dart
            }
        }
    });
    x
}

// ------------------------------------------------------------- the timer --

/// The timer install an app writes: a fire-and-forget request channel out, and
/// `fire_timer` back in. Two one-way trips and not a `DartFunction`, because
/// the Dart closure behind one answers synchronously — it can compute, it
/// cannot wait (docs/design/callbacks.md).
#[bridge(sync)]
pub fn install_timer(requests: DartCallback<(i64, i64)>) {
    frustrate::runtime::install_dart_timer(requests);
}

/// The host's half of the round trip.
#[bridge(sync)]
pub fn fire_timer(id: i64) {
    frustrate::runtime::fire_timer(id);
}

/// Drop the timer registration, and with it the request channel.
///
/// Not ceremony: the installed callback holds a `StreamSink`, and an open
/// stream pins the isolate (docs/design/streams.md). An app wants that for its
/// whole life; a test suite that never released it would leave a process that
/// runs every test, passes, and does not exit.
#[bridge(sync)]
pub fn uninstall_timer() {
    frustrate::runtime::uninstall_timer();
}

/// Sleep, then answer `x`. The elapsed time is Dart's to measure: there is no
/// clock in a stock wasm32 module, which is the same absence `install_timer`
/// exists for.
#[bridge]
pub async fn sleep_then(ms: i64, x: i64) -> i64 {
    frustrate::runtime::sleep(std::time::Duration::from_millis(ms as u64)).await;
    x
}

/// The shape the timer is actually for: a **detached** retry loop that sleeps
/// between attempts, answering nobody, outliving the call that started it. What
/// `runtime::spawn` and `runtime::sleep` are for, together and in one member.
#[bridge(sync)]
pub fn spawn_backoff(attempts: i64, ms: i64, sink: StreamSink<i64>) {
    frustrate::runtime::spawn(async move {
        let mut delay = std::time::Duration::from_millis(ms as u64);
        for attempt in 1..=attempts {
            frustrate::runtime::sleep(delay).await;
            if !sink.add(attempt) {
                return;
            }
            delay *= 2;
        }
    });
}

// ------------------------------------------------------ host entropy (web) --

/// `frustrate::random::fill_bytes` — the host CSPRNG, reachable from Rust on
/// wasm without a custom std, wasm-bindgen, or a hand-declared import.
///
/// One member on every platform rather than a `#[cfg]`-split pair, because the
/// wire interface (and so the schema hash) must not differ between the native
/// and web builds of the same bridge. Off wasm the buffer stays zeroed and
/// there is nothing to assert, which is why its test is `@TestOn('browser')`:
/// the module does not exist off wasm, deliberately (see its doc — native has
/// `getrandom`, and frustrate has nothing better to offer there).
#[bridge(sync)]
pub fn host_random_bytes(n: i64) -> Vec<u8> {
    // `mut` is used on wasm only, which is the whole shape of this member.
    #[allow(unused_mut)]
    let mut buf = vec![0u8; n as usize];
    #[cfg(target_family = "wasm")]
    frustrate::random::fill_bytes(&mut buf);
    buf
}

// ------------------------------- a Dart closure whose failure is a value --
//
// The mirror of a bridged `Result<T, E>` return: there, Rust returns `Err(E)`
// and Dart catches `EException`; here Dart throws `EException` and Rust
// receives `Err(E)`. The
// value form is Rust's, the exception form is Dart's, in both directions.

/// A failure the **Dart** side declares.
///
/// Deliberately used in this direction ONLY — no member returns it — so every
/// seam the mirror needs is proven by its use rather than by a sibling's: the
/// generated `RefusalErrorException` class, the Dart *encoder* for the enum
/// (which the Rust→Dart direction never needs), and the Rust *decoder*. Reusing
/// `WithdrawError` would have masked all three.
#[bridge(data)]
pub enum RefusalError {
    /// Carries a payload, so the value half is exercised and not just the tag.
    Busy { retry_in_ms: i64 },
    NotAllowed,
}

/// The portable fallible closure: an `async fn` awaiting `call_async`, whose
/// body **observably handles** the refusal. Each outcome maps to a distinct
/// number, so a Dart test can tell "the Err arrived as a value" apart from
/// "the value came back" and from "the call panicked".
#[bridge]
pub async fn ask_dart(x: i64, f: DartFunction<i64, Result<i64, RefusalError>>) -> i64 {
    match f.call_async(x).await {
        Ok(v) => v * 10,
        Err(RefusalError::Busy { retry_in_ms }) => -retry_in_ms,
        Err(RefusalError::NotAllowed) => -1,
    }
}

/// The pattern the gap made unwritable: **retry**. Rust asks Dart, and on a
/// `Busy` refusal asks again — which needs the failure to be a value the loop
/// can inspect, not the enclosing call's panic.
#[bridge]
pub async fn ask_dart_with_retry(
    attempts: i64,
    f: DartFunction<i64, Result<i64, RefusalError>>,
) -> String {
    let mut tried = 0;
    loop {
        tried += 1;
        match f.call_async(tried).await {
            Ok(v) => return format!("ok after {tried}: {v}"),
            Err(RefusalError::Busy { retry_in_ms }) => {
                if tried >= attempts {
                    return format!("gave up after {tried} (busy {retry_in_ms}ms)");
                }
            }
            Err(RefusalError::NotAllowed) => return format!("refused after {tried}"),
        }
    }
}

/// `Result<(), E>` — "do this; you may refuse". No value comes back, but the
/// refusal does, which is exactly why *fallibility* and not `ret` decides
/// whether a mirror has a reply frame.
#[bridge]
pub async fn tell_dart(x: i64, f: DartFunction<i64, Result<(), RefusalError>>) -> String {
    match f.call_async(x).await {
        Ok(()) => "accepted".to_string(),
        Err(RefusalError::Busy { retry_in_ms }) => format!("busy {retry_in_ms}"),
        Err(RefusalError::NotAllowed) => "not allowed".to_string(),
    }
}

/// The blocking twin, on a plain pool member: `call` reads a declared failure
/// exactly as `call_async` does — one decode, shared. Native-only by
/// derivation for the same reason as `transform_sum` (a non-`async fn` cannot
/// await, so it must park a worker).
#[bridge]
pub fn ask_dart_blocking(values: Vec<i64>, f: DartFunction<i64, Result<i64, RefusalError>>) -> i64 {
    values
        .into_iter()
        .map(|v| match f.call(v) {
            Ok(n) => n,
            Err(RefusalError::Busy { retry_in_ms }) => -retry_in_ms,
            Err(RefusalError::NotAllowed) => -1,
        })
        .sum()
}

// ------------------------------------------ bring your own async runtime --
//
// The worked example: a plain (non-`async fn`) #[bridge] pub fn whose body
// drives a REAL
// async body — a genuine `.await` that suspends and resumes — to completion on
// an EXTERNAL runtime the user owns. Here that runtime is `pollster`, a tiny,
// dependency-free `block_on` executor standing in for tokio/async-std (the user
// owns the crate dependency and the `block_on` call). It runs on a frustrate
// pool worker, and `pollster::block_on` parks THAT worker while it polls the
// future to completion — one parked pool thread per in-flight call, the plain-fn
// pool model — then returns a value, so Dart sees a resolved `Future`.
//
// Declared `native_only`, and that declaration is load-bearing rather than
// documentary. `block_on` parks, and parking is fatal on both web builds for
// two *different* reasons — which is why neither can be left to a convention:
//
//   - threaded web, main thread: `park` bottoms out in `memory.atomic.wait32`,
//     which the spec bars off the main thread. It traps. Loud.
//   - single-threaded web: there is no futex, so `park` is the `unsupported`
//     parker — literally `{}` (std's sys/sync/thread_parking/unsupported.rs).
//     It returns immediately, `block_on` polls again, and the browser's only
//     thread spins at 100% CPU forever. NOT loud: no error, no log, no
//     recovery, and no runtime seam that could notice (a watchdog needs an
//     event-loop turn, and the event loop is what is wedged).
//
// Before the declaration this fn was emitted into the web Dart surface and was
// callable there; only a comment on external_runtime_test.dart kept it off that
// path. `native_only` removes it from the web surface, so the mistake is a Dart
// compile error instead of a frozen tab, and `tools/check_no_park.dart` gates
// the Rust half — the pollster symbol must leave the single-threaded module.
// (pollster itself is wasm-safe; being *compilable* for wasm was never the same
// question as being *runnable* on it. The real runtimes this pattern targets,
// tokio and async-std, do not compile to wasm at all.)

/// Sum two numbers, but do it by `.await`ing inside a plain pool fn on an
/// external runtime (`pollster`) the caller brought. The awaited future
/// ([`YieldOnce`]) returns `Pending` on its first poll and `Ready` on the
/// second, so pollster MUST run a real poll loop across a genuine suspension —
/// the `.await` truly yields and resumes, it is not sugar over a sync call.
#[bridge(native_only)]
pub fn sum_on_external_runtime(a: i64, b: i64) -> i64 {
    pollster::block_on(async {
        // A real suspension point on the user's own runtime.
        YieldOnce::pending_once().await;
        a + b
    })
}

/// Live native actor-host count (0 on web, where the executor is a Worker
/// owned by the Dart side). Executor-leak pin: spawn/dispose — including a
/// *failed* spawn — must return this to its baseline.
#[bridge(sync)]
pub fn actor_host_count() -> i64 {
    frustrate::actor_host_count() as i64
}

// --------------------------------------------------------- Actor: Miner --

/// Worker-placed compute: the load-bearing web parallelism story. One
/// instance owns one executor (a dedicated thread on native, a
/// worker-hosted wasm instance on web); methods are async-only and run
/// there one at a time, in arrival order.
/// The gate a deferred wait parks on: opened once by [`Miner::release`],
/// waking every parked waiter with the released value.
///
/// Hand-rolled (a mutex and a waker list) on purpose: a `Deferred` body runs
/// on frustrate's cooperative executor, which provides no async runtime unless
/// the app registered one (`frustrate::runtime::register`, native-only) — so
/// the fixture models exactly what the docs tell a *portable* app to do:
/// capture an explicitly shared value in the prefix, await a plain future.
/// This fixture runs on web too, so it stays in the portable form.
#[derive(Default)]
struct Gate {
    state: Mutex<(Option<i64>, Vec<std::task::Waker>)>,
}

impl Gate {
    fn open(&self, value: i64) {
        let wakers = {
            let mut s = self.state.lock().unwrap();
            s.0 = Some(value);
            std::mem::take(&mut s.1)
        };
        for w in wakers {
            w.wake();
        }
    }

    fn wait(self: &Arc<Self>) -> GateWait {
        GateWait(self.clone())
    }
}

/// Resolves to the gate's value once it opens. Parks (registering its waker)
/// until then — the deferred analogue of the dial the iroh app detaches.
struct GateWait(Arc<Gate>);

impl std::future::Future for GateWait {
    type Output = i64;
    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<i64> {
        let mut s = self.0.state.lock().unwrap();
        match s.0 {
            Some(v) => std::task::Poll::Ready(v),
            None => {
                s.1.push(cx.waker().clone());
                std::task::Poll::Pending
            }
        }
    }
}

#[bridge(actor)]
pub struct Miner {
    label: String,
    calls: i64,
    /// The watch pattern on an *actor*: a sink the
    /// object keeps past the call that opened it. Exists so a reaped actor's
    /// channel terminal is observable — a `Miner` collected without
    /// `dispose()` must end this as LEAKED, naming the type, rather than
    /// closing it the way an orderly shutdown does.
    watchers: Vec<StreamSink<i64>>,
    /// What [`Miner::deferred_wait`] parks on and [`Miner::release`] opens.
    /// `Arc` because the deferred future cannot borrow `self` — the prefix
    /// clones the handle, which is the whole capture/await split.
    gate: Arc<Gate>,
}

#[bridge]
impl Miner {
    pub fn new(label: String) -> Self {
        Miner { label, calls: 0, watchers: Vec::new(), gate: Arc::default() }
    }

    /// `thread::sleep` on this actor's executor, returning the elapsed millis.
    /// -1 when the std facility is absent.
    ///
    /// The half of the sleep contract where waiting is **legal**: an actor owns
    /// its executor, which on web is a Worker, and a worker may wait. On the stock build the host busy-waits — there is no wait
    /// instruction without atomics — so this burns that worker's core for the
    /// duration and nothing else. The main-thread half, which is refused, is
    /// the free function [`std_sleep_millis`].
    ///
    /// Also the block-check fixture's **placement** half
    /// (`tools/check_block.dart`): a claim settled by where the body runs
    /// rather than by an artifact. The claim is not a euphemism here — this
    /// body genuinely waits, and it is still true that main-thread Dart
    /// calling `sleepOnExecutor` only serializes bytes and posts them. That is
    /// what `no_block` says, and it is why an actor
    /// member gets no check root.
    #[bridge(no_block)]
    pub fn sleep_on_executor(&mut self, millis: i64) -> i64 {
        self.calls += 1;
        #[cfg(any(not(target_family = "wasm"), target_os = "wasi", feature = "wasm-std-facilities"))]
        {
            let start = std::time::Instant::now();
            std::thread::sleep(std::time::Duration::from_millis(millis.max(0) as u64));
            start.elapsed().as_millis() as i64
        }
        #[cfg(all(target_family = "wasm", not(target_os = "wasi"), not(feature = "wasm-std-facilities")))]
        {
            let _ = millis;
            -1
        }
    }

    /// Waits without holding the instance — the whole point of `Deferred`.
    ///
    /// The prefix (this body) runs serialized on the executor: it may touch
    /// `&mut self` (`calls`), and it clones the gate. The wait itself is the
    /// returned future, which parks until [`Miner::release`] — a *later
    /// message on this same actor* — opens the gate. That is the proof shape:
    /// if a deferred method held the instance, `release` could never run and
    /// this could never complete.
    ///
    /// `token` rides into the completion (`gate value + token`) so a test can
    /// prove each caller's `await` got its own call's answer — the
    /// correlation the event workaround made apps do by hand.
    pub fn deferred_wait(&mut self, token: i64) -> Deferred<i64> {
        self.calls += 1;
        let gate = self.gate.clone();
        Deferred::new(async move { gate.wait().await + token })
    }

    /// Open the gate: every parked [`Miner::deferred_wait`] completes.
    pub fn release(&mut self, value: i64) {
        self.gate.open(value);
    }

    /// A typed error through the deferred completion path — `Deferred`'s
    /// analogue of [`Miner::withdraw_from`], which pins the same decoder on
    /// the inline actor route.
    pub fn deferred_withdraw(&mut self, amount: i64) -> Deferred<Result<i64, WithdrawError>> {
        self.calls += 1;
        Deferred::new(async move { withdraw(100, amount) })
    }

    /// Deliberately panicking, in either half: `in_prefix` panics on the
    /// executor (the call must still answer — with the panic envelope);
    /// otherwise the panic happens inside the detached future, where the
    /// executor's poll catch (native) or the worker pump's drain-trap
    /// attribution (web) must turn it into a `BridgePanicException` rather
    /// than a silently hung future.
    pub fn deferred_flawed(&self, in_prefix: bool) -> Deferred<i64> {
        if in_prefix {
            panic!("deferred prefix failed");
        }
        Deferred::new(async move { panic!("deferred future failed") })
    }

    /// A typed error from an *actor* method. The dispatch shape differs from a
    /// free function's — the call runs on the actor's own executor and its
    /// completion comes back through the host — so the decoder has to survive
    /// a route nothing else here exercises.
    pub fn withdraw_from(&mut self, amount: i64) -> Result<i64, WithdrawError> {
        self.calls += 1;
        withdraw(100, amount)
    }

    /// Open a stream this actor *keeps*, so the handle can be abandoned with a
    /// channel still live. The reaper's terminal is what this exists to expose.
    pub fn watch(&mut self, sink: StreamSink<i64>) {
        self.watchers.push(sink);
    }

    /// Install the logger from *inside this actor* — the shape web needs,
    /// where an actor is a separate wasm instance and `log`'s logger is one of
    /// its own `static`s.
    ///
    /// The same source is two different things on the two platforms, which is
    /// exactly what the cross-language test pins: natively this is the *process*
    /// logger, so it displaces whatever a free-function install registered and
    /// that stream is closed; on web it is a second, independent logger and both
    /// streams stay open.
    pub fn install_logging(&mut self, max_level: String, sink: StreamSink<LogLine>) -> Result<()> {
        self.calls += 1;
        frustrate::logging::install(level_filter(&max_level)?, sink, log_line)?;
        Ok(())
    }

    /// Log from the actor's own executor — its thread natively, its Worker on
    /// web. On web this record can only reach a sink installed *here*.
    pub fn emit_log(&mut self, message: String) {
        self.calls += 1;
        log::info!("{message}");
    }

    /// Deliberately panicking constructor: a failed spawn must be loud
    /// (BridgePanicException) and strand nothing — the freshly created
    /// executor is torn down, pinned via [actor_host_count].
    pub fn flawed(label: String) -> Self {
        panic!("miner `{label}` failed to assemble")
    }

    pub fn label(&self) -> String {
        self.label.clone()
    }

    pub fn calls(&self) -> i64 {
        self.calls
    }

    /// Deliberately brute-force CPU work, counted per call.
    pub fn nth_prime(&mut self, n: i64) -> i64 {
        self.calls += 1;
        let mut count = 0i64;
        let mut candidate = 1i64;
        while count < n {
            candidate += 1;
            let mut is_prime = candidate >= 2;
            let mut d = 2i64;
            while d * d <= candidate {
                if candidate % d == 0 {
                    is_prime = false;
                    break;
                }
                d += 1;
            }
            if is_prime {
                count += 1;
            }
        }
        candidate
    }

    /// Payload-bandwidth fixture for the benchmark: touches the data only
    /// sparsely so the measurement is the channel, not this loop.
    pub fn digest(&self, data: Vec<u8>) -> i64 {
        data.len() as i64 + data.iter().step_by(4096).map(|b| *b as i64).sum::<i64>()
    }

    /// Progress stream computed on this actor's executor: items relay out
    /// while the method is still running — real-time streaming during long
    /// CPU work, the capability the streams design pins on web.
    pub fn mine_progress(&mut self, rounds: i64, n: i64, sink: StreamSink<i64>) {
        for _ in 0..rounds {
            self.calls += 1;
            let mut count = 0i64;
            let mut candidate = 1i64;
            while count < n {
                candidate += 1;
                let mut is_prime = candidate >= 2;
                let mut d = 2i64;
                while d * d <= candidate {
                    if candidate % d == 0 {
                        is_prime = false;
                        break;
                    }
                    d += 1;
                }
                if is_prime {
                    count += 1;
                }
            }
            if !sink.add(candidate) {
                return;
            }
        }
    }

    /// Cooperative cancel observed from inside an actor's OWN executor.
    ///
    /// Delivery always stops on cancel — that is the Dart-side tombstone. The
    /// question this fixture answers is whether the *producer* is told, which
    /// on web means the signal has to reach the actor's own wasm instance and
    /// its own cancel registry, not the main instance's.
    /// Returns the item index at which `add` refused,
    /// or -1 if the whole budget ran without the flag ever being observed.
    pub fn mine_until_cancelled(&mut self, budget: i64, sink: StreamSink<i64>) -> i64 {
        for i in 0..budget {
            if !sink.add(i) {
                return i;
            }
            // A computed pause rather than `pace_producer`'s `thread::sleep`,
            // because unlike `stream_until_cancelled` this fixture's driver
            // (bridge_test, "cancelling an actor-owned stream") is NOT
            // `asyncIsParallel`-gated — it runs on single-threaded web too,
            // where std's `thread::sleep` is the `unsupported` stub that
            // panics, and under wasm's panic=abort that is a dead module.
            //
            // `black_box` INSIDE the loop is load-bearing: trailing, it pins
            // the value, LLVM closed-forms the triangular sum, and the pause
            // falls to 1 ns/item at `-C opt-level=3`. Per-iteration the
            // barrier must run each time — 56.7 µs/item at `-O3` versus
            // 1.06 ms at `-O0` (rustc 1.91.1). See `pace_producer`.
            let mut x = 0u64;
            for j in 0..200_000u64 {
                x = std::hint::black_box(x.wrapping_add(j));
            }
        }
        -1
    }

    /// Fire-and-forget callback from an actor's executor.
    pub fn report(&self, cb: DartCallback<String>) {
        cb.call(format!("{}:{}", self.label, self.calls));
    }

    /// Returning callback from an actor's executor (native-only): the
    /// actor thread blocks while the application thread runs the closure.
    pub fn refine(&self, x: i64, f: DartFunction<i64, i64>) -> i64 {
        f.call(x) + 1
    }

    pub fn checked_div(&self, a: i64, b: i64) -> Result<i64> {
        if b == 0 {
            bail!("division by zero in miner `{}`", self.label);
        }
        Ok(a / b)
    }

    pub fn explode(&self) -> i64 {
        panic!("miner `{}` exploded", self.label)
    }

    /// [`pool_rendezvous`] from inside this actor: true iff `width` actors are
    /// in here at once. Native only — web actors are separate wasm instances
    /// and share no counter.
    pub fn meet(&self, width: i64, max_wait_ms: i64) -> bool {
        pool_rendezvous(width, max_wait_ms)
    }
}

/// Panics in Drop: pins that a generated actor `dispose()` releases the
/// executor even when the drop call itself fails (the panic must still
/// surface — contracts loud — but strand nothing; see actor_host_count).
#[bridge(actor)]
pub struct Grenade {}

#[bridge]
impl Grenade {
    pub fn new() -> Self {
        Grenade {}
    }
}

impl Drop for Grenade {
    fn drop(&mut self) {
        panic!("grenade went off in Drop")
    }
}

/// The Actor exemption, proven end to end: a deliberately thread-affine type.
///
/// `Rc<Cell<i64>>` is neither `Send` nor `Sync`, so this type could not be
/// confined, frozen or locked — each of those mints its handle behind a bound
/// (runtime/rust/src/handle.rs) that codegen also asserts per opaque. Actor is
/// exempt because it is affine end to end: the constructor body, every method
/// body and the drop all run on this object's own executor, and the handle
/// never reaches another thread.
///
/// **The `Rc` field is load-bearing.** Replacing it with an `Arc` or an
/// `AtomicI64` deletes the witness: the crate would still compile if a `Send`
/// bound leaked onto the actor path.
#[bridge(actor)]
pub struct Affine {
    ticks: std::rc::Rc<std::cell::Cell<i64>>,
}

#[bridge]
impl Affine {
    pub fn new() -> Self {
        Affine {
            ticks: std::rc::Rc::new(std::cell::Cell::new(0)),
        }
    }

    /// Clones the `Rc` and drops the clone inside the body. The refcount is
    /// non-atomic, so this is sound only because both ends of the clone's life
    /// happen on the executor thread.
    pub fn tick(&self) -> i64 {
        let alias = std::rc::Rc::clone(&self.ticks);
        alias.set(alias.get() + 1);
        self.ticks.get()
    }
}

// ----------------------------------------------------------------- traits --
//
// Trait objects across the bridge: the trait
// declaration is the bridged surface; concrete impls are ordinary Rust,
// invisible to codegen. One trait per concurrency model.

/// Confined trait: single-owner mutation through a vtable.
///
/// `Send`, and only `Send` (FR0021): the handle holds `Box<dyn Tally>`, which
/// a dispatched factory mints on a pool worker before the calling isolate
/// uses it. Erasure means the bound has nowhere to live but the trait.
#[bridge(confined)]
pub trait Tally: Send {
    fn bump(&mut self) -> i64;
    fn total(&self) -> i64;
    /// Default-bodied methods bridge like required ones (dyn dispatch).
    fn describe(&self) -> String {
        format!("tally at {}", self.total())
    }
    /// A consuming receiver on a bridged trait: `t.take().settle()` on the
    /// Dart side, whichever implementor is behind the token. The impl tag the
    /// extension writes says which registry the take comes out of.
    fn settle(self: Box<Self>) -> i64 {
        self.total() * 100
    }
}

struct StepTally {
    step: i64,
    total: i64,
}

impl Tally for StepTally {
    fn bump(&mut self) -> i64 {
        self.total += self.step;
        self.total
    }
    fn total(&self) -> i64 {
        self.total
    }
}

struct SquareTally {
    n: i64,
}

impl Tally for SquareTally {
    fn bump(&mut self) -> i64 {
        self.n += 1;
        self.n * self.n
    }
    fn total(&self) -> i64 {
        self.n * self.n
    }
    fn describe(&self) -> String {
        format!("{}^2 = {}", self.n, self.n * self.n)
    }
}

/// One factory, two behaviors behind one Dart type — the polymorphism the
/// integration test pins.
#[bridge(sync)]
pub fn new_tally(kind: String, step: i64) -> Result<Box<dyn Tally>> {
    match kind.as_str() {
        "step" => Ok(Box::new(StepTally { step, total: 0 })),
        "square" => Ok(Box::new(SquareTally { n: 0 })),
        other => bail!("unknown tally kind `{other}`"),
    }
}

/// A bridged trait **by value**: the parameter's impl tag names the registry,
/// and what comes out of it is re-boxed into the `Box<dyn Tally>` declared
/// here. Confined, so the take is infallible.
#[bridge(sync)]
pub fn settle_tally(t: Box<dyn Tally>) -> i64 {
    t.settle()
}

/// Frozen trait: immutable sharing, sync and async methods, and a trait
/// method returning a fresh trait object.
#[bridge(frozen)]
pub trait Greeter: Send + Sync {
    #[bridge(sync)]
    fn greet(&self, name: String) -> String;
    /// Async: runs on the pool over the shared Arc'd trait object.
    fn greet_many(&self, names: Vec<String>) -> Vec<String>;
    /// A trait method minting a new trait-object handle.
    fn louder(&self) -> Box<dyn Greeter>;
    /// A consuming receiver on a **frozen** trait: the take is
    /// `Arc::try_unwrap`, so every call on the handle must have finished.
    fn farewell(self: Box<Self>) -> String {
        format!("{} — and goodbye", self.greet("you".into()))
    }
}

struct PlainGreeter {
    excitement: usize,
}

impl Greeter for PlainGreeter {
    fn greet(&self, name: String) -> String {
        format!("hello {name}{}", "!".repeat(self.excitement))
    }
    fn greet_many(&self, names: Vec<String>) -> Vec<String> {
        names.into_iter().map(|n| self.greet(n)).collect()
    }
    fn louder(&self) -> Box<dyn Greeter> {
        Box::new(PlainGreeter {
            excitement: self.excitement + 1,
        })
    }
}

struct PirateGreeter;

impl Greeter for PirateGreeter {
    fn greet(&self, name: String) -> String {
        format!("ahoy {name}")
    }
    fn greet_many(&self, names: Vec<String>) -> Vec<String> {
        names.into_iter().map(|n| self.greet(n)).collect()
    }
    fn louder(&self) -> Box<dyn Greeter> {
        Box::new(PirateGreeter)
    }
}

#[bridge(sync)]
pub fn new_greeter(kind: String) -> Box<dyn Greeter> {
    match kind.as_str() {
        "pirate" => Box::new(PirateGreeter),
        _ => Box::new(PlainGreeter { excitement: 0 }),
    }
}

/// Async free function borrowing a trait object (`&dyn` param).
#[bridge]
pub fn greet_crowd(g: &dyn Greeter, names: Vec<String>) -> String {
    g.greet_many(names).join(", ")
}

/// A frozen trait taken by value on a **pool** arm: the adoption happens on the
/// calling thread and the fallible unwrap inside the body, where an `Outcome`
/// can carry a contended take.
#[bridge]
pub fn dismiss_greeter(g: Box<dyn Greeter>) -> String {
    g.farewell()
}

/// Locked trait: shared mutation behind the same contention contracts as a
/// concrete Locked type.
#[bridge(locked)]
pub trait Store: Send + Sync {
    fn put(&mut self, key: String, value: String);
    fn get(&self, key: String) -> Option<String>;
    #[bridge(sync, on_contention = "error")]
    fn size(&self) -> usize;
    /// Holds the write lock (contention fixture; asyncIsParallel-gated in
    /// the suite, like Counter::hold_write).
    fn compact(&mut self, millis: i64);
    /// A consuming receiver on a **locked** trait, taken synchronously.
    /// Nothing acquires a guard — `Arc::try_unwrap` then `into_inner` — so
    /// there is no contention contract to name and no `on_contention` here.
    #[bridge(sync)]
    fn drain(self: Box<Self>) -> usize {
        self.size()
    }
}

#[derive(Default)]
struct MemStore {
    map: HashMap<String, String>,
}

impl Store for MemStore {
    fn put(&mut self, key: String, value: String) {
        self.map.insert(key, value);
    }
    fn get(&self, key: String) -> Option<String> {
        self.map.get(&key).cloned()
    }
    fn size(&self) -> usize {
        self.map.len()
    }
    fn compact(&mut self, millis: i64) {
        std::thread::sleep(std::time::Duration::from_millis(millis as u64));
    }
}

#[derive(Default)]
struct ShoutStore {
    map: HashMap<String, String>,
}

impl Store for ShoutStore {
    fn put(&mut self, key: String, value: String) {
        self.map.insert(key, value.to_uppercase());
    }
    fn get(&self, key: String) -> Option<String> {
        self.map.get(&key).cloned()
    }
    fn size(&self) -> usize {
        self.map.len()
    }
    fn compact(&mut self, millis: i64) {
        std::thread::sleep(std::time::Duration::from_millis(millis as u64));
    }
}

/// A locked trait taken by value on a pool arm.
#[bridge]
pub fn close_store(s: Box<dyn Store>) -> usize {
    s.drain()
}

/// A trait handle **borrowed** beside one **taken**: the duplicate check has to
/// see both acquisitions, and the take must not join the lock plan — nothing
/// acquires a guard for it.
#[bridge]
pub fn store_absorb(keep: &mut dyn Store, gone: Box<dyn Store>) -> usize {
    let n = gone.drain();
    for i in 0..n {
        keep.put(format!("a{i}"), "x".into());
    }
    keep.size()
}

#[bridge]
pub fn open_store(kind: String) -> Result<Box<dyn Store>> {
    match kind.as_str() {
        "mem" => Ok(Box::new(MemStore::default())),
        "shout" => Ok(Box::new(ShoutStore::default())),
        other => bail!("unknown store kind `{other}`"),
    }
}

// ------------------------------------------- traits: bridged implementors --
//
// Bridged `impl Trait for Type` blocks.
// The Dart classes implement the traits' interfaces; their handles cross at
// `&dyn` parameters via impl tags. Written methods dispatch statically;
// unwritten default methods are synthesized from the trait's signatures.

/// Confined implementor of Tally. `describe` is NOT written here: the
/// trait's default body is synthesized onto the class (UFCS needs only the
/// signature).
#[bridge(confined)]
pub struct Abacus {
    beads: i64,
}

#[bridge]
impl Abacus {
    #[bridge(sync)]
    pub fn new() -> Self {
        Abacus { beads: 0 }
    }
}

#[bridge]
impl Tally for Abacus {
    fn bump(&mut self) -> i64 {
        self.beads += 10;
        self.beads
    }
    fn total(&self) -> i64 {
        self.beads
    }
}

/// Frozen implementor of Greeter.
#[bridge(frozen)]
pub struct RobotGreeter {
    id: i64,
}

#[bridge]
impl RobotGreeter {
    #[bridge(sync)]
    pub fn build(id: i64) -> Self {
        RobotGreeter { id }
    }
}

#[bridge]
impl Greeter for RobotGreeter {
    #[bridge(sync)]
    fn greet(&self, name: String) -> String {
        format!("BEEP {name} [unit {}]", self.id)
    }
    fn greet_many(&self, names: Vec<String>) -> Vec<String> {
        names.into_iter().map(|n| self.greet(n)).collect()
    }
    fn louder(&self) -> Box<dyn Greeter> {
        Box::new(RobotGreeter { id: self.id + 1 })
    }
}

/// Locked implementor of Store.
#[bridge(locked)]
pub struct CountingStore {
    map: HashMap<String, String>,
    puts: i64,
}

#[bridge]
impl CountingStore {
    #[bridge(sync)]
    pub fn fresh() -> Self {
        CountingStore {
            map: HashMap::new(),
            puts: 0,
        }
    }

    pub fn puts(&self) -> i64 {
        self.puts
    }
}

#[bridge]
impl Store for CountingStore {
    fn put(&mut self, key: String, value: String) {
        self.puts += 1;
        self.map.insert(key, value);
    }
    fn get(&self, key: String) -> Option<String> {
        self.map.get(&key).cloned()
    }
    #[bridge(sync, on_contention = "error")]
    fn size(&self) -> usize {
        self.map.len()
    }
    fn compact(&mut self, millis: i64) {
        std::thread::sleep(std::time::Duration::from_millis(millis as u64));
    }
}

// Tagged `&dyn` crossings, one per model/mutability shape.

/// Sync mutable borrow of a confined trait object.
#[bridge(sync)]
pub fn bump_twice(t: &mut dyn Tally) -> i64 {
    t.bump();
    t.bump()
}

// Tagged `&dyn` crossings from *inside* a container: each element carries its
// own impl tag, and the acquisition behind each tag is chosen per element.

/// Confined, synchronous: the tag is read off the staged pair where it stands
/// and the arms unify to `&dyn Tally` at an annotated `let`.
#[bridge(sync)]
pub fn tally_sum(ts: Vec<&dyn Tally>) -> i64 {
    ts.iter().map(|t| t.total()).sum()
}

/// `&mut` elements: the duplicate check runs over the runtime count, and each
/// entry is built for the tag's own backing type.
#[bridge(sync)]
pub fn tally_bump_all(ts: Vec<&mut dyn Tally>) -> i64 {
    ts.into_iter().map(|t| t.bump()).sum()
}

/// An `Option` rather than a list, so the tagged staging is exercised where the
/// count is zero or one.
#[bridge(sync)]
pub fn tally_or_zero(t: Option<&dyn Tally>) -> i64 {
    t.map(|t| t.total()).unwrap_or(-1)
}

/// A bridged trait **taken** from inside a container: every element carries its
/// own impl tag, and the take behind each tag goes to that tag's registry.
#[bridge(sync)]
pub fn settle_tallies(ts: Vec<Box<dyn Tally>>) -> i64 {
    ts.into_iter().map(|t| t.settle()).sum()
}

/// Frozen, on the pool arm: one `Arc` clone per element, carried into the
/// closure as the trait's own carrier enum.
#[bridge]
pub fn greet_each(gs: Vec<&dyn Greeter>, name: String) -> Vec<String> {
    gs.iter().map(|g| g.greet(name.clone())).collect()
}

/// The fallible take, per element: any one of them still being in flight
/// refuses the whole call, and the objects already adopted drop with the
/// prelude's locals.
#[bridge]
pub fn dismiss_greeters(gs: Vec<Box<dyn Greeter>>) -> Vec<String> {
    gs.into_iter().map(|g| g.farewell()).collect()
}

/// Locked, over a runtime count: the guards are taken in handle-id order, so
/// two calls naming the same objects in different orders cannot invert.
#[bridge]
pub fn store_sizes(ss: Vec<&dyn Store>) -> Vec<usize> {
    ss.iter().map(|s| s.size()).collect()
}

/// Async read through a locked trait object (read lock on the pool).
#[bridge]
pub fn store_probe(s: &dyn Store, key: String) -> Option<String> {
    s.get(key)
}

/// Async write through a locked trait object (write lock on the pool).
#[bridge]
pub fn store_fill(s: &mut dyn Store, n: i64) {
    for i in 0..n {
        s.put(format!("k{i}"), format!("v{i}"));
    }
}

/// Contract-marked sync read through a locked trait object.
#[bridge(sync, on_contention = "error")]
pub fn store_size_now(s: &dyn Store) -> usize {
    s.size()
}

// -------------------------------------------------- Vec<Opaque> returns --
//
// `Vec<Opaque>` returns rested on codegen unit tests
// and the demo, with no cross-language runtime fixture. These return a list
// of handles the Dart side must then call methods on — one concrete opaque
// (frozen Snapshot) and one trait object (`Box<dyn Greeter>`).
//
// Returning works wherever an owned encode exists: the root, an `Option`, a
// `Vec`, a set, a map (key, value or both), a tuple, and a field of a returned
// struct or enum, at any depth. The same positions are consume positions
// inbound, where the Dart caller spells the ownership it gives up as `take()`
// — a struct field included, once the struct says which of the two shapes its
// Dart class is (`#[bridge(data, inbound)]`, and `Delivery` below).

/// A list of freshly-minted frozen handles, each with its own word list.
#[bridge(sync)]
pub fn snapshots(counts: Vec<i64>) -> Vec<Snapshot> {
    counts
        .into_iter()
        .map(|c| Snapshot {
            words: (0..c).map(|i| format!("w{i}")).collect(),
        })
        .collect()
}

/// A list of trait-object handles (`Box<dyn Greeter>`), each callable and
/// polymorphic behind the one Dart type.
#[bridge(sync)]
pub fn greeters(kinds: Vec<String>) -> Vec<Box<dyn Greeter>> {
    kinds.into_iter().map(new_greeter).collect()
}

/// Several handles of **different** types from one call, plus data about
/// them. Before this, the answer was one call per handle.
///
/// The Dart class holds real handles, so it has no encoder at all: it travels
/// Rust → Dart only. Its generated `==` is the ordinary structural one, in
/// which a handle field compares by identity — so two workspaces over
/// different objects are not the same value.
#[bridge(data)]
pub struct Workspace {
    pub label: String,
    pub snapshot: Snapshot,
    pub counter: Counter,
}

#[bridge(sync)]
pub fn open_workspace(label: String, words: Vec<String>) -> Workspace {
    Workspace {
        label,
        snapshot: Snapshot { words },
        counter: Counter { value: 0 },
    }
}

/// The same through a container and one declaration deeper, so the recursive
/// owned encode is exercised rather than only its top level.
/// A handle-owning data type carries **receiverless** members only: a receiver
/// is decoded out of the request, which is the direction such a type cannot
/// travel (FR0004). A static decodes nothing.
#[bridge]
impl Workspace {
    #[bridge(sync)]
    pub fn default_label() -> String {
        "untitled".into()
    }
}

#[bridge(sync)]
pub fn open_workspaces(labels: Vec<String>) -> Vec<Workspace> {
    labels
        .into_iter()
        .map(|l| open_workspace(l, vec!["w".into()]))
        .collect()
}

/// A tuple is the other ownable return position, and the only anonymous one:
/// two handles of different types come back without declaring a struct for
/// them. It is never copied or compared in return position, which is what the
/// old nesting rule was guarding.
#[bridge(sync)]
pub fn snapshot_and_counter(words: Vec<String>) -> (Snapshot, Counter) {
    (Snapshot { words }, Counter { value: 0 })
}

/// A handle beside a plain value in the same tuple.
#[bridge(sync)]
pub fn labelled_snapshot(label: String) -> (String, Snapshot) {
    (label, Snapshot { words: vec!["w".into()] })
}

/// A frozen handle with the `Hash`/`Ord` a set element or a map key needs.
/// Its own type rather than derives bolted onto `Snapshot`, so the ordering
/// the BTree fixtures below depend on is this type's own business.
#[bridge(frozen)]
#[derive(PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Tag {
    name: String,
}

#[bridge]
impl Tag {
    #[bridge(sync)]
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// Handles as map **values**. A map is consumed on the way out exactly as a
/// `Vec` is, so each handle is minted once and Dart gets one wrapper per entry.
#[bridge(sync)]
pub fn tags_by_name(names: Vec<String>) -> HashMap<String, Tag> {
    names
        .into_iter()
        .map(|n| (n.clone(), Tag { name: n }))
        .collect()
}

/// Handles as a **set**, through the BTree container, which arrives sorted —
/// so the Dart order is a fact the test can assert rather than a coincidence.
#[bridge(sync)]
pub fn tag_set(names: Vec<String>) -> BTreeSet<Tag> {
    names.into_iter().map(|n| Tag { name: n }).collect()
}

/// Handles in a **fixed array**, which is ownable for the same reason a `Vec`
/// is: an array is `IntoIterator` by value, so the elements are owned and each
/// handle is minted once.
#[bridge(sync)]
pub fn tag_pair(a: String, b: String) -> [Tag; 2] {
    [Tag { name: a }, Tag { name: b }]
}

/// A `Box` around a **handle**, in a return and inside a returned struct.
/// Transparent there too: the handle is minted from what the `Box` holds,
/// once, and Dart sees a plain `Tag`. Compiled rather than only asserted about
/// in emit text, because the owned encode's `Box` arm moves out through a
/// deref and that is a thing only rustc can settle.
#[bridge(data)]
pub struct Boxed {
    pub label: String,
    pub tag: Box<Tag>,
}

#[bridge(sync)]
pub fn boxed_tag(label: String) -> Boxed {
    Boxed {
        tag: Box::new(Tag { name: label.clone() }),
        label,
    }
}

/// Handles as map **keys**. Dart equality on a handle is identity, so this map
/// is identity-keyed: iterating it and reading each key is what it is for, not
/// looking one up. That is the same divergence a returned `List<Tag>` already
/// has on `contains`, which is why keys are not a line worth drawing.
#[bridge(sync)]
pub fn tag_lengths(names: Vec<String>) -> BTreeMap<Tag, i64> {
    names
        .into_iter()
        .map(|n| {
            let len = n.len() as i64;
            (Tag { name: n }, len)
        })
        .collect()
}

/// A data enum carrying a handle in one variant: the encoder writes the tag,
/// then mints only what that variant holds.
#[bridge(data)]
pub enum Slot {
    Empty,
    Filled { at: i64, snapshot: Snapshot },
}

#[bridge(sync)]
pub fn slots(counts: Vec<i64>) -> Vec<Slot> {
    counts
        .into_iter()
        .map(|c| {
            if c < 0 {
                Slot::Empty
            } else {
                Slot::Filled {
                    at: c,
                    snapshot: Snapshot { words: (0..c).map(|i| format!("w{i}")).collect() },
                }
            }
        })
        .collect()
}

// ------------------------------------------------ GC-finalizer probe --
//
// The un-disposed-handle → GC finalizer → Rust-side
// drop path. Every other handle test disposes explicitly. Here a confined
// opaque's lifetime is observable through a process-global live count: `new`
// increments, `Drop` decrements. Explicit dispose() and the GC finalizer
// (`frustrate_finalize_LiveProbe`) both run `Drop`, so both return the count
// to baseline — letting the Dart test assert reclamation of a handle it never
// disposed.

static LIVE_PROBES: AtomicI64 = AtomicI64::new(0);

#[bridge(confined)]
pub struct LiveProbe {}

#[bridge]
impl LiveProbe {
    /// Also the block-check fixture's **handed-out handle** half
    /// (`tools/check_block.dart`): a claimed member whose return value is an
    /// opaque with a real `Drop`.
    ///
    /// The claim covers more than this body. `dispose()` calls
    /// `frustrate_drop_LiveProbe` inline on the calling isolate, and the
    /// un-disposed path is a Dart `Finalizer` whose callback runs on the
    /// isolate that attached it — so for a handle the main isolate holds, the
    /// `Drop` below runs on the **main thread**. A `Drop` that waited would
    /// stall the page exactly as a blocking body would, one step later in the
    /// object's life, which is why the root reaches the drop glue as well as
    /// the constructor (`emit_block_checks`). This type is where that is
    /// pinned because its `Drop` is the one the Dart suite already drives
    /// through both routes.
    ///
    /// Sabotaging the `Drop` below with a `static Mutex` take produces
    ///
    /// ```text
    /// frustrate_check_block_203_new
    ///   test_api::frustrate_generated::drop_inner_LiveProbe
    ///     frustrate::handle::confined_drop::test_api::api::LiveProbe
    ///       core::mem::drop::alloc::boxed::Box::test_api::api::LiveProbe
    ///         core::ptr::drop_in_place::alloc::boxed::Box::..::LiveProbe
    ///           core::ptr::drop_in_place::test_api::api::LiveProbe
    ///             test_api::api::LiveProbe::core::ops::drop::Drop::drop
    ///               std::sync::poison::mutex::Mutex::lock
    ///                 std::sys::sync::mutex::futex::Mutex::lock_contended
    ///                   std::sys::pal::wasm::futex::futex_wait
    /// ```
    ///
    /// ...against `memory_atomic_wait32`. Measured, then reverted: the red
    /// direction has no home in a build system that can only assert success,
    /// so it is demonstrated by hand — the same convention, and the same
    /// reasoning, as `//tests/bazel_rules/async_fixture`.
    #[bridge(sync, no_block)]
    pub fn new() -> Self {
        LIVE_PROBES.fetch_add(1, Ordering::SeqCst);
        LiveProbe {}
    }
}

impl Drop for LiveProbe {
    fn drop(&mut self) {
        LIVE_PROBES.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Count of `LiveProbe` Rust objects currently alive.
#[bridge(sync)]
pub fn live_probe_count() -> i64 {
    LIVE_PROBES.load(Ordering::SeqCst)
}

// ------------------------------------------------- Dart-object handles --
// Composition fixtures. Everything here was a
// codegen error before handles became data: two channels on one call, a
// channel beside a real return value, channels nested in a struct or a Vec.

/// Two independent streams from one call, in a struct. The equivalence the
/// whole design exists to deliver: "a struct of two sinks" behaves exactly
/// like two sinks, because a handle is data and composes like data.
#[bridge(data)]
pub struct Fanout {
    pub evens: StreamSink<i64>,
    pub odds: StreamSink<i64>,
}

/// Routes `1..=n` to whichever nested sink matches parity. Proves the two
/// registrations are distinct and do not cross-talk.
#[bridge]
pub fn split_parity(n: i64, out: Fanout) {
    for i in 1..=n {
        let sink = if i % 2 == 0 { &out.evens } else { &out.odds };
        sink.add(i);
    }
}

/// A panic while holding sinks that arrived *nested in a struct* — the shape
/// no per-call special case could ever have covered, which is exactly why the
/// close is done by `Drop` (drop-retire) and not by inspecting the signature:
/// unwinding drops the struct, each nested sink drops, and each posts its own
/// end. Native ends both streams; web (`panic=abort`) leaves them open by
/// design.
#[bridge]
pub fn panic_with_nested_sinks(out: Fanout) {
    out.evens.add(0);
    out.odds.add(1);
    panic!("deliberate panic holding nested sinks");
}

/// A handle beside a real return value — what FR0017 used to forbid. The
/// stream is no longer forced to be the function's only data channel.
#[bridge]
pub fn tee_sum(values: Vec<i64>, echo: StreamSink<i64>) -> i64 {
    let mut total = 0;
    for v in &values {
        total += *v;
        echo.add(*v);
    }
    total
}

/// N channels minted from one `Vec`, each independently routable. Element
/// order on the wire is the order Dart minted them.
#[bridge]
pub fn broadcast_index(sinks: Vec<StreamSink<i64>>) {
    for (i, s) in sinks.iter().enumerate() {
        s.add(i as i64);
    }
}

/// The tighter write-end mirrors. `EventSink` can deliver a NON-terminal
/// error and keep going — Dart's contract, and the pair most easily confused
/// with the terminal `error()`.
#[bridge]
pub fn sink_then_error_then_more(out: frustrate::dart::r#async::EventSink<i64>) {
    out.add(1);
    out.add_error("recoverable");
    out.add(2);
}

/// A plain `dart::core::Sink`: add and close, nothing else.
#[bridge]
pub fn fill_plain_sink(n: i64, out: frustrate::dart::core::Sink<i64>) {
    for i in 0..n {
        out.add(i);
    }
}

/// A returning Dart method reached through a struct — the `call_async`
/// round-trip, composed. Portable: awaited on the executor, not blocking.
#[bridge(data)]
pub struct Transforms {
    pub double: DartFunction<i64, i64>,
}

#[bridge]
pub async fn apply_transform(x: i64, t: Transforms) -> i64 {
    t.double.call_async(x).await
}

// ------------------------------------------- a declared Dart interface --

/// An auditor the *Dart* side implements.
#[bridge(data, dart_interface)]
pub struct Auditor {
    /// Record one step of the audit.
    pub note: DartCallback<String>,
    /// Decide on an amount in a currency. Two parameters, because a tuple item
    /// spreads into a real multi-argument Dart method.
    pub approve: DartFunction<(i64, String), bool>,
    /// Reserve the amount, or refuse *as a value* — the fallible half, whose
    /// declared error rides the generated method's doc comment.
    pub reserve: DartFunction<i64, Result<i64, RefusalError>>,
}

/// Two void calls around one returning call, on ONE Dart object — the
/// round-trip the declared form exists for. Portable: the reply is awaited on
/// the cooperative executor, never blocked on.
#[bridge]
pub async fn audit(amount: i64, currency: String, a: Auditor) -> String {
    a.note.call(format!("reviewing {amount} {currency}"));
    let ok = a.approve.call_async((amount, currency)).await;
    a.note.call(if ok { "approved" } else { "denied" }.to_string());
    match a.reserve.call_async(amount).await {
        Ok(left) => format!("{ok}/{left}"),
        Err(RefusalError::Busy { retry_in_ms }) => format!("{ok}/busy {retry_in_ms}"),
        Err(RefusalError::NotAllowed) => format!("{ok}/not allowed"),
    }
}

/// The interface nested in an ordinary bridged struct, and riding a `Vec` —
/// "handles are data" is unchanged by the new surface, and one Dart object per
/// element gets its own registrations.
#[bridge(data)]
pub struct AuditRun {
    pub label: String,
    pub auditors: Vec<Auditor>,
}

#[bridge]
pub async fn run_audit(run: AuditRun, amount: i64) -> i64 {
    let mut approved = 0;
    for a in &run.auditors {
        a.note.call(run.label.clone());
        if a.approve.call_async((amount, "usd".to_string())).await {
            approved += 1;
        }
    }
    approved
}

/// Keep-alive across calls, so the leak report has something to name: one Dart
/// object holds three registrations, each labelled with its own method.
static AUDITOR: std::sync::Mutex<Option<Auditor>> = std::sync::Mutex::new(None);

/// `async fn`, and that is not decoration: `Auditor` has returning methods, so
/// a member taking one and *unable to await* would be native-only — the
/// classification is per-interface, not per-method. This file must compile for
/// web even though the test that reads the labels is native-only.
#[bridge]
pub async fn stash_auditor(a: Auditor) {
    *AUDITOR.lock().unwrap() = Some(a);
}

/// Notes through the stored interface. False if nothing is stored.
#[bridge]
pub fn poke_stashed_auditor(msg: String) -> bool {
    match AUDITOR.lock().unwrap().as_ref() {
        Some(a) => {
            a.note.call(msg);
            true
        }
        None => false,
    }
}

#[bridge]
pub fn drop_stashed_auditor() {
    *AUDITOR.lock().unwrap() = None;
}

/// Keep-alive across calls, composed: a stored struct of channels outlives
/// the call that opened it, and a later call pushes to both.
static FANOUT: std::sync::Mutex<Option<Fanout>> = std::sync::Mutex::new(None);

#[bridge]
pub fn stash_fanout(out: Fanout) {
    *FANOUT.lock().unwrap() = Some(out);
}

/// Pushes one item to each stored sink. Returns false if nothing is stored.
#[bridge]
pub fn poke_stashed_fanout(v: i64) -> bool {
    match FANOUT.lock().unwrap().as_ref() {
        Some(f) => {
            f.evens.add(v);
            f.odds.add(-v);
            true
        }
        None => false,
    }
}

/// Drops the stored channels: each Dart stream closes (drop-retire).
#[bridge]
pub fn drop_stashed_fanout() {
    *FANOUT.lock().unwrap() = None;
}

/// Per-sink cancellation observation for [`stream_both_until_cancelled`]:
/// bit 0 = the `evens` channel refused, bit 1 = `odds` refused.
static NESTED_CANCEL: AtomicI64 = AtomicI64::new(0);

/// Pushes to BOTH nested channels until each observes its own cancel flag.
/// Cancelling one Dart subscription must stop only that producer — the
/// per-handle cancellation that a struct of channels has to support if
/// "a struct of two sinks is two sinks" is to mean anything.
#[bridge]
pub fn stream_both_until_cancelled(out: Fanout) {
    NESTED_CANCEL.store(0, Ordering::SeqCst);
    let (mut evens_live, mut odds_live) = (true, true);
    for i in 0..20_000i64 {
        if evens_live && !out.evens.add(i) {
            evens_live = false;
            NESTED_CANCEL.fetch_or(1, Ordering::SeqCst);
        }
        if odds_live && !out.odds.add(-i) {
            odds_live = false;
            NESTED_CANCEL.fetch_or(2, Ordering::SeqCst);
        }
        if !evens_live && !odds_live {
            return;
        }
        // A computed pause rather than `pace_producer`'s `thread::sleep`. This
        // fixture's driver *is* `asyncIsParallel`-gated, so sleeping would be
        // legal here — it is declined for the failure path. Only ONE of the
        // two channels is cancelled; the sibling is stopped by the test's
        // closing `odds.close()` (done → the subscription auto-cancels →
        // the generated `t.onCancel` → `cancelStream`), so on the passing path
        // it stops promptly either way. On a run where a cancel is *missed*,
        // though, a millisecond cadence would leave a 20 s producer posting
        // into the tests that follow; a computed pause keeps that tail ~1.1 s
        // at `-c opt`, and unchanged at fastbuild.
        //
        // `black_box` INSIDE the loop is load-bearing: trailing, it pins the
        // value and LLVM closed-forms the triangular sum to 1 ns/item at
        // `-C opt-level=3`. See `pace_producer` for the full measurement.
        let mut x = 0u64;
        for j in 0..200_000u64 {
            x = std::hint::black_box(x.wrapping_add(j));
        }
    }
}

/// Which nested channels have observed their cancel (see [`NESTED_CANCEL`]).
#[bridge(sync)]
pub fn nested_cancel_state() -> i64 {
    NESTED_CANCEL.load(Ordering::SeqCst)
}

// ------------------------------------------- Confined: re-entrancy probe --

/// What one `&mut self` member observed about its own receiver across a post
/// into a caller-supplied Dart sink.
#[bridge(data)]
pub struct ReentryReport {
    /// `self.log.len()` read at the top of the call.
    pub before: i64,
    /// `self.log.len()` read at the bottom of the *same* call, through the
    /// *same* `&mut self`. A difference is the whole witness: it means a
    /// second `&mut` to this object was live inside this one and wrote
    /// through it.
    pub after: i64,
}

/// Settles whether a caller-supplied `Sink` implementation can re-enter the
/// handle it was handed to *while a `&mut self` borrow of that handle is
/// live*.
///
/// Confined + `&mut self` + `#[bridge(sync)]` is the shape that matters, and
/// it is the only shape available: FR0012 forbids confined-in-async, so a
/// confined member is always sync, and the generated glue holds
/// `handle::confined_mut` — a raw `&mut *ptr` — for the whole call
/// (codegen/src/emit_rust.rs). On wasm the sink post is delivered *inline*
/// (runtime/rust/src/post.rs), inside that call.
#[bridge(confined)]
pub struct ReentrantProbe {
    log: Vec<i64>,
}

#[bridge]
impl ReentrantProbe {
    #[bridge(sync)]
    pub fn new() -> Self {
        ReentrantProbe { log: vec![] }
    }

    /// Post `n` items into the caller's sink while holding `&mut self`, and
    /// report what `self` looked like on either side of the posts.
    #[bridge(sync)]
    pub fn feed(&mut self, n: i64, out: frustrate::dart::core::Sink<i64>) -> ReentryReport {
        let before = self.log.len() as i64;
        for i in 0..n {
            out.add(i);
        }
        let after = self.log.len() as i64;
        ReentryReport { before, after }
    }

    /// [`feed`](Self::feed)'s closure twin: same `&mut self` borrow, same
    /// posts, but into a `DartCallback` rather than a `Sink`.
    ///
    /// It exists because the two mirrors reached user code by different
    /// routes. A sink mirror was always called directly and depended on
    /// `StreamRouter._deferDelivery` for its web safety; a void closure mirror
    /// carried its own `scheduleMicrotask` in the generated glue, so it was
    /// safe *redundantly* and this path was never pinned. Deleting that hop
    /// makes the router the sole guard here too — exactly the situation
    /// a deferral flag that covers `_invoke` and not the sink path would
    /// silently stop guarding.
    #[bridge(sync)]
    pub fn feed_closure(&mut self, n: i64, out: DartCallback<i64>) -> ReentryReport {
        let before = self.log.len() as i64;
        for i in 0..n {
            out.call(i);
        }
        let after = self.log.len() as i64;
        ReentryReport { before, after }
    }

    /// The memory-safety shape, in ordinary safe Rust: iterate the object's
    /// own buffer while posting each element. `shrink_to_fit` makes the next
    /// `push` a guaranteed reallocation, so a `note` that landed *inside* this
    /// call would move the buffer out from under this iterator.
    ///
    /// It must return `[0..n)` on every configuration. Garbage means delivery
    /// ran on the Rust stack and the read below went through a pointer into
    /// the freed buffer — which is what both web builds did before
    /// `StreamRouter._deferDelivery` covered the void-method path.
    #[bridge(sync)]
    pub fn feed_from_log(&mut self, out: frustrate::dart::core::Sink<i64>) -> Vec<i64> {
        self.log.shrink_to_fit();
        let mut seen = vec![];
        for x in &self.log {
            out.add(*x);
            // Deliberately read AFTER the post, so the value witnesses whether
            // anything reallocated `self.log` during it.
            seen.push(*x);
        }
        seen
    }

    /// The re-entrant lever: what a caller's `Sink.add` calls back into. Takes
    /// its own `&mut self` — a second one, if `feed` is still on the stack.
    #[bridge(sync)]
    pub fn note(&mut self, v: i64) -> i64 {
        self.log.push(v);
        self.log.len() as i64
    }

    #[bridge(sync)]
    pub fn log(&self) -> Vec<i64> {
        self.log.clone()
    }
}

// ------------------------------------------------ cancelling an async fn --

/// Whether the future of the last `awaits_until_cancelled` call has been
/// dropped. Written by the guard the body holds across its suspension point,
/// so it flips only when the executor drops the task — which is what
/// cancellation *is*.
static CANCEL_DROPPED_FUTURE: AtomicBool = AtomicBool::new(false);

/// Flips [`CANCEL_DROPPED_FUTURE`] when it dies. Held by the suspended future,
/// so its drop is the future's drop.
struct DropSensor;

impl Drop for DropSensor {
    fn drop(&mut self) {
        CANCEL_DROPPED_FUTURE.store(true, Ordering::SeqCst);
    }
}

/// `Pending` forever, and never wakes: no waker is registered, so nothing but a
/// cancel can ever move this task again. That is deliberate — a self-waking
/// future would let a drain complete the call while a test was mid-cancel, and
/// the race under test would be decided by timing instead of by the claim.
struct NeverReady;

impl std::future::Future for NeverReady {
    type Output = ();
    fn poll(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        std::task::Poll::Pending
    }
}

/// Suspends forever, holding a drop sensor across the await.
///
/// The fixture for `FrustrateCancelToken`: the only way this call ever ends is
/// `frustrate_call_cancel` claiming it, and the only observable that
/// distinguishes "the Dart future was rejected" from "the Rust future was
/// actually dropped" is [`cancelled_future_was_dropped`]. A test that only
/// checked the Dart side would pass against a runtime that leaked every
/// cancelled task.
///
/// Portable on purpose: a Rust `async fn` runs on the cooperative executor on
/// every configuration, so this exercises native, single-threaded web and
/// threaded web through the same source.
#[bridge]
pub async fn awaits_until_cancelled() -> i64 {
    let _sensor = DropSensor;
    NeverReady.await;
    0
}

/// Whether the last `awaits_until_cancelled` future has been dropped.
///
/// `#[bridge(sync)]` so a test can read it on the application thread without
/// queueing behind the async work it is asking about.
#[bridge(sync)]
pub fn cancelled_future_was_dropped() -> bool {
    CANCEL_DROPPED_FUTURE.load(Ordering::SeqCst)
}

/// Reset the sensor. Every suite in the local `dart test` flow shares this
/// dylib, so a test that asserted on a flag another file had already set would
/// pass for the wrong reason.
#[bridge(sync)]
pub fn reset_cancel_sensor() {
    CANCEL_DROPPED_FUTURE.store(false, Ordering::SeqCst);
}

/// An ordinary `async fn` that answers immediately — the control for
/// "cancelling after the answer changes nothing".
#[bridge]
pub async fn answers_at_once(x: i64) -> i64 {
    x + 1
}

// ---------------------------------------------------- multi-lock ordering --
//
// A member taking two or more locked handles acquires them in
// `handle::lock_plan` order rather than parameter order, and refuses a
// repeated handle. These fixtures exist to compile that emission and to give
// `tests/dart_integration` something to call: `Vault::merge(v, v)` used to
// take two write guards on one `RwLock` from one thread, which hangs.

#[bridge(locked)]
pub struct Vault {
    pub balance: i64,
}

#[bridge]
impl Vault {
    #[bridge(sync)]
    pub fn new_(balance: i64) -> Vault {
        Vault { balance }
    }

    /// The sync try-lock reader, on every target: the acquisition is a
    /// compare-exchange and the release reaches no wait instruction, so the
    /// browser main thread may run it.
    #[bridge(sync, on_contention = "error")]
    pub fn balance(&self) -> i64 {
        self.balance
    }

    /// The dispatched reader beside it: the guard is taken and dropped
    /// wherever the executor runs the body, so a contended call waits instead
    /// of refusing.
    pub fn read_balance(&self) -> i64 {
        self.balance
    }

    /// A locked receiver and a locked parameter of the same type: the pair a
    /// caller is most likely to accidentally make one object.
    pub fn merge(&mut self, other: &mut Vault) -> i64 {
        self.balance += other.balance;
        other.balance = 0;
        self.balance
    }
}

/// Two locked parameters, and a sibling naming them in the opposite order.
/// Acquired in plan order, both agree, so neither can hold the other's next
/// lock.
#[bridge]
pub fn transfer(from: &mut Vault, to: &mut Vault, amount: i64) -> i64 {
    from.balance -= amount;
    to.balance += amount;
    to.balance
}

#[bridge]
pub fn transfer_reversed(to: &mut Vault, from: &mut Vault, amount: i64) -> i64 {
    from.balance -= amount;
    to.balance += amount;
    to.balance
}

/// The sync try-lock arm still returns `Contention` from inside the planned
/// loop.
#[bridge(sync, on_contention = "error")]
pub fn sum_vaults_try(a: &mut Vault, b: &mut Vault) -> i64 {
    a.balance + b.balance
}

/// The blocking arm is native-only, and plans exactly as the others do.
#[bridge(sync, on_contention = "block", native_only)]
pub fn sum_vaults_blocking(a: &mut Vault, b: &mut Vault) -> i64 {
    a.balance + b.balance
}

// ------------------------------------------- confined handle aliasing --
//
// A confined method taking a same-type parameter can be handed one object
// twice by Dart, which would materialize `&mut` and `&` over one Box —
// undefined behaviour, not a hang. The handles are compared before either
// deref happens (handle::alias_check).

#[derive(Hash, PartialEq, Eq, PartialOrd, Ord)]
#[bridge(confined)]
pub struct Ledger {
    pub total: i64,
}

#[bridge]
impl Ledger {
    #[bridge(sync)]
    pub fn new_(total: i64) -> Ledger {
        Ledger { total }
    }

    #[bridge(sync)]
    pub fn total(&self) -> i64 {
        self.total
    }

    /// The receiver is `&mut` and the parameter is `&`: one object passed for
    /// both is the aliasing case.
    #[bridge(sync)]
    pub fn absorb(&mut self, other: &Ledger) -> i64 {
        self.total += other.total;
        self.total
    }
}

/// Two shared borrows cannot alias badly, so this carries no check and one
/// object passed twice is legal.
#[bridge(sync)]
pub fn ledgers_equal(a: &Ledger, b: &Ledger) -> bool {
    a.total == b.total
}

// --------------------------------------------------------------- logging --
//
// The `frustrate::logging` fixture. Everything here
// is ordinary bridge surface — a struct, a sink parameter, a `Result` — which
// is itself the claim under test: the facility is a runtime pair, and codegen
// knows nothing about it.

/// The record shape, declared **here** rather than in the runtime. It has to
/// be: codegen parses this crate's declared files and nothing else, so a
/// struct the runtime supplied could never appear in a generated binding.
#[bridge(data)]
pub struct LogLine {
    pub level: String,
    pub target: String,
    pub message: String,
}

fn log_line(record: &log::Record<'_>) -> LogLine {
    LogLine {
        level: record.level().to_string(),
        target: record.target().to_string(),
        message: record.args().to_string(),
    }
}

fn level_filter(name: &str) -> Result<log::LevelFilter> {
    name.parse::<log::LevelFilter>()
        .map_err(|_| anyhow::anyhow!("not a log level: {name}"))
}

/// Send every record `log` accepts to `sink`. Sync, because installing is a
/// registration and not work — the same reason `TextDoc::watch` is.
#[bridge(sync)]
pub fn install_logging(max_level: String, sink: StreamSink<LogLine>) -> Result<()> {
    frustrate::logging::install(level_filter(&max_level)?, sink, log_line)?;
    Ok(())
}

#[bridge(sync)]
pub fn uninstall_logging() {
    frustrate::logging::uninstall();
}

/// Records accepted and not delivered, process-wide and monotonic. Read twice
/// and subtract; a `u64` crosses as a `BigInt`.
#[bridge(sync)]
pub fn logging_dropped() -> u64 {
    frustrate::logging::dropped()
}

/// Log from the caller's own thread — the isolate's, on a sync member.
#[bridge(sync)]
pub fn emit_log(level: String, message: String) -> Result<()> {
    let level = level_filter(&level)?
        .to_level()
        .ok_or_else(|| anyhow::anyhow!("`off` is a filter, not a level to emit at"))?;
    log::log!(level, "{message}");
    Ok(())
}

/// Log from wherever an async body runs: a pool worker natively, the calling
/// thread on single-threaded web. Both must reach the same sink, which is the
/// point of a process-global logger over a per-call one.
#[bridge]
pub async fn emit_log_async(level: String, message: String) -> Result<()> {
    let level = level_filter(&level)?
        .to_level()
        .ok_or_else(|| anyhow::anyhow!("`off` is a filter, not a level to emit at"))?;
    log::log!(level, "{message}");
    Ok(())
}

/// A `Display` that logs while it is being formatted — the realistic way a
/// record gets logged from *inside* the logger, since the mapper calls
/// `record.args().to_string()`. The outer record must arrive and the inner one
/// must be counted as dropped, with no recursion and no hang.
struct LogsWhileFormatting;

impl std::fmt::Display for LogsWhileFormatting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        log::error!("re-entrant record, from inside a Display impl");
        f.write_str("outer record")
    }
}

#[bridge(sync)]
pub fn emit_reentrant_log() {
    log::info!("{LogsWhileFormatting}");
}

// ------------------------------------------------- consuming a handle --
//
// A Rust signature that takes `self`, or a handle by value, says the call
// consumes the object. The Dart caller says so too, with `take()`; these
// fixtures are one of each shape the bridge accepts.

/// Confined: nothing can be in flight (one isolate, sync calls), so taking is
/// unconditional.
///
/// The comparison traits are here so a `HashSet<Slip>`/`BTreeMap<Slip, _>`
/// parameter compiles: what a set or a map inbound collapses, it collapses on
/// the object, which is the bridged type's own `Hash`/`Ord` and nothing the
/// bridge supplies.
#[derive(Hash, PartialEq, Eq, PartialOrd, Ord)]
#[bridge(confined)]
pub struct Slip {
    entries: Vec<i64>,
}

#[bridge]
impl Slip {
    #[bridge(sync)]
    pub fn new() -> Self {
        Slip { entries: vec![] }
    }

    #[bridge(sync)]
    pub fn add(&mut self, n: i64) {
        self.entries.push(n);
    }

    #[bridge(sync)]
    pub fn count(&self) -> i64 {
        self.entries.len() as i64
    }

    /// The plain consuming receiver.
    #[bridge(sync)]
    pub fn into_total(self) -> i64 {
        self.entries.into_iter().sum()
    }

    /// The boxed one. Object-safe, and the only consuming receiver a trait
    /// could ever have — here on a concrete type, which is where the bridge
    /// supports it.
    #[bridge(sync)]
    pub fn into_first(self: Box<Self>) -> Option<i64> {
        self.entries.into_iter().next()
    }
}

/// Frozen: shared, so a take can find a call still running and says so.
#[derive(Hash, PartialEq, Eq)]
#[bridge(frozen)]
pub struct Tape {
    marks: Vec<String>,
}

#[bridge]
impl Tape {
    #[bridge(sync)]
    pub fn new(marks: Vec<String>) -> Self {
        Tape { marks }
    }

    /// Parks on Dart while holding an `Arc` clone of this tape — the portable
    /// way to have a call in flight when the suite tries to consume it. The
    /// same Rust → Dart → Rust shape `Counter::hold_write_asking` uses, and
    /// portable for the same reason: the body is an `async fn`, so it awaits
    /// the reply instead of parking a thread.
    pub async fn count_after(&self, f: DartFunction<i64, i64>) -> i64 {
        f.call_async(self.marks.len() as i64).await
    }

    #[bridge(sync)]
    pub fn into_count(self) -> i64 {
        self.marks.len() as i64
    }
}

/// Locked: shared and mutable. The consuming member is **sync and portable** —
/// it acquires no guard, so nothing is released through the lock's waiter
/// queue and no contention contract applies.
#[bridge(locked)]
pub struct Jar {
    coins: i64,
}

#[bridge]
impl Jar {
    #[bridge(sync)]
    pub fn new(coins: i64) -> Self {
        Jar { coins }
    }

    pub fn add(&mut self, n: i64) -> i64 {
        self.coins += n;
        self.coins
    }

    /// Holds the write guard across a Dart call, so the suite can have one in
    /// flight when it tries to consume. See [`Tape::count_after`].
    pub async fn add_after(&mut self, f: DartFunction<i64, i64>) -> i64 {
        self.coins += f.call_async(self.coins).await;
        self.coins
    }

    #[bridge(sync)]
    pub fn into_coins(self) -> i64 {
        self.coins
    }
}

/// Actor: the consuming call queues behind everything already sent, so it sees
/// their effects; the Dart side releases the executor after it.
#[bridge(actor)]
pub struct Kiln {
    fired: i64,
}

#[bridge]
impl Kiln {
    pub fn new() -> Self {
        Kiln { fired: 0 }
    }

    pub fn fire(&mut self, n: i64) -> i64 {
        self.fired += n;
        self.fired
    }

    pub fn into_fired(self) -> i64 {
        self.fired
    }
}

/// A handle by value, at every position the bridge accepts one.
#[bridge(sync)]
pub fn ledger_total(l: Slip) -> i64 {
    l.entries.into_iter().sum()
}

#[bridge(sync)]
pub fn ledger_total_opt(l: Option<Slip>) -> i64 {
    l.map(|l| l.entries.into_iter().sum()).unwrap_or(-1)
}

#[bridge(sync)]
pub fn ledger_total_all(ls: Vec<Slip>) -> i64 {
    ls.into_iter().flat_map(|l| l.entries).sum()
}

/// Two containers of one kind at one depth, in one member and in one struct.
/// The encoders name their locals by depth, so both halves of each pair land
/// at the same name in the same Dart scope unless each is emitted inside a
/// block of its own. Nothing else in the fixtures has this shape, and the
/// generated Dart did not compile without it — so what these prove is that the
/// Dart compiler sees it.
#[bridge(sync)]
pub fn slip_total_pair(a: Option<Slip>, b: Option<Slip>) -> i64 {
    let sum = |s: Option<Slip>| s.map(|s| s.entries.into_iter().sum()).unwrap_or(-1);
    sum(a) + sum(b)
}

#[bridge(data)]
pub struct TwoNotes {
    pub first: Option<String>,
    pub second: Option<String>,
}

#[bridge(sync)]
pub fn join_notes(n: TwoNotes) -> String {
    format!(
        "{}/{}",
        n.first.unwrap_or_default(),
        n.second.unwrap_or_default()
    )
}

/// A tuple element beside a value, and an `Option<Vec<..>>` nesting.
#[bridge(sync)]
pub fn ledger_total_tagged(t: (Slip, i64)) -> i64 {
    t.0.entries.into_iter().sum::<i64>() * t.1
}

#[bridge(sync)]
pub fn ledger_total_nested(ls: Option<Vec<Slip>>) -> i64 {
    ls.map(|v| v.into_iter().flat_map(|l| l.entries).sum())
        .unwrap_or(-1)
}

/// One handle consumed beside another borrowed: the pair the duplicate check
/// exists for.
#[bridge(sync)]
pub fn ledger_merge_into(keep: &mut Slip, gone: Slip) -> i64 {
    keep.entries.extend(gone.entries);
    keep.entries.len() as i64
}

/// A consumed **confined** handle on an async member. Borrowing one there is
/// FR0012; consuming one is not, because there is no owner left to race with —
/// the `Box` moves into the future under the model's own `Send` bound.
#[bridge]
pub async fn ledger_total_async(l: Slip) -> i64 {
    l.entries.into_iter().sum()
}

/// The frozen consume on the async arm, where the contended answer has to come
/// out of the future rather than the entry frame.
#[bridge]
pub async fn tape_count_async(t: Tape) -> i64 {
    t.marks.len() as i64
}

/// Two nestings that only the emitters can get wrong, so they exist to be
/// *compiled*: an `Option` inside an `Option` (whose Dart form is `FrOption`,
/// not a second nullable) and a tuple mixing two models (whose duplicate-check
/// entries must be built per handle, because only a `Box`-held object may
/// forgive a zero-sized type).
#[bridge(sync)]
pub fn slip_total_nested_opt(l: Option<Option<Slip>>) -> i64 {
    l.flatten()
        .map(|l| l.entries.into_iter().sum())
        .unwrap_or(-1)
}

#[bridge(sync)]
pub fn slip_and_tape(t: (Slip, Tape)) -> i64 {
    t.0.entries.len() as i64 + t.1.marks.len() as i64
}

// -------------------------------- one container that both lends and takes --
//
// The argument is built by walking the **taken** value, moving: each taken
// object moves straight in, and each lent position is acquired from an id
// gathered off the staged value before any of it was adopted. That works
// because a lent handle's reference points at the object, not into the
// container the walk consumes — which is also why a borrowed *value* in the
// same container stays refused (FR0078).

/// Confined, shared lend beside a take.
#[bridge(sync)]
pub fn ledger_slip_pairs(ps: Vec<(&Ledger, Slip)>) -> i64 {
    ps.into_iter()
        .map(|(l, s)| l.total + s.entries.iter().sum::<i64>())
        .sum()
}

/// A **mutable** lend beside a take of the *same* type: one object in both
/// positions is exactly the pair the duplicate check exists for, and without
/// it the take would free the object under the borrow.
#[bridge(sync)]
pub fn slip_absorb_pairs(ps: Vec<(&mut Slip, Slip)>) -> i64 {
    let mut n = 0;
    for (keep, gone) in ps {
        keep.entries.extend(gone.entries);
        n += keep.entries.len() as i64;
    }
    n
}

/// A map that lends its key and takes its value. The take leaves it **flat**:
/// building the real map before the key is acquired would collapse on the id
/// the key still is, which is the hazard the flat staging exists for.
#[bridge(sync)]
pub fn ledger_slip_map(ps: std::collections::HashMap<&Ledger, Slip>) -> i64 {
    ps.into_iter()
        .map(|(l, s)| l.total + s.entries.iter().sum::<i64>())
        .sum()
}

/// Locked on the pool arm: a guard per lent element, taken in handle-id order,
/// and a fallible take beside it.
#[bridge]
pub fn vault_jar_pairs(ps: Vec<(&mut Vault, Jar)>) -> i64 {
    let mut n = 0;
    for (v, j) in ps {
        v.balance += j.coins;
        n += v.balance;
    }
    n
}

// ------------------------------------------- a set or a map of handles, in --
//
// Both stage **flat** — a `Vec` of elements, a `Vec` of pairs — so every raw
// is adopted before the container is rebuilt. Staging the set itself would
// have collapsed on the *raw*, and every zero-sized object of one type shares
// one raw, so two distinct handles would have become one and the second `Box`
// (already given up by the caller) would have had nobody left to free it.
// Rebuilding afterwards collapses on the objects, which is what the Rust
// signature asked for.

/// A `Set<Consumed<Slip>>` on the Dart side: a set of tokens, with identity
/// equality, which is what a set of handles is there.
#[bridge(sync)]
pub fn slip_total_set(ss: std::collections::HashSet<Slip>) -> i64 {
    ss.into_iter().flat_map(|s| s.entries).sum()
}

/// A handle as a map **value**, ordered so the test can name what it gets.
#[bridge(sync)]
pub fn slip_total_by_name(ss: std::collections::BTreeMap<String, Slip>) -> String {
    ss.into_iter()
        .map(|(k, s)| format!("{k}={}", s.entries.into_iter().sum::<i64>()))
        .collect::<Vec<_>>()
        .join(",")
}

/// A handle as a map **key**, with a plain value beside it.
#[bridge(sync)]
pub fn slip_weighted(ss: std::collections::BTreeMap<Slip, i64>) -> i64 {
    ss.into_iter()
        .map(|(s, w)| s.entries.into_iter().sum::<i64>() * w)
        .sum()
}

/// A frozen element, so the fallible take runs inside the set's collect: one
/// element still in flight refuses the whole call, and every object already
/// adopted drops with the prelude's locals.
#[bridge(sync)]
pub fn tape_marks_set(ts: std::collections::HashSet<Tape>) -> i64 {
    ts.into_iter().map(|t| t.marks.len() as i64).sum()
}

/// **Lent** rather than taken: the container the body sees is a set of
/// references, built out of the same flat staging.
#[bridge(sync)]
pub fn ledgers_sum_set(ls: std::collections::HashSet<&Ledger>) -> i64 {
    ls.into_iter().map(|l| l.total).sum()
}

#[bridge(sync)]
pub fn ledgers_sum_by_name(ls: std::collections::BTreeMap<String, &Ledger>) -> String {
    ls.into_iter()
        .map(|(k, l)| format!("{k}={}", l.total))
        .collect::<Vec<_>>()
        .join(",")
}

/// A lent handle as a map key, mutably: the duplicate check runs over the
/// runtime count and names the entry it refuses.
#[bridge(sync)]
pub fn ledgers_bump_keys(ls: std::collections::HashMap<&mut Ledger, i64>) -> i64 {
    let mut n = 0;
    for (l, d) in ls {
        l.total += d;
        n += 1;
    }
    n
}

/// A member that both **locks** and **consumes** — the shape whose glue did
/// not compile, because the guard plan counted an acquisition that takes no
/// guard. The duplicate (`a.absorb(a.take())`) is refused by the check every
/// consuming member joins.
#[bridge]
impl Jar {
    pub async fn absorb(&mut self, other: Jar) -> i64 {
        self.coins += other.coins;
        self.coins
    }
}

// ------------------------------------------- an inbound data struct --
//
// `#[bridge(data, inbound)]` gives a data struct's Dart class the field types a
// *parameter* of each field's Rust type has, where a plain one gets the types a
// *return* has. So `Slip` is `Consumed<Slip>` here and `Vec<Slip>` is
// `List<Consumed<Slip>>`, while `label` is an ordinary `String`. Rust sees the
// plain owned fields.
//
// The direction falls out of the shape: the two differ exactly where a handle
// is reached, so the class cannot be returned, held in a returned struct, or
// carried by a variant of an enum — each FR0004, naming the shape.

/// Several handles plus data about them, going the other way: the inbound twin
/// of `Workspace`.
///
/// One consuming call spends every token the value holds, in field order, after
/// the whole request has encoded — the ordering rule a bare `Consumed<Slip>`
/// parameter already follows, unchanged by the struct around it.
#[bridge(data, inbound)]
pub struct Delivery {
    pub label: String,
    pub primary: Slip,
    pub rest: Vec<Slip>,
    pub optional: Option<Slip>,
}

#[bridge(sync)]
pub fn deliver(d: Delivery) -> String {
    let mut total: i64 = d.primary.entries.iter().sum();
    for s in &d.rest {
        total += s.entries.iter().sum::<i64>();
    }
    let opt = d.optional.map_or(-1, |s| s.entries.iter().sum());
    format!("{}={total},{opt}", d.label)
}

/// An inbound struct **inside** an inbound struct, and inside a `Vec`: the
/// nesting the declaration admits, and the one a plain struct may not hold.
#[bridge(data, inbound)]
pub struct Manifest {
    pub head: Delivery,
    pub tail: Vec<Delivery>,
}

#[bridge(sync)]
pub fn deliver_all(m: Manifest) -> String {
    let mut out = vec![deliver(m.head)];
    for d in m.tail {
        out.push(deliver(d));
    }
    out.join(";")
}

/// Both handle kinds in one declaration. On a *plain* data struct this is
/// refused — a Dart-object handle can only travel Dart → Rust and an opaque in
/// a return-shaped class only Rust → Dart, so no position could carry it — and
/// `inbound` is what makes them agree: a parameter-shaped class hands the
/// opaque over as a token and opens the channel in the same request.
#[bridge(data, inbound)]
pub struct Feed {
    pub source: Slip,
    pub out: StreamSink<i64>,
}

#[bridge(sync)]
pub fn drain_feed(feed: Feed) {
    for n in feed.source.entries {
        feed.out.add(n);
    }
    feed.out.close();
}

/// A **frozen** field, so the fallible take runs inside the struct's rebuild:
/// one field still in flight refuses the whole call, and every object already
/// adopted drops with the prelude's locals.
#[bridge(data, inbound)]
pub struct Reel {
    pub tape: Tape,
    pub note: String,
}

#[bridge(sync)]
pub fn wind_reel(reel: Reel) -> String {
    format!("{}:{}", reel.note, reel.tape.marks.len())
}

// -------------------------------------------------- generic data types --
//
// A `#[bridge(data)]` declaration may take type parameters. Codegen does not
// monomorphize the Dart side — Dart has generics, so ONE `class Page<T>` comes
// out and `Page<Item>` is a type rather than a name — but it does expand the
// **wire**: every fully-applied use in a signature becomes its own declaration
// with its own codecs, which is what lets the direction and handle rules see
// through a parameter (`Page<Doc>` is return-only, `Page<Item>` is not).
//
// The instantiation set is derived, never declared: a bridged signature is
// never generic (FR0056), so every use is fully applied, and the set is the
// closure of those uses through the templates' fields.

/// One element of a page. A plain data struct, so `Page<Item>` composes
/// through the ordinary struct field codec.
#[bridge(data)]
pub struct Item {
    pub id: i64,
    pub label: String,
}

/// The template. Instantiated below at a struct, at a container, at a
/// primitive and through another template.
#[bridge(data)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub total: i64,
}

/// A generic data **enum**: one `sealed class Either<L, R>` with variant
/// classes generic in the same parameters, so a Dart `switch` over it is still
/// exhaustive.
#[bridge(data)]
pub enum Either<L, R> {
    Left(L),
    Right(R),
}

/// A template whose own field is another template at the same parameter —
/// what makes the expansion a fixpoint rather than a single pass. `Wrapper<i64>`
/// pulls `Page<i64>` in even though no signature writes it.
#[bridge(data)]
pub struct Wrapper<T> {
    pub page: Page<T>,
    pub tag: String,
}

/// A parameter bound to a **fixed-width numeric**, which is the one binding
/// where the generic class and the codec disagree about representation: the
/// class declares `List<T>`, and `Vec<i32>` crosses on the typed-list fast
/// path. The bulk writers take `List<int>`/`List<double>` and test the
/// representation at runtime, so the fast path survives and this compiles.
#[bridge(sync)]
pub fn double_page(p: Page<i32>) -> Page<i32> {
    Page {
        items: p.items.iter().map(|x| x * 2).collect(),
        total: p.total * 2,
    }
}

/// The struct instantiation, returned so the Dart side decodes it.
#[bridge(sync)]
pub fn item_page(n: i64) -> Page<Item> {
    Page {
        items: (0..n)
            .map(|i| Item {
                id: i,
                label: format!("item{i}"),
            })
            .collect(),
        total: n,
    }
}

/// A container argument, which the identity has to render structurally —
/// `Page<Vec<String>>` is one instantiation and `Page<Vec<i64>>` another.
#[bridge(sync)]
pub fn flatten_page(p: Page<Vec<String>>) -> Vec<String> {
    p.items.into_iter().flatten().collect()
}

/// The enum, at two different argument pairs, so the expansion is proven to
/// key on the arguments and not on the template.
#[bridge(sync)]
pub fn either_sum(e: Either<i64, Item>) -> i64 {
    match e {
        Either::Left(n) => n,
        Either::Right(i) => i.id,
    }
}

#[bridge(sync)]
pub fn either_flip(e: Either<String, bool>) -> Either<bool, String> {
    match e {
        Either::Left(s) => Either::Right(s),
        Either::Right(b) => Either::Left(b),
    }
}

/// Two instantiations of `Wrapper` in one signature, each pulling its own
/// `Page` in behind it.
#[bridge(sync)]
pub fn rewrap(a: Wrapper<Item>) -> Wrapper<i64> {
    Wrapper {
        page: Page {
            items: a.page.items.iter().map(|i| i.id).collect(),
            total: a.page.total,
        },
        tag: a.tag,
    }
}

/// A generic that recurs at the **same** argument: the ordinary recursive
/// shape, and the one that terminates because the instantiation is registered
/// before its fields are expanded. Polymorphic recursion — the same template at
/// a *different* argument — is FR0075.
#[bridge(data)]
pub struct Chain<T> {
    pub value: T,
    /// `Self` in a generic declaration is the declaration **applied to its own
    /// parameters** — `Chain<T>`, not `Chain`, which is not a type — so the
    /// idiomatic recursive spelling is the one that works here too.
    pub next: Option<Box<Self>>,
}

/// A second instantiation of `Chain`, so the generic `impl` below lands on two
/// — and on two the Dart side tells apart, which is the condition a generic
/// impl has to meet (see the block).
#[bridge(sync)]
pub fn chain_words(c: Chain<String>) -> String {
    let mut out = c.value.clone();
    let mut cur = &c;
    while let Some(next) = &cur.next {
        out.push(' ');
        out.push_str(&next.value);
        cur = next;
    }
    out
}

/// A **generic** bridged impl: each member is generated onto every
/// instantiation of `Chain` the signatures derive — `Chain<i64>` and
/// `Chain<String>` — as its own `Function` with its own dispatch id, calling
/// `crate::api::Chain::<i64>::depth` and `crate::api::Chain::<String>::depth`.
/// On the Dart side each instantiation gets an `extension Chain$i64 on
/// Chain<int>` / `Chain$String on Chain<String>`, because Dart resolves an
/// extension member from the *static* type and that is where the dispatch id
/// can come from.
///
/// The `Clone` bound is read and dropped rather than refused: the bridge
/// cannot evaluate a trait bound, both instantiations satisfy it, and an
/// instantiation that did not would fail in rustc on the generated call.
#[bridge]
impl<T: Clone> Chain<T> {
    /// An ordinary borrowed receiver: the value is decoded out of the request
    /// like a parameter, exactly as it is on a non-generic data type.
    #[bridge(sync)]
    pub fn depth(&self) -> i64 {
        let mut n = 1;
        let mut cur = self;
        while let Some(next) = &cur.next {
            n += 1;
            cur = next;
        }
        n
    }

    /// A **parameter** bound: `int` at `Chain<i64>`, `String` at
    /// `Chain<String>`, one member written once.
    #[bridge(sync, getter)]
    pub fn head(&self) -> T {
        self.value.clone()
    }

    /// A `Self` return inside a generic block is the block's self type —
    /// `Chain<T>` — substituted per instantiation, so this lands as
    /// `Chain<int> grow(int value)` on one extension and
    /// `Chain<String> grow(String value)` on the other.
    #[bridge(sync)]
    pub fn grow(self, value: T) -> Self {
        Chain {
            value,
            next: Some(Box::new(self)),
        }
    }

    /// A **static**, which an extension carries too. Dart reaches it through
    /// the extension's own name: `Chain$i64.one(1)`.
    #[bridge(sync)]
    pub fn one(value: T) -> Self {
        Chain { value, next: None }
    }
}

/// A **concrete** bridged impl: its members go onto that one instantiation,
/// and the instantiation is added to the derived set by the block itself.
/// `Page` cannot take a generic impl — it is instantiated at both `i32` and
/// `i64`, which are one Dart type, and at `TextDoc`, whose receiver would be
/// decoded Dart to Rust and is FR0004 — so this is the shape it takes.
#[bridge]
impl Page<Item> {
    #[bridge(sync)]
    pub fn label_of(&self, i: i64) -> String {
        self.items
            .get(i as usize)
            .map(|it| it.label.clone())
            .unwrap_or_default()
    }

    /// A **by-value** receiver on a data type: the decoded value is moved into
    /// the body and the caller's Dart object is untouched, because it was never
    /// the same value.
    #[bridge(sync)]
    pub fn concat(self, other: Page<Item>) -> Self {
        let total = self.total + other.total;
        let mut items = self.items;
        items.extend(other.items);
        Page { items, total }
    }
}

#[bridge(sync)]
pub fn chain_len(c: Chain<i64>) -> i64 {
    let mut n = 1;
    let mut cur = &c;
    while let Some(next) = &cur.next {
        n += 1;
        cur = next;
    }
    n
}

/// A **handle** as a type argument. This instantiation reaches an opaque, so it
/// is return-only (FR0004) and gets an owned consuming encoder and no decoder —
/// while `Page<i64>` above, the same template, gets both. That is the whole
/// reason the expansion happens before the rules rather than inside them.
#[bridge(sync)]
pub fn doc_page(n: i64) -> Page<TextDoc> {
    Page {
        items: (0..n).map(|_| TextDoc::new()).collect(),
        total: n,
    }
}

/// A **generic typed error**. One `RefusalException<T>` comes out — named for
/// the template, because `Refusal<i64>Exception` is not a Dart identifier —
/// and every instantiation shares it, applied at the throw site so the caught
/// value's `error` field has the type the codec takes.
#[bridge(data)]
pub enum Refusal<T> {
    Busy(T),
    Gone,
}

#[bridge(sync)]
pub fn reserve_seats(n: i64) -> Result<i64, Refusal<i64>> {
    if n > 10 {
        Err(Refusal::Busy(n - 10))
    } else if n < 0 {
        Err(Refusal::Gone)
    } else {
        Ok(n)
    }
}

/// A second instantiation of the same error, which shares the one exception
/// class and does not mint a second.
#[bridge(sync)]
pub fn reserve_named(name: String) -> Result<String, Refusal<String>> {
    if name.is_empty() {
        Err(Refusal::Busy("anonymous".into()))
    } else {
        Ok(name)
    }
}

// ---------------------------------------------- a borrow inside a type --
//
// `Vec<&Doc>`, `Option<&Doc>`, `&[&Doc]`, `(&Doc, i64)` — several handles lent
// by one parameter, and the value borrows (`&str`, `&[u8]`, `&Point`) that ride
// the same machinery. How many handles a container carries is a runtime fact,
// so the duplicate check and the lock plan both work over a count the caller
// decides.

/// Confined, shared: two references to one object are ordinary Rust, so this
/// carries no duplicate check and one handle passed twice is legal.
#[bridge(sync)]
pub fn ledger_sum_all(ls: Vec<&Ledger>) -> i64 {
    ls.iter().map(|l| l.total).sum()
}

/// Confined and **mutable**: `&mut` per element, so one handle passed twice is
/// two exclusive references to one `Box` — undefined behaviour, not a hang.
/// Refused before any deref, naming the two elements.
#[bridge(sync)]
pub fn ledgers_bump_all(ls: Vec<&mut Ledger>, by: i64) -> i64 {
    let n = ls.len() as i64;
    for l in ls {
        l.total += by;
    }
    n
}

/// A lent container beside a single borrowed handle: both join one check.
#[bridge(sync)]
pub fn ledgers_bump_to(ls: Vec<&mut Ledger>, target: &Ledger) -> i64 {
    for l in ls {
        l.total = target.total;
    }
    target.total
}

/// A **consumed** handle beside a container lending more of the same type.
/// The Dart side has to compare the list's elements against the token before
/// anything is spent: without that, `f(a.take(), [a])` would give the object
/// up, meet Rust's refusal, and leave it with nobody left to free it.
#[bridge(sync)]
pub fn slip_merge_all(gone: Slip, keep: Vec<&mut Slip>) -> i64 {
    let n = gone.entries.len() as i64;
    for k in keep {
        k.entries.extend(gone.entries.iter().copied());
    }
    n
}

/// The `Option` and tuple spellings.
#[bridge(sync)]
pub fn ledger_or_zero(l: Option<&Ledger>) -> i64 {
    l.map(|l| l.total).unwrap_or(0)
}

#[bridge(sync)]
pub fn ledger_scaled(t: (&Ledger, i64)) -> i64 {
    t.0.total * t.1
}

/// Frozen on the **sync** arm, spelled as a slice: `frozen_ref` per element,
/// uncounted, because the Dart handles are alive for the whole call.
#[bridge(sync)]
pub fn tape_marks_all(ts: &[&Tape]) -> i64 {
    ts.iter().map(|t| t.marks.len() as i64).sum()
}

/// Frozen on the **pool** arm: an `Arc` clone per element, taken on the calling
/// thread while the Dart handles are still guaranteed alive.
#[bridge]
pub async fn tape_marks_all_async(ts: Vec<&Tape>) -> i64 {
    ts.iter().map(|t| t.marks.len() as i64).sum()
}

/// Locked: one guard per element, acquired in `handle::lock_plan` order over a
/// count the caller decides. The sibling below names the same objects in the
/// opposite list order, so a plan that followed wire order rather than handle
/// id could invert against it.
#[bridge]
pub async fn vaults_total(vs: Vec<&Vault>) -> i64 {
    vs.iter().map(|v| v.balance).sum()
}

#[bridge]
pub async fn vaults_total_reversed(vs: Vec<&Vault>) -> i64 {
    vs.iter().rev().map(|v| v.balance).sum()
}

#[bridge]
pub async fn vaults_bump_all(vs: Vec<&mut Vault>, by: i64) -> i64 {
    let n = vs.len() as i64;
    for v in vs {
        v.balance += by;
    }
    n
}

/// The **sync** arm of the same thing: the try-lock still returns `Contention`
/// from inside the planned loop, now over a count the caller decides.
#[bridge(sync, on_contention = "error")]
pub fn vaults_total_try(vs: Vec<&Vault>) -> i64 {
    vs.iter().map(|v| v.balance).sum()
}

/// A lent locked handle two containers deep, so the argument walk nests the
/// iterator that consumes the guards inside a second closure. Here to be
/// compiled; the shape is not otherwise reachable from a fixture.
#[bridge]
pub async fn vaults_total_maybe(vs: Vec<Option<&Vault>>) -> i64 {
    vs.into_iter().flatten().map(|v| v.balance).sum()
}

/// A locked **receiver** beside a lent locked container: one plan over both, so
/// the receiver's guard is taken in id order among the elements', and passing
/// the receiver again inside the list is the duplicate the plan refuses.
#[bridge]
impl Vault {
    pub async fn drain_into(&mut self, others: Vec<&mut Vault>) -> i64 {
        for o in others {
            self.balance += o.balance;
            o.balance = 0;
        }
        self.balance
    }
}

/// Value borrows: `&str`, `&[u8]` and a borrowed data struct, each read out of
/// the owned local the decode built.
#[bridge(sync)]
pub fn join_parts(parts: Vec<&str>, sep: &str) -> String {
    parts.join(sep)
}

#[bridge(sync)]
pub fn label_or(name: Option<&str>) -> String {
    name.unwrap_or("none").to_string()
}

#[bridge(sync)]
pub fn byte_len_or(b: Option<&[u8]>) -> i64 {
    b.map(|b| b.len() as i64).unwrap_or(-1)
}

#[bridge(sync)]
pub fn sum_x_borrowed(ps: Vec<&Point>) -> f64 {
    ps.iter().map(|p| p.x).sum()
}

/// A value borrow beside a lent handle in one container, and an `Option` under
/// a list — the two nestings only the emitters can get wrong.
#[bridge(sync)]
pub fn ledger_named(t: (&Ledger, &str)) -> String {
    format!("{}={}", t.1, t.0.total)
}

#[bridge(sync)]
pub fn ledger_sum_maybe(ls: Vec<Option<&Ledger>>) -> i64 {
    ls.into_iter().flatten().map(|l| l.total).sum()
}

/// An **owned** value beside a lent handle, inside a list. The argument is
/// built by walking the staged value by reference — every borrow in it points
/// there and it has to stay alive — so this element is the one shape that
/// cannot be moved out and is cloned instead. Here to compile that.
#[bridge(sync)]
pub fn ledgers_named_all(ls: Vec<(&Ledger, String)>) -> String {
    ls.into_iter()
        .map(|(l, n)| format!("{n}={}", l.total))
        .collect::<Vec<_>>()
        .join(",")
}

/// Nested borrows in the **return**: the values are copied into the response
/// through the references, which still point at the receiver's own data because
/// the encode runs in the same scope as the call.
#[bridge]
impl Tape {
    #[bridge(sync)]
    pub fn first_mark(&self) -> Option<&str> {
        self.marks.first().map(|s| s.as_str())
    }

    #[bridge(sync)]
    pub fn marks_ref(&self) -> Vec<&str> {
        self.marks.iter().map(|s| s.as_str()).collect()
    }
}

// ------------------------------------------------------ Resident: Scene --

/// A resident type holding an `Rc` — the shape `confined` cannot take.
///
/// This is the whole point of the model stated as a fixture: `Rc<RefCell<..>>`
/// is `!Send`, so `handle::confined_new` refuses it (its doctest says so), and
/// before `resident` the only home for it was `actor` — one OS thread per
/// instance and every member async. Here the members are sync, run on the
/// caller, and cost nothing but the call.
///
/// The graph is deliberately shared rather than a plain `Vec`: a second `Rc`
/// clone kept beside the object is what makes the type genuinely thread-affine
/// rather than merely un-annotated, so a build that lost the `!Send`-ness
/// would stop testing anything.
#[bridge(resident)]
pub struct Scene {
    nodes: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
    /// A second owner of the same allocation, held so the refcount this type
    /// mutates is never 1 — an `Rc` with one owner is indistinguishable from a
    /// `Box` at runtime, which would make the fixture pass for the wrong
    /// reason.
    also: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
}

#[bridge]
impl Scene {
    /// `sync` is not a choice here: FR0079 refuses a dispatched constructor on
    /// a resident type, because the value would be built on a pool worker.
    #[bridge(sync)]
    pub fn new() -> Self {
        let nodes = std::rc::Rc::new(std::cell::RefCell::new(vec![]));
        let also = std::rc::Rc::clone(&nodes);
        Scene { nodes, also }
    }

    #[bridge(sync)]
    pub fn add(&mut self, name: String) {
        self.nodes.borrow_mut().push(name);
    }

    #[bridge(sync)]
    pub fn count(&self) -> i64 {
        self.nodes.borrow().len() as i64
    }

    /// Two `Rc` owners of one allocation, read through the second handle.
    #[bridge(sync)]
    pub fn shared_count(&self) -> i64 {
        self.also.borrow().len() as i64
    }

    /// A consuming receiver on a resident handle: `resident_adopt` hands back
    /// the original `Box`, and the registry entry goes with it.
    #[bridge(sync)]
    pub fn into_names(self) -> Vec<String> {
        self.nodes.borrow().clone()
    }
}

/// A resident handle in a **parameter**, lent beside the receiver — the
/// aliasing shape `handle::alias_check` refuses when one side is `&mut`.
/// Resident is `Box`-held, so it participates exactly as confined does.
#[bridge(sync)]
pub fn scene_merge(into: &mut Scene, from: &Scene) -> i64 {
    let taken = from.nodes.borrow().clone();
    into.nodes.borrow_mut().extend(taken);
    into.nodes.borrow().len() as i64
}

/// What an isolate's exit left behind, by Rust type name and count. The Dart
/// suite reads this after killing an isolate that held a `Scene`: the object
/// itself is unreachable by construction, so the report is the only observable.
///
/// `native_only`, and `#[cfg]`-gated to match, because the registry it reads
/// is: web has neither isolates nor threads to migrate between, so there is
/// nothing there to report (see `frustrate::resident`). The declaration is
/// what makes the gate legal — FR0034 refuses a `#[cfg]`-gated bridged item
/// that does not say what the gate means.
#[cfg(not(target_family = "wasm"))]
#[bridge(sync, native_only)]
pub fn resident_leak_report() -> Vec<String> {
    frustrate::resident::leak_report()
        .into_iter()
        .map(|(name, n)| format!("{name}:{n}"))
        .collect()
}

/// Live resident objects in this process. `new` raises it, `dispose()` and the
/// GC finalizer lower it — which is how a Dart test asserts that a handle it
/// never disposed was reclaimed. `native_only` and `#[cfg]`-gated, like the
/// registry it reads.
#[cfg(not(target_family = "wasm"))]
#[bridge(sync, native_only)]
pub fn resident_live_count() -> i64 {
    frustrate::resident::live_count() as i64
}

// ------------------------------------- a handle crossing a channel out --
//
// A channel item may carry an opaque handle. The producer mints, the router's
// item handler builds the one Dart wrapper that disposes it — and where the
// item is *not* delivered, something has to give the object back. `Chit` is
// what makes that observable: nothing else can see it. `openChannelCount`
// counts channels, and a leak report names an abandoned channel, not an
// abandoned object.

/// `Chit` objects Rust still owns. Incremented by [`Chit::mint`], decremented
/// by `Drop` — so a reclaim that did not happen is a number that did not go
/// back to zero.
static CHITS_LIVE: AtomicI64 = AtomicI64::new(0);

#[bridge(confined)]
pub struct Chit {
    pub n: i64,
}

impl Chit {
    fn mint(n: i64) -> Chit {
        CHITS_LIVE.fetch_add(1, Ordering::SeqCst);
        Chit { n }
    }
}

impl Drop for Chit {
    fn drop(&mut self) {
        CHITS_LIVE.fetch_sub(1, Ordering::SeqCst);
    }
}

#[bridge]
impl Chit {
    #[bridge(sync)]
    pub fn n(&self) -> i64 {
        self.n
    }
}

/// How many `Chit` objects are alive. Zero after a run means every handle that
/// crossed — delivered or not — was freed exactly once.
#[bridge(sync)]
pub fn chits_live() -> i64 {
    CHITS_LIVE.load(Ordering::SeqCst)
}

/// A stream of handles. Sync, so the whole burst is posted before the call
/// returns: a Dart consumer that cancels before draining gets every item
/// through the router's absorb path, which is what makes the Dart-side reclaim
/// testable without racing a live producer.
#[bridge(sync)]
pub fn chits_to(n: i64, sink: StreamSink<Chit>) {
    for i in 0..n {
        if !sink.add(Chit::mint(i)) {
            break;
        }
    }
}

/// The same, into a fire-and-forget closure.
#[bridge(sync)]
pub fn chits_to_closure(n: i64, cb: DartCallback<Chit>) {
    for i in 0..n {
        cb.call(Chit::mint(i));
    }
}

/// A `Vec<Chit>` item: handles transfer element by element, exactly as a
/// returned vector does, and the reclaim walks the same length prefix.
#[bridge(sync)]
pub fn chit_batches(batches: i64, per: i64, sink: StreamSink<Vec<Chit>>) {
    for b in 0..batches {
        let batch: Vec<Chit> = (0..per).map(|i| Chit::mint(b * per + i)).collect();
        if !sink.add(batch) {
            break;
        }
    }
}

/// An `Option` item — the one container arm whose reclaim is not literally the
/// client decoder's, because the presence tag decides whether anything is
/// freed. Every second item is `None`.
#[bridge(sync)]
pub fn chits_or_none(n: i64, sink: StreamSink<Option<Chit>>) {
    for i in 0..n {
        let item = if i % 2 == 0 { Some(Chit::mint(i)) } else { None };
        if !sink.add(item) {
            break;
        }
    }
}

/// A handle inside a **declared** item type, which is what makes the generated
/// per-declaration reclaim run rather than the inline walk.
#[bridge(data)]
pub struct ChitNote {
    pub chit: Chit,
    pub note: String,
}

#[bridge(sync)]
pub fn chit_notes(n: i64, sink: StreamSink<ChitNote>) {
    for i in 0..n {
        let note = ChitNote {
            chit: Chit::mint(i),
            note: format!("note {i}"),
        };
        if !sink.add(note) {
            break;
        }
    }
}

/// A handle as a **returning** closure's argument. Portable (`call_async`), so
/// the web suite exercises it too. The Dart closure owns the `Chit` it is
/// handed and disposes it; the number it answers with is ordinary data.
#[bridge]
pub async fn weigh_chit(n: i64, f: DartFunction<Chit, i64>) -> i64 {
    f.call_async(Chit::mint(n)).await
}

/// A parked handle sink, for the dead-consumer pin — the same separation
/// [`park_sink`] makes, and for the same reason: the owning isolate is provably
/// gone before a single post is attempted, so the refusal is deterministic
/// rather than a race with a live producer.
static PARKED_CHIT_SINKS: Mutex<Vec<StreamSink<Chit>>> = Mutex::new(Vec::new());

#[bridge(sync)]
pub fn park_chit_sink(sink: StreamSink<Chit>) {
    PARKED_CHIT_SINKS.lock().unwrap().push(sink);
}

/// Push one freshly minted `Chit` to every parked sink; returns how many
/// accepted it. A refused post has already minted, so what this pins is that
/// the object comes back rather than outliving the isolate that would have
/// disposed it.
#[bridge(sync)]
pub fn push_parked_chit(value: i64) -> usize {
    let mut sinks = PARKED_CHIT_SINKS.lock().unwrap();
    let mut accepted = 0usize;
    sinks.retain(|s| {
        if s.add(Chit::mint(value)) {
            accepted += 1;
            true
        } else {
            false
        }
    });
    accepted
}

/// Drop every parked `Chit` sink, so a suite that parked one leaves no open
/// registration behind.
#[bridge(sync)]
pub fn clear_parked_chit_sinks() -> usize {
    let mut sinks = PARKED_CHIT_SINKS.lock().unwrap();
    let n = sinks.len();
    sinks.clear();
    n
}

// ------------------------------------ a handle riding a reply nobody takes --
//
// A dispatched member's answer is *posted*, not returned in the caller's
// frame, so the isolate that asked for it can be gone by the time it arrives.
// The handle in that answer was minted while encoding, which is before the
// post, so a refusal that did nothing else would leave the object alive with
// nothing left that could free it. `Chit` is again what makes it observable.

/// Calls parked in [`gated_chit`], waiting for the gate.
static CHIT_GATE_PARKED: AtomicI64 = AtomicI64::new(0);
/// Set by [`open_chit_gate`]; lets every parked call through.
static CHIT_GATE_OPEN: AtomicBool = AtomicBool::new(false);

/// Park until [`open_chit_gate`], then answer with a freshly minted `Chit`.
///
/// The gate is what makes the refusal deterministic rather than a race: a
/// test can watch [`chit_gate_parked`] to know the call is in flight and has
/// minted *nothing* yet, kill the isolate that made it, wait for the exit
/// notice, and only then let the body finish. So the mint provably happens
/// after the consumer is provably gone.
///
/// Async — the default — because that is the whole point: a `#[bridge(sync)]`
/// answer is returned in the caller's frame and cannot be refused.
///
/// Bounded like [`pool_rendezvous`], and for the same reason: a pool thread
/// that never returns would outlive the test and take the rest of the suite's
/// pool calls with it.
#[bridge]
pub fn gated_chit(n: i64, max_wait_ms: i64) -> Chit {
    CHIT_GATE_PARKED.fetch_add(1, Ordering::SeqCst);
    for _ in 0..max_wait_ms {
        if CHIT_GATE_OPEN.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    // Mint *before* clearing the counter, so a watcher that sees it reach zero
    // knows the object existed. Cleared after, and the test can then only see
    // the count return to its baseline because something freed it — which,
    // with the consumer gone, is the reply's reclaim and nothing else.
    let chit = Chit::mint(n);
    CHIT_GATE_PARKED.fetch_sub(1, Ordering::SeqCst);
    chit
}

/// How many calls are parked in [`gated_chit`].
#[bridge(sync)]
pub fn chit_gate_parked() -> i64 {
    CHIT_GATE_PARKED.load(Ordering::SeqCst)
}

/// Let every parked [`gated_chit`] through. Latched: the gate never shuts
/// again, so a second call after it opens simply does not park.
#[bridge(sync)]
pub fn open_chit_gate() {
    CHIT_GATE_OPEN.store(true, Ordering::SeqCst);
}
