//! Live connection list, streamed from the sing-box API service's
//! `SubscribeConnections` into a `ConnectionTable`. Owned by `AppState`;
//! started on the process Stopped→Running edge and cleared on the reverse
//! edge (see the observer in `AppState::new`), the same way `Traffic` and
//! `ProxyGroups` are driven.

use crate::core::settings::{StatusEvent, StatusLevel};
use crate::core::singbox_api::{ConnectionEvents, ConnectionTable, SingBoxApi};
use gpui::{Context, EventEmitter, Task};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// How often the UI-thread drain task applies queued batches. sing-box sends
/// rate updates once per `CONNECTIONS_INTERVAL` (1s) and opens/closes as
/// they happen; half a second keeps opens, closes and Close-button feedback
/// prompt without a render per batch.
const DRAIN_INTERVAL: Duration = Duration::from_millis(500);
/// While connections are open but the stream is quiet, still re-render this
/// often so their age column keeps counting.
const AGE_TICK: Duration = Duration::from_secs(1);
/// Delay before the reader thread re-subscribes after the stream fails while
/// still running — covers the window before the sing-box API is listening.
/// An idle-stream timeout re-subscribes at once instead. Bounded by the
/// `running` flag so it never spins.
const RECONNECT_DELAY: Duration = Duration::from_secs(1);

/// Every connection sing-box tracks — open ones plus the closed ones it
/// still remembers. Session-scoped: empty while sing-box is stopped, never
/// persisted.
pub struct Connections {
    /// Latest state, maintained from the event stream.
    pub table: ConnectionTable,
    /// Bumped on every change to `table`, so views can cache what they derive
    /// from it (filtered/sorted rows) between unrelated re-renders.
    pub revision: u64,
    /// Whether a streaming session is up (sing-box running). The page tells
    /// "not connected" from "no connections" with it.
    pub live: bool,
    /// sing-box API 句柄(端口 Settings 可配)。改端口经 `set_api` 换新句柄,
    /// 运行中由 AppState 重启 sing-box 才生效。
    api: SingBoxApi,
    /// Liveness flag for the current streaming session. Cleared by `stop()`
    /// and `Drop` so the detached reader thread self-terminates.
    running: Arc<AtomicBool>,
    /// UI-thread task draining batches into `table` — dropped (= cancelled)
    /// by `stop()`. `close`/`close_all` are fire-and-forget `.detach()`
    /// requests, bounded by the unary call timeout.
    _drain: Option<Task<()>>,
}

impl EventEmitter<StatusEvent> for Connections {}

impl Connections {
    pub fn new(api: SingBoxApi) -> Self {
        Self {
            table: ConnectionTable::new(),
            revision: 0,
            live: false,
            api,
            running: Arc::new(AtomicBool::new(false)),
            _drain: None,
        }
    }

    /// Swap the sing-box API handle after a Settings port change. The next
    /// `start()` streams from it; AppState restarts sing-box when running so
    /// a live session moves to the new port.
    pub fn set_api(&mut self, api: SingBoxApi) {
        self.api = api;
    }

    /// Subscribe to the connection stream (Stopped→Running edge). A dedicated
    /// reader thread holds `SubscribeConnections` and re-subscribes while
    /// running — an idle sing-box times the stream out routinely, and every
    /// new subscription opens with a fresh `reset` snapshot. A UI-thread
    /// task applies the batches.
    pub fn start(&mut self, cx: &mut Context<Self>) {
        // A prior session's thread reads the *old* Arc, so flipping it and
        // replacing `self.running` cleanly separates the two sessions.
        self.running.store(false, Ordering::SeqCst);
        let running = Arc::new(AtomicBool::new(true));
        self.running = running.clone();
        self.table.clear();
        self.revision += 1;
        self.live = true;

        let (tx, rx) = mpsc::channel::<ConnectionEvents>();
        let api = self.api;
        thread::spawn(move || {
            while running.load(Ordering::SeqCst) {
                let mut receiver_gone = false;
                let result = api.stream_connections(|batch| {
                    if !running.load(Ordering::SeqCst) {
                        return false;
                    }
                    receiver_gone = tx.send(batch).is_err();
                    !receiver_gone
                });
                if receiver_gone || !running.load(Ordering::SeqCst) {
                    break;
                }
                // Quiet stream: nothing changed for a while. Re-subscribe at
                // once; anything else (API not up yet, sing-box going away)
                // waits a beat — the edge observer stops us if it's gone.
                if !matches!(&result, Err(e) if e.is_timeout()) {
                    thread::sleep(RECONNECT_DELAY);
                }
            }
            // `tx` drops here → the drain task sees `Disconnected` and exits.
        });

        let drain = cx.spawn(async move |this, cx| {
            let mut last_notify = Instant::now();
            loop {
                cx.background_executor().timer(DRAIN_INTERVAL).await;

                let mut batches = Vec::new();
                let mut disconnected = false;
                loop {
                    match rx.try_recv() {
                        Ok(batch) => batches.push(batch),
                        Err(mpsc::TryRecvError::Empty) => break,
                        Err(mpsc::TryRecvError::Disconnected) => {
                            disconnected = true;
                            break;
                        }
                    }
                }

                let alive = this.update(cx, |state, cx| {
                    let changed = !batches.is_empty();
                    if changed {
                        // Batches apply in order; a `reset` mid-queue wipes
                        // what came before it, as it should.
                        for batch in batches {
                            state.table.apply(batch);
                        }
                        state.revision += 1;
                    }
                    let ages_due =
                        state.table.open_count() > 0 && last_notify.elapsed() >= AGE_TICK;
                    if changed || ages_due {
                        last_notify = Instant::now();
                        cx.notify();
                    }
                });
                if alive.is_err() || disconnected {
                    return;
                }
            }
        });
        self._drain = Some(drain);
        cx.notify();
    }

    /// End the stream and empty the list: entered on the Running→Stopped
    /// edge. Connections belong to one sing-box run.
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        self.running.store(false, Ordering::SeqCst);
        self._drain = None;
        self.table.clear();
        self.revision += 1;
        self.live = false;
        cx.notify();
    }

    /// Close one open connection. The row turns closed when sing-box's CLOSED
    /// event comes back on the stream; a failed request shows an error toast.
    pub fn close(&mut self, id: String, cx: &mut Context<Self>) {
        if !self.live {
            return;
        }
        let api = self.api;
        self.request(
            move || {
                api.close_connection(&id)
                    .map_err(|e| format!("Failed to close connection: {}", e))
            },
            cx,
        );
    }

    /// Close every open connection (the closed list stays). Rows update from
    /// the stream; a failed request shows an error toast.
    pub fn close_all(&mut self, cx: &mut Context<Self>) {
        if !self.live {
            return;
        }
        let api = self.api;
        self.request(
            move || {
                api.close_all_connections()
                    .map_err(|e| format!("Failed to close connections: {}", e))
            },
            cx,
        );
    }

    /// Run a blocking unary call off the UI thread; toast its error.
    fn request(
        &mut self,
        call: impl FnOnce() -> Result<(), String> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { call() }).await;
            if let Err(message) = result {
                let _ = this.update(cx, |_, cx| {
                    cx.emit(StatusEvent {
                        level: StatusLevel::Error,
                        message,
                    });
                });
            }
        })
        .detach();
    }
}

impl Drop for Connections {
    fn drop(&mut self) {
        // Let the detached reader thread exit at its next batch or reconnect
        // check once the entity is gone (e.g. on app quit).
        self.running.store(false, Ordering::SeqCst);
    }
}
