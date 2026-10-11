//! The accept loop, shared by the service and the console seam: verify the
//! trees, take the pipe name, then serve connections until told to stop or
//! idle for [`IDLE_EXIT`].
//!
//! - At most [`MAX_CONNECTIONS`] are served at once. One more instance is
//!   always listening, so the pipe name is never free for someone else to
//!   take; a client that connects to it while the helper is full waits
//!   there until a connection ends.
//! - At most [`MAX_READ_ONLY_CONNECTIONS`] of them are callers that may not
//!   start (`conn::ReadOnlySlots`): any interactive user can connect, so
//!   the rest stay free for callers that may.
//! - Every connection's requests share one memory [`Budget`].
//! - Each connection's caller is read from its token on its own thread,
//!   before its first byte is read.

use super::pipe::{self, Accepted, PipeTransport};
use super::security::SecurityDescriptor;
use super::supervisor::{Setup, WinSupervisor};
use super::sys::{wait_events, wide, Event};
use super::token;
use crate::conn::{self, Budget, ConnConfig, ReadOnlySlots};
use crate::exit;
use crate::helper::{ConnId, Helper, HelperCore};
use crate::helper_log;
use boxpilot_protocol::Limits;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::ERROR_ACCESS_DENIED;

/// Connections served at once.
pub(crate) const MAX_CONNECTIONS: usize = 8;

/// Of those, connections from callers that may not start.
pub(crate) const MAX_READ_ONLY_CONNECTIONS: usize = MAX_CONNECTIONS / 2;

/// With no connection and no sing-box for this long, the helper exits; the
/// GUI starts it again on demand (ADR 0006 rule 6).
pub(crate) const IDLE_EXIT: Duration = Duration::from_secs(60);

/// How often the loop looks up from waiting for a client.
const TICK: Duration = Duration::from_secs(1);

/// How long shutting down waits for connections to finish.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(20);

type Core = HelperCore<WinSupervisor>;

/// The connections being served.
struct Connections {
    core: Arc<Core>,
    budget: Arc<Budget>,
    read_only: Arc<ReadOnlySlots>,
    config: ConnConfig,
    /// Each connection's transport, to close them all at shutdown.
    open: Arc<Mutex<HashMap<ConnId, Arc<PipeTransport>>>>,
    /// Connections whose pipe handle isn't closed yet.
    live: Arc<AtomicUsize>,
    /// Set whenever a connection ends.
    ended: Arc<Event>,
    threads: Vec<JoinHandle<()>>,
    next: ConnId,
}

impl Connections {
    fn live(&self) -> usize {
        self.live.load(Ordering::SeqCst)
    }

