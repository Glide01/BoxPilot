//! The parts of a sing-box run config that BoxPilot writes itself (ADR 0006,
//! rule 1): the inbounds, `experimental.cache_file.enabled`, and BoxPilot's
//! own `api` service.
//!
//! Two callers build a run config, and both must build the same one:
//!
//! - the GUI, for the sing-box it starts at the user's own privilege
//!   (`prepare_config`), on the profile as written (ADR 0002);
//! - the privileged helper, for the sing-box it starts as SYSTEM, on the
//!   config `boxpilot_policy` checked and materialized.
//!
//! So the injection lives here, pure (no I/O, no gpui): JSON in, JSON out.
//! Picking the `api` service's port needs the OS, so [`pick_port_avoiding`]
//! takes the source of candidate ports from its caller.
//!
//! What is injected comes from typed options ([`Inject`]), never from the
//! profile: a profile's own `inbounds` are replaced, and its own services
//! stay beside BoxPilot's rather than under it.
//!
//! The privileged helper adds one thing the GUI doesn't:
//! [`reject_loopback`], a first route rule that keeps a SYSTEM sing-box
//! from connecting to this machine's loopback on anyone's behalf.
//!
//! [`system_proxy`] is how both recognize, on macOS, the system proxy a
//! sing-box with `set_system_proxy` left behind, to reset only that one.

#![forbid(unsafe_code)]

pub mod system_proxy;

use serde_json::{Map, Value};
use std::fmt;
use std::io;

/// The TUN interface's IPv4 address, always present in TUN mode. The GUI's
/// LAN hint leaves this /30 out of the addresses it lists.
pub const TUN_IPV4_ADDRESS: &str = "172.18.0.1/30";

/// Added to the TUN interface only when [`Inject::tun_ipv6`] is on.
pub const TUN_IPV6_ADDRESS: &str = "fdfe:dcba:9876::1/126";

/// The TUN inbound's `strict_route`. Off on Linux: there sing-box enforces
/// it with its own nftables / ip rules, which cut off whatever isn't routed
/// through the tunnel (Docker and libvirt bridges, other VPNs, hosts on the
/// LAN). Elsewhere it keeps traffic from leaking around the tunnel.
pub const TUN_STRICT_ROUTE: bool = !cfg!(target_os = "linux");

/// The `services[]` tag of BoxPilot's `api` service. Service tags share one
/// namespace, so [`inject`] adds `-2`, `-3`, … when the config's own
/// services already use it.
pub const API_SERVICE_TAG: &str = "boxpilot-api";

/// The one origin the `api` service's CORS allows. `.invalid` can never
/// resolve (RFC 6761), so no page can be served from it, unlike `null`,
/// which sandboxed iframes and `file:` pages send. BoxPilot itself sends no
/// `Origin` at all, which the CORS layer lets through. Also how BoxPilot
/// recognizes its own service in a config fed back to it.
pub const API_ALLOWED_ORIGIN: &str = "http://boxpilot.invalid";

/// The destinations [`reject_loopback`] refuses by address: IPv4 and IPv6
/// loopback, the IPv4-mapped forms of IPv4 loopback, and the unspecified
/// addresses, which reach the local host on some systems (Linux, macOS).
pub const LOOPBACK_CIDRS: &[&str] = &[
    "127.0.0.0/8",
    "::1/128",
    "::ffff:127.0.0.0/104",
    "0.0.0.0/8",
    "::/128",
    "::ffff:0.0.0.0/104",
];

/// The destinations [`reject_loopback`] refuses by name: `localhost` and
/// every name under it (RFC 6761), with and without the final dot. sing-box
/// takes a `domain_suffix` without a leading dot as the name itself and its
/// subdomains, and lowercases the name before matching.
pub const LOOPBACK_DOMAIN_SUFFIXES: &[&str] = &["localhost", "localhost."];

/// How many candidate ports [`pick_port_avoiding`] draws before giving up.
pub const PORT_PICK_ATTEMPTS: usize = 16;

