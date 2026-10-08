//! The wire protocol between BoxPilot and its privileged helper (ADR 0006,
//! rule 1).
//!
//! The helper runs as root or SYSTEM, and any local process that can reach
//! its pipe or socket can talk to it, so everything here parses what an
//! untrusted process sends. The crate is pure (no I/O, no threads, no
//! clocks): the helper's I/O layer reads bytes, feeds them in, and writes out
//! what comes back.
//!
//! - **Frames** ([`FrameDecoder`], [`encode_frame`]): a 4-byte big-endian
//!   payload length, a type byte (`0x01` JSON, `0x02` blob), the payload.
//!   Each length is checked against its cap from the header alone, before
//!   anything is allocated.
//! - **Messages**: a [`Request`] goes to the helper; a [`ToClient`] comes
//!   back, either a [`Reply`] to the one outstanding request or an [`Event`]
//!   of the sing-box the connection started. A `start` is a JSON frame
//!   followed by one blob frame per attachment.
//! - **The helper's side** ([`ServerSession`], [`encode_to_client`]): the
//!   per-connection state machine that turns frames into validated requests,
//!   or into the error to answer before closing.
//! - **The GUI's side** ([`encode_request`], [`decode_to_client`]).
//!
//! ## What never changes
//!
//! A GUI and a helper of different releases must still learn that they
//! differ, so three things stay as they are in every protocol version: the
//! frame header, the `hello` request (`{"type":"hello","protocol_version":N}`)
//! and the `error` reply (`{"type":"error","code":…,"message":…}`) with its
//! `version_mismatch` code. Everything else may change with
//! [`PROTOCOL_VERSION`].
//!
//! ## Driving a connection (the helper)
//!
//! ```
//! use boxpilot_protocol::{encode_to_client, Authority, FrameDecoder, Limits, ServerSession};
//!
//! let limits = Limits::default();
//! let mut session = ServerSession::new(Authority::ReadOnly, limits);
//! let mut decoder = FrameDecoder::new(session.frame_caps());
//! # let bytes_read: &[u8] = &[];
//! // For each chunk read from the connection:
//! let mut input = bytes_read;
//! loop {
//!     // `feed` stops at the end of a frame, so the decoder never holds
//!     // more than one; `next_frame` returning `None` means it took all.
//!     input = &input[decoder.feed(input)..];
//!     let step = match decoder.next_frame() {
//!         Ok(Some(frame)) => session.accept(frame),
//!         Ok(None) => break,
//!         Err(error) => Err(error),
//!     };
//!     // What the session can take next: before `hello`, from a caller that
//!     // may not start, or while blobs are owed, far less than the maximum.
//!     decoder.set_caps(session.frame_caps());
//!     match step {
//!         // Act; once the reply is ready, `replied()`, then write it.
//!         Ok(Some(_request)) => session.replied(),
//!         Ok(None) => {} // a `start` still owes blobs
//!         Err(error) => {
//!             let _bytes = encode_to_client(&error.reply().into(), &limits);
//!             // write them, then close the connection
//!             break;
//!         }
//!     }
//! }
//! ```
//!
//! ## What the I/O layer must still do
//!
//! The crate has no clock and no socket, so the helper itself must: put a
//! deadline on `hello` and on every frame once it has begun (a peer that
//! stalls mid-frame holds only what it sent, but holds it); bound the
//! number of connections; bound what it queues for a GUI that stops reading
//! (drop log lines rather than block sing-box or grow without end; never
//! drop a reply or `exited`), with a write deadline; send events only to the
//! connection whose `start` is running; stop that sing-box when its
//! connection closes, mid-frame or not; and decide the caller's
//! [`Authority`] from the OS, never from anything the caller sends.

#![forbid(unsafe_code)]

mod error;
mod frame;
mod message;
mod session;

pub use error::ProtocolError;
pub use frame::{encode_frame, Frame, FrameCaps, FrameDecoder, FrameType, HEADER_LEN};
pub use message::{
    decode_to_client, encode_request, encode_to_client, ErrorCode, Event, ExitInfo, HelloReply,
    RefusalCode, Reply, Request, RunState, StartRequest, Started, ToClient, TunOptions,
    WireRefusal, MAX_ERROR_MESSAGE, MAX_REFUSALS, MAX_REFUSAL_TEXT,
};
pub use session::{Authority, ServerSession};

