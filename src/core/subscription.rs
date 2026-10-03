use crate::core::atomic_write::{
    stage_atomic, unique_suffix, write_atomic, FileAccess, StagedFile,
};
use crate::core::paths::create_private_dir;
use crate::core::settings::{AppSettings, HTTP_TIMEOUT_SECS, PROXY_PORT};
use crate::core::singbox_api::{is_boxpilot_api_service, SingBoxApi, API_SERVICE_TAG};
use crate::core::sub_usage::{parse_userinfo, SubscriptionUsage, USERINFO_HEADER};
use crate::core::timefmt::to_unix_secs;
use reqwest::blocking::Client;
use serde_json::Value;
use std::fs;
use std::io;
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Subscription User-Agent. Servers sniff the literal `sing-box` token to
/// decide whether to serve sing-box JSON or Clash YAML, and read the version
/// after it to gate config-format features — so the token is always present,
/// and the advertised version is the real one whenever startup detection got
/// it. `None` (binary missing/unreadable) falls back to the historical
/// string, which fills the slot with BoxPilot's own version.
pub fn user_agent(sing_box_version: Option<&str>) -> String {
    const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
    format!(
        "BoxPilot/{APP_VERSION} ({APP_VERSION}; sing-box {})",
        sing_box_version.unwrap_or(APP_VERSION)
    )
}

/// Result of a subscription fetch / local import.
///
/// `Changed` carries the new, validated config staged next to the profile's
/// `configs/<id>.json` but not yet in place: the fetch runs on the
/// background executor and can't know whether its profile still exists, so
/// the caller checks that on the UI thread and then `commit`s, or drops the
/// outcome to discard it. `Unchanged` means the fetched config was
/// byte-identical to what's already on disk; nothing was written.
#[derive(Debug)]
pub enum UpdateOutcome {
    Changed(StagedFile),
    Unchanged,
}

impl UpdateOutcome {
    /// Land the outcome: move a `Changed` config into place. `Ok(true)` when
    /// the profile's config changed, `Ok(false)` when it was already current.
    pub fn commit(self) -> Result<bool, String> {
        match self {
            UpdateOutcome::Changed(staged) => {
                let target = staged.target().to_path_buf();
                staged
                    .commit()
                    .map_err(|e| format!("Failed to write config ({}): {}", target.display(), e))?;
                Ok(true)
            }
            UpdateOutcome::Unchanged => Ok(false),
        }
    }
}

/// A successful subscription fetch: the config outcome, plus the traffic /
/// expiry the server reported in its `subscription-userinfo` header (`None`
/// when it sent none, or nothing usable). Carried on `Unchanged` too: usage
/// moves even when the config doesn't.
#[derive(Debug)]
pub struct Fetched {
    pub outcome: UpdateOutcome,
    pub usage: Option<SubscriptionUsage>,
}

/// Strip BoxPilot-managed sections from config (used when saving subscription
/// data). BoxPilot owns `inbounds`: `prepare_config` injects them for the
/// current mode at process start, so the canonical on-disk form has none and
/// the subscription's own are discarded. BoxPilot's own `api` service goes
/// too, should the input carry one (`is_boxpilot_api_service`), so
/// `prepare_config` never adds a second. Everything else stays as the
/// config wrote it, its own controllers included: its `api` services and its
/// whole `experimental` (clash_api, v2ray_api, cache_file), which
/// `prepare_config` merges into rather than replaces (ADR 0002). A `services`
/// array left empty is removed, so strip ∘ prepare gives back the canonical
/// form, short of the `cache_file.enabled` that prepare forces on.
pub fn strip_inbounds(config_data: &str) -> Result<String, String> {
    let mut json: Value = serde_json::from_str(config_data)
        .map_err(|e| format!("Failed to parse config JSON: {}", e))?;
    let obj = json
        .as_object_mut()
        .ok_or_else(|| "Config is not a JSON object".to_string())?;
    obj.remove("inbounds");
    if let Some(services) = obj.get_mut("services").and_then(Value::as_array_mut) {
        services.retain(|service| !is_boxpilot_api_service(service));
        if services.is_empty() {
            obj.remove("services");
        }
    }
    serde_json::to_string_pretty(&json)
        .map_err(|e| format!("Failed to serialize config: {}", e))
}

/// The TUN interface's IPv4 address — always present in TUN mode. Public so
/// `core::lan` can leave the TUN's own /30 out of the LAN addresses.
pub const TUN_IPV4_ADDRESS: &str = "172.18.0.1/30";
/// Added to the TUN interface only when `RuntimeOptions::tun_ipv6` is on.
const TUN_IPV6_ADDRESS: &str = "fdfe:dcba:9876::1/126";

/// Everything `prepare_config` needs to turn a canonical config into the form
/// sing-box actually runs. Grouped into one struct so the injected shape can
/// keep growing (more TUN knobs are likely) without every call site
/// maintaining a row of bare booleans in the right order.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RuntimeOptions {
    /// `true` = Proxy mode (mixed inbound only), `false` = TUN mode.
    pub proxy_mode: bool,
    pub set_system_proxy: bool,
    pub proxy_port: u16,
    /// The sing-box API endpoint (port + secret) of the run this config is
    /// for: a port from `pick_api_port` and a fresh secret. The clients must
    /// use this same value, or sing-box rejects them.
    pub api: SingBoxApi,
    /// TUN mode only: give the TUN interface an IPv6 address so IPv6 traffic
    /// is routed into the tunnel. Off means the interface carries no IPv6
    /// route at all and IPv6 traffic leaves via the physical interface — this
    /// injects nothing into `dns`/`route`, which stay the subscription's.
    pub tun_ipv6: bool,
    /// Settings › Network "Allow LAN connections": the mixed inbound
    /// listens on every IPv4 interface (`0.0.0.0`) instead of loopback, in
    /// TUN and Proxy mode alike. The system proxy sing-box sets still points
    /// at `127.0.0.1` — it substitutes loopback for an unspecified listen
    /// address (`common/listener/listener.go`) — so `set_system_proxy` and
    /// `process::disable_system_proxy` are unaffected.
    pub allow_lan: bool,
}

impl RuntimeOptions {
    /// The options for one sing-box start: the user's settings, plus that
    /// start's API endpoint, whose `api` then goes to the clients.
    pub fn new(settings: &AppSettings, api: SingBoxApi) -> Self {
        Self {
            proxy_mode: settings.proxy_mode,
            set_system_proxy: settings.set_system_proxy,
            proxy_port: settings.proxy_port,
            api,
            tun_ipv6: settings.tun_ipv6,
            allow_lan: settings.allow_lan,
        }
    }
}

