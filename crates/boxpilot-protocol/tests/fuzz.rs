//! A deterministic fuzz loop over the helper's side: random bytes, random
//! mutations of valid conversations, and conversations built to break one
//! rule at a time, fed in random splits through `FrameDecoder` and
//! `ServerSession`. Nothing may panic, the decoder may never hold more than
//! its caps allow, and no request that breaks a session rule may come out.
//! (ADR 0006 asks for `cargo fuzz` too; this keeps a fast, seeded version
//! in every `cargo test`.)

mod common;

use boxpilot_policy::is_attachment_id;
use boxpilot_protocol::{
    encode_request, encode_to_client, Authority, Frame, FrameDecoder, Limits, Request,
    ServerSession, StartRequest, TunOptions, HEADER_LEN, PROTOCOL_VERSION,
};
use common::{blob_bytes, json_bytes, raw, Rng, HELLO};
use std::collections::{BTreeMap, BTreeSet};

/// Small enough that random input reaches every limit.
fn small_limits() -> Limits {
    Limits {
        max_request_json: 2048,
        max_control_json: 256,
        max_blob: 50,
        max_start_total: 120,
        max_attachments: 4,
        ..Limits::default()
    }
}

/// What the fuzz saw, to prove it reached past the first byte.
#[derive(Default)]
struct Seen {
    requests: BTreeMap<&'static str, usize>,
    starts_with_attachments: usize,
    errors: BTreeMap<String, usize>,
}

/// The rules, checked from outside against every request that comes out.
struct Rules {
    authority: Authority,
    limits: Limits,
    hello: bool,
    outstanding: bool,
}

impl Rules {
    fn check(&mut self, request: &Request, seen: &mut Seen) {
        assert!(!self.outstanding, "a request while one is outstanding");
        let name = match request {
            Request::Hello { protocol_version } => {
                assert!(!self.hello, "a second hello");
                assert_eq!(*protocol_version, PROTOCOL_VERSION);
                self.hello = true;
                "hello"
            }
            Request::Status => "status",
            Request::Stop => "stop",
            Request::Start(start) => {
                self.check_start(start);
                if !start.attachments.is_empty() {
                    seen.starts_with_attachments += 1;
                }
                "start"
            }
        };
        if name != "hello" {
            assert!(self.hello, "{name} before hello");
        }
        if matches!(request, Request::Start(_) | Request::Stop) {
            assert_eq!(
                self.authority,
                Authority::MayStart,
                "{name} while read-only"
            );
        }
        *seen.requests.entry(name).or_default() += 1;
        self.outstanding = true;
    }

    fn check_start(&self, start: &StartRequest) {
        assert_ne!(start.options.proxy_port, 0);
        assert!(start.attachments.len() <= self.limits.max_attachments);
        let mut ids = BTreeSet::new();
        let mut total = start.config.len();
        for (id, data) in &start.attachments {
            assert!(is_attachment_id(id), "{id:?}");
            assert!(ids.insert(id), "duplicate {id:?}");
            assert!(data.len() <= self.limits.max_blob);
            total += data.len();
        }
        assert!(total <= self.limits.max_start_total, "{total}");
    }
}

