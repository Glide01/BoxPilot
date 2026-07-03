use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

pub const SING_EXECUTABLE: &str = "sing-box.exe";
pub const CONFIG_FILENAME: &str = "config.json";
/// Per-profile configs live in `<app_dir>/configs/<profile_id>.json`. The
/// legacy single `config.json` is migrated into here on first launch.
pub const PROFILES_DIR: &str = "configs";
/// The prepared (inbounds + experimental injected) config sing-box actually
/// runs with. Rewritten from the active profile's canonical config on every
/// process start; the canonical `configs/<id>.json` files are never touched
/// by a start, so their bytes/mtime only change on a real subscription update.
pub const RUNTIME_CONFIG_FILENAME: &str = "running_config.json";
pub const SETTINGS_FILE: &str = "box_pilot_settings.json";
pub const PROXY_PORT: u16 = 7788;
pub const CLASH_API_PORT: u16 = 7789;
pub const MAX_LOG_LINES: usize = 1000;
pub const HTTP_TIMEOUT_SECS: u64 = 8;
/// 整组延迟测速(Clash API /group/{name}/delay)探测的目标 URL。
pub const DELAY_TEST_URL: &str = "https://www.gstatic.com/generate_204";
/// 传给测速端点的单节点超时,毫秒。
pub const DELAY_TEST_TIMEOUT_MS: u32 = 5000;

#[derive(PartialEq, Clone, Copy, Debug)]
pub enum StatusLevel {
    Info,
    Success,
    Warning,
    Error,
}

/// A transient status message destined for the single toast slot. Emitted
/// (via gpui `EventEmitter<StatusEvent>`) by `AppState`, `ProcessSession` and
/// `ProxyGroups` alike; `RootView` wires every emitter to `toast::show` with
/// one helper. A new emission always supersedes whatever is on screen.
pub struct StatusEvent {
    pub level: StatusLevel,
    pub message: String,
}

#[derive(PartialEq, Clone, Copy, Debug)]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

pub struct LogEntry {
    pub message: String,
    pub level: LogLevel,
}

#[derive(PartialEq, Clone, Copy, Debug)]
pub enum LogFilter {
    All,
    Error,
    Warn,
}

pub fn matches_filter(level: LogLevel, filter: LogFilter) -> bool {
    match filter {
        LogFilter::All => true,
        LogFilter::Error => level == LogLevel::Error,
        LogFilter::Warn => matches!(level, LogLevel::Warn | LogLevel::Error),
    }
}

/// Where a profile's config comes from. Serialized as an internally-tagged
/// object (`"source": { "kind": "remote", "url": … }`). A `Remote` profile is
/// fetched over HTTP and auto-updated on its interval; a `Local` profile is a
/// one-time snapshot of a file the user picked (re-read only on manual ⟳).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProfileSource {
    Remote {
        url: String,
        /// Per-profile auto-update cadence; 0 = off. Every Remote profile is
        /// scheduled independently by the auto-update loop, not just the active.
        #[serde(default = "default_auto_update_interval")]
        auto_update_interval_minutes: u64,
    },
    Local {
        /// The file the user picked; re-read by ⟳ to refresh the snapshot.
        path: String,
    },
}

/// A profile: one named config source. Its fetched/imported config is stored at
/// `<app_dir>/configs/<id>.json` (see `paths::profile_config_path`).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(from = "ProfileDe")]
pub struct Profile {
    /// Stable identifier, doubles as the config file name ("p1", "p2", …).
    pub id: String,
    pub name: String,
    pub source: ProfileSource,
    /// Unix-epoch seconds of the last time this profile's config content
    /// actually changed (a fetch/import that wrote new bytes). `None` = never
    /// updated. Drives the "updated N ago" label — read from here rather than
    /// the config file's mtime, which a sing-box start would otherwise bump.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_updated_secs: Option<u64>,
}

