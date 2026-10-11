//! Tailscale endpoints (`endpoints[]` of type `tailscale`):
//! `SubscribeTailscaleStatus`, `StartTailscalePing`, `SetTailscaleExitNode`,
//! `TailscaleLogout`. Taildrop and certificates live in `taildrop.rs`; SSH
//! is not wrapped (see the ADR for client-streaming limits).

use super::transport::{ApiError, IDLE_STREAM_READ_TIMEOUT};
use super::{pb, SingBoxApi};
use std::time::Duration;

/// `SetTailscaleExitNode` and `TailscaleLogout` talk to the Tailscale backend
/// (logout restarts it and starts a fresh login) — more than the default
/// unary bound allows.
const TAILSCALE_ACTION_TIMEOUT: Duration = Duration::from_secs(15);
/// Ping results arrive once a second; one attempt may take a while to fail.
const PING_READ_TIMEOUT: Duration = Duration::from_secs(30);

impl SingBoxApi {
    /// Stream `SubscribeTailscaleStatus`: on subscribe, one update with
    /// every Tailscale endpoint's status (an empty list when the config has
    /// none), then a full update — all endpoints again, not a diff —
    /// whenever any of them changes (login state, peers, exit node, traffic
    /// counters). Idle otherwise: `TimedOut` after
    /// `IDLE_STREAM_READ_TIMEOUT`; re-subscribe.
    pub fn stream_tailscale_status(
        &self,
        mut on_update: impl FnMut(Vec<TailscaleEndpointStatus>) -> bool,
    ) -> Result<(), ApiError> {
        self.stream(
            "SubscribeTailscaleStatus",
            &(),
            IDLE_STREAM_READ_TIMEOUT,
            |update: pb::TailscaleStatusUpdate| {
                on_update(
                    update
                        .endpoints
                        .into_iter()
                        .map(TailscaleEndpointStatus::from_proto)
                        .collect(),
                )
            },
        )
    }

    /// Ping a peer (disco ping) once a second, forever, until `on_result`
    /// returns `false`. `endpoint_tag` empty = the first Tailscale endpoint.
    /// A failed attempt arrives as a result with `error` set; the stream
    /// goes on. Errors: `NOT_FOUND` (no such endpoint), `INVALID_ARGUMENT`
    /// (not a Tailscale endpoint), or `UNKNOWN` for an unparsable `peer_ip`.
    pub fn start_tailscale_ping(
        &self,
        endpoint_tag: &str,
        peer_ip: &str,
        mut on_result: impl FnMut(TailscalePing) -> bool,
    ) -> Result<(), ApiError> {
        let request = pb::TailscalePingRequest {
            endpoint_tag: endpoint_tag.to_string(),
            peer_ip: peer_ip.to_string(),
        };
        self.stream(
            "StartTailscalePing",
            &request,
            PING_READ_TIMEOUT,
            |response: pb::TailscalePingResponse| on_result(TailscalePing::from_proto(response)),
        )
    }

    /// `SetTailscaleExitNode` — route through the peer with `stable_id`
    /// (`TailscalePeer::stable_id` of an `exit_node_option` peer), or stop
    /// using an exit node with an empty `stable_id`. `endpoint_tag` must name
    /// the endpoint exactly. Fails while Tailscale isn't running yet, or if
    /// the endpoint itself advertises an exit node.
    pub fn set_tailscale_exit_node(
        &self,
        endpoint_tag: &str,
        stable_id: &str,
    ) -> Result<(), ApiError> {
        let request = pb::SetTailscaleExitNodeRequest {
            endpoint_tag: endpoint_tag.to_string(),
            stable_id: stable_id.to_string(),
        };
        self.unary_with_timeout("SetTailscaleExitNode", &request, TAILSCALE_ACTION_TIMEOUT)
    }

    /// `TailscaleLogout` — log the endpoint out and start a fresh
    /// interactive login; the new `auth_url` arrives on
    /// `stream_tailscale_status`. `endpoint_tag` must name the endpoint
    /// exactly.
    pub fn tailscale_logout(&self, endpoint_tag: &str) -> Result<(), ApiError> {
        let request = pb::TailscaleLogoutRequest {
            endpoint_tag: endpoint_tag.to_string(),
        };
        self.unary_with_timeout("TailscaleLogout", &request, TAILSCALE_ACTION_TIMEOUT)
    }
}

