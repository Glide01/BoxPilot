//! System tray icon and menu: status at a glance (colour icon while
//! connected, tooltip), and Connect/Disconnect, System Proxy, Proxy Mode,
//! Clash Mode and Profile without opening the window. While a tray is up,
//! closing the window can leave BoxPilot running (`ui::app_window`); see
//! `docs/adr/0004-tray-and-window-lifecycle.md`.
//!
//! Backends (`windows`: tray-icon, `linux`: StatusNotifierItem via ksni)
//! only render a [`TraySnapshot`] and forward clicks as [`TrayCommand`]s
//! into a channel; the task drained here on the UI thread is the only place
//! a click turns into an `AppState` call. No tray (a Linux desktop without
//! a StatusNotifier host, or a failed registration) = [`is_available`] stays
//! false and BoxPilot behaves exactly as it does without this module.

mod icon;
#[cfg(target_os = "linux")]
mod linux;
mod model;
#[cfg(target_os = "windows")]
mod windows;

pub use model::{menu_entries, MenuEntry, TrayCommand, TraySnapshot};

use crate::core::presentation::ConnectionStatus;
use crate::state::AppState;
use crate::ui::app_window;
use futures_channel::mpsc::{unbounded, UnboundedSender};
use futures_util::StreamExt;
use gpui::{App, BorrowAppContext, Entity, Global, Subscription, WeakEntity};

#[cfg(target_os = "linux")]
use linux::Backend;
#[cfg(target_os = "windows")]
use windows::Backend;

/// The tray, as a gpui global: the backend once it is up, and the snapshot
/// it last showed.
pub struct TrayController {
    app_state: WeakEntity<AppState>,
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    backend: Option<Backend>,
    /// A tray icon is on screen, so the window may close without quitting.
    available: bool,
    snapshot: Option<TraySnapshot>,
    _subscriptions: Vec<Subscription>,
}

impl Global for TrayController {}

/// Whether a tray icon is showing right now. False until the backend is up
/// (Linux registers in the background), and after the host goes away.
pub fn is_available(cx: &App) -> bool {
    cx.try_global::<TrayController>()
        .is_some_and(|tray| tray.available)
}

/// Start the tray. Never blocks the UI thread on D-Bus; failures only log.
pub fn init(app_state: &Entity<AppState>, cx: &mut App) {
    let (commands, mut received) = unbounded::<TrayCommand>();

    let process = app_state.read(cx).process.clone();
    let clash_mode = app_state.read(cx).clash_mode.clone();
    let subscriptions = vec![
        cx.observe(app_state, |_, cx| refresh(cx, false)),
        cx.observe(&process, |_, cx| refresh(cx, false)),
        cx.observe(&clash_mode, |_, cx| refresh(cx, false)),
    ];
    let snapshot = snapshot_of(app_state, cx);
    cx.set_global(TrayController {
        app_state: app_state.downgrade(),
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        backend: None,
        available: false,
        snapshot: Some(snapshot.clone()),
        _subscriptions: subscriptions,
    });

    cx.spawn(async move |cx| {
        while let Some(command) = received.next().await {
            cx.update(|cx| dispatch(command, cx));
        }
    })
    .detach();

    start_backend(snapshot, commands, cx);

    // Take the icon down with the app. Dropping the backend removes the
    // Windows icon at once (otherwise it lingers until hovered).
    cx.on_app_quit(|cx| {
        if cx.has_global::<TrayController>() {
            let tray = cx.remove_global::<TrayController>();
            #[cfg(target_os = "linux")]
            if let Some(backend) = &tray.backend {
                backend.shutdown();
            }
            drop(tray);
        }
        async {}
    })
    .detach();
}

#[cfg(target_os = "windows")]
fn start_backend(snapshot: TraySnapshot, commands: UnboundedSender<TrayCommand>, cx: &mut App) {
    // tray-icon's window must live on the thread gpui pumps: this one.
    match Backend::new(&snapshot, commands) {
        Ok(backend) => backend_ready(backend, cx),
        Err(e) => eprintln!("System tray unavailable: {e}"),
    }
}

