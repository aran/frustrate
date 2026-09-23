//! Response envelope: 1 status byte + payload.
//!
//!   0 = ok          -> return value payload
//!   1 = error       -> display string of the user's Err (Dart: BridgeException)
//!   2 = panic       -> panic message (Dart: BridgePanicException)
//!   3 = contention  -> ContentionError display (Dart: ContentionException)
//!   4 = stream item     -> one encoded stream item (see stream.rs)
//!   5 = stream end      -> empty payload; the stream closed normally
//!   6 = callback call   -> invocation id (u64) + encoded argument; the
//!                          Dart side responds through the
//!                          frustrate_callback_respond export (callback.rs)
//!   7 = leaked          -> the holder's type name; a terminal saying this
//!                          channel died because its Dart handle was collected
//!                          without dispose() (Dart: LeakedChannelException)
//!
//! The same envelope is used for sync returns and async completions; stream
//! and callback events reuse it keyed by channel id (statuses 4/5/6/7, plus
//! 1/2 as stream terminal errors, plus 5 to retire a dropped callback's
//! registration), so one post channel carries everything.

use crate::codec::FramedWriter;
use crate::error::ContentionError;
use std::panic::{catch_unwind, AssertUnwindSafe};

pub const STATUS_OK: u8 = 0;
pub const STATUS_ERROR: u8 = 1;
pub const STATUS_PANIC: u8 = 2;
pub const STATUS_CONTENTION: u8 = 3;
pub const STATUS_STREAM_ITEM: u8 = 4;
pub const STATUS_STREAM_END: u8 = 5;
pub const STATUS_CALLBACK_CALL: u8 = 6;
/// Terminal for a channel whose Dart handle was garbage-collected without
/// `dispose()`. Distinct from [`STATUS_STREAM_END`] because the two are
/// opposite facts: end means the producer finished, leaked means the consumer
/// dropped the only thing that could have stopped it. Payload is the holder's
/// type name (stream.rs, `finalize_scope`).
pub const STATUS_LEAKED: u8 = 7;

/// A call returned `Err(e)` where `e` is a **bridged type**, encoded by value
/// like any other payload rather than flattened to its `Display` string.
///
/// Distinct from [`STATUS_ERROR`] rather than a discriminator inside its
/// string, because a discriminator would make the Dart side parse prose to
/// recover a fact the Rust side already had as a value — which is the defect
/// the typed path exists to remove.
pub const STATUS_TYPED_ERROR: u8 = 8;

/// Outcome of one bridge call body, before enveloping.
///
/// `Ok` carries a [`FramedWriter`], not a [`ByteWriter`], and that is the
/// whole of the status byte's safety: the frame is written when the writer is
/// constructed, so there is no state in which a response buffer has room for a
/// status it has not been given. `STATUS_OK == 0`, so the alternative failure
/// is silent — an unstamped byte decodes as a valid OK envelope with every
/// field shifted by one.
pub enum Outcome {
    Ok(FramedWriter),
    Error(String),
    /// `Err(e)` with `e` encoded by value. Carries a [`FramedWriter`] for the
    /// same reason `Ok` does — the status is stamped at construction, so the
    /// buffer cannot exist without it — and the invariant is therefore not
    /// "Ok is the only framed variant" but "every framed variant carries the
    /// status it was constructed with".
    TypedError(FramedWriter),
    Contention(ContentionError),
}

impl Outcome {
    /// Free what this outcome's encode minted, for a call somebody else has
    /// already answered.
    ///
    /// The answer-once gates — the cooperative executor's, and the actor host
    /// loop's — run *after* the body, so by the time one of them loses the race
    /// the reply is encoded and its handles are registered. Dropping it whole
    /// would strand them: the Dart wrapper that disposes them is never built,
    /// and nothing else holds the pointer.
    pub fn reclaim(self) {
        match self {
            Outcome::Ok(w) | Outcome::TypedError(w) => w.into_parts().1.reclaim(),
            // Neither carries a payload, so neither can have minted.
            Outcome::Error(_) | Outcome::Contention(_) => {}
        }
    }
}

/// Run a call body, converting panics into the panic envelope. The body
/// returns an `Outcome`; this returns the final wire buffer, still paired with
/// the ledger of what its encode minted — see [`encode`].
pub fn run(body: impl FnOnce() -> Outcome) -> FramedWriter {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(outcome) => encode(outcome),
        Err(panic) => panic_envelope(panic),
    }
}

