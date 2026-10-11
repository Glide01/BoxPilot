//! What the Groups page shows, as pure functions over the live groups: the
//! search filter, the delay sort, the flat row list its virtual list lays
//! out, and which groups "Test all" has to ask sing-box to test.
//!
//! The page caches the result of `flatten_rows` and only recomputes it when
//! the groups' revision, the query, the sort or the column count change, so
//! nothing here runs per frame.

use crate::core::singbox_api::{DelayState, GroupKind, ProxyGroup};
use std::collections::{HashMap, HashSet};

/// Node order inside each group.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NodeSort {
    /// The order the config lists them in.
    #[default]
    Default,
    /// Fastest first, then timeouts, then untested (stable within each).
    Delay,
}

/// The query as matching uses it: trimmed and lowercased. Empty = no filter.
pub fn normalize_query(query: &str) -> String {
    query.trim().to_lowercase()
}

/// Whether a node passes the search: `query` (already normalized) is a
/// case-insensitive substring of its name or its protocol type.
pub fn node_matches(node: &str, node_type: &str, query: &str) -> bool {
    query.is_empty()
        || node.to_lowercase().contains(query)
        || node_type.to_lowercase().contains(query)
}

/// Indices into `nodes` of the ones that pass the search, in order.
pub fn filter_nodes(
    nodes: &[String],
    node_types: &HashMap<String, String>,
    query: &str,
) -> Vec<usize> {
    nodes
        .iter()
        .enumerate()
        .filter(|(_, node)| {
            let node_type = node_types.get(*node).map_or("", String::as_str);
            node_matches(node, node_type, query)
        })
        .map(|(ix, _)| ix)
        .collect()
}

/// Sort key for `NodeSort::Delay`: results by delay, then timeouts, then
/// nodes without a result.
fn delay_rank(delay: Option<&DelayState>) -> (u8, u32) {
    match delay {
        Some(DelayState::Ok(ms)) => (0, *ms),
        Some(DelayState::Timeout) => (1, 0),
        None => (2, 0),
    }
}

/// Reorder `indices` (into `nodes`) fastest first; ties keep their order.
pub fn sort_by_delay(
    indices: &mut [usize],
    nodes: &[String],
    delays: &HashMap<String, DelayState>,
) {
    indices.sort_by_key(|&ix| delay_rank(delays.get(&nodes[ix])));
}

/// One line of the Groups page's virtual list. Each kind has a fixed height
/// on the page; the group cards are drawn row by row (borders per row).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    /// A group card's title line. `expanded` is what to draw (forced open
    /// while searching); `shown` is how many of its nodes are listed.
    Header {
        group: usize,
        expanded: bool,
        shown: usize,
    },
    /// One line of node cards: `GroupsLayout::nodes[start..end]`, indices
    /// into the group's `all`. `last` closes the card.
    Nodes {
        group: usize,
        start: usize,
        end: usize,
        last: bool,
    },
    /// Space between two group cards.
    Gap,
}

/// Everything the page's list renders, flattened.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GroupsLayout {
    pub rows: Vec<Row>,
    /// Node indices (into their group's `all`) in display order; `Row::Nodes`
    /// ranges point in here.
    pub nodes: Vec<usize>,
}

/// Lay the groups out as rows, `columns` node cards per line.
///
/// With a query (normalized, see `normalize_query`), each group lists only
/// its matching nodes and is drawn expanded whatever its stored state; a
/// group with no match is left out. Without one, every group is listed and
/// collapsed groups are just their title line.
pub fn flatten_rows(
    groups: &[ProxyGroup],
    node_types: &HashMap<String, String>,
    delays: &HashMap<String, DelayState>,
    query: &str,
    sort: NodeSort,
    columns: usize,
) -> GroupsLayout {
    let columns = columns.max(1);
    let searching = !query.is_empty();
    let mut layout = GroupsLayout::default();
    for (gi, group) in groups.iter().enumerate() {
        let mut members = if searching {
            filter_nodes(&group.all, node_types, query)
        } else {
            (0..group.all.len()).collect()
        };
        if searching && members.is_empty() {
            continue;
        }
        let expanded = searching || group.expanded;
        if !layout.rows.is_empty() {
            layout.rows.push(Row::Gap);
        }
        layout.rows.push(Row::Header {
            group: gi,
            expanded,
            shown: members.len(),
        });
        if !expanded {
            continue;
        }
        if sort == NodeSort::Delay {
            sort_by_delay(&mut members, &group.all, delays);
        }
        let base = layout.nodes.len();
        let count = members.len();
        layout.nodes.extend(members);
        let mut start = 0;
        while start < count {
            let end = (start + columns).min(count);
            layout.rows.push(Row::Nodes {
                group: gi,
                start: base + start,
                end: base + end,
                last: end == count,
            });
            start = end;
        }
    }
    layout
}

