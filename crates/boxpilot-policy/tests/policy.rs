//! The policy against real-looking profiles: what passes unchanged, what is
//! dropped, and every refusal by its pointer. Both fixtures decode as sing-box
//! 1.14.2 configs (`sing-box check` gets past decoding).

use boxpilot_policy::{
    attach, check, is_attachment_id, local_file_fields, materialize, AttachError, Checked,
    DropReason, Dropped, Expected, Limits, LocalFileField, Placement, Refusal, RefusalKind,
    ATTACHMENT_PREFIX,
};
use serde_json::{json, Value};
use std::collections::BTreeSet;

/// A typical subscription: vless+ws+tls, vmess+grpc, hysteria2,
/// shadowsocks behind selector / urltest groups; DoH; remote rule sets,
/// process rules and a logical rule; a cache file and a dashboard.
const SUBSCRIPTION: &str = include_str!("fixtures/subscription.json");

/// A corporate profile in which every file-read field the policy knows
/// holds a local path.
const LOCAL_FILES: &str = include_str!("fixtures/local_files.json");

fn parse(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

fn ids(list: &[&str]) -> BTreeSet<String> {
    list.iter().map(|id| id.to_string()).collect()
}

fn reference(id: &str) -> String {
    format!("{ATTACHMENT_PREFIX}{id}")
}

fn passes_with(config: &str, attachments: &[&str]) -> Checked {
    check(config, &ids(attachments), &Limits::default())
        .unwrap_or_else(|refusals| panic!("refused: {refusals:#?}"))
}

fn passes(config: &str) -> Checked {
    passes_with(config, &[])
}

/// Every refusal as (pointer, kind), sorted by pointer.
fn refusals_with(config: &str, attachments: &[&str]) -> Vec<(String, RefusalKind)> {
    let refusals = match check(config, &ids(attachments), &Limits::default()) {
        Ok(checked) => panic!("passed: {:#}", checked.config()),
        Err(refusals) => refusals,
    };
    let mut list: Vec<_> = refusals.into_iter().map(|r| (r.pointer, r.kind)).collect();
    list.sort_by(|a, b| a.0.cmp(&b.0));
    list
}

fn refusals(config: &str) -> Vec<(String, RefusalKind)> {
    refusals_with(config, &[])
}

fn at(pointer: &str, kind: RefusalKind) -> (String, RefusalKind) {
    (pointer.to_string(), kind)
}

fn dropped(pointer: &str, reason: DropReason) -> Dropped {
    Dropped {
        pointer: pointer.to_string(),
        reason,
    }
}

fn placement() -> Placement {
    Placement {
        run_dir: "/var/run/boxpilot/run-7".into(),
        cache_file: "/Library/Application Support/BoxPilot Helper/501/cache.db".into(),
        tailscale_dir: "/Library/Application Support/BoxPilot Helper/501/tailscale".into(),
        separator: '/',
    }
}

// ---- Real profiles pass, meaning unchanged ----

/// The subscription runs as written, short of its dashboard: only
/// `clash_api` is dropped, and every routing section is untouched.
#[test]
fn subscription_passes_with_only_its_dashboard_dropped() {
    let checked = passes(SUBSCRIPTION);
    assert_eq!(
        checked.dropped(),
        [dropped("/experimental/clash_api", DropReason::ControlPlane)]
    );
    let original = parse(SUBSCRIPTION);
    let config = checked.config();
    for section in ["log", "dns", "outbounds", "route"] {
        assert_eq!(
            config[section], original[section],
            "{section} must be unchanged"
        );
    }
    assert_eq!(
        config["experimental"],
        json!({"cache_file": {"enabled": true, "store_fakeip": true}})
    );
    let sections: Vec<&str> = config
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        sections,
        ["log", "dns", "outbounds", "route", "experimental"]
    );
    assert!(checked.attachment_ids().is_empty());
}

#[test]
fn subscription_materializes_with_the_helpers_cache_file() {
    let config = materialize(passes(SUBSCRIPTION), &placement());
    assert_eq!(
        config["experimental"]["cache_file"],
        json!({
            "enabled": true,
            "store_fakeip": true,
            "path": "/Library/Application Support/BoxPilot Helper/501/cache.db"
        })
    );
    let original = parse(SUBSCRIPTION);
    for section in ["log", "dns", "outbounds", "route"] {
        assert_eq!(
            config[section], original[section],
            "{section} must be unchanged"
        );
    }
}

/// A canonical config with controllers of its own (as `strip_inbounds`
/// keeps them): its `api` service, `clash_api` and `v2ray_api` are dropped,
/// and so is its cache path; the rest of `cache_file` stays.
#[test]
fn control_planes_and_the_cache_path_are_dropped() {
    let config = r#"{
        "outbounds": [{"type": "direct", "tag": "direct"}],
        "services": [
            {"type": "api", "tag": "theirs", "listen": "0.0.0.0", "listen_port": 9091}
        ],
        "experimental": {
            "clash_api": {"external_controller": "0.0.0.0:9090", "secret": ""},
            "v2ray_api": {"listen": "127.0.0.1:8080", "stats": {"enabled": true}},
            "cache_file": {"enabled": false, "path": "C:\\Windows\\System32\\config\\SAM", "store_rdrc": true}
        }
    }"#;
    let checked = passes(config);
    assert_eq!(
        checked.dropped(),
        [
            dropped("/services/0", DropReason::ControlPlane),
            dropped("/experimental/clash_api", DropReason::ControlPlane),
            dropped("/experimental/v2ray_api", DropReason::ControlPlane),
            dropped("/experimental/cache_file/path", DropReason::HelperOwned),
        ]
    );
    assert_eq!(
        *checked.config(),
        json!({
            "outbounds": [{"type": "direct", "tag": "direct"}],
            "experimental": {"cache_file": {"enabled": false, "store_rdrc": true}}
        })
    );
}

#[test]
fn log_output_is_dropped_for_stdout() {
    let config =
        r#"{"log": {"level": "debug", "output": "C:\\Windows\\win.ini", "timestamp": true}}"#;
    let checked = passes(config);
    assert_eq!(
        checked.dropped(),
        [dropped("/log/output", DropReason::HelperOwned)]
    );
    assert_eq!(
        checked.config()["log"],
        json!({"level": "debug", "timestamp": true})
    );
}

