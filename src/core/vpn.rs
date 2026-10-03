//! Pure logic behind the VPN page (`state/vpn.rs`, `ui/pages/vpn.rs`):
//! which VPN-ish things the running config has, which challenges are new,
//! and the display models for endpoint and tunnel status. No gpui
//! dependency — keep it that way so the core test shim keeps working.

use crate::core::singbox_api::{
    grpc_code, ApiError, OpenConnectBrowserMode, OpenConnectBrowserRequest,
    OpenConnectEndpointStatus, OpenConnectTunnel, OpenVpnEndpointStatus, OpenVpnTunnel, VpnState,
};
use crate::i18n::s;
use serde_json::Value;
use std::collections::HashSet;
use std::hash::Hash;
use std::time::Duration;

/// sing-box endpoint type of an OpenConnect client.
pub const OPENCONNECT_TYPE: &str = "openconnect";
/// sing-box endpoint type of an OpenVPN client (`openvpn-server` has no
/// status in the API).
pub const OPENVPN_CLIENT_TYPE: &str = "openvpn-client";
/// sing-box service type of a USB/IP server.
pub const USBIP_SERVER_TYPE: &str = "usbip-server";

/// The OpenConnect / OpenVPN endpoints and USB/IP servers in a config, by
/// tag, in config order. Decides which status streams to open and whether
/// the VPN page shows at all.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VpnPresence {
    pub openconnect: Vec<String>,
    pub openvpn: Vec<String>,
    /// `usbip-server`s with `"provider": "dynamic"` — the only ones the API
    /// reports on.
    pub usbip_dynamic: Vec<String>,
    /// Default-provider `usbip-server`s: they share host devices on their
    /// own and have no API status, so the page only lists them.
    pub usbip_default: Vec<String>,
}

impl VpnPresence {
    /// Read from a sing-box config (the prepared runtime config, which
    /// passes the profile's `endpoints` and `services` through untouched).
    /// Unparsable JSON reads as empty.
    pub fn from_config(config: &str) -> Self {
        let Ok(json) = serde_json::from_str::<Value>(config) else {
            return Self::default();
        };
        let entries = |key: &str| -> Vec<Value> {
            json.get(key)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        };
        let tag = |entry: &Value| entry["tag"].as_str().unwrap_or_default().to_string();
        let mut presence = Self::default();
        for endpoint in entries("endpoints") {
            match endpoint["type"].as_str() {
                Some(OPENCONNECT_TYPE) => presence.openconnect.push(tag(&endpoint)),
                Some(OPENVPN_CLIENT_TYPE) => presence.openvpn.push(tag(&endpoint)),
                _ => {}
            }
        }
        for service in entries("services") {
            if service["type"].as_str() != Some(USBIP_SERVER_TYPE) {
                continue;
            }
            // `provider` defaults to `default` upstream.
            if service["provider"].as_str() == Some("dynamic") {
                presence.usbip_dynamic.push(tag(&service));
            } else {
                presence.usbip_default.push(tag(&service));
            }
        }
        presence
    }

    pub fn is_empty(&self) -> bool {
        self.openconnect.is_empty()
            && self.openvpn.is_empty()
            && self.usbip_dynamic.is_empty()
            && self.usbip_default.is_empty()
    }

    pub fn has_usbip(&self) -> bool {
        !self.usbip_dynamic.is_empty() || !self.usbip_default.is_empty()
    }
}

/// Which API a challenge belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VpnProtocol {
    OpenConnect,
    OpenVpn,
}

impl VpnProtocol {
    pub fn label(self) -> &'static str {
        match self {
            VpnProtocol::OpenConnect => "OpenConnect",
            VpnProtocol::OpenVpn => "OpenVPN",
        }
    }
}

/// Identifies one pending challenge across status updates. A challenge's
/// id never repeats, so a key that comes back means the same challenge.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ChallengeKey {
    pub protocol: VpnProtocol,
    pub endpoint_tag: String,
    pub challenge_id: String,
}

