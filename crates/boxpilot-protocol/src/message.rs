//! The messages, and their encoding into frames for each direction.
//!
//! Every JSON message is an object tagged by its `type`, with
//! `deny_unknown_fields` throughout and no catch-all variant: a field or a
//! type this build doesn't know is an error, not something skipped. Each
//! message has one spelling: an object, never the array of its fields that
//! serde would also take ([`MapOnly`]). The one exception is deliberate: a
//! refusal's `code` comes from the policy, which can grow with a sing-box
//! bump without a protocol change, so the GUI keeps a code it doesn't know
//! rather than failing on it.

use crate::frame::{append_frame, encode_json, FrameCaps, FrameType};
use crate::session::check_start;
use crate::{floor_prefix, shorten, Frame, Limits, ProtocolError, PROTOCOL_VERSION};
use boxpilot_policy::{Expected, Refusal, RefusalKind};
use serde::de::value::MapAccessDeserializer;
use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::borrow::Cow;
use std::collections::BTreeSet;
use std::fmt;
use std::marker::PhantomData;

/// The most refusals one `refused` reply lists; [`Reply::refused`] counts
/// the rest in `omitted`. The GUI runs the same policy before it asks, so
/// the helper's list is a fallback; this keeps it inside
/// [`Limits::max_reply_json`] however hostile the config.
pub const MAX_REFUSALS: usize = 128;

/// The longest `pointer` or `detail` of a [`WireRefusal`], in bytes; longer
/// text is cut and ends in `…`. Real pointers are tens of bytes, but both
/// fields quote the config, and a key can be megabytes long. 128 refusals
/// of two such fields, each escaped sixfold at worst, stay under 800 KiB.
pub const MAX_REFUSAL_TEXT: usize = 512;

/// The longest `message` of an `error` reply, in bytes; longer text is cut
/// and ends in `…`. serde_json's messages can quote what the peer sent
/// (`unknown variant "…"`), which can be as large as a frame.
pub const MAX_ERROR_MESSAGE: usize = 4096;

// ---- GUI → helper ----

/// A request to the helper. The GUI encodes one with [`encode_request`];
/// the helper gets one, validated, from
/// [`crate::ServerSession::accept`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// The first message on every connection. The helper answers
    /// [`Reply::Hello`], or [`ErrorCode::VersionMismatch`] and closes.
    Hello { protocol_version: u32 },
    /// Run sing-box on this profile config. Answered [`Reply::Started`] or
    /// [`Reply::Refused`]. Only a caller that may start sends it.
    Start(StartRequest),
    /// Stop the sing-box this connection started. Answered
    /// [`Reply::Stopped`]. Only a caller that may start sends it.
    Stop,
    /// Answered [`Reply::Status`]. Open to every caller.
    Status,
}

impl Request {
    /// The `hello` this build sends.
    pub fn hello() -> Self {
        Request::Hello {
            protocol_version: PROTOCOL_VERSION,
        }
    }
}

/// A `start` with its attachments. Out of the helper's session, the config
/// is non-empty UTF-8, the ids are valid, unique and within the limits, the
/// config and each attachment hold exactly the bytes the header declared,
/// and `options.proxy_port` isn't 0. The config is not checked yet: that is
/// the policy's job (`boxpilot_policy::check`, with
/// [`StartRequest::attachment_ids`]).
#[derive(Clone, PartialEq, Eq)]
pub struct StartRequest {
    /// The profile's canonical config, as JSON text. It travels as the
    /// first blob after the header, raw.
    pub config: String,
    /// `(id, content)` per attachment, in the order sent.
    pub attachments: Vec<(String, Vec<u8>)>,
    pub options: TunOptions,
}

impl StartRequest {
    /// The ids the request carries, as `boxpilot_policy::check` takes them.
    pub fn attachment_ids(&self) -> BTreeSet<String> {
        self.attachments.iter().map(|(id, _)| id.clone()).collect()
    }
}

