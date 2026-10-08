//! One connection, driven from its first byte to its close (ADR 0006 rule
//! 1, "What only the helper's I/O layer can do").
//!
//! Two threads per connection. The **reader** reads bytes with deadlines,
//! decodes frames ([`FrameDecoder`]), hands them to the protocol session
//! ([`ServerSession`]), narrows the decoder's caps after every frame, and
//! answers each request. The **writer** drains the connection's [`Outbox`]
//! with a write deadline. The reader:
//!
//! - holds the peer to deadlines: `hello` within [`Timeouts::hello`] of
//!   connecting, each frame within [`Timeouts::frame`] of its first byte,
//!   and each request, a `start` with all its blobs, within
//!   [`Timeouts::request`] of its first byte. Between requests there is no
//!   deadline: a GUI holds its connection for as long as its sing-box runs;
//! - charges what it holds (the decoder's reservation and a `start`'s blobs)
//!   to a [`Budget`] shared by every connection;
//! - reserves each reply's place in the outbox before acting, calls
//!   `replied()` before the reply is written, and reads the next request
//!   only once it is written: a peer that sends requests without reading
//!   replies can't queue more than one;
//! - answers any protocol error with its `error` reply, then closes.
//!
//! However the connection ends (end of stream, even mid-frame; a deadline;
//! a failed write; a protocol error), the sing-box it started is stopped.

#![forbid(unsafe_code)]

use crate::helper::{Caller, ConnId, Helper};
use crate::helper_log;
use crate::outbox::{Outbox, OutboxLimits};
use crate::transport::Transport;
use boxpilot_protocol::{
    encode_to_client, Authority, ErrorCode, Frame, FrameDecoder, Limits, ProtocolError, Reply,
    Request, ServerSession, ToClient,
};
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// How long the helper waits for a peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    /// From connecting to a whole `hello`.
    pub hello: Duration,
    /// From a frame's first byte to its last.
    pub frame: Duration,
    /// From a request's first byte to its last frame: a `start` with its
    /// config and every attachment.
    pub request: Duration,
    /// For each frame the helper writes.
    pub write: Duration,
}

impl Default for Timeouts {
    /// Generous for a local pipe, where 32 MiB take well under a second,
    /// and short enough that a stalled peer doesn't hold memory for long.
    fn default() -> Self {
        Self {
            hello: Duration::from_secs(10),
            frame: Duration::from_secs(30),
            request: Duration::from_secs(60),
            write: Duration::from_secs(10),
        }
    }
}

/// Everything one connection is driven with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnConfig {
    pub limits: Limits,
    pub timeouts: Timeouts,
    pub outbox: OutboxLimits,
    /// Bytes read from the transport at a time.
    pub read_chunk: usize,
}

impl Default for ConnConfig {
    fn default() -> Self {
        Self {
            limits: Limits::default(),
            timeouts: Timeouts::default(),
            outbox: OutboxLimits::default(),
            read_chunk: 64 * 1024,
        }
    }
}

/// The memory every connection's requests may hold at once: what their
/// decoders have reserved, and the blobs of `start`s still arriving.
#[derive(Debug)]
pub struct Budget {
    limit: usize,
    used: AtomicUsize,
}

impl Budget {
    /// Two whole `start`s at once (32 MiB each), and room for every other
    /// connection's small requests besides.
    pub const DEFAULT_LIMIT: usize = 80 * 1024 * 1024;

    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            used: AtomicUsize::new(0),
        }
    }

    pub fn used(&self) -> usize {
        self.used.load(Ordering::SeqCst)
    }

    /// Move one connection's share from `*held` to `want`. Shrinking always
    /// succeeds; growing fails, moving nothing, if it would put the total
    /// over the limit.
    fn adjust(&self, held: &mut usize, want: usize) -> bool {
        if want <= *held {
            self.used.fetch_sub(*held - want, Ordering::SeqCst);
            *held = want;
            return true;
        }
        let extra = want - *held;
        let mut used = self.used.load(Ordering::SeqCst);
        loop {
            if used.saturating_add(extra) > self.limit {
                return false;
            }
            match self.used.compare_exchange_weak(
                used,
                used + extra,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => {
                    *held = want;
                    return true;
                }
                Err(now) => used = now,
            }
        }
    }
}

