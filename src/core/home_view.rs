//! What the Home page shows of a running sing-box, as pure functions over
//! what the sing-box API streams: the exit node each group leads to
//! (`SubscribeGroups`), the open connections by process and network, and
//! who moved the most bytes (`SubscribeConnections`).

use crate::core::connections_view::process_name;
use crate::core::singbox_api::{Connection, ProxyGroup};
use std::collections::{HashMap, HashSet};

/// The node `group` finally sends its traffic to: its `now`, followed
/// through every group that is itself a member's choice (`节点选择` → `自动`
/// → `香港-01`). `None` for an unknown group, or one with nothing chosen.
/// A loop of groups stops at the last group before it closes.
pub fn leaf_node<'a>(groups: &'a [ProxyGroup], group: &str) -> Option<&'a str> {
    let by_name: HashMap<&str, &ProxyGroup> = groups.iter().map(|g| (g.name.as_str(), g)).collect();
    let mut current = *by_name.get(group)?;
    let mut seen = HashSet::from([current.name.as_str()]);
    loop {
        let next = current.now.as_str();
        if next.is_empty() {
            return None;
        }
        match by_name.get(next) {
            Some(inner) if seen.insert(next) => current = inner,
            Some(_) => return Some(current.name.as_str()),
            None => return Some(next),
        }
    }
}

/// The groups Home shows, in the order sing-box lists them (the config's),
/// each with the node it leads to. The first is the exit node Home names:
/// configs put their main selector first.
pub fn group_leaves(groups: &[ProxyGroup], limit: usize) -> Vec<(&str, Option<&str>)> {
    groups
        .iter()
        .take(limit)
        .map(|g| (g.name.as_str(), leaf_node(groups, &g.name)))
        .collect()
}

/// The open connections, counted by what Home's connection card breaks
/// them into.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OpenBreakdown {
    /// Distinct processes behind them; only connections sing-box looked the
    /// process up for count (TUN with `find_process`, or a process rule).
    pub processes: usize,
    pub tcp: usize,
    pub udp: usize,
}

pub fn open_breakdown<'a>(connections: impl IntoIterator<Item = &'a Connection>) -> OpenBreakdown {
    let mut processes = HashSet::new();
    let mut breakdown = OpenBreakdown::default();
    for connection in connections.into_iter().filter(|c| !c.is_closed()) {
        if let Some(name) = process_name(connection) {
            processes.insert(name);
        }
        if connection.network.eq_ignore_ascii_case("tcp") {
            breakdown.tcp += 1;
        } else if connection.network.eq_ignore_ascii_case("udp") {
            breakdown.udp += 1;
        }
    }
    breakdown.processes = processes.len();
    breakdown
}

/// What Home's traffic ranking groups connections by.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RankBy {
    #[default]
    Process,
    Domain,
    /// The final outbound, the node that carried the bytes.
    Outbound,
}

/// One row of the ranking.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Talker {
    pub name: String,
    /// Uploaded + downloaded.
    pub bytes: u64,
}

/// The `limit` names that moved the most bytes over `connections` (open and
/// remembered closed alike), most first; ties by name. Connections without
/// the name asked for (no process looked up) and names that moved nothing
/// are left out.
pub fn top_talkers<'a>(
    connections: impl IntoIterator<Item = &'a Connection>,
    by: RankBy,
    limit: usize,
) -> Vec<Talker> {
    let mut totals: HashMap<&str, u64> = HashMap::new();
    for connection in connections {
        let Some(name) = rank_name(connection, by) else {
            continue;
        };
        let bytes = connection
            .uplink_total
            .saturating_add(connection.downlink_total);
        let total = totals.entry(name).or_default();
        *total = total.saturating_add(bytes);
    }
    let mut talkers: Vec<Talker> = totals
        .into_iter()
        .filter(|&(_, bytes)| bytes > 0)
        .map(|(name, bytes)| Talker {
            name: name.to_string(),
            bytes,
        })
        .collect();
    talkers.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.name.cmp(&b.name)));
    talkers.truncate(limit);
    talkers
}

fn rank_name(connection: &Connection, by: RankBy) -> Option<&str> {
    let name = match by {
        RankBy::Process => {
            let name = process_name(connection)?;
            // `chrome.exe` reads as Chrome's name without its extension.
            match name.len().checked_sub(4) {
                Some(cut) if name[cut..].eq_ignore_ascii_case(".exe") => &name[..cut],
                _ => name,
            }
        }
        RankBy::Domain => {
            if connection.domain.is_empty() {
                destination_host(&connection.destination)
            } else {
                connection.domain.as_str()
            }
        }
        RankBy::Outbound => connection.outbound.as_str(),
    };
    (!name.is_empty()).then_some(name)
}