#[test]
fn schema_annotation_is_dropped() {
    let config = r#"{
        "$schema": "https://sing-box.sagernet.org/schema.json",
        "outbounds": [{"type": "direct", "tag": "direct"}]
    }"#;
    let checked = passes(config);
    assert_eq!(
        checked.dropped(),
        [dropped("/$schema", DropReason::Annotation)]
    );
    assert!(checked.config().get("$schema").is_none());
}

// ---- Keys that match the shape but name no file ----

/// V2Ray http / ws / httpupgrade transports, the `http` outbound and DoH /
/// DoH3 servers all have a `path` that is a URL path.
#[test]
fn url_paths_are_not_files() {
    let config = r#"{
        "dns": {"servers": [
            {"type": "https", "tag": "doh", "server": "dns.google", "path": "/dns-query"},
            {"type": "h3", "tag": "doh3", "server": "1.1.1.1", "path": "/dns-query", "headers": {"Accept": "application/dns-message"}}
        ]},
        "outbounds": [
            {"type": "vless", "tag": "ws", "server": "a.example", "server_port": 443,
             "uuid": "bf000d23-0752-40b4-affe-68f7707a9661",
             "transport": {"type": "ws", "path": "/ray?ed=2048", "headers": {"Host": "a.example"}}},
            {"type": "vmess", "tag": "h2", "server": "b.example", "server_port": 443,
             "uuid": "bf000d23-0752-40b4-affe-68f7707a9661",
             "transport": {"type": "http", "host": ["b.example"], "path": "/h2"}},
            {"type": "trojan", "tag": "up", "server": "c.example", "server_port": 443, "password": "p",
             "transport": {"type": "httpupgrade", "host": "c.example", "path": "/upgrade"}},
            {"type": "http", "tag": "corp-proxy", "server": "proxy.corp.example", "server_port": 8080,
             "path": "/connect", "headers": {"Proxy-Authorization": "Basic dTpw"}}
        ]
    }"#;
    let checked = passes(config);
    assert!(checked.dropped().is_empty());
    assert_eq!(*checked.config(), parse(config));
}

/// `process_path` matches a connection's process; nothing is opened. In
/// route and DNS rules, logical rules, and an inline rule set's headless
/// rules, nested logical ones included. `process_path_regex` never matched
/// the shape.
#[test]
fn process_path_matchers_are_not_files() {
    let config = r#"{
        "dns": {"rules": [
            {"process_path": "C:\\Program Files\\Mozilla Firefox\\firefox.exe", "server": "doh"},
            {"type": "logical", "mode": "or", "rules": [
                {"process_path": ["/usr/bin/curl"]},
                {"process_path_regex": ["^/opt/.+"]}
            ], "action": "reject"}
        ]},
        "route": {
            "rules": [
                {"process_path": ["C:\\Games\\game.exe"], "outbound": "direct"},
                {"type": "logical", "mode": "and", "rules": [
                    {"type": "logical", "mode": "or", "rules": [{"process_path": ["/usr/bin/wget"]}]},
                    {"process_path_regex": ["(?i)steam"], "invert": true}
                ], "outbound": "direct"}
            ],
            "rule_set": [
                {"tag": "apps", "rules": [
                    {"process_path": ["/Applications/Slack.app/Contents/MacOS/Slack"]},
                    {"type": "logical", "mode": "and", "rules": [{"process_path": "/usr/bin/ssh"}]}
                ]},
                {"type": "inline", "tag": "more-apps", "rules": [{"process_path": ["/usr/bin/git"]}]}
            ]
        }
    }"#;
    assert_eq!(*passes(config).config(), parse(config));
}

/// `tcp_multi_path` is a dial flag (MPTCP), allowed where dial fields
/// stand, and nowhere else: the allowlist is scoped, not a name.
#[test]
fn tcp_multi_path_is_a_dial_flag_where_dial_fields_stand() {
    let config = r#"{
        "ntp": {"enabled": true, "server": "time.apple.com", "tcp_multi_path": true},
        "dns": {"servers": [{"type": "tcp", "tag": "t", "server": "8.8.8.8", "tcp_multi_path": true}]},
        "outbounds": [{"type": "direct", "tag": "direct", "tcp_multi_path": true}],
        "endpoints": [{"type": "wireguard", "tag": "wg", "address": ["10.0.0.2/32"],
                       "private_key": "YNXtAzepDqRv9H52osJVDQnznT5AL11eVZ7+JmZ9b2I=",
                       "tcp_multi_path": true}],
        "route": {
            "rules": [{"ip_is_private": true, "action": "direct", "tcp_multi_path": true}],
            "rule_set": [
                {"type": "remote", "tag": "r", "url": "https://rules.example.com/r.srs",
                 "http_client": {"detour": "direct", "tcp_multi_path": true}},
                {"type": "remote", "tag": "s", "url": "https://rules.example.com/s.srs",
                 "http_client": "rules-client"}
            ]
        }
    }"#;
    passes(config);

    let elsewhere = r#"{"outbounds": [{"type": "vless", "tag": "v", "server": "a.example", "server_port": 443,
        "uuid": "bf000d23-0752-40b4-affe-68f7707a9661",
        "multiplex": {"enabled": true, "tcp_multi_path": true}}]}"#;
    assert_eq!(
        refusals(elsewhere),
        [at(
            "/outbounds/0/multiplex/tcp_multi_path",
            RefusalKind::FilesystemPath
        )]
    );
}

// ---- One hostile fixture per refused class ----

#[test]
fn tor_runs_a_program() {
    let config = r#"{"outbounds": [
        {"type": "direct", "tag": "direct"},
        {"type": "tor", "tag": "tor", "executable_path": "C:\\Tools\\tor.exe",
         "extra_args": ["--ControlPort", "9051"], "data_directory": "C:\\ProgramData\\tor",
         "torrc": {"ClientOnly": "1"}}
    ]}"#;
    assert_eq!(
        refusals(config),
        [
            at("/outbounds/1/data_directory", RefusalKind::FilesystemPath),
            at("/outbounds/1/executable_path", RefusalKind::RunsProgram),
            at("/outbounds/1/type", RefusalKind::RunsProgram),
        ]
    );
    // Even with nothing to configure, it starts tor.
    assert_eq!(
        refusals(r#"{"outbounds": [{"type": "tor", "tag": "tor"}]}"#),
        [at("/outbounds/0/type", RefusalKind::RunsProgram)]
    );
}

