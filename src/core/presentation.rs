//! Pure presentation derivations shared by the views: status → label mapping,
//! "updated N ago" labels, port-field sanitizing, profile-row subtitles, log
//! counts. Everything here is a plain function of state — the render fns just
//! place the results in layout. No gpui dependency.

use crate::core::settings::ProfileSource;
use crate::core::timefmt::{format_relative_time, from_unix_secs};
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
        match self {
            ConnectionStatus::Disconnected => "Disconnected",
            ConnectionStatus::Starting => "Starting…",
            ConnectionStatus::Connected => "Connected",
        }
    }
}

/// "updated N ago" label from a profile's last-content-change stamp.
/// `fallback` is page wording for `None` ("not updated yet" / "never updated").
pub fn updated_label(last_updated_secs: Option<u64>, now: SystemTime, fallback: &str) -> String {
    last_updated_secs
        .map(|secs| format!("updated {}", format_relative_time(from_unix_secs(secs), now)))
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

/// Profile-row subtitle + the empty-source flag that disables its ⟳ button.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileRowInfo {
    pub subtitle: String,
    pub source_empty: bool,
}

pub fn profile_row_info(source: &ProfileSource) -> ProfileRowInfo {
    let source_empty = source.is_empty_source();
    let subtitle = match source {
        ProfileSource::Remote {
            url,
            auto_update_interval_minutes,
        } => {
            if source_empty {
                "No subscription URL".to_string()
            } else if *auto_update_interval_minutes > 0 {
                format!("{} · auto-update {}m", url, auto_update_interval_minutes)
            } else {
                format!("{} · auto-update off", url)
            }
        }
        ProfileSource::Local { path } => {
            if source_empty {
                "No file selected".to_string()
            } else {
                format!("Local file · {}", path)
            }
        }
    };
    ProfileRowInfo {
        subtitle,
        source_empty,
    }
}

/// Logs-header count: the total, or "visible of total" while a filter hides
/// some lines.
pub fn log_count_label(visible: usize, total: usize) -> String {
    if visible == total {
        format!("{}", total)
    } else {
        format!("{} of {}", visible, total)
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
        assert_eq!(on.subtitle, "https://a/s · auto-update 30m");
        assert!(!on.source_empty);

        let off = profile_row_info(&ProfileSource::Remote {
            url: "https://a/s".into(),
            auto_update_interval_minutes: 0,
        });
        assert_eq!(off.subtitle, "https://a/s · auto-update off");

        let empty_remote = profile_row_info(&ProfileSource::Remote {
            url: "  ".into(),
            auto_update_interval_minutes: 60,
        });
        assert_eq!(empty_remote.subtitle, "No subscription URL");
        assert!(empty_remote.source_empty);

        let local = profile_row_info(&ProfileSource::Local {
            path: "C:\\box.json".into(),
        });
        assert_eq!(local.subtitle, "Local file · C:\\box.json");
        assert!(!local.source_empty);

        let empty_local = profile_row_info(&ProfileSource::Local { path: "".into() });
        assert_eq!(empty_local.subtitle, "No file selected");
        assert!(empty_local.source_empty);
    }

    #[test]
    fn log_count_label_shows_ratio_only_when_filtered() {
        assert_eq!(log_count_label(10, 10), "10");
        assert_eq!(log_count_label(3, 10), "3 of 10");
        assert_eq!(log_count_label(0, 0), "0");
    }
}
