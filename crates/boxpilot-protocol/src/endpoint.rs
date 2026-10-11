//! Where the GUI finds the helper on Windows and macOS, and what the
//! helper's exit codes mean: the parts of the contract between them that
//! aren't messages. They live here, next to the messages, so the GUI depends
//! on this crate alone and never on the helper's.

/// The Windows service's name, which the MSI installs it under and the GUI
/// starts it by.
pub const SERVICE_NAME: &str = "BoxPilotHelper";

/// The service's pipe. Only administrators (and SYSTEM) can create names
/// under `ProtectedPrefix\Administrators`, so nobody else can squat it
/// (ADR 0006 rule 5).
pub const PIPE_NAME: &str = r"\\.\pipe\ProtectedPrefix\Administrators\BoxPilot\helper";

/// The macOS helper (ADR 0006 rules 5 and 7): a launchd daemon that launchd
/// starts when a client connects to its socket, installed once, with one
/// administrator prompt, by `packaging/macos/helper-install.sh`. Every path
/// is fixed here, root-owned once installed, and never chosen by a user;
/// the install scripts, the launchd plist and the CI smoke test spell the
/// same values, and tests hold them to these.
pub mod macos {
    /// The daemon's launchd label, and the installed helper's file name.
    pub const LABEL: &str = "io.github.glide01.boxpilot.helper";

    /// The daemon's plist, which `launchctl bootstrap system` loads.
    pub const PLIST_PATH: &str = "/Library/LaunchDaemons/io.github.glide01.boxpilot.helper.plist";

    /// The helper itself. Not under `/usr/local`, which Homebrew gives the
    /// user on an Intel Mac (ADR 0006 rule 3).
    pub const HELPER_PATH: &str =
        "/Library/PrivilegedHelperTools/io.github.glide01.boxpilot.helper";

    /// What the helper keeps beside it: its sing-box and manifest, and its
    /// state, in two sibling directories, so nothing written as state can
    /// land among the binaries.
    pub const SUPPORT_DIR: &str = "/Library/Application Support/BoxPilot Helper";

    /// The helper's sing-box and its install manifest: the helper
    /// directory, in the helper's own terms.
    pub const BIN_DIR: &str = "/Library/Application Support/BoxPilot Helper/bin";

    /// The helper's own copy of sing-box, the one the manifest hashes.
    pub const SING_BOX_PATH: &str = "/Library/Application Support/BoxPilot Helper/bin/sing-box";

    /// The install manifest (`manifest.json`, as on Windows).
    pub const MANIFEST_PATH: &str =
        "/Library/Application Support/BoxPilot Helper/bin/manifest.json";

    /// The state directory, root:wheel 0700: nobody else may even read it
    /// (each account's `cache.db` and Tailscale node keys, the helper's
    /// log).
    pub const STATE_DIR: &str = "/Library/Application Support/BoxPilot Helper/state";

    /// The owner record, root:wheel 0600 in the state directory: the uid of
    /// the account an administrator authorized through the install prompt,
    /// in decimal (ADR 0006 rule 4). The last install wins.
    pub const OWNER_FILE: &str = "/Library/Application Support/BoxPilot Helper/state/owner";

    /// The helper's own log, in the state directory.
    pub const LOG_FILE: &str = "/Library/Application Support/BoxPilot Helper/state/helper.log";

    /// The socket launchd creates for the daemon (the plist's `Sockets`),
    /// mode 0666 in root-owned `/var/run`: every user shares group `staff`,
    /// so every connection is authorized by its uid instead (rule 5).
    pub const SOCKET_PATH: &str = "/var/run/io.github.glide01.boxpilot.helper.sock";

    /// The plist's name for that socket, which the helper asks launchd for
    /// (`launch_activate_socket`).
    pub const SOCKET_NAME: &str = "Listener";

    /// What BoxPilot.app ships for the install, relative to its `Contents`
    /// directory, which is the install script's first argument. The helper,
    /// beside the app and its sing-box.
    pub const BUNDLE_HELPER: &str = "MacOS/boxpilot-helper";

    /// The sing-box the helper installs: the app's own, byte for byte.
    pub const BUNDLE_SING_BOX: &str = "MacOS/sing-box";

    /// The directory holding the manifest, the plist and the two scripts.
    pub const BUNDLE_PAYLOAD_DIR: &str = "Resources/Helper";

    /// The plist's file name, in [`BUNDLE_PAYLOAD_DIR`] and in
    /// `/Library/LaunchDaemons`.
    pub const PLIST_FILE: &str = "io.github.glide01.boxpilot.helper.plist";

    /// The install script, in [`BUNDLE_PAYLOAD_DIR`]: `helper-install.sh
    /// <Contents dir> <owner uid>`, run as root.
    pub const INSTALL_SCRIPT: &str = "helper-install.sh";

    /// The uninstall script, in [`BUNDLE_PAYLOAD_DIR`]:
    /// `helper-uninstall.sh [--keep-state | --remove-state]`, run as root.
    pub const UNINSTALL_SCRIPT: &str = "helper-uninstall.sh";
}

