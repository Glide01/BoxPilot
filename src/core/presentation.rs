//! Pure presentation derivations shared by the views: status → label mapping,
//! a profile's freshness (its update button), port-field sanitizing,
//! profile-row subtitles,
//! redacted subscription URLs, log counts. Everything here is a plain
//! function of state — the render fns just place the results in layout. No
//! gpui dependency.

use crate::core::settings::{Profile, ProfileSource};
use crate::core::timefmt::{
    format_relative_time, format_uptime, from_unix_secs, local_day_and_time, to_unix_secs,
    uptime_since, LocalDay,
};
use crate::i18n::{s, Strings};
use std::fmt::Write;
use std::time::SystemTime;

/// The three-state connection status. The single source for its wording —
/// the sidebar footer dot/label and the Home hero title/power button all
/// derive from this instead of re-deriving from booleans.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionStatus {
    Disconnected,
    Starting,
    Connected,
}

impl ConnectionStatus {
    /// `Starting` wins over `Connected`: `ProcessSession` is `Preparing`
    /// before any child exists, so the two flags are mutually exclusive, but
    /// order the check defensively anyway.
    pub fn from_flags(is_starting: bool, is_running: bool) -> Self {
        if is_starting {
            ConnectionStatus::Starting
        } else if is_running {
            ConnectionStatus::Connected
        } else {
            ConnectionStatus::Disconnected
        }
    }

    pub fn label(self) -> &'static str {
        self.label_in(s())
    }

    /// [`Self::label`] in a given language (the tray builds its menu from
    /// the language in its snapshot).
    pub fn label_in(self, t: &'static Strings) -> &'static str {
        match self {
            ConnectionStatus::Disconnected => t.status.disconnected,
            ConnectionStatus::Starting => t.status.starting,
            ConnectionStatus::Connected => t.status.connected,
        }
    }

    /// Whether the power button takes a click. Only a start in progress
    /// holds it off (`AppState::toggle_process` ignores one anyway); a
    /// subscription fetch doesn't.
    pub fn can_toggle(self) -> bool {
        self != ConnectionStatus::Starting
    }

    /// The power button's tooltip: what a click does.
    pub fn power_action_label(self) -> &'static str {
        self.power_action_label_in(s())
    }

    pub fn power_action_label_in(self, t: &'static Strings) -> &'static str {
        match self {
            ConnectionStatus::Disconnected => t.status.connect,
            ConnectionStatus::Starting => t.status.starting,
            ConnectionStatus::Connected => t.status.disconnect,
        }
    }
}

/// A profile older than this reads as stale (if also older than
/// `STALE_INTERVALS` of its auto-updates): a day, so a subscription with
/// auto-update off, or an app closed overnight, isn't flagged too soon.
pub const STALE_AFTER_SECS: u64 = 86_400;
/// Auto-update intervals a profile may miss before it reads as stale.
pub const STALE_INTERVALS: u64 = 3;
/// Characters of a fetch error the update button's tooltip shows.
const ERROR_ROOM: usize = 160;

/// Where a profile's update button stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FreshnessState {
    /// Its fetch / re-read is in flight.
    Updating,
    /// Its latest fetch failed (the reason is in the tooltip).
    Failed,
    /// Never fetched: the button just says "Update".
    Never,
    /// Fetched; the label says how long ago. `stale` = a subscription
    /// noticeably older than it should be (see [`STALE_AFTER_SECS`]).
    Fresh { stale: bool },
}

/// A profile's update button: freshness and the update action in one
/// control, the same on Home and the Profiles page. The label is the state
/// ("25 min ago", "Updating…"), the tooltip the details and what a click
/// does, one sentence per line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Freshness {
    pub state: FreshnessState,
    pub label: String,
    pub tooltip: String,
}

