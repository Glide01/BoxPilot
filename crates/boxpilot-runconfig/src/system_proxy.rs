//! The system proxy on macOS as sing-box's `set_system_proxy` sets it, and
//! how to tell it from a proxy someone else set: the conservative reset rule
//! of ADR 0003 and ADR 0005, shared by the GUI, which runs it as the user
//! after its own sing-box, and the privileged helper, which sets the proxy
//! itself for a sing-box of its own and resets it after (ADR 0006 rule 6,
//! "System proxy").
//!
//! sing-box (`common/settings/proxy_darwin.go`) sets the SOCKS, web and
//! secure web proxy ([`MACOS_PROXY_SETTERS`]) of the network service of the
//! default route's interface ([`default_route_interface`],
//! [`service_for_device`]) with `networksetup`, always on `127.0.0.1` (it
//! substitutes loopback for an unspecified listen address) and the mixed
//! inbound's port, with no bypass domains, and turns them off again when it
//! stops. The helper does the same, outside sing-box's sandbox, so that
//! sing-box never needs `networksetup` (which runs a shell). After a crash
//! or a SIGKILL they stay on. A reset lists every service and turns each of
//! those proxies off only while it is still enabled on `127.0.0.1`, and on
//! the port sing-box used when the caller knows it, so a proxy the user or
//! another program set is left alone.
//!
//! Pure: the callers run `networksetup` and `route` (by their absolute
//! paths, never through a shell) and hand their output here.

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

/// The arguments that list the services in order, each with its hardware
/// port and device ([`service_for_device`]).
pub const LIST_SERVICE_ORDER: &str = "-listnetworkserviceorder";

/// The host every proxy sing-box sets points at.
pub const MACOS_PROXY_HOST: &str = "127.0.0.1";

/// The `networksetup` setters of the proxies sing-box sets, in its order:
/// `networksetup <setter> <service> 127.0.0.1 <port>` sets the proxy and
/// turns it on.
pub const MACOS_PROXY_SETTERS: [&str; 3] = [
    "-setsocksfirewallproxy",
    "-setwebproxy",
    "-setsecurewebproxy",
];

/// The interface of the default route, from `route -n get default`'s
/// `interface: en0` line. sing-box's TUN routes the address space in
/// pieces (`1.0.0.0/8` … `128.0.0.0/1`), never as a default route, so this
/// stays the physical interface while it runs. Only a plain interface name
/// (ASCII letters and digits) is taken.
pub fn default_route_interface(output: &str) -> Option<&str> {
    output.lines().find_map(|line| {
        let name = line.trim().strip_prefix("interface:")?.trim();
        let plain =
            !name.is_empty() && name.len() <= 16 && name.bytes().all(|b| b.is_ascii_alphanumeric());
        plain.then_some(name)
    })
}

/// The first enabled network service on `device`, from `networksetup
/// -listnetworkserviceorder`, which lists each service as a `(1) Wi-Fi`
/// line (`(*) Wi-Fi` when disabled) followed by its `(Hardware Port: Wi-Fi,
/// Device: en0)` line. The first in the order is the one macOS uses for
/// that device.
pub fn service_for_device<'a>(output: &'a str, device: &str) -> Option<&'a str> {
    if device.is_empty() {
        return None;
    }
    let lines: Vec<&str> = output.lines().map(str::trim).collect();
    lines.windows(2).find_map(|pair| {
        let (header, port) = (pair[0], pair[1]);
        let details = port.strip_prefix("(Hardware Port:")?.strip_suffix(')')?;
        let (_, found) = details.rsplit_once(", Device:")?;
        if found.trim() != device || !header.starts_with('(') || header.starts_with("(*)") {
            return None;
        }
        let (_, name) = header.split_once(") ")?;
        let name = name.trim();
        (!name.is_empty()).then_some(name)
    })
}

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

    /// `route -n get default` while sing-box's TUN routes are up.
    #[test]
    fn the_default_route_names_its_interface() {
        let out = "   route to: default\n\
                   destination: default\n       mask: default\n    gateway: 192.168.64.1\n\
                   \x20 interface: en0\n      flags: <UP,GATEWAY,DONE,STATIC,PRCLONING,GLOBAL>\n";
        assert_eq!(default_route_interface(out), Some("en0"));
        assert_eq!(default_route_interface("interface: utun4\n"), Some("utun4"));
        for bad in [
            "",
            "route: writing to routing socket: not in table\n",
            "interface: \n",
            "interface: en0; rm\n",
            "interface: ../x\n",
            "interface: aaaaaaaaaaaaaaaaa\n",
        ] {
            assert_eq!(default_route_interface(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_service_on_a_device_is_the_first_enabled_one() {
        let out = "An asterisk (*) denotes that a network service is disabled.\n\
                   (1) Ethernet\n\
                   (Hardware Port: Ethernet, Device: en0)\n\
                   \n\
                   (*) Wi-Fi (old)\n\
                   (Hardware Port: Wi-Fi, Device: en1)\n\
                   \n\
                   (2) Wi-Fi (home)\n\
                   (Hardware Port: Wi-Fi, Device: en1)\n\
                   \n\
                   (3) Thunderbolt Bridge\n\
                   (Hardware Port: Thunderbolt Bridge, Device: bridge0)\n\
                   \n\
                   (4) Corp VPN\n\
                   (Hardware Port: L2TP, Device: )\n";
        assert_eq!(service_for_device(out, "en0"), Some("Ethernet"));
        assert_eq!(service_for_device(out, "en1"), Some("Wi-Fi (home)"));
        assert_eq!(
            service_for_device(out, "bridge0"),
            Some("Thunderbolt Bridge")
        );
        assert_eq!(service_for_device(out, "en"), None);
        assert_eq!(service_for_device(out, "utun4"), None);
        assert_eq!(service_for_device(out, ""), None);
        assert_eq!(service_for_device("", "en0"), None);
    }

    /// sing-box's setters, its host, no bypass list.
    #[test]
    fn the_proxies_are_set_as_sing_box_sets_them() {
        assert_eq!(
            MACOS_PROXY_SETTERS,
            [
                "-setsocksfirewallproxy",
                "-setwebproxy",
                "-setsecurewebproxy"
            ]
        );
        assert_eq!(MACOS_PROXY_HOST, "127.0.0.1");
        // Each one set is one the reset turns off.
        for (setter, (getter, state)) in MACOS_PROXY_SETTERS.iter().zip([
            MACOS_PROXY_KINDS[2],
            MACOS_PROXY_KINDS[0],
            MACOS_PROXY_KINDS[1],
        ]) {
            let kind = setter.trim_start_matches("-set");
            assert_eq!(getter, &format!("-get{kind}"));
            assert_eq!(state, &format!("-set{kind}state"));
        }
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
