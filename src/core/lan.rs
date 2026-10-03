//! LAN IPv4 addresses for the "Allow LAN connections" hint: where other
//! devices on the network can reach the proxy once the mixed inbound listens
//! on `0.0.0.0` (`subscription::RuntimeOptions::allow_lan`).
//!
//! The lookup is cheap but not free (`GetAdaptersAddresses` on Windows), and
//! the Settings page asks on every render, so `cached_lan_addresses` keeps
//! the answer for `CACHE_TTL`. Nothing polls: a page shows what it got at
//! its last render.

use crate::core::subscription::TUN_IPV4_ADDRESS;
use std::net::Ipv4Addr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long one interface lookup stays fresh.
pub const CACHE_TTL: Duration = Duration::from_secs(10);

/// At most this many addresses are listed in the hint; more interfaces than
/// that are almost always virtual adapters nobody connects through.
pub const MAX_LISTED: usize = 3;

/// Whether another device could plausibly reach this host at `ip`: not
/// loopback, link-local (169.254/16, an unconfigured adapter), unspecified,
/// broadcast or multicast, and not inside the TUN interface's own /30,
/// which only this host routes.
pub fn usable(ip: Ipv4Addr) -> bool {
    !(ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || in_tun_subnet(ip))
}

/// Whether `ip` lies in the TUN interface's subnet (`TUN_IPV4_ADDRESS`).
fn in_tun_subnet(ip: Ipv4Addr) -> bool {
    let (net, prefix) = parse_cidr(TUN_IPV4_ADDRESS).expect("TUN_IPV4_ADDRESS is a valid CIDR");
    let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
    u32::from(ip) & mask == u32::from(net) & mask
}

/// `a.b.c.d/n` → (address, prefix length).
fn parse_cidr(cidr: &str) -> Option<(Ipv4Addr, u8)> {
    let (addr, prefix) = cidr.split_once('/')?;
    let prefix: u8 = prefix.parse().ok().filter(|p| *p <= 32)?;
    Some((addr.parse().ok()?, prefix))
}

/// The usable addresses in display order: private (RFC 1918) ones first —
/// those are what a home or office LAN hands out — then the rest (CGNAT,
/// public), each group in the order given; duplicates dropped.
pub fn rank(addrs: impl IntoIterator<Item = Ipv4Addr>) -> Vec<Ipv4Addr> {
    let mut seen = Vec::new();
    for ip in addrs {
        if usable(ip) && !seen.contains(&ip) {
            seen.push(ip);
        }
    }
    // Stable: keeps interface order within each group.
    seen.sort_by_key(|ip| !ip.is_private());
    seen
}

/// This host's LAN IPv4 addresses, ranked (`rank`). Interfaces that are
/// down (no carrier: an idle Docker bridge, an unplugged port) or
/// point-to-point (VPN tunnels, PPP) are skipped. Empty when the lookup
/// fails.
pub fn lan_ipv4_addresses() -> Vec<Ipv4Addr> {
    let Ok(interfaces) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };
    rank(
        interfaces
            .into_iter()
            .filter(|iface| iface.is_oper_up() && !iface.is_p2p())
            .filter_map(|iface| match iface.addr {
                if_addrs::IfAddr::V4(v4) => Some(v4.ip),
                if_addrs::IfAddr::V6(_) => None,
            }),
    )
}

type Cache = Mutex<Option<(Instant, Vec<Ipv4Addr>)>>;

static CACHE: Cache = Mutex::new(None);

/// `lan_ipv4_addresses`, looked up at most once per `CACHE_TTL`.
pub fn cached_lan_addresses() -> Vec<Ipv4Addr> {
    cached_with(&CACHE, Instant::now(), lan_ipv4_addresses)
}

/// The cache rule, with the clock and the lookup passed in for tests.
fn cached_with(
    cache: &Cache,
    now: Instant,
    lookup: impl FnOnce() -> Vec<Ipv4Addr>,
) -> Vec<Ipv4Addr> {
    let mut slot = cache.lock().unwrap_or_else(|e| e.into_inner());
    match slot.as_ref() {
        Some((at, addrs)) if now.saturating_duration_since(*at) < CACHE_TTL => addrs.clone(),
        _ => {
            let addrs = lookup();
            *slot = Some((now, addrs.clone()));
            addrs
        }
    }
}