/// The host of a `host:port` / `[v6]:port` destination, without the port
/// or the brackets.
fn destination_host(destination: &str) -> &str {
    if let Some(rest) = destination.strip_prefix('[') {
        return rest.split_once(']').map_or(rest, |(host, _)| host);
    }
    match destination.rsplit_once(':') {
        // A bare IPv6 address has colons but no port.
        Some((host, _)) if !host.contains(':') => host,
        _ => destination,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::singbox_api::{GroupKind, ProcessInfo};

    fn group(name: &str, now: &str) -> ProxyGroup {
        ProxyGroup {
            name: name.into(),
            now: now.into(),
            all: vec![now.into()],
            kind: GroupKind::Selector,
            group_type: "selector".into(),
            expanded: false,
        }
    }

    fn conn(network: &str, process: Option<&str>, bytes: (u64, u64)) -> Connection {
        Connection {
            id: format!("{network}-{process:?}-{bytes:?}"),
            network: network.into(),
            uplink_total: bytes.0,
            downlink_total: bytes.1,
            process: process.map(|path| ProcessInfo {
                process_id: 1,
                user_id: 0,
                user_name: String::new(),
                process_path: path.into(),
                package_names: Vec::new(),
            }),
            ..Connection::default()
        }
    }

    #[test]
    fn a_group_leads_through_nested_groups_to_a_node() {
        let groups = vec![
            group("节点选择", "自动"),
            group("自动", "香港-01"),
            group("流媒体", "日本-03"),
        ];
        assert_eq!(leaf_node(&groups, "节点选择"), Some("香港-01"));
        assert_eq!(leaf_node(&groups, "流媒体"), Some("日本-03"));
        assert_eq!(leaf_node(&groups, "missing"), None);
        assert_eq!(leaf_node(&[group("empty", "")], "empty"), None);
    }

    #[test]
    fn a_loop_of_groups_stops_before_it_closes() {
        let groups = vec![group("a", "b"), group("b", "a")];
        assert_eq!(leaf_node(&groups, "a"), Some("b"));
        assert_eq!(leaf_node(&[group("self", "self")], "self"), Some("self"));
    }

    #[test]
    fn group_leaves_keep_the_listed_order() {
        let groups = vec![group("a", "x"), group("b", "y"), group("c", "z")];
        assert_eq!(
            group_leaves(&groups, 2),
            vec![("a", Some("x")), ("b", Some("y"))]
        );
    }

    #[test]
    fn open_connections_break_down_by_process_and_network() {
        let mut closed = conn("tcp", Some("C:\\x\\old.exe"), (0, 0));
        closed.closed_at = Some(1);
        let connections = vec![
            conn("tcp", Some("C:\\Apps\\chrome.exe"), (0, 0)),
            conn("tcp", Some("C:\\Apps\\chrome.exe"), (1, 0)),
            conn("udp", Some("/usr/bin/telegram"), (0, 0)),
            conn("udp", None, (0, 0)),
            closed,
        ];
        assert_eq!(
            open_breakdown(&connections),
            OpenBreakdown {
                processes: 2,
                tcp: 2,
                udp: 2,
            }
        );
    }

    #[test]
    fn talkers_rank_by_bytes_with_exe_dropped() {
        let connections = vec![
            conn("tcp", Some("C:\\Apps\\chrome.exe"), (10, 100)),
            conn("tcp", Some("C:\\Apps\\Chrome.EXE"), (0, 5)),
            conn("tcp", Some("C:\\Apps\\chrome.exe"), (5, 5)),
            conn("udp", Some("/usr/bin/telegram"), (50, 200)),
            conn("udp", None, (999, 999)),
            conn("tcp", Some("/usr/bin/idle"), (0, 0)),
        ];
        let talkers = top_talkers(&connections, RankBy::Process, 5);
        assert_eq!(
            talkers,
            vec![
                Talker {
                    name: "telegram".into(),
                    bytes: 250
                },
                Talker {
                    name: "chrome".into(),
                    bytes: 120
                },
                Talker {
                    name: "Chrome".into(),
                    bytes: 5
                },
            ]
        );
        assert_eq!(top_talkers(&connections, RankBy::Process, 1).len(), 1);
    }

    #[test]
    fn domains_fall_back_to_the_destination_host() {
        let mut named = conn("tcp", None, (1, 1));
        named.domain = "github.com".into();
        named.destination = "140.82.112.3:443".into();
        let mut v4 = conn("tcp", None, (3, 0));
        v4.destination = "1.1.1.1:53".into();
        let mut v6 = conn("udp", None, (0, 1));
        v6.destination = "[2606:4700::1111]:443".into();
        let talkers = top_talkers(&[named, v4, v6], RankBy::Domain, 5);
        let names: Vec<&str> = talkers.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["1.1.1.1", "github.com", "2606:4700::1111"]);
        assert_eq!(destination_host("2606:4700::1111"), "2606:4700::1111");
    }

    #[test]
    fn outbounds_rank_by_the_final_outbound() {
        let mut a = conn("tcp", None, (10, 0));
        a.outbound = "香港-01".into();
        let mut b = conn("tcp", None, (0, 20));
        b.outbound = "direct".into();
        let talkers = top_talkers(&[a, b], RankBy::Outbound, 5);
        assert_eq!(talkers[0].name, "direct");
        assert_eq!(talkers[1].name, "香港-01");
    }
}
