//! One connection's byte stream on macOS: a Unix socket accepted from the
//! launchd-created listener, with the deadlines `conn` holds peers to.
//!
//! Safe code over `std`'s `UnixStream`: each read or write sets the
//! socket's receive or send timeout to what is left of its deadline first
//! (`SO_RCVTIMEO` / `SO_SNDTIMEO`, which are independent, so the reader and
//! the writer thread don't disturb each other). `close` shuts the socket
//! down both ways, which wakes a read or write blocked on it in another
//! thread.

#![forbid(unsafe_code)]

use crate::transport::Transport;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

/// A connected Unix socket as a [`Transport`].
#[derive(Debug)]
pub struct UnixTransport {
    stream: UnixStream,
    closed: AtomicBool,
}

impl UnixTransport {
    /// Take a connected, blocking stream.
    pub fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            closed: AtomicBool::new(false),
        }
    }

    pub fn stream(&self) -> &UnixStream {
        &self.stream
    }

    fn closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}

/// What is left until `deadline`, or the deadline error once it has passed.
fn remaining(deadline: Instant) -> io::Result<std::time::Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(io::ErrorKind::TimedOut.into());
    }
    Ok(left)
}

/// Set a socket timeout. macOS fails `setsockopt` with `EINVAL` once the
/// peer has closed its end (Linux doesn't); the read or write that follows
/// reports that itself (the end of the stream, or `EPIPE`) and can't block
/// on a gone peer, so that error isn't one here.
fn set_timeout(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(error) if error.raw_os_error() == Some(libc::EINVAL) => Ok(()),
        other => other,
    }
}

/// A socket timeout's error: `EAGAIN` on macOS and Linux.
fn timed_out(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

impl Transport for UnixTransport {
    fn read(&self, buf: &mut [u8], deadline: Option<Instant>) -> io::Result<usize> {
        loop {
            if self.closed() {
                return Err(io::ErrorKind::ConnectionAborted.into());
            }
            let timeout = deadline.map(remaining).transpose()?;
            set_timeout(self.stream.set_read_timeout(timeout))?;
            match (&self.stream).read(buf) {
                // `close` from another thread reads as the end of the
                // stream here; it is the end of this connection either way.
                Ok(0) if self.closed() => return Err(io::ErrorKind::ConnectionAborted.into()),
                Ok(n) => return Ok(n),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                // The timeout: go round, and the deadline decides.
                Err(error) if timed_out(&error) && deadline.is_some() => {}
                Err(error) => return Err(error),
            }
        }
    }

    fn write_all(&self, mut buf: &[u8], deadline: Instant) -> io::Result<()> {
        while !buf.is_empty() {
            if self.closed() {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            set_timeout(self.stream.set_write_timeout(Some(remaining(deadline)?)))?;
            match (&self.stream).write(buf) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => buf = &buf[n..],
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if timed_out(&error) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    fn pair() -> (UnixTransport, UnixStream) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        (UnixTransport::new(ours), theirs)
    }

    #[test]
    fn bytes_go_both_ways() {
        let (transport, mut peer) = pair();
        peer.write_all(b"hello").unwrap();
        let mut buf = [0u8; 16];
        let n = transport
            .read(&mut buf, Some(Instant::now() + Duration::from_secs(5)))
            .unwrap();
        assert_eq!(&buf[..n], b"hello");
        transport
            .write_all(b"world", Instant::now() + Duration::from_secs(5))
            .unwrap();
        let mut back = [0u8; 5];
        peer.read_exact(&mut back).unwrap();
        assert_eq!(&back, b"world");
        drop(peer);
        assert_eq!(transport.read(&mut buf, None).unwrap(), 0);
    }

    #[test]
    fn a_read_waits_no_longer_than_its_deadline() {
        let (transport, _peer) = pair();
        let began = Instant::now();
        let error = transport
            .read(&mut [0u8; 4], Some(began + Duration::from_millis(200)))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(began.elapsed() >= Duration::from_millis(190));
        assert!(began.elapsed() < Duration::from_secs(3));
        // A deadline already past fails at once.
        let error = transport
            .read(&mut [0u8; 4], Some(Instant::now()))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    /// A peer that stops reading: the write fails at its deadline instead
    /// of blocking for ever.
    #[test]
    fn a_write_waits_no_longer_than_its_deadline() {
        let (transport, _peer) = pair();
        let flood = vec![0u8; 8 * 1024 * 1024];
        let began = Instant::now();
        let error = transport
            .write_all(&flood, began + Duration::from_millis(300))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(began.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn close_ends_a_blocked_read_in_another_thread() {
        let (transport, _peer) = pair();
        thread::scope(|scope| {
            let reader = scope.spawn(|| transport.read(&mut [0u8; 4], None));
            thread::sleep(Duration::from_millis(100));
            transport.close();
            let result = reader.join().unwrap();
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::ConnectionAborted);
        });
        assert_eq!(
            transport.read(&mut [0u8; 4], None).unwrap_err().kind(),
            io::ErrorKind::ConnectionAborted
        );
        assert_eq!(
            transport
                .write_all(b"x", Instant::now() + Duration::from_secs(1))
                .unwrap_err()
                .kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn close_ends_a_blocked_write_in_another_thread() {
        let (transport, _peer) = pair();
        let flood = vec![0u8; 8 * 1024 * 1024];
        thread::scope(|scope| {
            let writer = scope
                .spawn(|| transport.write_all(&flood, Instant::now() + Duration::from_secs(30)));
            thread::sleep(Duration::from_millis(200));
            let began = Instant::now();
            transport.close();
            assert!(writer.join().unwrap().is_err());
            assert!(began.elapsed() < Duration::from_secs(5));
        });
    }
}
