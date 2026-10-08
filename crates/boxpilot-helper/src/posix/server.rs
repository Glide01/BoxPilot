//! The accept loop on macOS: serve connections on the listening socket
//! launchd handed over until told to stop, or idle for
//! [`ServerConfig::idle_exit`] (ADR 0006 rules 4 to 6); `win::server`'s
//! counterpart.
//!
//! - At most [`ServerConfig::max_connections`] are served at once. A client
//!   that connects while the helper is full waits in the socket's backlog,
//!   unanswered, until a connection ends, as on Windows.
//! - At most [`ServerConfig::max_read_only`] of them are callers that may
//!   not start (`conn::ReadOnlySlots`): the socket is 0666, so any account
//!   can connect, and the rest stay free for callers that may.
//! - Every connection's requests share one memory `Budget`.
//! - Each connection's caller comes from the platform's `identify`, from
//!   what the kernel says about the peer (`peer`), on the connection's own
//!   thread, before its first byte is read.
//! - Idle means no connection and no sing-box: then the helper exits, and
//!   launchd starts it again for the next client (socket activation).

use super::transport::UnixTransport;
use crate::conn::{self, Budget, ConnConfig, ReadOnlySlots};
use crate::helper::{Caller, ConnId, Helper};
use crate::helper_log;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// How the accept loop runs.
#[derive(Debug, Clone, Copy)]
pub struct ServerConfig {
    /// Connections served at once.
    pub max_connections: usize,
    /// Of those, connections from callers that may not start.
    pub max_read_only: usize,
    /// With no connection and no sing-box for this long, stop.
    pub idle_exit: Duration,
    /// How often the loop looks up from waiting for a client.
    pub tick: Duration,
    /// How long stopping waits for connections to finish.
    pub shutdown_grace: Duration,
    pub conn: ConnConfig,
    /// The memory every connection's requests may hold at once.
    pub budget: usize,
}

impl Default for ServerConfig {
    /// Windows' numbers (`win::server`).
    fn default() -> Self {
        Self {
            max_connections: 8,
            max_read_only: 4,
            idle_exit: Duration::from_secs(60),
            tick: Duration::from_secs(1),
            shutdown_grace: Duration::from_secs(20),
            conn: ConnConfig::default(),
            budget: Budget::DEFAULT_LIMIT,
        }
    }
}

/// Why the loop ended.
#[derive(Debug)]
pub enum Stopped {
    /// Idle for `idle_exit`.
    Idle,
    /// The stop flag was set.
    Asked,
    /// Accepting kept failing.
    Failed(io::Error),
}

/// Says who a peer is: its caller, from what the kernel says about it.
pub type Identify = Arc<dyn Fn(&UnixStream) -> Caller + Send + Sync>;

/// Set whenever a connection ends, to wake a loop waiting for room.
#[derive(Default)]
struct Ended {
    count: Mutex<u64>,
    changed: Condvar,
}

impl Ended {
    fn notify(&self) {
        *self
            .count
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) += 1;
        self.changed.notify_all();
    }

    /// Wait until a connection ends or `timeout` passes.
    fn wait(&self, timeout: Duration) {
        let count = self
            .count
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let seen = *count;
        let _ = self
            .changed
            .wait_timeout_while(count, timeout, |count| *count == seen);
    }
}

/// The connections being served.
struct Connections<H: ?Sized> {
    core: Arc<H>,
    identify: Identify,
    budget: Arc<Budget>,
    read_only: Arc<ReadOnlySlots>,
    config: ServerConfig,
    open: Arc<Mutex<HashMap<ConnId, Arc<UnixTransport>>>>,
    live: Arc<AtomicUsize>,
    ended: Arc<Ended>,
    threads: Vec<JoinHandle<()>>,
    next: ConnId,
}

impl<H: Helper + ?Sized + 'static> Connections<H> {
    fn live(&self) -> usize {
        self.live.load(Ordering::SeqCst)
    }

    /// Serve an accepted connection on its own thread.
    fn serve(&mut self, stream: UnixStream) {
        // On macOS an accepted socket inherits the listener's O_NONBLOCK.
        if let Err(error) = stream.set_nonblocking(false) {
            helper_log!("a connection could not be set up: {error}");
            return;
        }
        let transport = Arc::new(UnixTransport::new(stream));
        let id = self.next;
        self.next += 1;
        self.open
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id, transport.clone());
        self.live.fetch_add(1, Ordering::SeqCst);
        let (core, identify, budget) = (
            self.core.clone(),
            self.identify.clone(),
            self.budget.clone(),
        );
        let (read_only, conn_config) = (self.read_only.clone(), self.config.conn);
        let (open, live, ended) = (self.open.clone(), self.live.clone(), self.ended.clone());
        let max_read_only = self.config.max_read_only;
        let thread = thread::Builder::new()
            .name(format!("connection {id}"))
            .spawn(move || {
                let caller = identify(transport.stream());
                match read_only.admit(caller.authority) {
                    Some(_admitted) => {
                        conn::serve(&*transport, id, &caller, &*core, &conn_config, &budget);
                    }
                    None => helper_log!(
                        "connection {id}: closed, {max_read_only} read-only connections are \
                         served already"
                    ),
                }
                open.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&id);
                // The last reference: the socket closes here.
                drop(transport);
                live.fetch_sub(1, Ordering::SeqCst);
                ended.notify();
            });
        match thread {
            Ok(thread) => self.threads.push(thread),
            Err(error) => {
                helper_log!("connection {id}: no thread: {error}");
                let transport = self
                    .open
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&id);
                drop(transport);
                self.live.fetch_sub(1, Ordering::SeqCst);
            }
        }
        self.threads.retain(|thread| !thread.is_finished());
    }

    /// Close every connection and wait, a while, for their threads.
    fn close_all(self) {
        for transport in self
            .open
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
        {
            use crate::transport::Transport;
            transport.close();
        }
        let deadline = Instant::now() + self.config.shutdown_grace;
        for thread in self.threads {
            while !thread.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(20));
            }
            if thread.is_finished() {
                let _ = thread.join();
            }
        }
    }
}