/// Sizes only: the config holds the profile's passwords and keys, and an
/// attachment can be a private key.
impl fmt::Debug for StartRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        struct Sizes<'a>(&'a [(String, Vec<u8>)]);
        impl fmt::Debug for Sizes<'_> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_map()
                    .entries(self.0.iter().map(|(id, data)| (id, data.len())))
                    .finish()
            }
        }
        f.debug_struct("StartRequest")
            .field("config_len", &self.config.len())
            .field("attachment_lens", &Sizes(&self.attachments))
            .field("options", &self.options)
            .finish()
    }
}

/// The typed options of a TUN start, from which the helper builds the
/// privileged parts of the config itself (ADR 0006 rule 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TunOptions {
    /// Give the TUN inbound an IPv6 address too.
    pub ipv6: bool,
    /// The local proxy's port (Settings › Network). Never 0.
    pub proxy_port: u16,
    /// "Allow LAN connections": the local proxy listens beyond loopback.
    pub allow_lan: bool,
    /// Have sing-box set the system proxy.
    pub system_proxy: bool,
}

/// A request as JSON. A `start` here is only its header: the config and
/// the attachments follow as blobs, so nothing large is ever parsed as
/// JSON. Ids borrow when encoding.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum WireRequest<'a> {
    Hello {
        protocol_version: u32,
    },
    Start {
        config_len: u64,
        #[serde(deserialize_with = "seq_of_maps")]
        attachments: Vec<AttachmentHeader<'a>>,
        #[serde(deserialize_with = "map_only")]
        options: TunOptions,
    },
    Stop {},
    Status {},
}

/// One attachment, as a `start` declares it ahead of its blob.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AttachmentHeader<'a> {
    pub(crate) id: Cow<'a, str>,
    pub(crate) len: u64,
}

/// The frames that carry `request` to the helper: one JSON frame, then, for
/// a `start`, the config as a blob and one blob per attachment, in order. A
/// request the helper's session would refuse (an empty config, a bad id, too
/// many attachments, too many bytes, a zero proxy port) is refused here with
/// the same error, before anything is sent.
pub fn encode_request(request: &Request, limits: &Limits) -> Result<Vec<u8>, ProtocolError> {
    let control = FrameCaps {
        max_json: Some(limits.max_control_json),
        max_blob: None,
    };
    let wire = match request {
        Request::Hello { protocol_version } => WireRequest::Hello {
            protocol_version: *protocol_version,
        },
        Request::Stop => WireRequest::Stop {},
        Request::Status => WireRequest::Status {},
        Request::Start(start) => return encode_start(start, limits),
    };
    encode_json(&wire, &control)
}

fn encode_start(start: &StartRequest, limits: &Limits) -> Result<Vec<u8>, ProtocolError> {
    let headers: Vec<AttachmentHeader> = start
        .attachments
        .iter()
        .map(|(id, data)| AttachmentHeader {
            id: Cow::Borrowed(id),
            len: data.len() as u64,
        })
        .collect();
    let config_len = start.config.len() as u64;
    check_start(config_len, &headers, &start.options, limits)?;
    let caps = limits.to_helper_caps();
    let wire = WireRequest::Start {
        config_len,
        attachments: headers,
        options: start.options,
    };
    let mut out = encode_json(&wire, &caps)?;
    append_frame(&mut out, FrameType::Blob, start.config.as_bytes(), &caps)?;
    for (_, data) in &start.attachments {
        append_frame(&mut out, FrameType::Blob, data, &caps)?;
    }
    Ok(out)
}

// ---- helper → GUI ----

/// What the helper sends: a reply to the one outstanding request, or an
/// event of the sing-box this connection started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToClient {
    Reply(Reply),
    Event(Event),
}

impl From<Reply> for ToClient {
    fn from(reply: Reply) -> Self {
        ToClient::Reply(reply)
    }
}

impl From<Event> for ToClient {
    fn from(event: Event) -> Self {
        ToClient::Event(event)
    }
}

/// The answer to a request. Any request can get [`Reply::Error`] instead of
/// its own reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// To `hello`.
    Hello(HelloReply),
    /// To `start`: sing-box runs, and its sing-box API listens here.
    Started(Started),
    /// To `start`: the policy refused the config. At most [`MAX_REFUSALS`]
    /// refusals; `omitted` counts the rest.
    Refused {
        refusals: Vec<WireRefusal>,
        omitted: u32,
    },
    /// To `stop`.
    Stopped,
    /// To `status`.
    Status {
        state: RunState,
        last_exit: Option<ExitInfo>,
    },
    Error {
        code: ErrorCode,
        message: String,
    },
}

