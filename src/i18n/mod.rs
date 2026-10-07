//! BoxPilot's UI strings: one typed table per language ([`EN`],
//! [`ZH_CN`]), the current one behind [`s()`].
//!
//! Every string is a field, so a language that misses one does not compile.
//! Plain text is a `&'static str`; anything with a value in it is a
//! `fn(..) -> String`, so each language orders its words its own way.
//!
//! The current language is a process-wide atomic: read from any thread
//! (subscription fetches build their errors on the background executor), set
//! once at startup and again when the user picks another language in
//! Settings. No gpui here — `core` uses it too. Tests never set it (they run
//! in parallel); they read a table directly (`EN.home.memory`) instead.

mod en;
mod zh_cn;

pub use en::EN;
pub use zh_cn::ZH_CN;

use crate::core::settings::LanguagePreference;
use std::sync::atomic::{AtomicU8, Ordering};

/// A language BoxPilot's UI is available in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Hash)]
pub enum Language {
    #[default]
    English,
    SimplifiedChinese,
}

impl Language {
    /// This language's string table.
    pub fn strings(self) -> &'static Strings {
        match self {
            Language::English => &EN,
            Language::SimplifiedChinese => &ZH_CN,
        }
    }

    /// The locale gpui-component's own strings (dialog buttons, input
    /// context menus) use for this language (its `locales/ui.yml` keys).
    pub fn component_locale(self) -> &'static str {
        match self {
            Language::English => "en",
            Language::SimplifiedChinese => "zh-CN",
        }
    }

    fn to_u8(self) -> u8 {
        match self {
            Language::English => 0,
            Language::SimplifiedChinese => 1,
        }
    }

    fn from_u8(value: u8) -> Self {
        match value {
            1 => Language::SimplifiedChinese,
            _ => Language::English,
        }
    }
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

/// The current language's strings. English until [`set_language`] runs.
pub fn s() -> &'static Strings {
    current().strings()
}

/// The current language.
pub fn current() -> Language {
    Language::from_u8(CURRENT.load(Ordering::Relaxed))
}

/// Switch every later [`s()`] to `language`. Never call this from a test.
pub fn set_language(language: Language) {
    CURRENT.store(language.to_u8(), Ordering::Relaxed);
}

/// The language a preference means here: English / 简体中文 as chosen, and
/// "System" from the OS locale.
pub fn resolve(preference: LanguagePreference) -> Language {
    match preference {
        LanguagePreference::English => Language::English,
        LanguagePreference::SimplifiedChinese => Language::SimplifiedChinese,
        LanguagePreference::System => language_for_locale(sys_locale::get_locale().as_deref()),
    }
}

/// The language for an OS locale tag: any Chinese (`zh`, `zh-CN`,
/// `zh_TW.UTF-8`, `zh-Hans-SG`…) gets Simplified Chinese — the only Chinese
/// table there is — everything else (or no locale) English.
pub fn language_for_locale(locale: Option<&str>) -> Language {
    let Some(locale) = locale else {
        return Language::English;
    };
    let primary = locale
        .trim()
        .split(['-', '_', '.', '@'])
        .next()
        .unwrap_or("");
    if primary.eq_ignore_ascii_case("zh") {
        Language::SimplifiedChinese
    } else {
        Language::English
    }
}

/// A message with one value in it.
pub type Fmt1 = fn(&str) -> String;
/// A message with two values in it.
pub type Fmt2 = fn(&str, &str) -> String;
/// A message with three values in it.
pub type Fmt3 = fn(&str, &str, &str) -> String;
/// A message with a count in it.
pub type FmtN = fn(u64) -> String;

/// Every user-visible string BoxPilot writes itself, by area.
pub struct Strings {
    pub common: Common,
    pub status: Status,
    pub time: Time,
    pub nav: Nav,
    pub home: Home,
    pub profiles: Profiles,
    pub usage: Usage,
    pub groups: Groups,
    pub connections: Connections,
    pub connection_details: ConnectionDetails,
    pub logs: Logs,
    pub tools: Tools,
    pub tailscale: Tailscale,
    pub vpn: Vpn,
    pub settings: Settings,
    pub updates: Updates,
    pub config_viewer: ConfigViewer,
    pub chart: Chart,
    pub tray: Tray,
    pub close_dialog: CloseDialog,
    pub dialogs: Dialogs,
    pub messages: Messages,
    pub errors: Errors,
}

