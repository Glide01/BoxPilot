//! Settings › General "Close button": what closing the window does while a
//! tray icon is up (`ui::app_window::close_decision`).

use super::SettingsPage;
use crate::core::settings::CloseAction;
use crate::i18n::{s, Strings};
use crate::state::AppState;
use crate::ui::{tray, widgets::setting_row};
use gpui::{div, AnyElement, Context, Entity, IntoElement, ParentElement, Styled, Window};
use gpui_component::{
    tab::{Tab, TabBar},
    ActiveTheme,
};

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
    _window: &mut Window,
    cx: &mut Context<SettingsPage>,
) -> Vec<AnyElement> {
    // Availability changes refresh every window (`tray::set_available`), so
    // reading it here stays current even on a cached page.
    if !tray::is_available(cx) {
        // Nothing to choose: closing always quits. Show that as text rather
        // than a disabled segmented control (whose selected tab also lost its
        // highlight on re-render). The saved choice is kept for a desktop
        // that has a tray.
        let theme = cx.theme();
        return vec![setting_row(
            theme,
            s().settings.close_button,
            Some(s().settings.close_no_tray_hint),
        )
        .child(
            div()
                .flex_shrink_0()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(s().settings.close_quits),
        )
        .into_any_element()];
    }

    let current = app_state.read(cx).settings.close_action;
    let selected = OPTIONS
        .iter()
        .position(|action| *action == current)
        .unwrap_or(0);

    let app_state = app_state.clone();
    let control = TabBar::new("close-action")
        .segmented()
        .selected_index(selected)
        .on_click(move |ix: &usize, _, cx| {
            if let Some(action) = OPTIONS.get(*ix) {
                let action = *action;
                app_state.update(cx, |state, cx| state.set_close_action(action, cx));
            }
        })
        .children(
            OPTIONS
                .iter()
                .map(|action| Tab::new().label(label(*action, s()))),
        );

    vec![setting_row(
        cx.theme(),
        s().settings.close_button,
        Some(s().settings.close_hint),
    )
    .child(control)
    .into_any_element()]
}
