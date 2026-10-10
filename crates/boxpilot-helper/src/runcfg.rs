//! A `start` request, turned into the config sing-box runs (ADR 0006 rules 1
//! and 2).
//!
//! In two steps, so a refused config costs nothing but the check:
//!
//! 1. [`check`]: `boxpilot_policy::check` on the config as received. Its
//!    refusals become the `refused` reply. Of the attachments, only those
//!    the checked config refers to are kept.
//! 2. [`build`]: `materialize` with this start's [`Placement`], then
//!    `boxpilot_runconfig`'s injection with the helper's own `api` service:
//!    a loopback port none of the config's listeners or the proxy uses, and
//!    a fresh secret from the OS RNG. Then the privileged path's own first
//!    route rule, `boxpilot_runconfig::reject_loopback`: a SYSTEM sing-box
//!    never connects to this machine's loopback for anyone.
//!
//! sing-box runs on the serialization of what `build` returns, never on the
//! bytes received: those can say things (duplicate keys, comments) that
//! sing-box's Go decoder reads differently from the parse the policy
//! checked.

#![forbid(unsafe_code)]

use boxpilot_policy::{Checked, Limits, Placement, Refusal};
use boxpilot_protocol::{StartRequest, Started, TunOptions};
use boxpilot_runconfig::{
    config_listen_ports, pick_port_avoiding, reject_loopback, ApiService, Inject,
};
use std::fmt;
use std::io;
use std::net::{Ipv4Addr, TcpListener};

/// Bytes of OS randomness in the `api` service's secret, as the GUI uses.
pub const SECRET_LEN: usize = 32;

/// The config's file name in the run directory.
pub const CONFIG_FILE: &str = "config.json";

/// Who sets the OS proxy when a `start` asks for it (ADR 0006, "System
/// proxy"). Never the helper's sing-box: the config the helper writes has
/// no `set_system_proxy`, on any platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemProxy {
    /// Nobody on the helper's side: the request's `system_proxy` is
    /// ignored here.
    Forbidden,
    /// The helper itself, as the request asks: it sets the proxy once
    /// sing-box is up and resets it after (`cleanup`), outside sing-box's
    /// sandbox.
    ByHelper,
}

/// This platform's [`SystemProxy`]:
///
/// - **Windows: forbidden.** A SYSTEM sing-box would write SYSTEM's proxy,
///   not the user's; the GUI sets the user's itself, as the user.
/// - **macOS: by the helper.** The proxy is a machine-wide setting of each
///   network service there, which root's `networksetup` can write on a
///   standard account too (closing a gap ADR 0005 notes). sing-box would
///   set it the same way, but `networksetup` runs a shell and writes
///   SystemConfiguration's files, which sing-box's sandbox denies it
///   (`sandboxplan`); so the helper, which isn't sandboxed, does it.
pub const SYSTEM_PROXY: SystemProxy = if cfg!(target_os = "macos") {
    SystemProxy::ByHelper
} else {
    SystemProxy::Forbidden
};

/// A `start` whose config passed the policy, with only the attachments it
/// refers to.
pub struct CheckedStart {
    checked: Checked,
    attachments: Vec<(String, Vec<u8>)>,
    options: TunOptions,
}

/// Sizes only: the config and the attachments hold keys and passwords.
impl fmt::Debug for CheckedStart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CheckedStart")
            .field("attachments", &self.attachments.len())
            .field("options", &self.options)
            .finish()
    }
}

impl CheckedStart {
    /// The ids of the attachments kept, in the order they arrived.
    pub fn attachment_ids(&self) -> Vec<&str> {
        self.attachments.iter().map(|(id, _)| id.as_str()).collect()
    }
}

/// Check a `start`'s config against the policy. The protocol's session has
/// already held the request to its limits, which are the policy's own
/// (32 MiB), so `Limits::default()` refuses nothing it let through for size.
pub fn check(start: StartRequest) -> Result<CheckedStart, Vec<Refusal>> {
    let ids = start.attachment_ids();
    let checked = boxpilot_policy::check(&start.config, &ids, &Limits::default())?;
    let wanted = checked.attachment_ids();
    let attachments = start
        .attachments
        .into_iter()
        .filter(|(id, _)| wanted.contains(id.as_str()))
        .collect();
    Ok(CheckedStart {
        checked,
        attachments,
        options: start.options,
    })
}

