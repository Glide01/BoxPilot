//! Minimal client for the sing-box API service (`services[]` entry of type
//! `api`, sing-box ≥ 1.14.0), plus pure helpers that derive selector groups
//! from `config.json` and URL-test results from the live group stream. No gpui
//! dependency — keep it that way so the core test shim keeps working.
//!
//! The service is a gRPC server (`daemon.StartedService`, upstream
//! `daemon/started_service.proto`). We speak its gRPC-Web flavour over the
//! blocking reqwest client: one HTTP/1.1 POST per call, request and response
//! bodies framed as `flag(1) | length(4, BE) | payload`, the gRPC status in a
//! final trailer frame (or in the HTTP headers for a trailers-only response).
//! That keeps tonic/tokio and build-time protobuf codegen out of the build;
//! the few messages we need are hand-derived below with the upstream field
//! numbers. See docs/adr/0002-sing-box-api-service.md.

use prost::Message;
use reqwest::blocking::{Client, Response};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::{ErrorKind, Read};
use std::time::Duration;

/// Tag of the `api` service BoxPilot injects into the runtime config.
/// Distinct from anything a subscription is likely to use, since service tags
/// share one namespace.
pub const API_SERVICE_TAG: &str = "boxpilot-api";

/// Fully qualified gRPC service name; every method path is
/// `/daemon.StartedService/<Method>`.
const SERVICE_NAME: &str = "daemon.StartedService";

/// Loopback-only API — short timeout so a missing/just-started sing-box fails
/// fast instead of hanging the caller.
const UNARY_TIMEOUT_SECS: u64 = 2;
/// Dial bound for the long-lived streams, so a not-yet-listening API fails
/// fast (the reader thread then retries).
const STREAM_CONNECT_TIMEOUT_SECS: u64 = 5;
/// Per-read bound on `SubscribeStatus`. sing-box emits one message per
/// `STATUS_INTERVAL` even while idle, so this never trips in normal operation;
/// it only lets the reader notice a wedged connection.
const STATUS_READ_TIMEOUT_SECS: u64 = 8;
/// Per-read bound on `SubscribeGroups`. That stream only pushes on change and
/// may sit idle indefinitely, so a timeout here is routine: the reader simply
/// re-subscribes and gets a fresh snapshot.
const GROUPS_READ_TIMEOUT_SECS: u64 = 30;
/// `SubscribeStatus` interval. The `uplink`/`downlink` fields are byte deltas
/// per interval, so one second makes them bytes/sec directly.
const STATUS_INTERVAL: Duration = Duration::from_secs(1);
/// Sanity cap on one gRPC-Web frame — a group snapshot is a few KiB.
const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