/// Every challenge pending in these statuses, in endpoint order.
pub fn pending_challenges(
    openconnect: &[OpenConnectEndpointStatus],
    openvpn: &[OpenVpnEndpointStatus],
) -> Vec<ChallengeKey> {
    let openconnect = openconnect.iter().filter_map(|status| {
        status.challenge.as_ref().map(|challenge| ChallengeKey {
            protocol: VpnProtocol::OpenConnect,
            endpoint_tag: status.endpoint_tag.clone(),
            challenge_id: challenge.id.clone(),
        })
    });
    let openvpn = openvpn.iter().filter_map(|status| {
        status.challenge.as_ref().map(|challenge| ChallengeKey {
            protocol: VpnProtocol::OpenVpn,
            endpoint_tag: status.endpoint_tag.clone(),
            challenge_id: challenge.id.clone(),
        })
    });
    openconnect.chain(openvpn).collect()
}

/// The items of `current` not in `seen` — the ones to act on (prompt for a
/// challenge, toast a failure) — and the new `seen`. `seen` keeps only what
/// is current, so it stays bounded; an item that stays current is acted on
/// once. Challenge ids never repeat, so no challenge is prompted twice.
pub fn newly_seen<K: Clone + Eq + Hash>(
    seen: &HashSet<K>,
    current: Vec<K>,
) -> (Vec<K>, HashSet<K>) {
    let fresh = current
        .iter()
        .filter(|key| !seen.contains(*key))
        .cloned()
        .collect();
    (fresh, current.into_iter().collect())
}

/// An endpoint that gave up, with sing-box's reason.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct EndpointFailure {
    pub protocol: VpnProtocol,
    pub endpoint_tag: String,
    pub error: String,
}

impl EndpointFailure {
    /// Toast text: `OpenVPN "office": authentication challenge canceled`.
    pub fn message(&self) -> String {
        format!(
            "{} \"{}\": {}",
            self.protocol.label(),
            self.endpoint_tag,
            self.error
        )
    }
}

/// Every endpoint in the `error` state, in endpoint order.
pub fn failed_endpoints(
    openconnect: &[OpenConnectEndpointStatus],
    openvpn: &[OpenVpnEndpointStatus],
) -> Vec<EndpointFailure> {
    let failure = |protocol, state: &VpnState, tag: &str, error: &str| {
        (*state == VpnState::Error).then(|| EndpointFailure {
            protocol,
            endpoint_tag: tag.to_string(),
            error: if error.trim().is_empty() {
                s().vpn.failed.to_string()
            } else {
                error.trim().to_string()
            },
        })
    };
    let openconnect = openconnect.iter().filter_map(|status| {
        failure(
            VpnProtocol::OpenConnect,
            &status.state,
            &status.endpoint_tag,
            &status.error,
        )
    });
    let openvpn = openvpn.iter().filter_map(|status| {
        failure(
            VpnProtocol::OpenVpn,
            &status.state,
            &status.endpoint_tag,
            &status.error,
        )
    });
    openconnect.chain(openvpn).collect()
}

/// Whether a status stream error will repeat on every retry, so the reader
/// should stop: `NOT_FOUND` / `UNIMPLEMENTED` mean this sing-box build has
/// no such service (e.g. built without `with_usbip`). Everything else —
/// timeouts, sing-box not listening yet or going away — is worth a retry.
pub fn stream_error_is_permanent(error: &ApiError) -> bool {
    matches!(
        error.code(),
        Some(grpc_code::NOT_FOUND) | Some(grpc_code::UNIMPLEMENTED)
    )
}

/// Grace period after sing-box starts during which a failing status stream
/// is normal (its API isn't listening yet) and not worth showing.
pub const STREAM_ERROR_GRACE: Duration = Duration::from_secs(5);

/// Whether the VPN page should show a status stream's failure. A permanent
/// one always; otherwise only for a stream that never delivered a snapshot
/// this session, once `STREAM_ERROR_GRACE` has passed — the reader retries
/// every second, so a lasting problem shows up right after the grace period,
/// while a startup race or a reconnect after an idle drop never does.
pub fn should_report_stream_error(permanent: bool, loaded: bool, since_start: Duration) -> bool {
    permanent || (!loaded && since_start >= STREAM_ERROR_GRACE)
}

/// Badge color for an endpoint state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VpnTone {
    Success,
    Warning,
    Danger,
    Muted,
}

