//! Keyboard-shortcut action types for the gpui binary.
//!
//! These are GPUI `Action` types declared via the `actions!` macro. They are
//! bound to keystrokes in `main.rs` (`cx.bind_keys`) and dispatched from
//! `RootView::render` via `.on_action(...)`.

use gpui::actions;

actions!(box_pilot, [UpdateSubscription, ToggleProcess]);
