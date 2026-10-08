//! The main window's lifecycle: opening it, surfacing it for every launch
//! attempt, and what its close button does — with a tray icon up, close
//! the window and keep BoxPilot running; without one, quit. See
//! `docs/adr/0004-tray-and-window-lifecycle.md`.
//!
//! "Hide to tray" closes the window for real (gpui has no per-window hide,
//! and Wayland can't hide a toplevel); `AppState` lives on in the
//! [`MainWindow`] global and a fresh window is opened on demand. Quitting
//! removes that global, which drops `AppState` — and with it
//! `ProcessSession`, whose `Drop` stops sing-box and resets the system
//! proxy.

use crate::actions::ShowSettings;
use crate::core::settings::{StatusEvent, StatusLevel};
#[cfg(target_os = "linux")]
use crate::state::TunGrantRequested;
use crate::state::{ActivateRequested, AppState};
use crate::ui::{theme, title_bar, toast, tray, RootView};
use gpui::*;
use gpui_component::Root;

/// The one main window, and the `AppState` that outlives it.
pub struct MainWindow {
    app_state: Entity<AppState>,
    /// `None` while closed to the tray (or not opened yet).
    handle: Option<WindowHandle<Root>>,
    /// Where the window was when it last closed; a reopened window comes
    /// back there.
    last_bounds: Option<WindowBounds>,
    /// Set by the close path that decided to keep running; read (and
    /// cleared) when the window is gone. False = closing quits.
    keep_running_on_close: bool,
    /// The open window's `RootView` routes status events to its toasts.
    /// False while closed, and for the rest of the effect cycle that opened
    /// the window: gpui activates its subscriptions only after that.
    view_routes_status: bool,
    /// The last warning/error raised while no window was open (e.g. a
    /// failed Connect from the tray menu); shown when the window opens.
    parked_status: Option<(StatusLevel, String)>,
    _subscriptions: Vec<Subscription>,
}

impl Global for MainWindow {}

/// Take ownership of `app_state` for the app's lifetime and wire the
/// app-level handlers. Opens nothing; see [`show`].
pub fn init(app_state: Entity<AppState>, cx: &mut App) {
    // Quitting is decided here (window closed without keep-running, tray
    // Quit), not by gpui's "last window closed".
    cx.set_quit_mode(QuitMode::Explicit);

    let mut subscriptions = vec![
        // Every launch attempt surfaces the window (ADR 0001) — reopening
        // it if it was closed to the tray. App-level, so it holds while no
        // window (and no `RootView`) exists.
        cx.subscribe(&app_state, |_, _: &ActivateRequested, cx| show(cx)),
        cx.on_window_closed(on_window_closed),
    ];
    // Status toasts the window can't show yet: the sources the tray menu
    // drives (and that a launch attempt reports through).
    let process = app_state.read(cx).process.clone();
    let clash_mode = app_state.read(cx).clash_mode.clone();
    subscriptions.extend([
        cx.subscribe(&app_state, |_, ev: &StatusEvent, cx| {
            unrouted_status(ev, cx)
        }),
        cx.subscribe(&process, |_, ev: &StatusEvent, cx| unrouted_status(ev, cx)),
        cx.subscribe(&clash_mode, |_, ev: &StatusEvent, cx| {
            unrouted_status(ev, cx)
        }),
    ]);
    // A TUN start from the tray menu may need the one-time grant, which is
    // asked in the window. With the window open its `RootView` asks; with
    // it closed, reopen it and ask there.
    #[cfg(target_os = "linux")]
    subscriptions.push(
        cx.subscribe(&app_state, |app_state, _: &TunGrantRequested, cx| {
            if is_open(cx) {
                return;
            }
            show(cx);
            if let Some(handle) = cx.try_global::<MainWindow>().and_then(|mw| mw.handle) {
                let _ = handle.update(cx, |_, window, _| {
                    window.on_next_frame(move |window, cx| {
                        RootView::prompt_tun_grant(app_state, window, cx);
                    });
                });
            }
        }),
    );

    // REQUIRED for a clean exit: the global owns `AppState`; removing it
    // drops `AppState` → `ProcessSession` (stops sing-box, resets the system
    // proxy). gpui never drops globals on its own.
    cx.on_app_quit(|cx| {
        if cx.has_global::<MainWindow>() {
            drop(cx.remove_global::<MainWindow>());
        }
        async {}
    })
    .detach();

    cx.set_global(MainWindow {
        app_state,
        handle: None,
        last_bounds: None,
        keep_running_on_close: false,
        view_routes_status: false,
        parked_status: None,
        _subscriptions: subscriptions,
    });
}

