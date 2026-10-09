//! What Settings › Troubleshooting › "Running config" shows: the config
//! sing-box runs with (`running_config.json`), or — while sing-box is
//! stopped — a preview of what the next start would run (through the
//! privileged helper, what the helper would), with credentials masked on
//! request and BoxPilot's per-run API secret masked always.
//!
//! Pure and gpui-free; `load` does blocking file I/O and JSON work, so the
//! UI runs it on the background executor.

use crate::core::paths::runtime_config_path;
use crate::core::privileged_helper::{preview_start, tun_options};
use crate::core::settings::AppSettings;
use crate::core::singbox_api::{is_boxpilot_api_service, SingBoxApi};
use crate::core::subscription::{prepare_config, RuntimeOptions};
use crate::i18n::s;
use serde_json::Value;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// What a masked value reads as.
pub const REDACTED: &str = "<redacted>";

/// What a preview shows for the API port BoxPilot picks at each start.
pub const PICKED_AT_START: &str = "<picked at each start>";

/// Object keys whose values are credentials (compared ASCII
/// case-insensitively, so an HTTP `Authorization` header matches too).
/// Everything under one is masked, scalar by scalar.
const CREDENTIAL_KEYS: &[&str] = &[
    "password",
    "uuid",
    "private_key",
    "private_key_passphrase",
    "pre_shared_key",
    "psk",
    "auth",
    "auth_str",
    "auth_key",
    "token",
    "secret",
    "key",
    "authorization",
];

fn is_credential_key(key: &str) -> bool {
    CREDENTIAL_KEYS
        .iter()
        .any(|credential| credential.eq_ignore_ascii_case(key))
}

/// Mask `config`'s credentials and pretty-print it.
///
/// BoxPilot's own `api` service secret (`is_boxpilot_api_service`) is masked
/// whatever `hide_credentials` says — it guards the live sing-box API. With
/// `hide_credentials`, so is every value under a credential key
/// (`CREDENTIAL_KEYS`, at any depth, inside arrays and objects alike, plus
/// Hysteria's string `obfs` password), and the query string and user info
/// of every `http(s)://` URL, which subscription providers use for tokens.
///
/// Masking replaces scalars in place and never removes or adds an element,
/// so both forms of one config have the same lines — a view can switch
/// between them and stay where it was.
pub fn redact_config(config: &str, hide_credentials: bool) -> Result<String, String> {
    let mut json: Value =
        serde_json::from_str(config).map_err(|e| (s().config_viewer.not_json)(&e.to_string()))?;
    redact_value(&mut json, hide_credentials);
    pretty(&json)
}

/// `redact_config` on a parsed value, in place.
pub fn redact_value(value: &mut Value, hide_credentials: bool) {
    if is_boxpilot_api_service(value) {
        if let Some(secret) = value.get_mut("secret") {
            mask_all(secret);
        }
    }
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                if hide_credentials
                    && (is_credential_key(key) || (key == "obfs" && child.is_string()))
                {
                    mask_all(child);
                } else {
                    redact_value(child, hide_credentials);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_value(item, hide_credentials);
            }
        }
        Value::String(text) if hide_credentials => {
            if let Some(masked) = mask_url(text) {
                *text = masked;
            }
        }
        _ => {}
    }
}

/// Mask every scalar in `value`. Empty strings, `null` and booleans stay:
/// they reveal nothing, and "no password set" is worth seeing.
fn mask_all(value: &mut Value) {
    match value {
        Value::String(text) if !text.is_empty() => *text = REDACTED.to_string(),
        Value::Number(_) => *value = Value::from(REDACTED),
        Value::Array(items) => items.iter_mut().for_each(mask_all),
        Value::Object(map) => map.values_mut().for_each(mask_all),
        _ => {}
    }
}

