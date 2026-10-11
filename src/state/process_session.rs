use crate::core::privileged_helper::{
    self, HelperConnection, HelperEvent, RunningStart, StartCancel, GUI_SETS_SYSTEM_PROXY,
};
use crate::core::process::{
    cleanup_after_process_stop, describe_exit, disable_system_proxy, disable_system_proxy_on,
    enable_system_proxy, exit_message, prepare_process_start, reap_child, signal_stop,
    start_sing_box, terminate_child,
};
#[cfg(unix)]
use crate::core::pid_file::{forget_sing_box_pid, record_sing_box_pid, stop_stale_sing_box};
use crate::core::proxy_marker;
use crate::core::settings::{StatusEvent, StatusLevel, SING_EXECUTABLE};
use crate::core::singbox_api::SingBoxApi;
use crate::core::subscription::is_api_bind_failure;
use crate::i18n::s;
use crate::state::drain::next_batch;
use crate::state::log_buffer::LogBuffer;
use boxpilot_protocol::TunOptions;
use futures_channel::mpsc::UnboundedReceiver;
use futures_channel::oneshot;
use gpui::{Context, Entity, EventEmitter, Task};
use std::path::{Path, PathBuf};
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
    /// No sing-box. `cleanup` is what the last run or start left under way,
    /// which `start()` awaits before prepping, so a start never races it: a
    /// stopped run's reap and TUN/system-proxy teardown; a start stopped
    /// before it was up, which starts nothing once done (and stops what the
    /// helper started); or, at launch, the undoing of a system proxy a
    /// BoxPilot that ended without stopping left set (`proxy_marker`).
    Stopped { cleanup: Option<Task<()>> },
    /// A start: `prep` awaits the last cleanup, which it owns, then, on the
    /// background executor, runs `prepare_process_start` (TUN cleanup and
    /// DNS flush) and, through the helper, the whole start. `start` numbers
    /// it. A stop doesn't wait for it: the session is `Stopped` at once with
    /// `prep` as its cleanup (dropping it would cancel the cleanup it owns
    /// too), a helper start under way is cancelled (`cancel`), and the task,
    /// once done, starts nothing (`is_current`).
    Preparing {
        prep: Task<()>,
        start: u64,
        cancel: Option<StartCancel>,
    },
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
    /// BoxPilot's data dir, where a system proxy set for a helper run is
    /// noted (`proxy_marker`).
    data_dir: PathBuf,
    /// The last start's number (`ProcessState::Preparing::start`).
    starts: u64,
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
    /// `data_dir`: BoxPilot's. A system proxy a helper run left set, if
    /// BoxPilot ended without clearing it, is undone first: the first start
    /// waits for that like for any cleanup.
    pub fn new(logs: Entity<LogBuffer>, data_dir: PathBuf, cx: &mut Context<Self>) -> Self {
        let cleanup = GUI_SETS_SYSTEM_PROXY.then(|| {
            let data_dir = data_dir.clone();
            cx.background_executor()
                .spawn(async move { clear_left_system_proxy(&data_dir) })
        });
        Self {
            state: ProcessState::Stopped { cleanup },
            logs,
            data_dir,
            starts: 0,
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

    /// Watch the cleanup still under way, if any, for a caller that must
    /// wait for it before changing what it used: reinstalling or removing
    /// the privileged helper, which waits for its run's `stop` to be
    /// answered. The cleanup stays the next start's to await as well, so no
    /// start, in either mode, runs before it is done. The receiver resolves
    /// once it is.
    pub fn watch_cleanup(&mut self, cx: &mut Context<Self>) -> Option<oneshot::Receiver<()>> {
        let ProcessState::Stopped { cleanup } = &mut self.state else {
            return None;
        };
        let task = cleanup.take()?;
        let (done, watched) = oneshot::channel();
        *cleanup = Some(cx.spawn(async move |_, _| {
            task.await;
            let _ = done.send(());
        }));
        Some(watched)
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
        self.starts += 1;
        let start = self.starts;
        let (prep, cancel) = match &pending.launch {
            Launch::Local { .. } => (Self::prep_local(start, prior_cleanup, pending, cx), None),
            Launch::Helper { .. } => {
                let cancel = StartCancel::default();
                let prep = Self::prep_helper(start, prior_cleanup, pending, cancel.clone(), cx);
                (prep, Some(cancel))
            }
        };
        self.state = ProcessState::Preparing {
            prep,
            start,
            cancel,
        };
        cx.notify();
    }

    /// Whether start `start` is still the one under way: neither stopped nor
    /// superseded since.
    fn is_current(&self, start: u64) -> bool {
        matches!(self.state, ProcessState::Preparing { start: current, .. } if current == start)
    }

    fn prep_local(
        start: u64,
        prior_cleanup: Option<Task<()>>,
        pending: PendingStart,
        cx: &mut Context<Self>,
    ) -> Task<()> {
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
                if session.is_current(start) {
                    session.spawn_child(pending, cx);
                }
            });
        })
    }

    /// A TUN start through the privileged helper: prep, then the whole
    /// start (local files read, the policy run, the helper started and
    /// asked) on the background executor, since it blocks; `cancel` ends it
    /// early on a stop.
    fn prep_helper(
        start: u64,
        prior_cleanup: Option<Task<()>>,
        pending: PendingStart,
        cancel: StartCancel,
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
                    // A Proxy-mode sing-box a crashed BoxPilot left behind
                    // (macOS has no PDEATHSIG) would hold the port the
                    // helper's sing-box listens on, and its system proxy.
                    #[cfg(unix)]
                    stop_stale_sing_box(&app_dir, STOP_GRACE);
                    // DNS flush only: the adapter cleanup needs elevation,
                    // and the helper does it itself.
                    prepare_process_start(true);
                    privileged_helper::start_profile(&config_path, &app_dir, options, &cancel)
                })
                .await;
            let dropped = this
                .update(cx, |session, cx| {
                    session.helper_started(start, outcome, pending, cx)
                })
                .ok()
                .flatten();
            // Stopped or superseded meanwhile: stop what the helper started
            // here, so that whoever waits for this task waits for that too.
            if let Some(run) = dropped {
                cx.background_executor()
                    .spawn(async move { stop_helper_run(run.connection, None) })
                    .await;
            }
        })
    }

    /// Helper start `start` finished. Still the one under way: its run is
    /// `Running` now, or its failure is shown. Stopped or superseded
    /// meanwhile: what it started is handed back, for its task to stop.
    fn helper_started(
        &mut self,
        start: u64,
        outcome: Result<RunningStart, String>,
        pending: PendingStart,
        cx: &mut Context<Self>,
    ) -> Option<RunningStart> {
        if !self.is_current(start) {
            return outcome.ok();
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
                return None;
            }
        };
        // Windows: the user's system proxy is the user's to set, so BoxPilot
        // sets it, as the user: the helper's SYSTEM sing-box never does. One
        // WinINet call, here rather than in the start, so it is set exactly
        // while this run is `Running`, and the stop's cleanup clears it. It
        // is noted first: should BoxPilot end before clearing it, the next
        // launch does (`proxy_marker`).
        // macOS: the helper sets and unsets it itself, so the run is
        // marked as not having set it, and no stop or exit path here
        // touches it (`GUI_SETS_SYSTEM_PROXY`).
        let mut set_system_proxy = false;
        if pending.set_system_proxy && GUI_SETS_SYSTEM_PROXY {
            if let Launch::Helper { options, .. } = &pending.launch {
                if let Err(e) = proxy_marker::mark(&self.data_dir, options.proxy_port) {
                    eprintln!("Warning: the system proxy couldn't be noted: {e}");
                }
                match enable_system_proxy(options.proxy_port) {
                    Ok(()) => set_system_proxy = true,
                    Err(message) => {
                        proxy_marker::unmark(&self.data_dir);
                        cx.emit(StatusEvent {
                            level: StatusLevel::Warning,
                            message,
                        })
                    }
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
        None
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

    /// Stop the running sing-box. For a child, `signal_stop` only sends the
    /// signal (SIGTERM on Linux and macOS, kill on Windows), so it stays on
    /// the UI thread; the potentially slow reap (up to `STOP_GRACE` before a
    /// SIGKILL) and the system-proxy/TUN cleanup run on the background
    /// executor. For the helper, the background task sends `stop`, waits for
    /// `stopped`, closes the connection and clears the system proxy
    /// (`stop_helper_run`). The task is kept in `Stopped { cleanup }` so a
    /// subsequent `start()` can await it. A start not up yet is stopped at
    /// once too (`ProcessState::Preparing`).
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        match std::mem::replace(&mut self.state, ProcessState::Stopped { cleanup: None }) {
            ProcessState::Running {
                backend: RunBackend::Helper { connection, .. },
                running_set_system_proxy,
                drain,
                ..
            } => {
                drain.detach();
                let proxy_set_in = running_set_system_proxy.then(|| self.data_dir.clone());
                let cleanup = cx
                    .background_executor()
                    .spawn(async move { stop_helper_run(connection, proxy_set_in.as_deref()) });
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
            // Not up yet: its task becomes the cleanup the next start
            // awaits, and starts nothing once done; a helper start under way
            // ends now.
            ProcessState::Preparing { prep, cancel, .. } => {
                if let Some(cancel) = cancel {
                    cancel.cancel();
                }
                self.state = ProcessState::Stopped {
                    cleanup: Some(prep),
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
                    clear_helper_system_proxy(&self.data_dir);
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
            // A helper start ends with its connection; one that already
            // started stops with it (rule 6).
            ProcessState::Preparing {
                cancel: Some(cancel),
                ..
            } => cancel.cancel(),
            _ => {}
        }
    }
}

/// Stop a helper run and undo what BoxPilot set for it: ask the helper to
/// stop sing-box, wait for it up to `HELPER_STOP_TIMEOUT`, close the
/// connection, then, if this run set the user's system proxy (Windows only,
/// see `GUI_SETS_SYSTEM_PROXY`), clear it and its note in `proxy_set_in`
/// (`clear_helper_system_proxy`). The TUN adapter, and on macOS the system
/// proxy, are the helper's to clean up. Blocking.
fn stop_helper_run(connection: HelperConnection, proxy_set_in: Option<&Path>) {
    connection.stop(HELPER_STOP_TIMEOUT);
    if let Some(data_dir) = proxy_set_in {
        clear_helper_system_proxy(data_dir);
    }
}

/// Clear the system proxy a helper run set, while it still points at
/// BoxPilot's loopback proxy, then its note; a failure keeps the note, for
/// the next launch to try again (`proxy_marker`). Blocking.
fn clear_helper_system_proxy(data_dir: &Path) {
    match disable_system_proxy() {
        Ok(()) => proxy_marker::unmark(data_dir),
        Err(e) => eprintln!("Warning: {}", e),
    }
}

/// At launch, before any start: undo the system proxy a helper run left
/// set, if BoxPilot ended without clearing it, and it still points at that
/// run's port (`proxy_marker`). The helper stopped that sing-box when the
/// connection ended. A failure keeps the note. Blocking.
fn clear_left_system_proxy(data_dir: &Path) {
    let Some(port) = proxy_marker::marked_port(data_dir) else {
        return;
    };
    match disable_system_proxy_on(port) {
        Ok(()) => proxy_marker::unmark(data_dir),
        Err(e) => eprintln!("Warning: {}", e),
    }
}
