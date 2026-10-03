//! gRPC-Web over HTTP/1.1 on the blocking reqwest client: one POST per call,
//! request and response bodies framed as `flag(1) | length(4, BE) | payload`,
//! the gRPC status in a final trailer frame (or in the HTTP headers for a
//! trailers-only response). Server streams are just more data frames on the
//! same body. Client streaming can't be expressed this way, so those RPCs
//! are out of reach (see the ADR).

use super::SingBoxApi;
use prost::Message;
use reqwest::blocking::{Client, Response};
use reqwest::header::{HeaderValue, AUTHORIZATION};
use std::fmt;
use std::io::{self, ErrorKind, Read};
use std::time::Duration;

/// Fully qualified gRPC service name; every method path is
/// `/daemon.StartedService/<Method>`.
pub(super) const SERVICE_NAME: &str = "daemon.StartedService";

/// Loopback-only API — short timeout so a missing/just-started sing-box fails
/// fast instead of hanging the caller.
const UNARY_TIMEOUT: Duration = Duration::from_secs(2);
/// Dial bound for the long-lived streams, so a not-yet-listening API fails
/// fast (the reader thread then retries).
const STREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Per-read bound for streams that only push on change and may sit idle
/// indefinitely (groups, outbounds, clash mode, logs, connections, service
/// status, notifications, Tailscale status). A timeout there is routine:
/// re-subscribe and take the fresh snapshot. sing-box sends the response
/// headers with the first message, so a stream that has nothing to say at
/// all (notifications) times out before the first byte, every time.
pub const IDLE_STREAM_READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Sanity cap on one gRPC-Web frame — a group snapshot is a few KiB, a
/// connection or log reset snapshot well under a MiB.
const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

/// gRPC status codes sing-box uses (`ApiError::Status::code`). Go handlers
/// that return a plain error (e.g. `os.ErrInvalid` while not started) arrive
/// as `UNKNOWN` with the Go error text as the message.
pub mod grpc_code {
    pub const CANCELLED: u32 = 1;
    pub const UNKNOWN: u32 = 2;
    pub const INVALID_ARGUMENT: u32 = 3;
    pub const DEADLINE_EXCEEDED: u32 = 4;
    pub const NOT_FOUND: u32 = 5;
    pub const PERMISSION_DENIED: u32 = 7;
    pub const FAILED_PRECONDITION: u32 = 9;
    pub const UNIMPLEMENTED: u32 = 12;
    pub const INTERNAL: u32 = 13;
    pub const UNAVAILABLE: u32 = 14;
    pub const UNAUTHENTICATED: u32 = 16;
}

/// Why a call failed. `Display` gives a user-presentable message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApiError {
    /// Could not reach the API at all (not listening yet, sing-box not
    /// running, client setup failed).
    Unreachable(String),
    /// No byte arrived within the read bound — for a unary call, or while
    /// waiting on a stream (before or after its first message). On an idle
    /// stream this is routine; re-subscribe.
    TimedOut,
    /// The connection dropped mid-response, or the body ended without a gRPC
    /// status — typically sing-box exiting under an open stream.
    Disconnected(String),
    /// sing-box answered with a non-OK gRPC status (`grpc_code`).
    Status { code: u32, message: String },
    /// Something that isn't a well-formed gRPC-Web response (HTTP error,
    /// undecodable message, compressed or oversized frame).
    InvalidResponse(String),
}

impl ApiError {
    pub fn is_timeout(&self) -> bool {
        matches!(self, ApiError::TimedOut)
    }

    /// The gRPC status code, when sing-box sent one.
    pub fn code(&self) -> Option<u32> {
        match self {
            ApiError::Status { code, .. } => Some(*code),
            _ => None,
        }
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::Unreachable(reason) => write!(f, "sing-box API unreachable: {}", reason),
            ApiError::TimedOut => write!(f, "sing-box API timed out"),
            ApiError::Disconnected(reason) => write!(f, "sing-box API stream {}", reason),
            ApiError::Status { code, message } if message.is_empty() => {
                write!(f, "sing-box API: {}", grpc_code_name(*code))
            }
            ApiError::Status { message, .. } => write!(f, "sing-box API: {}", message),
            ApiError::InvalidResponse(reason) => {
                write!(f, "Invalid sing-box API response: {}", reason)
            }
        }
    }
}

