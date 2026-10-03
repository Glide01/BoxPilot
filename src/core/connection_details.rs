//! Pure presentation logic for the Connections page's details panel: which
//! fields one connection has, grouped into sections, as display strings —
//! empty fields left out — plus keyboard Up/Down stepping through the list.
//! The panel only lays the result out. No gpui dependency.

use crate::core::bytefmt::{format_bytes, format_speed};
use crate::core::connections_view::{
    chain_label, connection_age_ms, format_elapsed, process_name, rule_label,
};
use crate::core::singbox_api::Connection;
use crate::i18n::s;

/// A group of fields, in panel order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DetailSection {
    Overview,
    Route,
    Source,
    Process,
    Traffic,
}

impl DetailSection {
    pub fn label(self) -> &'static str {
        let t = &s().connection_details;
        match self {
            DetailSection::Overview => t.overview,
            DetailSection::Route => t.route,
            DetailSection::Source => t.source_section,
            DetailSection::Process => t.process_section,
            DetailSection::Traffic => t.traffic_section,
        }
    }
}

/// One labelled value. Doubles as the field's identity in the panel (copy
/// feedback is per field).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DetailField {
    Destination,
    Domain,
    Protocol,
    Network,
    IpVersion,
    State,
    Inbound,
    Rule,
    Chain,
    Outbound,
    FromOutbound,
    SourceAddress,
    User,
    ProcessName,
    ProcessPath,
    ProcessId,
    ProcessUser,
    UploadSpeed,
    DownloadSpeed,
    Uploaded,
    Downloaded,
    OpenedAt,
    ClosedAt,
    Duration,
}

