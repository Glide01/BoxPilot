//! Tools page state: the outbound list to test through, and at most one
//! network quality run and one STUN run. Owned by `AppState`; started on the
//! process Stopped→Running edge and cleared on the reverse edge (see the
//! observer in `AppState::new`), the same way `ProxyGroups` and `Traffic`
//! are driven.
//!
//! Each run is a sing-box API server stream held by a dedicated thread; a
//! UI-thread task drains its progress into the run's display model
//! (`core::network_tools`). Cancelling, or sing-box stopping, drops that
//! task — and with it the channel — so the thread's next callback returns
//! `false` and the stream (and sing-box's test) ends.

use crate::core::network_tools::{
    outbound_choices, test_error_message, OutboundChoice, QualityRun, StunRun, ENDED_WITHOUT_RESULT,
};
use crate::core::singbox_api::{
    ApiError, NetworkQualityProgress, NetworkQualityRequest, SingBoxApi, StunProgress, StunRequest,
};
use crate::state::drain::next_batch;
use futures_channel::mpsc::{self, UnboundedReceiver};
use futures_channel::oneshot;
use gpui::{Context, Task};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// Once progress arrives, how long the UI-thread task lets more queue up
/// before applying it. sing-box reports every 500ms while measuring, so this
/// is a batch of one in practice.
const COALESCE: Duration = Duration::from_millis(50);
/// Delay between attempts to load the outbound list while the sing-box API
/// isn't answering yet. Bounded by the session flag.
const RETRY_DELAY: Duration = Duration::from_secs(1);

/// What a test thread hands the UI thread.
enum TestEvent<P> {
    Progress(P),
    /// The call failed before the final message.
    Failed(String),
}

/// A run's display model, as the generic drain task drives it.
trait TestRun: 'static {
    type Progress: Send + 'static;
    fn apply(&mut self, progress: Self::Progress);
    fn fail(&mut self, message: String);
    fn is_running(&self) -> bool;
}

impl TestRun for QualityRun {
    type Progress = NetworkQualityProgress;
    fn apply(&mut self, progress: NetworkQualityProgress) {
        QualityRun::apply(self, progress)
    }
    fn fail(&mut self, message: String) {
        QualityRun::fail(self, message)
    }
    fn is_running(&self) -> bool {
        self.status.is_running()
    }
}

impl TestRun for StunRun {
    type Progress = StunProgress;
    fn apply(&mut self, progress: StunProgress) {
        StunRun::apply(self, progress)
    }
    fn fail(&mut self, message: String) {
        StunRun::fail(self, message)
    }
    fn is_running(&self) -> bool {
        self.status.is_running()
    }
}

pub struct NetworkTools {
    /// sing-box is running (between the Start and Stop edges); tests can
    /// only start then.
    pub active: bool,
    /// Picker entries: "Default outbound" first, then sing-box's outbounds
    /// and endpoints. Just the default entry until the list has loaded.
    pub outbounds: Vec<OutboundChoice>,
    /// The current or last network quality run of this session.
    pub quality: Option<QualityRun>,
    /// The current or last STUN run of this session.
    pub stun: Option<StunRun>,
    /// sing-box API 句柄(端口 + 本次运行的 secret);AppState 每次启动
    /// sing-box 前经 `set_api` 换新句柄。
    api: SingBoxApi,
    /// Liveness flag for the current sing-box session. Cleared by `stop()`
    /// and `Drop` so detached threads exit at their next check.
    session: Arc<AtomicBool>,
    _outbounds_task: Option<Task<()>>,
    /// Drain tasks of the current runs. Dropping one cancels its run (see
    /// the module docs); a finished run's task has already returned.
    quality_task: Option<Task<()>>,
    stun_task: Option<Task<()>>,
}

impl NetworkTools {
    pub fn new(api: SingBoxApi) -> Self {
        Self {
            active: false,
            outbounds: outbound_choices(&[]),
            quality: None,
            stun: None,
            api,
            session: Arc::new(AtomicBool::new(false)),
            _outbounds_task: None,
            quality_task: None,
            stun_task: None,
        }
    }

    /// Swap in the API handle (port + secret) of the sing-box run about to
    /// start; AppState calls this before every start, and the edges move a
    /// live session over.
    pub fn set_api(&mut self, api: SingBoxApi) {
        self.api = api;
    }

    pub fn quality_running(&self) -> bool {
        self.quality
            .as_ref()
            .is_some_and(|run| run.status.is_running())
    }

    pub fn stun_running(&self) -> bool {
        self.stun
            .as_ref()
            .is_some_and(|run| run.status.is_running())
    }

