//! The helper's process exit codes. A service reports them to the SCM as its
//! service-specific exit code, where the GUI can read them
//! (`QueryServiceStatus`, which `SERVICE_QUERY_STATUS` allows) to say why
//! TUN is unavailable. Stable: the GUI matches on them.

#![forbid(unsafe_code)]

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
/// The state directory failed verification the same way, or could not be
/// created.
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
