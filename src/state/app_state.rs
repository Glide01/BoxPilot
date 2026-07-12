use crate::core::clash_api::ClashApi;
use crate::core::deeplink::{derive_profile_name, parse_import_uri, ImportRequest};
use crate::core::orchestration::{process_edge_effects, ProcessEdgeEffect};
use crate::core::process::query_sing_box_version;
use crate::core::paths::{
    get_app_data_dir, get_install_dir, profile_config_path, runtime_config_path,
};
use crate::core::settings::{
    default_auto_update_interval, AppSettings, Profile, ProfileSource, StatusEvent, StatusLevel,
    CONFIG_FILENAME, SING_EXECUTABLE,
};
use crate::core::subscription::{
    import_local_config, perform_update, prepare_config, UpdateOutcome,
};
use crate::core::timefmt::{file_mtime, to_unix_secs};
use crate::state::log_buffer::LogBuffer;
use crate::state::process_session::{PendingStart, ProcessSession};
use crate::state::proxy_groups::ProxyGroups;
use crate::state::traffic::Traffic;
use futures_channel::mpsc::UnboundedReceiver;
use futures_util::StreamExt;
use gpui::{App, AppContext, Context, Entity, EventEmitter, Task};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

/// How often the auto-update loop wakes up. We use a 60-second tick (the
/// smallest user-meaningful interval) and gate actual fetches on elapsed
/// time vs. the configured interval, so changing the interval via settings
/// takes effect at the next tick without restarting the task.
const AUTO_UPDATE_TICK: Duration = Duration::from_secs(60);

