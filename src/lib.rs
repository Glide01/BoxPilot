//! BoxPilot library: GPUI views + reactive state + UI-agnostic core logic.
//!
//! - `core` — pure Rust + serde/reqwest/dirs (no gpui). Settings, subscription
//!   fetch, sing-box process helpers, path resolution.
//! - `state` — `Entity<T>` types (`AppState`, `ProcessSession`, `LogBuffer`)
//!   driving reactive UI updates.
//! - `ui` — gpui views, one entity per card.
//! - `actions` — `gpui::actions!` types for keyboard shortcuts.

pub mod actions;
pub mod core;
pub mod state;
pub mod ui;
