//! Light / dark theme: the user's Appearance preference (System / Light /
//! Dark) applied to gpui-component's global `Theme`, and — for System —
//! kept in step with the OS while the window is open.
//!
//! The shadcn defaults gpui-component ships are neutral (a black / white
//! primary); BoxPilot's accent is blue, so every mode switch re-applies
//! [`accent_palette`] on top of the freshly loaded theme. Both go through
//! `Theme::change` / `Theme::update`, which refresh every window — cached
//! pages included — so nothing else has to be notified.

use crate::core::settings::ThemePreference;
use crate::state::AppState;
use gpui::{rgb, App, Entity, Hsla, Subscription, Window, WindowAppearance};
use gpui_component::{Theme, ThemeMode};

/// The accent colours BoxPilot lays over gpui-component's base theme.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AccentPalette {
    pub primary: Hsla,
    pub primary_hover: Hsla,
    pub primary_active: Hsla,
    /// Text / icons on `primary` (buttons, the Home power button, pills).
    pub primary_foreground: Hsla,
    /// The selected sidebar entry.
    pub sidebar_accent: Hsla,
    pub sidebar_accent_foreground: Hsla,
}

/// Blue accents for `dark` or light mode. Dark brightens the primary one
/// step (blue-500, hovering lighter) so it holds up against a near-black
/// background, and keeps white text on it — the dark base theme's own
/// primary foreground is near-black, made for its near-white primary.
pub fn accent_palette(dark: bool) -> AccentPalette {
    if dark {
        AccentPalette {
            primary: rgb(0x3B82F6).into(),        // blue-500
            primary_hover: rgb(0x60A5FA).into(),  // blue-400
            primary_active: rgb(0x2563EB).into(), // blue-600
            primary_foreground: rgb(0xFFFFFF).into(),
            sidebar_accent: rgb(0x172554).into(), // blue-950
            sidebar_accent_foreground: rgb(0xBFDBFE).into(), // blue-200
        }
    } else {
        AccentPalette {
            primary: rgb(0x2563EB).into(),        // blue-600
            primary_hover: rgb(0x1D4ED8).into(),  // blue-700
            primary_active: rgb(0x1E40AF).into(), // blue-800
            primary_foreground: rgb(0xFFFFFF).into(),
            sidebar_accent: rgb(0xEAF1FE).into(),
            sidebar_accent_foreground: rgb(0x1D4ED8).into(), // blue-700
        }
    }
}

/// The mode a preference resolves to, given the OS appearance.
pub fn resolve(pref: ThemePreference, system: WindowAppearance) -> ThemeMode {
    match pref {
        ThemePreference::Light => ThemeMode::Light,
        ThemePreference::Dark => ThemeMode::Dark,
        ThemePreference::System => system.into(),
    }
}

/// Load the theme for `pref` (System resolved against `window`'s
/// appearance when there is a window — the more reliable source on Linux —
/// else the app's) with BoxPilot's accents, and match `window`'s native
/// title bar to it. Every window is refreshed.
pub fn apply(pref: ThemePreference, window: Option<&mut Window>, cx: &mut App) {
    let system = window
        .as_ref()
        .map(|window| window.appearance())
        .unwrap_or_else(|| cx.window_appearance());
    let mode = resolve(pref, system);

    // `change` loads the mode's registered theme over every colour, so the
    // accents go on in a second edit (as `Theme::update` asks).
    Theme::change(mode, None, cx);
    let accents = accent_palette(mode.is_dark());
    Theme::update(cx, |theme| {
        theme.primary = accents.primary;
        theme.primary_hover = accents.primary_hover;
        theme.primary_active = accents.primary_active;
        theme.primary_foreground = accents.primary_foreground;
        theme.sidebar_accent = accents.sidebar_accent;
        theme.sidebar_accent_foreground = accents.sidebar_accent_foreground;
    });

    if let Some(window) = window {
        native_title_bar(window, mode);
    }
}

