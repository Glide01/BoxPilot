//! Live status of the running config's OpenConnect / OpenVPN client
//! endpoints and USB/IP servers, plus their sign-in challenges. Owned by
//! `AppState`; drives the VPN page and its sign-in dialogs.
//!
//! Started on the process Stopped→Running edge and cleared on the reverse
//! one, like `ProxyGroups` / `Traffic` — but it watches `ProcessSession`
//! itself (see `new`) instead of through `AppState`'s observer. On start it
//! reads the prepared runtime config and opens a status stream only for the
//! kinds the config actually has (`VpnPresence`): a profile with none of
//! them costs nothing and keeps the page hidden.
//!
//! Threading mirrors `ProxyGroups`: one dedicated reader thread per stream,
//! re-subscribing while the session's `running` flag holds, and one
//! UI-thread task draining their snapshots. Submits and cancels are
//! fire-and-forget background calls bounded by their own timeouts.

use crate::core::settings::{StatusEvent, StatusLevel};
use crate::core::singbox_api::{
    ApiError, OpenConnectBrowserResult, OpenConnectChallenge, OpenConnectEndpointStatus,
    OpenVpnAnswer, OpenVpnChallenge, OpenVpnEndpointStatus, SingBoxApi, UsbipServerStatus,
};
use crate::core::vpn::{
    failed_endpoints, newly_seen, pending_challenges, should_report_stream_error,
    stream_error_is_permanent, ChallengeKey, EndpointFailure, VpnPresence, VpnProtocol,
};
use crate::state::drain::next_batch;
use crate::state::process_session::ProcessSession;
use futures_channel::mpsc::{self, UnboundedSender};
use gpui::{Context, Entity, EventEmitter, Task};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// Once a snapshot arrives, how long the UI-thread task lets more queue up
/// before applying the newest of each as one render. Nothing runs between
/// snapshots.
const COALESCE: Duration = Duration::from_millis(100);
/// Delay before a reader re-subscribes after its stream failed while still
/// running (API not listening yet, transient drop). An idle timeout
/// re-subscribes at once instead.
const RECONNECT_DELAY: Duration = Duration::from_secs(1);

/// A sign-in challenge the user hasn't been asked about yet. The VPN page
/// answers it with a dialog.
#[derive(Clone, Debug)]
pub struct ChallengeRequested(pub ChallengeKey);

/// Which status stream a reader thread holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum VpnStream {
    OpenConnect,
    OpenVpn,
    Usbip,
}

impl VpnStream {
    fn label(self) -> &'static str {
        match self {
            VpnStream::OpenConnect => "OpenConnect",
            VpnStream::OpenVpn => "OpenVPN",
            VpnStream::Usbip => "USB/IP",
        }
    }
}

/// What the reader threads hand the UI thread.
enum StreamEvent {
    OpenConnect(Vec<OpenConnectEndpointStatus>),
    OpenVpn(Vec<OpenVpnEndpointStatus>),
    Usbip(Vec<UsbipServerStatus>),
    /// Why a subscription attempt ended, and whether the reader gave up
    /// for good. See `should_report_stream_error` for when it is shown.
    Failed {
        stream: VpnStream,
        error: String,
        permanent: bool,
    },
}

pub struct VpnStatus {
    /// What the running config has; empty while stopped. The VPN page and
    /// its sidebar entry show only while this is non-empty.
    pub presence: VpnPresence,
    /// Latest snapshots, in the order sing-box reports them. Empty until the
    /// first one arrives.
    pub openconnect: Vec<OpenConnectEndpointStatus>,
    pub openvpn: Vec<OpenVpnEndpointStatus>,
    pub usbip: Vec<UsbipServerStatus>,
    /// Whether each stream has delivered a snapshot this session.
    pub loaded: HashSet<VpnStream>,
    /// The last reportable stream error per stream, cleared by its next
    /// snapshot.
    pub stream_errors: BTreeMap<VpnStream, String>,
    /// Challenges with a submit or cancel in flight.
    pub busy: HashSet<ChallengeKey>,
    api: SingBoxApi,
    runtime_config_path: PathBuf,
    /// Challenges already announced (`ChallengeRequested`) — still pending.
    announced: HashSet<ChallengeKey>,
    /// Endpoint failures already toasted — still failed.
    toasted: HashSet<EndpointFailure>,
    /// When the current session started (the grace period for errors).
    started: Option<Instant>,
    /// Last `is_running()` acted on — detects the Running/Stopped edges.
    saw_running: bool,
    /// Liveness flag of the current session's reader threads.
    running: Arc<AtomicBool>,
    _task: Option<Task<()>>,
}