/// An `executable_path` runs a program wherever it turns up.
#[test]
fn executable_path_runs_a_program_anywhere() {
    let config = r#"{
        "dns": {"servers": [{"type": "udp", "tag": "u", "server": "1.1.1.1", "executable_path": "/bin/sh"}]},
        "outbounds": [{"type": "direct", "tag": "d", "executable_path": "/bin/sh"}]
    }"#;
    assert_eq!(
        refusals(config),
        [
            at("/dns/servers/0/executable_path", RefusalKind::RunsProgram),
            at("/outbounds/0/executable_path", RefusalKind::RunsProgram),
        ]
    );
}

#[test]
fn openconnect_wrappers_run_programs() {
    let config = r#"{"endpoints": [{
        "type": "openconnect", "tag": "vpn", "server": "vpn.corp.example", "flavor": "gp",
        "csd": {"wrapper_path": "C:\\Users\\Public\\csd-wrapper.exe"},
        "hip": {"wrapper_path": "/usr/libexec/openconnect/hipreport.sh"},
        "tncc": {"wrapper_path": "/usr/libexec/openconnect/tncc-wrapper.py"}
    }]}"#;
    assert_eq!(
        refusals(config),
        [
            at("/endpoints/0/csd/wrapper_path", RefusalKind::RunsProgram),
            at("/endpoints/0/hip/wrapper_path", RefusalKind::RunsProgram),
            at("/endpoints/0/tncc/wrapper_path", RefusalKind::RunsProgram),
        ]
    );
}

/// The AnyConnect flavor, the default, answers the server's host scan by
/// statting the files it names; the other flavors pass.
#[test]
fn anyconnect_host_scan_is_refused() {
    assert_eq!(
        refusals(
            r#"{"endpoints": [{"type": "openconnect", "tag": "vpn", "server": "vpn.corp.example"}]}"#
        ),
        [at("/endpoints/0/type", RefusalKind::ServerFileScan)]
    );
    assert_eq!(
        refusals(
            r#"{"endpoints": [{"type": "openconnect", "tag": "vpn", "server": "vpn.corp.example", "flavor": "anyconnect"}]}"#
        ),
        [at("/endpoints/0/flavor", RefusalKind::ServerFileScan)]
    );
    for flavor in ["gp", "fortinet", "f5", "pulse", "nc"] {
        let config = format!(
            r#"{{"endpoints": [{{"type": "openconnect", "tag": "vpn", "server": "vpn.corp.example",
                "flavor": "{flavor}", "username": "me", "password": "secret"}}]}}"#
        );
        passes(&config);
    }
}

#[test]
fn ntp_write_to_system_changes_the_system() {
    let config =
        r#"{"ntp": {"enabled": true, "server": "time.apple.com", "write_to_system": true}}"#;
    assert_eq!(
        refusals(config),
        [at("/ntp/write_to_system", RefusalKind::SystemChange)]
    );
    // Anything but a plain `false` is refused, not interpreted.
    let config =
        r#"{"ntp": {"enabled": true, "server": "time.apple.com", "write_to_system": "true"}}"#;
    assert_eq!(
        refusals(config),
        [at("/ntp/write_to_system", RefusalKind::SystemChange)]
    );
    passes(r#"{"ntp": {"enabled": true, "server": "time.apple.com", "write_to_system": false}}"#);
}

#[test]
fn tailscale_ssh_server_changes_the_system() {
    let endpoint = |ssh_server: &str| {
        format!(
            r#"{{"endpoints": [{{"type": "tailscale", "tag": "ts", "auth_key": "tskey-auth-k1", "ssh_server": {ssh_server}}}]}}"#
        )
    };
    for on in ["true", r#"{"enabled": true, "disable_sftp": true}"#] {
        assert_eq!(
            refusals(&endpoint(on)),
            [at("/endpoints/0/ssh_server", RefusalKind::SystemChange)],
            "{on}"
        );
    }
    for off in ["false", r#"{"enabled": false, "disable_pty": true}"#] {
        passes(&endpoint(off));
    }
}

/// `api` services are dropped; every other service is refused by its type.
#[test]
fn services_other_than_api_are_refused() {
    let config = r#"{"services": [
        {"type": "derp", "tag": "derp", "listen_port": 3478, "config_path": "derper.key"},
        {"type": "api", "tag": "dash", "listen": "0.0.0.0", "listen_port": 9090},
        {"type": "usbip-server", "tag": "usb"},
        {"type": "ssm-api", "tag": "ssm", "servers": {"/": "ss-in"}, "cache_path": "ssm.json"},
        {"type": "resolved", "tag": "resolved"},
        {"type": "ccm", "tag": "ccm", "credential_path": "~/.claude/.credentials.json"},
        {"tag": "untyped"}
    ]}"#;
    let service = |kind: &str| RefusalKind::Service {
        service_type: Some(kind.to_string()),
    };
    assert_eq!(
        refusals(config),
        [
            at("/services/0/type", service("derp")),
            at("/services/2/type", service("usbip-server")),
            at("/services/3/type", service("ssm-api")),
            at("/services/4/type", service("resolved")),
            at("/services/5/type", service("ccm")),
            at("/services/6", RefusalKind::Service { service_type: None }),
        ]
    );
}

/// Sections sing-box 1.14 has that the helper doesn't run, and ones it
/// doesn't have.
#[test]
fn unknown_sections_are_refused() {
    let config = r#"{
        "certificate_providers": [{"type": "acme", "tag": "le", "domain": ["a.example"], "data_directory": "acme"}],
        "http_clients": [{"tag": "c", "detour": "direct"}],
        "network_namespaces": [{"type": "unshare", "tag": "ns", "pid_file": "/run/ns.pid"}],
        "outbounds": [{"type": "direct", "tag": "direct"}],
        "workspace": {}
    }"#;
    assert_eq!(
        refusals(config),
        [
            at("/certificate_providers", RefusalKind::UnknownSection),
            at("/http_clients", RefusalKind::UnknownSection),
            at("/network_namespaces", RefusalKind::UnknownSection),
            at("/workspace", RefusalKind::UnknownSection),
        ]
    );
}