/// How many connections from callers that may not start are served at
/// once. Any interactive user may connect, and between requests a
/// connection has no deadline, so without this cap another account could
/// hold every connection slot and keep administrators from starting TUN.
/// Read-only callers ask `hello` and `status`, which need no lasting
/// connection; past the cap, a new one is closed as soon as its token says
/// what it is, and the slots left stay free for callers that may start.
#[derive(Debug)]
pub struct ReadOnlySlots {
    limit: usize,
    used: AtomicUsize,
}

/// A connection admitted by [`ReadOnlySlots::admit`]: a read-only one
/// holds its slot until this is dropped.
#[must_use = "the slot is freed when this is dropped"]
#[derive(Debug)]
pub struct Admitted<'a> {
    slots: Option<&'a ReadOnlySlots>,
}

impl ReadOnlySlots {
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            used: AtomicUsize::new(0),
        }
    }

    /// Read-only connections being served.
    pub fn used(&self) -> usize {
        self.used.load(Ordering::SeqCst)
    }

    /// Admit a connection whose caller has `authority`: always, if it may
    /// start; if it is read-only, only while fewer than the limit are.
    pub fn admit(&self, authority: Authority) -> Option<Admitted<'_>> {
        if authority.may_start() {
            return Some(Admitted { slots: None });
        }
        self.used
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                (used < self.limit).then_some(used + 1)
            })
            .ok()
            .map(|_| Admitted { slots: Some(self) })
    }
}

impl Drop for Admitted<'_> {
    fn drop(&mut self) {
        if let Some(slots) = self.slots {
            slots.used.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

/// How a connection ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ended {
    /// The peer closed the connection, between frames or in the middle of
    /// one.
    Eof,
    /// The peer missed a deadline: `hello`, a frame, or a request.
    Deadline,
    /// The peer broke the protocol, and was answered with this code.
    Protocol(ErrorCode),
    /// The request would have held more memory than the [`Budget`] had
    /// left; the peer was answered `busy`.
    OverBudget,
    /// A write failed or missed its deadline: the peer stopped reading.
    WriteFailed,
    /// Reading failed.
    ReadFailed(io::ErrorKind),
}

/// Serve one connection until it ends, then stop the sing-box it started.
/// `caller` is what the OS said about the peer.
pub fn serve<T: Transport, H: Helper + ?Sized>(
    transport: &T,
    conn: ConnId,
    caller: &Caller,
    helper: &H,
    config: &ConnConfig,
    budget: &Budget,
) -> Ended {
    helper_log!(
        "connection {conn}: opened, {:?}, account {}",
        caller.authority,
        caller.user.as_deref().unwrap_or("unknown")
    );
    let outbox = Arc::new(Outbox::new(config.outbox));
    let (ended, wrote_all) = thread::scope(|scope| {
        let writer = scope.spawn(|| write_frames(transport, &outbox, config.timeouts.write));
        let mut reader = Reader {
            transport,
            conn,
            caller,
            helper,
            config,
            budget,
            outbox: &outbox,
            charged: 0,
        };
        let ended = reader.run();
        reader.uncharge();
        match ended {
            // The `error` reply is queued: let the writer send it, then stop.
            Ended::Protocol(_) | Ended::OverBudget => outbox.finish(),
            _ => {
                outbox.close();
                transport.close();
            }
        }
        helper.disconnected(conn);
        let wrote_all = writer.join().unwrap_or(false);
        (ended, wrote_all)
    });
    transport.close();
    // The writer closes the transport when a write fails, which the reader
    // sees as the end of the stream.
    let ended = match ended {
        Ended::Eof | Ended::ReadFailed(_) if !wrote_all => Ended::WriteFailed,
        ended => ended,
    };
    helper_log!(
        "connection {conn}: ended ({ended:?}); {} log lines dropped",
        outbox.dropped()
    );
    ended
}

/// The writer: every frame the outbox gives, each by its deadline. `false`
/// when a write failed, after which the connection is closed.
fn write_frames<T: Transport>(transport: &T, outbox: &Outbox, timeout: Duration) -> bool {
    while let Some(frame) = outbox.next() {
        if transport
            .write_all(&frame.bytes, Instant::now() + timeout)
            .is_err()
        {
            outbox.close();
            transport.close();
            return false;
        }
        outbox.sent(&frame);
    }
    true
}

struct Reader<'a, T, H: ?Sized> {
    transport: &'a T,
    conn: ConnId,
    caller: &'a Caller,
    helper: &'a H,
    config: &'a ConnConfig,
    budget: &'a Budget,
    outbox: &'a Arc<Outbox>,
    /// This connection's share of the budget.
    charged: usize,
}

