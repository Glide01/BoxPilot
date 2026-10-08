//! Helpers the test files share.

#![allow(dead_code)]

use boxpilot_protocol::{
    Authority, Frame, FrameDecoder, Limits, ProtocolError, Request, ServerSession, TunOptions,
};

/// A frame with any type byte and payload, the header written by hand.
pub fn raw(type_byte: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = (payload.len() as u32).to_be_bytes().to_vec();
    out.push(type_byte);
    out.extend_from_slice(payload);
    out
}

pub fn json_bytes(text: &str) -> Vec<u8> {
    raw(0x01, text.as_bytes())
}

pub fn blob_bytes(bytes: &[u8]) -> Vec<u8> {
    raw(0x02, bytes)
}

pub fn json(text: &str) -> Frame {
    Frame::Json(text.to_owned())
}

pub fn blob(bytes: &[u8]) -> Frame {
    Frame::Blob(bytes.to_vec())
}

pub const HELLO: &str = r#"{"type":"hello","protocol_version":1}"#;

pub fn options() -> TunOptions {
    TunOptions {
        ipv6: true,
        proxy_port: 7890,
        allow_lan: false,
        system_proxy: true,
    }
}

/// A `start`'s JSON, attachments declared as `(id, len)`.
pub fn start_json(config: &str, attachments: &[(&str, u64)]) -> String {
    let attachments: Vec<_> = attachments
        .iter()
        .map(|(id, len)| serde_json::json!({"id": id, "len": len}))
        .collect();
    serde_json::json!({
        "type": "start",
        "config": config,
        "attachments": attachments,
        "options": {"ipv6": true, "proxy_port": 7890, "allow_lan": false, "system_proxy": true}
    })
    .to_string()
}

/// One connection on the helper's side, driven as the crate docs say: the
/// decoder's caps narrowed after every frame unless `narrow` is off.
pub struct Connection {
    pub decoder: FrameDecoder,
    pub session: ServerSession,
    pub narrow: bool,
}

impl Connection {
    pub fn new(authority: Authority, limits: Limits) -> Self {
        let session = ServerSession::new(authority, limits);
        Self {
            decoder: FrameDecoder::new(session.frame_caps()),
            session,
            narrow: true,
        }
    }

    /// The same, with the decoder at the widest caps throughout.
    pub fn wide(authority: Authority, limits: Limits) -> Self {
        Self {
            decoder: FrameDecoder::new(limits.to_helper_caps()),
            session: ServerSession::new(authority, limits),
            narrow: false,
        }
    }

    /// Feed `input`; every request yielded or error raised, in order. Each
    /// request is answered (`replied`) before the next frame.
    pub fn feed(&mut self, mut input: &[u8]) -> Vec<Result<Request, ProtocolError>> {
        let mut out = Vec::new();
        loop {
            input = &input[self.decoder.feed(input)..];
            let step = match self.decoder.next_frame() {
                Ok(Some(frame)) => self.session.accept(frame),
                Ok(None) => {
                    assert!(
                        input.is_empty(),
                        "next_frame gave nothing, but input is left"
                    );
                    return out;
                }
                Err(error) => Err(error),
            };
            if self.narrow {
                self.decoder.set_caps(self.session.frame_caps());
            }
            match step {
                Ok(Some(request)) => {
                    out.push(Ok(request));
                    self.session.replied();
                }
                Ok(None) => {}
                Err(error) => {
                    out.push(Err(error));
                    return out;
                }
            }
        }
    }
}

/// A deterministic xorshift64 generator: no crates, same sequence every run.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// Uniform enough in `0..n`; `n` must not be 0.
    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    pub fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }

    pub fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next() as u8).collect()
    }

    /// Fewer than `max` random bytes.
    pub fn some_bytes(&mut self, max: usize) -> Vec<u8> {
        let len = self.below(max);
        self.bytes(len)
    }

    /// `input` cut into random pieces, each fed separately.
    pub fn splits<'a>(&mut self, input: &'a [u8]) -> Vec<&'a [u8]> {
        let mut pieces = Vec::new();
        let mut rest = input;
        while !rest.is_empty() {
            let n = 1 + self.below(rest.len().min(64));
            pieces.push(&rest[..n]);
            rest = &rest[n..];
        }
        pieces
    }
}