/// The config and files of one run, ready to write into its run directory,
/// and the `api` service the GUI is told about.
pub struct Prepared {
    config: String,
    files: Vec<(String, Vec<u8>)>,
    api: ApiService,
    system_proxy: Option<u16>,
}

/// Sizes only, and the secret redacted.
impl fmt::Debug for Prepared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Prepared")
            .field("config_len", &self.config.len())
            .field("files", &self.files.len())
            .field("api", &self.api)
            .field("system_proxy", &self.system_proxy)
            .finish()
    }
}

impl Prepared {
    /// The config sing-box runs, as JSON text.
    pub fn config(&self) -> &str {
        &self.config
    }

    /// `(file name, content)` of each attachment the config refers to: a
    /// plain name the helper derived, to write in the run directory.
    pub fn files(&self) -> &[(String, Vec<u8>)] {
        &self.files
    }

    pub fn api(&self) -> &ApiService {
        &self.api
    }

    /// The port the helper points the OS proxy setting at for this run,
    /// when the start asked for it and [`SystemProxy::ByHelper`] holds:
    /// always `127.0.0.1` and the proxy port. What the platform sets once
    /// sing-box is up, and resets after.
    pub fn system_proxy_port(&self) -> Option<u16> {
        self.system_proxy
    }

    /// The `started` reply: where the `api` service listens, and its
    /// secret, for the connection that started this run only.
    pub fn started(&self) -> Started {
        Started {
            api_port: self.api.port(),
            api_secret: self.api.secret_hex().to_owned(),
        }
    }
}

/// Why a checked start could not be built. Nothing here quotes the config.
#[derive(Debug)]
pub enum BuildError {
    /// No loopback port for the `api` service.
    Port(io::Error),
    /// An attachment's path, as the policy names it, is not a plain file in
    /// the run directory. The policy never makes one; refused all the same.
    AttachmentOutsideRunDir,
    Serialize(String),
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuildError::Port(error) => write!(f, "no loopback port for the sing-box API: {error}"),
            BuildError::AttachmentOutsideRunDir => {
                f.write_str("an attachment would land outside the run directory")
            }
            BuildError::Serialize(message) => {
                write!(f, "the config could not be written: {message}")
            }
        }
    }
}

impl std::error::Error for BuildError {}

/// Build the run config: the checked config materialized with `placement`,
/// then BoxPilot's inbounds, cache file and `api` service injected, and the
/// rule that rejects loopback destinations put first in `route.rules`. The
/// `api` service's port comes from `ports` (see `pick_port_avoiding`),
/// avoiding the proxy port and every port the config itself listens on;
/// its secret is `secret`. sing-box never sets the OS proxy; whether the
/// helper does is `system_proxy`'s call: [`SYSTEM_PROXY`] on the helper's
/// path.
pub fn build<H>(
    start: CheckedStart,
    placement: &Placement,
    system_proxy: SystemProxy,
    ports: impl FnMut() -> io::Result<(u16, H)>,
    secret: &[u8; SECRET_LEN],
) -> Result<Prepared, BuildError> {
    let CheckedStart {
        checked,
        attachments,
        options,
    } = start;
    let files = attachments
        .into_iter()
        .map(|(id, data)| Ok((attachment_file_name(placement, &id)?, data)))
        .collect::<Result<Vec<_>, BuildError>>()?;

    let mut config = boxpilot_policy::materialize(checked, placement);
    let mut excluded = config_listen_ports(&config);
    excluded.push(options.proxy_port);
    let port = pick_port_avoiding(&excluded, ports).map_err(BuildError::Port)?;
    let api = ApiService::new(port, secret);

    let root = config
        .as_object_mut()
        .expect("the policy checks that the config is an object");
    boxpilot_runconfig::inject(
        root,
        &Inject {
            proxy_mode: false,
            set_system_proxy: options.system_proxy,
            forbid_system_proxy: true,
            proxy_port: options.proxy_port,
            tun_ipv6: options.ipv6,
            allow_lan: options.allow_lan,
            api: &api,
        },
    );
    reject_loopback(root);
    let config = serde_json::to_string_pretty(&config)
        .map_err(|error| BuildError::Serialize(error.to_string()))?;
    Ok(Prepared {
        config,
        files,
        api,
        system_proxy: (options.system_proxy && system_proxy == SystemProxy::ByHelper)
            .then_some(options.proxy_port),
    })
}

