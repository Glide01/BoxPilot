//! Settings › General "Close button": what closing the window does while a
//! tray icon is up (`ui::app_window::close_decision`).

use super::SettingsPage;
use crate::core::settings::CloseAction;
use crate::state::AppState;
use crate::ui::{tray, widgets::setting_row};
use gpui::{AnyElement, Context, Entity, IntoElement, ParentElement, Window};
use gpui_component::{
    tab::{Tab, TabBar},
    ActiveTheme,
};

/// The options, in display order (never rely on `CloseAction`'s variant
/// order: serde's `other` fallback forces `Ask` last).
const OPTIONS: [(CloseAction, &str); 3] = [
    (CloseAction::Ask, "Ask"),
    (CloseAction::MinimizeToTray, "Minimize to tray"),
    (CloseAction::Quit, "Quit"),
];

/// This slot's rows, in display order; empty = nothing to show.
pub(super) fn rows(
    app_state: &Entity<AppState>,
    _window: &mut Window,
    cx: &mut Context<SettingsPage>,
) -> Vec<AnyElement> {
    // Availability changes refresh every window (`tray::set_available`), so
    // reading it here stays current even on a cached page.
    let tray_available = tray::is_available(cx);
    let current = app_state.read(cx).settings.close_action;
    let selected = OPTIONS
        .iter()
        .position(|(action, _)| *action == current)
        .unwrap_or(0);
    let description = if tray_available {
        "While BoxPilot runs in the tray, sing-box stays connected."
    } else {
        "No system tray on this desktop — closing quits BoxPilot"
    };

    let app_state = app_state.clone();
    let control = TabBar::new("close-action")
        .segmented()
        .selected_index(selected)
        .on_click(move |ix: &usize, _, cx| {
            if let Some((action, _)) = OPTIONS.get(*ix) {
                let action = *action;
                app_state.update(cx, |state, cx| state.set_close_action(action, cx));
            }
        })
        .children(
            OPTIONS
                .iter()
                .map(|(_, label)| Tab::new().label(*label).disabled(!tray_available)),
        );

    vec![setting_row(cx.theme(), "Close button", Some(description))
        .child(control)
        .into_any_element()]
}