/// Hard cap on how long a URL test of one group counts as in flight.
/// `URLTest` returns immediately and runs in the background; every probe is
/// bounded by sing-box's `C.TCPTimeout` (15s).
pub const URL_TEST_WINDOW: Duration = Duration::from_secs(16);
/// A URL test also ends once the group stream has been quiet this long. A
/// failed probe deletes the node's history instead of reporting a failure, so
/// there is no "all done" signal when any node fails — but every probe that
/// finishes, success or failure, makes sing-box push a snapshot. Quiet for
/// this long means whatever is still running is slower than the old 5s
/// delay-test timeout; it shows `Timeout` until a late result replaces it.
pub const URL_TEST_QUIET: Duration = Duration::from_secs(5);

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
        format!("http://127.0.0.1:{}/{}/{}", self.port, SERVICE_NAME, method)
    }

    /// `SelectOutbound` — switch a selector group's node. sing-box persists
    /// the choice in `cache_file` itself.
    pub fn select_outbound(&self, group: &str, node: &str) -> Result<(), String> {
        let request = pb::SelectOutboundRequest {
            group_tag: group.to_string(),
            outbound_tag: node.to_string(),
        };
        self.unary(&unary_client()?, "SelectOutbound", &request)
            .map_err(|e| format!("Failed to switch node: {}", e))
    }

    /// `URLTest` — start a delay test of every node in `group` (a urltest
    /// group re-checks and may re-select). Returns as soon as sing-box has
    /// accepted it; results arrive on the `SubscribeGroups` stream.
    pub fn url_test(&self, group: &str) -> Result<(), String> {
        let request = pb::UrlTestRequest {
            outbound_tag: group.to_string(),
        };
        self.unary(&unary_client()?, "URLTest", &request)
            .map_err(|e| format!("Delay test failed: {}", e))
    }

    /// Stream `SubscribeGroups`, feeding each snapshot to `on_snapshot` until
    /// the stream ends/errors or the callback returns `false`. Blocking and
    /// never-ending while sing-box runs — run it on a dedicated thread (like
    /// the stdout/stderr pipe readers in `core/process.rs`), never on the
    /// async executor. sing-box sends a snapshot on subscribe, then again on
    /// every URL-test result (throttled to 4/s). `Err` carries why the stream
    /// ended so the caller can surface it; the caller decides whether to retry.
    pub fn stream_groups(
        &self,
        mut on_snapshot: impl FnMut(GroupsSnapshot) -> bool,
    ) -> Result<(), String> {
        let client = stream_client(GROUPS_READ_TIMEOUT_SECS)?;
        self.stream(&client, "SubscribeGroups", &(), |groups: pb::Groups| {
            on_snapshot(GroupsSnapshot::from_proto(groups))
        })
    }

    /// Stream `SubscribeStatus` at a one-second interval, feeding each traffic
    /// sample to `on_sample`. Same threading and termination contract as
    /// `stream_groups`.
    pub fn stream_status(
        &self,
        mut on_sample: impl FnMut(TrafficSample) -> bool,
    ) -> Result<(), String> {
        let client = stream_client(STATUS_READ_TIMEOUT_SECS)?;
        let request = pb::SubscribeStatusRequest {
            // Go `time.Duration`: nanoseconds.
            interval: STATUS_INTERVAL.as_nanos() as i64,
        };
        self.stream(
            &client,
            "SubscribeStatus",
            &request,
            |status: pb::Status| on_sample(TrafficSample::from_proto(&status)),
        )
    }

    /// A unary call whose response is `google.protobuf.Empty`.
    fn unary(&self, client: &Client, method: &str, request: &impl Message) -> Result<(), String> {
        self.stream(client, method, request, |_: ()| true)
    }

    /// Issue one call and decode each response message until the trailer
    /// frame (whose gRPC status decides the result) or until `on_message`
    /// returns `false`.
    fn stream<M: Message + Default>(
        &self,
        client: &Client,
        method: &str,
        request: &impl Message,
        mut on_message: impl FnMut(M) -> bool,
    ) -> Result<(), String> {
        let response = client
            .post(self.method_url(method))
            .header("Content-Type", "application/grpc-web+proto")
            .header("X-Grpc-Web", "1")
            .body(encode_frame(request))
            .send()
            .map_err(|e| format!("sing-box API unreachable: {}", e))?;
        if !response.status().is_success() {
            return Err(format!("sing-box API error: HTTP {}", response.status()));
        }
        // Trailers-only response (an error before any message): the gRPC
        // status rides in the HTTP headers instead of a trailer frame.
        let header_status = header_status(&response);
        if let Some(status) = &header_status {
            if status.status.is_some_and(|code| code != 0) {
                return status.clone().into_result();
            }
        }

        let mut body = response;
        loop {
            match read_frame(&mut body)? {
                Some(Frame::Data(payload)) => {
                    let message = M::decode(payload.as_slice())
                        .map_err(|e| format!("Invalid sing-box API response: {}", e))?;
                    if !on_message(message) {
                        return Ok(());
                    }
                }
                Some(Frame::Trailers(trailers)) => {
                    return match (trailers.status, header_status) {
                        (Some(_), _) => trailers.into_result(),
                        (None, Some(header)) => header.into_result(),
                        (None, None) => Err("sing-box API response has no gRPC status".into()),
                    };
                }
                None => return Err("sing-box API stream closed".to_string()),
            }
        }
    }
}

fn unary_client() -> Result<Client, String> {
    Client::builder()
        .timeout(Duration::from_secs(UNARY_TIMEOUT_SECS))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))
}

