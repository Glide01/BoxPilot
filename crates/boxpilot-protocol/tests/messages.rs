//! Every message both ways, its exact JSON, the refusal codes, and the
//! bounds on what the helper sends.

mod common;

use boxpilot_policy::{check, Expected, Limits as PolicyLimits, Refusal, RefusalKind};
use boxpilot_protocol::{
    decode_to_client, encode_request, encode_to_client, Authority, ErrorCode, Event, ExitInfo,
    Frame, FrameDecoder, FrameType, HelloReply, Limits, ProtocolError, RefusalCode, Reply, Request,
    RunState, StartRequest, Started, ToClient, WireRefusal, MAX_ERROR_MESSAGE, MAX_REFUSALS,
    MAX_REFUSAL_TEXT, PROTOCOL_VERSION,
};
use common::{json, options, Connection};
use std::collections::BTreeSet;

const MIB: usize = 1024 * 1024;

/// The JSON payloads of the frames in `bytes`, blobs as `<blob N>`.
fn payloads(bytes: &[u8], caps_limits: &Limits) -> Vec<String> {
    let mut decoder = FrameDecoder::new(caps_limits.to_helper_caps());
    let mut input = bytes;
    let mut out = Vec::new();
    loop {
        input = &input[decoder.feed(input)..];
        match decoder.next_frame().unwrap() {
            Some(Frame::Json(text)) => out.push(text),
            Some(Frame::Blob(data)) => out.push(format!("<blob {}>", data.len())),
            None => return out,
        }
    }
}

/// Encode for the GUI, then read it back as the GUI does.
fn to_gui(message: &ToClient) -> ToClient {
    let limits = Limits::default();
    let bytes = encode_to_client(message, &limits).unwrap();
    let mut decoder = FrameDecoder::new(limits.to_gui_caps());
    assert_eq!(decoder.feed(&bytes), bytes.len());
    let frame = decoder.next_frame().unwrap().unwrap();
    decode_to_client(&frame).unwrap()
}

