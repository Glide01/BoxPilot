//! Settings › General "Appearance": System / Light / Dark, applied live
//! (`ui::theme`).

use super::SettingsPage;
use crate::core::settings::ThemePreference;
use crate::state::AppState;
use crate::ui::{theme, widgets::setting_row};
use gpui::{AnyElement, Context, Entity, IntoElement, ParentElement, Window};
use gpui_component::{
    tab::{Tab, TabBar},
    ActiveTheme,
};

/// The options, in display order (never rely on `ThemePreference`'s variant
/// order: serde's `other` fallback forces `System` last).
const OPTIONS: [(ThemePreference, &str); 3] = [
    (ThemePreference::System, "System"),
    (ThemePreference::Light, "Light"),
    (ThemePreference::Dark, "Dark"),
];

/// This slot's rows, in display order; empty = nothing to show.
pub(super) fn rows(
    app_state: &Entity<AppState>,
    _window: &mut Window,
    cx: &mut Context<SettingsPage>,
) -> Vec<AnyElement> {
    let current = app_state.read(cx).settings.theme;
    let selected = OPTIONS
        .iter()
        .position(|(pref, _)| *pref == current)
        .unwrap_or(0);

    let app_state = app_state.clone();
    let control = TabBar::new("appearance")
        .segmented()
        .selected_index(selected)
        .on_click(move |ix: &usize, window, cx| {
            if let Some((pref, _)) = OPTIONS.get(*ix) {
                let pref = *pref;
                app_state.update(cx, |state, cx| state.set_theme(pref, cx));
                theme::apply(pref, Some(window), cx);
            }
        })
        .children(OPTIONS.iter().map(|(_, label)| Tab::new().label(*label)));

    vec![setting_row(
        cx.theme(),
        "Appearance",
        Some("System follows your desktop's light or dark setting."),
    )
    .child(control)
    .into_any_element()]
}
