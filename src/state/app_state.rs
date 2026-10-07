use crate::core::deeplink::{derive_profile_name, parse_import_uri, ImportRequest, LaunchAttempt};
use crate::core::orchestration::{
    config_change_action, fetch_result_applies, process_edge_effects, ApiPortRetry,
    ConfigChangeAction, ProcessEdgeEffect, StartPhase,
};
#[cfg(target_os = "linux")]
use crate::core::privilege::{evaluate_tun_plan, run_grant, TunPlan, PRIVILEGED_COPY_PATH};
use crate::core::process::query_sing_box_version;
use crate::core::paths::{
    create_private_dir, get_app_data_dir, get_install_dir, profile_config_path,
    runtime_config_path,
};
use crate::core::settings::{
    default_auto_update_interval, default_update_via_sing_box, AppSettings, CloseAction,
    LanguagePreference, Profile, ProfileSource, StatusEvent, StatusLevel, ThemePreference,
    CONFIG_FILENAME, SING_EXECUTABLE,
};
use crate::core::singbox_api::{supports_api_service, SingBoxApi, MIN_SING_BOX_VERSION};
use crate::core::sub_usage::{SubscriptionUsage, UsageLevel};
use crate::core::subscription::{
    import_local_config, local_proxy, perform_update, pick_api_port, prepare_config,
    save_runtime_config, Fetched, RuntimeOptions,
};
use crate::core::timefmt::{file_mtime, to_unix_secs};
use crate::core::update_check::{
    fetch_latest, is_newer, same_version, should_notify, ReleaseInfo, CHECK_INTERVAL,
    CURRENT_VERSION, FIRST_CHECK_DELAY,
};
use crate::i18n::s;
use crate::state::clash_mode::ClashMode;
use crate::state::connections::Connections;
use crate::state::log_buffer::LogBuffer;
use crate::state::network_tools::NetworkTools;
use crate::state::process_session::{ApiPortLost, PendingStart, ProcessSession};
use crate::state::proxy_groups::ProxyGroups;
use crate::state::tailscale::TailscaleState;
use crate::state::traffic::Traffic;
use crate::state::vpn::VpnStatus;
use futures_channel::mpsc::UnboundedReceiver;
use futures_channel::oneshot;
use futures_util::StreamExt;
use gpui::{App, AppContext, Context, Entity, EventEmitter, Task};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

/// How often the auto-update loop wakes up. We use a 60-second tick (the
/// smallest user-meaningful interval) and gate actual fetches on elapsed
/// time vs. the configured interval, so changing the interval via settings
/// takes effect at the next tick without restarting the task.
const AUTO_UPDATE_TICK: Duration = Duration::from_secs(60);

/// How often the BoxPilot update-check loop wakes to see whether a check is
/// due (`CHECK_INTERVAL` since the last one). Hourly is plenty against a
/// daily interval and keeps the loop's idle wakeups negligible; wall-clock
/// time (`SystemTime`) decides, so a machine that slept through the day
/// still checks within the hour after waking.
const UPDATE_CHECK_TICK: Duration = Duration::from_secs(60 * 60);

/// The BoxPilot update check (Settings › About; the Settings sidebar dot).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateCheck {
    /// No check has run this session.
    Idle,
    Checking,
    UpToDate,
    /// A newer release exists. It may be one the user skipped — see
    /// [`AppState::update_available`].
    Available(ReleaseInfo),
    /// The last check failed; a short reason.
    Failed(String),
}

/// The one profile fetch in flight, if any. `fetch` numbers it (see
/// `AppState::begin_fetch`); a result lands only while it is still its
/// profile's newest fetch and the profile exists (`fetch_result_applies`).
pub enum UpdateStatus {
    Idle,
    /// Subscription fetch / local import in flight for one profile.
    /// Dropping the `Task` drops its UI-thread continuation, so the result
    /// is never applied — but the blocking fetch itself, one synchronous
    /// job on the background executor, still runs to the end. It only
    /// stages its config (`UpdateOutcome::Changed`), and dropping that
    /// unapplied result deletes the staged file, so nothing is written.
    Updating {
        profile_id: String,
        fetch: u64,
        origin: FetchOrigin,
        _task: Task<()>,
    },
    /// The auto-update loop is fetching this profile. The loop's own future
    /// does the work, so there is no task to hold; it claims this state
    /// before a fetch and releases it after, only if it still holds it (an
    /// import or a profile delete may have replaced it meanwhile). Counts
    /// as in flight like `Updating`, so manual and import fetches don't run
    /// alongside it.
    AutoUpdating {
        profile_id: String,
        fetch: u64,
    },
}

/// How a profile fetch was initiated — failure handling differs by door.
#[derive(Clone, Copy, PartialEq)]
pub enum FetchOrigin {
    /// Row-level ⟳ / Ctrl+U / first fetch after the Add dialog: never
    /// activates, and a failure keeps the profile — the user created it
    /// explicitly and can see and edit it on the Profiles page.
    Manual,
    /// User-confirmed `sing-box://` URI import: activates once the config
    /// lands on disk. If the import *created* the profile and the fetch
    /// fails, the profile is rolled back — Home keys its "Add subscription"
    /// empty card on `has_profiles()`, so a lingering never-fetched profile
    /// would dismiss the card and make the failed import read as a success.
    UriImport { created_profile: bool },
}

/// Inputs the auto-update loop needs from `AppState`, captured on the UI
/// thread before each tick so the blocking fetches can run on the background
/// executor without re-entering `AppState`. Every profile carries its own
/// interval, so the whole list is snapshotted.
struct AutoUpdateSnap {
    profiles: Vec<Profile>,
    app_dir: PathBuf,
    sing_box_path: PathBuf,
    sing_box_version: Option<String>,
    updating: bool,
    starting: bool,
}

/// A deep link arrived (argv or the single-instance pipe) and parsed into a
/// usable import link. The request itself is parked in
/// `AppState::pending_import` — `RootView` consumes it from there and shows
/// the confirm dialog.
pub struct ImportRequested;

/// A launch attempt reached this instance — any launch attempt, whatever it
/// carried. `app_window` responds by showing the main window, reopening it
/// if it was closed to the tray; see `docs/adr/0004-tray-and-window-lifecycle.md`.
pub struct ActivateRequested;

/// A TUN-mode start found no usable granted sing-box copy (Linux, not
/// root; see `core::privilege`), so nothing was started. `RootView` asks the
/// user to grant TUN permission; confirming calls
/// [`AppState::grant_tun_permission`].
#[cfg(target_os = "linux")]
pub struct TunGrantRequested;