fn wire(message: &ToClient) -> String {
    let bytes = encode_to_client(message, &Limits::default()).unwrap();
    String::from_utf8(bytes[5..].to_vec()).unwrap()
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

fn hello_reply() -> HelloReply {
    HelloReply {
        protocol_version: PROTOCOL_VERSION,
        helper_version: "1.14.0".into(),
        sing_box_version: "1.14.2".into(),
        sing_box_sha256: "ab".repeat(32),
        may_start: true,
    }
}

fn refusal(pointer: &str, kind: RefusalKind) -> Refusal {
    Refusal {
        pointer: pointer.into(),
        kind,
    }
}

// ---- Requests ----

#[test]
fn every_request_round_trips() {
    let limits = Limits::default();
    let requests = [
        Request::hello(),
        Request::Status,
        Request::Stop,
        start("{}", &[]),
        start(r#"{"route":{"rule_set":[]}}"#, &[("ca", b"-----BEGIN")]),
        start(
            "{\"log\":{\"level\":\"info\"},\"x\":\"\\u0000 \\\" \\\\ 日本\"}",
            &[("a", b""), ("b", &[0xff; 3000]), ("C-d_9", b"\0")],
        ),
    ];
    let mut bytes = Vec::new();
    for request in &requests {
        bytes.extend(encode_request(request, &limits).unwrap());
    }
    let mut connection = Connection::new(Authority::MayStart, limits);
    let got: Vec<Request> = connection
        .feed(&bytes)
        .into_iter()
        .map(Result::unwrap)
        .collect();
    assert_eq!(got, requests);
}

/// The JSON each request is, exactly. `hello` is frozen across versions.
#[test]
fn requests_on_the_wire() {
    let limits = Limits::default();
    let one = |request: &Request| payloads(&encode_request(request, &limits).unwrap(), &limits);
    assert_eq!(
        one(&Request::hello()),
        [r#"{"type":"hello","protocol_version":1}"#]
    );
    assert_eq!(one(&Request::Status), [r#"{"type":"status"}"#]);
    assert_eq!(one(&Request::Stop), [r#"{"type":"stop"}"#]);
    assert_eq!(
        one(&start(r#"{"log":{}}"#, &[("ca", b"pem"), ("rules", b"")])),
        [
            r#"{"type":"start","config":"{\"log\":{}}","attachments":[{"id":"ca","len":3},{"id":"rules","len":0}],"options":{"ipv6":true,"proxy_port":7890,"allow_lan":false,"system_proxy":true}}"#,
            "<blob 3>",
            "<blob 0>",
        ]
    );
}

/// The GUI's encoder refuses, with the session's own error, what the helper
/// would refuse; nothing is sent.
#[test]
fn the_encoder_refuses_what_the_session_would() {
    let limits = Limits::default();
    let cases = [
        (
            start("{}", &[("a", b"1"), ("a", b"2")]),
            ProtocolError::DuplicateAttachmentId { index: 1 },
        ),
        (
            start("{}", &[("ok", b""), ("a/b", b"")]),
            ProtocolError::InvalidAttachmentId { index: 1 },
        ),
        (
            Request::Start(StartRequest {
                config: "{}".into(),
                attachments: Vec::new(),
                options: boxpilot_protocol::TunOptions {
                    proxy_port: 0,
                    ..options()
                },
            }),
            ProtocolError::ZeroProxyPort,
        ),
    ];
    for (request, error) in cases {
        assert_eq!(encode_request(&request, &limits), Err(error));
    }

    let ids: Vec<String> = (0..65).map(|i| format!("a{i}")).collect();
    let many: Vec<(&str, &[u8])> = ids.iter().map(|id| (id.as_str(), &b""[..])).collect();
    assert_eq!(
        encode_request(&start("{}", &many), &limits),
        Err(ProtocolError::TooManyAttachments {
            count: 65,
            limit: 64
        })
    );

    let small = Limits {
        max_start_total: 10,
        ..limits
    };
    assert_eq!(
        encode_request(&start("{}", &[("a", b"123456789")]), &small),
        Err(ProtocolError::StartTooLarge {
            bytes: 11,
            limit: 10
        })
    );
}

/// A config within the Start budget whose escaping takes the frame over its
/// cap is refused before it is sent, never cut.
#[test]
fn a_config_that_escapes_past_the_frame_cap_is_refused() {
    let limits = Limits {
        max_request_json: 200,
        max_start_total: 1000,
        ..Limits::default()
    };
    let config = "\"".repeat(150);
    let Err(error) = encode_request(&start(&config, &[]), &limits) else {
        panic!("encoded");
    };
    assert!(
        matches!(
            error,
            ProtocolError::FrameTooLarge {
                frame_type: FrameType::Json,
                limit: 200,
                ..
            }
        ),
        "{error:?}"
    );
}

// ---- To the GUI ----

fn every_message() -> Vec<ToClient> {
    let mut messages: Vec<ToClient> = vec![
        Reply::Hello(hello_reply()).into(),
        Reply::Hello(HelloReply {
            may_start: false,
            ..hello_reply()
        })
        .into(),
        Reply::Started(Started {
            api_port: 51234,
            api_secret: "0f".repeat(32),
        })
        .into(),
        Reply::refused(&[
            refusal(
                "/outbounds/0/type",
                RefusalKind::TypeNotAllowed {
                    type_name: Some("tor".into()),
                },
            ),
            refusal("", RefusalKind::NotAnObject),
        ])
        .into(),
        Reply::Refused {
            refusals: vec![WireRefusal {
                pointer: "/x".into(),
                code: RefusalCode::Other("from_the_future".into()),
                detail: None,
            }],
            omitted: 3,
        }
        .into(),
        Reply::Stopped.into(),
        Event::Log {
            line: "INFO[0000] sing-box started (0.12s)".into(),
            truncated: false,
        }
        .into(),
        Event::Log {
            line: "日本語 ✓".into(),
            truncated: true,
        }
        .into(),
    ];
    for state in [RunState::Stopped, RunState::Starting, RunState::Running] {
        for last_exit in [
            None,
            Some(ExitInfo {
                code: Some(0),
                signal: None,
            }),
            Some(ExitInfo {
                code: None,
                signal: Some(9),
            }),
        ] {
            messages.push(Reply::Status { state, last_exit }.into());
        }
    }
    for code in [
        ErrorCode::Unauthorized,
        ErrorCode::VersionMismatch,
        ErrorCode::Busy,
        ErrorCode::BadRequest,
        ErrorCode::Internal,
    ] {
        messages.push(Reply::error(code, "why").into());
    }
    for (code, signal) in [
        (Some(0), None),
        (Some(1), None),
        (Some(0xC000_0005_u32 as i32), None),
        (None, Some(15)),
        (None, None),
    ] {
        messages.push(Event::Exited(ExitInfo { code, signal }).into());
    }
    messages
}

#[test]
fn every_message_to_the_gui_round_trips() {
    for message in every_message() {
        assert_eq!(to_gui(&message), message, "{message:?}");
    }
}

/// The JSON each message to the GUI is, exactly. `error` is frozen across
/// versions.
#[test]
fn messages_to_the_gui_on_the_wire() {
    assert_eq!(
        wire(&Reply::Hello(hello_reply()).into()),
        format!(
            r#"{{"type":"hello","protocol_version":1,"helper_version":"1.14.0","sing_box_version":"1.14.2","sing_box_sha256":"{}","may_start":true}}"#,
            "ab".repeat(32)
        )
    );
    assert_eq!(
        wire(
            &Reply::Started(Started {
                api_port: 51234,
                api_secret: "0f".into()
            })
            .into()
        ),
        r#"{"type":"started","api_port":51234,"api_secret":"0f"}"#
    );
    assert_eq!(
        wire(
            &Reply::refused(&[refusal(
                "/outbounds/0/type",
                RefusalKind::TypeNotAllowed {
                    type_name: Some("tor".into())
                }
            )])
            .into()
        ),
        r#"{"type":"refused","refusals":[{"pointer":"/outbounds/0/type","code":"type_not_allowed","detail":"tor"}],"omitted":0}"#
    );
    assert_eq!(wire(&Reply::Stopped.into()), r#"{"type":"stopped"}"#);
    assert_eq!(
        wire(
            &Reply::Status {
                state: RunState::Running,
                last_exit: None
            }
            .into()
        ),
        r#"{"type":"status","state":"running","last_exit":null}"#
    );
    assert_eq!(
        wire(
            &Reply::Status {
                state: RunState::Stopped,
                last_exit: Some(ExitInfo {
                    code: Some(1),
                    signal: None
                })
            }
            .into()
        ),
        r#"{"type":"status","state":"stopped","last_exit":{"code":1,"signal":null}}"#
    );
    assert_eq!(
        wire(&Reply::error(ErrorCode::VersionMismatch, "v").into()),
        r#"{"type":"error","code":"version_mismatch","message":"v"}"#
    );
    assert_eq!(
        wire(
            &Event::Log {
                line: "a\tb".into(),
                truncated: false
            }
            .into()
        ),
        r#"{"type":"log","line":"a\tb","truncated":false}"#
    );
    assert_eq!(
        wire(
            &Event::Exited(ExitInfo {
                code: None,
                signal: Some(9)
            })
            .into()
        ),
        r#"{"type":"exited","code":null,"signal":9}"#
    );
}

#[test]
fn codes_and_states_on_the_wire() {
    for (code, text) in [
        (ErrorCode::Unauthorized, "unauthorized"),
        (ErrorCode::VersionMismatch, "version_mismatch"),
        (ErrorCode::Busy, "busy"),
        (ErrorCode::BadRequest, "bad_request"),
        (ErrorCode::Internal, "internal"),
    ] {
        assert_eq!(serde_json::to_string(&code).unwrap(), format!("\"{text}\""));
        assert_eq!(code.as_str(), text);
        assert_eq!(code.to_string(), text);
    }
    for (state, text) in [
        (RunState::Stopped, "stopped"),
        (RunState::Starting, "starting"),
        (RunState::Running, "running"),
    ] {
        assert_eq!(
            serde_json::to_string(&state).unwrap(),
            format!("\"{text}\"")
        );
    }
}

#[test]
fn the_gui_refuses_what_isnt_a_message() {
    let cases = [
        r#"{"type":"stopped","note":"x"}"#,
        r#"{"type":"started","api_port":1,"api_secret":"x","pid":7}"#,
        r#"{"type":"hello","protocol_version":1}"#,
        r#"{"type":"status","state":"crashed","last_exit":null}"#,
        r#"{"type":"status","state":"running","last_exit":[0,null]}"#,
        r#"{"type":"error","code":"teapot","message":""}"#,
        r#"{"type":"refused","refusals":[["/x","inbounds",null]],"omitted":0}"#,
        r#"{"type":"refused","refusals":[{"pointer":"/x","code":"inbounds","detail":null,"x":1}],"omitted":0}"#,
        r#"{"type":"log","line":"x"}"#,
        r#"{"type":"logs"}"#,
        r#"{"type":"stop"}"#,
        r#"["stopped"]"#,
        r#"{"type":"stopped","type":"stopped"}"#,
        r#"{"type":"exited","code":2147483648,"signal":null}"#,
    ];
    for text in cases {
        let result = decode_to_client(&json(text));
        assert!(
            matches!(result, Err(ProtocolError::InvalidMessage(_))),
            "{text}: {result:?}"
        );
    }
    assert_eq!(
        decode_to_client(&Frame::Blob(vec![1])),
        Err(ProtocolError::UnexpectedFrame(FrameType::Blob))
    );
}

#[test]
fn a_message_over_the_cap_is_refused_not_cut() {
    let huge = Reply::Hello(HelloReply {
        helper_version: "x".repeat(2 * MIB),
        ..hello_reply()
    });
    let Err(error) = encode_to_client(&huge.into(), &Limits::default()) else {
        panic!("encoded");
    };
    assert!(
        matches!(
            error,
            ProtocolError::FrameTooLarge {
                frame_type: FrameType::Json,
                limit: MIB,
                ..
            }
        ),
        "{error:?}"
    );
}

#[test]
fn started_debug_redacts_the_secret() {
    let started = Started {
        api_port: 51234,
        api_secret: "deadbeef".into(),
    };
    let shown = format!("{:?}", ToClient::from(Reply::Started(started)));
    assert!(!shown.contains("deadbeef"), "{shown}");
    assert!(shown.contains("51234"), "{shown}");
}

// ---- Log lines ----

#[test]
fn a_long_log_line_is_cut_at_a_utf8_boundary() {
    let limits = Limits::default();
    let max = limits.max_log_line;
    // A 4-byte character straddles the limit.
    let line = format!("{}😀tail", "a".repeat(max - 2));
    let sent: ToClient = Event::Log {
        line,
        truncated: false,
    }
    .into();
    assert_eq!(
        to_gui(&sent),
        Event::Log {
            line: "a".repeat(max - 2),
            truncated: true
        }
        .into()
    );

    // At the limit exactly, nothing is cut.
    let line = format!("{}é", "a".repeat(max - 2));
    let sent: ToClient = Event::Log {
        line: line.clone(),
        truncated: false,
    }
    .into();
    assert_eq!(to_gui(&sent), sent);

    // A line the helper already cut stays marked.
    let sent: ToClient = Event::Log {
        line: "short".into(),
        truncated: true,
    }
    .into();
    assert_eq!(to_gui(&sent), sent);
}

#[test]
fn every_cut_point_lands_on_a_boundary() {
    let line = "aé😀b€";
    for max in 0..=line.len() + 1 {
        let limits = Limits {
            max_log_line: max,
            ..Limits::default()
        };
        let bytes = encode_to_client(
            &Event::Log {
                line: line.into(),
                truncated: false,
            }
            .into(),
            &limits,
        )
        .unwrap();
        let mut decoder = FrameDecoder::new(limits.to_gui_caps());
        let _ = decoder.feed(&bytes);
        let ToClient::Event(Event::Log {
            line: got,
            truncated,
        }) = decode_to_client(&decoder.next_frame().unwrap().unwrap()).unwrap()
        else {
            panic!("not a log line");
        };
        assert!(got.len() <= max, "max {max}: {got:?}");
        assert!(line.starts_with(&got));
        assert_eq!(truncated, got.len() < line.len(), "max {max}");
        // The longest prefix that fits: the next character would not.
        if let Some(next) = line[got.len()..].chars().next() {
            assert!(got.len() + next.len_utf8() > max, "max {max}: {got:?}");
        }
    }
}

/// The worst log line, all control characters, escapes sixfold and still
/// fits the GUI's cap.
#[test]
fn the_worst_log_line_fits() {
    let limits = Limits::default();
    let line = "\u{1}".repeat(10 * limits.max_log_line);
    let bytes = encode_to_client(
        &Event::Log {
            line,
            truncated: false,
        }
        .into(),
        &limits,
    )
    .unwrap();
    assert!(bytes.len() <= limits.max_reply_json, "{}", bytes.len());
    assert!(bytes.len() > 6 * limits.max_log_line);
}

// ---- Refusals ----

/// Which `RefusalKind` variant this is. Exhaustive: a variant the policy
/// adds breaks the build here, and then the coverage check below until it
/// has a pinned code.
fn variant(kind: &RefusalKind) -> usize {
    match kind {
        RefusalKind::TooLarge { .. } => 0,
        RefusalKind::TooDeep { .. } => 1,
        RefusalKind::InvalidJson(_) => 2,
        RefusalKind::NotAnObject => 3,
        RefusalKind::Malformed { .. } => 4,
        RefusalKind::NonCanonicalKey => 5,
        RefusalKind::UnknownSection => 6,
        RefusalKind::TypeNotAllowed { .. } => 7,
        RefusalKind::Inbounds => 8,
        RefusalKind::Service { .. } => 9,
        RefusalKind::UnknownExperimental => 10,
        RefusalKind::RunsProgram => 11,
        RefusalKind::SystemChange => 12,
        RefusalKind::ServerFileScan => 13,
        RefusalKind::FilesystemPath => 14,
        RefusalKind::Directory => 15,
        RefusalKind::LocalFile => 16,
        RefusalKind::MalformedAttachment => 17,
        RefusalKind::MissingAttachment { .. } => 18,
    }
}
const VARIANTS: usize = 19;

/// Every variant's code and detail, pinned.
fn pinned() -> Vec<(RefusalKind, &'static str, Option<&'static str>)> {
    vec![
        (
            RefusalKind::TooLarge {
                bytes: 40_000_000,
                limit: 33_554_432,
            },
            "too_large",
            Some("40000000 > 33554432"),
        ),
        (RefusalKind::TooDeep { limit: 64 }, "too_deep", Some("64")),
        (
            RefusalKind::InvalidJson("EOF while parsing a value at line 1 column 0".into()),
            "invalid_json",
            Some("EOF while parsing a value at line 1 column 0"),
        ),
        (RefusalKind::NotAnObject, "not_an_object", None),
        (
            RefusalKind::Malformed {
                expected: Expected::StringOrArray,
            },
            "malformed",
            Some("string_or_array"),
        ),
        (RefusalKind::NonCanonicalKey, "non_canonical_key", None),
        (RefusalKind::UnknownSection, "unknown_section", None),
        (
            RefusalKind::TypeNotAllowed {
                type_name: Some("tor".into()),
            },
            "type_not_allowed",
            Some("tor"),
        ),
        (RefusalKind::Inbounds, "inbounds", None),
        (
            RefusalKind::Service {
                service_type: Some("derp".into()),
            },
            "service",
            Some("derp"),
        ),
        (
            RefusalKind::UnknownExperimental,
            "unknown_experimental",
            None,
        ),
        (RefusalKind::RunsProgram, "runs_program", None),
        (RefusalKind::SystemChange, "system_change", None),
        (RefusalKind::ServerFileScan, "server_file_scan", None),
        (RefusalKind::FilesystemPath, "filesystem_path", None),
        (RefusalKind::Directory, "directory", None),
        (RefusalKind::LocalFile, "local_file", None),
        (
            RefusalKind::MalformedAttachment,
            "malformed_attachment",
            None,
        ),
        (
            RefusalKind::MissingAttachment { id: "ca".into() },
            "missing_attachment",
            Some("ca"),
        ),
    ]
}

#[test]
fn every_refusal_kind_has_a_pinned_code() {
    let pinned = pinned();
    let covered: BTreeSet<usize> = pinned.iter().map(|(kind, ..)| variant(kind)).collect();
    assert_eq!(covered, (0..VARIANTS).collect(), "every variant, once");
    assert_eq!(pinned.len(), VARIANTS);
    let codes: BTreeSet<&str> = pinned.iter().map(|(_, code, _)| *code).collect();
    assert_eq!(codes.len(), VARIANTS, "codes are distinct");

    for (kind, code, detail) in pinned {
        let wire = WireRefusal::from(&refusal("/p", kind.clone()));
        assert_eq!(wire.code.as_str(), code, "{kind:?}");
        assert_eq!(wire.detail.as_deref(), detail, "{kind:?}");
        assert_eq!(wire.pointer, "/p");
        assert_eq!(RefusalCode::from_wire(code), wire.code);
        assert_ne!(wire.code, RefusalCode::Other(code.into()));
        let json = serde_json::to_string(&wire).unwrap();
        let detail_json = detail.map_or("null".to_owned(), |d| format!("\"{d}\""));
        assert_eq!(
            json,
            format!(r#"{{"pointer":"/p","code":"{code}","detail":{detail_json}}}"#)
        );
    }
}

#[test]
fn malformed_details_are_pinned() {
    for (expected, detail) in [
        (Expected::Object, "object"),
        (Expected::Array, "array"),
        (Expected::String, "string"),
        (Expected::StringOrArray, "string_or_array"),
        (Expected::PluginOptions, "plugin_options"),
    ] {
        let wire = WireRefusal::from(&refusal("", RefusalKind::Malformed { expected }));
        assert_eq!(wire.detail.as_deref(), Some(detail));
    }
}

#[test]
fn a_kind_without_its_value_has_no_detail() {
    for kind in [
        RefusalKind::TypeNotAllowed { type_name: None },
        RefusalKind::Service { service_type: None },
    ] {
        assert_eq!(WireRefusal::from(&refusal("/x", kind)).detail, None);
    }
}

/// A code from a newer helper's policy still decodes, kept as it came, so
/// the GUI can show something.
#[test]
fn an_unknown_refusal_code_decodes_as_itself() {
    let text = r#"{"type":"refused","refusals":[{"pointer":"/services/0","code":"runs_quantum_program","detail":"qubit"}],"omitted":0}"#;
    let message = decode_to_client(&json(text)).unwrap();
    let ToClient::Reply(Reply::Refused { refusals, omitted }) = &message else {
        panic!("{message:?}");
    };
    assert_eq!(*omitted, 0);
    assert_eq!(
        refusals[0].code,
        RefusalCode::Other("runs_quantum_program".into())
    );
    assert_eq!(refusals[0].code.as_str(), "runs_quantum_program");
    assert_eq!(refusals[0].detail.as_deref(), Some("qubit"));
    // And goes back out unchanged.
    assert_eq!(wire(&message), text);
}

/// The policy's own verdict on a hostile config, as the GUI receives it.
#[test]
fn a_policy_verdict_reaches_the_gui() {
    let config = r#"{
        "inbounds": [],
        "outbounds": [{"type": "tor", "tag": "t", "executable_path": "/tmp/x"}],
        "Log": {}
    }"#;
    let refusals = check(config, &BTreeSet::new(), &PolicyLimits::default()).unwrap_err();
    let ToClient::Reply(Reply::Refused {
        refusals: got,
        omitted,
    }) = to_gui(&Reply::refused(&refusals).into())
    else {
        panic!("not refused");
    };
    assert_eq!(omitted, 0);
    let mut got: Vec<(String, String)> = got
        .into_iter()
        .map(|r| (r.pointer, r.code.to_string()))
        .collect();
    got.sort();
    let mut expected: Vec<(String, String)> = refusals
        .iter()
        .map(|r| (r.pointer.clone(), RefusalCode::from(&r.kind).to_string()))
        .collect();
    expected.sort();
    assert_eq!(got, expected);
    let codes: BTreeSet<&str> = got.iter().map(|(_, code)| code.as_str()).collect();
    assert!(codes.contains("inbounds"), "{codes:?}");
    assert!(codes.contains("non_canonical_key"), "{codes:?}");
}

#[test]
fn refusal_text_is_cut_on_a_char_boundary() {
    let pointer = format!("/{}", "é".repeat(1000));
    let wire = WireRefusal::from(&refusal(
        &pointer,
        RefusalKind::TypeNotAllowed {
            type_name: Some("😀".repeat(1000)),
        },
    ));
    for text in [&wire.pointer, wire.detail.as_ref().unwrap()] {
        assert!(text.len() <= MAX_REFUSAL_TEXT, "{}", text.len());
        assert!(text.ends_with('…'));
        assert!(text.len() > MAX_REFUSAL_TEXT - 8);
    }
    assert!(pointer.starts_with(wire.pointer.trim_end_matches('…')));
}

#[test]
fn refused_lists_at_most_128_and_counts_the_rest() {
    let refusals: Vec<Refusal> = (0..200)
        .map(|i| refusal(&format!("/outbounds/{i}"), RefusalKind::RunsProgram))
        .collect();
    let Reply::Refused {
        refusals: listed,
        omitted,
    } = Reply::refused(&refusals)
    else {
        unreachable!()
    };
    assert_eq!(listed.len(), MAX_REFUSALS);
    assert_eq!(omitted, 72);
    assert_eq!(listed[127].pointer, "/outbounds/127");
}

/// The largest `refused` the helper can build, every text field as long as
/// it may be and every byte escaped sixfold, fits the GUI's cap.
#[test]
fn the_worst_refused_reply_fits() {
    let hostile = "\u{1}".repeat(100_000);
    let refusals: Vec<Refusal> = (0..1000)
        .map(|_| refusal(&hostile, RefusalKind::InvalidJson(hostile.clone())))
        .collect();
    let limits = Limits::default();
    let bytes = encode_to_client(&Reply::refused(&refusals).into(), &limits).unwrap();
    assert!(bytes.len() <= limits.max_reply_json, "{}", bytes.len());
    assert!(bytes.len() > 700 * 1024, "{}", bytes.len());
}

#[test]
fn an_error_message_is_cut() {
    let Reply::Error { code, message } = Reply::error(ErrorCode::Internal, "ü".repeat(10_000))
    else {
        unreachable!()
    };
    assert_eq!(code, ErrorCode::Internal);
    assert!(message.len() <= MAX_ERROR_MESSAGE);
    assert!(message.ends_with('…'));
    assert!(message.starts_with("üü"));
}