/// BoxPilot's own `api` service for one sing-box run: the loopback port it
/// listens on and the secret it requires. The caller draws both, the secret
/// from the OS RNG; this type only turns them into the `services[]` entry,
/// so every caller writes the same one.
///
/// `Debug` redacts the secret: whoever holds it controls the run, and on the
/// privileged path that run is SYSTEM's.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiService {
    port: u16,
    secret: String,
}

impl ApiService {
    /// The service on `port`, behind `secret` (raw bytes, at least 16 of
    /// them in practice), which sing-box takes as lowercase hex.
    pub fn new(port: u16, secret: &[u8]) -> Self {
        Self {
            port,
            secret: secret.iter().map(|b| format!("{b:02x}")).collect(),
        }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The secret as sing-box's `secret` option and the bearer token carry
    /// it: lowercase hex.
    pub fn secret_hex(&self) -> &str {
        &self.secret
    }

    /// The `services[]` entry, tagged [`API_SERVICE_TAG`]. Without `secret`
    /// sing-box serves anyone who can reach the port, and without
    /// `access_control_allow_origin` its CORS answers `*`, so every web page
    /// could read the responses.
    pub fn service_config(&self) -> Value {
        serde_json::json!({
            "type": "api",
            "tag": API_SERVICE_TAG,
            "listen": "127.0.0.1",
            "listen_port": self.port,
            "secret": self.secret,
            "access_control_allow_origin": [API_ALLOWED_ORIGIN]
        })
    }
}

impl fmt::Debug for ApiService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiService")
            .field("port", &self.port)
            .field("secret", &"<redacted>")
            .finish()
    }
}

/// What [`inject`] writes, all of it typed: nothing here comes from the
/// profile.
#[derive(Debug, Clone, Copy)]
pub struct Inject<'a> {
    /// `true`: Proxy mode, the mixed inbound only. `false`: TUN mode, a TUN
    /// inbound and then the mixed inbound.
    pub proxy_mode: bool,
    /// Have sing-box point the OS proxy setting at the mixed inbound while
    /// it runs, unless [`Inject::forbid_system_proxy`] says otherwise.
    pub set_system_proxy: bool,
    /// Never let sing-box write the OS proxy setting, whatever
    /// `set_system_proxy` asks. The privileged helper sets it on Windows: a
    /// SYSTEM sing-box would write SYSTEM's proxy, not the user's, and the
    /// user's proxy setting is the user's task, so the GUI sets it itself,
    /// as the user (ADR 0006, "System proxy").
    pub forbid_system_proxy: bool,
    /// The local proxy's port (Settings › Network): the mixed inbound's.
    pub proxy_port: u16,
    /// TUN mode only: give the TUN interface an IPv6 address too, so IPv6
    /// traffic is routed into the tunnel. Off, the interface carries no IPv6
    /// route at all; nothing is injected into `dns` or `route` to make up
    /// for it.
    pub tun_ipv6: bool,
    /// "Allow LAN connections": the mixed inbound listens on every IPv4
    /// interface (`0.0.0.0`) instead of loopback, in both modes. The system
    /// proxy sing-box sets still points at `127.0.0.1`: sing-box substitutes
    /// loopback for an unspecified listen address.
    pub allow_lan: bool,
    /// BoxPilot's `api` service for this run.
    pub api: &'a ApiService,
}

