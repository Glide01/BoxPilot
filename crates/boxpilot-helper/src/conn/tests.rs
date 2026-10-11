//! The helper's core end to end: connections over an in-memory transport,
//! the real core, and a fake supervisor standing in for sing-box.

use super::*;
use crate::helper::Helper;
use crate::outbox::OutboxLimits;
use crate::testing::{
    admin, names, other_admin, user, Client, Harness, Script, ADMIN_SID, PATIENCE,
};
use boxpilot_protocol::{
    Event, ExitInfo, HelloReply, RefusalCode, RunState, StartRequest, Started, TunOptions,
};
use serde_json::{json, Value};
use std::net::{Ipv4Addr, TcpStream};
use std::thread;

fn options() -> TunOptions {
    TunOptions {
        ipv6: false,
        proxy_port: 7890,
        allow_lan: false,
        system_proxy: true,
    }
}

fn start_request(config: Value, attachments: &[(&str, &[u8])]) -> Request {
    Request::Start(StartRequest {
        config: config.to_string(),
        attachments: attachments
            .iter()
            .map(|(id, data)| (id.to_string(), data.to_vec()))
            .collect(),
        options: options(),
    })
}

fn plain_start() -> Request {
    start_request(
        json!({"outbounds": [{"type": "direct", "tag": "direct"}]}),
        &[],
    )
}

fn started(reply: Reply) -> Started {
    match reply {
        Reply::Started(started) => started,
        other => panic!("expected started, got {other:?}"),
    }
}

fn error_code(reply: Reply) -> ErrorCode {
    match reply {
        Reply::Error { code, .. } => code,
        other => panic!("expected an error, got {other:?}"),
    }
}

fn status(client: &mut Client) -> (RunState, Option<ExitInfo>) {
    match client.ask(&Request::Status) {
        Reply::Status { state, last_exit } => (state, last_exit),
        other => panic!("expected status, got {other:?}"),
    }
}

fn exited(code: i32) -> Event {
    Event::Exited(ExitInfo {
        code: Some(code),
        signal: None,
    })
}

/// Wait for the core to have no sing-box: stops are asynchronous to the
/// client that caused them.
fn wait_idle(harness: &Harness) {
    let deadline = Instant::now() + PATIENCE;
    while !harness.core.is_idle() {
        assert!(Instant::now() < deadline, "sing-box never stopped");
        thread::sleep(Duration::from_millis(5));
    }
}

/// Wait for `n` sing-boxes to have been spawned in all. The core is idle
/// before a start is handled too: [`wait_idle`] after a start whose reply
/// hasn't been read waits for nothing unless this comes first.
fn wait_spawned(harness: &Harness, n: usize) {
    let deadline = Instant::now() + PATIENCE;
    while harness.fake().spawned().len() < n {
        assert!(Instant::now() < deadline, "sing-box was never spawned");
        thread::sleep(Duration::from_millis(5));
    }
}

fn short_timeouts() -> ConnConfig {
    ConnConfig {
        timeouts: Timeouts {
            hello: Duration::from_millis(200),
            frame: Duration::from_millis(200),
            request: Duration::from_millis(400),
            write: Duration::from_millis(200),
        },
        ..ConnConfig::default()
    }
}

// ---- hello and authority ----

#[test]
fn hello_reports_the_installed_sing_box_and_what_the_caller_may_do() {
    let harness = Harness::new();
    for (caller, may_start) in [(admin(), true), (user(), false)] {
        let (mut client, served) = harness.connect(caller);
        assert_eq!(
            client.hello(),
            HelloReply {
                protocol_version: 1,
                helper_version: env!("CARGO_PKG_VERSION").into(),
                sing_box_version: "1.14.2".into(),
                sing_box_sha256: "ab".repeat(32),
                may_start,
            }
        );
        assert_eq!(status(&mut client), (RunState::Stopped, None));
        client.close();
        assert_eq!(served.join().unwrap(), Ended::Eof);
    }
}

