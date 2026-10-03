//! The main window's lifecycle: opening it, surfacing it for every launch
//! attempt, and what its close button does — quit, or (with a tray icon
//! up) close the window and keep BoxPilot running. See
//! `docs/adr/0004-tray-and-window-lifecycle.md`.
//!
//! "Hide to tray" closes the window for real (gpui has no per-window hide,
//! and Wayland can't hide a toplevel); `AppState` lives on in the
//! [`MainWindow`] global and a fresh window is opened on demand. Quitting
//! removes that global, which drops `AppState` — and with it
//! `ProcessSession`, whose `Drop` stops sing-box and resets the system
//! proxy.

use crate::core::settings::{CloseAction, StatusEvent, StatusLevel};
#[cfg(target_os = "linux")]
use crate::state::TunGrantRequested;
use crate::state::{ActivateRequested, AppState};
use crate::ui::{toast, tray, RootView};
use gpui::*;
use gpui_component::{
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    dialog::DialogFooter,
    ActiveTheme, Root, WindowExt,
};
use std::cell::Cell;
use std::rc::Rc;

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
    /// The "Keep BoxPilot running in the tray?" prompt is open.
    close_prompt_open: bool,
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

/// What a click on the close button does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseDecision {
    /// Close the window now; keep the app running only if `keep_running`.
    Close { keep_running: bool },
    /// Keep the window and ask the user first.
    AskFirst,
}

/// The close button's effect for the user's setting. Without a tray icon
/// the window is the only way back, so closing always quits — exactly the
/// behaviour from before the tray existed.
pub fn close_decision(action: CloseAction, tray_available: bool) -> CloseDecision {
    if !tray_available {
        return CloseDecision::Close {
            keep_running: false,
        };
    }
    match action {
        CloseAction::Ask => CloseDecision::AskFirst,
        CloseAction::MinimizeToTray => CloseDecision::Close { keep_running: true },
        CloseAction::Quit => CloseDecision::Close {
            keep_running: false,
        },
    }
}

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
        close_prompt_open: false,
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
    if let Some(handle) = open_handle(cx) {
        // Errs only while that window is mid-update itself — it is up
        // anyway, so there's nothing to open.
        let _ = handle.update(cx, |_, window, _| window.activate_window());
        return;
    }
    open(cx);
}

