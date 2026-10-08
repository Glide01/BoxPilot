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
mod openconnect;
mod openvpn;
mod pb;
mod service;
mod status;
mod taildrop;
mod tailscale;
mod transport;
mod usbip;

pub use clash_mode::*;
pub use connections::*;
pub use diagnostics::*;
pub use groups::*;
pub use logs::*;
pub use notifications::*;
pub use openconnect::*;
pub use openvpn::*;
pub use service::*;
pub use status::*;
pub use taildrop::*;
pub use tailscale::*;
pub use transport::{grpc_code, ApiError, IDLE_STREAM_READ_TIMEOUT};
pub use usbip::*;

use serde_json::Value;
use std::fmt;

/// Tag of the `api` service BoxPilot injects into the runtime config.
/// Distinct from anything a subscription is likely to use, since service tags
/// share one namespace; `prepare_config` adds a suffix in case the config's
/// own services use it anyway. Defined with the injection itself, which the
/// privileged helper shares (`boxpilot_runconfig`).
pub use boxpilot_runconfig::API_SERVICE_TAG;

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

/// Bytes of OS randomness in each run's API secret (hex-encoded on the wire).
const SECRET_LEN: usize = 32;

// Origin the `api` service's CORS allows (`boxpilot_runconfig` explains
// the choice), and how BoxPilot recognizes its own service.
use boxpilot_runconfig::API_ALLOWED_ORIGIN as ALLOWED_ORIGIN;

/// The sing-box API service endpoint for one sing-box run, always on
/// loopback, plus that run's secret. The single owner of host + port +
/// secret: every request URL and `Authorization` header *and* the `api`
/// service entry injected into the runtime config derive from here, so they
/// can't disagree.
///
/// The secret keeps other local processes, and any web page the browser
/// opens, off an API that can list connections and logs, switch nodes and
/// hand out the Tailscale certificate key: a loopback listener alone is
/// reachable from all of them. `Debug` redacts it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct SingBoxApi {
    port: u16,
    secret: [u8; SECRET_LEN],
}

impl SingBoxApi {
    /// An endpoint on `port` with a fresh secret from the OS RNG. Make one
    /// per sing-box start and give the same value to the runtime config
    /// and to every client.
    ///
    /// Panics only if the OS RNG fails, the same failure std's `HashMap`
    /// seeding already panics on.
    pub fn new(port: u16) -> Self {
        let mut secret = [0u8; SECRET_LEN];
        getrandom::fill(&mut secret).expect("OS random number generator unavailable");
        Self { port, secret }
    }

    /// The endpoint of a sing-box BoxPilot didn't configure itself: the
    /// privileged helper's (ADR 0006), from its `started` reply. `None`
    /// unless `secret_hex` is the lowercase hex of exactly as many bytes as
    /// BoxPilot's own secrets. The secret only ever lives in memory: it is
    /// never written to disk, and `Debug` redacts it.
    pub fn from_secret_hex(port: u16, secret_hex: &str) -> Option<Self> {
        let digits = secret_hex.as_bytes();
        if digits.len() != 2 * SECRET_LEN {
            return None;
        }
        let nibble = |digit: u8| match digit {
            b'0'..=b'9' => Some(digit - b'0'),
            b'a'..=b'f' => Some(digit - b'a' + 10),
            _ => None,
        };
        let mut secret = [0u8; SECRET_LEN];
        for (byte, pair) in secret.iter_mut().zip(digits.chunks_exact(2)) {
            *byte = nibble(pair[0])? << 4 | nibble(pair[1])?;
        }
        Some(Self { port, secret })
    }

    /// The secret as sing-box's `secret` option and the bearer token carry
    /// it: lowercase hex.
    fn secret_hex(&self) -> String {
        self.secret.iter().map(|b| format!("{:02x}", b)).collect()
    }

    /// Value of the `Authorization` header every call sends.
    fn authorization(&self) -> String {
        format!("Bearer {}", self.secret_hex())
    }

    /// This endpoint as the run config's `api` service, which
    /// `boxpilot_runconfig` writes the same way for the GUI and for the
    /// privileged helper.
    pub fn service(&self) -> boxpilot_runconfig::ApiService {
        boxpilot_runconfig::ApiService::new(self.port, &self.secret)
    }

