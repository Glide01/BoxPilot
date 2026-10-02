//! Client for the sing-box API service (`services[]` entry of type `api`,
//! sing-box ≥ 1.14.0), plus pure helpers over what it returns. No gpui
//! dependency — keep it that way so the core test shim keeps working.
//!
//! The service is a gRPC server (`daemon.StartedService`, upstream
//! `daemon/started_service.proto`). We speak its gRPC-Web flavour over the
//! blocking reqwest client (`transport`); the messages are hand-derived in
//! `pb` with the upstream field numbers. That keeps tonic/tokio and
//! build-time protobuf codegen out of the build. See
//! docs/adr/0002-sing-box-api-service.md.
//!
//! Layout: this file owns the endpoint (`SingBoxApi`, the injected service
//! entry, the version gate); each domain module adds its RPCs to
//! `SingBoxApi` in its own `impl` block and keeps its domain types and pure
//! helpers next to them. Public API types are plain Rust, never `pb` types.
//! Everything is re-exported here, so `core::singbox_api::X` works for all
//! of it.
//!
//! Threading: every call blocks. Unary calls are short (loopback, 2s bound)
//! but still belong off the UI thread. `stream_*` calls block for as long as
//! the stream lives — run each on a dedicated thread, never on the async
//! executor. A stream callback returns `false` to stop; otherwise the call
//! returns when sing-box ends the stream or a read times out
//! (`ApiError::TimedOut`, routine on idle streams: re-subscribe).
//!
//! sing-box runs the `api` service attached to its own box (not the
//! sing-box for Desktop daemon), so a few RPCs behave differently from the
//! official GUIs: the service status is `Started` for the whole life of the
//! process, deprecation warnings go to stderr instead of the API, and
//! nothing ever sends notifications. Each method documents its case.

mod clash_mode;
mod connections;
mod diagnostics;
mod groups;
mod logs;
mod notifications;
mod pb;
mod service;
mod status;
mod taildrop;
mod tailscale;
mod transport;

pub use clash_mode::*;
pub use connections::*;
pub use diagnostics::*;
pub use groups::*;
pub use logs::*;
pub use notifications::*;
pub use service::*;
pub use status::*;
pub use taildrop::*;
pub use tailscale::*;
pub use transport::{grpc_code, ApiError, IDLE_STREAM_READ_TIMEOUT};

use serde_json::Value;

/// Tag of the `api` service BoxPilot injects into the runtime config.
/// Distinct from anything a subscription is likely to use, since service tags
/// share one namespace.
pub const API_SERVICE_TAG: &str = "boxpilot-api";

/// First sing-box release with the `api` service. Older binaries reject the
/// runtime config ("unknown service type").
pub const MIN_SING_BOX_VERSION: &str = "1.14.0";

/// Whether a `sing-box version` string is new enough for the `api` service.
/// Only the leading `major.minor` matters (pre-releases of 1.14 count);
/// anything unparsable is given the benefit of the doubt — sing-box itself
/// will reject the config if it really is too old.
pub fn supports_api_service(version: &str) -> bool {
    let mut parts = version.trim().split(|c: char| !c.is_ascii_digit());
    let major = parts.next().and_then(|p| p.parse::<u32>().ok());
    let minor = parts.next().and_then(|p| p.parse::<u32>().ok());
    match (major, minor) {
        (Some(major), Some(minor)) => (major, minor) >= (1, 14),
        _ => true,
    }
}

/// The sing-box API service endpoint, always on loopback. The single owner of
/// host + port: every request URL *and* the `api` service entry injected into
/// the runtime config derive from here, so they can't disagree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SingBoxApi {
    port: u16,
}

impl SingBoxApi {
    pub fn new(port: u16) -> Self {
        Self { port }
    }

    /// The `services[]` entry for the runtime config — sing-box must listen
    /// exactly where this client will call. No secret: it is loopback-only,
    /// same exposure the Clash API had.
    pub fn service_config(&self) -> Value {
        serde_json::json!({
            "type": "api",
            "tag": API_SERVICE_TAG,
            "listen": "127.0.0.1",
            "listen_port": self.port
        })
    }

    /// POST URL of one `StartedService` method. Public so the path shape is
    /// testable without HTTP.
    pub fn method_url(&self, method: &str) -> String {
        format!(
            "http://127.0.0.1:{}/{}/{}",
            self.port,
            transport::SERVICE_NAME,
            method
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_service_needs_sing_box_1_14() {
        assert!(supports_api_service(MIN_SING_BOX_VERSION));
        assert!(supports_api_service("1.14.2"));
        assert!(supports_api_service("1.14.0-beta.3"));
        assert!(supports_api_service("1.15.0-alpha.9"));
        assert!(supports_api_service("2.0.0"));
        assert!(!supports_api_service("1.13.21"));
        assert!(!supports_api_service("1.9.0"));
        assert!(!supports_api_service("1.11.15"));
        assert!(supports_api_service("unknown"), "unparsable is not blocked");
    }

    #[test]
    fn method_url_targets_started_service_on_loopback() {
        assert_eq!(
            SingBoxApi::new(7789).method_url("SelectOutbound"),
            "http://127.0.0.1:7789/daemon.StartedService/SelectOutbound"
        );
    }

    /// The injected service and every request URL must agree on the port —
    /// they all derive from the same `SingBoxApi`.
    #[test]
    fn service_config_matches_request_urls() {
        let api = SingBoxApi::new(17900);
        let service = api.service_config();
        assert_eq!(service["type"], "api");
        assert_eq!(service["tag"], API_SERVICE_TAG);
        assert_eq!(service["listen"], "127.0.0.1");
        assert_eq!(service["listen_port"], 17900);
        assert!(api
            .method_url("URLTest")
            .starts_with("http://127.0.0.1:17900/"));
    }
}
