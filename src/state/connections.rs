//! Live connection list, streamed from the sing-box API service's
//! `SubscribeConnections` into a `ConnectionTable`. Owned by `AppState`;
//! started once the run is Ready (its API answered) and cleared when it
//! stops (`AppState::sync_run_phase`), the same way `Traffic` and
//! `ProxyGroups` are driven.

use crate::core::settings::{StatusEvent, StatusLevel};
use crate::core::singbox_api::{ConnectionEvents, ConnectionTable, SingBoxApi};
use crate::state::drain::{next_batch_or, Wake};
use futures_channel::mpsc;
use gpui::{Context, EventEmitter, Task};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// Once a batch arrives, how long the drain task lets the rest of a burst
/// queue up before applying it. sing-box sends rate updates once per
/// `CONNECTIONS_INTERVAL` (1s) and opens/closes as they happen; this keeps
/// opens, closes and Close-button feedback prompt without a render per batch.
const COALESCE: Duration = Duration::from_millis(250);
/// While connections are open but the stream is quiet, still re-render this
/// often so their age column keeps counting. No clock runs with none open.
const AGE_TICK: Duration = Duration::from_secs(1);
/// Delay before the reader thread re-subscribes after the stream fails while
/// still running — covers a transient drop.
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
    /// sing-box API 句柄(端口 + 本次运行的 secret)。AppState 每次启动
    /// sing-box 前经 `set_api` 换新句柄。
    api: SingBoxApi,
    /// Liveness flag for the current streaming session. Cleared by `stop()`
    /// and `Drop` so the detached reader thread self-terminates.
    running: Arc<AtomicBool>,
    /// UI-thread task applying batches to `table` as they arrive — dropped (= cancelled)
    /// by `stop()`. `close`/`close_many`/`close_all` are fire-and-forget
    /// `.detach()` requests, bounded by the unary call timeout.
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

    /// Swap in the API handle (port + secret) of the sing-box run about to
    /// start; AppState calls this before every start. The next `start()`
    /// streams from it.
    pub fn set_api(&mut self, api: SingBoxApi) {
        self.api = api;
    }

    /// Subscribe to the connection stream (Ready edge). A dedicated
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

        let (tx, mut rx) = mpsc::unbounded::<ConnectionEvents>();
        let api = self.api;
        thread::spawn(move || {
            while running.load(Ordering::SeqCst) {
                let mut receiver_gone = false;
                let result = api.stream_connections(|batch| {
                    if !running.load(Ordering::SeqCst) {
                        return false;
                    }
                    receiver_gone = tx.unbounded_send(batch).is_err();
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
            // `tx` drops here → the drain task sees the close and exits.
        });

        let drain = cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            let mut last_notify = Instant::now();
            // Open connections: the age column needs a clock.
            let mut ticking = false;
            loop {
                let clock =
                    ticking.then(|| executor.timer(AGE_TICK.saturating_sub(last_notify.elapsed())));
                let batches = match next_batch_or(&mut rx, clock, || executor.timer(COALESCE)).await
                {
                    Wake::Batch(batches) => batches,
                    Wake::Timer => Vec::new(),
                    Wake::Closed => return,
                };

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
                    state.table.open_count() > 0
                });
                match alive {
                    Ok(open) => ticking = open,
                    Err(_) => return,
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
                api.close_connection(&id).map_err(|e| {
                    (crate::i18n::s().messages.close_connection_failed)(&e.to_string())
                })
            },
            cx,
        );
    }

    /// Close the open connections `ids` (Close all under a filter): one
    /// background task closes them in turn, since the sing-box API closes
    /// one id per call. Rows update from the stream. A failed call (an
    /// already-closed id is not one) stops the rest — the API is likely
    /// gone — and shows an error toast.
    pub fn close_many(&mut self, ids: Vec<String>, cx: &mut Context<Self>) {
        if !self.live || ids.is_empty() {
            return;
        }
        let api = self.api;
        self.request(
            move || {
                ids.iter()
                    .try_for_each(|id| api.close_connection(id))
                    .map_err(|e| {
                        (crate::i18n::s().messages.close_connections_failed)(&e.to_string())
                    })
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
                api.close_all_connections().map_err(|e| {
                    (crate::i18n::s().messages.close_connections_failed)(&e.to_string())
                })
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
