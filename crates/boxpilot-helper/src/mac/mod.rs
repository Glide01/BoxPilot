//! The macOS platform layer (ADR 0006, the macOS phase): the launchd
//! daemon's entry, launchd's socket, the system proxy and DNS cleanup, and
//! the system log. Everything POSIX it does through `posix`, which is
//! tested on Linux too.
//!
//! How the daemon runs:
//!
//! 1. launchd starts it, as root, when a client connects to the socket it
//!    created from the plist (`launchd`); it takes no arguments.
//! 2. It blocks the termination signals for a `sigwait` thread, puts SIGCHLD
//!    back to its default, and makes sure descriptors 0 to 2 are open, so
//!    no pipe of sing-box's lands on them.
//! 3. It adopts the socket (`exit::SOCKET_FAILED` if launchd gave none).
//! 4. It verifies its install before serving anyone (`posix::supervisor`):
//!    itself at `endpoint::macos::HELPER_PATH`, the `bin` and `state`
//!    trees, the manifest and sing-box's hash. A broken install is logged
//!    (and to the system log), the clients already waiting are turned away,
//!    and it exits with the matching `endpoint::exit` code.
//! 5. It leads its own process group, which sing-box joins, so launchd
//!    ends sing-box with the helper's job however the helper dies. sing-box
//!    runs under its sandbox profile, through `/usr/bin/sandbox-exec`
//!    (`sandboxplan`: measuring; not enforced yet).
//! 6. It serves (`posix::server`): each caller's authority from its uid
//!    and the owner record, read again for each connection (`owner`).
//! 7. Idle for a minute, or on SIGTERM from launchd, it stops sing-box
//!    first, and exits 0; launchd starts it again for the next client.

#![warn(clippy::undocumented_unsafe_blocks)]

mod launchd;
mod syslog;
mod system;

use crate::exit;
use crate::helper::{Caller, HelperCore};
use crate::helper_log;
use crate::modes::Trust;
use crate::owner;
use crate::paths::Layout;
use crate::posix::peer;
use crate::posix::server::{self, Identify, ServerConfig, Stopped};
use crate::posix::signals;
use crate::posix::supervisor::{PosixSupervisor, Setup, STOP_GRACE};
use crate::sandboxplan::SANDBOX_EXEC;
use boxpilot_protocol::endpoint::macos::{HELPER_PATH, PLIST_PATH, SOCKET_NAME};
use boxpilot_protocol::Limits;
use std::fs::OpenOptions;
use std::io;
use std::os::fd::IntoRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub const USAGE: &str = "\
usage: boxpilot-helper
           as the io.github.glide01.boxpilot.helper launchd daemon, which launchd
           starts on demand from its plist; it takes no arguments";

/// Say why the helper won't run, wherever it can be read: its own log if
/// it is open, the system log, and stderr.
fn refuse(code: i32, message: &str) -> i32 {
    helper_log!("refusing to run (exit code {code}): {message}");
    syslog::error(&format!("refusing to run (exit code {code}): {message}"));
    eprintln!("boxpilot-helper: refusing to run (exit code {code}): {message}");
    code
}