/// Words many places share.
pub struct Common {
    pub ok: &'static str,
    pub cancel: &'static str,
    pub close: &'static str,
    pub save: &'static str,
    /// "Save…": opens a save dialog.
    pub save_as: &'static str,
    pub delete: &'static str,
    pub copy: &'static str,
    pub copied: &'static str,
    pub search: &'static str,
    pub start: &'static str,
    pub stop: &'static str,
    pub unknown: &'static str,
    pub loading: &'static str,
    pub view: &'static str,
    pub download: &'static str,
    pub upload: &'static str,
    /// Between what failed and why: "Failed to save the file: <reason>".
    pub colon: &'static str,
}

/// The connection status (sidebar, Home, tray) and the power button.
pub struct Status {
    pub disconnected: &'static str,
    pub starting: &'static str,
    pub connected: &'static str,
    pub connect: &'static str,
    pub disconnect: &'static str,
}

/// Relative times and compact durations.
pub struct Time {
    pub just_now: &'static str,
    pub minutes_ago: FmtN,
    pub hours_ago: FmtN,
    pub days_ago: FmtN,
    /// Compact duration units (`1h 23m` / `1小时23分`).
    pub day: &'static str,
    pub hour: &'static str,
    pub minute: &'static str,
    pub second: &'static str,
    /// Between two compact units: " " / "".
    pub unit_sep: &'static str,
    /// Coarse durations (VPN uptime): "45s", "12 min", "3 hr 4 min", "2 d 5 hr".
    pub coarse_secs: FmtN,
    pub coarse_mins: FmtN,
    pub coarse_hours_mins: fn(u64, u64) -> String,
    pub coarse_days_hours: fn(u64, u64) -> String,
}

/// Sidebar page names.
pub struct Nav {
    pub home: &'static str,
    pub groups: &'static str,
    pub connections: &'static str,
    pub tailscale: &'static str,
    pub vpn: &'static str,
    pub profiles: &'static str,
    pub logs: &'static str,
    pub tools: &'static str,
    pub settings: &'static str,
}

pub struct Home {
    pub no_subscription_title: &'static str,
    pub no_subscription_hint: &'static str,
    pub add_subscription: &'static str,
    pub clash_mode: &'static str,
    pub memory: &'static str,
    pub connections: &'static str,
    pub uploaded: &'static str,
    pub downloaded: &'static str,
    pub proxy_mode: &'static str,
    pub mode_tun: &'static str,
    pub mode_proxy: &'static str,
    pub system_proxy: &'static str,
    /// "Running for 1h 23m".
    pub running_for: Fmt1,
    /// Under "Disconnected": what Connect would start with.
    pub ready_with: Fmt1,
    /// Under "Starting…": the profile sing-box is starting with (keeps the
    /// hero's second line, so the card doesn't shrink and grow back).
    pub starting_with: Fmt1,
    /// Heading of the Home card holding Proxy Mode / System Proxy / Clash
    /// Mode.
    pub quick_settings: &'static str,
    /// Heading of the Home card for the active profile.
    pub profile: &'static str,
    /// Last item of the Home profile switcher: opens the Profiles page.
    pub manage_profiles: &'static str,
}

