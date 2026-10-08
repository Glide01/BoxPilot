//! The machine-wide state behind every connection (ADR 0006 rules 1, 4 and
//! 6): at most one sing-box at a time, owned by the connection that started
//! it.
//!
//! - **One sing-box, machine-wide.** A `start` while another connection's
//!   sing-box runs is answered `busy`. A `start` from the connection whose
//!   sing-box runs replaces it.
//! - **Stop.** Any caller that may start may stop it, whoever started it:
//!   who may control machine-wide networking is the administrator's call
//!   (rule 4), and every such caller is one the administrator allowed.
//! - **Events only to the starting connection.** sing-box's lines and its
//!   exit go to that connection's outbox and nowhere else: its logs are as
//!   private as the files it reads.
//! - **Session-bound.** When that connection ends, for any reason, its
//!   sing-box stops (rule 6).
//!
//! The platform supplies the [`Supervisor`]: it makes the run directory,
//! verifies and spawns sing-box, and reports its lines and exit. The core is
//! reached through the [`Helper`] trait, which [`HelperCore`] implements.

#![forbid(unsafe_code)]

use crate::helper_log;
use crate::outbox::Outbox;
use crate::runcfg;
use crate::rundir::RunDir;
use boxpilot_policy::Placement;
use boxpilot_protocol::{
    encode_to_client, Authority, ErrorCode, Event, ExitInfo, HelloReply, Limits, ProtocolError,
    Reply, RunState, StartRequest, Started, ToClient, PROTOCOL_VERSION,
};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};

/// One connection, numbered by the server.
pub type ConnId = u64;

/// Who is asking, as the OS says: decided before the connection's first
/// byte is read, never from anything the caller sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    pub authority: Authority,
    /// The account's SID string. `None` when it could not be read, and then
    /// the caller is read-only too.
    pub user: Option<String>,
}

impl Caller {
    /// A caller the helper knows nothing about: `hello` and `status` only.
    pub fn read_only() -> Self {
        Self {
            authority: Authority::ReadOnly,
            user: None,
        }
    }
}

/// The installed sing-box, as `hello` reports it: from the manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub sing_box_version: String,
    pub sing_box_sha256: String,
}

/// A failure on the helper's own side, worded for the `internal` error
/// reply and the helper's log: it never quotes a config, an attachment or a
/// secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelperError(pub String);

impl HelperError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for HelperError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for HelperError {}

/// Where a running sing-box's output goes.
pub trait RunEvents: Send + Sync {
    /// One line of sing-box's stdout or stderr.
    fn line(&self, line: String, truncated: bool);
    /// sing-box exited, and its run is cleaned up: called exactly once,
    /// last.
    fn exited(&self, exit: ExitInfo);
}

/// A running sing-box, as the platform keeps it.
pub trait Process: Send {
    /// Stop sing-box, and return once [`RunEvents::exited`] has been
    /// delivered for it (at once, if it already has).
    fn stop(&mut self);
}

/// What the core needs from the platform.
///
/// A `Box<dyn Process>` may be dropped while the core holds its lock, from
/// any thread, sing-box's own exit path included: dropping one must neither
/// block nor stop anything.
pub trait Supervisor: Send + Sync + 'static {
    /// The installed sing-box, from the manifest.
    fn installed(&self) -> &Installed;

    /// A fresh, private run directory for one start by the account `user`
    /// (a SID string), and where the policy points the helper-owned fields:
    /// the run directory for attachments, `user`'s own state directory for
    /// the cache file and the Tailscale state.
    fn prepare_run(&self, user: &str) -> Result<(RunDir, Placement), HelperError>;

    /// Start sing-box on the config already written in `run`, after
    /// checking it is the installed one. `events` gets its lines, then,
    /// once it has exited and `run` is removed, exactly one `exited`.
    fn spawn(
        &self,
        run: RunDir,
        events: Arc<dyn RunEvents>,
    ) -> Result<Box<dyn Process>, HelperError>;
}

/// The helper as a connection sees it: one call per request, plus the end
/// of the connection.
pub trait Helper: Send + Sync {
    fn hello(&self, caller: &Caller) -> Reply;
    fn status(&self) -> Reply;
    /// `outbox` is the starting connection's: the only one that gets this
    /// run's events.
    fn start(
        &self,
        conn: ConnId,
        caller: &Caller,
        start: StartRequest,
        outbox: &Arc<Outbox>,
    ) -> Reply;
    fn stop(&self, caller: &Caller) -> Reply;
    /// Connection `conn` has ended: stop its sing-box, if one runs.
    fn disconnected(&self, conn: ConnId);
    /// No sing-box runs or is starting.
    fn is_idle(&self) -> bool;
    /// The helper is stopping: stop whatever runs.
    fn shutdown(&self);
}

