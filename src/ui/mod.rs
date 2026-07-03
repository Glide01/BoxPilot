//! GPUI views: sidebar-navigation layout.
//!
//! `RootView` owns the sidebar + four page entities (`ui/pages/`); each
//! page observes the slice of `AppState` it cares about. Shared theme
//! comes from `gpui_component::ActiveTheme`.

pub mod assets;
pub mod pages;
pub mod root;
pub mod sidebar;
pub mod toast;
pub mod widgets;

pub use root::RootView;

use gpui::{div, Div, Styled};
use gpui_component::{theme::Theme, StyledExt};

/// Shared chrome for every card panel.
pub fn card_frame(theme: &Theme) -> Div {
    div()
        .p_4()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .bg(theme.background)
        .v_flex()
        .gap_3()
        .w_full()
}
