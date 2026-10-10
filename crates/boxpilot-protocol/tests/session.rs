//! The helper's session: every rule, each with the exact error and the code
//! the helper answers it with.

mod common;

use boxpilot_policy::is_attachment_id;
use boxpilot_protocol::{
    encode_request, Authority, ErrorCode, Frame, FrameCaps, FrameType, Limits, ProtocolError,
    Reply, Request, ServerSession, StartRequest,
};
use common::{blob, blob_bytes, json, json_bytes, options, start_json, Connection, HELLO};

const MIB: usize = 1024 * 1024;

fn session(authority: Authority) -> ServerSession {
    ServerSession::new(authority, Limits::default())
}

/// A session past its `hello`, answered.
fn greeted_with(authority: Authority, limits: Limits) -> ServerSession {
    let mut session = ServerSession::new(authority, limits);
    assert_eq!(
        session.accept(json(HELLO)),
        Ok(Some(Request::Hello {
            protocol_version: 1
        }))
    );
    session.replied();
    session
}

fn greeted(authority: Authority) -> ServerSession {
    greeted_with(authority, Limits::default())
}

/// Accept `frame` and expect `error`, answered with `code`; the session is
/// finished after it.
fn refuses(session: &mut ServerSession, frame: Frame, error: ProtocolError, code: ErrorCode) {
    assert_eq!(session.accept(frame), Err(error.clone()));
    assert_eq!(error.code(), code);
    assert!(session.is_finished());
}

/// A `start` header declaring `config` and `attachments`, then the config's
/// blob: what the config blob gave.
fn begin(
    session: &mut ServerSession,
    config: &str,
    attachments: &[(&str, u64)],
) -> Result<Option<Request>, ProtocolError> {
    let header = start_json(config.len() as u64, attachments);
    assert_eq!(
        session.accept(json(&header)),
        Ok(None),
        "the config is owed"
    );
    session.accept(blob(config.as_bytes()))
}

fn start(config: &str, attachments: &[(&str, &[u8])]) -> Request {
    Request::Start(StartRequest {
        config: config.into(),
        attachments: attachments
            .iter()
            .map(|(id, data)| (id.to_string(), data.to_vec()))
            .collect(),
        options: options(),
    })
}

// ---- hello ----