/// The update button of `profile`; `None` without a source (nothing to
/// update — its source line says so). `updating` = its fetch is in flight,
/// `error` = why its latest one failed.
pub fn profile_freshness(
    profile: &Profile,
    updating: bool,
    error: Option<&str>,
    now: SystemTime,
) -> Option<Freshness> {
    if profile.source.is_empty_source() {
        return None;
    }
    let t = &s().profiles;
    // Installs from before `last_checked_secs` have only the content stamp.
    let synced = profile.last_checked_secs.max(profile.last_updated_secs);
    let synced_line = synced.map(|secs| {
        let (day, time) = local_day_and_time(secs, now);
        match day {
            LocalDay::Today => (t.updated_today)(&time),
            LocalDay::Yesterday => (t.updated_yesterday)(&time),
            LocalDay::Date(date) => (t.updated_on)(&date, &time),
        }
    });
    let (interval_line, click) = match &profile.source {
        ProfileSource::Remote {
            auto_update_interval_minutes: 0,
            ..
        } => (Some(t.auto_update_off_hint.to_string()), t.click_to_update),
        ProfileSource::Remote {
            auto_update_interval_minutes: minutes,
            ..
        } => (Some((t.auto_update_every)(*minutes)), t.click_to_update),
        ProfileSource::Local { .. } => (None, t.click_to_reread),
    };
    let join =
        |lines: Vec<Option<String>>| lines.into_iter().flatten().collect::<Vec<_>>().join("\n");

    let (state, label, tooltip) = if updating {
        (
            FreshnessState::Updating,
            t.updating.to_string(),
            join(vec![synced_line]),
        )
    } else if let Some(error) = error {
        (
            FreshnessState::Failed,
            t.update_failed.to_string(),
            join(vec![
                Some(short_error(error)),
                synced_line,
                Some(t.click_to_retry.to_string()),
            ]),
        )
    } else if let Some(secs) = synced {
        let age = to_unix_secs(now).unwrap_or(0).saturating_sub(secs);
        let interval_secs = profile.auto_update_interval() * 60;
        let stale = !profile.is_local()
            && age > STALE_AFTER_SECS.max(interval_secs.saturating_mul(STALE_INTERVALS));
        (
            FreshnessState::Fresh { stale },
            format_relative_time(from_unix_secs(secs), now),
            join(vec![synced_line, interval_line, Some(click.to_string())]),
        )
    } else {
        (
            FreshnessState::Never,
            t.update.to_string(),
            join(vec![interval_line, Some(click.to_string())]),
        )
    };
    Some(Freshness {
        state,
        label,
        tooltip,
    })
}

/// A fetch error's first line, cut to `ERROR_ROOM` characters.
fn short_error(error: &str) -> String {
    let line = error.lines().next().unwrap_or_default().trim();
    if line.chars().count() <= ERROR_ROOM {
        return line.to_string();
    }
    let mut out: String = line.chars().take(ERROR_ROOM - 1).collect();
    out.truncate(out.trim_end().len());
    out.push('…');
    out
}

/// The Settings-page port rule: a port field parses to a non-zero u16 or
/// falls back to `default`. The caller writes the sanitized value back into
/// the field so the display always matches what took effect.
pub fn sanitize_port(raw: &str, default: u16) -> u16 {
    match raw.trim().parse::<u16>() {
        Ok(p) if p > 0 => p,
        _ => default,
    }
}

/// What a profile's meta line says about its source: where it comes from,
/// short, with the whole of it for a tooltip, and a note when it is not a
/// subscription kept up to date on its own. Separate items of the line,
/// never joined into one string. (The auto-update interval itself lives in
/// the update button's tooltip.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileRowInfo {
    /// The subscription's host (`sub.example.com`) or the file's name; the
    /// "no URL / no file" wording when there is none.
    pub source: String,
    /// The redacted URL or the full path, when it says more than `source`.
    pub source_full: Option<String>,
    /// "Auto-update off" or "Local file"; `None` for an auto-updating
    /// subscription and without a source.
    pub note: Option<String>,
    pub source_empty: bool,
}

