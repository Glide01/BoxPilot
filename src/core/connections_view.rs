//! Pure presentation logic for the Connections page: which connections a
//! view shows (active/closed tab + text filter), in what order, the summary
//! header, and each row's display strings. The page only places the results
//! in layout. No gpui dependency — keep it that way so the core test shim
//! keeps working.

use crate::core::singbox_api::Connection;

/// Which half of the connection table the page lists.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ConnectionView {
    /// Open connections (the default).
    #[default]
    Active,
    /// Connections sing-box still remembers after they closed.
    Closed,
}

impl ConnectionView {
    pub fn includes(self, connection: &Connection) -> bool {
        match self {
            ConnectionView::Active => !connection.is_closed(),
            ConnectionView::Closed => connection.is_closed(),
        }
    }
}

/// What the rows are ordered by. Each key has a natural direction
/// (`natural_direction`): biggest / newest first for the figures, A→Z for
/// the text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ConnectionSort {
    /// When it opened (active) or closed (closed); most recent first.
    #[default]
    Newest,
    /// Bytes moved, up + down totals; heaviest first.
    Traffic,
    /// The current rate, up + down (`current_rate`); fastest first.
    Speed,
    /// The host as the row shows it (`host_label`), A→Z.
    Host,
    /// The matched rule (`rule_label`), A→Z.
    Rule,
    /// The outbound chain in reading order (`chain_label`), A→Z.
    Chain,
}

impl ConnectionSort {
    /// Every key, in the order the sort menu lists them.
    pub const ALL: [ConnectionSort; 6] = [
        ConnectionSort::Newest,
        ConnectionSort::Traffic,
        ConnectionSort::Speed,
        ConnectionSort::Host,
        ConnectionSort::Rule,
        ConnectionSort::Chain,
    ];

    /// The direction a freshly chosen key sorts in.
    pub fn natural_direction(self) -> SortDirection {
        match self {
            ConnectionSort::Newest | ConnectionSort::Traffic | ConnectionSort::Speed => {
                SortDirection::Descending
            }
            ConnectionSort::Host | ConnectionSort::Rule | ConnectionSort::Chain => {
                SortDirection::Ascending
            }
        }
    }
}

/// Which way the sort key runs. Only the key flips: ties stay newest
/// first, then by id, either way.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SortDirection {
    /// Smallest / oldest / A first.
    Ascending,
    /// Biggest / newest / Z first.
    #[default]
    Descending,
}

impl SortDirection {
    pub fn reversed(self) -> Self {
        match self {
            SortDirection::Ascending => SortDirection::Descending,
            SortDirection::Descending => SortDirection::Ascending,
        }
    }
}

/// A row's value under one sort key. Text compares case-insensitively.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum SortValue {
    Time(i64),
    Amount(u64),
    Text(String),
}

fn sort_value(connection: &Connection, sort: ConnectionSort) -> SortValue {
    match sort {
        ConnectionSort::Newest => SortValue::Time(newest(connection)),
        ConnectionSort::Traffic => SortValue::Amount(total_bytes(connection)),
        ConnectionSort::Speed => SortValue::Amount(current_rate(connection)),
        ConnectionSort::Host => SortValue::Text(host_label(connection).to_lowercase()),
        ConnectionSort::Rule => SortValue::Text(rule_label(connection).to_lowercase()),
        ConnectionSort::Chain => SortValue::Text(chain_label(connection).to_lowercase()),
    }
}

/// When the connection last changed state: its close, else its open.
fn newest(connection: &Connection) -> i64 {
    connection.closed_at.unwrap_or(connection.created_at)
}

