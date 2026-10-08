//! The macOS helper's launchd plist (ADR 0006 rules 5 and 6), as the text
//! `packaging/macos/io.github.glide01.boxpilot.helper.plist` must hold, built
//! from `endpoint::macos` so it is tested on every OS: a test holds the
//! packaged file to [`plist`] byte for byte.
//!
//! Every key, and why:
//!
//! - `Label` and `ProgramArguments`: the installed helper, by its absolute
//!   path, with no arguments.
//! - `Sockets` → `Listener`: launchd creates the socket at
//!   `endpoint::macos::SOCKET_PATH`, in root-owned `/var/run`, owned by
//!   root:wheel with mode 0666 (438: plists have no octal), and starts the
//!   helper when a client connects; the helper adopts it with
//!   `launch_activate_socket`. No `RunAtLoad` and no `KeepAlive`: on demand
//!   only, and the helper exits after a minute idle (rule 6).
//! - `Umask` 077 (63): whatever the helper and its sing-box create, in the
//!   0700 state directory, is private too.
//! - `ExitTimeOut` 30: on `launchctl bootout` or at shutdown launchd sends
//!   SIGTERM and waits this long before SIGKILL, room for the helper to
//!   stop sing-box (its own grace period) and clean up after it.
//! - `AbandonProcessGroup` false, launchd's default, spelled out because
//!   the helper relies on it: sing-box runs in the helper's process group
//!   (the helper leads it), and when the helper's job ends for any reason,
//!   SIGKILL included, launchd signals what is left of that group to end
//!   it. That is how a root sing-box never outlives its helper, as a job
//!   object's `KILL_ON_JOB_CLOSE` does it on Windows.
//! - `AssociatedBundleIdentifiers`: macOS 13 and later show the daemon
//!   under BoxPilot's name in Login Items; older versions ignore it.

#![forbid(unsafe_code)]

use boxpilot_protocol::endpoint::macos::{HELPER_PATH, LABEL, SOCKET_NAME, SOCKET_PATH};

/// BoxPilot.app's bundle identifier (`packaging/macos/Info.plist`).
pub const APP_BUNDLE_ID: &str = "io.github.glide01.boxpilot";

/// The socket's mode, 0666: every connection is authorized by its uid.
pub const SOCKET_MODE: u32 = 0o666;

/// The umask the helper and its sing-box run with.
pub const UMASK: u32 = 0o077;

/// Seconds between launchd's SIGTERM and its SIGKILL.
pub const EXIT_TIMEOUT_SECS: u32 = 30;

/// The plist, as installed into `/Library/LaunchDaemons`.
pub fn plist() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{LABEL}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{HELPER_PATH}</string>
	</array>
	<key>Sockets</key>
	<dict>
		<key>{SOCKET_NAME}</key>
		<dict>
			<key>SockPathName</key>
			<string>{SOCKET_PATH}</string>
			<key>SockPathMode</key>
			<integer>{SOCKET_MODE}</integer>
			<key>SockPathOwner</key>
			<integer>0</integer>
			<key>SockPathGroup</key>
			<integer>0</integer>
		</dict>
	</dict>
	<key>Umask</key>
	<integer>{UMASK}</integer>
	<key>ExitTimeOut</key>
	<integer>{EXIT_TIMEOUT_SECS}</integer>
	<key>AbandonProcessGroup</key>
	<false/>
	<key>AssociatedBundleIdentifiers</key>
	<array>
		<string>{APP_BUNDLE_ID}</string>
	</array>
</dict>
</plist>
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The integers are decimal, as plists require.
    #[test]
    fn modes_are_written_in_decimal() {
        let plist = plist();
        assert!(plist.contains("<integer>438</integer>"), "{plist}");
        assert!(plist.contains("<key>Umask</key>\n\t<integer>63</integer>"));
        assert!(!plist.contains("0666") && !plist.contains("0o"));
    }

    /// On demand only: nothing that would start or keep the helper running
    /// without a client.
    #[test]
    fn the_daemon_runs_on_demand_only() {
        let plist = plist();
        for key in [
            "RunAtLoad",
            "KeepAlive",
            "Disabled",
            "UserName",
            "GroupName",
            "Program<",
        ] {
            assert!(!plist.contains(key), "{key}");
        }
        assert!(plist.contains(&format!("<string>{HELPER_PATH}</string>")));
        assert_eq!(plist.matches("<string>").count(), 4);
    }

    #[test]
    fn the_app_is_the_one_info_plist_names() {
        // LF whatever the checkout made of it (Windows' may be CRLF).
        let info = include_str!("../../../packaging/macos/Info.plist").replace("\r\n", "\n");
        assert!(info.contains(&format!(
            "<key>CFBundleIdentifier</key>\n\t<string>{APP_BUNDLE_ID}</string>"
        )));
    }
}
