//! One connection to the privileged helper, over any byte stream
//! ([`HelperIo`]): the Windows pipe in production, a socket pair in the
//! tests. No platform code here.
//!
//! The connection is sing-box's lifetime: the helper stops the sing-box a
//! connection started when that connection ends (ADR 0006 rule 6), so it
//! stays open while TUN is up, and closing it is the stop of last resort —
//! including a BoxPilot that crashes. The helper also drops a client that
//! stops reading (a 10 s write deadline), which stops sing-box too, so a
//! thread reads it continuously from the moment it opens: replies go to
//! whoever waits for one ([`HelperConnection::hello`], `start`, `stop`),
//! sing-box's lines and exit to an event channel the UI drains.
//!
//! Deadlines mirror the helper's: `hello` within 10 s, a whole `start`
//! within 60 s.

use super::{error_message, refused_message, OpenError};
use crate::core::singbox_api::{supports_api_service, SingBoxApi};
use crate::i18n::s;
use boxpilot_protocol::{
    decode_to_client, encode_request, ErrorCode, Event, ExitInfo, FrameDecoder, HelloReply, Limits,
    ProtocolError, Reply, Request, StartRequest, ToClient, WireRefusal, PROTOCOL_VERSION,
};
use futures_channel::mpsc::{self, UnboundedReceiver, UnboundedSender};
use std::io;
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// How long the helper's answer to `hello` may take; the helper itself
/// drops a client whose `hello` takes longer.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// A whole `start`: writing it (the helper's own limit is 60 s from its
/// first byte) and the helper checking it and spawning sing-box.
pub const START_TIMEOUT: Duration = Duration::from_secs(60);

/// A byte stream to the helper, shared by the reading thread and whoever
/// sends a request. Both may use it at once.
pub trait HelperIo: Send + Sync {
    /// Read what has arrived, blocking until something has; `Ok(0)` at the
    /// end of the stream. After [`HelperIo::close`] it returns at once.
    fn read(&self, buf: &mut [u8]) -> io::Result<usize>;
    /// Write all of `buf`, or fail at `deadline`.
    fn write_all(&self, buf: &[u8], deadline: Instant) -> io::Result<()>;
    /// End the connection: reads and writes under way and later ones end at
    /// once. The helper sees the end of the stream once the last owner
    /// drops the stream, and stops the sing-box this connection started.
    fn close(&self);
}

/// What the helper reports unasked about the sing-box this connection
/// started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelperEvent {
    /// One line of sing-box's output. The helper may drop lines when
    /// BoxPilot falls behind; it never drops `exited`.
    Log(String),
    /// sing-box exited.
    Exited(ExitInfo),
    /// The connection ended (the reason, for the log). If no `Exited` came
    /// first, sing-box is gone with it all the same: the helper stops it.
    Closed(String),
}

/// Why a start through the helper didn't happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelperFailure {
    Open(OpenError),
    /// The helper says this account may not start sing-box.
    NotAllowed,
    /// The helper's sing-box predates the `api` service BoxPilot needs.
    SingBoxTooOld(String),
    /// The helper's policy refused the config.
    Refused {
        refusals: Vec<WireRefusal>,
        omitted: u32,
    },
    /// An `error` reply.
    Error {
        code: ErrorCode,
        message: String,
    },
    /// No reply in time.
    TimedOut,
    /// The connection ended before the reply.
    Lost,
    /// A reply BoxPilot can't decode, or the wrong one.
    BadReply(String),
    /// Writing to the helper failed.
    Io(String),
}

