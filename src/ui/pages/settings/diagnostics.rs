//! Settings › Troubleshooting "Running config" row. Owned by WP-J.

use super::SettingsPage;
use crate::state::AppState;
use gpui::{AnyElement, Context, Entity, Window};

/// This slot's rows, in display order; empty = nothing to show.
pub(super) fn rows(
    _app_state: &Entity<AppState>,
    _window: &mut Window,
    _cx: &mut Context<SettingsPage>,
) -> Vec<AnyElement> {
    Vec::new()
}