/// The helper's process exit codes. A service reports them to the SCM as
/// its service-specific exit code, where the GUI can read them
/// (`QueryServiceStatus`, which `SERVICE_QUERY_STATUS` allows) to say why
/// TUN is unavailable; launchd records them as the daemon's last exit code
/// (`launchctl print system/<label>`). Stable: the GUI matches on them.
pub mod exit {
    /// Stopped normally: by the SCM or launchd, or after the idle timeout.
    pub const OK: i32 = 0;
    /// The command line was not one the helper takes.
    pub const USAGE: i32 = 2;
    /// Not built for this OS: the helper runs on Windows and macOS only.
    pub const UNSUPPORTED_OS: i32 = 3;
    /// The helper's own directory (or a file in it) failed verification.
    ///
    /// - Windows: a reparse point, an owner other than SYSTEM /
    ///   Administrators / TrustedInstaller, or a non-administrator who may
    ///   write there.
    /// - macOS: a symbolic link in the path, a directory or file not owned
    ///   by root, a directory others may write (or a group other than wheel
    ///   or admin), sing-box or the manifest writable by its group or
    ///   others, or the helper not running from its installed path.
    pub const HELPER_DIR_REFUSED: i32 = 10;
    /// The state directory failed verification, or could not be created.
    ///
    /// - Windows: `%ProgramFiles%\BoxPilot\HelperState`, beside the
    ///   helper's: a reparse point anywhere in its path, an owner other than
    ///   SYSTEM / Administrators / TrustedInstaller, or a non-administrator
    ///   who may write there, or even read (it holds each account's cache
    ///   and Tailscale node keys, so Program Files' inherited "Users: read"
    ///   is refused too). Reinstalling BoxPilot recreates it as the helper
    ///   wants it.
    /// - macOS: [`macos::STATE_DIR`](super::macos::STATE_DIR): a symbolic
    ///   link in its path, a directory not owned by root, or any permission
    ///   for its group or others (it must be 0700). Reinstalling the helper
    ///   recreates it.
    pub const STATE_DIR_REFUSED: i32 = 11;
    /// The install manifest is missing or malformed, or sing-box (or a file
    /// beside it) doesn't match it.
    pub const MANIFEST_REFUSED: i32 = 12;
    /// Windows: another process holds the pipe name.
    pub const PIPE_SQUATTED: i32 = 13;
    /// Windows: the pipe could not be created for another reason.
    pub const PIPE_FAILED: i32 = 14;
    /// Windows: `--console` was run elevated or as SYSTEM: the test seam
    /// trusts the invoking user's files, which is safe only without
    /// privilege.
    pub const CONSOLE_ELEVATED: i32 = 15;
    /// Windows: the helper could not give up the privileges it doesn't
    /// need: when it starts, before serving anyone, it removes from its own
    /// token every privilege but the few it keeps (ADR 0006, "Defense in
    /// depth"), and it refuses to run if one remains or its token can't be
    /// read back.
    pub const PRIVILEGES_REFUSED: i32 = 16;
    /// macOS: launchd gave the helper no listening socket: it was not
    /// started by launchd from its plist, or the plist names no
    /// [`macos::SOCKET_NAME`](super::macos::SOCKET_NAME) socket at
    /// [`macos::SOCKET_PATH`](super::macos::SOCKET_PATH).
    pub const SOCKET_FAILED: i32 = 17;
    /// macOS: the helper runs as root only; it was started as another user.
    pub const NOT_ROOT: i32 = 18;
    /// Anything else the helper could not do on its own side.
    pub const INTERNAL: i32 = 20;
}

#[cfg(test)]
mod tests {
    use super::macos::*;

    /// The paths are written out whole, so they read as what is installed;
    /// they must still agree with each other.
    #[test]
    fn the_macos_paths_agree() {
        assert_eq!(PLIST_PATH, format!("/Library/LaunchDaemons/{PLIST_FILE}"));
        assert_eq!(PLIST_FILE, format!("{LABEL}.plist"));
        assert_eq!(
            HELPER_PATH,
            format!("/Library/PrivilegedHelperTools/{LABEL}")
        );
        assert_eq!(BIN_DIR, format!("{SUPPORT_DIR}/bin"));
        assert_eq!(STATE_DIR, format!("{SUPPORT_DIR}/state"));
        assert_eq!(SING_BOX_PATH, format!("{BIN_DIR}/sing-box"));
        assert_eq!(MANIFEST_PATH, format!("{BIN_DIR}/manifest.json"));
        assert_eq!(OWNER_FILE, format!("{STATE_DIR}/owner"));
        assert_eq!(LOG_FILE, format!("{STATE_DIR}/helper.log"));
        assert_eq!(SOCKET_PATH, format!("/var/run/{LABEL}.sock"));
        assert_eq!(BUNDLE_SING_BOX, "MacOS/sing-box");
        assert!(!SUPPORT_DIR.starts_with("/usr/local"));
        // `struct sockaddr_un`'s path holds 104 bytes on macOS, its NUL
        // included.
        assert!(SOCKET_PATH.len() < 104);
    }
}
