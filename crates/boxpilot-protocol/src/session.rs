//! The helper's side of one connection: frames in, validated requests out.

use crate::frame::FrameCaps;
use crate::message::{AttachmentHeader, MapOnly, WireRequest};
use crate::{
    Frame, FrameType, Limits, ProtocolError, Request, StartRequest, TunOptions, PROTOCOL_VERSION,
};
use boxpilot_policy::is_attachment_id;
use std::collections::{BTreeSet, VecDeque};
use std::{fmt, mem};

/// What the caller may ask, which the helper decides from the OS before the
/// first byte is read: the pipe client's token on Windows, the socket peer's
/// uid on macOS (ADR 0006 rule 4). Nothing the caller sends changes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Authority {
    /// May `start` and `stop`, as well as `hello` and `status`.
    MayStart,
    /// `hello` and `status` only.
    ReadOnly,
}

impl Authority {
    /// What `hello`'s reply says in `may_start`.
    pub fn may_start(self) -> bool {
        self == Authority::MayStart
    }
}

/// The protocol state of one connection, which the helper drives: each
/// decoded frame goes to [`accept`](Self::accept), which returns a complete,
/// validated [`Request`] to act on, nothing yet (a `start` still owes
/// blobs), or the error to answer before closing.
///
/// It enforces, in this order:
///
/// - `hello` comes first, once, with this build's [`PROTOCOL_VERSION`];
/// - one request at a time: none until the helper calls
///   [`replied`](Self::replied) for the one before;
/// - a [`Authority::ReadOnly`] caller gets [`ProtocolError::Unauthorized`]
///   for `start` and `stop`;
/// - JSON frames are small: a `start` header within
///   [`Limits::max_start_json`], anything else within
///   [`Limits::max_control_json`], both checked before parsing as far as
///   the frame's length tells;
/// - a `start` header is checked whole when it arrives, before any blob is
///   taken: at most [`Limits::max_attachments`], a non-zero `config_len`,
///   each id valid ([`is_attachment_id`]) and unique, each length within
///   [`Limits::max_blob`], config and attachments together within
///   [`Limits::max_start_total`], and `proxy_port` not 0;
/// - exactly the declared blobs follow, in order: the config, of exactly
///   `config_len` bytes and UTF-8, then each attachment of exactly its
///   length; nothing else until they have.
///
/// After any error the session is finished, and refuses every later frame
/// with that same error.
pub struct ServerSession {
    authority: Authority,
    limits: Limits,
    state: State,
}

enum State {
    AwaitHello,
    Idle,
    /// A request went to the helper, which hasn't called `replied` yet.
    Outstanding,
    /// A `start` arrived and owes blobs.
    Blobs(PendingStart),
    Failed(ProtocolError),
}

struct PendingStart {
    options: TunOptions,
    config_len: u64,
    /// `None` until the config's blob, the first one owed, has arrived.
    config: Option<String>,
    /// `(id, declared length)` of each attachment still owed, in order.
    owed: VecDeque<(String, u64)>,
    received: Vec<(String, Vec<u8>)>,
}

impl PendingStart {
    /// The declared length of the next blob owed, if any.
    fn next_len(&self) -> Option<u64> {
        match self.config {
            None => Some(self.config_len),
            Some(_) => self.owed.front().map(|(_, len)| *len),
        }
    }
}

impl ServerSession {
    pub fn new(authority: Authority, limits: Limits) -> Self {
        Self {
            authority,
            limits,
            state: State::AwaitHello,
        }
    }

    pub fn authority(&self) -> Authority {
        self.authority
    }

    /// Take one decoded frame.
    pub fn accept(&mut self, frame: Frame) -> Result<Option<Request>, ProtocolError> {
        let result = self.step(frame);
        if let Err(error) = &result {
            self.state = State::Failed(error.clone());
        }
        result
    }

    /// The helper has answered the outstanding request, so the next one may
    /// come. Call it once the reply is ready and before writing it: the GUI
    /// may send its next request the moment it reads the reply, and one
    /// that arrives first is refused ([`ProtocolError::RequestOutstanding`]).
    /// Events (`log`, `exited`) are not answers. Nothing changes when no
    /// request is outstanding, and [`frame_caps`](Self::frame_caps) never
    /// does.
    pub fn replied(&mut self) {
        if let State::Outstanding = self.state {
            self.state = State::Idle;
        }
    }