impl std::error::Error for ApiError {}

impl From<ApiError> for String {
    fn from(error: ApiError) -> Self {
        error.to_string()
    }
}

impl SingBoxApi {
    /// A unary call with the default 2s bound. `R = ()` for methods that
    /// return `google.protobuf.Empty`.
    pub(super) fn unary<R: Message + Default>(
        &self,
        method: &str,
        request: &impl Message,
    ) -> Result<R, ApiError> {
        self.unary_with_timeout(method, request, UNARY_TIMEOUT)
    }

    /// A unary call for methods that do real work before answering.
    pub(super) fn unary_with_timeout<R: Message + Default>(
        &self,
        method: &str,
        request: &impl Message,
        timeout: Duration,
    ) -> Result<R, ApiError> {
        let client = build_client(Client::builder().timeout(timeout))?;
        let mut response = None;
        // Read on to the trailer frame: its status, not the message, decides.
        self.exchange(&client, method, request, |message: R| {
            response.get_or_insert(message);
            true
        })?;
        response.ok_or_else(|| ApiError::InvalidResponse("no response message".into()))
    }

    /// A server-streaming call: decode each message into `on_message` until
    /// the trailer frame (whose gRPC status decides the result) or until
    /// `on_message` returns `false` (→ `Ok`). `read_timeout` bounds the wait
    /// for the response headers and then every read; when it passes the call
    /// returns `ApiError::TimedOut`.
    pub(super) fn stream<M: Message + Default>(
        &self,
        method: &str,
        request: &impl Message,
        read_timeout: Duration,
        on_message: impl FnMut(M) -> bool,
    ) -> Result<(), ApiError> {
        // Set explicitly because the blocking client's 30s default would
        // otherwise govern each read. (A per-request timeout would not do:
        // reqwest turns that into a total deadline for the whole body.)
        let client = build_client(
            Client::builder()
                .connect_timeout(STREAM_CONNECT_TIMEOUT)
                .timeout(read_timeout),
        )?;
        self.exchange(&client, method, request, on_message)
    }

    /// This run's bearer token, marked sensitive so reqwest keeps it out of
    /// its own `Debug` output.
    fn authorization_header(&self) -> Result<HeaderValue, ApiError> {
        let mut value = HeaderValue::from_str(&self.authorization())
            .map_err(|_| ApiError::Unreachable("invalid API secret".into()))?;
        value.set_sensitive(true);
        Ok(value)
    }

    fn exchange<M: Message + Default>(
        &self,
        client: &Client,
        method: &str,
        request: &impl Message,
        mut on_message: impl FnMut(M) -> bool,
    ) -> Result<(), ApiError> {
        let response = client
            .post(self.method_url(method))
            .header(AUTHORIZATION, self.authorization_header()?)
            .header("Content-Type", "application/grpc-web+proto")
            .header("X-Grpc-Web", "1")
            .body(encode_frame(request))
            .send()
            .map_err(|e| {
                if e.is_timeout() {
                    ApiError::TimedOut
                } else {
                    ApiError::Unreachable(root_cause(&e))
                }
            })?;
        if !response.status().is_success() {
            return Err(ApiError::InvalidResponse(format!(
                "HTTP {}",
                response.status()
            )));
        }
        // Trailers-only response (an error before any message): the gRPC
        // status rides in the HTTP headers instead of a trailer frame.
        let header_status = header_status(&response);
        if let Some(status) = &header_status {
            if status.status.is_some_and(|code| code != 0) {
                return status.clone().into_result();
            }
        }

        let mut body = response;
        loop {
            match read_frame(&mut body)? {
                Some(Frame::Data(payload)) => {
                    let message = M::decode(payload.as_slice())
                        .map_err(|e| ApiError::InvalidResponse(e.to_string()))?;
                    if !on_message(message) {
                        return Ok(());
                    }
                }
                Some(Frame::Trailers(trailers)) => {
                    return match (trailers.status, header_status) {
                        (Some(_), _) => trailers.into_result(),
                        (None, Some(header)) => header.into_result(),
                        (None, None) => Err(ApiError::InvalidResponse("no gRPC status".into())),
                    };
                }
                None => return Err(ApiError::Disconnected("closed".into())),
            }
        }
    }
}

