//! Live runtime status of the running sing-box: the up/down rate, memory,
//! connection counts and transfer totals, all from the one `SubscribeStatus`
//! stream of the sing-box API service — plus when it started and which
//! version it is (`GetStartedAt` / `GetVersion`, fetched once per run).
//! Owned by `AppState`; started once the run is Ready (its API answered)
//! and stopped when it stops (`AppState::sync_run_phase`), the same way `ProxyGroups` is driven.

use crate::core::singbox_api::{ApiError, RuntimeStatus, SingBoxApi};
use crate::state::drain::next_batch;
use futures_channel::mpsc;
use gpui::{Context, Task};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// Once a sample arrives, how long the drain task waits for stragglers
/// before applying. Samples come ~1/sec, so this is almost always a batch of
/// one; it only folds a backlog (e.g. after a stall) into a single render.
const COALESCE: Duration = Duration::from_millis(50);
/// Delay before the reader thread reconnects after a stream ends while still
/// running — covers any transient drop. Bounded by the `running` flag so it never spins.
const RECONNECT_DELAY: Duration = Duration::from_secs(1);
/// How many times (`RECONNECT_DELAY` apart) to ask for the start time and
/// version before giving up for this run. Both answer as soon as the API
/// listens, which is within a second or two of the process starting.
const INFO_ATTEMPTS: usize = 30;
/// How many samples the rate history keeps: sing-box reports once a second,
/// so this is the Home traffic chart's two-minute window.
pub const HISTORY_LEN: usize = 120;

/// One status sample's rates, bytes/sec, as the Home traffic chart plots them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RatePoint {
    pub up: u64,
    pub down: u64,
}

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
    /// The last [`HISTORY_LEN`] samples' rates, oldest first. Every sample
    /// lands here (see `ingest`); cleared with the rest of the readout when a
    /// run starts or stops.
    history: VecDeque<RatePoint>,
    /// sing-box API 句柄(端口 + 本次运行的 secret)。`start()` 从它订阅
    /// `SubscribeStatus`;AppState 每次启动 sing-box 前经 `set_api` 换新句柄。
    api: SingBoxApi,
    /// Liveness flag for the current streaming session. Cleared by `stop()`
    /// and `Drop` so the detached reader thread self-terminates instead of
    /// outliving the session.
    running: Arc<AtomicBool>,
    /// UI-thread task applying samples as they arrive (no wakeups between
    /// them). Dropping it cancels the task; the spawn closure's `WeakEntity` also stops it on entity drop.
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
            history: VecDeque::with_capacity(HISTORY_LEN),
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

        let (tx, mut rx) = mpsc::unbounded::<RuntimeStatus>();
        let api = self.api;

        // Dedicated blocking reader thread: gpui's executor is not built for
        // blocking stream reads (same reason the stdout/stderr pipe readers
        // use raw threads). It reconnects while `running` so it rides out a
        // transient drop, and exits once the flag
        // clears or the receiver is gone (drain task dropped on stop()).
        thread::spawn(move || {
            while running.load(Ordering::SeqCst) {
                // Why a stream ended doesn't matter here: either sing-box is
                // going away (the edge observer stops us) or it isn't up yet.
                let _ = api.stream_status(|status| {
                    running.load(Ordering::SeqCst) && tx.unbounded_send(status).is_ok()
                });
                if !running.load(Ordering::SeqCst) {
                    break;
                }
                thread::sleep(RECONNECT_DELAY);
            }
            // `tx` drops here → the drain task sees the close and zeroes.
        });

        let drain = cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            while let Some(samples) = next_batch(&mut rx, || executor.timer(COALESCE)).await {
                if this
                    .update(cx, |traffic, cx| {
                        traffic.ingest(samples);
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
            // Reader thread ended (process stopped or API gone). Zero the
            // readout so it doesn't freeze on a stale value, then exit — a
            // new session spawns a fresh drain task.
            let _ = this.update(cx, |traffic, cx| {
                if traffic.status != RuntimeStatus::default() {
                    traffic.apply(RuntimeStatus::default());
                    cx.notify();
                }
            });
        });
        self._drain = Some(drain);

        // Start time + version: plain unary calls, retried until the API
        // answers. On the background executor like `ProxyGroups::select` —
        // each call is bounded by the transport's 2s unary timeout.
        let info = cx.spawn(async move |this, cx| {
            for _ in 0..INFO_ATTEMPTS {
                let result =
                    cx.background_executor()
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

    /// Every sample of a batch, oldest first. The readout shows the newest
    /// (totals are cumulative, rates per-second, so nothing is lost by
    /// skipping one); each one still passes through here.
    /// Each one also joins the rate history, so a backlog folded into one
    /// batch still plots one point per second.
    fn ingest(&mut self, samples: Vec<RuntimeStatus>) {
        for sample in samples {
            if self.history.len() == HISTORY_LEN {
                self.history.pop_front();
            }
            self.history.push_back(RatePoint {
                up: sample.uplink,
                down: sample.downlink,
            });
            self.apply(sample);
        }
    }

    /// The recent rates, oldest first: at most [`HISTORY_LEN`] samples, one
    /// a second, of the current run only.
    pub fn history(&self) -> &VecDeque<RatePoint> {
        &self.history
    }

    fn apply(&mut self, status: RuntimeStatus) {
        self.up = status.uplink;
        self.down = status.downlink;
        self.status = status;
    }

    /// Back to the stopped state: no sample, no history, no run facts.
    fn reset(&mut self) {
        self.apply(RuntimeStatus::default());
        self.history.clear();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(up: u64, down: u64) -> RuntimeStatus {
        RuntimeStatus {
            uplink: up,
            downlink: down,
            ..RuntimeStatus::default()
        }
    }

    fn traffic() -> Traffic {
        Traffic::new(SingBoxApi::new(0))
    }

    #[test]
    fn every_sample_of_a_batch_joins_the_history_oldest_first() {
        let mut traffic = traffic();
        traffic.ingest(vec![sample(1, 10), sample(2, 20), sample(3, 30)]);
        let history: Vec<_> = traffic.history().iter().copied().collect();
        assert_eq!(
            history,
            vec![
                RatePoint { up: 1, down: 10 },
                RatePoint { up: 2, down: 20 },
                RatePoint { up: 3, down: 30 },
            ]
        );
        // The readout shows the newest.
        assert_eq!((traffic.up, traffic.down), (3, 30));
    }

    #[test]
    fn history_keeps_only_the_newest_window() {
        let mut traffic = traffic();
        // One batch past the cap, then single samples like the live stream.
        traffic.ingest((0..HISTORY_LEN as u64 + 5).map(|i| sample(i, i)).collect());
        traffic.ingest(vec![sample(1000, 2000)]);
        let history = traffic.history();
        assert_eq!(history.len(), HISTORY_LEN);
        assert_eq!(history.front(), Some(&RatePoint { up: 6, down: 6 }));
        assert_eq!(
            history.back(),
            Some(&RatePoint {
                up: 1000,
                down: 2000
            })
        );
    }

    #[test]
    fn reset_clears_the_history_with_the_readout() {
        let mut traffic = traffic();
        traffic.ingest(vec![sample(5, 7), sample(6, 8)]);
        traffic.reset();
        assert!(traffic.history().is_empty());
        assert_eq!((traffic.up, traffic.down), (0, 0));
        traffic.ingest(vec![sample(9, 9)]);
        assert_eq!(traffic.history().len(), 1);
    }
}