/// Wait until `listener` has a client to accept, or `timeout` passes.
fn readable(listener: &UnixListener, timeout: Duration) -> io::Result<bool> {
    let mut poll = libc::pollfd {
        fd: listener.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
    // SAFETY: one valid `pollfd`, for a descriptor open while `listener`
    // is borrowed.
    match unsafe { libc::poll(&mut poll, 1, millis) } {
        -1 => {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                Ok(false)
            } else {
                Err(error)
            }
        }
        0 => Ok(false),
        _ => Ok(true),
    }
}

/// Serve `listener` until `stop` is set, or the helper has been idle for
/// `config.idle_exit`; then stop sing-box, close every connection, and say
/// why it ended.
pub fn run<H: Helper + ?Sized + 'static>(
    listener: &UnixListener,
    core: Arc<H>,
    identify: Identify,
    stop: &AtomicBool,
    config: ServerConfig,
) -> Stopped {
    if let Err(error) = listener.set_nonblocking(true) {
        return Stopped::Failed(error);
    }
    let mut connections = Connections {
        core: core.clone(),
        identify,
        budget: Arc::new(Budget::new(config.budget)),
        read_only: Arc::new(ReadOnlySlots::new(config.max_read_only)),
        config,
        open: Arc::new(Mutex::new(HashMap::new())),
        live: Arc::new(AtomicUsize::new(0)),
        ended: Arc::new(Ended::default()),
        threads: Vec::new(),
        next: 1,
    };
    let mut idle_since = Instant::now();
    let mut failures = 0u32;
    let stopped = loop {
        if stop.load(Ordering::SeqCst) {
            break Stopped::Asked;
        }
        let live = connections.live();
        if live > 0 || !core.is_idle() {
            idle_since = Instant::now();
        } else if idle_since.elapsed() >= config.idle_exit {
            helper_log!("idle for {}s: stopping", config.idle_exit.as_secs());
            break Stopped::Idle;
        }
        if live >= config.max_connections {
            // Full: a new client waits in the backlog until one ends.
            connections.ended.wait(config.tick);
            continue;
        }
        match readable(listener, config.tick) {
            Ok(false) => continue,
            Ok(true) => {}
            Err(error) => {
                helper_log!("waiting for a client failed: {error}");
                failures += 1;
                if failures > 10 {
                    break Stopped::Failed(error);
                }
                thread::sleep(config.tick);
                continue;
            }
        }
        match listener.accept() {
            Ok((stream, _)) => {
                failures = 0;
                connections.serve(stream);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => {
                // ECONNABORTED and the like: that client is gone.
                helper_log!("accepting a client failed: {error}");
                failures += 1;
                if failures > 10 {
                    break Stopped::Failed(error);
                }
            }
        }
    };
    core.shutdown();
    connections.close_all();
    stopped
}

/// The most clients [`turn_away_waiting`] closes: the backlog launchd's
/// socket holds, so a client connecting in a loop can't keep a helper that
/// refuses to run from exiting.
pub const MAX_TURNED_AWAY: usize = 128;

/// The path `listener` is bound to, for the log, up to its first NUL.
/// launchd's socket reports its whole `sun_path`, the NULs after the path
/// included, and `std` keeps them (`Some("/var/run/….sock\0\0\0…")`); a
/// path never holds a NUL, so nothing after one is part of it.
pub fn listening_path(listener: &UnixListener) -> Option<PathBuf> {
    let address = listener.local_addr().ok()?;
    address.as_pathname().map(before_nul)
}

/// `path` up to its first NUL.
fn before_nul(path: &Path) -> PathBuf {
    let bytes = path.as_os_str().as_bytes();
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    PathBuf::from(OsStr::from_bytes(&bytes[..end]))
}

/// Accept and close the clients already waiting, unanswered: a helper that
/// refuses to run does this before it exits, so they see the end of the
/// stream at once, instead of launchd starting it again for them.
pub fn turn_away_waiting(listener: &UnixListener) -> usize {
    if listener.set_nonblocking(true).is_err() {
        return 0;
    }
    let mut count = 0;
    while count < MAX_TURNED_AWAY {
        let Ok((stream, _)) = listener.accept() else {
            break;
        };
        drop(stream);
        count += 1;
    }
    count
}

#[cfg(test)]
mod tests;
