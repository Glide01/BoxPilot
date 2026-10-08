//! What the macOS helper undoes after a sing-box of its own (ADR 0006 rule
//! 6), as pure decisions, so they are tested on every OS. The macOS layer
//! carries them out (`networksetup`, `dscacheutil`, `killall`, by absolute
//! path, never through a shell).
//!
//! - **DNS is flushed after every run**, however it ended:
//!   mDNSResponder may have cached answers that came through the tunnel (a
//!   fake-IP profile's, say), which mean nothing once it is down. Flushing
//!   mDNSResponder needs root, so only the helper can (ADR 0005).
//! - **The system proxy is reset only when sing-box couldn't do it
//!   itself**: the run had `set_system_proxy` on, and sing-box didn't exit
//!   cleanly (code 0) after the helper asked it to stop. A sing-box that
//!   crashed, was killed, or exited on its own may have left the proxy on.
//!   The reset uses ADR 0005's conservative rule, narrowed to the port this
//!   run used (`boxpilot_runconfig::system_proxy`): a proxy the user or
//!   another program set is left alone.
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

use boxpilot_protocol::ExitInfo;

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

/// How a run ended, as the platform saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunEnd {
    /// The run's `RunDir::system_proxy_port`.
    pub system_proxy: Option<u16>,
    /// The helper asked sing-box to stop before it exited.
    pub stop_requested: bool,
    pub exit: ExitInfo,
}

/// What to undo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cleanup {
    /// Turn off the proxies still enabled on `127.0.0.1` and this port.
    pub reset_proxy: Option<u16>,
    /// Flush the DNS caches (`dscacheutil -flushcache`, mDNSResponder).
    pub flush_dns: bool,
}

/// What to undo after a run.
pub fn after_run(end: &RunEnd) -> Cleanup {
    let clean = end.stop_requested && end.exit.code == Some(0) && end.exit.signal.is_none();
    Cleanup {
        reset_proxy: if clean { None } else { end.system_proxy },
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

    fn exit(code: Option<i32>, signal: Option<i32>) -> ExitInfo {
        ExitInfo { code, signal }
    }

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

    /// sing-box stopped by the helper, cleanly: it unset its proxy itself.
    #[test]
    fn a_clean_stop_leaves_the_proxy_to_sing_box() {
        let end = RunEnd {
            system_proxy: Some(7890),
            stop_requested: true,
            exit: exit(Some(0), None),
        };
        assert_eq!(
            after_run(&end),
            Cleanup {
                reset_proxy: None,
                flush_dns: true
            }
        );
    }

    #[test]
    fn anything_else_resets_the_runs_proxy() {
        for (stop_requested, exit) in [
            // SIGKILL after the grace period.
            (true, exit(None, Some(9))),
            // "sing-box did not close!"
            (true, exit(Some(1), None)),
            // Crashed, or exited on its own, even with 0.
            (false, exit(None, Some(11))),
            (false, exit(Some(1), None)),
            (false, exit(Some(0), None)),
            // Unknown.
            (true, exit(None, None)),
        ] {
            let end = RunEnd {
                system_proxy: Some(7890),
                stop_requested,
                exit,
            };
            assert_eq!(after_run(&end).reset_proxy, Some(7890), "{end:?}");
            assert!(after_run(&end).flush_dns);
            let end = RunEnd {
                system_proxy: None,
                ..end
            };
            assert_eq!(after_run(&end).reset_proxy, None, "{end:?}");
            assert!(after_run(&end).flush_dns);
        }
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