pub enum UpdateStatus {
    Idle,
    /// Subscription fetch in flight for one profile. Dropping the `Task`
    /// cancels the future (and `cx.update` from inside it returns Err, so
    /// the result is ignored).
    Updating {
        profile_id: String,
        origin: FetchOrigin,
        _task: Task<()>,
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

/// A `sing-box://import-remote-profile` deep link arrived (argv or the
/// single-instance pipe) and was parsed successfully. The request itself is
/// parked in `AppState::pending_import` — `RootView` consumes it from there
/// (and on startup checks the field directly, since the initial link can be
/// processed before any subscriber exists) and shows the confirm dialog.
pub struct ImportRequested;

/// A plain second launch pinged the single-instance pipe (empty payload, no
/// URI). The user tried to open the app again — `RootView` responds by
/// bringing the existing window to the foreground.
pub struct ActivateRequested;

/// Top-level reactive state owned by `RootView`. Holds persisted settings,
/// resolved paths, the child entities for the process and log subsystems,
/// and an initial status message that `RootView` consumes once on startup.
pub struct AppState {
    pub settings: AppSettings,
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
    pub process: Entity<ProcessSession>,
    pub logs: Entity<LogBuffer>,
    pub proxy_groups: Entity<ProxyGroups>,
    /// Live up/down network rate; streamed while sing-box is running.
    pub traffic: Entity<Traffic>,
    /// Last `is_running()` seen by the process observer — detects
    /// Running/Stopped edges so groups + traffic refresh exactly once per
    /// transition.
    groups_saw_running: bool,
    /// Long-lived background task that periodically refreshes the
    /// subscription. Held so it lives as long as `AppState` and is dropped
    /// (cancelled) on app exit.
    _auto_update_task: Task<()>,
    /// Drains deep-link URIs (argv + single-instance pipe) for the lifetime
    /// of the app.
    _deeplink_task: Task<()>,
}

impl EventEmitter<StatusEvent> for AppState {}

impl EventEmitter<ImportRequested> for AppState {}

impl EventEmitter<ActivateRequested> for AppState {}

impl AppState {
    pub fn new(deeplinks: UnboundedReceiver<String>, cx: &mut App) -> Entity<Self> {
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

        let mut settings = AppSettings::load(&app_dir);

        // Multi-profile migration: the pre-profiles single `config.json`
        // becomes the active profile's `configs/<id>.json`. Same rename
        // guard as above so a re-run can't clobber an already-fetched
        // profile config with the stale legacy file.
        let active_config = profile_config_path(&app_dir, &settings.active_profile_id);
        if !active_config.exists() && new_config.exists() {
            if let Some(parent) = active_config.parent() {
                let _ = fs::create_dir_all(parent);
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
        if backfilled {
            settings.save(&app_dir);
        }

        let pending_status = if !errors.is_empty() {
            Some((StatusLevel::Error, errors.join("; ")))
        } else if settings.active_profile_id.is_empty() {
            // Fresh install / no profiles — the Home empty state guides the
            // user; no scary "config not found" toast.
            None
        } else if active_config.exists() {
            Some((StatusLevel::Success, "Ready.".to_string()))
        } else {
            Some((
                StatusLevel::Warning,
                "Config not found. Please update subscription.".to_string(),
            ))
        };

        let logs = cx.new(|_| LogBuffer::new());
        let process = cx.new({
            let logs = logs.clone();
            move |_| ProcessSession::new(logs)
        });

        let api = ClashApi::new(settings.clash_api_port);
        let proxy_groups = cx.new(|_| ProxyGroups::new(active_config.clone(), api));
        let traffic = cx.new(|_| Traffic::new(api));

        cx.new(|cx| {
            // Drive ProxyGroups + Traffic from process Running/Stopped edges.
            // The edge decision is pure (`core::orchestration`, unit-tested);
            // this observer just executes the returned effects and stores the
            // acted-on state, which is what makes each transition fire once.
            cx.observe(&process, |this: &mut AppState, process, cx| {
                let running = process.read(cx).is_running();
                let effects = process_edge_effects(this.groups_saw_running, running);
                if effects.is_empty() {
                    return;
                }
                this.groups_saw_running = running;
                for effect in effects {
                    match effect {
                        ProcessEdgeEffect::RefreshGroups => this
                            .proxy_groups
                            .update(cx, |groups, cx| groups.refresh_from_api(cx)),
                        ProcessEdgeEffect::StartTraffic => {
                            this.traffic.update(cx, |traffic, cx| traffic.start(cx))
                        }
                        ProcessEdgeEffect::ClearGroups => {
                            this.proxy_groups.update(cx, |groups, cx| groups.clear(cx))
                        }
                        ProcessEdgeEffect::StopTraffic => {
                            this.traffic.update(cx, |traffic, cx| traffic.stop(cx))
                        }
                    }
                }
            })
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
                                    &app_dir,
                                    &config_path,
                                    Some(&sing_box),
                                    sing_box_version.as_deref(),
                                )
                            })
                            .await;

                        let profile_id = profile.id;
                        let exited = this
                            .update(cx, |state, cx| match result {
                                Ok(UpdateOutcome::Changed) => {
                                    state.stamp_profile_updated(&profile_id);
                                    state.save_settings();
                                    // Restart/toast only matter for the
                                    // profile that's actually in use;
                                    // background profiles refresh silently.
                                    if state.settings.active_profile_id == profile_id {
                                        cx.emit(StatusEvent {
                                            level: StatusLevel::Success,
                                            message: "Subscription auto-updated.".to_string(),
                                        });
                                        state.restart_if_running(cx);
                                    }
                                }
                                Ok(UpdateOutcome::Unchanged) => {
                                    // Silent: nothing was written.
                                }
                                Err(err) => {
                                    // No toast — auto-update can fail
                                    // repeatedly when offline; we don't want
                                    // to spam the user.
                                    eprintln!("Auto-update failed: {}", err);
                                }
                            })
                            .is_err();
                        if exited {
                            return;
                        }
                    }
                }
            });