/// `text` with the user info and query string masked, if it is an
/// `http(s)://` URL that has either; `None` otherwise.
fn mask_url(text: &str) -> Option<String> {
    let scheme_end = ["https://", "http://"]
        .iter()
        .find(|scheme| {
            text.len() >= scheme.len() && text[..scheme.len()].eq_ignore_ascii_case(scheme)
        })?
        .len();
    let (scheme, rest) = text.split_at(scheme_end);
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    let (userinfo, host) = match authority.rsplit_once('@') {
        Some((_, host)) => (true, host),
        None => (false, authority),
    };
    let (path, query, fragment) = match tail.split_once('?') {
        Some((path, query_and_fragment)) => match query_and_fragment.split_once('#') {
            Some((_, fragment)) => (path, true, Some(fragment)),
            None => (path, true, None),
        },
        None => (tail, false, None),
    };
    if !userinfo && !query {
        return None;
    }
    let mut masked = String::with_capacity(text.len());
    masked.push_str(scheme);
    if userinfo {
        masked.push_str(REDACTED);
        masked.push('@');
    }
    masked.push_str(host);
    masked.push_str(path);
    if query {
        masked.push('?');
        masked.push_str(REDACTED);
    }
    if let Some(fragment) = fragment {
        masked.push('#');
        masked.push_str(fragment);
    }
    Some(masked)
}

fn pretty(json: &Value) -> Result<String, String> {
    serde_json::to_string_pretty(json)
        .map_err(|e| (s().config_viewer.format_failed)(&e.to_string()))
}

/// What the next start would write for `profile_json` (a profile's
/// canonical config) under `settings`: `prepare_config` with a stand-in API
/// endpoint whose port reads `PICKED_AT_START`. Its secret is a fresh,
/// never-used one; `redact_config` masks it like a real one.
pub fn preview_config(profile_json: &str, settings: &AppSettings) -> Result<String, String> {
    let prepared = prepare_config(
        profile_json,
        RuntimeOptions::new(settings, SingBoxApi::new(0)),
    )?;
    let mut json: Value = serde_json::from_str(&prepared)
        .map_err(|e| (s().config_viewer.not_json)(&e.to_string()))?;
    if let Some(services) = json.get_mut("services").and_then(Value::as_array_mut) {
        for service in services.iter_mut().filter(|s| is_boxpilot_api_service(s)) {
            service["listen_port"] = Value::from(PICKED_AT_START);
        }
    }
    pretty(&json)
}

/// Where the shown config came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSource {
    /// `running_config.json`, the file the running sing-box was started with.
    Running,
    /// Built from the active profile and the current settings: sing-box is
    /// not running.
    Preview,
}

/// A loaded config in both forms, so switching "Hide credentials" needs no
/// reload. `hidden` and `revealed` have the same lines (`redact_config`).
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigView {
    pub source: ConfigSource,
    /// The file it was read from: `running_config.json`, or the profile's
    /// canonical config for a preview. "Open folder" reveals it.
    pub file: PathBuf,
    /// Credentials masked.
    pub hidden: String,
    /// Only BoxPilot's API secret masked.
    pub revealed: String,
}

impl ConfigView {
    pub fn text(&self, hide_credentials: bool) -> &str {
        if hide_credentials {
            &self.hidden
        } else {
            &self.revealed
        }
    }
}

/// Why there is nothing to show.
#[derive(Debug, Clone, PartialEq)]
pub enum ConfigViewError {
    /// No profile at all.
    NoProfile,
    /// The active profile has no config on disk yet (never fetched).
    NotDownloaded,
    /// Reading or parsing failed. The message names only local paths and
    /// parse positions, never config content.
    Failed(String),
}

/// What `load` needs, snapshotted on the UI thread.
#[derive(Debug, Clone)]
pub struct ConfigRequest {
    pub app_dir: PathBuf,
    /// sing-box is starting or running.
    pub running: bool,
    /// The active profile's canonical config; `None` without a profile.
    pub profile_config: Option<PathBuf>,
    pub settings: AppSettings,
    /// The next start goes through the privileged helper
    /// (`privileged_helper::StartRoute::Helper`): the preview is what the
    /// helper would run, not the local runtime config.
    pub through_helper: bool,
}

