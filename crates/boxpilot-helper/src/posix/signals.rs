//! The signals that end the helper (launchd's SIGTERM at `bootout` and
//! shutdown; SIGINT and SIGHUP for good measure), taken by one thread with
//! `sigwait` instead of a handler, so nothing runs in signal context: the
//! helper then stops sing-box before it exits (ADR 0006 rule 6).
//!
//! [`block_termination`] must run before the process starts any thread:
//! threads inherit the mask, and a signal delivered to a thread that
//! doesn't block it would take its default action (exit at once, sing-box
//! still running) instead. sing-box itself starts with an empty mask and
//! default dispositions (`child`), and `std::process::Command` resets the
//! mask of what it runs too.

use std::io;
use std::mem::MaybeUninit;
use std::thread::{self, JoinHandle};

/// The signals [`block_termination`] blocks, and [`Termination::wait`]
/// takes.
pub const TERMINATION: [libc::c_int; 3] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP];

/// The blocked set, for the thread that waits on it.
#[derive(Clone, Copy)]
pub struct Termination {
    set: libc::sigset_t,
}

/// A signal set holding exactly `signals`.
pub fn set_of(signals: &[libc::c_int]) -> io::Result<libc::sigset_t> {
    let mut set = MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: sigemptyset initializes the set it is given.
    if unsafe { libc::sigemptyset(set.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: initialized by sigemptyset just above.
    let mut set = unsafe { set.assume_init() };
    for &signal in signals {
        // SAFETY: `set` is an initialized signal set.
        if unsafe { libc::sigaddset(&mut set, signal) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(set)
}

/// Block [`TERMINATION`] in the calling thread, and so in every thread it
/// starts from now on.
pub fn block_termination() -> io::Result<Termination> {
    let set = set_of(&TERMINATION)?;
    // SAFETY: `set` is an initialized signal set; no old mask is asked for.
    let error = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) };
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error));
    }
    Ok(Termination { set })
}

impl Termination {
    /// Wait for one of the signals, and return it.
    pub fn wait(&self) -> io::Result<libc::c_int> {
        let mut signal: libc::c_int = 0;
        loop {
            // SAFETY: `self.set` is an initialized signal set, blocked in
            // every thread; `signal` is valid for a write.
            let error = unsafe { libc::sigwait(&self.set, &mut signal) };
            match error {
                0 => return Ok(signal),
                libc::EINTR => continue,
                error => return Err(io::Error::from_raw_os_error(error)),
            }
        }
    }

    /// On a thread of its own, call `on_signal` with the first termination
    /// signal that arrives.
    pub fn spawn_waiter(
        self,
        on_signal: impl FnOnce(libc::c_int) + Send + 'static,
    ) -> io::Result<JoinHandle<()>> {
        thread::Builder::new()
            .name("signals".into())
            .spawn(move || {
                if let Ok(signal) = self.wait() {
                    on_signal(signal);
                }
            })
    }
}

/// Put SIGCHLD back to its default disposition, whatever the helper
/// inherited: ignored, the kernel would reap sing-box by itself, and the
/// helper could no longer wait for it (`child`).
pub fn default_sigchld() -> io::Result<()> {
    // SAFETY: SIG_DFL for SIGCHLD installs no handler; nothing else in the
    // process has one for it.
    if unsafe { libc::signal(libc::SIGCHLD, libc::SIG_DFL) } == libc::SIG_ERR {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_set_holds_what_it_was_given() {
        let set = set_of(&TERMINATION).unwrap();
        for signal in TERMINATION {
            // SAFETY: `set` is initialized.
            assert_eq!(unsafe { libc::sigismember(&set, signal) }, 1);
        }
        // SAFETY: as above.
        assert_eq!(unsafe { libc::sigismember(&set, libc::SIGKILL) }, 0);
    }
}
