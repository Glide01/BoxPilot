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
use gpui::{px, rgb, App, Entity, Hsla, Subscription, Window, WindowAppearance};
use gpui_component::{Theme, ThemeMode};

/// The colours BoxPilot lays over gpui-component's base theme: the blue
/// accent, and the two surfaces of the window — the canvas the sidebar and
/// every page sit on, and the cards raised on it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AccentPalette {
    pub primary: Hsla,
    pub primary_hover: Hsla,
    pub primary_active: Hsla,
    /// Text / icons on `primary` (buttons, the Home power button).
    pub primary_foreground: Hsla,
    /// The window's canvas, behind the sidebar and the pages alike.
    pub sidebar: Hsla,
    /// Cards, inputs and popovers on the canvas.
    pub background: Hsla,
    pub border: Hsla,
    /// The track of segmented controls; the chosen segment is filled grey
    /// on it (`widgets::segmented`).
    pub segmented_track: Hsla,
    /// The selected sidebar entry: a tile a shade darker (lighter in dark
    /// mode) than the canvas.
    pub sidebar_accent: Hsla,
    pub sidebar_accent_foreground: Hsla,
}

/// Blue accents for `dark` or light mode, after Surge's Activity view: no
/// panel framing the page, the sidebar and pages share one canvas and the
/// cards stand on it, white on soft grey in light mode, a lighter grey on
/// near-black in dark. Dark brightens the primary one
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
            // One dark canvas behind sidebar and pages alike; cards a step
            // lighter on it.
            sidebar: rgb(0x1C1C1E).into(),
            background: rgb(0x2C2C2E).into(),
            border: rgb(0x3A3A3C).into(),
            segmented_track: rgb(0x3A3A3C).into(),
            sidebar_accent: rgb(0x2E2E31).into(),
            sidebar_accent_foreground: rgb(0xFAFAFA).into(),
        }
    } else {
        AccentPalette {
            primary: rgb(0x2563EB).into(),        // blue-600
            primary_hover: rgb(0x1D4ED8).into(),  // blue-700
            primary_active: rgb(0x1E40AF).into(), // blue-800
            primary_foreground: rgb(0xFFFFFF).into(),
            // A soft grey canvas behind sidebar and pages alike; white cards
            // on it.
            sidebar: rgb(0xEFF0F4).into(),
            background: rgb(0xFFFFFF).into(),
            border: rgb(0xE5E5EA).into(),
            segmented_track: rgb(0xEDEDF0).into(),
            sidebar_accent: rgb(0xE1E2E8).into(),
            sidebar_accent_foreground: rgb(0x0A0A0A).into(),
        }
    }
}

/// Corner radius of cards and other panels inside a page.
pub const CARD_RADIUS: f32 = 18.;
/// Corner radius of the content panel the pages sit in.
pub const PANEL_RADIUS: f32 = 12.;
/// Gap between the content panel and the window's bottom and right edges
/// (and the title bar or top edge); the sidebar's status tile ends on the
/// same line.
pub const PANEL_INSET: f32 = 8.;
/// Room between the content panel's edges and the page in it: at its
/// sides, and at its top and bottom.
pub const PANEL_PADDING_X: f32 = 28.;
pub const PANEL_PADDING_Y: f32 = 24.;

/// Widest a form page's column (Settings, Tools) grows: past it, a label
/// and its control drift apart across a wide window. Centred past it, page
/// title included (`widgets::form_column`), as the system's own settings
/// windows are, so a wide window keeps even margins rather than an empty
/// right half.
pub const FORM_MAX_WIDTH: f32 = 880.;

/// A floating surface one step above the content panel (toasts): white
/// over the white panel in light mode, lifted by its border and shadow; a
/// lighter grey in dark mode, where a shadow barely shows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Surface {
    pub background: Hsla,
    pub border: Hsla,
}

pub fn toast_surface(dark: bool) -> Surface {
    if dark {
        Surface {
            background: rgb(0x242428).into(),
            border: rgb(0x34343A).into(),
        }
    } else {
        Surface {
            background: rgb(0xFFFFFF).into(),
            border: rgb(0xE4E4E7).into(), // zinc-200
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
/// frame to it. Every window is refreshed.
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
        // Primary buttons have colours of their own, which the base theme
        // derives from its (black / white) primary when it loads — not from
        // the accent laid over it here.
        theme.button_primary = accents.primary;
        theme.button_primary_hover = accents.primary_hover;
        theme.button_primary_active = accents.primary_active;
        theme.button_primary_foreground = accents.primary_foreground;
        theme.sidebar = accents.sidebar;
        theme.sidebar_border = accents.sidebar;
        theme.background = accents.background;
        theme.border = accents.border;
        theme.tab_bar_segmented = accents.segmented_track;
        theme.sidebar_accent = accents.sidebar_accent;
        theme.sidebar_accent_foreground = accents.sidebar_accent_foreground;
        theme.radius_lg = px(PANEL_RADIUS);
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

/// Windows draws the window frame (border, and the system menu; the title
/// bar itself is ours, `ui::title_bar`) light or dark after the OS setting
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

/// Linux: the title bar is the compositor's (or, without server-side
/// decorations, ours, drawn from the theme), nothing to do.
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
        // White cards on a grey canvas; the selected entry a shade darker.
        assert!(light.sidebar.l < light.background.l, "cards stand out");
        assert!(light.sidebar_accent.l < light.sidebar.l);
    }

    #[test]
    fn dark_accents_are_brighter_on_a_dark_sidebar() {
        let dark = accent_palette(true);
        assert_eq!(dark.primary, hex(0x3B82F6));
        assert_eq!(dark.primary_hover, hex(0x60A5FA));
        assert_eq!(dark.primary_active, hex(0x2563EB));
        // Text on the accent stays light, and the selected sidebar entry
        // reads light-on-dark.
        assert!(dark.primary_foreground.l > 0.9);
        assert!(dark.sidebar_accent.l < 0.3);
        assert!(dark.sidebar_accent_foreground.l > 0.8);
        // The canvas is the darkest surface, cards a step lighter on it;
        // the selected entry is lighter than the canvas.
        assert!(dark.background.l > dark.sidebar.l);
        assert!(dark.sidebar_accent.l > dark.sidebar.l);
        // A segmented track stands out from the card it sits on.
        for palette in [dark, accent_palette(false)] {
            assert_ne!(palette.segmented_track, palette.background);
        }
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