pub fn profile_row_info(source: &ProfileSource) -> ProfileRowInfo {
    let t = s();
    let source_empty = source.is_empty_source();
    let (short, full, note) = match source {
        ProfileSource::Remote { .. } if source_empty => {
            (t.profiles.no_subscription_url.to_string(), None, None)
        }
        ProfileSource::Remote {
            url,
            auto_update_interval_minutes,
        } => {
            let note = (*auto_update_interval_minutes == 0)
                .then(|| t.profiles.auto_update_off.to_string());
            match url_host(url) {
                Some(host) => (host, Some(redact_url(url)), note),
                None => (t.profiles.invalid_url.to_string(), None, note),
            }
        }
        ProfileSource::Local { .. } if source_empty => {
            (t.profiles.no_file_selected.to_string(), None, None)
        }
        ProfileSource::Local { path } => {
            let path = path.trim();
            // Either separator: a profile synced from Windows reads the
            // same on Linux.
            let name = path
                .rsplit(['/', '\\'])
                .find(|part| !part.is_empty())
                .unwrap_or(path);
            (
                name.to_string(),
                Some(path.to_string()),
                Some(t.profiles.local_file.to_string()),
            )
        }
    };
    ProfileRowInfo {
        source_full: full.filter(|full| *full != short),
        source: short,
        note,
        source_empty,
    }
}

/// A subscription URL's host, with its port when it names one:
/// `sub.example.com`, `127.0.0.1:8080`. `None` for anything that isn't a
/// URL with a host. Never carries a credential (see [`redact_url`]).
pub fn url_host(raw: &str) -> Option<String> {
    let url = reqwest::Url::parse(raw.trim()).ok()?;
    let host = url.host_str().filter(|h| !h.is_empty())?;
    Some(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    })
}

/// What `redact_url` shows for something that isn't a URL with a host (in
/// English; the UI shows the current language's `profiles.invalid_url`).
pub const INVALID_URL_LABEL: &str = crate::i18n::EN.profiles.invalid_url;

/// A subscription URL as BoxPilot shows or logs it: scheme, host, port and
/// path, with what may carry a credential masked as `…`: the query, the
/// fragment, any `user:pass@`, and path segments that look like an access
/// token (`looks_like_token`). So
/// `https://sub.example.com/api/v1/client/subscribe?token=abcd` shows as
/// `https://sub.example.com/api/v1/client/subscribe?…`, and
/// `https://sub.example.com/s/Xk2fP9qLm7RtW3vZ` as `https://sub.example.com/s/…`.
/// Anything that doesn't parse as a URL with a host is `INVALID_URL_LABEL`,
/// never echoed raw. Display only: the edit dialog and the fetch itself use
/// the real URL.
pub fn redact_url(raw: &str) -> String {
    let Ok(url) = reqwest::Url::parse(raw.trim()) else {
        return s().profiles.invalid_url.to_string();
    };
    let Some(host) = url.host_str().filter(|h| !h.is_empty()) else {
        return s().profiles.invalid_url.to_string();
    };
    let mut out = format!("{}://", url.scheme());
    if !url.username().is_empty() || url.password().is_some() {
        out.push_str("…@");
    }
    out.push_str(host);
    if let Some(port) = url.port() {
        let _ = write!(out, ":{}", port);
    }
    if url.path() != "/" {
        for segment in url.path_segments().into_iter().flatten() {
            out.push('/');
            out.push_str(&redact_path_segment(segment));
        }
    }
    if url.query().is_some() {
        out.push_str("?…");
    }
    if url.fragment().is_some() {
        out.push_str("#…");
    }
    out
}

/// `…` for a token-like segment, keeping a short extension (`….yaml`).
fn redact_path_segment(segment: &str) -> String {
    let (stem, ext) = match segment.rsplit_once('.') {
        Some((stem, ext))
            if (1..=5).contains(&ext.len()) && ext.chars().all(|c| c.is_ascii_alphanumeric()) =>
        {
            (stem, Some(ext))
        }
        _ => (segment, None),
    };
    if !looks_like_token(stem) {
        return segment.to_string();
    }
    match ext {
        Some(ext) => format!("….{}", ext),
        None => "…".to_string(),
    }
}

