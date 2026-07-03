//! Minimal client for sing-box's Clash-compatible REST API
//! (`experimental.clash_api`), plus pure helpers that derive selector groups
//! from `config.json` for the stopped state. No gpui dependency — keep it
//! that way so the core test shim keeps working.

use crate::core::settings::{DELAY_TEST_TIMEOUT_MS, DELAY_TEST_URL};
use reqwest::blocking::Client;
use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::time::Duration;

/// Loopback-only API — short timeout so a missing/just-started sing-box
/// fails fast instead of hanging the retry loop.
const CLASH_API_TIMEOUT_SECS: u64 = 2;

/// 整组测速服务端最多按每节点 DELAY_TEST_TIMEOUT_MS 并发跑——HTTP client
/// 超时必须远大于 2s 的元数据超时,否则必然中途掐断。
const DELAY_REQUEST_TIMEOUT_SECS: u64 = 30;

/// The Clash API endpoint (sing-box's `experimental.clash_api`), always on
/// loopback. The single owner of host + port and of URL construction: every
/// request URL *and* the `external_controller` string injected into the
/// runtime config derive from here, so the port-assembly rule and
/// percent-encoding of non-ASCII group names live in one place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClashApi {
    port: u16,
}

impl ClashApi {
    pub fn new(port: u16) -> Self {
        Self { port }
    }

    /// The `experimental.clash_api.external_controller` value for the runtime
    /// config — sing-box must listen exactly where this client will call.
    pub fn external_controller(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    /// Base URL with `segments` appended as individually percent-encoded path
    /// segments (safe for non-ASCII group names).
    fn url(&self, segments: &[&str]) -> reqwest::Url {
        let mut url = reqwest::Url::parse(&format!("http://{}", self.external_controller()))
            .expect("loopback base URL is valid");
        {
            let mut path = url
                .path_segments_mut()
                .expect("http URL can have path segments");
            for segment in segments {
                path.push(segment);
            }
        }
        url
    }

    /// PUT /proxies/{group} URL. Public so percent-encoding is testable
    /// without HTTP.
    pub fn select_url(&self, group: &str) -> String {
        self.url(&["proxies", group]).to_string()
    }

    /// GET /group/{name}/delay URL(含 query)。公开以便测中文组名的
    /// percent-encoding 和 query 参数,无需 HTTP。
    pub fn group_delay_url(&self, group: &str, test_url: &str, timeout_ms: u32) -> String {
        let mut url = self.url(&["group", group, "delay"]);
        url.query_pairs_mut()
            .append_pair("url", test_url)
            .append_pair("timeout", &timeout_ms.to_string());
        url.to_string()
    }

    /// GET /proxies:selector 分组 + 节点协议 map(同一份响应体两次解析)。
    pub fn fetch_proxies(&self) -> Result<(Vec<ProxyGroup>, HashMap<String, String>), String> {
        let response = client()?
            .get(self.url(&["proxies"]))
            .send()
            .map_err(|e| format!("Clash API unreachable: {}", e))?;
        if !response.status().is_success() {
            return Err(format!("Clash API error: {}", response.status()));
        }
        let body = response
            .text()
            .map_err(|e| format!("Failed to read Clash API response: {}", e))?;
        let groups = parse_proxies_response(&body)?;
        let types = parse_node_types_response(&body);
        Ok((groups, types))
    }

    /// PUT /proxies/{group} to switch the selected node.
    /// (reqwest's `json` feature is not enabled in this crate — set the header
    /// and body manually.)
    pub fn select_proxy(&self, group: &str, node: &str) -> Result<(), String> {
        let body = serde_json::json!({ "name": node }).to_string();
        let response = client()?
            .put(self.select_url(group))
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .map_err(|e| format!("Clash API unreachable: {}", e))?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(format!("Failed to switch node: {}", response.status()))
        }
    }

    /// GET /group/{name}/delay — sing-box 并发测完整组后一次性返回。
    pub fn test_group_delay(&self, group: &str) -> Result<HashMap<String, u32>, String> {
        let url = self.group_delay_url(group, DELAY_TEST_URL, DELAY_TEST_TIMEOUT_MS);
        let client = Client::builder()
            .timeout(Duration::from_secs(DELAY_REQUEST_TIMEOUT_SECS))
            .build()
            .map_err(|e| format!("Failed to create HTTP client: {}", e))?;
        let response = client
            .get(url)
            .send()
            .map_err(|e| format!("Delay test failed: {}", e))?;
        if !response.status().is_success() {
            return Err(format!("Delay test failed: {}", response.status()));
        }
        let body = response
            .text()
            .map_err(|e| format!("Failed to read delay response: {}", e))?;
        parse_delay_response(&body)
    }