/// Top-level reactive state owned by `RootView`. Holds persisted settings,
/// resolved paths, the child entities for the process and log subsystems,
/// and an initial status message that `RootView` consumes once on startup.
pub struct AppState {
    pub settings: AppSettings,
    /// False when the settings file exists but couldn't be read at startup
    /// (see `AppSettings::load`): saves are skipped for the session so the
    /// defaults in memory never replace the user's real file.
    persist_settings: bool,
    pub app_dir: PathBuf,
    pub install_dir: PathBuf,
    /// The bundled sing-box binary's self-reported version, probed once at
    /// startup on the background executor (the MSI-installed binary can't
    /// change mid-run). `None` until the probe lands — or forever, if the
    /// binary is missing. Feeds the Settings ABOUT card and the subscription
    /// User-Agent.
    pub sing_box_version: Option<String>,
    pub update_status: UpdateStatus,
    /// One-shot startup status drained by `RootView::new` after subscribers
    /// are wired. Always `None` after that initial read.
    pub pending_status: Option<(StatusLevel, String)>,
    /// Parsed deep-link import awaiting user confirmation; see
    /// [`ImportRequested`]. A newer link simply replaces an unconfirmed one.
    pub pending_import: Option<ImportRequest>,
    /// Fires once `RootView` has wired its subscribers, releasing the
    /// launch-attempt gate in the deep-link task. `None` after that.
    view_ready: Option<oneshot::Sender<()>>,
    /// sing-box API endpoint + secret of the current (or last) run. `launch`
    /// makes a fresh one for every start, writes it into the runtime config
    /// and hands the same value to every entity below (`set_api`).
    api: SingBoxApi,
    pub process: Entity<ProcessSession>,
    pub logs: Entity<LogBuffer>,
    pub proxy_groups: Entity<ProxyGroups>,
    /// Live runtime status (rates, memory, connections, totals, start time,
    /// version); streamed while sing-box is running.
    pub traffic: Entity<Traffic>,
    /// Clash mode list + current mode; followed while sing-box is running.
    pub clash_mode: Entity<ClashMode>,
    /// Live connection list; streamed while sing-box is running.
    pub connections: Entity<Connections>,
    /// Tools page test runs; enabled while sing-box is running.
    pub network_tools: Entity<NetworkTools>,
    /// Tailscale endpoints of the running config; streamed while running.
    pub tailscale: Entity<TailscaleState>,
    /// OpenConnect / OpenVPN / USB/IP status and sign-in challenges; follows
    /// the process Running/Stopped edges on its own.
    pub vpn: Entity<VpnStatus>,
    /// Last `is_running()` seen by the process observer — detects
    /// Running/Stopped edges so groups + traffic refresh exactly once per
    /// transition.
    groups_saw_running: bool,
    /// Long-lived background task that periodically refreshes the
    /// subscription. Held so it lives as long as `AppState` and is dropped
    /// (cancelled) on app exit.
    _auto_update_task: Task<()>,
    /// Drains launch attempts (argv + single-instance pipe) for the lifetime
    /// of the app.
    _deeplink_task: Task<()>,
    /// A Linux TUN-mode start that hasn't reached `ProcessSession` yet: the
    /// background TUN-plan probe, or the pkexec grant. Holds off a second
    /// start meanwhile; `stop_process` drops (cancels) it.
    #[cfg(target_os = "linux")]
    tun_gate: Option<Task<()>>,
    /// A config change arrived while sing-box was `Preparing` with the old
    /// one: that start was abandoned (`ConfigChangeAction::RedoStart`), and
    /// the process observer starts again once it is back to `Stopped`.
    /// Cleared by `stop_process`.
    restart_pending: bool,
    /// The one automatic redo of a start that lost its API port; the
    /// process observer runs it once sing-box has stopped. Re-armed by
    /// every other start, and by `stop_process`.
    api_port_retry: ApiPortRetry,
    /// Numbers every profile fetch; the last one handed out.
    fetch_seq: u64,
    /// The newest fetch started per profile id. A finished fetch whose
    /// number isn't here any more (newer fetch, profile deleted) is stale.
    latest_fetch: HashMap<String, u64>,
    /// Manual fetches asked for while another was in flight, run in order
    /// as each one finishes (`run_queued_fetches`) — e.g. the Add dialog's
    /// first fetch during an auto-update.
    queued_fetches: VecDeque<String>,
    /// Why each profile's latest fetch failed, until one succeeds (or its
    /// source is edited, or it is deleted). Shown on its update button —
    /// the only trace an auto-update failure leaves, as it raises no toast.
    /// Not persisted: a restart starts with a clean slate.
    fetch_errors: HashMap<String, String>,
    /// The subscription-usage level each profile was last warned about this
    /// session (`warn_usage`): one toast per profile and level, again only
    /// after it changes. Seeded by the startup status.
    usage_warned: HashMap<String, UsageLevel>,
    /// The BoxPilot update check's latest state (`check_for_updates`).
    pub update_check: UpdateCheck,
    /// When the last update check started (manual or automatic); the
    /// automatic loop waits `CHECK_INTERVAL` from here.
    last_update_check: Option<SystemTime>,
    /// The release version this session already toasted about — one toast
    /// per newly found version.
    update_notified: Option<String>,
    /// Wakes `FIRST_CHECK_DELAY` after start, then every `UPDATE_CHECK_TICK`,
    /// and runs a check when one is due and automatic checks are on.
    _update_check_task: Task<()>,
}

impl EventEmitter<StatusEvent> for AppState {}

impl EventEmitter<ImportRequested> for AppState {}

impl EventEmitter<ActivateRequested> for AppState {}

#[cfg(target_os = "linux")]
impl EventEmitter<TunGrantRequested> for AppState {}

