//! The Logs page's lines. Two feeds go in: sing-box's stdout/stderr (pushed
//! by `ProcessSession`'s pipe drain) and the sing-box API's `SubscribeLog`
//! stream, held while sing-box runs — started and stopped on the process
//! Running/Stopped edges (see the observer in `AppState::new`), the same way
//! `Traffic` is driven. `core::log_merge` decides what is shown.

use crate::core::log_merge::{visible, LogEntry, LogMerge};
use crate::core::settings::MAX_LOG_LINES;
use crate::core::singbox_api::{LogBatch, LogLevel, SingBoxApi};
use gpui::{Context, Task};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// How often the UI-thread task applies queued API batches and advances the
/// merge clock. One render per tick at most, however busy the stream.
const DRAIN_INTERVAL: Duration = Duration::from_millis(100);
/// Delay before the reader thread re-subscribes after the stream ends while
/// still running — covers the window before the sing-box API is listening,
/// and the routine idle read timeout. Bounded by the `running` flag.
const RECONNECT_DELAY: Duration = Duration::from_secs(1);
/// The level shown before sing-box has reported its own: sing-box's usual
/// `log.level`.
const INITIAL_DEFAULT_LEVEL: LogLevel = LogLevel::Info;

/// What the reader thread hands the UI thread.
enum ApiEvent {
    DefaultLevel(LogLevel),
    Batch(LogBatch),
}

/// Mutated only via the public methods on this type — every mutator that
/// changes what is shown emits `cx.notify()` so observing views re-render.
pub struct LogBuffer {
    merge: LogMerge,
    /// sing-box's configured `log.level`, from the API (`Trace` when the
    /// config sets none). Kept after sing-box stops.
    default_level: LogLevel,
    /// The user's pick on the level control; `None` follows `default_level`.
    level_override: Option<LogLevel>,
    /// sing-box API 句柄(端口 + 本次运行的 secret)。AppState 每次启动
    /// sing-box 前经 `set_api` 换新句柄。
    api: SingBoxApi,
    /// Liveness flag for the current API stream. Cleared by `stop_api()` and
    /// `Drop` so the detached reader thread self-terminates.
    running: Arc<AtomicBool>,
    /// Queued API events; drained by `_drain` every tick and once more by
    /// `stop_api()`, so the last lines before an exit aren't lost.
    api_rx: Option<mpsc::Receiver<ApiEvent>>,
    /// UI-thread task ticking the drain. Dropped (= cancelled) by `stop_api()`.
    _drain: Option<Task<()>>,
}

impl LogBuffer {
    pub fn new(api: SingBoxApi) -> Self {
        let mut merge = LogMerge::new(MAX_LOG_LINES);
        merge.set_keep_level(INITIAL_DEFAULT_LEVEL);
        Self {
            merge,
            default_level: INITIAL_DEFAULT_LEVEL,
            level_override: None,
            api,
            running: Arc::new(AtomicBool::new(false)),
            api_rx: None,
            _drain: None,
        }
    }

    /// Oldest first, every level; filter with `threshold()`.
    pub fn entries(&self) -> &VecDeque<LogEntry> {
        self.merge.entries()
    }

    /// Lines at this level or more severe are shown.
    pub fn threshold(&self) -> LogLevel {
        self.level_override
            .unwrap_or_else(|| self.default_threshold())
    }

    /// sing-box's configured level, as the level control offers it: `panic`
    /// and `fatal` widen to `Error`, the most severe choice it has.
    pub fn default_threshold(&self) -> LogLevel {
        self.default_level.max(LogLevel::Error)
    }

    pub fn visible_count(&self) -> usize {
        visible(self.merge.entries(), self.threshold()).count()
    }

    /// The level control. Picking the configured level goes back to
    /// following it (a later run with another config moves it along).
    pub fn set_threshold(&mut self, level: LogLevel, cx: &mut Context<Self>) {
        let level_override = (level != self.default_threshold()).then_some(level);
        if self.level_override != level_override {
            self.level_override = level_override;
            self.merge.set_keep_level(self.threshold());
            cx.notify();
        }
    }