/// The connections `view` shows, filtered by `query` (see `matches_query`)
/// and ordered by `sort` in `direction`. Ties fall back to newest, then
/// id, whatever the direction, so rows don't shuffle between
/// identical-looking refreshes.
pub fn select_connections<'a>(
    connections: impl IntoIterator<Item = &'a Connection>,
    view: ConnectionView,
    query: &str,
    sort: ConnectionSort,
    direction: SortDirection,
) -> Vec<&'a Connection> {
    let terms = query_terms(query);
    // Each row's key once, not per comparison (text keys allocate).
    let mut rows: Vec<(SortValue, &Connection)> = connections
        .into_iter()
        .filter(|c| view.includes(c) && matches_terms(c, &terms))
        .map(|c| (sort_value(c, sort), c))
        .collect();
    rows.sort_by(|(key_a, a), (key_b, b)| {
        let by_key = match direction {
            SortDirection::Ascending => key_a.cmp(key_b),
            SortDirection::Descending => key_b.cmp(key_a),
        };
        by_key
            .then_with(|| newest(b).cmp(&newest(a)))
            .then_with(|| a.id.cmp(&b.id))
    });
    rows.into_iter().map(|(_, c)| c).collect()
}

/// The connection's current rate, up + down, bytes/sec: the latest
/// per-second delta sing-box reported. 0 once closed.
pub fn current_rate(connection: &Connection) -> u64 {
    if connection.is_closed() {
        return 0;
    }
    connection.uplink.saturating_add(connection.downlink)
}

fn total_bytes(connection: &Connection) -> u64 {
    connection
        .uplink_total
        .saturating_add(connection.downlink_total)
}

/// Whitespace-separated, lowercased filter terms; empty = match everything.
fn query_terms(query: &str) -> Vec<String> {
    query.split_whitespace().map(str::to_lowercase).collect()
}

/// Whether `connection` matches the filter box text: every whitespace-
/// separated term must appear (case-insensitively) in one of the fields the
/// row shows — host, destination, network/protocol, inbound, rule, chain,
/// process — or the source address.
pub fn matches_query(connection: &Connection, query: &str) -> bool {
    matches_terms(connection, &query_terms(query))
}

fn matches_terms(connection: &Connection, terms: &[String]) -> bool {
    if terms.is_empty() {
        return true;
    }
    let mut haystack = [
        connection.domain.as_str(),
        connection.destination.as_str(),
        connection.source.as_str(),
        connection.network.as_str(),
        connection.protocol.as_str(),
        connection.inbound.as_str(),
        connection.inbound_type.as_str(),
        rule_label(connection),
        connection.outbound.as_str(),
    ]
    .join("\n");
    for hop in &connection.chain {
        haystack.push('\n');
        haystack.push_str(hop);
    }
    if let Some(process) = &connection.process {
        haystack.push('\n');
        haystack.push_str(&process.process_path);
    }
    let haystack = haystack.to_lowercase();
    terms.iter().all(|term| haystack.contains(term.as_str()))
}

/// What the connection reached: the domain (sniffed or requested) with the
/// destination port, else the destination as sing-box reports it (which may
/// itself be `domain:port` when the client asked for a name).
pub fn host_label(connection: &Connection) -> String {
    if connection.domain.is_empty() {
        return connection.destination.clone();
    }
    match destination_port(&connection.destination) {
        Some(port) => format!("{}:{}", connection.domain, port),
        None => connection.domain.clone(),
    }
}

/// The port of a `host:port` / `[v6]:port` destination.
fn destination_port(destination: &str) -> Option<&str> {
    let (host, port) = destination.rsplit_once(':')?;
    // A bare IPv6 address without brackets has colons but no port.
    if host.contains(':') && !host.ends_with(']') {
        return None;
    }
    (!port.is_empty() && port.bytes().all(|b| b.is_ascii_digit())).then_some(port)
}

/// `tcp` or `tcp/tls` (network, then the sniffed protocol if any).
pub fn network_label(connection: &Connection) -> String {
    if connection.protocol.is_empty() {
        connection.network.clone()
    } else {
        format!("{}/{}", connection.network, connection.protocol)
    }
}

/// The inbound tag, falling back to its type for an untagged inbound.
pub fn inbound_label(connection: &Connection) -> &str {
    if connection.inbound.is_empty() {
        &connection.inbound_type
    } else {
        &connection.inbound
    }
}

/// The matched route rule; sing-box leaves it empty when `route.final` took
/// the connection.
pub fn rule_label(connection: &Connection) -> &str {
    if connection.rule.is_empty() {
        "final"
    } else {
        &connection.rule
    }
}

