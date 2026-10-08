//! Frames: a 4-byte big-endian payload length, a type byte, the payload.
//!
//! The decoder is the first thing an untrusted peer's bytes meet. It checks
//! a declared length against the cap for its frame type from the header
//! alone, holds at most one frame, and grows its buffer only as the payload
//! actually arrives, so a peer that declares a large frame and stalls holds
//! no more than it sent. OpenVPN's CVE-2024-27459 copied a client-declared
//! size into a fixed buffer; Rust rules that out, but not allocating
//! whatever a client declares, which is what this guards.

use crate::ProtocolError;
use std::{fmt, mem};

/// Bytes before every payload: the length (4, big-endian), then the type.
pub const HEADER_LEN: usize = 5;

/// The most the decoder sets aside for a payload before more of it arrives
/// (or the payload's length, when that is smaller). It doubles from there.
const INITIAL_RESERVE: usize = 64 * 1024;

/// What a frame's payload is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrameType {
    /// A message: UTF-8 JSON, never empty. Type byte `0x01`.
    Json,
    /// An attachment's bytes, after the `start` that declared it. Type byte
    /// `0x02`.
    Blob,
}

impl FrameType {
    /// The type byte on the wire.
    pub fn byte(self) -> u8 {
        match self {
            FrameType::Json => 0x01,
            FrameType::Blob => 0x02,
        }
    }

    /// The frame type a type byte names, if any.
    pub fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0x01 => Some(FrameType::Json),
            0x02 => Some(FrameType::Blob),
            _ => None,
        }
    }
}

impl fmt::Display for FrameType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            FrameType::Json => "JSON",
            FrameType::Blob => "blob",
        })
    }
}

/// One whole frame.
#[derive(Clone, PartialEq, Eq)]
pub enum Frame {
    /// A JSON frame, already checked to be non-empty UTF-8.
    Json(String),
    /// A blob frame.
    Blob(Vec<u8>),
}

impl Frame {
    pub fn frame_type(&self) -> FrameType {
        match self {
            Frame::Json(_) => FrameType::Json,
            Frame::Blob(_) => FrameType::Blob,
        }
    }

    pub fn payload(&self) -> &[u8] {
        match self {
            Frame::Json(text) => text.as_bytes(),
            Frame::Blob(bytes) => bytes,
        }
    }
}

/// Type and length only: a `start` frame carries the config, and with it
/// the profile's passwords and keys, which have no place in a log.
impl fmt::Debug for Frame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Frame::{:?}({} bytes)",
            self.frame_type(),
            self.payload().len()
        )
    }
}

/// The largest payload taken for each frame type; `None` refuses the type
/// outright.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameCaps {
    pub max_json: Option<usize>,
    pub max_blob: Option<usize>,
}

/// Whether a frame of `frame_type` with a `len`-byte payload may pass under
/// `caps`. The decoder and the encoders share it, so whatever an encoder
/// produces under the same caps decodes.
pub(crate) fn check_header(
    frame_type: FrameType,
    len: usize,
    caps: &FrameCaps,
) -> Result<(), ProtocolError> {
    let limit = match frame_type {
        FrameType::Json => caps.max_json,
        FrameType::Blob => caps.max_blob,
    }
    .ok_or(ProtocolError::UnexpectedFrame(frame_type))?;
    // The header can't say more than u32::MAX, whatever the cap.
    let limit = limit.min(u32::MAX as usize);
    if len > limit {
        return Err(ProtocolError::FrameTooLarge {
            frame_type,
            len,
            limit,
        });
    }
    if frame_type == FrameType::Json && len == 0 {
        return Err(ProtocolError::EmptyJson);
    }
    Ok(())
}

/// The bytes of `frame`, if `caps` let it pass: encoding over a cap is an
/// error, never a truncation.
pub fn encode_frame(frame: &Frame, caps: &FrameCaps) -> Result<Vec<u8>, ProtocolError> {
    let mut out = Vec::new();
    append_frame(&mut out, frame.frame_type(), frame.payload(), caps)?;
    Ok(out)
}