pub fn vpn_state_tone(state: &VpnState) -> VpnTone {
    match state {
        VpnState::Connected => VpnTone::Success,
        VpnState::AuthPending => VpnTone::Warning,
        VpnState::Error => VpnTone::Danger,
        VpnState::Connecting | VpnState::Other(_) => VpnTone::Muted,
    }
}

/// sing-box's own state text, else one derived from the state.
pub fn vpn_state_label(state: &VpnState, state_text: &str) -> String {
    if !state_text.trim().is_empty() {
        return state_text.trim().to_string();
    }
    let t = &s().vpn;
    match state {
        VpnState::Connecting => t.connecting.to_string(),
        VpnState::AuthPending => t.waiting_sign_in.to_string(),
        VpnState::Connected => t.connected.to_string(),
        VpnState::Error => t.error.to_string(),
        VpnState::Other(other) => other.clone(),
    }
}

/// One label/value line of the tunnel details.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InfoRow {
    pub label: &'static str,
    pub value: String,
}

fn push_row(rows: &mut Vec<InfoRow>, label: &'static str, value: String) {
    if !value.trim().is_empty() {
        rows.push(InfoRow { label, value });
    }
}

fn push_common(
    rows: &mut Vec<InfoRow>,
    ipv4: &[String],
    ipv6: &[String],
    dns: &[String],
    mtu: u32,
    connected_since: Option<i64>,
    now_secs: i64,
) {
    push_row(rows, "IPv4", ipv4.join(", "));
    push_row(rows, "IPv6", ipv6.join(", "));
    push_row(rows, "DNS", dns.join(", "));
    if mtu > 0 {
        push_row(rows, "MTU", mtu.to_string());
    }
    if let Some(since) = connected_since {
        push_row(rows, s().vpn.row_uptime, format_uptime(now_secs - since));
    }
}

/// Tunnel details for an OpenConnect endpoint, empty values left out.
pub fn openconnect_tunnel_rows(tunnel: &OpenConnectTunnel, now_secs: i64) -> Vec<InfoRow> {
    let t = &s().vpn;
    let mut rows = Vec::new();
    push_row(&mut rows, t.row_server, tunnel.server.clone());
    push_row(
        &mut rows,
        t.row_protocol,
        openconnect_flavor_label(&tunnel.flavor),
    );
    push_row(&mut rows, t.row_transport, tunnel.transport.clone());
    push_common(
        &mut rows,
        &tunnel.ipv4,
        &tunnel.ipv6,
        &tunnel.dns,
        tunnel.mtu,
        tunnel.connected_since,
        now_secs,
    );
    rows
}

/// Tunnel details for an OpenVPN endpoint, empty values left out.
pub fn openvpn_tunnel_rows(tunnel: &OpenVpnTunnel, now_secs: i64) -> Vec<InfoRow> {
    let t = &s().vpn;
    let mut rows = Vec::new();
    push_row(&mut rows, t.row_server, tunnel.server.clone());
    push_row(&mut rows, t.row_network, tunnel.network.to_uppercase());
    push_row(&mut rows, t.row_cipher, tunnel.cipher.clone());
    push_common(
        &mut rows,
        &tunnel.ipv4,
        &tunnel.ipv6,
        &tunnel.dns,
        tunnel.mtu,
        tunnel.connected_since,
        now_secs,
    );
    rows
}

/// Product name of an OpenConnect `flavor`; unknown ones pass through.
pub fn openconnect_flavor_label(flavor: &str) -> String {
    match flavor {
        "anyconnect" => "Cisco AnyConnect",
        "gp" => "GlobalProtect",
        "fortinet" => "Fortinet",
        "f5" => "F5 BIG-IP",
        "pulse" => "Pulse Secure",
        "nc" => "Juniper Network Connect",
        other => other,
    }
    .to_string()
}

/// A coarse elapsed time: `45s`, `12 min`, `3 hr 4 min`, `2 d 5 hr`.
/// Negative (clock skew) reads as `0s`.
pub fn format_uptime(secs: i64) -> String {
    let secs = secs.max(0) as u64;
    let t = &s().time;
    if secs < 60 {
        (t.coarse_secs)(secs)
    } else if secs < 3600 {
        (t.coarse_mins)(secs / 60)
    } else if secs < 86400 {
        (t.coarse_hours_mins)(secs / 3600, secs % 3600 / 60)
    } else {
        (t.coarse_days_hours)(secs / 86400, secs % 86400 / 3600)
    }
}

