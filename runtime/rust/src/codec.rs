//! The frustrate wire codec.
//!
//! One codec for every platform: little-endian fixed-width scalars, u64
//! lengths, UTF-8 strings. The Dart implementation (package:frustrate
//! binary_codec.dart) mirrors this exactly; cross-language golden tests pin
//! the format.
//!
//! Readers panic on malformed input. Generated Rust and Dart are produced
//! from the same checked interface, so a malformed buffer is a bridge bug,
//! not a user error; panics are caught at the dispatch boundary and surface
//! as attributable bridge-internal errors.

/// User-supplied byte codec for a bridge-external type
/// (`#[bridge(bytes(...))]`): the type crosses
/// the bridge as a length-prefixed byte payload, converted by this trait on
/// the Rust side and by the declared Dart methods on the other. The
/// protobuf pattern: implement with `Message::encode_to_vec` /
/// `Message::decode`.
///
/// `from_bytes` panics on malformed input; the panic crosses as the call's
/// attributable panic envelope (loud, never silent) — include context.
pub trait BytesCodec: Sized {
    fn to_bytes(&self) -> Vec<u8>;
    fn from_bytes(bytes: &[u8]) -> Self;
}

#[derive(Default)]
pub struct ByteWriter {
    buf: Vec<u8>,
    /// The handles this payload has **minted**, and how to free each — see
    /// [`Minted`]. Kept by every writer, because whether a payload can be
    /// turned away is a property of where it is *sent* and not of how it is
    /// built: the same generated line builds the reply of a synchronous call,
    /// which is answered in the caller's frame, and of a dispatched one, which
    /// is posted into an isolate that may already be gone. An encode that mints
    /// nothing pays nothing — `Vec::new` does not allocate.
    minted: Minted,
}

/// One handle an owned encode registered, paired with the fn that frees it.
///
/// The drop fn is the opaque's own `handle::*_drop::<T>` — the very function
/// the generated `frustrate_drop_<T>` export calls — so a reclaim and a
/// `dispose()` free the object by the same route.
pub type Mint = (u64, unsafe fn(u64));

/// Every handle one framed payload minted, in the order the encoder wrote them.
///
/// **Recorded at the mint, never re-derived from the bytes.** An encoded
/// payload that cannot be delivered has already registered its objects, and the
/// Dart wrapper that would dispose them is never built — so something has to
/// free them. Reading them back out of the payload would mean a second walk of
/// the layout, and a walk that drifted from the encoder while consuming the
/// same number of bytes would hand `Box::from_raw` an address that was never a
/// handle. This cannot drift: it *is* what the encoder minted.
///
/// **Every ledger ends in exactly one of [`reclaim`](Self::reclaim) or
/// [`delivered`](Self::delivered)**, and the [`Drop`] below is what says so.
/// That `Drop` frees nothing — freeing there would have to be disarmed on every
/// successful path, and a disarm forgotten is a free of objects Dart holds,
/// where a `reclaim` forgotten is a leak; the asymmetry decides it. It only
/// *asserts*, in debug builds, so a path that drops a payload without answering
/// the question fails a test run instead of leaking silently in production.
///
/// The one path that still leaks is an **encoder that panics after minting** (a
/// user `bytes(...)` codec failing on a later field): the unwind carries the
/// writer past the discharge. That panic crosses as the enclosing call's panic
/// envelope, so it is loud and attributable — what it costs is the handles that
/// payload had already registered. `thread::panicking` is why the assert stays
/// quiet there: a panic in a `Drop` during an unwind is a process abort, which
/// would turn a legible leak into one.
#[derive(Default)]
pub struct Minted(Vec<Mint>);

impl Drop for Minted {
    fn drop(&mut self) {
        debug_assert!(
            self.0.is_empty() || std::thread::panicking(),
            "frustrate: a payload that minted {} handle(s) was dropped without \
             being delivered or reclaimed, so those objects can never be freed \
             — every Minted must end in `reclaim()` or `delivered()`",
            self.0.len()
        );
    }
}

impl Minted {
    /// Free every handle the payload minted, in encode order. For a payload
    /// that is known **not** to have been delivered.
    ///
    /// Safe: every entry got here through the `unsafe`
    /// [`ByteWriter::write_minted`], which is where the obligation is
    /// discharged. What is left for a caller to get right is *when* — and that
    /// is a correctness question, not a soundness one: reclaiming a delivered
    /// payload would free an object Dart also holds. That is why the split is
    /// made where delivery is *known* ([`crate::post::deliver`]) rather than at
    /// the sites that hand it a payload.
    pub fn reclaim(mut self) {
        for (h, drop_fn) in std::mem::take(&mut self.0) {
            unsafe { drop_fn(h) };
        }
    }

    /// Give up the ledger without freeing: the payload reached the far side,
    /// and the Dart wrapper built from it owns these objects now.
    pub fn delivered(mut self) {
        self.0.clear();
    }

