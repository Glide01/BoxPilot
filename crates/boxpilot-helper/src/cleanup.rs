//! What the macOS helper does around a sing-box of its own (ADR 0006 rule
//! 6, and "System proxy"), as pure decisions, so they are tested on every
//! OS. The macOS layer carries them out (`networksetup`, `route`,
//! `dscacheutil`, `killall`, by absolute path, never through a shell).
//!
//! - **Once sing-box is up** ([`after_start`]), the helper flushes DNS, and
//!   sets the system proxy if the start asked for it. sing-box would do
//!   both itself (sing-tun runs `dscacheutil`; the mixed inbound's
//!   `set_system_proxy` runs `networksetup`), but its sandbox denies it
//!   every program but itself (`sandboxplan`): `networksetup` runs a shell
//!   and writes SystemConfiguration's files. So the helper, which isn't
//!   sandboxed, does what sing-box did, the way it did it
//!   (`boxpilot_runconfig::system_proxy`).
//! - **DNS is flushed after every run**, however it ended:
//!   mDNSResponder may have cached answers that came through the tunnel (a
//!   fake-IP profile's, say), which mean nothing once it is down. Flushing
//!   mDNSResponder needs root, so only the helper can (ADR 0005).
//! - **The system proxy is reset after every run that asked for it**: the
//!   helper set it, and nothing else undoes it. The reset uses ADR 0005's
//!   conservative rule, narrowed to the port this run used
//!   (`boxpilot_runconfig::system_proxy`): a proxy the user or another
//!   program set is left alone.
//! - **After a crash of the helper itself**, the same, for the run it died
//!   with. Before each spawn it writes a marker into the state directory
//!   saying whether that run sets the system proxy, and on which port
//!   ([`marker_text`]); it removes it once the run is cleaned up after. A
//!   helper that finds one when it starts died with a run (SIGKILL, a
//!   panic, power loss), and cleans up after it before serving anyone.
//!   Without a marker there is nothing of the helper's to undo, so a helper
//!   start never touches a proxy some other program set on loopback, or the
//!   user's own Proxy-mode sing-box, whoever connects. A marker the helper
//!   can't parse still gets the DNS flush, but no proxy reset: it doesn't
//!   say which proxy would be the helper's.

#![forbid(unsafe_code)]

/// The longest marker the helper reads; a real one is under 30 bytes.
pub const MAX_MARKER_BYTES: usize = 64;

/// The marker's one key.
const SYSTEM_PROXY_KEY: &str = "system_proxy=";
/// Its value when the run sets no proxy.
const NONE: &str = "none";

/// What a running sing-box may leave behind, as the marker records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Marker {
    /// The port of the system proxy the run sets on `127.0.0.1`, if any.
    pub system_proxy: Option<u16>,
}

/// The marker file's content: `system_proxy=<port>` or
/// `system_proxy=none`, and a newline.
pub fn marker_text(marker: &Marker) -> String {
    match marker.system_proxy {
        Some(port) => format!("{SYSTEM_PROXY_KEY}{port}\n"),
        None => format!("{SYSTEM_PROXY_KEY}{NONE}\n"),
    }
}

/// A marker's content as [`marker_text`] writes it, or `None`.
pub fn parse_marker(bytes: &[u8]) -> Option<Marker> {
    if bytes.len() > MAX_MARKER_BYTES {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    let value = text.strip_suffix('\n')?.strip_prefix(SYSTEM_PROXY_KEY)?;
    if value == NONE {
        return Some(Marker { system_proxy: None });
    }
    let plain = !value.is_empty()
        && value.len() <= 5
        && value.bytes().all(|b| b.is_ascii_digit())
        && !value.starts_with('0');
    let port: u16 = value.parse().ok().filter(|_| plain)?;
    Some(Marker {
        system_proxy: Some(port),
    })
}

/// What to do once sing-box is up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AfterStart {
    /// Set the SOCKS, web and secure web proxies of the default route's
    /// network service to `127.0.0.1` and this port.
    pub set_proxy: Option<u16>,
    /// Flush the DNS caches (`dscacheutil -flushcache`, mDNSResponder).
    pub flush_dns: bool,
}

