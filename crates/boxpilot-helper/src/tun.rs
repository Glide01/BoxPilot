//! Which network adapters the helper removes after sing-box exits, and when
//! it starts after a crash: stale sing-box wintun adapters. The one
//! judgement in the Windows-only cleanup, so it is tested on every OS:
//! getting it wrong would uninstall a real network card, or another
//! program's tunnel.
//!
//! - **By name**: the same rule as the GUI's
//!   `process::is_sing_tun_friendly_name`, which keeps its own copy until
//!   the GUI stops elevating. The name says "a sing-tun adapter", not "the
//!   helper's": every sing-box-based client names its adapters so.
//! - **Only when not present**: a crashed or stopped sing-box leaves its
//!   adapter behind as a device that is no longer present. A present one
//!   belongs to a program that is running: another sing-box-based VPN
//!   client, or a sing-box the user started as Administrator. Any local
//!   user can start the helper, so removing present adapters would let
//!   anyone cut those tunnels with BoxPilot's SYSTEM rights.

#![forbid(unsafe_code)]

/// Whether an adapter's FriendlyName is one of sing-box's: it begins with
/// `sing-tun`, ignoring case and surrounding spaces.
pub fn is_sing_tun_friendly_name(name: &str) -> bool {
    name.trim().to_ascii_lowercase().starts_with("sing-tun")
}

/// Whether the helper removes an adapter named `name`: one of sing-box's
/// that is no longer present. `present` must be `false` only when the OS
/// said so; when in doubt, the platform says present, and nothing goes.
pub fn is_stale_sing_tun(name: &str, present: bool) -> bool {
    !present && is_sing_tun_friendly_name(name)
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

    /// A present sing-tun adapter is a running program's tunnel.
    #[test]
    fn only_adapters_no_longer_present_are_stale() {
        assert!(is_stale_sing_tun("sing-tun Tunnel", false));
        assert!(!is_stale_sing_tun("sing-tun Tunnel", true));
        assert!(!is_stale_sing_tun("Intel(R) Wi-Fi 6 AX201", false));
        assert!(!is_stale_sing_tun("Intel(R) Wi-Fi 6 AX201", true));
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
