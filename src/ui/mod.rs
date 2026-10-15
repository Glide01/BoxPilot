//! GPUI views: sidebar-navigation layout.
//!
//! `RootView` owns the sidebar + four page entities (`ui/pages/`); each
//! page observes the slice of `AppState` it cares about. Shared theme
//! comes from `gpui_component::ActiveTheme`.

#[cfg(target_os = "macos")]
pub mod app_menu;
pub mod app_window;
pub mod assets;
pub mod config_viewer;
pub mod dialog;
pub mod locale;
pub mod pages;
pub mod root;
pub mod sidebar;
pub mod theme;
pub mod title_bar;
pub mod toast;
pub mod traffic_chart;
pub mod tray;
pub mod widgets;

pub use root::RootView;

use gpui::{div, hsla, point, prelude::FluentBuilder, px, BoxShadow, Div, Styled};
use gpui_component::{theme::Theme, StyledExt};

/// Shared chrome for every card: a white (dark grey) tile standing on the
/// window's canvas, no border, a soft shadow where one shows.
pub fn card_frame(theme: &Theme) -> Div {
    div()
        .p_4()
        .rounded(px(self::theme::CARD_RADIUS))
        .bg(theme.background)
        .when(!theme.is_dark(), |card| {
            card.shadow(vec![BoxShadow {
                color: hsla(0., 0., 0., 0.06),
                offset: point(px(0.), px(1.)),
                blur_radius: px(4.),
                spread_radius: px(0.),
                inset: false,
            }])
        })
        .v_flex()
        .gap_3()
        .w_full()
}
