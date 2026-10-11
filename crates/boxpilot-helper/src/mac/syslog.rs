//! The system log, for what the helper must say when its own log can't be
//! written: why it refuses to run (a broken install, whose state directory
//! may be the broken part), and that it stopped. `log show` finds these
//! under the helper's process name, which is its launchd label.
//!
//! Only the reason goes here, never anything a caller sent or an account's
//! activity: those stay in the private log in the state directory.

use std::ffi::CString;

/// Log `message` at error level.
pub fn error(message: &str) {
    let Ok(text) = CString::new(message.replace('\0', " ")) else {
        return;
    };
    // SAFETY: the format is the constant "%s", and its one argument a
    // NUL-terminated string that outlives the call.
    unsafe {
        libc::syslog(
            libc::LOG_ERR | libc::LOG_DAEMON,
            c"%s".as_ptr(),
            text.as_ptr(),
        )
    };
}
