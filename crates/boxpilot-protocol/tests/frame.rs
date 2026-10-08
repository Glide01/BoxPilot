//! The frame decoder against whole, split, oversized and malformed input.
//! What a JSON frame says is the session's business (tests/session.rs);
//! here a frame is a header and bytes.

mod common;

use boxpilot_protocol::{
    encode_frame, Frame, FrameCaps, FrameDecoder, FrameType, Limits, ProtocolError, HEADER_LEN,
};
use common::{blob_bytes, json_bytes, raw, Rng};

const MIB: usize = 1024 * 1024;

fn caps(max_json: Option<usize>, max_blob: Option<usize>) -> FrameCaps {
    FrameCaps { max_json, max_blob }
}

fn helper_decoder() -> FrameDecoder {
    FrameDecoder::new(Limits::default().to_helper_caps())
}

/// Feed all of `pieces` in turn, taking every frame that completes.
fn decode_pieces(decoder: &mut FrameDecoder, pieces: &[&[u8]]) -> Vec<Frame> {
    let mut frames = Vec::new();
    for piece in pieces {
        let mut input = *piece;
        loop {
            input = &input[decoder.feed(input)..];
            match decoder.next_frame().expect("valid frames") {
                Some(frame) => frames.push(frame),
                None => {
                    assert!(input.is_empty());
                    break;
                }
            }
        }
    }
    frames
}

