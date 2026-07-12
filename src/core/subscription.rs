use crate::core::clash_api::ClashApi;
use crate::core::settings::{CLASH_API_PORT, HTTP_TIMEOUT_SECS, PROXY_PORT};
use reqwest::blocking::Client;
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::time::Duration;

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

/// Result of a subscription fetch + write.
///
/// `Changed` means we wrote a new `config.json` to disk; the caller should
/// persist settings and (for auto-update) restart sing-box if running.
/// `Unchanged` means the fetched config was byte-identical to what's already
/// on disk; nothing was written and no further action is needed.
#[derive(Debug)]
pub enum UpdateOutcome {
    Changed,
    Unchanged,
}

/// Strip BoxPilot-managed sections from config (used when saving subscription
/// data). Both `inbounds` and `experimental` are re-injected mode-specifically
/// at process start by `prepare_config`, so the canonical on-disk form
/// contains neither. Subscription-provided `experimental` content is
/// intentionally discarded — same ownership rule as inbounds.
pub fn strip_inbounds(config_data: &str) -> Result<String, String> {
    let mut json: Value = serde_json::from_str(config_data)
        .map_err(|e| format!("Failed to parse config JSON: {}", e))?;
    let obj = json
        .as_object_mut()
        .ok_or_else(|| "Config is not a JSON object".to_string())?;
    obj.remove("inbounds");
    obj.remove("experimental");
    serde_json::to_string_pretty(&json)
        .map_err(|e| format!("Failed to serialize config: {}", e))
}

/// Inject mode-specific inbounds into config (used at process start)
pub fn prepare_config(
    config_data: &str,
    proxy_mode: bool,
    set_system_proxy: bool,
    proxy_port: u16,
    clash_api_port: u16,
) -> Result<String, String> {
    let mut json: Value = serde_json::from_str(config_data)
        .map_err(|e| format!("Failed to parse config JSON: {}", e))?;

    let port = proxy_port;
    let mut mixed_inbound = serde_json::json!({
        "type": "mixed",
        "tag": "proxy",
        "listen": "127.0.0.1",
        "listen_port": port
    });
    if set_system_proxy {
        mixed_inbound["set_system_proxy"] = serde_json::Value::Bool(true);
    }

    let inbounds = if proxy_mode {
        serde_json::Value::Array(vec![mixed_inbound])
    } else {
        let tun_inbound = serde_json::json!({
            "type": "tun",
            "tag": "tun0",
            "address": ["172.18.0.1/30", "fdfe:dcba:9876::1/126"],
            "auto_route": true,
            "strict_route": true,
            "stack": "mixed"
        });
        serde_json::Value::Array(vec![tun_inbound, mixed_inbound])
    };

    json["inbounds"] = inbounds;

    // Clash API for runtime selector switching. With cache_file enabled,
    // sing-box (≥1.8) automatically persists the chosen selector node across
    // restarts (cache.db) — the old `store_selected` field was removed
    // upstream and now fails config validation as an unknown field.
    json["experimental"] = serde_json::json!({
        "clash_api": {
            "external_controller": ClashApi::new(clash_api_port).external_controller()
        },
        "cache_file": {
            "enabled": true
        }
    });

    serde_json::to_string_pretty(&json)
        .map_err(|e| format!("Failed to serialize config: {}", e))
}

/// Fetch the subscription, strip its inbounds, and write to `config_path`
/// (the profile's `configs/<id>.json`) — but only if the result differs from
/// what's already on disk. `app_dir` is still needed separately: it's where
/// the validation temp file goes so `sing-box check -D` resolves relative
/// resources exactly like at runtime.
///
/// Settings persistence is the caller's responsibility (`AppState::save_settings`),
/// so the caller doesn't risk clobbering settings fields not visible here.
pub fn perform_update(
    sub_url: &str,
    app_dir: &Path,
    config_path: &Path,
    sing_box: Option<&Path>,
    sing_box_version: Option<&str>,
) -> Result<UpdateOutcome, String> {
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
            format!("Network error fetching subscription: {}", e)
        }
    })?;

    if !response.status().is_success() {
        return Err(format!(
            "Failed to download subscription. Status: {}",
            response.status()
        ));
    }

    let config_data = response
        .text()
        .map_err(|e| format!("Failed to read subscription response: {}", e))?;

    apply_config_text(&config_data, app_dir, config_path, sing_box)
}

/// Shared tail once the raw config text is in hand: strip → unchanged-detection
/// → `sing-box check` validation → write. Both the remote (HTTP) and local
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
            validate_downloaded_config(sing_box, app_dir, &stripped)?;
        }
    }

    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create {}: {}", parent.display(), e))?;
    }
    fs::write(config_path, &stripped).map_err(|e| {
        format!(
            "Failed to write config ({}): {}",
            config_path.display(),
            e
        )
    })?;

    Ok(UpdateOutcome::Changed)
}