/// One Tailscale endpoint's status.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TailscaleEndpointStatus {
    pub endpoint_tag: String,
    /// Tailscale's backend state: `NoState`, `NeedsLogin`,
    /// `NeedsMachineAuth`, `InUseOtherUser`, `Stopped`, `Starting`,
    /// `Running`.
    pub backend_state: String,
    /// `backend_state` as display text, in English (BoxPilot sends no
    /// `accept-language`).
    pub state_text: String,
    /// Login URL to open while `NeedsLogin`; empty otherwise.
    pub auth_url: String,
    /// Tailnet name.
    pub network_name: String,
    pub magic_dns_suffix: String,
    /// This node.
    pub self_peer: Option<TailscalePeer>,
    /// Peers grouped by owning user.
    pub user_groups: Vec<TailscaleUserGroup>,
    /// The exit node in use, if any.
    pub exit_node: Option<TailscalePeer>,
    /// Logged in with an auth key (so logout has no interactive way back).
    pub key_auth: bool,
    /// Taildrop is available.
    pub can_share_files: bool,
    pub waiting_file_count: u32,
    pub receiving_file_count: u32,
    pub unread_file_count: u32,
    /// Domains `GetTailscaleCertificate` can issue for.
    pub cert_domains: Vec<String>,
}

impl TailscaleEndpointStatus {
    fn from_proto(status: pb::TailscaleEndpointStatus) -> Self {
        Self {
            endpoint_tag: status.endpoint_tag,
            backend_state: status.backend_state,
            state_text: status.state_text,
            auth_url: status.auth_url,
            network_name: status.network_name,
            magic_dns_suffix: status.magic_dns_suffix,
            self_peer: status.self_peer.map(TailscalePeer::from_proto),
            user_groups: status
                .user_groups
                .into_iter()
                .map(TailscaleUserGroup::from_proto)
                .collect(),
            exit_node: status.exit_node.map(TailscalePeer::from_proto),
            key_auth: status.key_auth,
            can_share_files: status.can_share_files,
            waiting_file_count: status.waiting_file_count.max(0) as u32,
            receiving_file_count: status.receiving_file_count.max(0) as u32,
            unread_file_count: status.unread_file_count.max(0) as u32,
            cert_domains: status.cert_domains,
        }
    }
}

/// The peers one tailnet user owns.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TailscaleUserGroup {
    pub user_id: i64,
    pub login_name: String,
    pub display_name: String,
    pub profile_pic_url: String,
    pub peers: Vec<TailscalePeer>,
}

impl TailscaleUserGroup {
    fn from_proto(group: pb::TailscaleUserGroup) -> Self {
        Self {
            user_id: group.user_id,
            login_name: group.login_name,
            display_name: group.display_name,
            profile_pic_url: group.profile_pic_url,
            peers: group
                .peers
                .into_iter()
                .map(TailscalePeer::from_proto)
                .collect(),
        }
    }
}

/// One tailnet node. Times are unix seconds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TailscalePeer {
    /// Identifies the node to `set_tailscale_exit_node`.
    pub stable_id: String,
    pub host_name: String,
    pub dns_name: String,
    pub os: String,
    pub tailscale_ips: Vec<String>,
    pub online: bool,
    /// Currently our exit node.
    pub exit_node: bool,
    /// Offers itself as an exit node.
    pub exit_node_option: bool,
    /// Traffic flowed recently.
    pub active: bool,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    /// `None` = the key never expires.
    pub key_expiry: Option<i64>,
    pub expired: bool,
    pub ssh_host_keys: Vec<String>,
    /// Shared into this tailnet from another one.
    pub sharee_node: bool,
    pub last_seen: Option<i64>,
    pub can_receive_files: bool,
}

impl TailscalePeer {
    fn from_proto(peer: pb::TailscalePeer) -> Self {
        Self {
            stable_id: peer.stable_id,
            host_name: peer.host_name,
            dns_name: peer.dns_name,
            os: peer.os,
            tailscale_ips: peer.tailscale_ips,
            online: peer.online,
            exit_node: peer.exit_node,
            exit_node_option: peer.exit_node_option,
            active: peer.active,
            rx_bytes: peer.rx_bytes.max(0) as u64,
            tx_bytes: peer.tx_bytes.max(0) as u64,
            key_expiry: (peer.key_expiry > 0).then_some(peer.key_expiry),
            expired: peer.expired,
            ssh_host_keys: peer.ssh_host_keys,
            sharee_node: peer.sharee_node,
            last_seen: (peer.last_seen > 0).then_some(peer.last_seen),
            can_receive_files: peer.can_receive_files,
        }
    }
}

/// One `StartTailscalePing` result.
#[derive(Clone, Debug, PartialEq)]
pub struct TailscalePing {
    pub latency_ms: f64,
    /// Reached the peer directly (`endpoint` set) rather than via DERP or a
    /// peer relay.
    pub is_direct: bool,
    /// The direct `ip:port`, if any.
    pub endpoint: String,
    /// The DERP relay used, when not direct.
    pub derp_region_id: i32,
    pub derp_region_code: String,
    /// The peer relay used, if any.
    pub peer_relay: String,
    /// This attempt failed; the stream continues.
    pub error: Option<String>,
}

