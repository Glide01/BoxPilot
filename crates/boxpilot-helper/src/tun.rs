//! Which network adapters the helper removes after sing-box exits, and when
//! it starts after a crash: sing-box's wintun adapters, by name. The one
//! judgement in the Windows-only cleanup, so it is tested on every OS:
//! getting it wrong would uninstall a real network card.
//!
//! The same rule as the GUI's `process::is_sing_tun_friendly_name`, which
//! keeps its own copy until the GUI stops elevating.

#![forbid(unsafe_code)]

/// Whether an adapter's FriendlyName is one of sing-box's: it begins with
/// `sing-tun`, ignoring case and surrounding spaces.
pub fn is_sing_tun_friendly_name(name: &str) -> bool {
    name.trim().to_ascii_lowercase().starts_with("sing-tun")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sing_boxs_adapters_match() {
        for name in [
            "sing-tun",
            "sing-tun0",
            "Sing-Tun Tunnel",
            "SING-TUN",
            "  sing-tun0  ",
        ] {
            assert!(is_sing_tun_friendly_name(name), "{name:?}");
        }
    }

    #[test]
    fn real_adapters_never_do() {
        for name in [
            "Intel(R) Wi-Fi 6 AX201",
            "Realtek PCIe GbE Family Controller",
            "WireGuard Tunnel",
            "TAP-Windows Adapter V9",
            "my sing-tun clone",
            "sing_tun",
            "",
        ] {
            assert!(!is_sing_tun_friendly_name(name), "{name:?}");
        }
    }
}
