//! GPUI reactive state.
//!
//! Five `Entity<T>` types form the reactive graph:
//! - [`LogBuffer`] — log `VecDeque` + filter
//! - [`ProcessSession`] — child process lifecycle, encoded as a single
//!   `ProcessState` enum (`Stopped` / `Preparing` / `Running`)
//! - [`ProxyGroups`] — selector outbound groups (sing-box API group stream,
//!   ordered by config)
//! - [`Traffic`] — live up/down network rate (sing-box API status stream)
//! - [`VpnStatus`] — OpenConnect / OpenVPN endpoints and USB/IP servers
//!   (sing-box API status streams + sign-in challenges)
//! - [`AppState`] — settings, paths, status, owns the other entities

pub mod app_state;
pub mod log_buffer;
pub mod process_session;
pub mod proxy_groups;
pub mod traffic;
pub mod vpn;

pub use app_state::{ActivateRequested, AppState, ImportRequested};
pub use log_buffer::LogBuffer;
pub use process_session::{PendingStart, ProcessSession, ProcessState};
pub use proxy_groups::{DelayState, GroupSource, ProxyGroups};
pub use traffic::Traffic;
pub use vpn::{ChallengeRequested, VpnStatus};