impl AppState {
    pub fn new(launches: UnboundedReceiver<LaunchAttempt>, cx: &mut App) -> Entity<Self> {
        let (view_ready_tx, view_ready_rx) = oneshot::channel::<()>();
        // Startup messages below are worded in the UI language: the OS's
        // until the settings say otherwise (an unreadable settings file
        // falls back to "System" anyway).
        crate::i18n::set_language(crate::i18n::resolve(LanguagePreference::System));
        let mut errors = Vec::new();
        let app_dir = get_app_data_dir().unwrap_or_else(|e| {
            errors.push(e);
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
        });
        let install_dir = get_install_dir().unwrap_or_else(|e| {
            errors.push(e);
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
        });

        // Migrate legacy config filenames from older releases. Guard the
        // destination explicitly: `fs::rename` overwrites on both Unix and
        // recent Windows, so without the `!new_config.exists()` check a stale
        // legacy `config_original.json` would clobber a current `config.json`.
        let new_config = app_dir.join(CONFIG_FILENAME);
        if !new_config.exists() {
            let _ = fs::rename(app_dir.join("config_original.json"), &new_config);
        }
        let _ = fs::remove_file(app_dir.join("config_active.json"));

        // A settings file that couldn't be loaded is reported in the startup
        // toast. `persist` false means it is still in place and unread:
        // nothing may be saved over it this session (`save_settings`).
        let loaded = AppSettings::load(&app_dir);
        let mut settings = loaded.settings;
        crate::i18n::set_language(crate::i18n::resolve(settings.language));
        let persist_settings = loaded.persist;
        errors.extend(loaded.problem);

        // Multi-profile migration: the pre-profiles single `config.json`
        // becomes the active profile's `configs/<id>.json`. Same rename
        // guard as above so a re-run can't clobber an already-fetched
        // profile config with the stale legacy file.
        let active_config = profile_config_path(&app_dir, &settings.active_profile_id);
        if !active_config.exists() && new_config.exists() {
            if let Some(parent) = active_config.parent() {
                let _ = create_private_dir(parent);
            }
            let _ = fs::rename(&new_config, &active_config);
        }

        // One-time backfill of `last_updated_secs` for installs that predate
        // it: seed from the config file's mtime so existing profiles read
        // "updated N ago" rather than "never updated". Approximate (a prior
        // sing-box start may have bumped the mtime), but it's a one-shot seed
        // — the next content change overwrites it with the exact time. Runs
        // after the legacy rename so the migrated active profile is covered.
        let mut backfilled = false;
        for profile in &mut settings.profiles {
            if profile.last_updated_secs.is_none() {
                if let Some(secs) =
                    file_mtime(&profile_config_path(&app_dir, &profile.id)).and_then(to_unix_secs)
                {
                    profile.last_updated_secs = Some(secs);
                    backfilled = true;
                }
            }
        }
        if backfilled && persist_settings {
            settings.save(&app_dir);
        }

        let mut usage_warned = HashMap::new();
        let pending_status = if !errors.is_empty() {
            Some((StatusLevel::Error, errors.join("; ")))
        } else if settings.active_profile_id.is_empty() {
            // Fresh install / no profiles — the Home empty state guides the
            // user; no scary "config not found" toast.
            None
        } else if active_config.exists() {
            // A subscription running out outranks "Ready.": the startup
            // toast is the one place it's seen without opening a page.
            match settings.active_profile().and_then(|p| usage_alert(p, now_secs())) {
                Some((level, status_level, message)) => {
                    usage_warned.insert(settings.active_profile_id.clone(), level);
                    Some((status_level, message))
                }
                None => Some((StatusLevel::Success, s().messages.ready.to_string())),
            }
        } else {
            Some((
                StatusLevel::Warning,
                s().messages.config_missing_startup.to_string(),
            ))
        };

        // Replaced by `launch` before any sing-box runs; until then nothing
        // listens for this one (nothing can listen on port 0).
        let api = SingBoxApi::new(0);
        let logs = cx.new(|_| LogBuffer::new(api));
        let process = cx.new({
            let logs = logs.clone();
            move |_| ProcessSession::new(logs)
        });

        let proxy_groups = cx.new(|_| ProxyGroups::new(active_config.clone(), api));
        let traffic = cx.new(|_| Traffic::new(api));
        let clash_mode = cx.new(|_| ClashMode::new(api));
        let connections = cx.new(|_| Connections::new(api));
        let network_tools = cx.new(|_| NetworkTools::new(api));
        let tailscale = cx.new(|_| TailscaleState::new(api));
        let vpn = cx.new({
            let runtime_config = runtime_config_path(&app_dir);
            let process = process.clone();
            move |cx| VpnStatus::new(api, runtime_config, &process, cx)
        });

        cx.new(|cx| {
            // Drive ProxyGroups + Traffic + Connections from process
            // Running/Stopped edges.
            // The edge decision is pure (`core::orchestration`, unit-tested);
            // this observer just executes the returned effects and stores the
            // acted-on state, which is what makes each transition fire once.
            cx.observe(&process, |this: &mut AppState, process, cx| {
                let (running, stopped) = {
                    let process = process.read(cx);
                    (process.is_running(), process.is_stopped())
                };
                let effects = process_edge_effects(this.groups_saw_running, running);
                if !effects.is_empty() {
                    this.groups_saw_running = running;
                }
                for effect in effects {
                    match effect {
                        ProcessEdgeEffect::StartGroups => this
                            .proxy_groups
                            .update(cx, |groups, cx| groups.start(cx)),
                        ProcessEdgeEffect::StartTraffic => {
                            this.traffic.update(cx, |traffic, cx| traffic.start(cx))
                        }
                        ProcessEdgeEffect::ClearGroups => {
                            this.proxy_groups.update(cx, |groups, cx| groups.clear(cx))
                        }
                        ProcessEdgeEffect::StopTraffic => {
                            this.traffic.update(cx, |traffic, cx| traffic.stop(cx))
                        }
                        ProcessEdgeEffect::StartClashMode => {
                            this.clash_mode.update(cx, |mode, cx| mode.start(cx))
                        }
                        ProcessEdgeEffect::ClearClashMode => {
                            this.clash_mode.update(cx, |mode, cx| mode.clear(cx))
                        }
                        ProcessEdgeEffect::StartConnections => this
                            .connections
                            .update(cx, |connections, cx| connections.start(cx)),
                        ProcessEdgeEffect::StopConnections => this
                            .connections
                            .update(cx, |connections, cx| connections.stop(cx)),
                        ProcessEdgeEffect::StartNetworkTools => this
                            .network_tools
                            .update(cx, |tools, cx| tools.start(cx)),
                        ProcessEdgeEffect::StopNetworkTools => this
                            .network_tools
                            .update(cx, |tools, cx| tools.stop(cx)),
                        ProcessEdgeEffect::StartTailscale => {
                            this.tailscale.update(cx, |tailscale, cx| tailscale.start(cx))
                        }
                        ProcessEdgeEffect::ClearTailscale => {
                            this.tailscale.update(cx, |tailscale, cx| tailscale.clear(cx))
                        }
                        ProcessEdgeEffect::StartLogs => {
                            this.logs.update(cx, |logs, cx| logs.start_api(cx))
                        }
                        ProcessEdgeEffect::StopLogs => {
                            this.logs.update(cx, |logs, cx| logs.stop_api(cx))
                        }
                    }
                }
                // The start abandoned for a config change has finished its
                // prep: start again, with the new config.
                if this.restart_pending && stopped {
                    this.restart_pending = false;
                    this.start_process(cx);
                } else if stopped {
                    this.redo_start_if_api_port_lost(cx);
                }
            })
            .detach();

            cx.subscribe(
                &process,
                |this: &mut AppState, process, lost: &ApiPortLost, cx| {
                    this.api_port_retry = this.api_port_retry.port_lost(lost.port, this.api.port());
                    // Usually sing-box is still exiting, and the observer
                    // redoes the start once it's stopped; the report can
                    // also come last.
                    if process.read(cx).is_stopped() {
                        this.redo_start_if_api_port_lost(cx);
                    }
                },
            )
            .detach();

            let auto_update_task = cx.spawn(async move |this, cx| {
                // Per-profile fetch clocks. A profile's first sighting only
                // arms its clock (no fetch) — same "wait one full interval
                // after launch/creation" behavior the single-profile loop
                // had. Entries for deleted profiles linger harmlessly.
                let mut last_attempt: HashMap<String, Instant> = HashMap::new();
                loop {
                    cx.background_executor().timer(AUTO_UPDATE_TICK).await;

                    // Snapshot the inputs we need on the UI thread, so the
                    // blocking fetches can run on the background executor
                    // without holding any locks. `this` is a `WeakEntity`,
                    // so each `update` call returns `Result` — `Err` means
                    // the entity has been released and the app is shutting
                    // down, so we exit the loop.
                    let snap = this.update(cx, |state: &mut AppState, cx| AutoUpdateSnap {
                        profiles: state.settings.profiles.clone(),
                        app_dir: state.app_dir.clone(),
                        sing_box_path: state.sing_box_path(),
                        sing_box_version: state.sing_box_version.clone(),
                        updating: state.is_updating(),
                        starting: state.process.read(cx).is_starting(),
                    });
                    let Ok(snap) = snap else {
                        return;
                    };

                    if snap.updating || snap.starting {
                        continue;
                    }

                    for profile in snap.profiles {
                        if !last_attempt.contains_key(&profile.id) {
                            last_attempt.insert(profile.id.clone(), Instant::now());
                            continue;
                        }
                        // Local profiles are never auto-updated; only a Remote
                        // with a URL and a non-zero interval is polled.
                        let ProfileSource::Remote {
                            url,
                            auto_update_interval_minutes,
                            ..
                        } = &profile.source
                        else {
                            continue;
                        };
                        let interval = *auto_update_interval_minutes;
                        if interval == 0 || url.trim().is_empty() {
                            continue;
                        }
                        let due =
                            last_attempt[&profile.id].elapsed().as_secs() >= interval * 60;
                        if !due {
                            continue;
                        }

                        // Claim the shared in-flight state, so a manual
                        // refresh or an import can't fetch alongside us (the
                        // snapshot above is a whole fetch or more old by
                        // now). Busy → leave this and the remaining profiles
                        // for the next tick; their clocks stay due.
                        let claimed = this.update(cx, |state: &mut AppState, cx| {
                            if state.is_updating() || state.process.read(cx).is_starting() {
                                return None;
                            }
                            let fetch = state.begin_fetch(&profile.id);
                            state.update_status = UpdateStatus::AutoUpdating {
                                profile_id: profile.id.clone(),
                                fetch,
                            };
                            // The route as of now, by the profile's current
                            // setting (the snapshot may be a fetch old).
                            let current = state
                                .settings
                                .profiles
                                .iter()
                                .find(|p| p.id == profile.id)
                                .unwrap_or(&profile);
                            let proxy = state.subscription_proxy(current, cx);
                            cx.notify();
                            Some((fetch, proxy))
                        });
                        let (fetch, proxy) = match claimed {
                            Ok(Some(claim)) => claim,
                            Ok(None) => break,
                            Err(_) => return,
                        };
                        last_attempt.insert(profile.id.clone(), Instant::now());

                        let url = url.trim().to_string();
                        let app_dir = snap.app_dir.clone();
                        let config_path = profile_config_path(&snap.app_dir, &profile.id);
                        let sing_box = snap.sing_box_path.clone();
                        let sing_box_version = snap.sing_box_version.clone();
                        let result = cx
                            .background_executor()
                            .spawn(async move {
                                perform_update(
                                    &url,
                                    proxy.as_deref(),
                                    &app_dir,
                                    &config_path,
                                    Some(&sing_box),
                                    sing_box_version.as_deref(),
                                )
                            })
                            .await;

                        let profile_id = profile.id;
                        let exited = this
                            .update(cx, |state, cx| {
                                // Deleted meanwhile, or an import of it took
                                // over: drop the result, and with it the
                                // staged config.
                                if !state.fetch_applies(&profile_id, fetch) {
                                    state.release_auto_update(fetch, cx);
                                    state.run_queued_fetches(cx);
                                    return;
                                }
                                let (landed, usage) = split_fetched(result);
                                state.release_auto_update(fetch, cx);
                                let usage_changed = landed.is_ok()
                                    && state.record_usage(&profile_id, usage);
                                state.record_fetch_result(
                                    &profile_id,
                                    landed.as_ref().map(|_| ()).map_err(String::as_str),
                                );
                                match landed {
                                    Ok(true) => {
                                        state.stamp_profile_updated(&profile_id);
                                        state.save_settings();
                                        // Restart/toast only matter for the
                                        // profile that's actually in use;
                                        // background profiles refresh silently.
                                        if state.settings.active_profile_id == profile_id {
                                            cx.emit(StatusEvent {
                                                level: StatusLevel::Success,
                                                message: s().messages.auto_updated.to_string(),
                                            });
                                            state.restart_if_running(cx);
                                        }
                                    }
                                    Ok(false) => {
                                        // Silent: no config was written. It
                                        // is checked as of now, though, and
                                        // the usage reading may have moved.
                                        state.save_settings();
                                        cx.notify();
                                    }
                                    Err(err) => {
                                        // No toast — auto-update can fail
                                        // repeatedly when offline; we don't
                                        // want to spam the user. The update
                                        // button shows it instead.
                                        eprintln!("Auto-update failed: {}", err);
                                        cx.notify();
                                    }
                                }
                                if usage_changed {
                                    state.warn_usage(&profile_id, cx);
                                }
                                state.run_queued_fetches(cx);
                            })
                            .is_err();
                        if exited {
                            return;
                        }
                    }
                }
            });

            let deeplink_task = cx.spawn(async move |this, cx| {
                // Gate: consume nothing until `RootView` has subscribed.
                // gpui drops events emitted before a subscriber exists, and
                // the window opens several executor turns after this task is
                // spawned — an argv link, or one the pipe forwards during
                // that window, would otherwise vanish without a trace.
                // Attempts queue harmlessly in the unbounded channel
                // meanwhile. `Err` = the sender was dropped with no view
                // (app shutting down).
                if view_ready_rx.await.is_err() {
                    return;
                }

                let mut launches = launches;
                while let Some(attempt) = launches.next().await {
                    // Every launch attempt ends with the user seeing the
                    // window — enforced once, here, for every arm. What the
                    // attempt carried only decides what is shown next.
                    let delivered = this.update(cx, |state: &mut AppState, cx| {
                        cx.emit(ActivateRequested);
                        if let LaunchAttempt::DeepLink(uri) = attempt {
                            state.handle_deeplink(&uri, cx);
                        }
                    });
                    if delivered.is_err() {
                        return;
                    }
                }
            });

            // Probe the bundled sing-box's version once (ABOUT card + real
            // version in the subscription User-Agent). One shot is enough:
            // the binary is MSI-installed and can't change mid-run.
            let sing_path = install_dir.join(SING_EXECUTABLE);
            cx.spawn(async move |this, cx| {
                let version = cx
                    .background_executor()
                    .spawn(async move { query_sing_box_version(&sing_path) })
                    .await;
                if let Some(version) = version {
                    let _ = this.update(cx, |state: &mut AppState, cx| {
                        state.sing_box_version = Some(version);
                        cx.notify();
                    });
                }
            })
            .detach();

            // BoxPilot update check: first shortly after start, then daily.
            // The loop itself never touches the network; the check does,
            // on the background executor, and only while enabled.
            let update_check_task = cx.spawn(async move |this, cx| {
                cx.background_executor().timer(FIRST_CHECK_DELAY).await;
                loop {
                    let alive = this.update(cx, |state: &mut AppState, cx| {
                        state.check_for_updates_if_due(cx)
                    });
                    if alive.is_err() {
                        return;
                    }
                    cx.background_executor().timer(UPDATE_CHECK_TICK).await;
                }
            });

            Self {
                settings,
                persist_settings,
                app_dir,
                install_dir,
                sing_box_version: None,
                update_status: UpdateStatus::Idle,
                pending_status,
                pending_import: None,
                view_ready: Some(view_ready_tx),
                api,
                process,
                logs,
                proxy_groups,
                traffic,
                clash_mode,
                connections,
                network_tools,
                tailscale,
                vpn,
                groups_saw_running: false,
                _auto_update_task: auto_update_task,
                _deeplink_task: deeplink_task,
                #[cfg(target_os = "linux")]
                tun_gate: None,
                restart_pending: false,
                api_port_retry: ApiPortRetry::default(),
                fetch_seq: 0,
                latest_fetch: HashMap::new(),
                queued_fetches: VecDeque::new(),
                fetch_errors: HashMap::new(),
                usage_warned,
                update_check: UpdateCheck::Idle,
                last_update_check: None,
                update_notified: None,
                _update_check_task: update_check_task,
            }
        })
    }