/// One connection's worth of `input`, fed in random splits.
fn run(
    input: &[u8],
    authority: Authority,
    limits: Limits,
    narrow: bool,
    rng: &mut Rng,
    seen: &mut Seen,
) {
    let mut session = ServerSession::new(authority, limits);
    let mut decoder = FrameDecoder::new(if narrow {
        session.frame_caps()
    } else {
        limits.to_helper_caps()
    });
    let mut rules = Rules {
        authority,
        limits,
        hello: false,
        outstanding: false,
    };
    let widest = limits.max_request_json.max(limits.max_blob);
    for mut piece in rng.splits(input) {
        loop {
            piece = &piece[decoder.feed(piece)..];
            // What the decoder holds stays within its caps; with narrowed
            // caps, a caller before hello or a read-only one gets 4 KiB-ish.
            assert!(decoder.buffered() <= HEADER_LEN + widest);
            assert!(decoder.reserved() <= widest);
            if narrow && (!rules.hello || authority == Authority::ReadOnly) {
                assert!(decoder.buffered() <= HEADER_LEN + limits.max_control_json);
            }
            let step = match decoder.next_frame() {
                Ok(Some(frame)) => session.accept(frame),
                Ok(None) => {
                    assert!(
                        piece.is_empty(),
                        "next_frame gave nothing, but input is left"
                    );
                    break;
                }
                Err(error) => {
                    // Poisoned: the same error again, nothing more taken.
                    assert_eq!(decoder.next_frame(), Err(error.clone()));
                    assert_eq!(decoder.feed(&json_bytes(HELLO)), 0);
                    Err(error)
                }
            };
            if narrow {
                decoder.set_caps(session.frame_caps());
            }
            match step {
                Ok(Some(request)) => {
                    rules.check(&request, seen);
                    if rng.chance(90) {
                        session.replied();
                        rules.outstanding = false;
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    // The helper can always send the answer.
                    assert!(encode_to_client(&error.reply().into(), &limits).is_ok());
                    if session.is_finished() {
                        // And the session refuses everything after.
                        for frame in [Frame::Json(HELLO.into()), Frame::Blob(Vec::new())] {
                            assert_eq!(session.accept(frame), Err(error.clone()));
                        }
                    }
                    let name = format!("{error:?}");
                    let name = name.split(['(', ' ', '{']).next().unwrap().to_owned();
                    *seen.errors.entry(name).or_default() += 1;
                    return;
                }
            }
        }
    }
}

const ID_POOL: [&str; 8] = ["a", "ca", "rules", "A", "a-b_9", "geoip", "k", "Z0"];
const BAD_IDS: [&str; 6] = ["", "a/b", "..", "é", "a b", "x.srs"];

fn random_options(rng: &mut Rng) -> TunOptions {
    TunOptions {
        ipv6: rng.chance(50),
        proxy_port: 1 + rng.below(65535) as u16,
        allow_lan: rng.chance(50),
        system_proxy: rng.chance(50),
    }
}

fn random_config(rng: &mut Rng) -> String {
    const SNIPPETS: [&str; 6] = [
        "{}",
        r#"{"log":{"level":"info"}}"#,
        r#"{"outbounds":[{"type":"direct","tag":"d"}]}"#,
        "\"\\u0000\"",
        "日本 \" \\ \n",
        "",
    ];
    SNIPPETS[rng.below(SNIPPETS.len())].to_owned()
}

/// A conversation the session accepts whole, under `limits`.
fn valid_conversation(rng: &mut Rng, limits: &Limits) -> Vec<u8> {
    let mut out = encode_request(&Request::hello(), limits).unwrap();
    for _ in 0..rng.below(5) {
        let request = match rng.below(4) {
            0 => Request::Status,
            1 => Request::Stop,
            _ => {
                let mut pool = ID_POOL.to_vec();
                let count = rng.below(limits.max_attachments.min(pool.len()) + 1);
                let mut budget = limits.max_start_total.saturating_sub(64);
                let mut attachments = Vec::new();
                for _ in 0..count {
                    let id = pool.swap_remove(rng.below(pool.len()));
                    let len = rng.below(budget.min(limits.max_blob).min(24) + 1);
                    budget -= len;
                    attachments.push((id.to_owned(), rng.bytes(len)));
                }
                Request::Start(StartRequest {
                    config: random_config(rng),
                    attachments,
                    options: random_options(rng),
                })
            }
        };
        out.extend(encode_request(&request, limits).unwrap());
    }
    out
}

const INTERESTING_LENGTHS: [u32; 9] = [0, 1, 2, 49, 50, 51, 256, 0x7fff_ffff, 0xffff_ffff];

fn mutate(rng: &mut Rng, input: &mut Vec<u8>, other: &[u8]) {
    for _ in 0..1 + rng.below(4) {
        let len = input.len();
        match rng.below(8) {
            0 if len > 0 => input[rng.below(len)] ^= 1 << rng.below(8),
            1 if len > 0 => {
                const BYTES: [u8; 10] = [0, 1, 2, 3, 0x7f, 0x80, 0xff, b'"', b'{', b'['];
                input[rng.below(len)] = BYTES[rng.below(BYTES.len())];
            }
            2 => {
                let at = rng.below(len + 1);
                let bytes = rng.some_bytes(8);
                input.splice(at..at, bytes);
            }
            3 if len > 0 => {
                let at = rng.below(len);
                let end = (at + 1 + rng.below(16)).min(len);
                input.drain(at..end);
            }
            4 if len > 0 => {
                let at = rng.below(len);
                let end = (at + 1 + rng.below(32)).min(len);
                let copy = input[at..end].to_vec();
                let to = rng.below(len + 1);
                input.splice(to..to, copy);
            }
            5 => input.truncate(rng.below(len + 1)),
            6 if len >= 4 => {
                let at = rng.below(len - 3);
                let value = INTERESTING_LENGTHS[rng.below(INTERESTING_LENGTHS.len())];
                input[at..at + 4].copy_from_slice(&value.to_be_bytes());
            }
            7 if !other.is_empty() => {
                let from = rng.below(other.len());
                input.extend_from_slice(&other[from..]);
            }
            _ => {}
        }
    }
}

/// A conversation of frames built to break the session's rules one at a
/// time: bad or repeated ids, wrong lengths, missing or extra blobs, zero
/// ports, wrong versions, garbage JSON, stray frames.
fn structured(rng: &mut Rng) -> Vec<u8> {
    let mut out = Vec::new();
    if rng.chance(85) {
        out.extend(json_bytes(HELLO));
    }
    for _ in 0..1 + rng.below(6) {
        match rng.below(12) {
            0 => {
                let version = [0, 1, 1, 2, u32::MAX][rng.below(5)];
                let hello = format!(r#"{{"type":"hello","protocol_version":{version}}}"#);
                out.extend(json_bytes(&hello));
            }
            1 => out.extend(json_bytes(r#"{"type":"status"}"#)),
            2 => out.extend(json_bytes(r#"{"type":"stop"}"#)),
            3..=7 => {
                let count = rng.below(7);
                let mut declared = Vec::new();
                for _ in 0..count {
                    let id = if rng.chance(10) {
                        BAD_IDS[rng.below(BAD_IDS.len())].to_owned()
                    } else if rng.chance(10) && !declared.is_empty() {
                        let (id, _): &(String, u64) = &declared[rng.below(declared.len())];
                        id.clone()
                    } else {
                        ID_POOL[rng.below(ID_POOL.len())].to_owned()
                    };
                    let len = match rng.below(20) {
                        0 => u64::MAX,
                        1 => 51,
                        2 => 120,
                        3..=6 => 40 + rng.below(11) as u64,
                        _ => rng.below(30) as u64,
                    };
                    declared.push((id, len));
                }
                let port = if rng.chance(10) { 0 } else { 7890 };
                let attachments: Vec<_> = declared
                    .iter()
                    .map(|(id, len)| serde_json::json!({"id": id, "len": len}))
                    .collect();
                let start = serde_json::json!({
                    "type": "start",
                    "config": random_config(rng),
                    "attachments": attachments,
                    "options": {"ipv6": false, "proxy_port": port, "allow_lan": true, "system_proxy": false},
                });
                out.extend(json_bytes(&start.to_string()));
                let blobs = if rng.chance(85) {
                    declared.len()
                } else {
                    rng.below(declared.len() + 2)
                };
                for i in 0..blobs {
                    let want = declared
                        .get(i)
                        .map_or(3, |(_, len)| (*len).min(60) as usize);
                    let len = match rng.below(12) {
                        0 => want + 1,
                        1 => want.saturating_sub(1),
                        _ => want,
                    };
                    out.extend(blob_bytes(&rng.bytes(len)));
                }
            }
            8 => out.extend(blob_bytes(&rng.some_bytes(8))),
            9 => {
                const GARBAGE: [&str; 8] = [
                    "{",
                    "[]",
                    r#"["stop"]"#,
                    r#"{"type":"status","x":1}"#,
                    r#"{"type":"logs"}"#,
                    r#"{"type":"stop","type":"stop"}"#,
                    r#"{"type":"start","config":"{}","attachments":[],"options":[true,1,true,true]}"#,
                    "null",
                ];
                out.extend(json_bytes(GARBAGE[rng.below(GARBAGE.len())]));
            }
            10 => {
                let type_byte = rng.next() as u8;
                out.extend(raw(type_byte, &rng.some_bytes(8)));
            }
            _ => out.extend(raw(0x01, &[0xc3, 0x28])),
        }
    }
    out
}

#[test]
fn fuzz_the_helper_side() {
    let mut rng = Rng::new(0x0b0c_5eed_2026_1008);
    let mut seen = Seen::default();
    let authorities = [Authority::MayStart, Authority::ReadOnly];
    for round in 0..18_000 {
        let limits = if round % 3 == 0 {
            Limits::default()
        } else {
            small_limits()
        };
        let authority = if rng.chance(80) {
            authorities[0]
        } else {
            authorities[1]
        };
        let narrow = rng.chance(50);
        let input = match round % 6 {
            0 => rng.some_bytes(200),
            1 => {
                // Random frames behind headers that pass.
                let mut out = Vec::new();
                for _ in 0..1 + rng.below(4) {
                    let type_byte = 1 + rng.below(2) as u8;
                    out.extend(raw(type_byte, &rng.some_bytes(64)));
                }
                out
            }
            2 | 3 => {
                let mut input = valid_conversation(&mut rng, &limits);
                let other = valid_conversation(&mut rng, &limits);
                mutate(&mut rng, &mut input, &other);
                input
            }
            _ => structured(&mut rng),
        };
        run(&input, authority, limits, narrow, &mut rng, &mut seen);
    }

    // The fuzz got past the first byte: every request came out, many
    // starts carried attachments, and most rules were broken somewhere.
    for name in ["hello", "status", "stop", "start"] {
        assert!(
            seen.requests.get(name).copied().unwrap_or(0) > 100,
            "{name}: {:?}",
            seen.requests
        );
    }
    assert!(
        seen.starts_with_attachments > 100,
        "{}",
        seen.starts_with_attachments
    );
    for error in [
        "UnknownFrameType",
        "UnexpectedFrame",
        "FrameTooLarge",
        "EmptyJson",
        "InvalidUtf8",
        "InvalidMessage",
        "HelloFirst",
        "RepeatedHello",
        "VersionMismatch",
        "RequestOutstanding",
        "Unauthorized",
        "ZeroProxyPort",
        "TooManyAttachments",
        "InvalidAttachmentId",
        "DuplicateAttachmentId",
        "AttachmentTooLarge",
        "StartTooLarge",
        "BlobLength",
    ] {
        assert!(
            seen.errors.contains_key(error),
            "never hit {error}: {:?}",
            seen.errors
        );
    }
}

/// A valid conversation stays valid in any split, narrowed or not.
#[test]
fn valid_conversations_pass_in_any_split() {
    let mut rng = Rng::new(42);
    for round in 0..2000 {
        let limits = if round % 2 == 0 {
            Limits::default()
        } else {
            small_limits()
        };
        let input = valid_conversation(&mut rng, &limits);
        let mut seen = Seen::default();
        let narrow = rng.chance(50);
        let mut session = ServerSession::new(Authority::MayStart, limits);
        let mut decoder = FrameDecoder::new(if narrow {
            session.frame_caps()
        } else {
            limits.to_helper_caps()
        });
        let mut rules = Rules {
            authority: Authority::MayStart,
            limits,
            hello: false,
            outstanding: false,
        };
        for mut piece in rng.splits(&input) {
            loop {
                piece = &piece[decoder.feed(piece)..];
                let Some(frame) = decoder.next_frame().unwrap() else {
                    break;
                };
                let step = session.accept(frame).unwrap();
                if narrow {
                    decoder.set_caps(session.frame_caps());
                }
                if let Some(request) = step {
                    rules.check(&request, &mut seen);
                    session.replied();
                    rules.outstanding = false;
                }
            }
        }
        assert!(!session.is_finished());
        assert_eq!(decoder.buffered(), 0);
    }
}