impl TailscalePing {
    fn from_proto(response: pb::TailscalePingResponse) -> Self {
        Self {
            latency_ms: response.latency_ms,
            is_direct: response.is_direct,
            endpoint: response.endpoint,
            derp_region_id: response.derp_region_id,
            derp_region_code: response.derp_region_code,
            peer_relay: response.peer_relay,
            error: (!response.error.is_empty()).then_some(response.error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proto_peer(host: &str) -> pb::TailscalePeer {
        pb::TailscalePeer {
            host_name: host.into(),
            dns_name: format!("{}.tail1234.ts.net.", host),
            os: "windows".into(),
            tailscale_ips: vec!["100.64.0.1".into(), "fd7a:115c:a1e0::1".into()],
            online: true,
            exit_node: false,
            exit_node_option: true,
            active: true,
            rx_bytes: 1_000,
            tx_bytes: -1,
            key_expiry: 1_800_000_000,
            stable_id: format!("n{}", host),
            expired: false,
            ssh_host_keys: vec!["ssh-ed25519 AAAA".into()],
            sharee_node: false,
            last_seen: 0,
            can_receive_files: true,
        }
    }

    #[test]
    fn peer_maps_every_field() {
        assert_eq!(
            TailscalePeer::from_proto(proto_peer("box")),
            TailscalePeer {
                stable_id: "nbox".into(),
                host_name: "box".into(),
                dns_name: "box.tail1234.ts.net.".into(),
                os: "windows".into(),
                tailscale_ips: vec!["100.64.0.1".into(), "fd7a:115c:a1e0::1".into()],
                online: true,
                exit_node: false,
                exit_node_option: true,
                active: true,
                rx_bytes: 1_000,
                tx_bytes: 0,
                key_expiry: Some(1_800_000_000),
                expired: false,
                ssh_host_keys: vec!["ssh-ed25519 AAAA".into()],
                sharee_node: false,
                last_seen: None,
                can_receive_files: true,
            }
        );
    }

    #[test]
    fn endpoint_status_maps_every_field() {
        let status = TailscaleEndpointStatus::from_proto(pb::TailscaleEndpointStatus {
            endpoint_tag: "ts".into(),
            backend_state: "Running".into(),
            state_text: "Running".into(),
            auth_url: String::new(),
            network_name: "example.com".into(),
            magic_dns_suffix: "tail1234.ts.net".into(),
            self_peer: Some(proto_peer("me")),
            user_groups: vec![pb::TailscaleUserGroup {
                user_id: 7,
                login_name: "me@example.com".into(),
                display_name: "Me".into(),
                profile_pic_url: "https://example.com/me.png".into(),
                peers: vec![proto_peer("other")],
            }],
            exit_node: Some(proto_peer("exit")),
            key_auth: true,
            can_share_files: true,
            waiting_file_count: 1,
            receiving_file_count: 2,
            unread_file_count: -1,
            cert_domains: vec!["me.tail1234.ts.net".into()],
        });
        assert_eq!(status.endpoint_tag, "ts");
        assert_eq!(status.backend_state, "Running");
        assert_eq!(status.state_text, "Running");
        assert_eq!(status.network_name, "example.com");
        assert_eq!(status.magic_dns_suffix, "tail1234.ts.net");
        assert_eq!(status.self_peer.unwrap().host_name, "me");
        assert_eq!(status.user_groups.len(), 1);
        let group = &status.user_groups[0];
        assert_eq!(
            (
                group.user_id,
                group.login_name.as_str(),
                group.display_name.as_str()
            ),
            (7, "me@example.com", "Me")
        );
        assert_eq!(group.profile_pic_url, "https://example.com/me.png");
        assert_eq!(group.peers[0].host_name, "other");
        assert_eq!(status.exit_node.unwrap().stable_id, "nexit");
        assert!(status.key_auth && status.can_share_files);
        assert_eq!(
            (
                status.waiting_file_count,
                status.receiving_file_count,
                status.unread_file_count
            ),
            (1, 2, 0)
        );
        assert_eq!(status.cert_domains, vec!["me.tail1234.ts.net"]);
    }

    #[test]
    fn ping_maps_every_field_and_error() {
        let ping = TailscalePing::from_proto(pb::TailscalePingResponse {
            latency_ms: 12.5,
            is_direct: false,
            endpoint: String::new(),
            derp_region_id: 2,
            derp_region_code: "sfo".into(),
            error: String::new(),
            peer_relay: "100.64.0.9:41641".into(),
        });
        assert_eq!(
            ping,
            TailscalePing {
                latency_ms: 12.5,
                is_direct: false,
                endpoint: String::new(),
                derp_region_id: 2,
                derp_region_code: "sfo".into(),
                peer_relay: "100.64.0.9:41641".into(),
                error: None,
            }
        );
        let failed = TailscalePing::from_proto(pb::TailscalePingResponse {
            error: "timeout".into(),
            ..Default::default()
        });
        assert_eq!(failed.error.as_deref(), Some("timeout"));
    }
}
