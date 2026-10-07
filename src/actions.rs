//! Keyboard-shortcut action types for the gpui binary.
//!
//! These are GPUI `Action` types declared via the `actions!` macro. They are
//! bound to keystrokes in `main.rs` (`cx.bind_keys`) and dispatched from
//! `RootView::render` via `.on_action(...)`.

use gpui::actions;

actions!(box_pilot, [UpdateSubscription, ToggleProcess]);

/// Key context of `RootView`, the scope both shortcuts are bound in.
pub const KEY_CONTEXT: &str = "BoxPilot";

// Keyboard navigation in the main window: Tab / Shift+Tab walk the focusable
// controls (gpui binds no Tab of its own), Ctrl+1..7 open the pages that are
// always in the sidebar, in sidebar order. Dispatched from `RootView`.
actions!(
    box_pilot,
    [
        FocusNext,
        FocusPrevious,
        ShowHome,
        ShowGroups,
        ShowConnections,
        ShowProfiles,
        ShowLogs,
        ShowTools,
        ShowSettings
    ]
);

// The Connections page's details panel: Esc closes it, Up / Down select the
// neighbouring connection. Dispatched from `ConnectionsPage`.
actions!(
    box_pilot,
    [
        CloseConnectionDetails,
        SelectPreviousConnection,
        SelectNextConnection
    ]
);

/// Key context the Connections page sets while its details panel is open;
/// the panel's keys are bound in it (and not while typing in an `Input`).
pub const CONNECTION_DETAILS_CONTEXT: &str = "ConnectionDetails";

// The macOS menu bar's own items (`ui::app_menu`). Handled app-wide, not in
// `RootView`, so they also work while the window is closed to the menu bar
// icon (Settings… then reopens it).
actions!(
    box_pilot,
    [
        OpenSettings,
        HideApp,
        HideOtherApps,
        ShowAllApps,
        Quit,
        MinimizeWindow,
        CloseWindow
    ]
);
