//! The client against a fake helper on the other end of a socket pair,
//! driven by the protocol crate's own server session, so both ends speak
//! the real protocol.

use super::*;
use boxpilot_protocol::{
    encode_to_client, Authority, HelloReply, RefusalCode, ServerSession, Started, TunOptions,
};
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;

/// The client's end: a Unix socket as a [`HelperIo`].
struct SocketIo(UnixStream);

impl HelperIo for SocketIo {
    fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        (&self.0).read(buf)
    }

    fn write_all(&self, buf: &[u8], deadline: Instant) -> io::Result<()> {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        self.0.set_write_timeout(Some(left))?;
        (&self.0).write_all(buf)
    }

    fn close(&self) {
        let _ = self.0.shutdown(Shutdown::Both);
    }
}

/// The helper's end.
struct Fake {
    requests: std_mpsc::Receiver<Request>,
    closed: std_mpsc::Receiver<()>,
    server: UnixStream,
}

impl Fake {
    fn next_request(&self) -> Request {
        self.requests
            .recv_timeout(Duration::from_secs(5))
            .expect("the fake got no request")
    }

    fn wait_closed(&self) {
        self.closed
            .recv_timeout(Duration::from_secs(5))
            .expect("the client never closed the connection");
    }

    /// The helper goes away, as a crash would end the pipe.
    fn crash(&self) {
        let _ = self.server.shutdown(Shutdown::Both);
    }
}

fn frame(message: impl Into<ToClient>) -> Vec<u8> {
    encode_to_client(&message.into(), &Limits::default()).unwrap()
}

/// A helper that answers each request with what `respond` returns (frames,
/// already encoded) and reports each request and the end of the stream.
fn spawn_fake(
    mut respond: impl FnMut(&Request) -> Vec<Vec<u8>> + Send + 'static,
) -> (Arc<dyn HelperIo>, Fake) {
    let (client, server) = UnixStream::pair().unwrap();
    let (requests_tx, requests) = std_mpsc::channel();
    let (closed_tx, closed) = std_mpsc::channel();
    let mut stream = server.try_clone().unwrap();
    thread::spawn(move || {
        let limits = Limits::default();
        let mut session = ServerSession::new(Authority::MayStart, limits);
        let mut decoder = FrameDecoder::new(session.frame_caps());
        let mut buf = vec![0u8; 64 * 1024];
        'serve: loop {
            let mut input = match stream.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => &buf[..n],
            };
            loop {
                input = &input[decoder.feed(input)..];
                let step = match decoder.next_frame() {
                    Ok(Some(frame)) => session.accept(frame),
                    Ok(None) => break,
                    Err(error) => Err(error),
                };
                decoder.set_caps(session.frame_caps());
                match step {
                    Ok(Some(request)) => {
                        session.replied();
                        let out = respond(&request);
                        let _ = requests_tx.send(request);
                        for bytes in out {
                            if stream.write_all(&bytes).is_err() {
                                break 'serve;
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        let _ = stream.write_all(&frame(error.reply()));
                        break 'serve;
                    }
                }
            }
        }
        let _ = closed_tx.send(());
    });
    (
        Arc::new(SocketIo(client)),
        Fake {
            requests,
            closed,
            server,
        },
    )
}

fn hello(may_start: bool, sing_box_version: &str) -> Vec<u8> {
    frame(Reply::Hello(HelloReply {
        protocol_version: PROTOCOL_VERSION,
        helper_version: "0.1.0".into(),
        sing_box_version: sing_box_version.into(),
        sing_box_sha256: "00".repeat(32),
        may_start,
    }))
}

const SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn started() -> Vec<u8> {
    frame(Reply::Started(Started {
        api_port: 40123,
        api_secret: SECRET.into(),
    }))
}

fn log(line: &str) -> Vec<u8> {
    frame(Event::Log {
        line: line.into(),
        truncated: false,
    })
}