/// Long-lived stream client: only a dial bound and a per-read bound. Set
/// explicitly because the blocking client's 30s default would otherwise govern
/// each read.
fn stream_client(read_timeout_secs: u64) -> Result<Client, String> {
    Client::builder()
        .connect_timeout(Duration::from_secs(STREAM_CONNECT_TIMEOUT_SECS))
        .timeout(Duration::from_secs(read_timeout_secs))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))
}

/// Protobuf messages from upstream `daemon/started_service.proto` (sing-box
/// v1.14.x) — only the ones BoxPilot uses, with the upstream field numbers.
/// Unknown fields are skipped by prost, so upstream additions stay harmless.
mod pb {
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
}

// ---------------------------------------------------------------------------
// gRPC-Web framing
// ---------------------------------------------------------------------------

/// Flag bit marking a trailer (metadata) frame.
const FRAME_TRAILERS: u8 = 0x80;
/// Flag bit marking a compressed message — never requested, so never expected.
const FRAME_COMPRESSED: u8 = 0x01;

#[derive(Debug, PartialEq)]
enum Frame {
    Data(Vec<u8>),
    Trailers(GrpcStatus),
}

/// The gRPC status of a call, from a trailer frame or trailers-only headers.
#[derive(Clone, Debug, Default, PartialEq)]
struct GrpcStatus {
    status: Option<u32>,
    message: String,
}

impl GrpcStatus {
    fn into_result(self) -> Result<(), String> {
        match self.status {
            Some(0) => Ok(()),
            Some(code) if self.message.is_empty() => {
                Err(format!("sing-box API: {}", grpc_code_name(code)))
            }
            Some(_) => Err(format!("sing-box API: {}", self.message)),
            None => Err("sing-box API response has no gRPC status".into()),
        }
    }
}

/// Request body: one uncompressed data frame.
fn encode_frame(message: &impl Message) -> Vec<u8> {
    let payload = message.encode_to_vec();
    let mut frame = Vec::with_capacity(5 + payload.len());
    frame.push(0);
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    frame
}

/// Read one frame. `Ok(None)` on a clean EOF at a frame boundary.
fn read_frame(reader: &mut impl Read) -> Result<Option<Frame>, String> {
    let mut header = [0u8; 5];
    let mut filled = 0;
    while filled < header.len() {
        match reader.read(&mut header[filled..]) {
            Ok(0) if filled == 0 => return Ok(None),
            Ok(0) => return Err("sing-box API stream truncated".to_string()),
            Ok(n) => filled += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(format!("sing-box API stream error: {}", e)),
        }
    }
    let flags = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if len > MAX_FRAME_LEN {
        return Err(format!("sing-box API frame too large: {} bytes", len));
    }
    let mut payload = vec![0u8; len];
    reader
        .read_exact(&mut payload)
        .map_err(|e| format!("sing-box API stream error: {}", e))?;
    if flags & FRAME_TRAILERS != 0 {
        Ok(Some(Frame::Trailers(parse_trailers(&payload))))
    } else if flags & FRAME_COMPRESSED != 0 {
        Err("sing-box API sent a compressed frame".to_string())
    } else {
        Ok(Some(Frame::Data(payload)))
    }
}

/// Parse a trailer frame: HTTP/1-style `key: value\r\n` lines, keys in any
/// case.
fn parse_trailers(payload: &[u8]) -> GrpcStatus {
    let text = String::from_utf8_lossy(payload);
    let mut trailers = GrpcStatus::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key.trim().to_ascii_lowercase().as_str() {
            "grpc-status" => trailers.status = value.parse().ok(),
            "grpc-message" => trailers.message = percent_decode(value),
            _ => {}
        }
    }
    trailers
}

fn header_status(response: &Response) -> Option<GrpcStatus> {
    let headers = response.headers();
    let status = headers
        .get("grpc-status")?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok();
    let message = headers
        .get("grpc-message")
        .and_then(|v| v.to_str().ok())
        .map(percent_decode)
        .unwrap_or_default();
    Some(GrpcStatus { status, message })
}

/// `grpc-message` is percent-encoded (UTF-8). Malformed escapes pass through.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(byte) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn grpc_code_name(code: u32) -> String {
    let name = match code {
        1 => "cancelled",
        2 => "unknown error",
        3 => "invalid argument",
        4 => "deadline exceeded",
        5 => "not found",
        7 => "permission denied",
        12 => "unimplemented",
        13 => "internal error",
        14 => "unavailable",
        16 => "unauthenticated",
        _ => return format!("gRPC status {}", code),
    };
    name.to_string()
}