/// Deserialization shim that accepts both the current shape (`source` object)
/// and the pre-`ProfileSource` flat shape (`url` + `auto_update_interval_minutes`
/// at the top level). Older `settings.json` files migrate to `Remote` on load;
/// the next `save()` rewrites them in the current shape.
#[derive(Deserialize)]
struct ProfileDe {
    id: String,
    name: String,
    #[serde(default)]
    source: Option<ProfileSource>,
    #[serde(default)]
    last_updated_secs: Option<u64>,
    // Legacy flat fields (releases before ProfileSource existed).
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    auto_update_interval_minutes: Option<u64>,
}

impl From<ProfileDe> for Profile {
    fn from(de: ProfileDe) -> Self {
        let source = de.source.unwrap_or_else(|| ProfileSource::Remote {
            url: de.url.unwrap_or_default(),
            auto_update_interval_minutes: de
                .auto_update_interval_minutes
                .unwrap_or_else(default_auto_update_interval),
        });
        Profile {
            id: de.id,
            name: de.name,
            source,
            last_updated_secs: de.last_updated_secs,
        }
    }
}

impl ProfileSource {
    /// True when there is nothing to fetch: an empty subscription URL or no
    /// file picked. The one owner of this rule — gates the immediate fetch
    /// after dialog Save, the per-row ⟳ button, and `update_profile`'s guard.
    pub fn is_empty_source(&self) -> bool {
        match self {
            ProfileSource::Remote { url, .. } => url.trim().is_empty(),
            ProfileSource::Local { path } => path.trim().is_empty(),
        }
    }
}

impl Profile {
    /// True for a `Local` (file-snapshot) profile.
    pub fn is_local(&self) -> bool {
        matches!(self.source, ProfileSource::Local { .. })
    }

    /// The subscription URL for a `Remote` profile; `None` for `Local`.
    pub fn remote_url(&self) -> Option<&str> {
        match &self.source {
            ProfileSource::Remote { url, .. } => Some(url),
            ProfileSource::Local { .. } => None,
        }
    }

    /// The picked file path for a `Local` profile; `None` for `Remote`.
    pub fn local_path(&self) -> Option<&str> {
        match &self.source {
            ProfileSource::Local { path } => Some(path),
            ProfileSource::Remote { .. } => None,
        }
    }