    /// Stopped→Running edge: enable the tools and load the outbound list.
    /// The list is fixed for a sing-box run (a config change restarts it),
    /// so one `SubscribeOutbounds` snapshot is enough; the thread retries
    /// until the API answers or the session ends.
    pub fn start(&mut self, cx: &mut Context<Self>) {
        self.stop(cx);
        let session = Arc::new(AtomicBool::new(true));
        self.session = session.clone();
        self.active = true;

        let (tx, rx) = oneshot::channel();
        let api = self.api;
        thread::spawn(move || {
            while session.load(Ordering::SeqCst) {
                let mut snapshot = None;
                let _ = api.stream_outbounds(|list| {
                    snapshot = Some(list);
                    false
                });
                if let Some(list) = snapshot {
                    let _ = tx.send(list);
                    return;
                }
                thread::sleep(RETRY_DELAY);
            }
        });
        self._outbounds_task = Some(cx.spawn(async move |this, cx| {
            // Err = the thread gave up because the session ended.
            if let Ok(list) = rx.await {
                let _ = this.update(cx, |tools, cx| {
                    tools.outbounds = outbound_choices(&list);
                    cx.notify();
                });
            }
        }));
        cx.notify();
    }

    /// Running→Stopped edge: cancel any running test and clear the page —
    /// results belong to one sing-box session, like the Groups delays.
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        self.session.store(false, Ordering::SeqCst);
        self.active = false;
        self.outbounds = outbound_choices(&[]);
        self._outbounds_task = None;
        self.quality_task = None;
        self.stun_task = None;
        self.quality = None;
        self.stun = None;
        cx.notify();
    }

    /// Start a network quality test unless one is running.
    pub fn start_quality(&mut self, request: NetworkQualityRequest, cx: &mut Context<Self>) {
        if !self.active || self.quality_running() {
            return;
        }
        self.quality = Some(QualityRun::new(
            request.serial,
            effective_max_runtime(request.max_runtime_seconds),
        ));
        let api = self.api;
        let rx = spawn_test(self.session.clone(), move |on_progress| {
            api.start_network_quality_test(&request, on_progress)
        });
        self.quality_task = Some(drain(rx, |tools| &mut tools.quality, cx));
        cx.notify();
    }

    pub fn cancel_quality(&mut self, cx: &mut Context<Self>) {
        if let Some(run) = &mut self.quality {
            run.cancel();
        }
        self.quality_task = None;
        cx.notify();
    }

    /// Start a STUN test unless one is running.
    pub fn start_stun(&mut self, request: StunRequest, cx: &mut Context<Self>) {
        if !self.active || self.stun_running() {
            return;
        }
        self.stun = Some(StunRun::new());
        let api = self.api;
        let rx = spawn_test(self.session.clone(), move |on_progress| {
            api.start_stun_test(&request, on_progress)
        });
        self.stun_task = Some(drain(rx, |tools| &mut tools.stun, cx));
        cx.notify();
    }

    pub fn cancel_stun(&mut self, cx: &mut Context<Self>) {
        if let Some(run) = &mut self.stun {
            run.cancel();
        }
        self.stun_task = None;
        cx.notify();
    }
}

impl Drop for NetworkTools {
    fn drop(&mut self) {
        self.session.store(false, Ordering::SeqCst);
    }
}

/// The budget a run uses: sing-box treats 0 as its 20s default.
fn effective_max_runtime(seconds: u32) -> u32 {
    if seconds == 0 {
        crate::core::network_tools::DEFAULT_MAX_RUNTIME
    } else {
        seconds
    }
}

/// Run one test stream on a dedicated thread (every call blocks for the
/// whole test). Progress is forwarded while the session lives and the
/// receiver exists; once either is gone the callback returns `false`, which
/// closes the stream and cancels the test in sing-box.
fn spawn_test<P: Send + 'static>(
    session: Arc<AtomicBool>,
    run: impl FnOnce(&mut dyn FnMut(P) -> bool) -> Result<(), ApiError> + Send + 'static,
) -> UnboundedReceiver<TestEvent<P>> {
    let (tx, rx) = mpsc::unbounded();
    thread::spawn(move || {
        let result = run(&mut |progress| {
            session.load(Ordering::SeqCst)
                && tx.unbounded_send(TestEvent::Progress(progress)).is_ok()
        });
        if let Err(error) = result {
            let _ = tx.unbounded_send(TestEvent::Failed(test_error_message(&error)));
        }
        // `tx` drops here → the drain task sees the channel close.
    });
    rx
}

/// UI-thread task folding a test thread's events into the run that `slot`
/// selects. Returns once the run has ended or the thread is gone.
fn drain<R: TestRun>(
    mut rx: UnboundedReceiver<TestEvent<R::Progress>>,
    slot: fn(&mut NetworkTools) -> &mut Option<R>,
    cx: &mut Context<NetworkTools>,
) -> Task<()> {
    cx.spawn(async move |this, cx| loop {
        let executor = cx.background_executor().clone();
        // `None`: the test thread is done.
        let (events, disconnected) = match next_batch(&mut rx, || executor.timer(COALESCE)).await {
            Some(events) => (events, false),
            None => (Vec::new(), true),
        };

        let ended = this.update(cx, |tools, cx| {
            let Some(run) = slot(tools).as_mut() else {
                return true;
            };
            for event in events {
                match event {
                    TestEvent::Progress(progress) => run.apply(progress),
                    TestEvent::Failed(message) => run.fail(message),
                }
            }
            if disconnected {
                // A no-op when the final message already ended the run.
                run.fail(ENDED_WITHOUT_RESULT.to_string());
            }
            cx.notify();
            !run.is_running()
        });
        if ended.unwrap_or(true) {
            return;
        }
    })
}