/// The version this build speaks. A helper answers a `hello` with any other
/// version with an `error` of code `version_mismatch`, and closes.
pub const PROTOCOL_VERSION: u32 = 1;

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;

/// Hard caps, one set per direction. Every one is checked before the bytes
/// it limits are read into memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// To the helper: the largest JSON frame, which only a caller that may
    /// start can send, and only between requests (see `max_control_json`).
    pub max_request_json: usize,
    /// To the helper: the largest JSON frame before `hello`, from a caller
    /// that may not start, or while a request is outstanding.
    pub max_control_json: usize,
    /// To the helper: the largest blob frame (one attachment).
    pub max_blob: usize,
    /// To the helper: a whole `start`, its config text and the declared
    /// lengths of all its attachments together.
    pub max_start_total: usize,
    /// To the helper: the most attachments one `start` may declare.
    pub max_attachments: usize,
    /// To the GUI: the largest JSON frame, replies and events alike.
    pub max_reply_json: usize,
    /// To the GUI: the longest log line; [`encode_to_client`] cuts a longer
    /// one at a UTF-8 boundary and marks it truncated.
    pub max_log_line: usize,
}

impl Default for Limits {
    fn default() -> Self {
        // 32 MiB, the policy's own cap on config text, which ADR 0006 rule 1
        // sizes for real profiles and rule sets. It bounds what one `start`
        // makes the helper hold, config and attachments together, so a
        // config the policy would take is never refused here for its size
        // alone, short of its JSON escaping (below).
        let start = boxpilot_policy::Limits::default().max_bytes;
        Self {
            // A `start` carries its config as a JSON string, so its frame
            // holds a config of up to `start` bytes, the attachment list (at
            // most 64 entries of an id of at most 64 bytes and a length:
            // under 8 KiB) and the envelope; the extra 1 MiB covers those
            // and the escaping of a typical profile. Escaping can double the
            // worst case (every `"` and `\` takes two bytes), so a config
            // near 32 MiB may not fit: the GUI's encoder then refuses it as
            // too large before sending. Real profiles are a few MiB.
            max_request_json: start + MIB,
            // Every request but `start` is under 100 bytes; 4 KiB leaves
            // room for whitespace. A process that may not start can't make
            // the root helper hold more than this for it (rule 4).
            max_control_json: 4 * KIB,
            // One attachment may use the whole `start` budget: a large local
            // `.srs` rule set.
            max_blob: start,
            max_start_total: start,
            // A profile reads a handful of files: a CA, a client certificate
            // and key per outbound, a few local rule sets. The policy's
            // fixture that fills every file-read field it knows reads about
            // twenty. 64 leaves room and keeps the id list, checked before
            // any blob is read, small.
            max_attachments: 64,
            // A log line of 64 KiB grows to at most 384 KiB when every byte
            // is a control character (`\u0001`, six bytes each), and a
            // `refused` reply is at most `MAX_REFUSALS` refusals of bounded
            // text (under 800 KiB); the rest is a few hundred bytes. So
            // 1 MiB holds anything a well-behaved helper sends, and the GUI
            // never reads more per frame.
            max_reply_json: MIB,
            // sing-box's lines are a few hundred bytes. A longer one (a
            // config dump, a panic) is cut rather than refused: logs are
            // what the user debugs with.
            max_log_line: 64 * KIB,
        }
    }
}

impl Limits {
    /// The widest caps the helper reads with, for a decoder that doesn't
    /// narrow them per state with [`ServerSession::frame_caps`].
    pub fn to_helper_caps(&self) -> FrameCaps {
        FrameCaps {
            max_json: Some(self.max_request_json),
            max_blob: Some(self.max_blob),
        }
    }

    /// The caps the GUI reads with: JSON only, as the helper sends no blobs.
    pub fn to_gui_caps(&self) -> FrameCaps {
        FrameCaps {
            max_json: Some(self.max_reply_json),
            max_blob: None,
        }
    }
}

/// The longest prefix of `text` of at most `max` bytes that ends on a char
/// boundary.
pub(crate) fn floor_prefix(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// `text` in at most `max` bytes, a cut marked with a trailing `…`.
pub(crate) fn shorten(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    const MARK: char = '…';
    let mut short = floor_prefix(text, max.saturating_sub(MARK.len_utf8())).to_owned();
    short.push(MARK);
    short
}