impl Reply {
    /// The `refused` reply for what the policy refused: the first
    /// [`MAX_REFUSALS`], the rest counted in `omitted`.
    pub fn refused(refusals: &[Refusal]) -> Self {
        let listed = refusals.len().min(MAX_REFUSALS);
        Reply::Refused {
            refusals: refusals[..listed].iter().map(WireRefusal::from).collect(),
            omitted: u32::try_from(refusals.len() - listed).unwrap_or(u32::MAX),
        }
    }

    /// An `error` reply, its message cut to [`MAX_ERROR_MESSAGE`].
    pub fn error(code: ErrorCode, message: impl AsRef<str>) -> Self {
        Reply::Error {
            code,
            message: shorten(message.as_ref(), MAX_ERROR_MESSAGE),
        }
    }
}

/// The reply to `hello`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelloReply {
    /// Always [`PROTOCOL_VERSION`]: a helper of another version answers
    /// `error` instead.
    pub protocol_version: u32,
    pub helper_version: String,
    /// The version the installed sing-box reports.
    pub sing_box_version: String,
    /// The SHA-256 of the installed sing-box, lowercase hex.
    pub sing_box_sha256: String,
    /// Whether this caller may `start` and `stop`.
    pub may_start: bool,
}

/// The reply to a `start` that runs: where the helper's own `api` service
/// listens, on loopback, and its per-run secret. The GUI's `SingBoxApi`
/// talks to it as it does to a sing-box it started itself.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Started {
    pub api_port: u16,
    /// Lowercase hex, as sing-box's `secret` option takes it.
    pub api_secret: String,
}

/// Redacts the secret, as `SingBoxApi`'s `Debug` does.
impl fmt::Debug for Started {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Started")
            .field("api_port", &self.api_port)
            .field("api_secret", &"<redacted>")
            .finish()
    }
}

/// What the helper reports unasked, and only to the connection whose
/// `start` is running: sing-box's logs are as private as the files it reads
/// (ADR 0006 rule 4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// One line of sing-box's stdout or stderr. `truncated` when the helper
    /// cut it at [`Limits::max_log_line`].
    Log { line: String, truncated: bool },
    /// sing-box exited.
    Exited(ExitInfo),
}

/// How sing-box exited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExitInfo {
    /// The exit code, if it exited. Windows exit codes are 32-bit unsigned;
    /// they travel as the i32 with the same bits.
    pub code: Option<i32>,
    /// The signal that ended it, on Unix.
    pub signal: Option<i32>,
}

/// Whether sing-box runs, machine-wide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Stopped,
    Starting,
    Running,
}

/// Why a request failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The caller may not do this: `start` or `stop` from a read-only
    /// caller.
    Unauthorized,
    /// The `hello` named another protocol version. The connection ends.
    VersionMismatch,
    /// A sing-box another connection started is running.
    Busy,
    /// The request broke the protocol. The connection ends.
    BadRequest,
    /// The helper failed on its own side.
    Internal,
}

impl ErrorCode {
    /// The code on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::Unauthorized => "unauthorized",
            ErrorCode::VersionMismatch => "version_mismatch",
            ErrorCode::Busy => "busy",
            ErrorCode::BadRequest => "bad_request",
            ErrorCode::Internal => "internal",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A policy refusal on the wire: where, why ([`RefusalCode`]), and the
/// refusal's own data, if it has any.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireRefusal {
    /// JSON Pointer to the refused key; empty for the whole config. At most
    /// [`MAX_REFUSAL_TEXT`] bytes.
    pub pointer: String,
    pub code: RefusalCode,
    /// The refusal's data, as text, at most [`MAX_REFUSAL_TEXT`] bytes:
    /// `"{bytes} > {limit}"` for `too_large`, the depth limit for
    /// `too_deep`, serde_json's message for `invalid_json`, the expected
    /// shape for `malformed` (`object`, `array`, `string`,
    /// `string_or_array`, `plugin_options`), the type for
    /// `type_not_allowed` and `service` (when there is one), the id for
    /// `missing_attachment`. `None` for every other code.
    pub detail: Option<String>,
}