pub struct Profiles {
    pub title: &'static str,
    pub add: &'static str,
    pub add_title: &'static str,
    pub edit_title: &'static str,
    pub name: &'static str,
    pub name_placeholder: &'static str,
    pub kind_subscription: &'static str,
    pub kind_local: &'static str,
    pub subscription_url: &'static str,
    pub url_placeholder: &'static str,
    /// Heading of the subscription dialog's update options.
    pub update_section: &'static str,
    /// The auto-update dropdown's label.
    pub auto_update: &'static str,
    /// Auto-update choices: "Off", "Every hour", "Every 6 hours",
    /// "Every day".
    pub interval_off: &'static str,
    pub interval_hours: FmtN,
    pub interval_daily: &'static str,
    /// The subscription dialog's switch: fetch through the running sing-box.
    pub update_via_sing_box: &'static str,
    /// Under that switch: the fallback when it fails.
    pub update_via_sing_box_hint: &'static str,
    pub config_file: &'static str,
    pub browse: &'static str,
    pub no_file_selected: &'static str,
    pub choose_json: &'static str,
    pub delete_title: Fmt1,
    pub delete_body: &'static str,
    pub active: &'static str,
    pub use_profile: &'static str,
    pub empty: &'static str,
    pub no_subscription_url: &'static str,
    /// How a subscription stays fresh, a sentence in the update button's
    /// tooltip: "Auto-updates every 30 min.".
    pub auto_update_every: FmtN,
    /// The same when it doesn't: "Auto-update is off.".
    pub auto_update_off_hint: &'static str,
    /// Beside the source of a subscription that doesn't auto-update.
    pub auto_update_off: &'static str,
    /// What a local-file profile is, beside its file name.
    pub local_file: &'static str,
    /// The update button of a profile never updated: "Update".
    pub update: &'static str,
    /// The update button while its profile updates: "Updating…".
    pub updating: &'static str,
    /// The update button after its profile's latest update failed.
    pub update_failed: &'static str,
    /// Update-button tooltip, when the profile last updated (local time):
    /// "Updated today at 14:32.".
    pub updated_today: Fmt1,
    pub updated_yesterday: Fmt1,
    /// "Updated on 2026-10-03 at 14:32.".
    pub updated_on: Fmt2,
    /// Last line of the update button's tooltip: what a click does.
    pub click_to_update: &'static str,
    pub click_to_reread: &'static str,
    pub click_to_retry: &'static str,
    pub invalid_url: &'static str,
    /// Name for a new profile created without one: "Profile 2".
    pub default_name: Fmt1,
    /// Name for an imported profile whose link names no host.
    pub imported: &'static str,
}

/// Subscription traffic / expiry.
pub struct Usage {
    /// "3.5 GB used".
    pub used: Fmt1,
    pub expires_today: &'static str,
    pub expires_in_days: fn(i64) -> String,
    pub expired_today: &'static str,
    pub expired_days_ago: fn(i64) -> String,
    pub used_up: &'static str,
    pub percent_used: fn(u128) -> String,
    /// Between two alert reasons.
    pub reason_sep: &'static str,
    /// `"Work" subscription: 92% of traffic used.`
    pub alert: Fmt2,
    pub expires_on: Fmt1,
    pub as_of: Fmt1,
}

pub struct Groups {
    pub title: &'static str,
    pub search_placeholder: &'static str,
    pub sort: &'static str,
    pub sort_default: &'static str,
    pub sort_delay: &'static str,
    pub test_all: &'static str,
    /// Tooltip of a group's test icon button.
    pub test_group: &'static str,
    pub test_delay: &'static str,
    /// urltest groups' badge.
    pub auto: &'static str,
    pub timeout: &'static str,
    pub empty_title: &'static str,
    pub empty_hint: &'static str,
    pub no_match_title: &'static str,
    pub no_match_hint: &'static str,
}

pub struct Connections {
    pub title: &'static str,
    pub filter_placeholder: &'static str,
    /// "3 open" — the rest of the summary line is rates and totals.
    pub open_count: FmtN,
    pub total: &'static str,
    /// The view choices; the segmented control adds each count.
    pub active_tab: &'static str,
    pub closed_tab: &'static str,
    pub newest: &'static str,
    pub traffic: &'static str,
    pub close_all: &'static str,
    pub close_connection: &'static str,
    /// A closed row's rate column.
    pub closed: &'static str,
    pub empty_title: &'static str,
    pub empty_hint: &'static str,
    pub no_match_title: &'static str,
    pub no_match_hint: &'static str,
    pub no_active_title: &'static str,
    pub no_active_hint: &'static str,
    pub no_closed_title: &'static str,
    pub no_closed_hint: &'static str,
}