/// The innermost error ("Connection refused (os error 111)") — reqwest's
/// own message only says which URL failed.
fn root_cause(error: &(dyn std::error::Error + 'static)) -> String {
    let mut cause = error;
    while let Some(source) = cause.source() {
        cause = source;
    }
    cause.to_string()
}

/// Never through a proxy: the API is on loopback, and the system proxy may
/// well be sing-box itself.
fn build_client(builder: reqwest::blocking::ClientBuilder) -> Result<Client, ApiError> {
    builder
        .no_proxy()
        .build()
        .map_err(|e| ApiError::Unreachable(format!("failed to create HTTP client: {}", e)))
}

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

/// Flag bit marking a trailer (metadata) frame.
const FRAME_TRAILERS: u8 = 0x80;
/// Flag bit marking a compressed message — never requested, so never expected.
const FRAME_COMPRESSED: u8 = 0x01;

#[derive(Debug, PartialEq)]
enum Frame {
    Data(Vec<u8>),
    Trailers(GrpcStatus),
}

/// The gRPC status of a call, from a trailer frame or trailers-only headers.
#[derive(Clone, Debug, Default, PartialEq)]
struct GrpcStatus {
    status: Option<u32>,
    message: String,
}

impl GrpcStatus {
    fn into_result(self) -> Result<(), ApiError> {
        match self.status {
            Some(0) => Ok(()),
            Some(code) => Err(ApiError::Status {
                code,
                message: self.message,
            }),
            None => Err(ApiError::InvalidResponse("no gRPC status".into())),
        }
    }
}

/// Request body: one uncompressed data frame.
pub(super) fn encode_frame(message: &impl Message) -> Vec<u8> {
    let payload = message.encode_to_vec();
    let mut frame = Vec::with_capacity(5 + payload.len());
    frame.push(0);
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    frame
}

/// Read one frame. `Ok(None)` on a clean EOF at a frame boundary.
fn read_frame(reader: &mut impl Read) -> Result<Option<Frame>, ApiError> {
    let mut header = [0u8; 5];
    let mut filled = 0;
    while filled < header.len() {
        match reader.read(&mut header[filled..]) {
            Ok(0) if filled == 0 => return Ok(None),
            Ok(0) => return Err(ApiError::Disconnected("truncated".into())),
            Ok(n) => filled += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(read_error(e)),
        }
    }
    let flags = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if len > MAX_FRAME_LEN {
        return Err(ApiError::InvalidResponse(format!(
            "frame too large: {} bytes",
            len
        )));
    }
    let mut payload = vec![0u8; len];
    reader
        .read_exact(&mut payload)
        .map_err(|e| match e.kind() {
            ErrorKind::UnexpectedEof => ApiError::Disconnected("truncated".into()),
            _ => read_error(e),
        })?;
    if flags & FRAME_TRAILERS != 0 {
        Ok(Some(Frame::Trailers(parse_trailers(&payload))))
    } else if flags & FRAME_COMPRESSED != 0 {
        Err(ApiError::InvalidResponse("compressed frame".into()))
    } else {
        Ok(Some(Frame::Data(payload)))
    }
}

/// A body read error: the blocking client reports its read timeout as an
/// `Other` io::Error wrapping a timed-out `reqwest::Error`.
fn read_error(error: io::Error) -> ApiError {
    let reqwest_timeout = error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<reqwest::Error>())
        .is_some_and(reqwest::Error::is_timeout);
    if reqwest_timeout || matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) {
        ApiError::TimedOut
    } else {
        ApiError::Disconnected(format!("error: {}", error))
    }
}

/// Parse a trailer frame: HTTP/1-style `key: value\r\n` lines, keys in any
/// case.
fn parse_trailers(payload: &[u8]) -> GrpcStatus {
    let text = String::from_utf8_lossy(payload);
    let mut trailers = GrpcStatus::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key.trim().to_ascii_lowercase().as_str() {
            "grpc-status" => trailers.status = value.parse().ok(),
            "grpc-message" => trailers.message = percent_decode(value),
            _ => {}
        }
    }
    trailers
}