    pub fn is_updating(&self) -> bool {
        !matches!(self.update_status, UpdateStatus::Idle)
    }

    /// 正在拉订阅的 profile id(驱动 Profiles 页行级 spinner)。
    pub fn updating_profile_id(&self) -> Option<&str> {
        match &self.update_status {
            UpdateStatus::Updating { profile_id, .. }
            | UpdateStatus::AutoUpdating { profile_id, .. } => Some(profile_id),
            UpdateStatus::Idle => None,
        }
    }

    /// Why `profile_id`'s latest fetch failed; `None` once one succeeded.
    pub fn fetch_error(&self, profile_id: &str) -> Option<&str> {
        self.fetch_errors.get(profile_id).map(String::as_str)
    }

    /// Record how a fetch of `profile_id` ended: success clears its error
    /// and stamps it checked (fresh as of now, changed or not), failure
    /// keeps the reason. The caller persists via `save_settings`.
    fn record_fetch_result(&mut self, profile_id: &str, result: Result<(), &str>) {
        match result {
            Ok(()) => {
                self.fetch_errors.remove(profile_id);
                if let Some(profile) = self
                    .settings
                    .profiles
                    .iter_mut()
                    .find(|p| p.id == profile_id)
                {
                    profile.last_checked_secs = to_unix_secs(SystemTime::now());
                }
            }
            Err(reason) => {
                self.fetch_errors
                    .insert(profile_id.to_string(), reason.to_string());
            }
        }
    }

    /// Number a new fetch of `profile_id` and make it that profile's newest:
    /// any older fetch of it still running becomes stale.
    fn begin_fetch(&mut self, profile_id: &str) -> u64 {
        self.fetch_seq += 1;
        self.latest_fetch.insert(profile_id.to_string(), self.fetch_seq);
        self.fetch_seq
    }

    /// Whether `fetch` of `profile_id` may still land its config; see
    /// `fetch_result_applies`.
    fn fetch_applies(&self, profile_id: &str, fetch: u64) -> bool {
        fetch_result_applies(
            self.latest_fetch.get(profile_id).copied(),
            fetch,
            self.settings.profiles.iter().any(|p| p.id == profile_id),
        )
    }

    /// End the auto-update loop's claim on its `fetch` — unless an import or
    /// a delete has replaced it meanwhile, whose state (and task) must stay.
    fn release_auto_update(&mut self, fetch: u64, cx: &mut Context<Self>) {
        if matches!(
            &self.update_status,
            UpdateStatus::AutoUpdating { fetch: held, .. } if *held == fetch
        ) {
            self.update_status = UpdateStatus::Idle;
            cx.notify();
        }
    }

    /// Start the queued manual fetches, oldest first, until one is in
    /// flight. Called whenever a fetch ends. Entries whose profile is gone
    /// or has nothing to fetch fall through `update_profile`'s guards.
    fn run_queued_fetches(&mut self, cx: &mut Context<Self>) {
        while !self.is_updating() {
            let Some(id) = self.queued_fetches.pop_front() else {
                return;
            };
            self.update_profile(id, FetchOrigin::Manual, cx);
        }
    }

    pub fn save_settings(&self) {
        if self.persist_settings {
            self.settings.save(&self.app_dir);
        }
    }

    /// Stamp `profile_id` with the current time as its last-content-change
    /// moment. Called only when a fetch/import actually wrote new bytes
    /// (`UpdateOutcome::Changed`); the caller persists via `save_settings`.
    fn stamp_profile_updated(&mut self, profile_id: &str) {
        if let Some(profile) = self.settings.profiles.iter_mut().find(|p| p.id == profile_id) {
            profile.last_updated_secs = to_unix_secs(SystemTime::now());
        }
    }

    /// Store a fetch's `subscription-userinfo` reading on `profile_id`
    /// (Remote profiles only; a server that stopped reporting clears the old
    /// reading). `true` when the stored value changed — the caller persists
    /// via `save_settings`.
    fn record_usage(&mut self, profile_id: &str, usage: Option<SubscriptionUsage>) -> bool {
        let Some(profile) = self.settings.profiles.iter_mut().find(|p| p.id == profile_id) else {
            return false;
        };
        if !matches!(profile.source, ProfileSource::Remote { .. }) || profile.usage == usage {
            return false;
        }
        profile.usage = usage;
        true
    }

    /// Toast once when the active profile's subscription crosses into
    /// Warning or Critical (traffic nearly / fully used, expiring / expired);
    /// again only after its level changes. Back to Normal (renewed) re-arms.
    fn warn_usage(&mut self, profile_id: &str, cx: &mut Context<Self>) {
        if self.settings.active_profile_id != profile_id {
            return;
        }
        let Some(profile) = self.settings.profiles.iter().find(|p| p.id == profile_id) else {
            return;
        };
        match usage_alert(profile, now_secs()) {
            None => {
                self.usage_warned.remove(profile_id);
            }
            Some((level, status_level, message)) => {
                if self.usage_warned.get(profile_id) != Some(&level) {
                    self.usage_warned.insert(profile_id.to_string(), level);
                    cx.emit(StatusEvent {
                        level: status_level,
                        message,
                    });
                }
            }
        }
    }

    /// The active profile's canonical on-disk config (`configs/<id>.json`).
    /// Written only by subscription fetches / local imports — never by a
    /// process start, so its bytes and mtime track real content changes.
    pub fn active_config_path(&self) -> PathBuf {
        profile_config_path(&self.app_dir, &self.settings.active_profile_id)
    }

    /// The sing-box binary next to our own executable.
    fn sing_box_path(&self) -> PathBuf {
        self.install_dir.join(SING_EXECUTABLE)
    }

    /// Read the active profile's canonical config, inject mode-specific
    /// inbounds, cache_file, and BoxPilot's `api` service on a freshly
    /// picked free port with a fresh secret, and write the result to the
    /// separate runtime config (the `-c` target). Returns that path and the
    /// API endpoint it was written for. Done synchronously immediately
    /// before the prep task — order matters, do not move to the background
    /// executor.
    fn write_runtime_config(&self) -> Result<(PathBuf, SingBoxApi), String> {
        let config_path = self.active_config_path();
        let data = fs::read_to_string(&config_path)
            .map_err(|e| (s().errors.read_failed)(&config_path.display().to_string(), &e.to_string()))?;
        let api = SingBoxApi::new(pick_api_port(&data, self.settings.proxy_port)?);
        let opts = RuntimeOptions::new(&self.settings, api);
        let prepared = prepare_config(&data, opts)?;
        let runtime_path = runtime_config_path(&self.app_dir);
        save_runtime_config(&runtime_path, &prepared)
            .map_err(|e| (s().errors.write_failed)(&runtime_path.display().to_string(), &e.to_string()))?;
        Ok((runtime_path, opts.api))
    }

