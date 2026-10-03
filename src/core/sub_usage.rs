//! Subscription traffic / expiry reported by a subscription server's
//! `subscription-userinfo` response header
//! (`upload=…; download=…; total=…; expire=…`), and how BoxPilot presents
//! it: a usage label, a warning level, an expiry date. Pure functions, no
//! gpui; the clock is always passed in.

use crate::core::bytefmt::format_bytes;
use crate::i18n::s;
use serde::{Deserialize, Serialize};

/// The response header subscription servers report usage in.
pub const USERINFO_HEADER: &str = "subscription-userinfo";

const DAY_SECS: i64 = 86_400;
/// Warn once this share of the allowance is used.
const WARN_FRACTION: f32 = 0.9;
/// Warn once this many whole days (or fewer) are left before expiry.
const WARN_DAYS: i64 = 3;

/// One `subscription-userinfo` reading, stored on the profile it came from.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubscriptionUsage {
    /// Bytes uploaded in the current billing period.
    pub upload: u64,
    /// Bytes downloaded in the current billing period.
    pub download: u64,
    /// Traffic allowance in bytes; 0 = unlimited / not reported.
    pub total: u64,
    /// Expiry as Unix-epoch seconds; `None` = no expiry reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expire: Option<u64>,
    /// When this reading was fetched, Unix-epoch seconds.
    pub fetched_at: u64,
}

/// How urgently a subscription needs the user's attention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum UsageLevel {
    Normal,
    /// ≥ 90% of the allowance used, or 3 days or fewer left.
    Warning,
    /// Allowance used up, or expired.
    Critical,
}

/// Parse a `subscription-userinfo` header value. Fields are `;`-separated
/// `key=value` pairs; keys are matched case-insensitively, whitespace is
/// trimmed, and numbers may be written as floats (`1.5e10`, some panels do).
/// Unknown keys, garbage and negative numbers are ignored. `expire` of 0
/// (or missing) means no expiry. `None` when none of upload / download /
/// total is present — nothing worth showing.
pub fn parse_userinfo(header: &str, now_secs: u64) -> Option<SubscriptionUsage> {
    let (mut upload, mut download, mut total, mut expire) = (None, None, None, None);
    for field in header.split(';') {
        let Some((key, value)) = field.split_once('=') else {
            continue;
        };
        let Some(value) = parse_amount(value) else {
            continue;
        };
        match key.trim().to_ascii_lowercase().as_str() {
            "upload" => upload = Some(value),
            "download" => download = Some(value),
            "total" => total = Some(value),
            "expire" => expire = Some(value),
            _ => {}
        }
    }
    if upload.is_none() && download.is_none() && total.is_none() {
        return None;
    }
    Some(SubscriptionUsage {
        upload: upload.unwrap_or(0),
        download: download.unwrap_or(0),
        total: total.unwrap_or(0),
        expire: expire.filter(|&secs| secs > 0),
        fetched_at: now_secs,
    })
}

/// A non-negative, finite number, as an integer (fractions truncated,
/// beyond `u64::MAX` saturated).
fn parse_amount(raw: &str) -> Option<u64> {
    let value: f64 = raw.trim().parse().ok()?;
    (value.is_finite() && value >= 0.0).then(|| value as u64)
}

impl SubscriptionUsage {
    /// Bytes used so far: upload + download.
    pub fn used(&self) -> u64 {
        self.upload.saturating_add(self.download)
    }

    /// Share of the allowance used, clamped to `0.0..=1.0`; `None` when the
    /// allowance is unlimited / not reported.
    pub fn fraction_used(&self) -> Option<f32> {
        (self.total > 0).then(|| (self.used() as f64 / self.total as f64).min(1.0) as f32)
    }

    /// Whole days until expiry, truncated toward zero: 0 within the last
    /// day before expiry and the first day after it, negative once a whole
    /// day past. `None` without an expiry.
    pub fn days_left(&self, now_secs: u64) -> Option<i64> {
        self.expire
            .map(|expire| (expire as i64 - now_secs as i64) / DAY_SECS)
    }

