//! Protobuf messages from upstream `daemon/started_service.proto` (sing-box
//! v1.14.x), hand-derived with the upstream field numbers — only the ones
//! BoxPilot calls. Unknown fields are skipped by prost, so upstream additions
//! stay harmless. Proto enums are kept as `int32` (wire-identical) and mapped
//! to Rust enums in each domain module's `from_proto`. Field names follow
//! the proto in snake_case; nothing outside `singbox_api` sees these types.

// --- Service --------------------------------------------------------------

#[derive(Clone, PartialEq, prost::Message)]
pub struct Version {
    #[prost(string, tag = "1")]
    pub version: String,
    #[prost(int32, tag = "2")]
    pub api_version: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ServiceStatus {
    /// `ServiceStatus.Type`: IDLE 0, STARTING 1, STARTED 2, STOPPING 3,
    /// FATAL 4.
    #[prost(int32, tag = "1")]
    pub status: i32,
    #[prost(string, tag = "2")]
    pub error_message: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct StartedAt {
    /// Unix milliseconds.
    #[prost(int64, tag = "1")]
    pub started_at: i64,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct DeprecatedWarnings {
    #[prost(message, repeated, tag = "1")]
    pub warnings: Vec<DeprecatedWarning>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct DeprecatedWarning {
    #[prost(string, tag = "1")]
    pub message: String,
    #[prost(bool, tag = "2")]
    pub impending: bool,
    #[prost(string, tag = "3")]
    pub migration_link: String,
    #[prost(string, tag = "4")]
    pub description: String,
    #[prost(string, tag = "5")]
    pub deprecated_version: String,
    #[prost(string, tag = "6")]
    pub scheduled_version: String,
}

// --- Logs -----------------------------------------------------------------

#[derive(Clone, PartialEq, prost::Message)]
pub struct Log {
    #[prost(message, repeated, tag = "1")]
    pub messages: Vec<LogMessage>,
    #[prost(bool, tag = "2")]
    pub reset: bool,
}

/// `Log.Message`.
#[derive(Clone, PartialEq, prost::Message)]
pub struct LogMessage {
    /// `LogLevel`: PANIC 0, FATAL 1, ERROR 2, WARN 3, INFO 4, DEBUG 5,
    /// TRACE 6.
    #[prost(int32, tag = "1")]
    pub level: i32,
    #[prost(string, tag = "2")]
    pub message: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct DefaultLogLevel {
    /// `LogLevel`.
    #[prost(int32, tag = "1")]
    pub level: i32,
}

// --- Status ---------------------------------------------------------------

#[derive(Clone, PartialEq, prost::Message)]
pub struct SubscribeStatusRequest {
    /// Go `time.Duration` (nanoseconds); ≤ 0 means one second.
    #[prost(int64, tag = "1")]
    pub interval: i64,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Status {
    #[prost(uint64, tag = "1")]
    pub memory: u64,
    #[prost(int32, tag = "2")]
    pub goroutines: i32,
    #[prost(int32, tag = "3")]
    pub connections_in: i32,
    #[prost(int32, tag = "4")]
    pub connections_out: i32,
    #[prost(bool, tag = "5")]
    pub traffic_available: bool,
    /// Bytes uploaded during the last interval (0 in the first message).
    #[prost(int64, tag = "6")]
    pub uplink: i64,
    /// Bytes downloaded during the last interval (0 in the first message).
    #[prost(int64, tag = "7")]
    pub downlink: i64,
    #[prost(int64, tag = "8")]
    pub uplink_total: i64,
    #[prost(int64, tag = "9")]
    pub downlink_total: i64,
}

// --- Groups and outbounds -------------------------------------------------

#[derive(Clone, PartialEq, prost::Message)]
pub struct Groups {
    #[prost(message, repeated, tag = "1")]
    pub group: Vec<Group>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Group {
    #[prost(string, tag = "1")]
    pub tag: String,
    #[prost(string, tag = "2")]
    pub r#type: String,
    /// True only for `selector` groups.
    #[prost(bool, tag = "3")]
    pub selectable: bool,
    #[prost(string, tag = "4")]
    pub selected: String,
    #[prost(bool, tag = "5")]
    pub is_expand: bool,
    #[prost(message, repeated, tag = "6")]
    pub items: Vec<GroupItem>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct GroupItem {
    #[prost(string, tag = "1")]
    pub tag: String,
    #[prost(string, tag = "2")]
    pub r#type: String,
    /// Unix seconds of the last successful URL test; 0 = no history.
    #[prost(int64, tag = "3")]
    pub url_test_time: i64,
    /// Milliseconds.
    #[prost(int32, tag = "4")]
    pub url_test_delay: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct UrlTestRequest {
    #[prost(string, tag = "1")]
    pub outbound_tag: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct SelectOutboundRequest {
    #[prost(string, tag = "1")]
    pub group_tag: String,
    #[prost(string, tag = "2")]
    pub outbound_tag: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct SetGroupExpandRequest {
    #[prost(string, tag = "1")]
    pub group_tag: String,
    #[prost(bool, tag = "2")]
    pub is_expand: bool,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct OutboundList {
    #[prost(message, repeated, tag = "1")]
    pub outbounds: Vec<GroupItem>,
}

// --- Clash mode -----------------------------------------------------------

#[derive(Clone, PartialEq, prost::Message)]
pub struct ClashMode {
    /// Field 3 upstream — not a typo.
    #[prost(string, tag = "3")]
    pub mode: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ClashModeStatus {
    #[prost(string, repeated, tag = "1")]
    pub mode_list: Vec<String>,
    #[prost(string, tag = "2")]
    pub current_mode: String,
}

// --- Connections ----------------------------------------------------------

#[derive(Clone, PartialEq, prost::Message)]
pub struct SubscribeConnectionsRequest {
    /// Go `time.Duration` (nanoseconds); ≤ 0 means one second.
    #[prost(int64, tag = "1")]
    pub interval: i64,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ConnectionEvents {
    #[prost(message, repeated, tag = "1")]
    pub events: Vec<ConnectionEvent>,
    #[prost(bool, tag = "2")]
    pub reset: bool,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ConnectionEvent {
    /// `ConnectionEventType`: NEW 0, UPDATE 1, CLOSED 2.
    #[prost(int32, tag = "1")]
    pub r#type: i32,
    #[prost(string, tag = "2")]
    pub id: String,
    #[prost(message, optional, tag = "3")]
    pub connection: Option<Connection>,
    #[prost(int64, tag = "4")]
    pub uplink_delta: i64,
    #[prost(int64, tag = "5")]
    pub downlink_delta: i64,
    /// Unix milliseconds.
    #[prost(int64, tag = "6")]
    pub closed_at: i64,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Connection {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(string, tag = "2")]
    pub inbound: String,
    #[prost(string, tag = "3")]
    pub inbound_type: String,
    #[prost(int32, tag = "4")]
    pub ip_version: i32,
    #[prost(string, tag = "5")]
    pub network: String,
    #[prost(string, tag = "6")]
    pub source: String,
    #[prost(string, tag = "7")]
    pub destination: String,
    #[prost(string, tag = "8")]
    pub domain: String,
    #[prost(string, tag = "9")]
    pub protocol: String,
    #[prost(string, tag = "10")]
    pub user: String,
    #[prost(string, tag = "11")]
    pub from_outbound: String,
    /// Unix milliseconds.
    #[prost(int64, tag = "12")]
    pub created_at: i64,
    /// Unix milliseconds; 0 while open.
    #[prost(int64, tag = "13")]
    pub closed_at: i64,
    /// Never set by sing-box 1.14 (rates travel as UPDATE deltas).
    #[prost(int64, tag = "14")]
    pub uplink: i64,
    /// Never set by sing-box 1.14 (rates travel as UPDATE deltas).
    #[prost(int64, tag = "15")]
    pub downlink: i64,
    #[prost(int64, tag = "16")]
    pub uplink_total: i64,
    #[prost(int64, tag = "17")]
    pub downlink_total: i64,
    #[prost(string, tag = "18")]
    pub rule: String,
    #[prost(string, tag = "19")]
    pub outbound: String,
    #[prost(string, tag = "20")]
    pub outbound_type: String,
    #[prost(string, repeated, tag = "21")]
    pub chain_list: Vec<String>,
    #[prost(message, optional, tag = "22")]
    pub process_info: Option<ProcessInfo>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ProcessInfo {
    #[prost(uint32, tag = "1")]
    pub process_id: u32,
    #[prost(int32, tag = "2")]
    pub user_id: i32,
    #[prost(string, tag = "3")]
    pub user_name: String,
    #[prost(string, tag = "4")]
    pub process_path: String,
    #[prost(string, repeated, tag = "5")]
    pub package_names: Vec<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct CloseConnectionRequest {
    #[prost(string, tag = "1")]
    pub id: String,
}

// --- Diagnostics ----------------------------------------------------------

#[derive(Clone, PartialEq, prost::Message)]
pub struct NetworkQualityTestRequest {
    #[prost(string, tag = "1")]
    pub config_url: String,
    #[prost(string, tag = "2")]
    pub outbound_tag: String,
    #[prost(bool, tag = "3")]
    pub serial: bool,
    #[prost(int32, tag = "4")]
    pub max_runtime_seconds: i32,
    #[prost(bool, tag = "5")]
    pub http3: bool,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct NetworkQualityTestProgress {
    #[prost(int32, tag = "1")]
    pub phase: i32,
    #[prost(int64, tag = "2")]
    pub download_capacity: i64,
    #[prost(int64, tag = "3")]
    pub upload_capacity: i64,
    #[prost(int32, tag = "4")]
    pub download_rpm: i32,
    #[prost(int32, tag = "5")]
    pub upload_rpm: i32,
    #[prost(int32, tag = "6")]
    pub idle_latency_ms: i32,
    #[prost(int64, tag = "7")]
    pub elapsed_ms: i64,
    #[prost(bool, tag = "8")]
    pub is_final: bool,
    #[prost(string, tag = "9")]
    pub error: String,
    #[prost(int32, tag = "10")]
    pub download_capacity_accuracy: i32,
    #[prost(int32, tag = "11")]
    pub upload_capacity_accuracy: i32,
    #[prost(int32, tag = "12")]
    pub download_rpm_accuracy: i32,
    #[prost(int32, tag = "13")]
    pub upload_rpm_accuracy: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct StunTestRequest {
    #[prost(string, tag = "1")]
    pub server: String,
    #[prost(string, tag = "2")]
    pub outbound_tag: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct StunTestProgress {
    #[prost(int32, tag = "1")]
    pub phase: i32,
    #[prost(string, tag = "2")]
    pub external_addr: String,
    #[prost(int32, tag = "3")]
    pub latency_ms: i32,
    #[prost(int32, tag = "4")]
    pub nat_mapping: i32,
    #[prost(int32, tag = "5")]
    pub nat_filtering: i32,
    #[prost(bool, tag = "6")]
    pub is_final: bool,
    #[prost(string, tag = "7")]
    pub error: String,
    #[prost(bool, tag = "8")]
    pub nat_type_supported: bool,
}

// --- Tailscale ------------------------------------------------------------

#[derive(Clone, PartialEq, prost::Message)]
pub struct TailscaleStatusUpdate {
    #[prost(message, repeated, tag = "1")]
    pub endpoints: Vec<TailscaleEndpointStatus>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct TailscaleEndpointStatus {
    #[prost(string, tag = "1")]
    pub endpoint_tag: String,
    #[prost(string, tag = "2")]
    pub backend_state: String,
    #[prost(string, tag = "3")]
    pub state_text: String,
    #[prost(string, tag = "4")]
    pub auth_url: String,
    #[prost(string, tag = "5")]
    pub network_name: String,
    #[prost(string, tag = "6")]
    pub magic_dns_suffix: String,
    /// `self` upstream.
    #[prost(message, optional, tag = "7")]
    pub self_peer: Option<TailscalePeer>,
    #[prost(message, repeated, tag = "8")]
    pub user_groups: Vec<TailscaleUserGroup>,
    #[prost(message, optional, tag = "9")]
    pub exit_node: Option<TailscalePeer>,
    #[prost(bool, tag = "10")]
    pub key_auth: bool,
    #[prost(bool, tag = "11")]
    pub can_share_files: bool,
    #[prost(int32, tag = "12")]
    pub waiting_file_count: i32,
    #[prost(int32, tag = "13")]
    pub receiving_file_count: i32,
    #[prost(int32, tag = "14")]
    pub unread_file_count: i32,
    #[prost(string, repeated, tag = "15")]
    pub cert_domains: Vec<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct TailscaleUserGroup {
    #[prost(int64, tag = "1")]
    pub user_id: i64,
    #[prost(string, tag = "2")]
    pub login_name: String,
    #[prost(string, tag = "3")]
    pub display_name: String,
    #[prost(string, tag = "4")]
    pub profile_pic_url: String,
    #[prost(message, repeated, tag = "5")]
    pub peers: Vec<TailscalePeer>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct TailscalePeer {
    #[prost(string, tag = "1")]
    pub host_name: String,
    #[prost(string, tag = "2")]
    pub dns_name: String,
    #[prost(string, tag = "3")]
    pub os: String,
    #[prost(string, repeated, tag = "4")]
    pub tailscale_ips: Vec<String>,
    #[prost(bool, tag = "5")]
    pub online: bool,
    #[prost(bool, tag = "6")]
    pub exit_node: bool,
    #[prost(bool, tag = "7")]
    pub exit_node_option: bool,
    #[prost(bool, tag = "8")]
    pub active: bool,
    #[prost(int64, tag = "9")]
    pub rx_bytes: i64,
    #[prost(int64, tag = "10")]
    pub tx_bytes: i64,
    /// Unix seconds; 0 = no expiry.
    #[prost(int64, tag = "11")]
    pub key_expiry: i64,
    #[prost(string, tag = "12")]
    pub stable_id: String,
    #[prost(bool, tag = "13")]
    pub expired: bool,
    #[prost(string, repeated, tag = "14")]
    pub ssh_host_keys: Vec<String>,
    #[prost(bool, tag = "15")]
    pub sharee_node: bool,
    /// Unix seconds; 0 = unknown.
    #[prost(int64, tag = "16")]
    pub last_seen: i64,
    #[prost(bool, tag = "17")]
    pub can_receive_files: bool,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct TailscalePingRequest {
    #[prost(string, tag = "1")]
    pub endpoint_tag: String,
    #[prost(string, tag = "2")]
    pub peer_ip: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct TailscalePingResponse {
    #[prost(double, tag = "1")]
    pub latency_ms: f64,
    #[prost(bool, tag = "2")]
    pub is_direct: bool,
    #[prost(string, tag = "3")]
    pub endpoint: String,
    #[prost(int32, tag = "4")]
    pub derp_region_id: i32,
    #[prost(string, tag = "5")]
    pub derp_region_code: String,
    #[prost(string, tag = "6")]
    pub error: String,
    #[prost(string, tag = "7")]
    pub peer_relay: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct SetTailscaleExitNodeRequest {
    #[prost(string, tag = "1")]
    pub endpoint_tag: String,
    #[prost(string, tag = "2")]
    pub stable_id: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct TailscaleLogoutRequest {
    #[prost(string, tag = "1")]
    pub endpoint_tag: String,
}

// --- Notifications --------------------------------------------------------

#[derive(Clone, PartialEq, prost::Message)]
pub struct NotificationEvent {
    #[prost(oneof = "notification_event::Event", tags = "1, 2")]
    pub event: Option<notification_event::Event>,
}

pub mod notification_event {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Event {
        #[prost(message, tag = "1")]
        Send(super::Notification),
        #[prost(message, tag = "2")]
        Cancel(super::NotificationCancel),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Notification {
    #[prost(string, tag = "1")]
    pub identifier: String,
    #[prost(string, tag = "2")]
    pub type_name: String,
    #[prost(int32, tag = "3")]
    pub type_id: i32,
    #[prost(string, tag = "4")]
    pub title: String,
    #[prost(string, tag = "5")]
    pub subtitle: String,
    #[prost(string, tag = "6")]
    pub body: String,
    #[prost(string, tag = "7")]
    pub open_url: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct NotificationCancel {
    #[prost(string, tag = "1")]
    pub identifier: String,
    #[prost(int32, tag = "2")]
    pub type_id: i32,
}

/// Request encodings pinned byte for byte, so a field-number slip can't hide
/// behind a symmetric encode/decode round trip. Tag byte = `field << 3 |
/// wire type` (0 varint, 1 fixed64, 2 length-delimited).
#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;

    fn bytes(message: &impl Message) -> Vec<u8> {
        message.encode_to_vec()
    }

    #[test]
    fn subscribe_requests_carry_interval_in_field_1() {
        let second = 1_000_000_000i64;
        let expected = [0x08, 0x80, 0x94, 0xeb, 0xdc, 0x03];
        assert_eq!(
            bytes(&SubscribeStatusRequest { interval: second }),
            expected
        );
        assert_eq!(
            bytes(&SubscribeConnectionsRequest { interval: second }),
            expected
        );
    }

    #[test]
    fn group_requests_use_upstream_field_numbers() {
        assert_eq!(
            bytes(&UrlTestRequest {
                outbound_tag: "a".into()
            }),
            [0x0a, 1, b'a']
        );
        assert_eq!(
            bytes(&SelectOutboundRequest {
                group_tag: "g".into(),
                outbound_tag: "n".into()
            }),
            [0x0a, 1, b'g', 0x12, 1, b'n']
        );
        assert_eq!(
            bytes(&SetGroupExpandRequest {
                group_tag: "g".into(),
                is_expand: true
            }),
            [0x0a, 1, b'g', 0x10, 1]
        );
    }

    #[test]
    fn clash_mode_lives_in_field_3() {
        assert_eq!(
            bytes(&ClashMode {
                mode: "Rule".into()
            }),
            [0x1a, 4, b'R', b'u', b'l', b'e']
        );
    }

    #[test]
    fn close_connection_request_uses_field_1() {
        assert_eq!(
            bytes(&CloseConnectionRequest { id: "x".into() }),
            [0x0a, 1, b'x']
        );
    }

    #[test]
    fn diagnostic_requests_use_upstream_field_numbers() {
        assert_eq!(
            bytes(&NetworkQualityTestRequest {
                config_url: "u".into(),
                outbound_tag: "o".into(),
                serial: true,
                max_runtime_seconds: 5,
                http3: true,
            }),
            [0x0a, 1, b'u', 0x12, 1, b'o', 0x18, 1, 0x20, 5, 0x28, 1]
        );
        assert_eq!(
            bytes(&StunTestRequest {
                server: "s".into(),
                outbound_tag: "o".into()
            }),
            [0x0a, 1, b's', 0x12, 1, b'o']
        );
    }

    #[test]
    fn tailscale_requests_use_upstream_field_numbers() {
        assert_eq!(
            bytes(&TailscalePingRequest {
                endpoint_tag: "t".into(),
                peer_ip: "p".into()
            }),
            [0x0a, 1, b't', 0x12, 1, b'p']
        );
        assert_eq!(
            bytes(&SetTailscaleExitNodeRequest {
                endpoint_tag: "t".into(),
                stable_id: "s".into()
            }),
            [0x0a, 1, b't', 0x12, 1, b's']
        );
        assert_eq!(
            bytes(&TailscaleLogoutRequest {
                endpoint_tag: "t".into()
            }),
            [0x0a, 1, b't']
        );
    }

    /// Responses with an oddity worth pinning: the oneof arms are fields 1
    /// and 2 of `NotificationEvent`; `self` is field 7 of the endpoint
    /// status; the ping latency is a double (fixed64 wire type).
    #[test]
    fn response_oddities_decode_from_upstream_bytes() {
        let cancel = NotificationEvent::decode(&[0x12, 4, 0x0a, 0, 0x10, 7][..]).unwrap();
        assert_eq!(
            cancel.event,
            Some(notification_event::Event::Cancel(NotificationCancel {
                identifier: String::new(),
                type_id: 7
            }))
        );

        let status = TailscaleEndpointStatus::decode(&[0x3a, 3, 0x0a, 1, b'h'][..]).unwrap();
        assert_eq!(status.self_peer.unwrap().host_name, "h");

        let mut ping = vec![0x09];
        ping.extend_from_slice(&12.5f64.to_le_bytes());
        let ping = TailscalePingResponse::decode(ping.as_slice()).unwrap();
        assert_eq!(ping.latency_ms, 12.5);
    }
}
