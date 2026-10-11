//! The UI language at runtime: puts a Language preference into effect for
//! BoxPilot's own strings (`crate::i18n`), gpui-component's built-in ones
//! (dialog buttons, input menus) and every open window.
//!
//! Most text is read from `i18n::s()` at render time, so a full window
//! refresh (pages are cached views) is all a switch needs. What a view set
//! once — an input's placeholder — follows through [`observe`]; the tray
//! rebuilds from the language in its snapshot.

use crate::core::settings::LanguagePreference;
use crate::i18n::{self, Language};
use gpui::{App, Context, Global, Subscription, Window};

/// The language the UI currently shows. A gpui global so views can observe
/// a switch.
struct UiLanguage(Language);

impl Global for UiLanguage {}

/// Show the UI in `preference`'s language from now on. At startup (before
/// any window) it just sets the language; later it re-renders every window
/// and notifies [`observe`]rs, but only when the language actually changes
/// (System → English on an English desktop changes nothing).
pub fn apply(preference: LanguagePreference, cx: &mut App) {
    let language = i18n::resolve(preference);
    i18n::set_language(language);
    gpui_component::set_locale(language.component_locale());
    let previous = cx.try_global::<UiLanguage>().map(|current| current.0);
    if previous == Some(language) {
        return;
    }
    cx.set_global(UiLanguage(language));
    if previous.is_some() {
        cx.refresh_windows();
    }
}

/// Run `f` on `cx`'s view whenever the UI language changes — for text the
/// view handed to a component once (input placeholders), which a re-render
/// alone doesn't redo.
pub fn observe<T: 'static>(
    window: &mut Window,
    cx: &mut Context<T>,
    f: impl FnMut(&mut T, &mut Window, &mut Context<T>) + 'static,
) -> Subscription {
    cx.observe_global_in::<UiLanguage>(window, f)
}