    /// Auto-update cadence in minutes; always 0 for `Local` (never polled).
    pub fn auto_update_interval(&self) -> u64 {
        match &self.source {
            ProfileSource::Remote {
                auto_update_interval_minutes,
                ..
            } => *auto_update_interval_minutes,
            ProfileSource::Local { .. } => 0,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AppSettings {
    pub proxy_mode: bool,
    #[serde(default)]
    pub set_system_proxy: bool,
    /// 本地 mixed 代理入站端口(Settings 页可配)。
    #[serde(default = "default_proxy_port")]
    pub proxy_port: u16,
    /// Clash API(external_controller)端口——节点切换/整组测速/网速流都走它,
    /// 固定 127.0.0.1(Settings 页可配)。
    #[serde(default = "default_clash_api_port")]
    pub clash_api_port: u16,
    #[serde(default)]
    pub profiles: Vec<Profile>,
    #[serde(default)]
    pub active_profile_id: String,
}

pub fn default_auto_update_interval() -> u64 {
    60
}

pub fn default_proxy_port() -> u16 {
    PROXY_PORT
}

pub fn default_clash_api_port() -> u16 {
    CLASH_API_PORT
}

impl Default for AppSettings {
    fn default() -> Self {
        let mut settings = Self {
            proxy_mode: false,
            set_system_proxy: false,
            proxy_port: default_proxy_port(),
            clash_api_port: default_clash_api_port(),
            profiles: Vec::new(),
            active_profile_id: String::new(),
        };
        settings.normalize_profiles();
        settings
    }
}

impl AppSettings {
    pub fn load(app_dir: &Path) -> Self {
        let settings_path = app_dir.join(SETTINGS_FILE);
        let mut settings = match fs::read_to_string(&settings_path) {
            Ok(data) => match serde_json::from_str(&data) {
                Ok(settings) => settings,
                Err(e) => {
                    eprintln!(
                        "Failed to parse settings from {}: {}. Using default.",
                        settings_path.display(),
                        e
                    );
                    AppSettings::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => AppSettings::default(),
            Err(e) => {
                eprintln!(
                    "Failed to read settings file {}: {}. Using default.",
                    settings_path.display(),
                    e
                );
                AppSettings::default()
            }
        };
        settings.normalize_profiles();
        settings
    }

    /// Enforce the profile invariants every other consumer relies on:
    /// `active_profile_id` always refers to an existing profile, or is empty
    /// when there are no profiles at all (the empty first-run / deleted-all
    /// state). `profiles` is allowed to be empty.
    pub fn normalize_profiles(&mut self) {
        if !self.profiles.iter().any(|p| p.id == self.active_profile_id) {
            self.active_profile_id = self
                .profiles
                .first()
                .map(|p| p.id.clone())
                .unwrap_or_default();
        }
    }

    pub fn active_profile(&self) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.id == self.active_profile_id)
    }

    /// True when at least one profile exists. `false` = empty first-run /
    /// deleted-all state (Home shows the "Add subscription" empty card).
    pub fn has_profiles(&self) -> bool {
        !self.profiles.is_empty()
    }

    /// Next "p<n>" id above the current maximum. A deleted maximum gets
    /// reused — safe only because profile deletion also removes its
    /// `configs/<id>.json`, so a reused id can't pick up stale config data.
    pub fn next_profile_id(&self) -> String {
        let max = self
            .profiles
            .iter()
            .filter_map(|p| p.id.strip_prefix('p').and_then(|n| n.parse::<u64>().ok()))
            .max()
            .unwrap_or(0);
        format!("p{}", max + 1)
    }

    pub fn save(&self, app_dir: &Path) {
        let settings_path = app_dir.join(SETTINGS_FILE);
        match serde_json::to_string_pretty(self) {
            Ok(data) => {
                if let Err(e) = fs::write(&settings_path, data) {
                    eprintln!(
                        "Failed to write settings to {}: {}",
                        settings_path.display(),
                        e
                    );
                }
            }
            Err(e) => {
                eprintln!("Failed to serialize settings: {}", e);
            }
        }
    }
}

pub fn powershell_proxy_command(port: u16) -> String {
    format!(
        "$env:HTTP_PROXY=\"http://127.0.0.1:{port}\"; $env:HTTPS_PROXY=\"http://127.0.0.1:{port}\"; $env:ALL_PROXY=\"socks5://127.0.0.1:{port}\"",
        port = port,
    )
}

pub fn wsl_proxy_command(port: u16) -> String {
    format!(
        "export http_proxy=\"http://127.0.0.1:{port}\" && export https_proxy=\"http://127.0.0.1:{port}\" && export all_proxy=\"socks5://127.0.0.1:{port}\"",
        port = port,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_all_passes_every_level() {
        for level in [LogLevel::Info, LogLevel::Warn, LogLevel::Error] {
            assert!(matches_filter(level, LogFilter::All));
        }
    }

    #[test]
    fn filter_warn_passes_warn_and_error_only() {
        assert!(!matches_filter(LogLevel::Info, LogFilter::Warn));
        assert!(matches_filter(LogLevel::Warn, LogFilter::Warn));
        assert!(matches_filter(LogLevel::Error, LogFilter::Warn));
    }

    #[test]
    fn filter_error_passes_error_only() {
        assert!(!matches_filter(LogLevel::Info, LogFilter::Error));
        assert!(!matches_filter(LogLevel::Warn, LogFilter::Error));
        assert!(matches_filter(LogLevel::Error, LogFilter::Error));
    }

    /// Settings files written by releases that predate `set_system_proxy` /
    /// `proxy_port` must still deserialize with the documented defaults filled
    /// in. The removed legacy `subscription_input` field is simply ignored.
    #[test]
    fn legacy_settings_file_gets_defaults_for_new_fields() {
        let legacy = r#"{"proxy_mode": true, "subscription_input": "https://example.com/sub"}"#;
        let settings: AppSettings = serde_json::from_str(legacy).unwrap();
        assert!(settings.proxy_mode);
        assert!(!settings.set_system_proxy);
        assert_eq!(settings.proxy_port, 7788);
        assert_eq!(settings.clash_api_port, 7789);
    }

    /// A settings file with no `profiles` array stays empty — no Default
    /// profile is fabricated — and has no active profile.
    #[test]
    fn settings_without_profiles_stays_empty() {
        let json = r#"{
            "proxy_mode": false,
            "subscription_input": "https://example.com/sub"
        }"#;
        let mut settings: AppSettings = serde_json::from_str(json).unwrap();
        settings.normalize_profiles();
        assert!(settings.profiles.is_empty());
        assert!(!settings.has_profiles());
        assert_eq!(settings.active_profile_id, "");
        assert!(settings.active_profile().is_none());
    }

    /// A profiles array written before per-profile intervals existed must
    /// deserialize with the documented 60-minute default.
    #[test]
    fn profile_without_interval_field_gets_default() {
        let profile: Profile = serde_json::from_str(
            r#"{"id": "p1", "name": "Default", "url": "https://a.example/s"}"#,
        )
        .unwrap();
        assert_eq!(profile.auto_update_interval(), 60);
    }

    /// An `active_profile_id` pointing at a deleted profile must snap back to
    /// the first remaining profile, never panic.
    #[test]
    fn normalize_fixes_dangling_active_profile_id() {
        let mut settings = AppSettings::default();
        settings.profiles = vec![
            Profile {
                id: "p3".into(),
                name: "A".into(),
                source: ProfileSource::Remote {
                    url: "https://a.example".into(),
                    auto_update_interval_minutes: 60,
                },
                last_updated_secs: None,
            },
            Profile {
                id: "p7".into(),
                name: "B".into(),
                source: ProfileSource::Remote {
                    url: "https://b.example".into(),
                    auto_update_interval_minutes: 60,
                },
                last_updated_secs: None,
            },
        ];
        settings.active_profile_id = "p99".into();
        settings.normalize_profiles();
        assert_eq!(settings.active_profile_id, "p3");
    }

    #[test]
    fn next_profile_id_increments_past_max() {
        let mut settings = AppSettings::default(); // now empty
        assert_eq!(settings.next_profile_id(), "p1");
        settings.profiles.push(Profile {
            id: "p7".into(),
            name: "X".into(),
            source: ProfileSource::Remote {
                url: String::new(),
                auto_update_interval_minutes: 60,
            },
            last_updated_secs: None,
        });
        assert_eq!(settings.next_profile_id(), "p8");
        // Non-numeric ids are ignored rather than crashing.
        settings.profiles.push(Profile {
            id: "imported".into(),
            name: "Y".into(),
            source: ProfileSource::Remote {
                url: String::new(),
                auto_update_interval_minutes: 60,
            },
            last_updated_secs: None,
        });
        assert_eq!(settings.next_profile_id(), "p8");
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "box_pilot_test_{}_{}",
            tag,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn load_returns_default_when_file_missing() {
        let dir = temp_dir("missing");
        let settings = AppSettings::load(&dir);
        assert_eq!(
            serde_json::to_string(&settings).unwrap(),
            serde_json::to_string(&AppSettings::default()).unwrap()
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_returns_default_when_file_corrupted() {
        let dir = temp_dir("corrupt");
        fs::write(dir.join(SETTINGS_FILE), "{not json").unwrap();
        let settings = AppSettings::load(&dir);
        assert_eq!(settings.proxy_port, 7788);
        assert_eq!(settings.clash_api_port, 7789);
        assert!(settings.profiles.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_then_load_roundtrips_all_fields() {
        let dir = temp_dir("roundtrip");
        let original = AppSettings {
            proxy_mode: true,
            set_system_proxy: true,
            proxy_port: 18888,
            clash_api_port: 17900,
            profiles: vec![
                Profile {
                    id: "p1".into(),
                    name: "Default".into(),
                    source: ProfileSource::Remote {
                        url: "https://example.com/sub?token=abc".into(),
                        auto_update_interval_minutes: 30,
                    },
                    last_updated_secs: Some(1_700_000_000),
                },
                Profile {
                    id: "p2".into(),
                    name: "Lab".into(),
                    source: ProfileSource::Local {
                        path: "/home/u/box.json".into(),
                    },
                    last_updated_secs: None,
                },
            ],
            active_profile_id: "p2".to_string(),
        };
        original.save(&dir);
        let loaded = AppSettings::load(&dir);
        assert_eq!(loaded.proxy_mode, original.proxy_mode);
        assert_eq!(loaded.set_system_proxy, original.set_system_proxy);
        assert_eq!(loaded.proxy_port, 18888);
        assert_eq!(loaded.clash_api_port, 17900);
        assert_eq!(loaded.profiles, original.profiles);
        assert_eq!(loaded.active_profile_id, "p2");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_flat_profile_migrates_to_remote_source() {
        let json = r#"{"id":"p1","name":"Sub","url":"https://a/s","auto_update_interval_minutes":30}"#;
        let profile: Profile = serde_json::from_str(json).unwrap();
        assert_eq!(profile.id, "p1");
        assert_eq!(
            profile.source,
            ProfileSource::Remote {
                url: "https://a/s".into(),
                auto_update_interval_minutes: 30,
            }
        );
    }

    #[test]
    fn local_profile_roundtrips_and_accessors() {
        let profile = Profile {
            id: "p2".into(),
            name: "Lab".into(),
            source: ProfileSource::Local {
                path: "/home/u/box.json".into(),
            },
            last_updated_secs: None,
        };
        let back: Profile =
            serde_json::from_str(&serde_json::to_string(&profile).unwrap()).unwrap();
        assert_eq!(back, profile);
        assert!(back.is_local());
        assert_eq!(back.local_path(), Some("/home/u/box.json"));
        assert_eq!(back.remote_url(), None);
        assert_eq!(back.auto_update_interval(), 0);
    }

    #[test]
    fn remote_profile_serializes_in_source_shape() {
        let profile = Profile {
            id: "p1".into(),
            name: "Sub".into(),
            source: ProfileSource::Remote {
                url: "https://a/s".into(),
                auto_update_interval_minutes: 15,
            },
            last_updated_secs: None,
        };
        let json = serde_json::to_string(&profile).unwrap();
        assert!(json.contains("\"source\""), "new shape: {}", json);
        assert!(json.contains("\"kind\":\"remote\""), "new shape: {}", json);
        let back: Profile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.remote_url(), Some("https://a/s"));
        assert!(!back.is_local());
    }

    /// `last_updated_secs` is absent in every pre-existing settings file and
    /// must default to `None`; when `None` it is omitted from the serialized
    /// form (skip_serializing_if), and a present value round-trips.
    #[test]
    fn last_updated_secs_defaults_and_round_trips() {
        let without = r#"{"id":"p1","name":"S","source":{"kind":"remote","url":"https://a/s","auto_update_interval_minutes":60}}"#;
        let profile: Profile = serde_json::from_str(without).unwrap();
        assert_eq!(profile.last_updated_secs, None);
        assert!(
            !serde_json::to_string(&profile)
                .unwrap()
                .contains("last_updated_secs"),
            "None must be skipped in the serialized form"
        );

        let stamped = Profile {
            last_updated_secs: Some(1_700_000_000),
            ..profile
        };
        let back: Profile =
            serde_json::from_str(&serde_json::to_string(&stamped).unwrap()).unwrap();
        assert_eq!(back.last_updated_secs, Some(1_700_000_000));
    }

    #[test]
    fn load_migrates_legacy_flat_profiles() {
        let dir = temp_dir("legacy_profiles");
        let legacy = r#"{
            "proxy_mode": false,
            "profiles": [
                {"id":"p1","name":"Sub","url":"https://a/s","auto_update_interval_minutes":45}
            ],
            "active_profile_id":"p1"
        }"#;
        fs::write(dir.join(SETTINGS_FILE), legacy).unwrap();
        let settings = AppSettings::load(&dir);
        assert_eq!(settings.profiles.len(), 1);
        assert_eq!(
            settings.profiles[0].source,
            ProfileSource::Remote {
                url: "https://a/s".into(),
                auto_update_interval_minutes: 45,
            }
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
