use crate::core::privileged_helper::{
    self, HelperConnection, HelperEvent, RunningStart, GUI_SETS_SYSTEM_PROXY,
};
use crate::core::process::{
    cleanup_after_process_stop, describe_exit, disable_system_proxy, enable_system_proxy,
    exit_message, prepare_process_start, reap_child, signal_stop, start_sing_box, terminate_child,
};
#[cfg(unix)]
use crate::core::pid_file::{forget_sing_box_pid, record_sing_box_pid, stop_stale_sing_box};
use crate::core::settings::{StatusEvent, StatusLevel, SING_EXECUTABLE};
use crate::core::singbox_api::SingBoxApi;
use crate::core::subscription::is_api_bind_failure;
use crate::i18n::s;
use crate::state::drain::next_batch;
use crate::state::log_buffer::LogBuffer;
use boxpilot_protocol::TunOptions;
use futures_channel::mpsc::UnboundedReceiver;
use gpui::{Context, Entity, EventEmitter, Task};
use std::path::PathBuf;
use std::process::Child;
use std::time::Duration;

/// Once a pipe line arrives, how long the drain waits for the rest of the
/// burst before handing it to `LogBuffer` in one go.
const LOG_COALESCE: Duration = Duration::from_millis(50);
const CHILD_WAIT_INTERVAL: Duration = Duration::from_millis(200);
/// How long a stop waits for sing-box to exit on SIGTERM before killing it
/// (Linux and macOS; Windows kills right away).
const STOP_GRACE: Duration = Duration::from_secs(3);
/// How long a stop waits for the privileged helper's `stopped` before it
/// closes the connection, which stops sing-box all the same.
const HELPER_STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// Which sing-box a start runs (`privileged_helper::StartRoute`).
#[derive(Clone)]
pub enum Launch {
    /// sing-box as BoxPilot's own child, on the runtime config `AppState`
    /// wrote.
    Local {
        sing_path: PathBuf,
        config_path: PathBuf,
        working_dir: PathBuf,
        /// The port of BoxPilot's `api` service in this run's config.
        api_port: u16,
    },
    /// TUN through the privileged helper (Windows and macOS, ADR 0006). The
    /// start is built in the prep, off the UI thread: it reads the profile's
    /// local files and runs the policy.
    Helper {
        /// The active profile's canonical config.
        config_path: PathBuf,
        /// BoxPilot's data dir: a profile's relative paths resolve against
        /// it, as for a local sing-box, and the running view goes there.
        app_dir: PathBuf,
        options: TunOptions,
    },
}

/// Snapshot of paths + mode flags captured when start is requested.
#[derive(Clone)]
pub struct PendingStart {
    pub launch: Launch,
    pub proxy_mode: bool,
    pub set_system_proxy: bool,
}

/// A run through the privileged helper has started: its sing-box API, on a
/// port and secret the helper picked. Emitted just before the Running
/// edge, so `AppState` hands it to every entity first.
pub struct HelperApi(pub SingBoxApi);

/// sing-box said it could not listen on BoxPilot's API port `port` (another
/// program took it after it was picked); it exits right after. `AppState`
/// starts once more with a fresh port (`ApiPortRetry`).
pub struct ApiPortLost {
    pub port: u16,
}

pub enum ProcessState {
    /// No child process. `cleanup` holds the previous run's background
    /// reap + TUN/system-proxy teardown while it is still in flight;
    /// `start()` awaits it before prepping so a restart never races the
    /// old instance's cleanup.
    Stopped { cleanup: Option<Task<()>> },
    /// Background task running `prepare_process_start` (TUN cleanup + DNS
    /// flush), after awaiting the previous run's cleanup, which it owns:
    /// dropping `_prep` would cancel that too, so a stop or a superseded
    /// start sets `abandoned` instead and the task, once prepped, goes back
    /// to `Stopped` without spawning sing-box.
    Preparing { _prep: Task<()>, abandoned: bool },
    /// sing-box is alive. `drain` awaits the pipe channel (a child) or the
    /// helper's events and feeds `LogBuffer` (no wakeups while sing-box is
    /// quiet); `stop()` detaches it rather than cancelling it, so whatever
    /// sing-box wrote on its way out (a fatal error, a panic) still lands —
    /// it ends on its own when both pipes close, or the helper connection
    /// does. For a child, `_wait` polls `child.try_wait()` and transitions
    /// back to `Stopped` on exit; for the helper, the drain does that when
    /// `exited` arrives or the connection ends. The tasks exit cleanly when
    /// their entities drop.
    Running {
        backend: RunBackend,
        running_mode: bool,
        running_set_system_proxy: bool,
        drain: Task<()>,
        _wait: Option<Task<()>>,
    },
}