/// The outbound path in reading order — the group the rule picked first,
/// the node that carried the traffic last: `节点选择 → auto → 香港-01`.
/// sing-box reports `chain` the other way round (final outbound first).
pub fn chain_label(connection: &Connection) -> String {
    if connection.chain.is_empty() {
        return connection.outbound.clone();
    }
    let hops: Vec<&str> = connection.chain.iter().rev().map(String::as_str).collect();
    hops.join(" → ")
}

/// The executable name behind the connection (`chrome.exe`), when sing-box
/// looked the process up. Handles both Windows and POSIX separators.
pub fn process_name(connection: &Connection) -> Option<&str> {
    let path = connection.process.as_ref()?.process_path.as_str();
    let name = path.rsplit(['\\', '/']).next().unwrap_or(path);
    (!name.is_empty()).then_some(name)
}

/// How long the connection has lived, in ms: up to `now_ms` while open, up
/// to its close once closed. Never negative (clock skew).
pub fn connection_age_ms(connection: &Connection, now_ms: i64) -> i64 {
    let end = connection.closed_at.unwrap_or(now_ms);
    (end - connection.created_at).max(0)
}

/// A compact duration: `0s`, `45s`, `3m 12s`, `1h 05m`, `2d 3h`.
pub fn format_elapsed(ms: i64) -> String {
    let secs = ms.max(0) / 1000;
    let t = &crate::i18n::s().time;
    if secs < 60 {
        format!("{}{}", secs, t.second)
    } else if secs < 3600 {
        format!(
            "{}{}{}{:02}{}",
            secs / 60,
            t.minute,
            t.unit_sep,
            secs % 60,
            t.second
        )
    } else if secs < 86400 {
        let (hours, mins) = (secs / 3600, (secs % 3600) / 60);
        format!("{}{}{}{:02}{}", hours, t.hour, t.unit_sep, mins, t.minute)
    } else {
        let (days, hours) = (secs / 86400, (secs % 86400) / 3600);
        format!("{}{}{}{}{}", days, t.day, t.unit_sep, hours, t.hour)
    }
}

/// The page's summary header.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ConnectionSummary {
    pub open: usize,
    pub closed: usize,
    /// Current rate summed over open connections, bytes/sec.
    pub up_rate: u64,
    pub down_rate: u64,
    /// Bytes moved by every listed connection, open and remembered closed.
    pub up_total: u64,
    pub down_total: u64,
}