/// How long until a challenge `deadline`, for the dialog.
pub fn deadline_label(deadline: i64, now_secs: i64) -> String {
    let left = deadline - now_secs;
    let t = &s().vpn;
    if left <= 0 {
        t.deadline_passed.to_string()
    } else if left < 60 {
        (t.deadline_secs)(left)
    } else {
        (t.deadline_mins)((left + 30) / 60)
    }
}

/// Why BoxPilot can't finish this browser sign-in, or `None` when it can
/// (callback mode: the user pastes the callback address). Cookie and header
/// modes need values only an embedded browser can capture — BoxPilot hands
/// sign-in to the system browser, which never gives them back.
pub fn openconnect_browser_limitation(request: &OpenConnectBrowserRequest) -> Option<String> {
    let t = &s().vpn;
    let captured = match request.mode() {
        OpenConnectBrowserMode::Callback => return None,
        OpenConnectBrowserMode::Cookies => (t.cookies)(
            &request.cookie_names.join(", "),
            request.cookie_names.len(),
        ),
        OpenConnectBrowserMode::Headers => (t.headers)(
            &request.header_names.join(", "),
            request.header_names.len(),
        ),
    };
    Some((t.browser_limitation)(&captured))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::singbox_api::{OpenConnectBrowserRequest, OpenConnectChallenge};

    #[test]
    fn presence_reads_endpoints_and_usbip_services() {
        let config = r#"{
            "endpoints": [
                {"type": "openconnect", "tag": "corp"},
                {"type": "wireguard", "tag": "wg"},
                {"type": "openvpn-client", "tag": "office"},
                {"type": "openvpn-server", "tag": "srv"},
                {"type": "openconnect", "tag": "lab"}
            ],
            "services": [
                {"type": "api", "tag": "boxpilot-api"},
                {"type": "usbip-server", "tag": "share", "provider": "dynamic"},
                {"type": "usbip-server", "tag": "host"},
                {"type": "usbip-client", "tag": "client"}
            ]
        }"#;
        let presence = VpnPresence::from_config(config);
        assert_eq!(
            presence,
            VpnPresence {
                openconnect: vec!["corp".into(), "lab".into()],
                openvpn: vec!["office".into()],
                usbip_dynamic: vec!["share".into()],
                usbip_default: vec!["host".into()],
            }
        );
        assert!(!presence.is_empty());
        assert!(presence.has_usbip());
    }

    /// The runtime config is what gets read: `prepare_config` must pass the
    /// subscription's endpoints and USB/IP services through, and its own
    /// injected `api` service must not count.
    #[test]
    fn presence_survives_prepare_config() {
        use crate::core::subscription::{prepare_config, RuntimeOptions};
        let subscription = r#"{
            "endpoints": [{"type": "openvpn-client", "tag": "office"}],
            "services": [{"type": "usbip-server", "tag": "share", "provider": "dynamic"}]
        }"#;
        let prepared = prepare_config(subscription, RuntimeOptions::default()).unwrap();
        let presence = VpnPresence::from_config(&prepared);
        assert_eq!(presence.openvpn, vec!["office"]);
        assert_eq!(presence.usbip_dynamic, vec!["share"]);

        let plain = prepare_config(r#"{"outbounds": []}"#, RuntimeOptions::default()).unwrap();
        assert!(VpnPresence::from_config(&plain).is_empty());
    }

    #[test]
    fn presence_is_empty_without_vpn_entries_or_valid_json() {
        let plain = VpnPresence::from_config(
            r#"{"outbounds": [{"type": "direct"}], "services": [{"type": "api"}]}"#,
        );
        assert!(plain.is_empty());
        assert!(!plain.has_usbip());
        assert!(VpnPresence::from_config("not json").is_empty());
        assert!(VpnPresence::from_config(r#"{"endpoints": {}}"#).is_empty());
    }

    fn openconnect_status(tag: &str, challenge_id: Option<&str>) -> OpenConnectEndpointStatus {
        OpenConnectEndpointStatus {
            endpoint_tag: tag.into(),
            state: if challenge_id.is_some() {
                VpnState::AuthPending
            } else {
                VpnState::Connecting
            },
            state_text: String::new(),
            error: String::new(),
            challenge: challenge_id.map(|id| OpenConnectChallenge {
                id: id.into(),
                banner: String::new(),
                message: String::new(),
                error: String::new(),
                prompt: crate::core::singbox_api::OpenConnectPrompt::Unknown,
            }),
            tunnel: None,
        }
    }

    fn key(protocol: VpnProtocol, tag: &str, id: &str) -> ChallengeKey {
        ChallengeKey {
            protocol,
            endpoint_tag: tag.into(),
            challenge_id: id.into(),
        }
    }

    #[test]
    fn each_challenge_is_new_exactly_once() {
        let pending = pending_challenges(
            &[
                openconnect_status("corp", Some("1")),
                openconnect_status("lab", None),
            ],
            &[],
        );
        assert_eq!(pending, vec![key(VpnProtocol::OpenConnect, "corp", "1")]);

        let (fresh, seen) = newly_seen(&HashSet::new(), pending.clone());
        assert_eq!(fresh, pending);
        // The same snapshot again (any status change resends everything).
        let (fresh, seen) = newly_seen(&seen, pending);
        assert!(fresh.is_empty());
        // Answered: the next step comes with a new id.
        let next = pending_challenges(&[openconnect_status("corp", Some("2"))], &[]);
        let (fresh, seen) = newly_seen(&seen, next);
        assert_eq!(fresh, vec![key(VpnProtocol::OpenConnect, "corp", "2")]);
        assert_eq!(seen.len(), 1, "only what is still pending is remembered");
    }

    #[test]
    fn same_id_on_another_protocol_or_endpoint_is_distinct() {
        let seen: HashSet<_> = [key(VpnProtocol::OpenConnect, "a", "1")].into();
        let (fresh, _) = newly_seen(
            &seen,
            vec![
                key(VpnProtocol::OpenVpn, "a", "1"),
                key(VpnProtocol::OpenConnect, "b", "1"),
            ],
        );
        assert_eq!(fresh.len(), 2);
    }

    #[test]
    fn failures_are_listed_with_their_reason() {
        let mut failed = openconnect_status("corp", None);
        failed.state = VpnState::Error;
        failed.error = "certificate signed by unknown authority".into();
        let mut silent = openconnect_status("lab", None);
        silent.state = VpnState::Error;
        let failures = failed_endpoints(&[failed, silent, openconnect_status("ok", None)], &[]);
        assert_eq!(failures.len(), 2);
        assert_eq!(
            failures[0].message(),
            "OpenConnect \"corp\": certificate signed by unknown authority"
        );
        assert_eq!(failures[1].error, "failed");
        // A failure is toasted once, however many updates repeat it.
        let (fresh, seen) = newly_seen(&HashSet::new(), failures.clone());
        assert_eq!(fresh.len(), 2);
        assert!(newly_seen(&seen, failures).0.is_empty());
    }

    #[test]
    fn only_missing_services_stop_the_stream_reader() {
        let status = |code| ApiError::Status {
            code,
            message: String::new(),
        };
        assert!(stream_error_is_permanent(&status(grpc_code::NOT_FOUND)));
        assert!(stream_error_is_permanent(&status(grpc_code::UNIMPLEMENTED)));
        assert!(!stream_error_is_permanent(&status(grpc_code::UNAVAILABLE)));
        assert!(!stream_error_is_permanent(&ApiError::TimedOut));
        assert!(!stream_error_is_permanent(&ApiError::Unreachable(
            "x".into()
        )));
    }

    #[test]
    fn stream_errors_wait_out_the_startup_grace_period() {
        let early = Duration::from_secs(1);
        let late = STREAM_ERROR_GRACE;
        // The API not listening yet right after start: quiet.
        assert!(!should_report_stream_error(false, false, early));
        // Still failing after the grace period: shown.
        assert!(should_report_stream_error(false, false, late));
        // A stream that worked and dropped: it reconnects, stay quiet.
        assert!(!should_report_stream_error(false, true, late));
        // The service doesn't exist in this build: shown at once.
        assert!(should_report_stream_error(true, false, early));
    }

    #[test]
    fn states_map_to_tones_and_labels() {
        assert_eq!(vpn_state_tone(&VpnState::Connected), VpnTone::Success);
        assert_eq!(vpn_state_tone(&VpnState::AuthPending), VpnTone::Warning);
        assert_eq!(vpn_state_tone(&VpnState::Error), VpnTone::Danger);
        assert_eq!(vpn_state_tone(&VpnState::Connecting), VpnTone::Muted);
        assert_eq!(
            vpn_state_label(&VpnState::Connected, " Connected "),
            "Connected"
        );
        assert_eq!(
            vpn_state_label(&VpnState::AuthPending, ""),
            "Waiting for sign-in"
        );
        assert_eq!(vpn_state_label(&VpnState::Other("x".into()), ""), "x");
    }

    #[test]
    fn tunnel_rows_skip_empty_values() {
        let tunnel = OpenConnectTunnel {
            server: "vpn.example.com".into(),
            flavor: "gp".into(),
            transport: String::new(),
            ipv4: vec!["10.0.0.2/32".into(), "10.0.1.2/32".into()],
            ipv6: Vec::new(),
            dns: vec!["10.0.0.1".into()],
            mtu: 1400,
            connected_since: Some(1_000),
        };
        let rows = openconnect_tunnel_rows(&tunnel, 1_000 + 3_725);
        let pairs: Vec<(&str, &str)> = rows
            .iter()
            .map(|row| (row.label, row.value.as_str()))
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("Server", "vpn.example.com"),
                ("Protocol", "GlobalProtect"),
                ("IPv4", "10.0.0.2/32, 10.0.1.2/32"),
                ("DNS", "10.0.0.1"),
                ("MTU", "1400"),
                ("Uptime", "1 hr 2 min"),
            ]
        );

        let openvpn = OpenVpnTunnel {
            server: "1.2.3.4:1194".into(),
            network: "udp".into(),
            cipher: "AES-256-GCM".into(),
            mtu: 0,
            ..Default::default()
        };
        let labels: Vec<&str> = openvpn_tunnel_rows(&openvpn, 0)
            .iter()
            .map(|row| row.label)
            .collect();
        assert_eq!(labels, vec!["Server", "Network", "Cipher"]);
        assert_eq!(openvpn_tunnel_rows(&openvpn, 0)[1].value, "UDP");
    }

    #[test]
    fn uptime_and_deadline_labels() {
        assert_eq!(format_uptime(-5), "0s");
        assert_eq!(format_uptime(59), "59s");
        assert_eq!(format_uptime(600), "10 min");
        assert_eq!(format_uptime(86400 + 7200), "1 d 2 hr");
        assert_eq!(openconnect_flavor_label("custom"), "custom");
        assert!(deadline_label(100, 100).contains("passed"));
        assert_eq!(
            deadline_label(130, 100),
            "The server waits 30 more seconds."
        );
        assert_eq!(
            deadline_label(400, 100),
            "The server waits about 5 more min."
        );
    }

    #[test]
    fn only_callback_sign_in_is_possible_from_the_system_browser() {
        let callback = OpenConnectBrowserRequest {
            url: "u".into(),
            callback_url_prefixes: vec!["http://127.0.0.1:".into()],
            ..Default::default()
        };
        assert_eq!(openconnect_browser_limitation(&callback), None);

        let cookies = OpenConnectBrowserRequest {
            url: "u".into(),
            final_url: "f".into(),
            cookie_names: vec!["acSamlv2Token".into()],
            ..Default::default()
        };
        let message = openconnect_browser_limitation(&cookies).unwrap();
        assert!(message.contains("the acSamlv2Token cookie to"));

        let headers = OpenConnectBrowserRequest {
            url: "u".into(),
            header_names: vec!["saml-username".into(), "prelogin-cookie".into()],
            ..Default::default()
        };
        let message = openconnect_browser_limitation(&headers).unwrap();
        assert!(message.contains("saml-username, prelogin-cookie response headers"));
    }
}