/// What to do once sing-box is up: `system_proxy` is the run's
/// `RunDir::system_proxy_port`.
pub fn after_start(system_proxy: Option<u16>) -> AfterStart {
    AfterStart {
        set_proxy: system_proxy,
        flush_dns: true,
    }
}

/// What to undo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cleanup {
    /// Turn off the proxies still enabled on `127.0.0.1` and this port.
    pub reset_proxy: Option<u16>,
    /// Flush the DNS caches (`dscacheutil -flushcache`, mDNSResponder).
    pub flush_dns: bool,
}

/// What to undo after a run, however it ended: `system_proxy` is the run's
/// `RunDir::system_proxy_port`.
pub fn after_run(system_proxy: Option<u16>) -> Cleanup {
    Cleanup {
        reset_proxy: system_proxy,
        flush_dns: true,
    }
}

/// What to undo when the helper starts and finds a marker: `marker` is
/// its parse (`None`: malformed).
pub fn after_crash(marker: Option<Marker>) -> Cleanup {
    Cleanup {
        reset_proxy: marker.and_then(|marker| marker.system_proxy),
        flush_dns: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_round_trip() {
        for marker in [
            Marker {
                system_proxy: Some(7890),
            },
            Marker {
                system_proxy: Some(1),
            },
            Marker {
                system_proxy: Some(65535),
            },
            Marker { system_proxy: None },
        ] {
            let text = marker_text(&marker);
            assert_eq!(parse_marker(text.as_bytes()), Some(marker), "{text:?}");
        }
        assert_eq!(
            marker_text(&Marker { system_proxy: None }),
            "system_proxy=none\n"
        );
        assert_eq!(
            marker_text(&Marker {
                system_proxy: Some(7890)
            }),
            "system_proxy=7890\n"
        );
    }

    #[test]
    fn anything_else_is_no_marker() {
        for bad in [
            &b""[..],
            b"system_proxy=7890",
            b"system_proxy=\n",
            b"system_proxy=0\n",
            b"system_proxy=07890\n",
            b"system_proxy=65536\n",
            b"system_proxy=+7890\n",
            b"system_proxy=7890 \n",
            b"system_proxy=7890\n\n",
            b"system_proxy=None\n",
            b"proxy=7890\n",
            b" system_proxy=7890\n",
            b"\xff",
        ] {
            assert_eq!(
                parse_marker(bad),
                None,
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
        let mut long = b"system_proxy=none\n".to_vec();
        long.resize(MAX_MARKER_BYTES + 1, b' ');
        assert_eq!(parse_marker(&long), None);
    }

    /// Once sing-box is up: DNS always, the proxy as the start asked.
    #[test]
    fn once_up_the_helper_does_what_sing_box_did() {
        assert_eq!(
            after_start(Some(7890)),
            AfterStart {
                set_proxy: Some(7890),
                flush_dns: true
            }
        );
        assert_eq!(
            after_start(None),
            AfterStart {
                set_proxy: None,
                flush_dns: true
            }
        );
    }

    /// The helper set the proxy, so it resets it after every run that
    /// asked for it, a clean stop included; DNS always.
    #[test]
    fn after_every_run_the_runs_proxy_is_reset() {
        assert_eq!(
            after_run(Some(7890)),
            Cleanup {
                reset_proxy: Some(7890),
                flush_dns: true
            }
        );
        assert_eq!(
            after_run(None),
            Cleanup {
                reset_proxy: None,
                flush_dns: true
            }
        );
    }

    #[test]
    fn a_crash_marker_says_what_to_reset() {
        assert_eq!(
            after_crash(Some(Marker {
                system_proxy: Some(7890)
            })),
            Cleanup {
                reset_proxy: Some(7890),
                flush_dns: true
            }
        );
        assert_eq!(
            after_crash(Some(Marker { system_proxy: None })),
            Cleanup {
                reset_proxy: None,
                flush_dns: true
            }
        );
        // Malformed: DNS only.
        assert_eq!(
            after_crash(None),
            Cleanup {
                reset_proxy: None,
                flush_dns: true
            }
        );
    }
}