    fn is_expired(&self, now_secs: u64) -> bool {
        self.expire.is_some_and(|expire| expire <= now_secs)
    }

    fn is_exhausted(&self) -> bool {
        self.total > 0 && self.used() >= self.total
    }

    pub fn level(&self, now_secs: u64) -> UsageLevel {
        if self.is_exhausted() || self.is_expired(now_secs) {
            UsageLevel::Critical
        } else if self.fraction_used().is_some_and(|f| f >= WARN_FRACTION)
            || self.days_left(now_secs).is_some_and(|d| d <= WARN_DAYS)
        {
            UsageLevel::Warning
        } else {
            UsageLevel::Normal
        }
    }

    /// "12.0 GB / 100.0 GB · expires in 12 days", "3.5 GB used",
    /// "100.0 GB / 100.0 GB · expired 2 days ago".
    pub fn usage_label(&self, now_secs: u64) -> String {
        let traffic = if self.total > 0 {
            format!(
                "{} / {}",
                format_bytes(self.used()),
                format_bytes(self.total)
            )
        } else {
            (s().usage.used)(&format_bytes(self.used()))
        };
        match self.expiry_phrase(now_secs) {
            Some(expiry) => format!("{}{}{}", traffic, s().common.sep, expiry),
            None => traffic,
        }
    }

    /// "expires in 3 days" / "expires today" / "expired 2 days ago".
    fn expiry_phrase(&self, now_secs: u64) -> Option<String> {
        let days = self.days_left(now_secs)?;
        let t = &s().usage;
        Some(if self.is_expired(now_secs) {
            match -days {
                0 => t.expired_today.to_string(),
                n => (t.expired_days_ago)(n),
            }
        } else {
            match days {
                0 => t.expires_today.to_string(),
                n => (t.expires_in_days)(n),
            }
        })
    }

    /// Why the subscription needs attention, for a toast: "92% of traffic
    /// used, expires in 2 days". `None` at `UsageLevel::Normal`.
    pub fn alert_reason(&self, now_secs: u64) -> Option<String> {
        if self.level(now_secs) == UsageLevel::Normal {
            return None;
        }
        let mut reasons = Vec::new();
        if self.is_exhausted() {
            reasons.push(s().usage.used_up.to_string());
        } else if self.fraction_used().is_some_and(|f| f >= WARN_FRACTION) {
            // Integer percent, rounded down: never claims more than was used.
            let percent = u128::from(self.used()) * 100 / u128::from(self.total);
            reasons.push((s().usage.percent_used)(percent));
        }
        // Expired counts too: `days_left` is ≤ 0 then.
        if self.days_left(now_secs).is_some_and(|d| d <= WARN_DAYS) {
            reasons.push(self.expiry_phrase(now_secs)?);
        }
        Some(reasons.join(s().usage.reason_sep))
    }

    /// The toast for a subscription that needs attention, naming the
    /// profile (never its URL, which may carry a token):
    /// `"Work" subscription: 92% of traffic used.` `None` at Normal.
    pub fn alert_message(&self, profile_name: &str, now_secs: u64) -> Option<String> {
        let reason = self.alert_reason(now_secs)?;
        Some((s().usage.alert)(profile_name, &reason))
    }
}

/// `secs` (Unix epoch) as a UTC calendar date, `YYYY-MM-DD`.
pub fn expiry_date_utc(secs: u64) -> String {
    let (y, m, d) = civil_from_days((secs / DAY_SECS as u64) as i64);
    format!("{:04}-{:02}-{:02}", y, m, d)
}