/// Follow the OS appearance while `window` is open: with the System
/// preference, re-apply the theme on every OS light/dark switch. A forced
/// mode keeps its theme, but the title bar is re-asserted, since the
/// platform resets it to the OS appearance on that switch. Lives as long as
/// the returned subscription (or the window, once detached).
pub fn watch_system(
    window: &mut Window,
    app_state: &Entity<AppState>,
    _cx: &mut App,
) -> Subscription {
    // Weak: the window's observer list must not keep `AppState` alive past
    // quit (dropping it is what stops sing-box).
    let app_state = app_state.downgrade();
    window.observe_window_appearance(move |window, cx| {
        let Some(pref) = app_state
            .upgrade()
            .map(|state| state.read(cx).settings.theme)
        else {
            return;
        };
        match pref {
            ThemePreference::System => apply(pref, Some(window), cx),
            ThemePreference::Light | ThemePreference::Dark => {
                native_title_bar(window, resolve(pref, window.appearance()))
            }
        }
    })
}

/// Windows draws the title bar itself, light or dark after the OS setting
/// (gpui sets that at creation and on each OS switch). When the app forces
/// the other mode, switch it with `DWMWA_USE_IMMERSIVE_DARK_MODE` so the
/// frame matches the content.
#[cfg(target_os = "windows")]
fn native_title_bar(window: &Window, mode: ThemeMode) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::{BOOL, HWND};
    use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};

    // Fully qualified: gpui's inherent `Window::window_handle` (its own
    // handle type) would win over the raw-window-handle trait method.
    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return;
    };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return;
    };
    let hwnd = HWND(handle.hwnd.get() as *mut std::ffi::c_void);
    let dark = BOOL::from(mode.is_dark());
    // SAFETY: `hwnd` is this live window's handle; the attribute value is a
    // BOOL, passed with its size.
    let _ = unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &dark as *const BOOL as *const std::ffi::c_void,
            std::mem::size_of::<BOOL>() as u32,
        )
    };
}

/// Linux: the title bar is the compositor's (or gpui's own client-side
/// decorations, drawn from the theme), nothing to do.
#[cfg(not(target_os = "windows"))]
fn native_title_bar(_window: &Window, _mode: ThemeMode) {}

#[cfg(test)]
mod tests {
    use super::{accent_palette, resolve};
    use crate::core::settings::ThemePreference;
    use gpui::{rgb, Hsla, WindowAppearance};
    use gpui_component::ThemeMode;

    fn hex(value: u32) -> Hsla {
        rgb(value).into()
    }

    #[test]
    fn light_accents_are_the_existing_blue() {
        let light = accent_palette(false);
        assert_eq!(light.primary, hex(0x2563EB));
        assert_eq!(light.primary_hover, hex(0x1D4ED8));
        assert_eq!(light.primary_active, hex(0x1E40AF));
        assert_eq!(light.sidebar_accent, hex(0xEAF1FE));
        assert_eq!(light.sidebar_accent_foreground, hex(0x1D4ED8));
    }

    #[test]
    fn dark_accents_are_brighter_on_a_dark_sidebar() {
        let dark = accent_palette(true);
        assert_eq!(dark.primary, hex(0x3B82F6));
        assert_eq!(dark.primary_hover, hex(0x60A5FA));
        assert_eq!(dark.primary_active, hex(0x2563EB));
        assert_eq!(dark.sidebar_accent, hex(0x172554));
        assert_eq!(dark.sidebar_accent_foreground, hex(0xBFDBFE));
        // Text on the accent stays light, and the selected sidebar entry
        // reads light-on-dark.
        assert!(dark.primary_foreground.l > 0.9);
        assert!(dark.sidebar_accent.l < 0.3);
        assert!(dark.sidebar_accent_foreground.l > 0.8);
        // Hover lightens in dark mode (darkens in light).
        assert!(dark.primary_hover.l > dark.primary.l);
        let light = accent_palette(false);
        assert!(light.primary_hover.l < light.primary.l);
    }

    #[test]
    fn preference_resolves_against_the_os_only_for_system() {
        for system in [WindowAppearance::Light, WindowAppearance::Dark] {
            assert_eq!(resolve(ThemePreference::Light, system), ThemeMode::Light);
            assert_eq!(resolve(ThemePreference::Dark, system), ThemeMode::Dark);
        }
        assert_eq!(
            resolve(ThemePreference::System, WindowAppearance::Dark),
            ThemeMode::Dark
        );
        assert_eq!(
            resolve(ThemePreference::System, WindowAppearance::VibrantLight),
            ThemeMode::Light
        );
    }
}