    /// Give every entity that calls the sing-box API this run's endpoint.
    /// Their streams start on the Running edge, after this.
    fn set_api(&mut self, api: SingBoxApi, cx: &mut Context<Self>) {
        self.api = api;
        self.proxy_groups.update(cx, |groups, _| groups.set_api(api));
        self.traffic.update(cx, |traffic, _| traffic.set_api(api));
        self.clash_mode.update(cx, |mode, _| mode.set_api(api));
        self.connections
            .update(cx, |connections, _| connections.set_api(api));
        self.network_tools.update(cx, |tools, _| tools.set_api(api));
        self.tailscale.update(cx, |tailscale, _| tailscale.set_api(api));
        self.logs.update(cx, |logs, _| logs.set_api(api));
        self.vpn.update(cx, |vpn, _| vpn.set_api(api));
    }

    /// Validate paths, prepare the config, and ask the `ProcessSession` to
    /// start. No-op if a process is already running or starting. Every start
    /// — toggle, and the restarts after a settings / profile change or an
    /// auto-update — comes through here, so the Linux TUN gate below covers
    /// them all.
    pub fn start_process(&mut self, cx: &mut Context<Self>) {
        if !self.process.read(cx).is_stopped() {
            return;
        }
        #[cfg(target_os = "linux")]
        if self.tun_gate.is_some() {
            return;
        }

        if !self.settings.has_profiles() {
            cx.emit(StatusEvent {
                level: StatusLevel::Warning,
                message: s().messages.add_subscription_first.to_string(),
            });
            return;
        }

        let sing_path = self.sing_box_path();
        if !sing_path.exists() {
            cx.emit(StatusEvent {
                level: StatusLevel::Error,
                message: (s().messages.sing_box_not_found)(
                    SING_EXECUTABLE,
                    &sing_path.display().to_string(),
                ),
            });
            return;
        }

        // The runtime config carries the `api` service, which older binaries
        // reject; say so instead of letting sing-box die on an unknown type.
        // Unknown version (query failed / still running) → let it try.
        if let Some(version) = self.sing_box_version.as_deref() {
            if !supports_api_service(version) {
                cx.emit(StatusEvent {
                    level: StatusLevel::Error,
                    message: (s().messages.sing_box_too_old)(version, MIN_SING_BOX_VERSION),
                });
                return;
            }
        }

        if !self.active_config_path().exists() {
            cx.emit(StatusEvent {
                level: StatusLevel::Error,
                message: s().messages.config_missing.to_string(),
            });
            return;
        }

        // Linux TUN mode needs CAP_NET_ADMIN, which a normal user's bundled
        // sing-box lacks: start the granted copy instead, or ask for the
        // grant first. Proxy mode always runs the bundled sing-box.
        #[cfg(target_os = "linux")]
        if !self.settings.proxy_mode {
            self.start_tun_gated(sing_path, cx);
            return;
        }

        self.launch(sing_path, cx);
    }

    /// Write the runtime config and hand the start to `ProcessSession`,
    /// running `sing_path`. The `-D` working dir stays the user's data dir
    /// whichever binary runs, so `cache.db` and friends stay user-owned.
    fn launch(&mut self, sing_path: PathBuf, cx: &mut Context<Self>) {
        self.api_port_retry = self.api_port_retry.launched();
        let (config_path, api_port) = match self.write_runtime_config() {
            Ok((path, api)) => {
                self.set_api(api, cx);
                (path, api.port())
            }
            Err(e) => {
                cx.emit(StatusEvent {
                    level: StatusLevel::Error,
                    message: e,
                });
                return;
            }
        };

        let pending = PendingStart {
            sing_path,
            config_path,
            working_dir: self.app_dir.clone(),
            proxy_mode: self.settings.proxy_mode,
            set_system_proxy: self.settings.set_system_proxy,
            api_port,
        };

        self.process.update(cx, |p, cx| p.start(pending, cx));
        cx.notify();
    }

    /// Decide which sing-box a Linux TUN-mode start runs (blocking probes,
    /// so on the background executor), then start it — or, with no usable
    /// granted copy, start nothing and ask the view to offer the grant.
    #[cfg(target_os = "linux")]
    fn start_tun_gated(&mut self, bundled: PathBuf, cx: &mut Context<Self>) {
        let bundled_version = self.sing_box_version.clone();
        self.tun_gate = Some(cx.spawn(async move |this, cx| {
            let plan = cx
                .background_executor()
                .spawn(async move { evaluate_tun_plan(&bundled, bundled_version) })
                .await;
            let _ = this.update(cx, |state, cx| {
                state.tun_gate = None;
                cx.notify();
                match plan {
                    TunPlan::UseBundled(path) | TunPlan::UsePrivilegedCopy(path) => {
                        state.launch_after_gate(path, cx)
                    }
                    // Switched to Proxy mode meanwhile: no grant needed.
                    TunPlan::NeedsGrant if state.settings.proxy_mode => state.start_process(cx),
                    TunPlan::NeedsGrant => cx.emit(TunGrantRequested),
                }
            });
        }));
        // Shows as Starting (`is_starting`) until it resolves.
        cx.notify();
    }