impl From<&Refusal> for WireRefusal {
    fn from(refusal: &Refusal) -> Self {
        let short = |text: &str| Some(shorten(text, MAX_REFUSAL_TEXT));
        let detail = match &refusal.kind {
            RefusalKind::TooLarge { bytes, limit } => Some(format!("{bytes} > {limit}")),
            RefusalKind::TooDeep { limit } => Some(limit.to_string()),
            RefusalKind::InvalidJson(message) => short(message),
            RefusalKind::Malformed { expected } => Some(expected_code(*expected).to_owned()),
            RefusalKind::TypeNotAllowed { type_name } => type_name.as_deref().and_then(short),
            RefusalKind::Service { service_type } => service_type.as_deref().and_then(short),
            RefusalKind::MissingAttachment { id } => short(id),
            RefusalKind::NotAnObject
            | RefusalKind::NonCanonicalKey
            | RefusalKind::UnknownSection
            | RefusalKind::Inbounds
            | RefusalKind::UnknownExperimental
            | RefusalKind::RunsProgram
            | RefusalKind::SystemChange
            | RefusalKind::ServerFileScan
            | RefusalKind::FilesystemPath
            | RefusalKind::Directory
            | RefusalKind::LocalFile
            | RefusalKind::MalformedAttachment => None,
        };
        WireRefusal {
            pointer: shorten(&refusal.pointer, MAX_REFUSAL_TEXT),
            code: RefusalCode::from(&refusal.kind),
            detail,
        }
    }
}

/// The wire name of what a `malformed` place takes.
fn expected_code(expected: Expected) -> &'static str {
    match expected {
        Expected::Object => "object",
        Expected::Array => "array",
        Expected::String => "string",
        Expected::StringOrArray => "string_or_array",
        Expected::PluginOptions => "plugin_options",
    }
}

/// Why the policy refused something: one stable snake_case code per
/// [`RefusalKind`] variant, spelled out here rather than derived from the
/// variant's name, so renaming a variant can't change the wire.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RefusalCode {
    TooLarge,
    TooDeep,
    InvalidJson,
    NotAnObject,
    Malformed,
    NonCanonicalKey,
    UnknownSection,
    TypeNotAllowed,
    Inbounds,
    Service,
    UnknownExperimental,
    RunsProgram,
    SystemChange,
    ServerFileScan,
    FilesystemPath,
    Directory,
    LocalFile,
    MalformedAttachment,
    MissingAttachment,
    /// A code this build doesn't know, from a newer helper's policy. Only
    /// decoding makes one; a known code never decodes to it.
    Other(String),
}

impl RefusalCode {
    /// The code on the wire.
    pub fn as_str(&self) -> &str {
        match self {
            RefusalCode::TooLarge => "too_large",
            RefusalCode::TooDeep => "too_deep",
            RefusalCode::InvalidJson => "invalid_json",
            RefusalCode::NotAnObject => "not_an_object",
            RefusalCode::Malformed => "malformed",
            RefusalCode::NonCanonicalKey => "non_canonical_key",
            RefusalCode::UnknownSection => "unknown_section",
            RefusalCode::TypeNotAllowed => "type_not_allowed",
            RefusalCode::Inbounds => "inbounds",
            RefusalCode::Service => "service",
            RefusalCode::UnknownExperimental => "unknown_experimental",
            RefusalCode::RunsProgram => "runs_program",
            RefusalCode::SystemChange => "system_change",
            RefusalCode::ServerFileScan => "server_file_scan",
            RefusalCode::FilesystemPath => "filesystem_path",
            RefusalCode::Directory => "directory",
            RefusalCode::LocalFile => "local_file",
            RefusalCode::MalformedAttachment => "malformed_attachment",
            RefusalCode::MissingAttachment => "missing_attachment",
            RefusalCode::Other(code) => code,
        }
    }

