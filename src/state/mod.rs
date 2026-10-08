//! GPUI reactive state.
//!
//! Five `Entity<T>` types form the reactive graph:
//! - [`LogBuffer`] — the Logs page lines (pipes + sing-box API log stream,
//!   merged by `core::log_merge`) + level filter
//! - [`ProcessSession`] — sing-box's lifecycle, encoded as a single
//!   `ProcessState` enum (`Stopped` / `Preparing` / `Running`); a running
//!   sing-box is BoxPilot's child or the privileged helper's (`RunBackend`)
//! - [`ProxyGroups`] — selector outbound groups (sing-box API group stream,
//!   ordered by config)
//! - [`Traffic`] — live runtime status: rates, memory, connections, totals,
//!   start time, version (sing-box API status stream)
//! - [`ClashMode`] — clash mode list + current mode (sing-box API)
//! - [`Connections`] — live connection list (sing-box API connection
//!   stream), plus closing one or all of them
//! - [`NetworkTools`] — Tools page: network quality / STUN test runs
//! - [`TailscaleState`] — Tailscale endpoints, Taildrop inboxes, peer ping
//! - [`VpnStatus`] — OpenConnect / OpenVPN endpoints and USB/IP servers
//!   (sing-box API status streams + sign-in challenges)
//! - [`AppState`] — settings, paths, status, owns the other entities

pub mod app_state;
pub mod clash_mode;
pub mod connections;
pub mod drain;
pub mod log_buffer;
pub mod network_tools;
pub mod process_session;
pub mod proxy_groups;
pub mod tailscale;
pub mod traffic;
pub mod vpn;

pub use app_state::{ActivateRequested, AppState, ImportRequested};
#[cfg(target_os = "linux")]
pub use app_state::TunGrantRequested;
pub use clash_mode::ClashMode;
pub use connections::Connections;
pub use log_buffer::LogBuffer;
pub use network_tools::NetworkTools;
pub use process_session::{
    HelperApi, Launch, PendingStart, ProcessSession, ProcessState, RunBackend,
};
pub use proxy_groups::{DelayState, GroupSource, ProxyGroups};
pub use tailscale::TailscaleState;
pub use traffic::Traffic;
pub use vpn::{ChallengeRequested, VpnStatus};