/// Append one frame to `out`, if `caps` let it pass.
pub(crate) fn append_frame(
    out: &mut Vec<u8>,
    frame_type: FrameType,
    payload: &[u8],
    caps: &FrameCaps,
) -> Result<(), ProtocolError> {
    check_header(frame_type, payload.len(), caps)?;
    out.reserve(HEADER_LEN + payload.len());
    out.extend_from_slice(&header(frame_type, payload.len()));
    out.extend_from_slice(payload);
    Ok(())
}

/// A JSON frame of `value`, serialized in place behind its header.
pub(crate) fn encode_json<T: serde::Serialize>(
    value: &T,
    caps: &FrameCaps,
) -> Result<Vec<u8>, ProtocolError> {
    let mut out = vec![0; HEADER_LEN];
    serde_json::to_writer(&mut out, value)
        .expect("protocol messages have string keys and plain values, so they always serialize");
    let len = out.len() - HEADER_LEN;
    check_header(FrameType::Json, len, caps)?;
    out[..HEADER_LEN].copy_from_slice(&header(FrameType::Json, len));
    Ok(out)
}

/// The header of a frame `check_header` passed, so `len` fits in a u32.
fn header(frame_type: FrameType, len: usize) -> [u8; HEADER_LEN] {
    let len = u32::try_from(len).expect("check_header caps every length at u32::MAX");
    let [a, b, c, d] = len.to_be_bytes();
    [a, b, c, d, frame_type.byte()]
}

/// An incremental, sans-I/O frame decoder: [`feed`](Self::feed) it bytes as
/// they arrive, then take frames with [`next_frame`](Self::next_frame).
///
/// After an error the decoder is poisoned: it takes no more bytes, and
/// `next_frame` returns the same error every time.
pub struct FrameDecoder {
    caps: FrameCaps,
    state: State,
}

enum State {
    /// Reading a header; `filled` of its bytes have arrived.
    Header {
        bytes: [u8; HEADER_LEN],
        filled: usize,
    },
    /// Reading a payload of `len` bytes, its header already checked.
    Payload {
        frame_type: FrameType,
        len: usize,
        bytes: Vec<u8>,
    },
    /// A whole frame, waiting for `next_frame`.
    Ready(Frame),
    Poisoned(ProtocolError),
}

impl State {
    fn header() -> Self {
        State::Header {
            bytes: [0; HEADER_LEN],
            filled: 0,
        }
    }
}

impl FrameDecoder {
    pub fn new(caps: FrameCaps) -> Self {
        Self {
            caps,
            state: State::header(),
        }
    }

    /// The caps for every header completed from now on. The helper narrows
    /// them after each frame to what its session takes next
    /// ([`crate::ServerSession::frame_caps`]); a frame already under way
    /// keeps the caps it was checked against.
    pub fn set_caps(&mut self, caps: FrameCaps) {
        self.caps = caps;
    }

    /// Take bytes from `input` and return how many were taken. It stops at
    /// the end of a frame, so the decoder never holds more than one: feed
    /// the rest after [`next_frame`](Self::next_frame) hands that frame out.
    /// It takes nothing while a frame waits or after an error. So when
    /// `next_frame` then returns `Ok(None)`, all of `input` was taken.
    #[must_use = "the bytes after a whole frame are not taken; feed them again after next_frame"]
    pub fn feed(&mut self, input: &[u8]) -> usize {
        let mut taken = 0;
        while taken < input.len() {
            let rest = &input[taken..];
            match &mut self.state {
                State::Header { bytes, filled } => {
                    let n = rest.len().min(HEADER_LEN - *filled);
                    bytes[*filled..*filled + n].copy_from_slice(&rest[..n]);
                    *filled += n;
                    taken += n;
                    if *filled == HEADER_LEN {
                        let header = *bytes;
                        self.state = self.begin(header);
                    }
                }
                State::Payload {
                    frame_type,
                    len,
                    bytes,
                } => {
                    let n = rest.len().min(*len - bytes.len());
                    reserve_for(bytes, *len, n);
                    bytes.extend_from_slice(&rest[..n]);
                    taken += n;
                    if bytes.len() == *len {
                        let frame_type = *frame_type;
                        let bytes = mem::take(bytes);
                        self.state = finish(frame_type, bytes);
                    }
                }
                State::Ready(_) | State::Poisoned(_) => break,
            }
        }
        taken
    }