/// The daemon's entry; returns its exit code.
pub fn main() -> i32 {
    if std::env::args_os().len() > 1 {
        eprintln!("boxpilot-helper: no arguments are taken\n{USAGE}");
        return exit::USAGE;
    }
    // SAFETY: geteuid has no preconditions and cannot fail.
    if unsafe { libc::geteuid() } != 0 {
        return refuse(exit::NOT_ROOT, "it runs as root only, started by launchd");
    }
    // Before any thread: they inherit the mask.
    let termination = match signals::block_termination() {
        Ok(termination) => termination,
        Err(error) => return refuse(exit::INTERNAL, &format!("blocking signals: {error}")),
    };
    if let Err(error) = signals::default_sigchld() {
        return refuse(exit::INTERNAL, &format!("SIGCHLD: {error}"));
    }
    if let Err(error) = open_standard_descriptors() {
        return refuse(exit::INTERNAL, &format!("descriptors 0 to 2: {error}"));
    }
    let listener = match launchd::listener(SOCKET_NAME) {
        Ok(listener) => listener,
        Err(error) => {
            let why = format!("launchd gave no {SOCKET_NAME} socket ({PLIST_PATH}): {error}");
            return refuse(exit::SOCKET_FAILED, &why);
        }
    };
    let refuse_and_turn_away = |code: i32, message: &str| {
        let code = refuse(code, message);
        let turned_away = server::turn_away_waiting(&listener);
        helper_log!("turned away {turned_away} waiting clients");
        code
    };

    match std::env::current_exe() {
        Ok(exe) if exe == Path::new(HELPER_PATH) => {}
        Ok(exe) => {
            return refuse_and_turn_away(
                exit::HELPER_DIR_REFUSED,
                &format!("it runs from {}, not {HELPER_PATH}", exe.display()),
            )
        }
        Err(error) => {
            return refuse_and_turn_away(exit::INTERNAL, &format!("its own path: {error}"))
        }
    }
    let limits = Limits::default();
    let setup = Setup {
        layout: Layout::installed_macos(),
        trust: Trust::root(),
        own_exe: Some(PathBuf::from(HELPER_PATH)),
        cleanup: Arc::new(system::carry_out),
        stop_grace: STOP_GRACE,
        sandbox_exec: PathBuf::from(SANDBOX_EXEC),
    };
    let supervisor = match PosixSupervisor::start(setup, limits.max_log_line) {
        Ok(supervisor) => supervisor,
        Err((code, message)) => return refuse_and_turn_away(code, &message),
    };
    lead_process_group();
    if let Some(path) = server::listening_path(&listener) {
        helper_log!("listening on {}", path.display());
    }

    let core = Arc::new(HelperCore::new(supervisor, limits));
    let stop = Arc::new(AtomicBool::new(false));
    let waiter = termination.spawn_waiter({
        let stop = stop.clone();
        move |signal| {
            helper_log!("signal {signal}: stopping sing-box, then the helper");
            stop.store(true, Ordering::SeqCst);
        }
    });
    if let Err(error) = waiter {
        return refuse_and_turn_away(exit::INTERNAL, &format!("no signal thread: {error}"));
    }
    let identify: Identify = {
        let core = core.clone();
        Arc::new(move |stream: &UnixStream| caller(core.supervisor(), stream))
    };
    let code = match server::run(&listener, core, identify, &stop, ServerConfig::default()) {
        Stopped::Idle | Stopped::Asked => exit::OK,
        Stopped::Failed(error) => {
            helper_log!("accepting clients failed: {error}");
            exit::INTERNAL
        }
    };
    helper_log!("stopped (exit code {code})");
    code
}

/// Who `stream`'s peer is: its uid, from the kernel, and what the owner
/// record makes of it. A uid that can't be read is read-only, and so is
/// everyone while the owner record can't be.
fn caller(supervisor: &PosixSupervisor, stream: &UnixStream) -> Caller {
    let uid = match peer::peer_uid(stream) {
        Ok(uid) => uid,
        Err(error) => {
            helper_log!("a client's uid could not be read: {error}");
            return Caller::read_only();
        }
    };
    let owner = match supervisor.owner() {
        Ok(owner) => Some(owner),
        Err(why) => {
            helper_log!("nobody may start: {why}");
            None
        }
    };
    Caller {
        authority: owner::authority(uid, owner),
        user: Some(uid.to_string()),
    }
}

/// Lead a process group of the helper's own, which sing-box joins: when
/// the job ends, launchd ends what is left of its process group
/// (`AbandonProcessGroup` is false), sing-box with it.
fn lead_process_group() {
    // SAFETY: getpgrp and getpid have no preconditions and cannot fail.
    let (group, pid) = unsafe { (libc::getpgrp(), libc::getpid()) };
    if group == pid {
        helper_log!("leading process group {group}");
        return;
    }
    // SAFETY: setsid takes nothing; it fails harmlessly if the helper
    // already leads a group.
    if unsafe { libc::setsid() } == -1 {
        helper_log!(
            "process group {group} is not the helper's ({pid}), and setsid failed: {}",
            io::Error::last_os_error()
        );
    } else {
        helper_log!("leading process group {pid}, a new session");
    }
}

/// Make sure descriptors 0, 1 and 2 are open (on `/dev/null` if launchd
/// left one closed), so no descriptor the helper opens later takes their
/// place.
fn open_standard_descriptors() -> io::Result<()> {
    for fd in 0..=2 {
        // SAFETY: F_GETFD only reads a descriptor's flags.
        if unsafe { libc::fcntl(fd, libc::F_GETFD) } != -1 {
            continue;
        }
        let null = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")?;
        // The lowest free descriptor, `fd` itself: kept open for good.
        let _ = null.into_raw_fd();
    }
    Ok(())
}