#[test]
fn another_protocol_version_is_answered_then_closed() {
    let harness = Harness::new();
    let (mut client, served) = harness.connect(admin());
    client.send_bytes(&[0, 0, 0, 38, 1]);
    client.send_bytes(br#"{"type":"hello","protocol_version":2}"#);
    // 38 bytes declared, 37 sent: the decoder waits for one more.
    client.send_bytes(b" ");
    assert_eq!(
        client.reply(),
        Reply::Error {
            code: ErrorCode::VersionMismatch,
            message: "the helper speaks protocol version 1, the client 2".into(),
        }
    );
    assert_eq!(client.recv(), None, "the helper closes after the error");
    assert_eq!(
        served.join().unwrap(),
        Ended::Protocol(ErrorCode::VersionMismatch)
    );
}

#[test]
fn a_read_only_caller_can_neither_start_nor_stop() {
    let harness = Harness::new();
    for request in [plain_start(), Request::Stop] {
        let (mut client, served) = harness.connect(user());
        assert!(!client.hello().may_start);
        assert_eq!(status(&mut client), (RunState::Stopped, None));
        assert_eq!(error_code(client.ask(&request)), ErrorCode::Unauthorized);
        assert_eq!(client.recv(), None);
        assert_eq!(
            served.join().unwrap(),
            Ended::Protocol(ErrorCode::Unauthorized)
        );
    }
    assert!(harness.fake().spawned().is_empty());
}

/// The core checks the authority too, whatever the session let through.
#[test]
fn the_core_refuses_a_read_only_caller_on_its_own() {
    let harness = Harness::new();
    let outbox = Arc::new(Outbox::new(OutboxLimits::default()));
    let Request::Start(start) = plain_start() else {
        unreachable!()
    };
    assert_eq!(
        error_code(harness.core.start(1, &user(), start, &outbox)),
        ErrorCode::Unauthorized
    );
    assert_eq!(
        error_code(harness.core.stop(&user())),
        ErrorCode::Unauthorized
    );
    assert!(harness.fake().spawned().is_empty());
}

// ---- start ----

#[test]
fn start_runs_sing_box_with_a_working_api_endpoint() {
    let harness = Harness::new();
    let (mut client, served) = harness.connect(admin());
    client.hello();
    let reply = client.ask(&start_request(
        json!({
            "outbounds": [{"type": "direct", "tag": "direct"}],
            "route": {"rule_set": [{
                "type": "local", "tag": "geo", "format": "binary",
                "path": "boxpilot-attachment:geo"
            }]}
        }),
        &[("geo", b"rule bytes"), ("extra", b"never referenced")],
    ));
    let started = started(reply);
    assert_eq!(started.api_secret.len(), 64);

    let spawned = harness.fake().spawned();
    assert_eq!(spawned.len(), 1);
    let run = &spawned[0];
    let api = &run.config["services"][0];
    assert_eq!(
        *api,
        json!({
            "type": "api",
            "tag": "boxpilot-api",
            "listen": "127.0.0.1",
            "listen_port": started.api_port,
            "secret": started.api_secret,
            "access_control_allow_origin": ["http://boxpilot.invalid"]
        })
    );
    // The fake listens where the config says: the GUI reaches it there.
    TcpStream::connect((Ipv4Addr::LOCALHOST, started.api_port)).expect("the api port answers");
    // The helper's sing-box never sets the system proxy, whatever was
    // asked: on macOS the helper sets it itself (`runcfg::SYSTEM_PROXY`).
    let proxy =
        json!({"type": "mixed", "tag": "proxy", "listen": "127.0.0.1", "listen_port": 7890});
    assert_eq!(run.config["inbounds"][1], proxy);
    // Attachments land only in the run directory, and only those the
    // config refers to.
    let layout = &harness.fake().layout;
    assert_eq!(run.run_dir.parent().unwrap(), layout.runs_dir());
    assert_eq!(run.files.keys().collect::<Vec<_>>(), ["attachment-67656f"]);
    assert_eq!(run.files["attachment-67656f"], b"rule bytes");
    assert_eq!(
        run.config["route"]["rule_set"][0]["path"],
        json!(run.run_dir.join("attachment-67656f").to_str().unwrap())
    );
    let user_dir = layout.user_dir(ADMIN_SID).unwrap();
    assert_eq!(
        run.config["experimental"]["cache_file"],
        json!({"path": user_dir.join("cache.db").to_str().unwrap(), "enabled": true})
    );
    assert_eq!(names(layout.state_dir()), ["runs", "users"]);

    assert_eq!(status(&mut client), (RunState::Running, None));
    assert_eq!(client.ask(&Request::Stop), Reply::Stopped);
    assert_eq!(client.event(), exited(1));
    assert_eq!(
        status(&mut client),
        (
            RunState::Stopped,
            Some(ExitInfo {
                code: Some(1),
                signal: None
            })
        )
    );
    assert!(!run.run_dir.exists(), "the run directory goes with the run");
    assert_eq!(names(&layout.runs_dir()), Vec::<String>::new());
    client.close();
    assert_eq!(served.join().unwrap(), Ended::Eof);
}

#[test]
fn refused_configs_are_answered_and_the_connection_stays() {
    let harness = Harness::new();
    let (mut client, served) = harness.connect(admin());
    client.hello();
    let reply = client.ask(&start_request(
        json!({
            "inbounds": [{"type": "socks"}],
            "outbounds": [{"type": "tor", "tag": "t"}],
            "log": {"output": "C:\\Windows\\x.log", "level": "info"},
            "route": {"rule_set": [{"type": "local", "tag": "r", "path": "C:\\rules.srs"}]}
        }),
        &[],
    ));
    let Reply::Refused { refusals, omitted } = reply else {
        panic!("expected refused, got {reply:?}");
    };
    assert_eq!(omitted, 0);
    assert_eq!(
        refusals
            .iter()
            .map(|r| (r.pointer.as_str(), r.code.clone()))
            .collect::<Vec<_>>(),
        [
            ("/inbounds", RefusalCode::Inbounds),
            ("/outbounds/0/type", RefusalCode::RunsProgram),
            ("/route/rule_set/0/path", RefusalCode::LocalFile),
        ]
    );
    assert_eq!(status(&mut client), (RunState::Stopped, None));
    assert!(harness.fake().spawned().is_empty());
    assert_eq!(
        names(&harness.fake().layout.runs_dir()),
        Vec::<String>::new()
    );
    client.close();
    assert_eq!(served.join().unwrap(), Ended::Eof);
}

/// A refused restart leaves the sing-box already running alone.
#[test]
fn a_refused_restart_keeps_the_running_sing_box() {
    let harness = Harness::new();
    let (mut client, _served) = harness.connect(admin());
    client.hello();
    started(client.ask(&plain_start()));
    let reply = client.ask(&start_request(json!({"inbounds": []}), &[]));
    assert!(matches!(reply, Reply::Refused { .. }), "{reply:?}");
    assert_eq!(status(&mut client), (RunState::Running, None));
    assert_eq!(harness.fake().stops.load(Ordering::SeqCst), 0);
}

#[test]
fn a_restart_from_the_same_connection_replaces_its_sing_box() {
    let harness = Harness::new();
    let (mut client, _served) = harness.connect(admin());
    client.hello();
    let first = started(client.ask(&plain_start()));
    let second = started(client.ask(&plain_start()));
    assert_ne!(first.api_secret, second.api_secret);
    // The first run's exit was set off by the second start, so it follows
    // that start's reply.
    assert_eq!(client.event(), exited(1));
    assert_eq!(harness.fake().spawned().len(), 2);
    assert_eq!(harness.fake().stops.load(Ordering::SeqCst), 1);
    assert_eq!(
        status(&mut client),
        (
            RunState::Running,
            Some(ExitInfo {
                code: Some(1),
                signal: None
            })
        )
    );
}

#[test]
fn a_failed_spawn_frees_the_slot_and_the_run_directory() {
    let harness = Harness::new();
    harness.fake().set_script(Script {
        fail_spawn: true,
        ..Script::default()
    });
    let (mut client, _served) = harness.connect(admin());
    client.hello();
    assert_eq!(
        client.ask(&plain_start()),
        Reply::Error {
            code: ErrorCode::Internal,
            message: "the fake refuses to spawn".into()
        }
    );
    assert_eq!(status(&mut client), (RunState::Stopped, None));
    assert!(harness.core.is_idle());
    assert_eq!(
        names(&harness.fake().layout.runs_dir()),
        Vec::<String>::new()
    );
}

#[test]
fn sing_box_exiting_by_itself_reaches_its_connection() {
    let harness = Harness::new();
    harness.fake().set_script(Script {
        lines: 2,
        exit_code: Some(3),
        ..Script::default()
    });
    let (mut client, _served) = harness.connect(admin());
    client.hello();
    started(client.ask(&plain_start()));
    assert_eq!(
        client.event(),
        Event::Log {
            line: "INFO[0000] line 0".into(),
            truncated: false
        }
    );
    assert_eq!(
        client.event(),
        Event::Log {
            line: "INFO[0000] line 1".into(),
            truncated: false
        }
    );
    assert_eq!(client.event(), exited(3));
    assert_eq!(
        status(&mut client),
        (
            RunState::Stopped,
            Some(ExitInfo {
                code: Some(3),
                signal: None
            })
        )
    );
    assert_eq!(harness.fake().stops.load(Ordering::SeqCst), 0);
}

// ---- one sing-box, machine-wide ----

#[test]
fn another_connection_is_busy_and_may_stop_the_first() {
    let harness = Harness::new();
    harness.fake().set_script(Script {
        lines: 3,
        ..Script::default()
    });
    let (mut first, _first_served) = harness.connect(admin());
    let (mut second, _second_served) = harness.connect(other_admin());
    first.hello();
    second.hello();
    started(first.ask(&plain_start()));
    assert_eq!(error_code(second.ask(&plain_start())), ErrorCode::Busy);
    assert_eq!(status(&mut second), (RunState::Running, None));
    assert_eq!(second.ask(&Request::Stop), Reply::Stopped);
    // Only the starting connection gets the run's events.
    for n in 0..3 {
        assert_eq!(
            first.event(),
            Event::Log {
                line: format!("INFO[0000] line {n}"),
                truncated: false
            }
        );
    }
    assert_eq!(first.event(), exited(1));
    assert_eq!(
        second.recv_by(Instant::now() + Duration::from_millis(100)),
        Err(()),
        "nothing arrives unasked on the other connection"
    );
    assert_eq!(harness.fake().spawned().len(), 1);
    // The slot is free again for either.
    started(second.ask(&plain_start()));
}

/// Wait for the core to have asked `n` sing-boxes to stop in all.
fn wait_stops(harness: &Harness, n: usize) {
    let deadline = Instant::now() + PATIENCE;
    while harness.fake().stops.load(Ordering::SeqCst) < n {
        assert!(
            Instant::now() < deadline,
            "sing-box was never asked to stop"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

/// A start from another connection while the first's sing-box is being
/// stopped, because its connection ended, waits for that stop rather than
/// being turned away: it is often the same user's BoxPilot, starting again
/// after a stop it gave up waiting for.
#[test]
fn a_start_waits_out_another_connections_sing_box_being_stopped() {
    let harness = Harness::new();
    harness.fake().set_script(Script {
        stop_delay: Duration::from_millis(300),
        ..Script::default()
    });
    let (mut first, first_served) = harness.connect(admin());
    let (mut second, _second_served) = harness.connect(admin());
    first.hello();
    second.hello();
    started(first.ask(&plain_start()));
    first.close();
    wait_stops(&harness, 1);
    started(second.ask(&plain_start()));
    assert_eq!(first_served.join().unwrap(), Ended::Eof);
    assert_eq!(
        status(&mut second),
        (
            RunState::Running,
            Some(ExitInfo {
                code: Some(1),
                signal: None
            })
        )
    );
    assert_eq!(harness.fake().spawned().len(), 2);
}

/// A `stop` stops the sing-box that ran when it came, and only that one: a
/// start another connection makes as it exits keeps running.
#[test]
fn a_stop_leaves_alone_a_run_started_once_it_is_done() {
    let harness = Harness::new();
    harness.fake().set_script(Script {
        stop_delay: Duration::from_millis(300),
        ..Script::default()
    });
    let (mut first, _first_served) = harness.connect(admin());
    let (mut second, _second_served) = harness.connect(other_admin());
    let (mut third, _third_served) = harness.connect(admin());
    first.hello();
    second.hello();
    third.hello();
    started(first.ask(&plain_start()));
    second.send(&Request::Stop);
    wait_stops(&harness, 1);
    started(third.ask(&plain_start()));
    assert_eq!(second.reply(), Reply::Stopped);
    assert_eq!(first.event(), exited(1));
    assert_eq!(status(&mut third).0, RunState::Running);
    assert_eq!(harness.fake().stops.load(Ordering::SeqCst), 1);
}

#[test]
fn stop_with_nothing_running_is_stopped() {
    let harness = Harness::new();
    let (mut client, _served) = harness.connect(admin());
    client.hello();
    assert_eq!(client.ask(&Request::Stop), Reply::Stopped);
    assert_eq!(harness.fake().stops.load(Ordering::SeqCst), 0);
}

// ---- the end of a connection stops its sing-box ----

#[test]
fn the_end_of_the_stream_stops_sing_box() {
    let harness = Harness::new();
    let (mut client, served) = harness.connect(admin());
    client.hello();
    let run = started(client.ask(&plain_start()));
    client.close();
    assert_eq!(served.join().unwrap(), Ended::Eof);
    wait_idle(&harness);
    assert_eq!(harness.fake().stops.load(Ordering::SeqCst), 1);
    assert!(TcpStream::connect((Ipv4Addr::LOCALHOST, run.api_port)).is_err());
    assert_eq!(
        names(&harness.fake().layout.runs_dir()),
        Vec::<String>::new()
    );
}

#[test]
fn the_end_of_the_stream_mid_frame_stops_sing_box() {
    let harness = Harness::new();
    let (mut client, served) = harness.connect(admin());
    client.hello();
    started(client.ask(&plain_start()));
    client.send_bytes(&[0, 0, 0, 17, 1, b'{']);
    client.close();
    assert_eq!(served.join().unwrap(), Ended::Eof);
    wait_idle(&harness);
    assert_eq!(harness.fake().stops.load(Ordering::SeqCst), 1);
}

/// Another connection ending leaves the running sing-box alone.
#[test]
fn only_the_starting_connection_ending_stops_sing_box() {
    let harness = Harness::new();
    let (mut first, _first_served) = harness.connect(admin());
    first.hello();
    started(first.ask(&plain_start()));
    let (mut second, second_served) = harness.connect(other_admin());
    second.hello();
    second.close();
    assert_eq!(second_served.join().unwrap(), Ended::Eof);
    assert_eq!(status(&mut first), (RunState::Running, None));
    assert_eq!(harness.fake().stops.load(Ordering::SeqCst), 0);
}

#[test]
fn a_protocol_error_stops_sing_box_too() {
    let harness = Harness::new();
    let (mut client, served) = harness.connect(admin());
    client.hello();
    started(client.ask(&plain_start()));
    client.send(&Request::hello());
    assert_eq!(error_code(client.reply()), ErrorCode::BadRequest);
    assert_eq!(client.recv(), None);
    assert_eq!(
        served.join().unwrap(),
        Ended::Protocol(ErrorCode::BadRequest)
    );
    wait_idle(&harness);
    assert_eq!(harness.fake().stops.load(Ordering::SeqCst), 1);
}

// ---- deadlines ----

#[test]
fn a_peer_that_never_says_hello_is_closed() {
    let harness = Harness::with(short_timeouts(), Budget::DEFAULT_LIMIT);
    let (mut client, served) = harness.connect(admin());
    let began = Instant::now();
    assert_eq!(served.join().unwrap(), Ended::Deadline);
    assert!(began.elapsed() >= Duration::from_millis(200));
    assert_eq!(client.recv(), None);
}

#[test]
fn a_frame_that_stalls_is_closed_and_its_sing_box_stopped() {
    let harness = Harness::with(short_timeouts(), Budget::DEFAULT_LIMIT);
    let (mut client, served) = harness.connect(admin());
    client.hello();
    started(client.ask(&plain_start()));
    // Waiting between requests is fine, for longer than any deadline.
    thread::sleep(Duration::from_millis(500));
    assert_eq!(status(&mut client), (RunState::Running, None));
    // A frame begun is not.
    client.send_bytes(&[0, 0]);
    assert_eq!(served.join().unwrap(), Ended::Deadline);
    wait_idle(&harness);
    assert_eq!(harness.fake().stops.load(Ordering::SeqCst), 1);
}

#[test]
fn a_start_that_stalls_between_its_blobs_is_closed() {
    let harness = Harness::with(short_timeouts(), Budget::DEFAULT_LIMIT);
    let (mut client, served) = harness.connect(admin());
    client.hello();
    let header = br#"{"type":"start","config_len":2,"attachments":[{"id":"a","len":3}],"options":{"ipv6":false,"proxy_port":7890,"allow_lan":false,"system_proxy":false}}"#;
    let mut bytes = (header.len() as u32).to_be_bytes().to_vec();
    bytes.push(1);
    bytes.extend_from_slice(header);
    bytes.extend_from_slice(&[0, 0, 0, 2, 2, b'{', b'}']);
    client.send_bytes(&bytes);
    // Every frame is whole, so no frame deadline runs; the attachment's
    // blob never comes, and the request's deadline closes the connection.
    let began = Instant::now();
    assert_eq!(served.join().unwrap(), Ended::Deadline);
    assert!(began.elapsed() >= Duration::from_millis(300));
    assert!(harness.fake().spawned().is_empty());
}

// ---- a peer that doesn't read ----

#[test]
fn a_slow_reader_loses_log_lines_but_gets_its_reply_and_exited() {
    let mut harness = Harness::with(
        ConnConfig {
            outbox: OutboxLimits {
                max_log_lines: 8,
                max_log_bytes: 64 * 1024,
            },
            timeouts: Timeouts {
                write: Duration::from_secs(5),
                ..Timeouts::default()
            },
            ..ConnConfig::default()
        },
        Budget::DEFAULT_LIMIT,
    );
    harness.pipe_capacity = 1024;
    harness.fake().set_script(Script {
        lines: 2000,
        exit_code: Some(0),
        ..Script::default()
    });
    let (mut client, served) = harness.connect(admin());
    client.hello();
    client.send(&plain_start());
    // sing-box prints everything and exits while nobody reads.
    wait_spawned(&harness, 1);
    wait_idle(&harness);
    thread::sleep(Duration::from_millis(100));
    assert!(matches!(client.reply(), Reply::Started(_)));
    let mut logs = 0;
    let exit = loop {
        match client.event() {
            Event::Log { .. } => logs += 1,
            Event::Exited(exit) => break exit,
        }
    };
    assert_eq!(
        exit,
        ExitInfo {
            code: Some(0),
            signal: None
        }
    );
    assert!((1..2000).contains(&logs), "{logs} of 2000 lines arrived");
    client.close();
    assert_eq!(served.join().unwrap(), Ended::Eof);
}

#[test]
fn a_reader_that_stops_reading_is_closed_and_its_sing_box_stopped() {
    let mut harness = Harness::with(
        ConnConfig {
            timeouts: Timeouts {
                write: Duration::from_millis(100),
                ..Timeouts::default()
            },
            ..ConnConfig::default()
        },
        Budget::DEFAULT_LIMIT,
    );
    harness.pipe_capacity = 1024;
    harness.fake().set_script(Script {
        lines: 2000,
        ..Script::default()
    });
    let (mut client, served) = harness.connect(admin());
    client.hello();
    client.send(&plain_start());
    assert_eq!(served.join().unwrap(), Ended::WriteFailed);
    wait_idle(&harness);
    assert_eq!(harness.fake().stops.load(Ordering::SeqCst), 1);
    // What the pipe held still reads, then the end.
    assert!(matches!(client.reply(), Reply::Started(_)));
    let rest = client.rest();
    assert!(rest
        .iter()
        .all(|m| matches!(m, ToClient::Event(Event::Log { .. }))));
}

/// Requests sent without reading the replies are answered one at a time,
/// in order: each is read only once the one before is written.
#[test]
fn pipelined_requests_are_answered_in_order() {
    let harness = Harness::new();
    let (mut client, _served) = harness.connect(user());
    let mut bytes = encode_request(&Request::hello());
    for _ in 0..3 {
        bytes.extend(encode_request(&Request::Status));
    }
    client.send_bytes(&bytes);
    assert!(matches!(client.reply(), Reply::Hello(_)));
    for _ in 0..3 {
        assert!(matches!(client.reply(), Reply::Status { .. }));
    }
}

fn encode_request(request: &Request) -> Vec<u8> {
    boxpilot_protocol::encode_request(request, &Limits::default()).unwrap()
}

// ---- memory ----

/// A `start` header declaring `config_len` bytes, then `sent` of them.
fn partial_start(config_len: usize, sent: usize) -> Vec<u8> {
    let header = format!(
        r#"{{"type":"start","config_len":{config_len},"attachments":[],"options":{{"ipv6":false,"proxy_port":7890,"allow_lan":false,"system_proxy":false}}}}"#
    );
    let mut bytes = (header.len() as u32).to_be_bytes().to_vec();
    bytes.push(1);
    bytes.extend_from_slice(header.as_bytes());
    bytes.extend_from_slice(&(config_len as u32).to_be_bytes());
    bytes.push(2);
    bytes.extend(std::iter::repeat_n(b' ', sent));
    bytes
}

#[test]
fn the_budget_bounds_what_all_connections_hold() {
    // The decoder sets 64 KiB aside for a large payload's first bytes: room
    // for one such connection, not two.
    let harness = Harness::with(ConnConfig::default(), 100 * 1024);
    let (mut first, first_served) = harness.connect(admin());
    let (mut second, second_served) = harness.connect(admin());
    first.hello();
    second.hello();
    first.send_bytes(&partial_start(1024 * 1024, 10));
    // The reader may see the bytes in pieces: while the JSON header is
    // still arriving, the budget holds what the decoder has of it. Wait for
    // the blob's reservation, the state this test is about.
    let deadline = Instant::now() + PATIENCE;
    while harness.budget.used() != 64 * 1024 {
        assert!(
            Instant::now() < deadline,
            "the budget holds {} bytes",
            harness.budget.used()
        );
        thread::sleep(Duration::from_millis(5));
    }
    second.send_bytes(&partial_start(1024 * 1024, 10));
    assert_eq!(error_code(second.reply()), ErrorCode::Busy);
    assert_eq!(second.recv(), None);
    assert_eq!(second_served.join().unwrap(), Ended::OverBudget);
    first.close();
    assert_eq!(first_served.join().unwrap(), Ended::Eof);
    assert_eq!(harness.budget.used(), 0);
}

/// Before `hello`, and for a caller that may not start, only small frames
/// pass, refused from the header before anything is held.
#[test]
fn caps_narrow_to_what_the_session_takes_next() {
    let harness = Harness::new();
    let (mut client, served) = harness.connect(user());
    client.hello();
    client.send_bytes(&[0, 0, 0x20, 0, 1]);
    assert_eq!(
        client.reply(),
        Reply::Error {
            code: ErrorCode::BadRequest,
            message: "a JSON frame of 8192 bytes is over the 4096-byte limit".into()
        }
    );
    assert_eq!(
        served.join().unwrap(),
        Ended::Protocol(ErrorCode::BadRequest)
    );
    assert_eq!(harness.budget.used(), 0);
}

#[test]
fn the_budget_moves_by_exactly_what_is_held() {
    let budget = Budget::new(100);
    let mut held = 0;
    assert!(budget.adjust(&mut held, 60));
    assert_eq!((held, budget.used()), (60, 60));
    let mut other = 0;
    assert!(!budget.adjust(&mut other, 41));
    assert_eq!((other, budget.used()), (0, 60));
    assert!(budget.adjust(&mut other, 40));
    assert!(budget.adjust(&mut held, 10));
    assert_eq!(budget.used(), 50);
    assert!(budget.adjust(&mut held, 0));
    assert!(budget.adjust(&mut other, 0));
    assert_eq!(budget.used(), 0);
}

// ---- connection slots ----

/// Read-only callers get at most their share of the connections; callers
/// that may start are always admitted, and a slot comes back when its
/// connection ends.
#[test]
fn read_only_callers_cant_take_every_connection() {
    let slots = ReadOnlySlots::new(2);
    let first = slots.admit(Authority::ReadOnly).expect("under the cap");
    let second = slots.admit(Authority::ReadOnly).expect("at the cap");
    assert_eq!(slots.used(), 2);
    assert!(slots.admit(Authority::ReadOnly).is_none());
    assert_eq!(slots.used(), 2, "a refusal holds nothing");
    let starters: Vec<_> = (0..5)
        .map(|_| slots.admit(Authority::MayStart).expect("always"))
        .collect();
    assert_eq!(
        slots.used(),
        2,
        "callers that may start hold no read-only slot"
    );
    drop(starters);
    drop(first);
    assert_eq!(slots.used(), 1);
    let third = slots.admit(Authority::ReadOnly).expect("a slot came back");
    drop((second, third));
    assert_eq!(slots.used(), 0);
}

#[test]
fn no_read_only_slots_admits_only_callers_that_may_start() {
    let slots = ReadOnlySlots::new(0);
    assert!(slots.admit(Authority::ReadOnly).is_none());
    assert!(slots.admit(Authority::MayStart).is_some());
    assert_eq!(slots.used(), 0);
}