/// Read a sing-box config from a local file and snapshot it into `config_path`
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
/// removed afterwards.
fn validate_downloaded_config(
    sing_box: &Path,
    app_dir: &Path,
    stripped: &str,
) -> Result<(), String> {
    // 校验用的入站/Clash API 端口与运行时无关,固定默认值即可。
    let prepared = prepare_config(stripped, true, false, PROXY_PORT, CLASH_API_PORT)?;
    let tmp_path = app_dir.join("config_check.tmp");
    fs::write(&tmp_path, &prepared)
        .map_err(|e| format!("Failed to write validation temp file: {}", e))?;
    let result = crate::core::process::validate_config(sing_box, app_dir, &tmp_path);
    let _ = fs::remove_file(&tmp_path);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUB_CONFIG: &str = r#"{
        "log": {"level": "info"},
        "dns": {"servers": [{"tag": "remote", "address": "8.8.8.8"}]},
        "inbounds": [{"type": "tun", "tag": "upstream-tun"}],
        "outbounds": [{"type": "vless", "tag": "proxy-out"}],
        "route": {"rules": []},
        "experimental": {"clash_api": {"external_controller": "0.0.0.0:9090"}}
    }"#;

    fn parse(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
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

    #[test]
    fn strip_removes_experimental() {
        let stripped = strip_inbounds(SUB_CONFIG).unwrap();
        let json = parse(&stripped);
        assert!(json.get("experimental").is_none());
    }

    /// BoxPilot owns the `experimental` section: clash_api on the loopback port
    /// for node switching, cache_file so sing-box persists the user's selector
    /// choices across restarts and subscription updates (automatic when
    /// enabled — sing-box ≥1.8 removed `store_selected` and rejects it as an
    /// unknown field, so we must NOT inject it).
    #[test]
    fn prepare_injects_clash_api_and_cache_file() {
        let stripped = strip_inbounds(SUB_CONFIG).unwrap();
        let prepared = parse(&prepare_config(&stripped, true, false, PROXY_PORT, CLASH_API_PORT).unwrap());
        assert_eq!(
            prepared["experimental"]["clash_api"]["external_controller"],
            format!("127.0.0.1:{}", CLASH_API_PORT)
        );
        assert_eq!(prepared["experimental"]["cache_file"]["enabled"], true);
        assert!(
            prepared["experimental"]["cache_file"]
                .get("store_selected")
                .is_none(),
            "store_selected was removed upstream; injecting it fails sing-box config validation"
        );
    }

    #[test]
    fn strip_rejects_non_object_and_invalid_json() {
        assert!(strip_inbounds("[1, 2]").is_err());
        assert!(strip_inbounds("not json").is_err());
    }

    #[test]
    fn proxy_mode_injects_single_mixed_inbound() {
        let stripped = strip_inbounds(SUB_CONFIG).unwrap();
        let prepared = parse(&prepare_config(&stripped, true, false, PROXY_PORT, CLASH_API_PORT).unwrap());
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
        let prepared = parse(&prepare_config(&stripped, true, false, 18888, CLASH_API_PORT).unwrap());
        assert_eq!(prepared["inbounds"][0]["listen_port"], 18888);
    }

    #[test]
    fn prepare_uses_custom_clash_api_port() {
        let stripped = strip_inbounds(SUB_CONFIG).unwrap();
        let prepared = parse(&prepare_config(&stripped, true, false, PROXY_PORT, 17900).unwrap());
        assert_eq!(
            prepared["experimental"]["clash_api"]["external_controller"],
            "127.0.0.1:17900"
        );
    }

    #[test]
    fn system_proxy_flag_is_injected_only_when_enabled() {
        let stripped = strip_inbounds(SUB_CONFIG).unwrap();
        let prepared = parse(&prepare_config(&stripped, true, true, PROXY_PORT, CLASH_API_PORT).unwrap());
        assert_eq!(prepared["inbounds"][0]["set_system_proxy"], true);
    }

    #[test]
    fn tun_mode_injects_tun_then_mixed() {
        let stripped = strip_inbounds(SUB_CONFIG).unwrap();
        let prepared = parse(&prepare_config(&stripped, false, false, PROXY_PORT, CLASH_API_PORT).unwrap());
        let inbounds = prepared["inbounds"].as_array().unwrap();
        assert_eq!(inbounds.len(), 2);
        assert_eq!(inbounds[0]["type"], "tun");
        assert_eq!(inbounds[0]["auto_route"], true);
        assert_eq!(inbounds[0]["strict_route"], true);
        assert_eq!(inbounds[1]["type"], "mixed");
    }

    /// The subscription's own inbounds must be replaced, never merged —
    /// `start_process` relies on the active config containing exactly the
    /// inbounds BoxPilot injected.
    #[test]
    fn prepare_replaces_existing_inbounds() {
        let prepared = parse(&prepare_config(SUB_CONFIG, true, false, PROXY_PORT, CLASH_API_PORT).unwrap());
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
    /// tolerating them requires strip ∘ prepare to be the identity on
    /// stripped configs, for every mode combination.
    #[test]
    fn strip_after_prepare_recovers_canonical_form() {
        let canonical = strip_inbounds(SUB_CONFIG).unwrap();
        for (proxy_mode, system_proxy) in
            [(true, true), (true, false), (false, true), (false, false)]
        {
            let on_disk =
                prepare_config(&canonical, proxy_mode, system_proxy, PROXY_PORT, CLASH_API_PORT)
                    .unwrap();
            let recovered = strip_inbounds(&on_disk).unwrap();
            assert_eq!(
                recovered, canonical,
                "strip(prepare(x, {}, {})) must equal x",
                proxy_mode, system_proxy
            );
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
        assert!(matches!(outcome, UpdateOutcome::Changed));
        let json = parse(&fs::read_to_string(&config_path).unwrap());
        assert!(json.get("inbounds").is_none(), "inbounds must be stripped");
        assert!(json.get("experimental").is_none(), "experimental must be stripped");
        assert_eq!(json["outbounds"], parse(SUB_CONFIG)["outbounds"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_local_second_identical_import_is_unchanged() {
        let dir = sub_temp_dir("import_unchanged");
        let src = dir.join("source.json");
        fs::write(&src, SUB_CONFIG).unwrap();
        let config_path = dir.join("configs").join("p1.json");
        import_local_config(&src, &dir, &config_path, None).unwrap();
        let outcome = import_local_config(&src, &dir, &config_path, None).unwrap();
        assert!(matches!(outcome, UpdateOutcome::Unchanged));
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
