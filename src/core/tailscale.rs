//! Pure presentation helpers for the Tailscale page: peer ordering and
//! naming, exit-node choices, ping/transfer display strings, and safe local
//! file names for Taildrop saves and certificates. No gpui dependency — keep
//! it that way so it stays unit-testable.

use crate::core::singbox_api::{
    TaildropReceivingFile, TailscaleCertificate, TailscaleEndpointStatus, TailscalePeer,
    TailscalePing, TailscaleUserGroup,
};
use crate::core::timefmt::{format_relative_time, from_unix_secs};
use std::cmp::Ordering;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Tailscale's backend state once logged in and connected.
pub const BACKEND_RUNNING: &str = "Running";

/// A node's display name: its host name, else the first label of its
/// MagicDNS name, else its first Tailscale IP, else its stable ID. (The
/// exit node reported from the netmap fallback carries only ID + IPs.)
pub fn peer_name(peer: &TailscalePeer) -> String {
    if !peer.host_name.is_empty() {
        return peer.host_name.clone();
    }
    if let Some(label) = peer.dns_name.split('.').find(|l| !l.is_empty()) {
        return label.to_string();
    }
    if let Some(ip) = peer.tailscale_ips.first() {
        return ip.clone();
    }
    peer.stable_id.clone()
}

/// A MagicDNS name without its trailing root dot.
pub fn dns_name_display(dns_name: &str) -> &str {
    dns_name.strip_suffix('.').unwrap_or(dns_name)
}

/// The address to ping a peer at: its first IPv4 Tailscale address, else
/// its first address of any kind.
pub fn ping_address(peer: &TailscalePeer) -> Option<&str> {
    peer.tailscale_ips
        .iter()
        .find(|ip| !ip.contains(':'))
        .or_else(|| peer.tailscale_ips.first())
        .map(String::as_str)
}

/// A user group's heading: display name, else login name.
pub fn user_label(group: &TailscaleUserGroup) -> String {
    if !group.display_name.is_empty() {
        group.display_name.clone()
    } else if !group.login_name.is_empty() {
        group.login_name.clone()
    } else {
        "Unknown user".to_string()
    }
}

fn caseless(a: &str, b: &str) -> Ordering {
    a.to_lowercase()
        .cmp(&b.to_lowercase())
        .then_with(|| a.cmp(b))
}

/// Online peers first, then by name (case-insensitive), then stable ID so
/// the order never flickers between pushes.
fn compare_peers(a: &TailscalePeer, b: &TailscalePeer) -> Ordering {
    b.online
        .cmp(&a.online)
        .then_with(|| caseless(&peer_name(a), &peer_name(b)))
        .then_with(|| a.stable_id.cmp(&b.stable_id))
}

/// User groups in display order: users with an online peer first, then by
/// heading (case-insensitive), then user ID; each group's peers sorted
/// online-first, then by name. sing-box sorts peers within a group but
/// leaves the groups in map order, which shuffles between pushes. Empty
/// groups are dropped.
pub fn sorted_user_groups(groups: &[TailscaleUserGroup]) -> Vec<TailscaleUserGroup> {
    let mut groups: Vec<TailscaleUserGroup> = groups
        .iter()
        .filter(|g| !g.peers.is_empty())
        .cloned()
        .collect();
    for group in &mut groups {
        group.peers.sort_by(compare_peers);
    }
    groups.sort_by(|a, b| {
        let a_online = a.peers.iter().any(|p| p.online);
        let b_online = b.peers.iter().any(|p| p.online);
        b_online
            .cmp(&a_online)
            .then_with(|| caseless(&user_label(a), &user_label(b)))
            .then_with(|| a.user_id.cmp(&b.user_id))
    });
    groups
}

/// The peers the exit-node picker offers (besides "None").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExitNodeChoice {
    pub stable_id: String,
    pub label: String,
    pub online: bool,
    /// The exit node currently in use.
    pub selected: bool,
}