/// What a running sing-box is to BoxPilot.
pub enum RunBackend {
    /// BoxPilot's own child.
    Child(Child),
    /// The privileged helper's sing-box, which lives exactly as long as
    /// this connection (ADR 0006 rule 6). `run` numbers it, so a late
    /// event from an earlier run is told apart.
    Helper {
        connection: HelperConnection,
        run: u64,
    },
}

pub struct ProcessSession {
    pub state: ProcessState,
    pub logs: Entity<LogBuffer>,
    /// The last helper run's number (`RunBackend::Helper::run`).
    helper_runs: u64,
    /// Where the running sing-box's pid file lives (its working dir), for
    /// the stop that reaps it to remove (`core::pid_file`). Unix only.
    #[cfg(unix)]
    pid_dir: Option<PathBuf>,
}

impl EventEmitter<StatusEvent> for ProcessSession {}
impl EventEmitter<ApiPortLost> for ProcessSession {}
impl EventEmitter<HelperApi> for ProcessSession {}

impl ProcessSession {
    pub fn new(logs: Entity<LogBuffer>) -> Self {
        Self {
            state: ProcessState::Stopped { cleanup: None },
            logs,
            helper_runs: 0,
            #[cfg(unix)]
            pid_dir: None,
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(self.state, ProcessState::Running { .. })
    }

    pub fn is_starting(&self) -> bool {
        matches!(self.state, ProcessState::Preparing { .. })
    }

    pub fn is_stopped(&self) -> bool {
        matches!(self.state, ProcessState::Stopped { .. })
    }

    /// Whether the running sing-box is the privileged helper's.
    pub fn runs_helper(&self) -> bool {
        matches!(
            self.state,
            ProcessState::Running {
                backend: RunBackend::Helper { .. },
                ..
            }
        )
    }

    /// The last stop's cleanup, if it is still under way, for a caller that
    /// must wait for it before changing what it used: reinstalling or
    /// removing the privileged helper, which waits for its run's `stop` to
    /// be answered. Taken, so the next `start` doesn't wait for it again;
    /// the caller holds starts off until it is done.
    pub fn take_cleanup(&mut self) -> Option<Task<()>> {
        match &mut self.state {
            ProcessState::Stopped { cleanup } => cleanup.take(),
            _ => None,
        }
    }

    /// Prep runs on the background executor before sing-box starts —
    /// `prepare_process_start` does TUN-adapter cleanup and DNS flush, both
    /// of which must complete before sing-box opens its own TUN handle. A
    /// helper start then builds its request and asks the helper there too.
    pub fn start(&mut self, pending: PendingStart, cx: &mut Context<Self>) {
        let ProcessState::Stopped { cleanup } = &mut self.state else {
            return;
        };
        let prior_cleanup = cleanup.take();
        let prep_task = match &pending.launch {
            Launch::Local { .. } => Self::prep_local(prior_cleanup, pending, cx),
            Launch::Helper { .. } => Self::prep_helper(prior_cleanup, pending, cx),
        };
        self.state = ProcessState::Preparing {
            _prep: prep_task,
            abandoned: false,
        };
        cx.notify();
    }

    fn prep_local(
        prior_cleanup: Option<Task<()>>,
        pending: PendingStart,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let after_prep = pending.clone();
        cx.spawn(async move |this, cx| {
            // A restart can get here while the previous child is still being
            // reaped on the background executor; await that cleanup so the
            // old instance's TUN adapter and system-proxy teardown finish
            // before we prep (and the new sing-box binds ports) on top of it.
            if let Some(prior_cleanup) = prior_cleanup {
                prior_cleanup.await;
            }

            let is_tun_mode = !pending.proxy_mode;
            #[cfg(unix)]
            let working_dir = match &pending.launch {
                Launch::Local { working_dir, .. } => working_dir.clone(),
                Launch::Helper { app_dir, .. } => app_dir.clone(),
            };
            cx.background_executor()
                .spawn(async move {
                    // A sing-box can outlive a crashed BoxPilot (no
                    // PDEATHSIG across a file-caps exec on Linux, none at
                    // all on macOS) and would hold the ports, the system
                    // proxy and the TUN device this start needs.
                    #[cfg(unix)]
                    stop_stale_sing_box(&working_dir, STOP_GRACE);
                    prepare_process_start(is_tun_mode)
                })
                .await;

            let _ = this.update(cx, |session, cx| {
                if session.start_abandoned() {
                    session.state = ProcessState::Stopped { cleanup: None };
                    cx.notify();
                } else {
                    session.spawn_child(after_prep, cx);
                }
            });
        })
    }

    /// A TUN start through the privileged helper: prep, then the whole
    /// start (local files read, the policy run, the helper started and
    /// asked) on the background executor, since it blocks.
    fn prep_helper(
        prior_cleanup: Option<Task<()>>,
        pending: PendingStart,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        cx.spawn(async move |this, cx| {
            if let Some(prior_cleanup) = prior_cleanup {
                prior_cleanup.await;
            }
            let Launch::Helper {
                config_path,
                app_dir,
                options,
            } = pending.launch.clone()
            else {
                return;
            };
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    // DNS flush only: the adapter cleanup needs elevation,
                    // and the helper does it itself.
                    prepare_process_start(true);
                    privileged_helper::start_profile(&config_path, &app_dir, options)
                })
                .await;
            let _ = this.update(cx, |session, cx| {
                session.helper_started(outcome, pending, cx)
            });
        })
    }

    /// The helper start finished. Abandoned meanwhile: stop what started.
    fn helper_started(
        &mut self,
        outcome: Result<RunningStart, String>,
        pending: PendingStart,
        cx: &mut Context<Self>,
    ) {
        if self.start_abandoned() {
            let cleanup = outcome.ok().map(|run| {
                cx.background_executor()
                    .spawn(async move { stop_helper_run(run.connection, false) })
            });
            self.state = ProcessState::Stopped { cleanup };
            cx.notify();
            return;
        }
        let run = match outcome {
            Ok(run) => run,
            Err(message) => {
                self.state = ProcessState::Stopped { cleanup: None };
                cx.emit(StatusEvent {
                    level: StatusLevel::Error,
                    message,
                });
                cx.notify();
                return;
            }
        };
        // Windows: the user's system proxy is the user's to set, so BoxPilot
        // sets it, as the user: the helper's SYSTEM sing-box never does. One
        // WinINet call, here rather than in the start, so it is set exactly
        // while this run is `Running`, and the stop's cleanup clears it.
        // macOS: the helper's sing-box sets and unsets it itself, so the
        // run is marked as not having set it, and no stop or exit path
        // here touches it (`GUI_SETS_SYSTEM_PROXY`).
        let mut set_system_proxy = false;
        if pending.set_system_proxy && GUI_SETS_SYSTEM_PROXY {
            if let Launch::Helper { options, .. } = &pending.launch {
                match enable_system_proxy(options.proxy_port) {
                    Ok(()) => set_system_proxy = true,
                    Err(message) => cx.emit(StatusEvent {
                        level: StatusLevel::Warning,
                        message,
                    }),
                }
            }
        }
        // Before the Running edge below: effects run in order, so every
        // entity has this run's API when its stream starts.
        cx.emit(HelperApi(run.api));
        self.helper_runs += 1;
        let run_id = self.helper_runs;
        let drain = self.helper_drain(run_id, run.events, cx);
        self.state = ProcessState::Running {
            backend: RunBackend::Helper {
                connection: run.connection,
                run: run_id,
            },
            running_mode: pending.proxy_mode,
            running_set_system_proxy: set_system_proxy,
            drain,
            _wait: None,
        };
        cx.notify();
    }

    /// Feed the helper's `log` events to `LogBuffer` the way pipe lines go,
    /// and end the run on `exited`, or when the connection ends without it
    /// (the helper stops sing-box then all the same).
    fn helper_drain(
        &self,
        run: u64,
        events: UnboundedReceiver<HelperEvent>,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let weak_logs = self.logs.downgrade();
        cx.spawn(async move |this, cx| {
            let mut events = events;
            let executor = cx.background_executor().clone();
            while let Some(batch) = next_batch(&mut events, || executor.timer(LOG_COALESCE)).await {
                let mut lines = Vec::new();
                let mut ended = None;
                for event in batch {
                    match event {
                        HelperEvent::Log(line) => lines.push(line),
                        HelperEvent::Exited(exit) => {
                            ended.get_or_insert_with(|| exit_message(exit.code, exit.signal));
                        }
                        HelperEvent::Closed(reason) => {
                            eprintln!("Privileged helper: the connection ended ({reason})");
                            ended.get_or_insert_with(|| s().helper.lost_running.to_string());
                        }
                    }
                }
                if !lines.is_empty()
                    && weak_logs
                        .update(cx, |logs, cx| logs.push_pipe(lines, cx))
                        .is_err()
                {
                    return;
                }
                if let Some(message) = ended {
                    let _ =
                        this.update(cx, |session, cx| session.helper_run_ended(run, message, cx));
                    return;
                }
            }
        })
    }

    /// Helper run `run` ended on its own. A stop already under way, or a
    /// later run, has nothing to do with it.
    fn helper_run_ended(&mut self, run: u64, message: String, cx: &mut Context<Self>) {
        let current = matches!(
            &self.state,
            ProcessState::Running {
                backend: RunBackend::Helper { run: current, .. },
                ..
            } if *current == run
        );
        if current {
            cx.emit(StatusEvent {
                level: StatusLevel::Warning,
                message,
            });
            self.stop(cx);
        }
    }

    fn spawn_child(&mut self, pending: PendingStart, cx: &mut Context<Self>) {
        let Launch::Local {
            sing_path,
            config_path,
            working_dir,
            api_port,
        } = &pending.launch
        else {
            return;
        };
        match start_sing_box(sing_path, config_path, working_dir) {
            Ok((child, log_rx)) => {
                #[cfg(unix)]
                {
                    record_sing_box_pid(working_dir, child.id());
                    self.pid_dir = Some(working_dir.clone());
                }
                let weak_logs = self.logs.downgrade();
                let api_port = *api_port;
                let drain = cx.spawn(async move |this, cx| {
                    let mut log_rx = log_rx;
                    let mut api_port_lost = false;
                    let executor = cx.background_executor().clone();
                    // `None`: both pipes closed — sing-box is gone.
                    while let Some(batch) =
                        next_batch(&mut log_rx, || executor.timer(LOG_COALESCE)).await
                    {
                        if !api_port_lost
                            && batch.iter().any(|line| is_api_bind_failure(line, api_port))
                        {
                            api_port_lost = true;
                            let _ =
                                this.update(cx, |_, cx| cx.emit(ApiPortLost { port: api_port }));
                        }
                        if weak_logs
                            .update(cx, |logs, cx| logs.push_pipe(batch, cx))
                            .is_err()
                        {
                            return;
                        }
                    }
                });

                let wait = cx.spawn(async move |this, cx| {
                    loop {
                        cx.background_executor().timer(CHILD_WAIT_INTERVAL).await;
                        // Some(message) once the child has exited on its own.
                        let exited = this.update(cx, |session, _cx| {
                            if let ProcessState::Running {
                                backend: RunBackend::Child(child),
                                ..
                            } = &mut session.state
                            {
                                match child.try_wait() {
                                    Ok(Some(status)) => Some(describe_exit(&status)),
                                    _ => None,
                                }
                            } else {
                                Some(s().messages.sing_box_exited.to_string())
                            }
                        });
                        match exited {
                            Ok(Some(message)) => {
                                let _ = this.update(cx, |session, cx| {
                                    cx.emit(StatusEvent {
                                        level: StatusLevel::Warning,
                                        message,
                                    });
                                    session.stop(cx);
                                });
                                return;
                            }
                            Ok(None) => continue,
                            Err(_) => return,
                        }
                    }
                });

                self.state = ProcessState::Running {
                    backend: RunBackend::Child(child),
                    running_mode: pending.proxy_mode,
                    running_set_system_proxy: pending.set_system_proxy,
                    drain,
                    _wait: Some(wait),
                };
            }
            Err(e) => {
                self.state = ProcessState::Stopped { cleanup: None };
                cx.emit(StatusEvent {
                    level: StatusLevel::Error,
                    message: (s().messages.start_failed)(
                        SING_EXECUTABLE,
                        &config_path.display().to_string(),
                        &e.to_string(),
                    ),
                });
            }
        }
        cx.notify();
    }

    /// Drop the start in progress: its prep runs to the end (see
    /// `ProcessState::Preparing`), then the session returns to `Stopped`
    /// without spawning sing-box. No-op unless `Preparing`.
    pub fn abandon_start(&mut self) {
        if let ProcessState::Preparing { abandoned, .. } = &mut self.state {
            *abandoned = true;
        }
    }

    fn start_abandoned(&self) -> bool {
        match self.state {
            ProcessState::Preparing { abandoned, .. } => abandoned,
            _ => false,
        }
    }

    /// Stop the running sing-box. For a child, `signal_stop` only sends the
    /// signal (SIGTERM on Linux and macOS, kill on Windows), so it stays on
    /// the UI thread; the potentially slow reap (up to `STOP_GRACE` before a
    /// SIGKILL) and the system-proxy/TUN cleanup run on the background
    /// executor. For the helper, the background task sends `stop`, waits for
    /// `stopped`, closes the connection and clears the system proxy
    /// (`stop_helper_run`). The task is kept in `Stopped { cleanup }` so a
    /// subsequent `start()` can await it.
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        match std::mem::replace(&mut self.state, ProcessState::Stopped { cleanup: None }) {
            ProcessState::Running {
                backend: RunBackend::Helper { connection, .. },
                running_set_system_proxy,
                drain,
                ..
            } => {
                drain.detach();
                let cleanup = cx
                    .background_executor()
                    .spawn(async move { stop_helper_run(connection, running_set_system_proxy) });
                self.state = ProcessState::Stopped {
                    cleanup: Some(cleanup),
                };
            }
            ProcessState::Running {
                backend: RunBackend::Child(mut child),
                running_mode,
                running_set_system_proxy,
                drain,
                ..
            } => {
                let _ = signal_stop(&mut child);
                drain.detach();
                #[cfg(unix)]
                let pid_dir = self.pid_dir.take();
                let cleanup = cx.background_executor().spawn(async move {
                    let mut child = child;
                    let _ = reap_child(&mut child, STOP_GRACE);
                    #[cfg(unix)]
                    if let Some(dir) = pid_dir {
                        forget_sing_box_pid(&dir, child.id());
                    }
                    cleanup_after_process_stop(running_set_system_proxy, !running_mode);
                });
                self.state = ProcessState::Stopped {
                    cleanup: Some(cleanup),
                };
            }
            // Already stopped: keep a still-running cleanup task alive
            // instead of dropping (cancelling) it.
            ProcessState::Stopped { cleanup } => {
                self.state = ProcessState::Stopped { cleanup };
            }
            // Preparing: don't spawn once prepped; it stays `Preparing`
            // (starting) until then.
            ProcessState::Preparing { _prep, .. } => {
                self.state = ProcessState::Preparing {
                    _prep,
                    abandoned: true,
                };
            }
        }
        cx.notify();
    }
}

