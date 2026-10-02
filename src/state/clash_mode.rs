//! Clash mode of the running sing-box: the selector that `clash_mode`
//! route/DNS rules match on. The modes come from the running config
//! (`GetClashModeStatus`), the current one is followed live
//! (`SubscribeClashMode`), and the Home switcher changes it
//! (`SetClashMode`). Owned by `AppState`; started on the process
//! Stopped→Running edge and cleared on the reverse edge, the same way
//! `ProxyGroups` and `Traffic` are driven.
//!
//! Nothing is persisted here: sing-box itself stores the chosen mode in
//! `cache_file` (which BoxPilot always enables) and restores it on the next
//! start when the new config still has that mode.

use crate::core::settings::{StatusEvent, StatusLevel};
use crate::core::singbox_api::{grpc_code, mode_index, ClashModeStatus, SingBoxApi};
use gpui::{Context, EventEmitter, Task};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// How often the UI-thread task applies what the reader thread queued. Mode
/// pushes are rare (one per switch), so this only bounds the echo latency.
const DRAIN_INTERVAL: Duration = Duration::from_millis(250);
/// Delay before the reader thread retries after a failed call while still
/// running — covers the window before the sing-box API is listening.
/// Bounded by the `running` flag.
const RECONNECT_DELAY: Duration = Duration::from_secs(1);

/// What the reader thread hands the UI thread.
enum ModeEvent {
    /// The mode list and the current mode, once per run.
    Status(ClashModeStatus),
    /// The current mode, on subscribe and on every change.
    Current(String),
}

pub struct ClashMode {
    /// Selectable modes in sing-box's order; empty while stopped.
    pub modes: Vec<String>,
    /// The current mode, spelled as in `modes`; empty while stopped.
    pub current: String,
    /// sing-box API 句柄(端口 Settings 可配);改端口经 `set_api` 换新句柄。
    api: SingBoxApi,
    /// Liveness flag for the current session's reader thread. Cleared by
    /// `clear()` and `Drop` so the detached thread self-terminates.
    running: Arc<AtomicBool>,
    /// UI-thread drain task; dropped (= cancelled) by `clear()`.
    _task: Option<Task<()>>,
}

impl EventEmitter<StatusEvent> for ClashMode {}

impl ClashMode {
    pub fn new(api: SingBoxApi) -> Self {
        Self {
            modes: Vec::new(),
            current: String::new(),
            api,
            running: Arc::new(AtomicBool::new(false)),
            _task: None,
        }
    }

    /// Swap the sing-box API handle after a Settings port change. AppState
    /// restarts sing-box when running, and the edges re-drive the stream.
    pub fn set_api(&mut self, api: SingBoxApi) {
        self.api = api;
    }

    /// Whether the Home switcher has anything to offer: running, with two or
    /// more modes.
    pub fn is_switchable(&self) -> bool {
        crate::core::singbox_api::is_switchable(&self.modes)
    }

    /// Index of the current mode in `modes`, for the segmented control.
    pub fn current_index(&self) -> Option<usize> {
        mode_index(&self.modes, &self.current)
    }

    /// Load the modes and follow the current one (Stopped→Running edge). A
    /// dedicated reader thread fetches the list (retrying until the API is
    /// up), then holds `SubscribeClashMode`, re-subscribing on the routine
    /// idle timeout.
    pub fn start(&mut self, cx: &mut Context<Self>) {
        // A prior session's thread reads the *old* Arc, so flipping it and
        // replacing `self.running` cleanly separates the two sessions.
        self.running.store(false, Ordering::SeqCst);
        let running = Arc::new(AtomicBool::new(true));
        self.running = running.clone();
        self.modes.clear();
        self.current.clear();

        let (tx, rx) = mpsc::channel::<ModeEvent>();
        let api = self.api;
        thread::spawn(move || {
            let mut loaded = false;
            while running.load(Ordering::SeqCst) {
                if !loaded {
                    match api.get_clash_mode_status() {
                        Ok(status) => {
                            if tx.send(ModeEvent::Status(status)).is_err() {
                                break;
                            }
                            loaded = true;
                        }
                        // sing-box runs no clash mode manager: nothing to
                        // follow for this run. (Never the case with the
                        // `api` service, which always creates one.)
                        Err(e) if e.code() == Some(grpc_code::NOT_FOUND) => break,
                        Err(_) => {
                            thread::sleep(RECONNECT_DELAY);
                            continue;
                        }
                    }
                }
                let result = api.stream_clash_mode(|mode| {
                    running.load(Ordering::SeqCst) && tx.send(ModeEvent::Current(mode)).is_ok()
                });
                if !running.load(Ordering::SeqCst) {
                    break;
                }
                // The stream is silent between switches, so the idle read
                // timeout is routine: re-subscribe at once. Anything else
                // (sing-box going away, API not up) backs off first.
                if !matches!(&result, Err(e) if e.is_timeout()) {
                    thread::sleep(RECONNECT_DELAY);
                }
            }
        });

        let task = cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(DRAIN_INTERVAL).await;

            let mut events = Vec::new();
            let mut disconnected = false;
            loop {
                match rx.try_recv() {
                    Ok(event) => events.push(event),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }

            if !events.is_empty() {
                let alive = this.update(cx, |state, cx| {
                    for event in events {
                        state.apply(event);
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    return;
                }
            }
            if disconnected {
                return;
            }
        });
        self._task = Some(task);
        cx.notify();
    }

    fn apply(&mut self, event: ModeEvent) {
        match event {
            ModeEvent::Status(status) => {
                self.modes = status.modes;
                self.current = status.current;
            }
            // An empty or unknown mode (the box not started) is no news.
            ModeEvent::Current(mode) => {
                if let Some(ix) = mode_index(&self.modes, &mode) {
                    self.current = self.modes[ix].clone();
                }
            }
        }
    }

    /// Empty the switcher and end the stream (Running→Stopped edge).
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.running.store(false, Ordering::SeqCst);
        self._task = None;
        self.modes.clear();
        self.current.clear();
        cx.notify();
    }

    /// Optimistically switch to `mode`, then tell sing-box. On failure: error
    /// toast + revert. Success needs no follow-up — the stream echoes the
    /// new mode, and sing-box persists it in `cache_file` itself.
    pub fn select(&mut self, mode: String, cx: &mut Context<Self>) {
        let Some(ix) = mode_index(&self.modes, &mode) else {
            return;
        };
        let mode = self.modes[ix].clone();
        if self.current == mode {
            return;
        }
        let previous = std::mem::replace(&mut self.current, mode.clone());
        cx.notify();

        let api = self.api;
        cx.spawn(async move |this, cx| {
            let request = mode.clone();
            let result = cx
                .background_executor()
                .spawn(async move { api.set_clash_mode(&request) })
                .await;
            if let Err(e) = result {
                let _ = this.update(cx, |state, cx| {
                    if state.current == mode {
                        state.current = previous;
                    }
                    cx.emit(StatusEvent {
                        level: StatusLevel::Error,
                        message: format!("Failed to switch clash mode: {}", e),
                    });
                    cx.notify();
                });
            }
        })
        .detach();
    }
}

impl Drop for ClashMode {
    fn drop(&mut self) {
        // Let the detached reader thread exit at its next push or retry once
        // the entity is gone (e.g. on app quit).
        self.running.store(false, Ordering::SeqCst);
    }
}