    /// Swap in the API handle (port + secret) of the sing-box run about to
    /// start; AppState calls this before every start. The next `start_api()`
    /// streams from it.
    pub fn set_api(&mut self, api: SingBoxApi) {
        self.api = api;
    }

    /// Lines from sing-box's stdout/stderr, raw, oldest first.
    pub fn push_pipe(&mut self, lines: Vec<String>, cx: &mut Context<Self>) {
        if self.merge.push_pipe(&lines, Instant::now()) {
            cx.notify();
        }
    }

    /// Empty the view. sing-box's own buffer is `AppState::clear_logs`'s
    /// business.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        if self.merge.clear() {
            cx.notify();
        }
    }

    /// Stopped→Running edge: a new run starts, and a dedicated reader thread
    /// holds the `SubscribeLog` stream, re-subscribing while running (before
    /// the API is up, and after each idle timeout).
    pub fn start_api(&mut self, cx: &mut Context<Self>) {
        // A prior session's thread reads the *old* Arc, so flipping it and
        // replacing `self.running` cleanly separates the two sessions.
        self.running.store(false, Ordering::SeqCst);
        let running = Arc::new(AtomicBool::new(true));
        self.running = running.clone();
        self.merge.begin_run();

        let (tx, rx) = mpsc::channel::<ApiEvent>();
        self.api_rx = Some(rx);
        let api = self.api;
        thread::spawn(move || {
            let mut level_known = false;
            while running.load(Ordering::SeqCst) {
                if !level_known {
                    if let Ok(level) = api.get_default_log_level() {
                        level_known = true;
                        if tx.send(ApiEvent::DefaultLevel(level)).is_err() {
                            break;
                        }
                    }
                }
                // Why a stream ended doesn't matter: sing-box is either not
                // up yet, idle (re-subscribe; the snapshot replaces what we
                // have), or going away (the edge observer stops us).
                let _ = api.stream_logs(|batch| {
                    running.load(Ordering::SeqCst) && tx.send(ApiEvent::Batch(batch)).is_ok()
                });
                if !running.load(Ordering::SeqCst) {
                    break;
                }
                thread::sleep(RECONNECT_DELAY);
            }
        });

        let drain = cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(DRAIN_INTERVAL).await;
            if this.update(cx, |logs, cx| logs.drain_api(cx)).is_err() {
                return;
            }
        });
        self._drain = Some(drain);
    }

    /// Running→Stopped edge: apply what the stream delivered, end it, and
    /// let held-back pipe lines through (stderr after exit shows as it
    /// arrives).
    pub fn stop_api(&mut self, cx: &mut Context<Self>) {
        self.drain_api(cx);
        self.running.store(false, Ordering::SeqCst);
        self._drain = None;
        self.api_rx = None;
        if self.merge.end_api() {
            cx.notify();
        }
    }

    fn drain_api(&mut self, cx: &mut Context<Self>) {
        let Some(rx) = &self.api_rx else {
            return;
        };
        let events: Vec<ApiEvent> = rx.try_iter().collect();
        let now = Instant::now();
        let mut changed = false;
        for event in events {
            match event {
                ApiEvent::DefaultLevel(level) => {
                    if self.default_level != level {
                        self.default_level = level;
                        // An override equal to the new default is no override.
                        if self.level_override == Some(self.default_threshold()) {
                            self.level_override = None;
                        }
                        self.merge.set_keep_level(self.threshold());
                        changed = true;
                    }
                }
                ApiEvent::Batch(batch) => changed |= self.merge.push_api(batch, now),
            }
        }
        changed |= self.merge.tick(now);
        if changed {
            cx.notify();
        }
    }
}

impl Drop for LogBuffer {
    fn drop(&mut self) {
        // Let the detached reader thread exit at its next batch or reconnect
        // check once the entity is gone (e.g. on app quit).
        self.running.store(false, Ordering::SeqCst);
    }
}
