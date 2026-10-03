//! Live runtime status of the running sing-box: the up/down rate, memory,
//! connection counts and transfer totals, all from the one `SubscribeStatus`
//! stream of the sing-box API service — plus when it started and which
//! version it is (`GetStartedAt` / `GetVersion`, fetched once per run).
//! Owned by `AppState`; started on the process Stopped→Running edge and
//! stopped on the reverse edge (see the observer in `AppState::new`), the
//! same way `ProxyGroups` is driven.

use crate::core::singbox_api::{ApiError, RuntimeStatus, SingBoxApi};
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
/// running — covers the brief window before the sing-box API is listening
/// and any transient drop. Bounded by the `running` flag so it never spins.
const RECONNECT_DELAY: Duration = Duration::from_secs(1);
/// How many times (`RECONNECT_DELAY` apart) to ask for the start time and
/// version before giving up for this run. Both answer as soon as the API
/// listens, which is within a second or two of the process starting.
const INFO_ATTEMPTS: usize = 30;

/// Current runtime status. Session-scoped: zeroed when sing-box stops, never
/// persisted.
pub struct Traffic {
    /// Upload rate, bytes/sec, from the latest status sample
    /// (= `status.uplink`; the sidebar speed footer reads it).
    pub up: u64,
    /// Download rate, bytes/sec, from the latest status sample
    /// (= `status.downlink`).
    pub down: u64,
    /// The latest full status sample: memory, goroutines, connection counts,
    /// totals. All zero until the first sample and after a stop.
    pub status: RuntimeStatus,
    /// When the running sing-box started, unix milliseconds
    /// (`GetStartedAt`); the Home uptime ticks from it.
    pub started_at: Option<i64>,
    /// The running sing-box's own version (`GetVersion`).
    pub version: Option<String>,
    /// sing-box API 句柄(端口 + 本次运行的 secret)。`start()` 从它订阅
    /// `SubscribeStatus`;AppState 每次启动 sing-box 前经 `set_api` 换新句柄。
    api: SingBoxApi,
    /// Liveness flag for the current streaming session. Cleared by `stop()`
    /// and `Drop` so the detached reader thread self-terminates instead of
    /// outliving the session.
    running: Arc<AtomicBool>,
    /// UI-thread task draining samples into `up`/`down`. Dropping it cancels
    /// the task; the spawn closure's `WeakEntity` also stops it on entity drop.
    _drain: Option<Task<()>>,
    /// Fetches `started_at` + `version` once per run; dropped with the
    /// session like `_drain`.
    _info: Option<Task<()>>,
}

impl Traffic {
    pub fn new(api: SingBoxApi) -> Self {
        Self {
            up: 0,
            down: 0,
            status: RuntimeStatus::default(),
            started_at: None,
            version: None,
            api,
            running: Arc::new(AtomicBool::new(false)),
            _drain: None,
            _info: None,
        }
    }

    /// Swap in the API handle (port + secret) of the sing-box run about to
    /// start; AppState calls this before every start. The next `start()`
    /// streams from it.
    pub fn set_api(&mut self, api: SingBoxApi) {
        self.api = api;
    }

    /// Start streaming live status. Tears down any prior session first (clears
    /// the old flag, drops the old drain task) so a restart can't leave two
    /// reader threads racing onto one display.
    pub fn start(&mut self, cx: &mut Context<Self>) {
        // Signal a prior session's thread to exit before standing up a fresh
        // flag — the old thread reads the *old* Arc, so flipping it here and
        // replacing `self.running` cleanly separates the two sessions.
        self.running.store(false, Ordering::SeqCst);
        let running = Arc::new(AtomicBool::new(true));
        self.running = running.clone();
        self.reset();

        let (tx, rx) = mpsc::channel::<RuntimeStatus>();
        let api = self.api;

        // Dedicated blocking reader thread: gpui's executor is not built for
        // blocking stream reads (same reason the stdout/stderr pipe readers
        // use raw threads). It reconnects while `running` so it tolerates the
        // startup window before the API is up, and exits once the flag
        // clears or the receiver is gone (drain task dropped on stop()).
        thread::spawn(move || {
            while running.load(Ordering::SeqCst) {
                // Why a stream ended doesn't matter here: either sing-box is
                // going away (the edge observer stops us) or it isn't up yet.
                let _ = api.stream_status(|status| {
                    running.load(Ordering::SeqCst) && tx.send(status).is_ok()
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
                // newest sample is current (totals are cumulative, rates are
                // per-second, so nothing is lost by skipping one).
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
                            traffic.apply(sample);
                            cx.notify();
                        })
                        .is_err()
                    {
                        return;
                    }
                }

                if disconnected {
                    // Reader thread ended (process stopped or API gone). Zero
                    // the readout so it doesn't freeze on a stale value, then
                    // exit — a new session spawns a fresh drain task.
                    let _ = this.update(cx, |traffic, cx| {
                        if traffic.status != RuntimeStatus::default() {
                            traffic.apply(RuntimeStatus::default());
                            cx.notify();
                        }
                    });
                    return;
                }
            }
        });
        self._drain = Some(drain);

        // Start time + version: plain unary calls, retried until the API
        // answers. On the background executor like `ProxyGroups::select` —
        // each call is bounded by the transport's 2s unary timeout.
        let info = cx.spawn(async move |this, cx| {
            for _ in 0..INFO_ATTEMPTS {
                let result = cx
                    .background_executor()
                    .spawn(async move {
                        Ok::<_, ApiError>((api.get_started_at()?, api.get_version()?))
                    })
                    .await;
                if let Ok((started_at, version)) = result {
                    let _ = this.update(cx, |traffic, cx| {
                        traffic.started_at = started_at;
                        traffic.version = Some(version.version);
                        cx.notify();
                    });
                    return;
                }
                cx.background_executor().timer(RECONNECT_DELAY).await;
            }
        });
        self._info = Some(info);
        cx.notify();
    }

    fn apply(&mut self, status: RuntimeStatus) {
        self.up = status.uplink;
        self.down = status.downlink;
        self.status = status;
    }

    /// Back to the stopped state: no sample, no run facts.
    fn reset(&mut self) {
        self.apply(RuntimeStatus::default());
        self.started_at = None;
        self.version = None;
    }

    /// Stop streaming and clear the readout. The reader thread notices the
    /// cleared flag (or the broken connection when sing-box exits) and
    /// terminates on its own; dropping `_drain`/`_info` cancels the UI tasks.
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        self.running.store(false, Ordering::SeqCst);
        self._drain = None;
        self._info = None;
        self.reset();
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
