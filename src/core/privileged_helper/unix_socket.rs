//! The macOS helper's socket as a [`HelperIo`]: a Unix stream to the
//! socket launchd created for the daemon (`endpoint::macos::SOCKET_PATH`).
//! Safe code over `std`'s `UnixStream`, as the helper's own
//! `posix::transport` is; built on Linux too, for its tests.
//!
//! - A read blocks with no timeout: the reading thread waits for sing-box's
//!   next line as long as TUN is up. The receive timeout is never set.
//! - A write first sets the send timeout (`SO_SNDTIMEO`) to what is left of
//!   its deadline, so a helper that stops reading fails it at the deadline
//!   (`TimedOut`). The two timeouts are independent, so the reading thread
//!   and a writer never disturb each other.
//! - `close` shuts the socket down both ways, which wakes a read or write
//!   blocked on it in another thread, and refuses later ones.
//! - macOS fails `setsockopt` with `EINVAL` once the peer has closed its
//!   end (Linux doesn't). That is no error here: the write that follows
//!   reports the end itself (`EPIPE`), and can't block on a peer that is
//!   gone. The helper's `posix::transport` does the same.

#![forbid(unsafe_code)]

use super::client::HelperIo;
use super::{socket_connect_error, OpenError};
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

/// A connected stream to the helper.
#[derive(Debug)]
pub struct SocketIo {
    stream: UnixStream,
    closed: AtomicBool,
}

/// Connect to the helper's socket at `path`. `installed` says whether the
/// helper is installed, asked only when there is nobody at `path`, to tell
/// "not installed" from "installed but not running".
pub fn connect(path: &Path, installed: impl FnOnce() -> bool) -> Result<SocketIo, OpenError> {
    match UnixStream::connect(path) {
        Ok(stream) => Ok(SocketIo::new(stream)),
        Err(error) => Err(socket_connect_error(&error, installed)),
    }
}

impl SocketIo {
    fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            closed: AtomicBool::new(false),
        }
    }

    fn closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}

/// A socket timeout set, or the peer already gone (`EINVAL`, on macOS).
fn gone_peer_ok(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(error) if error.raw_os_error() == Some(libc::EINVAL) => Ok(()),
        other => other,
    }
}

