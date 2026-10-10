//! Human-readable byte formatting for the network-speed display and the Home
//! runtime stats. No gpui dependency — pure functions, unit-tested off-Windows
//! via the core shim.

/// Format a byte count into a compact label: `0 B`, `512 B`, `12.3 KB`,
/// `1.5 MB`, `2.0 GB`. Binary (1024) steps with one decimal place above the
/// byte range — the convention Clash dashboards use. Totals beyond GB keep
/// counting in GB (`2048.0 GB`); a desktop session never gets near that.
pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;

    let value = bytes as f64;
    if value < KB {
        format!("{} B", bytes)
    } else if value < MB {
        format!("{:.1} KB", value / KB)
    } else if value < GB {
        format!("{:.1} MB", value / MB)
    } else {
        format!("{:.1} GB", value / GB)
    }
}

/// Format a byte/second rate: `format_bytes` plus `/s` (`0 B/s`, `1.5 MB/s`).
pub fn format_speed(bytes_per_sec: u64) -> String {
    format!("{}/s", format_bytes(bytes_per_sec))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_range_is_integer_with_b_suffix() {
        assert_eq!(format_speed(0), "0 B/s");
        assert_eq!(format_speed(512), "512 B/s");
        assert_eq!(format_speed(1023), "1023 B/s");
    }

    #[test]
    fn kilobyte_range_uses_one_decimal() {
        assert_eq!(format_speed(1024), "1.0 KB/s");
        assert_eq!(format_speed(1536), "1.5 KB/s");
        assert_eq!(format_speed(1024 * 1024 - 1), "1024.0 KB/s");
    }

    #[test]
    fn megabyte_range_uses_one_decimal() {
        assert_eq!(format_speed(1024 * 1024), "1.0 MB/s");
        assert_eq!(format_speed(3 * 1024 * 1024 / 2), "1.5 MB/s");
    }

    #[test]
    fn gigabyte_range_uses_one_decimal() {
        assert_eq!(format_speed(1024 * 1024 * 1024), "1.0 GB/s");
        assert_eq!(format_speed(5 * 1024 * 1024 * 1024), "5.0 GB/s");
    }

    #[test]
    fn boundaries_round_up_to_next_unit() {
        // Just below the next unit boundary stays in the lower unit's label.
        assert_eq!(format_speed(1024 + 512), "1.5 KB/s");
        // Exactly at a boundary crosses to the next unit.
        assert_eq!(format_speed(1024 * 1024), "1.0 MB/s");
    }

    #[test]
    fn byte_counts_share_the_rate_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(50 * 1024 * 1024), "50.0 MB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024 / 2), "1.5 GB");
        assert_eq!(format_bytes(2048 * 1024 * 1024 * 1024), "2048.0 GB");
    }
}