    /// The `services[]` entry for the runtime config — sing-box must listen
    /// exactly where this client will call, and accept only its secret.
    /// Without `secret` sing-box serves anyone who can reach the port, and
    /// without `access_control_allow_origin` its CORS answers `*`, so every
    /// web page could read the responses.
    pub fn service_config(&self) -> Value {
        self.service().service_config()
    }

    /// The port this endpoint listens on.
    pub fn port(&self) -> u16 {
        self.port
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

/// Whether a `services[]` entry is one `SingBoxApi::service_config` wrote,
/// whatever its tag, port or secret: recognized by the CORS origin only
/// BoxPilot uses. Found only in a runtime config fed back in (say an import
/// of `running_config.json`), where its secret is long dead.
pub fn is_boxpilot_api_service(service: &Value) -> bool {
    service["type"] == "api"
        && service["access_control_allow_origin"] == serde_json::json!([ALLOWED_ORIGIN])
}

impl fmt::Debug for SingBoxApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SingBoxApi")
            .field("port", &self.port)
            .field("secret", &"<redacted>")
            .finish()
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

    /// sing-box enforces the bearer token only when `secret` is set, and its
    /// CORS answers `*` unless an origin is named.
    #[test]
    fn service_config_requires_the_secret_and_names_no_real_origin() {
        let api = SingBoxApi::new(7789);
        let service = api.service_config();
        let secret = service["secret"].as_str().unwrap();
        assert_eq!(secret.len(), 2 * SECRET_LEN);
        assert!(secret.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(api.authorization(), format!("Bearer {}", secret));
        assert_eq!(
            service["access_control_allow_origin"],
            serde_json::json!(["http://boxpilot.invalid"])
        );
    }

    /// Ours is recognized by its origin, not its tag; a config's own `api`
    /// service is not ours, even under our tag.
    #[test]
    fn boxpilot_api_service_is_recognized_by_origin() {
        let mut ours = SingBoxApi::new(7789).service_config();
        assert!(is_boxpilot_api_service(&ours));
        ours["tag"] = Value::from("boxpilot-api-2");
        assert!(is_boxpilot_api_service(&ours));
        let theirs = serde_json::json!({
            "type": "api", "tag": API_SERVICE_TAG, "listen": "0.0.0.0", "listen_port": 9090
        });
        assert!(!is_boxpilot_api_service(&theirs));
    }

    #[test]
    fn every_endpoint_gets_its_own_secret() {
        let first = SingBoxApi::new(7789);
        let second = SingBoxApi::new(7789);
        assert_ne!(first, second);
        assert_ne!(
            first.service_config()["secret"],
            second.service_config()["secret"]
        );
    }

    /// The helper hands its run's secret over as lowercase hex; the client
    /// sends back exactly that as its bearer token.
    #[test]
    fn an_endpoint_from_the_helpers_secret_round_trips() {
        let ours = SingBoxApi::new(40123);
        let theirs = SingBoxApi::from_secret_hex(40123, &ours.secret_hex()).unwrap();
        assert_eq!(theirs, ours);
        assert_eq!(theirs.authorization(), ours.authorization());
        let hex = ours.secret_hex();
        assert!(SingBoxApi::from_secret_hex(1, &hex[..hex.len() - 2]).is_none());
        assert!(SingBoxApi::from_secret_hex(1, &format!("{hex}00")).is_none());
        assert!(SingBoxApi::from_secret_hex(1, &"AB".repeat(SECRET_LEN)).is_none());
        assert_eq!(
            SingBoxApi::from_secret_hex(1, &"ab".repeat(SECRET_LEN)).map(|api| api.secret),
            Some([0xab; SECRET_LEN])
        );
        assert!(SingBoxApi::from_secret_hex(1, &"zz".repeat(SECRET_LEN)).is_none());
        assert!(SingBoxApi::from_secret_hex(1, "").is_none());
    }

    #[test]
    fn debug_redacts_the_secret() {
        let api = SingBoxApi::new(7789);
        let debug = format!("{:?}", api);
        assert!(debug.contains("7789"));
        assert!(!debug.contains(&api.secret_hex()));
        assert!(debug.contains("<redacted>"));
    }
}