/// The Connections page's details panel (one connection).
pub struct ConnectionDetails {
    /// Tooltip of the panel's ✕ (Esc does the same).
    pub close_panel: &'static str,
    pub gone_title: &'static str,
    pub gone_hint: &'static str,
    // Section headings.
    pub overview: &'static str,
    pub route: &'static str,
    pub source_section: &'static str,
    pub process_section: &'static str,
    pub traffic_section: &'static str,
    // Field labels.
    pub destination: &'static str,
    pub domain: &'static str,
    pub protocol: &'static str,
    pub network: &'static str,
    pub ip_version: &'static str,
    pub state: &'static str,
    pub active: &'static str,
    pub closed: &'static str,
    pub inbound: &'static str,
    pub rule: &'static str,
    pub chain: &'static str,
    pub outbound: &'static str,
    /// `from_outbound`: the outbound that handed the connection back to the
    /// router.
    pub from_outbound: &'static str,
    pub source_address: &'static str,
    pub user: &'static str,
    pub process_name: &'static str,
    pub process_path: &'static str,
    pub process_id: &'static str,
    pub process_user: &'static str,
    pub upload_speed: &'static str,
    pub download_speed: &'static str,
    pub uploaded: &'static str,
    pub downloaded: &'static str,
    pub opened_at: &'static str,
    pub closed_at: &'static str,
    pub duration: &'static str,
}

pub struct Logs {
    pub title: &'static str,
    /// "3 of 10" while a level filter hides some lines.
    pub count_of: fn(usize, usize) -> String,
    pub configured_level: &'static str,
    pub clear: &'static str,
    pub empty_title: &'static str,
    pub empty_hint: &'static str,
}

pub struct Tools {
    pub title: &'static str,
    pub not_running_title: &'static str,
    pub not_running_hint: &'static str,
    pub outbound: &'static str,
    pub search_outbounds: &'static str,
    pub default_outbound: &'static str,
    pub quality_section: &'static str,
    pub mode: &'static str,
    pub parallel: &'static str,
    pub serial: &'static str,
    pub max_runtime: &'static str,
    pub seconds: fn(u32) -> String,
    pub config_url: &'static str,
    pub config_url_placeholder: &'static str,
    pub accuracy: Fmt2,
    pub idle_latency: &'static str,
    pub accuracy_low: &'static str,
    pub accuracy_medium: &'static str,
    pub accuracy_high: &'static str,
    pub done: &'static str,
    pub failed: &'static str,
    pub cancelled: &'static str,
    pub fetching_config: &'static str,
    pub measuring_idle: &'static str,
    pub measuring_both: &'static str,
    pub measuring_download: &'static str,
    pub measuring_upload: &'static str,
    pub finishing: &'static str,
    pub measuring: &'static str,
    pub progress_timeout: &'static str,
    pub ended_without_result: &'static str,
    pub stun_section: &'static str,
    pub stun_server: &'static str,
    pub stun_binding: &'static str,
    pub stun_binding_answered: &'static str,
    pub stun_mapping: &'static str,
    pub stun_filtering: &'static str,
    pub stun_testing: &'static str,
    pub external_address: &'static str,
    pub latency: &'static str,
    pub nat_mapping: &'static str,
    pub nat_filtering: &'static str,
    pub nat_unsupported: &'static str,
    pub nat_endpoint_independent: &'static str,
    pub nat_address_dependent: &'static str,
    pub nat_address_port_dependent: &'static str,
    pub nat_full_cone: &'static str,
    pub nat_restricted_cone: &'static str,
    pub nat_port_restricted_cone: &'static str,
    pub nat_symmetric: &'static str,
    pub nat_full_cone_hint: &'static str,
    pub nat_restricted_cone_hint: &'static str,
    pub nat_port_restricted_cone_hint: &'static str,
    pub nat_independent_unknown_hint: &'static str,
    pub nat_symmetric_hint: &'static str,
    pub nat_dependent_hint: &'static str,
}