/// Write BoxPilot's parts into `config`, a config's top-level object:
///
/// - `inbounds` replaced by BoxPilot's own: the TUN inbound (TUN mode) and
///   the mixed inbound;
/// - `experimental.cache_file.enabled` forced on, so sing-box remembers the
///   selected nodes and the Clash mode; the rest of `experimental` stays as
///   it is, and a non-object `experimental` or `cache_file` gives way to an
///   object;
/// - BoxPilot's `api` service appended to `services`, under the first tag of
///   [`API_SERVICE_TAG`], `…-2`, `…-3` that none of the config's services
///   uses. Those services stay as written, in order; a non-array `services`
///   gives way to an array.
pub fn inject(config: &mut Map<String, Value>, options: &Inject<'_>) {
    let mut mixed = serde_json::json!({
        "type": "mixed",
        "tag": "proxy",
        "listen": mixed_listen_address(options.allow_lan),
        "listen_port": options.proxy_port
    });
    if options.set_system_proxy && !options.forbid_system_proxy {
        mixed["set_system_proxy"] = Value::Bool(true);
    }
    let inbounds = if options.proxy_mode {
        vec![mixed]
    } else {
        let mut address = vec![Value::from(TUN_IPV4_ADDRESS)];
        if options.tun_ipv6 {
            address.push(Value::from(TUN_IPV6_ADDRESS));
        }
        let tun = serde_json::json!({
            "type": "tun",
            "tag": "tun0",
            "address": address,
            "auto_route": true,
            "strict_route": TUN_STRICT_ROUTE,
            "stack": "mixed"
        });
        vec![tun, mixed]
    };
    config.insert("inbounds".into(), Value::Array(inbounds));

    // With cache_file enabled, sing-box (≥ 1.8) persists the chosen selector
    // node across restarts on its own; the old `store_selected` field was
    // removed upstream and now fails config validation, so only `enabled`
    // is written.
    let cache_file = object_entry(object_entry(config, "experimental"), "cache_file");
    cache_file.insert("enabled".into(), Value::Bool(true));

    let mut services = match config.get("services") {
        Some(Value::Array(services)) => services.clone(),
        _ => Vec::new(),
    };
    let mut ours = options.api.service_config();
    ours["tag"] = Value::from(api_service_tag(&services));
    services.push(ours);
    config.insert("services".into(), Value::Array(services));
}

/// The route rule [`reject_loopback`] puts first, in sing-box 1.14's
/// syntax: a destination that is a loopback address or a `localhost` name
/// (these items are one "destination address" group, so any one matching
/// is enough), from any inbound or endpoint, gets the `reject` action.
pub fn loopback_rule() -> Value {
    serde_json::json!({
        "ip_cidr": LOOPBACK_CIDRS,
        "domain_suffix": LOOPBACK_DOMAIN_SUFFIXES,
        "action": "reject"
    })
}

/// The privileged path only: put [`loopback_rule`] first in
/// `route.rules`, before the profile's own, creating `route` and its
/// `rules` if need be (a non-array `rules` gives way to an array, as
/// [`inject`]'s `services` does).
///
/// A sing-box running as SYSTEM makes every connection it routes from a
/// SYSTEM process. Without this, any local account could reach a service
/// that listens on this machine's loopback only, through the mixed
/// inbound (no password, on `127.0.0.1`, or the LAN with "Allow LAN"), and
/// so could a peer of a WireGuard, Tailscale or OpenVPN-server endpoint;
/// some such services trust a SYSTEM peer more than a user's. So the rule
/// applies to every inbound and endpoint, not the mixed inbound alone. No
/// legitimate traffic is lost: what goes through TUN never has a loopback
/// destination, and an outbound's own server (a local SOCKS proxy, say) is
/// dialed without a route rule.
///
/// What it can't see: a name that resolves to loopback (sing-box matches
/// `ip_cidr` against an IP destination only, unless a `resolve` action ran
/// first, which would change how every profile routes), and a
/// `route-options` rule of the profile that sets `override_address` after
/// it. The GUI's own run, at the user's privilege, has no such rule: there
/// the profile runs as written (ADR 0002).
pub fn reject_loopback(config: &mut Map<String, Value>) {
    let rules = object_entry(config, "route")
        .entry("rules")
        .or_insert_with(|| Value::Array(Vec::new()));
    if !rules.is_array() {
        *rules = Value::Array(Vec::new());
    }
    rules
        .as_array_mut()
        .expect("`rules` was just made an array")
        .insert(0, loopback_rule());
}