impl HelperFailure {
    pub fn message(&self) -> String {
        let h = &s().helper;
        match self {
            HelperFailure::Open(error) => error.message(),
            HelperFailure::NotAllowed => h.not_allowed.to_string(),
            HelperFailure::SingBoxTooOld(version) => (s().messages.sing_box_too_old)(
                version,
                crate::core::singbox_api::MIN_SING_BOX_VERSION,
            ),
            HelperFailure::Refused { refusals, omitted } => refused_message(refusals, *omitted),
            HelperFailure::Error { code, message } => error_message(*code, message),
            HelperFailure::TimedOut => h.no_answer.to_string(),
            HelperFailure::Lost => h.lost.to_string(),
            HelperFailure::BadReply(detail) => (h.bad_reply)(detail),
            HelperFailure::Io(error) => (h.talk_failed)(error),
        }
    }
}

impl From<OpenError> for HelperFailure {
    fn from(error: OpenError) -> Self {
        HelperFailure::Open(error)
    }
}

/// What the reading thread hands a waiting request: a reply, or the
/// protocol error that ended the connection.
type Incoming = Result<Reply, ProtocolError>;

/// An open connection. Dropping it closes the connection, and with it
/// stops the sing-box it started.
pub struct HelperConnection {
    io: Arc<dyn HelperIo>,
    replies: std_mpsc::Receiver<Incoming>,
    limits: Limits,
}

impl HelperConnection {
    /// Start reading `io` on a thread of its own. Events go to the returned
    /// channel, which closes after its last event, [`HelperEvent::Closed`].
    pub fn new(io: Arc<dyn HelperIo>) -> (Self, UnboundedReceiver<HelperEvent>) {
        let limits = Limits::default();
        let (replies_tx, replies) = std_mpsc::channel();
        let (events_tx, events) = mpsc::unbounded();
        let reader = io.clone();
        thread::spawn(move || read_loop(reader.as_ref(), &limits, replies_tx, events_tx));
        (
            Self {
                io,
                replies,
                limits,
            },
            events,
        )
    }

    /// Send `request` and wait at most `timeout` for its reply.
    fn request(&self, request: &Request, timeout: Duration) -> Result<Reply, HelperFailure> {
        let deadline = Instant::now() + timeout;
        let bytes =
            encode_request(request, &self.limits).map_err(|error| HelperFailure::Error {
                code: ErrorCode::BadRequest,
                message: error.to_string(),
            })?;
        self.io
            .write_all(&bytes, deadline)
            .map_err(|error| match error.kind() {
                io::ErrorKind::TimedOut => HelperFailure::TimedOut,
                _ => HelperFailure::Io(error.to_string()),
            })?;
        let left = deadline.saturating_duration_since(Instant::now());
        match self.replies.recv_timeout(left) {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(error)) => Err(HelperFailure::BadReply(error.to_string())),
            Err(std_mpsc::RecvTimeoutError::Timeout) => Err(HelperFailure::TimedOut),
            Err(std_mpsc::RecvTimeoutError::Disconnected) => Err(HelperFailure::Lost),
        }
    }

    /// The first request on every connection.
    pub fn hello(&self) -> Result<HelloReply, HelperFailure> {
        match self.request(&Request::hello(), HELLO_TIMEOUT)? {
            Reply::Hello(hello) if hello.protocol_version == PROTOCOL_VERSION => Ok(hello),
            Reply::Hello(hello) => Err(HelperFailure::BadReply(format!(
                "protocol version {}",
                hello.protocol_version
            ))),
            reply => Err(unexpected(reply)),
        }
    }

    /// Ask the helper to run sing-box on `request`. Its sing-box API comes
    /// back as `SingBoxApi`; the secret stays in memory.
    pub fn start(&self, request: StartRequest) -> Result<SingBoxApi, HelperFailure> {
        match self.request(&Request::Start(request), START_TIMEOUT)? {
            Reply::Started(started) => {
                SingBoxApi::from_secret_hex(started.api_port, &started.api_secret)
                    .ok_or_else(|| HelperFailure::BadReply("api_secret".into()))
            }
            Reply::Refused { refusals, omitted } => {
                Err(HelperFailure::Refused { refusals, omitted })
            }
            reply => Err(unexpected(reply)),
        }
    }

    /// Stop sing-box and close: send `stop`, wait up to `timeout` for
    /// `stopped` (the helper sends it once sing-box has exited and its run
    /// is cleaned up; sing-box's `exited` follows, through the events, and
    /// may be cut off by the close), then close the connection, which would
    /// stop it anyway. Blocking.
    pub fn stop(self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        if let Ok(bytes) = encode_request(&Request::Stop, &self.limits) {
            if self.io.write_all(&bytes, deadline).is_ok() {
                loop {
                    let left = deadline.saturating_duration_since(Instant::now());
                    match self.replies.recv_timeout(left) {
                        Ok(Ok(Reply::Stopped | Reply::Error { .. })) | Ok(Err(_)) | Err(_) => break,
                        // A late answer to an earlier request.
                        Ok(Ok(_)) => continue,
                    }
                }
            }
        }
        // Drop closes.
    }
}