/// The tests' baseline: mirrors `AppSettings::default()` (TUN mode, default
/// proxy port, IPv6 off), with a fresh API secret on a fixed port.
#[cfg(test)]
impl Default for RuntimeOptions {
    fn default() -> Self {
        Self::new(&AppSettings::default(), SingBoxApi::new(tests::API_PORT))
    }
}

/// How many ports `pick_api_port` draws before giving up.
const API_PORT_PICK_ATTEMPTS: usize = 16;

/// The loopback port BoxPilot's `api` service gets for one sing-box start:
/// one the OS reports free right now, other than the local proxy port and the
/// config's own listener ports (`config_listen_ports`), which aren't bound
/// yet while sing-box is down. Nothing holds the port afterwards, so another
/// program can take it before sing-box binds; sing-box then fails to start
/// on it (`is_api_bind_failure`), and `AppState` starts once more, which
/// picks again (`ApiPortRetry`).
pub fn pick_api_port(config_data: &str, proxy_port: u16) -> Result<u16, String> {
    let json: Value = serde_json::from_str(config_data)
        .map_err(|e| format!("Failed to parse config JSON: {}", e))?;
    let mut excluded = config_listen_ports(&json);
    excluded.push(proxy_port);
    pick_port_avoiding(&excluded, free_loopback_port)
        .map_err(|e| format!("Failed to find a free port for the sing-box API: {}", e))
}

/// A port the OS hands out for `127.0.0.1:0`, with the listener that holds
/// it.
fn free_loopback_port() -> io::Result<(u16, TcpListener)> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    Ok((listener.local_addr()?.port(), listener))
}

/// Draw ports from `next` until one is not `excluded`. Each rejected draw's
/// holder stays alive until the end, so the OS can't offer the same port
/// twice; the picked one's is dropped, freeing the port for sing-box.
fn pick_port_avoiding<H>(
    excluded: &[u16],
    mut next: impl FnMut() -> io::Result<(u16, H)>,
) -> io::Result<u16> {
    let mut rejected = Vec::new();
    for _ in 0..API_PORT_PICK_ATTEMPTS {
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

/// Ports the config's own listeners will take besides the inbounds BoxPilot
/// owns: every service's `listen_port` (its own `api` services among them),
/// and the `clash_api` / `v2ray_api` controller addresses.
fn config_listen_ports(json: &Value) -> Vec<u16> {
    let mut ports: Vec<u16> = json["services"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|service| service["listen_port"].as_u64())
        .filter_map(|port| u16::try_from(port).ok())
        .collect();
    let experimental = &json["experimental"];
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
fn address_port(address: &str) -> Option<u16> {
    address.rsplit_once(':')?.1.parse().ok()
}

/// Whether a line sing-box printed says it could not listen on `api_port`,
/// i.e. BoxPilot's `api` service lost its port (to another program, between
/// `pick_api_port` and the bind, or to a reserved range on Windows). sing-box
/// exits right after it: `FATAL[0000] start service: finish-start
/// service/api[boxpilot-api]: listen tcp 127.0.0.1:41234: bind: address
/// already in use`.
pub fn is_api_bind_failure(line: &str, api_port: u16) -> bool {
    line.contains(&format!("listen tcp 127.0.0.1:{}: bind: ", api_port))
}

/// The `services[]` tag for BoxPilot's `api` service: `API_SERVICE_TAG`, or
/// with a `-2`, `-3`, … suffix if the config's own services already use it.
fn api_service_tag(services: &[Value]) -> String {
    let taken = |tag: &str| services.iter().any(|service| service["tag"] == tag);
    std::iter::once(API_SERVICE_TAG.to_string())
        .chain((2..).map(|n| format!("{}-{}", API_SERVICE_TAG, n)))
        .find(|tag| !taken(tag))
        .expect("a free tag among infinitely many")
}

/// The object at `parent[key]`, created empty — or replacing a non-object —
/// if need be. `parent` must be an object.
fn object_entry<'a>(parent: &'a mut Value, key: &str) -> &'a mut Value {
    if !parent[key].is_object() {
        parent[key] = Value::Object(Default::default());
    }
    &mut parent[key]
}

/// Where the mixed inbound listens: every IPv4 interface when LAN
/// connections are allowed, loopback otherwise. IPv4 only, matching the
/// addresses the Settings hint lists (`core::lan`).
fn mixed_listen_address(allow_lan: bool) -> &'static str {
    if allow_lan {
        "0.0.0.0"
    } else {
        "127.0.0.1"
    }
}

/// Inject mode-specific inbounds into config (used at process start)
pub fn prepare_config(config_data: &str, opts: RuntimeOptions) -> Result<String, String> {
    let mut json: Value = serde_json::from_str(config_data)
        .map_err(|e| format!("Failed to parse config JSON: {}", e))?;
    if !json.is_object() {
        return Err("Config is not a JSON object".to_string());
    }

    let mut mixed_inbound = serde_json::json!({
        "type": "mixed",
        "tag": "proxy",
        "listen": mixed_listen_address(opts.allow_lan),
        "listen_port": opts.proxy_port
    });
    if opts.set_system_proxy {
        mixed_inbound["set_system_proxy"] = serde_json::Value::Bool(true);
    }

    let inbounds = if opts.proxy_mode {
        serde_json::Value::Array(vec![mixed_inbound])
    } else {
        let mut address = vec![serde_json::Value::from(TUN_IPV4_ADDRESS)];
        if opts.tun_ipv6 {
            address.push(serde_json::Value::from(TUN_IPV6_ADDRESS));
        }
        let tun_inbound = serde_json::json!({
            "type": "tun",
            "tag": "tun0",
            "address": address,
            "auto_route": true,
            "strict_route": true,
            "stack": "mixed"
        });
        serde_json::Value::Array(vec![tun_inbound, mixed_inbound])
    };

    json["inbounds"] = inbounds;

    // With cache_file enabled, sing-box (≥1.8) automatically persists the
    // chosen selector node across restarts (cache.db) — the old
    // `store_selected` field was removed upstream and now fails config
    // validation as an unknown field. Only `enabled` is forced: the rest of
    // `experimental` (the config's own clash_api / v2ray_api, its other
    // cache_file fields) runs as the config wrote it (ADR 0002).
    let experimental = object_entry(&mut json, "experimental");
    object_entry(experimental, "cache_file")["enabled"] = Value::Bool(true);

    // BoxPilot's own sing-box API service (≥1.14) for groups, node
    // switching, delay tests and the traffic readout: loopback, behind this
    // run's secret. The config's own services, its `api` ones included,
    // pass through untouched; ours takes a tag none of them uses, and a port
    // none of them listens on (`pick_api_port`).
    let mut services: Vec<Value> = json
        .get("services")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut ours = opts.api.service_config();
    ours["tag"] = Value::from(api_service_tag(&services));
    services.push(ours);
    json["services"] = Value::Array(services);

    serde_json::to_string_pretty(&json)
        .map_err(|e| format!("Failed to serialize config: {}", e))
}

