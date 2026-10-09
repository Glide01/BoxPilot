//! Pure presentation logic for the Connections page: which connections a
//! view shows (active/closed tab + text filter), in what order, the summary
//! header, and each row's display strings. The page only places the results
//! in layout. No gpui dependency — keep it that way so the core test shim
//! keeps working.

use crate::core::singbox_api::Connection;
use std::cmp::Reverse;

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

/// Row order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ConnectionSort {
    /// Most recent first: opened (active) or closed (closed) last on top.
    #[default]
    Newest,
    /// Most bytes moved (up + down totals) first.
    Traffic,
}

/// The connections `view` shows, filtered by `query` (see `matches_query`)
/// and ordered by `sort`. Ties fall back to newest, then id, so rows don't
/// shuffle between identical-looking refreshes.
pub fn select_connections<'a>(
    connections: impl IntoIterator<Item = &'a Connection>,
    view: ConnectionView,
    query: &str,
    sort: ConnectionSort,
) -> Vec<&'a Connection> {
    let terms = query_terms(query);
    let mut rows: Vec<&Connection> = connections
        .into_iter()
        .filter(|c| view.includes(c) && matches_terms(c, &terms))
        .collect();
    let newest = |c: &Connection| c.closed_at.unwrap_or(c.created_at);
    match sort {
        ConnectionSort::Newest => {
            rows.sort_by(|a, b| (Reverse(newest(a)), &a.id).cmp(&(Reverse(newest(b)), &b.id)));
        }
        ConnectionSort::Traffic => {
            rows.sort_by(|a, b| {
                (Reverse(total_bytes(a)), Reverse(newest(a)), &a.id).cmp(&(
                    Reverse(total_bytes(b)),
                    Reverse(newest(b)),
                    &b.id,
                ))
            });
        }
    }
    rows
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

/// A unix-ms instant as a local wall-clock time, split for the list's time
/// column: `("16:16:31", ".902")` — the seconds' fraction is drawn quieter.
pub fn format_clock(ms: i64) -> (String, String) {
    use chrono::TimeZone;
    let time = chrono::Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|at| at.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "--:--:--".into());
    (time, format!(".{:03}", ms.rem_euclid(1000)))
}

/// A unix-ms instant as local hours and minutes, for the timeline's axis.
pub fn format_hour_minute(ms: i64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|at| at.format("%H:%M").to_string())
        .unwrap_or_default()
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

/// How far back the page's timeline reaches: the shortest of these that
/// covers the oldest matching connection, else the longest.
pub const TIMELINE_SPANS_MIN: [i64; 5] = [5, 15, 30, 60, 180];

/// One bar of the timeline: connections opened in its interval, by network.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TimelineBucket {
    pub tcp: u32,
    /// UDP, and anything that is neither (sing-box only reports the two).
    pub udp: u32,
}

impl TimelineBucket {
    pub fn total(&self) -> u32 {
        self.tcp + self.udp
    }
}

/// When the listed connections were opened: `buckets.len()` equal
/// intervals ending at `now`, oldest first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Timeline {
    /// Unix ms where the first interval starts.
    pub start_ms: i64,
    pub bucket_ms: i64,
    pub buckets: Vec<TimelineBucket>,
}

impl Timeline {
    pub fn span_ms(&self) -> i64 {
        self.bucket_ms * self.buckets.len() as i64
    }

    /// The tallest bar, at least 1 so an empty timeline scales.
    pub fn peak(&self) -> u32 {
        self.buckets
            .iter()
            .map(TimelineBucket::total)
            .max()
            .unwrap_or(0)
            .max(1)
    }
}