    /// The user confirmed the TUN grant: install + setcap the copy through
    /// pkexec (blocks on the password prompt, so on the background
    /// executor), then start with it. Failure or a dismissed prompt: error
    /// toast, nothing started.
    #[cfg(target_os = "linux")]
    pub fn grant_tun_permission(&mut self, cx: &mut Context<Self>) {
        if self.tun_gate.is_some() || !self.process.read(cx).is_stopped() {
            return;
        }
        let bundled = self.sing_box_path();
        self.tun_gate = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { run_grant(&bundled) })
                .await;
            let _ = this.update(cx, |state, cx| {
                state.tun_gate = None;
                cx.notify();
                match result {
                    Ok(()) => state.launch_after_gate(PathBuf::from(PRIVILEGED_COPY_PATH), cx),
                    Err(message) => cx.emit(StatusEvent {
                        level: StatusLevel::Error,
                        message,
                    }),
                }
            });
        }));
        cx.notify();
    }

    /// Launch the sing-box a resolved TUN gate picked. Settings changed
    /// while the gate was pending need nothing here — `launch` writes the
    /// runtime config from the current ones — except a switch to Proxy
    /// mode, which runs the bundled sing-box through the usual start.
    #[cfg(target_os = "linux")]
    fn launch_after_gate(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.settings.proxy_mode {
            self.start_process(cx);
        } else {
            self.launch(path, cx);
        }
    }

    /// Stop sing-box, or the start in progress: a pending TUN gate is
    /// dropped, a `Preparing` start is abandoned (`ProcessSession::stop`).
    pub fn stop_process(&mut self, cx: &mut Context<Self>) {
        self.restart_pending = false;
        self.api_port_retry = ApiPortRetry::Armed;
        #[cfg(target_os = "linux")]
        {
            self.tun_gate = None;
        }
        self.process.update(cx, |p, cx| p.stop(cx));
        cx.notify();
    }

    /// sing-box is stopped: if its run lost the API port it was given, start
    /// once more, which picks a fresh one (`ApiPortRetry`).
    fn redo_start_if_api_port_lost(&mut self, cx: &mut Context<Self>) {
        if self.api_port_retry.take_redo() {
            cx.emit(StatusEvent {
                level: StatusLevel::Info,
                message: s().messages.api_port_retry.to_string(),
            });
            self.start_process(cx);
        }
    }

    pub fn toggle_process(&mut self, cx: &mut Context<Self>) {
        let process = self.process.read(cx);
        if process.is_running() {
            self.stop_process(cx);
        } else if process.is_stopped() {
            self.start_process(cx);
        }
        // If currently `Preparing`, ignore — let it complete.
    }

    /// The Logs page's Clear: empty the view, and while sing-box runs empty
    /// its own log buffer too (`ClearLogs`), or a re-subscribe would replay
    /// the cleared lines. Failure: error toast; the view stays cleared.
    pub fn clear_logs(&mut self, cx: &mut Context<Self>) {
        self.logs.update(cx, |logs, cx| logs.clear(cx));
        if !self.process.read(cx).is_running() {
            return;
        }
        let api = self.api;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { api.clear_logs() })
                .await;
            if let Err(e) = result {
                let _ = this.update(cx, |_, cx| {
                    cx.emit(StatusEvent {
                        level: StatusLevel::Error,
                        message: (s().messages.clear_logs_failed)(&e.to_string()),
                    });
                });
            }
        })
        .detach();
    }

    /// The running sing-box's local proxy, for BoxPilot's own requests
    /// (`local_proxy`); `None` unless sing-box runs. The port is the
    /// setting's: changing it restarts sing-box, so a running one listens
    /// there.
    fn sing_box_proxy(&self, cx: &App) -> Option<String> {
        local_proxy(self.process.read(cx).is_running(), self.settings.proxy_port)
    }

    /// The route for a fetch of `profile`, decided on the UI thread when the
    /// fetch starts: the running sing-box if the profile updates through it
    /// (`Profile::updates_via_sing_box`), else direct.
    fn subscription_proxy(&self, profile: &Profile, cx: &App) -> Option<String> {
        if profile.updates_via_sing_box() {
            self.sing_box_proxy(cx)
        } else {
            None
        }
    }

    /// A start is under way: sing-box `Preparing`, or a Linux TUN gate
    /// (plan probe or pkexec prompt) still pending. What Home and the
    /// sidebar show as Starting, and what holds the power button off.
    pub fn is_starting(&self, cx: &App) -> bool {
        matches!(self.start_phase(cx), StartPhase::Gated | StartPhase::Preparing)
    }

    /// How far a start has got; see `StartPhase`.
    fn start_phase(&self, cx: &App) -> StartPhase {
        let process = self.process.read(cx);
        if process.is_running() {
            return StartPhase::Running;
        }
        if process.is_starting() {
            return StartPhase::Preparing;
        }
        #[cfg(target_os = "linux")]
        if self.tun_gate.is_some() {
            return StartPhase::Gated;
        }
        StartPhase::Idle
    }

    /// Make a change to the runtime config's inputs take effect: restart a
    /// running sing-box, and redo a start that is still preparing with the
    /// old runtime config (it would otherwise come up with stale values).
    fn restart_if_running(&mut self, cx: &mut Context<Self>) {
        match config_change_action(self.start_phase(cx)) {
            ConfigChangeAction::Nothing => {}
            ConfigChangeAction::Restart => {
                self.stop_process(cx);
                self.start_process(cx);
            }
            ConfigChangeAction::RedoStart => {
                self.process.update(cx, |p, _| p.abandon_start());
                self.restart_pending = true;
            }
        }
    }

    pub fn set_proxy_mode(&mut self, value: bool, cx: &mut Context<Self>) {
        if self.settings.proxy_mode == value {
            return;
        }
        self.settings.proxy_mode = value;
        self.save_settings();
        self.restart_if_running(cx);
        cx.notify();
    }

    /// Settings 页改本地代理端口:同 `set_proxy_mode`——持久化并在
    /// 运行中(或启动中)立即重启生效(注册表系统代理由 sing-box 按入站端口自写)。
    pub fn set_proxy_port(&mut self, value: u16, cx: &mut Context<Self>) {
        if self.settings.proxy_port == value {
            return;
        }
        self.settings.proxy_port = value;
        self.save_settings();
        self.restart_if_running(cx);
        cx.notify();
    }

    /// Settings 页的 TUN IPv6 开关:同 `set_proxy_mode`——持久化并在运行中
    /// (或启动中)重启生效。Proxy 模式下改它同样合法,只是要等切回 TUN 才看得出区别。
    pub fn set_tun_ipv6(&mut self, value: bool, cx: &mut Context<Self>) {
        if self.settings.tun_ipv6 == value {
            return;
        }
        self.settings.tun_ipv6 = value;
        self.save_settings();
        self.restart_if_running(cx);
        cx.notify();
    }

    pub fn set_system_proxy(&mut self, value: bool, cx: &mut Context<Self>) {
        if self.settings.set_system_proxy == value {
            return;
        }
        self.settings.set_system_proxy = value;
        self.save_settings();
        self.restart_if_running(cx);
        cx.notify();
    }

    /// Settings › General "Appearance". Persists only; applying the theme
    /// to the windows is the caller's job (`ui::theme`).
    pub fn set_theme(&mut self, value: ThemePreference, cx: &mut Context<Self>) {
        if self.settings.theme == value {
            return;
        }
        self.settings.theme = value;
        self.save_settings();
        cx.notify();
    }

    /// Settings › General "Language". Persists only; switching the UI
    /// strings is the caller's job.
    pub fn set_language(&mut self, value: LanguagePreference, cx: &mut Context<Self>) {
        if self.settings.language == value {
            return;
        }
        self.settings.language = value;
        self.save_settings();
        cx.notify();
    }

    /// Settings › Network "Allow LAN connections": changes the inbound's
    /// listen address, so a running (or starting) sing-box is restarted.
    pub fn set_allow_lan(&mut self, value: bool, cx: &mut Context<Self>) {
        if self.settings.allow_lan == value {
            return;
        }
        self.settings.allow_lan = value;
        self.save_settings();
        self.restart_if_running(cx);
        cx.notify();
    }

    /// Settings › General "Close button".
    pub fn set_close_action(&mut self, value: CloseAction, cx: &mut Context<Self>) {
        if self.settings.close_action == value {
            return;
        }
        self.settings.close_action = value;
        self.save_settings();
        cx.notify();
    }

    /// Settings › About "Check for updates automatically". Turning it on
    /// checks right away when a check is due (none yet, or the last one is a
    /// day old) — the loop would otherwise only notice at its next tick.
    pub fn set_check_updates(&mut self, value: bool, cx: &mut Context<Self>) {
        if self.settings.check_updates == value {
            return;
        }
        self.settings.check_updates = value;
        self.save_settings();
        cx.notify();
        if value {
            self.check_for_updates_if_due(cx);
        }
    }

    /// The release to offer — newer than this BoxPilot and not skipped.
    /// Drives the Settings sidebar dot and the About card's Skip button.
    pub fn update_available(&self) -> Option<&ReleaseInfo> {
        match &self.update_check {
            UpdateCheck::Available(info)
                if !self
                    .settings
                    .skipped_update_version
                    .as_deref()
                    .is_some_and(|skipped| same_version(skipped, &info.version)) =>
            {
                Some(info)
            }
            _ => None,
        }
    }

    /// The automatic check: runs only while enabled and when the last check
    /// (if any) is `CHECK_INTERVAL` old. A clock set back counts as due.
    fn check_for_updates_if_due(&mut self, cx: &mut Context<Self>) {
        if !self.settings.check_updates {
            return;
        }
        let due = self.last_update_check.is_none_or(|last| {
            SystemTime::now()
                .duration_since(last)
                .map_or(true, |elapsed| elapsed >= CHECK_INTERVAL)
        });
        if due {
            self.check_for_updates(false, cx);
        }
    }

    /// Ask GitHub for the latest BoxPilot release, off the UI thread. An
    /// automatic check (`manual` false) never runs while the setting is off;
    /// "Check now" always does. One check at a time. Goes through sing-box's
    /// local proxy while it runs, in either mode (`local_proxy`), directly
    /// otherwise. A newer release that isn't skipped gets one Info toast per
    /// session.
    pub fn check_for_updates(&mut self, manual: bool, cx: &mut Context<Self>) {
        if self.update_check == UpdateCheck::Checking || !(manual || self.settings.check_updates) {
            return;
        }
        let proxy = self.sing_box_proxy(cx);
        self.update_check = UpdateCheck::Checking;
        self.last_update_check = Some(SystemTime::now());
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { fetch_latest(proxy.as_deref()) })
                .await;
            let _ = this.update(cx, |state: &mut AppState, cx| {
                state.finish_update_check(result, cx)
            });
        })
        .detach();
    }

    fn finish_update_check(&mut self, result: Result<ReleaseInfo, String>, cx: &mut Context<Self>) {
        self.update_check = match result {
            Ok(info) if is_newer(&info.version, CURRENT_VERSION) => {
                if should_notify(
                    &info.version,
                    CURRENT_VERSION,
                    self.settings.skipped_update_version.as_deref(),
                    self.update_notified.as_deref(),
                ) {
                    self.update_notified = Some(info.version.clone());
                    cx.emit(StatusEvent {
                        level: StatusLevel::Info,
                        message: (s().updates.available_toast)(&info.version),
                    });
                }
                UpdateCheck::Available(info)
            }
            Ok(_) => UpdateCheck::UpToDate,
            Err(reason) => {
                eprintln!("Update check failed: {reason}");
                UpdateCheck::Failed(reason)
            }
        };
        cx.notify();
    }

    /// "Skip this version" (`Some(version)`), or forget a skipped one (`None`).
    pub fn skip_update_version(&mut self, version: Option<String>, cx: &mut Context<Self>) {
        if self.settings.skipped_update_version == version {
            return;
        }
        self.settings.skipped_update_version = version;
        self.save_settings();
        cx.notify();
    }

    /// Delete every `*.db` file in `app_dir`. sing-box stores its DNS/fakeip
    /// cache as `cache.db`. No-op when the process is running or preparing —
    /// the file is locked on Windows and the UI button is disabled in that
    /// state, but we double-check here so programmatic dispatch can't bypass it.
    pub fn clear_cache(&mut self, cx: &mut Context<Self>) {
        if !self.process.read(cx).is_stopped() {
            cx.emit(StatusEvent {
                level: StatusLevel::Warning,
                message: s().messages.disconnect_to_clear_cache.to_string(),
            });
            return;
        }

        let entries = match fs::read_dir(&self.app_dir) {
            Ok(it) => it,
            Err(e) => {
                cx.emit(StatusEvent {
                    level: StatusLevel::Error,
                    message: (s().messages.read_app_dir_failed)(&e.to_string()),
                });
                return;
            }
        };

        let mut deleted = 0usize;
        let mut errors: Vec<String> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("db") {
                match fs::remove_file(&path) {
                    Ok(_) => deleted += 1,
                    Err(e) => errors.push(format!("{}: {}", path.display(), e)),
                }
            }
        }

        let (level, message) = if !errors.is_empty() {
            (
                StatusLevel::Error,
                (s().messages.delete_cache_failed)(&errors.join("; ")),
            )
        } else if deleted > 0 {
            (
                StatusLevel::Success,
                (s().messages.cache_cleared)(deleted as u64),
            )
        } else {
            (StatusLevel::Info, s().messages.no_cache.to_string())
        };
        cx.emit(StatusEvent { level, message });
        cx.notify();
    }

    /// Fetch the active profile's subscription (Home update button, Ctrl+U).
    pub fn update_subscription(&mut self, cx: &mut Context<Self>) {
        if !self.settings.has_profiles() {
            cx.emit(StatusEvent {
                level: StatusLevel::Warning,
                message: s().messages.add_subscription_first.to_string(),
            });
            return;
        }
        let id = self.settings.active_profile_id.clone();
        self.update_profile(id, FetchOrigin::Manual, cx);
    }

    /// Kick off a subscription fetch / local re-import for `profile_id` on
    /// the background executor, landing in `configs/<id>.json`. Fetching does
    /// NOT activate the profile and never touches a running process — except
    /// for `FetchOrigin::UriImport`, which activates once the config landed
    /// on disk, because activating before the fetch would point a running
    /// sing-box at a config that doesn't exist yet; on failure it rolls the
    /// profile back if this import created it (see [`FetchOrigin`]). The
    /// `Task<()>` is stored in `update_status`(连同 profile id,供行级
    /// spinner);dropping it (an import taking over, the profile deleted)
    /// discards the result, see [`UpdateStatus::Updating`]. A manual fetch
    /// asked for while another is in flight is queued behind it.
    pub fn update_profile(
        &mut self,
        profile_id: String,
        origin: FetchOrigin,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.settings.profiles.iter().find(|p| p.id == profile_id) else {
            return;
        };
        let profile_name = profile.name.clone();
        let source = profile.source.clone();
        let proxy = self.subscription_proxy(profile, cx);
        // Reject an empty source up front, with a source-appropriate message.
        match &source {
            ProfileSource::Remote { url, .. } if url.trim().is_empty() => {
                cx.emit(StatusEvent {
                    level: StatusLevel::Warning,
                    message: s().messages.url_empty.to_string(),
                });
                return;
            }
            ProfileSource::Local { path } if path.trim().is_empty() => {
                cx.emit(StatusEvent {
                    level: StatusLevel::Warning,
                    message: s().messages.no_file_selected.to_string(),
                });
                return;
            }
            _ => {}
        }

        if self.is_updating() {
            // Only one fetch at a time (manual, import or auto-update). A
            // manual one waits its turn instead of vanishing — e.g. the Add
            // dialog's first fetch while an auto-update runs. Already being
            // fetched or queued → nothing to add. Imports never get here
            // busy: `import_profile` takes over first.
            let already = self.updating_profile_id() == Some(profile_id.as_str())
                || self.queued_fetches.contains(&profile_id);
            if origin == FetchOrigin::Manual && !already {
                self.queued_fetches.push_back(profile_id);
                cx.emit(StatusEvent {
                    level: StatusLevel::Info,
                    message: (s().messages.queued_update)(&profile_name),
                });
            }
            return;
        }

        let app_dir = self.app_dir.clone();
        let config_path = profile_config_path(&self.app_dir, &profile_id);
        let sing_box = self.sing_box_path();
        let sing_box_version = self.sing_box_version.clone();
        let status_id = profile_id.clone();
        let fetch = self.begin_fetch(&profile_id);

        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    match source {
                        ProfileSource::Remote { url, .. } => perform_update(
                            url.trim(),
                            proxy.as_deref(),
                            &app_dir,
                            &config_path,
                            Some(sing_box.as_path()),
                            sing_box_version.as_deref(),
                        ),
                        ProfileSource::Local { path } => import_local_config(
                            std::path::Path::new(path.trim()),
                            &app_dir,
                            &config_path,
                            Some(sing_box.as_path()),
                        )
                        .map(|outcome| Fetched {
                            outcome,
                            usage: None,
                        }),
                    }
                })
                .await;

            let _ = this.update(cx, |state, cx| {
                state.update_status = UpdateStatus::Idle;
                // Defensive: whatever deletes the profile or supersedes this
                // fetch also drops this task, so this continuation shouldn't
                // run. If it does, the result (and its staged config) goes.
                if !state.fetch_applies(&profile_id, fetch) {
                    cx.notify();
                    state.run_queued_fetches(cx);
                    return;
                }
                let (landed, usage) = split_fetched(result);
                let usage_changed = landed.is_ok() && state.record_usage(&profile_id, usage);
                state.record_fetch_result(
                    &profile_id,
                    landed.as_ref().map(|_| ()).map_err(String::as_str),
                );
                let (level, message) = match landed {
                    Ok(true) => {
                        // Content changed → stamp the "last updated" time, then
                        // persist the URL just used (and any other settings).
                        // Manual update intentionally does NOT auto-restart
                        // the running process — let the user restart at their
                        // own cadence.
                        state.stamp_profile_updated(&profile_id);
                        state.save_settings();
                        (
                            StatusLevel::Success,
                            (s().messages.profile_updated)(&profile_name),
                        )
                    }
                    Ok(false) => {
                        state.save_settings();
                        (
                            StatusLevel::Info,
                            (s().messages.profile_up_to_date)(&profile_name),
                        )
                    }
                    Err(msg) => {
                        if origin == (FetchOrigin::UriImport { created_profile: true }) {
                            state.rollback_import_created(&profile_id);
                        }
                        (
                            StatusLevel::Error,
                            (s().messages.profile_failed)(&profile_name, &msg),
                        )
                    }
                };
                if matches!(level, StatusLevel::Success | StatusLevel::Info)
                    && matches!(origin, FetchOrigin::UriImport { .. })
                {
                    state.set_active_profile(profile_id.clone(), cx);
                }
                cx.emit(StatusEvent { level, message });
                if usage_changed {
                    state.warn_usage(&profile_id, cx);
                }
                cx.notify();
                state.run_queued_fetches(cx);
            });
        });

        self.update_status = UpdateStatus::Updating {
            profile_id: status_id,
            fetch,
            origin,
            _task: task,
        };
        cx.notify();
    }

    /// 弹窗 Save(编辑):一次性写回名称 + source 并持久化。名称留空保持原名。
    pub fn update_profile_fields(
        &mut self,
        id: String,
        name: String,
        source: ProfileSource,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.settings.profiles.iter_mut().find(|p| p.id == id) else {
            return;
        };
        let name = name.trim().to_string();
        if !name.is_empty() {
            profile.name = name;
        }
        // A new URL, file or route (Update through sing-box): the old
        // failure no longer says anything.
        if profile.source != source {
            self.fetch_errors.remove(&id);
        }
        profile.source = source;
        self.save_settings();
        cx.notify();
    }

    /// 弹窗 Save(新增):创建 profile(不激活),返回 id。名称留空用
    /// "Profile N" 兜底;调用方决定是否随后触发一次拉取/导入。
    pub fn create_profile(
        &mut self,
        name: String,
        source: ProfileSource,
        cx: &mut Context<Self>,
    ) -> String {
        let id = self.new_profile_id();
        let number = id.strip_prefix('p').unwrap_or(&id).to_string();
        let name = {
            let trimmed = name.trim();
            if trimmed.is_empty() {
                (s().profiles.default_name)(&number)
            } else {
                trimmed.to_string()
            }
        };
        let had_active = self.settings.active_profile().is_some();
        self.settings.profiles.push(Profile {
            id: id.clone(),
            name,
            source,
            last_updated_secs: None,
            last_checked_secs: None,
            usage: None,
        });
        self.save_settings();
        // First profile in an empty app → make it active so Home leaves the
        // empty state. Later adds don't change the active profile.
        if !had_active {
            self.set_active_profile(id.clone(), cx);
        }
        cx.notify();
        id
    }

    /// Switch the active profile: persist, point `ProxyGroups` at the new
    /// config, and restart sing-box if it's running or starting (same
    /// pattern as `set_proxy_mode`). If the new profile has no fetched config yet, the
    /// restart's `start_process` fails with the usual "Config not found"
    /// toast — honest, and the user is one Update click away from fixing it.
    pub fn set_active_profile(&mut self, id: String, cx: &mut Context<Self>) {
        if self.settings.active_profile_id == id
            || !self.settings.profiles.iter().any(|p| p.id == id)
        {
            return;
        }
        self.settings.active_profile_id = id;
        self.save_settings();
        let config_path = self.active_config_path();
        self.proxy_groups
            .update(cx, |groups, cx| groups.set_config_path(config_path, cx));
        self.restart_if_running(cx);
        cx.notify();
    }

    /// A fresh id for a new profile (never a deleted one's, see
    /// `AppSettings::next_profile_id`). A config already at its path was
    /// left by an older release, which reused ids and could land a fetch for
    /// a deleted profile; it belongs to no profile, so it goes.
    fn new_profile_id(&mut self) -> String {
        let id = self.settings.next_profile_id();
        let _ = fs::remove_file(profile_config_path(&self.app_dir, &id));
        id
    }

    /// Forget a removed profile's fetches: one in flight is dropped (its
    /// result is discarded, see [`UpdateStatus`]), a queued one never runs,
    /// and `fetch_applies` turns stale for any still finishing.
    fn forget_profile_fetches(&mut self, id: &str) {
        if self.updating_profile_id() == Some(id) {
            self.update_status = UpdateStatus::Idle;
        }
        self.queued_fetches.retain(|queued| queued != id);
        self.latest_fetch.remove(id);
        self.fetch_errors.remove(id);
    }

    /// Delete a profile and its fetched config file, and drop any fetch of
    /// it. Deleting the active profile activates the first remaining one;
    /// deleting the *last* profile stops sing-box (or the start in
    /// progress) and drops to the empty state.
    pub fn delete_profile(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(index) = self.settings.profiles.iter().position(|p| p.id == id) else {
            return;
        };
        self.settings.profiles.remove(index);
        self.forget_profile_fetches(&id);
        let _ = fs::remove_file(profile_config_path(&self.app_dir, &id));

        if self.settings.active_profile_id == id {
            let fallback = self.settings.profiles.first().map(|p| p.id.clone());
            match fallback {
                Some(fid) => {
                    // Activate the first remaining profile. Inline the parts of
                    // set_active_profile we need — its same-id guard doesn't
                    // apply (the old id no longer exists).
                    self.settings.active_profile_id = fid;
                    let config_path = self.active_config_path();
                    self.proxy_groups
                        .update(cx, |groups, cx| groups.set_config_path(config_path, cx));
                    self.restart_if_running(cx);
                }
                None => {
                    // Deleted the last profile — drop to the empty state.
                    if self.start_phase(cx) != StartPhase::Idle {
                        self.stop_process(cx);
                    }
                    self.settings.active_profile_id = String::new();
                    let config_path = self.active_config_path();
                    self.proxy_groups
                        .update(cx, |groups, cx| groups.set_config_path(config_path, cx));
                }
            }
        }
        self.save_settings();
        cx.notify();
        // The deleted profile's fetch may have been the one in flight.
        self.run_queued_fetches(cx);
    }

    /// Called by `RootView::new` once its subscribers are wired: opens the
    /// launch-attempt gate so queued attempts can be delivered as events
    /// that someone is listening for. Idempotent.
    pub fn view_attached(&mut self) {
        if let Some(ready) = self.view_ready.take() {
            let _ = ready.send(());
        }
    }

    /// Parse a received deep link. Valid → park as `pending_import` and ask
    /// the view layer to confirm; invalid → warning toast (links arrive from
    /// arbitrary web pages, never import silently). Surfacing the window is
    /// not this function's job — the caller has already done it for every
    /// launch attempt, which is what makes the failure toast visible.
    pub fn handle_deeplink(&mut self, uri: &str, cx: &mut Context<Self>) {
        match parse_import_uri(uri) {
            Ok(request) => {
                self.pending_import = Some(request);
                cx.emit(ImportRequested);
            }
            Err(reason) => {
                cx.emit(StatusEvent {
                    level: StatusLevel::Warning,
                    message: (s().messages.ignored_import)(&reason),
                });
            }
        }
    }

    /// Remove a profile that a URI import created but never landed a config
    /// for. Keeping it would dismiss Home's "Add subscription" empty card
    /// and make the failed import read as a success. The profile is never
    /// active at this point (imports only activate on success), so
    /// `normalize_profiles` is just a safety net, as is removing its config
    /// file: a superseded import's result is discarded, never written.
    fn rollback_import_created(&mut self, profile_id: &str) {
        self.settings.profiles.retain(|p| p.id != profile_id);
        self.settings.normalize_profiles();
        self.latest_fetch.remove(profile_id);
        self.queued_fetches.retain(|queued| queued != profile_id);
        self.fetch_errors.remove(profile_id);
        let _ = fs::remove_file(profile_config_path(&self.app_dir, profile_id));
        self.save_settings();
    }

    /// User-confirmed import. A profile that already has this URL is reused
    /// (re-import = refresh) instead of duplicated. The fetch activates the
    /// profile once its config is on disk.
    pub fn import_profile(&mut self, request: ImportRequest, cx: &mut Context<Self>) {
        // An explicit user action outranks whatever fetch is in flight:
        // dropping the task drops its continuation, so its result is never
        // applied (an auto-update has no task here; its fetch finishes on
        // its own, lands only if this import isn't for the same profile —
        // `fetch_applies` — and leaves this import's state alone, see
        // `release_auto_update`). Queued manual fetches still run after the
        // import. If the superseded fetch was itself an import that created
        // its profile, roll that phantom back now — its failure arm will
        // never run, and the URL lookup below must not resurrect it
        // (re-clicking the same link mid-fetch would otherwise "reuse" the
        // phantom and lose the created-by-import marker).
        if let UpdateStatus::Updating {
            profile_id,
            origin: FetchOrigin::UriImport {
                created_profile: true,
            },
            ..
        } = &self.update_status
        {
            let stale = profile_id.clone();
            self.update_status = UpdateStatus::Idle;
            self.rollback_import_created(&stale);
        } else {
            self.update_status = UpdateStatus::Idle;
        }

        let url = request.url.trim().to_string();
        let (id, created_profile) = match self
            .settings
            .profiles
            .iter()
            .find(|p| p.remote_url() == Some(url.as_str()))
        {
            Some(existing) => (existing.id.clone(), false),
            None => {
                let id = self.new_profile_id();
                let name = request
                    .name
                    .clone()
                    .filter(|n| !n.trim().is_empty())
                    .unwrap_or_else(|| derive_profile_name(&url));
                self.settings.profiles.push(Profile {
                    id: id.clone(),
                    name,
                    source: ProfileSource::Remote {
                        url,
                        auto_update_interval_minutes: default_auto_update_interval(),
                        update_via_sing_box: default_update_via_sing_box(),
                    },
                    last_updated_secs: None,
                    last_checked_secs: None,
                    usage: None,
                });
                self.save_settings();
                (id, true)
            }
        };
        self.update_profile(id, FetchOrigin::UriImport { created_profile }, cx);
        cx.notify();
    }
}