/// The file name attachment `id` gets in the run directory: the last part of
/// the path the policy points the config at, which must be directly inside
/// the run directory.
fn attachment_file_name(placement: &Placement, id: &str) -> Result<String, BuildError> {
    let path = placement.attachment_path(id);
    let prefix = format!(
        "{}{}",
        placement.run_dir.trim_end_matches(placement.separator),
        placement.separator
    );
    let name = path
        .strip_prefix(&prefix)
        .ok_or(BuildError::AttachmentOutsideRunDir)?;
    let plain = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !plain {
        return Err(BuildError::AttachmentOutsideRunDir);
    }
    Ok(name.to_owned())
}

/// A secret for one run's `api` service, from the OS RNG.
pub fn fresh_secret() -> Result<[u8; SECRET_LEN], getrandom::Error> {
    let mut secret = [0u8; SECRET_LEN];
    getrandom::fill(&mut secret)?;
    Ok(secret)
}

/// A port the OS hands out for `127.0.0.1:0`, with the listener that holds
/// it: the candidates `build` draws from.
pub fn free_loopback_port() -> io::Result<(u16, TcpListener)> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    Ok((listener.local_addr()?.port(), listener))
}

#[cfg(test)]
mod tests {
    use super::*;
    use boxpilot_policy::RefusalKind;
    use serde_json::{json, Value};

    const SECRET: [u8; SECRET_LEN] = [0xab; SECRET_LEN];

    fn options() -> TunOptions {
        TunOptions {
            ipv6: true,
            proxy_port: 7890,
            allow_lan: false,
            system_proxy: true,
        }
    }

    fn start(config: Value, attachments: &[(&str, &[u8])]) -> StartRequest {
        StartRequest {
            config: config.to_string(),
            attachments: attachments
                .iter()
                .map(|(id, data)| (id.to_string(), data.to_vec()))
                .collect(),
            options: options(),
        }
    }

    fn placement() -> Placement {
        Placement {
            run_dir: r"C:\State\runs\r1".into(),
            cache_file: r"C:\State\users\S-1-5-21-1-2-3-1001\cache.db".into(),
            tailscale_dir: r"C:\State\users\S-1-5-21-1-2-3-1001\tailscale".into(),
            separator: '\\',
        }
    }

    fn fixed_port(port: u16) -> impl FnMut() -> io::Result<(u16, ())> {
        move || Ok((port, ()))
    }

    fn built(config: Value, attachments: &[(&str, &[u8])]) -> (Value, Prepared) {
        let checked = check(start(config, attachments)).unwrap();
        let prepared = build(
            checked,
            &placement(),
            SystemProxy::Forbidden,
            fixed_port(41234),
            &SECRET,
        )
        .unwrap();
        (serde_json::from_str(prepared.config()).unwrap(), prepared)
    }

    #[test]
    fn a_profile_becomes_the_run_config() {
        let (config, prepared) = built(
            json!({
                "log": {"level": "info", "output": "C:\\Windows\\evil.log"},
                "outbounds": [{"type": "direct", "tag": "direct"}],
                "experimental": {"clash_api": {"external_controller": "0.0.0.0:9090"}}
            }),
            &[],
        );
        let secret = "ab".repeat(SECRET_LEN);
        assert_eq!(
            config,
            json!({
                "log": {"level": "info"},
                "outbounds": [{"type": "direct", "tag": "direct"}],
                "experimental": {"cache_file": {
                    "path": r"C:\State\users\S-1-5-21-1-2-3-1001\cache.db",
                    "enabled": true
                }},
                "route": {"rules": [boxpilot_runconfig::loopback_rule()]},
                "inbounds": [
                    {
                        "type": "tun",
                        "tag": "tun0",
                        "address": ["172.18.0.1/30", "fdfe:dcba:9876::1/126"],
                        "auto_route": true,
                        "strict_route": boxpilot_runconfig::TUN_STRICT_ROUTE,
                        "stack": "mixed"
                    },
                    {"type": "mixed", "tag": "proxy", "listen": "127.0.0.1", "listen_port": 7890}
                ],
                "services": [{
                    "type": "api",
                    "tag": "boxpilot-api",
                    "listen": "127.0.0.1",
                    "listen_port": 41234,
                    "secret": secret,
                    "access_control_allow_origin": ["http://boxpilot.invalid"]
                }]
            })
        );
        assert_eq!(
            prepared.started(),
            Started {
                api_port: 41234,
                api_secret: secret,
            }
        );
        assert!(prepared.files().is_empty());
    }

