//! Pure presentation derivations shared by the views: status → label mapping,
//! "updated N ago" labels, port-field sanitizing, profile-row subtitles,
//! redacted subscription URLs, log counts. Everything here is a plain
//! function of state — the render fns just place the results in layout. No
//! gpui dependency.

use crate::core::settings::ProfileSource;
use crate::i18n::{s, Strings};
use crate::core::timefmt::{format_relative_time, format_uptime, from_unix_secs, uptime_since};
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

/// "updated N ago" label from a profile's last-content-change stamp.
/// `fallback` is page wording for `None` ("not updated yet" / "never updated").
pub fn updated_label(last_updated_secs: Option<u64>, now: SystemTime, fallback: &str) -> String {
    last_updated_secs
        .map(|secs| (s().profiles.updated)(&format_relative_time(from_unix_secs(secs), now)))
        .unwrap_or_else(|| fallback.to_string())
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

/// What a profile row shows about its source: where it comes from (the
/// redacted URL or the file path), how it stays fresh, and the empty-source
/// flag that disables its ⟳ button. Source and detail are separate lines of
/// the row, never joined into one string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileRowInfo {
    /// The redacted subscription URL or the file path; the "no URL / no
    /// file" wording when there is none.
    pub source: String,
    /// "Auto-updates every 30 min", "Auto-update off" or "Local file";
    /// `None` without a source.
    pub detail: Option<String>,
    pub source_empty: bool,
}

pub fn profile_row_info(source: &ProfileSource) -> ProfileRowInfo {
    let t = s();
    let source_empty = source.is_empty_source();
    let (source, detail) = match source {
        ProfileSource::Remote {
            url,
            auto_update_interval_minutes,
        } => {
            if source_empty {
                (t.profiles.no_subscription_url.to_string(), None)
            } else if *auto_update_interval_minutes > 0 {
                (
                    redact_url(url),
                    Some((t.profiles.auto_update_every)(
                        *auto_update_interval_minutes,
                    )),
                )
            } else {
                (
                    redact_url(url),
                    Some(t.profiles.auto_update_off.to_string()),
                )
            }
        }
        ProfileSource::Local { path } => {
            if source_empty {
                (t.profiles.no_file_selected.to_string(), None)
            } else {
                (path.clone(), Some(t.profiles.local_file.to_string()))
            }
        }
    };
    ProfileRowInfo {
        source,
        detail,
        source_empty,
    }
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

    #[test]
    fn updated_label_formats_or_falls_back() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        let five_min_ago = 1_000_000_000 - 300;
        assert_eq!(
            updated_label(Some(five_min_ago), now, "never updated"),
            "updated 5 min ago"
        );
        assert_eq!(
            updated_label(None, now, "not updated yet"),
            "not updated yet"
        );
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
        assert_eq!(on.source, "https://a/s");
        assert_eq!(on.detail.as_deref(), Some("Auto-updates every 30 min"));
        assert!(!on.source_empty);

        let off = profile_row_info(&ProfileSource::Remote {
            url: "https://a/s".into(),
            auto_update_interval_minutes: 0,
        });
        assert_eq!(off.source, "https://a/s");
        assert_eq!(off.detail.as_deref(), Some("Auto-update off"));

        let empty_remote = profile_row_info(&ProfileSource::Remote {
            url: "  ".into(),
            auto_update_interval_minutes: 60,
        });
        assert_eq!(empty_remote.source, "No subscription URL");
        assert_eq!(empty_remote.detail, None);
        assert!(empty_remote.source_empty);

        let local = profile_row_info(&ProfileSource::Local {
            path: "C:\\box.json".into(),
        });
        assert_eq!(local.source, "C:\\box.json");
        assert_eq!(local.detail.as_deref(), Some("Local file"));
        assert!(!local.source_empty);

        let empty_local = profile_row_info(&ProfileSource::Local { path: "".into() });
        assert_eq!(empty_local.source, "No file selected");
        assert_eq!(empty_local.detail, None);
        assert!(empty_local.source_empty);
    }

    #[test]
    fn profile_row_info_redacts_the_url() {
        let row = profile_row_info(&ProfileSource::Remote {
            url: "https://sub.example.com/api/v1/client/subscribe?token=secret".into(),
            auto_update_interval_minutes: 30,
        });
        assert_eq!(
            row.source,
            "https://sub.example.com/api/v1/client/subscribe?…"
        );
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
