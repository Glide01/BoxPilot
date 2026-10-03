//! Live state of the running profile's Tailscale endpoints: status
//! (`SubscribeTailscaleStatus`), each endpoint's Taildrop inbox, an optional
//! peer ping, and the one-shot actions (exit node, logout, Taildrop file
//! operations, certificates). Owned by `AppState`; started on the process
//! Stopped→Running edge and cleared on the reverse edge, the same way
//! `ProxyGroups` and `Traffic` are driven.
//!
//! Whether the Tailscale page exists at all is decided here:
//! `has_endpoints()` is true once sing-box reports at least one Tailscale
//! endpoint. The status stream is the source of truth (it answers with an
//! empty list when the config has none), so the page can never offer an
//! endpoint the API can't act on.

use crate::core::settings::{StatusEvent, StatusLevel};
use crate::core::singbox_api::{
    SingBoxApi, TaildropInbox, TailscaleCertificate, TailscaleEndpointStatus, TailscalePing,
};
use crate::i18n::s;
use crate::state::drain::next_batch;
use futures_channel::mpsc::{self, UnboundedSender};
use futures_channel::oneshot;
use gpui::{Context, EventEmitter, Task};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// Once an update arrives, how long the UI-thread task lets more queue up
/// before applying them as one render. Nothing runs between updates.
const COALESCE: Duration = Duration::from_millis(100);
/// Delay before a reader thread re-subscribes after its stream ended while
/// still running (API not up yet, or the routine idle read timeout).
const RECONNECT_DELAY: Duration = Duration::from_secs(1);
/// Ping results kept on screen, newest first.
const PING_HISTORY: usize = 8;

/// A one-shot action in flight — drives the button spinners and keeps a
/// second click from starting a duplicate.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TailscaleAction {
    ExitNode {
        tag: String,
    },
    Logout {
        tag: String,
    },
    MarkRead {
        tag: String,
    },
    Download {
        tag: String,
        name: String,
    },
    Delete {
        tag: String,
        name: String,
    },
    CancelReceiving {
        tag: String,
        sender_id: String,
        name: String,
    },
    Certificate {
        tag: String,
        domain: String,
    },
}

/// A fetched certificate, handed to the page (which owns the window needed
/// to show and save it). Never stored in the state, never logged.
pub struct CertificateFetched {
    pub domain: String,
    pub certificate: TailscaleCertificate,
}

/// The current (or last) peer ping.
pub struct PingSession {
    pub endpoint_tag: String,
    pub peer_name: String,
    pub peer_ip: String,
    /// Newest first, at most `PING_HISTORY`.
    pub results: VecDeque<TailscalePing>,
    /// Still pinging.
    pub active: bool,
    /// Why the ping stream itself ended, if it failed.
    pub error: Option<String>,
    running: Arc<AtomicBool>,
    _task: Option<Task<()>>,
}

enum StreamEvent {
    Status(Vec<TailscaleEndpointStatus>),
    Inbox(TaildropInbox),
}

enum PingEvent {
    Result(TailscalePing),
    Ended(Option<String>),
}

pub struct TailscaleState {
    /// Every Tailscale endpoint in the running config, in config order.
    /// Empty while stopped, and when the config has none.
    pub endpoints: Vec<TailscaleEndpointStatus>,
    /// Taildrop inbox per endpoint tag.
    pub inboxes: HashMap<String, TaildropInbox>,
    pub ping: Option<PingSession>,
    pub busy: HashSet<TailscaleAction>,
    /// sing-box API handle (port + this run's secret); swapped by `set_api`
    /// before every sing-box start.
    api: SingBoxApi,
    /// Liveness flag for the current session's reader threads (status and
    /// every inbox). Cleared by `clear()` and `Drop`.
    running: Arc<AtomicBool>,
    _task: Option<Task<()>>,
}

impl EventEmitter<StatusEvent> for TailscaleState {}
impl EventEmitter<CertificateFetched> for TailscaleState {}

impl TailscaleState {
    pub fn new(api: SingBoxApi) -> Self {
        Self {
            endpoints: Vec::new(),
            inboxes: HashMap::new(),
            ping: None,
            busy: HashSet::new(),
            api,
            running: Arc::new(AtomicBool::new(false)),
            _task: None,
        }
    }