// ---------------------------------------------------------------------------
// Groups
// ---------------------------------------------------------------------------

/// Whether a group lets the user pick a node (`Selector`) or auto-selects the
/// fastest one by latency (`UrlTest`). URLTest groups are shown read-only:
/// their `now` reflects sing-box's automatic choice, and `SelectOutbound`
/// rejects anything that isn't a selector, so the UI disables node selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupKind {
    Selector,
    UrlTest,
}

/// One proxy outbound group: its tag, currently selected/active node, the
/// candidate nodes in config order, and whether the user may switch it.
#[derive(Clone, Debug, PartialEq)]
pub struct ProxyGroup {
    pub name: String,
    pub now: String,
    pub all: Vec<String>,
    pub kind: GroupKind,
}

/// A node's latest successful URL test, as sing-box's history storage holds
/// it. A failed test deletes the entry rather than recording a failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UrlTestHistory {
    /// Unix seconds.
    pub time: i64,
    pub delay_ms: u32,
}

/// One `SubscribeGroups` message, flattened for the UI.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GroupsSnapshot {
    /// In sing-box's outbound order — callers still order via `merge_groups`.
    pub groups: Vec<ProxyGroup>,
    /// Tag → protocol type (lowercase), for groups and their members.
    pub node_types: HashMap<String, String>,
    /// Tag → latest URL test; absent = untested or last test failed.
    pub history: HashMap<String, UrlTestHistory>,
}