/// Days since 1970-01-01 → (year, month, day) in the proleptic Gregorian
/// calendar (Howard Hinnant's `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1024 * 1024 * 1024;
    const NOW: u64 = 1_700_000_000;

    fn usage(used: u64, total: u64, expire: Option<u64>) -> SubscriptionUsage {
        SubscriptionUsage {
            upload: 0,
            download: used,
            total,
            expire,
            fetched_at: NOW,
        }
    }

    #[test]
    fn parses_the_standard_header() {
        let u = parse_userinfo(
            "upload=1073741824; download=2147483648; total=10737418240; expire=1800000000",
            NOW,
        )
        .unwrap();
        assert_eq!(
            u,
            SubscriptionUsage {
                upload: GB,
                download: 2 * GB,
                total: 10 * GB,
                expire: Some(1_800_000_000),
                fetched_at: NOW,
            }
        );
    }

    #[test]
    fn parse_tolerates_spaces_case_and_empty_fields() {
        let u = parse_userinfo(" Upload = 10 ;DOWNLOAD=20;; Total=100 ; ", NOW).unwrap();
        assert_eq!(
            (u.upload, u.download, u.total, u.expire),
            (10, 20, 100, None)
        );
    }

    #[test]
    fn parse_accepts_scientific_notation_and_floats() {
        let u = parse_userinfo("upload=0; download=1.5e10; total=1.0995116e12", NOW).unwrap();
        assert_eq!(u.download, 15_000_000_000);
        assert_eq!(u.total, 1_099_511_600_000);
        let u = parse_userinfo("download=12.9", NOW).unwrap();
        assert_eq!(u.download, 12);
    }

    #[test]
    fn parse_defaults_missing_keys_to_zero() {
        let u = parse_userinfo("download=5", NOW).unwrap();
        assert_eq!((u.upload, u.download, u.total, u.expire), (0, 5, 0, None));
        let u = parse_userinfo("total=100; expire=1800000000", NOW).unwrap();
        assert_eq!((u.upload, u.download, u.total), (0, 0, 100));
        assert_eq!(u.expire, Some(1_800_000_000));
    }

    #[test]
    fn parse_ignores_garbage_and_negatives() {
        let u = parse_userinfo(
            "upload=-5; download=abc; total=100; expire=; foo=bar; noequals; download=NaN",
            NOW,
        )
        .unwrap();
        assert_eq!((u.upload, u.download, u.total, u.expire), (0, 0, 100, None));
        let u = parse_userinfo("download=inf; total=10", NOW).unwrap();
        assert_eq!(u.download, 0);
    }

    #[test]
    fn parse_without_traffic_fields_is_none() {
        assert_eq!(parse_userinfo("", NOW), None);
        assert_eq!(parse_userinfo("expire=1800000000", NOW), None);
        assert_eq!(parse_userinfo("upload=x; download=-1", NOW), None);
        assert_eq!(parse_userinfo("garbage", NOW), None);
    }

    #[test]
    fn expire_zero_means_no_expiry() {
        let u = parse_userinfo("upload=1; download=2; total=3; expire=0", NOW).unwrap();
        assert_eq!(u.expire, None);
        assert_eq!(u.days_left(NOW), None);
    }

    #[test]
    fn fraction_used_needs_a_total() {
        assert_eq!(usage(5, 0, None).fraction_used(), None);
        assert_eq!(usage(25, 100, None).fraction_used(), Some(0.25));
        assert_eq!(usage(250, 100, None).fraction_used(), Some(1.0));
        let both = SubscriptionUsage {
            upload: u64::MAX,
            download: 1,
            ..usage(0, 10, None)
        };
        assert_eq!(both.used(), u64::MAX);
    }

    #[test]
    fn levels_around_the_traffic_thresholds() {
        assert_eq!(usage(89, 100, None).level(NOW), UsageLevel::Normal);
        assert_eq!(usage(90, 100, None).level(NOW), UsageLevel::Warning);
        assert_eq!(usage(99, 100, None).level(NOW), UsageLevel::Warning);
        assert_eq!(usage(100, 100, None).level(NOW), UsageLevel::Critical);
        assert_eq!(usage(150, 100, None).level(NOW), UsageLevel::Critical);
        // Unlimited never warns on traffic.
        assert_eq!(usage(1000 * GB, 0, None).level(NOW), UsageLevel::Normal);
    }

    #[test]
    fn levels_around_the_expiry_thresholds() {
        let day = DAY_SECS as u64;
        let at = |expire| usage(0, 100, Some(expire)).level(NOW);
        assert_eq!(at(NOW + 4 * day), UsageLevel::Normal);
        assert_eq!(at(NOW + 4 * day - 1), UsageLevel::Warning);
        assert_eq!(at(NOW + 1), UsageLevel::Warning);
        assert_eq!(at(NOW), UsageLevel::Critical);
        assert_eq!(at(NOW - 10 * day), UsageLevel::Critical);
    }

    #[test]
    fn days_left_truncates_toward_zero() {
        let day = DAY_SECS as u64;
        let left = |expire| usage(0, 0, Some(expire)).days_left(NOW).unwrap();
        assert_eq!(left(NOW + 3 * day + 5), 3);
        assert_eq!(left(NOW + day - 1), 0);
        assert_eq!(left(NOW - day + 1), 0);
        assert_eq!(left(NOW - 2 * day - 1), -2);
    }

    #[test]
    fn labels_cover_traffic_and_expiry() {
        let day = DAY_SECS as u64;
        assert_eq!(
            usage(12 * GB, 100 * GB, Some(NOW + 12 * day + 60)).usage_label(NOW),
            "12.0 GB / 100.0 GB · expires in 12 days"
        );
        assert_eq!(usage(GB / 2, 0, None).usage_label(NOW), "512.0 MB used");
        assert_eq!(
            usage(0, 0, Some(NOW + day + 1)).usage_label(NOW),
            "0 B used · expires in 1 day"
        );
        assert_eq!(
            usage(0, 0, Some(NOW + 60)).usage_label(NOW),
            "0 B used · expires today"
        );
        assert_eq!(
            usage(0, 0, Some(NOW - 60)).usage_label(NOW),
            "0 B used · expired today"
        );
        assert_eq!(
            usage(0, 0, Some(NOW - day - 60)).usage_label(NOW),
            "0 B used · expired 1 day ago"
        );
        assert_eq!(
            usage(0, 0, Some(NOW - 5 * day)).usage_label(NOW),
            "0 B used · expired 5 days ago"
        );
    }

    #[test]
    fn alert_reason_names_what_crossed_the_line() {
        let day = DAY_SECS as u64;
        assert_eq!(usage(50, 100, Some(NOW + 30 * day)).alert_reason(NOW), None);
        assert_eq!(
            usage(92, 100, None).alert_reason(NOW).as_deref(),
            Some("92% of traffic used")
        );
        assert_eq!(
            usage(10, 100, Some(NOW + 2 * day + 5))
                .alert_reason(NOW)
                .as_deref(),
            Some("expires in 2 days")
        );
        assert_eq!(
            usage(95, 100, Some(NOW + 60)).alert_reason(NOW).as_deref(),
            Some("95% of traffic used, expires today")
        );
        assert_eq!(
            usage(100, 100, Some(NOW - 3 * day))
                .alert_reason(NOW)
                .as_deref(),
            Some("traffic used up, expired 3 days ago")
        );
        assert_eq!(
            usage(92, 100, None).alert_message("Work", NOW).as_deref(),
            Some("\"Work\" subscription: 92% of traffic used.")
        );
        assert_eq!(usage(1, 100, None).alert_message("Work", NOW), None);
    }

    #[test]
    fn expiry_dates_are_utc_calendar_dates() {
        assert_eq!(expiry_date_utc(0), "1970-01-01");
        assert_eq!(expiry_date_utc(1_700_000_000), "2023-11-14");
        assert_eq!(expiry_date_utc(951_782_400), "2000-02-29");
        assert_eq!(expiry_date_utc(4_102_444_799), "2099-12-31");
    }

    #[test]
    fn serde_round_trip_skips_a_missing_expiry() {
        let with = usage(1, 2, Some(3));
        let json = serde_json::to_string(&with).unwrap();
        assert_eq!(
            serde_json::from_str::<SubscriptionUsage>(&json).unwrap(),
            with
        );
        let without = usage(1, 2, None);
        let json = serde_json::to_string(&without).unwrap();
        assert!(!json.contains("expire"), "{}", json);
        assert_eq!(
            serde_json::from_str::<SubscriptionUsage>(&json).unwrap(),
            without
        );
    }
}
