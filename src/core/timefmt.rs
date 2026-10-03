//! Time formatting: the subscription "last updated" label, the Home
//! uptime readout and local date-times (connection details).

use crate::i18n::s;
use std::path::Path;
use std::time::{Duration, SystemTime};

/// "just now" (<60s) / "x min ago" / "x hr ago" / "x day(s) ago".
/// A `then` in the future (clock rollback) is treated as 0s → "just now".
pub fn format_relative_time(then: SystemTime, now: SystemTime) -> String {
    let secs = now
        .duration_since(then)
        .unwrap_or(Duration::ZERO)
        .as_secs();
    let t = &s().time;
    if secs < 60 {
        t.just_now.to_string()
    } else if secs < 3600 {
        (t.minutes_ago)(secs / 60)
    } else if secs < 86400 {
        (t.hours_ago)(secs / 3600)
    } else {
        (t.days_ago)(secs / 86400)
    }
}

/// `path` 的文件修改时间;文件不存在或元数据不可读时返回 `None`。
pub fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

/// `t` 距 Unix 纪元的整秒数;`t` 早于纪元(理论上不会)时返回 `None`。
/// 用于把"最后更新时刻"存进 settings.json(`Profile.last_updated_secs`)。
pub fn to_unix_secs(t: SystemTime) -> Option<u64> {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// `to_unix_secs` 的逆:Unix 秒 → `SystemTime`,供显示层喂给 `format_relative_time`。
pub fn from_unix_secs(secs: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
}

/// Compact elapsed-time label for the Home uptime readout: `42s` (under a
/// minute), `12m 5s` (under an hour), `1h 23m` (under a day), `2d 4h`. Two
/// units at most, so the label stays short; seconds show only within the
/// first hour, where the per-second tick is what tells the user it's live.
pub fn format_uptime(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    let (days, hours, mins, sec) = (
        secs / 86400,
        secs % 86400 / 3600,
        secs % 3600 / 60,
        secs % 60,
    );
    let t = &s().time;
    if secs < 60 {
        format!("{}{}", secs, t.second)
    } else if secs < 3600 {
        format!("{}{}{}{}{}", mins, t.minute, t.unit_sep, sec, t.second)
    } else if secs < 86400 {
        format!("{}{}{}{}{}", hours, t.hour, t.unit_sep, mins, t.minute)
    } else {
        format!("{}{}{}{}{}", days, t.day, t.unit_sep, hours, t.hour)
    }
}

/// How long ago `started_at_millis` (unix milliseconds, as sing-box's
/// `GetStartedAt` reports it) was, as of `now`. A start time in the future
/// (clock adjusted since) counts as zero.
pub fn uptime_since(started_at_millis: i64, now: SystemTime) -> Duration {
    let started = SystemTime::UNIX_EPOCH + Duration::from_millis(started_at_millis.max(0) as u64);
    now.duration_since(started).unwrap_or(Duration::ZERO)
}

/// Unix milliseconds as a date-time in the local time zone:
/// `2026-10-03 14:05:09`. The same in every UI language.
pub fn format_local_datetime(unix_millis: i64) -> String {
    format_datetime_in(unix_millis, &chrono::Local)
}

/// `format_local_datetime` in an explicit zone (tests pin one). Empty for a
/// timestamp chrono can't represent.
fn format_datetime_in<Tz: chrono::TimeZone>(unix_millis: i64, zone: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    match chrono::DateTime::from_timestamp_millis(unix_millis) {
        Some(utc) => utc
            .with_timezone(zone)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datetime_is_shown_in_the_given_zone() {
        // 2026-10-03 06:05:09.750 UTC; sub-seconds are dropped.
        let ms = 1_791_007_509_750;
        assert_eq!(format_datetime_in(ms, &chrono::Utc), "2026-10-03 06:05:09");
        let utc8 = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
        assert_eq!(format_datetime_in(ms, &utc8), "2026-10-03 14:05:09");
        // West of UTC it is still the day before.
        let utc_minus_7 = chrono::FixedOffset::west_opt(7 * 3600).unwrap();
        assert_eq!(format_datetime_in(ms, &utc_minus_7), "2026-10-02 23:05:09");
        assert_eq!(format_datetime_in(i64::MAX, &chrono::Utc), "");
        // The local zone gives the same shape, whatever zone the test runs in.
        assert_eq!(format_local_datetime(ms).len(), "2026-10-03 06:05:09".len());
    }

    fn at(secs_ago: u64) -> (SystemTime, SystemTime) {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        (now - Duration::from_secs(secs_ago), now)
    }

    #[test]
    fn just_now_under_a_minute() {
        let (then, now) = at(0);
        assert_eq!(format_relative_time(then, now), "just now");
        let (then, now) = at(59);
        assert_eq!(format_relative_time(then, now), "just now");
    }

    #[test]
    fn minutes() {
        let (then, now) = at(60);
        assert_eq!(format_relative_time(then, now), "1 min ago");
        let (then, now) = at(3599);
        assert_eq!(format_relative_time(then, now), "59 min ago");
    }

    #[test]
    fn hours() {
        let (then, now) = at(3600);
        assert_eq!(format_relative_time(then, now), "1 hr ago");
        let (then, now) = at(86399);
        assert_eq!(format_relative_time(then, now), "23 hr ago");
    }

    #[test]
    fn days() {
        let (then, now) = at(86400);
        assert_eq!(format_relative_time(then, now), "1 day ago");
        let (then, now) = at(86400 * 3 + 100);
        assert_eq!(format_relative_time(then, now), "3 days ago");
    }

    #[test]
    fn future_then_is_just_now() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        let then = now + Duration::from_secs(500); // clock rolled back
        assert_eq!(format_relative_time(then, now), "just now");
    }

    #[test]
    fn file_mtime_none_for_missing_file() {
        assert!(file_mtime(Path::new("/nonexistent/definitely/missing.json")).is_none());
    }

    #[test]
    fn file_mtime_some_for_existing_file() {
        // Cargo.toml 一定存在于工作目录。
        assert!(file_mtime(Path::new("Cargo.toml")).is_some());
    }

    #[test]
    fn unix_secs_round_trips() {
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let secs = to_unix_secs(t).unwrap();
        assert_eq!(secs, 1_700_000_000);
        assert_eq!(from_unix_secs(secs), t);
    }

    #[test]
    fn unix_secs_drops_sub_second() {
        let t = SystemTime::UNIX_EPOCH + Duration::from_millis(1_700_000_000_500);
        assert_eq!(to_unix_secs(t), Some(1_700_000_000));
    }

    #[test]
    fn from_unix_secs_feeds_relative_time() {
        let then = from_unix_secs(1_000_000_000);
        let now = from_unix_secs(1_000_000_000 + 120);
        assert_eq!(format_relative_time(then, now), "2 min ago");
    }

    #[test]
    fn uptime_under_a_minute_is_seconds() {
        assert_eq!(format_uptime(Duration::ZERO), "0s");
        assert_eq!(format_uptime(Duration::from_millis(59_999)), "59s");
    }

    #[test]
    fn uptime_under_an_hour_ticks_seconds() {
        assert_eq!(format_uptime(Duration::from_secs(60)), "1m 0s");
        assert_eq!(format_uptime(Duration::from_secs(12 * 60 + 5)), "12m 5s");
        assert_eq!(format_uptime(Duration::from_secs(3599)), "59m 59s");
    }

    #[test]
    fn uptime_hours_and_days_drop_seconds() {
        assert_eq!(format_uptime(Duration::from_secs(3600)), "1h 0m");
        assert_eq!(
            format_uptime(Duration::from_secs(3600 + 23 * 60 + 59)),
            "1h 23m"
        );
        assert_eq!(format_uptime(Duration::from_secs(86399)), "23h 59m");
        assert_eq!(format_uptime(Duration::from_secs(86400)), "1d 0h");
        assert_eq!(
            format_uptime(Duration::from_secs(2 * 86400 + 4 * 3600 + 59)),
            "2d 4h"
        );
    }

    #[test]
    fn uptime_since_measures_from_unix_millis() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_millis(1_759_400_065_500);
        assert_eq!(
            uptime_since(1_759_400_000_000, now),
            Duration::from_millis(65_500)
        );
    }

    #[test]
    fn uptime_since_future_start_is_zero() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_759_400_000);
        assert_eq!(uptime_since(1_759_400_005_000, now), Duration::ZERO);
    }
}