/// The timeline of `connections` matching `query` (open and closed alike:
/// it shows activity, whichever tab is listed), in `count` bars up to
/// `now_ms`, over the shortest of [`TIMELINE_SPANS_MIN`] that reaches back
/// to the oldest of them. Connections older than the longest span are left
/// out; ones dated after `now_ms` (clock skew) land in the last bar.
pub fn timeline<'a>(
    connections: impl IntoIterator<Item = &'a Connection>,
    query: &str,
    now_ms: i64,
    count: usize,
) -> Timeline {
    let count = count.max(1);
    let terms = query_terms(query);
    let opened: Vec<(i64, bool)> = connections
        .into_iter()
        .filter(|c| matches_terms(c, &terms))
        .map(|c| (c.created_at, c.network.eq_ignore_ascii_case("tcp")))
        .collect();
    let oldest = opened.iter().map(|(at, _)| *at).min().unwrap_or(now_ms);
    let span_min = TIMELINE_SPANS_MIN
        .iter()
        .copied()
        .find(|min| now_ms - oldest <= min * 60_000)
        .unwrap_or(TIMELINE_SPANS_MIN[TIMELINE_SPANS_MIN.len() - 1]);
    let bucket_ms = (span_min * 60_000 / count as i64).max(1);
    let start_ms = now_ms - bucket_ms * count as i64;
    let mut buckets = vec![TimelineBucket::default(); count];
    for (at, tcp) in opened {
        if at < start_ms {
            continue;
        }
        let ix = (((at - start_ms) / bucket_ms) as usize).min(count - 1);
        if tcp {
            buckets[ix].tcp += 1;
        } else {
            buckets[ix].udp += 1;
        }
    }
    Timeline {
        start_ms,
        bucket_ms,
        buckets,
    }
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

    fn ids(rows: &[&Connection]) -> Vec<String> {
        rows.iter().map(|c| c.id.clone()).collect()
    }

    #[test]
    fn views_split_open_and_closed() {
        let all = [conn("a", 1), closed("b", 2, 3)];
        let active = select_connections(&all, ConnectionView::Active, "", ConnectionSort::Newest);
        assert_eq!(ids(&active), ["a"]);
        let gone = select_connections(&all, ConnectionView::Closed, "", ConnectionSort::Newest);
        assert_eq!(ids(&gone), ["b"]);
    }

    #[test]
    fn newest_first_uses_close_time_for_closed() {
        let open = [conn("old", 100), conn("new", 300), conn("mid", 200)];
        let rows = select_connections(&open, ConnectionView::Active, "", ConnectionSort::Newest);
        assert_eq!(ids(&rows), ["new", "mid", "old"]);

        // Opened first but closed last → on top of the closed tab.
        let gone = [closed("x", 100, 900), closed("y", 500, 600)];
        let rows = select_connections(&gone, ConnectionView::Closed, "", ConnectionSort::Newest);
        assert_eq!(ids(&rows), ["x", "y"]);
    }

    #[test]
    fn ties_are_ordered_by_id_for_stable_rows() {
        let all = [conn("b", 100), conn("a", 100), conn("c", 100)];
        let rows = select_connections(&all, ConnectionView::Active, "", ConnectionSort::Newest);
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
        let rows = select_connections(&all, ConnectionView::Active, "", ConnectionSort::Traffic);
        assert_eq!(ids(&rows), ["heavy", "tie-new", "tie-old", "light"]);
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
    #[test]
    fn timeline_picks_the_shortest_span_covering_the_oldest() {
        let now = 10 * 3_600_000;
        let mut udp = conn("u", now - 4 * 60_000);
        udp.network = "udp".into();
        let recent = [conn("a", now - 1_000), conn("b", now - 1_000), udp];
        let t = timeline(recent.iter(), "", now, 10);
        assert_eq!(t.span_ms(), 5 * 60_000);
        assert_eq!(t.start_ms, now - 5 * 60_000);
        assert_eq!(t.buckets.len(), 10);
        assert_eq!(t.buckets[9], TimelineBucket { tcp: 2, udp: 0 });
        assert_eq!(t.buckets[2], TimelineBucket { tcp: 0, udp: 1 });
        assert_eq!(t.peak(), 2);

        // 20 minutes back needs the 30-minute span.
        let older = [conn("a", now - 20 * 60_000)];
        assert_eq!(timeline(older.iter(), "", now, 30).span_ms(), 30 * 60_000);
    }

    #[test]
    fn timeline_filters_and_drops_what_is_out_of_range() {
        let now = 100 * 3_600_000;
        let mut google = conn("g", now - 1);
        google.domain = "www.google.com".into();
        let rows = [
            google,
            conn("x", now - 1),
            // Past the longest span: left out, and the span stays maximal.
            conn("old", now - 10 * 3_600_000),
            // From the future: the last bar.
            conn("skew", now + 5_000),
        ];
        let t = timeline(rows.iter(), "google", now, 6);
        assert_eq!(t.buckets.iter().map(|b| b.total()).sum::<u32>(), 1);
        let all = timeline(rows.iter(), "", now, 6);
        assert_eq!(all.span_ms(), 180 * 60_000);
        assert_eq!(all.buckets[5].tcp, 3);
        assert_eq!(all.buckets.iter().map(|b| b.total()).sum::<u32>(), 3);
        // Nothing yet: an empty bar chart still scales.
        let empty = timeline([], "", now, 4);
        assert_eq!(empty.span_ms(), 5 * 60_000);
        assert_eq!(empty.peak(), 1);
    }

    #[test]
    fn clock_splits_off_the_milliseconds() {
        let (time, fraction) = format_clock(1_700_000_000_902);
        assert_eq!(time.len(), 8);
        assert_eq!(time.matches(':').count(), 2);
        assert_eq!(fraction, ".902");
        assert_eq!(format_clock(5).1, ".005");
        assert_eq!(format_hour_minute(1_700_000_000_000).len(), 5);
    }
}
