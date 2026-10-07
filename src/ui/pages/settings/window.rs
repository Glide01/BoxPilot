//! Settings › General "Close button": what closing the window does while a
//! tray icon is up (`ui::app_window::close_decision`).

use super::SettingsPage;
use crate::core::settings::CloseAction;
use crate::i18n::{s, Strings};
use crate::state::AppState;
use crate::ui::{
    tray,
    widgets::{choice_select, setting_row},
};
use gpui::{AnyElement, Context, Entity, IntoElement, ParentElement, Window};
use gpui_component::ActiveTheme;

/// The options, in display order (never rely on `CloseAction`'s variant
/// order: serde's `other` fallback forces `Ask` last).
const OPTIONS: [CloseAction; 3] = [
    CloseAction::Ask,
    CloseAction::MinimizeToTray,
    CloseAction::Quit,
];

fn label(action: CloseAction, t: &'static Strings) -> &'static str {
    match action {
        CloseAction::Ask => t.settings.close_ask,
        CloseAction::MinimizeToTray => t.settings.close_minimize,
        CloseAction::Quit => t.settings.close_quit,
    }
}

/// This slot's rows, in display order; empty = nothing to show.
pub(super) fn rows(
    app_state: &Entity<AppState>,
    window: &mut Window,
    cx: &mut Context<SettingsPage>,
) -> Vec<AnyElement> {
    // Availability changes refresh every window (`tray::set_available`), so
    // reading it here stays current even on a cached page.
    if !tray::is_available(cx) {
        // Nothing to choose: closing always quits, and the hint says so.
        // No control on the right — a value there would read as a disabled
        // setting. The saved choice is kept for a desktop that has a tray.
        return vec![setting_row(
            cx.theme(),
            s().settings.close_button,
            Some(s().settings.close_no_tray_hint),
        )
        .into_any_element()];
    }

    let current = app_state.read(cx).settings.close_action;
    let app_state = app_state.clone();
    let control = choice_select(
        "close-action",
        OPTIONS.map(|action| (action, label(action, s()))),
        current,
        move |action, _, cx| {
            app_state.update(cx, |state, cx| state.set_close_action(action, cx));
        },
        window,
        cx,
    );

    vec![setting_row(
        cx.theme(),
        s().settings.close_button,
        Some(s().settings.close_hint),
    )
    .child(control)
    .into_any_element()]
}