fn exited(code: i32) -> Vec<u8> {
    frame(Event::Exited(ExitInfo {
        code: Some(code),
        signal: None,
    }))
}

fn request() -> StartRequest {
    StartRequest {
        config: r#"{"outbounds":[{"type":"direct","tag":"direct"}]}"#.into(),
        attachments: vec![("file-1".into(), b"a CA".to_vec())],
        options: TunOptions {
            ipv6: false,
            proxy_port: 7788,
            allow_lan: false,
            system_proxy: true,
        },
    }
}

/// The helper as it behaves: hello, start, then sing-box's lines; on stop,
/// sing-box's exit and then `stopped`.
fn well_behaved(request: &Request) -> Vec<Vec<u8>> {
    match request {
        Request::Hello { .. } => vec![hello(true, "1.14.2")],
        Request::Start(_) => vec![started(), log("INFO started"), log("INFO tun up")],
        Request::Stop => vec![exited(0), frame(Reply::Stopped)],
        Request::Status => vec![],
    }
}

fn next_event(events: &mut UnboundedReceiver<HelperEvent>) -> Option<HelperEvent> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match events.try_recv() {
            Ok(event) => return Some(event),
            Err(mpsc::TryRecvError::Closed) => return None,
            Err(mpsc::TryRecvError::Empty) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(5))
            }
            Err(mpsc::TryRecvError::Empty) => panic!("no event"),
        }
    }
}

fn failure(
    result: Result<(HelperConnection, SingBoxApi, UnboundedReceiver<HelperEvent>), HelperFailure>,
) -> HelperFailure {
    match result {
        Ok(_) => panic!("the start succeeded"),
        Err(failure) => failure,
    }
}

#[test]
fn a_start_runs_until_it_is_stopped() {
    let (io, fake) = spawn_fake(well_behaved);
    let (connection, api, mut events) = start_session(io, request()).unwrap();

    assert!(
        matches!(fake.next_request(), Request::Hello { protocol_version } if protocol_version == PROTOCOL_VERSION)
    );
    match fake.next_request() {
        Request::Start(start) => assert_eq!(start, request()),
        other => panic!("{other:?}"),
    }
    // The API the helper's sing-box listens on, with its secret, which
    // never shows in a debug print.
    assert_eq!(api, SingBoxApi::from_secret_hex(40123, SECRET).unwrap());
    assert!(!format!("{api:?}").contains(SECRET));

    assert_eq!(
        next_event(&mut events),
        Some(HelperEvent::Log("INFO started".into()))
    );
    assert_eq!(
        next_event(&mut events),
        Some(HelperEvent::Log("INFO tun up".into()))
    );

    connection.stop(Duration::from_secs(5));
    assert_eq!(fake.next_request(), Request::Stop);
    assert_eq!(
        next_event(&mut events),
        Some(HelperEvent::Exited(ExitInfo {
            code: Some(0),
            signal: None
        }))
    );
    assert!(matches!(
        next_event(&mut events),
        Some(HelperEvent::Closed(_))
    ));
    assert_eq!(
        next_event(&mut events),
        None,
        "the channel ends after Closed"
    );
    fake.wait_closed();
}

/// sing-box lives exactly as long as the connection: dropping it closes
/// the stream, which the helper takes as the stop.
#[test]
fn dropping_the_connection_closes_it() {
    let (io, fake) = spawn_fake(well_behaved);
    let (connection, _, _events) = start_session(io, request()).unwrap();
    drop(connection);
    fake.wait_closed();
}

#[test]
fn a_helper_that_goes_away_ends_the_run() {
    let (io, fake) = spawn_fake(well_behaved);
    let (_connection, _, mut events) = start_session(io, request()).unwrap();
    assert!(matches!(next_event(&mut events), Some(HelperEvent::Log(_))));
    assert!(matches!(next_event(&mut events), Some(HelperEvent::Log(_))));
    fake.crash();
    assert!(matches!(
        next_event(&mut events),
        Some(HelperEvent::Closed(_))
    ));
}

