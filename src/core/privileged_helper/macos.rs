//! The macOS half of the helper client: the socket launchd created for the
//! helper's daemon, and whether BoxPilot runs as root. What a failed
//! connect means is the pure module's (`socket_connect_error`); the stream
//! is `unix_socket`'s.
//!
//! No connect plan, unlike Windows' `ConnectPlan`: launchd holds the
//! socket whether or not the helper runs, and starts the helper for the
//! first client that connects (socket activation). So a connect reaches
//! launchd at once, or there is no loaded helper to wait for.

use super::client::HelperIo;
use super::macos_status::plist_installed;
use super::unix_socket;
use super::OpenError;
use boxpilot_protocol::endpoint::macos::SOCKET_PATH;
use std::path::Path;
use std::sync::Arc;

/// Whether BoxPilot runs as root: only the user can have made it so
/// (`sudo`), since BoxPilot never asks. That privilege is theirs to lend,
/// so TUN then runs the bundled sing-box directly, as written, with no
/// helper (ADR 0006, "Privilege the user brings is theirs").
pub(super) fn process_is_elevated() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// Connect to the helper's socket.
pub(super) fn open() -> Result<Arc<dyn HelperIo>, OpenError> {
    let io = unix_socket::connect(Path::new(SOCKET_PATH), plist_installed)?;
    Ok(Arc::new(io))
}
