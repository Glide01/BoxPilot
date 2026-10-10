//! One connection's outgoing queue: frames already encoded, written in
//! order by the connection's writer thread.
//!
//! Two kinds of frame go out, and the queue treats them differently (ADR
//! 0006 rule 1):
//!
//! - **replies and `exited`** are never dropped. They can't pile up either:
//!   a connection reads its next request only once the last reply is
//!   written ([`Outbox::wait_written`]), and `exited` comes once per run;
//! - **log lines** are dropped, and counted, once the queue holds its limit
//!   of them. A GUI that stops reading must neither grow the root helper's
//!   memory nor block the thread that drains sing-box's pipe (which would
//!   block sing-box itself, with TUN up). The write deadline then closes
//!   such a connection.
//!
//! A reply's place can be reserved before it exists ([`Outbox::reserve`]):
//! whatever a request sets off while it is handled (sing-box's first lines,
//! or the `exited` of a run it stops) then goes out after its reply.

#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, MutexGuard};

/// How many log lines, and how many of their bytes, one connection's queue
/// holds before it drops new ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutboxLimits {
    pub max_log_lines: usize,
    pub max_log_bytes: usize,
}

impl Default for OutboxLimits {
    /// 1,024 lines or 2 MiB: seconds of a chatty sing-box at debug level,
    /// and well over what a GUI that keeps up ever has waiting. A frame of
    /// one log line is at most 384 KiB (protocol `Limits`), so the byte cap
    /// always holds a few of the longest.
    fn default() -> Self {
        Self {
            max_log_lines: 1024,
            max_log_bytes: 2 * 1024 * 1024,
        }
    }
}

/// What happened to a log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogPush {
    Queued,
    /// The queue held its limit; the line is counted in
    /// [`Outbox::dropped`].
    Dropped,
    /// The connection is going away.
    Closed,
}

/// A frame for the writer, and whether it is one that must go out.
#[derive(Debug)]
pub struct Outgoing {
    pub bytes: Vec<u8>,
    must: Option<u64>,
}

/// Which must-go-out frame to wait for ([`Outbox::wait_written`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ticket(u64);

pub struct Outbox {
    limits: OutboxLimits,
    state: Mutex<State>,
    changed: Condvar,
}

struct State {
    queue: VecDeque<Entry>,
    log_lines: usize,
    log_bytes: usize,
    dropped: u64,
    phase: Phase,
    /// The sequence number the next must-go-out frame gets.
    next_must: u64,
    /// Every must-go-out frame numbered below this one has been written.
    written_below: u64,
}

enum Entry {
    Must(u64, Vec<u8>),
    Log(Vec<u8>),
    /// A reply's reserved place, not filled yet: the writer waits here.
    Reserved(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Open,
    /// Nothing new is taken; the writer drains what is queued, then stops.
    Finishing,
    /// Everything queued is dropped; the writer stops now.
    Closed,
}

/// A reserved place for a reply ([`Outbox::reserve`]). Dropped unfilled, it
/// leaves the queue, so the writer never waits on it forever.
pub struct Reservation<'a> {
    outbox: &'a Outbox,
    id: u64,
    filled: bool,
}

impl Reservation<'_> {
    /// Put `bytes` in the reserved place. The ticket waits for them.
    pub fn fill(mut self, bytes: Vec<u8>) -> Ticket {
        self.filled = true;
        let mut state = self.outbox.lock();
        if let Some(entry) = state
            .queue
            .iter_mut()
            .find(|entry| matches!(entry, Entry::Reserved(id) if *id == self.id))
        {
            *entry = Entry::Must(self.id, bytes);
        }
        self.outbox.changed.notify_all();
        Ticket(self.id)
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        if self.filled {
            return;
        }
        let mut state = self.outbox.lock();
        state
            .queue
            .retain(|entry| !matches!(entry, Entry::Reserved(id) if *id == self.id));
        // Nothing was written for it, but nothing ever will be: waiters on
        // later frames must not wait for this one.
        if state.written_below == self.id {
            state.written_below += 1;
        }
        self.outbox.changed.notify_all();
    }
}