    /// How many handles were minted. Zero for a payload that carries none.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// A [`ByteWriter`] whose frame bytes are **already written**, so the body
/// encodes straight into its final wire position and nothing re-copies it to
/// make room for a prefix.
///
/// Every framed buffer on this wire is `[status]` or `[status, selector]`
/// followed by a payload (envelope.rs). Building the payload first and
/// prepending afterwards costs one allocation plus a full copy of the payload,
/// which scales with the payload and is a real fraction of a large native
/// round trip (`tests/dart_integration/tool/native_bench.dart` section B is
/// the instrument).
///
/// **Why a type and not a `ByteWriter::with_prefix(n)`.** `STATUS_OK == 0`, so
/// a reserved-but-unstamped leading byte does not decode as a corrupt frame —
/// it decodes as a *valid-looking OK envelope*, silently, with every field
/// shifted. Any reserve-then-stamp API has a window in which that state exists.
/// This has no such window: the frame is written by the constructor, and
/// [`Outcome::Ok`](crate::envelope::Outcome::Ok) takes this type rather than a
/// `ByteWriter`, so an unframed writer cannot become a response at all.
///
/// Not *unrepresentable* — `DerefMut` plus `std::mem::take` would swap in a
/// `Default` writer and reach it — but unreachable from any shape codegen
/// emits, which is the honest claim and the one worth relying on.
///
/// Generated encoders and user codec hooks keep taking `&mut ByteWriter`: deref coercion covers `enc_Point(&mut w, v)` and
/// `w.write_i32(..)` alike, while `ByteWriter::take` takes `self` by value and
/// so is uncallable through `Deref` — which is the half that matters.
pub struct FramedWriter {
    inner: ByteWriter,
}

impl FramedWriter {
    /// Room the frame is allocated with, so the first payload write does not
    /// immediately realloc.
    ///
    /// `vec![status]` would be capacity **1**, which is worse than the bare
    /// `ByteWriter` it replaces: that one started empty and let the first
    /// write allocate a right-sized buffer, where a capacity-1 buffer
    /// guarantees a grow-and-copy on the very first scalar. Measured, that
    /// mistake cost **+6.8% on `noArgsNoRet` and +5.5% on `addI32`** — small,
    /// but on the overhead floor, which is the number a UI app making many
    /// small calls actually feels.
    ///
    /// 32 covers a scalar response outright and costs one small allocation
    /// that a void return would have made anyway. Anything larger grows from
    /// here exactly as a `Vec` always did.
    const FRAME_CAPACITY: usize = 32;

    fn with_frame(frame: &[u8]) -> Self {
        let mut buf = Vec::with_capacity(Self::FRAME_CAPACITY);
        buf.extend_from_slice(frame);
        Self {
            inner: ByteWriter {
                buf,
                minted: Minted::default(),
            },
        }
    }

    /// The framed buffer **and** its mint ledger — the only way out, so a
    /// payload cannot become bytes without someone taking on the question the
    /// ledger asks. The caller then either hands the bytes over and calls
    /// [`Minted::delivered`], or [`Minted::reclaim`]s.
    pub fn into_parts(self) -> (Vec<u8>, Minted) {
        // `mem::take` rather than moving the fields out: `Minted` has a `Drop`,
        // so `self.inner` cannot be destructured.
        let mut inner = self.inner;
        let buf = std::mem::take(&mut inner.buf);
        let minted = std::mem::take(&mut inner.minted);
        (buf, minted)
    }

    /// A one-byte frame: `[status]`. The response shape.
    ///
    /// `pub` with an arbitrary status on purpose — `string_envelope` builds
    /// error, panic and contention envelopes through this same door, and a
    /// test needs to be able to hand an `Outcome::Ok` a non-OK status to prove
    /// the byte is read from where the writer put it.
    pub fn status(status: u8) -> Self {
        Self::with_frame(&[status])
    }

    /// A two-byte frame: `[status, selector]`. Stream items and callback
    /// invocations, which name a mirror method beside their status.
    pub fn event(status: u8, selector: u8) -> Self {
        Self::with_frame(&[status, selector])
    }

}

/// The two doors a test needs and production does not. Every shipped payload
/// is built by writing into the frame this type stamps and ends at a transport
/// that discharges its ledger; a test wants to hand a transport bytes it chose,
/// and to read back a reply it built. `cfg(test)`, so neither exists in a
/// shipped build.
#[cfg(test)]
impl FramedWriter {
    /// A writer over bytes a test already has.
    pub(crate) fn from_bytes(bytes: Vec<u8>) -> Self {
        Self {
            inner: ByteWriter {
                buf: bytes,
                minted: Minted::default(),
            },
        }
    }

    /// The bytes, with the ledger discharged as delivered — for a test that
    /// inspects a reply instead of posting it.
    pub(crate) fn delivered_bytes(self) -> Vec<u8> {
        let (bytes, minted) = self.into_parts();
        minted.delivered();
        bytes
    }
}

impl std::ops::Deref for FramedWriter {
    type Target = ByteWriter;
    fn deref(&self) -> &ByteWriter {
        &self.inner
    }
}

impl std::ops::DerefMut for FramedWriter {
    fn deref_mut(&mut self) -> &mut ByteWriter {
        &mut self.inner
    }
}

impl ByteWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// The buffer, for a writer that is not a framed reply — a request encode,
    /// which travels Dart-ward from nothing and so mints nothing.
    ///
    /// The assert names that precondition where it would be broken. Without it
    /// the [`Minted`] left behind fires the generic ledger message instead,
    /// which points at delivery rather than at the door that has none: this
    /// writer is not going anywhere that could refuse it, so there is nobody to
    /// discharge the ledger and a payload that minted has no business here.
    pub fn take(mut self) -> Vec<u8> {
        debug_assert!(
            self.minted.is_empty(),
            "frustrate: a bare ByteWriter minted handles — only a framed reply \
             or a channel event has a transport to discharge that ledger"
        );
        std::mem::take(&mut self.buf)
    }