impl HelperIo for SocketIo {
    fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.closed() {
                return Err(io::ErrorKind::ConnectionAborted.into());
            }
            match (&self.stream).read(buf) {
                // `close` from another thread reads as the end of the
                // stream; it is this side's end, not the helper's.
                Ok(0) if self.closed() => return Err(io::ErrorKind::ConnectionAborted.into()),
                Ok(n) => return Ok(n),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                // The helper closed its end with data unread: the end of
                // the stream all the same.
                Err(error) if error.kind() == io::ErrorKind::ConnectionReset => return Ok(0),
                Err(error) => return Err(error),
            }
        }
    }

    fn write_all(&self, mut buf: &[u8], deadline: Instant) -> io::Result<()> {
        while !buf.is_empty() {
            if self.closed() {
                return Err(io::ErrorKind::ConnectionAborted.into());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::ErrorKind::TimedOut.into());
            }
            gone_peer_ok(self.stream.set_write_timeout(Some(left)))?;
            match (&self.stream).write(buf) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => buf = &buf[n..],
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                // The send timeout (`EAGAIN`): go round, and the deadline
                // decides.
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// Ends every read and write under way and refuses later ones. The
    /// socket itself closes when the last owner drops this, which the
    /// helper sees as the end of the stream.
    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        // The peer may be gone already (`ENOTCONN`): closed either way.
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

#[cfg(test)]
mod tests {
    use super::super::client::HelperConnection;
    use super::*;
    use boxpilot_protocol::{
        encode_to_client, Authority, FrameDecoder, HelloReply, Limits, Reply, ServerSession,
        ToClient, PROTOCOL_VERSION,
    };
    use std::os::unix::net::UnixListener;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    fn pair() -> (SocketIo, UnixStream) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        (SocketIo::new(ours), theirs)
    }

    /// A directory of its own under the system temp dir, removed on drop.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let mut random = [0u8; 8];
            getrandom::fill(&mut random).unwrap();
            let name: String = random.iter().map(|b| format!("{b:02x}")).collect();
            let dir = std::env::temp_dir().join(format!("boxpilot-{tag}-{name}"));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn bytes_go_both_ways_until_the_helper_closes() {
        let (io, mut peer) = pair();
        peer.write_all(b"hello").unwrap();
        let mut buf = [0u8; 16];
        let n = io.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello");
        io.write_all(b"world", Instant::now() + Duration::from_secs(5))
            .unwrap();
        let mut back = [0u8; 5];
        peer.read_exact(&mut back).unwrap();
        assert_eq!(&back, b"world");
        drop(peer);
        assert_eq!(io.read(&mut buf).unwrap(), 0);
    }

    /// A helper that stops reading: the write fails at its deadline instead
    /// of blocking for ever.
    #[test]
    fn a_write_fails_at_its_deadline() {
        let (io, _peer) = pair();
        let flood = vec![0u8; 8 * 1024 * 1024];
        let began = Instant::now();
        let error = io
            .write_all(&flood, began + Duration::from_millis(300))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(began.elapsed() >= Duration::from_millis(250));
        assert!(began.elapsed() < Duration::from_secs(5));
        // A deadline already past fails at once.
        let error = io.write_all(b"x", Instant::now()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    /// What `HelperIo::close` promises: a read blocked in another thread
    /// returns at once, and later reads and writes fail.
    #[test]
    fn close_ends_a_blocked_read_in_another_thread() {
        let (io, _peer) = pair();
        thread::scope(|scope| {
            let reader = scope.spawn(|| io.read(&mut [0u8; 4]));
            thread::sleep(Duration::from_millis(100));
            let began = Instant::now();
            io.close();
            let result = reader.join().unwrap();
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::ConnectionAborted);
            assert!(began.elapsed() < Duration::from_secs(2));
        });
        assert!(io.read(&mut [0u8; 4]).is_err());
        assert!(io
            .write_all(b"x", Instant::now() + Duration::from_secs(1))
            .is_err());
    }

    #[test]
    fn close_ends_a_blocked_write_in_another_thread() {
        let (io, _peer) = pair();
        let flood = vec![0u8; 8 * 1024 * 1024];
        thread::scope(|scope| {
            let writer =
                scope.spawn(|| io.write_all(&flood, Instant::now() + Duration::from_secs(30)));
            thread::sleep(Duration::from_millis(200));
            let began = Instant::now();
            io.close();
            assert!(writer.join().unwrap().is_err());
            assert!(began.elapsed() < Duration::from_secs(5));
        });
    }

    /// A write after the helper closed its end fails as a write does
    /// (`EPIPE`), never with the `EINVAL` macOS gives `setsockopt` then, and
    /// closing afterwards is quiet.
    #[test]
    fn a_write_after_the_helper_is_gone_fails_cleanly() {
        let (io, peer) = pair();
        drop(peer);
        assert_eq!(io.read(&mut [0u8; 4]).unwrap(), 0);
        let error = io
            .write_all(b"stop", Instant::now() + Duration::from_secs(5))
            .unwrap_err();
        assert_ne!(error.raw_os_error(), Some(libc::EINVAL), "{error}");
        io.close();
        io.close();
    }

    #[test]
    fn gone_peer_einval_is_not_an_error() {
        assert!(gone_peer_ok(Err(io::Error::from_raw_os_error(libc::EINVAL))).is_ok());
        assert!(gone_peer_ok(Ok(())).is_ok());
        let error = gone_peer_ok(Err(io::Error::from_raw_os_error(libc::EBADF))).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EBADF));
    }

    /// Nobody at the path: not installed, or installed but not running,
    /// as the plist says; a socket file nobody listens on reads the same.
    #[test]
    fn a_missing_or_dead_socket_is_not_installed_or_not_running() {
        let temp = TempDir::new("socket");
        let missing = temp.0.join("missing.sock");
        assert_eq!(
            connect(&missing, || false).unwrap_err(),
            OpenError::NotInstalled
        );
        assert_eq!(connect(&missing, || true).unwrap_err(), OpenError::Disabled);

        let stale = temp.0.join("stale.sock");
        drop(UnixListener::bind(&stale).unwrap());
        assert!(stale.exists(), "a dropped listener leaves its file");
        assert_eq!(
            connect(&stale, || false).unwrap_err(),
            OpenError::NotInstalled
        );
        assert_eq!(connect(&stale, || true).unwrap_err(), OpenError::Disabled);
    }

    /// The client over this stream, end to end: `hello` is answered; then
    /// the helper goes away, and stopping (a write to a gone peer) returns
    /// at once instead of failing or waiting out its deadline.
    #[test]
    fn the_client_says_hello_and_stops_after_the_helper_is_gone() {
        let temp = TempDir::new("socket");
        let path = temp.0.join("helper.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let limits = Limits::default();
            let mut session = ServerSession::new(Authority::MayStart, limits);
            let mut decoder = FrameDecoder::new(session.frame_caps());
            let mut buf = vec![0u8; 4096];
            loop {
                let n = stream.read(&mut buf).unwrap();
                // One `hello`: nothing follows a whole frame.
                assert_eq!(decoder.feed(&buf[..n]), n);
                if let Some(frame) = decoder.next_frame().unwrap() {
                    assert!(session.accept(frame).unwrap().is_some());
                    session.replied();
                    let hello = ToClient::Reply(Reply::Hello(HelloReply {
                        protocol_version: PROTOCOL_VERSION,
                        helper_version: "0.1.0".into(),
                        sing_box_version: "1.14.2".into(),
                        sing_box_sha256: "00".repeat(32),
                        may_start: true,
                    }));
                    stream
                        .write_all(&encode_to_client(&hello, &limits).unwrap())
                        .unwrap();
                    // The helper goes away.
                    return;
                }
            }
        });
        let io = connect(&path, || true).unwrap();
        let (connection, _events) = HelperConnection::new(Arc::new(io));
        let hello = connection.hello().unwrap();
        assert!(hello.may_start);
        server.join().unwrap();
        let began = Instant::now();
        connection.stop(Duration::from_secs(10));
        assert!(
            began.elapsed() < Duration::from_secs(5),
            "stopping waited {:?} on a helper that is gone",
            began.elapsed()
        );
    }
}