/// Every peer that offers itself as an exit node, online first, then by
/// name. The one in use is `selected`, whether the peer list or the
/// status's own `exit_node` says so.
pub fn exit_node_choices(status: &TailscaleEndpointStatus) -> Vec<ExitNodeChoice> {
    let current = current_exit_node_id(status);
    let mut peers: Vec<&TailscalePeer> = status
        .user_groups
        .iter()
        .flat_map(|g| g.peers.iter())
        .filter(|p| p.exit_node_option)
        .collect();
    peers.sort_by(|a, b| compare_peers(a, b));
    peers
        .into_iter()
        .map(|peer| ExitNodeChoice {
            stable_id: peer.stable_id.clone(),
            label: peer_name(peer),
            online: peer.online,
            selected: peer.exit_node || current == Some(peer.stable_id.as_str()),
        })
        .collect()
}

/// Stable ID of the exit node in use, if any.
pub fn current_exit_node_id(status: &TailscaleEndpointStatus) -> Option<&str> {
    status
        .exit_node
        .as_ref()
        .map(|p| p.stable_id.as_str())
        .filter(|id| !id.is_empty())
        .or_else(|| {
            status
                .user_groups
                .iter()
                .flat_map(|g| g.peers.iter())
                .find(|p| p.exit_node)
                .map(|p| p.stable_id.as_str())
        })
}

/// The exit node in use, named from the full peer entry when there is one
/// (the netmap fallback in `exit_node` carries no host name).
pub fn current_exit_node_label(status: &TailscaleEndpointStatus) -> Option<String> {
    let id = current_exit_node_id(status)?;
    let peer = status
        .user_groups
        .iter()
        .flat_map(|g| g.peers.iter())
        .find(|p| p.stable_id == id)
        .or(status.exit_node.as_ref())?;
    Some(peer_name(peer))
}

/// "Online", "Last seen 5 min ago", or "Offline" (never seen).
pub fn peer_presence_label(peer: &TailscalePeer, now: SystemTime) -> String {
    if peer.online {
        return "Online".to_string();
    }
    match peer.last_seen {
        Some(secs) if secs > 0 => format!(
            "Last seen {}",
            format_relative_time(from_unix_secs(secs as u64), now)
        ),
        _ => "Offline".to_string(),
    }
}

/// One ping result line: "23.4 ms · direct (1.2.3.4:41641)", "… · DERP
/// (fra)", "… · peer relay (…)", or "Failed: …".
pub fn ping_summary(ping: &TailscalePing) -> String {
    if let Some(error) = &ping.error {
        return format!("Failed: {}", error);
    }
    let path = if ping.is_direct {
        if ping.endpoint.is_empty() {
            "direct".to_string()
        } else {
            format!("direct ({})", ping.endpoint)
        }
    } else if !ping.peer_relay.is_empty() {
        format!("peer relay ({})", ping.peer_relay)
    } else if !ping.derp_region_code.is_empty() {
        format!("DERP ({})", ping.derp_region_code)
    } else if ping.derp_region_id != 0 {
        format!("DERP (region {})", ping.derp_region_id)
    } else {
        "relayed".to_string()
    };
    format!("{:.1} ms · {}", ping.latency_ms, path)
}

/// A byte count: `512 B`, `1.5 KB`, `12.0 MB`, `2.3 GB` (binary steps).
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{} B", bytes);
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{:.1} {}", value, UNITS[unit])
}

/// "↓ 1.5 MB · ↑ 200 B" for a peer's traffic counters.
pub fn traffic_label(peer: &TailscalePeer) -> String {
    format!(
        "↓ {} · ↑ {}",
        format_bytes(peer.rx_bytes),
        format_bytes(peer.tx_bytes)
    )
}

/// Completed fraction of an incoming transfer, when its size is known.
pub fn receiving_fraction(file: &TaildropReceivingFile) -> Option<f32> {
    match file.size {
        Some(0) => Some(1.0),
        Some(size) => Some((file.received_bytes as f64 / size as f64).clamp(0.0, 1.0) as f32),
        None => None,
    }
}

/// "1.0 MB of 4.0 MB (25%)", or "1.0 MB received" when the size is unknown.
pub fn receiving_progress_label(file: &TaildropReceivingFile) -> String {
    match (file.size, receiving_fraction(file)) {
        (Some(size), Some(fraction)) => format!(
            "{} of {} ({}%)",
            format_bytes(file.received_bytes),
            format_bytes(size),
            (fraction * 100.0).floor() as u32
        ),
        _ => format!("{} received", format_bytes(file.received_bytes)),
    }
}