    /// Stream `GET /traffic`, feeding each parsed sample to `on_sample` until
    /// the connection closes/errors or the callback returns `false`. Blocking:
    /// the endpoint streams one newline-delimited `{"up","down"}` object per
    /// second and never closes on its own, so run this on a dedicated thread
    /// (mirrors the stdout/stderr pipe readers in `core/process.rs`), never on
    /// the async executor.
    ///
    /// The client is built explicitly rather than via `Client::new()`: only a
    /// dial bound (`connect_timeout`) and a generous per-read bound
    /// (`TRAFFIC_READ_TIMEOUT_SECS`, well above the 1s emission cadence) are
    /// set, the latter just so the reader can notice a wedged connection. Any
    /// error (connection reset when sing-box exits, read timeout) simply
    /// returns; the caller decides whether to retry.
    pub fn stream_traffic(&self, mut on_sample: impl FnMut(TrafficSample) -> bool) {
        let client = match Client::builder()
            .connect_timeout(Duration::from_secs(TRAFFIC_CONNECT_TIMEOUT_SECS))
            .timeout(Duration::from_secs(TRAFFIC_READ_TIMEOUT_SECS))
            .build()
        {
            Ok(client) => client,
            Err(_) => return,
        };
        let response = match client.get(self.url(&["traffic"])).send() {
            Ok(response) if response.status().is_success() => response,
            _ => return,
        };
        let reader = BufReader::new(response);
        for line in reader.lines() {
            let Ok(line) = line else { return };
            if let Some(sample) = parse_traffic_line(&line) {
                if !on_sample(sample) {
                    return;
                }
            }
        }
    }
}

/// Whether a group lets the user pick a node (`Selector`) or auto-selects the
/// fastest one by latency (`UrlTest`). URLTest groups are shown read-only: their
/// `now` reflects sing-box's automatic choice and cannot be switched via the
/// API (a PUT would be rejected), so the UI disables node selection for them.
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

/// Parse the GET /proxies response body into proxy groups.
///
/// Keeps `type == "Selector"` and `type == "URLTest"`, dropping sing-box's
/// synthetic `GLOBAL` group. The response is a JSON map keyed by group name, so
/// cross-group ordering is NOT meaningful here — callers order via `merge_groups`.
pub fn parse_proxies_response(body: &str) -> Result<Vec<ProxyGroup>, String> {
    let json: Value = serde_json::from_str(body)
        .map_err(|e| format!("Invalid Clash API response: {}", e))?;
    let proxies = json
        .get("proxies")
        .and_then(Value::as_object)
        .ok_or_else(|| "Clash API response has no `proxies` object".to_string())?;

    let mut groups = Vec::new();
    for (name, entry) in proxies {
        if name == "GLOBAL" {
            continue;
        }
        let kind = match entry["type"].as_str() {
            Some("Selector") => GroupKind::Selector,
            Some("URLTest") => GroupKind::UrlTest,
            _ => continue,
        };
        let all: Vec<String> = entry["all"]
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
        let now = entry["now"].as_str().unwrap_or_default().to_string();
        groups.push(ProxyGroup {
            name: name.clone(),
            now,
            all,
            kind,
        });
    }
    Ok(groups)
}

/// 从 GET /proxies 响应提取 节点名 → 协议类型(lowercase)。
/// API 返回 CamelCase("VMess"),config 是小写——统一 lowercase 显示。
/// 垃圾输入返回空 map(协议行留空,不报错)。
pub fn parse_node_types_response(body: &str) -> HashMap<String, String> {
    let Ok(json) = serde_json::from_str::<Value>(body) else {
        return HashMap::new();
    };
    let Some(proxies) = json.get("proxies").and_then(Value::as_object) else {
        return HashMap::new();
    };
    proxies
        .iter()
        .filter_map(|(name, entry)| {
            entry["type"]
                .as_str()
                .map(|t| (name.clone(), t.to_lowercase()))
        })
        .collect()
}

/// Parse proxy groups (selector + urltest) straight from `config.json` — used
/// as the ordering skeleton for `merge_groups` and to provide config-derived
/// data for any group the live API happens to lack. Garbage input yields an
/// empty list, never an error.
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
/// 停止态下 `parse_node_types_response` 的对应物。
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

/// Order API groups by their position in config (`/proxies` is a map, so its
/// order is meaningless). Config entries missing from the API keep their
/// config-derived data; API-only extras (should not happen, but covers a
/// failed config parse) are appended sorted by name for determinism.
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

