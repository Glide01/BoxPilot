//! Settings › General "Language": System / English / 简体中文, applied live
//! (`ui::locale`).

use super::SettingsPage;
use crate::core::settings::LanguagePreference;
use crate::i18n::s;
use crate::state::AppState;
use crate::ui::{locale, widgets::setting_row};
use gpui::{AnyElement, Context, Entity, IntoElement, ParentElement, Window};
use gpui_component::{
    tab::{Tab, TabBar},
    ActiveTheme,
};

/// The options, in display order (never rely on `LanguagePreference`'s
/// variant order: serde's `other` fallback forces `System` last).
const OPTIONS: [LanguagePreference; 3] = [
    LanguagePreference::System,
    LanguagePreference::English,
    LanguagePreference::SimplifiedChinese,
];

/// A language reads its own name in every UI language, so it can be found
/// from either; only "System" is translated.
fn label(preference: LanguagePreference) -> &'static str {
    match preference {
        LanguagePreference::System => s().settings.follow_system,
        LanguagePreference::English => "English",
        LanguagePreference::SimplifiedChinese => "简体中文",
    }
}

/// This slot's rows, in display order; empty = nothing to show.
pub(super) fn rows(
    app_state: &Entity<AppState>,
    _window: &mut Window,
    cx: &mut Context<SettingsPage>,
) -> Vec<AnyElement> {
    let current = app_state.read(cx).settings.language;
    let selected = OPTIONS
        .iter()
        .position(|preference| *preference == current)
        .unwrap_or(0);

    let app_state = app_state.clone();
    let control = TabBar::new("language")
        .segmented()
        .selected_index(selected)
        .on_click(move |ix: &usize, _, cx| {
            if let Some(preference) = OPTIONS.get(*ix) {
                let preference = *preference;
                // Language first, so everything the save notifies (pages,
                // tray) already reads the new strings.
                locale::apply(preference, cx);
                app_state.update(cx, |state, cx| state.set_language(preference, cx));
            }
        })
        .children(
            OPTIONS
                .iter()
                .map(|preference| Tab::new().label(label(*preference))),
        );

    vec![setting_row(
        cx.theme(),
        s().settings.language,
        Some(s().settings.language_hint),
    )
    .child(control)
    .into_any_element()]
}