/// A status event `RootView` isn't listening for (yet): toast it directly
/// into a window that is open but still wiring up — e.g. "Ignored import
/// link" for the link that just reopened it (ADR 0001: failures are as loud
/// as successes) — or, with no window at all, keep a warning/error for
/// when it opens.
fn unrouted_status(ev: &StatusEvent, cx: &mut App) {
    let Some(main) = cx.try_global::<MainWindow>() else {
        return;
    };
    if main.view_routes_status {
        return;
    }
    if is_open(cx) {
        toast::show(ev.level, ev.message.clone(), cx);
    } else if matches!(ev.level, StatusLevel::Warning | StatusLevel::Error) {
        let parked = (ev.level, ev.message.clone());
        cx.update_global::<MainWindow, _>(|main, _| main.parked_status = Some(parked));
    }
}

/// Whether the main window exists (it may still be minimized or behind
/// others).
pub fn is_open(cx: &App) -> bool {
    open_handle(cx).is_some()
}

fn open_handle(cx: &App) -> Option<WindowHandle<Root>> {
    let handle = cx.try_global::<MainWindow>()?.handle?;
    let id = handle.window_id();
    cx.windows()
        .iter()
        .any(|window| window.window_id() == id)
        .then_some(handle)
}

/// Bring the main window to the front, opening it if it's closed.
pub fn show(cx: &mut App) {
    // macOS: ordering a window front doesn't activate its app, so after a
    // menu bar icon click or a link handled in the background it would
    // stay behind the frontmost app.
    #[cfg(target_os = "macos")]
    cx.activate(true);
    if let Some(handle) = open_handle(cx) {
        // Errs only while that window is mid-update itself — it is up
        // anyway, so there's nothing to open.
        let _ = handle.update(cx, |_, window, _| window.activate_window());
        return;
    }
    open(cx);
}

/// Bring the window up on the Settings page (the macOS app menu's
/// Settings…). A window opened for it gets the page once its `RootView`
/// has drawn, and with it its action handlers.
pub fn show_settings(cx: &mut App) {
    let was_open = is_open(cx);
    show(cx);
    let Some(handle) = open_handle(cx) else {
        return;
    };
    let _ = handle.update(cx, |_, window, cx| {
        if was_open {
            window.dispatch_action(Box::new(ShowSettings), cx);
        } else {
            window.on_next_frame(|window, cx| window.dispatch_action(Box::new(ShowSettings), cx));
        }
    });
}

/// Minimize the window, if it is open (the macOS Window menu).
pub fn minimize(cx: &mut App) {
    if let Some(handle) = open_handle(cx) {
        let _ = handle.update(cx, |_, window, _| window.minimize_window());
    }
}

/// Close the window as its close button would (the macOS Window menu's
/// Close Window): `should_close` decides whether that quits.
pub fn close(cx: &mut App) {
    if let Some(handle) = open_handle(cx) {
        let _ = handle.update(cx, |_, window, cx| request_close(window, cx));
    }
}

