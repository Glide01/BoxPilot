//! What sing-box's `set_system_proxy` leaves behind on macOS, and how to
//! tell it from a proxy someone else set: the conservative reset rule of
//! ADR 0003 and ADR 0005, shared by the GUI, which runs it as the user after
//! its own sing-box, and the privileged helper, which runs it as root after
//! a sing-box of its own that couldn't undo its proxy itself (ADR 0006
//! rule 6).
//!
//! sing-box (`common/settings/proxy_darwin.go`) sets the web, secure web and
//! SOCKS proxy of a network service with `networksetup`, always on
//! `127.0.0.1` (it substitutes loopback for an unspecified listen address)
//! and the mixed inbound's port, and turns them off again when it stops.
//! After a crash or a SIGKILL they stay on. A reset lists every service and
//! turns each of those proxies off only while it is still enabled on
//! `127.0.0.1`, and on the port sing-box used when the caller knows it, so a
//! proxy the user or another program set is left alone.
//!
//! Pure: the callers run `networksetup` (by its absolute path, never
//! through a shell) and hand its output here.

#![forbid(unsafe_code)]

/// `networksetup`, by its absolute path.
pub const NETWORKSETUP: &str = "/usr/sbin/networksetup";

/// The `networksetup` getter and state setter of each proxy sing-box sets:
/// `networksetup <getter> <service>` reads it, `networksetup <setter>
/// <service> off` turns it off.
pub const MACOS_PROXY_KINDS: [(&str, &str); 3] = [
    ("-getwebproxy", "-setwebproxystate"),
    ("-getsecurewebproxy", "-setsecurewebproxystate"),
    ("-getsocksfirewallproxy", "-setsocksfirewallproxystate"),
];

/// The arguments that list the services.
pub const LIST_SERVICES: &str = "-listallnetworkservices";

/// The service names in `networksetup -listallnetworkservices` output: one
/// per line after the header line, a disabled one prefixed with `*` (its
/// proxy is reset too: it would come back with it when re-enabled).
pub fn parse_network_services(output: &str) -> Vec<&str> {
    output
        .lines()
        .skip(1)
        .map(|line| line.trim_start_matches('*').trim())
        .filter(|name| !name.is_empty())
        .collect()
}

/// Whether a proxy is sing-box's: still enabled on the loopback host
/// sing-box writes, and, if `port` is given, on that port. Takes
/// `networksetup -getwebproxy <service>` output (`Enabled: Yes` /
/// `Server: 127.0.0.1` / `Port: 7890` lines).
pub fn macos_proxy_is_ours(output: &str, port: Option<u16>) -> bool {
    let field = |name: &str| {
        output.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            (key.trim() == name).then(|| value.trim())
        })
    };
    field("Enabled") == Some("Yes")
        && field("Server") == Some("127.0.0.1")
        && port.is_none_or(|port| field("Port") == Some(port.to_string().as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_services_skip_the_header_and_unmark_disabled_ones() {
        let out = "An asterisk (*) denotes that a network service is disabled.\n\
                   Wi-Fi\n\
                   *Thunderbolt Bridge\n\
                   USB 10/100/1000 LAN\n\
                   \n";
        assert_eq!(
            parse_network_services(out),
            vec!["Wi-Fi", "Thunderbolt Bridge", "USB 10/100/1000 LAN"]
        );
        assert!(parse_network_services("").is_empty());
    }

    fn get(enabled: &str, server: &str, port: &str) -> String {
        format!(
            "Enabled: {enabled}\nServer: {server}\nPort: {port}\nAuthenticated Proxy Enabled: 0\n"
        )
    }

    #[test]
    fn a_proxy_is_ours_only_when_enabled_on_loopback() {
        assert!(macos_proxy_is_ours(&get("Yes", "127.0.0.1", "7788"), None));
        assert!(!macos_proxy_is_ours(&get("No", "127.0.0.1", "7788"), None));
        assert!(!macos_proxy_is_ours(
            &get("Yes", "proxy.corp.example", "7788"),
            None
        ));
        assert!(!macos_proxy_is_ours(
            &get("Yes", "127.0.0.10", "7788"),
            None
        ));
        assert!(!macos_proxy_is_ours(&get("No", "", "0"), None));
        assert!(!macos_proxy_is_ours("", None));
    }

    /// The helper knows the port its sing-box used: another program's proxy
    /// on loopback (Surge, ClashX, the user's Proxy-mode sing-box) is left
    /// alone.
    #[test]
    fn with_a_port_only_that_port_is_ours() {
        assert!(macos_proxy_is_ours(
            &get("Yes", "127.0.0.1", "7890"),
            Some(7890)
        ));
        assert!(!macos_proxy_is_ours(
            &get("Yes", "127.0.0.1", "6152"),
            Some(7890)
        ));
        assert!(!macos_proxy_is_ours(
            &get("Yes", "127.0.0.1", "78900"),
            Some(7890)
        ));
        assert!(!macos_proxy_is_ours(
            "Enabled: Yes\nServer: 127.0.0.1\n",
            Some(7890)
        ));
        assert!(!macos_proxy_is_ours(
            &get("No", "127.0.0.1", "7890"),
            Some(7890)
        ));
    }
}