#[test]
fn inbounds_are_refused() {
    let config = r#"{
        "inbounds": [{"type": "mixed", "tag": "mixed-in", "listen": "0.0.0.0", "listen_port": 7890}],
        "outbounds": [{"type": "direct", "tag": "direct"}]
    }"#;
    assert_eq!(refusals(config), [at("/inbounds", RefusalKind::Inbounds)]);
}

#[test]
fn unknown_experimental_keys_are_refused() {
    let config = r#"{"experimental": {
        "cache_file": {"enabled": true},
        "debug": {"listen": "0.0.0.0:6060"},
        "telemetry": {}
    }}"#;
    assert_eq!(
        refusals(config),
        [
            at("/experimental/debug", RefusalKind::UnknownExperimental),
            at("/experimental/telemetry", RefusalKind::UnknownExperimental),
        ]
    );
    // `cache_file` passes as an object only: as a string it is a path.
    assert_eq!(
        refusals(r#"{"experimental": {"cache_file": "C:\\Windows\\System32\\config\\SAM"}}"#),
        [at("/experimental/cache_file", RefusalKind::FilesystemPath)]
    );
}

/// Locations sing-box would write to (or open) that the helper doesn't own
/// are refused wherever they stand.
#[test]
fn write_locations_the_helper_doesnt_own_are_refused() {
    let config = r#"{
        "outbounds": [
            {"type": "trojan", "tag": "t", "server": "a.example", "server_port": 443, "password": "p",
             "tls": {"enabled": true, "acme": {"domain": ["a.example"], "data_directory": "C:\\ProgramData\\acme"}}},
            {"type": "direct", "tag": "d", "netns": "/proc/1/ns/net", "protect_path": "/data/protect.sock"}
        ],
        "endpoints": [
            {"type": "wireguard", "tag": "wg", "address": ["10.0.0.2/32"],
             "private_key": "YNXtAzepDqRv9H52osJVDQnznT5AL11eVZ7+JmZ9b2I=",
             "state_directory": "C:\\Windows\\Temp\\wg"}
        ],
        "route": {
            "geoip": {"path": "geoip.db"},
            "dhcp_lease_files": ["/var/lib/misc/dnsmasq.leases"]
        }
    }"#;
    assert_eq!(
        refusals(config),
        [
            at("/endpoints/0/state_directory", RefusalKind::FilesystemPath),
            at(
                "/outbounds/0/tls/acme/data_directory",
                RefusalKind::FilesystemPath
            ),
            at("/outbounds/1/netns", RefusalKind::FilesystemPath),
            at("/outbounds/1/protect_path", RefusalKind::FilesystemPath),
            at("/route/dhcp_lease_files", RefusalKind::FilesystemPath),
            at("/route/geoip/path", RefusalKind::FilesystemPath),
        ]
    );
    // An empty `netns` names no namespace.
    passes(r#"{"outbounds": [{"type": "direct", "tag": "d", "netns": ""}]}"#);
}

#[test]
fn certificate_directories_are_refused() {
    let config = r#"{"certificate": {"store": "mozilla", "certificate_directory_path": ["/etc/ssl/certs"]}}"#;
    assert_eq!(
        refusals(config),
        [at(
            "/certificate/certificate_directory_path",
            RefusalKind::Directory
        )]
    );
}

/// v2ray-plugin reads `cert=` from `plugin_opts` as a certificate file,
/// escaped or not; other options pass.
#[test]
fn shadowsocks_plugin_cert_is_a_file() {
    let outbound = |opts: &str| {
        let config = json!({"outbounds": [{
            "type": "shadowsocks", "tag": "ss", "server": "a.example", "server_port": 8388,
            "method": "aes-128-gcm", "password": "p",
            "plugin": "v2ray-plugin", "plugin_opts": opts
        }]});
        config.to_string()
    };
    for opts in [
        "tls;host=a.example;cert=/etc/shadow",
        r"tls;c\ert=C:\\Windows\\win.ini",
    ] {
        assert_eq!(
            refusals(&outbound(opts)),
            [at("/outbounds/0/plugin_opts", RefusalKind::FilesystemPath)],
            "{opts}"
        );
    }
    assert_eq!(
        refusals(&outbound(r"host=a.example\")),
        [at(
            "/outbounds/0/plugin_opts",
            RefusalKind::Malformed {
                expected: Expected::PluginOptions
            }
        )]
    );
    passes(&outbound("tls;host=a.example;path=/ws;mux=4"));
    passes(&outbound(r"tls;certRaw=MIIB;host=a\;cert=x"));
}

// ---- Fail closed ----

/// A path-shaped key the policy doesn't know is refused, and so is a
/// `path` where sing-box has none, or has a file.
#[test]
fn unknown_path_shaped_keys_fail_closed() {
    let config = r#"{
        "dns": {"servers": [{"type": "udp", "tag": "u", "server": "1.1.1.1", "path": "/dns-query"}]},
        "outbounds": [
            {"type": "vless", "tag": "v", "server": "a.example", "server_port": 443,
             "uuid": "bf000d23-0752-40b4-affe-68f7707a9661",
             "future_thing_path": "C:\\x", "path": "/ws",
             "transport": {"type": "grpc", "service_name": "g", "path": "/grpc"}},
            {"type": "socks", "tag": "s", "server": "b.example", "server_port": 1080,
             "udp_over_tcp": {"enabled": true, "log_directories": ["/var/log"]}}
        ],
        "route": {"rules": [{"path": "/etc", "outbound": "v"}]}
    }"#;
    assert_eq!(
        refusals(config),
        [
            at("/dns/servers/0/path", RefusalKind::FilesystemPath),
            at(
                "/outbounds/0/future_thing_path",
                RefusalKind::FilesystemPath
            ),
            at("/outbounds/0/path", RefusalKind::FilesystemPath),
            at("/outbounds/0/transport/path", RefusalKind::FilesystemPath),
            at(
                "/outbounds/1/udp_over_tcp/log_directories",
                RefusalKind::FilesystemPath
            ),
            at("/route/rules/0/path", RefusalKind::FilesystemPath),
        ]
    );
}

/// sing-box reads field names case-insensitively (`Executable_Path`, `ſ` for
/// `s`), so anything not spelled in lower case is refused; map keys, which
/// are data, keep their case.
#[test]
fn keys_sing_box_would_read_under_another_spelling_are_refused() {
    let config = r#"{
        "NTP": {"enabled": true, "Write_To_System": true},
        "outbounds": [
            {"type": "direct", "tag": "d", "Executable_Path": "/bin/sh"},
            {"type": "trojan", "tag": "t", "server": "a.example", "server_port": 443, "password": "p",
             "tls": {"enabled": true, "cErtificate_path": "C:\\Windows\\System32\\config\\SAM"}},
            {"Type": "tor", "tag": "tor"}
        ],
        "endpoints": [{"type": "tailscale", "tag": "ts", "\u017ftate_directory": "C:\\Windows", "ssh_server": {"Enabled": true}}],
        "experimental": {"cache_file": {"enabled": true, "PATH": "C:\\Windows\\win.ini"}, "Clash_API": {}},
        "services": [{"type": "api", "TYPE": "derp"}]
    }"#;
    assert_eq!(
        refusals(config),
        [
            at("/NTP", RefusalKind::NonCanonicalKey),
            at(
                "/endpoints/0/ssh_server/Enabled",
                RefusalKind::NonCanonicalKey
            ),
            at(
                "/endpoints/0/\u{17F}tate_directory",
                RefusalKind::NonCanonicalKey
            ),
            at("/experimental/Clash_API", RefusalKind::NonCanonicalKey),
            at(
                "/experimental/cache_file/PATH",
                RefusalKind::NonCanonicalKey
            ),
            at("/outbounds/0/Executable_Path", RefusalKind::NonCanonicalKey),
            at(
                "/outbounds/1/tls/cErtificate_path",
                RefusalKind::NonCanonicalKey
            ),
            at("/outbounds/2/Type", RefusalKind::NonCanonicalKey),
            at("/services/0/TYPE", RefusalKind::NonCanonicalKey),
        ]
    );
    let maps = r#"{
        "dns": {"servers": [{"type": "hosts", "tag": "h", "predefined": {"Router.LAN": "192.168.1.1"}}]},
        "outbounds": [{"type": "vless", "tag": "v", "server": "a.example", "server_port": 443,
            "uuid": "bf000d23-0752-40b4-affe-68f7707a9661",
            "transport": {"type": "ws", "path": "/", "headers": {"Host": "a.example", "User-Agent": ["Mozilla/5.0"]}}}]
    }"#;
    passes(maps);
}