fn open(cx: &mut App) {
    let Some(main) = cx.try_global::<MainWindow>() else {
        return;
    };
    let app_state = main.app_state.clone();
    // Sizes are of the client area. On Windows that now includes our own
    // title bar, which used to sit outside it: add it so the window opens
    // with as much room for the page as before. (macOS's content runs
    // under its transparent title bar too, but without a strip of ours.)
    let bar_height = if cfg!(target_os = "windows") {
        title_bar::TITLE_BAR_HEIGHT
    } else {
        px(0.)
    };
    let bounds = main.last_bounds.unwrap_or_else(|| {
        WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(860.), px(620.) + bar_height),
            cx,
        ))
    });

    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(bounds),
            window_min_size: Some(size(px(720.), px(500.) + bar_height)),
            // Wayland only raises a window that has an app id, and the
            // `.desktop` file is matched by it. Ignored elsewhere.
            app_id: Some("boxpilot".into()),
            // Windows: no native title bar, `RootView` draws its own
            // (`ui::title_bar`); macOS keeps only the traffic lights, over
            // the sidebar. Linux keeps asking for server-side decorations
            // (gpui's default) and draws its own only where the compositor
            // has none.
            titlebar: Some(title_bar::titlebar_options()),
            app_owns_titlebar_drag: title_bar::APP_OWNS_TITLEBAR_DRAG,
            ..Default::default()
        },
        |window, cx| {
            window.on_window_should_close(cx, should_close);
            // Resolve System against this window's own appearance (the
            // reliable source on Linux), set its frame, and follow OS
            // switches for as long as the window lives.
            let theme_pref = app_state.read(cx).settings.theme;
            theme::apply(theme_pref, Some(window), cx);
            theme::watch_system(window, &app_state, cx).detach();
            let view = cx.new(|cx| RootView::new(app_state.clone(), window, cx));
            // No `.bg(..)`: `Root` paints the current theme's background
            // itself, so it follows light/dark switches.
            cx.new(|cx| Root::new(view, window, cx))
        },
    );
    match opened {
        Ok(handle) => {
            let parked = cx.update_global::<MainWindow, _>(|main, _| {
                main.handle = Some(handle);
                main.view_routes_status = false;
                main.parked_status.take()
            });
            if let Some((level, message)) = parked {
                let _ = handle.update(cx, |_, window, _| {
                    window.on_next_frame(move |_, cx| toast::show(level, message, cx));
                });
            }
            // Runs after the subscription activations `RootView::new`
            // queued, so from then on its own routes show status events.
            cx.defer(|cx| {
                if cx.has_global::<MainWindow>() && is_open(cx) {
                    cx.update_global::<MainWindow, _>(|main, _| main.view_routes_status = true);
                }
            });
        }
        Err(e) => {
            eprintln!("Failed to open the BoxPilot window: {e:#}");
            // Nothing on screen and no tray to come back from: don't linger
            // invisibly with sing-box running.
            if !tray::is_available(cx) {
                cx.quit();
            }
        }
    }
}

/// The platform's close request (title-bar button, Alt+F4, taskbar
/// "Close window", compositor close). Always lets the window close: with a
/// tray icon up BoxPilot keeps running in it; without one the window is the
/// only way back, so closing quits — exactly the behaviour from before the
/// tray existed.
fn should_close(window: &mut Window, cx: &mut App) -> bool {
    remember_bounds(window, cx);
    set_keep_running(tray::is_available(cx), cx);
    true
}

/// The close button BoxPilot draws itself (a client-decorated Linux
/// window; on Windows the OS turns ours into its own close request), and
/// the macOS Close Window item. Same path as the platform's.
pub fn request_close(window: &mut Window, cx: &mut App) {
    if should_close(window, cx) {
        window.remove_window();
    }
}

fn on_window_closed(cx: &mut App, window_id: WindowId) {
    let Some(main) = cx.try_global::<MainWindow>() else {
        return;
    };
    if main.handle.map(|handle| handle.window_id()) != Some(window_id) {
        return;
    }
    let keep_running = main.keep_running_on_close;
    cx.update_global::<MainWindow, _>(|main, _| {
        main.handle = None;
        main.keep_running_on_close = false;
        main.view_routes_status = false;
    });
    if !keep_running {
        cx.quit();
    }
}

fn remember_bounds(window: &Window, cx: &mut App) {
    let bounds = window.window_bounds();
    if cx.has_global::<MainWindow>() {
        cx.update_global::<MainWindow, _>(|main, _| main.last_bounds = Some(bounds));
    }
}

fn set_keep_running(keep_running: bool, cx: &mut App) {
    if cx.has_global::<MainWindow>() {
        cx.update_global::<MainWindow, _>(|main, _| main.keep_running_on_close = keep_running);
    }
}