impl EventEmitter<StatusEvent> for VpnStatus {}
impl EventEmitter<ChallengeRequested> for VpnStatus {}

impl VpnStatus {
    /// `runtime_config_path` is the prepared config sing-box runs
    /// (`paths::runtime_config_path`); it is read on every Running edge.
    pub fn new(
        api: SingBoxApi,
        runtime_config_path: PathBuf,
        process: &Entity<ProcessSession>,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe(process, |this: &mut Self, process, cx| {
            let running = process.read(cx).is_running();
            if running == this.saw_running {
                return;
            }
            this.saw_running = running;
            if running {
                this.start(cx);
            } else {
                this.clear(cx);
            }
        })
        .detach();
        Self {
            presence: VpnPresence::default(),
            openconnect: Vec::new(),
            openvpn: Vec::new(),
            usbip: Vec::new(),
            loaded: HashSet::new(),
            stream_errors: BTreeMap::new(),
            busy: HashSet::new(),
            api,
            runtime_config_path,
            announced: HashSet::new(),
            toasted: HashSet::new(),
            started: None,
            saw_running: false,
            running: Arc::new(AtomicBool::new(false)),
            _task: None,
        }
    }

    /// Swap in the API handle (port + secret) of the sing-box run about to
    /// start; AppState calls this before every start, and the edges restart
    /// the streams.
    pub fn set_api(&mut self, api: SingBoxApi) {
        self.api = api;
    }

    /// Whether the VPN page has anything to show.
    pub fn is_visible(&self) -> bool {
        !self.presence.is_empty()
    }

    /// The pending OpenConnect challenge `key` names, if still pending.
    pub fn openconnect_challenge(&self, key: &ChallengeKey) -> Option<&OpenConnectChallenge> {
        if key.protocol != VpnProtocol::OpenConnect {
            return None;
        }
        self.openconnect
            .iter()
            .find(|status| status.endpoint_tag == key.endpoint_tag)
            .and_then(|status| status.challenge.as_ref())
            .filter(|challenge| challenge.id == key.challenge_id)
    }

    /// The pending OpenVPN challenge `key` names, if still pending.
    pub fn openvpn_challenge(&self, key: &ChallengeKey) -> Option<&OpenVpnChallenge> {
        if key.protocol != VpnProtocol::OpenVpn {
            return None;
        }
        self.openvpn
            .iter()
            .find(|status| status.endpoint_tag == key.endpoint_tag)
            .and_then(|status| status.challenge.as_ref())
            .filter(|challenge| challenge.id == key.challenge_id)
    }

    pub fn is_pending(&self, key: &ChallengeKey) -> bool {
        self.openconnect_challenge(key).is_some() || self.openvpn_challenge(key).is_some()
    }

    /// Stopped→Running: read the runtime config, then stream what it has.
    fn start(&mut self, cx: &mut Context<Self>) {
        self.reset();
        self.started = Some(Instant::now());
        let running = Arc::new(AtomicBool::new(true));
        self.running = running.clone();
        let api = self.api;
        let path = self.runtime_config_path.clone();

        let task = cx.spawn(async move |this, cx| {
            let presence = cx
                .background_executor()
                .spawn(async move {
                    fs::read_to_string(&path)
                        .map(|config| VpnPresence::from_config(&config))
                        .unwrap_or_default()
                })
                .await;

            let (tx, mut rx) = mpsc::unbounded::<StreamEvent>();
            let mut streams = Vec::new();
            if !presence.openconnect.is_empty() {
                streams.push(VpnStream::OpenConnect);
            }
            if !presence.openvpn.is_empty() {
                streams.push(VpnStream::OpenVpn);
            }
            if !presence.usbip_dynamic.is_empty() {
                streams.push(VpnStream::Usbip);
            }
            for stream in streams {
                spawn_reader(stream, api, running.clone(), tx.clone());
            }
            // Only the readers hold senders now: when they all exit, the
            // drain below sees the channel close and ends.
            drop(tx);

            if this
                .update(cx, |state, cx| {
                    state.presence = presence;
                    cx.notify();
                })
                .is_err()
            {
                return;
            }

            let executor = cx.background_executor().clone();
            while let Some(events) = next_batch(&mut rx, || executor.timer(COALESCE)).await {
                let mut openconnect = None;
                let mut openvpn = None;
                let mut usbip = None;
                let mut failures = Vec::new();
                for event in events {
                    match event {
                        StreamEvent::OpenConnect(update) => openconnect = Some(update),
                        StreamEvent::OpenVpn(update) => openvpn = Some(update),
                        StreamEvent::Usbip(update) => usbip = Some(update),
                        StreamEvent::Failed {
                            stream,
                            error,
                            permanent,
                        } => failures.push((stream, error, permanent)),
                    }
                }
                if this
                    .update(cx, |state, cx| {
                        state.apply(openconnect, openvpn, usbip, failures, cx);
                    })
                    .is_err()
                {
                    return;
                }
            }
        });
        self._task = Some(task);
        cx.notify();
    }

