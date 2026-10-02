//! Outbound groups and node switching: `SubscribeGroups`, `URLTest`,
//! `SelectOutbound`, `SetGroupExpand`, `SubscribeOutbounds`, plus pure
//! helpers that derive selector groups from `config.json` and URL-test
//! results from the live group stream.

use super::transport::{ApiError, IDLE_STREAM_READ_TIMEOUT};
use super::{pb, SingBoxApi};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

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

impl SingBoxApi {
    /// `SelectOutbound` — switch a selector group's node. sing-box persists
    /// the choice in `cache_file` itself. Errors: `NOT_FOUND` for an unknown
    /// group or a node not in it, `INVALID_ARGUMENT` for a non-selector.
    pub fn select_outbound(&self, group: &str, node: &str) -> Result<(), ApiError> {
        let request = pb::SelectOutboundRequest {
            group_tag: group.to_string(),
            outbound_tag: node.to_string(),
        };
        self.unary("SelectOutbound", &request)
    }

    /// `URLTest` — start a delay test of every node in `group` (a urltest
    /// group re-checks and may re-select; a plain outbound tag tests just
    /// that one). Returns as soon as sing-box has accepted it; results
    /// arrive on the `SubscribeGroups` / `SubscribeOutbounds` streams.
    /// `NOT_FOUND` for an unknown tag.
    pub fn url_test(&self, group: &str) -> Result<(), ApiError> {
        let request = pb::UrlTestRequest {
            outbound_tag: group.to_string(),
        };
        self.unary("URLTest", &request)
    }

    /// `SetGroupExpand` — remember whether the UI shows `group` expanded.
    /// Stored in `cache_file` (a silent no-op without one) and echoed back as
    /// `ProxyGroup::expanded`; it does not by itself trigger a groups push.
    pub fn set_group_expand(&self, group: &str, expanded: bool) -> Result<(), ApiError> {
        let request = pb::SetGroupExpandRequest {
            group_tag: group.to_string(),
            is_expand: expanded,
        };
        self.unary("SetGroupExpand", &request)
    }

    /// Stream `SubscribeGroups`, feeding each snapshot to `on_snapshot`.
    /// sing-box sends a snapshot on subscribe, then again on every URL-test
    /// history change (throttled to one per 250ms) — including urltest
    /// groups' own periodic checks. A node switch alone does *not* push, so
    /// callers update `now` optimistically after `select_outbound`. Idle
    /// otherwise: `TimedOut` after `IDLE_STREAM_READ_TIMEOUT`, re-subscribe.
    pub fn stream_groups(
        &self,
        mut on_snapshot: impl FnMut(GroupsSnapshot) -> bool,
    ) -> Result<(), ApiError> {
        self.stream(
            "SubscribeGroups",
            &(),
            IDLE_STREAM_READ_TIMEOUT,
            |groups: pb::Groups| on_snapshot(GroupsSnapshot::from_proto(groups)),
        )
    }

    /// Stream `SubscribeOutbounds`: every outbound *and* endpoint (groups,
    /// `direct`, WireGuard/Tailscale endpoints included), outbounds first in
    /// sing-box's order, each with its latest URL test. Same push cadence and
    /// idle behaviour as `stream_groups`.
    pub fn stream_outbounds(
        &self,
        mut on_list: impl FnMut(Vec<OutboundItem>) -> bool,
    ) -> Result<(), ApiError> {
        self.stream(
            "SubscribeOutbounds",
            &(),
            IDLE_STREAM_READ_TIMEOUT,
            |list: pb::OutboundList| {
                on_list(
                    list.outbounds
                        .into_iter()
                        .map(OutboundItem::from_proto)
                        .collect(),
                )
            },
        )
    }
}

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
    /// sing-box's group type, lowercase (`selector`, `urltest`).
    pub group_type: String,
    /// The expand state last stored with `set_group_expand`; `false` when
    /// never set or when derived from config.
    pub expanded: bool,
}

/// A node's latest successful URL test, as sing-box's history storage holds
/// it. A failed test deletes the entry rather than recording a failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UrlTestHistory {
    /// Unix seconds.
    pub time: i64,
    pub delay_ms: u32,
}

impl UrlTestHistory {
    fn from_item(item: &pb::GroupItem) -> Option<Self> {
        (item.url_test_time > 0).then(|| Self {
            time: item.url_test_time,
            delay_ms: item.url_test_delay.max(0) as u32,
        })
    }
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
            let group_type = group.r#type.to_lowercase();
            snapshot
                .node_types
                .insert(group.tag.clone(), group_type.clone());
            let mut all = Vec::with_capacity(group.items.len());
            for item in group.items {
                snapshot
                    .node_types
                    .insert(item.tag.clone(), item.r#type.to_lowercase());
                if let Some(history) = UrlTestHistory::from_item(&item) {
                    snapshot.history.insert(item.tag.clone(), history);
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
                group_type,
                expanded: group.is_expand,
            });
        }
        snapshot
    }
}