/// Where the mixed inbound listens: every IPv4 interface when LAN
/// connections are allowed, loopback otherwise. IPv4 only, matching the
/// addresses the GUI's LAN hint lists.
fn mixed_listen_address(allow_lan: bool) -> &'static str {
    if allow_lan {
        "0.0.0.0"
    } else {
        "127.0.0.1"
    }
}

/// The tag for BoxPilot's `api` service: [`API_SERVICE_TAG`], or with a
/// `-2`, `-3`, … suffix if `services` already use it.
fn api_service_tag(services: &[Value]) -> String {
    let taken = |tag: &str| services.iter().any(|service| service["tag"] == tag);
    std::iter::once(API_SERVICE_TAG.to_owned())
        .chain((2..).map(|n| format!("{API_SERVICE_TAG}-{n}")))
        .find(|tag| !taken(tag))
        .expect("a free tag among infinitely many")
}

/// The object at `parent[key]`, created empty, or replacing a non-object,
/// if need be. It keeps its place among `parent`'s keys.
fn object_entry<'a>(parent: &'a mut Map<String, Value>, key: &str) -> &'a mut Map<String, Value> {
    let slot = parent
        .entry(key)
        .or_insert_with(|| Value::Object(Map::new()));
    if !slot.is_object() {
        *slot = Value::Object(Map::new());
    }
    slot.as_object_mut()
        .expect("the slot was just made an object")
}

/// Ports the config's own listeners take besides the inbounds BoxPilot owns:
/// every service's `listen_port` (its own `api` services among them), and
/// the `clash_api` / `v2ray_api` controller addresses. The `api` service's
/// port must be none of them, since none is bound yet while sing-box is
/// down.
pub fn config_listen_ports(config: &Value) -> Vec<u16> {
    let mut ports: Vec<u16> = config["services"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|service| service["listen_port"].as_u64())
        .filter_map(|port| u16::try_from(port).ok())
        .collect();
    let experimental = &config["experimental"];
    for address in [
        &experimental["clash_api"]["external_controller"],
        &experimental["v2ray_api"]["listen"],
    ] {
        if let Some(port) = address.as_str().and_then(address_port) {
            ports.push(port);
        }
    }
    ports
}

/// The port of a `host:port` listen address (`127.0.0.1:9090`, `[::]:9090`,
/// `:9090`).
pub fn address_port(address: &str) -> Option<u16> {
    address.rsplit_once(':')?.1.parse().ok()
}