    fn apply(
        &mut self,
        openconnect: Option<Vec<OpenConnectEndpointStatus>>,
        openvpn: Option<Vec<OpenVpnEndpointStatus>>,
        usbip: Option<Vec<UsbipServerStatus>>,
        failures: Vec<(VpnStream, String, bool)>,
        cx: &mut Context<Self>,
    ) {
        let elapsed = self
            .started
            .map_or(Duration::ZERO, |started| started.elapsed());
        for (stream, error, permanent) in failures {
            if should_report_stream_error(permanent, self.loaded.contains(&stream), elapsed) {
                self.stream_errors
                    .insert(stream, format!("{} status: {}", stream.label(), error));
            }
        }
        let endpoints_changed = openconnect.is_some() || openvpn.is_some();
        if let Some(update) = openconnect {
            self.openconnect = update;
            self.mark_loaded(VpnStream::OpenConnect);
        }
        if let Some(update) = openvpn {
            self.openvpn = update;
            self.mark_loaded(VpnStream::OpenVpn);
        }
        if let Some(update) = usbip {
            self.usbip = update;
            self.mark_loaded(VpnStream::Usbip);
        }
        if endpoints_changed {
            let (fresh, announced) = newly_seen(
                &self.announced,
                pending_challenges(&self.openconnect, &self.openvpn),
            );
            self.announced = announced;
            // A challenge that ended can't still be busy.
            let announced = &self.announced;
            self.busy.retain(|key| announced.contains(key));
            for key in fresh {
                cx.emit(ChallengeRequested(key));
            }

            let (fresh, toasted) = newly_seen(
                &self.toasted,
                failed_endpoints(&self.openconnect, &self.openvpn),
            );
            self.toasted = toasted;
            for failure in fresh {
                cx.emit(StatusEvent {
                    level: StatusLevel::Warning,
                    message: failure.message(),
                });
            }
        }
        cx.notify();
    }

    fn mark_loaded(&mut self, stream: VpnStream) {
        self.loaded.insert(stream);
        self.stream_errors.remove(&stream);
    }

    /// Running→Stopped: end the streams and forget the session.
    fn clear(&mut self, cx: &mut Context<Self>) {
        self.reset();
        cx.notify();
    }

    fn reset(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        self._task = None;
        self.presence = VpnPresence::default();
        self.openconnect.clear();
        self.openvpn.clear();
        self.usbip.clear();
        self.loaded.clear();
        self.stream_errors.clear();
        self.busy.clear();
        self.announced.clear();
        self.toasted.clear();
        self.started = None;
    }

    /// Answer an OpenConnect form challenge (values from
    /// `openconnect_form_values`).
    pub fn submit_openconnect_form(
        &mut self,
        key: ChallengeKey,
        values: BTreeMap<String, String>,
        cx: &mut Context<Self>,
    ) {
        let api = self.api;
        self.run_action(key, "Sign-in failed", cx, move |key| {
            api.submit_openconnect_form(&key.endpoint_tag, &key.challenge_id, values)
        });
    }