impl GroupsSnapshot {
    fn from_proto(groups: pb::Groups) -> Self {
        let mut snapshot = Self::default();
        for group in groups.group {
            if group.items.is_empty() {
                continue;
            }
            snapshot
                .node_types
                .insert(group.tag.clone(), group.r#type.to_lowercase());
            let mut all = Vec::with_capacity(group.items.len());
            for item in group.items {
                snapshot
                    .node_types
                    .insert(item.tag.clone(), item.r#type.to_lowercase());
                if item.url_test_time > 0 {
                    snapshot.history.insert(
                        item.tag.clone(),
                        UrlTestHistory {
                            time: item.url_test_time,
                            delay_ms: item.url_test_delay.max(0) as u32,
                        },
                    );
                }
                all.push(item.tag);
            }
            snapshot.groups.push(ProxyGroup {
                name: group.tag,
                now: group.selected,
                all,
                kind: if group.selectable {
                    GroupKind::Selector
                } else {
                    GroupKind::UrlTest
                },
            });
        }
        snapshot
    }
}

/// Result of a node's most recent delay test, as shown on the Groups page.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DelayState {
    Ok(u32),
    /// Tested, but sing-box holds no result — the probe failed or timed out.
    Timeout,
}

/// Derive the per-node badges: any recorded history is a result; a node the
/// user tested (`timed_out_candidates`) with no history failed. Recomputed on
/// every snapshot, so a late result overrides an early `Timeout`.
pub fn delay_states(
    history: &HashMap<String, UrlTestHistory>,
    timed_out_candidates: &HashSet<String>,
) -> HashMap<String, DelayState> {
    let mut states: HashMap<String, DelayState> = timed_out_candidates
        .iter()
        .map(|node| (node.clone(), DelayState::Timeout))
        .collect();
    for (node, entry) in history {
        states.insert(node.clone(), DelayState::Ok(entry.delay_ms));
    }
    states
}

/// Whether a URL test of `group` started at `started_at` (unix seconds) has
/// visibly finished: every member has a result at least that recent. Failed
/// probes never show up here (they delete history) — see `url_test_done`.
pub fn url_test_settled(
    group: &ProxyGroup,
    history: &HashMap<String, UrlTestHistory>,
    started_at: i64,
) -> bool {
    group
        .all
        .iter()
        .all(|node| history.get(node).is_some_and(|h| h.time >= started_at))
}

/// Whether the Test button's spinner should stop: every member answered, or
/// the stream has gone quiet (`quiet` = time since the later of the test start
/// and the last snapshot), or the hard cap passed (`elapsed` = time since the
/// test start).
pub fn url_test_done(
    group: &ProxyGroup,
    history: &HashMap<String, UrlTestHistory>,
    started_at: i64,
    elapsed: Duration,
    quiet: Duration,
) -> bool {
    url_test_settled(group, history, started_at)
        || quiet >= URL_TEST_QUIET
        || elapsed >= URL_TEST_WINDOW
}

/// Parse proxy groups (selector + urltest) straight from `config.json` — the
/// ordering skeleton for `merge_groups`, and config-derived data for any group
/// the live API happens to lack. Garbage input yields an empty list, never an
/// error.
pub fn parse_groups_from_config(config: &str) -> Vec<ProxyGroup> {
    let Ok(json) = serde_json::from_str::<Value>(config) else {
        return Vec::new();
    };
    let Some(outbounds) = json.get("outbounds").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut groups = Vec::new();
    for outbound in outbounds {
        let kind = match outbound["type"].as_str() {
            Some("selector") => GroupKind::Selector,
            Some("urltest") => GroupKind::UrlTest,
            _ => continue,
        };
        let Some(name) = outbound["tag"].as_str() else {
            continue;
        };
        let all: Vec<String> = outbound["outbounds"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        if all.is_empty() {
            continue;
        }
        // Selector honours `default`; urltest has none, so the first node is a
        // placeholder until the live API supplies the auto-selected `now`.
        let now = outbound["default"]
            .as_str()
            .map(String::from)
            .unwrap_or_else(|| all[0].clone());
        groups.push(ProxyGroup {
            name: name.to_string(),
            now,
            all,
            kind,
        });
    }
    groups
}

/// 从 config.json 的 outbounds 提取 tag → 协议类型(lowercase)。
/// 补全 API 快照里没有的节点(只出现在组外的 outbound)。
pub fn parse_node_types_from_config(config: &str) -> HashMap<String, String> {
    let Ok(json) = serde_json::from_str::<Value>(config) else {
        return HashMap::new();
    };
    let Some(outbounds) = json.get("outbounds").and_then(Value::as_array) else {
        return HashMap::new();
    };
    outbounds
        .iter()
        .filter_map(|outbound| {
            let tag = outbound["tag"].as_str()?;
            let kind = outbound["type"].as_str()?;
            Some((tag.to_string(), kind.to_lowercase()))
        })
        .collect()
}

/// Order API groups by their position in config. Config entries missing from
/// the API keep their config-derived data; API-only extras (should not
/// happen, but covers a failed config parse) are appended sorted by name for
/// determinism.
pub fn merge_groups(config_order: &[ProxyGroup], api_groups: Vec<ProxyGroup>) -> Vec<ProxyGroup> {
    let mut by_name: HashMap<String, ProxyGroup> = api_groups
        .into_iter()
        .map(|g| (g.name.clone(), g))
        .collect();
    let mut merged: Vec<ProxyGroup> = config_order
        .iter()
        .map(|cfg| by_name.remove(&cfg.name).unwrap_or_else(|| cfg.clone()))
        .collect();
    let mut leftover: Vec<ProxyGroup> = by_name.into_values().collect();
    leftover.sort_by(|a, b| a.name.cmp(&b.name));
    merged.extend(leftover);
    merged
}

/// Nodes 页延迟徽标的色阶分档。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DelayLevel {
    /// <= 200 ms,绿色
    Fast,
    /// 201..=500 ms,黄色
    Medium,
    /// > 500 ms,红色
    Slow,
}

pub fn classify_delay(ms: u32) -> DelayLevel {
    match ms {
        0..=200 => DelayLevel::Fast,
        201..=500 => DelayLevel::Medium,
        _ => DelayLevel::Slow,
    }
}

// ---------------------------------------------------------------------------
// Traffic
// ---------------------------------------------------------------------------

/// One traffic sample: bytes transferred in the last one-second window, i.e.
/// an instantaneous rate in bytes/sec.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct TrafficSample {
    /// Upload rate, bytes/sec.
    pub up: u64,
    /// Download rate, bytes/sec.
    pub down: u64,
}