/// The real core, over a platform [`Supervisor`].
pub struct HelperCore<S> {
    supervisor: S,
    shared: Arc<Shared>,
    limits: Limits,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

struct State {
    slot: Slot,
    last_exit: Option<ExitInfo>,
    next_run: u64,
}

enum Slot {
    Idle,
    /// A start is under way: its sing-box isn't spawned yet, or its
    /// `Process` isn't stored yet.
    Starting {
        conn: ConnId,
        run: u64,
    },
    /// `process` is `None` while some thread is stopping it.
    Running {
        conn: ConnId,
        run: u64,
        process: Option<Box<dyn Process>>,
    },
}

impl Slot {
    fn owner(&self) -> Option<ConnId> {
        match self {
            Slot::Idle => None,
            Slot::Starting { conn, .. } | Slot::Running { conn, .. } => Some(*conn),
        }
    }

    fn run(&self) -> Option<u64> {
        match self {
            Slot::Idle => None,
            Slot::Starting { run, .. } | Slot::Running { run, .. } => Some(*run),
        }
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn wait<'a>(&self, guard: MutexGuard<'a, State>) -> MutexGuard<'a, State> {
        self.changed
            .wait(guard)
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Run `run`'s sing-box has exited.
    fn exited(&self, run: u64, exit: ExitInfo) {
        let mut state = self.lock();
        state.last_exit = Some(exit);
        let finished = if state.slot.run() == Some(run) {
            std::mem::replace(&mut state.slot, Slot::Idle)
        } else {
            Slot::Idle
        };
        drop(state);
        drop(finished);
        self.changed.notify_all();
    }
}

impl<S: Supervisor> HelperCore<S> {
    pub fn new(supervisor: S, limits: Limits) -> Self {
        Self {
            supervisor,
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    slot: Slot::Idle,
                    last_exit: None,
                    next_run: 1,
                }),
                changed: Condvar::new(),
            }),
            limits,
        }
    }

    pub fn supervisor(&self) -> &S {
        &self.supervisor
    }

    /// Stop the sing-box connection `owner` started (any, for `None`), and
    /// return once it has exited. Waits out a start under way first, and a
    /// stop another thread began.
    fn stop_where(&self, owner: Option<ConnId>) {
        let mut state = self.shared.lock();
        loop {
            match &mut state.slot {
                Slot::Idle => return,
                slot if owner.is_some_and(|conn| slot.owner() != Some(conn)) => return,
                Slot::Running { process, .. } if process.is_some() => {
                    let mut process = process.take().expect("checked to be Some");
                    drop(state);
                    process.stop();
                    drop(process);
                    state = self.shared.lock();
                }
                _ => state = self.shared.wait(state),
            }
        }
    }

    /// Take the slot for a start by `conn`: its run number, or the `busy`
    /// reply. `conn`'s own running sing-box is stopped first.
    fn claim(&self, conn: ConnId) -> Result<u64, Reply> {
        let mut state = self.shared.lock();
        loop {
            match &mut state.slot {
                Slot::Idle => {
                    let run = state.next_run;
                    state.next_run += 1;
                    state.slot = Slot::Starting { conn, run };
                    return Ok(run);
                }
                slot if slot.owner() != Some(conn) => {
                    return Err(Reply::error(
                        ErrorCode::Busy,
                        "sing-box is running for another connection",
                    ))
                }
                Slot::Running { process, .. } if process.is_some() => {
                    let mut process = process.take().expect("checked to be Some");
                    drop(state);
                    process.stop();
                    drop(process);
                    state = self.shared.lock();
                }
                _ => state = self.shared.wait(state),
            }
        }
    }

    /// Give the slot back after a start that failed before sing-box ran.
    fn release(&self, run: u64) {
        let mut state = self.shared.lock();
        if matches!(state.slot, Slot::Starting { run: r, .. } if r == run) {
            state.slot = Slot::Idle;
        }
        drop(state);
        self.shared.changed.notify_all();
    }

    fn launch(
        &self,
        conn: ConnId,
        run: u64,
        user: &str,
        checked: runcfg::CheckedStart,
        outbox: &Arc<Outbox>,
    ) -> Result<Started, HelperError> {
        let (run_dir, placement) = self.supervisor.prepare_run(user)?;
        let secret = runcfg::fresh_secret()
            .map_err(|error| HelperError::new(format!("the OS RNG failed: {error}")))?;
        let prepared = runcfg::build(checked, &placement, runcfg::free_loopback_port, &secret)
            .map_err(|error| HelperError::new(error.to_string()))?;
        run_dir.write(&prepared).map_err(|error| {
            HelperError::new(format!("the run directory could not be written: {error}"))
        })?;
        let started = prepared.started();
        drop(prepared);
        let events = Arc::new(RunSink {
            shared: Arc::downgrade(&self.shared),
            run,
            outbox: outbox.clone(),
            limits: self.limits,
            exited: AtomicBool::new(false),
        });
        let process = self.supervisor.spawn(run_dir, events)?;
        let mut state = self.shared.lock();
        // If sing-box already exited, its `exited` freed the slot and is on
        // its way; the start still happened.
        let early = if matches!(state.slot, Slot::Starting { run: r, .. } if r == run) {
            state.slot = Slot::Running {
                conn,
                run,
                process: Some(process),
            };
            None
        } else {
            Some(process)
        };
        drop(state);
        drop(early);
        self.shared.changed.notify_all();
        Ok(started)
    }
}