    /// Whether an error finished the session.
    pub fn is_finished(&self) -> bool {
        matches!(self.state, State::Failed(_))
    }

    /// The frames the session can take next, for
    /// [`FrameDecoder::set_caps`](crate::FrameDecoder::set_caps), so a frame
    /// it would refuse is refused from its header:
    ///
    /// - before `hello`, and always for a read-only caller: JSON of at most
    ///   [`Limits::max_control_json`];
    /// - after `hello`, for a caller that may start: JSON of up to
    ///   [`Limits::max_start_json`];
    /// - while a `start` owes blobs: only a blob, no longer than the next one
    ///   owed;
    /// - after an error: nothing.
    ///
    /// Only [`accept`](Self::accept) changes it, never
    /// [`replied`](Self::replied), so setting it after every frame is
    /// enough. Correctness doesn't depend on it: `accept` checks every rule
    /// again. It bounds what an untrusted caller can make the helper hold
    /// (ADR 0006 rule 4).
    pub fn frame_caps(&self) -> FrameCaps {
        let control = FrameCaps {
            max_json: Some(self.limits.max_control_json),
            max_blob: None,
        };
        match &self.state {
            State::AwaitHello => control,
            State::Idle | State::Outstanding => match self.authority {
                Authority::MayStart => FrameCaps {
                    max_json: Some(self.limits.max_start_json()),
                    max_blob: None,
                },
                Authority::ReadOnly => control,
            },
            State::Blobs(pending) => FrameCaps {
                max_json: None,
                max_blob: pending
                    .next_len()
                    .map(|len| usize::try_from(len).unwrap_or(usize::MAX)),
            },
            State::Failed(_) => FrameCaps {
                max_json: None,
                max_blob: None,
            },
        }
    }

    fn step(&mut self, frame: Frame) -> Result<Option<Request>, ProtocolError> {
        let text = match (&mut self.state, frame) {
            (State::Failed(error), _) => return Err(error.clone()),
            (State::Blobs(_), Frame::Blob(data)) => return self.blob(data),
            (State::Blobs(_), Frame::Json(_)) => {
                return Err(ProtocolError::UnexpectedFrame(FrameType::Json))
            }
            (_, Frame::Blob(_)) => return Err(ProtocolError::UnexpectedFrame(FrameType::Blob)),
            // Not even parsed: whatever it is, it is one request too many.
            (State::Outstanding, Frame::Json(_)) => return Err(ProtocolError::RequestOutstanding),
            (State::AwaitHello | State::Idle, Frame::Json(text)) => text,
        };
        // Held to the caps whether or not the decoder was: nothing larger
        // reaches serde_json.
        let too_large = |limit| ProtocolError::FrameTooLarge {
            frame_type: FrameType::Json,
            len: text.len(),
            limit,
        };
        let cap = self.frame_caps().max_json.unwrap_or(0);
        if text.len() > cap {
            return Err(too_large(cap));
        }
        let MapOnly(request) = serde_json::from_str::<MapOnly<WireRequest>>(&text)
            .map_err(ProtocolError::invalid_message)?;
        let control = self.limits.max_control_json;
        if text.len() > control && !matches!(request, WireRequest::Start { .. }) {
            return Err(too_large(control));
        }
        if let State::AwaitHello = self.state {
            return match request {
                WireRequest::Hello { protocol_version } => self.hello(protocol_version),
                _ => Err(ProtocolError::HelloFirst),
            };
        }
        match request {
            WireRequest::Hello { .. } => Err(ProtocolError::RepeatedHello),
            WireRequest::Status {} => self.yield_request(Request::Status),
            WireRequest::Stop {} => {
                self.authorize()?;
                self.yield_request(Request::Stop)
            }
            WireRequest::Start {
                config_len,
                attachments,
                options,
            } => {
                self.authorize()?;
                check_start(config_len, &attachments, &options, &self.limits)?;
                // `config_len` isn't 0, so a blob is always owed.
                self.state = State::Blobs(PendingStart {
                    options,
                    config_len,
                    config: None,
                    received: Vec::with_capacity(attachments.len()),
                    owed: attachments
                        .into_iter()
                        .map(|header| (header.id.into_owned(), header.len))
                        .collect(),
                });
                Ok(None)
            }
        }
    }