impl TrafficSample {
    fn from_proto(status: &pb::Status) -> Self {
        Self {
            up: status.uplink.max(0) as u64,
            down: status.downlink.max(0) as u64,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn data_frame(message: &impl Message) -> Vec<u8> {
        encode_frame(message)
    }

    fn trailer_frame(text: &str) -> Vec<u8> {
        let mut frame = vec![FRAME_TRAILERS];
        frame.extend_from_slice(&(text.len() as u32).to_be_bytes());
        frame.extend_from_slice(text.as_bytes());
        frame
    }

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

    #[test]
    fn request_frame_is_length_prefixed_protobuf() {
        let request = pb::SelectOutboundRequest {
            group_tag: "节点选择".into(),
            outbound_tag: "香港-01".into(),
        };
        let frame = encode_frame(&request);
        assert_eq!(frame[0], 0, "uncompressed data frame");
        let len = u32::from_be_bytes([frame[1], frame[2], frame[3], frame[4]]) as usize;
        assert_eq!(len, frame.len() - 5);
        let decoded = pb::SelectOutboundRequest::decode(&frame[5..]).unwrap();
        assert_eq!(decoded, request);
    }

    #[test]
    fn empty_request_is_a_zero_length_frame() {
        assert_eq!(encode_frame(&()), vec![0, 0, 0, 0, 0]);
    }

    /// Field numbers must match upstream `started_service.proto`: interval is
    /// field 1 (varint) — `0x08` tag byte.
    #[test]
    fn subscribe_status_request_uses_upstream_field_number() {
        let request = pb::SubscribeStatusRequest {
            interval: STATUS_INTERVAL.as_nanos() as i64,
        };
        let bytes = request.encode_to_vec();
        assert_eq!(bytes[0], 0x08);
        assert_eq!(
            pb::SubscribeStatusRequest::decode(bytes.as_slice())
                .unwrap()
                .interval,
            1_000_000_000
        );
    }

    #[test]
    fn frames_read_in_sequence_until_eof() {
        let status = pb::Status {
            uplink: 10,
            downlink: 20,
            ..Default::default()
        };
        let mut body = data_frame(&status);
        body.extend(trailer_frame("grpc-status: 0\r\n"));
        let mut reader = Cursor::new(body);
        match read_frame(&mut reader).unwrap() {
            Some(Frame::Data(payload)) => {
                assert_eq!(pb::Status::decode(payload.as_slice()).unwrap(), status)
            }
            other => panic!("expected data frame, got {:?}", other),
        }
        match read_frame(&mut reader).unwrap() {
            Some(Frame::Trailers(trailers)) => assert_eq!(trailers.into_result(), Ok(())),
            other => panic!("expected trailers, got {:?}", other),
        }
        assert_eq!(read_frame(&mut reader).unwrap(), None);
    }

    #[test]
    fn truncated_or_compressed_frames_are_errors() {
        assert!(read_frame(&mut Cursor::new(vec![0, 0, 0])).is_err());
        assert!(read_frame(&mut Cursor::new(vec![0, 0, 0, 0, 4, 1])).is_err());
        assert!(read_frame(&mut Cursor::new(vec![FRAME_COMPRESSED, 0, 0, 0, 0])).is_err());
    }

    #[test]
    fn trailers_parse_case_insensitively_and_decode_message() {
        let trailers = parse_trailers(
            b"Grpc-Status: 5\r\nGrpc-Message: outbound not found: %E9%A6%99%E6%B8%AF\r\n",
        );
        assert_eq!(trailers.status, Some(5));
        assert_eq!(
            trailers.into_result(),
            Err("sing-box API: outbound not found: 香港".to_string())
        );
    }

    #[test]
    fn nonzero_status_without_message_names_the_code() {
        assert_eq!(
            parse_trailers(b"grpc-status: 3\r\n").into_result(),
            Err("sing-box API: invalid argument".to_string())
        );
        assert!(parse_trailers(b"").into_result().is_err());
    }

    #[test]
    fn percent_decode_passes_malformed_escapes_through() {
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("%4"), "%4");
    }

    fn proto_groups() -> pb::Groups {
        pb::Groups {
            group: vec![
                pb::Group {
                    tag: "节点选择".into(),
                    r#type: "selector".into(),
                    selectable: true,
                    selected: "日本-02".into(),
                    is_expand: false,
                    items: vec![
                        pb::GroupItem {
                            tag: "香港-01".into(),
                            r#type: "vless".into(),
                            url_test_time: 1_700_000_000,
                            url_test_delay: 45,
                        },
                        pb::GroupItem {
                            tag: "日本-02".into(),
                            r#type: "Shadowsocks".into(),
                            url_test_time: 0,
                            url_test_delay: 0,
                        },
                        pb::GroupItem {
                            tag: "auto".into(),
                            r#type: "urltest".into(),
                            url_test_time: 1_700_000_001,
                            url_test_delay: 45,
                        },
                    ],
                },
                pb::Group {
                    tag: "auto".into(),
                    r#type: "urltest".into(),
                    selectable: false,
                    selected: "香港-01".into(),
                    is_expand: false,
                    items: vec![pb::GroupItem {
                        tag: "香港-01".into(),
                        r#type: "vless".into(),
                        url_test_time: 1_700_000_000,
                        url_test_delay: 45,
                    }],
                },
            ],
        }
    }

    #[test]
    fn snapshot_maps_groups_types_and_history() {
        let snapshot = GroupsSnapshot::from_proto(proto_groups());
        assert_eq!(snapshot.groups.len(), 2);
        let selector = &snapshot.groups[0];
        assert_eq!(selector.name, "节点选择");
        assert_eq!(selector.kind, GroupKind::Selector);
        assert_eq!(selector.now, "日本-02");
        assert_eq!(selector.all, vec!["香港-01", "日本-02", "auto"]);
        assert_eq!(snapshot.groups[1].kind, GroupKind::UrlTest);
        assert_eq!(snapshot.node_types["日本-02"], "shadowsocks", "lowercased");
        assert_eq!(snapshot.node_types["节点选择"], "selector");
        assert_eq!(snapshot.history["香港-01"].delay_ms, 45);
        assert!(
            !snapshot.history.contains_key("日本-02"),
            "url_test_time 0 means no history"
        );
    }

    #[test]
    fn snapshot_survives_a_protobuf_round_trip() {
        let bytes = proto_groups().encode_to_vec();
        let decoded = pb::Groups::decode(bytes.as_slice()).unwrap();
        assert_eq!(decoded, proto_groups());
    }

    #[test]
    fn delay_states_prefer_history_over_timeout() {
        let snapshot = GroupsSnapshot::from_proto(proto_groups());
        let tested: HashSet<String> = ["香港-01", "日本-02"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let states = delay_states(&snapshot.history, &tested);
        assert_eq!(states["香港-01"], DelayState::Ok(45));
        assert_eq!(states["日本-02"], DelayState::Timeout);
        assert_eq!(
            states["auto"],
            DelayState::Ok(45),
            "untested-by-user history still shows"
        );
    }

    #[test]
    fn url_test_settles_only_when_every_member_is_fresh() {
        let snapshot = GroupsSnapshot::from_proto(proto_groups());
        let auto = &snapshot.groups[1];
        assert!(url_test_settled(auto, &snapshot.history, 1_700_000_000));
        assert!(
            !url_test_settled(auto, &snapshot.history, 1_700_000_001),
            "stale result"
        );
        let selector = &snapshot.groups[0];
        assert!(
            !url_test_settled(selector, &snapshot.history, 0),
            "a member without history keeps the test open"
        );
    }

    #[test]
    fn url_test_ends_on_results_quiet_stream_or_cap() {
        let snapshot = GroupsSnapshot::from_proto(proto_groups());
        let selector = &snapshot.groups[0];
        let auto = &snapshot.groups[1];
        let short = Duration::from_secs(1);
        assert!(url_test_done(
            auto,
            &snapshot.history,
            1_700_000_000,
            short,
            short
        ));
        assert!(!url_test_done(selector, &snapshot.history, 0, short, short));
        assert!(url_test_done(
            selector,
            &snapshot.history,
            0,
            URL_TEST_QUIET,
            URL_TEST_QUIET
        ));
        assert!(
            url_test_done(selector, &snapshot.history, 0, URL_TEST_WINDOW, short),
            "a stream kept busy by other activity still ends at the cap"
        );
    }

    #[test]
    fn traffic_sample_clamps_negative_deltas() {
        let status = pb::Status {
            uplink: -5,
            downlink: 1234,
            ..Default::default()
        };
        assert_eq!(
            TrafficSample::from_proto(&status),
            TrafficSample { up: 0, down: 1234 }
        );
    }

    const CONFIG_WITH_SELECTORS: &str = r#"{
        "outbounds": [
            {"type": "vless", "tag": "香港-01"},
            {"type": "selector", "tag": "节点选择", "outbounds": ["香港-01", "日本-02"], "default": "日本-02"},
            {"type": "selector", "tag": "无默认", "outbounds": ["香港-01"]},
            {"type": "selector", "tag": "空组", "outbounds": []},
            {"type": "urltest", "tag": "auto", "outbounds": ["香港-01"]}
        ]
    }"#;

    #[test]
    fn config_parse_extracts_groups_in_order() {
        let groups = parse_groups_from_config(CONFIG_WITH_SELECTORS);
        assert_eq!(groups.len(), 3, "empty group dropped; urltest kept");
        assert_eq!(groups[0].name, "节点选择");
        assert_eq!(groups[0].kind, GroupKind::Selector);
        assert_eq!(groups[0].now, "日本-02", "now must come from `default`");
        assert_eq!(groups[1].name, "无默认");
        assert_eq!(
            groups[1].now, "香港-01",
            "missing `default` falls back to first node"
        );
        assert_eq!(groups[2].name, "auto");
        assert_eq!(groups[2].kind, GroupKind::UrlTest);
        assert_eq!(
            groups[2].now, "香港-01",
            "urltest has no `default` → first node placeholder"
        );
    }

    #[test]
    fn config_parse_tolerates_garbage() {
        assert!(parse_groups_from_config("not json").is_empty());
        assert!(parse_groups_from_config(r#"{"outbounds": "nope"}"#).is_empty());
        assert!(parse_groups_from_config("{}").is_empty());
    }

    fn group(name: &str, now: &str, all: &[&str]) -> ProxyGroup {
        ProxyGroup {
            name: name.into(),
            now: now.into(),
            all: all.iter().map(|s| s.to_string()).collect(),
            kind: GroupKind::Selector,
        }
    }

    #[test]
    fn merge_orders_by_config_and_takes_api_data() {
        let config = vec![group("A", "a1", &["a1", "a2"]), group("B", "b1", &["b1"])];
        let api = vec![group("B", "b1", &["b1"]), group("A", "a2", &["a1", "a2"])];
        let merged = merge_groups(&config, api);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].name, "A", "config order wins over API order");
        assert_eq!(merged[0].now, "a2", "API data wins over config default");
        assert_eq!(merged[1].name, "B");
    }

    #[test]
    fn merge_keeps_config_entry_when_api_lacks_group_and_appends_api_extras() {
        let config = vec![group("A", "a1", &["a1"])];
        let api = vec![group("Z", "z1", &["z1"]), group("M", "m1", &["m1"])];
        let merged = merge_groups(&config, api);
        assert_eq!(merged[0].name, "A", "config skeleton survives");
        assert_eq!(
            merged[1].name, "M",
            "API-only extras appended sorted by name"
        );
        assert_eq!(merged[2].name, "Z");
    }

    #[test]
    fn node_types_from_config_map_tag_to_type() {
        let types = parse_node_types_from_config(CONFIG_WITH_SELECTORS);
        assert_eq!(types["香港-01"], "vless");
        assert_eq!(types["节点选择"], "selector");
        assert_eq!(types["auto"], "urltest");
    }

    #[test]
    fn node_types_from_config_tolerate_garbage() {
        assert!(parse_node_types_from_config("not json").is_empty());
        assert!(parse_node_types_from_config(r#"{"outbounds": "nope"}"#).is_empty());
    }

    #[test]
    fn delay_levels_split_at_200_and_500() {
        assert_eq!(classify_delay(45), DelayLevel::Fast);
        assert_eq!(classify_delay(200), DelayLevel::Fast);
        assert_eq!(classify_delay(201), DelayLevel::Medium);
        assert_eq!(classify_delay(500), DelayLevel::Medium);
        assert_eq!(classify_delay(501), DelayLevel::Slow);
    }
}
