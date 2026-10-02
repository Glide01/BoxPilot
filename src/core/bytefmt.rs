//! Human-readable byte-rate and byte-count formatting for the network-speed
//! and traffic displays. No gpui dependency — pure functions, unit-tested
//! off-Windows via the core shim.

/// Format a byte/second rate into a compact label: `0 B/s`, `512 B/s`,
/// `12.3 KB/s`, `1.5 MB/s`, `2.0 GB/s`. Binary (1024) steps with one decimal
/// place above the byte range — the convention Clash dashboards use for the
/// traffic graph.
pub fn format_speed(bytes_per_sec: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;

    let bytes = bytes_per_sec as f64;
    if bytes < KB {
        format!("{} B/s", bytes_per_sec)
    } else if bytes < MB {
        format!("{:.1} KB/s", bytes / KB)
    } else if bytes < GB {
        format!("{:.1} MB/s", bytes / MB)
    } else {
        format!("{:.1} GB/s", bytes / GB)
    }
}

/// Format a byte count (a traffic total) the same way `format_speed` formats
/// a rate, minus the `/s`: `0 B`, `512 B`, `12.3 KB`, `1.5 MB`, `2.0 GB`.
pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    const TB: f64 = 1024.0 * 1024.0 * 1024.0 * 1024.0;

    let value = bytes as f64;
    if value < KB {
        format!("{} B", bytes)
    } else if value < MB {
        format!("{:.1} KB", value / KB)
    } else if value < GB {
        format!("{:.1} MB", value / MB)
    } else if value < TB {
        format!("{:.1} GB", value / GB)
    } else {
        format!("{:.1} TB", value / TB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_counts_mirror_speed_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(3 * 1024 * 1024 / 2), "1.5 MB");
        assert_eq!(format_bytes(5 * 1024 * 1024 * 1024), "5.0 GB");
        assert_eq!(format_bytes(2 * 1024 * 1024 * 1024 * 1024), "2.0 TB");
    }

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
}