pub fn summarize<'a>(connections: impl IntoIterator<Item = &'a Connection>) -> ConnectionSummary {
    let mut summary = ConnectionSummary::default();
    for connection in connections {
        if connection.is_closed() {
            summary.closed += 1;
        } else {
            summary.open += 1;
            summary.up_rate = summary.up_rate.saturating_add(connection.uplink);
            summary.down_rate = summary.down_rate.saturating_add(connection.downlink);
        }
        summary.up_total = summary.up_total.saturating_add(connection.uplink_total);
        summary.down_total = summary.down_total.saturating_add(connection.downlink_total);
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::singbox_api::ProcessInfo;

    fn conn(id: &str, created_at: i64) -> Connection {
        Connection {
            id: id.into(),
            inbound: "mixed-in".into(),
            inbound_type: "mixed".into(),
            network: "tcp".into(),
            source: "127.0.0.1:50000".into(),
            destination: "1.1.1.1:443".into(),
            outbound: "香港-01".into(),
            chain: vec!["香港-01".into(), "auto".into(), "节点选择".into()],
            created_at,
            ..Default::default()
        }
    }

    fn closed(id: &str, created_at: i64, closed_at: i64) -> Connection {
        Connection {
            closed_at: Some(closed_at),
            ..conn(id, created_at)
        }
    }

    /// `all`'s rows in `view`, unfiltered, by `sort` in its natural
    /// direction.
    fn sorted(all: &[Connection], view: ConnectionView, sort: ConnectionSort) -> Vec<&Connection> {
        select_connections(all, view, "", sort, sort.natural_direction())
    }

    fn ids(rows: &[&Connection]) -> Vec<String> {
        rows.iter().map(|c| c.id.clone()).collect()
    }

    #[test]
    fn views_split_open_and_closed() {
        let all = [conn("a", 1), closed("b", 2, 3)];
        let active = sorted(&all, ConnectionView::Active, ConnectionSort::Newest);
        assert_eq!(ids(&active), ["a"]);
        let gone = sorted(&all, ConnectionView::Closed, ConnectionSort::Newest);
        assert_eq!(ids(&gone), ["b"]);
    }

    #[test]
    fn newest_first_uses_close_time_for_closed() {
        let open = [conn("old", 100), conn("new", 300), conn("mid", 200)];
        let rows = sorted(&open, ConnectionView::Active, ConnectionSort::Newest);
        assert_eq!(ids(&rows), ["new", "mid", "old"]);

        // Opened first but closed last → on top of the closed tab.
        let gone = [closed("x", 100, 900), closed("y", 500, 600)];
        let rows = sorted(&gone, ConnectionView::Closed, ConnectionSort::Newest);
        assert_eq!(ids(&rows), ["x", "y"]);
    }

    #[test]
    fn ties_are_ordered_by_id_for_stable_rows() {
        let all = [conn("b", 100), conn("a", 100), conn("c", 100)];
        let rows = sorted(&all, ConnectionView::Active, ConnectionSort::Newest);
        assert_eq!(ids(&rows), ["a", "b", "c"]);
    }

    #[test]
    fn traffic_sort_puts_heaviest_first_then_newest() {
        let mut light = conn("light", 300);
        light.uplink_total = 10;
        let mut heavy = conn("heavy", 100);
        heavy.downlink_total = 5_000;
        let mut tie_old = conn("tie-old", 100);
        tie_old.uplink_total = 50;
        let mut tie_new = conn("tie-new", 200);
        tie_new.downlink_total = 50;
        let all = [light, heavy, tie_old, tie_new];
        let rows = sorted(&all, ConnectionView::Active, ConnectionSort::Traffic);
        assert_eq!(ids(&rows), ["heavy", "tie-new", "tie-old", "light"]);
    }

    #[test]
    fn speed_sort_puts_fastest_first_then_newest() {
        let mut slow = conn("slow", 300);
        slow.uplink = 10;
        let mut fast = conn("fast", 100);
        fast.downlink = 5_000;
        let mut tie_old = conn("tie-old", 100);
        tie_old.uplink = 50;
        let mut tie_new = conn("tie-new", 200);
        tie_new.downlink = 50;
        let idle = conn("idle", 400);
        let all = [slow, fast, tie_old, tie_new, idle];
        let rows = sorted(&all, ConnectionView::Active, ConnectionSort::Speed);
        assert_eq!(ids(&rows), ["fast", "tie-new", "tie-old", "slow", "idle"]);
    }

    #[test]
    fn text_sorts_run_a_to_z_case_insensitively() {
        let mut b = conn("1", 100);
        b.domain = "Beta.example".into();
        b.rule = "rule-set=b".into();
        b.chain = vec!["Zeta".into()];
        let mut a = conn("2", 200);
        a.domain = "alpha.example".into();
        a.chain = vec!["香港-01".into(), "Alpha".into()];
        let mut c = conn("3", 300);
        c.destination = "10.0.0.1:53".into();
        c.rule = "network=udp".into();
        c.chain = vec!["beta".into()];
        let all = [b, a, c];
        let host = sorted(&all, ConnectionView::Active, ConnectionSort::Host);
        assert_eq!(ids(&host), ["3", "2", "1"], "10.0.0.1 < alpha < Beta");
        // `final` (empty rule) sorts by its label.
        let rule = sorted(&all, ConnectionView::Active, ConnectionSort::Rule);
        assert_eq!(ids(&rule), ["2", "3", "1"]);
        let chain = sorted(&all, ConnectionView::Active, ConnectionSort::Chain);
        assert_eq!(
            ids(&chain),
            ["2", "3", "1"],
            "Alpha → 香港-01 < beta < Zeta"
        );
    }

    #[test]
    fn natural_directions() {
        use ConnectionSort::*;
        for sort in [Newest, Traffic, Speed] {
            assert_eq!(sort.natural_direction(), SortDirection::Descending);
        }
        for sort in [Host, Rule, Chain] {
            assert_eq!(sort.natural_direction(), SortDirection::Ascending);
        }
        assert_eq!(ConnectionSort::ALL.len(), 6);
        assert_eq!(
            SortDirection::Ascending.reversed(),
            SortDirection::Descending
        );
        assert_eq!(
            SortDirection::Descending.reversed(),
            SortDirection::Ascending
        );
    }

    #[test]
    fn reversing_flips_the_key_but_not_the_tie_breaks() {
        let mut light = conn("light", 300);
        light.uplink_total = 10;
        let mut heavy = conn("heavy", 100);
        heavy.downlink_total = 5_000;
        let mut tie_old = conn("tie-old", 100);
        tie_old.uplink_total = 50;
        let mut tie_new = conn("tie-new", 200);
        tie_new.downlink_total = 50;
        let mut tie_new_b = conn("tie-new-b", 200);
        tie_new_b.downlink_total = 50;
        let all = [light, heavy, tie_old, tie_new_b, tie_new];
        let rows = select_connections(
            &all,
            ConnectionView::Active,
            "",
            ConnectionSort::Traffic,
            SortDirection::Ascending,
        );
        // Lightest first; the 50-byte ties still newest first, then by id.
        assert_eq!(
            ids(&rows),
            ["light", "tie-new", "tie-new-b", "tie-old", "heavy"]
        );

        // Oldest first; equal times by id.
        let all = [conn("b", 100), conn("new", 300), conn("a", 100)];
        let rows = select_connections(
            &all,
            ConnectionView::Active,
            "",
            ConnectionSort::Newest,
            SortDirection::Ascending,
        );
        assert_eq!(ids(&rows), ["a", "b", "new"]);

        // Z→A; the same host keeps newest first.
        let mut x_old = conn("x-old", 100);
        x_old.domain = "x.example".into();
        let mut x_new = conn("x-new", 200);
        x_new.domain = "x.example".into();
        let mut y = conn("y", 50);
        y.domain = "y.example".into();
        let all = [x_old, y, x_new];
        let rows = select_connections(
            &all,
            ConnectionView::Active,
            "",
            ConnectionSort::Host,
            SortDirection::Descending,
        );
        assert_eq!(ids(&rows), ["y", "x-new", "x-old"]);
    }

    #[test]
    fn closed_connections_have_no_rate() {
        let mut open = conn("a", 1);
        open.uplink = 3;
        open.downlink = 4;
        assert_eq!(current_rate(&open), 7);
        let mut gone = closed("b", 1, 2);
        gone.uplink = 3;
        assert_eq!(current_rate(&gone), 0);
    }

    #[test]
    fn filter_matches_every_shown_field_case_insensitively() {
        let mut c = conn("a", 1);
        c.domain = "www.Example.com".into();
        c.protocol = "tls".into();
        c.rule = "domain_suffix=example.com".into();
        c.process = Some(ProcessInfo {
            process_path: r"C:\Program Files\Google\Chrome.exe".into(),
            ..Default::default()
        });
        for query in [
            "",
            "   ",
            "example",
            "EXAMPLE.COM",
            "1.1.1.1",
            "tls",
            "tcp",
            "mixed-in",
            "domain_suffix",
            "节点选择",
            "auto",
            "chrome",
            "127.0.0.1",
            "chrome example",
        ] {
            assert!(matches_query(&c, query), "query {query:?} should match");
        }
        assert!(!matches_query(&c, "udp"));
        assert!(!matches_query(&c, "chrome firefox"), "all terms must match");
    }

    #[test]
    fn final_rule_is_searchable_by_its_label() {
        let c = conn("a", 1);
        assert!(matches_query(&c, "final"));
        let rows = select_connections(
            [&c],
            ConnectionView::Active,
            "FINAL",
            ConnectionSort::Newest,
            SortDirection::Descending,
        );
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn host_prefers_domain_with_destination_port() {
        let mut c = conn("a", 1);
        assert_eq!(host_label(&c), "1.1.1.1:443");
        c.domain = "example.com".into();
        assert_eq!(host_label(&c), "example.com:443");
        c.destination = "[2606:4700::1111]:8443".into();
        assert_eq!(host_label(&c), "example.com:8443");
        c.destination = "2606:4700::1111".into();
        assert_eq!(host_label(&c), "example.com", "no port to borrow");
        // A request by name with no sniffing: the name is the destination.
        let mut by_name = conn("b", 1);
        by_name.destination = "localtest.example:18812".into();
        assert_eq!(host_label(&by_name), "localtest.example:18812");
    }

    #[test]
    fn network_label_appends_sniffed_protocol() {
        let mut c = conn("a", 1);
        assert_eq!(network_label(&c), "tcp");
        c.network = "udp".into();
        c.protocol = "quic".into();
        assert_eq!(network_label(&c), "udp/quic");
    }

    #[test]
    fn inbound_and_rule_labels_fall_back() {
        let mut c = conn("a", 1);
        assert_eq!(inbound_label(&c), "mixed-in");
        c.inbound.clear();
        assert_eq!(inbound_label(&c), "mixed");
        assert_eq!(rule_label(&c), "final");
        c.rule = "domain=example.com".into();
        assert_eq!(rule_label(&c), "domain=example.com");
    }

    #[test]
    fn chain_reads_group_to_node() {
        let mut c = conn("a", 1);
        assert_eq!(chain_label(&c), "节点选择 → auto → 香港-01");
        c.chain = vec!["direct".into()];
        assert_eq!(chain_label(&c), "direct");
        c.chain.clear();
        assert_eq!(chain_label(&c), "香港-01", "falls back to the outbound");
    }

    #[test]
    fn process_name_is_the_path_basename() {
        let mut c = conn("a", 1);
        assert_eq!(process_name(&c), None);
        c.process = Some(ProcessInfo {
            process_path: r"C:\Program Files\Mozilla Firefox\firefox.exe".into(),
            ..Default::default()
        });
        assert_eq!(process_name(&c), Some("firefox.exe"));
        c.process.as_mut().unwrap().process_path = "/usr/bin/curl".into();
        assert_eq!(process_name(&c), Some("curl"));
        c.process.as_mut().unwrap().process_path = "svchost.exe".into();
        assert_eq!(process_name(&c), Some("svchost.exe"));
        c.process.as_mut().unwrap().process_path.clear();
        assert_eq!(process_name(&c), None, "looked up but no path");
    }

    #[test]
    fn age_runs_to_now_while_open_and_stops_at_close() {
        assert_eq!(connection_age_ms(&conn("a", 1_000), 5_500), 4_500);
        assert_eq!(connection_age_ms(&closed("b", 1_000, 3_000), 9_000), 2_000);
        assert_eq!(connection_age_ms(&conn("c", 9_000), 5_000), 0, "clock skew");
    }

    #[test]
    fn elapsed_is_compact() {
        assert_eq!(format_elapsed(-5), "0s");
        assert_eq!(format_elapsed(999), "0s");
        assert_eq!(format_elapsed(45_000), "45s");
        assert_eq!(format_elapsed(60_000), "1m 00s");
        assert_eq!(format_elapsed(192_000), "3m 12s");
        assert_eq!(format_elapsed(3_900_000), "1h 05m");
        assert_eq!(format_elapsed(86_400_000 + 3 * 3_600_000), "1d 3h");
    }

    #[test]
    fn summary_counts_rates_of_open_and_totals_of_all() {
        let mut a = conn("a", 1);
        a.uplink = 10;
        a.downlink = 100;
        a.uplink_total = 1_000;
        a.downlink_total = 10_000;
        let mut b = closed("b", 1, 2);
        b.uplink = 7; // ignored: a closed connection has no rate
        b.uplink_total = 5;
        b.downlink_total = 50;
        assert_eq!(
            summarize([&a, &b]),
            ConnectionSummary {
                open: 1,
                closed: 1,
                up_rate: 10,
                down_rate: 100,
                up_total: 1_005,
                down_total: 10_050,
            }
        );
        assert_eq!(summarize([]), ConnectionSummary::default());
    }
}