/// A map's values are strings or lists; an object there is not walked as
/// data.
#[test]
fn objects_inside_maps_are_refused() {
    let config = r#"{"outbounds": [{"type": "vless", "tag": "v", "server": "a.example", "server_port": 443,
        "uuid": "bf000d23-0752-40b4-affe-68f7707a9661",
        "transport": {"type": "ws", "headers": {"Host": {"key_path": "/etc/shadow"}}}}]}"#;
    assert_eq!(
        refusals(config),
        [at(
            "/outbounds/0/transport/headers/Host",
            RefusalKind::Malformed {
                expected: Expected::StringOrArray
            }
        )]
    );
}

/// Where the policy has to look inside, the wrong type is refused rather
/// than skipped.
#[test]
fn malformed_sections_are_refused() {
    let config = r#"{
        "dns": [],
        "outbounds": {"0": {"type": "tor"}},
        "endpoints": ["tailscale"],
        "services": {"type": "derp"},
        "experimental": "on"
    }"#;
    let malformed = |expected| RefusalKind::Malformed { expected };
    assert_eq!(
        refusals(config),
        [
            at("/dns", malformed(Expected::Object)),
            at("/endpoints/0", malformed(Expected::Object)),
            at("/experimental", malformed(Expected::Object)),
            at("/outbounds", malformed(Expected::Array)),
            at("/services", malformed(Expected::Array)),
        ]
    );
}

/// Every refusal is collected, not just the first.
#[test]
fn all_refusals_are_collected() {
    let config = r#"{
        "inbounds": [],
        "ntp": {"write_to_system": true},
        "outbounds": [{"type": "tor", "tag": "tor"}],
        "certificate": {"certificate_directory_path": "/etc/ssl/certs"},
        "experimental": {"debug": {}}
    }"#;
    assert_eq!(
        refusals(config),
        [
            at(
                "/certificate/certificate_directory_path",
                RefusalKind::Directory
            ),
            at("/experimental/debug", RefusalKind::UnknownExperimental),
            at("/inbounds", RefusalKind::Inbounds),
            at("/ntp/write_to_system", RefusalKind::SystemChange),
            at("/outbounds/0/type", RefusalKind::RunsProgram),
        ]
    );
}

// ---- Attachments ----

#[test]
fn attachment_reference_passes() {
    let config = json!({"outbounds": [{
        "type": "trojan", "tag": "t", "server": "a.example", "server_port": 443, "password": "p",
        "tls": {"enabled": true, "certificate_path": reference("corp-ca")}
    }]})
    .to_string();
    let checked = passes_with(&config, &["corp-ca", "unused"]);
    assert_eq!(checked.attachment_ids(), BTreeSet::from(["corp-ca"]));
    assert!(checked.dropped().is_empty());
    assert_eq!(*checked.config(), parse(&config));
}

#[test]
fn reference_to_a_missing_attachment_is_refused() {
    let config = json!({"route": {"rule_set": [
        {"type": "local", "tag": "corp", "format": "binary", "path": reference("rules")}
    ]}})
    .to_string();
    assert_eq!(
        refusals_with(&config, &["other"]),
        [at(
            "/route/rule_set/0/path",
            RefusalKind::MissingAttachment { id: "rules".into() }
        )]
    );
}

#[test]
fn malformed_attachment_ids_are_refused() {
    let too_long = "a".repeat(65);
    for id in ["", "../ca", "a/b", "ca.pem", "c a", "ü", too_long.as_str()] {
        assert!(!is_attachment_id(id), "{id}");
        let config = json!({"outbounds": [{
            "type": "ssh", "tag": "s", "server": "a.example", "server_port": 22,
            "private_key_path": reference(id)
        }]})
        .to_string();
        assert_eq!(
            refusals_with(&config, &[id]),
            [at(
                "/outbounds/0/private_key_path",
                RefusalKind::MalformedAttachment
            )],
            "{id}"
        );
    }
    for id in ["a", "corp-ca_2", &"Z9".repeat(32)] {
        assert!(is_attachment_id(id), "{id}");
    }
}

