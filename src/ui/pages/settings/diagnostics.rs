//! Settings › Troubleshooting "Running config" row: opens the viewer for the
//! config sing-box runs with (`ui::config_viewer`).

use super::SettingsPage;
use crate::state::AppState;
use crate::ui::{config_viewer, widgets::setting_row};
use gpui::{AnyElement, Context, Entity, IntoElement, ParentElement, Window};
use gpui_component::{button::Button, ActiveTheme, Sizable};

/// This slot's rows, in display order; empty = nothing to show.
pub(super) fn rows(
    app_state: &Entity<AppState>,
    _window: &mut Window,
    cx: &mut Context<SettingsPage>,
) -> Vec<AnyElement> {
    let app_state = app_state.clone();
    vec![setting_row(
        cx.theme(),
        "Running config",
        Some("The exact config sing-box runs with."),
    )
    .child(
        Button::new("view-running-config")
            .outline()
            .small()
            .label("View")
            .on_click(move |_, window, cx| config_viewer::open(app_state.clone(), window, cx)),
    )
    .into_any_element()]
}