/// Unix seconds now (0 should the clock read before 1970).
fn now_secs() -> u64 {
    to_unix_secs(SystemTime::now()).unwrap_or(0)
}

/// A fetch result split into the committed config outcome (see
/// `UpdateOutcome::commit`) and the usage reading that came with it.
fn split_fetched(
    result: Result<Fetched, String>,
) -> (Result<bool, String>, Option<SubscriptionUsage>) {
    match result {
        Ok(Fetched { outcome, usage }) => (outcome.commit(), usage),
        Err(err) => (Err(err), None),
    }
}

/// The toast for `profile`'s subscription usage, if it needs attention:
/// its level, the toast level (Warning → warning, Critical → error) and the
/// message, which names the profile, never its URL.
fn usage_alert(profile: &Profile, now: u64) -> Option<(UsageLevel, StatusLevel, String)> {
    let usage = profile.usage?;
    let message = usage.alert_message(&profile.name, now)?;
    let level = usage.level(now);
    let status_level = match level {
        UsageLevel::Critical => StatusLevel::Error,
        _ => StatusLevel::Warning,
    };
    Some((level, status_level, message))
}

impl Drop for AppState {
    fn drop(&mut self) {
        eprintln!("AppState dropping — saving settings to {}", self.app_dir.display());
        self.save_settings();
    }
}
