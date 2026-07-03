//! Live up/down network rate, streamed from sing-box's Clash API `/traffic`
//! endpoint. Owned by `AppState`; started on the process Stopped→Running edge
//! and stopped on the reverse edge (see the observer in `AppState::new`), the
//! same way `ProxyGroups` is driven.

use crate::core::clash_api::{ClashApi, TrafficSample};
use gpui::{Context, Task};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// How often the UI-thread drain task coalesces queued samples into the
/// displayed rate. Samples arrive ~1/sec; a sub-second tick keeps the readout
/// responsive without per-sample render churn (mirrors the log-drain pattern).
const DRAIN_INTERVAL: Duration = Duration::from_millis(500);
/// Delay before the reader thread reconnects after a stream ends while still
/// running — covers the brief window before sing-box's Clash API is listening
/// and any transient drop. Bounded by the `running` flag so it never spins.
const RECONNECT_DELAY: Duration = Duration::from_secs(1);

/// Current network rate in bytes/sec. Session-scoped: zeroed when sing-box
/// stops, never persisted.
pub struct Traffic {
    /// Upload rate, bytes/sec, from the latest `/traffic` sample.
    pub up: u64,
    /// Download rate, bytes/sec, from the latest `/traffic` sample.
    pub down: u64,
    /// Clash API 句柄(端口 Settings 可配)。`start()` streams `/traffic` from
    /// it;改端口经 `set_api` 换新句柄,运行中由 AppState 重启才生效。
    api: ClashApi,
    /// Liveness flag for the current streaming session. Cleared by `stop()`
    /// and `Drop` so the detached reader thread self-terminates instead of
    /// outliving the session.
    running: Arc<AtomicBool>,
    /// UI-thread task draining samples into `up`/`down`. Dropping it cancels
    /// the task; the spawn closure's `WeakEntity` also stops it on entity drop.
    _drain: Option<Task<()>>,
}

impl Traffic {
    pub fn new(api: ClashApi) -> Self {
        Self {
            up: 0,
            down: 0,
            api,
            running: Arc::new(AtomicBool::new(false)),
            _drain: None,
        }
    }

    /// Swap the Clash API handle after a Settings port change. The next
    /// `start()` streams from it; AppState restarts sing-box when running so
    /// a live session moves to the new port.
    pub fn set_api(&mut self, api: ClashApi) {
        self.api = api;
    }

    /// Start streaming live rates. Tears down any prior session first (clears
    /// the old flag, drops the old drain task) so a restart can't leave two
    /// reader threads racing onto one display.
    pub fn start(&mut self, cx: &mut Context<Self>) {
        // Signal a prior session's thread to exit before standing up a fresh
        // flag — the old thread reads the *old* Arc, so flipping it here and
        // replacing `self.running` cleanly separates the two sessions.
        self.running.store(false, Ordering::SeqCst);
        let running = Arc::new(AtomicBool::new(true));
        self.running = running.clone();
        self.up = 0;
        self.down = 0;

        let (tx, rx) = mpsc::channel::<TrafficSample>();
        let api = self.api;

        // Dedicated blocking reader thread: gpui's executor is not built for
        // blocking stream reads (same reason the stdout/stderr pipe readers
        // use raw threads). It reconnects while `running` so it tolerates the
        // startup window before the Clash API is up, and exits once the flag
        // clears or the receiver is gone (drain task dropped on stop()).
        thread::spawn(move || {
            while running.load(Ordering::SeqCst) {
                api.stream_traffic(|sample| {
                    running.load(Ordering::SeqCst) && tx.send(sample).is_ok()
                });
                if !running.load(Ordering::SeqCst) {
                    break;
                }
                thread::sleep(RECONNECT_DELAY);
            }
            // `tx` drops here → the drain task sees `Disconnected` and zeroes.
        });

        let drain = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(DRAIN_INTERVAL).await;

                // Coalesce everything queued since the last tick; only the
                // newest sample is the current rate.
                let mut latest = None;
                let mut disconnected = false;
                loop {
                    match rx.try_recv() {
                        Ok(sample) => latest = Some(sample),
                        Err(mpsc::TryRecvError::Empty) => break,
                        Err(mpsc::TryRecvError::Disconnected) => {
                            disconnected = true;
                            break;
                        }
                    }
                }

                if let Some(sample) = latest {
                    if this
                        .update(cx, |traffic, cx| {
                            traffic.up = sample.up;
                            traffic.down = sample.down;
                            cx.notify();
                        })
                        .is_err()
                    {
                        return;
                    }
                }

                if disconnected {
                    // Reader thread ended (process stopped or API gone). Zero
                    // the rates so the readout doesn't freeze on a stale value,
                    // then exit — a new session spawns a fresh drain task.
                    let _ = this.update(cx, |traffic, cx| {
                        if traffic.up != 0 || traffic.down != 0 {
                            traffic.up = 0;
                            traffic.down = 0;
                            cx.notify();
                        }
                    });
                    return;
                }
            }
        });
        self._drain = Some(drain);
        cx.notify();
    }

    /// Stop streaming and clear the displayed rates. The reader thread notices
    /// the cleared flag (or the broken connection when sing-box exits) and
    /// terminates on its own; dropping `_drain` cancels the UI task.
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        self.running.store(false, Ordering::SeqCst);
        self._drain = None;
        self.up = 0;
        self.down = 0;
        cx.notify();
    }
}

impl Drop for Traffic {
    fn drop(&mut self) {
        // Make the detached reader thread's teardown deterministic instead of
        // relying on sing-box's connection breaking — clear the flag so it
        // exits at its next sample (or reconnect check) once the entity is
        // gone (e.g. on app quit).
        self.running.store(false, Ordering::SeqCst);
    }
}