    /// The code `code` names: a known one, or [`RefusalCode::Other`].
    pub fn from_wire(code: &str) -> Self {
        match code {
            "too_large" => RefusalCode::TooLarge,
            "too_deep" => RefusalCode::TooDeep,
            "invalid_json" => RefusalCode::InvalidJson,
            "not_an_object" => RefusalCode::NotAnObject,
            "malformed" => RefusalCode::Malformed,
            "non_canonical_key" => RefusalCode::NonCanonicalKey,
            "unknown_section" => RefusalCode::UnknownSection,
            "type_not_allowed" => RefusalCode::TypeNotAllowed,
            "inbounds" => RefusalCode::Inbounds,
            "service" => RefusalCode::Service,
            "unknown_experimental" => RefusalCode::UnknownExperimental,
            "runs_program" => RefusalCode::RunsProgram,
            "system_change" => RefusalCode::SystemChange,
            "server_file_scan" => RefusalCode::ServerFileScan,
            "filesystem_path" => RefusalCode::FilesystemPath,
            "directory" => RefusalCode::Directory,
            "local_file" => RefusalCode::LocalFile,
            "malformed_attachment" => RefusalCode::MalformedAttachment,
            "missing_attachment" => RefusalCode::MissingAttachment,
            other => RefusalCode::Other(other.to_owned()),
        }
    }
}

impl From<&RefusalKind> for RefusalCode {
    fn from(kind: &RefusalKind) -> Self {
        match kind {
            RefusalKind::TooLarge { .. } => RefusalCode::TooLarge,
            RefusalKind::TooDeep { .. } => RefusalCode::TooDeep,
            RefusalKind::InvalidJson(_) => RefusalCode::InvalidJson,
            RefusalKind::NotAnObject => RefusalCode::NotAnObject,
            RefusalKind::Malformed { .. } => RefusalCode::Malformed,
            RefusalKind::NonCanonicalKey => RefusalCode::NonCanonicalKey,
            RefusalKind::UnknownSection => RefusalCode::UnknownSection,
            RefusalKind::TypeNotAllowed { .. } => RefusalCode::TypeNotAllowed,
            RefusalKind::Inbounds => RefusalCode::Inbounds,
            RefusalKind::Service { .. } => RefusalCode::Service,
            RefusalKind::UnknownExperimental => RefusalCode::UnknownExperimental,
            RefusalKind::RunsProgram => RefusalCode::RunsProgram,
            RefusalKind::SystemChange => RefusalCode::SystemChange,
            RefusalKind::ServerFileScan => RefusalCode::ServerFileScan,
            RefusalKind::FilesystemPath => RefusalCode::FilesystemPath,
            RefusalKind::Directory => RefusalCode::Directory,
            RefusalKind::LocalFile => RefusalCode::LocalFile,
            RefusalKind::MalformedAttachment => RefusalCode::MalformedAttachment,
            RefusalKind::MissingAttachment { .. } => RefusalCode::MissingAttachment,
        }
    }
}

impl fmt::Display for RefusalCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for RefusalCode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for RefusalCode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let code = Cow::<str>::deserialize(deserializer)?;
        Ok(RefusalCode::from_wire(&code))
    }
}

/// What the helper sends, as JSON: replies and events share one tag space.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum WireToClient {
    Hello(HelloReply),
    Started(Started),
    Refused {
        #[serde(deserialize_with = "seq_of_maps")]
        refusals: Vec<WireRefusal>,
        omitted: u32,
    },
    Stopped {},
    Status {
        state: RunState,
        #[serde(deserialize_with = "option_of_map")]
        last_exit: Option<ExitInfo>,
    },
    Error {
        code: ErrorCode,
        message: String,
    },
    Log {
        line: String,
        truncated: bool,
    },
    Exited(ExitInfo),
}