impl<S: Supervisor> Helper for HelperCore<S> {
    fn hello(&self, caller: &Caller) -> Reply {
        let installed = self.supervisor.installed();
        Reply::Hello(HelloReply {
            protocol_version: PROTOCOL_VERSION,
            helper_version: env!("CARGO_PKG_VERSION").to_owned(),
            sing_box_version: installed.sing_box_version.clone(),
            sing_box_sha256: installed.sing_box_sha256.clone(),
            may_start: caller.authority.may_start(),
        })
    }

    fn status(&self) -> Reply {
        let state = self.shared.lock();
        Reply::Status {
            state: match state.slot {
                Slot::Idle => RunState::Stopped,
                Slot::Starting { .. } => RunState::Starting,
                Slot::Running { .. } => RunState::Running,
            },
            last_exit: state.last_exit,
        }
    }

    fn start(
        &self,
        conn: ConnId,
        caller: &Caller,
        start: StartRequest,
        outbox: &Arc<Outbox>,
    ) -> Reply {
        // The session refuses this already; the core doesn't rely on it.
        if !caller.authority.may_start() {
            return ProtocolError::Unauthorized.reply();
        }
        let Some(user) = caller.user.as_deref() else {
            return Reply::error(
                ErrorCode::Internal,
                "the caller's account could not be read",
            );
        };
        // Checked before the slot is taken: a refused config never stops
        // the sing-box this connection already runs.
        let checked = match runcfg::check(start) {
            Ok(checked) => checked,
            Err(refusals) => {
                helper_log!(
                    "connection {conn}: start refused by the policy ({} refusals)",
                    refusals.len()
                );
                return Reply::refused(&refusals);
            }
        };
        let run = match self.claim(conn) {
            Ok(run) => run,
            Err(busy) => {
                helper_log!("connection {conn}: start refused, busy");
                return busy;
            }
        };
        match self.launch(conn, run, user, checked, outbox) {
            Ok(started) => {
                helper_log!("connection {conn}: sing-box run {run} started for {user}");
                Reply::Started(started)
            }
            Err(error) => {
                self.release(run);
                helper_log!("connection {conn}: start failed: {error}");
                Reply::error(ErrorCode::Internal, error.0)
            }
        }
    }

    fn stop(&self, caller: &Caller) -> Reply {
        if !caller.authority.may_start() {
            return ProtocolError::Unauthorized.reply();
        }
        self.stop_where(None);
        Reply::Stopped
    }

    fn disconnected(&self, conn: ConnId) {
        self.stop_where(Some(conn));
    }

    fn is_idle(&self) -> bool {
        matches!(self.shared.lock().slot, Slot::Idle)
    }

    fn shutdown(&self) {
        self.stop_where(None);
    }
}

/// One run's [`RunEvents`]: into the starting connection's outbox, and the
/// exit into the core.
struct RunSink {
    shared: Weak<Shared>,
    run: u64,
    outbox: Arc<Outbox>,
    limits: Limits,
    exited: AtomicBool,
}

impl RunEvents for RunSink {
    fn line(&self, line: String, truncated: bool) {
        if self.exited.load(Ordering::SeqCst) {
            return;
        }
        // A log line always encodes: `encode_to_client` cuts it to the
        // protocol's limit first.
        if let Ok(bytes) = encode_to_client(
            &ToClient::Event(Event::Log { line, truncated }),
            &self.limits,
        ) {
            self.outbox.push_log(bytes);
        }
    }

    fn exited(&self, exit: ExitInfo) {
        if self.exited.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Ok(bytes) = encode_to_client(&ToClient::Event(Event::Exited(exit)), &self.limits) {
            self.outbox.push_must(bytes);
        }
        helper_log!(
            "sing-box run {} exited (code {:?}); {} log lines dropped for its connection",
            self.run,
            exit.code,
            self.outbox.dropped()
        );
        if let Some(shared) = self.shared.upgrade() {
            shared.exited(self.run, exit);
        }
    }
}