impl Drop for ProcessSession {
    fn drop(&mut self) {
        // Defensive: if dropped without `stop()` (e.g. on app quit before
        // `cx.on_app_quit` runs), still kill the child so the pipe-reader
        // threads exit cleanly. Cleanup runs synchronously since we have no
        // executor handle here, so quitting can block for up to `STOP_GRACE`
        // while sing-box handles SIGTERM.
        match std::mem::replace(&mut self.state, ProcessState::Stopped { cleanup: None }) {
            ProcessState::Running {
                backend: RunBackend::Helper { connection, .. },
                running_set_system_proxy,
                ..
            } => {
                // Quitting: a short wait for the helper's `stopped`, then the
                // connection closes, which stops sing-box regardless.
                connection.stop(STOP_GRACE);
                if running_set_system_proxy {
                    let _ = disable_system_proxy();
                }
            }
            ProcessState::Running {
                backend: RunBackend::Child(child),
                running_mode,
                running_set_system_proxy,
                ..
            } => {
                let mut child = child;
                let _ = terminate_child(&mut child, STOP_GRACE);
                #[cfg(unix)]
                if let Some(dir) = self.pid_dir.take() {
                    forget_sing_box_pid(&dir, child.id());
                }
                cleanup_after_process_stop(running_set_system_proxy, !running_mode);
            }
            // A recent stop()'s background cleanup may still be in flight;
            // detach it so dropping the entity doesn't cancel the reap.
            ProcessState::Stopped {
                cleanup: Some(cleanup),
            } => cleanup.detach(),
            _ => {}
        }
    }
}

/// Stop a helper run and undo what BoxPilot set for it: ask the helper to
/// stop sing-box, wait for it up to `HELPER_STOP_TIMEOUT`, close the
/// connection, then clear the user's system proxy if this run set it (only
/// while it still points at BoxPilot's loopback proxy; Windows only, see
/// `GUI_SETS_SYSTEM_PROXY`). The TUN adapter, and on macOS the system
/// proxy, are the helper's to clean up. Blocking.
fn stop_helper_run(connection: HelperConnection, was_system_proxy: bool) {
    connection.stop(HELPER_STOP_TIMEOUT);
    if was_system_proxy {
        if let Err(e) = disable_system_proxy() {
            eprintln!("Warning: {}", e);
        }
    }
}