            // Empty payloads are second-launch pings from the
            // single-instance pipe (no URI attached) — the user tried to
            // open the app again, so surface the existing window.
            let deeplink_task = cx.spawn(async move |this, cx| {
                let mut deeplinks = deeplinks;
                while let Some(uri) = deeplinks.next().await {
                    if uri.is_empty() {
                        if this
                            .update(cx, |_, cx| cx.emit(ActivateRequested))
                            .is_err()
                        {
                            return;
                        }
                        continue;
                    }
                    if this
                        .update(cx, |state: &mut AppState, cx| state.handle_deeplink(&uri, cx))
                        .is_err()
                    {
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

            Self {
                settings,
                app_dir,
                install_dir,
                sing_box_version: None,
                update_status: UpdateStatus::Idle,
                pending_status,
                pending_import: None,
                process,
                logs,
                proxy_groups,
                traffic,
                groups_saw_running: false,
                _auto_update_task: auto_update_task,
                _deeplink_task: deeplink_task,
            }
        })
    }

    pub fn is_updating(&self) -> bool {
        matches!(self.update_status, UpdateStatus::Updating { .. })
    }

    /// 正在拉订阅的 profile id(驱动 Profiles 页行级 spinner)。
    pub fn updating_profile_id(&self) -> Option<&str> {
        match &self.update_status {
            UpdateStatus::Updating { profile_id, .. } => Some(profile_id),
            UpdateStatus::Idle => None,
        }
    }

    pub fn save_settings(&self) {
        self.settings.save(&self.app_dir);
    }

    /// Stamp `profile_id` with the current time as its last-content-change
    /// moment. Called only when a fetch/import actually wrote new bytes
    /// (`UpdateOutcome::Changed`); the caller persists via `save_settings`.
    fn stamp_profile_updated(&mut self, profile_id: &str) {
        if let Some(profile) = self.settings.profiles.iter_mut().find(|p| p.id == profile_id) {
            profile.last_updated_secs = to_unix_secs(SystemTime::now());
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
    /// inbounds + experimental, and write the result to the separate runtime
    /// config (the `-c` target). Returns that path. Done synchronously
    /// immediately before the prep task — order matters, do not move to the
    /// background executor.
    fn write_runtime_config(&self) -> Result<PathBuf, String> {
        let config_path = self.active_config_path();
        let data = fs::read_to_string(&config_path)
            .map_err(|e| format!("Failed to read {}: {}", config_path.display(), e))?;
        let prepared = prepare_config(
            &data,
            self.settings.proxy_mode,
            self.settings.set_system_proxy,
            self.settings.proxy_port,
            self.settings.clash_api_port,
        )?;
        let runtime_path = runtime_config_path(&self.app_dir);
        fs::write(&runtime_path, prepared)
            .map_err(|e| format!("Failed to write {}: {}", runtime_path.display(), e))?;
        Ok(runtime_path)
    }

    /// Validate paths, prepare the config, and ask the `ProcessSession` to
    /// start. No-op if a process is already running or starting.
    pub fn start_process(&mut self, cx: &mut Context<Self>) {
        if !self.process.read(cx).is_stopped() {
            return;
        }

        if !self.settings.has_profiles() {
            cx.emit(StatusEvent {
                level: StatusLevel::Warning,
                message: "Add a subscription first.".to_string(),
            });
            return;
        }

        let sing_path = self.sing_box_path();
        if !sing_path.exists() {
            cx.emit(StatusEvent {
                level: StatusLevel::Error,
                message: format!("{} not found at {}", SING_EXECUTABLE, sing_path.display()),
            });
            return;
        }

        if !self.active_config_path().exists() {
            cx.emit(StatusEvent {
                level: StatusLevel::Error,
                message: "Config not found. Update subscription first.".to_string(),
            });
            return;
        }

        let config_path = match self.write_runtime_config() {
            Ok(path) => path,
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
        };

        self.process.update(cx, |p, cx| p.start(pending, cx));
        cx.notify();
    }

    pub fn stop_process(&mut self, cx: &mut Context<Self>) {
        self.process.update(cx, |p, cx| p.stop(cx));
        cx.notify();
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

    fn restart_if_running(&mut self, cx: &mut Context<Self>) {
        if self.process.read(cx).is_running() {
            self.stop_process(cx);
            self.start_process(cx);
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
    /// 运行中立即重启生效(注册表系统代理由 sing-box 按入站端口自写)。
    pub fn set_proxy_port(&mut self, value: u16, cx: &mut Context<Self>) {
        if self.settings.proxy_port == value {
            return;
        }
        self.settings.proxy_port = value;
        self.save_settings();
        self.restart_if_running(cx);
        cx.notify();
    }

    /// Settings 页改 Clash API 端口:持久化 + 给 ProxyGroups/Traffic 换新的
    /// `ClashApi` 句柄(下次刷新/启动用它)+ 运行中重启 sing-box 让新
    /// external_controller 生效。
    pub fn set_clash_api_port(&mut self, value: u16, cx: &mut Context<Self>) {
        if self.settings.clash_api_port == value {
            return;
        }
        self.settings.clash_api_port = value;
        let api = ClashApi::new(value);
        self.proxy_groups.update(cx, |groups, _| groups.set_api(api));
        self.traffic.update(cx, |traffic, _| traffic.set_api(api));
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

    /// Delete every `*.db` file in `app_dir`. sing-box stores its DNS/fakeip
    /// cache as `cache.db`. No-op when the process is running or preparing —
    /// the file is locked on Windows and the UI button is disabled in that
    /// state, but we double-check here so programmatic dispatch can't bypass it.
    pub fn clear_cache(&mut self, cx: &mut Context<Self>) {
        if !self.process.read(cx).is_stopped() {
            cx.emit(StatusEvent {
                level: StatusLevel::Warning,
                message: "Disconnect first to clear the cache.".to_string(),
            });
            return;
        }

        let entries = match fs::read_dir(&self.app_dir) {
            Ok(it) => it,
            Err(e) => {
                cx.emit(StatusEvent {
                    level: StatusLevel::Error,
                    message: format!("Failed to read app directory: {}", e),
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
                format!("Failed to delete cache: {}", errors.join("; ")),
            )
        } else if deleted > 0 {
            (
                StatusLevel::Success,
                format!("Cleared {} cache file(s). Node selection reset.", deleted),
            )
        } else {
            (StatusLevel::Info, "No cache files to clear.".to_string())
        };
        cx.emit(StatusEvent { level, message });
        cx.notify();
    }

    /// Fetch the active profile's subscription (Home update button, Ctrl+U).
    pub fn update_subscription(&mut self, cx: &mut Context<Self>) {
        if !self.settings.has_profiles() {
            cx.emit(StatusEvent {
                level: StatusLevel::Warning,
                message: "Add a subscription first.".to_string(),
            });
            return;
        }
        let id = self.settings.active_profile_id.clone();
        self.update_profile(id, FetchOrigin::Manual, cx);
    }

    /// Kick off a subscription fetch / local re-import for `profile_id` on
    /// the background executor, writing `configs/<id>.json`. Fetching does
    /// NOT activate the profile and never touches a running process — except
    /// for `FetchOrigin::UriImport`, which activates once the config landed
    /// on disk, because activating before the fetch would point a running
    /// sing-box at a config that doesn't exist yet; on failure it rolls the
    /// profile back if this import created it (see [`FetchOrigin`]). The
    /// `Task<()>` is stored in `update_status`(连同 profile id,供行级
    /// spinner)so dropping it (e.g. by overwriting with another update)
    /// cancels the in-flight fetch.
    pub fn update_profile(
        &mut self,
        profile_id: String,
        origin: FetchOrigin,
        cx: &mut Context<Self>,
    ) {
        if self.is_updating() {
            return;
        }
        let Some(profile) = self.settings.profiles.iter().find(|p| p.id == profile_id) else {
            return;
        };
        let profile_name = profile.name.clone();
        let source = profile.source.clone();
        // Reject an empty source up front, with a source-appropriate message.
        match &source {
            ProfileSource::Remote { url, .. } if url.trim().is_empty() => {
                cx.emit(StatusEvent {
                    level: StatusLevel::Warning,
                    message: "Subscription URL is empty.".to_string(),
                });
                return;
            }
            ProfileSource::Local { path } if path.trim().is_empty() => {
                cx.emit(StatusEvent {
                    level: StatusLevel::Warning,
                    message: "No file selected.".to_string(),
                });
                return;
            }
            _ => {}
        }

        let app_dir = self.app_dir.clone();
        let config_path = profile_config_path(&self.app_dir, &profile_id);
        let sing_box = self.sing_box_path();
        let sing_box_version = self.sing_box_version.clone();
        let status_id = profile_id.clone();

        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    match source {
                        ProfileSource::Remote { url, .. } => perform_update(
                            url.trim(),
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
                        ),
                    }
                })
                .await;

            let _ = this.update(cx, |state, cx| {
                state.update_status = UpdateStatus::Idle;
                let (level, message) = match result {
                    Ok(UpdateOutcome::Changed) => {
                        // Content changed → stamp the "last updated" time, then
                        // persist the URL just used (and any other settings).
                        // Manual update intentionally does NOT auto-restart
                        // the running process — let the user restart at their
                        // own cadence.
                        state.stamp_profile_updated(&profile_id);
                        state.save_settings();
                        (
                            StatusLevel::Success,
                            format!("\"{}\" updated.", profile_name),
                        )
                    }
                    Ok(UpdateOutcome::Unchanged) => {
                        state.save_settings();
                        (
                            StatusLevel::Info,
                            format!("\"{}\" is up to date.", profile_name),
                        )
                    }
                    Err(msg) => {
                        if origin == (FetchOrigin::UriImport { created_profile: true }) {
                            state.rollback_import_created(&profile_id);
                        }
                        (
                            StatusLevel::Error,
                            format!("\"{}\": {}", profile_name, msg),
                        )
                    }
                };
                if matches!(level, StatusLevel::Success | StatusLevel::Info)
                    && matches!(origin, FetchOrigin::UriImport { .. })
                {
                    state.set_active_profile(profile_id.clone(), cx);
                }
                cx.emit(StatusEvent { level, message });
                cx.notify();
            });
        });

        self.update_status = UpdateStatus::Updating {
            profile_id: status_id,
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
        let id = self.settings.next_profile_id();
        let number = id.strip_prefix('p').unwrap_or(&id).to_string();
        let name = {
            let trimmed = name.trim();
            if trimmed.is_empty() {
                format!("Profile {}", number)
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
    /// config, and restart sing-box if it's running (same pattern as
    /// `set_proxy_mode`). If the new profile has no fetched config yet, the
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

    /// Delete a profile and its fetched config file. Deleting the active
    /// profile activates the first remaining one; deleting the *last* profile
    /// stops sing-box and drops to the empty state.
    pub fn delete_profile(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(index) = self.settings.profiles.iter().position(|p| p.id == id) else {
            return;
        };
        self.settings.profiles.remove(index);
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
                    if self.process.read(cx).is_running() {
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
    }

    /// Parse a received deep link. Valid → park as `pending_import` and ask
    /// the view layer to confirm; invalid → warning toast (links arrive from
    /// arbitrary web pages, never import silently).
    pub fn handle_deeplink(&mut self, uri: &str, cx: &mut Context<Self>) {
        match parse_import_uri(uri) {
            Ok(request) => {
                self.pending_import = Some(request);
                cx.emit(ImportRequested);
            }
            Err(reason) => {
                cx.emit(StatusEvent {
                    level: StatusLevel::Warning,
                    message: format!("Ignored import link: {}", reason),
                });
            }
        }
    }

    /// Remove a profile that a URI import created but never landed a config
    /// for. Keeping it would dismiss Home's "Add subscription" empty card
    /// and make the failed import read as a success. The profile is never
    /// active at this point (imports only activate on success), so
    /// `normalize_profiles` is just a safety net.
    fn rollback_import_created(&mut self, profile_id: &str) {
        self.settings.profiles.retain(|p| p.id != profile_id);
        self.settings.normalize_profiles();
        self.save_settings();
    }

    /// User-confirmed import. A profile that already has this URL is reused
    /// (re-import = refresh) instead of duplicated. The fetch activates the
    /// profile once its config is on disk.
    pub fn import_profile(&mut self, request: ImportRequest, cx: &mut Context<Self>) {
        // An explicit user action outranks whatever fetch is in flight:
        // dropping the task cancels it. If the cancelled fetch was itself an
        // import that created its profile, roll that phantom back now — its
        // failure arm will never run, and the URL lookup below must not
        // resurrect it (re-clicking the same link mid-fetch would otherwise
        // "reuse" the phantom and lose the created-by-import marker).
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
                let id = self.settings.next_profile_id();
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
                    },
                    last_updated_secs: None,
                });
                self.save_settings();
                (id, true)
            }
        };
        self.update_profile(id, FetchOrigin::UriImport { created_profile }, cx);
        cx.notify();
    }
}

impl Drop for AppState {
    fn drop(&mut self) {
        eprintln!("AppState dropping — saving settings to {}", self.app_dir.display());
        self.save_settings();
    }
}