/// Write a prepared runtime config. It carries the API secret, so on Unix
/// the file is owner-only (sing-box, the privileged TUN copy included, runs
/// as the same user); on Windows it inherits the per-user data folder's ACL.
/// Replaced atomically, and the new secret only ever lands in a 0600 file.
pub fn save_runtime_config(path: &Path, prepared: &str) -> io::Result<()> {
    write_atomic(path, prepared.as_bytes(), FileAccess::OwnerOnly)
}

/// Fetch the subscription, strip its inbounds, and stage it for `config_path`
/// (the profile's `configs/<id>.json`; see `UpdateOutcome`) — but only if
/// the result differs from what's already on disk. `app_dir` is still needed separately: it's where
/// the validation temp file goes so `sing-box check -D` resolves relative
/// resources exactly like at runtime.
///
/// Settings persistence is the caller's responsibility (`AppState::save_settings`),
/// so the caller doesn't risk clobbering settings fields not visible here —
/// that includes storing the returned `Fetched::usage`.
pub fn perform_update(
    sub_url: &str,
    app_dir: &Path,
    config_path: &Path,
    sing_box: Option<&Path>,
    sing_box_version: Option<&str>,
) -> Result<Fetched, String> {
    if !sub_url.starts_with("http://") && !sub_url.starts_with("https://") {
        return Err("Invalid URL: must start with http:// or https://".to_string());
    }

    let client = Client::builder()
        .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))?;

    let response = client
        .get(sub_url)
        .header("User-Agent", user_agent(sing_box_version))
        .send()
        .map_err(|e| {
            if e.is_timeout() {
                "Update timed out. Please try again.".to_string()
            } else {
                format!(
                    "Network error fetching subscription: {}",
                    http_error_text(e)
                )
            }
        })?;

    if !response.status().is_success() {
        return Err(format!(
            "Failed to download subscription. Status: {}",
            response.status()
        ));
    }

    // Before `text()`, which consumes the response.
    let now = to_unix_secs(SystemTime::now()).unwrap_or(0);
    let usage = response
        .headers()
        .get(USERINFO_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| parse_userinfo(value, now));

    let config_data = response.text().map_err(|e| {
        format!(
            "Failed to read subscription response: {}",
            http_error_text(e)
        )
    })?;

    let outcome = apply_config_text(&config_data, app_dir, config_path, sing_box)?;
    Ok(Fetched { outcome, usage })
}

/// A reqwest error for a message that reaches toasts and stderr. Its
/// `Display` ends in ` for url (<full url>)`, and the subscription URL often
/// carries an access token, so the URL goes. What's left ("error sending
/// request") says little, so the innermost cause follows (a DNS failure, a
/// refused connection, a TLS error), which names no URL.
fn http_error_text(e: reqwest::Error) -> String {
    use std::error::Error;
    let e = e.without_url();
    match std::iter::successors(e.source(), |&cause| cause.source()).last() {
        Some(cause) => format!("{} ({})", e, cause),
        None => e.to_string(),
    }
}

/// Shared tail once the raw config text is in hand: strip → unchanged-detection
/// → `sing-box check` validation → stage. Both the remote (HTTP) and local
/// (file) sources funnel through here.
fn apply_config_text(
    raw: &str,
    app_dir: &Path,
    config_path: &Path,
    sing_box: Option<&Path>,
) -> Result<UpdateOutcome, String> {
    let stripped = strip_inbounds(raw)?;

    // Compare against the on-disk config after running it through
    // `strip_inbounds` again. Configs written by this version are already
    // canonical (the prepared runtime form goes to `running_config.json`,
    // never back over the profile file), so the re-strip is usually a no-op —
    // it remains to tolerate files from older releases, which wrote the
    // inbounds-injected form back to this same path on every process start.
    // If parsing fails (corrupted file), fall through to overwrite.
    if let Ok(existing) = fs::read_to_string(config_path) {
        if let Ok(existing_stripped) = strip_inbounds(&existing) {
            if existing_stripped == stripped {
                return Ok(UpdateOutcome::Unchanged);
            }
        }
    }

    // New content — validate it with `sing-box check` before it clobbers the
    // good config, so a broken source never replaces a working one. Runs only
    // on the changed path (the early return above skips no-op ticks). Skipped
    // entirely when the binary is absent (macOS dev, pre-install) — we fall
    // back to the JSON-only checks above.
    if let Some(sing_box) = sing_box {
        if sing_box.exists() {
            validate_downloaded_config(sing_box, app_dir, config_path, &stripped)?;
        }
    }

    if let Some(parent) = config_path.parent() {
        create_private_dir(parent)
            .map_err(|e| format!("Failed to create {}: {}", parent.display(), e))?;
    }
    // Staged beside the target and renamed over it by `UpdateOutcome::commit`:
    // a crash mid-write must not corrupt the profile's (possibly active)
    // config, and a result nobody wants any more must not land at all.
    // Owner-only on Unix: it holds server passwords and UUIDs.
    let staged = stage_atomic(config_path, stripped.as_bytes(), FileAccess::OwnerOnly)
        .map_err(|e| format!("Failed to write config ({}): {}", config_path.display(), e))?;

    Ok(UpdateOutcome::Changed(staged))
}

/// Read a sing-box config from a local file and stage a snapshot for `config_path`
/// (a Local profile's create / ⟳). Same strip + validate + unchanged-detection
/// path as `perform_update`, just sourced from disk instead of HTTP.
pub fn import_local_config(
    source_path: &Path,
    app_dir: &Path,
    config_path: &Path,
    sing_box: Option<&Path>,
) -> Result<UpdateOutcome, String> {
    if !source_path.exists() {
        return Err(format!("File not found: {}", source_path.display()));
    }
    if !source_path.is_file() {
        return Err(format!("Not a file: {}", source_path.display()));
    }
    let raw = fs::read_to_string(source_path)
        .map_err(|e| format!("Failed to read {}: {}", source_path.display(), e))?;
    apply_config_text(&raw, app_dir, config_path, sing_box)
}