#[test]
fn local_paths_in_read_fields_are_refused_until_attached() {
    let config = r#"{"outbounds": [{"type": "ssh", "tag": "s", "server": "a.example", "server_port": 22,
        "private_key_path": "C:\\Users\\me\\.ssh\\id_ed25519"}]}"#;
    assert_eq!(
        refusals(config),
        [at("/outbounds/0/private_key_path", RefusalKind::LocalFile)]
    );
}

/// Lists of references where sing-box takes a list of paths (the top-level
/// certificates, hosts files); a single-path field takes no list.
#[test]
fn reference_lists_where_sing_box_takes_lists() {
    let config = json!({
        "certificate": {"certificate_path": [reference("root"), reference("issuing")]},
        "dns": {"servers": [{"type": "hosts", "tag": "h", "path": [reference("hosts")]}]}
    })
    .to_string();
    let checked = passes_with(&config, &["root", "issuing", "hosts"]);
    assert_eq!(
        checked.attachment_ids(),
        BTreeSet::from(["hosts", "issuing", "root"])
    );
    // A single string works for a list field too (sing-box's `Listable`).
    passes_with(
        &json!({"certificate": {"certificate_path": reference("root")}}).to_string(),
        &["root"],
    );

    let config = json!({
        "certificate": {"certificate_path": [reference("root"), 7, "/etc/ssl/extra.pem"]},
        "outbounds": [{"type": "trojan", "tag": "t", "server": "a.example", "server_port": 443,
                       "password": "p", "tls": {"certificate_path": [reference("root")]}}]
    })
    .to_string();
    assert_eq!(
        refusals_with(&config, &["root"]),
        [
            at(
                "/certificate/certificate_path/1",
                RefusalKind::Malformed {
                    expected: Expected::String
                }
            ),
            at("/certificate/certificate_path/2", RefusalKind::LocalFile),
            at(
                "/outbounds/0/tls/certificate_path",
                RefusalKind::Malformed {
                    expected: Expected::String
                }
            ),
        ]
    );
}

/// An empty or null path names no file: it passes, and the GUI has nothing
/// to attach.
#[test]
fn empty_read_fields_name_no_file() {
    let config = r#"{"outbounds": [{"type": "trojan", "tag": "t", "server": "a.example", "server_port": 443,
        "password": "p", "tls": {"certificate_path": "", "client_key_path": null}}],
        "certificate": {"certificate_path": []}}"#;
    assert_eq!(*passes(config).config(), parse(config));
    assert!(local_file_fields(&parse(config)).is_empty());
}

/// Every file-read field the policy knows, with its pointer.
#[test]
fn local_file_fields_lists_every_file_the_config_reads() {
    let mut fields = local_file_fields(&parse(LOCAL_FILES));
    fields.sort_by(|a, b| a.pointer.cmp(&b.pointer));
    let field = |pointer: &str, path: &str| LocalFileField {
        pointer: pointer.into(),
        path: path.into(),
    };
    assert_eq!(
        fields,
        [
            field("/certificate/certificate_path/0", r"C:\certs\corp-root.pem"),
            field(
                "/certificate/certificate_path/1",
                r"C:\certs\corp-issuing.pem"
            ),
            field("/dns/servers/0/path/0", r"C:\Users\me\corp.hosts"),
            field(
                "/dns/servers/1/tls/certificate_path",
                r"C:\certs\dns-ca.pem"
            ),
            field(
                "/endpoints/0/tls/certificate_path",
                r"C:\certs\office-ca.crt"
            ),
            field(
                "/endpoints/0/tls/client_certificate_path",
                r"C:\certs\office-me.crt"
            ),
            field(
                "/endpoints/0/tls/client_key_path",
                r"C:\certs\office-me.key"
            ),
            field(
                "/endpoints/0/tls/control_wrap/key_path",
                r"C:\certs\office-tc.key"
            ),
            field("/endpoints/0/tls/crl_path", r"C:\certs\office.crl"),
            field(
                "/endpoints/1/static_key_path",
                r"C:\certs\legacy-static.key"
            ),
            field(
                "/endpoints/2/tls/certificate_path",
                r"C:\certs\home-server.crt"
            ),
            field(
                "/endpoints/2/tls/client_certificate_path",
                r"C:\certs\home-clients-ca.crt"
            ),
            field("/endpoints/2/tls/key_path", r"C:\certs\home-server.key"),
            field(
                "/endpoints/3/tls/certificate_authority_path",
                r"C:\certs\lab-ca.pem"
            ),
            field(
                "/endpoints/3/tls/client_certificate_path",
                r"C:\certs\lab-me.pem"
            ),
            field("/endpoints/3/tls/client_key_path", r"C:\certs\lab-me.key"),
            field(
                "/endpoints/3/tls/mca_certificate_path",
                r"C:\certs\lab-machine.pem"
            ),
            field("/endpoints/3/tls/mca_key_path", r"C:\certs\lab-machine.key"),
            field("/endpoints/3/token/secret_path", r"C:\Users\me\lab.totp"),
            field(
                "/endpoints/4/tncc/certificates/0/certificate_path",
                r"C:\certs\pulse-device.pem"
            ),
            field(
                "/outbounds/0/tls/certificate_path",
                r"C:\certs\trojan-ca.pem"
            ),
            field(
                "/outbounds/0/tls/client_certificate_path",
                r"C:\certs\me.crt"
            ),
            field("/outbounds/0/tls/client_key_path", r"C:\certs\me.key"),
            field(
                "/outbounds/0/tls/ech/config_path",
                r"C:\certs\ech-config.pem"
            ),
            field(
                "/outbounds/1/private_key_path",
                r"C:\Users\me\.ssh\id_ed25519"
            ),
            field(
                "/route/rule_set/0/path",
                r"C:\Users\me\rules\corp-direct.srs"
            ),
            field(
                "/route/rule_set/1/http_client/tls/certificate_path",
                r"C:\certs\rules-ca.pem"
            ),
            field(
                "/route/rule_set/1/initial_path",
                r"C:\Users\me\rules\ads.srs"
            ),
        ]
    );
    // Before attaching, each one is a refusal, and nothing else is.
    let refused = refusals(LOCAL_FILES);
    assert_eq!(refused.len(), fields.len());
    for ((pointer, kind), field) in refused.iter().zip(&fields) {
        assert_eq!((pointer, kind), (&field.pointer, &RefusalKind::LocalFile));
    }
}