pub struct Tailscale {
    pub title: &'static str,
    pub empty_title: &'static str,
    pub empty_hint: &'static str,
    pub log_in: &'static str,
    pub log_in_tooltip: &'static str,
    pub log_out: &'static str,
    pub waiting_login_link: &'static str,
    pub log_in_hint: &'static str,
    pub waiting_approval: &'static str,
    pub tailnet: &'static str,
    pub this_device: &'static str,
    pub dns_name: &'static str,
    pub addresses: &'static str,
    pub logout_title: &'static str,
    pub logout_body: Fmt1,
    pub logout_key_auth: &'static str,
    pub exit_node: &'static str,
    pub exit_node_on: &'static str,
    pub exit_node_off: &'static str,
    pub no_exit_nodes: &'static str,
    pub exit_node_none: &'static str,
    pub offline_choice: Fmt1,
    pub ping_title: Fmt2,
    pub running: &'static str,
    pub waiting_reply: &'static str,
    pub mark_read: &'static str,
    pub new_files: FmtN,
    pub taildrop: &'static str,
    pub no_files_share: &'static str,
    pub no_files: &'static str,
    /// "Receiving <progress><sender suffix>".
    /// "Receiving 1.0 MB of 4.0 MB (25%)".
    pub receiving: Fmt1,
    /// "from laptop", shown beside a Taildrop file.
    pub from_sender: Fmt1,
    pub save_dialog_failed: Fmt1,
    pub delete_title: Fmt1,
    pub delete_body: &'static str,
    pub https_certs: &'static str,
    pub https_hint: &'static str,
    pub get_certificate: &'static str,
    pub copy_certificate: &'static str,
    pub certificate_copied: &'static str,
    pub certificate_title: Fmt1,
    pub certificate_body: Fmt2,
    pub save_here: &'static str,
    pub folder_dialog_failed: Fmt1,
    pub saved_pair: Fmt2,
    pub save_certificate_failed: Fmt1,
    pub devices: &'static str,
    pub no_devices: &'static str,
    pub ping: &'static str,
    pub badge_exit_node: &'static str,
    pub badge_exit_option: &'static str,
    pub badge_shared: &'static str,
    pub badge_key_expired: &'static str,
    pub unknown_user: &'static str,
    pub online: &'static str,
    pub last_seen: Fmt1,
    pub offline: &'static str,
    pub ping_failed: Fmt1,
    pub direct: &'static str,
    pub direct_via: Fmt1,
    pub peer_relay: Fmt1,
    pub derp_region: fn(i64) -> String,
    pub relayed: &'static str,
    /// "1.0 MB of 4.0 MB (25%)".
    pub progress_of: fn(&str, &str, u32) -> String,
    pub received: Fmt1,
    pub set_exit_node_failed: &'static str,
    pub logout_failed: &'static str,
    pub logged_out: &'static str,
    pub mark_read_failed: &'static str,
    pub delete_failed: &'static str,
    pub cancel_failed: &'static str,
    pub save_failed: &'static str,
    pub saved_to: Fmt1,
    pub certificate_failed: Fmt1,
    pub write_failed: Fmt1,
    pub download_incomplete: fn(u64, u64) -> String,
    pub download_cancelled: &'static str,
}

pub struct Vpn {
    pub title: &'static str,
    pub empty_title: &'static str,
    pub empty_hint: &'static str,
    /// "Sign in to OpenVPN "office"".
    pub sign_in_title: Fmt2,
    pub sign_in: &'static str,
    pub later: &'static str,
    pub continue_: &'static str,
    pub cancel_sign_in: &'static str,
    pub disconnect: &'static str,
    pub ended: &'static str,
    pub open_sign_in_page: &'static str,
    pub callback_address: &'static str,
    pub step_too_new: &'static str,
    pub callback_intro: &'static str,
    pub callback_body: Fmt1,
    /// Joins alternatives: "a or b".
    pub or: &'static str,
    pub last_attempt_failed: Fmt1,
    pub open_url_body: &'static str,
    pub unknown_step: Fmt1,
    pub username: &'static str,
    pub password: &'static str,
    pub account: Fmt1,
    pub response: &'static str,
    pub waiting_for_sing_box: &'static str,
    pub bus: Fmt1,
    pub serial: Fmt1,
    pub usbip_server: &'static str,
    pub no_devices_shared: &'static str,
    pub no_status: &'static str,
    pub default_server_hint: &'static str,
    pub connecting: &'static str,
    pub waiting_sign_in: &'static str,
    pub connected: &'static str,
    pub error: &'static str,
    pub failed: &'static str,
    pub row_uptime: &'static str,
    pub row_server: &'static str,
    pub row_protocol: &'static str,
    pub row_transport: &'static str,
    pub row_network: &'static str,
    pub row_cipher: &'static str,
    pub deadline_passed: &'static str,
    pub deadline_secs: fn(i64) -> String,
    pub deadline_mins: fn(i64) -> String,
    /// What an embedded browser would have to capture: cookie names…
    pub cookies: fn(&str, usize) -> String,
    /// …or response header names.
    pub headers: fn(&str, usize) -> String,
    pub browser_limitation: Fmt1,
    pub stream_status: Fmt2,
    pub sign_in_failed: &'static str,
    pub cancel_sign_in_failed: &'static str,
    pub form_changed: &'static str,
    pub choose_value: Fmt1,
    pub paste_address: &'static str,
    pub address_mismatch: Fmt1,
    pub cannot_answer: &'static str,
    pub enter_username: &'static str,
    pub enter_response: &'static str,
    pub usb_available: &'static str,
    pub usb_in_use: &'static str,
    pub usb_unavailable: &'static str,
    pub usb_wireless: &'static str,
    pub usb_device: &'static str,
}