/// A file name that is safe to create on Windows: characters it rejects
/// (`<>:"/\|?*`, control characters) become `_`, trailing dots and spaces
/// go, and reserved device names (`CON`, `NUL`, `COM1`…) get a `_` prefix.
/// Taildrop names come from other tailnet nodes, which may run anything.
pub fn safe_file_name(name: &str) -> String {
    let mut safe: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    while safe.ends_with('.') || safe.ends_with(' ') {
        safe.pop();
    }
    if safe.is_empty() {
        return "file".to_string();
    }
    let stem = safe
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit());
    if reserved {
        safe.insert(0, '_');
    }
    safe
}

/// File names for a fetched certificate pair: `<domain>.crt` and
/// `<domain>.key`.
pub fn certificate_file_names(domain: &str) -> (String, String) {
    let base = safe_file_name(domain);
    (format!("{}.crt", base), format!("{}.key", base))
}

/// Where save dialogs open: the user's Downloads folder, else home.
pub fn default_save_dir() -> PathBuf {
    dirs::download_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(std::env::temp_dir)
}

/// Write a certificate pair into `dir` as `<domain>.crt` + `<domain>.key`,
/// replacing older copies (re-fetching after renewal is the common case).
/// A newly created key file is owner-only where the platform has Unix
/// permissions; on Windows it inherits the folder's ACL. Returns both paths.
pub fn save_certificate_pair(
    dir: &Path,
    domain: &str,
    certificate: &TailscaleCertificate,
) -> io::Result<(PathBuf, PathBuf)> {
    let (cert_name, key_name) = certificate_file_names(domain);
    let cert_path = dir.join(cert_name);
    let key_path = dir.join(key_name);
    fs::write(&cert_path, &certificate.certificate_pem)?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut key = options.open(&key_path)?;
    key.write_all(&certificate.private_key_pem)?;
    key.sync_all()?;
    Ok((cert_path, key_path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(name: &str, online: bool) -> TailscalePeer {
        TailscalePeer {
            stable_id: format!("id-{}", name),
            host_name: name.into(),
            online,
            ..Default::default()
        }
    }

    fn group(user_id: i64, display: &str, peers: Vec<TailscalePeer>) -> TailscaleUserGroup {
        TailscaleUserGroup {
            user_id,
            display_name: display.into(),
            login_name: format!("{}@example.com", display.to_lowercase()),
            peers,
            ..Default::default()
        }
    }

    fn names(peers: &[TailscalePeer]) -> Vec<&str> {
        peers.iter().map(|p| p.host_name.as_str()).collect()
    }

    #[test]
    fn peer_name_falls_back_through_dns_ip_and_id() {
        let mut p = TailscalePeer {
            stable_id: "nX".into(),
            dns_name: "box.tail1234.ts.net.".into(),
            tailscale_ips: vec!["100.64.0.9".into()],
            ..Default::default()
        };
        assert_eq!(peer_name(&p), "box");
        p.host_name = "Box-PC".into();
        assert_eq!(peer_name(&p), "Box-PC");
        p.host_name.clear();
        p.dns_name.clear();
        assert_eq!(peer_name(&p), "100.64.0.9");
        p.tailscale_ips.clear();
        assert_eq!(peer_name(&p), "nX");
    }

    #[test]
    fn dns_name_loses_trailing_dot() {
        assert_eq!(dns_name_display("a.b.ts.net."), "a.b.ts.net");
        assert_eq!(dns_name_display("a.b"), "a.b");
    }

    #[test]
    fn ping_address_prefers_ipv4() {
        let mut p = TailscalePeer {
            tailscale_ips: vec!["fd7a:115c:a1e0::1".into(), "100.64.0.1".into()],
            ..Default::default()
        };
        assert_eq!(ping_address(&p), Some("100.64.0.1"));
        p.tailscale_ips.truncate(1);
        assert_eq!(ping_address(&p), Some("fd7a:115c:a1e0::1"));
        p.tailscale_ips.clear();
        assert_eq!(ping_address(&p), None);
    }

    #[test]
    fn user_groups_sort_online_users_first_then_by_name() {
        let groups = vec![
            group(3, "zoe", vec![peer("z1", true)]),
            group(1, "Bob", vec![peer("b1", false)]),
            group(
                2,
                "alice",
                vec![peer("a2", false), peer("A1", true), peer("a0", false)],
            ),
            group(4, "Empty", vec![]),
        ];
        let sorted = sorted_user_groups(&groups);
        let users: Vec<_> = sorted.iter().map(|g| g.display_name.as_str()).collect();
        assert_eq!(
            users,
            ["alice", "zoe", "Bob"],
            "online first, caseless, empty dropped"
        );
        assert_eq!(names(&sorted[0].peers), ["A1", "a0", "a2"]);
    }

    #[test]
    fn user_label_falls_back_to_login() {
        let mut g = group(1, "", vec![]);
        assert_eq!(user_label(&g), "@example.com");
        g.login_name.clear();
        assert_eq!(user_label(&g), "Unknown user");
        g.display_name = "Me".into();
        assert_eq!(user_label(&g), "Me");
    }

    fn status_with(groups: Vec<TailscaleUserGroup>) -> TailscaleEndpointStatus {
        TailscaleEndpointStatus {
            endpoint_tag: "ts".into(),
            backend_state: BACKEND_RUNNING.into(),
            user_groups: groups,
            ..Default::default()
        }
    }

    #[test]
    fn exit_node_choices_list_only_offering_peers() {
        let mut exit_a = peer("exit-a", false);
        exit_a.exit_node_option = true;
        let mut exit_b = peer("Exit-B", true);
        exit_b.exit_node_option = true;
        exit_b.exit_node = true;
        let plain = peer("plain", true);
        let status = status_with(vec![
            group(1, "a", vec![exit_a, plain]),
            group(2, "b", vec![exit_b]),
        ]);
        assert_eq!(
            exit_node_choices(&status),
            vec![
                ExitNodeChoice {
                    stable_id: "id-Exit-B".into(),
                    label: "Exit-B".into(),
                    online: true,
                    selected: true,
                },
                ExitNodeChoice {
                    stable_id: "id-exit-a".into(),
                    label: "exit-a".into(),
                    online: false,
                    selected: false,
                },
            ]
        );
        assert_eq!(current_exit_node_id(&status), Some("id-Exit-B"));
        assert_eq!(current_exit_node_label(&status).as_deref(), Some("Exit-B"));
    }

    /// The netmap fallback reports the exit node with only ID + IPs, and
    /// the peer entry may not flag it — the status field still selects it,
    /// and the name comes from the full peer entry.
    #[test]
    fn exit_node_from_status_field_selects_and_names() {
        let mut exit = peer("exit", true);
        exit.exit_node_option = true;
        let mut status = status_with(vec![group(1, "a", vec![exit])]);
        status.exit_node = Some(TailscalePeer {
            stable_id: "id-exit".into(),
            tailscale_ips: vec!["100.64.0.7".into()],
            exit_node: true,
            ..Default::default()
        });
        assert!(exit_node_choices(&status)[0].selected);
        assert_eq!(current_exit_node_label(&status).as_deref(), Some("exit"));

        status.user_groups.clear();
        assert_eq!(
            current_exit_node_label(&status).as_deref(),
            Some("100.64.0.7")
        );
        status.exit_node = None;
        assert_eq!(current_exit_node_label(&status), None);
    }

    #[test]
    fn presence_label() {
        let now = from_unix_secs(1_000_000);
        let mut p = peer("p", true);
        assert_eq!(peer_presence_label(&p, now), "Online");
        p.online = false;
        assert_eq!(peer_presence_label(&p, now), "Offline");
        p.last_seen = Some(1_000_000 - 300);
        assert_eq!(peer_presence_label(&p, now), "Last seen 5 min ago");
    }

    fn ping() -> TailscalePing {
        TailscalePing {
            latency_ms: 23.44,
            is_direct: false,
            endpoint: String::new(),
            derp_region_id: 0,
            derp_region_code: String::new(),
            peer_relay: String::new(),
            error: None,
        }
    }

    #[test]
    fn ping_summary_names_the_path() {
        let mut p = ping();
        p.is_direct = true;
        p.endpoint = "203.0.113.5:41641".into();
        assert_eq!(ping_summary(&p), "23.4 ms · direct (203.0.113.5:41641)");

        let mut p = ping();
        p.derp_region_id = 4;
        p.derp_region_code = "fra".into();
        assert_eq!(ping_summary(&p), "23.4 ms · DERP (fra)");
        p.derp_region_code.clear();
        assert_eq!(ping_summary(&p), "23.4 ms · DERP (region 4)");

        let mut p = ping();
        p.peer_relay = "100.64.0.3:7777".into();
        p.derp_region_code = "fra".into();
        assert_eq!(ping_summary(&p), "23.4 ms · peer relay (100.64.0.3:7777)");

        let mut p = ping();
        p.error = Some("timeout".into());
        assert_eq!(ping_summary(&p), "Failed: timeout");
    }

    #[test]
    fn bytes_format_in_binary_steps() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(12 * 1024 * 1024), "12.0 MB");
        assert_eq!(
            format_bytes(5 * 1024 * 1024 * 1024 * 1024 * 1024),
            "5120.0 TB"
        );
        let mut p = peer("p", true);
        p.rx_bytes = 2048;
        p.tx_bytes = 10;
        assert_eq!(traffic_label(&p), "↓ 2.0 KB · ↑ 10 B");
    }

    #[test]
    fn receiving_progress() {
        let mut f = TaildropReceivingFile {
            size: Some(4 * 1024 * 1024),
            received_bytes: 1024 * 1024,
            ..Default::default()
        };
        assert_eq!(receiving_fraction(&f), Some(0.25));
        assert_eq!(receiving_progress_label(&f), "1.0 MB of 4.0 MB (25%)");
        f.size = None;
        assert_eq!(receiving_fraction(&f), None);
        assert_eq!(receiving_progress_label(&f), "1.0 MB received");
        f.size = Some(0);
        f.received_bytes = 0;
        assert_eq!(receiving_fraction(&f), Some(1.0));
    }

    #[test]
    fn safe_file_names_survive_windows() {
        assert_eq!(safe_file_name("photo.jpg"), "photo.jpg");
        assert_eq!(safe_file_name("a:b?c*.txt"), "a_b_c_.txt");
        assert_eq!(safe_file_name("notes. "), "notes");
        assert_eq!(safe_file_name("..."), "file");
        assert_eq!(safe_file_name("con.txt"), "_con.txt");
        assert_eq!(safe_file_name("COM1"), "_COM1");
        assert_eq!(safe_file_name("COM10.txt"), "COM10.txt");
        assert_eq!(safe_file_name("console.log"), "console.log");
        assert_eq!(safe_file_name("tab\there"), "tab_here");
        assert_eq!(safe_file_name("报告 (1).pdf"), "报告 (1).pdf");
    }

    #[test]
    fn certificate_files_are_named_after_the_domain() {
        assert_eq!(
            certificate_file_names("box.tail1234.ts.net"),
            (
                "box.tail1234.ts.net.crt".to_string(),
                "box.tail1234.ts.net.key".to_string()
            )
        );
    }

    #[test]
    fn certificate_pair_is_written_next_to_each_other() {
        let dir = std::env::temp_dir().join(format!("boxpilot-cert-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let certificate = TailscaleCertificate {
            certificate_pem: b"CERT".to_vec(),
            private_key_pem: b"KEY".to_vec(),
        };
        // Twice: a second save replaces the first.
        save_certificate_pair(&dir, "a.ts.net", &certificate).unwrap();
        let (cert, key) = save_certificate_pair(&dir, "a.ts.net", &certificate).unwrap();
        assert_eq!(cert, dir.join("a.ts.net.crt"));
        assert_eq!(key, dir.join("a.ts.net.key"));
        assert_eq!(fs::read(&cert).unwrap(), b"CERT");
        assert_eq!(fs::read(&key).unwrap(), b"KEY");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&key).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        fs::remove_dir_all(&dir).unwrap();
    }
}