impl<T: Transport, H: Helper + ?Sized> Reader<'_, T, H> {
    fn run(&mut self) -> Ended {
        let limits = self.config.limits;
        let timeouts = self.config.timeouts;
        let mut session = ServerSession::new(self.caller.authority, limits);
        let mut decoder = FrameDecoder::new(session.frame_caps());
        let mut buf = vec![0u8; self.config.read_chunk.max(1)];
        let opened = Instant::now();
        let mut greeted = false;
        let mut frame_began: Option<Instant> = None;
        let mut request_began: Option<Instant> = None;
        // The blobs of a `start` that is still arriving, which the session
        // holds.
        let mut blobs = 0usize;
        loop {
            let deadline = [
                (!greeted).then_some(opened + timeouts.hello),
                frame_began.map(|began| began + timeouts.frame),
                request_began.map(|began| began + timeouts.request),
            ]
            .into_iter()
            .flatten()
            .min();
            let n = match self.transport.read(&mut buf, deadline) {
                Ok(0) => return Ended::Eof,
                Ok(n) => n,
                Err(error) if error.kind() == io::ErrorKind::TimedOut => return Ended::Deadline,
                Err(error) => return Ended::ReadFailed(error.kind()),
            };
            let mut input = &buf[..n];
            while !input.is_empty() {
                if decoder.buffered() == 0 {
                    let now = Instant::now();
                    frame_began = Some(now);
                    request_began.get_or_insert(now);
                }
                input = &input[decoder.feed(input)..];
                if !self.charge(decoder.reserved() + blobs) {
                    return self.over_budget();
                }
                let frame = match decoder.next_frame() {
                    Ok(Some(frame)) => frame,
                    // `feed` took all of `input`: the frame isn't whole yet.
                    Ok(None) => break,
                    Err(error) => return self.protocol_error(error),
                };
                frame_began = None;
                if let Frame::Blob(bytes) = &frame {
                    blobs += bytes.len();
                }
                let step = session.accept(frame);
                decoder.set_caps(session.frame_caps());
                match step {
                    // A `start` still owes blobs.
                    Ok(None) => {}
                    Ok(Some(request)) => {
                        request_began = None;
                        greeted = true;
                        let answered = self.answer(&mut session, request);
                        blobs = 0;
                        self.charge(decoder.reserved());
                        if let Err(ended) = answered {
                            return ended;
                        }
                    }
                    Err(error) => return self.protocol_error(error),
                }
            }
        }
    }

    /// Act on a request and queue its reply, in the place reserved before
    /// acting; return once it is written.
    fn answer(&mut self, session: &mut ServerSession, request: Request) -> Result<(), Ended> {
        let reservation = self.outbox.reserve().ok_or(Ended::WriteFailed)?;
        let reply = match request {
            Request::Hello { .. } => self.helper.hello(self.caller),
            Request::Status => self.helper.status(),
            Request::Stop => self.helper.stop(self.caller),
            Request::Start(start) => self
                .helper
                .start(self.conn, self.caller, start, self.outbox),
        };
        session.replied();
        let ticket = reservation.fill(encode_reply(reply, &self.config.limits));
        if self.outbox.wait_written(ticket) {
            Ok(())
        } else {
            Err(Ended::WriteFailed)
        }
    }

    fn protocol_error(&mut self, error: ProtocolError) -> Ended {
        self.outbox
            .push_must(encode_reply(error.reply(), &self.config.limits));
        Ended::Protocol(error.code())
    }

    fn over_budget(&mut self) -> Ended {
        let reply = Reply::error(
            ErrorCode::Busy,
            "the helper holds as much request data as it allows; try again",
        );
        self.outbox
            .push_must(encode_reply(reply, &self.config.limits));
        Ended::OverBudget
    }

    fn charge(&mut self, want: usize) -> bool {
        self.budget.adjust(&mut self.charged, want)
    }

    fn uncharge(&mut self) {
        self.budget.adjust(&mut self.charged, 0);
    }
}

/// A reply's frame. Every reply the helper builds fits the protocol's
/// limits (`Reply::refused` and `Reply::error` cap themselves); should one
/// not, the peer learns of an internal error rather than nothing.
fn encode_reply(reply: Reply, limits: &Limits) -> Vec<u8> {
    encode_to_client(&ToClient::Reply(reply), limits).unwrap_or_else(|_| {
        let fallback = Reply::error(ErrorCode::Internal, "the reply was too large to send");
        encode_to_client(&ToClient::Reply(fallback), limits)
            .expect("a short error reply always encodes")
    })
}

#[cfg(test)]
mod tests;