/// Read and redact the config to show: `running_config.json` while sing-box
/// runs, else (or should that file be missing) a preview from the active
/// profile. A stopped sing-box's leftover `running_config.json` is not
/// shown: the profile or settings may have changed since, and the preview
/// is what the next start will run. Blocking.
pub fn load(request: &ConfigRequest) -> Result<ConfigView, ConfigViewError> {
    if request.running {
        let path = runtime_config_path(&request.app_dir);
        match fs::read_to_string(&path) {
            Ok(text) => return view(ConfigSource::Running, path, &text),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(read_failed(&path, e)),
        }
    }
    let path = request
        .profile_config
        .clone()
        .ok_or(ConfigViewError::NoProfile)?;
    let profile_json = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(ConfigViewError::NotDownloaded)
        }
        Err(e) => return Err(read_failed(&path, e)),
    };
    let preview = if request.through_helper {
        // A refusal is already in the user's words, naming the profile.
        preview_start(
            &profile_json,
            &request.app_dir,
            tun_options(&request.settings),
        )
        .map_err(ConfigViewError::Failed)?
    } else {
        preview_config(&profile_json, &request.settings).map_err(|e| {
            ConfigViewError::Failed((s().config_viewer.profile_unreadable)(&e.to_string()))
        })?
    };
    view(ConfigSource::Preview, path, &preview)
}

fn read_failed(path: &Path, e: io::Error) -> ConfigViewError {
    ConfigViewError::Failed((s().errors.read_failed)(
        &path.display().to_string(),
        &e.to_string(),
    ))
}