/// Encode a completed [`Outcome`] into its wire envelope. This is the
/// non-panic half of [`run`], split out for the cooperative executor: an
/// `async fn` body cannot be wrapped in a single `catch_unwind` (the panic
/// may happen across an `.await`, in a later poll), so the executor catches
/// the poll panic itself and calls this for the ordinary path.
///
/// **A [`FramedWriter`] and not a `Vec<u8>`**, all the way to the transport,
/// because the bytes alone cannot say what they minted. A reply carrying a
/// handle registers the Rust object while encoding — before anyone knows
/// whether the answer will be taken — so the ledger has to travel beside the
/// bytes as far as the one place that learns the answer,
/// [`crate::post::deliver`]. Handing a bare `Vec` out here is what let a reply
/// refused by a dead isolate leak every handle it carried.
pub fn encode(outcome: Outcome) -> FramedWriter {
    match outcome {
        // No copy: the body encoded straight past the status byte its writer
        // was constructed with, so the buffer is already the wire buffer.
        // Same for TypedError, whose writer was built with its own status.
        Outcome::Ok(writer) | Outcome::TypedError(writer) => writer,
        Outcome::Error(msg) => string_envelope(STATUS_ERROR, &msg),
        Outcome::Contention(e) => string_envelope(STATUS_CONTENTION, &e.to_string()),
    }
}

/// Envelope for a payload caught by catch_unwind.
///
/// The only constructor of a `STATUS_PANIC` envelope, which is what makes it
/// the panic listener's funnel: every dispatch shape that reports a panic to
/// Dart — a sync body, a pool job, an executor poll, the two prefix catches in
/// generated glue — arrives here, so a shape added later is covered without
/// anyone remembering to cover it.
///
/// The listener is told **after** the bytes exist and before they are
/// returned: the wire path is never left half-built because a listener was
/// slow, and no answer has left for Dart yet either.
pub fn panic_envelope(panic: Box<dyn std::any::Any + Send>) -> FramedWriter {
    let msg = crate::panic::message(&*panic);
    let reply = string_envelope(STATUS_PANIC, msg);
    crate::panic::observed(msg);
    reply
}

pub(crate) fn string_envelope(status: u8, msg: &str) -> FramedWriter {
    let mut w = FramedWriter::status(status);
    w.write_string(msg);
    w
}

/// [`string_envelope`] as bare bytes, for an envelope that is **not posted**:
/// `callback::fail_invocations_for` synthesizes one and hands it to a Rust
/// frame already parked on it. Nothing can refuse it, and a string mints
/// nothing, so the ledger is empty and given up on the spot.
///
/// Native only, because its one caller is: failing the invocations an exited
/// isolate left parked, and there are no isolates on web.
#[cfg(not(target_family = "wasm"))]
pub(crate) fn string_bytes(status: u8, msg: &str) -> Vec<u8> {
    let (bytes, minted) = string_envelope(status, msg).into_parts();
    minted.delivered();
    bytes
}

/// [`run`] as bare bytes. Test-only: every shipped caller hands the writer to a
/// transport, which is what discharges its ledger; a test that only wants to
/// read the envelope back has no transport to do that for it.
#[cfg(test)]
pub(crate) fn delivered_run(body: impl FnOnce() -> Outcome) -> Vec<u8> {
    run(body).delivered_bytes()
}

/// Smallest `out` buffer [`respond_out`] accepts: three `u64`s, which is what
/// the overflow path has to write there. A transport that offers less has
/// nowhere to receive the fallback, so it is refused rather than truncated.
pub const RESP_OUT_MIN_CAP: u64 = 24;

/// What [`respond_out`] returns when the response did not fit and was leased
/// instead. Negative, so "fits" and "does not fit" are told apart by the sign
/// of one register and never by a sentinel a real length could collide with.
pub const RESP_LEASED: i32 = -1;