#[test]
fn a_caller_that_may_not_start_is_told_so() {
    let (io, fake) = spawn_fake(|request| match request {
        Request::Hello { .. } => vec![hello(false, "1.14.2")],
        _ => panic!("asked more than hello"),
    });
    assert_eq!(
        failure(start_session(io, request())),
        HelperFailure::NotAllowed
    );
    fake.wait_closed();
    assert_eq!(
        HelperFailure::NotAllowed.message(),
        crate::i18n::EN.helper.not_allowed
    );
}

#[test]
fn a_helper_sing_box_without_the_api_service_is_refused() {
    let (io, _fake) = spawn_fake(|_| vec![hello(true, "1.13.9")]);
    assert_eq!(
        failure(start_session(io, request())),
        HelperFailure::SingBoxTooOld("1.13.9".into())
    );
}

#[test]
fn the_helpers_refusals_come_back_whole() {
    let refusals = vec![WireRefusal {
        pointer: "/outbounds/0/type".into(),
        code: RefusalCode::RunsProgram,
        detail: None,
    }];
    let reply = Reply::Refused {
        refusals: refusals.clone(),
        omitted: 3,
    };
    let (io, _fake) = spawn_fake(move |request| match request {
        Request::Hello { .. } => vec![hello(true, "1.14.2")],
        _ => vec![frame(reply.clone())],
    });
    let failure = failure(start_session(io, request()));
    assert_eq!(
        failure,
        HelperFailure::Refused {
            refusals,
            omitted: 3
        }
    );
    assert!(failure
        .message()
        .contains("/outbounds/0/type runs a program; and 3 more"));
}

#[test]
fn error_replies_keep_their_code() {
    for code in [
        ErrorCode::VersionMismatch,
        ErrorCode::Busy,
        ErrorCode::Unauthorized,
    ] {
        let (io, _fake) = spawn_fake(move |request| match request {
            Request::Hello { .. } if code == ErrorCode::VersionMismatch => {
                vec![frame(Reply::error(code, "no"))]
            }
            Request::Hello { .. } => vec![hello(true, "1.14.2")],
            _ => vec![frame(Reply::error(code, "no"))],
        });
        assert_eq!(
            failure(start_session(io, request())),
            HelperFailure::Error {
                code,
                message: "no".into()
            }
        );
    }
}

#[test]
fn a_reply_that_breaks_the_protocol_is_a_bad_reply() {
    // A frame type the protocol doesn't have.
    let (io, fake) = spawn_fake(|_| vec![vec![0, 0, 0, 1, 0x7f, b'x']]);
    assert!(matches!(
        failure(start_session(io, request())),
        HelperFailure::BadReply(_)
    ));
    fake.wait_closed();

    // JSON that is no message.
    let (io, _fake) = spawn_fake(|_| vec![vec![0, 0, 0, 2, 0x01, b'{', b'}']]);
    assert!(matches!(
        failure(start_session(io, request())),
        HelperFailure::BadReply(_)
    ));
}

#[test]
fn a_secret_that_is_not_lowercase_hex_is_a_bad_reply() {
    let (io, _fake) = spawn_fake(|request| match request {
        Request::Hello { .. } => vec![hello(true, "1.14.2")],
        _ => vec![frame(Reply::Started(Started {
            api_port: 40123,
            api_secret: "not hex".into(),
        }))],
    });
    assert!(matches!(
        failure(start_session(io, request())),
        HelperFailure::BadReply(_)
    ));
}

#[test]
fn a_connection_that_ends_before_the_reply_is_lost() {
    let (io, fake) = spawn_fake(|_| vec![]);
    let (connection, _events) = HelperConnection::new(io);
    let waiting = thread::spawn(move || connection.hello());
    fake.next_request();
    fake.crash();
    assert_eq!(waiting.join().unwrap().unwrap_err(), HelperFailure::Lost);
}