    /// Answer an OpenConnect browser challenge (callback mode).
    pub fn submit_openconnect_browser(
        &mut self,
        key: ChallengeKey,
        result: OpenConnectBrowserResult,
        cx: &mut Context<Self>,
    ) {
        let api = self.api;
        self.run_action(key, "Sign-in failed", cx, move |key| {
            api.submit_openconnect_browser(&key.endpoint_tag, &key.challenge_id, &result)
        });
    }

    /// Answer an OpenVPN challenge (from `openvpn_answer`).
    pub fn submit_openvpn(
        &mut self,
        key: ChallengeKey,
        answer: OpenVpnAnswer,
        cx: &mut Context<Self>,
    ) {
        let api = self.api;
        self.run_action(key, "Sign-in failed", cx, move |key| {
            api.submit_openvpn_challenge(&key.endpoint_tag, &key.challenge_id, &answer)
        });
    }

    /// Refuse a challenge. OpenVPN then fails the endpoint for good;
    /// OpenConnect retries later and asks again.
    pub fn cancel_challenge(&mut self, key: ChallengeKey, cx: &mut Context<Self>) {
        let api = self.api;
        self.run_action(key, "Couldn't cancel sign-in", cx, move |key| {
            match key.protocol {
                VpnProtocol::OpenConnect => {
                    api.cancel_openconnect_auth(&key.endpoint_tag, &key.challenge_id)
                }
                VpnProtocol::OpenVpn => {
                    api.cancel_openvpn_challenge(&key.endpoint_tag, &key.challenge_id)
                }
            }
        });
    }

    /// One submit/cancel off the UI thread. While in flight the challenge is
    /// `busy`; a failure toasts (the challenge stays pending, so the page's
    /// Sign in button can reopen it).
    fn run_action(
        &mut self,
        key: ChallengeKey,
        failure: &'static str,
        cx: &mut Context<Self>,
        call: impl FnOnce(&ChallengeKey) -> Result<(), ApiError> + Send + 'static,
    ) {
        if !self.busy.insert(key.clone()) {
            return;
        }
        cx.notify();
        cx.spawn(async move |this, cx| {
            let request_key = key.clone();
            let result = cx
                .background_executor()
                .spawn(async move { call(&request_key) })
                .await;
            let _ = this.update(cx, |state, cx| {
                state.busy.remove(&key);
                if let Err(error) = result {
                    cx.emit(StatusEvent {
                        level: StatusLevel::Error,
                        message: format!("{}: {}", failure, error),
                    });
                }
                cx.notify();
            });
        })
        .detach();
    }
}

impl Drop for VpnStatus {
    fn drop(&mut self) {
        // Let the reader threads exit at their next update or retry once
        // the entity is gone (e.g. on app quit).
        self.running.store(false, Ordering::SeqCst);
    }
}

/// A dedicated blocking reader for one status stream (gpui's executor isn't
/// for blocking reads). Re-subscribes while `running`: at once after the
/// routine idle timeout, after `RECONNECT_DELAY` otherwise. Stops for good
/// when sing-box says the service doesn't exist in this build.
fn spawn_reader(
    stream: VpnStream,
    api: SingBoxApi,
    running: Arc<AtomicBool>,
    tx: UnboundedSender<StreamEvent>,
) {
    thread::spawn(move || {
        while running.load(Ordering::SeqCst) {
            let forward = |event: StreamEvent| {
                running.load(Ordering::SeqCst) && tx.unbounded_send(event).is_ok()
            };
            let result = match stream {
                VpnStream::OpenConnect => api
                    .stream_openconnect_status(|update| forward(StreamEvent::OpenConnect(update))),
                VpnStream::OpenVpn => {
                    api.stream_openvpn_status(|update| forward(StreamEvent::OpenVpn(update)))
                }
                VpnStream::Usbip => {
                    api.stream_usbip_server_status(|update| forward(StreamEvent::Usbip(update)))
                }
            };
            if !running.load(Ordering::SeqCst) {
                break;
            }
            match result {
                Err(error) if error.is_timeout() => continue,
                Err(error) => {
                    let permanent = stream_error_is_permanent(&error);
                    let failed = StreamEvent::Failed {
                        stream,
                        error: error.to_string(),
                        permanent,
                    };
                    if tx.unbounded_send(failed).is_err() || permanent {
                        break;
                    }
                }
                Ok(()) => {}
            }
            thread::sleep(RECONNECT_DELAY);
        }
    });
}