fn header_status(response: &Response) -> Option<GrpcStatus> {
    let headers = response.headers();
    let status = headers
        .get("grpc-status")?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok();
    let message = headers
        .get("grpc-message")
        .and_then(|v| v.to_str().ok())
        .map(percent_decode)
        .unwrap_or_default();
    Some(GrpcStatus { status, message })
}

/// `grpc-message` is percent-encoded (UTF-8). Malformed escapes pass through.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(byte) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn grpc_code_name(code: u32) -> String {
    let name = match code {
        grpc_code::CANCELLED => "cancelled",
        grpc_code::UNKNOWN => "unknown error",
        grpc_code::INVALID_ARGUMENT => "invalid argument",
        grpc_code::DEADLINE_EXCEEDED => "deadline exceeded",
        grpc_code::NOT_FOUND => "not found",
        grpc_code::PERMISSION_DENIED => "permission denied",
        grpc_code::FAILED_PRECONDITION => "failed precondition",
        grpc_code::UNIMPLEMENTED => "unimplemented",
        grpc_code::INTERNAL => "internal error",
        grpc_code::UNAVAILABLE => "unavailable",
        grpc_code::UNAUTHENTICATED => "unauthenticated",
        _ => return format!("gRPC status {}", code),
    };
    name.to_string()
}

#[cfg(test)]
mod tests {
    use super::super::pb;
    use super::*;
    use std::io::Cursor;

    fn data_frame(message: &impl Message) -> Vec<u8> {
        encode_frame(message)
    }

    fn trailer_frame(text: &str) -> Vec<u8> {
        let mut frame = vec![FRAME_TRAILERS];
        frame.extend_from_slice(&(text.len() as u32).to_be_bytes());
        frame.extend_from_slice(text.as_bytes());
        frame
    }

    #[test]
    fn request_frame_is_length_prefixed_protobuf() {
        let request = pb::SelectOutboundRequest {
            group_tag: "节点选择".into(),
            outbound_tag: "香港-01".into(),
        };
        let frame = encode_frame(&request);
        assert_eq!(frame[0], 0, "uncompressed data frame");
        let len = u32::from_be_bytes([frame[1], frame[2], frame[3], frame[4]]) as usize;
        assert_eq!(len, frame.len() - 5);
        let decoded = pb::SelectOutboundRequest::decode(&frame[5..]).unwrap();
        assert_eq!(decoded, request);
    }

    #[test]
    fn empty_request_is_a_zero_length_frame() {
        assert_eq!(encode_frame(&()), vec![0, 0, 0, 0, 0]);
    }

    #[test]
    fn frames_read_in_sequence_until_eof() {
        let status = pb::Status {
            uplink: 10,
            downlink: 20,
            ..Default::default()
        };
        let mut body = data_frame(&status);
        body.extend(trailer_frame("grpc-status: 0\r\n"));
        let mut reader = Cursor::new(body);
        match read_frame(&mut reader).unwrap() {
            Some(Frame::Data(payload)) => {
                assert_eq!(pb::Status::decode(payload.as_slice()).unwrap(), status)
            }
            other => panic!("expected data frame, got {:?}", other),
        }
        match read_frame(&mut reader).unwrap() {
            Some(Frame::Trailers(trailers)) => assert_eq!(trailers.into_result(), Ok(())),
            other => panic!("expected trailers, got {:?}", other),
        }
        assert_eq!(read_frame(&mut reader).unwrap(), None);
    }

    #[test]
    fn truncated_or_compressed_frames_are_errors() {
        assert!(matches!(
            read_frame(&mut Cursor::new(vec![0, 0, 0])),
            Err(ApiError::Disconnected(_))
        ));
        assert!(matches!(
            read_frame(&mut Cursor::new(vec![0, 0, 0, 0, 4, 1])),
            Err(ApiError::Disconnected(_))
        ));
        assert!(matches!(
            read_frame(&mut Cursor::new(vec![FRAME_COMPRESSED, 0, 0, 0, 0])),
            Err(ApiError::InvalidResponse(_))
        ));
    }

