//! GPUI reactive state.
//!
//! Six `Entity<T>` types form the reactive graph:
//! - [`LogBuffer`] — log `VecDeque` + filter
//! - [`ProcessSession`] — child process lifecycle, encoded as a single
//!   `ProcessState` enum (`Stopped` / `Preparing` / `Running`)
//! - [`ProxyGroups`] — selector outbound groups (sing-box API group stream,
//!   ordered by config)
//! - [`Traffic`] — live up/down network rate (sing-box API status stream)
//! - [`Connections`] — live connection list (sing-box API connection
//!   stream), plus closing one or all of them
//! - [`AppState`] — settings, paths, status, owns the other five entities

pub mod app_state;
pub mod connections;
pub mod log_buffer;
pub mod process_session;
pub mod proxy_groups;
pub mod traffic;

pub use app_state::{ActivateRequested, AppState, ImportRequested};
pub use connections::Connections;
pub use log_buffer::LogBuffer;
pub use process_session::{PendingStart, ProcessSession, ProcessState};
pub use proxy_groups::{DelayState, GroupSource, ProxyGroups};
pub use traffic::Traffic;
