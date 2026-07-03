//! Relative-time formatting for the subscription "last updated" label.

use std::path::Path;
use std::time::{Duration, SystemTime};

/// "just now" (<60s) / "x min ago" / "x hr ago" / "x day(s) ago".
/// A `then` in the future (clock rollback) is treated as 0s → "just now".
pub fn format_relative_time(then: SystemTime, now: SystemTime) -> String {
    let secs = now
        .duration_since(then)
        .unwrap_or(Duration::ZERO)
        .as_secs();
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{} min ago", secs / 60)
    } else if secs < 86400 {
        format!("{} hr ago", secs / 3600)
    } else if secs < 86400 * 2 {
        "1 day ago".to_string()
    } else {
        format!("{} days ago", secs / 86400)
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