/// The GUI's side: list the local files, attach each, and the config
/// passes with those attachments; the helper's side then points each
/// field at the file it wrote.
#[test]
fn attach_round_trip() {
    let mut config = parse(LOCAL_FILES);
    let fields = local_file_fields(&config);
    let mut attached = Vec::new();
    for (i, field) in fields.iter().enumerate() {
        let id = format!("file-{i}");
        attach(&mut config, &field.pointer, &id).unwrap();
        assert_eq!(
            config.pointer(&field.pointer).unwrap(),
            &json!(reference(&id))
        );
        attached.push(id);
    }
    assert!(local_file_fields(&config).is_empty());

    let ids: Vec<&str> = attached.iter().map(String::as_str).collect();
    let checked = passes_with(&config.to_string(), &ids);
    assert_eq!(checked.attachment_ids(), ids.iter().copied().collect());
    let placement = placement();
    let materialized = materialize(checked, &placement);
    for (field, id) in fields.iter().zip(&attached) {
        assert_eq!(
            materialized.pointer(&field.pointer).unwrap(),
            &json!(placement.attachment_path(id)),
            "{}",
            field.pointer
        );
    }
    // Nothing of the user's paths is left for the privileged side to open.
    assert!(!materialized.to_string().contains(r"C:\\certs"));
    assert!(!materialized.to_string().contains(r"C:\\Users"));
}

#[test]
fn attach_changes_only_local_files() {
    let original = parse(SUBSCRIPTION);
    let mut config = original.clone();
    for pointer in [
        "/outbounds/2/transport/path",
        "/dns/servers/0/path",
        "/route/rules/5/process_path/0",
        "/outbounds/9/tls/certificate_path",
        "",
    ] {
        assert_eq!(
            attach(&mut config, pointer, "corp-ca"),
            Err(AttachError::NotALocalFile),
            "{pointer}"
        );
    }
    let mut local = parse(LOCAL_FILES);
    assert_eq!(
        attach(&mut local, "/outbounds/1/private_key_path", "../id"),
        Err(AttachError::MalformedId)
    );
    attach(&mut local, "/outbounds/1/private_key_path", "ssh-key").unwrap();
    // Once attached it is no longer a local file.
    assert_eq!(
        attach(&mut local, "/outbounds/1/private_key_path", "other"),
        Err(AttachError::NotALocalFile)
    );
    assert_eq!(config, original);
}

/// A value built in code can nest deeper than any parsed text; the GUI-side
/// walk stops at serde_json's depth rather than overflow the stack.
#[test]
fn deep_values_built_in_code_stop_the_walk() {
    let mut deep = json!({"process_path": "/usr/bin/curl"});
    for _ in 0..200 {
        deep = json!({"type": "logical", "mode": "and", "rules": [deep]});
    }
    let mut config = json!({"route": {"rules": [deep]}});
    assert!(local_file_fields(&config).is_empty());
    assert_eq!(
        attach(&mut config, "/route/rules/0/process_path", "x"),
        Err(AttachError::NotALocalFile)
    );
}

// ---- What the helper owns ----

/// After `materialize`, everything the helper owns is the helper's: the
/// profile's own log file, cache path and Tailscale directories are gone,
/// and each Tailscale endpoint is placed by the tag sing-box runs it under.
#[test]
fn materialize_places_everything_the_helper_owns() {
    let config = json!({
        "log": {"level": "info", "output": "/etc/cron.d/boxpilot"},
        "certificate": {"certificate_path": [reference("root")]},
        "endpoints": [
            {"type": "wireguard", "tag": "wg", "address": ["10.0.0.2/32"],
             "private_key": "YNXtAzepDqRv9H52osJVDQnznT5AL11eVZ7+JmZ9b2I="},
            {"type": "tailscale", "auth_key": "tskey-auth-k2",
             "state_directory": "/Users/victim/Library/ts", "taildrop_directory": "/Users/victim/Desktop"},
            {"type": "tailscale", "tag": "ts-home", "auth_key": "tskey-auth-k1",
             "state_directory": "/Users/victim/Library/ts-home"}
        ],
        "experimental": {"cache_file": {"enabled": true, "path": "/Users/victim/.ssh/authorized_keys"}}
    })
    .to_string();
    let checked = passes_with(&config, &["root"]);
    assert_eq!(
        checked.dropped(),
        [
            dropped("/log/output", DropReason::HelperOwned),
            dropped("/endpoints/1/state_directory", DropReason::HelperOwned),
            dropped("/endpoints/1/taildrop_directory", DropReason::HelperOwned),
            dropped("/endpoints/2/state_directory", DropReason::HelperOwned),
            dropped("/experimental/cache_file/path", DropReason::HelperOwned),
        ]
    );
    let placement = placement();
    let config = materialize(checked, &placement);
    assert_eq!(config["log"], json!({"level": "info"}));
    assert_eq!(
        config["certificate"]["certificate_path"],
        json!([placement.attachment_path("root")])
    );
    assert_eq!(config["endpoints"][0].get("state_directory"), None);
    for (i, tag) in [(1, "1"), (2, "ts-home")] {
        let endpoint = &config["endpoints"][i];
        assert_eq!(
            endpoint["state_directory"],
            json!(placement.tailscale_state_dir(tag))
        );
        assert_eq!(
            endpoint["taildrop_directory"],
            json!(placement.tailscale_taildrop_dir(tag))
        );
    }
    assert_eq!(
        config["experimental"]["cache_file"],
        json!({"enabled": true, "path": placement.cache_file})
    );
    assert!(!config.to_string().contains("victim"));
    assert!(!config.to_string().contains("cron"));
}