/// Draw ports from `next` until one is not `excluded`, at most
/// [`PORT_PICK_ATTEMPTS`] times. `next` returns a port the OS says is free
/// together with whatever holds it (a bound listener). Each rejected draw's
/// holder stays alive until the end, so the OS can't offer the same port
/// twice; the picked one's is dropped, freeing the port for sing-box.
///
/// Nothing holds the port afterwards, so another program can take it before
/// sing-box binds; sing-box then fails to start, and the caller picks again.
pub fn pick_port_avoiding<H>(
    excluded: &[u16],
    mut next: impl FnMut() -> io::Result<(u16, H)>,
) -> io::Result<u16> {
    let mut rejected = Vec::new();
    for _ in 0..PORT_PICK_ATTEMPTS {
        let (port, holder) = next()?;
        if !excluded.contains(&port) {
            return Ok(port);
        }
        rejected.push(holder);
    }
    Err(io::Error::new(
        io::ErrorKind::AddrInUse,
        "every port offered is one the config uses",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;
    use std::rc::Rc;

    const SECRET: [u8; 4] = [0x00, 0x0f, 0xa5, 0xff];

    fn api() -> ApiService {
        ApiService::new(41234, &SECRET)
    }

    fn options(api: &ApiService) -> Inject<'_> {
        Inject {
            proxy_mode: false,
            set_system_proxy: false,
            forbid_system_proxy: false,
            proxy_port: 7890,
            tun_ipv6: false,
            allow_lan: false,
            api,
        }
    }

    fn injected(config: Value, options: &Inject<'_>) -> Value {
        let Value::Object(mut root) = config else {
            panic!("tests inject into objects")
        };
        inject(&mut root, options);
        Value::Object(root)
    }

    fn our_service() -> Value {
        json!({
            "type": "api",
            "tag": "boxpilot-api",
            "listen": "127.0.0.1",
            "listen_port": 41234,
            "secret": "000fa5ff",
            "access_control_allow_origin": ["http://boxpilot.invalid"]
        })
    }

    fn mixed(listen: &str) -> Value {
        json!({"type": "mixed", "tag": "proxy", "listen": listen, "listen_port": 7890})
    }

    fn tun(address: Value) -> Value {
        json!({
            "type": "tun",
            "tag": "tun0",
            "address": address,
            "auto_route": true,
            "strict_route": !cfg!(target_os = "linux"),
            "stack": "mixed"
        })
    }

    #[test]
    fn an_empty_config_gets_every_part() {
        let api = api();
        assert_eq!(
            injected(json!({}), &options(&api)),
            json!({
                "inbounds": [tun(json!(["172.18.0.1/30"])), mixed("127.0.0.1")],
                "experimental": {"cache_file": {"enabled": true}},
                "services": [our_service()]
            })
        );
    }

    #[test]
    fn proxy_mode_injects_the_mixed_inbound_only() {
        let api = api();
        let config = injected(
            json!({}),
            &Inject {
                proxy_mode: true,
                tun_ipv6: true,
                ..options(&api)
            },
        );
        assert_eq!(config["inbounds"], json!([mixed("127.0.0.1")]));
    }

    #[test]
    fn tun_ipv6_adds_the_ipv6_address() {
        let api = api();
        let config = injected(
            json!({}),
            &Inject {
                tun_ipv6: true,
                ..options(&api)
            },
        );
        assert_eq!(
            config["inbounds"],
            json!([
                tun(json!(["172.18.0.1/30", "fdfe:dcba:9876::1/126"])),
                mixed("127.0.0.1")
            ])
        );
    }

    #[test]
    fn allow_lan_moves_only_the_mixed_inbound() {
        let api = api();
        for proxy_mode in [false, true] {
            let config = injected(
                json!({}),
                &Inject {
                    proxy_mode,
                    allow_lan: true,
                    ..options(&api)
                },
            );
            let inbounds = config["inbounds"].as_array().unwrap();
            assert_eq!(inbounds.last().unwrap(), &mixed("0.0.0.0"));
            assert_eq!(config["services"], json!([our_service()]));
        }
    }

    #[test]
    fn the_system_proxy_flag_follows_the_option() {
        let api = api();
        for proxy_mode in [false, true] {
            let config = injected(
                json!({}),
                &Inject {
                    proxy_mode,
                    set_system_proxy: true,
                    ..options(&api)
                },
            );
            let mut expected = mixed("127.0.0.1");
            expected["set_system_proxy"] = json!(true);
            assert_eq!(
                config["inbounds"].as_array().unwrap().last().unwrap(),
                &expected
            );
        }
    }

    /// The helper's case: whatever the request asks, a SYSTEM sing-box
    /// never gets the flag, in either mode, with LAN access or without.
    #[test]
    fn forbidding_the_system_proxy_wins() {
        let api = api();
        for proxy_mode in [false, true] {
            for allow_lan in [false, true] {
                let config = injected(
                    json!({}),
                    &Inject {
                        proxy_mode,
                        allow_lan,
                        set_system_proxy: true,
                        forbid_system_proxy: true,
                        ..options(&api)
                    },
                );
                let listen = if allow_lan { "0.0.0.0" } else { "127.0.0.1" };
                assert_eq!(
                    config["inbounds"].as_array().unwrap().last().unwrap(),
                    &mixed(listen)
                );
            }
        }
    }

    #[test]
    fn a_profiles_inbounds_are_replaced() {
        let api = api();
        let config = injected(
            json!({"inbounds": [{"type": "socks", "tag": "theirs", "listen_port": 1080}]}),
            &Inject {
                proxy_mode: true,
                ..options(&api)
            },
        );
        assert_eq!(config["inbounds"], json!([mixed("127.0.0.1")]));
    }

    #[test]
    fn experimental_is_merged_and_only_enabled_is_forced() {
        let api = api();
        let config = injected(
            json!({"experimental": {
                "clash_api": {"external_controller": "127.0.0.1:9090"},
                "cache_file": {"enabled": false, "path": "custom.db"}
            }}),
            &options(&api),
        );
        assert_eq!(
            config["experimental"],
            json!({
                "clash_api": {"external_controller": "127.0.0.1:9090"},
                "cache_file": {"enabled": true, "path": "custom.db"}
            })
        );
        for malformed in [
            json!({"experimental": null}),
            json!({"experimental": {"cache_file": true}}),
        ] {
            let config = injected(malformed, &options(&api));
            assert_eq!(
                config["experimental"]["cache_file"],
                json!({"enabled": true})
            );
        }
    }

    /// Keys keep their place: the profile's order survives, and a replaced
    /// section stays where the profile had it.
    #[test]
    fn key_order_is_kept() {
        let api = api();
        let config = injected(
            json!({"log": {}, "inbounds": [], "experimental": null, "route": {}}),
            &options(&api),
        );
        let keys: Vec<&str> = config
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            ["log", "inbounds", "experimental", "route", "services"]
        );
    }

    #[test]
    fn the_configs_services_stay_before_ours() {
        let api = api();
        let theirs = json!([
            {"type": "resolved", "tag": "resolved"},
            {"type": "api", "tag": "api", "listen": "0.0.0.0", "listen_port": 9090}
        ]);
        let config = injected(json!({"services": theirs.clone()}), &options(&api));
        let mut expected = theirs.as_array().unwrap().clone();
        expected.push(our_service());
        assert_eq!(config["services"], Value::Array(expected));

        let config = injected(json!({"services": {"not": "an array"}}), &options(&api));
        assert_eq!(config["services"], json!([our_service()]));
    }

    #[test]
    fn our_tag_steps_around_the_configs() {
        let api = api();
        let config = injected(
            json!({"services": [
                {"type": "api", "tag": "boxpilot-api-2"},
                {"type": "derp", "tag": "boxpilot-api"}
            ]}),
            &options(&api),
        );
        let mut ours = our_service();
        ours["tag"] = json!("boxpilot-api-3");
        assert_eq!(config["services"][2], ours);
    }

    #[test]
    fn the_api_service_is_hex_and_redacted() {
        let api = ApiService::new(17900, &[0xde, 0xad, 0x01]);
        assert_eq!(api.port(), 17900);
        assert_eq!(api.secret_hex(), "dead01");
        assert_eq!(
            format!("{api:?}"),
            r#"ApiService { port: 17900, secret: "<redacted>" }"#
        );
        assert_eq!(ApiService::new(1, &[]).secret_hex(), "");
    }

    /// The exact rule, in sing-box 1.14.2's syntax (`option/rule.go`,
    /// `option/rule_action.go`).
    #[test]
    fn the_loopback_rule_is_sing_boxs_reject_rule() {
        assert_eq!(
            loopback_rule(),
            json!({
                "ip_cidr": [
                    "127.0.0.0/8",
                    "::1/128",
                    "::ffff:127.0.0.0/104",
                    "0.0.0.0/8",
                    "::/128",
                    "::ffff:0.0.0.0/104"
                ],
                "domain_suffix": ["localhost", "localhost."],
                "action": "reject"
            })
        );
    }

    /// First, before the profile's own rules, which stay as they were and
    /// in order; `route`'s other keys keep their place.
    #[test]
    fn the_loopback_rule_goes_first() {
        let profile_rules = json!([
            {"action": "sniff"},
            {"ip_cidr": ["127.0.0.1/32"], "outbound": "direct"},
            {"domain": ["example.com"], "outbound": "proxy"}
        ]);
        let Value::Object(mut root) = json!({
            "route": {"final": "proxy", "rules": profile_rules.clone(), "auto_detect_interface": true}
        }) else {
            unreachable!()
        };
        reject_loopback(&mut root);
        let route = &root["route"];
        let rules = route["rules"].as_array().unwrap();
        assert_eq!(rules[0], loopback_rule());
        assert_eq!(Value::Array(rules[1..].to_vec()), profile_rules);
        let keys: Vec<&String> = route.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["final", "rules", "auto_detect_interface"]);
    }

    #[test]
    fn the_loopback_rule_makes_its_own_route_if_need_be() {
        for config in [
            json!({}),
            json!({"route": "not an object"}),
            json!({"route": {"rules": "not an array"}}),
            json!({"route": {"final": "direct"}}),
        ] {
            let Value::Object(mut root) = config.clone() else {
                unreachable!()
            };
            reject_loopback(&mut root);
            assert_eq!(root["route"]["rules"], json!([loopback_rule()]), "{config}");
        }
    }

    /// The GUI's own run has no such rule: `inject` never adds one.
    #[test]
    fn inject_alone_adds_no_route_rule() {
        let api = api();
        for proxy_mode in [false, true] {
            let mut options = options(&api);
            options.proxy_mode = proxy_mode;
            let config = injected(json!({"route": {"rules": []}}), &options);
            assert_eq!(config["route"], json!({"rules": []}));
            assert_eq!(injected(json!({}), &options).get("route"), None);
        }
    }

    #[test]
    fn listen_ports_cover_services_and_controllers() {
        let config = json!({
            "inbounds": [{"type": "mixed", "listen_port": 2080}],
            "services": [
                {"type": "resolved", "listen_port": 53},
                {"type": "api", "listen_port": 9091},
                {"type": "api", "listen_port": 70000},
                {"type": "derp"}
            ],
            "experimental": {
                "clash_api": {"external_controller": "127.0.0.1:9090"},
                "v2ray_api": {"listen": "[::]:8080"}
            }
        });
        assert_eq!(config_listen_ports(&config), vec![53, 9091, 9090, 8080]);
        assert_eq!(config_listen_ports(&json!({})), Vec::<u16>::new());
        assert_eq!(config_listen_ports(&json!([1, 2])), Vec::<u16>::new());
        assert_eq!(address_port(":9090"), Some(9090));
        assert_eq!(address_port("0.0.0.0"), None);
        assert_eq!(address_port("127.0.0.1:"), None);
        assert_eq!(address_port("127.0.0.1:65536"), None);
    }

    /// Stands in for the listener holding a drawn port: removes it from the
    /// shared list of held ports when dropped.
    struct Held(u16, Rc<RefCell<Vec<u16>>>);

    impl Drop for Held {
        fn drop(&mut self) {
            self.1.borrow_mut().retain(|&port| port != self.0);
        }
    }

    #[test]
    fn picking_skips_excluded_ports_and_holds_them_until_the_end() {
        let held = Rc::new(RefCell::new(Vec::new()));
        let draws = [9090u16, 7890, 41234, 41235];
        let mut drawn = 0;
        let port = pick_port_avoiding(&[7890, 9090], || {
            assert_eq!(*held.borrow(), draws[..drawn]);
            let port = draws[drawn];
            drawn += 1;
            held.borrow_mut().push(port);
            Ok((port, Held(port, held.clone())))
        })
        .unwrap();
        assert_eq!(port, 41234);
        assert_eq!(drawn, 3);
        assert!(held.borrow().is_empty());
    }

    #[test]
    fn picking_gives_up_and_passes_errors_on() {
        let mut drawn = 0;
        let error = pick_port_avoiding(&[9090], || {
            drawn += 1;
            Ok((9090, ()))
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        assert_eq!(drawn, PORT_PICK_ATTEMPTS);

        let error = pick_port_avoiding::<()>(&[], || {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "no"))
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }
}