impl From<ToClient> for WireToClient {
    fn from(message: ToClient) -> Self {
        match message {
            ToClient::Reply(Reply::Hello(hello)) => WireToClient::Hello(hello),
            ToClient::Reply(Reply::Started(started)) => WireToClient::Started(started),
            ToClient::Reply(Reply::Refused { refusals, omitted }) => {
                WireToClient::Refused { refusals, omitted }
            }
            ToClient::Reply(Reply::Stopped) => WireToClient::Stopped {},
            ToClient::Reply(Reply::Status { state, last_exit }) => {
                WireToClient::Status { state, last_exit }
            }
            ToClient::Reply(Reply::Error { code, message }) => {
                WireToClient::Error { code, message }
            }
            ToClient::Event(Event::Log { line, truncated }) => {
                WireToClient::Log { line, truncated }
            }
            ToClient::Event(Event::Exited(exit)) => WireToClient::Exited(exit),
        }
    }
}

impl From<WireToClient> for ToClient {
    fn from(wire: WireToClient) -> Self {
        match wire {
            WireToClient::Hello(hello) => Reply::Hello(hello).into(),
            WireToClient::Started(started) => Reply::Started(started).into(),
            WireToClient::Refused { refusals, omitted } => {
                Reply::Refused { refusals, omitted }.into()
            }
            WireToClient::Stopped {} => Reply::Stopped.into(),
            WireToClient::Status { state, last_exit } => Reply::Status { state, last_exit }.into(),
            WireToClient::Error { code, message } => Reply::Error { code, message }.into(),
            WireToClient::Log { line, truncated } => Event::Log { line, truncated }.into(),
            WireToClient::Exited(exit) => Event::Exited(exit).into(),
        }
    }
}

/// The JSON frame that carries `message` to the GUI. A log line longer than
/// [`Limits::max_log_line`] is cut at a UTF-8 boundary and marked
/// truncated; anything else over [`Limits::max_reply_json`] is an error.
/// Build `refused` and `error` replies with [`Reply::refused`] and
/// [`Reply::error`], which keep them in bounds.
pub fn encode_to_client(message: &ToClient, limits: &Limits) -> Result<Vec<u8>, ProtocolError> {
    let wire = match message {
        ToClient::Event(Event::Log { line, truncated }) => {
            let kept = floor_prefix(line, limits.max_log_line);
            WireToClient::Log {
                line: kept.to_owned(),
                truncated: *truncated || kept.len() < line.len(),
            }
        }
        other => WireToClient::from(other.clone()),
    };
    encode_json(&wire, &limits.to_gui_caps())
}

/// The message in a frame from the helper, which the GUI reads with
/// [`Limits::to_gui_caps`]. The helper sends no blobs.
pub fn decode_to_client(frame: &Frame) -> Result<ToClient, ProtocolError> {
    match frame {
        Frame::Json(text) => serde_json::from_str::<MapOnly<WireToClient>>(text)
            .map(|MapOnly(wire)| ToClient::from(wire))
            .map_err(ProtocolError::invalid_message),
        Frame::Blob(_) => Err(ProtocolError::UnexpectedFrame(FrameType::Blob)),
    }
}

/// `T` from a JSON object only. serde's derived structs and internally
/// tagged enums also take an array of their fields in order: to them
/// `["hello", 1]` is a `hello`, and `[true, 7890, false, true]` is
/// [`TunOptions`]. `deny_unknown_fields` doesn't reach that form, so the
/// protocol refuses it: one spelling per message.
pub(crate) struct MapOnly<T>(pub(crate) T);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for MapOnly<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        map_only(deserializer).map(MapOnly)
    }
}

fn map_only<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<T, D::Error> {
    struct Object<T>(PhantomData<T>);

    impl<'de, T: Deserialize<'de>> Visitor<'de> for Object<T> {
        type Value = T;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a JSON object")
        }

        fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<T, A::Error> {
            T::deserialize(MapAccessDeserializer::new(map))
        }
    }

    deserializer.deserialize_map(Object(PhantomData))
}

fn seq_of_maps<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Vec<T>, D::Error> {
    let items = Vec::<MapOnly<T>>::deserialize(deserializer)?;
    Ok(items.into_iter().map(|MapOnly(item)| item).collect())
}

fn option_of_map<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    let item = Option::<MapOnly<T>>::deserialize(deserializer)?;
    Ok(item.map(|MapOnly(item)| item))
}