    /// Serve a connected pipe instance on its own thread.
    fn serve(&mut self, pipe: std::os::windows::io::OwnedHandle) {
        let transport = match PipeTransport::new(pipe) {
            Ok(transport) => Arc::new(transport),
            Err(error) => {
                helper_log!("a connection could not be set up: {error}");
                return;
            }
        };
        let id = self.next;
        self.next += 1;
        self.open
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id, transport.clone());
        self.live.fetch_add(1, Ordering::SeqCst);
        let (core, budget, config) = (self.core.clone(), self.budget.clone(), self.config);
        let read_only = self.read_only.clone();
        let (open, live, ended) = (self.open.clone(), self.live.clone(), self.ended.clone());
        let thread = thread::Builder::new()
            .name(format!("connection {id}"))
            .spawn(move || {
                let caller = token::caller(transport.handle());
                match read_only.admit(caller.authority) {
                    Some(_admitted) => {
                        conn::serve(&*transport, id, &caller, &*core, &config, &budget);
                    }
                    None => helper_log!(
                        "connection {id}: closed, {MAX_READ_ONLY_CONNECTIONS} read-only \
                         connections are served already"
                    ),
                }
                open.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&id);
                // The last reference: the pipe instance closes here.
                drop(transport);
                live.fetch_sub(1, Ordering::SeqCst);
                ended.set();
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
        let deadline = Instant::now() + SHUTDOWN_GRACE;
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

/// Run the helper on `pipe_name`, with the DACL `pipe_sddl`, until `stop`
/// is set or it has been idle for [`IDLE_EXIT`]. `ready` is called once the
/// pipe exists. Returns the process exit code.
pub(crate) fn run(
    setup: Setup,
    pipe_name: &str,
    pipe_sddl: &str,
    stop: &Event,
    ready: impl FnOnce(),
) -> i32 {
    let limits = Limits::default();
    let supervisor = match WinSupervisor::start(setup, limits.max_log_line) {
        Ok(supervisor) => supervisor,
        Err((code, message)) => {
            helper_log!("refusing to run: {message}");
            return code;
        }
    };
    let core = Arc::new(HelperCore::new(supervisor, limits));
    let (descriptor, name) = match (SecurityDescriptor::from_sddl(pipe_sddl), wide(pipe_name)) {
        (Ok(descriptor), Ok(name)) => (descriptor, name),
        (Err(error), _) | (_, Err(error)) => {
            helper_log!("the pipe could not be described: {error}");
            return exit::PIPE_FAILED;
        }
    };
    let max_instances = MAX_CONNECTIONS as u32 + 1;
    let mut listening = match pipe::create_instance(&name, &descriptor, true, max_instances) {
        Ok(pipe) => Some(pipe),
        Err(error) if error.raw_os_error() == Some(ERROR_ACCESS_DENIED.0 as i32) => {
            helper_log!("refusing to run: another process holds {pipe_name}");
            return exit::PIPE_SQUATTED;
        }
        Err(error) => {
            helper_log!("refusing to run: {pipe_name} could not be created: {error}");
            return exit::PIPE_FAILED;
        }
    };
    ready();
    helper_log!("listening on {pipe_name}");

    let mut connections = Connections {
        core: core.clone(),
        budget: Arc::new(Budget::new(Budget::DEFAULT_LIMIT)),
        read_only: Arc::new(ReadOnlySlots::new(MAX_READ_ONLY_CONNECTIONS)),
        config: ConnConfig::default(),
        open: Arc::new(Mutex::new(HashMap::new())),
        live: Arc::new(AtomicUsize::new(0)),
        ended: match Event::new() {
            Ok(event) => Arc::new(event),
            Err(error) => {
                helper_log!("no event: {error}");
                return exit::INTERNAL;
            }
        },
        threads: Vec::new(),
        next: 1,
    };
    let mut idle_since = Instant::now();
    let mut failures = 0u32;
    let code = loop {
        connections.ended.reset();
        let live = connections.live();
        if live > 0 || !core.is_idle() {
            idle_since = Instant::now();
        } else if idle_since.elapsed() >= IDLE_EXIT {
            helper_log!("idle for {}s: stopping", IDLE_EXIT.as_secs());
            break exit::OK;
        }
        let Some(pipe) = &listening else {
            // The last instance broke. Take the name again only if no
            // instance of ours is left to hold it.
            match pipe::create_instance(&name, &descriptor, live == 0, max_instances) {
                Ok(pipe) => listening = Some(pipe),
                Err(error)
                    if live == 0 && error.raw_os_error() == Some(ERROR_ACCESS_DENIED.0 as i32) =>
                {
                    helper_log!("another process took {pipe_name}: stopping");
                    break exit::PIPE_SQUATTED;
                }
                Err(error) => {
                    helper_log!("no listening instance: {error}");
                    failures += 1;
                    if live == 0 || failures > 10 {
                        break exit::PIPE_FAILED;
                    }
                    if let Ok(Some(1)) =
                        wait_events(&[&connections.ended, stop], Some(Instant::now() + TICK))
                    {
                        break exit::OK;
                    }
                }
            }
            continue;
        };
        if live >= MAX_CONNECTIONS {
            // Full: the listening instance waits until a connection ends.
            if let Ok(Some(1)) =
                wait_events(&[&connections.ended, stop], Some(Instant::now() + TICK))
            {
                break exit::OK;
            }
            continue;
        }
        match pipe::accept(pipe, stop, Instant::now() + TICK) {
            Ok(Accepted::Client) => {
                failures = 0;
                let connected = listening.take().expect("matched Some above");
                // The connected instance holds the name while the next one
                // is created.
                match pipe::create_instance(&name, &descriptor, false, max_instances) {
                    Ok(next) => listening = Some(next),
                    Err(error) => helper_log!("no next listening instance: {error}"),
                }
                connections.serve(connected);
            }
            Ok(Accepted::Stopped) => break exit::OK,
            Ok(Accepted::TimedOut) => {}
            Err(error) => {
                helper_log!("waiting for a client failed: {error}");
                // This instance may be unusable: replace it, after a pause.
                listening = None;
                failures += 1;
                if failures > 10 {
                    break exit::PIPE_FAILED;
                }
                if let Ok(Some(0)) = wait_events(&[stop], Some(Instant::now() + TICK)) {
                    break exit::OK;
                }
            }
        }
    };
    drop(listening);
    core.shutdown();
    connections.close_all();
    helper_log!("stopped (exit code {code})");
    code
}