#[cfg(target_os = "linux")]
fn start_backend(snapshot: TraySnapshot, commands: UnboundedSender<TrayCommand>, cx: &mut App) {
    // Registration blocks on D-Bus round trips (and can wait on a slow or
    // absent watcher): a thread of its own, never the UI thread.
    let (done_tx, done_rx) = futures_channel::oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name("tray-register".into())
        .spawn(move || {
            let _ = done_tx.send(Backend::spawn(snapshot, commands));
        });
    if let Err(e) = spawned {
        eprintln!("System tray unavailable: {e}");
        return;
    }
    cx.spawn(async move |cx| match done_rx.await {
        Ok(Ok(backend)) => cx.update(|cx| backend_ready(backend, cx)),
        Ok(Err(e)) => eprintln!("System tray unavailable: {e}"),
        Err(_) => {}
    })
    .detach();
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn start_backend(_: TraySnapshot, _: UnboundedSender<TrayCommand>, _: &mut App) {}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn backend_ready(backend: Backend, cx: &mut App) {
    if !cx.has_global::<TrayController>() {
        return;
    }
    cx.update_global::<TrayController, _>(|tray, _| tray.backend = Some(backend));
    set_available(true, cx);
    // State may have moved on while the backend was registering.
    refresh(cx, true);
}

/// Flip availability; the Settings close-button row depends on it, and
/// cached views only see a global change through a full refresh.
fn set_available(available: bool, cx: &mut App) {
    if !cx.has_global::<TrayController>() || is_available(cx) == available {
        return;
    }
    cx.update_global::<TrayController, _>(|tray, _| tray.available = available);
    cx.refresh_windows();
}

/// What the tray shows for the current state.
fn snapshot_of(app_state: &Entity<AppState>, cx: &App) -> TraySnapshot {
    let state = app_state.read(cx);
    let running = state.process.read(cx).is_running();
    let clash = state.clash_mode.read(cx);
    let (clash_modes, clash_current) = if running && clash.is_switchable() {
        (clash.modes.clone(), clash.current.clone())
    } else {
        (Vec::new(), String::new())
    };
    TraySnapshot {
        status: ConnectionStatus::from_flags(state.is_starting(cx), running),
        proxy_mode: state.settings.proxy_mode,
        system_proxy: state.settings.set_system_proxy,
        clash_modes,
        clash_current,
        profiles: state
            .settings
            .profiles
            .iter()
            .map(|p| (p.id.clone(), p.name.clone()))
            .collect(),
        active_profile: state.settings.active_profile_id.clone(),
        language: crate::i18n::current(),
    }
}

/// Rebuild the snapshot and push it to the backend when it changed (or
/// always, with `force`: after a click, so a menu the platform toggled on
/// its own is put back in line with the real state).
fn refresh(cx: &mut App, force: bool) {
    let Some(app_state) = cx
        .try_global::<TrayController>()
        .and_then(|tray| tray.app_state.upgrade())
    else {
        return;
    };
    let snapshot = snapshot_of(&app_state, cx);
    cx.update_global::<TrayController, _>(|tray, _| {
        if !force && tray.snapshot.as_ref() == Some(&snapshot) {
            return;
        }
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        if let Some(backend) = tray.backend.as_mut() {
            backend.update(&snapshot);
        }
        tray.snapshot = Some(snapshot);
    });
}

/// A tray click, on the UI thread.
fn dispatch(command: TrayCommand, cx: &mut App) {
    let app_state = cx
        .try_global::<TrayController>()
        .and_then(|tray| tray.app_state.upgrade());
    match command {
        TrayCommand::ShowWindow => app_window::show(cx),
        TrayCommand::Quit => {
            cx.quit();
            return;
        }
        TrayCommand::HostLost => {
            set_available(false, cx);
            // No icon left to bring the window back with.
            if !app_window::is_open(cx) {
                app_window::show(cx);
            }
        }
        TrayCommand::HostRestored => set_available(true, cx),
        TrayCommand::ToggleConnection => {
            if let Some(app_state) = app_state {
                app_state.update(cx, |state, cx| state.toggle_process(cx));
            }
        }
        TrayCommand::SetProxyMode(proxy) => {
            if let Some(app_state) = app_state {
                app_state.update(cx, |state, cx| state.set_proxy_mode(proxy, cx));
            }
        }
        TrayCommand::SetSystemProxy(on) => {
            if let Some(app_state) = app_state {
                app_state.update(cx, |state, cx| state.set_system_proxy(on, cx));
            }
        }
        TrayCommand::SetClashMode(mode) => {
            if let Some(app_state) = app_state {
                let clash_mode = app_state.read(cx).clash_mode.clone();
                clash_mode.update(cx, |clash, cx| clash.select(mode, cx));
            }
        }
        TrayCommand::SetActiveProfile(id) => {
            if let Some(app_state) = app_state {
                app_state.update(cx, |state, cx| state.set_active_profile(id, cx));
            }
        }
    }
    refresh(cx, true);
}