    /// The SYSTEM sing-box rejects loopback destinations before any rule of
    /// the profile can route them, a profile's rule for 127.0.0.1 included.
    #[test]
    fn loopback_is_rejected_before_the_profiles_rules() {
        let profile_rules = json!([
            {"ip_cidr": ["127.0.0.1/32"], "outbound": "direct"},
            {"domain_suffix": ["localhost"], "outbound": "direct"}
        ]);
        let (config, _) = built(
            json!({
                "outbounds": [{"type": "direct", "tag": "direct"}],
                "route": {"rules": profile_rules.clone(), "final": "direct"}
            }),
            &[],
        );
        let rules = config["route"]["rules"].as_array().unwrap();
        assert_eq!(rules[0], boxpilot_runconfig::loopback_rule());
        assert_eq!(rules[0]["action"], "reject");
        assert_eq!(Value::Array(rules[1..].to_vec()), profile_rules);
        assert_eq!(config["route"]["final"], "direct");
    }

    fn build_with(system_proxy: SystemProxy, requested: bool) -> Prepared {
        let mut request = start(json!({}), &[]);
        request.options.system_proxy = requested;
        build(
            check(request).unwrap(),
            &placement(),
            system_proxy,
            fixed_port(41234),
            &SECRET,
        )
        .unwrap()
    }

    /// Where it is forbidden (Windows), the request asks for the system
    /// proxy and nobody on the helper's side sets it.
    #[test]
    fn a_forbidden_system_proxy_is_never_set() {
        let (config, prepared) = built(json!({}), &[]);
        assert!(config["inbounds"][1].get("set_system_proxy").is_none());
        assert!(!config.to_string().contains("set_system_proxy"));
        assert_eq!(prepared.system_proxy_port(), None);
    }

    /// Where the helper sets it (macOS), it learns the port as the request
    /// asks, and sing-box still never gets `set_system_proxy`: its sandbox
    /// would deny it `networksetup`.
    #[test]
    fn the_helper_sets_the_system_proxy_and_sing_box_never_does() {
        let prepared = build_with(SystemProxy::ByHelper, true);
        let config: Value = serde_json::from_str(prepared.config()).unwrap();
        assert!(config["inbounds"][1].get("set_system_proxy").is_none());
        assert!(!prepared.config().contains("set_system_proxy"));
        assert_eq!(config["inbounds"][1]["listen_port"], json!(7890));
        assert_eq!(prepared.system_proxy_port(), Some(7890));
        let prepared = build_with(SystemProxy::ByHelper, false);
        assert!(!prepared.config().contains("set_system_proxy"));
        assert_eq!(prepared.system_proxy_port(), None);
        let prepared = build_with(SystemProxy::Forbidden, true);
        assert_eq!(prepared.system_proxy_port(), None);
    }

    #[test]
    fn only_macos_has_the_helper_set_the_system_proxy() {
        assert_eq!(
            SYSTEM_PROXY == SystemProxy::ByHelper,
            cfg!(target_os = "macos")
        );
    }

    #[test]
    fn allow_lan_moves_the_mixed_inbound_only() {
        let mut request = start(json!({}), &[]);
        request.options.allow_lan = true;
        request.options.ipv6 = false;
        let prepared = build(
            check(request).unwrap(),
            &placement(),
            SystemProxy::Forbidden,
            fixed_port(41234),
            &SECRET,
        )
        .unwrap();
        let config: Value = serde_json::from_str(prepared.config()).unwrap();
        assert_eq!(config["inbounds"][0]["address"], json!(["172.18.0.1/30"]));
        assert_eq!(config["inbounds"][1]["listen"], "0.0.0.0");
        assert_eq!(config["services"][0]["listen"], "127.0.0.1");
    }

