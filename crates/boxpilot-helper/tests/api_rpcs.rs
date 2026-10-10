//! The RPCs of sing-box's `api` service, as audited (ADR 0006, "Audited at
//! every `SINGBOX_VERSION` bump").
//!
//! The helper's sing-box runs as SYSTEM with BoxPilot's own `api` service,
//! and the GUI that started it gets the service's secret: every RPC of
//! `daemon.StartedService` then runs as SYSTEM for that caller. Each one is
//! listed here with what it touches, as audited against sing-box 1.14.2's
//! `daemon/started_service*.go` under the helper's config policy. A new RPC
//! must be audited before it runs as SYSTEM, so the Windows release job
//! fetches `daemon/started_service.proto` for the `SINGBOX_VERSION` it
//! bundles and runs the ignored test below on it, with
//! `BOXPILOT_SINGBOX_PROTO` naming the file: an RPC added or removed
//! upstream fails the release until this list (and the audit) catch up.

use std::collections::BTreeSet;
use std::env;
use std::fs;

/// `daemon.StartedService`'s RPCs, as audited for sing-box 1.14.2. None
/// runs or controls a process, changes a system setting, or takes a host
/// path. Those beyond "bring TUN up or down" are marked, with why they lend
/// nothing a caller shouldn't have.
const AUDITED_RPCS: &[&str] = &[
    // Read in-memory state only.
    "GetVersion",
    "SubscribeServiceStatus",
    "GetDefaultLogLevel",
    "ClearLogs",
    "SubscribeStatus",
    "SubscribeGroups",
    "GetClashModeStatus",
    "SubscribeClashMode",
    "GetDeprecatedWarnings",
    "GetStartedAt",
    "SubscribeOutbounds",
    "SubscribeNotifications",
    // Routing, persisted in the caller's own users\<SID>\cache.db.
    "SetClashMode",
    "SelectOutbound",
    "SetGroupExpand",
    // Network only.
    "URLTest",
    "CloseConnection",
    "CloseAllConnections",
    // Network metadata: every connection through the machine-wide TUN,
    // other accounts' included, with process path and PID. Inherent in
    // controlling machine-wide networking.
    "SubscribeLog",
    "SubscribeConnections",
    // Network, to a caller-chosen URL or STUN server through any outbound
    // (from SYSTEM with `direct`); only metrics come back.
    "StartNetworkQualityTest",
    "StartSTUNTest",
    // Tailscale, on the caller's own node, whose state is in its own
    // users\<SID>\tailscale.
    "SubscribeTailscaleStatus",
    "StartTailscalePing",
    "SetTailscaleExitNode",
    "TailscaleLogout",
    // Returns the node's TLS private key: the caller's own node's secret.
    "GetTailscaleCertificate",
    // An SSH *client* to a tailnet peer, as this node. Agent forwarding
    // needs a platform handler, which `sing-box run` doesn't have. The
    // policy refuses the Tailscale SSH *server*.
    "StartTailscaleSSHSession",
    // Taildrop, in the endpoint's own taildrop directory. Sending streams
    // the client's bytes (no local path is read); downloading and deleting
    // take a file name sing-box validates (a base name, `IsLocal`, at most
    // 255 bytes), so they stay in that directory.
    "SubscribeTaildropInbox",
    "MarkTaildropInboxRead",
    "SendTaildropFiles",
    "DownloadTaildropFile",
    "DeleteTaildropFile",
    "CancelTaildropReceiving",
    // USB/IP: needs a `usbip` service, and the policy refuses every
    // profile service, so these find none.
    "ProvideUSBDevices",
    "SubscribeUSBIPServerStatus",
    // OpenConnect and OpenVPN sign-in, on the caller's own VPN: network.
    "SubscribeOpenConnectStatus",
    "SubmitOpenConnectAuthResponse",
    "CancelOpenConnectAuthChallenge",
    "SubscribeOpenVPNStatus",
    "SubmitOpenVPNChallengeResponse",
    "CancelOpenVPNChallenge",
];

/// The names of the RPCs in `service StartedService { … }` of a `.proto`.
fn started_service_rpcs(proto: &str) -> BTreeSet<String> {
    let mut rpcs = BTreeSet::new();
    let mut inside = false;
    for line in proto.lines() {
        let line = line.trim();
        if !inside {
            inside = line.starts_with("service StartedService");
            continue;
        }
        if line.starts_with('}') {
            break;
        }
        if let Some(rest) = line.strip_prefix("rpc ") {
            let name: String = rest
                .trim_start()
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            assert!(!name.is_empty(), "an rpc line without a name: {line}");
            rpcs.insert(name);
        }
    }
    assert!(inside, "no `service StartedService` in the proto");
    rpcs
}

fn audited() -> BTreeSet<String> {
    AUDITED_RPCS.iter().map(|name| name.to_string()).collect()
}

#[test]
fn the_audited_list_has_every_rpc_once() {
    assert_eq!(AUDITED_RPCS.len(), 42);
    assert_eq!(
        audited().len(),
        AUDITED_RPCS.len(),
        "a name is listed twice"
    );
}

#[test]
fn the_proto_parser_reads_only_the_services_rpcs() {
    let proto = r#"
syntax = "proto3";
service StartedService {
  rpc GetVersion(google.protobuf.Empty) returns(Version) {}
  rpc   SendTaildropFiles(stream TaildropSendClientMessage) returns (stream X) {}
}
service ManagedService {
  rpc StartService(Empty) returns(Empty) {}
}
message Version { string version = 1; }
"#;
    assert_eq!(
        started_service_rpcs(proto),
        BTreeSet::from(["GetVersion".to_owned(), "SendTaildropFiles".to_owned()])
    );
}

#[test]
#[ignore = "checks a fetched proto; the Windows release job runs it with BOXPILOT_SINGBOX_PROTO set"]
fn the_bundled_sing_box_serves_no_unaudited_rpc() {
    let path = env::var_os("BOXPILOT_SINGBOX_PROTO")
        .expect("BOXPILOT_SINGBOX_PROTO names sing-box's daemon/started_service.proto");
    let proto = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path:?}: {e}"));
    let served = started_service_rpcs(&proto);
    let audited = audited();
    let new: Vec<&String> = served.difference(&audited).collect();
    let gone: Vec<&String> = audited.difference(&served).collect();
    assert!(
        new.is_empty() && gone.is_empty(),
        "sing-box's api service changed: new RPCs {new:?}, removed {gone:?}. \
         A new RPC must be audited before it runs as SYSTEM (ADR 0006): \
         update AUDITED_RPCS in crates/boxpilot-helper/tests/api_rpcs.rs \
         with what it touches"
    );
}