    pub fn write_bool(&mut self, v: bool) {
        self.buf.push(v as u8);
    }
    pub fn write_u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    pub fn write_i8(&mut self, v: i8) {
        self.buf.push(v as u8);
    }
    pub fn write_u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn write_i16(&mut self, v: i16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn write_u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn write_i32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn write_i64(&mut self, v: i64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    /// u64 crosses as 8 LE bytes; the Dart side carries it as BigInt.
    pub fn write_u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    /// i128/u128 cross as 16 LE bytes; the Dart side carries them as BigInt on
    /// the same big-integer codec as u64, extended to two u64 halves.
    pub fn write_i128(&mut self, v: i128) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn write_u128(&mut self, v: u128) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn write_f32(&mut self, v: f32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn write_f64(&mut self, v: f64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    /// usize crosses as i64 (Dart int is 64-bit signed).
    pub fn write_usize(&mut self, v: usize) {
        self.write_i64(i64::try_from(v).expect("usize exceeds i64 range"));
    }
    pub fn write_isize(&mut self, v: isize) {
        self.write_i64(v as i64);
    }
    pub fn write_len(&mut self, v: usize) {
        self.write_usize(v);
    }
    pub fn write_string(&mut self, v: &str) {
        self.write_len(v.len());
        self.buf.extend_from_slice(v.as_bytes());
    }
    /// A `char` crosses as its `u32` Unicode scalar value (same wire form as
    /// `write_u32`); the Dart side carries it as a one-character String.
    pub fn write_char(&mut self, v: char) {
        self.write_u32(v as u32);
    }
    pub fn write_bytes(&mut self, v: &[u8]) {
        self.write_len(v.len());
        self.buf.extend_from_slice(v);
    }
    pub fn write_byte_array(&mut self, v: &[u8]) {
        self.buf.extend_from_slice(v);
    }
    pub fn write_handle(&mut self, v: u64) {
        self.write_u64(v);
    }
    /// Write a handle this encode **minted**, recording how to free it if the
    /// payload is never delivered. Generated owned encoders call this wherever
    /// they register an object.
    ///
    /// # Safety
    /// `drop_fn` must be the `handle::*_drop::<T>` matching the `handle::*_new`
    /// that produced `v` — the same pairing `frustrate_drop_<T>` makes.
    /// [`Minted::reclaim`] calls it on `v`, so a mismatched pair is a
    /// wrong-type free and an invented `v` is a free of an arbitrary address.
    /// `unsafe` for that reason and not the write: recording the pair is what
    /// carries the obligation, which is why `reclaim` can stay safe.
    pub unsafe fn write_minted(&mut self, v: u64, drop_fn: unsafe fn(u64)) {
        self.write_handle(v);
        self.minted.0.push((v, drop_fn));
    }
}

// ------------------------------------------------- typed numeric lists --

/// The bulk slice codec for fixed-width numeric lists.
///
/// `Vec<f64>` used to encode one element at a time — n calls to
/// `write_f64`, each an `extend_from_slice` of an 8-byte array — and decode
/// one at a time into a `push` loop. On a little-endian target the elements
/// of a `[f64]` are *already* the wire's bytes in the wire's order, so the
/// whole payload is one `memcpy` in either direction.
///
/// **The length prefix is the caller's**, unlike `write_bytes`/`write_string`
/// which write their own. That split is what lets a `VecDeque` — one length,
/// two slices (`as_slices`) — use the same writer: generated code emits
/// `write_len(v.len())` once and then feeds both halves in. The Dart side
/// keeps the identical shape (`w.writeLen(xs.length); w.writeF64List(xs);`)
/// so the two seams read the same. Do not "unify" these with `write_bytes`.
///
/// **Endianness.** `cfg!(target_endian = ...)` is a compile-time constant, so
/// exactly one arm survives codegen. The big-endian arm is the element loop
/// that shipped — kept so a big-endian target produces slow numbers rather
/// than wrong ones.
///
/// Every type here is a fixed-width numeric with no padding and no invalid bit
/// patterns, which is what makes the byte reinterpretation sound; that is the
/// argument each `SAFETY` block below refers back to.
macro_rules! bulk_numeric_codec {
    ($(($ty:ty, $write_one:ident, $read_one:ident, $write_slice:ident, $read_vec:ident),)*) => {
        impl ByteWriter {
            $(
                #[doc = concat!("Bulk-write the elements of a `[", stringify!($ty), "]`.")]
                ///
                /// Elements only — the caller writes the length prefix. See
                /// the macro's docs for why.
                pub fn $write_slice(&mut self, v: &[$ty]) {
                    if cfg!(target_endian = "little") {
                        // SAFETY: the source is a live slice of `$ty`, a
                        // fixed-width numeric with no padding, so
                        // `size_of_val(v)` bytes starting at `v.as_ptr()` are
                        // initialized and readable for the lifetime of the
                        // borrow. `u8` has alignment 1, so the cast cannot
                        // produce a misaligned pointer, and the resulting
                        // slice is only read.
                        let bytes: &[u8] = unsafe {
                            core::slice::from_raw_parts(
                                v.as_ptr().cast::<u8>(),
                                core::mem::size_of_val(v),
                            )
                        };
                        self.buf.extend_from_slice(bytes);
                    } else {
                        for x in v {
                            self.$write_one(*x);
                        }
                    }
                }
            )*
        }

        impl ByteReader<'_> {
            $(
                #[doc = concat!("Bulk-read `n` elements into a `Vec<", stringify!($ty), ">`.")]
                ///
                /// Elements only — the caller reads the length prefix, so a
                /// `VecDeque` return can wrap the result without a second
                /// wire shape.
                pub fn $read_vec(&mut self, n: usize) -> Vec<$ty> {
                    const W: usize = core::mem::size_of::<$ty>();
                    // Checked, and checked *before* anything is allocated: a
                    // corrupt element count times the width wraps on a 32-bit
                    // target (wasm32 is one), and a wrapped byte count would
                    // size the copy from a slice shorter than the destination.
                    let bytes = n
                        .checked_mul(W)
                        .expect("frustrate codec: list length overflows the byte count");
                    let src = self.chunk(bytes);
                    if cfg!(target_endian = "little") {
                        let mut out: Vec<$ty> = Vec::with_capacity(n);
                        // SAFETY: `out` was just allocated with room for `n`
                        // elements, i.e. exactly `bytes` bytes, and a fresh
                        // allocation cannot overlap `src`. `$ty` is a
                        // fixed-width numeric with no padding and no invalid
                        // bit patterns, so every byte pattern names a value
                        // and all `n` elements are initialized by the copy —
                        // which is what makes `set_len` sound. The copy is
                        // byte-wise, so the unaligned `src` is fine.
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                src.as_ptr(),
                                out.as_mut_ptr().cast::<u8>(),
                                bytes,
                            );
                            out.set_len(n);
                        }
                        out
                    } else {
                        let mut r = ByteReader::new(src);
                        (0..n).map(|_| r.$read_one()).collect()
                    }
                }
            )*
        }
    };
}

bulk_numeric_codec![
    (i8, write_i8, read_i8, write_i8_slice, read_i8_vec),
    (u8, write_u8, read_u8, write_u8_slice, read_u8_vec),
    (i16, write_i16, read_i16, write_i16_slice, read_i16_vec),
    (u16, write_u16, read_u16, write_u16_slice, read_u16_vec),
    (i32, write_i32, read_i32, write_i32_slice, read_i32_vec),
    (u32, write_u32, read_u32, write_u32_slice, read_u32_vec),
    // `i64` is bulk-coded on the Dart side too, but only on the VM: its byte
    // copy is the element loop's value exactly where `int` is a real 64-bit
    // integer, so `bulkI64Ok` (runtime/dart/lib/src/binary_codec.dart) keeps
    // dart2js on the loop, where `jsWriteI64`/`jsReadI64` throw past 2^53
    // rather than truncating.
    (i64, write_i64, read_i64, write_i64_slice, read_i64_vec),
    // `u64` is bulk-coded here and nowhere on the Dart side: it crosses as a
    // `BigInt`, which has no typed list to copy into. The seams are
    // independent — the wire is identical either way — so each takes the
    // widths it can.
    (u64, write_u64, read_u64, write_u64_slice, read_u64_vec),
    (f32, write_f32, read_f32, write_f32_slice, read_f32_vec),
    (f64, write_f64, read_f64, write_f64_slice, read_f64_vec),
];

pub struct ByteReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> ByteReader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn is_at_end(&self) -> bool {
        self.pos == self.buf.len()
    }

