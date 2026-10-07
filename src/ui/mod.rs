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

use gpui::{div, px, Div, Styled};
use gpui_component::{theme::Theme, StyledExt};

/// Shared chrome for every card panel.
pub fn card_frame(theme: &Theme) -> Div {
    div()
        .p_4()
        .rounded(px(self::theme::CARD_RADIUS))
        .border_1()
        .border_color(theme.border)
        .bg(theme.background)
        .v_flex()
        .gap_3()
        .w_full()
}