    pub fn set_api(&mut self, api: SingBoxApi) {
        self.api = api;
    }

    /// Whether the sidebar shows the Tailscale page.
    pub fn has_endpoints(&self) -> bool {
        !self.endpoints.is_empty()
    }

    pub fn is_busy(&self, action: &TailscaleAction) -> bool {
        self.busy.contains(action)
    }

    /// Subscribe (Stopped→Running edge). One reader thread holds the status
    /// stream; one more per endpoint holds its Taildrop inbox, started as
    /// the endpoint first appears (the set is fixed for a run — a config
    /// change restarts sing-box). A UI-thread task applies the newest of
    /// each.
    pub fn start(&mut self, cx: &mut Context<Self>) {
        self.running.store(false, Ordering::SeqCst);
        let running = Arc::new(AtomicBool::new(true));
        self.running = running.clone();

        let (tx, mut rx) = mpsc::unbounded::<StreamEvent>();
        let api = self.api;
        {
            let running = running.clone();
            let tx = tx.clone();
            thread::spawn(move || {
                while running.load(Ordering::SeqCst) {
                    let _ = api.stream_tailscale_status(|endpoints| {
                        running.load(Ordering::SeqCst)
                            && tx.unbounded_send(StreamEvent::Status(endpoints)).is_ok()
                    });
                    if !running.load(Ordering::SeqCst) {
                        break;
                    }
                    thread::sleep(RECONNECT_DELAY);
                }
            });
        }

        let task = cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            let mut inbox_readers: HashSet<String> = HashSet::new();
            // `tx` lives in this task, so the channel can't close while it
            // runs; `None` is only a formality.
            while let Some(events) = next_batch(&mut rx, || executor.timer(COALESCE)).await {
                let mut status = None;
                let mut inboxes = Vec::new();
                for event in events {
                    match event {
                        StreamEvent::Status(endpoints) => status = Some(endpoints),
                        StreamEvent::Inbox(inbox) => inboxes.push(inbox),
                    }
                }
                if let Some(endpoints) = &status {
                    for endpoint in endpoints {
                        if inbox_readers.insert(endpoint.endpoint_tag.clone()) {
                            spawn_inbox_reader(
                                api,
                                endpoint.endpoint_tag.clone(),
                                running.clone(),
                                tx.clone(),
                            );
                        }
                    }
                }
                let alive = this.update(cx, |state, cx| {
                    if let Some(endpoints) = status {
                        state
                            .inboxes
                            .retain(|tag, _| endpoints.iter().any(|e| &e.endpoint_tag == tag));
                        state.endpoints = endpoints;
                    }
                    for inbox in inboxes {
                        // The no-endpoint placeholder has no tag.
                        if !inbox.endpoint_tag.is_empty() {
                            state.inboxes.insert(inbox.endpoint_tag.clone(), inbox);
                        }
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    return;
                }
            }
        });
        self._task = Some(task);
    }

