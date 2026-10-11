//! Who is on the other end of a Unix socket, as the kernel says (ADR 0006
//! rule 4): the peer's uid, from the credentials the kernel recorded when
//! it connected. Never its PID, which can be reused by the time it is
//! checked.
//!
//! - **macOS**: `getsockopt(SOL_LOCAL, LOCAL_PEERCRED)`, a `struct xucred`
//!   whose `cr_version` must be `XUCRED_VERSION`.
//! - **Linux** (the tests): `getsockopt(SOL_SOCKET, SO_PEERCRED)`.

use std::io;
use std::mem::size_of;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

/// The uid of the process that connected `stream`, as of its `connect`.
#[cfg(target_os = "macos")]
pub fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    // SAFETY: `xucred` is a plain C struct of integers, for which all zero
    // bytes is a valid value.
    let mut cred: libc::xucred = unsafe { std::mem::zeroed() };
    let mut len = size_of::<libc::xucred>() as libc::socklen_t;
    // SAFETY: the descriptor is open while `stream` is borrowed; `cred` and
    // `len` are valid for writes, and `len` says how many bytes `cred` has.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERCRED,
            (&mut cred as *mut libc::xucred).cast::<libc::c_void>(),
            &mut len,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    if len as usize != size_of::<libc::xucred>() || cred.cr_version != libc::XUCRED_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "LOCAL_PEERCRED gave {len} bytes of xucred version {}",
                cred.cr_version
            ),
        ));
    }
    Ok(cred.cr_uid)
}

/// The uid of the process that connected `stream`, as of its `connect`.
#[cfg(target_os = "linux")]
pub fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: u32::MAX,
        gid: u32::MAX,
    };
    let mut len = size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: the descriptor is open while `stream` is borrowed; `cred` and
    // `len` are valid for writes, and `len` says how many bytes `cred` has.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast::<libc::c_void>(),
            &mut len,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    if len as usize != size_of::<libc::ucred>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("SO_PEERCRED gave {len} bytes"),
        ));
    }
    Ok(cred.uid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;
    use std::os::unix::net::UnixListener;

    /// A connection to a listening socket, as the helper's are (macOS
    /// records no credentials for a `socketpair`): both ends are this
    /// process, so its own uid.
    #[test]
    fn the_peer_is_this_process() {
        let temp = TempDir::new("p");
        let path = temp.0.join("s");
        let listener = UnixListener::bind(&path).unwrap();
        let client = UnixStream::connect(&path).unwrap();
        let (server, _) = listener.accept().unwrap();
        // SAFETY: geteuid has no preconditions and cannot fail.
        let me = unsafe { libc::geteuid() };
        assert_eq!(peer_uid(&server).unwrap(), me);
        assert_eq!(peer_uid(&client).unwrap(), me);
    }
}
