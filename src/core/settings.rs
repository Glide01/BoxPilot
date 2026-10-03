use crate::core::atomic_write::{write_atomic, FileAccess};
use crate::core::sub_usage::SubscriptionUsage;
use crate::core::timefmt::to_unix_secs;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

#[cfg(target_os = "windows")]
pub const SING_EXECUTABLE: &str = "sing-box.exe";
#[cfg(not(target_os = "windows"))]
pub const SING_EXECUTABLE: &str = "sing-box";
pub const CONFIG_FILENAME: &str = "config.json";
/// Per-profile configs live in `<app_dir>/configs/<profile_id>.json`. The
/// legacy single `config.json` is migrated into here on first launch.
pub const PROFILES_DIR: &str = "configs";
/// The prepared (inbounds + cache_file + BoxPilot's `api` service injected)
/// config sing-box actually
/// runs with. Rewritten from the active profile's canonical config on every
/// process start; the canonical `configs/<id>.json` files are never touched
/// by a start, so their bytes/mtime only change on a real subscription update.
pub const RUNTIME_CONFIG_FILENAME: &str = "running_config.json";
pub const SETTINGS_FILE: &str = "box_pilot_settings.json";
pub const PROXY_PORT: u16 = 7788;
pub const MAX_LOG_LINES: usize = 1000;
pub const HTTP_TIMEOUT_SECS: u64 = 8;

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
    /// Traffic / expiry from the subscription's `subscription-userinfo`
    /// header at the last fetch. `None` = never reported (or a Local profile).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<SubscriptionUsage>,
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
    #[serde(default)]
    usage: Option<SubscriptionUsage>,
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
            usage: de.usage,
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
    /// TUN 模式下是否给 TUN 接口分配 IPv6 地址(Settings 页可配)。关闭时
    /// IPv6 流量不进隧道,走物理网卡直连。默认关——设置文件里没有这个字段的
    /// 老安装升级后同样是关。
    #[serde(default)]
    pub tun_ipv6: bool,
    #[serde(default)]
    pub profiles: Vec<Profile>,
    #[serde(default)]
    pub active_profile_id: String,
    /// The highest `p<N>` number ever handed out (`next_profile_id`), so a
    /// deleted profile's id is never reused. Absent in files from releases
    /// that predate it; the first allocation then starts above the highest
    /// existing id.
    #[serde(default)]
    pub profile_id_counter: u64,
    /// Light / Dark / follow the OS.
    #[serde(default)]
    pub theme: ThemePreference,
    /// UI language; `System` follows the OS locale.
    #[serde(default)]
    pub language: LanguagePreference,
    /// Listen on all interfaces so other devices on the LAN can use the
    /// local proxy inbound. Off = loopback only.
    #[serde(default)]
    pub allow_lan: bool,
    /// What the window's close button does.
    #[serde(default)]
    pub close_action: CloseAction,
    /// Check GitHub for a newer BoxPilot release in the background.
    #[serde(default = "default_true")]
    pub check_updates: bool,
    /// A release the user chose to skip ("Skip this version"); no reminder
    /// for it, only for something newer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped_update_version: Option<String>,
}

/// Appearance setting. Unknown values (from a newer release) load as
/// `System` instead of failing the whole settings parse.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ThemePreference {
    Light,
    Dark,
    // Last: serde requires the `other` fallback to be the final variant.
    #[default]
    #[serde(other)]
    System,
}

/// UI language setting. Unknown values load as `System`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LanguagePreference {
    #[serde(rename = "en")]
    English,
    #[serde(rename = "zh-CN")]
    SimplifiedChinese,
    // Last: serde requires the `other` fallback to be the final variant.
    #[default]
    #[serde(rename = "system", other)]
    System,
}

/// What the window's close button does. Unknown values load as `Ask`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum CloseAction {
    MinimizeToTray,
    Quit,
    // Last: serde requires the `other` fallback to be the final variant.
    #[default]
    #[serde(other)]
    Ask,
}

pub fn default_true() -> bool {
    true
}

pub fn default_auto_update_interval() -> u64 {
    60
}

pub fn default_proxy_port() -> u16 {
    PROXY_PORT
}