    /// The next whole frame, `Ok(None)` if none is complete yet, or the
    /// error that poisoned the decoder.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, ProtocolError> {
        match mem::replace(&mut self.state, State::header()) {
            State::Ready(frame) => Ok(Some(frame)),
            State::Poisoned(error) => {
                self.state = State::Poisoned(error.clone());
                Err(error)
            }
            pending => {
                self.state = pending;
                Ok(None)
            }
        }
    }

    /// The bytes held for the frame in progress, its header included: never
    /// more than a header and one payload its caps allowed.
    pub fn buffered(&self) -> usize {
        match &self.state {
            State::Header { filled, .. } => *filled,
            State::Payload { bytes, .. } => HEADER_LEN + bytes.len(),
            State::Ready(frame) => HEADER_LEN + frame.payload().len(),
            State::Poisoned(_) => 0,
        }
    }

    /// The memory set aside for the payload in progress: no more than its
    /// declared length, and no more than about twice what has arrived (or
    /// 64 KiB). The helper can sum it over connections.
    pub fn reserved(&self) -> usize {
        match &self.state {
            State::Payload { bytes, .. } => bytes.capacity(),
            State::Ready(Frame::Json(text)) => text.capacity(),
            State::Ready(Frame::Blob(bytes)) => bytes.capacity(),
            State::Header { .. } | State::Poisoned(_) => 0,
        }
    }

    /// The state after a whole header: the payload to read, or an error.
    fn begin(&self, header: [u8; HEADER_LEN]) -> State {
        let [a, b, c, d, type_byte] = header;
        let len = usize::try_from(u32::from_be_bytes([a, b, c, d])).unwrap_or(usize::MAX);
        let Some(frame_type) = FrameType::from_byte(type_byte) else {
            return State::Poisoned(ProtocolError::UnknownFrameType(type_byte));
        };
        if let Err(error) = check_header(frame_type, len, &self.caps) {
            return State::Poisoned(error);
        }
        if len == 0 {
            // Only a blob gets here: an empty JSON frame failed the check.
            return finish(frame_type, Vec::new());
        }
        State::Payload {
            frame_type,
            len,
            bytes: Vec::new(),
        }
    }
}

impl fmt::Debug for FrameDecoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = match &self.state {
            State::Header { .. } => "header",
            State::Payload { .. } => "payload",
            State::Ready(_) => "ready",
            State::Poisoned(_) => "poisoned",
        };
        f.debug_struct("FrameDecoder")
            .field("caps", &self.caps)
            .field("state", &state)
            .field("buffered", &self.buffered())
            .finish()
    }
}

/// Make room in `bytes` for `n` more of a `len`-byte payload: doubling from
/// `INITIAL_RESERVE`, never past `len`.
fn reserve_for(bytes: &mut Vec<u8>, len: usize, n: usize) {
    let needed = bytes.len() + n;
    if needed <= bytes.capacity() {
        return;
    }
    let target = needed
        .max(bytes.capacity().saturating_mul(2))
        .max(INITIAL_RESERVE)
        .min(len);
    bytes.reserve_exact(target - bytes.len());
}

/// The state after a whole payload.
fn finish(frame_type: FrameType, bytes: Vec<u8>) -> State {
    match frame_type {
        FrameType::Json => match String::from_utf8(bytes) {
            Ok(text) => State::Ready(Frame::Json(text)),
            Err(_) => State::Poisoned(ProtocolError::InvalidUtf8),
        },
        FrameType::Blob => State::Ready(Frame::Blob(bytes)),
    }
}