/// 解析测速端点返回的 `{节点名: 延迟ms}` map;测速失败的节点不在响应里。
pub fn parse_delay_response(body: &str) -> Result<HashMap<String, u32>, String> {
    let json: Value =
        serde_json::from_str(body).map_err(|e| format!("Invalid delay response: {}", e))?;
    let map = json
        .as_object()
        .ok_or_else(|| "Delay response is not an object".to_string())?;
    Ok(map
        .iter()
        .filter_map(|(name, v)| v.as_u64().map(|ms| (name.clone(), ms as u32)))
        .collect())
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

fn client() -> Result<Client, String> {
    Client::builder()
        .timeout(Duration::from_secs(CLASH_API_TIMEOUT_SECS))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))
}

/// Connect timeout for the long-lived `/traffic` stream dial — bounds only
/// the initial connection so a not-yet-listening Clash API fails fast (the
/// reader thread then retries).
const TRAFFIC_CONNECT_TIMEOUT_SECS: u64 = 5;
/// Per-read timeout for the `/traffic` stream. sing-box emits one sample every
/// second (even `{"up":0,"down":0}` while idle), so this never trips in normal
/// operation — it only bounds a wedged connection so the reader thread can
/// notice a stop request and exit. Must stay comfortably above the 1s emission
/// cadence. Set explicitly because the blocking client's 30s default request
/// timeout would otherwise govern each read.
const TRAFFIC_READ_TIMEOUT_SECS: u64 = 8;

/// One sample from the Clash API `/traffic` stream: bytes transferred in the
/// last one-second window, i.e. an instantaneous rate in bytes/sec (sing-box
/// emits the delta, not a cumulative counter).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct TrafficSample {
    /// Upload rate, bytes/sec.
    pub up: u64,
    /// Download rate, bytes/sec.
    pub down: u64,
}