impl Default for AppSettings {
    fn default() -> Self {
        let mut settings = Self {
            proxy_mode: false,
            set_system_proxy: false,
            proxy_port: default_proxy_port(),
            tun_ipv6: false,
            profiles: Vec::new(),
            active_profile_id: String::new(),
            profile_id_counter: 0,
            theme: ThemePreference::default(),
            language: LanguagePreference::default(),
            allow_lan: false,
            close_action: CloseAction::default(),
            check_updates: true,
            skipped_update_version: None,
        };
        settings.normalize_profiles();
        settings
    }
}

impl AppSettings {
    /// Read the settings file. Missing → defaults (first run). Any other
    /// problem never ends in silently replacing the user's file with
    /// defaults:
    /// - unparsable (corrupt, or truncated by a crash in an older release):
    ///   the file is moved aside to `<name>.bak-<unix secs>` first, so the
    ///   next save can't destroy the only copy;
    /// - unreadable (after a few retries, for a transient lock such as an
    ///   antivirus scan): the file is left alone and `persist` is false, so
    ///   the caller must not save over it this session.
    ///
    /// Either way `problem` says what happened, for the startup toast.
    pub fn load(app_dir: &Path) -> LoadedSettings {
        let settings_path = app_dir.join(SETTINGS_FILE);
        let mut loaded = match read_with_retry(&settings_path) {
            Ok(data) => match serde_json::from_str(&data) {
                Ok(settings) => LoadedSettings::ok(settings),
                Err(e) => {
                    eprintln!(
                        "Failed to parse settings from {}: {}",
                        settings_path.display(),
                        e
                    );
                    match back_up_bad_file(&settings_path) {
                        Ok(backup) => LoadedSettings {
                            settings: AppSettings::default(),
                            persist: true,
                            problem: Some(format!(
                                "Settings file was unreadable and has been backed up to {}. Started with default settings.",
                                backup.display()
                            )),
                        },
                        Err(backup_err) => LoadedSettings {
                            settings: AppSettings::default(),
                            persist: false,
                            problem: Some(format!(
                                "Settings file {} is unreadable ({}) and could not be backed up ({}). Started with default settings; changes won't be saved this session.",
                                settings_path.display(),
                                e,
                                backup_err
                            )),
                        },
                    }
                }
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                LoadedSettings::ok(AppSettings::default())
            }
            Err(e) => {
                eprintln!(
                    "Failed to read settings file {}: {}",
                    settings_path.display(),
                    e
                );
                LoadedSettings {
                    settings: AppSettings::default(),
                    persist: false,
                    problem: Some(format!(
                        "Couldn't read settings file {} ({}). Started with default settings; changes won't be saved until BoxPilot is restarted.",
                        settings_path.display(),
                        e
                    )),
                }
            }
        };
        loaded.settings.normalize_profiles();
        loaded
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

    /// Allocate a fresh "p<n>" id: above every id handed out before, deleted
    /// ones included, so a new profile can never inherit a deleted one's
    /// `configs/<id>.json` (or a fetch still landing for it). Bumps
    /// `profile_id_counter`; the caller persists it with the new profile.
    pub fn next_profile_id(&mut self) -> String {
        let max_existing = self
            .profiles
            .iter()
            .filter_map(|p| p.id.strip_prefix('p').and_then(|n| n.parse::<u64>().ok()))
            .max()
            .unwrap_or(0);
        let next = self.profile_id_counter.max(max_existing) + 1;
        self.profile_id_counter = next;
        format!("p{}", next)
    }

    /// Owner-only on Unix: subscription URLs often carry an access token.
    pub fn save(&self, app_dir: &Path) {
        let settings_path = app_dir.join(SETTINGS_FILE);
        match serde_json::to_string_pretty(self) {
            Ok(data) => {
                if let Err(e) = write_atomic(&settings_path, data.as_bytes(), FileAccess::OwnerOnly)
                {
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

/// What `AppSettings::load` found.
pub struct LoadedSettings {
    pub settings: AppSettings,
    /// False when the settings file exists but could be neither read nor
    /// moved aside: saving now would overwrite it with defaults, so the
    /// caller must not save this session.
    pub persist: bool,
    /// What went wrong, worded for the user; `None` on a normal load.
    pub problem: Option<String>,
}

impl LoadedSettings {
    fn ok(settings: AppSettings) -> Self {
        Self {
            settings,
            persist: true,
            problem: None,
        }
    }
}

/// How often `read_with_retry` tries before giving up, and the pause between.
const READ_ATTEMPTS: u32 = 4;
const READ_RETRY_DELAY: Duration = Duration::from_millis(100);

/// `fs::read_to_string`, retried briefly on errors other than NotFound: on
/// Windows an antivirus scan or backup tool can hold the file with a sharing
/// violation for a moment.
fn read_with_retry(path: &Path) -> io::Result<String> {
    let mut attempt = 1;
    loop {
        match fs::read_to_string(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound && attempt < READ_ATTEMPTS => {
                attempt += 1;
                std::thread::sleep(READ_RETRY_DELAY);
            }
            result => return result,
        }
    }
}

/// Move a settings file that failed to parse to `<name>.bak-<unix secs>`
/// (with `-<n>` appended if that name is taken) and return the new path.
fn back_up_bad_file(path: &Path) -> io::Result<PathBuf> {
    let secs = to_unix_secs(SystemTime::now()).unwrap_or(0);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut backup = path.with_file_name(format!("{}.bak-{}", name, secs));
    let mut n = 1;
    while backup.exists() {
        backup = path.with_file_name(format!("{}.bak-{}-{}", name, secs, n));
        n += 1;
    }
    fs::rename(path, &backup)?;
    Ok(backup)
}

pub fn powershell_proxy_command(port: u16) -> String {
    format!(
        "$env:HTTP_PROXY=\"http://127.0.0.1:{port}\"; $env:HTTPS_PROXY=\"http://127.0.0.1:{port}\"; $env:ALL_PROXY=\"socks5://127.0.0.1:{port}\"",
        port = port,
    )
}

/// For POSIX shells: bash/zsh on Linux, and WSL on Windows.
pub fn posix_proxy_command(port: u16) -> String {
    format!(
        "export http_proxy=\"http://127.0.0.1:{port}\" && export https_proxy=\"http://127.0.0.1:{port}\" && export all_proxy=\"socks5://127.0.0.1:{port}\"",
        port = port,
    )
}

pub fn fish_proxy_command(port: u16) -> String {
    format!(
        "set -gx http_proxy http://127.0.0.1:{port}; set -gx https_proxy http://127.0.0.1:{port}; set -gx all_proxy socks5://127.0.0.1:{port}",
        port = port,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_proxy_commands_use_the_port() {
        assert_eq!(
            powershell_proxy_command(7788),
            "$env:HTTP_PROXY=\"http://127.0.0.1:7788\"; $env:HTTPS_PROXY=\"http://127.0.0.1:7788\"; $env:ALL_PROXY=\"socks5://127.0.0.1:7788\""
        );
        assert_eq!(
            posix_proxy_command(7788),
            "export http_proxy=\"http://127.0.0.1:7788\" && export https_proxy=\"http://127.0.0.1:7788\" && export all_proxy=\"socks5://127.0.0.1:7788\""
        );
        assert_eq!(
            fish_proxy_command(1080),
            "set -gx http_proxy http://127.0.0.1:1080; set -gx https_proxy http://127.0.0.1:1080; set -gx all_proxy socks5://127.0.0.1:1080"
        );
    }

    /// Settings files written by releases that predate `set_system_proxy` /
    /// `proxy_port` / `tun_ipv6` must still deserialize with the documented
    /// defaults filled in. The removed legacy `subscription_input` field is
    /// simply ignored.
    #[test]
    fn legacy_settings_file_gets_defaults_for_new_fields() {
        let legacy = r#"{"proxy_mode": true, "subscription_input": "https://example.com/sub"}"#;
        let settings: AppSettings = serde_json::from_str(legacy).unwrap();
        assert!(settings.proxy_mode);
        assert!(!settings.set_system_proxy);
        assert_eq!(settings.proxy_port, 7788);
        assert!(
            !settings.tun_ipv6,
            "installs that predate the toggle get TUN IPv6 off"
        );
        assert_eq!(settings.theme, ThemePreference::System);
        assert_eq!(settings.language, LanguagePreference::System);
        assert!(!settings.allow_lan);
        assert_eq!(settings.close_action, CloseAction::Ask);
        assert!(settings.check_updates, "update checks default on");
        assert_eq!(settings.skipped_update_version, None);
        let saved = serde_json::to_value(&settings).unwrap();
        assert!(saved.get("skipped_update_version").is_none(), "{}", saved);
    }

    /// Preference values from a newer release must load as the default,
    /// not fail the whole parse (which would back up and reset settings).
    #[test]
    fn unknown_preference_values_load_as_defaults() {
        let json = r#"{"proxy_mode": true, "proxy_port": 18888,
            "theme": "auto", "language": "fr", "close_action": "hibernate"}"#;
        let settings: AppSettings = serde_json::from_str(json).unwrap();
        assert!(settings.proxy_mode);
        assert_eq!(settings.proxy_port, 18888);
        assert_eq!(settings.theme, ThemePreference::System);
        assert_eq!(settings.language, LanguagePreference::System);
        assert_eq!(settings.close_action, CloseAction::Ask);
    }

    /// The preference enums serialize to their documented strings.
    #[test]
    fn preference_values_serialize_to_stable_strings() {
        let to = |v: serde_json::Value| v.as_str().unwrap().to_string();
        let cases = [
            (serde_json::to_value(ThemePreference::Dark), "dark"),
            (serde_json::to_value(ThemePreference::System), "system"),
            (serde_json::to_value(LanguagePreference::English), "en"),
            (
                serde_json::to_value(LanguagePreference::SimplifiedChinese),
                "zh-CN",
            ),
            (serde_json::to_value(LanguagePreference::System), "system"),
            (
                serde_json::to_value(CloseAction::MinimizeToTray),
                "minimize_to_tray",
            ),
        ];
        for (value, expected) in cases {
            assert_eq!(to(value.unwrap()), expected);
        }
        let lang: LanguagePreference = serde_json::from_str(r#""zh-CN""#).unwrap();
        assert_eq!(lang, LanguagePreference::SimplifiedChinese);
        let theme: ThemePreference = serde_json::from_str(r#""light""#).unwrap();
        assert_eq!(theme, ThemePreference::Light);
    }

    /// A profile without `usage` (every pre-existing file) loads as `None`
    /// and serializes without the key; a present value round-trips.
    #[test]
    fn profile_usage_defaults_and_round_trips() {
        let without = r#"{"id":"p1","name":"S","source":{"kind":"remote","url":"https://a/s","auto_update_interval_minutes":60}}"#;
        let profile: Profile = serde_json::from_str(without).unwrap();
        assert_eq!(profile.usage, None);
        assert!(!serde_json::to_string(&profile).unwrap().contains("usage"));

        let with = Profile {
            usage: Some(SubscriptionUsage {
                upload: 1,
                download: 2,
                total: 0,
                expire: None,
                fetched_at: 3,
            }),
            ..profile
        };
        let json = serde_json::to_string(&with).unwrap();
        assert!(!json.contains("expire"), "None expire is skipped: {}", json);
        let back: Profile = serde_json::from_str(&json).unwrap();
        assert_eq!(back, with);
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
                usage: None,
            },
            Profile {
                id: "p7".into(),
                name: "B".into(),
                source: ProfileSource::Remote {
                    url: "https://b.example".into(),
                    auto_update_interval_minutes: 60,
                },
                last_updated_secs: None,
                usage: None,
            },
        ];
        settings.active_profile_id = "p99".into();
        settings.normalize_profiles();
        assert_eq!(settings.active_profile_id, "p3");
    }

    fn remote(id: &str) -> Profile {
        Profile {
            id: id.into(),
            name: id.into(),
            source: ProfileSource::Remote {
                url: String::new(),
                auto_update_interval_minutes: 60,
            },
            last_updated_secs: None,
            usage: None,
        }
    }

    #[test]
    fn next_profile_id_increments_past_max() {
        let mut settings = AppSettings::default(); // now empty
        assert_eq!(settings.next_profile_id(), "p1");
        settings.profiles.push(remote("p7"));
        assert_eq!(settings.next_profile_id(), "p8");
        // Non-numeric ids are ignored rather than crashing.
        settings.profiles.push(remote("imported"));
        assert_eq!(settings.next_profile_id(), "p9");
    }

    /// Deleting the newest profile must not free its id: a new profile
    /// would inherit its `configs/<id>.json`, or a fetch still landing for it.
    #[test]
    fn next_profile_id_never_reuses_a_deleted_id() {
        let mut settings = AppSettings::default();
        let first = settings.next_profile_id();
        settings.profiles.push(remote(&first));
        let second = settings.next_profile_id();
        settings.profiles.push(remote(&second));
        assert_eq!((first.as_str(), second.as_str()), ("p1", "p2"));

        settings.profiles.retain(|p| p.id != "p2");
        assert_eq!(settings.next_profile_id(), "p3");
        settings.profiles.clear();
        assert_eq!(settings.next_profile_id(), "p4");
    }

    /// Settings from releases without the counter start it above the
    /// highest existing id, and the counter survives a save/load.
    #[test]
    fn profile_id_counter_migrates_and_persists() {
        let legacy = r#"{
            "proxy_mode": false,
            "profiles": [
                {"id":"p4","name":"A","url":"https://a/s"},
                {"id":"p2","name":"B","url":"https://b/s"}
            ],
            "active_profile_id":"p4"
        }"#;
        let mut settings: AppSettings = serde_json::from_str(legacy).unwrap();
        assert_eq!(settings.profile_id_counter, 0);
        assert_eq!(settings.next_profile_id(), "p5");
        assert_eq!(settings.profile_id_counter, 5);

        let dir = temp_dir("id_counter");
        settings.profiles.retain(|p| p.id != "p4");
        settings.save(&dir);
        let mut loaded = AppSettings::load(&dir).settings;
        assert_eq!(loaded.profile_id_counter, 5);
        assert_eq!(loaded.next_profile_id(), "p6");
        let _ = fs::remove_dir_all(&dir);
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
        let loaded = AppSettings::load(&dir);
        assert!(loaded.persist);
        assert!(loaded.problem.is_none());
        let settings = loaded.settings;
        assert_eq!(
            serde_json::to_string(&settings).unwrap(),
            serde_json::to_string(&AppSettings::default()).unwrap()
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The sing-box API port used to be a setting (`api_port`, earlier
    /// `clash_api_port`); BoxPilot now picks a free one for every start.
    /// Files that still carry either field load as before, and the next
    /// save drops it.
    #[test]
    fn legacy_api_port_fields_are_ignored() {
        for legacy in [
            r#"{"proxy_mode": true, "proxy_port": 18888, "api_port": 17900}"#,
            r#"{"proxy_mode": true, "proxy_port": 18888, "clash_api_port": 17900}"#,
        ] {
            let settings: AppSettings = serde_json::from_str(legacy).unwrap();
            assert!(settings.proxy_mode);
            assert_eq!(settings.proxy_port, 18888);
            let saved = serde_json::to_value(&settings).unwrap();
            assert!(saved.get("api_port").is_none(), "{}", saved);
            assert!(saved.get("clash_api_port").is_none(), "{}", saved);
        }
    }

    fn backups(dir: &Path) -> Vec<PathBuf> {
        fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(&format!("{}.bak-", SETTINGS_FILE))
            })
            .collect()
    }

    /// A corrupt file falls back to defaults, but only after it was moved
    /// aside: the save that follows must not destroy the only copy.
    #[test]
    fn load_backs_up_corrupted_file_before_falling_back() {
        let dir = temp_dir("corrupt");
        fs::write(dir.join(SETTINGS_FILE), "{not json").unwrap();
        let loaded = AppSettings::load(&dir);
        assert_eq!(loaded.settings.proxy_port, 7788);
        assert!(loaded.settings.profiles.is_empty());
        assert!(loaded.persist);

        let backups = backups(&dir);
        assert_eq!(backups.len(), 1);
        assert_eq!(fs::read_to_string(&backups[0]).unwrap(), "{not json");
        let problem = loaded.problem.expect("the user is told");
        assert!(
            problem.contains(&backups[0].display().to_string()),
            "message names the backup: {}",
            problem
        );

        loaded.settings.save(&dir);
        assert_eq!(fs::read_to_string(&backups[0]).unwrap(), "{not json");
        let _ = fs::remove_dir_all(&dir);
    }

    /// An empty file (a truncated write from an older release) is corrupt
    /// too; a second corrupt load in the same second gets its own backup.
    #[test]
    fn repeated_corrupt_loads_keep_every_backup() {
        let dir = temp_dir("corrupt_twice");
        fs::write(dir.join(SETTINGS_FILE), "").unwrap();
        AppSettings::load(&dir);
        fs::write(dir.join(SETTINGS_FILE), "{").unwrap();
        AppSettings::load(&dir);
        let mut contents: Vec<String> = backups(&dir)
            .iter()
            .map(|p| fs::read_to_string(p).unwrap())
            .collect();
        contents.sort();
        assert_eq!(contents, vec!["".to_string(), "{".to_string()]);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A settings file that exists but can't be read is left alone, and the
    /// caller is told not to save over it.
    #[test]
    fn unreadable_file_is_not_persisted_over() {
        let dir = temp_dir("unreadable");
        // A directory in the file's place: reading it fails with an error
        // other than NotFound, on every platform.
        fs::create_dir(dir.join(SETTINGS_FILE)).unwrap();
        let loaded = AppSettings::load(&dir);
        assert!(!loaded.persist);
        assert!(loaded.problem.is_some());
        assert!(loaded.settings.profiles.is_empty());
        assert!(dir.join(SETTINGS_FILE).is_dir());
        assert!(backups(&dir).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_then_load_roundtrips_all_fields() {
        let dir = temp_dir("roundtrip");
        let original = AppSettings {
            proxy_mode: true,
            set_system_proxy: true,
            proxy_port: 18888,
            tun_ipv6: true,
            profiles: vec![
                Profile {
                    id: "p1".into(),
                    name: "Default".into(),
                    source: ProfileSource::Remote {
                        url: "https://example.com/sub?token=abc".into(),
                        auto_update_interval_minutes: 30,
                    },
                    last_updated_secs: Some(1_700_000_000),
                    usage: Some(SubscriptionUsage {
                        upload: 1_024,
                        download: 2_048,
                        total: 107_374_182_400,
                        expire: Some(1_800_000_000),
                        fetched_at: 1_700_000_100,
                    }),
                },
                Profile {
                    id: "p2".into(),
                    name: "Lab".into(),
                    source: ProfileSource::Local {
                        path: "/home/u/box.json".into(),
                    },
                    last_updated_secs: None,
                    usage: None,
                },
            ],
            active_profile_id: "p2".to_string(),
            profile_id_counter: 9,
            theme: ThemePreference::Dark,
            language: LanguagePreference::SimplifiedChinese,
            allow_lan: true,
            close_action: CloseAction::MinimizeToTray,
            check_updates: false,
            skipped_update_version: Some("1.14.0".into()),
        };
        original.save(&dir);
        let loaded = AppSettings::load(&dir).settings;
        assert_eq!(loaded.proxy_mode, original.proxy_mode);
        assert_eq!(loaded.set_system_proxy, original.set_system_proxy);
        assert_eq!(loaded.proxy_port, 18888);
        assert!(loaded.tun_ipv6);
        assert_eq!(loaded.profiles, original.profiles);
        assert_eq!(loaded.active_profile_id, "p2");
        assert_eq!(loaded.profile_id_counter, 9);
        assert_eq!(loaded.theme, ThemePreference::Dark);
        assert_eq!(loaded.language, LanguagePreference::SimplifiedChinese);
        assert!(loaded.allow_lan);
        assert_eq!(loaded.close_action, CloseAction::MinimizeToTray);
        assert!(!loaded.check_updates);
        assert_eq!(loaded.skipped_update_version.as_deref(), Some("1.14.0"));
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
            usage: None,
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
            usage: None,
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
        let settings = AppSettings::load(&dir).settings;
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
