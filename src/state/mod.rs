//! GPUI reactive state.
//!
//! Five `Entity<T>` types form the reactive graph:
//! - [`LogBuffer`] — log `VecDeque` + filter
//! - [`ProcessSession`] — child process lifecycle, encoded as a single
//!   `ProcessState` enum (`Stopped` / `Preparing` / `Running`)
//! - [`ProxyGroups`] — selector outbound groups (Clash API / config-derived)
//! - [`Traffic`] — live up/down network rate (Clash API `/traffic` stream)
//! - [`AppState`] — settings, paths, status, owns the other four entities

pub mod app_state;
pub mod log_buffer;
pub mod process_session;
pub mod proxy_groups;
pub mod traffic;

pub use app_state::{ActivateRequested, AppState, ImportRequested};
pub use log_buffer::LogBuffer;
pub use process_session::{PendingStart, ProcessSession, ProcessState};
pub use proxy_groups::{DelayState, GroupSource, ProxyGroups};
pub use traffic::Traffic;