    /// A reader that fails like the blocking client does when its read bound
    /// passes, after optionally yielding some bytes.
    struct TimingOut(Vec<u8>);

    impl Read for TimingOut {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.0.is_empty() {
                return Err(io::Error::new(ErrorKind::TimedOut, "read timed out"));
            }
            let n = buf.len().min(self.0.len());
            buf[..n].copy_from_slice(&self.0[..n]);
            self.0.drain(..n);
            Ok(n)
        }
    }

    #[test]
    fn read_timeouts_are_told_apart_from_failures() {
        assert_eq!(read_frame(&mut TimingOut(vec![])), Err(ApiError::TimedOut));
        assert_eq!(
            read_frame(&mut TimingOut(vec![0, 0, 0, 0, 9, 1])),
            Err(ApiError::TimedOut),
            "mid-frame too"
        );
        let reset = io::Error::new(ErrorKind::ConnectionReset, "reset");
        assert!(matches!(read_error(reset), ApiError::Disconnected(_)));
        assert!(ApiError::TimedOut.is_timeout());
        assert!(!ApiError::Disconnected("closed".into()).is_timeout());
    }

    #[test]
    fn trailers_parse_case_insensitively_and_decode_message() {
        let trailers = parse_trailers(
            b"Grpc-Status: 5\r\nGrpc-Message: outbound not found: %E9%A6%99%E6%B8%AF\r\n",
        );
        assert_eq!(trailers.status, Some(5));
        let error = trailers.into_result().unwrap_err();
        assert_eq!(error.code(), Some(grpc_code::NOT_FOUND));
        assert_eq!(error.to_string(), "sing-box API: outbound not found: 香港");
    }

    #[test]
    fn nonzero_status_without_message_names_the_code() {
        assert_eq!(
            parse_trailers(b"grpc-status: 3\r\n")
                .into_result()
                .unwrap_err()
                .to_string(),
            "sing-box API: invalid argument"
        );
        assert!(parse_trailers(b"").into_result().is_err());
    }

    #[test]
    fn percent_decode_passes_malformed_escapes_through() {
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("%4"), "%4");
    }

    /// Serve one gRPC-Web call on a loopback port: answer an empty message
    /// and an OK trailer, and hand back the raw request head.
    fn serve_once() -> (u16, std::thread::JoinHandle<String>) {
        use std::io::Write;
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                socket.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            let head = String::from_utf8(request).unwrap();
            let length = head
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            socket.read_exact(&mut vec![0u8; length]).unwrap();
            let mut body = encode_frame(&());
            body.extend(trailer_frame("grpc-status: 0\r\n"));
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: application/grpc-web+proto\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            socket.write_all(&body).unwrap();
            head
        });
        (port, server)
    }

    fn authorization_of(head: &str) -> Option<&str> {
        head.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("authorization")
                .then(|| value.trim())
        })
    }

    /// sing-box only lets through calls bearing the secret its config got.
    #[test]
    fn unary_and_stream_calls_send_the_bearer_secret() {
        let (port, server) = serve_once();
        let api = SingBoxApi::new(port);
        api.unary::<()>("GetVersion", &()).unwrap();
        let head = server.join().unwrap();
        assert!(head.starts_with("POST /daemon.StartedService/GetVersion "));
        assert_eq!(authorization_of(&head), Some(api.authorization().as_str()));

        let (port, server) = serve_once();
        let api = SingBoxApi::new(port);
        api.stream::<()>("SubscribeStatus", &(), Duration::from_secs(5), |_| false)
            .unwrap();
        let head = server.join().unwrap();
        assert_eq!(authorization_of(&head), Some(api.authorization().as_str()));
    }

    #[test]
    fn errors_convert_to_display_strings() {
        let message: String = ApiError::Unreachable("connection refused".into()).into();
        assert_eq!(message, "sing-box API unreachable: connection refused");
        assert_eq!(
            ApiError::Disconnected("closed".into()).to_string(),
            "sing-box API stream closed"
        );
    }
}