/// Parse one line of the `/traffic` stream — `{"up":N,"down":N}`. Returns
/// `None` for blank lines, malformed JSON, or missing/non-integer fields, so
/// the reader can skip noise without aborting the stream.
pub fn parse_traffic_line(line: &str) -> Option<TrafficSample> {
    let value: Value = serde_json::from_str(line.trim()).ok()?;
    let up = value.get("up")?.as_u64()?;
    let down = value.get("down")?.as_u64()?;
    Some(TrafficSample { up, down })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shape of sing-box's GET /proxies response. Note the synthetic GLOBAL
    /// group sing-box adds for dashboard compatibility — it is type Selector
    /// but must NOT surface in the UI.
    const PROXIES_RESPONSE: &str = r#"{
        "proxies": {
            "DIRECT": {"type": "Direct", "history": []},
            "GLOBAL": {"type": "Selector", "now": "auto", "all": ["auto", "节点选择", "DIRECT"]},
            "auto": {"type": "URLTest", "now": "香港-01", "all": ["香港-01", "日本-02"]},
            "节点选择": {"type": "Selector", "now": "香港-01", "all": ["香港-01", "日本-02", "auto"], "history": []}
        }
    }"#;

    #[test]
    fn parse_keeps_selector_and_urltest_groups() {
        let groups = parse_proxies_response(PROXIES_RESPONSE).unwrap();
        assert_eq!(groups.len(), 2, "GLOBAL and DIRECT are dropped");
        // /proxies is a JSON map, so order is not meaningful — index by name.
        let by_name: HashMap<&str, &ProxyGroup> =
            groups.iter().map(|g| (g.name.as_str(), g)).collect();
        let selector = by_name["节点选择"];
        assert_eq!(selector.kind, GroupKind::Selector);
        assert_eq!(selector.now, "香港-01");
        assert_eq!(selector.all, vec!["香港-01", "日本-02", "auto"]);
        let urltest = by_name["auto"];
        assert_eq!(urltest.kind, GroupKind::UrlTest);
        assert_eq!(urltest.now, "香港-01");
        assert_eq!(urltest.all, vec!["香港-01", "日本-02"]);
    }

    #[test]
    fn parse_rejects_invalid_or_shapeless_json() {
        assert!(parse_proxies_response("not json").is_err());
        assert!(parse_proxies_response(r#"{"no_proxies": {}}"#).is_err());
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
        assert_eq!(groups[1].now, "香港-01", "missing `default` falls back to first node");
        assert_eq!(groups[2].name, "auto");
        assert_eq!(groups[2].kind, GroupKind::UrlTest);
        assert_eq!(groups[2].now, "香港-01", "urltest has no `default` → first node placeholder");
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
        assert_eq!(merged[0].name, "A", "config order wins over API map order");
        assert_eq!(merged[0].now, "a2", "API data wins over config default");
        assert_eq!(merged[1].name, "B");
    }

    #[test]
    fn merge_keeps_config_entry_when_api_lacks_group_and_appends_api_extras() {
        let config = vec![group("A", "a1", &["a1"])];
        let api = vec![group("Z", "z1", &["z1"]), group("M", "m1", &["m1"])];
        let merged = merge_groups(&config, api);
        assert_eq!(merged[0].name, "A", "config skeleton survives");
        assert_eq!(merged[1].name, "M", "API-only extras appended sorted by name");
        assert_eq!(merged[2].name, "Z");
    }

    #[test]
    fn select_url_percent_encodes_group_name() {
        let url = ClashApi::new(7789).select_url("节点选择");
        assert_eq!(
            url,
            "http://127.0.0.1:7789/proxies/%E8%8A%82%E7%82%B9%E9%80%89%E6%8B%A9"
        );
    }

    /// `external_controller` and every request URL must agree on host + port —
    /// they all derive from the same `ClashApi`.
    #[test]
    fn external_controller_matches_request_urls() {
        let api = ClashApi::new(17900);
        assert_eq!(api.external_controller(), "127.0.0.1:17900");
        assert!(api.select_url("g").starts_with("http://127.0.0.1:17900/"));
        assert!(api
            .group_delay_url("g", "https://e/x", 1000)
            .starts_with("http://127.0.0.1:17900/"));
    }

    #[test]
    fn node_types_from_api_are_lowercased() {
        let types = parse_node_types_response(PROXIES_RESPONSE);
        assert_eq!(types["auto"], "urltest");
        assert_eq!(types["DIRECT"], "direct");
        assert_eq!(types["节点选择"], "selector");
    }

    #[test]
    fn node_types_from_api_tolerate_garbage() {
        assert!(parse_node_types_response("not json").is_empty());
        assert!(parse_node_types_response(r#"{"no_proxies": {}}"#).is_empty());
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
    fn group_delay_url_encodes_group_and_params() {
        let url = ClashApi::new(7789).group_delay_url("节点选择", "https://example.com/gen", 5000);
        assert_eq!(
            url,
            "http://127.0.0.1:7789/group/%E8%8A%82%E7%82%B9%E9%80%89%E6%8B%A9/delay?url=https%3A%2F%2Fexample.com%2Fgen&timeout=5000"
        );
    }

    #[test]
    fn delay_response_parses_map_and_rejects_garbage() {
        let parsed = parse_delay_response(r#"{"香港-01": 45, "日本-02": 128}"#).unwrap();
        assert_eq!(parsed["香港-01"], 45);
        assert_eq!(parsed.len(), 2);
        assert!(parse_delay_response("{}").unwrap().is_empty());
        assert!(parse_delay_response("not json").is_err());
        assert!(parse_delay_response("[1,2]").is_err());
    }

    #[test]
    fn delay_levels_split_at_200_and_500() {
        assert_eq!(classify_delay(45), DelayLevel::Fast);
        assert_eq!(classify_delay(200), DelayLevel::Fast);
        assert_eq!(classify_delay(201), DelayLevel::Medium);
        assert_eq!(classify_delay(500), DelayLevel::Medium);
        assert_eq!(classify_delay(501), DelayLevel::Slow);
    }

    #[test]
    fn traffic_line_parses_up_and_down() {
        let sample = parse_traffic_line(r#"{"up": 1234, "down": 5678}"#).unwrap();
        assert_eq!(sample, TrafficSample { up: 1234, down: 5678 });
    }

    #[test]
    fn traffic_line_tolerates_zero_and_surrounding_whitespace() {
        let sample = parse_traffic_line("  {\"up\":0,\"down\":0}\n").unwrap();
        assert_eq!(sample, TrafficSample::default());
    }

    #[test]
    fn traffic_line_ignores_extra_fields() {
        // sing-box only sends up/down, but be forgiving of additions.
        let sample = parse_traffic_line(r#"{"up": 10, "down": 20, "extra": 1}"#).unwrap();
        assert_eq!(sample, TrafficSample { up: 10, down: 20 });
    }

    #[test]
    fn traffic_line_rejects_garbage_and_missing_fields() {
        assert!(parse_traffic_line("").is_none());
        assert!(parse_traffic_line("not json").is_none());
        assert!(parse_traffic_line("{}").is_none());
        assert!(parse_traffic_line(r#"{"up": 1}"#).is_none());
        assert!(parse_traffic_line(r#"{"down": 1}"#).is_none());
        assert!(parse_traffic_line(r#"{"up": "x", "down": 1}"#).is_none());
        assert!(parse_traffic_line(r#"{"up": -1, "down": 1}"#).is_none());
    }
}