#[test]
fn a_whole_conversation() {
    let mut session = session(Authority::MayStart);
    assert_eq!(session.authority(), Authority::MayStart);
    assert!(session.authority().may_start());
    assert_eq!(
        session.accept(json(HELLO)),
        Ok(Some(Request::Hello {
            protocol_version: 1
        }))
    );
    session.replied();
    assert_eq!(
        session.accept(json(r#"{"type":"status"}"#)),
        Ok(Some(Request::Status))
    );
    session.replied();
    assert_eq!(begin(&mut session, "{}", &[]), Ok(Some(start("{}", &[]))));
    session.replied();
    assert_eq!(
        session.accept(json(r#"{"type":"stop"}"#)),
        Ok(Some(Request::Stop))
    );
    session.replied();
    assert!(!session.is_finished());
}

#[test]
fn hello_must_come_first() {
    for first in [
        r#"{"type":"status"}"#.to_owned(),
        r#"{"type":"stop"}"#.to_owned(),
        start_json(2, &[]),
    ] {
        refuses(
            &mut session(Authority::MayStart),
            json(&first),
            ProtocolError::HelloFirst,
            ErrorCode::BadRequest,
        );
    }
    refuses(
        &mut session(Authority::MayStart),
        blob(b"x"),
        ProtocolError::UnexpectedFrame(FrameType::Blob),
        ErrorCode::BadRequest,
    );
}

#[test]
fn a_second_hello_is_refused() {
    refuses(
        &mut greeted(Authority::MayStart),
        json(HELLO),
        ProtocolError::RepeatedHello,
        ErrorCode::BadRequest,
    );
    refuses(
        &mut greeted(Authority::ReadOnly),
        json(r#"{"type":"hello","protocol_version":2}"#),
        ProtocolError::RepeatedHello,
        ErrorCode::BadRequest,
    );
}

#[test]
fn another_protocol_version_is_refused() {
    for client in [0, 2, u32::MAX] {
        let hello = format!(r#"{{"type":"hello","protocol_version":{client}}}"#);
        let error = ProtocolError::VersionMismatch { helper: 1, client };
        refuses(
            &mut session(Authority::MayStart),
            json(&hello),
            error.clone(),
            ErrorCode::VersionMismatch,
        );
        assert_eq!(
            error.reply(),
            Reply::Error {
                code: ErrorCode::VersionMismatch,
                message: format!("the helper speaks protocol version 1, the client {client}"),
            }
        );
    }
}

// ---- One request at a time ----

#[test]
fn a_request_before_the_reply_is_refused() {
    let mut session = session(Authority::MayStart);
    assert!(session.accept(json(HELLO)).unwrap().is_some());
    refuses(
        &mut session,
        json(r#"{"type":"status"}"#),
        ProtocolError::RequestOutstanding,
        ErrorCode::BadRequest,
    );

    let mut session = greeted(Authority::MayStart);
    assert_eq!(begin(&mut session, "{}", &[("a", 1)]), Ok(None));
    assert!(session.accept(blob(b"x")).unwrap().is_some());
    refuses(
        &mut session,
        json(r#"{"type":"stop"}"#),
        ProtocolError::RequestOutstanding,
        ErrorCode::BadRequest,
    );
}

/// Even an unparsable frame is one request too many: it isn't parsed.
#[test]
fn anything_before_the_reply_is_refused_unparsed() {
    let mut session = session(Authority::MayStart);
    assert!(session.accept(json(HELLO)).unwrap().is_some());
    refuses(
        &mut session,
        json("not json"),
        ProtocolError::RequestOutstanding,
        ErrorCode::BadRequest,
    );
}

#[test]
fn replied_with_nothing_outstanding_changes_nothing() {
    let mut session = greeted(Authority::MayStart);
    session.replied();
    session.replied();
    assert_eq!(
        session.accept(json(r#"{"type":"status"}"#)),
        Ok(Some(Request::Status))
    );
    refuses(
        &mut session,
        json(r#"{"type":"status"}"#),
        ProtocolError::RequestOutstanding,
        ErrorCode::BadRequest,
    );
}

// ---- Authority ----

#[test]
fn read_only_may_say_hello_and_ask_status() {
    let mut session = greeted(Authority::ReadOnly);
    assert!(!session.authority().may_start());
    for _ in 0..3 {
        assert_eq!(
            session.accept(json(r#"{"type":"status"}"#)),
            Ok(Some(Request::Status))
        );
        session.replied();
    }
}

#[test]
fn read_only_may_not_start_or_stop() {
    for request in [
        r#"{"type":"stop"}"#.to_owned(),
        start_json(2, &[]),
        start_json(2, &[("ca", 3)]),
    ] {
        refuses(
            &mut greeted(Authority::ReadOnly),
            json(&request),
            ProtocolError::Unauthorized,
            ErrorCode::Unauthorized,
        );
    }
}

/// Authority is checked before the header's rules: a read-only caller
/// learns nothing about which headers would pass.
#[test]
fn authority_comes_before_the_start_rules() {
    let bad = start_json(0, &[("../etc/passwd", 3), ("a", u64::MAX)]);
    refuses(
        &mut greeted(Authority::ReadOnly),
        json(&bad),
        ProtocolError::Unauthorized,
        ErrorCode::Unauthorized,
    );
}

#[test]
fn unauthorized_is_answered_with_its_code() {
    let reply = ProtocolError::Unauthorized.reply();
    let Reply::Error { code, message } = reply else {
        panic!("not an error reply");
    };
    assert_eq!(code, ErrorCode::Unauthorized);
    assert!(message.contains("only `hello` and `status`"), "{message}");
}

// ---- start: the config ----

/// The config always travels as the first blob, so even a `start` without
/// attachments owes one.
#[test]
fn start_without_attachments_owes_the_config() {
    let mut session = greeted(Authority::MayStart);
    let config = r#"{"outbounds":[{"type":"direct","tag":"direct"}]}"#;
    assert_eq!(
        session.accept(json(&start_json(config.len() as u64, &[]))),
        Ok(None)
    );
    assert_eq!(
        session.frame_caps(),
        FrameCaps {
            max_json: None,
            max_blob: Some(config.len())
        }
    );
    assert_eq!(
        session.accept(blob(config.as_bytes())),
        Ok(Some(start(config, &[])))
    );
}

#[test]
fn an_empty_config_is_refused() {
    refuses(
        &mut greeted(Authority::MayStart),
        json(&start_json(0, &[("a", 1)])),
        ProtocolError::EmptyConfig,
        ErrorCode::BadRequest,
    );
}

#[test]
fn a_short_config_blob_is_refused() {
    let mut session = greeted(Authority::MayStart);
    assert_eq!(session.accept(json(&start_json(10, &[]))), Ok(None));
    refuses(
        &mut session,
        blob(b"{}"),
        ProtocolError::ConfigLength {
            declared: 10,
            received: 2,
        },
        ErrorCode::BadRequest,
    );
}

#[test]
fn a_long_config_blob_is_refused() {
    let mut session = greeted(Authority::MayStart);
    assert_eq!(session.accept(json(&start_json(2, &[("a", 3)]))), Ok(None));
    refuses(
        &mut session,
        blob(b"{} "),
        ProtocolError::ConfigLength {
            declared: 2,
            received: 3,
        },
        ErrorCode::BadRequest,
    );
}

/// The config must be UTF-8, and it is checked as soon as it arrives,
/// before any attachment is taken.
#[test]
fn a_config_that_isnt_utf8_is_refused() {
    let cases: [&[u8]; 4] = [
        b"{\xff}",
        b"{\"a\":\"\xc3\"}",
        b"\"\xed\xa0\x80\"",
        b"{\"a\":1}\x80",
    ];
    for config in cases {
        let mut session = greeted(Authority::MayStart);
        let header = start_json(config.len() as u64, &[("a", 1)]);
        assert_eq!(session.accept(json(&header)), Ok(None));
        refuses(
            &mut session,
            blob(config),
            ProtocolError::ConfigNotUtf8,
            ErrorCode::BadRequest,
        );
    }
}

/// The 32 MiB budget is exact: the config travels raw, so a config of
/// exactly 32 MiB passes, and one byte more is refused from the header.
#[test]
fn a_32_mib_config_passes_and_one_byte_more_is_refused_from_the_header() {
    let limits = Limits::default();
    assert_eq!(limits.max_start_total, 32 * MIB);
    let config = format!("{{}}{}", " ".repeat(32 * MIB - 2));
    let request = start(&config, &[]);

    let mut bytes = encode_request(&Request::hello(), &limits).unwrap();
    bytes.extend(encode_request(&request, &limits).unwrap());
    for mut connection in [
        Connection::new(Authority::MayStart, limits),
        Connection::wide(Authority::MayStart, limits),
    ] {
        assert_eq!(
            connection.feed(&bytes),
            [Ok(Request::hello()), Ok(request.clone())]
        );
    }

    let over = ProtocolError::StartTooLarge {
        bytes: 32 * MIB as u64 + 1,
        limit: 32 * MIB,
    };
    refuses(
        &mut greeted(Authority::MayStart),
        json(&start_json(32 * MIB as u64 + 1, &[])),
        over.clone(),
        ErrorCode::BadRequest,
    );
    let config = format!("{config} ");
    assert_eq!(encode_request(&start(&config, &[]), &limits), Err(over));
}

// ---- start: the attachments ----

#[test]
fn start_with_one_attachment() {
    let mut session = greeted(Authority::MayStart);
    assert_eq!(begin(&mut session, "{}", &[("ca", 5)]), Ok(None));
    assert_eq!(
        session.accept(blob(b"-----")),
        Ok(Some(start("{}", &[("ca", b"-----")])))
    );
}

#[test]
fn start_with_several_attachments_keeps_their_order() {
    let mut session = greeted(Authority::MayStart);
    let declared = [("geoip-cn", 4), ("client_key", 0), ("Root-CA", 3)];
    assert_eq!(begin(&mut session, "{}", &declared), Ok(None));
    assert_eq!(session.accept(blob(b"\0\x01\x02\xff")), Ok(None));
    assert_eq!(session.accept(blob(b"")), Ok(None));
    let request = session.accept(blob(b"pem")).unwrap().unwrap();
    assert_eq!(
        request,
        start(
            "{}",
            &[
                ("geoip-cn", b"\0\x01\x02\xff"),
                ("client_key", b""),
                ("Root-CA", b"pem")
            ]
        )
    );
    let Request::Start(start) = request else {
        unreachable!()
    };
    assert_eq!(
        start.attachment_ids().into_iter().collect::<Vec<_>>(),
        ["Root-CA", "client_key", "geoip-cn"]
    );
}

/// The id rule is the policy's own `is_attachment_id`, at every position.
#[test]
fn attachment_ids_follow_the_policy_rule() {
    let long_ok = "a".repeat(64);
    let too_long = "a".repeat(65);
    let candidates = [
        "a",
        "A-z_0-9",
        long_ok.as_str(),
        "",
        too_long.as_str(),
        "a/b",
        "a\\b",
        "..",
        "a.srs",
        "a b",
        "é",
        "CON",
        "a\u{0}",
        "ſ",
    ];
    let mut passed = 0;
    for candidate in candidates {
        for index in 0..3 {
            let mut ids: Vec<(String, u64)> = (0..3).map(|i| (format!("ok{i}"), 1)).collect();
            ids[index].0 = candidate.to_owned();
            let declared: Vec<(&str, u64)> = ids.iter().map(|(id, n)| (id.as_str(), *n)).collect();
            let mut session = greeted(Authority::MayStart);
            let result = session.accept(json(&start_json(2, &declared)));
            if is_attachment_id(candidate) {
                assert_eq!(result, Ok(None), "{candidate:?}");
                passed += 1;
            } else {
                assert_eq!(
                    result,
                    Err(ProtocolError::InvalidAttachmentId { index }),
                    "{candidate:?}"
                );
                assert_eq!(
                    ProtocolError::InvalidAttachmentId { index }.code(),
                    ErrorCode::BadRequest
                );
            }
        }
    }
    assert_eq!(
        passed,
        3 * 4,
        "CON is a valid id: the helper never uses ids as file names"
    );
}

#[test]
fn duplicate_ids_are_refused() {
    refuses(
        &mut greeted(Authority::MayStart),
        json(&start_json(2, &[("a", 1), ("b", 1), ("a", 1)])),
        ProtocolError::DuplicateAttachmentId { index: 2 },
        ErrorCode::BadRequest,
    );
}

/// Ids differ by case: the helper hex-encodes them into file names, so
/// they can't collide on a case-insensitive file system.
#[test]
fn ids_are_case_sensitive() {
    let mut session = greeted(Authority::MayStart);
    assert_eq!(
        session.accept(json(&start_json(2, &[("a", 1), ("A", 1)]))),
        Ok(None)
    );
}

#[test]
fn at_most_64_attachments() {
    let ids: Vec<String> = (0..65).map(|i| format!("a{i}")).collect();
    let declared: Vec<(&str, u64)> = ids.iter().map(|id| (id.as_str(), 0)).collect();

    let mut session = greeted(Authority::MayStart);
    assert_eq!(
        session.accept(json(&start_json(2, &declared[..64]))),
        Ok(None)
    );

    refuses(
        &mut greeted(Authority::MayStart),
        json(&start_json(2, &declared)),
        ProtocolError::TooManyAttachments {
            count: 65,
            limit: 64,
        },
        ErrorCode::BadRequest,
    );
}

/// The total is checked from the declared lengths when the header
/// arrives, before a single blob is taken.
#[test]
fn the_declared_total_is_checked_before_any_blob() {
    refuses(
        &mut greeted(Authority::MayStart),
        json(&start_json(2, &[("rules", 32 * MIB as u64)])),
        ProtocolError::StartTooLarge {
            bytes: 32 * MIB as u64 + 2,
            limit: 32 * MIB,
        },
        ErrorCode::BadRequest,
    );
    refuses(
        &mut greeted(Authority::MayStart),
        json(&start_json(
            2,
            &[("a", 16 * MIB as u64), ("b", 16 * MIB as u64)],
        )),
        ProtocolError::StartTooLarge {
            bytes: 32 * MIB as u64 + 2,
            limit: 32 * MIB,
        },
        ErrorCode::BadRequest,
    );
}

fn small_limits() -> Limits {
    Limits {
        max_start_total: 100,
        max_blob: 60,
        ..Limits::default()
    }
}

#[test]
fn the_total_counts_the_config_and_is_inclusive() {
    // 40 + 60 = 100: at the limit.
    let mut session = greeted_with(Authority::MayStart, small_limits());
    assert_eq!(
        session.accept(json(&start_json(40, &[("a", 60)]))),
        Ok(None)
    );
    // 40 + 30 + 31 = 101.
    refuses(
        &mut greeted_with(Authority::MayStart, small_limits()),
        json(&start_json(40, &[("a", 30), ("b", 31)])),
        ProtocolError::StartTooLarge {
            bytes: 101,
            limit: 100,
        },
        ErrorCode::BadRequest,
    );
    // The config alone: it may use the whole budget, past `max_blob`.
    let mut session = greeted_with(Authority::MayStart, small_limits());
    assert_eq!(session.accept(json(&start_json(100, &[]))), Ok(None));
    refuses(
        &mut greeted_with(Authority::MayStart, small_limits()),
        json(&start_json(101, &[])),
        ProtocolError::StartTooLarge {
            bytes: 101,
            limit: 100,
        },
        ErrorCode::BadRequest,
    );
}

#[test]
fn one_attachment_over_the_blob_cap_is_refused() {
    refuses(
        &mut greeted_with(Authority::MayStart, small_limits()),
        json(&start_json(2, &[("a", 1), ("b", 61)])),
        ProtocolError::AttachmentTooLarge {
            index: 1,
            len: 61,
            limit: 60,
        },
        ErrorCode::BadRequest,
    );
    refuses(
        &mut greeted(Authority::MayStart),
        json(&start_json(2, &[("a", u64::MAX)])),
        ProtocolError::AttachmentTooLarge {
            index: 0,
            len: u64::MAX,
            limit: 32 * MIB,
        },
        ErrorCode::BadRequest,
    );
}

/// Declared lengths that would overflow a sum saturate instead.
#[test]
fn huge_declared_lengths_do_not_overflow() {
    let limits = Limits {
        max_blob: usize::MAX,
        max_start_total: 100,
        ..Limits::default()
    };
    refuses(
        &mut greeted_with(Authority::MayStart, limits),
        json(&start_json(u64::MAX, &[("a", u64::MAX), ("b", u64::MAX)])),
        ProtocolError::StartTooLarge {
            bytes: u64::MAX,
            limit: 100,
        },
        ErrorCode::BadRequest,
    );
}

#[test]
fn a_short_blob_is_refused() {
    let mut session = greeted(Authority::MayStart);
    assert_eq!(begin(&mut session, "{}", &[("a", 2), ("b", 4)]), Ok(None));
    assert_eq!(session.accept(blob(b"ok")), Ok(None));
    refuses(
        &mut session,
        blob(b"abc"),
        ProtocolError::BlobLength {
            index: 1,
            declared: 4,
            received: 3,
        },
        ErrorCode::BadRequest,
    );
}

#[test]
fn a_long_blob_is_refused() {
    let mut session = greeted(Authority::MayStart);
    assert_eq!(begin(&mut session, "{}", &[("a", 2)]), Ok(None));
    refuses(
        &mut session,
        blob(b"abc"),
        ProtocolError::BlobLength {
            index: 0,
            declared: 2,
            received: 3,
        },
        ErrorCode::BadRequest,
    );
}

#[test]
fn a_blob_nobody_declared_is_refused() {
    refuses(
        &mut greeted(Authority::MayStart),
        blob(b""),
        ProtocolError::UnexpectedFrame(FrameType::Blob),
        ErrorCode::BadRequest,
    );
    // One more than the start declared.
    let mut session = greeted(Authority::MayStart);
    assert_eq!(begin(&mut session, "{}", &[("a", 1)]), Ok(None));
    assert!(session.accept(blob(b"x")).unwrap().is_some());
    session.replied();
    refuses(
        &mut session,
        blob(b"y"),
        ProtocolError::UnexpectedFrame(FrameType::Blob),
        ErrorCode::BadRequest,
    );
}

#[test]
fn json_while_blobs_are_owed_is_refused() {
    // The config owed.
    let mut session = greeted(Authority::MayStart);
    assert_eq!(session.accept(json(&start_json(2, &[]))), Ok(None));
    refuses(
        &mut session,
        json(r#"{"type":"stop"}"#),
        ProtocolError::UnexpectedFrame(FrameType::Json),
        ErrorCode::BadRequest,
    );
    // An attachment owed.
    let mut session = greeted(Authority::MayStart);
    assert_eq!(begin(&mut session, "{}", &[("a", 1), ("b", 1)]), Ok(None));
    assert_eq!(session.accept(blob(b"x")), Ok(None));
    refuses(
        &mut session,
        json(r#"{"type":"stop"}"#),
        ProtocolError::UnexpectedFrame(FrameType::Json),
        ErrorCode::BadRequest,
    );
}

#[test]
fn proxy_port_zero_is_refused() {
    let start = start_json(2, &[]).replace("7890", "0");
    refuses(
        &mut greeted(Authority::MayStart),
        json(&start),
        ProtocolError::ZeroProxyPort,
        ErrorCode::BadRequest,
    );
}

// ---- JSON frame sizes ----

/// The largest header this crate's encoder can write: 64 attachments, each
/// a 64-byte id and a 20-digit length, and a 20-digit `config_len`. It
/// fits `max_start_json` (6,614 bytes), so a header is refused for what it
/// says, never for its size.
#[test]
fn the_largest_start_header_fits_its_cap() {
    let limits = Limits::default();
    assert_eq!(limits.max_start_json(), 6614);
    let ids: Vec<String> = (0..64).map(|i| format!("{i:0>64}")).collect();
    let declared: Vec<(&str, u64)> = ids.iter().map(|id| (id.as_str(), u64::MAX)).collect();
    let header = start_json(u64::MAX, &declared).replace(
        r#"{"ipv6":true,"proxy_port":7890,"allow_lan":false,"system_proxy":true}"#,
        r#"{"ipv6":false,"proxy_port":65535,"allow_lan":false,"system_proxy":false}"#,
    );
    assert_eq!(header.len(), 6614 - 1, "no comma after the last entry");
    // Past the size check, refused for what it says.
    refuses(
        &mut greeted(Authority::MayStart),
        json(&header),
        ProtocolError::AttachmentTooLarge {
            index: 0,
            len: u64::MAX,
            limit: 32 * MIB,
        },
        ErrorCode::BadRequest,
    );
    // The largest one that passes.
    let declared: Vec<(&str, u64)> = ids.iter().map(|id| (id.as_str(), 500_000)).collect();
    let mut session = greeted(Authority::MayStart);
    assert_eq!(
        session.accept(json(&start_json(1_000_000, &declared))),
        Ok(None)
    );
}

#[test]
fn the_start_header_cap_follows_max_attachments() {
    let entry = 101;
    for (attachments, cap) in [(0, 4096), (40, 150 + 40 * entry), (64, 6614), (100, 10_250)] {
        let limits = Limits {
            max_attachments: attachments,
            ..Limits::default()
        };
        assert_eq!(limits.max_start_json(), cap, "{attachments}");
    }
}

/// The session holds JSON to the caps itself, before parsing, whether or
/// not the decoder did: a `start` header to `max_start_json`, anything else
/// to `max_control_json`.
#[test]
fn json_frames_are_held_to_their_caps_before_parsing() {
    let too_large = |len, limit| ProtocolError::FrameTooLarge {
        frame_type: FrameType::Json,
        len,
        limit,
    };
    // A header past its cap, padded.
    let header = format!("{}{}", " ".repeat(6615), start_json(2, &[]));
    refuses(
        &mut greeted(Authority::MayStart),
        json(&header),
        too_large(header.len(), 6614),
        ErrorCode::BadRequest,
    );
    // Anything else past 4 KiB, though under the header's cap.
    let status = format!("{}{}", " ".repeat(5000), r#"{"type":"status"}"#);
    refuses(
        &mut greeted(Authority::MayStart),
        json(&status),
        too_large(status.len(), 4096),
        ErrorCode::BadRequest,
    );
    // A header past 4 KiB from a read-only caller, or before hello.
    let header = format!("{}{}", " ".repeat(5000), start_json(2, &[]));
    refuses(
        &mut greeted(Authority::ReadOnly),
        json(&header),
        too_large(header.len(), 4096),
        ErrorCode::BadRequest,
    );
    let hello = format!("{}{HELLO}", " ".repeat(5000));
    refuses(
        &mut session(Authority::MayStart),
        json(&hello),
        too_large(hello.len(), 4096),
        ErrorCode::BadRequest,
    );
    // A megabyte is refused by its size, not parsed.
    let tag = "x".repeat(MIB);
    let text = format!(r#"{{"type":"{tag}"}}"#);
    refuses(
        &mut greeted(Authority::MayStart),
        json(&text),
        too_large(text.len(), 6614),
        ErrorCode::BadRequest,
    );
}

// ---- Malformed messages ----

/// Each is refused as `InvalidMessage`, before and after `hello`.
#[test]
fn malformed_messages_are_refused() {
    let before_hello = [
        "{",
        "nul",
        "[]",
        "\"hello\"",
        "1",
        r#"{"type":"hello","protocol_version":1} {}"#,
        r#"{"protocol_version":1}"#,
        r#"{"type":"Hello","protocol_version":1}"#,
        r#"{"type":"hello"}"#,
        r#"{"type":"hello","protocol_version":1,"may_start":true}"#,
        r#"{"type":"hello","protocol_version":1,"protocol_version":1}"#,
        r#"{"type":"hello","type":"hello","protocol_version":1}"#,
        r#"{"type":"hello","protocol_version":-1}"#,
        r#"{"type":"hello","protocol_version":4294967296}"#,
        r#"{"type":"hello","protocol_version":1.0}"#,
        r#"{"type":"hello","protocol_version":"1"}"#,
        r#"{"type":null,"protocol_version":1}"#,
        "\u{feff}{\"type\":\"hello\",\"protocol_version\":1}",
        // serde's derive would take the array of a message's fields.
        r#"["hello",1]"#,
        r#"[{"type":"hello","protocol_version":1}]"#,
    ];
    for text in before_hello {
        let mut session = session(Authority::MayStart);
        let result = session.accept(json(text));
        assert!(
            matches!(result, Err(ProtocolError::InvalidMessage(_))),
            "{text}: {result:?}"
        );
        assert_eq!(result.unwrap_err().code(), ErrorCode::BadRequest);
    }

    let good_start = start_json(2, &[("a", 1)]);
    let after_hello = [
        r#"{"type":"logs"}"#.to_owned(),
        r#"{"type":"status","verbose":true}"#.to_owned(),
        r#"{"type":"stop","force":true}"#.to_owned(),
        r#"{"type":"stop","type":"status"}"#.to_owned(),
        good_start.replace(r#""options""#, r#""binary_path":"/bin/sh","options""#),
        good_start.replace(r#""config_len":2"#, r#""config":"{}","config_len":2"#),
        good_start.replace(
            r#""system_proxy":true"#,
            r#""system_proxy":true,"tun_name":"x""#,
        ),
        good_start.replace(r#","system_proxy":true"#, ""),
        good_start.replace(r#""len":1"#, r#""len":1,"path":"/etc/shadow""#),
        good_start.replace(r#""len":1"#, r#""len":-1"#),
        good_start.replace(r#""len":1"#, r#""len":1.5"#),
        good_start.replace(r#""len":1"#, r#""len":18446744073709551616"#),
        good_start.replace("7890", "65536"),
        good_start.replace(r#""config_len":2"#, r#""config_len":null"#),
        good_start.replace(r#""config_len":2"#, r#""config_len":"2""#),
        good_start.replace(r#""config_len":2"#, r#""config_len":-2"#),
        good_start.replace(r#""config_len":2"#, r#""config_len":2.0"#),
        good_start.replace(r#""config_len":2,"#, ""),
        good_start.replace(r#""ipv6":true"#, r#""ipv6":"true""#),
        good_start.replace(r#""ipv6":true"#, r#""ipv6":true,"ipv6":true"#),
        r#"["stop"]"#.to_owned(),
        r#"["status"]"#.to_owned(),
        good_start.replace(r#"{"id":"a","len":1}"#, r#"["a",1]"#),
        good_start.replace(
            r#"{"ipv6":true,"proxy_port":7890,"allow_lan":false,"system_proxy":true}"#,
            "[true,7890,false,true]",
        ),
        format!("{}{}", "[".repeat(200), "]".repeat(200)),
    ];
    for text in after_hello {
        let mut session = greeted(Authority::MayStart);
        let result = session.accept(json(&text));
        assert!(
            matches!(result, Err(ProtocolError::InvalidMessage(_))),
            "{text}: {result:?}"
        );
    }
}

/// serde_json quotes what it couldn't take; the error keeps only so much of
/// it.
#[test]
fn an_error_quotes_little_of_what_was_sent() {
    let tag = "x".repeat(6000);
    let mut session = greeted(Authority::MayStart);
    let Err(error) = session.accept(json(&format!(r#"{{"type":"{tag}"}}"#))) else {
        panic!("accepted");
    };
    assert!(
        matches!(error, ProtocolError::InvalidMessage(_)),
        "{error:?}"
    );
    let Reply::Error { message, .. } = error.reply() else {
        unreachable!()
    };
    assert!(message.len() <= 4096, "{}", message.len());
    assert!(message.ends_with('…'));
}

// ---- After an error ----

#[test]
fn after_an_error_everything_is_refused_with_that_error() {
    let mut session = greeted(Authority::MayStart);
    let error = ProtocolError::UnexpectedFrame(FrameType::Blob);
    assert_eq!(session.accept(blob(b"x")), Err(error.clone()));
    session.replied();
    for frame in [
        json(HELLO),
        json(r#"{"type":"status"}"#),
        json(&start_json(2, &[])),
        blob(b""),
    ] {
        assert_eq!(session.accept(frame), Err(error.clone()));
    }
    assert_eq!(
        session.frame_caps(),
        FrameCaps {
            max_json: None,
            max_blob: None
        }
    );
}

// ---- Frame caps per state ----

#[test]
fn frame_caps_follow_the_state() {
    let control = FrameCaps {
        max_json: Some(4096),
        max_blob: None,
    };
    let wide = FrameCaps {
        max_json: Some(6614),
        max_blob: None,
    };
    let blob_of = |len| FrameCaps {
        max_json: None,
        max_blob: Some(len),
    };

    let mut session = session(Authority::MayStart);
    assert_eq!(session.frame_caps(), control, "before hello");
    session.accept(json(HELLO)).unwrap();
    assert_eq!(session.frame_caps(), wide, "hello outstanding");
    session.replied();
    assert_eq!(session.frame_caps(), wide, "idle");
    session
        .accept(json(&start_json(2, &[("a", 3), ("b", 0)])))
        .unwrap();
    assert_eq!(session.frame_caps(), blob_of(2), "the config owed");
    session.accept(blob(b"{}")).unwrap();
    assert_eq!(session.frame_caps(), blob_of(3));
    session.accept(blob(b"abc")).unwrap();
    assert_eq!(session.frame_caps(), blob_of(0));
    session.accept(blob(b"")).unwrap();
    assert_eq!(session.frame_caps(), wide, "start outstanding");

    let mut read_only = ServerSession::new(Authority::ReadOnly, Limits::default());
    assert_eq!(read_only.frame_caps(), control, "before hello");
    read_only.accept(json(HELLO)).unwrap();
    assert_eq!(read_only.frame_caps(), control, "hello outstanding");
    read_only.replied();
    assert_eq!(read_only.frame_caps(), control, "idle");
}

/// `replied` never changes the caps, so a helper that sets them after each
/// frame never holds a caller to the wrong ones.
#[test]
fn replied_never_changes_the_caps() {
    for authority in [Authority::MayStart, Authority::ReadOnly] {
        let mut session = session(authority);
        let frames = [
            json(HELLO),
            json(r#"{"type":"status"}"#),
            json(&start_json(2, &[("a", 1)])),
            blob(b"{}"),
            blob(b"x"),
        ];
        for frame in frames {
            let before = session.frame_caps();
            session.replied();
            assert_eq!(session.frame_caps(), before);
            if session.accept(frame).is_err() {
                break;
            }
            let after = session.frame_caps();
            session.replied();
            assert_eq!(session.frame_caps(), after);
        }
    }
}

// ---- Through the decoder ----

#[test]
fn a_start_through_the_decoder() {
    let limits = Limits::default();
    let request = start(
        &format!("{{\"x\":\"{}\"}}", "y".repeat(100_000)),
        &[("ca", b"pem"), ("rules", &[0; 70_000])],
    );
    let mut bytes = encode_request(&Request::hello(), &limits).unwrap();
    bytes.extend(encode_request(&request, &limits).unwrap());
    bytes.extend(encode_request(&Request::Stop, &limits).unwrap());
    for mut connection in [
        Connection::new(Authority::MayStart, limits),
        Connection::wide(Authority::MayStart, limits),
    ] {
        assert_eq!(
            connection.feed(&bytes),
            [Ok(Request::hello()), Ok(request.clone()), Ok(Request::Stop)]
        );
    }
}

/// Narrowed caps refuse from the header what a read-only caller, or a
/// caller before `hello`, could otherwise make the helper hold.
#[test]
fn narrowed_caps_hold_untrusted_callers_to_4_kib() {
    let big = json_bytes(&format!("{}{}", " ".repeat(5000), start_json(2, &[])));
    let too_large = ProtocolError::FrameTooLarge {
        frame_type: FrameType::Json,
        len: big.len() - 5,
        limit: 4096,
    };

    let mut connection = Connection::new(Authority::MayStart, Limits::default());
    assert_eq!(connection.feed(&big), [Err(too_large.clone())]);
    assert_eq!(connection.decoder.reserved(), 0);

    let mut bytes = json_bytes(HELLO);
    bytes.extend(&big);
    let mut connection = Connection::new(Authority::ReadOnly, Limits::default());
    assert_eq!(
        connection.feed(&bytes),
        [Ok(Request::hello()), Err(too_large.clone())]
    );

    // Without narrowing, the session holds the line itself.
    let mut connection = Connection::wide(Authority::ReadOnly, Limits::default());
    assert_eq!(
        connection.feed(&bytes),
        [Ok(Request::hello()), Err(too_large)]
    );
}

/// While blobs are owed the decoder takes only a blob of at most the owed
/// length; a longer one is refused from its header. The config is owed
/// first.
#[test]
fn narrowed_caps_refuse_a_long_blob_from_its_header() {
    let mut bytes = json_bytes(HELLO);
    bytes.extend(json_bytes(&start_json(2, &[("a", 3)])));
    bytes.extend(blob_bytes(&[0; 100_000]));
    let mut connection = Connection::new(Authority::MayStart, Limits::default());
    assert_eq!(
        connection.feed(&bytes),
        [
            Ok(Request::hello()),
            Err(ProtocolError::FrameTooLarge {
                frame_type: FrameType::Blob,
                len: 100_000,
                limit: 2
            })
        ]
    );
}

// ---- Debug ----

#[test]
fn debug_shows_no_config_or_attachment() {
    let request = start(r#"{"password":"hunter2"}"#, &[("key", b"PRIVATE KEY")]);
    let shown = format!("{request:?}");
    assert!(!shown.contains("hunter2"), "{shown}");
    assert!(!shown.contains("PRIVATE"), "{shown}");
    assert!(shown.contains("config_len: 22"), "{shown}");
    assert!(shown.contains("\"key\": 11"), "{shown}");

    let mut session = greeted(Authority::MayStart);
    session.accept(json(&start_json(22, &[("a", 1)]))).unwrap();
    assert!(format!("{session:?}").contains("blobs (2 owed)"));
    session.accept(blob(br#"{"password":"hunter2"}"#)).unwrap();
    let shown = format!("{session:?}");
    assert!(!shown.contains("hunter2"), "{shown}");
    assert!(shown.contains("blobs (1 owed)"), "{shown}");
}
