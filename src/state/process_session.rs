use crate::core::process::{
    cleanup_after_process_stop, prepare_process_start, start_sing_box,
};
use crate::core::settings::{StatusEvent, StatusLevel, SING_EXECUTABLE};
use crate::state::log_buffer::LogBuffer;
use gpui::{Context, Entity, EventEmitter, Task};
use std::path::PathBuf;
use std::process::Child;
use std::sync::mpsc;
use std::time::Duration;

const LOG_DRAIN_INTERVAL: Duration = Duration::from_millis(50);
const CHILD_WAIT_INTERVAL: Duration = Duration::from_millis(200);

/// Snapshot of paths + mode flags captured when start is requested.
#[derive(Clone)]
pub struct PendingStart {
    pub sing_path: PathBuf,
    pub config_path: PathBuf,
    pub working_dir: PathBuf,
    pub proxy_mode: bool,
    pub set_system_proxy: bool,
}

pub enum ProcessState {
    /// No child process. `cleanup` holds the previous run's background
    /// reap + TUN/system-proxy teardown while it is still in flight;
    /// `start()` awaits it before prepping so a restart never races the
    /// old instance's cleanup.
    Stopped { cleanup: Option<Task<()>> },
    /// Background task running `prepare_process_start` (TUN cleanup + DNS
    /// flush). Dropping `_prep` cancels the task.
    Preparing { _prep: Task<()> },
    /// Child process is alive. `_drain` reads from the log channel into
    /// `LogBuffer`. `_wait` polls `child.try_wait()` and transitions back to
    /// `Stopped` on exit. Both tasks exit cleanly when the entity drops.
    Running {
        child: Child,
        running_mode: bool,
        running_set_system_proxy: bool,
        _drain: Task<()>,
        _wait: Task<()>,
    },
}

pub struct ProcessSession {
    pub state: ProcessState,
    pub logs: Entity<LogBuffer>,
}

impl EventEmitter<StatusEvent> for ProcessSession {}

impl ProcessSession {
    pub fn new(logs: Entity<LogBuffer>) -> Self {
        Self {
            state: ProcessState::Stopped { cleanup: None },
            logs,
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
            cx.background_executor()
                .spawn(async move { prepare_process_start(is_tun_mode) })
                .await;

            let _ = this.update(cx, |session, cx| {
                session.spawn_child(after_prep, cx);
            });
        });

        self.state = ProcessState::Preparing { _prep: prep_task };
        cx.notify();
    }

    fn spawn_child(&mut self, pending: PendingStart, cx: &mut Context<Self>) {
        match start_sing_box(&pending.sing_path, &pending.config_path, &pending.working_dir) {
            Ok((child, log_rx)) => {
                let weak_logs = self.logs.downgrade();
                let drain = cx.spawn(async move |_this, cx| {
                    loop {
                        cx.background_executor().timer(LOG_DRAIN_INTERVAL).await;
                        let mut batch = Vec::new();
                        let mut disconnected = false;
                        loop {
                            match log_rx.try_recv() {
                                Ok(entry) => batch.push(entry),
                                Err(mpsc::TryRecvError::Empty) => break,
                                Err(mpsc::TryRecvError::Disconnected) => {
                                    disconnected = true;
                                    break;
                                }
                            }
                        }
                        if !batch.is_empty() {
                            if weak_logs
                                .update(cx, |logs, cx| logs.extend(batch, cx))
                                .is_err()
                            {
                                return;
                            }
                        }
                        if disconnected {
                            return;
                        }
                    }
                });

                let wait = cx.spawn(async move |this, cx| {
                    loop {
                        cx.background_executor().timer(CHILD_WAIT_INTERVAL).await;
                        let exited = this.update(cx, |session, _cx| {
                            if let ProcessState::Running { child, .. } = &mut session.state {
                                matches!(child.try_wait(), Ok(Some(_)))
                            } else {
                                true
                            }
                        });
                        match exited {
                            Ok(true) => {
                                let _ = this.update(cx, |session, cx| {
                                    cx.emit(StatusEvent {
                                        level: StatusLevel::Warning,
                                        message: format!("{} exited.", SING_EXECUTABLE),
                                    });
                                    session.stop(cx);
                                });
                                return;
                            }
                            Ok(false) => continue,
                            Err(_) => return,
                        }
                    }
                });

                self.state = ProcessState::Running {
                    child,
                    running_mode: pending.proxy_mode,
                    running_set_system_proxy: pending.set_system_proxy,
                    _drain: drain,
                    _wait: wait,
                };
            }
            Err(e) => {
                self.state = ProcessState::Stopped { cleanup: None };
                cx.emit(StatusEvent {
                    level: StatusLevel::Error,
                    message: format!(
                        "Failed to start {} with config {}: {}",
                        SING_EXECUTABLE,
                        pending.config_path.display(),
                        e
                    ),
                });
            }
        }
        cx.notify();
    }

    /// Stop the running child. `kill()` only sends the termination signal, so
    /// it stays on the UI thread; the potentially slow reap (`wait()`) and the
    /// system-proxy/TUN cleanup run on the background executor. The task is
    /// kept in `Stopped { cleanup }` so a subsequent `start()` can await it.
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        match std::mem::replace(&mut self.state, ProcessState::Stopped { cleanup: None }) {
            ProcessState::Running {
                mut child,
                running_mode,
                running_set_system_proxy,
                ..
            } => {
                let _ = child.kill();
                let cleanup = cx.background_executor().spawn(async move {
                    let mut child = child;
                    let _ = child.wait();
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
            // Preparing: dropping `_prep` cancels the pending start.
            ProcessState::Preparing { .. } => {}
        }
        cx.notify();
    }
}

impl Drop for ProcessSession {
    fn drop(&mut self) {
        // Defensive: if dropped without `stop()` (e.g. on app quit before
        // `cx.on_app_quit` runs), still kill the child so the pipe-reader
        // threads exit cleanly. Cleanup runs synchronously since we have no
        // executor handle here.
        match std::mem::replace(&mut self.state, ProcessState::Stopped { cleanup: None }) {
            ProcessState::Running {
                child,
                running_mode,
                running_set_system_proxy,
                ..
            } => {
                let mut child = child;
                let _ = child.kill();
                let _ = child.wait();
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
