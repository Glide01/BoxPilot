use crate::core::process::{
    cleanup_after_process_stop, describe_exit, prepare_process_start, reap_child, signal_stop,
    start_sing_box, terminate_child,
};
#[cfg(target_os = "linux")]
use crate::core::privilege::{forget_sing_box_pid, record_sing_box_pid, stop_stale_sing_box};
use crate::core::settings::{StatusEvent, StatusLevel, SING_EXECUTABLE};
use crate::core::subscription::is_api_bind_failure;
use crate::state::drain::next_batch;
use crate::state::log_buffer::LogBuffer;
use gpui::{Context, Entity, EventEmitter, Task};
use std::path::PathBuf;
use std::process::Child;
use std::time::Duration;

/// Once a pipe line arrives, how long the drain waits for the rest of the
/// burst before handing it to `LogBuffer` in one go.
const LOG_COALESCE: Duration = Duration::from_millis(50);
const CHILD_WAIT_INTERVAL: Duration = Duration::from_millis(200);
/// How long a stop waits for sing-box to exit on SIGTERM before killing it
/// (Linux; Windows kills right away).
const STOP_GRACE: Duration = Duration::from_secs(3);

/// Snapshot of paths + mode flags captured when start is requested.
#[derive(Clone)]
pub struct PendingStart {
    pub sing_path: PathBuf,
    pub config_path: PathBuf,
    pub working_dir: PathBuf,
    pub proxy_mode: bool,
    pub set_system_proxy: bool,
    /// The port of BoxPilot's `api` service in this run's config.
    pub api_port: u16,
}

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
    /// Child process is alive. `drain` awaits the pipe channel and feeds
    /// `LogBuffer` (no wakeups while sing-box is quiet); `stop()` detaches it rather than cancelling it, so
    /// whatever sing-box wrote on its way out (a fatal error, a panic) still
    /// lands — it ends on its own when both pipes close. `_wait` polls
    /// `child.try_wait()` and transitions back to `Stopped` on exit. Both
    /// tasks exit cleanly when their entities drop.
    Running {
        child: Child,
        running_mode: bool,
        running_set_system_proxy: bool,
        drain: Task<()>,
        _wait: Task<()>,
    },
}

pub struct ProcessSession {
    pub state: ProcessState,
    pub logs: Entity<LogBuffer>,
    /// Where the running sing-box's pid file lives (its working dir), for
    /// the stop that reaps it to remove (`core::privilege`). Linux only.
    #[cfg(target_os = "linux")]
    pid_dir: Option<PathBuf>,
}

impl EventEmitter<StatusEvent> for ProcessSession {}
impl EventEmitter<ApiPortLost> for ProcessSession {}

impl ProcessSession {
    pub fn new(logs: Entity<LogBuffer>) -> Self {
        Self {
            state: ProcessState::Stopped { cleanup: None },
            logs,
            #[cfg(target_os = "linux")]
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

    /// Prep runs on the background executor before the child spawns —
    /// `prepare_process_start` does TUN-adapter cleanup and DNS flush, both of
    /// which must complete before sing-box opens its own TUN handle.
    pub fn start(&mut self, pending: PendingStart, cx: &mut Context<Self>) {
        let ProcessState::Stopped { cleanup } = &mut self.state else {
            return;
        };
        let prior_cleanup = cleanup.take();

        let after_prep = pending.clone();

        let prep_task = cx.spawn(async move |this, cx| {
            // A restart can get here while the previous child is still being
            // reaped on the background executor; await that cleanup so the
            // old instance's TUN adapter and system-proxy teardown finish
            // before we prep (and the new sing-box binds ports) on top of it.
            if let Some(prior_cleanup) = prior_cleanup {
                prior_cleanup.await;
            }

            let is_tun_mode = !pending.proxy_mode;
            #[cfg(target_os = "linux")]
            let working_dir = pending.working_dir.clone();
            cx.background_executor()
                .spawn(async move {
                    // A granted copy outlives a crashed BoxPilot (no
                    // PDEATHSIG across a file-caps exec) and would hold the
                    // TUN device and ports this start needs.
                    #[cfg(target_os = "linux")]
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
        });

        self.state = ProcessState::Preparing {
            _prep: prep_task,
            abandoned: false,
        };
        cx.notify();
    }

    fn spawn_child(&mut self, pending: PendingStart, cx: &mut Context<Self>) {
        match start_sing_box(&pending.sing_path, &pending.config_path, &pending.working_dir) {
            Ok((child, log_rx)) => {
                #[cfg(target_os = "linux")]
                {
                    record_sing_box_pid(&pending.working_dir, child.id());
                    self.pid_dir = Some(pending.working_dir.clone());
                }
                let weak_logs = self.logs.downgrade();
                let api_port = pending.api_port;
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
                            if let ProcessState::Running { child, .. } = &mut session.state {
                                match child.try_wait() {
                                    Ok(Some(status)) => Some(describe_exit(&status)),
                                    _ => None,
                                }
                            } else {
                                Some(crate::i18n::s().messages.sing_box_exited.to_string())
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
                    child,
                    running_mode: pending.proxy_mode,
                    running_set_system_proxy: pending.set_system_proxy,
                    drain,
                    _wait: wait,
                };
            }
            Err(e) => {
                self.state = ProcessState::Stopped { cleanup: None };
                cx.emit(StatusEvent {
                    level: StatusLevel::Error,
                    message: (crate::i18n::s().messages.start_failed)(
                        SING_EXECUTABLE,
                        &pending.config_path.display().to_string(),
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

    /// Stop the running child. `signal_stop` only sends the signal (SIGTERM
    /// on Linux, kill on Windows), so it stays on the UI thread; the
    /// potentially slow reap (up to `STOP_GRACE` before a SIGKILL on Linux)
    /// and the system-proxy/TUN cleanup run on the background executor. The
    /// task is kept in `Stopped { cleanup }` so a subsequent `start()` can
    /// await it.
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        match std::mem::replace(&mut self.state, ProcessState::Stopped { cleanup: None }) {
            ProcessState::Running {
                mut child,
                running_mode,
                running_set_system_proxy,
                drain,
                ..
            } => {
                let _ = signal_stop(&mut child);
                drain.detach();
                #[cfg(target_os = "linux")]
                let pid_dir = self.pid_dir.take();
                let cleanup = cx.background_executor().spawn(async move {
                    let mut child = child;
                    let _ = reap_child(&mut child, STOP_GRACE);
                    #[cfg(target_os = "linux")]
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
                child,
                running_mode,
                running_set_system_proxy,
                ..
            } => {
                let mut child = child;
                let _ = terminate_child(&mut child, STOP_GRACE);
                #[cfg(target_os = "linux")]
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