    /// Forget everything (Running→Stopped edge): ends the streams and any
    /// ping. In-flight actions finish on their own and report as usual.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.running.store(false, Ordering::SeqCst);
        self._task = None;
        self.endpoints.clear();
        self.inboxes.clear();
        if let Some(ping) = self.ping.take() {
            ping.running.store(false, Ordering::SeqCst);
        }
        cx.notify();
    }

    /// Ping `peer_ip` through `endpoint_tag` once a second until stopped,
    /// replacing any ping already running.
    pub fn start_ping(
        &mut self,
        endpoint_tag: String,
        peer_ip: String,
        peer_name: String,
        cx: &mut Context<Self>,
    ) {
        self.stop_ping(cx);
        let running = Arc::new(AtomicBool::new(true));
        let (tx, mut rx) = mpsc::unbounded::<PingEvent>();
        let api = self.api;
        {
            let running = running.clone();
            let tag = endpoint_tag.clone();
            let ip = peer_ip.clone();
            thread::spawn(move || {
                let result = api.start_tailscale_ping(&tag, &ip, |ping| {
                    running.load(Ordering::SeqCst)
                        && tx.unbounded_send(PingEvent::Result(ping)).is_ok()
                });
                let _ = tx.unbounded_send(PingEvent::Ended(result.err().map(|e| e.to_string())));
            });
        }
        let task = cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            loop {
                // `None` = the ping thread is done.
                let (events, gone) = match next_batch(&mut rx, || executor.timer(COALESCE)).await {
                    Some(events) => (events, false),
                    None => (Vec::new(), true),
                };
                let alive = this.update(cx, |state, cx| {
                    let Some(ping) = state.ping.as_mut() else {
                        return;
                    };
                    for event in events {
                        match event {
                            PingEvent::Result(result) => {
                                ping.results.push_front(result);
                                ping.results.truncate(PING_HISTORY);
                            }
                            PingEvent::Ended(error) => {
                                // A user stop ends the stream with `Ok`; only
                                // a failure while still wanted is worth
                                // showing.
                                if ping.active {
                                    ping.error = error;
                                }
                                ping.active = false;
                            }
                        }
                    }
                    if gone {
                        ping.active = false;
                    }
                    cx.notify();
                });
                if alive.is_err() || gone {
                    return;
                }
            }
        });
        self.ping = Some(PingSession {
            endpoint_tag,
            peer_name,
            peer_ip,
            results: VecDeque::new(),
            active: true,
            error: None,
            running,
            _task: Some(task),
        });
        cx.notify();
    }

    /// Stop the ping, keeping its results on screen.
    pub fn stop_ping(&mut self, cx: &mut Context<Self>) {
        if let Some(ping) = self.ping.as_mut() {
            ping.running.store(false, Ordering::SeqCst);
            ping.active = false;
            ping._task = None;
            cx.notify();
        }
    }

    /// Close the ping panel.
    pub fn dismiss_ping(&mut self, cx: &mut Context<Self>) {
        self.stop_ping(cx);
        self.ping = None;
        cx.notify();
    }

    /// Route through the exit node `stable_id`, or stop using one (empty).
    /// The result shows up on the status stream.
    pub fn set_exit_node(&mut self, tag: String, stable_id: String, cx: &mut Context<Self>) {
        let api = self.api;
        let call_tag = tag.clone();
        self.run_action(
            TailscaleAction::ExitNode { tag },
            s().tailscale.set_exit_node_failed,
            move || api.set_tailscale_exit_node(&call_tag, &stable_id),
            |_| None,
            cx,
        );
    }

    /// Log out; a fresh login URL then arrives on the status stream.
    pub fn logout(&mut self, tag: String, cx: &mut Context<Self>) {
        let api = self.api;
        let call_tag = tag.clone();
        self.run_action(
            TailscaleAction::Logout { tag },
            s().tailscale.logout_failed,
            move || api.tailscale_logout(&call_tag),
            |_| Some((StatusLevel::Success, s().tailscale.logged_out.to_string())),
            cx,
        );
    }

    pub fn mark_inbox_read(&mut self, tag: String, cx: &mut Context<Self>) {
        let api = self.api;
        let call_tag = tag.clone();
        self.run_action(
            TailscaleAction::MarkRead { tag },
            s().tailscale.mark_read_failed,
            move || api.mark_taildrop_inbox_read(&call_tag),
            |_| None,
            cx,
        );
    }

    pub fn delete_file(&mut self, tag: String, name: String, cx: &mut Context<Self>) {
        let api = self.api;
        let (call_tag, call_name) = (tag.clone(), name.clone());
        self.run_action(
            TailscaleAction::Delete { tag, name },
            s().tailscale.delete_failed,
            move || api.delete_taildrop_file(&call_tag, &call_name),
            |_| None,
            cx,
        );
    }

    pub fn cancel_receiving(
        &mut self,
        tag: String,
        sender_id: String,
        name: String,
        cx: &mut Context<Self>,
    ) {
        let api = self.api;
        let (call_tag, call_sender, call_name) = (tag.clone(), sender_id.clone(), name.clone());
        self.run_action(
            TailscaleAction::CancelReceiving {
                tag,
                sender_id,
                name,
            },
            s().tailscale.cancel_failed,
            move || api.cancel_taildrop_receiving(&call_tag, &call_sender, &call_name),
            |_| None,
            cx,
        );
    }

    /// Save a received file to `dest` (chosen by the user). The file stays
    /// in the inbox.
    pub fn download_file(
        &mut self,
        tag: String,
        name: String,
        dest: PathBuf,
        cx: &mut Context<Self>,
    ) {
        let api = self.api;
        let (call_tag, call_name) = (tag.clone(), name.clone());
        let shown = dest.display().to_string();
        self.run_action(
            TailscaleAction::Download { tag, name },
            s().tailscale.save_failed,
            move || api.download_taildrop_file_to(&call_tag, &call_name, &dest, |_, _| true),
            move |_| Some((StatusLevel::Success, (s().tailscale.saved_to)(&shown))),
            cx,
        );
    }

    /// Fetch a certificate for `domain`; on success emits
    /// `CertificateFetched` for the page to show and save.
    pub fn fetch_certificate(&mut self, tag: String, domain: String, cx: &mut Context<Self>) {
        let action = TailscaleAction::Certificate {
            tag: tag.clone(),
            domain: domain.clone(),
        };
        if !self.busy.insert(action.clone()) {
            return;
        }
        cx.notify();
        let api = self.api;
        let result = spawn_blocking(move || {
            api.get_tailscale_certificate(&tag, &domain, Duration::ZERO)
                .map(|certificate| CertificateFetched {
                    domain,
                    certificate,
                })
        });
        cx.spawn(async move |this, cx| {
            let result = result.await;
            let _ = this.update(cx, |state, cx| {
                state.busy.remove(&action);
                match result {
                    Some(Ok(fetched)) => cx.emit(fetched),
                    Some(Err(e)) => cx.emit(StatusEvent {
                        level: StatusLevel::Error,
                        message: (s().tailscale.certificate_failed)(&e.to_string()),
                    }),
                    None => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Run a blocking API call on its own thread while `action` shows as
    /// busy; toast `failure: error` when it fails, or whatever `success`
    /// returns when it succeeds. A duplicate of an action in flight is
    /// ignored.
    fn run_action<T: Send + 'static, E: std::fmt::Display + Send + 'static>(
        &mut self,
        action: TailscaleAction,
        failure: &'static str,
        call: impl FnOnce() -> Result<T, E> + Send + 'static,
        success: impl FnOnce(T) -> Option<(StatusLevel, String)> + 'static,
        cx: &mut Context<Self>,
    ) {
        if !self.busy.insert(action.clone()) {
            return;
        }
        cx.notify();
        let result = spawn_blocking(call);
        cx.spawn(async move |this, cx| {
            let result = result.await;
            let _ = this.update(cx, |state, cx| {
                state.busy.remove(&action);
                let status = match result {
                    Some(Ok(value)) => success(value),
                    Some(Err(e)) => Some((
                        StatusLevel::Error,
                        format!("{}{}{}", failure, s().common.colon, e),
                    )),
                    None => None,
                };
                if let Some((level, message)) = status {
                    cx.emit(StatusEvent { level, message });
                }
                cx.notify();
            });
        })
        .detach();
    }
}

/// Run a blocking call on a dedicated thread (some take tens of seconds —
/// certificates, downloads — too long for the shared executor); resolves to
/// `None` only if the thread panicked.
fn spawn_blocking<T: Send + 'static>(
    call: impl FnOnce() -> T + Send + 'static,
) -> impl std::future::Future<Output = Option<T>> {
    let (tx, rx) = oneshot::channel();
    thread::spawn(move || {
        let _ = tx.send(call());
    });
    async move { rx.await.ok() }
}

fn spawn_inbox_reader(
    api: SingBoxApi,
    tag: String,
    running: Arc<AtomicBool>,
    tx: UnboundedSender<StreamEvent>,
) {
    thread::spawn(move || {
        while running.load(Ordering::SeqCst) {
            // Idle timeouts are routine; a hard failure would repeat on
            // every attempt, and the delay keeps that from spinning.
            let _ = api.stream_taildrop_inbox(&tag, |inbox| {
                running.load(Ordering::SeqCst)
                    && tx.unbounded_send(StreamEvent::Inbox(inbox)).is_ok()
            });
            if !running.load(Ordering::SeqCst) {
                break;
            }
            thread::sleep(RECONNECT_DELAY);
        }
    });
}

impl Drop for TailscaleState {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(ping) = &self.ping {
            ping.running.store(false, Ordering::SeqCst);
        }
    }
}