impl DetailField {
    pub fn label(self) -> &'static str {
        let t = &s().connection_details;
        match self {
            DetailField::Destination => t.destination,
            DetailField::Domain => t.domain,
            DetailField::Protocol => t.protocol,
            DetailField::Network => t.network,
            DetailField::IpVersion => t.ip_version,
            DetailField::State => t.state,
            DetailField::Inbound => t.inbound,
            DetailField::Rule => t.rule,
            DetailField::Chain => t.chain,
            DetailField::Outbound => t.outbound,
            DetailField::FromOutbound => t.from_outbound,
            DetailField::SourceAddress => t.source_address,
            DetailField::User => t.user,
            DetailField::ProcessName => t.process_name,
            DetailField::ProcessPath => t.process_path,
            DetailField::ProcessId => t.process_id,
            DetailField::ProcessUser => t.process_user,
            DetailField::UploadSpeed => t.upload_speed,
            DetailField::DownloadSpeed => t.download_speed,
            DetailField::Uploaded => t.uploaded,
            DetailField::Downloaded => t.downloaded,
            DetailField::OpenedAt => t.opened_at,
            DetailField::ClosedAt => t.closed_at,
            DetailField::Duration => t.duration,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetailRow {
    pub field: DetailField,
    /// What the panel shows, and what its copy button copies.
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetailGroup {
    pub section: DetailSection,
    /// Never empty: a section without fields is left out.
    pub rows: Vec<DetailRow>,
}

/// Everything the panel shows for `connection`, as of `now_ms` (the open
/// duration runs to it). `local_time` turns unix millis into a local
/// date-time (`timefmt::format_local_datetime`; tests pin a zone).
pub fn connection_details(
    connection: &Connection,
    now_ms: i64,
    local_time: impl Fn(i64) -> String,
) -> Vec<DetailGroup> {
    let t = &s().connection_details;
    let closed = connection.is_closed();
    let mut groups = Vec::new();
    let mut push = |section: DetailSection, fields: Vec<(DetailField, String)>| {
        let rows: Vec<DetailRow> = fields
            .into_iter()
            .filter(|(_, value)| !value.trim().is_empty())
            .map(|(field, value)| DetailRow { field, value })
            .collect();
        if !rows.is_empty() {
            groups.push(DetailGroup { section, rows });
        }
    };

    push(
        DetailSection::Overview,
        vec![
            (DetailField::Destination, connection.destination.clone()),
            (DetailField::Domain, connection.domain.clone()),
            (DetailField::Protocol, connection.protocol.clone()),
            (DetailField::Network, connection.network.clone()),
            (
                DetailField::IpVersion,
                ip_version_label(connection.ip_version),
            ),
            (
                DetailField::State,
                if closed { t.closed } else { t.active }.to_string(),
            ),
        ],
    );

    // The chain repeats the outbound when nothing but the outbound is in it.
    let chain = if connection.chain.len() > 1 {
        chain_label(connection)
    } else {
        String::new()
    };
    push(
        DetailSection::Route,
        vec![
            (
                DetailField::Inbound,
                tag_with_type(&connection.inbound, &connection.inbound_type),
            ),
            (DetailField::Rule, rule_label(connection).to_string()),
            (DetailField::Chain, chain),
            (
                DetailField::Outbound,
                tag_with_type(&connection.outbound, &connection.outbound_type),
            ),
            (DetailField::FromOutbound, connection.from_outbound.clone()),
        ],
    );

    push(
        DetailSection::Source,
        vec![
            (DetailField::SourceAddress, connection.source.clone()),
            (DetailField::User, connection.user.clone()),
        ],
    );

    if let Some(process) = &connection.process {
        let pid = if process.process_id > 0 {
            process.process_id.to_string()
        } else {
            String::new()
        };
        push(
            DetailSection::Process,
            vec![
                (
                    DetailField::ProcessName,
                    process_name(connection).unwrap_or_default().to_string(),
                ),
                (DetailField::ProcessPath, process.process_path.clone()),
                (DetailField::ProcessId, pid),
                (
                    DetailField::ProcessUser,
                    process_user(&process.user_name, process.user_id),
                ),
            ],
        );
    }

    let mut traffic = Vec::new();
    if !closed {
        traffic.push((DetailField::UploadSpeed, format_speed(connection.uplink)));
        traffic.push((
            DetailField::DownloadSpeed,
            format_speed(connection.downlink),
        ));
    }
    traffic.push((DetailField::Uploaded, format_bytes(connection.uplink_total)));
    traffic.push((
        DetailField::Downloaded,
        format_bytes(connection.downlink_total),
    ));
    if connection.created_at > 0 {
        traffic.push((DetailField::OpenedAt, local_time(connection.created_at)));
    }
    if let Some(closed_at) = connection.closed_at {
        traffic.push((DetailField::ClosedAt, local_time(closed_at)));
    }
    if connection.created_at > 0 {
        traffic.push((
            DetailField::Duration,
            format_elapsed(connection_age_ms(connection, now_ms)),
        ));
    }
    push(DetailSection::Traffic, traffic);

    groups
}

/// `mixed-in (mixed)`; just one of them when the other is empty or the
/// same.
fn tag_with_type(tag: &str, kind: &str) -> String {
    match (tag.is_empty(), kind.is_empty()) {
        (true, _) => kind.to_string(),
        (false, true) => tag.to_string(),
        (false, false) if tag == kind => tag.to_string(),
        (false, false) => format!("{tag} ({kind})"),
    }
}

/// `IPv4` / `IPv6`; empty when sing-box didn't say.
fn ip_version_label(version: u8) -> String {
    match version {
        4 | 6 => format!("IPv{version}"),
        _ => String::new(),
    }
}

/// `alice (1000)`, or the bare uid when only that is known. sing-box marks
/// an unknown uid with -1; a bare 0 is not shown either, since an unset
/// field reads 0 too (a named root still shows as `root (0)`).
fn process_user(name: &str, uid: i32) -> String {
    match (name.is_empty(), uid) {
        (false, uid) if uid >= 0 => format!("{name} ({uid})"),
        (false, _) => name.to_string(),
        (true, uid) if uid > 0 => uid.to_string(),
        (true, _) => String::new(),
    }
}

/// Which way Up / Down moves the selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Previous,
    Next,
}

/// The row to select after one Up / Down step in a list of `len` rows.
/// `current` is the selected row's index when it is in the list; when it
/// isn't (it closed and left the Active tab, or the filter hides it),
/// `last` is where it was, so the step continues from there: Down picks
/// the row that took its place. With neither, the first row. Stops at the
/// ends. `None` only for an empty list.
pub fn step_selection(
    len: usize,
    current: Option<usize>,
    last: Option<usize>,
    step: Step,
) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let max = len - 1;
    let ix = match (current, last, step) {
        (Some(ix), _, Step::Previous) => ix.saturating_sub(1),
        (Some(ix), _, Step::Next) => ix + 1,
        (None, Some(ix), Step::Previous) => ix.saturating_sub(1),
        (None, Some(ix), Step::Next) => ix,
        (None, None, _) => 0,
    };
    Some(ix.min(max))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::singbox_api::ProcessInfo;
    use crate::i18n::EN;

    fn local(ms: i64) -> String {
        format!("t{ms}")
    }

    fn sample() -> Connection {
        Connection {
            id: "a".into(),
            inbound: "mixed-in".into(),
            inbound_type: "mixed".into(),
            ip_version: 4,
            network: "tcp".into(),
            source: "192.168.1.20:50000".into(),
            destination: "1.1.1.1:443".into(),
            domain: "example.com".into(),
            protocol: "tls".into(),
            created_at: 1_000,
            uplink: 10,
            downlink: 2048,
            uplink_total: 100,
            downlink_total: 4096,
            rule: "domain_suffix=[example.com] => route(节点选择)".into(),
            outbound: "香港-01".into(),
            outbound_type: "vless".into(),
            chain: vec!["香港-01".into(), "auto".into(), "节点选择".into()],
            ..Default::default()
        }
    }

    fn fields(groups: &[DetailGroup]) -> Vec<(DetailSection, DetailField, &str)> {
        groups
            .iter()
            .flat_map(|g| {
                g.rows
                    .iter()
                    .map(move |r| (g.section, r.field, r.value.as_str()))
            })
            .collect()
    }

    // Tests read the English table: `s()` is English unless a test sets the
    // language, and none does (see `i18n::tests`).
    #[test]
    fn open_connection_shows_every_known_field_in_order() {
        use DetailField as F;
        use DetailSection as S;
        let groups = connection_details(&sample(), 66_000, local);
        assert_eq!(
            fields(&groups),
            vec![
                (S::Overview, F::Destination, "1.1.1.1:443"),
                (S::Overview, F::Domain, "example.com"),
                (S::Overview, F::Protocol, "tls"),
                (S::Overview, F::Network, "tcp"),
                (S::Overview, F::IpVersion, "IPv4"),
                (S::Overview, F::State, EN.connection_details.active),
                (S::Route, F::Inbound, "mixed-in (mixed)"),
                (
                    S::Route,
                    F::Rule,
                    "domain_suffix=[example.com] => route(节点选择)"
                ),
                (S::Route, F::Chain, "节点选择 → auto → 香港-01"),
                (S::Route, F::Outbound, "香港-01 (vless)"),
                (S::Source, F::SourceAddress, "192.168.1.20:50000"),
                (S::Traffic, F::UploadSpeed, "10 B/s"),
                (S::Traffic, F::DownloadSpeed, "2.0 KB/s"),
                (S::Traffic, F::Uploaded, "100 B"),
                (S::Traffic, F::Downloaded, "4.0 KB"),
                (S::Traffic, F::OpenedAt, "t1000"),
                (S::Traffic, F::Duration, "1m 05s"),
            ]
        );
    }

    #[test]
    fn closed_connection_drops_rates_and_adds_close_time() {
        let mut c = sample();
        c.closed_at = Some(31_000);
        c.uplink = 0;
        c.downlink = 0;
        let groups = connection_details(&c, 999_000, local);
        let all = fields(&groups);
        assert!(all.contains(&(
            DetailSection::Overview,
            DetailField::State,
            EN.connection_details.closed
        )));
        let traffic: Vec<_> = all
            .iter()
            .filter(|(s, _, _)| *s == DetailSection::Traffic)
            .map(|(_, f, v)| (*f, *v))
            .collect();
        assert_eq!(
            traffic,
            vec![
                (DetailField::Uploaded, "100 B"),
                (DetailField::Downloaded, "4.0 KB"),
                (DetailField::OpenedAt, "t1000"),
                (DetailField::ClosedAt, "t31000"),
                (DetailField::Duration, "30s"),
            ],
            "duration stops at the close"
        );
    }

    #[test]
    fn empty_fields_and_sections_are_left_out() {
        let c = Connection {
            id: "b".into(),
            network: "udp".into(),
            destination: "8.8.8.8:53".into(),
            outbound: "direct".into(),
            outbound_type: "direct".into(),
            chain: vec!["direct".into()],
            created_at: 5_000,
            ..Default::default()
        };
        let groups = connection_details(&c, 5_000, local);
        let sections: Vec<_> = groups.iter().map(|g| g.section).collect();
        assert_eq!(
            sections,
            [
                DetailSection::Overview,
                DetailSection::Route,
                DetailSection::Traffic
            ],
            "no source, no process"
        );
        let route: Vec<_> = fields(&groups)
            .into_iter()
            .filter(|(s, _, _)| *s == DetailSection::Route)
            .map(|(_, f, v)| (f, v))
            .collect();
        assert_eq!(
            route,
            vec![
                (DetailField::Rule, "final"),
                (DetailField::Outbound, "direct"),
            ],
            "single-hop chain and same-as-tag type are not repeated"
        );
        assert!(groups.iter().all(|g| !g.rows.is_empty()));
        let overview: Vec<_> = groups[0].rows.iter().map(|r| r.field).collect();
        assert_eq!(
            overview,
            [
                DetailField::Destination,
                DetailField::Network,
                DetailField::State
            ]
        );
    }

    #[test]
    fn route_shows_detour_and_untagged_inbound() {
        let mut c = sample();
        c.inbound.clear();
        c.inbound_type = "tun".into();
        c.from_outbound = "dns-out".into();
        let groups = connection_details(&c, 1_000, local);
        let route = &groups[1];
        assert_eq!(route.section, DetailSection::Route);
        assert_eq!(route.rows[0].value, "tun");
        assert_eq!(route.rows.last().unwrap().field, DetailField::FromOutbound);
        assert_eq!(route.rows.last().unwrap().value, "dns-out");
    }

    #[test]
    fn process_section_shows_what_sing_box_found() {
        let mut c = sample();
        c.user = "alice".into();
        c.process = Some(ProcessInfo {
            process_id: 4242,
            user_id: 1000,
            user_name: "alice".into(),
            process_path: "/usr/bin/curl".into(),
            package_names: vec![],
        });
        let groups = connection_details(&c, 1_000, local);
        let source = groups
            .iter()
            .find(|g| g.section == DetailSection::Source)
            .unwrap();
        assert_eq!(source.rows[1].field, DetailField::User);
        let process = groups
            .iter()
            .find(|g| g.section == DetailSection::Process)
            .unwrap();
        let rows: Vec<_> = process
            .rows
            .iter()
            .map(|r| (r.field, r.value.as_str()))
            .collect();
        assert_eq!(
            rows,
            vec![
                (DetailField::ProcessName, "curl"),
                (DetailField::ProcessPath, "/usr/bin/curl"),
                (DetailField::ProcessId, "4242"),
                (DetailField::ProcessUser, "alice (1000)"),
            ]
        );

        // Looked up, but nothing found: no empty Process section.
        c.process = Some(ProcessInfo {
            user_id: -1,
            ..Default::default()
        });
        let groups = connection_details(&c, 1_000, local);
        assert!(groups.iter().all(|g| g.section != DetailSection::Process));
    }

    #[test]
    fn process_user_variants() {
        assert_eq!(process_user("root", 0), "root (0)");
        assert_eq!(process_user("me", -1), "me");
        assert_eq!(process_user("", 1000), "1000");
        assert_eq!(process_user("", 0), "");
        assert_eq!(process_user("", -1), "");
    }

    #[test]
    fn ip_version_only_when_known() {
        assert_eq!(ip_version_label(4), "IPv4");
        assert_eq!(ip_version_label(6), "IPv6");
        assert_eq!(ip_version_label(0), "");
    }

    #[test]
    fn steps_move_one_row_and_stop_at_the_ends() {
        use Step::*;
        assert_eq!(step_selection(5, Some(2), None, Next), Some(3));
        assert_eq!(step_selection(5, Some(2), None, Previous), Some(1));
        assert_eq!(step_selection(5, Some(4), None, Next), Some(4));
        assert_eq!(step_selection(5, Some(0), None, Previous), Some(0));
        assert_eq!(step_selection(0, None, Some(3), Next), None);
    }

    #[test]
    fn steps_continue_from_where_a_vanished_row_was() {
        use Step::*;
        // Row 2 left the list: Down lands on what is now at 2, Up on 1.
        assert_eq!(step_selection(5, None, Some(2), Next), Some(2));
        assert_eq!(step_selection(5, None, Some(2), Previous), Some(1));
        // It was the last row and the list shrank.
        assert_eq!(step_selection(3, None, Some(3), Next), Some(2));
        assert_eq!(step_selection(3, None, Some(7), Previous), Some(2));
        // Nothing to go by: the first row.
        assert_eq!(step_selection(3, None, None, Previous), Some(0));
        assert_eq!(step_selection(3, None, None, Next), Some(0));
    }
}