/// Providers that put the token in the path (`/sub/<token>`) use a long
/// random string: 16+ characters of `[A-Za-z0-9_-]` (base64 `=` padding
/// allowed) with a digit or a capital letter somewhere. All-lowercase words
/// like `client-subscribe-links` stay readable; a hex, base62 or UUID token
/// almost always has a digit or a capital.
fn looks_like_token(segment: &str) -> bool {
    let body = segment.trim_end_matches('=');
    body.len() >= 16
        && body
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && body
            .chars()
            .any(|c| c.is_ascii_digit() || c.is_ascii_uppercase())
}

/// Logs-header count, only while the level filter hides some lines
/// ("3 of 10"): a bare line total says nothing worth a glance.
pub fn log_count_label(visible: usize, total: usize) -> Option<String> {
    (visible != total).then(|| (s().logs.count_of)(visible, total))
}

/// What Home says about the running sing-box: "Running for 1h 23m" and
/// "sing-box 1.14.2", shown as separate lines. Either is `None` until its
/// API call has answered (`started_at_millis` from `GetStartedAt`,
/// `version` from `GetVersion` — the *running* sing-box, which Settings'
/// probed version need not be).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RuntimeInfo {
    pub uptime: Option<String>,
    pub version: Option<String>,
}

pub fn runtime_info(
    started_at_millis: Option<i64>,
    version: Option<&str>,
    now: SystemTime,
) -> RuntimeInfo {
    RuntimeInfo {
        uptime: started_at_millis
            .map(|started| (s().home.running_for)(&format_uptime(uptime_since(started, now)))),
        version: version
            .filter(|v| !v.is_empty())
            .map(|v| format!("sing-box {}", v)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn connection_status_maps_flags_and_labels() {
        assert_eq!(
            ConnectionStatus::from_flags(false, false),
            ConnectionStatus::Disconnected
        );
        assert_eq!(
            ConnectionStatus::from_flags(true, false),
            ConnectionStatus::Starting
        );
        assert_eq!(
            ConnectionStatus::from_flags(false, true),
            ConnectionStatus::Connected
        );
        assert_eq!(
            ConnectionStatus::from_flags(true, true),
            ConnectionStatus::Starting,
            "starting wins defensively"
        );
        assert_eq!(ConnectionStatus::Starting.label(), "Starting…");
        assert_eq!(ConnectionStatus::Connected.label(), "Connected");
        assert_eq!(ConnectionStatus::Disconnected.label(), "Disconnected");
    }

    #[test]
    fn power_button_is_only_held_off_while_starting() {
        assert!(ConnectionStatus::Disconnected.can_toggle());
        assert!(ConnectionStatus::Connected.can_toggle());
        assert!(!ConnectionStatus::Starting.can_toggle());
        assert_eq!(
            ConnectionStatus::Disconnected.power_action_label(),
            "Connect"
        );
        assert_eq!(
            ConnectionStatus::Connected.power_action_label(),
            "Disconnect"
        );
        assert_eq!(ConnectionStatus::Starting.power_action_label(), "Starting…");
    }

    fn profile(source: ProfileSource, checked_ago: Option<u64>) -> Profile {
        Profile {
            id: "p1".into(),
            name: "Work".into(),
            source,
            last_updated_secs: None,
            last_checked_secs: checked_ago.map(|ago| NOW - ago),
            usage: None,
        }
    }

    const NOW: u64 = 1_000_000_000;

    fn remote(minutes: u64) -> ProfileSource {
        ProfileSource::Remote {
            url: "https://sub.example.com/s".into(),
            auto_update_interval_minutes: minutes,
        }
    }

    fn freshness(profile: &Profile, updating: bool, error: Option<&str>) -> Freshness {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(NOW);
        profile_freshness(profile, updating, error, now).unwrap()
    }

    #[test]
    fn freshness_says_how_long_ago_and_what_a_click_does() {
        let fresh = freshness(&profile(remote(60), Some(300)), false, None);
        assert_eq!(fresh.state, FreshnessState::Fresh { stale: false });
        assert_eq!(fresh.label, "5 min ago");
        let lines: Vec<&str> = fresh.tooltip.lines().collect();
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(lines[0].starts_with("Updated "), "{lines:?}");
        assert_eq!(lines[1], "Auto-updates every 60 min.");
        assert_eq!(lines[2], "Click to update now.");
        assert!(!fresh.tooltip.contains('·'));

        let off = freshness(&profile(remote(0), Some(300)), false, None);
        assert!(off.tooltip.contains("Auto-update is off."));

        let local = freshness(
            &profile(
                ProfileSource::Local {
                    path: "/a/b.json".into(),
                },
                Some(300),
            ),
            false,
            None,
        );
        assert_eq!(local.tooltip.lines().count(), 2);
        assert!(local.tooltip.ends_with("Click to read the file again."));
    }

    #[test]
    fn freshness_falls_back_to_the_content_stamp() {
        let mut old = profile(remote(60), None);
        old.last_updated_secs = Some(NOW - 7200);
        assert_eq!(freshness(&old, false, None).label, "2 hr ago");
        // The newer of the two wins.
        old.last_checked_secs = Some(NOW - 60);
        assert_eq!(freshness(&old, false, None).label, "1 min ago");
    }

    #[test]
    fn freshness_goes_stale_after_a_day_and_three_intervals() {
        let state =
            |minutes, ago| freshness(&profile(remote(minutes), Some(ago)), false, None).state;
        let stale = FreshnessState::Fresh { stale: true };
        let fine = FreshnessState::Fresh { stale: false };
        assert_eq!(state(60, 20 * 3600), fine, "within a day");
        assert_eq!(state(60, 25 * 3600), stale);
        assert_eq!(state(0, 25 * 3600), stale, "auto-update off");
        assert_eq!(state(720, 30 * 3600), fine, "within three intervals");
        assert_eq!(state(720, 37 * 3600), stale);
        let local = profile(
            ProfileSource::Local {
                path: "/a.json".into(),
            },
            Some(9 * 86_400),
        );
        assert_eq!(
            freshness(&local, false, None).state,
            fine,
            "a file has no schedule"
        );
    }

    #[test]
    fn freshness_shows_updating_failed_and_never() {
        let p = profile(remote(60), Some(300));
        let updating = freshness(&p, true, Some("old error"));
        assert_eq!(updating.state, FreshnessState::Updating);
        assert_eq!(updating.label, "Updating…");

        let failed = freshness(&p, false, Some("HTTP 503\nbody"));
        assert_eq!(failed.state, FreshnessState::Failed);
        assert_eq!(failed.label, "Update failed");
        let lines: Vec<&str> = failed.tooltip.lines().collect();
        assert_eq!(lines[0], "HTTP 503");
        assert!(lines[1].starts_with("Updated "));
        assert_eq!(lines[2], "Click to try again.");

        let long = freshness(&p, false, Some("x".repeat(500).as_str()));
        assert!(long.tooltip.lines().next().unwrap().ends_with('…'));
        assert!(long.tooltip.len() < 300);

        let never = freshness(&profile(remote(60), None), false, None);
        assert_eq!(never.state, FreshnessState::Never);
        assert_eq!(never.label, "Update");
        assert_eq!(
            never.tooltip,
            "Auto-updates every 60 min.\nClick to update now."
        );

        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(NOW);
        let empty = profile(remote(60), None);
        let empty = Profile {
            source: ProfileSource::Remote {
                url: " ".into(),
                auto_update_interval_minutes: 60,
            },
            ..empty
        };
        assert_eq!(profile_freshness(&empty, false, None, now), None);
    }

    #[test]
    fn sanitize_port_accepts_valid_and_falls_back() {
        assert_eq!(sanitize_port("7788", 1234), 7788);
        assert_eq!(sanitize_port(" 8080 ", 1234), 8080);
        for bad in ["", "0", "abc", "-1", "65536", "80.5"] {
            assert_eq!(sanitize_port(bad, 1234), 1234, "raw: {:?}", bad);
        }
    }

    #[test]
    fn profile_row_info_covers_all_source_shapes() {
        let on = profile_row_info(&ProfileSource::Remote {
            url: "https://a/s".into(),
            auto_update_interval_minutes: 30,
        });
        assert_eq!(on.source, "a");
        assert_eq!(on.source_full.as_deref(), Some("https://a/s"));
        assert_eq!(on.note, None);
        assert!(!on.source_empty);

        let off = profile_row_info(&ProfileSource::Remote {
            url: "https://a/s".into(),
            auto_update_interval_minutes: 0,
        });
        assert_eq!(off.source, "a");
        assert_eq!(off.note.as_deref(), Some("Auto-update off"));

        let invalid = profile_row_info(&ProfileSource::Remote {
            url: "not a url".into(),
            auto_update_interval_minutes: 30,
        });
        assert_eq!(invalid.source, "Invalid URL");
        assert_eq!(invalid.source_full, None);

        let empty_remote = profile_row_info(&ProfileSource::Remote {
            url: "  ".into(),
            auto_update_interval_minutes: 60,
        });
        assert_eq!(empty_remote.source, "No subscription URL");
        assert_eq!(empty_remote.note, None);
        assert!(empty_remote.source_empty);

        let local = profile_row_info(&ProfileSource::Local {
            path: "C:\\box.json".into(),
        });
        assert_eq!(local.source, "box.json");
        assert_eq!(local.source_full.as_deref(), Some("C:\\box.json"));
        assert_eq!(local.note.as_deref(), Some("Local file"));
        assert!(!local.source_empty);
        let unix = profile_row_info(&ProfileSource::Local {
            path: "/home/me/configs/box.json".into(),
        });
        assert_eq!(unix.source, "box.json");

        let empty_local = profile_row_info(&ProfileSource::Local { path: "".into() });
        assert_eq!(empty_local.source, "No file selected");
        assert_eq!(empty_local.note, None);
        assert!(empty_local.source_empty);
    }

    #[test]
    fn profile_row_info_redacts_the_url() {
        let row = profile_row_info(&ProfileSource::Remote {
            url: "https://sub.example.com/api/v1/client/subscribe?token=secret".into(),
            auto_update_interval_minutes: 30,
        });
        assert_eq!(row.source, "sub.example.com");
        assert_eq!(
            row.source_full.as_deref(),
            Some("https://sub.example.com/api/v1/client/subscribe?…")
        );
    }

    #[test]
    fn url_host_keeps_an_explicit_port_and_drops_credentials() {
        assert_eq!(
            url_host(" https://user:pw@sub.example.com/s?token=x ").as_deref(),
            Some("sub.example.com")
        );
        assert_eq!(
            url_host("http://127.0.0.1:8001/sub.json").as_deref(),
            Some("127.0.0.1:8001")
        );
        assert_eq!(
            url_host("https://sub.example.com:443/s").as_deref(),
            Some("sub.example.com")
        );
        assert_eq!(url_host("nonsense"), None);
    }

    #[test]
    fn redact_url_keeps_scheme_host_port_and_path() {
        assert_eq!(redact_url("https://a/s"), "https://a/s");
        assert_eq!(
            redact_url("  https://sub.example.com:8443/api/v1/client/subscribe  "),
            "https://sub.example.com:8443/api/v1/client/subscribe"
        );
        assert_eq!(
            redact_url("https://sub.example.com"),
            "https://sub.example.com"
        );
        assert_eq!(
            redact_url("https://sub.example.com/"),
            "https://sub.example.com"
        );
        assert_eq!(
            redact_url("http://sub.example.com/a/"),
            "http://sub.example.com/a/"
        );
        // The default port is implied, as the URL parser has it.
        assert_eq!(
            redact_url("https://sub.example.com:443/s"),
            "https://sub.example.com/s"
        );
    }

    #[test]
    fn redact_url_masks_query_and_fragment() {
        assert_eq!(
            redact_url("https://sub.example.com/api/v1/client/subscribe?token=abcd"),
            "https://sub.example.com/api/v1/client/subscribe?…"
        );
        assert_eq!(
            redact_url("https://sub.example.com/s?flag=clash&token=abcd#frag"),
            "https://sub.example.com/s?…#…"
        );
        assert_eq!(
            redact_url("https://sub.example.com?token=abcd"),
            "https://sub.example.com?…"
        );
    }

    #[test]
    fn redact_url_masks_userinfo() {
        assert_eq!(
            redact_url("https://user:hunter2@sub.example.com/s"),
            "https://…@sub.example.com/s"
        );
        assert_eq!(
            redact_url("https://user@sub.example.com/s"),
            "https://…@sub.example.com/s"
        );
        assert_eq!(
            redact_url("https://:pw@sub.example.com/s"),
            "https://…@sub.example.com/s"
        );
    }

    #[test]
    fn redact_url_masks_token_like_path_segments() {
        assert_eq!(
            redact_url("https://sub.example.com/sub/Xk2fP9qLm7RtW3vZ"),
            "https://sub.example.com/sub/…"
        );
        assert_eq!(
            redact_url(
                "https://sub.example.com/link/0123456789abcdef0123456789abcdef?flag=sing-box"
            ),
            "https://sub.example.com/link/…?…"
        );
        assert_eq!(
            redact_url("https://sub.example.com/s/3f2b8c1e-9d4a-4b7e-a1c2-5e6f7a8b9c0d"),
            "https://sub.example.com/s/…"
        );
        assert_eq!(
            redact_url("https://sub.example.com/s/aGVsbG8gd29ybGQgdG9rZW4=.json"),
            "https://sub.example.com/s/….json"
        );
        // Words, and short ids, stay.
        assert_eq!(
            redact_url("https://sub.example.com/client-subscribe-links/v2/abc123"),
            "https://sub.example.com/client-subscribe-links/v2/abc123"
        );
        assert_eq!(
            redact_url("https://sub.example.com/Short1234567890"),
            "https://sub.example.com/Short1234567890"
        );
        assert_eq!(
            redact_url("https://sub.example.com/Short12345678901"),
            "https://sub.example.com/…"
        );
    }

    #[test]
    fn redact_url_handles_ip_hosts() {
        assert_eq!(
            redact_url("http://[2001:db8::1]:8080/sub?token=x"),
            "http://[2001:db8::1]:8080/sub?…"
        );
        assert_eq!(redact_url("http://192.0.2.7/sub"), "http://192.0.2.7/sub");
    }

    #[test]
    fn redact_url_never_echoes_what_it_cannot_parse() {
        for raw in [
            "",
            "sub.example.com/sub?token=secret",
            "https://",
            "https://[::1/sub?token=secret",
            "not a url token=secret",
            "mailto:secret@example.com",
            "file:///home/u/secret.json",
        ] {
            assert_eq!(redact_url(raw), INVALID_URL_LABEL, "raw: {:?}", raw);
        }
    }

    #[test]
    fn log_count_label_shows_ratio_only_when_filtered() {
        assert_eq!(log_count_label(10, 10), None);
        assert_eq!(log_count_label(3, 10).as_deref(), Some("3 of 10"));
        assert_eq!(log_count_label(0, 10).as_deref(), Some("0 of 10"));
        assert_eq!(log_count_label(0, 0), None);
    }

    #[test]
    fn runtime_info_keeps_uptime_and_version_apart() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_759_405_000);
        let started = Some(1_759_405_000_000 - (3600 + 23 * 60) * 1000);
        assert_eq!(
            runtime_info(started, Some("1.14.2"), now),
            RuntimeInfo {
                uptime: Some("Running for 1h 23m".into()),
                version: Some("sing-box 1.14.2".into()),
            }
        );
        assert_eq!(runtime_info(started, None, now).version, None);
        assert_eq!(runtime_info(None, Some("1.14.2"), now).uptime, None);
        assert_eq!(runtime_info(None, Some(""), now), RuntimeInfo::default());
        assert_eq!(runtime_info(None, None, now), RuntimeInfo::default());
    }
}