pub struct Settings {
    pub title: &'static str,
    pub general: &'static str,
    pub network: &'static str,
    pub tun: &'static str,
    pub troubleshooting: &'static str,
    pub about: &'static str,
    pub language: &'static str,
    pub follow_system: &'static str,
    pub appearance: &'static str,
    pub theme_light: &'static str,
    pub theme_dark: &'static str,
    pub close_button: &'static str,
    pub close_hint: &'static str,
    pub close_no_tray_hint: &'static str,
    pub close_ask: &'static str,
    pub close_minimize: &'static str,
    pub close_quit: &'static str,
    pub local_proxy_port: &'static str,
    pub allow_lan: &'static str,
    pub lan_on_at: Fmt1,
    pub lan_on_port: fn(u16) -> String,
    pub lan_off_at: Fmt1,
    pub lan_off: &'static str,
    pub ipv6: &'static str,
    pub ipv6_hint: &'static str,
    pub clear_cache: &'static str,
    /// What clearing forgets, while sing-box is stopped.
    pub clear_cache_hint: &'static str,
    /// The same while sing-box runs (and holds the cache): why the button
    /// is unavailable.
    pub clear_cache_hint_connected: &'static str,
    /// The Clear cache row's button.
    pub clear_cache_action: &'static str,
    pub running_config: &'static str,
    pub running_config_hint: &'static str,
}

/// BoxPilot's own update check.
pub struct Updates {
    pub updates: &'static str,
    pub check_automatically: &'static str,
    pub check_automatically_hint: &'static str,
    pub not_checked: &'static str,
    pub checking: &'static str,
    pub up_to_date: &'static str,
    pub available: Fmt1,
    pub available_skipped: Fmt1,
    pub failed: Fmt1,
    pub download: &'static str,
    pub skip: &'static str,
    pub check_now: &'static str,
    /// The toast when a newer release is found.
    pub available_toast: Fmt1,
    pub unexpected_response: &'static str,
    pub prerelease: &'static str,
    pub bad_tag: Fmt1,
    pub invalid_proxy: &'static str,
    pub client_setup: &'static str,
    pub timed_out: &'static str,
    pub cannot_connect: &'static str,
    pub interrupted: &'static str,
    pub network_error: &'static str,
    pub no_release: &'static str,
    pub rate_limited: &'static str,
    pub http_status: fn(u16) -> String,
}

/// The Running config dialog.
pub struct ConfigViewer {
    pub title: &'static str,
    pub running: &'static str,
    pub running_hint: &'static str,
    pub preview: &'static str,
    pub preview_hint: &'static str,
    pub hide_credentials: &'static str,
    pub hide_credentials_tooltip: &'static str,
    pub no_profile_title: &'static str,
    pub no_profile_hint: &'static str,
    pub no_config_title: &'static str,
    pub no_config_hint: &'static str,
    pub load_failed_title: &'static str,
    pub search_tooltip: &'static str,
    pub open_folder: &'static str,
    pub open_folder_tooltip: &'static str,
    pub not_json: Fmt1,
    pub format_failed: Fmt1,
    pub profile_unreadable: Fmt1,
}

/// The Home traffic chart.
pub struct Chart {
    pub last_two_minutes: &'static str,
    pub now: &'static str,
    pub secs_ago: fn(u16) -> String,
    pub mins_ago: fn(u16) -> String,
    pub mins_secs_ago: fn(u16, u16) -> String,
}

/// The system tray menu and tooltip.
pub struct Tray {
    pub show: &'static str,
    pub system_proxy: &'static str,
    pub proxy_mode: &'static str,
    pub clash_mode: &'static str,
    pub profile: &'static str,
    pub quit: &'static str,
    /// "BoxPilot — Connected".
    pub tooltip: Fmt1,
}