impl Drop for HelperConnection {
    fn drop(&mut self) {
        self.io.close();
    }
}

/// A reply that isn't the one asked for: an `error` says why, anything else
/// is out of place.
fn unexpected(reply: Reply) -> HelperFailure {
    match reply {
        Reply::Error { code, message } => HelperFailure::Error { code, message },
        other => HelperFailure::BadReply(format!("{other:?}")),
    }
}

/// Connect, say `hello`, check that this caller may start and that the
/// helper's sing-box is new enough, and start. On success sing-box runs
/// until the connection is stopped or dropped. On failure the connection is
/// closed.
pub fn start_session(
    io: Arc<dyn HelperIo>,
    request: StartRequest,
) -> Result<(HelperConnection, SingBoxApi, UnboundedReceiver<HelperEvent>), HelperFailure> {
    let (connection, events) = HelperConnection::new(io);
    let hello = connection.hello()?;
    if !hello.may_start {
        return Err(HelperFailure::NotAllowed);
    }
    if !supports_api_service(&hello.sing_box_version) {
        return Err(HelperFailure::SingBoxTooOld(hello.sing_box_version));
    }
    let api = connection.start(request)?;
    Ok((connection, api, events))
}

/// The reading thread: frames in, replies and events out, until the stream
/// ends or breaks the protocol. Decoded with the GUI's caps: JSON only, at
/// most 1 MiB a frame.
fn read_loop(
    io: &dyn HelperIo,
    limits: &Limits,
    replies: std_mpsc::Sender<Incoming>,
    events: UnboundedSender<HelperEvent>,
) {
    let mut decoder = FrameDecoder::new(limits.to_gui_caps());
    let mut buf = vec![0u8; 64 * 1024];
    let reason = 'read: loop {
        let mut input = match io.read(&mut buf) {
            Ok(0) => break 'read "the helper closed the connection".to_string(),
            Ok(n) => &buf[..n],
            Err(error) => break 'read error.to_string(),
        };
        loop {
            input = &input[decoder.feed(input)..];
            let message = match decoder.next_frame() {
                Ok(Some(frame)) => decode_to_client(&frame),
                // `feed` took all of `input`.
                Ok(None) => break,
                Err(error) => Err(error),
            };
            match message {
                Ok(ToClient::Reply(reply)) => {
                    let _ = replies.send(Ok(reply));
                }
                Ok(ToClient::Event(Event::Log { line, .. })) => {
                    let _ = events.unbounded_send(HelperEvent::Log(line));
                }
                Ok(ToClient::Event(Event::Exited(exit))) => {
                    let _ = events.unbounded_send(HelperEvent::Exited(exit));
                }
                Err(error) => {
                    let reason = error.to_string();
                    let _ = replies.send(Err(error));
                    io.close();
                    break 'read reason;
                }
            }
        }
    };
    let _ = events.unbounded_send(HelperEvent::Closed(reason));
}

#[cfg(all(test, unix))]
mod tests;