    /// Assert the buffer is fully consumed — a loud, attributable codec error
    /// otherwise. The mirror of Dart's `BinaryReader.assertConsumed`, which
    /// generated Dart already calls after every response decode; generated
    /// Rust calls this after every *request* decode, so trailing bytes are
    /// rejected rather than silently ignored. A request is exactly one
    /// function's encoded arguments, so a
    /// matched build always reaches the end.
    ///
    /// A panic, not an `Outcome::Error`, for two reasons. It is what every
    /// other codec error here does (see the module docs: readers panic, and
    /// the dispatch boundary turns that into an attributable panic envelope),
    /// and `Outcome::Error` is the *user*-error status — a trailing-byte
    /// request is a bridge bug, and reporting it as a `BridgeException` would
    /// misfile it as an application error. It is also not expressible at every
    /// site: the generated async `spawn_*` returns `()`.
    ///
    /// On wasm (panic = abort) this traps, which poisons the instance rather
    /// than failing one call. That is exactly what a truncated buffer already
    /// does there, and the condition is unreachable on a matched build.
    pub fn assert_consumed(&self) {
        if self.pos != self.buf.len() {
            panic!(
                "frustrate codec: {} trailing byte(s) after decoded value \
                 (buffer not fully consumed)",
                self.buf.len() - self.pos
            );
        }
    }