/// Validate freshly-downloaded (stripped) config before it overwrites the good
/// `config.json`. We inject a minimal mixed inbound via `prepare_config` so we
/// validate the exact shape sing-box actually runs — a config with no inbounds
/// is an edge case `sing-box check` might reject for reasons unrelated to the
/// subscription content. The injected inbound is our own trusted output, and
/// the subscription's outbounds/route/dns are identical across proxy modes, so
/// validating the mixed shape is sufficient. The temp file is written into
/// `app_dir` (so `-D` resolves relative resources like at runtime) and always
/// removed afterwards; its name is unique per call (see
/// `validation_temp_path`). `config_path` is only used to name it.
fn validate_downloaded_config(
    sing_box: &Path,
    app_dir: &Path,
    config_path: &Path,
    stripped: &str,
) -> Result<(), String> {
    // 校验用的入站端口和 API secret 与运行时无关,用默认值即可;API 端口照
    // 运行时的规则挑(`check` 不监听,只求和配置自己的端口不撞);proxy_mode
    // 显式设 true,校验的就是注释里说的那个 mixed 形态。
    let api = SingBoxApi::new(pick_api_port(stripped, PROXY_PORT)?);
    let prepared = prepare_config(
        stripped,
        RuntimeOptions {
            proxy_mode: true,
            ..RuntimeOptions::new(&AppSettings::default(), api)
        },
    )?;
    let tmp_path = validation_temp_path(app_dir, config_path);
    write_atomic(&tmp_path, prepared.as_bytes(), FileAccess::OwnerOnly)
        .map_err(|e| format!("Failed to write validation temp file: {}", e))?;
    let result = crate::core::process::validate_config(sing_box, app_dir, &tmp_path);
    let _ = fs::remove_file(&tmp_path);
    result
}