/// A config without `experimental` still gets the helper's cache path, so
/// enabling the cache later can't land anywhere else.
#[test]
fn materialize_sets_the_cache_path_even_without_a_cache_file() {
    let config = materialize(
        passes(r#"{"outbounds": [{"type": "direct", "tag": "direct"}]}"#),
        &placement(),
    );
    assert_eq!(
        config["experimental"],
        json!({"cache_file": {"path": placement().cache_file}})
    );
}

/// Names under the helper's directories derive from ids and tags, hex
/// encoded: no separator, `..` or device name gets through, and ids that
/// differ only in case stay apart.
#[test]
fn placement_derives_names_from_ids_and_tags() {
    let unix = placement();
    assert_eq!(
        unix.attachment_path("CON"),
        "/var/run/boxpilot/run-7/attachment-434f4e"
    );
    assert_ne!(unix.attachment_path("ca"), unix.attachment_path("CA"));
    assert_eq!(
        unix.tailscale_state_dir("../../etc"),
        "/Library/Application Support/BoxPilot Helper/501/tailscale/endpoint-2e2e2f2e2e2f657463/state"
    );
    let windows = Placement {
        run_dir: r"C:\ProgramData\BoxPilot\Helper\runs\7\".into(),
        cache_file: r"C:\ProgramData\BoxPilot\Helper\S-1-5-21-1\cache.db".into(),
        tailscale_dir: r"C:\ProgramData\BoxPilot\Helper\S-1-5-21-1\tailscale".into(),
        separator: '\\',
    };
    assert_eq!(
        windows.attachment_path("ca"),
        r"C:\ProgramData\BoxPilot\Helper\runs\7\attachment-6361"
    );
    assert_eq!(
        windows.tailscale_taildrop_dir(""),
        r"C:\ProgramData\BoxPilot\Helper\S-1-5-21-1\tailscale\endpoint-\taildrop"
    );
}

// ---- Limits ----

/// The size limit holds before parsing: oversize text is refused as such,
/// however broken.
#[test]
fn oversize_text_is_refused_before_parsing() {
    let limits = Limits {
        max_bytes: 16,
        ..Limits::default()
    };
    let refusals = check("{ this is not JSON at all", &BTreeSet::new(), &limits).unwrap_err();
    assert_eq!(
        refusals,
        [Refusal {
            pointer: String::new(),
            kind: RefusalKind::TooLarge {
                bytes: 25,
                limit: 16
            }
        }]
    );
    assert!(check(r#"{"log":{}}"#, &BTreeSet::new(), &limits).is_ok());
    assert_eq!(Limits::default().max_bytes, 32 * 1024 * 1024);
}

fn nested(depth: usize) -> String {
    // `route` is the first level; each `{"x": …}` adds one.
    format!(
        r#"{{"route": {}{}}}"#,
        r#"{"x": "#.repeat(depth - 2) + "{}",
        "}".repeat(depth - 2)
    )
}

/// Depth is held to `max_depth` before parsing, beyond serde_json's own
/// 128 too, and an unbalanced deep text is still "too deep".
#[test]
fn deep_nesting_is_refused_before_parsing() {
    let limits = Limits::default();
    assert!(check(&nested(64), &BTreeSet::new(), &limits).is_ok());
    for text in [nested(65), nested(300), "[".repeat(10_000)] {
        assert_eq!(
            check(&text, &BTreeSet::new(), &limits).unwrap_err(),
            [Refusal {
                pointer: String::new(),
                kind: RefusalKind::TooDeep { limit: 64 }
            }]
        );
    }
    // Brackets inside strings don't nest.
    let brackets = format!(r#"{{"route": {{"final": "{}"}}}}"#, "[{".repeat(500));
    assert!(check(&brackets, &BTreeSet::new(), &limits).is_ok());
}

#[test]
fn non_object_root_is_refused() {
    for text in ["[]", r#""sing-box""#, "null", "42"] {
        assert_eq!(
            check(text, &BTreeSet::new(), &Limits::default()).unwrap_err(),
            [Refusal {
                pointer: String::new(),
                kind: RefusalKind::NotAnObject
            }],
            "{text}"
        );
    }
}

/// Including the JSONC sing-box itself would accept: the policy only
/// passes what serde_json reads the same way.
#[test]
fn invalid_json_is_refused() {
    for text in [
        "",
        "{",
        r#"{"log": {}} trailing"#,
        "{\"outbounds\": [] // comment\n}",
        r#"{"outbounds": [{"type": "direct", "tag": "d",}]}"#,
    ] {
        let refusals = check(text, &BTreeSet::new(), &Limits::default()).unwrap_err();
        assert_eq!(refusals.len(), 1, "{text}");
        assert_eq!(refusals[0].pointer, "");
        assert!(
            matches!(refusals[0].kind, RefusalKind::InvalidJson(_)),
            "{text}: {:?}",
            refusals[0].kind
        );
    }
}

// ---- Pointers and messages ----

#[test]
fn pointers_escape_tilde_and_slash() {
    let config =
        r#"{"a/b~c": 1, "outbounds": [{"type": "direct", "tag": "d", "x/y~z_path": "/"}]}"#;
    assert_eq!(
        refusals(config),
        [
            at("/a~1b~0c", RefusalKind::UnknownSection),
            at("/outbounds/0/x~1y~0z_path", RefusalKind::FilesystemPath),
        ]
    );
}

#[test]
fn refusals_read_as_plain_english() {
    let refusal = |pointer: &str, kind| Refusal {
        pointer: pointer.into(),
        kind,
    };
    assert_eq!(
        refusal("/outbounds/0/executable_path", RefusalKind::RunsProgram).to_string(),
        "/outbounds/0/executable_path runs a program"
    );
    assert_eq!(
        refusal("", RefusalKind::NotAnObject).to_string(),
        "the config is not a JSON object"
    );
    assert_eq!(
        refusal(
            "/services/0/type",
            RefusalKind::Service {
                service_type: Some("derp".into())
            }
        )
        .to_string(),
        "/services/0/type runs a `derp` service, which the privileged path doesn't allow"
    );
    assert_eq!(
        refusal(
            "/route/rule_set/0/path",
            RefusalKind::MissingAttachment { id: "rules".into() }
        )
        .to_string(),
        "/route/rule_set/0/path refers to attachment `rules`, which the request doesn't carry"
    );
    assert_eq!(
        dropped("/experimental/clash_api", DropReason::ControlPlane).to_string(),
        "/experimental/clash_api dropped: a control plane doesn't run on the privileged path"
    );
}