/// The groups "Test all" sends to sing-box so every node is tested about
/// once: `URLTest` on a group probes all of its members, and the same node
/// usually sits in several groups. urltest groups come first — testing them
/// also lets them re-select at once — then selectors, largest first, each
/// only if it still has an uncovered member. Groups for which `busy` is true
/// (already being tested) are skipped, their members counted as covered.
/// Returns indices into `groups`.
pub fn test_cover(groups: &[ProxyGroup], busy: impl Fn(&str) -> bool) -> Vec<usize> {
    let mut covered: HashSet<&str> = HashSet::new();
    for group in groups.iter().filter(|g| busy(&g.name)) {
        covered.extend(group.all.iter().map(String::as_str));
    }
    let mut order: Vec<usize> = (0..groups.len())
        .filter(|&gi| !busy(&groups[gi].name))
        .collect();
    // Stable: config order within each kind / size.
    order.sort_by_key(|&gi| {
        let group = &groups[gi];
        (
            group.kind != GroupKind::UrlTest,
            std::cmp::Reverse(group.all.len()),
        )
    });
    let mut cover = Vec::new();
    for gi in order {
        let group = &groups[gi];
        if group
            .all
            .iter()
            .any(|node| !covered.contains(node.as_str()))
        {
            covered.extend(group.all.iter().map(String::as_str));
            cover.push(gi);
        }
    }
    cover.sort_unstable();
    cover
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(name: &str, kind: GroupKind, expanded: bool, all: &[&str]) -> ProxyGroup {
        ProxyGroup {
            name: name.into(),
            now: all[0].into(),
            all: all.iter().map(|s| s.to_string()).collect(),
            kind,
            group_type: match kind {
                GroupKind::Selector => "selector".into(),
                GroupKind::UrlTest => "urltest".into(),
            },
            expanded,
        }
    }

    fn types() -> HashMap<String, String> {
        [
            ("HK-01", "vless"),
            ("hk-02", "shadowsocks"),
            ("JP-01", "trojan"),
            ("US-01", "vless"),
            ("auto", "urltest"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
    }

    fn names(all: &[&str]) -> Vec<String> {
        all.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn query_matches_name_or_type_case_insensitively() {
        let nodes = names(&["HK-01", "hk-02", "JP-01", "US-01"]);
        assert_eq!(filter_nodes(&nodes, &types(), "hk"), vec![0, 1]);
        assert_eq!(
            filter_nodes(&nodes, &types(), &normalize_query("  VLESS ")),
            vec![0, 3],
            "protocol type, trimmed and lowercased"
        );
        assert_eq!(filter_nodes(&nodes, &types(), ""), vec![0, 1, 2, 3]);
        assert!(filter_nodes(&nodes, &types(), "kr").is_empty());
        assert!(
            node_matches("Tokyo", "", "tok"),
            "a node without a known type still matches by name"
        );
        assert!(node_matches("日本-东京", "vless", "东京"));
    }

    #[test]
    fn delay_sort_puts_results_then_timeouts_then_untested_and_is_stable() {
        let nodes = names(&["a", "b", "c", "d", "e", "f"]);
        let delays = HashMap::from([
            ("a".to_string(), DelayState::Timeout),
            ("b".to_string(), DelayState::Ok(300)),
            ("d".to_string(), DelayState::Ok(40)),
            ("e".to_string(), DelayState::Timeout),
            ("f".to_string(), DelayState::Ok(300)),
        ]);
        let mut order: Vec<usize> = (0..nodes.len()).collect();
        sort_by_delay(&mut order, &nodes, &delays);
        let sorted: Vec<&str> = order.iter().map(|&i| nodes[i].as_str()).collect();
        assert_eq!(sorted, vec!["d", "b", "f", "a", "e", "c"]);
    }

    #[test]
    fn flatten_lays_expanded_groups_out_in_column_chunks() {
        let groups = vec![
            group(
                "Proxy",
                GroupKind::Selector,
                true,
                &["HK-01", "hk-02", "JP-01"],
            ),
            group("auto", GroupKind::UrlTest, false, &["HK-01", "US-01"]),
            group("Media", GroupKind::Selector, true, &["US-01", "JP-01"]),
        ];
        let layout = flatten_rows(&groups, &types(), &HashMap::new(), "", NodeSort::Default, 2);
        assert_eq!(
            layout.rows,
            vec![
                Row::Header {
                    group: 0,
                    expanded: true,
                    shown: 3
                },
                Row::Nodes {
                    group: 0,
                    start: 0,
                    end: 2,
                    last: false
                },
                Row::Nodes {
                    group: 0,
                    start: 2,
                    end: 3,
                    last: true
                },
                Row::Gap,
                Row::Header {
                    group: 1,
                    expanded: false,
                    shown: 2
                },
                Row::Gap,
                Row::Header {
                    group: 2,
                    expanded: true,
                    shown: 2
                },
                Row::Nodes {
                    group: 2,
                    start: 3,
                    end: 5,
                    last: true
                },
            ]
        );
        assert_eq!(layout.nodes, vec![0, 1, 2, 0, 1]);

        let wide = flatten_rows(&groups, &types(), &HashMap::new(), "", NodeSort::Default, 3);
        assert_eq!(wide.rows.len(), 7, "three columns fit Proxy on one line");
        let narrow = flatten_rows(&groups, &types(), &HashMap::new(), "", NodeSort::Default, 0);
        assert_eq!(
            narrow
                .rows
                .iter()
                .filter(|r| matches!(r, Row::Nodes { .. }))
                .count(),
            5,
            "zero columns is treated as one"
        );
    }

    #[test]
    fn search_hides_groups_without_matches_and_opens_the_rest() {
        let groups = vec![
            group(
                "Proxy",
                GroupKind::Selector,
                false,
                &["HK-01", "hk-02", "JP-01"],
            ),
            group("Media", GroupKind::Selector, false, &["US-01", "JP-01"]),
            group("auto", GroupKind::UrlTest, false, &["US-01"]),
        ];
        let layout = flatten_rows(
            &groups,
            &types(),
            &HashMap::new(),
            "hk",
            NodeSort::Default,
            2,
        );
        assert_eq!(
            layout.rows,
            vec![
                Row::Header {
                    group: 0,
                    expanded: true,
                    shown: 2
                },
                Row::Nodes {
                    group: 0,
                    start: 0,
                    end: 2,
                    last: true
                },
            ],
            "collapsed Proxy is shown open; Media and auto have no match"
        );
        assert_eq!(layout.nodes, vec![0, 1]);
        assert!(
            !groups[0].expanded,
            "search never touches the stored expand state"
        );

        let none = flatten_rows(
            &groups,
            &types(),
            &HashMap::new(),
            "kr",
            NodeSort::Default,
            2,
        );
        assert!(none.rows.is_empty());
    }

    #[test]
    fn delay_sort_applies_within_each_group() {
        let groups = vec![group(
            "Proxy",
            GroupKind::Selector,
            true,
            &["HK-01", "hk-02", "JP-01"],
        )];
        let delays = HashMap::from([
            ("JP-01".to_string(), DelayState::Ok(80)),
            ("HK-01".to_string(), DelayState::Ok(120)),
        ]);
        let layout = flatten_rows(&groups, &types(), &delays, "", NodeSort::Delay, 4);
        assert_eq!(layout.nodes, vec![2, 0, 1]);
        let default = flatten_rows(&groups, &types(), &delays, "", NodeSort::Default, 4);
        assert_eq!(default.nodes, vec![0, 1, 2]);
    }

    #[test]
    fn test_cover_tests_each_node_through_as_few_groups_as_it_can() {
        let groups = vec![
            group(
                "Proxy",
                GroupKind::Selector,
                false,
                &["HK-01", "auto", "JP-01", "US-01"],
            ),
            group("HK", GroupKind::Selector, false, &["HK-01"]),
            group("auto", GroupKind::UrlTest, false, &["HK-01", "US-01"]),
            group("Extra", GroupKind::Selector, false, &["KR-01"]),
        ];
        assert_eq!(
            test_cover(&groups, |_| false),
            vec![0, 2, 3],
            "urltest first, then the biggest selector; HK is already covered"
        );
        assert_eq!(
            test_cover(&groups, |name| name == "Proxy"),
            vec![3],
            "a group under test covers its members, auto's included"
        );
        assert!(test_cover(&[], |_| false).is_empty());
    }
}
