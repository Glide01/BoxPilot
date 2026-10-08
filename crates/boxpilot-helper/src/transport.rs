//! The byte stream one connection runs over: a named pipe instance on
//! Windows, an in-memory pair in the tests.

#![forbid(unsafe_code)]

use std::io;
use std::time::Instant;

/// One connected byte stream, read by one thread and written by another
/// (the connection's reader and writer), and closable from any.
///
/// Deadlines are how the helper keeps a peer from holding it: a read waits
/// for the next frame for as long as the peer likes between requests, but
/// not once a frame has begun, and no write waits for a peer that stopped
/// reading for longer than the write deadline.
pub trait Transport: Send + Sync {
    /// Read at least one byte into `buf`, waiting no later than `deadline`
    /// (`None`: as long as it takes). `Ok(0)` is the end of the stream;
    /// [`io::ErrorKind::TimedOut`] is the deadline. Called from one thread
    /// at a time.
    fn read(&self, buf: &mut [u8], deadline: Option<Instant>) -> io::Result<usize>;

    /// Write all of `buf` by `deadline`, or fail with
    /// [`io::ErrorKind::TimedOut`]. Called from one thread at a time.
    fn write_all(&self, buf: &[u8], deadline: Instant) -> io::Result<()>;

    /// End the stream: a read or write under way, or any later one, fails
    /// at once. Idempotent; callable from any thread.
    fn close(&self);
}

/// An in-memory duplex stream for the tests: two [`MemoryEnd`]s, each
/// direction a byte queue of bounded capacity, so a writer blocks when its
/// peer stops reading, as on a pipe whose buffer is full.
#[cfg(test)]
pub(crate) mod memory {
    use super::Transport;
    use std::collections::VecDeque;
    use std::io;
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Instant;

    struct Queue {
        bytes: VecDeque<u8>,
        /// The writing end will write no more.
        write_closed: bool,
        /// The reading end will read no more.
        read_closed: bool,
    }

    struct Channel {
        capacity: usize,
        queue: Mutex<Queue>,
        changed: Condvar,
    }

    impl Channel {
        fn new(capacity: usize) -> Arc<Self> {
            Arc::new(Self {
                capacity,
                queue: Mutex::new(Queue {
                    bytes: VecDeque::new(),
                    write_closed: false,
                    read_closed: false,
                }),
                changed: Condvar::new(),
            })
        }

        /// Wait on the condvar until `deadline`, or forever without one.
        /// `false` when the deadline has passed.
        fn wait<'a>(
            &self,
            guard: std::sync::MutexGuard<'a, Queue>,
            deadline: Option<Instant>,
        ) -> Option<std::sync::MutexGuard<'a, Queue>> {
            match deadline {
                None => Some(self.changed.wait(guard).unwrap()),
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return None;
                    }
                    Some(self.changed.wait_timeout(guard, deadline - now).unwrap().0)
                }
            }
        }
    }

    /// One end of the pair.
    pub struct MemoryEnd {
        incoming: Arc<Channel>,
        outgoing: Arc<Channel>,
    }

    /// Two connected ends, each direction holding at most `capacity`
    /// unread bytes.
    pub fn pair(capacity: usize) -> (MemoryEnd, MemoryEnd) {
        let a_to_b = Channel::new(capacity);
        let b_to_a = Channel::new(capacity);
        (
            MemoryEnd {
                incoming: b_to_a.clone(),
                outgoing: a_to_b.clone(),
            },
            MemoryEnd {
                incoming: a_to_b,
                outgoing: b_to_a,
            },
        )
    }

    impl MemoryEnd {
        /// Close only the writing direction, as a client does that sent its
        /// last bytes and hung up.
        pub fn shutdown_write(&self) {
            let mut queue = self.outgoing.queue.lock().unwrap();
            queue.write_closed = true;
            self.outgoing.changed.notify_all();
        }
    }

    impl Transport for MemoryEnd {
        fn read(&self, buf: &mut [u8], deadline: Option<Instant>) -> io::Result<usize> {
            let channel = &self.incoming;
            let mut queue = channel.queue.lock().unwrap();
            loop {
                if queue.read_closed {
                    return Err(io::ErrorKind::ConnectionAborted.into());
                }
                if !queue.bytes.is_empty() {
                    let n = buf.len().min(queue.bytes.len());
                    for (slot, byte) in buf.iter_mut().zip(queue.bytes.drain(..n)) {
                        *slot = byte;
                    }
                    channel.changed.notify_all();
                    return Ok(n);
                }
                if queue.write_closed {
                    return Ok(0);
                }
                queue = channel
                    .wait(queue, deadline)
                    .ok_or(io::ErrorKind::TimedOut)?;
            }
        }

        fn write_all(&self, mut buf: &[u8], deadline: Instant) -> io::Result<()> {
            let channel = &self.outgoing;
            let mut queue = channel.queue.lock().unwrap();
            while !buf.is_empty() {
                if queue.write_closed || queue.read_closed {
                    return Err(io::ErrorKind::BrokenPipe.into());
                }
                let room = channel.capacity - queue.bytes.len();
                if room > 0 {
                    let n = room.min(buf.len());
                    queue.bytes.extend(&buf[..n]);
                    buf = &buf[n..];
                    channel.changed.notify_all();
                    continue;
                }
                queue = channel
                    .wait(queue, Some(deadline))
                    .ok_or(io::ErrorKind::TimedOut)?;
            }
            Ok(())
        }

        fn close(&self) {
            for (channel, reading) in [(&self.incoming, true), (&self.outgoing, false)] {
                let mut queue = channel.queue.lock().unwrap();
                if reading {
                    queue.read_closed = true;
                } else {
                    queue.write_closed = true;
                }
                channel.changed.notify_all();
            }
        }
    }
}