fn sample() -> (Vec<u8>, Vec<Frame>) {
    let frames = vec![
        Frame::Json(r#"{"type":"hello","protocol_version":1}"#.into()),
        Frame::Blob(vec![0, 1, 2, 0xff, 0xfe]),
        Frame::Blob(Vec::new()),
        Frame::Json("\"日本語 – ünïcödé 😀\"".into()),
        Frame::Blob((0..=255).collect()),
        Frame::Json("{}".into()),
    ];
    let bytes = frames
        .iter()
        .flat_map(|frame| encode_frame(frame, &Limits::default().to_helper_caps()).unwrap())
        .collect();
    (bytes, frames)
}

// ---- Layout ----

#[test]
fn header_is_big_endian_length_then_type() {
    let caps = Limits::default().to_helper_caps();
    assert_eq!(
        encode_frame(&Frame::Json("{}".into()), &caps).unwrap(),
        [0, 0, 0, 2, 0x01, b'{', b'}']
    );
    assert_eq!(
        encode_frame(&Frame::Blob(vec![9; 258]), &caps).unwrap()[..HEADER_LEN],
        [0, 0, 1, 2, 0x02]
    );
    assert_eq!(FrameType::Json.byte(), 0x01);
    assert_eq!(FrameType::Blob.byte(), 0x02);
    assert_eq!(FrameType::from_byte(0x01), Some(FrameType::Json));
    assert_eq!(FrameType::from_byte(0x02), Some(FrameType::Blob));
    assert_eq!(FrameType::from_byte(0x00), None);
}

// ---- Splits ----

#[test]
fn whole_input_gives_every_frame() {
    let (bytes, frames) = sample();
    assert_eq!(decode_pieces(&mut helper_decoder(), &[&bytes]), frames);
}

#[test]
fn one_byte_at_a_time_gives_the_same_frames() {
    let (bytes, frames) = sample();
    let pieces: Vec<&[u8]> = bytes.chunks(1).collect();
    let mut decoder = helper_decoder();
    assert_eq!(decode_pieces(&mut decoder, &pieces), frames);
    assert_eq!(decoder.buffered(), 0);
}

#[test]
fn random_splits_give_the_same_frames() {
    let (bytes, frames) = sample();
    let mut rng = Rng::new(0x5eed);
    for _ in 0..500 {
        let pieces = rng.splits(&bytes);
        assert_eq!(decode_pieces(&mut helper_decoder(), &pieces), frames);
    }
}

#[test]
fn every_split_point_inside_the_header_works() {
    let frame = json_bytes(r#"{"a":1}"#);
    for cut in 0..=frame.len() {
        let (a, b) = frame.split_at(cut);
        let frames = decode_pieces(&mut helper_decoder(), &[a, b]);
        assert_eq!(frames, [Frame::Json(r#"{"a":1}"#.into())], "cut at {cut}");
    }
}

/// `feed` stops at the end of a frame: the decoder holds one frame at most,
/// and the rest is fed again once it is taken.
#[test]
fn feed_stops_at_the_end_of_a_frame() {
    let mut input = json_bytes("{}");
    input.extend(blob_bytes(b"abc"));
    let mut decoder = helper_decoder();
    assert_eq!(decoder.feed(&input), HEADER_LEN + 2);
    assert_eq!(decoder.feed(&input[HEADER_LEN + 2..]), 0, "a frame waits");
    assert_eq!(decoder.buffered(), HEADER_LEN + 2);
    assert_eq!(decoder.next_frame(), Ok(Some(Frame::Json("{}".into()))));
    assert_eq!(decoder.feed(&input[HEADER_LEN + 2..]), HEADER_LEN + 3);
    assert_eq!(decoder.next_frame(), Ok(Some(Frame::Blob(b"abc".to_vec()))));
    assert_eq!(decoder.next_frame(), Ok(None));
}

// ---- Caps ----

/// A 4 GiB declaration is refused once its 5 header bytes are in, with
/// nothing reserved, and nothing after it is taken.
#[test]
fn four_gib_declared_is_refused_from_the_header_alone() {
    for type_byte in [0x01, 0x02] {
        let mut decoder = helper_decoder();
        let header = [0xff, 0xff, 0xff, 0xff, type_byte];
        assert_eq!(decoder.feed(&header[..4]), 4);
        assert_eq!(decoder.next_frame(), Ok(None));
        assert_eq!(decoder.buffered(), 4);
        assert_eq!(decoder.feed(&header[4..]), 1);
        let frame_type = FrameType::from_byte(type_byte).unwrap();
        let limit = match frame_type {
            FrameType::Json => 33 * MIB,
            FrameType::Blob => 32 * MIB,
        };
        let error = ProtocolError::FrameTooLarge {
            frame_type,
            len: u32::MAX as usize,
            limit,
        };
        assert_eq!(decoder.next_frame(), Err(error));
        assert_eq!(decoder.buffered(), 0);
        assert_eq!(decoder.reserved(), 0);
        assert_eq!(decoder.feed(&[0; 1024]), 0, "nothing more is taken");
    }
}

#[test]
fn caps_are_inclusive() {
    let mut decoder = FrameDecoder::new(caps(Some(10), Some(4)));
    let frames = decode_pieces(
        &mut decoder,
        &[&json_bytes("[1,2,3,4] "), &blob_bytes(b"1234")],
    );
    assert_eq!(frames.len(), 2);

    let mut decoder = FrameDecoder::new(caps(Some(10), Some(4)));
    assert_eq!(decoder.feed(&json_bytes("[1,2,3,4,5]")), HEADER_LEN);
    assert_eq!(
        decoder.next_frame(),
        Err(ProtocolError::FrameTooLarge {
            frame_type: FrameType::Json,
            len: 11,
            limit: 10
        })
    );

    let mut decoder = FrameDecoder::new(caps(Some(10), Some(4)));
    assert_eq!(decoder.feed(&blob_bytes(b"12345")), HEADER_LEN);
    assert_eq!(
        decoder.next_frame(),
        Err(ProtocolError::FrameTooLarge {
            frame_type: FrameType::Blob,
            len: 5,
            limit: 4
        })
    );
}

/// A large declared frame reserves only about what has arrived, so a peer
/// that declares 32 MiB and stalls holds no more than it sent.
#[test]
fn payload_memory_follows_what_arrived() {
    let mut decoder = helper_decoder();
    let header = [0x02, 0, 0, 0, 0x02]; // a blob of 32 MiB
    assert_eq!(decoder.feed(&header), HEADER_LEN);
    assert_eq!(decoder.reserved(), 0);
    assert_eq!(decoder.feed(&[7; 100]), 100);
    assert_eq!(decoder.buffered(), HEADER_LEN + 100);
    assert!(decoder.reserved() <= 64 * 1024, "{}", decoder.reserved());
    assert_eq!(decoder.feed(&vec![7; MIB]), MIB);
    assert!(
        decoder.reserved() <= 2 * (MIB + 100),
        "{}",
        decoder.reserved()
    );
    assert_eq!(decoder.next_frame(), Ok(None));
}

#[test]
fn reserve_never_passes_the_declared_length() {
    let len = 100_000;
    let mut decoder = helper_decoder();
    let frame = blob_bytes(&vec![1; len]);
    let mut rng = Rng::new(7);
    let pieces = rng.splits(&frame);
    let (last, pieces) = pieces.split_last().unwrap();
    for piece in pieces {
        assert_eq!(decoder.feed(piece), piece.len());
        assert!(decoder.reserved() <= len);
    }
    assert_eq!(decoder.feed(last), last.len());
    assert!(decoder.reserved() <= len);
    assert_eq!(decoder.next_frame(), Ok(Some(Frame::Blob(vec![1; len]))));
}

#[test]
fn a_frame_type_the_caps_refuse_is_refused_from_the_header() {
    let mut decoder = FrameDecoder::new(Limits::default().to_gui_caps());
    assert_eq!(decoder.feed(&blob_bytes(b"abc")), HEADER_LEN);
    assert_eq!(
        decoder.next_frame(),
        Err(ProtocolError::UnexpectedFrame(FrameType::Blob))
    );

    let mut decoder = FrameDecoder::new(caps(None, Some(10)));
    assert_eq!(decoder.feed(&json_bytes("{}")), HEADER_LEN);
    assert_eq!(
        decoder.next_frame(),
        Err(ProtocolError::UnexpectedFrame(FrameType::Json))
    );
}

#[test]
fn set_caps_applies_from_the_next_frame() {
    let mut decoder = FrameDecoder::new(caps(Some(100), None));
    let mut input = json_bytes("[1]");
    input.extend(json_bytes("[1,2]"));
    let taken = decoder.feed(&input);
    assert_eq!(decoder.next_frame(), Ok(Some(Frame::Json("[1]".into()))));
    decoder.set_caps(caps(Some(4), None));
    assert_eq!(decoder.feed(&input[taken..]), HEADER_LEN);
    assert_eq!(
        decoder.next_frame(),
        Err(ProtocolError::FrameTooLarge {
            frame_type: FrameType::Json,
            len: 5,
            limit: 4
        })
    );
}

// ---- Malformed frames ----

#[test]
fn unknown_frame_types_are_refused() {
    for type_byte in [0x00, 0x03, 0x10, 0x7f, 0x80, 0xff] {
        let mut decoder = helper_decoder();
        assert_eq!(decoder.feed(&raw(type_byte, b"{}")), HEADER_LEN);
        assert_eq!(
            decoder.next_frame(),
            Err(ProtocolError::UnknownFrameType(type_byte))
        );
    }
}

#[test]
fn an_empty_json_frame_is_refused_from_the_header() {
    let mut decoder = helper_decoder();
    assert_eq!(decoder.feed(&json_bytes("")), HEADER_LEN);
    assert_eq!(decoder.next_frame(), Err(ProtocolError::EmptyJson));
}

#[test]
fn an_empty_blob_is_a_frame() {
    let mut decoder = helper_decoder();
    assert_eq!(decoder.feed(&blob_bytes(b"")), HEADER_LEN);
    assert_eq!(decoder.next_frame(), Ok(Some(Frame::Blob(Vec::new()))));
    assert_eq!(decoder.buffered(), 0);
}

#[test]
fn a_json_frame_must_be_utf8() {
    let cases: [&[u8]; 5] = [
        b"\xff\xfe",
        b"{\"a\":\"\xc3\"}",        // a lead byte with no continuation
        b"\"\xed\xa0\x80\"",        // an encoded surrogate
        b"\"\xc0\xaf\"",            // an overlong `/`
        b"{\"type\":\"stop\"}\x80", // a stray continuation byte
    ];
    for payload in cases {
        let mut decoder = helper_decoder();
        assert_eq!(
            decoder.feed(&raw(0x01, payload)),
            HEADER_LEN + payload.len()
        );
        assert_eq!(decoder.next_frame(), Err(ProtocolError::InvalidUtf8));
    }
}

#[test]
fn an_error_poisons_the_decoder() {
    let mut decoder = helper_decoder();
    assert_eq!(decoder.feed(&raw(0x09, b"x")), HEADER_LEN);
    for _ in 0..3 {
        assert_eq!(
            decoder.next_frame(),
            Err(ProtocolError::UnknownFrameType(0x09))
        );
        assert_eq!(decoder.feed(&json_bytes("{}")), 0);
    }
    decoder.set_caps(Limits::default().to_helper_caps());
    assert_eq!(
        decoder.next_frame(),
        Err(ProtocolError::UnknownFrameType(0x09))
    );
}

// ---- Encoding ----

#[test]
fn encoding_over_a_cap_is_the_decoders_error() {
    let small = caps(Some(4), Some(4));
    assert_eq!(
        encode_frame(&Frame::Json("[1,2]".into()), &small),
        Err(ProtocolError::FrameTooLarge {
            frame_type: FrameType::Json,
            len: 5,
            limit: 4
        })
    );
    assert_eq!(
        encode_frame(&Frame::Blob(vec![0; 5]), &small),
        Err(ProtocolError::FrameTooLarge {
            frame_type: FrameType::Blob,
            len: 5,
            limit: 4
        })
    );
    assert_eq!(
        encode_frame(&Frame::Json(String::new()), &small),
        Err(ProtocolError::EmptyJson)
    );
    assert_eq!(
        encode_frame(&Frame::Blob(Vec::new()), &caps(Some(4), None)),
        Err(ProtocolError::UnexpectedFrame(FrameType::Blob))
    );
}

#[test]
fn debug_shows_no_payload() {
    let frame = Frame::Json(r#"{"password":"hunter2"}"#.into());
    assert_eq!(format!("{frame:?}"), "Frame::Json(22 bytes)");
    let mut decoder = helper_decoder();
    let _ = decoder.feed(&json_bytes(r#"{"password":"hunter2"}"#));
    assert!(!format!("{decoder:?}").contains("hunter2"));
}
