//! Where the GUI finds the helper on Windows, and what the helper's exit
//! codes mean: the parts of the contract between them that aren't messages.
//! They live here, next to the messages, so the GUI depends on this crate
//! alone and never on the helper's.

/// The Windows service's name, which the MSI installs it under and the GUI
/// starts it by.
pub const SERVICE_NAME: &str = "BoxPilotHelper";

/// The service's pipe. Only administrators (and SYSTEM) can create names
/// under `ProtectedPrefix\Administrators`, so nobody else can squat it
/// (ADR 0006 rule 5).
pub const PIPE_NAME: &str = r"\\.\pipe\ProtectedPrefix\Administrators\BoxPilot\helper";

/// The helper's process exit codes. A service reports them to the SCM as
/// its service-specific exit code, where the GUI can read them
/// (`QueryServiceStatus`, which `SERVICE_QUERY_STATUS` allows) to say why
/// TUN is unavailable. Stable: the GUI matches on them.
pub mod exit {
    /// Stopped normally: by the SCM, or after the idle timeout.
    pub const OK: i32 = 0;
    /// The command line was not one the helper takes.
    pub const USAGE: i32 = 2;
    /// Not built for this OS: the helper runs on Windows only for now.
    pub const UNSUPPORTED_OS: i32 = 3;
    /// The helper's own directory (or a file in it) failed verification: a
    /// reparse point, an owner other than SYSTEM / Administrators /
    /// TrustedInstaller, or a non-administrator who may write there.
    pub const HELPER_DIR_REFUSED: i32 = 10;
    /// The state directory (`%ProgramFiles%\BoxPilot\HelperState`, beside
    /// the helper's) failed verification, or could not be created: a reparse
    /// point anywhere in its path, an owner other than SYSTEM /
    /// Administrators / TrustedInstaller, or a non-administrator who may
    /// write there, or even read (it holds each account's cache and
    /// Tailscale node keys, so Program Files' inherited "Users: read" is
    /// refused too). Reinstalling BoxPilot recreates it as the helper wants
    /// it.
    pub const STATE_DIR_REFUSED: i32 = 11;
    /// The install manifest is missing or malformed, or sing-box (or a file
    /// beside it) doesn't match it.
    pub const MANIFEST_REFUSED: i32 = 12;
    /// Another process holds the pipe name.
    pub const PIPE_SQUATTED: i32 = 13;
    /// The pipe could not be created for another reason.
    pub const PIPE_FAILED: i32 = 14;
    /// `--console` was run elevated or as SYSTEM: the test seam trusts the
    /// invoking user's files, which is safe only without privilege.
    pub const CONSOLE_ELEVATED: i32 = 15;
    /// Anything else the helper could not do on its own side.
    pub const INTERNAL: i32 = 20;
}