/// `<app_dir>/config_check-<profile id>-<unique>.tmp`. Fetches can overlap (an
/// import takes over from a fetch whose blocking work still runs to
/// completion), and
/// with one shared name a run could check or delete the other's file: a
/// false pass that lets a broken config replace a good one, or a spurious
/// failure.
fn validation_temp_path(app_dir: &Path, config_path: &Path) -> PathBuf {
    let profile = config_path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    app_dir.join(format!("config_check-{}-{}.tmp", profile, unique_suffix()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// reqwest's `Display` ends in ` for url (<full url>)`; the token in the
    /// query must not reach the message.
    #[test]
    fn http_errors_name_no_url() {
        let client = Client::builder().no_proxy().build().unwrap();
        // Port 1 on loopback: refused at once, no network involved.
        let err = client
            .get("http://127.0.0.1:1/sub/Xk2fP9qLm7RtW3vZ?token=secret")
            .send()
            .unwrap_err();
        assert!(err.to_string().contains("secret"), "precondition: {}", err);
        let text = http_error_text(err);
        assert!(!text.contains("secret"), "{}", text);
        assert!(!text.contains("Xk2fP9qLm7RtW3vZ"), "{}", text);
        assert!(text.starts_with("error sending request ("), "{}", text);
    }

    use crate::core::singbox_api::is_boxpilot_api_service;

    /// The fixed API port of `RuntimeOptions::default()`.
    pub(super) const API_PORT: u16 = 7789;

    const SUB_CONFIG: &str = r#"{
        "log": {"level": "info"},
        "dns": {"servers": [{"tag": "remote", "address": "8.8.8.8"}]},
        "inbounds": [{"type": "tun", "tag": "upstream-tun"}],
        "outbounds": [{"type": "vless", "tag": "proxy-out"}],
        "route": {"rules": []},
        "experimental": {"clash_api": {"external_controller": "0.0.0.0:9090"}}
    }"#;

    /// A config with controllers of its own: an `api` service under
    /// BoxPilot's tag, a clash_api, a v2ray_api, and a cache_file with
    /// `enabled` off.
    const CONTROLLED_CONFIG: &str = r#"{
        "inbounds": [{"type": "mixed", "tag": "upstream-mixed", "listen_port": 2080}],
        "outbounds": [{"type": "direct", "tag": "direct"}],
        "services": [
            {"type": "resolved", "tag": "resolved", "listen_port": 53},
            {"type": "api", "tag": "boxpilot-api", "listen": "0.0.0.0", "listen_port": 9091}
        ],
        "experimental": {
            "clash_api": {"external_controller": "127.0.0.1:9090", "secret": "theirs"},
            "v2ray_api": {"listen": "127.0.0.1:8080", "stats": {"enabled": true}},
            "cache_file": {"enabled": false, "path": "custom.db", "store_rdrc": true}
        }
    }"#;

    /// BoxPilot's own service in a prepared config.
    fn our_service(prepared: &Value) -> &Value {
        let ours: Vec<&Value> = prepared["services"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|service| is_boxpilot_api_service(service))
            .collect();
        assert_eq!(ours.len(), 1, "exactly one BoxPilot api service");
        ours[0]
    }

    fn parse(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    /// Proxy mode (mixed inbound only) on default ports — the shape most of
    /// these tests assert against. Compose with `..proxy_opts()` to vary one
    /// field.
    fn proxy_opts() -> RuntimeOptions {
        RuntimeOptions {
            proxy_mode: true,
            ..Default::default()
        }
    }

    #[test]
    fn user_agent_advertises_real_sing_box_version() {
        let v = env!("CARGO_PKG_VERSION");
        assert_eq!(
            user_agent(Some("1.11.15")),
            format!("BoxPilot/{v} ({v}; sing-box 1.11.15)")
        );
    }

    /// Unknown version → byte-identical to the historical compile-time UA,
    /// so servers that sniff it see zero change.
    #[test]
    fn user_agent_falls_back_to_historical_string() {
        let v = env!("CARGO_PKG_VERSION");
        assert_eq!(user_agent(None), format!("BoxPilot/{v} ({v}; sing-box {v})"));
    }

    #[test]
    fn strip_removes_inbounds_and_preserves_everything_else() {
        let stripped = strip_inbounds(SUB_CONFIG).unwrap();
        let json = parse(&stripped);
        assert!(json.get("inbounds").is_none());
        for key in ["log", "dns", "outbounds", "route"] {
            assert_eq!(json[key], parse(SUB_CONFIG)[key], "{} must survive strip", key);
        }
    }

    #[test]
    fn strip_is_idempotent() {
        let once = strip_inbounds(SUB_CONFIG).unwrap();
        let twice = strip_inbounds(&once).unwrap();
        assert_eq!(once, twice);
    }

    /// The config's own controllers are not BoxPilot's to strip.
    #[test]
    fn strip_keeps_the_configs_own_controllers() {
        let json = parse(&strip_inbounds(CONTROLLED_CONFIG).unwrap());
        let original = parse(CONTROLLED_CONFIG);
        assert!(json.get("inbounds").is_none());
        assert_eq!(json["experimental"], original["experimental"]);
        assert_eq!(json["services"], original["services"]);
    }

    /// With no `experimental` of its own, a config gets just cache_file, so
    /// sing-box persists the user's selector choices across restarts and
    /// subscription updates (automatic when enabled — sing-box ≥1.8 removed
    /// `store_selected` and rejects it as an unknown field, so we must NOT
    /// inject it).
    #[test]
    fn prepare_injects_cache_file() {
        let config = r#"{"outbounds": [{"type": "direct", "tag": "direct"}]}"#;
        let prepared = parse(&prepare_config(config, proxy_opts()).unwrap());
        assert_eq!(
            prepared["experimental"],
            serde_json::json!({"cache_file": {"enabled": true}}),
            "store_selected was removed upstream; injecting it fails sing-box config validation"
        );
    }

    /// `experimental` is merged, not replaced: the config's clash_api,
    /// v2ray_api and cache_file fields stay, and only `cache_file.enabled`
    /// is forced on.
    #[test]
    fn prepare_merges_experimental() {
        let prepared = parse(&prepare_config(CONTROLLED_CONFIG, proxy_opts()).unwrap());
        let original = parse(CONTROLLED_CONFIG);
        let experimental = &prepared["experimental"];
        assert_eq!(
            experimental["clash_api"],
            original["experimental"]["clash_api"]
        );
        assert_eq!(
            experimental["v2ray_api"],
            original["experimental"]["v2ray_api"]
        );
        assert_eq!(
            experimental["cache_file"],
            serde_json::json!({"enabled": true, "path": "custom.db", "store_rdrc": true})
        );

        // A malformed `experimental` / `cache_file` gives way to a working one.
        for config in [
            r#"{"experimental": null}"#,
            r#"{"experimental": {"cache_file": true, "clash_api": {}}}"#,
        ] {
            let prepared = parse(&prepare_config(config, proxy_opts()).unwrap());
            assert_eq!(
                prepared["experimental"]["cache_file"]["enabled"], true,
                "{}",
                config
            );
        }
    }

    #[test]
    fn prepare_injects_api_service_on_loopback() {
        let stripped = strip_inbounds(SUB_CONFIG).unwrap();
        let prepared = parse(&prepare_config(&stripped, proxy_opts()).unwrap());
        let services = prepared["services"].as_array().unwrap();
        assert_eq!(services.len(), 1);
        assert_eq!(services[0]["type"], "api");
        assert_eq!(services[0]["tag"], API_SERVICE_TAG);
        assert_eq!(services[0]["listen"], "127.0.0.1");
        assert_eq!(services[0]["listen_port"], API_PORT);
    }

    /// The injected service is exactly the one `opts.api` describes — its
    /// secret included — so the clients holding that `api` get in.
    #[test]
    fn prepare_injects_the_runs_api_secret() {
        let opts = proxy_opts();
        let prepared = parse(&prepare_config(SUB_CONFIG, opts).unwrap());
        let service = &prepared["services"][0];
        assert_eq!(*service, opts.api.service_config());
        assert!(!service["secret"].as_str().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn runtime_config_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = sub_temp_dir("runtime_mode");
        let path = dir.join("running_config.json");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        save_runtime_config(&path, "{}").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "{}");
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = fs::remove_dir_all(&dir);
    }

    /// The options carry the settings and exactly the `api` they were
    /// given, which is what the clients get.
    #[test]
    fn runtime_options_carry_settings_and_the_given_api() {
        let settings = AppSettings {
            proxy_mode: true,
            set_system_proxy: true,
            proxy_port: 18888,
            tun_ipv6: true,
            allow_lan: true,
            ..AppSettings::default()
        };
        let api = SingBoxApi::new(41234);
        let opts = RuntimeOptions::new(&settings, api);
        assert_eq!(
            opts,
            RuntimeOptions {
                proxy_mode: true,
                set_system_proxy: true,
                proxy_port: 18888,
                api,
                tun_ipv6: true,
                allow_lan: true,
            }
        );
    }

    /// The config's own `api` service stays as written, next to ours; its
    /// other services survive in order, and ours comes last.
    #[test]
    fn prepare_keeps_the_configs_api_service_next_to_ours() {
        let config = r#"{
            "outbounds": [{"type": "direct", "tag": "direct"}],
            "services": [
                {"type": "resolved", "tag": "resolved"},
                {"type": "api", "tag": "api", "listen": "0.0.0.0", "listen_port": 9090},
                {"type": "derp", "tag": "derp"}
            ]
        }"#;
        let opts = proxy_opts();
        let prepared = parse(&prepare_config(config, opts).unwrap());
        let services = prepared["services"].as_array().unwrap();
        assert_eq!(
            services[..3],
            parse(config)["services"].as_array().unwrap()[..]
        );
        assert_eq!(services.len(), 4);
        assert_eq!(services[3], opts.api.service_config());
    }

    /// The config's own service under BoxPilot's tag keeps it; ours takes
    /// the first free suffix.
    #[test]
    fn prepare_resolves_a_tag_collision() {
        let prepared = parse(&prepare_config(CONTROLLED_CONFIG, proxy_opts()).unwrap());
        assert_eq!(prepared["services"][1]["tag"], API_SERVICE_TAG);
        assert_eq!(prepared["services"][1]["listen_port"], 9091);
        assert_eq!(our_service(&prepared)["tag"], "boxpilot-api-2");

        let config = r#"{"services": [
            {"type": "api", "tag": "boxpilot-api-2"},
            {"type": "derp", "tag": "boxpilot-api"}
        ]}"#;
        let prepared = parse(&prepare_config(config, proxy_opts()).unwrap());
        assert_eq!(our_service(&prepared)["tag"], "boxpilot-api-3");
    }

    /// Only BoxPilot's own service is stripped (say from an imported
    /// runtime config, whose secret is dead); the config's own `api`
    /// services stay, whatever their tag.
    #[test]
    fn strip_removes_only_boxpilot_api_service() {
        let runtime = prepare_config(CONTROLLED_CONFIG, proxy_opts()).unwrap();
        let json = parse(&strip_inbounds(&runtime).unwrap());
        assert_eq!(json["services"], parse(CONTROLLED_CONFIG)["services"]);

        let only_ours = prepare_config(r#"{}"#, proxy_opts()).unwrap();
        assert!(parse(&strip_inbounds(&only_ours).unwrap())
            .get("services")
            .is_none());
    }

    #[test]
    fn config_listen_ports_cover_services_and_controllers() {
        let mut ports = config_listen_ports(&parse(CONTROLLED_CONFIG));
        ports.sort_unstable();
        // The upstream inbound's 2080 is not among them: BoxPilot owns inbounds.
        assert_eq!(ports, vec![53, 8080, 9090, 9091]);
        assert!(config_listen_ports(&parse("{}")).is_empty());
        assert_eq!(address_port("[::]:9090"), Some(9090));
        assert_eq!(address_port(":9090"), Some(9090));
        assert_eq!(address_port("0.0.0.0"), None);
        assert_eq!(address_port("127.0.0.1:"), None);
    }

    /// Excluded draws are skipped, and their holders kept until the pick,
    /// so the OS can't offer the same port again.
    #[test]
    fn port_picking_skips_excluded_ports() {
        use std::cell::RefCell;
        use std::rc::Rc;
        let held = Rc::new(RefCell::new(Vec::new()));
        let draws = [9090u16, 7788, 41234, 41235];
        let mut drawn = 0;
        let port = pick_port_avoiding(&[7788, 9090], || {
            assert_eq!(
                *held.borrow(),
                draws[..drawn],
                "rejected draws are still held"
            );
            let port = draws[drawn];
            drawn += 1;
            held.borrow_mut().push(port);
            Ok((port, HeldPort(port, held.clone())))
        })
        .unwrap();
        assert_eq!(port, 41234);
        assert_eq!(drawn, 3);
        assert!(
            held.borrow().is_empty(),
            "every holder is dropped by the end"
        );

        // A source that keeps offering excluded ports gives up.
        let err = pick_port_avoiding(&[9090], || Ok((9090, ()))).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
    }

    /// Stands in for the listener holding a drawn port: removes it from the
    /// shared list of held ports when dropped.
    struct HeldPort(u16, std::rc::Rc<std::cell::RefCell<Vec<u16>>>);

    impl Drop for HeldPort {
        fn drop(&mut self) {
            self.1.borrow_mut().retain(|&port| port != self.0);
        }
    }

    /// The real picker: a free loopback port, never one the config or the
    /// proxy uses.
    #[test]
    fn picked_api_port_is_free_and_not_the_configs() {
        let port = pick_api_port(CONTROLLED_CONFIG, PROXY_PORT).unwrap();
        assert!(![53, 8080, 9090, 9091, PROXY_PORT].contains(&port));
        TcpListener::bind((Ipv4Addr::LOCALHOST, port)).expect("the port is free");
        assert!(pick_api_port("not json", PROXY_PORT).is_err());
    }

    #[test]
    fn api_bind_failure_names_our_port_only() {
        let line = "FATAL[0000] start service: finish-start service/api[boxpilot-api]: \
                    listen tcp 127.0.0.1:41234: bind: address already in use";
        assert!(is_api_bind_failure(line, 41234));
        assert!(!is_api_bind_failure(line, 4123));
        let windows = "FATAL[0000] start service: finish-start service/api[boxpilot-api]: \
                       listen tcp 127.0.0.1:41234: bind: An attempt was made to access a \
                       socket in a way forbidden by its access permissions.";
        assert!(is_api_bind_failure(windows, 41234));
        let proxy = "FATAL[0000] start inbound/mixed[proxy]: listen tcp 127.0.0.1:7788: \
                     bind: address already in use";
        assert!(!is_api_bind_failure(proxy, 41234));
    }

    #[test]
    fn strip_rejects_non_object_and_invalid_json() {
        assert!(strip_inbounds("[1, 2]").is_err());
        assert!(strip_inbounds("not json").is_err());
    }

    #[test]
    fn proxy_mode_injects_single_mixed_inbound() {
        let stripped = strip_inbounds(SUB_CONFIG).unwrap();
        let prepared = parse(&prepare_config(&stripped, proxy_opts()).unwrap());
        let inbounds = prepared["inbounds"].as_array().unwrap();
        assert_eq!(inbounds.len(), 1);
        let mixed = &inbounds[0];
        assert_eq!(mixed["type"], "mixed");
        assert_eq!(mixed["listen"], "127.0.0.1");
        assert_eq!(mixed["listen_port"], PROXY_PORT);
        assert!(mixed.get("set_system_proxy").is_none());
    }

    #[test]
    fn prepare_uses_custom_proxy_port() {
        let stripped = strip_inbounds(SUB_CONFIG).unwrap();
        let prepared = parse(
            &prepare_config(
                &stripped,
                RuntimeOptions {
                    proxy_port: 18888,
                    ..proxy_opts()
                },
            )
            .unwrap(),
        );
        assert_eq!(prepared["inbounds"][0]["listen_port"], 18888);
    }

    #[test]
    fn prepare_uses_custom_api_port() {
        let stripped = strip_inbounds(SUB_CONFIG).unwrap();
        let prepared = parse(
            &prepare_config(
                &stripped,
                RuntimeOptions {
                    api: SingBoxApi::new(17900),
                    ..proxy_opts()
                },
            )
            .unwrap(),
        );
        assert_eq!(prepared["services"][0]["listen_port"], 17900);
    }

    #[test]
    fn system_proxy_flag_is_injected_only_when_enabled() {
        let stripped = strip_inbounds(SUB_CONFIG).unwrap();
        let prepared = parse(
            &prepare_config(
                &stripped,
                RuntimeOptions {
                    set_system_proxy: true,
                    ..proxy_opts()
                },
            )
            .unwrap(),
        );
        assert_eq!(prepared["inbounds"][0]["set_system_proxy"], true);
    }

    /// "Allow LAN connections" moves the mixed inbound from loopback to
    /// every IPv4 interface, in both modes; nothing else about the inbounds
    /// changes, and the TUN inbound never gets a listen address.
    #[test]
    fn allow_lan_sets_the_mixed_listen_address_in_both_modes() {
        let stripped = strip_inbounds(SUB_CONFIG).unwrap();
        for proxy_mode in [true, false] {
            for (allow_lan, listen) in [(false, "127.0.0.1"), (true, "0.0.0.0")] {
                let prepared = parse(
                    &prepare_config(
                        &stripped,
                        RuntimeOptions {
                            proxy_mode,
                            allow_lan,
                            ..Default::default()
                        },
                    )
                    .unwrap(),
                );
                let inbounds = prepared["inbounds"].as_array().unwrap();
                let mixed = inbounds.last().unwrap();
                assert_eq!(mixed["type"], "mixed");
                assert_eq!(
                    mixed["listen"], listen,
                    "proxy_mode={proxy_mode}, allow_lan={allow_lan}"
                );
                assert_eq!(mixed["listen_port"], PROXY_PORT);
                if !proxy_mode {
                    assert_eq!(inbounds[0]["type"], "tun");
                    assert!(inbounds[0].get("listen").is_none());
                }
            }
        }
    }

    /// LAN access and the system proxy combine: sing-box still sets the
    /// system proxy (to 127.0.0.1, its stand-in for an unspecified listen
    /// address), so the flag is injected exactly as without LAN access.
    #[test]
    fn allow_lan_keeps_the_system_proxy_flag() {
        let stripped = strip_inbounds(SUB_CONFIG).unwrap();
        for proxy_mode in [true, false] {
            let prepared = parse(
                &prepare_config(
                    &stripped,
                    RuntimeOptions {
                        proxy_mode,
                        set_system_proxy: true,
                        allow_lan: true,
                        ..Default::default()
                    },
                )
                .unwrap(),
            );
            let mixed = prepared["inbounds"].as_array().unwrap().last().unwrap();
            assert_eq!(mixed["listen"], "0.0.0.0");
            assert_eq!(mixed["set_system_proxy"], true);
        }
    }

    /// BoxPilot's own sing-box API stays on loopback whatever the LAN
    /// setting: only the proxy is shared.
    #[test]
    fn allow_lan_leaves_the_api_service_on_loopback() {
        let stripped = strip_inbounds(SUB_CONFIG).unwrap();
        let prepared = parse(
            &prepare_config(
                &stripped,
                RuntimeOptions {
                    allow_lan: true,
                    ..proxy_opts()
                },
            )
            .unwrap(),
        );
        assert_eq!(prepared["services"][0]["listen"], "127.0.0.1");
    }

    #[test]
    fn tun_mode_injects_tun_then_mixed() {
        let stripped = strip_inbounds(SUB_CONFIG).unwrap();
        let prepared = parse(&prepare_config(&stripped, RuntimeOptions::default()).unwrap());
        let inbounds = prepared["inbounds"].as_array().unwrap();
        assert_eq!(inbounds.len(), 2);
        assert_eq!(inbounds[0]["type"], "tun");
        assert_eq!(inbounds[0]["auto_route"], true);
        assert_eq!(inbounds[0]["strict_route"], true);
        assert_eq!(inbounds[1]["type"], "mixed");
    }

    /// IPv6 on: the TUN interface gets both addresses, so IPv6 traffic is
    /// routed into the tunnel.
    #[test]
    fn tun_ipv6_on_injects_both_addresses() {
        let stripped = strip_inbounds(SUB_CONFIG).unwrap();
        let prepared = parse(
            &prepare_config(
                &stripped,
                RuntimeOptions {
                    tun_ipv6: true,
                    ..Default::default()
                },
            )
            .unwrap(),
        );
        assert_eq!(
            prepared["inbounds"][0]["address"],
            serde_json::json!([TUN_IPV4_ADDRESS, TUN_IPV6_ADDRESS])
        );
    }

    /// IPv6 off (the default): IPv4 only. The address is simply absent — we
    /// never inject a block rule or touch `dns`/`route` to compensate.
    #[test]
    fn tun_ipv6_off_injects_ipv4_only() {
        let stripped = strip_inbounds(SUB_CONFIG).unwrap();
        let prepared = parse(&prepare_config(&stripped, RuntimeOptions::default()).unwrap());
        assert_eq!(
            prepared["inbounds"][0]["address"],
            serde_json::json!([TUN_IPV4_ADDRESS])
        );
    }

    /// The toggle is TUN-only: Proxy mode injects a lone mixed inbound either
    /// way, with no TUN inbound to carry an address.
    #[test]
    fn tun_ipv6_does_not_affect_proxy_mode() {
        let stripped = strip_inbounds(SUB_CONFIG).unwrap();
        // One options value for both, so the API secret matches too.
        let opts = proxy_opts();
        let with = prepare_config(
            &stripped,
            RuntimeOptions {
                tun_ipv6: true,
                ..opts
            },
        )
        .unwrap();
        let without = prepare_config(&stripped, opts).unwrap();
        assert_eq!(with, without);
    }

    /// The subscription's own inbounds must be replaced, never merged —
    /// `start_process` relies on the active config containing exactly the
    /// inbounds BoxPilot injected.
    #[test]
    fn prepare_replaces_existing_inbounds() {
        let prepared = parse(&prepare_config(SUB_CONFIG, proxy_opts()).unwrap());
        let inbounds = prepared["inbounds"].as_array().unwrap();
        assert_eq!(inbounds.len(), 1);
        assert_eq!(inbounds[0]["tag"], "proxy");
        assert!(!prepared["inbounds"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["tag"] == "upstream-tun"));
    }

    /// `perform_update` detects "unchanged" by re-stripping the on-disk
    /// config and comparing to the freshly stripped download. Current
    /// releases keep the profile file canonical, but files from older
    /// releases carry mode-specific inbounds injected at process start —
    /// tolerating them requires strip ∘ prepare to give back the canonical
    /// form, for every mode combination. The one thing prepare adds that
    /// strip keeps is `cache_file.enabled`, which canonical configs that
    /// already have it on can't tell apart; and a config strip gives back
    /// prepares to exactly one BoxPilot service again.
    #[test]
    fn strip_after_prepare_recovers_canonical_form() {
        let with_cache_on = |config: &str| {
            let mut json = parse(config);
            object_entry(object_entry(&mut json, "experimental"), "cache_file")["enabled"] =
                Value::Bool(true);
            serde_json::to_string_pretty(&json).unwrap()
        };
        for source in [SUB_CONFIG, CONTROLLED_CONFIG] {
            let canonical = strip_inbounds(&with_cache_on(source)).unwrap();
            for proxy_mode in [true, false] {
                for set_system_proxy in [true, false] {
                    for tun_ipv6 in [true, false] {
                        for allow_lan in [true, false] {
                            let opts = RuntimeOptions {
                                proxy_mode,
                                set_system_proxy,
                                tun_ipv6,
                                allow_lan,
                                ..Default::default()
                            };
                            let on_disk = prepare_config(&canonical, opts).unwrap();
                            let recovered = strip_inbounds(&on_disk).unwrap();
                            assert_eq!(
                                recovered, canonical,
                                "strip(prepare(x, proxy_mode={}, system_proxy={}, tun_ipv6={}, allow_lan={})) must equal x",
                                proxy_mode, set_system_proxy, tun_ipv6, allow_lan
                            );
                            assert_eq!(prepare_config(&recovered, opts).unwrap(), on_disk);
                        }
                    }
                }
            }
            // With cache_file off, strip ∘ prepare only turns it on.
            let canonical = strip_inbounds(source).unwrap();
            let on_disk = prepare_config(&canonical, proxy_opts()).unwrap();
            assert_eq!(strip_inbounds(&on_disk).unwrap(), with_cache_on(&canonical));
        }
    }

    fn sub_temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("box_pilot_sub_{}_{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn import_local_writes_stripped_snapshot() {
        let dir = sub_temp_dir("import_ok");
        let src = dir.join("source.json");
        fs::write(&src, SUB_CONFIG).unwrap();
        let config_path = dir.join("configs").join("p1.json");
        let outcome = import_local_config(&src, &dir, &config_path, None).unwrap();
        assert!(matches!(outcome, UpdateOutcome::Changed(_)));
        assert!(!config_path.exists(), "nothing lands before commit");
        assert_eq!(outcome.commit(), Ok(true));
        let json = parse(&fs::read_to_string(&config_path).unwrap());
        assert!(json.get("inbounds").is_none(), "inbounds must be stripped");
        assert_eq!(json["experimental"], parse(SUB_CONFIG)["experimental"]);
        assert_eq!(json["outbounds"], parse(SUB_CONFIG)["outbounds"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_local_second_identical_import_is_unchanged() {
        let dir = sub_temp_dir("import_unchanged");
        let src = dir.join("source.json");
        fs::write(&src, SUB_CONFIG).unwrap();
        let config_path = dir.join("configs").join("p1.json");
        import_local_config(&src, &dir, &config_path, None)
            .unwrap()
            .commit()
            .unwrap();
        let outcome = import_local_config(&src, &dir, &config_path, None).unwrap();
        assert!(matches!(outcome, UpdateOutcome::Unchanged));
        assert_eq!(outcome.commit(), Ok(false));
        let _ = fs::remove_dir_all(&dir);
    }

    /// A result that is dropped instead of committed (its profile was
    /// deleted, or a newer fetch took over) leaves nothing behind.
    #[test]
    fn dropped_import_leaves_no_files() {
        let dir = sub_temp_dir("import_dropped");
        let src = dir.join("source.json");
        fs::write(&src, SUB_CONFIG).unwrap();
        let config_path = dir.join("configs").join("p1.json");
        let outcome = import_local_config(&src, &dir, &config_path, None).unwrap();
        drop(outcome);
        assert_eq!(fs::read_dir(dir.join("configs")).unwrap().count(), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Concurrent validations (even of the same profile) never share a temp
    /// file, and it stays in `app_dir` so `-D` relative resources resolve.
    #[test]
    fn validation_temp_paths_are_unique_per_call() {
        let app_dir = Path::new("/data/BoxPilot");
        let config = app_dir.join("configs").join("p3.json");
        let a = validation_temp_path(app_dir, &config);
        let b = validation_temp_path(app_dir, &config);
        assert_ne!(a, b);
        for path in [&a, &b] {
            assert_eq!(path.parent(), Some(app_dir));
            let name = path.file_name().unwrap().to_string_lossy();
            assert!(name.starts_with("config_check-p3-"), "got {}", name);
            assert!(name.ends_with(".tmp"), "got {}", name);
        }
    }

    /// A loopback HTTP server that answers exactly one request with `200`,
    /// the given `subscription-userinfo` header and `body`. Returns the URL
    /// (with a token-looking query, as real subscription URLs have) and the
    /// server thread, which yields the request head it received.
    fn serve_once(
        userinfo: &'static str,
        body: &'static str,
    ) -> (String, std::thread::JoinHandle<String>) {
        use std::io::{Read, Write};
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                if stream.read(&mut byte).unwrap() == 0 {
                    break;
                }
                head.push(byte[0]);
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                 Subscription-Userinfo: {}\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{}",
                userinfo,
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
            String::from_utf8_lossy(&head).into_owned()
        });
        (format!("http://127.0.0.1:{}/sub?token=abc", port), server)
    }

    #[test]
    fn fetch_carries_usage_on_changed_and_unchanged() {
        const BODY: &str = r#"{"outbounds":[{"type":"direct","tag":"direct"}]}"#;
        let dir = sub_temp_dir("fetch_usage");
        let config_path = dir.join("configs").join("p1.json");

        let (url, server) = serve_once(
            "upload=1024; download=2048; total=1e6; expire=1800000000",
            BODY,
        );
        let fetched = perform_update(&url, &dir, &config_path, None, Some("1.14.0")).unwrap();
        let head = server.join().unwrap();
        assert!(head.contains("sing-box 1.14.0"), "User-Agent: {}", head);
        assert!(matches!(fetched.outcome, UpdateOutcome::Changed(_)));
        let usage = fetched.usage.expect("usage parsed");
        assert_eq!(
            (usage.upload, usage.download, usage.total, usage.expire),
            (1024, 2048, 1_000_000, Some(1_800_000_000))
        );
        assert!(usage.fetched_at > 1_700_000_000, "stamped with now");
        assert_eq!(fetched.outcome.commit(), Ok(true));

        let (url, server) = serve_once("upload=4096; download=8192; total=1e6", BODY);
        let fetched = perform_update(&url, &dir, &config_path, None, None).unwrap();
        server.join().unwrap();
        assert!(matches!(fetched.outcome, UpdateOutcome::Unchanged));
        let usage = fetched.usage.expect("usage parsed on Unchanged too");
        assert_eq!((usage.upload, usage.download, usage.expire), (4096, 8192, None));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_local_missing_file_errors() {
        let dir = sub_temp_dir("import_missing");
        let src = dir.join("nope.json");
        let config_path = dir.join("configs").join("p1.json");
        let err = import_local_config(&src, &dir, &config_path, None).unwrap_err();
        assert!(err.contains("File not found"), "got: {}", err);
        let _ = fs::remove_dir_all(&dir);
    }
}