impl Outbox {
    pub fn new(limits: OutboxLimits) -> Self {
        Self {
            limits,
            state: Mutex::new(State {
                queue: VecDeque::new(),
                log_lines: 0,
                log_bytes: 0,
                dropped: 0,
                phase: Phase::Open,
                next_must: 0,
                written_below: 0,
            }),
            changed: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // A panic while holding the lock leaves plain data behind, never a
        // broken invariant worth refusing to send over; and the release
        // build aborts on panic anyway.
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Reserve the next place in the queue for a reply, or `None` once the
    /// connection is going away.
    pub fn reserve(&self) -> Option<Reservation<'_>> {
        let mut state = self.lock();
        if state.phase != Phase::Open {
            return None;
        }
        let id = state.next_must;
        state.next_must += 1;
        state.queue.push_back(Entry::Reserved(id));
        Some(Reservation {
            outbox: self,
            id,
            filled: false,
        })
    }

    /// Queue a frame that must go out: a reply, or `exited`. `None` once the
    /// connection is going away.
    pub fn push_must(&self, bytes: Vec<u8>) -> Option<Ticket> {
        let mut state = self.lock();
        if state.phase != Phase::Open {
            return None;
        }
        let id = state.next_must;
        state.next_must += 1;
        state.queue.push_back(Entry::Must(id, bytes));
        self.changed.notify_all();
        Some(Ticket(id))
    }

    /// Queue a log line's frame, unless the queue holds its limit of them.
    pub fn push_log(&self, bytes: Vec<u8>) -> LogPush {
        let mut state = self.lock();
        if state.phase != Phase::Open {
            return LogPush::Closed;
        }
        if state.log_lines >= self.limits.max_log_lines
            || state.log_bytes + bytes.len() > self.limits.max_log_bytes
        {
            state.dropped += 1;
            return LogPush::Dropped;
        }
        state.log_lines += 1;
        state.log_bytes += bytes.len();
        state.queue.push_back(Entry::Log(bytes));
        self.changed.notify_all();
        LogPush::Queued
    }

    /// The writer's next frame. Blocks while the queue is empty or starts at
    /// a reserved place; `None` once the outbox is closed, or finishing and
    /// drained.
    pub fn next(&self) -> Option<Outgoing> {
        let mut state = self.lock();
        loop {
            if state.phase == Phase::Closed {
                return None;
            }
            match state.queue.front() {
                Some(Entry::Reserved(_)) => {}
                Some(_) => break,
                None if state.phase == Phase::Finishing => return None,
                None => {}
            }
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        match state.queue.pop_front() {
            Some(Entry::Must(id, bytes)) => Some(Outgoing {
                bytes,
                must: Some(id),
            }),
            Some(Entry::Log(bytes)) => {
                state.log_lines -= 1;
                state.log_bytes -= bytes.len();
                Some(Outgoing { bytes, must: None })
            }
            Some(Entry::Reserved(_)) | None => unreachable!("the loop stops only at a frame"),
        }
    }

    /// The writer wrote `frame`.
    pub fn sent(&self, frame: &Outgoing) {
        if let Some(id) = frame.must {
            let mut state = self.lock();
            state.written_below = state.written_below.max(id + 1);
            self.changed.notify_all();
        }
    }

    /// Wait until the frame of `ticket` is written: `true`, or `false` if
    /// the outbox closed first.
    pub fn wait_written(&self, ticket: Ticket) -> bool {
        let mut state = self.lock();
        loop {
            if state.written_below > ticket.0 {
                return true;
            }
            if state.phase == Phase::Closed {
                return false;
            }
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    /// Take nothing more; the writer drains what is queued and stops. For
    /// the `error` reply a connection ends with.
    pub fn finish(&self) {
        let mut state = self.lock();
        if state.phase == Phase::Open {
            state.phase = Phase::Finishing;
        }
        self.changed.notify_all();
    }

    /// Drop everything queued and take nothing more: the connection is gone.
    pub fn close(&self) {
        let mut state = self.lock();
        state.phase = Phase::Closed;
        state.queue.clear();
        state.log_lines = 0;
        state.log_bytes = 0;
        self.changed.notify_all();
    }

    /// How many log lines were dropped because the queue was full.
    pub fn dropped(&self) -> u64 {
        self.lock().dropped
    }

    /// How many log lines wait in the queue.
    pub fn queued_logs(&self) -> usize {
        self.lock().log_lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    fn small() -> Outbox {
        Outbox::new(OutboxLimits {
            max_log_lines: 2,
            max_log_bytes: 10,
        })
    }

    fn drain(outbox: &Outbox) -> Vec<Vec<u8>> {
        outbox.finish();
        let mut out = Vec::new();
        while let Some(frame) = outbox.next() {
            outbox.sent(&frame);
            out.push(frame.bytes);
        }
        out
    }

    #[test]
    fn logs_are_dropped_and_counted_but_replies_never_are() {
        let outbox = small();
        assert_eq!(outbox.push_log(b"a".to_vec()), LogPush::Queued);
        assert_eq!(outbox.push_log(b"b".to_vec()), LogPush::Queued);
        assert_eq!(outbox.push_log(b"c".to_vec()), LogPush::Dropped);
        for n in 0..100u8 {
            assert!(outbox.push_must(vec![n]).is_some());
        }
        assert_eq!(outbox.dropped(), 1);
        assert_eq!(outbox.queued_logs(), 2);
        let sent = drain(&outbox);
        assert_eq!(sent.len(), 102);
        assert_eq!(sent[..2], [b"a".to_vec(), b"b".to_vec()]);
        assert_eq!(
            sent[2..],
            (0..100u8).map(|n| vec![n]).collect::<Vec<_>>()[..]
        );
    }

    #[test]
    fn the_byte_cap_counts_too_and_frees_as_lines_go_out() {
        let outbox = small();
        assert_eq!(outbox.push_log(vec![0; 8]), LogPush::Queued);
        assert_eq!(outbox.push_log(vec![0; 3]), LogPush::Dropped);
        assert_eq!(outbox.push_log(vec![0; 2]), LogPush::Queued);
        assert_eq!(outbox.push_log(vec![0; 1]), LogPush::Dropped);
        let first = outbox.next().unwrap();
        assert_eq!(first.bytes.len(), 8);
        assert_eq!(outbox.push_log(vec![0; 8]), LogPush::Queued);
        assert_eq!(outbox.dropped(), 2);
    }

    #[test]
    fn a_reserved_reply_goes_out_before_what_came_after_it() {
        let outbox = Outbox::new(OutboxLimits::default());
        assert_eq!(outbox.push_log(b"before".to_vec()), LogPush::Queued);
        let reservation = outbox.reserve().unwrap();
        assert_eq!(outbox.push_log(b"after".to_vec()), LogPush::Queued);
        let exited = outbox.push_must(b"exited".to_vec()).unwrap();
        assert_eq!(outbox.next().unwrap().bytes, b"before");
        let ticket = reservation.fill(b"reply".to_vec());
        let sent = drain(&outbox);
        assert_eq!(
            sent,
            [b"reply".to_vec(), b"after".to_vec(), b"exited".to_vec()]
        );
        assert!(outbox.wait_written(ticket));
        assert!(outbox.wait_written(exited));
    }

    #[test]
    fn the_writer_waits_at_a_reserved_place() {
        let outbox = Arc::new(small());
        let reservation_outbox = outbox.clone();
        let reservation = reservation_outbox.reserve().unwrap();
        outbox.push_must(b"later".to_vec()).unwrap();
        let writer = {
            let outbox = outbox.clone();
            thread::spawn(move || outbox.next().map(|frame| frame.bytes))
        };
        thread::sleep(Duration::from_millis(50));
        assert!(!writer.is_finished(), "the writer waits for the reply");
        reservation.fill(b"reply".to_vec());
        assert_eq!(writer.join().unwrap(), Some(b"reply".to_vec()));
    }

    #[test]
    fn an_unfilled_reservation_leaves_the_queue() {
        let outbox = small();
        drop(outbox.reserve().unwrap());
        let ticket = outbox.push_must(b"x".to_vec()).unwrap();
        assert_eq!(drain(&outbox), [b"x".to_vec()]);
        assert!(outbox.wait_written(ticket));
    }

    #[test]
    fn waiting_for_a_write_ends_when_the_outbox_closes() {
        let outbox = Arc::new(small());
        let ticket = outbox.push_must(b"never read".to_vec()).unwrap();
        let waiter = {
            let outbox = outbox.clone();
            thread::spawn(move || outbox.wait_written(ticket))
        };
        thread::sleep(Duration::from_millis(20));
        outbox.close();
        assert!(!waiter.join().unwrap());
        assert!(outbox.next().is_none());
    }

    #[test]
    fn nothing_is_taken_once_finishing_or_closed() {
        let outbox = small();
        outbox.push_must(b"error".to_vec()).unwrap();
        outbox.finish();
        assert!(outbox.push_must(b"exited".to_vec()).is_none());
        assert_eq!(outbox.push_log(b"log".to_vec()), LogPush::Closed);
        assert!(outbox.reserve().is_none());
        assert_eq!(outbox.next().unwrap().bytes, b"error");
        assert!(outbox.next().is_none());

        let outbox = small();
        outbox.push_must(b"dropped".to_vec()).unwrap();
        outbox.push_log(b"dropped".to_vec());
        outbox.close();
        assert!(outbox.next().is_none());
        assert_eq!(outbox.queued_logs(), 0);
        assert_eq!(outbox.push_log(b"x".to_vec()), LogPush::Closed);
    }
}