    fn hello(&mut self, protocol_version: u32) -> Result<Option<Request>, ProtocolError> {
        if protocol_version != PROTOCOL_VERSION {
            return Err(ProtocolError::VersionMismatch {
                helper: PROTOCOL_VERSION,
                client: protocol_version,
            });
        }
        self.yield_request(Request::Hello { protocol_version })
    }

    fn authorize(&self) -> Result<(), ProtocolError> {
        match self.authority {
            Authority::MayStart => Ok(()),
            Authority::ReadOnly => Err(ProtocolError::Unauthorized),
        }
    }

    /// The next owed blob: the config, then each attachment. The pending
    /// start moves out of the state and back in while blobs are still owed;
    /// on an error `accept` replaces it.
    fn blob(&mut self, data: Vec<u8>) -> Result<Option<Request>, ProtocolError> {
        let unexpected = ProtocolError::UnexpectedFrame(FrameType::Blob);
        let State::Blobs(mut pending) = mem::replace(&mut self.state, State::Outstanding) else {
            return Err(unexpected);
        };
        if pending.config.is_none() {
            if data.len() as u64 != pending.config_len {
                return Err(ProtocolError::ConfigLength {
                    declared: pending.config_len,
                    received: data.len(),
                });
            }
            let config = String::from_utf8(data).map_err(|_| ProtocolError::ConfigNotUtf8)?;
            pending.config = Some(config);
        } else {
            let index = pending.received.len();
            let Some((id, declared)) = pending.owed.pop_front() else {
                return Err(unexpected);
            };
            if data.len() as u64 != declared {
                return Err(ProtocolError::BlobLength {
                    index,
                    declared,
                    received: data.len(),
                });
            }
            pending.received.push((id, data));
        }
        match pending {
            PendingStart {
                config: Some(config),
                options,
                received,
                owed,
                ..
            } if owed.is_empty() => Ok(Some(Request::Start(StartRequest {
                config,
                attachments: received,
                options,
            }))),
            pending => {
                self.state = State::Blobs(pending);
                Ok(None)
            }
        }
    }

    fn yield_request(&mut self, request: Request) -> Result<Option<Request>, ProtocolError> {
        self.state = State::Outstanding;
        Ok(Some(request))
    }
}

/// States only: a pending `start` holds the config and attachments.
impl fmt::Debug for ServerSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = match &self.state {
            State::AwaitHello => "await_hello".to_owned(),
            State::Idle => "idle".to_owned(),
            State::Outstanding => "outstanding".to_owned(),
            State::Blobs(pending) => format!(
                "blobs ({} owed)",
                pending.owed.len() + usize::from(pending.config.is_none())
            ),
            State::Failed(error) => format!("failed ({error})"),
        };
        f.debug_struct("ServerSession")
            .field("authority", &self.authority)
            .field("state", &state)
            .finish()
    }
}

/// The rules a `start` header must meet before any of its blobs is taken.
/// The GUI's encoder runs them too, so it never sends what the helper would
/// refuse for them.
pub(crate) fn check_start(
    config_len: u64,
    attachments: &[AttachmentHeader],
    options: &TunOptions,
    limits: &Limits,
) -> Result<(), ProtocolError> {
    if attachments.len() > limits.max_attachments {
        return Err(ProtocolError::TooManyAttachments {
            count: attachments.len(),
            limit: limits.max_attachments,
        });
    }
    if config_len == 0 {
        return Err(ProtocolError::EmptyConfig);
    }
    let mut seen = BTreeSet::new();
    let mut total = config_len;
    for (index, attachment) in attachments.iter().enumerate() {
        if !is_attachment_id(&attachment.id) {
            return Err(ProtocolError::InvalidAttachmentId { index });
        }
        if !seen.insert(&*attachment.id) {
            return Err(ProtocolError::DuplicateAttachmentId { index });
        }
        if attachment.len > limits.max_blob as u64 {
            return Err(ProtocolError::AttachmentTooLarge {
                index,
                len: attachment.len,
                limit: limits.max_blob,
            });
        }
        total = total.saturating_add(attachment.len);
    }
    if total > limits.max_start_total as u64 {
        return Err(ProtocolError::StartTooLarge {
            bytes: total,
            limit: limits.max_start_total,
        });
    }
    if options.proxy_port == 0 {
        return Err(ProtocolError::ZeroProxyPort);
    }
    Ok(())
}