/// `ip:port` for each of the first `MAX_LISTED` addresses, comma-separated;
/// `None` when there are none.
pub fn endpoint_list(addrs: &[Ipv4Addr], port: u16) -> Option<String> {
    (!addrs.is_empty()).then(|| {
        addrs
            .iter()
            .take(MAX_LISTED)
            .map(|ip| format!("{ip}:{port}"))
            .collect::<Vec<_>>()
            .join(", ")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }

    #[test]
    fn usable_keeps_lan_and_routable_addresses() {
        for s in [
            "192.168.1.23",
            "10.0.0.5",
            "172.16.0.1",
            "172.17.0.1",
            "172.18.0.5",
            "100.64.1.2",
            "8.8.8.8",
        ] {
            assert!(usable(ip(s)), "{s} should be usable");
        }
    }

    #[test]
    fn usable_drops_local_only_addresses() {
        for s in [
            "127.0.0.1",
            "127.1.2.3",
            "169.254.10.20",
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.1",
        ] {
            assert!(!usable(ip(s)), "{s} should not be usable");
        }
    }

    /// The TUN's /30 (172.18.0.0–172.18.0.3) is this host's own; the
    /// neighbours just outside it are ordinary private addresses.
    #[test]
    fn usable_drops_the_tun_subnet_only() {
        for s in ["172.18.0.0", "172.18.0.1", "172.18.0.2", "172.18.0.3"] {
            assert!(!usable(ip(s)), "{s} is in the TUN /30");
        }
        assert!(usable(ip("172.18.0.4")));
        assert!(usable(ip("172.17.255.255")));
    }

    #[test]
    fn parse_cidr_reads_address_and_prefix() {
        assert_eq!(parse_cidr("172.18.0.1/30"), Some((ip("172.18.0.1"), 30)));
        assert_eq!(parse_cidr("0.0.0.0/0"), Some((ip("0.0.0.0"), 0)));
        assert_eq!(parse_cidr("1.2.3.4"), None);
        assert_eq!(parse_cidr("1.2.3.4/33"), None);
        assert_eq!(parse_cidr("nope/24"), None);
    }

    #[test]
    fn rank_puts_private_first_dedupes_and_filters() {
        let ranked = rank([
            ip("100.101.102.103"),
            ip("192.168.1.23"),
            ip("127.0.0.1"),
            ip("172.18.0.1"),
            ip("10.0.0.5"),
            ip("192.168.1.23"),
            ip("169.254.1.1"),
            ip("203.0.114.9"),
        ]);
        assert_eq!(
            ranked,
            vec![
                ip("192.168.1.23"),
                ip("10.0.0.5"),
                ip("100.101.102.103"),
                ip("203.0.114.9"),
            ]
        );
    }

    #[test]
    fn cache_reuses_a_fresh_lookup_and_refreshes_a_stale_one() {
        let cache: Cache = Mutex::new(None);
        let t0 = Instant::now();
        let first = cached_with(&cache, t0, || vec![ip("192.168.1.2")]);
        assert_eq!(first, vec![ip("192.168.1.2")]);
        let fresh = cached_with(&cache, t0 + Duration::from_secs(9), || {
            panic!("a fresh entry must not look up again")
        });
        assert_eq!(fresh, first);
        let stale = cached_with(&cache, t0 + CACHE_TTL, || vec![ip("10.0.0.9")]);
        assert_eq!(stale, vec![ip("10.0.0.9")]);
    }

    #[test]
    fn endpoint_list_formats_up_to_max_listed() {
        assert_eq!(endpoint_list(&[], 7788), None);
        assert_eq!(
            endpoint_list(&[ip("192.168.1.23")], 7788).as_deref(),
            Some("192.168.1.23:7788")
        );
        let many = [
            ip("192.168.1.2"),
            ip("10.0.0.3"),
            ip("172.16.0.4"),
            ip("100.64.0.5"),
        ];
        assert_eq!(
            endpoint_list(&many, 1080).as_deref(),
            Some("192.168.1.2:1080, 10.0.0.3:1080, 172.16.0.4:1080")
        );
    }

    /// The real lookup runs and only ever returns usable, unique addresses.
    #[test]
    fn lan_ipv4_addresses_returns_only_usable_unique_addresses() {
        let addrs = lan_ipv4_addresses();
        assert!(addrs.iter().all(|ip| usable(*ip)));
        let mut unique = addrs.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), addrs.len());
    }
}
