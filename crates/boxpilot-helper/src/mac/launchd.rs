//! The listening socket launchd created from the plist's `Sockets` entry
//! (ADR 0006 rule 5), adopted with `launch_activate_socket`.
//!
//! launchd creates it as root, at `endpoint::macos::SOCKET_PATH` in
//! root-owned `/var/run`, before the helper ever runs, so nobody can have
//! squatted the name; and only a process launchd started for this job gets
//! it. A helper not started by launchd from its plist gets an error here,
//! and exits (`exit::SOCKET_FAILED`).

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixListener;

extern "C" {
    /// `<launch.h>`, macOS 10.9 and later (libSystem). On success, `*fds`
    /// is a `malloc`ed array of `*cnt` descriptors, now the caller's, and
    /// the caller frees it. On failure, an `errno` value: `ENOENT` (no
    /// socket of that name), `ESRCH` (not managed by launchd), `EALREADY`
    /// (already activated).
    fn launch_activate_socket(
        name: *const libc::c_char,
        fds: *mut *mut libc::c_int,
        cnt: *mut libc::size_t,
    ) -> libc::c_int;
}

/// The descriptors launchd holds for the plist's socket `name`.
fn activate(name: &str) -> io::Result<Vec<OwnedFd>> {
    let name = CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a NUL in the socket name"))?;
    let mut fds: *mut libc::c_int = std::ptr::null_mut();
    let mut count: libc::size_t = 0;
    // SAFETY: `name` is NUL-terminated and outlives the call; both
    // out-pointers are valid for writes.
    let error = unsafe { launch_activate_socket(name.as_ptr(), &mut fds, &mut count) };
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error));
    }
    if fds.is_null() {
        return Ok(Vec::new());
    }
    // SAFETY: on success `fds` points at `count` descriptors, which are now
    // this process's and nobody else's: each is owned once, here.
    let owned = unsafe { std::slice::from_raw_parts(fds, count) }
        .iter()
        // SAFETY: as above.
        .map(|&fd| unsafe { OwnedFd::from_raw_fd(fd) })
        .collect();
    // SAFETY: `fds` was allocated by launch_activate_socket with malloc,
    // for the caller to free, and is freed once; nothing reads it after.
    unsafe { libc::free(fds.cast()) };
    Ok(owned)
}

/// The one listening Unix socket launchd created for `name`, close-on-exec,
/// so no tool the helper runs inherits it.
pub fn listener(name: &str) -> io::Result<UnixListener> {
    let mut fds = activate(name)?;
    if fds.len() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("launchd gave {} sockets for {name}, not 1", fds.len()),
        ));
    }
    let fd = fds.pop().expect("one descriptor");
    // SAFETY: sets a descriptor flag on a descriptor this process owns.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
        return Err(io::Error::last_os_error());
    }
    let listener = UnixListener::from(fd);
    // A socket that isn't a Unix one fails here: `std` checks the family.
    listener.local_addr()?;
    Ok(listener)
}
