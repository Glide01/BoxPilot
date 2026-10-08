//! What ends a connection: anything the peer sent that the protocol doesn't
//! allow.

use crate::frame::FrameType;
use crate::message::{ErrorCode, Reply, MAX_ERROR_MESSAGE};
use std::fmt;

/// Why a peer's bytes were refused. On the helper's side each one means
/// "send [`ProtocolError::reply`], then close the connection": after one, the
/// decoder and the session refuse everything else with the same error.
/// Encoders return the error the receiving side would raise, so what they
/// produce is always what the other side accepts.
///
/// No variant quotes more than [`MAX_ERROR_MESSAGE`] bytes of what the peer
/// sent, and none quotes an attachment id or the config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    // ---- Frames ----
    /// A frame type byte the protocol doesn't have.
    UnknownFrameType(u8),
    /// A frame of a type not taken here: a blob the GUI never receives, a
    /// blob no `start` declared, or JSON while a `start` still owes blobs.
    UnexpectedFrame(FrameType),
    /// A declared payload length over the cap for its frame type, refused
    /// from the header alone.
    FrameTooLarge {
        frame_type: FrameType,
        len: usize,
        limit: usize,
    },
    /// A JSON frame with no payload, which can't be JSON.
    EmptyJson,
    /// A JSON frame whose payload isn't UTF-8.
    InvalidUtf8,

    // ---- Messages ----
    /// JSON that isn't one of the protocol's messages: a syntax error, an
    /// unknown `type`, an unknown or missing field, a value out of range.
    /// serde_json's message, cut to [`MAX_ERROR_MESSAGE`].
    InvalidMessage(String),

    // ---- The helper's session ----
    /// The first message on a connection wasn't `hello`.
    HelloFirst,
    /// A second `hello` on one connection.
    RepeatedHello,
    /// The `hello` named a protocol version other than the helper's.
    VersionMismatch { helper: u32, client: u32 },
    /// A request before the helper answered the one before it.
    RequestOutstanding,
    /// `start` or `stop` from a caller whose [`crate::Authority`] is
    /// read-only.
    Unauthorized,
    /// A `start` whose `options.proxy_port` is 0.
    ZeroProxyPort,
    /// A `start` declaring more attachments than [`crate::Limits`] allows.
    TooManyAttachments { count: usize, limit: usize },
    /// The attachment at `index` has an id that isn't 1 to 64 of
    /// `A-Z a-z 0-9 _ -` ([`boxpilot_policy::is_attachment_id`]).
    InvalidAttachmentId { index: usize },
    /// The attachment at `index` repeats an earlier id.
    DuplicateAttachmentId { index: usize },
    /// The attachment at `index` declares more bytes than one blob may hold.
    AttachmentTooLarge {
        index: usize,
        len: u64,
        limit: usize,
    },
    /// The config and the declared attachment lengths add up to more than
    /// one `start` may carry.
    StartTooLarge { bytes: u64, limit: usize },
    /// The blob for the attachment at `index` isn't the length its header
    /// declared.
    BlobLength {
        index: usize,
        declared: u64,
        received: usize,
    },
}

impl ProtocolError {
    /// The code the helper answers this error with.
    pub fn code(&self) -> ErrorCode {
        match self {
            ProtocolError::VersionMismatch { .. } => ErrorCode::VersionMismatch,
            ProtocolError::Unauthorized => ErrorCode::Unauthorized,
            _ => ErrorCode::BadRequest,
        }
    }

    /// The `error` reply the helper sends before closing.
    pub fn reply(&self) -> Reply {
        Reply::error(self.code(), self.to_string())
    }

    pub(crate) fn invalid_message(error: serde_json::Error) -> Self {
        ProtocolError::InvalidMessage(crate::shorten(&error.to_string(), MAX_ERROR_MESSAGE))
    }
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtocolError::UnknownFrameType(byte) => {
                write!(f, "frame type 0x{byte:02x} is not one the protocol has")
            }
            ProtocolError::UnexpectedFrame(frame_type) => {
                write!(f, "a {frame_type} frame is not expected here")
            }
            ProtocolError::FrameTooLarge {
                frame_type,
                len,
                limit,
            } => write!(
                f,
                "a {frame_type} frame of {len} bytes is over the {limit}-byte limit"
            ),
            ProtocolError::EmptyJson => f.write_str("a JSON frame is empty"),
            ProtocolError::InvalidUtf8 => f.write_str("a JSON frame is not UTF-8"),
            ProtocolError::InvalidMessage(message) => write!(f, "not a valid message: {message}"),
            ProtocolError::HelloFirst => {
                f.write_str("the first message on a connection must be `hello`")
            }
            ProtocolError::RepeatedHello => {
                f.write_str("`hello` was already sent on this connection")
            }
            ProtocolError::VersionMismatch { helper, client } => write!(
                f,
                "the helper speaks protocol version {helper}, the client {client}"
            ),
            ProtocolError::RequestOutstanding => {
                f.write_str("a request arrived before the previous one was answered")
            }
            ProtocolError::Unauthorized => f.write_str(
                "this account may not start or stop sing-box through the helper; \
                 only `hello` and `status` are open to it",
            ),
            ProtocolError::ZeroProxyPort => f.write_str("`options.proxy_port` is 0"),
            ProtocolError::TooManyAttachments { count, limit } => write!(
                f,
                "{count} attachments are declared, over the limit of {limit}"
            ),
            ProtocolError::InvalidAttachmentId { index } => write!(
                f,
                "attachment {index} has an id that isn't 1 to 64 of A-Z a-z 0-9 _ -"
            ),
            ProtocolError::DuplicateAttachmentId { index } => {
                write!(f, "attachment {index} repeats an earlier id")
            }
            ProtocolError::AttachmentTooLarge { index, len, limit } => write!(
                f,
                "attachment {index} declares {len} bytes, over the {limit}-byte limit"
            ),
            ProtocolError::StartTooLarge { bytes, limit } => write!(
                f,
                "the config and attachments come to {bytes} bytes, over the {limit}-byte limit"
            ),
            ProtocolError::BlobLength {
                index,
                declared,
                received,
            } => write!(
                f,
                "attachment {index} declared {declared} bytes, but its blob has {received}"
            ),
        }
    }
}

impl std::error::Error for ProtocolError {}