    #[test]
    fn refusals_come_back_whole() {
        let refusals = check(start(
            json!({
                "inbounds": [],
                "outbounds": [{"type": "tor", "tag": "t", "executable_path": "C:\\x.exe"}]
            }),
            &[],
        ))
        .unwrap_err();
        assert_eq!(
            refusals
                .iter()
                .map(|r| (r.pointer.as_str(), r.kind.clone()))
                .collect::<Vec<_>>(),
            boxpilot_policy::check(
                &json!({
                    "inbounds": [],
                    "outbounds": [{"type": "tor", "tag": "t", "executable_path": "C:\\x.exe"}]
                })
                .to_string(),
                &Default::default(),
                &Limits::default()
            )
            .unwrap_err()
            .iter()
            .map(|r| (r.pointer.as_str(), r.kind.clone()))
            .collect::<Vec<_>>()
        );
        assert!(refusals.iter().any(|r| r.kind == RefusalKind::Inbounds));
    }

    /// Only the attachments the config refers to are kept, under names the
    /// helper derives, never the caller's ids.
    #[test]
    fn only_referenced_attachments_are_written_under_derived_names() {
        let (config, prepared) = built(
            json!({
                "route": {"rule_set": [{
                    "type": "local",
                    "tag": "geo",
                    "format": "binary",
                    "path": "boxpilot-attachment:geo"
                }]}
            }),
            &[("geo", b"rules"), ("unused", b"never written")],
        );
        assert_eq!(
            prepared.files(),
            [("attachment-67656f".to_owned(), b"rules".to_vec())]
        );
        assert_eq!(
            config["route"]["rule_set"][0]["path"],
            r"C:\State\runs\r1\attachment-67656f"
        );
    }

    #[test]
    fn the_api_port_avoids_the_proxy_and_the_configs_listeners() {
        // The policy drops the profile's own controllers, so only the proxy
        // port is left to avoid here; the rule is the GUI's.
        let checked = check(start(json!({}), &[])).unwrap();
        let mut draws = [7890u16, 41235].into_iter();
        let prepared = build(
            checked,
            &placement(),
            SystemProxy::Forbidden,
            || Ok((draws.next().unwrap(), ())),
            &SECRET,
        )
        .unwrap();
        assert_eq!(prepared.api().port(), 41235);

        let checked = check(start(json!({}), &[])).unwrap();
        let error = build(
            checked,
            &placement(),
            SystemProxy::Forbidden,
            fixed_port(7890),
            &SECRET,
        )
        .unwrap_err();
        assert!(matches!(error, BuildError::Port(e) if e.kind() == io::ErrorKind::AddrInUse));
    }

    /// The policy names the file; the helper only checks that the name is
    /// a plain one directly in the run directory, whatever the separator.
    #[test]
    fn attachment_names_are_plain_and_in_the_run_directory() {
        let mut odd = placement();
        odd.run_dir = r"C:\State\runs\r1\\".into();
        assert_eq!(
            attachment_file_name(&odd, "geo").unwrap(),
            "attachment-67656f",
            "a trailing separator changes nothing"
        );
        let mut other = placement();
        other.separator = '/';
        assert!(matches!(
            attachment_file_name(&other, "geo"),
            Ok(name) if name == "attachment-67656f"
        ));
    }

    #[test]
    fn debug_shows_no_content() {
        let checked = check(start(json!({"log": {"level": "secret-ish"}}), &[])).unwrap();
        assert!(!format!("{checked:?}").contains("secret-ish"));
        let prepared = build(
            checked,
            &placement(),
            SystemProxy::Forbidden,
            fixed_port(41234),
            &SECRET,
        )
        .unwrap();
        let debug = format!("{prepared:?}");
        assert!(!debug.contains("secret-ish"));
        assert!(!debug.contains(&"ab".repeat(SECRET_LEN)));
    }

    #[test]
    fn secrets_and_ports_come_from_the_os() {
        assert_ne!(fresh_secret().unwrap(), fresh_secret().unwrap());
        let (port, listener) = free_loopback_port().unwrap();
        assert_ne!(port, 0);
        assert_eq!(listener.local_addr().unwrap().port(), port);
    }
}