fn open(cx: &mut App) {
    let Some(main) = cx.try_global::<MainWindow>() else {
        return;
    };
    let app_state = main.app_state.clone();
    let bounds = main.last_bounds.clone().unwrap_or_else(|| {
        WindowBounds::Windowed(Bounds::centered(None, size(px(860.), px(620.)), cx))
    });

    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(bounds),
            window_min_size: Some(size(px(720.), px(500.))),
            // Wayland only raises a window that has an app id, and the
            // `.desktop` file is matched by it. Ignored elsewhere.
            app_id: Some("boxpilot".into()),
            titlebar: Some(TitlebarOptions {
                title: Some("BoxPilot".into()),
                ..Default::default()
            }),
            ..Default::default()
        },
        |window, cx| {
            window.on_window_should_close(cx, should_close);
            let view = cx.new(|cx| RootView::new(app_state.clone(), window, cx));
            cx.new(|cx| Root::new(view, window, cx).bg(cx.theme().background))
        },
    );
    match opened {
        Ok(handle) => {
            let parked = cx.update_global::<MainWindow, _>(|main, _| {
                main.handle = Some(handle);
                main.close_prompt_open = false;
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
/// "Close window", compositor close). `true` lets the window close.
fn should_close(window: &mut Window, cx: &mut App) -> bool {
    remember_bounds(window, cx);
    let Some(action) = cx
        .try_global::<MainWindow>()
        .map(|main| main.app_state.read(cx).settings.close_action)
    else {
        return true;
    };
    match close_decision(action, tray::is_available(cx)) {
        CloseDecision::Close { keep_running } => {
            set_keep_running(keep_running, cx);
            true
        }
        CloseDecision::AskFirst => {
            let prompt_open = cx
                .try_global::<MainWindow>()
                .is_some_and(|main| main.close_prompt_open);
            if !prompt_open {
                ask_keep_running(window, cx);
            }
            false
        }
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
        main.close_prompt_open = false;
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

fn set_close_prompt_open(open: bool, cx: &mut App) {
    if cx.has_global::<MainWindow>() {
        cx.update_global::<MainWindow, _>(|main, _| main.close_prompt_open = open);
    }
}

/// Persist the answer when "Don't ask again" is ticked.
fn remember_close_action(remember: bool, action: CloseAction, cx: &mut App) {
    if !remember {
        return;
    }
    if let Some(app_state) = cx
        .try_global::<MainWindow>()
        .map(|main| main.app_state.clone())
    {
        app_state.update(cx, |state, cx| state.set_close_action(action, cx));
    }
}

/// First close with "Ask": keep running in the tray, or quit? Esc and the
/// dialog's × dismiss it and leave the window open.
fn ask_keep_running(window: &mut Window, cx: &mut App) {
    set_close_prompt_open(true, cx);
    let remember = Rc::new(Cell::new(false));
    window.open_alert_dialog(cx, move |alert, _, _| {
        let remember_toggle = remember.clone();
        let remember_quit = remember.clone();
        let remember_keep = remember.clone();
        alert
            .title("Keep BoxPilot running in the tray?")
            .description(
                "BoxPilot can stay in the system tray when its window closes, so \
                 sing-box stays connected. Quit stops sing-box.",
            )
            .child(
                Checkbox::new("close-remember")
                    .label("Don't ask again")
                    .checked(remember.get())
                    .on_click(move |checked: &bool, window, _| {
                        remember_toggle.set(*checked);
                        window.refresh();
                    }),
            )
            .close_button(true)
            .on_close(|_, _, cx| set_close_prompt_open(false, cx))
            .footer(
                DialogFooter::new()
                    .child(Button::new("close-quit").label("Quit").on_click(
                        move |_, window, cx| {
                            window.close_dialog(cx);
                            set_close_prompt_open(false, cx);
                            remember_close_action(remember_quit.get(), CloseAction::Quit, cx);
                            cx.quit();
                        },
                    ))
                    .child(
                        Button::new("close-keep")
                            .primary()
                            .label("Keep in tray")
                            .on_click(move |_, window, cx| {
                                window.close_dialog(cx);
                                set_close_prompt_open(false, cx);
                                remember_close_action(
                                    remember_keep.get(),
                                    CloseAction::MinimizeToTray,
                                    cx,
                                );
                                // The icon may have gone while the prompt
                                // was up: then this close quits after all.
                                set_keep_running(tray::is_available(cx), cx);
                                remember_bounds(window, cx);
                                window.remove_window();
                            }),
                    ),
            )
    });
}

#[cfg(test)]
mod tests {
    // Not `super::*`: that brings in `gpui::*`, whose `test` attribute
    // macro would shadow the built-in one.
    use super::{close_decision, CloseDecision};
    use crate::core::settings::CloseAction;

    #[test]
    fn without_a_tray_closing_always_quits() {
        for action in [
            CloseAction::Ask,
            CloseAction::MinimizeToTray,
            CloseAction::Quit,
        ] {
            assert_eq!(
                close_decision(action, false),
                CloseDecision::Close {
                    keep_running: false
                }
            );
        }
    }

    #[test]
    fn with_a_tray_the_setting_decides() {
        assert_eq!(
            close_decision(CloseAction::Ask, true),
            CloseDecision::AskFirst
        );
        assert_eq!(
            close_decision(CloseAction::MinimizeToTray, true),
            CloseDecision::Close { keep_running: true }
        );
        assert_eq!(
            close_decision(CloseAction::Quit, true),
            CloseDecision::Close {
                keep_running: false
            }
        );
    }
}