/// Answer one sync call into a buffer the **caller** owns, leasing only when
/// the answer does not fit.
///
/// `out` is `out_cap` bytes the transport allocated as part of the same block
/// that carries the request, and frees on its way out regardless. When the
/// envelope fits there it is copied in and the length is returned: the
/// transport has nothing of ours to hand back, and therefore nothing to hand
/// it back *with*. When it does not fit, the buffer is leased exactly as it
/// always was — `(ptr, len, cap)`, three little-endian `u64`s written to the
/// first [`RESP_OUT_MIN_CAP`] bytes of `out` — and [`RESP_LEASED`] is
/// returned.
///
/// **Why the response moved into the caller's buffer.** On dart2wasm every
/// `dart:js_interop` operation is a wasm→JS boundary crossing, so the count of
/// wasm entries per bridge call is what sets the crossing floor. Handing a
/// fresh `Vec` back cost one
/// entry per call purely to free it. This removes that entry — the small
/// in-module `memcpy` that replaces it is not in the same cost class as a
/// crossing — and leaves the sizes that could not fit anywhere sensible on the
/// path they were already on.
///
/// **Why the size is not negotiated.** The transport cannot know the response
/// size before the call, and a two-pass protocol (ask, then retry) would mean
/// either running the body twice — wrong for anything with a side effect — or
/// parking the answer in cross-call state, which is the reentrancy hazard
/// below. Answering in place *when it fits* needs neither.
///
/// **Reentrancy.** There is none to manage: `out` belongs to one call, is
/// written only after [`run`] has returned — so after the body, and after any
/// nested bridge call the body started, has completed — and is freed by the
/// same transport frame that allocated it. A nested call allocates its own.
/// This is the property that made the slab worth having where a slab *reused
/// across calls* was not: the threaded pool's spawn hook and every Rust→Dart
/// callback can start a second call while an outer request is still
/// borrowed.
///
/// **The one destination that can never refuse**, and so the one place a mint
/// ledger is legitimately given up unread ([`crate::codec::Minted::delivered`]).
/// The answer goes back inside the caller's own frame; there is no port to
/// close and no isolate to outlive it. Every other destination posts, and posts
/// can be turned away — which is why [`crate::post::deliver`] reclaims instead.
///
/// # Safety
/// `out` must be non-null and valid for writes of `out_cap` bytes, and
/// `out_cap` must be at least [`RESP_OUT_MIN_CAP`]. Both are checked, because
/// the alternative to a panic here is a write past a live Dart-side buffer.
pub unsafe fn respond_out(reply: FramedWriter, out: *mut u8, out_cap: u64) -> i32 {
    assert!(
        !out.is_null() && out_cap >= RESP_OUT_MIN_CAP,
        "frustrate: the sync response slab must be non-null and at least \
         {RESP_OUT_MIN_CAP} bytes (got {out_cap}) — the transport and this \
         runtime disagree about the call ABI"
    );
    // After the ABI check, so a reply this frame refuses to write is not first
    // declared delivered. Nothing frees on that path either — the assert is
    // unwinding — but the two statements should not disagree.
    let (payload, minted) = reply.into_parts();
    minted.delivered();
    // `min` because the length is returned as an i32: a caller offering an
    // absurd capacity must not be able to produce one that reads as negative
    // and is mistaken for RESP_LEASED.
    let fits = out_cap.min(i32::MAX as u64);
    if payload.len() as u64 <= fits {
        // Disjoint by construction: `payload` is a fresh allocation and `out`
        // is the caller's block, and on wasm the request lives *after* `out`
        // inside that block, never inside the first `out_cap` bytes.
        unsafe { std::ptr::copy_nonoverlapping(payload.as_ptr(), out, payload.len()) };
        return payload.len() as i32;
    }
    let mut payload = payload;
    let triple = [
        payload.as_mut_ptr() as u64,
        payload.len() as u64,
        payload.capacity() as u64,
    ];
    // Byte-wise: the block is a `Vec<u8>` on the Dart side, so it carries no
    // alignment guarantee this could rely on, and an unaligned store is free
    // on every target that runs this.
    unsafe {
        std::ptr::copy_nonoverlapping(
            triple.as_ptr() as *const u8,
            out,
            RESP_OUT_MIN_CAP as usize,
        )
    };
    std::mem::forget(payload);
    RESP_LEASED
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::ByteReader;

    #[test]
    fn ok_envelope() {
        let buf = delivered_run(|| {
            let mut w = FramedWriter::status(STATUS_OK);
            w.write_i32(7);
            Outcome::Ok(w)
        });
        assert_eq!(buf[0], STATUS_OK);
        assert_eq!(ByteReader::new(&buf[1..]).read_i32(), 7);
    }

    #[test]
    fn panic_becomes_envelope() {
        // `run` funnels this panic through `panic_envelope`, which tells the
        // process-global panic listener. `post::test_lock` is what the tests
        // that register one hold, and without it this panic lands in their log
        // and fails their "exactly one report" assertions.
        let _serial = crate::post::test_lock();
        let buf = delivered_run(|| panic!("boom {}", 42));
        assert_eq!(buf[0], STATUS_PANIC);
        assert_eq!(ByteReader::new(&buf[1..]).read_string(), "boom 42");
    }

    /// A payload that fits is copied into the caller's own buffer, and the
    /// return is its length — so the caller has nothing to free and, on wasm,
    /// spends no boundary crossing freeing it.
    #[test]
    fn a_fitting_payload_lands_in_the_callers_slab() {
        let mut slab = [0u8; 64];
        let n = unsafe { respond_out(FramedWriter::from_bytes(vec![1, 2, 3, 4, 5]), slab.as_mut_ptr(), 64) };
        assert_eq!(n, 5);
        assert_eq!(&slab[..5], &[1, 2, 3, 4, 5]);
    }

    /// Exactly at capacity fits. An off-by-one here is invisible from the API
    /// — the lease path returns the same bytes — so nothing but this would
    /// notice the commonest envelope size quietly moving onto the slow path.
    #[test]
    fn a_payload_exactly_the_size_of_the_slab_fits() {
        let mut slab = [0u8; 32];
        let payload: Vec<u8> = (0..32u8).collect();
        let n = unsafe { respond_out(FramedWriter::from_bytes(payload.clone()), slab.as_mut_ptr(), 32) };
        assert_eq!(n, 32);
        assert_eq!(&slab[..], &payload[..]);
    }

    /// One byte over is leased instead, and the triple written back must be
    /// exactly the one that frees it — including the capacity, which is the
    /// field a `Vec` cannot be reconstructed without.
    #[test]
    fn a_payload_one_byte_too_large_is_leased() {
        let mut slab = [0u8; 32];
        let payload: Vec<u8> = (0..33u8).collect();
        let n = unsafe { respond_out(FramedWriter::from_bytes(payload.clone()), slab.as_mut_ptr(), 32) };
        assert_eq!(n, RESP_LEASED);

        // Read the triple back the way the transports do: three little-endian
        // u64s at the front of the slab, from a buffer with no alignment
        // guarantee (this one is a `[u8; 32]` on the stack).
        let mut triple = [0u64; 3];
        unsafe {
            std::ptr::copy_nonoverlapping(
                slab.as_ptr(),
                triple.as_mut_ptr() as *mut u8,
                RESP_OUT_MIN_CAP as usize,
            )
        };
        let (ptr, len, cap) = (triple[0] as *mut u8, triple[1], triple[2]);
        assert!(!ptr.is_null(), "nothing was leased");
        assert_eq!(len, 33);
        assert!(cap >= len, "capacity {cap} cannot be below length {len}");
        assert_eq!(
            unsafe { std::slice::from_raw_parts(ptr, len as usize) },
            &payload[..]
        );
        // The whole point of the triple: this must not corrupt the allocator.
        unsafe { crate::frustrate_buffer_free(ptr, len, cap) };
    }

    /// The floor on the slab is the size of the lease triple, because the
    /// overflow path has nowhere else to put it. A smaller one is refused
    /// loudly rather than writing past the caller's buffer.
    #[test]
    #[should_panic(expected = "response slab")]
    fn a_slab_too_small_for_the_lease_triple_is_refused() {
        let mut slab = [0u8; 16];
        unsafe { respond_out(FramedWriter::from_bytes(vec![7u8]), slab.as_mut_ptr(), 16) };
    }

    #[test]
    fn contention_envelope_is_attributable() {
        let buf = delivered_run(|| {
            Outcome::Contention(ContentionError {
                type_name: "Cache",
                method: "Cache::get",
                kind: crate::error::Contention::Lock,
            })
        });
        assert_eq!(buf[0], STATUS_CONTENTION);
        let msg = ByteReader::new(&buf[1..]).read_string();
        assert!(msg.contains("Cache::get"), "{msg}");
        assert!(msg.contains("on_contention"), "{msg}");
    }
}