    fn chunk(&mut self, n: usize) -> &'a [u8] {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&e| e <= self.buf.len())
            .expect("frustrate codec: truncated buffer");
        let out = &self.buf[self.pos..end];
        self.pos = end;
        out
    }

    pub fn read_bool(&mut self) -> bool {
        match self.chunk(1)[0] {
            0 => false,
            1 => true,
            other => panic!("frustrate codec: invalid bool byte {other}"),
        }
    }
    pub fn read_u8(&mut self) -> u8 {
        self.chunk(1)[0]
    }
    pub fn read_i8(&mut self) -> i8 {
        self.chunk(1)[0] as i8
    }
    pub fn read_u16(&mut self) -> u16 {
        u16::from_le_bytes(self.chunk(2).try_into().unwrap())
    }
    pub fn read_i16(&mut self) -> i16 {
        i16::from_le_bytes(self.chunk(2).try_into().unwrap())
    }
    pub fn read_u32(&mut self) -> u32 {
        u32::from_le_bytes(self.chunk(4).try_into().unwrap())
    }
    pub fn read_i32(&mut self) -> i32 {
        i32::from_le_bytes(self.chunk(4).try_into().unwrap())
    }
    pub fn read_i64(&mut self) -> i64 {
        i64::from_le_bytes(self.chunk(8).try_into().unwrap())
    }
    pub fn read_u64(&mut self) -> u64 {
        u64::from_le_bytes(self.chunk(8).try_into().unwrap())
    }
    /// See [`ByteWriter::write_i128`]: 16 LE bytes.
    pub fn read_i128(&mut self) -> i128 {
        i128::from_le_bytes(self.chunk(16).try_into().unwrap())
    }
    pub fn read_u128(&mut self) -> u128 {
        u128::from_le_bytes(self.chunk(16).try_into().unwrap())
    }
    pub fn read_f32(&mut self) -> f32 {
        f32::from_le_bytes(self.chunk(4).try_into().unwrap())
    }
    pub fn read_f64(&mut self) -> f64 {
        f64::from_le_bytes(self.chunk(8).try_into().unwrap())
    }
    pub fn read_usize(&mut self) -> usize {
        usize::try_from(self.read_i64()).expect("frustrate codec: negative usize")
    }
    pub fn read_isize(&mut self) -> isize {
        self.read_i64() as isize
    }
    pub fn read_len(&mut self) -> usize {
        self.read_usize()
    }

    /// Bytes left unread.
    ///
    /// The ceiling on a length prefix nothing has validated yet. Every element
    /// of every sequence on this wire occupies at least one byte, so a count
    /// above this cannot be honoured whatever it claims, and a decoder can
    /// size its container by the smaller of the two without trusting the
    /// count. On a well-formed buffer the count is always the smaller one, so
    /// clamping changes nothing; on a corrupt one it is the difference between
    /// a reservation the allocator refuses — outside the reach of
    /// `catch_unwind`, and naming neither this crate nor the call — and the
    /// loud, attributable `chunk` failure the reads that follow produce.
    ///
    /// Named on the reader rather than passed in, because only the reader
    /// knows: generated code sees a `&mut ByteReader` and the count, and
    /// nothing else about the request.
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }
    pub fn read_string(&mut self) -> String {
        let len = self.read_len();
        String::from_utf8(self.chunk(len).to_vec()).expect("frustrate codec: invalid UTF-8")
    }
    /// Decode a `char` from its `u32` codepoint. Rejects a non-scalar value
    /// (a lone surrogate or a codepoint above U+10FFFF) with a clean,
    /// attributable codec panic — never an unattributable `unwrap` — mirroring
    /// the loud UTF-8 check in `read_string`. A correctly generated peer only
    /// ever sends a valid scalar, so this fires on a corrupt/mismatched buffer.
    pub fn read_char(&mut self) -> char {
        let cp = self.read_u32();
        char::from_u32(cp)
            .unwrap_or_else(|| panic!("frustrate codec: invalid Unicode scalar value U+{cp:04X}"))
    }
    pub fn read_bytes(&mut self) -> Vec<u8> {
        let len = self.read_len();
        self.chunk(len).to_vec()
    }

    // ------------------------------------------------- borrowed decoders --
    //
    // `chunk` already returns `&'a [u8]` — a slice of the request buffer, with
    // the reader's own lifetime. `read_bytes`/`read_string` throw that away
    // with a `to_vec`, which on a 1 MiB argument is a full payload copy and an
    // allocation nobody asked for. These two keep it.
    //
    // They are emitted ONLY for sync arms with an unsized borrow parameter
    // (`&[u8]`, `&str`), and the emit is keyed on the arm kind rather than on
    // the parameter, because where the borrow is safe is a property of where
    // the decode runs:
    //
    //   * sync — the borrow ends when the generated body returns, and the
    //     response is written into the transport's block only after
    //     `envelope::run` has returned, into a region disjoint from the
    //     request by construction (`envelope.rs`, `respond_out`);
    //   * pool  — the decode runs on the caller thread inside the request
    //     lease but the values are MOVED into a `'static` closure that runs
    //     later, on another thread, after the request buffer is gone;
    //   * actor — owned today for the same reason, until that arm is
    //     restructured to decode-then-move like the pool arms.
    //
    // The other half of the safety argument is in the check tier: a named
    // lifetime on a borrowed parameter is rejected (FR0044), because
    // `request_slice` produces a `&'a [u8]` with an unbound `'a` and a user
    // writing `&'static [u8]` would otherwise unify with it and compile a
    // dangling reference.

    /// Borrow the byte payload out of the request buffer — no copy, no
    /// allocation. See the block comment above for where this is safe to emit.
    pub fn read_bytes_borrowed(&mut self) -> &'a [u8] {
        let len = self.read_len();
        self.chunk(len)
    }

    /// The `&str` twin of [`ByteReader::read_bytes_borrowed`], with the same
    /// loud, attributable UTF-8 check `read_string` carries — a corrupt or
    /// mismatched buffer must never reach a user body as a silently-lossy
    /// value or an unattributable `unwrap`.
    pub fn read_str_borrowed(&mut self) -> &'a str {
        let len = self.read_len();
        std::str::from_utf8(self.chunk(len)).expect("frustrate codec: invalid UTF-8")
    }
    pub fn read_byte_array<const N: usize>(&mut self) -> [u8; N] {
        self.chunk(N).try_into().unwrap()
    }
    pub fn read_handle(&mut self) -> u64 {
        self.read_u64()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `envelope::encode` used to do: build the payload, then allocate
    /// `len + prefix` and copy it in to make room. Kept here as the golden
    /// reference so the framed writer is checked against the bytes that
    /// shipped, not against a fresh re-derivation of what they should be.
    fn prepend(frame: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(payload.len() + frame.len());
        out.extend_from_slice(frame);
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn a_framed_writer_produces_exactly_what_prepending_did() {
        // Both widths, against the same payload written the ordinary way.
        let payload = {
            let mut w = ByteWriter::new();
            w.write_handle(0xdead_beef);
            w.write_string("héllo");
            w.write_bytes(&[1, 2, 3]);
            w.take()
        };

        let mut one = FramedWriter::status(7);
        one.write_handle(0xdead_beef);
        one.write_string("héllo");
        one.write_bytes(&[1, 2, 3]);
        assert_eq!(one.delivered_bytes(), prepend(&[7], &payload));

        let mut two = FramedWriter::event(4, 1);
        two.write_handle(0xdead_beef);
        two.write_string("héllo");
        two.write_bytes(&[1, 2, 3]);
        assert_eq!(two.delivered_bytes(), prepend(&[4, 1], &payload));
    }

    #[test]
    fn a_framed_buffer_reads_back_from_after_its_frame() {
        // The trap this type exists for is silent, so the check is that the
        // payload starts where the reader will look for it — not merely that
        // the first byte is right. `STATUS_OK == 0`, so an unstamped frame
        // would still decode "successfully" here, one field out of step.
        let mut w = FramedWriter::event(4, 1);
        w.write_i32(-7);
        w.write_string("after the frame");
        let buf = w.delivered_bytes();

        assert_eq!(&buf[..2], &[4, 1]);
        let mut r = ByteReader::new(&buf[2..]);
        assert_eq!(r.read_i32(), -7);
        assert_eq!(r.read_string(), "after the frame");
        r.assert_consumed();
    }

    #[test]
    fn an_empty_framed_writer_is_its_frame_and_nothing_else() {
        // The void-return response: `Outcome::Ok` with no payload at all.
        assert_eq!(FramedWriter::status(0).delivered_bytes(), vec![0]);
        assert_eq!(FramedWriter::event(4, 0).delivered_bytes(), vec![4, 0]);
    }

    #[test]
    fn a_framed_writer_writes_through_deref_like_any_other() {
        // Generated encoders take `&mut ByteWriter`; deref coercion is what
        // lets them keep doing so. If this stops compiling, every generated
        // `enc_*` call stops with it.
        fn enc(w: &mut ByteWriter, v: i64) {
            w.write_i64(v);
        }
        let mut w = FramedWriter::status(0);
        enc(&mut w, 99);
        let buf = w.delivered_bytes();
        assert_eq!(ByteReader::new(&buf[1..]).read_i64(), 99);
    }

    #[test]
    fn round_trip_scalars() {
        let mut w = ByteWriter::new();
        w.write_bool(true);
        w.write_i32(-7);
        w.write_i64(i64::MIN);
        w.write_u64(0);
        w.write_u64(u64::MAX);
        w.write_f64(2.5);
        w.write_usize(42);
        w.write_string("héllo");
        w.write_bytes(&[1, 2, 3]);
        w.write_byte_array(&[9; 4]);
        let buf = w.take();
        let mut r = ByteReader::new(&buf);
        assert!(r.read_bool());
        assert_eq!(r.read_i32(), -7);
        assert_eq!(r.read_i64(), i64::MIN);
        assert_eq!(r.read_u64(), 0);
        assert_eq!(r.read_u64(), u64::MAX);
        assert_eq!(r.read_f64(), 2.5);
        assert_eq!(r.read_usize(), 42);
        assert_eq!(r.read_string(), "héllo");
        assert_eq!(r.read_bytes(), vec![1, 2, 3]);
        assert_eq!(r.read_byte_array::<4>(), [9; 4]);
        assert!(r.is_at_end());
    }

    /// Golden vectors; the Dart suite pins the same bytes (the
    /// cross-language codec contract).
    #[test]
    fn u64_wire_bytes() {
        let mut w = ByteWriter::new();
        w.write_u64(0x0123_4567_89AB_CDEF);
        w.write_u64(u64::MAX);
        assert_eq!(
            w.take(),
            [
                0xEF, 0xCD, 0xAB, 0x89, 0x67, 0x45, 0x23, 0x01, //
                0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
            ]
        );
    }

    /// Golden vector; the Dart suite pins the same bytes. `char` rides the u32
    /// wire (LE codepoint), so '🦀' (U+1F980) is `80 F9 01 00`.
    #[test]
    fn char_wire_bytes() {
        let mut w = ByteWriter::new();
        w.write_char('A'); // U+0041
        w.write_char('é'); // U+00E9
        w.write_char('🦀'); // U+1F980
        assert_eq!(
            w.take(),
            [
                0x41, 0x00, 0x00, 0x00, //
                0xE9, 0x00, 0x00, 0x00, //
                0x80, 0xF9, 0x01, 0x00,
            ]
        );
    }

    #[test]
    fn char_round_trip() {
        let mut w = ByteWriter::new();
        for c in ['\0', 'A', 'é', '🦀', '\u{10FFFF}'] {
            w.write_char(c);
        }
        let buf = w.take();
        let mut r = ByteReader::new(&buf);
        for c in ['\0', 'A', 'é', '🦀', '\u{10FFFF}'] {
            assert_eq!(r.read_char(), c);
        }
        assert!(r.is_at_end());
    }

    #[test]
    #[should_panic(expected = "invalid Unicode scalar value")]
    fn read_char_rejects_surrogate() {
        // 0xD800 is a lone surrogate — not a Unicode scalar value.
        let mut r = ByteReader::new(&[0x00, 0xD8, 0x00, 0x00]);
        r.read_char();
    }

    #[test]
    #[should_panic(expected = "invalid Unicode scalar value")]
    fn read_char_rejects_out_of_range() {
        // 0x0011_0000 is one past U+10FFFF, the max scalar.
        let mut r = ByteReader::new(&[0x00, 0x00, 0x11, 0x00]);
        r.read_char();
    }

    /// Golden vectors; the Dart suite pins the same bytes. i128/u128 cross as
    /// 16 LE bytes — the u64 codec extended, byte-identical to two u64 halves.
    #[test]
    fn i128_u128_wire_bytes() {
        let mut w = ByteWriter::new();
        w.write_u128(0x0F0E_0D0C_0B0A_0908_0706_0504_0302_0100);
        w.write_i128(-1);
        w.write_i128(i128::MIN);
        assert_eq!(
            w.take(),
            [
                // u128: byte-incrementing, low byte first
                0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, //
                0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, //
                // i128 -1: all ones (two's complement)
                0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, //
                0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, //
                // i128::MIN = -2^127: only the top (sign) bit set
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80,
            ]
        );
    }

    #[test]
    fn i128_u128_round_trip() {
        let mut w = ByteWriter::new();
        w.write_i128(i128::MIN);
        w.write_i128(i128::MAX);
        w.write_i128(-1);
        w.write_i128(0);
        w.write_u128(0);
        w.write_u128(u128::MAX);
        w.write_u128(1 << 100);
        let buf = w.take();
        let mut r = ByteReader::new(&buf);
        assert_eq!(r.read_i128(), i128::MIN);
        assert_eq!(r.read_i128(), i128::MAX);
        assert_eq!(r.read_i128(), -1);
        assert_eq!(r.read_i128(), 0);
        assert_eq!(r.read_u128(), 0);
        assert_eq!(r.read_u128(), u128::MAX);
        assert_eq!(r.read_u128(), 1 << 100);
        assert!(r.is_at_end());
    }

    #[test]
    #[should_panic(expected = "truncated")]
    fn truncated_buffer_panics() {
        let mut r = ByteReader::new(&[1, 2]);
        r.read_i64();
    }

    /// The request-side mirror of Dart's `BinaryReader.assertConsumed`.
    /// Generated request decodes end with this, so a request carrying more
    /// bytes than the member's arguments is loud and attributable instead of
    /// a silent over-read (the class that a transport shipping a writer's
    /// unused capacity would produce).
    #[test]
    fn assert_consumed_accepts_a_fully_read_buffer() {
        let mut w = ByteWriter::new();
        w.write_i32(7);
        w.write_string("hi");
        let buf = w.take();
        let mut r = ByteReader::new(&buf);
        assert_eq!(r.read_i32(), 7);
        assert_eq!(r.read_string(), "hi");
        r.assert_consumed();
    }

    /// Empty in, nothing read: a no-argument member's request.
    #[test]
    fn assert_consumed_accepts_an_empty_buffer() {
        ByteReader::new(&[]).assert_consumed();
    }

    /// What a decoder clamps an unvalidated length prefix to, so a corrupt
    /// count sizes a reservation by the buffer rather than by itself.
    #[test]
    fn remaining_counts_down_as_the_buffer_is_read() {
        let mut w = ByteWriter::new();
        w.write_len(3);
        w.write_i8(1);
        w.write_i8(2);
        w.write_i8(3);
        let buf = w.take();
        let mut r = ByteReader::new(&buf);
        assert_eq!(r.remaining(), 11);
        let n = r.read_len();
        // The count is honest here, so the clamp is the count.
        assert_eq!(n.min(r.remaining()), 3);
        for _ in 0..n {
            r.read_i8();
        }
        assert_eq!(r.remaining(), 0);
    }

    /// The case the clamp exists for: a count nothing could satisfy is capped
    /// at what is left, and the read that follows is what reports the buffer
    /// short — loudly, and by name.
    #[test]
    #[should_panic(expected = "truncated buffer")]
    fn a_count_larger_than_the_buffer_clamps_and_then_fails_on_the_read() {
        let mut w = ByteWriter::new();
        w.write_len(usize::MAX / 2);
        let buf = w.take();
        let mut r = ByteReader::new(&buf);
        let n = r.read_len();
        assert_eq!(n.min(r.remaining()), 0);
        r.read_i8();
    }

    /// The count is part of the contract: a failure has to say how much was
    /// left over, or it cannot be attributed to a producer.
    #[test]
    #[should_panic(expected = "3 trailing byte(s) after decoded value")]
    fn assert_consumed_names_the_trailing_byte_count() {
        let mut r = ByteReader::new(&[1, 0, 0, 0, 9, 9, 9]);
        assert_eq!(r.read_i32(), 1);
        r.assert_consumed();
    }

    /// Nothing read at all from a non-empty buffer is the same failure, not a
    /// special case — a member with no arguments handed a payload.
    #[test]
    #[should_panic(expected = "2 trailing byte(s)")]
    fn assert_consumed_rejects_an_untouched_buffer() {
        ByteReader::new(&[1, 2]).assert_consumed();
    }

    // ------------------------------------------- bulk typed numeric lists --
    //
    // The bulk slice codec replaces a per-element loop. Its only possible
    // defect is a moved wire, so every test here is either "the bulk form
    // equals the element form" or a pinned golden vector.

    /// The equivalence that keeps the wire frozen: writing a slice in bulk
    /// must produce exactly the bytes the element loop produced. Every width
    /// (1, 2, 4, 8) is covered, because a stride mistake in one of them would
    /// otherwise only surface as a wrong value in an integration test.
    #[test]
    fn a_bulk_write_is_byte_identical_to_the_element_loop() {
        macro_rules! same {
            ($bulk:ident, $one:ident, $vals:expr) => {{
                let v = $vals;
                let mut a = ByteWriter::new();
                a.$bulk(&v);
                let mut b = ByteWriter::new();
                for x in v.iter() {
                    b.$one(*x);
                }
                assert_eq!(a.take(), b.take(), stringify!($bulk));
            }};
        }
        same!(write_i8_slice, write_i8, [i8::MIN, -1, 0, 1, i8::MAX]);
        same!(write_u8_slice, write_u8, [0u8, 1, 127, 128, u8::MAX]);
        same!(write_i16_slice, write_i16, [i16::MIN, -1, 0, 1, i16::MAX]);
        same!(write_u16_slice, write_u16, [0u16, 1, 32768, u16::MAX]);
        same!(write_i32_slice, write_i32, [i32::MIN, -1, 0, 1, i32::MAX]);
        same!(write_u32_slice, write_u32, [0u32, 1, 2147483648, u32::MAX]);
        same!(write_i64_slice, write_i64, [i64::MIN, -1, 0, 1, i64::MAX]);
        same!(write_u64_slice, write_u64, [0u64, 1, 1 << 63, u64::MAX]);
        same!(write_f32_slice, write_f32, [0.0f32, -0.0, 1.5, f32::MIN, f32::NAN]);
        same!(write_f64_slice, write_f64, [0.0f64, -0.0, 1.5, f64::MIN, f64::NAN]);
    }

    /// The other direction: a bulk read must return exactly what the element
    /// reader would have returned from the same bytes.
    #[test]
    fn a_bulk_read_is_value_identical_to_the_element_loop() {
        macro_rules! same {
            ($write:ident, $bulk:ident, $one:ident, $vals:expr) => {{
                let v = $vals;
                let mut w = ByteWriter::new();
                for x in v.iter() {
                    w.$write(*x);
                }
                let buf = w.take();

                let mut bulk = ByteReader::new(&buf);
                let got = bulk.$bulk(v.len());
                assert!(bulk.is_at_end(), stringify!($bulk));

                let mut loops = ByteReader::new(&buf);
                let want: Vec<_> = (0..v.len()).map(|_| loops.$one()).collect();
                assert_eq!(
                    format!("{got:?}"),
                    format!("{want:?}"),
                    stringify!($bulk)
                );
            }};
        }
        same!(write_i8, read_i8_vec, read_i8, [i8::MIN, -1, 0, 1, i8::MAX]);
        same!(write_u8, read_u8_vec, read_u8, [0u8, 1, 127, 128, u8::MAX]);
        same!(write_i16, read_i16_vec, read_i16, [i16::MIN, -1, 0, 1, i16::MAX]);
        same!(write_u16, read_u16_vec, read_u16, [0u16, 1, 32768, u16::MAX]);
        same!(write_i32, read_i32_vec, read_i32, [i32::MIN, -1, 0, 1, i32::MAX]);
        same!(write_u32, read_u32_vec, read_u32, [0u32, 1, 2147483648, u32::MAX]);
        same!(write_i64, read_i64_vec, read_i64, [i64::MIN, -1, 0, 1, i64::MAX]);
        same!(write_u64, read_u64_vec, read_u64, [0u64, 1, 1 << 63, u64::MAX]);
        // NaN is compared through Debug above, so -0.0 and NaN both count.
        same!(write_f32, read_f32_vec, read_f32, [0.0f32, -0.0, 1.5, f32::MIN, f32::NAN]);
        same!(write_f64, read_f64_vec, read_f64, [0.0f64, -0.0, 1.5, f64::MIN, f64::NAN]);
    }

    /// Golden vectors; the Dart suite pins the same bytes
    /// (`binary_codec_bulk_test.dart`, "golden wire vectors"). The length
    /// prefix is the caller's, so it is written here explicitly — exactly as
    /// generated code does.
    #[test]
    fn bulk_list_wire_bytes() {
        let mut w = ByteWriter::new();
        w.write_len(2);
        w.write_f64_slice(&[1.0, -2.5]);
        assert_eq!(
            w.take(),
            [
                // i64 length prefix: 2
                0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
                // 1.0
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xF0, 0x3F, //
                // -2.5
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0xC0,
            ]
        );

        let mut w = ByteWriter::new();
        w.write_len(2);
        w.write_i32_slice(&[1, -2]);
        assert_eq!(
            w.take(),
            [
                0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
                0x01, 0x00, 0x00, 0x00, //
                0xFE, 0xFF, 0xFF, 0xFF,
            ]
        );

        let mut w = ByteWriter::new();
        w.write_len(2);
        w.write_i64_slice(&[1, -2]);
        assert_eq!(
            w.take(),
            [
                0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
                0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
                0xFE, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
            ]
        );
    }

    #[test]
    fn a_bulk_read_leaves_the_cursor_exactly_past_the_list() {
        let mut w = ByteWriter::new();
        w.write_len(3);
        w.write_f64_slice(&[1.0, 2.0, 3.0]);
        w.write_i32(-99);
        let buf = w.take();

        let mut r = ByteReader::new(&buf);
        let n = r.read_len();
        assert_eq!(r.read_f64_vec(n), vec![1.0, 2.0, 3.0]);
        assert_eq!(r.read_i32(), -99);
        r.assert_consumed();
    }

    #[test]
    fn an_empty_bulk_list_is_zero_bytes_of_payload() {
        let mut w = ByteWriter::new();
        w.write_len(0);
        w.write_f64_slice(&[]);
        let buf = w.take();
        assert_eq!(buf.len(), 8);

        let mut r = ByteReader::new(&buf);
        let n = r.read_len();
        assert!(r.read_f64_vec(n).is_empty());
        r.assert_consumed();
    }

    /// A truncated payload is the codec's own loud failure, not an
    /// unattributable slice panic from inside the copy.
    #[test]
    #[should_panic(expected = "truncated")]
    fn a_truncated_bulk_list_panics_loudly() {
        // Claims 4 elements, carries 1.5.
        ByteReader::new(&[0u8; 12]).read_f64_vec(4);
    }

    /// A corrupt element count must be rejected on the *count*, before it is
    /// multiplied by the element width: `n * 8` wraps for a large `n`, and a
    /// wrapped byte count would size the copy from a slice shorter than the
    /// destination.
    #[test]
    #[should_panic(expected = "frustrate codec")]
    fn a_bulk_length_that_overflows_the_byte_count_panics_loudly() {
        ByteReader::new(&[0u8; 16]).read_f64_vec(usize::MAX / 4);
    }

    /// Every framed payload keeps a ledger, and every ledger ends in exactly
    /// one of `reclaim` or `delivered`.
    ///
    /// A reply's encode mints too — that is how a returned handle reaches Dart
    /// — and a reply *can* go undelivered: a dispatched call's answer is posted,
    /// so the isolate that asked for it may already be gone. Only the sync path
    /// is exempt, and it is exempt because of where the answer goes, not because
    /// of how the writer was built; so arming the ledger is not a decision the
    /// encoder is in a position to make, and it does not make one.
    #[test]
    fn every_framed_payload_carries_what_it_minted() {
        static FREED: std::sync::Mutex<Vec<u64>> = std::sync::Mutex::new(Vec::new());
        unsafe fn note(h: u64) {
            FREED.lock().unwrap().push(h);
        }

        // A reply frame: the handle travels, and the ledger travels beside it.
        let mut resp = FramedWriter::status(0);
        unsafe { resp.write_minted(0xAB, note) };
        let (bytes, minted) = resp.into_parts();
        assert_eq!(&bytes[1..], &0xABu64.to_le_bytes(), "the handle travels");
        assert_eq!(minted.len(), 1, "and so does what it minted");
        assert!(FREED.lock().unwrap().is_empty(), "delivery frees nothing");
        minted.delivered();
        assert!(FREED.lock().unwrap().is_empty());

        // A channel event: recorded in encode order, and freed on reclaim.
        let mut ev = FramedWriter::event(4, 0);
        unsafe {
            ev.write_minted(1, note);
            ev.write_minted(2, note);
        }
        let (bytes, minted) = ev.into_parts();
        assert_eq!(minted.len(), 2);
        assert_eq!(bytes.len(), 2 + 16, "the frame plus two handles");
        minted.reclaim();
        assert_eq!(*FREED.lock().unwrap(), vec![1, 2], "freed in encode order");

        // A payload that mints nothing reclaims nothing, and costs no ledger
        // allocation to say so.
        FREED.lock().unwrap().clear();
        let mut plain = FramedWriter::event(4, 0);
        plain.write_i64(7);
        let (_, minted) = plain.into_parts();
        assert!(minted.is_empty());
        minted.reclaim();
        assert!(FREED.lock().unwrap().is_empty());
    }

    /// The guard that makes the rule above enforceable rather than a
    /// convention: a ledger dropped with entries still in it is a set of
    /// objects nothing can ever free, so it fails the run.
    ///
    /// Debug-only, so a shipped build pays nothing — and quiet while
    /// unwinding, because a panic inside a `Drop` during an unwind aborts the
    /// process, which would turn the one leak this design knowingly accepts (an
    /// encoder that panics after minting) into a crash.
    #[test]
    #[should_panic(expected = "was dropped without being delivered or reclaimed")]
    #[cfg(debug_assertions)]
    fn an_undischarged_ledger_fails_the_run() {
        unsafe fn never(_h: u64) {
            unreachable!("the assert fires before anything is freed");
        }
        let mut w = FramedWriter::status(0);
        unsafe { w.write_minted(1, never) };
        drop(w.into_parts().1);
    }
}
