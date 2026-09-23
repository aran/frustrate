//! The wire protocol, in its entirety: a 4-byte big-endian length followed by
//! that many bytes of UTF-8.
//!
//! Deliberately free of iroh, tokio and frustrate. This file is linked by
//! `node.rs` and by `peerbot`, so that the app and the thing that tests the app
//! cannot disagree about the wire. Keeping
//! it dependency-light is what makes `#[path = "…/proto.rs"] mod proto;` a legal
//! way for a second crate to pick it up.
//!
//! There is no async in here. A codec that owns its own reads has to name an
//! `AsyncRead`, which means naming a runtime, which is exactly the coupling
//! `peerbot` would then inherit. Instead the caller does the reading and this
//! module answers two questions: how long is the next frame, and what does its
//! body say.

/// The length prefix, in bytes.
pub const HEADER_LEN: usize = 4;

/// The largest body this protocol will encode or accept, in bytes.
///
/// A chat line, generously. The number matters because the length prefix is
/// attacker-controlled on the read side: without a cap, one four-byte header
/// asks the reader to allocate 4 GiB.
pub const MAX_BODY_LEN: usize = 64 * 1024;

/// Everything that can be wrong with a frame.
///
/// Small and total on purpose: each variant is a distinct thing the caller may
/// want to say to a user, and `Display` is written to be shown as-is in
/// `PeerEvent::Left`/`Failed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// The body is longer than [`MAX_BODY_LEN`]. Carries the offending length.
    TooLong(usize),
    /// The body is not valid UTF-8.
    NotUtf8,
    /// The peer stopped mid-frame: fewer bytes arrived than the header promised.
    /// Carries `(expected, got)`.
    Truncated(usize, usize),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLong(n) => {
                write!(f, "frame of {n} bytes exceeds the {MAX_BODY_LEN}-byte limit")
            }
            Self::NotUtf8 => f.write_str("frame body is not valid UTF-8"),
            Self::Truncated(want, got) => {
                write!(f, "frame truncated: wanted {want} bytes, got {got}")
            }
        }
    }
}

impl std::error::Error for FrameError {}

/// Encode one frame: the length prefix followed by the UTF-8 body.
pub fn encode(body: &str) -> Result<Vec<u8>, FrameError> {
    let len = body.len();
    if len > MAX_BODY_LEN {
        return Err(FrameError::TooLong(len));
    }
    let mut out = Vec::with_capacity(HEADER_LEN + len);
    // `len <= MAX_BODY_LEN` (64 KiB), so the cast cannot truncate.
    out.extend_from_slice(&(len as u32).to_be_bytes());
    out.extend_from_slice(body.as_bytes());
    Ok(out)
}

/// How many body bytes the header promises, rejecting anything over
/// [`MAX_BODY_LEN`] *before* the caller allocates for it.
pub fn body_len(header: [u8; HEADER_LEN]) -> Result<usize, FrameError> {
    let len = u32::from_be_bytes(header) as usize;
    if len > MAX_BODY_LEN {
        return Err(FrameError::TooLong(len));
    }
    Ok(len)
}

/// Decode a body the caller has already read in full.
///
/// `expected` is what [`body_len`] said, so a short read is reported as
/// [`FrameError::Truncated`] rather than silently decoding a partial frame.
pub fn decode_body(expected: usize, body: &[u8]) -> Result<String, FrameError> {
    if body.len() != expected {
        return Err(FrameError::Truncated(expected, body.len()));
    }
    String::from_utf8(body.to_vec()).map_err(|_| FrameError::NotUtf8)
}

/// Decode a whole frame — header and body together — from one buffer.
///
/// The convenience form for a caller that already has the bytes (a test, or a
/// datagram-shaped transport). A streaming reader wants [`body_len`] and
/// [`decode_body`] instead, so that it can read exactly as much as it needs.
///
/// `allow(dead_code)`: `node.rs` reads from a stream and so never calls this,
/// but it is part of the codec's contract with `peerbot` (C5), which links this
/// file and does have whole frames in hand.
#[allow(dead_code)]
pub fn decode(frame: &[u8]) -> Result<String, FrameError> {
    if frame.len() < HEADER_LEN {
        return Err(FrameError::Truncated(HEADER_LEN, frame.len()));
    }
    let mut header = [0u8; HEADER_LEN];
    header.copy_from_slice(&frame[..HEADER_LEN]);
    let len = body_len(header)?;
    decode_body(len, &frame[HEADER_LEN..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        for body in ["", "hello", "héllo 🌍", &"x".repeat(MAX_BODY_LEN)] {
            let framed = encode(body).expect("under the limit");
            assert_eq!(framed.len(), HEADER_LEN + body.len());
            assert_eq!(decode(&framed).as_deref(), Ok(body));
        }
    }

    #[test]
    fn header_is_big_endian() {
        assert_eq!(&encode("abc").unwrap()[..HEADER_LEN], &[0, 0, 0, 3]);
    }

    #[test]
    fn rejects_oversize_on_encode() {
        let body = "x".repeat(MAX_BODY_LEN + 1);
        assert_eq!(encode(&body), Err(FrameError::TooLong(MAX_BODY_LEN + 1)));
    }

    #[test]
    fn rejects_oversize_header_before_allocating() {
        // u32::MAX would be a 4 GiB allocation if the length were trusted.
        assert_eq!(
            body_len(u32::MAX.to_be_bytes()),
            Err(FrameError::TooLong(u32::MAX as usize))
        );
    }

    #[test]
    fn rejects_truncation() {
        let framed = encode("hello").unwrap();
        assert_eq!(
            decode(&framed[..framed.len() - 2]),
            Err(FrameError::Truncated(5, 3))
        );
        assert_eq!(decode(&framed[..2]), Err(FrameError::Truncated(4, 2)));
        assert_eq!(decode(&[]), Err(FrameError::Truncated(4, 0)));
    }

    #[test]
    fn rejects_non_utf8() {
        let mut framed = encode("ab").unwrap();
        framed[HEADER_LEN] = 0xff;
        assert_eq!(decode(&framed), Err(FrameError::NotUtf8));
    }

    #[test]
    fn frames_are_self_delimiting() {
        // Two frames in one buffer decode independently — the property the
        // reader in node.rs relies on when a peer writes twice in a row.
        let mut buf = encode("one").unwrap();
        buf.extend_from_slice(&encode("two").unwrap());
        let mut header = [0u8; HEADER_LEN];
        header.copy_from_slice(&buf[..HEADER_LEN]);
        let first = body_len(header).unwrap();
        assert_eq!(
            decode_body(first, &buf[HEADER_LEN..HEADER_LEN + first]).unwrap(),
            "one"
        );
        assert_eq!(decode(&buf[HEADER_LEN + first..]).unwrap(), "two");
    }
}