/// The first-close "keep running in the tray?" dialog.
pub struct CloseDialog {
    pub title: &'static str,
    pub body: &'static str,
    pub dont_ask_again: &'static str,
    pub quit: &'static str,
    pub keep: &'static str,
}

/// Root-level dialogs.
pub struct Dialogs {
    pub import_title: &'static str,
    pub tun_grant_title: &'static str,
    pub tun_grant_body: &'static str,
    pub grant: &'static str,
}

/// Toasts and status lines from app state.
pub struct Messages {
    pub ready: &'static str,
    pub config_missing_startup: &'static str,
    pub config_missing: &'static str,
    pub auto_updated: &'static str,
    pub add_subscription_first: &'static str,
    /// "<sing-box binary> not found at <path>".
    pub sing_box_not_found: Fmt2,
    pub sing_box_too_old: Fmt2,
    pub api_port_retry: &'static str,
    pub clear_logs_failed: Fmt1,
    pub disconnect_to_clear_cache: &'static str,
    pub read_app_dir_failed: Fmt1,
    pub delete_cache_failed: Fmt1,
    pub cache_cleared: FmtN,
    pub no_cache: &'static str,
    pub url_empty: &'static str,
    pub no_file_selected: &'static str,
    pub queued_update: Fmt1,
    pub profile_updated: Fmt1,
    pub profile_up_to_date: Fmt1,
    /// `"Work": <error>`.
    pub profile_failed: Fmt2,
    pub ignored_import: Fmt1,
    pub clash_mode_failed: Fmt1,
    pub close_connection_failed: Fmt1,
    pub close_connections_failed: Fmt1,
    pub sing_box_exited: &'static str,
    pub start_failed: Fmt3,
    pub api_no_response: &'static str,
    pub groups_failed: Fmt1,
    pub switch_node_failed: Fmt1,
    pub save_group_state_failed: Fmt1,
    pub delay_test_failed: Fmt1,
    pub settings_backed_up: Fmt1,
    pub settings_unreadable: Fmt3,
    pub settings_read_failed: Fmt2,
}

/// BoxPilot-authored error text from `core` (what follows the colon is
/// usually the OS's or sing-box's own words).
pub struct Errors {
    pub read_failed: Fmt2,
    pub write_failed: Fmt2,
    pub write_config: Fmt2,
    pub create_failed: Fmt2,
    pub parse_config: Fmt1,
    pub not_object: &'static str,
    pub serialize_config: Fmt1,
    pub api_port: Fmt1,
    pub invalid_sub_url: &'static str,
    pub http_client: Fmt1,
    pub update_timed_out: &'static str,
    pub network_error: Fmt1,
    pub download_status: Fmt1,
    pub read_response: Fmt1,
    pub file_not_found: Fmt1,
    pub not_a_file: Fmt1,
    pub validation_temp: Fmt1,
    pub validation_failed: Fmt1,
    pub check_run_failed: Fmt1,
    pub killed_by_signal: Fmt1,
    pub exited_with_code: Fmt1,
    pub flush_dns_ok: &'static str,
    pub flush_dns_failed: Fmt1,
    pub flush_dns_run: Fmt1,
    pub run_command: Fmt2,
    pub command_failed: Fmt2,
    pub disable_proxy: Fmt1,
    pub pkexec_missing: &'static str,
    pub pkexec_run: Fmt1,
    pub tun_dismissed: &'static str,
    pub tun_not_authorized: &'static str,
    pub tun_failed: Fmt1,
    pub tun_failed_code: Fmt1,
    pub tun_terminated: &'static str,
    pub resolve_dir: &'static str,
    pub create_app_dir: Fmt1,
    pub exe_path: Fmt1,
    pub exe_dir: &'static str,
    pub unsupported_scheme: &'static str,
    pub unsupported_action: Fmt1,
    pub missing_url: &'static str,
    pub profile_url_scheme: &'static str,
    pub api_unreachable: Fmt1,
    pub api_timed_out: &'static str,
    pub api_stream: Fmt1,
    pub api_error: Fmt1,
    pub api_invalid_response: Fmt1,
}

#[cfg(test)]
mod tests;