/// One entry of `SubscribeOutbounds`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutboundItem {
    pub tag: String,
    /// Protocol type, lowercase (`vless`, `selector`, `direct`, `wireguard`…).
    pub outbound_type: String,
    /// Latest successful URL test; `None` = untested or last test failed.
    pub url_test: Option<UrlTestHistory>,
}

impl OutboundItem {
    fn from_proto(item: pb::GroupItem) -> Self {
        Self {
            url_test: UrlTestHistory::from_item(&item),
            outbound_type: item.r#type.to_lowercase(),
            tag: item.tag,
        }
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
            group_type: outbound["type"].as_str().unwrap_or_default().to_string(),
            expanded: false,
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

/// Lay the expand toggles made this session over a snapshot's stored ones.
/// `SetGroupExpand` doesn't push, so a snapshot sent before it landed (or
/// after it failed) would otherwise fold a card the user just opened; once it
/// lands the two agree anyway.
pub fn apply_expand_overrides(groups: &mut [ProxyGroup], overrides: &HashMap<String, bool>) {
    for group in groups {
        if let Some(&expanded) = overrides.get(&group.name) {
            group.expanded = expanded;
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;

    fn proto_groups() -> pb::Groups {
        pb::Groups {
            group: vec![
                pb::Group {
                    tag: "节点选择".into(),
                    r#type: "selector".into(),
                    selectable: true,
                    selected: "日本-02".into(),
                    is_expand: true,
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
        assert_eq!(selector.group_type, "selector");
        assert!(selector.expanded);
        assert_eq!(selector.now, "日本-02");
        assert_eq!(selector.all, vec!["香港-01", "日本-02", "auto"]);
        assert_eq!(snapshot.groups[1].kind, GroupKind::UrlTest);
        assert_eq!(snapshot.groups[1].group_type, "urltest");
        assert!(!snapshot.groups[1].expanded);
        assert_eq!(snapshot.node_types["日本-02"], "shadowsocks", "lowercased");
        assert_eq!(snapshot.node_types["节点选择"], "selector");
        assert_eq!(snapshot.history["香港-01"].delay_ms, 45);
        assert!(
            !snapshot.history.contains_key("日本-02"),
            "url_test_time 0 means no history"
        );
    }

    #[test]
    fn snapshot_skips_groups_without_members() {
        let mut groups = proto_groups();
        groups.group[1].items.clear();
        let snapshot = GroupsSnapshot::from_proto(groups);
        assert_eq!(snapshot.groups.len(), 1);
    }

    #[test]
    fn snapshot_survives_a_protobuf_round_trip() {
        let bytes = proto_groups().encode_to_vec();
        let decoded = pb::Groups::decode(bytes.as_slice()).unwrap();
        assert_eq!(decoded, proto_groups());
    }

    #[test]
    fn outbound_items_carry_type_and_history() {
        let items: Vec<OutboundItem> = proto_groups().group[0]
            .items
            .clone()
            .into_iter()
            .map(OutboundItem::from_proto)
            .collect();
        assert_eq!(
            items[0],
            OutboundItem {
                tag: "香港-01".into(),
                outbound_type: "vless".into(),
                url_test: Some(UrlTestHistory {
                    time: 1_700_000_000,
                    delay_ms: 45
                }),
            }
        );
        assert_eq!(items[1].outbound_type, "shadowsocks");
        assert_eq!(items[1].url_test, None);
    }

    #[test]
    fn negative_delay_clamps_to_zero() {
        let item = pb::GroupItem {
            tag: "x".into(),
            r#type: "direct".into(),
            url_test_time: 1,
            url_test_delay: -3,
        };
        assert_eq!(UrlTestHistory::from_item(&item).unwrap().delay_ms, 0);
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
        assert_eq!(groups[0].group_type, "selector");
        assert!(!groups[0].expanded);
        assert_eq!(groups[0].now, "日本-02", "now must come from `default`");
        assert_eq!(groups[1].name, "无默认");
        assert_eq!(
            groups[1].now, "香港-01",
            "missing `default` falls back to first node"
        );
        assert_eq!(groups[2].name, "auto");
        assert_eq!(groups[2].kind, GroupKind::UrlTest);
        assert_eq!(groups[2].group_type, "urltest");
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
            group_type: "selector".into(),
            expanded: false,
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
    fn expand_overrides_win_over_the_snapshot() {
        let mut groups = vec![group("A", "a1", &["a1"]), group("B", "b1", &["b1"])];
        groups[1].expanded = true;
        let overrides = HashMap::from([("A".to_string(), true), ("B".to_string(), false)]);
        apply_expand_overrides(&mut groups, &overrides);
        assert!(groups[0].expanded);
        assert!(!groups[1].expanded);

        let mut untouched = vec![group("C", "c1", &["c1"])];
        untouched[0].expanded = true;
        apply_expand_overrides(&mut untouched, &overrides);
        assert!(untouched[0].expanded, "no override keeps sing-box's value");
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
