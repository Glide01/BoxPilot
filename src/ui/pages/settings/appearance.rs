//! Settings › General "Appearance": System / Light / Dark, applied live
//! (`ui::theme`).

use super::SettingsPage;
use crate::core::settings::ThemePreference;
use crate::i18n::{s, Strings};
use crate::state::AppState;
use crate::ui::{
    theme,
    widgets::{choice_select, setting_row},
};
use gpui::{AnyElement, Context, Entity, IntoElement, ParentElement, Window};
use gpui_component::ActiveTheme;

/// The options, in display order (never rely on `ThemePreference`'s variant
/// order: serde's `other` fallback forces `System` last).
const OPTIONS: [ThemePreference; 3] = [
    ThemePreference::System,
    ThemePreference::Light,
    ThemePreference::Dark,
];

fn label(pref: ThemePreference, t: &'static Strings) -> &'static str {
    match pref {
        ThemePreference::System => t.settings.follow_system,
        ThemePreference::Light => t.settings.theme_light,
        ThemePreference::Dark => t.settings.theme_dark,
    }
}

/// This slot's rows, in display order; empty = nothing to show.
pub(super) fn rows(
    app_state: &Entity<AppState>,
    window: &mut Window,
    cx: &mut Context<SettingsPage>,
) -> Vec<AnyElement> {
    let current = app_state.read(cx).settings.theme;
    let app_state = app_state.clone();
    let control = choice_select(
        "appearance",
        OPTIONS.map(|pref| (pref, label(pref, s()))),
        current,
        move |pref, window, cx| {
            app_state.update(cx, |state, cx| state.set_theme(pref, cx));
            theme::apply(pref, Some(window), cx);
        },
        window,
        cx,
    );

    vec![setting_row(
        cx.theme(),
        s().settings.appearance,
        Some(s().settings.appearance_hint),
    )
    .child(control)
    .into_any_element()]
}