fn view(source: ConfigSource, file: PathBuf, text: &str) -> Result<ConfigView, ConfigViewError> {
    let failed = |e: String| ConfigViewError::Failed(format!("{}: {}", file.display(), e));
    let mut revealed: Value = serde_json::from_str(text)
        .map_err(|e| failed((s().config_viewer.not_json)(&e.to_string())))?;
    let mut hidden = revealed.clone();
    redact_value(&mut revealed, false);
    redact_value(&mut hidden, true);
    Ok(ConfigView {
        source,
        hidden: pretty(&hidden).map_err(failed)?,
        revealed: pretty(&revealed).map_err(failed)?,
        file,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::paths::profile_config_path;
    use serde_json::json;

    /// A config exercising every masking rule: credentials at the top of an
    /// outbound, in nested objects (`tls.reality`, `obfs`), in arrays of
    /// objects (`users`, `peers`) and of strings (`tls.key`), and URLs with
    /// tokens.
    fn fixture() -> Value {
        json!({
            "log": {"level": "info"},
            "outbounds": [
                {"type": "vless", "tag": "a", "server": "a.example", "server_port": 443,
                 "uuid": "11111111-2222-3333-4444-555555555555",
                 "tls": {"enabled": true, "server_name": "a.example",
                         "reality": {"enabled": true, "public_key": "PUBKEY", "short_id": "ab"}}},
                {"type": "hysteria2", "tag": "b", "server": "b.example",
                 "password": "hy2-pass", "obfs": {"type": "salamander", "password": "obfs-pass"}},
                {"type": "hysteria", "tag": "c", "auth_str": "hy-auth", "obfs": "hy-obfs"},
                {"type": "wireguard", "tag": "d", "private_key": "WG-PRIV",
                 "peers": [{"public_key": "WG-PUB", "pre_shared_key": "WG-PSK"}]},
                {"type": "shadowsocks", "tag": "e", "password": ""},
                {"type": "http", "tag": "f", "headers": {"Authorization": "Bearer HTTP-TOKEN"}},
                {"type": "ssh", "tag": "g", "private_key_passphrase": "SSH-PASS"}
            ],
            "inbounds": [
                {"type": "trojan", "tag": "in", "users": [{"name": "u", "password": "TROJAN-PASS"}],
                 "tls": {"enabled": true, "certificate": ["CERT"], "key": ["KEY-LINE-1", "KEY-LINE-2"]}}
            ],
            "route": {"rule_set": [
                {"tag": "rs", "type": "remote",
                 "url": "https://sub.example/rules.srs?token=RULE-TOKEN#frag"},
                {"tag": "plain", "type": "remote", "url": "https://cdn.example/geo.srs"}
            ]},
            "services": [
                {"type": "api", "tag": "theirs", "listen": "127.0.0.1", "listen_port": 9091,
                 "secret": "THEIR-API-SECRET"}
            ],
            "experimental": {"clash_api": {"external_controller": "127.0.0.1:9090",
                                           "secret": "CLASH-SECRET"}}
        })
    }

    /// The fixture as a running config: with BoxPilot's own `api` service,
    /// and BoxPilot's inbounds in place of the fixture's (those are checked
    /// unprepared, in `hide_masks_inbound_users_and_tls_key_lines`).
    fn running_fixture(api: SingBoxApi) -> String {
        prepare_config(
            &fixture().to_string(),
            RuntimeOptions::new(&AppSettings::default(), api),
        )
        .unwrap()
    }

    fn our_secret(api: SingBoxApi) -> String {
        api.service_config()["secret"].as_str().unwrap().to_string()
    }

    const CREDENTIALS: &[&str] = &[
        "11111111-2222-3333-4444-555555555555",
        "hy2-pass",
        "obfs-pass",
        "hy-auth",
        "hy-obfs",
        "WG-PRIV",
        "WG-PSK",
        "HTTP-TOKEN",
        "SSH-PASS",
        "RULE-TOKEN",
        "THEIR-API-SECRET",
        "CLASH-SECRET",
    ];

    #[test]
    fn our_api_secret_is_masked_even_with_credentials_shown() {
        let api = SingBoxApi::new(41234);
        let shown = redact_config(&running_fixture(api), false).unwrap();
        assert!(!shown.contains(&our_secret(api)));
        // Everything else stays as the config wrote it.
        for credential in CREDENTIALS {
            assert!(shown.contains(credential), "{} hidden", credential);
        }
        let json: Value = serde_json::from_str(&shown).unwrap();
        let ours = json["services"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| is_boxpilot_api_service(s))
            .unwrap();
        assert_eq!(ours["secret"], REDACTED);
        assert_eq!(ours["listen_port"], 41234);
    }

    #[test]
    fn hide_masks_credentials_in_nested_objects_and_arrays() {
        let api = SingBoxApi::new(41234);
        let hidden = redact_config(&running_fixture(api), true).unwrap();
        assert!(!hidden.contains(&our_secret(api)));
        for credential in CREDENTIALS {
            assert!(!hidden.contains(credential), "{} shown", credential);
        }
        let json: Value = serde_json::from_str(&hidden).unwrap();
        let outbounds = &json["outbounds"];
        assert_eq!(outbounds[0]["uuid"], REDACTED);
        // Non-credentials beside and above them stay.
        assert_eq!(outbounds[0]["server"], "a.example");
        assert_eq!(outbounds[0]["tls"]["reality"]["public_key"], "PUBKEY");
        assert_eq!(
            outbounds[1]["obfs"],
            json!({"type": "salamander", "password": REDACTED})
        );
        assert_eq!(outbounds[2]["obfs"], REDACTED);
        assert_eq!(outbounds[3]["peers"][0]["public_key"], "WG-PUB");
        assert_eq!(outbounds[3]["peers"][0]["pre_shared_key"], REDACTED);
        // An empty password says "none set" and stays.
        assert_eq!(outbounds[4]["password"], "");
        assert_eq!(outbounds[5]["headers"]["Authorization"], REDACTED);
        let rules = &json["route"]["rule_set"];
        assert_eq!(
            rules[0]["url"],
            "https://sub.example/rules.srs?<redacted>#frag"
        );
        assert_eq!(rules[1]["url"], "https://cdn.example/geo.srs");
        assert_eq!(
            json["experimental"]["clash_api"]["external_controller"],
            "127.0.0.1:9090"
        );
        // Numbers under a credential key become the marker too.
        let numeric = redact_config(r#"{"secret": 1234, "port": 1234}"#, true).unwrap();
        let numeric: Value = serde_json::from_str(&numeric).unwrap();
        assert_eq!(numeric, json!({"secret": REDACTED, "port": 1234}));
    }

    #[test]
    fn hide_masks_inbound_users_and_tls_key_lines() {
        // Unprepared, so the fixture's own inbounds survive.
        let hidden = redact_config(&fixture().to_string(), true).unwrap();
        let json: Value = serde_json::from_str(&hidden).unwrap();
        let inbound = &json["inbounds"][0];
        assert_eq!(
            inbound["users"][0],
            json!({"name": "u", "password": REDACTED})
        );
        assert_eq!(inbound["tls"]["key"], json!([REDACTED, REDACTED]));
        assert_eq!(inbound["tls"]["certificate"], json!(["CERT"]));
    }

    #[test]
    fn both_forms_have_the_same_lines() {
        let text = running_fixture(SingBoxApi::new(41234));
        let hidden = redact_config(&text, true).unwrap();
        let shown = redact_config(&text, false).unwrap();
        assert_eq!(hidden.lines().count(), shown.lines().count());
    }

    #[test]
    fn non_json_input_is_an_error() {
        assert!(redact_config("not json", true).is_err());
        assert!(redact_config("{\"a\": ", false).is_err());
        assert!(preview_config("not json", &AppSettings::default()).is_err());
        assert!(preview_config("[]", &AppSettings::default()).is_err());
    }

    #[test]
    fn url_masking() {
        assert_eq!(mask_url("https://cdn.example/a.srs"), None);
        assert_eq!(mask_url("tls://1.1.1.1"), None);
        assert_eq!(mask_url("not a url?x=1"), None);
        assert_eq!(
            mask_url("HTTPS://user:pw@host.example:8443/p?q=1").as_deref(),
            Some("HTTPS://<redacted>@host.example:8443/p?<redacted>")
        );
        assert_eq!(
            mask_url("http://host?token=x").as_deref(),
            Some("http://host?<redacted>")
        );
        // An `@` in the path is not user info.
        assert_eq!(mask_url("https://host/a@b"), None);
    }

    #[test]
    fn preview_marks_the_api_port_and_follows_settings() {
        let settings = AppSettings {
            proxy_mode: true,
            proxy_port: 18300,
            ..AppSettings::default()
        };
        let preview = preview_config(&fixture().to_string(), &settings).unwrap();
        let json: Value = serde_json::from_str(&preview).unwrap();
        assert_eq!(json["inbounds"].as_array().unwrap().len(), 1);
        assert_eq!(json["inbounds"][0]["listen_port"], 18300);
        let ours = json["services"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| is_boxpilot_api_service(s))
            .unwrap()
            .clone();
        assert_eq!(ours["listen_port"], PICKED_AT_START);
        let shown: Value = serde_json::from_str(&redact_config(&preview, false).unwrap()).unwrap();
        assert!(shown["services"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| is_boxpilot_api_service(s) && s["secret"] == REDACTED));
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "box_pilot_config_view_{}_{}",
            tag,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn request(dir: &Path, running: bool, profile: Option<&str>) -> ConfigRequest {
        ConfigRequest {
            app_dir: dir.to_path_buf(),
            running,
            profile_config: profile.map(|id| profile_config_path(dir, id)),
            settings: AppSettings::default(),
            through_helper: false,
        }
    }

    #[test]
    fn load_picks_running_config_preview_or_empty_state() {
        let dir = temp_dir("load");
        assert_eq!(
            load(&request(&dir, false, None)),
            Err(ConfigViewError::NoProfile)
        );
        assert_eq!(
            load(&request(&dir, false, Some("p1"))),
            Err(ConfigViewError::NotDownloaded)
        );

        let profile = profile_config_path(&dir, "p1");
        fs::create_dir_all(profile.parent().unwrap()).unwrap();
        fs::write(&profile, fixture().to_string()).unwrap();
        let preview = load(&request(&dir, false, Some("p1"))).unwrap();
        assert_eq!(preview.source, ConfigSource::Preview);
        assert_eq!(preview.file, profile);
        assert!(preview.revealed.contains("hy2-pass"));
        assert!(!preview.hidden.contains("hy2-pass"));
        // Running, but the file is gone: still a preview.
        assert_eq!(
            load(&request(&dir, true, Some("p1"))).unwrap().source,
            ConfigSource::Preview
        );

        let api = SingBoxApi::new(41234);
        let runtime = runtime_config_path(&dir);
        fs::write(&runtime, running_fixture(api)).unwrap();
        let running = load(&request(&dir, true, Some("p1"))).unwrap();
        assert_eq!(running.source, ConfigSource::Running);
        assert_eq!(running.file, runtime);
        assert!(!running.revealed.contains(&our_secret(api)));
        assert!(!running.hidden.contains(&our_secret(api)));
        assert_eq!(running.text(true), running.hidden);
        assert_eq!(running.text(false), running.revealed);
        // Stopped: the leftover file is not shown.
        assert_eq!(
            load(&request(&dir, false, Some("p1"))).unwrap().source,
            ConfigSource::Preview
        );

        fs::write(&runtime, "garbage").unwrap();
        assert!(matches!(
            load(&request(&dir, true, Some("p1"))),
            Err(ConfigViewError::Failed(_))
        ));
        let _ = fs::remove_dir_all(&dir);
    }
}
