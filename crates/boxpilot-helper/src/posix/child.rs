//! Starting sing-box as root on macOS (ADR 0006 rules 2 and 3), with
//! nothing it doesn't need, and stopping and reaping it without ever
//! signalling a PID that may have been reused.
//!
//! **Start** ([`spawn`]): `posix_spawn` by absolute path, never
//! `posix_spawnp`, a shell or `system`. The supervisor starts sing-box
//! through `/usr/bin/sandbox-exec`, which applies sing-box's sandbox
//! profile and then executes sing-box in its place (`sandboxplan`): one
//! process, the PID spawned here. What follows is what that process starts
//! with, and what sing-box keeps through the exec:
//!
//! - argv and the environment exactly as given (`spawnplan`: the
//!   environment is built from nothing);
//! - `POSIX_SPAWN_CLOEXEC_DEFAULT` on macOS: sing-box inherits no
//!   descriptor but what the file actions name, stdin from `/dev/null` and
//!   the write ends of its stdout and stderr pipes. (Linux, where only the
//!   tests run, has no such flag; `std` opens everything close-on-exec.)
//! - an empty signal mask and every signal at its default action, whatever
//!   the helper blocks or ignores (it blocks SIGTERM for its own `sigwait`,
//!   and `std` ignores SIGPIPE);
//! - its working directory, the run directory;
//! - **the helper's own process group**, deliberately: launchd ends what is
//!   left of a job's process group when the job dies (the plist's
//!   `AbandonProcessGroup` is false), so a sing-box in its own group would
//!   outlive a helper killed with SIGKILL. The helper leads its group
//!   (`mac`), and never signals the group itself, only sing-box's PID.
//!
//! **Stop and reap** ([`Reaper`]): one thread waits for the exit with
//! `waitid(WNOWAIT)`, which leaves sing-box a zombie, so its PID stays its
//! own; then, holding the lock that [`Reaper::signal`] takes, it reaps. A
//! signal is therefore only ever sent to a PID not yet reaped: never to a
//! process that has since been given sing-box's old PID.

use crate::spawnplan;
use boxpilot_protocol::ExitInfo;
use std::ffi::{CStr, CString};
use std::io::{self, PipeReader};
use std::mem::MaybeUninit;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

#[cfg(target_os = "macos")]
extern "C" {
    /// macOS 10.15 and later (libSystem; the deployment target is 12).
    fn posix_spawn_file_actions_addchdir_np(
        actions: *mut libc::posix_spawn_file_actions_t,
        path: *const libc::c_char,
    ) -> libc::c_int;
}

#[cfg(target_os = "linux")]
use libc::posix_spawn_file_actions_addchdir_np;

/// What to start.
#[derive(Debug)]
pub struct Launch<'a> {
    /// The absolute path of what is started: sandbox-exec, verified, which
    /// executes sing-box from the verified helper directory.
    pub program: &'a Path,
    /// The arguments after the program (`sandboxplan::sandbox_exec_args`,
    /// with `spawnplan::sing_box_args` at its end).
    pub args: &'a [String],
    /// Its whole environment (`spawnplan::posix_environment`).
    pub env: &'a [(String, String)],
    /// Its working directory: the run directory.
    pub cwd: &'a Path,
}

/// A started sing-box.
#[derive(Debug)]
pub struct Child {
    pub reaper: Reaper,
    pub stdout: PipeReader,
    pub stderr: PipeReader,
}

fn c_string(bytes: &[u8]) -> io::Result<CString> {
    CString::new(bytes).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a NUL byte"))
}

/// Check a `posix_spawn*` call's result, which is the error number itself.
fn check(what: &str, error: libc::c_int) -> io::Result<()> {
    if error == 0 {
        return Ok(());
    }
    let error = io::Error::from_raw_os_error(error);
    Err(io::Error::new(error.kind(), format!("{what}: {error}")))
}

/// `posix_spawn_file_actions_t`, initialized, destroyed on drop. Boxed so
/// its address doesn't change once initialized.
struct FileActions(Box<MaybeUninit<libc::posix_spawn_file_actions_t>>);

impl FileActions {
    fn new() -> io::Result<Self> {
        let mut actions = Box::new(MaybeUninit::uninit());
        // SAFETY: initializes the storage it is given, which is valid for
        // writes.
        check("posix_spawn_file_actions_init", unsafe {
            libc::posix_spawn_file_actions_init(actions.as_mut_ptr())
        })?;
        Ok(Self(actions))
    }

    fn as_mut_ptr(&mut self) -> *mut libc::posix_spawn_file_actions_t {
        self.0.as_mut_ptr()
    }

    fn open(&mut self, fd: libc::c_int, path: &CStr, flags: libc::c_int) -> io::Result<()> {
        // SAFETY: the actions are initialized; `path` is NUL-terminated and
        // copied by the call.
        check("posix_spawn_file_actions_addopen", unsafe {
            libc::posix_spawn_file_actions_addopen(self.as_mut_ptr(), fd, path.as_ptr(), flags, 0)
        })
    }

    fn dup2(&mut self, from: libc::c_int, to: libc::c_int) -> io::Result<()> {
        // SAFETY: the actions are initialized.
        check("posix_spawn_file_actions_adddup2", unsafe {
            libc::posix_spawn_file_actions_adddup2(self.as_mut_ptr(), from, to)
        })
    }

    fn chdir(&mut self, path: &CStr) -> io::Result<()> {
        // SAFETY: the actions are initialized; `path` is NUL-terminated and
        // copied by the call.
        check("posix_spawn_file_actions_addchdir_np", unsafe {
            posix_spawn_file_actions_addchdir_np(self.as_mut_ptr(), path.as_ptr())
        })
    }
}

impl Drop for FileActions {
    fn drop(&mut self) {
        // SAFETY: initialized in `new`, destroyed once.
        unsafe { libc::posix_spawn_file_actions_destroy(self.as_mut_ptr()) };
    }
}

/// `posix_spawnattr_t`, initialized, destroyed on drop.
struct Attributes(Box<MaybeUninit<libc::posix_spawnattr_t>>);

impl Attributes {
    fn new() -> io::Result<Self> {
        let mut attributes = Box::new(MaybeUninit::uninit());
        // SAFETY: initializes the storage it is given, which is valid for
        // writes.
        check("posix_spawnattr_init", unsafe {
            libc::posix_spawnattr_init(attributes.as_mut_ptr())
        })?;
        Ok(Self(attributes))
    }

    fn as_mut_ptr(&mut self) -> *mut libc::posix_spawnattr_t {
        self.0.as_mut_ptr()
    }

    fn as_ptr(&self) -> *const libc::posix_spawnattr_t {
        self.0.as_ptr()
    }

    /// An empty signal mask, every signal but SIGKILL and SIGSTOP (which
    /// can't be changed anyway) at its default action, and `flags` on top.
    fn reset_signals(&mut self, flags: libc::c_int) -> io::Result<()> {
        let empty = super::signals::set_of(&[])?;
        let mut all = MaybeUninit::<libc::sigset_t>::uninit();
        // SAFETY: sigfillset initializes the set it is given.
        if unsafe { libc::sigfillset(all.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: initialized just above.
        let mut all = unsafe { all.assume_init() };
        for signal in [libc::SIGKILL, libc::SIGSTOP] {
            // SAFETY: `all` is an initialized set.
            unsafe { libc::sigdelset(&mut all, signal) };
        }
        // SAFETY: the attributes are initialized; the sets are copied.
        check("posix_spawnattr_setsigmask", unsafe {
            libc::posix_spawnattr_setsigmask(self.as_mut_ptr(), &empty)
        })?;
        // SAFETY: as above.
        check("posix_spawnattr_setsigdefault", unsafe {
            libc::posix_spawnattr_setsigdefault(self.as_mut_ptr(), &all)
        })?;
        let flags = flags | libc::POSIX_SPAWN_SETSIGMASK | libc::POSIX_SPAWN_SETSIGDEF;
        // SAFETY: the attributes are initialized; the flags fit a c_short.
        check("posix_spawnattr_setflags", unsafe {
            libc::posix_spawnattr_setflags(self.as_mut_ptr(), flags as libc::c_short)
        })
    }
}

impl Drop for Attributes {
    fn drop(&mut self) {
        // SAFETY: initialized in `new`, destroyed once.
        unsafe { libc::posix_spawnattr_destroy(self.as_mut_ptr()) };
    }
}

/// The flags that close every descriptor sing-box isn't given.
#[cfg(target_os = "macos")]
const CLOEXEC_DEFAULT: libc::c_int = libc::POSIX_SPAWN_CLOEXEC_DEFAULT;
#[cfg(not(target_os = "macos"))]
const CLOEXEC_DEFAULT: libc::c_int = 0;

/// Start `launch.program`, as this process's user, in this process's
/// process group.
pub fn spawn(launch: &Launch<'_>) -> io::Result<Child> {
    let program = c_string(launch.program.as_os_str().as_bytes())?;
    if !launch.program.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "sing-box's path is not absolute",
        ));
    }
    let argv: Vec<CString> = std::iter::once(Ok(program.clone()))
        .chain(launch.args.iter().map(|arg| c_string(arg.as_bytes())))
        .collect::<io::Result<_>>()?;
    let envp: Vec<CString> = spawnplan::posix_environment_strings(launch.env)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?
        .iter()
        .map(|var| c_string(var.as_bytes()))
        .collect::<io::Result<_>>()?;
    let cwd = c_string(launch.cwd.as_os_str().as_bytes())?;
    let pointers = |strings: &[CString]| -> Vec<*mut libc::c_char> {
        strings
            .iter()
            .map(|string| string.as_ptr().cast_mut())
            .chain(std::iter::once(std::ptr::null_mut()))
            .collect()
    };
    let argv_pointers = pointers(&argv);
    let envp_pointers = pointers(&envp);

    let (stdout, stdout_write) = io::pipe()?;
    let (stderr, stderr_write) = io::pipe()?;
    let mut actions = FileActions::new()?;
    actions.open(0, c"/dev/null", libc::O_RDONLY)?;
    actions.dup2(stdout_write.as_raw_fd(), 1)?;
    actions.dup2(stderr_write.as_raw_fd(), 2)?;
    actions.chdir(&cwd)?;
    let mut attributes = Attributes::new()?;
    attributes.reset_signals(CLOEXEC_DEFAULT)?;

    let mut pid: libc::pid_t = 0;
    // SAFETY: `program` and every string `argv_pointers` and
    // `envp_pointers` point at are NUL-terminated and outlive the call; both
    // arrays end with a null pointer, as posix_spawn requires (it doesn't
    // write through them, whatever the type says); the file actions and
    // attributes are initialized, and so are the pipes they name.
    check("posix_spawn", unsafe {
        libc::posix_spawn(
            &mut pid,
            program.as_ptr(),
            actions.as_mut_ptr(),
            attributes.as_ptr(),
            argv_pointers.as_ptr(),
            envp_pointers.as_ptr(),
        )
    })?;
    // Only sing-box holds the write ends now: its exit ends the readers.
    drop(stdout_write);
    drop(stderr_write);
    Ok(Child {
        reaper: Reaper::new(pid),
        stdout,
        stderr,
    })
}

/// A started process, until it is reaped: its PID, and its exit once
/// reaped.
#[derive(Debug)]
pub struct Reaper {
    pid: libc::pid_t,
    exit: Mutex<Option<ExitInfo>>,
    reaped: Condvar,
}

/// How often [`Reaper::wait`] looks again if `waitid` can't wait for it.
const POLL: Duration = Duration::from_millis(50);

impl Reaper {
    fn new(pid: libc::pid_t) -> Self {
        Self {
            pid,
            exit: Mutex::new(None),
            reaped: Condvar::new(),
        }
    }

    pub fn pid(&self) -> u32 {
        self.pid as u32
    }

    fn lock(&self) -> MutexGuard<'_, Option<ExitInfo>> {
        self.exit
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Send `signal` to the process, unless it has been reaped. `false`
    /// when nothing was sent.
    pub fn signal(&self, signal: libc::c_int) -> bool {
        let exit = self.lock();
        if exit.is_some() {
            return false;
        }
        // SAFETY: kill takes a PID and a signal number. The PID is still
        // this process's: it isn't reaped (checked under the lock that
        // reaping takes), so it can't have been reused.
        unsafe { libc::kill(self.pid, signal) == 0 }
    }

    /// Wait until the process has exited, reap it, and return how it
    /// exited. From one thread only: the one that owns the run.
    pub fn wait(&self) -> ExitInfo {
        let waited = self.wait_without_reaping();
        loop {
            let mut exit = self.lock();
            if let Some(exit) = *exit {
                return exit;
            }
            let mut status: libc::c_int = 0;
            let flags = if waited { 0 } else { libc::WNOHANG };
            // SAFETY: waitpid takes the PID of this process's own child,
            // not yet reaped, and a valid pointer for its status. After a
            // successful `waitid` it is a zombie, and this returns at once.
            let reaped = unsafe { libc::waitpid(self.pid, &mut status, flags) };
            let error = io::Error::last_os_error();
            let info = if reaped == self.pid {
                exit_info(status)
            } else if reaped == -1 && error.kind() != io::ErrorKind::Interrupted {
                // ECHILD: someone else reaped it, which nothing in the
                // helper does. It is gone, how is unknown.
                ExitInfo {
                    code: None,
                    signal: None,
                }
            } else {
                drop(exit);
                if !waited {
                    thread::sleep(POLL);
                }
                continue;
            };
            *exit = Some(info);
            self.reaped.notify_all();
            return info;
        }
    }

    /// Wait until the process has been reaped, at most `timeout`. `true`
    /// once it has.
    pub fn wait_reaped(&self, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        let mut exit = self.lock();
        while exit.is_none() {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return false;
            }
            exit = self
                .reaped
                .wait_timeout(exit, left)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
        true
    }

    /// Block until the process has exited, leaving it unreaped. `false` if
    /// `waitid` couldn't: then `wait` polls instead.
    fn wait_without_reaping(&self) -> bool {
        loop {
            // SAFETY: `siginfo_t` is a plain C struct, valid all zero.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            // SAFETY: waitid takes this process's own child's PID and a
            // valid `siginfo_t` to fill; WNOWAIT leaves it unreaped.
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.pid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOWAIT,
                )
            };
            if result == 0 {
                return true;
            }
            if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                return false;
            }
        }
    }
}

/// A `waitpid` status as the protocol's `ExitInfo`.
fn exit_info(status: libc::c_int) -> ExitInfo {
    if libc::WIFEXITED(status) {
        ExitInfo {
            code: Some(libc::WEXITSTATUS(status)),
            signal: None,
        }
    } else if libc::WIFSIGNALED(status) {
        ExitInfo {
            code: None,
            signal: Some(libc::WTERMSIG(status)),
        }
    } else {
        ExitInfo {
            code: None,
            signal: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;
    use std::fs;
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Instant;

    /// A script standing in for sing-box. Tests only: the helper itself
    /// never runs a shell.
    fn script(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("fake-sing-box");
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// `spawn`, again while the script is "busy": another test's
    /// `posix_spawn` may briefly hold it open for writing, before its exec
    /// closes it.
    fn start(launch: &Launch<'_>) -> io::Result<Child> {
        let mut tries = 0;
        loop {
            match spawn(launch) {
                Err(error) if error.raw_os_error() == Some(libc::ETXTBSY) && tries < 20 => {
                    tries += 1;
                    thread::sleep(Duration::from_millis(50));
                }
                result => return result,
            }
        }
    }

    fn read_all(mut pipe: PipeReader) -> String {
        let mut text = String::new();
        pipe.read_to_string(&mut text).unwrap();
        text
    }

    fn run(dir: &Path, program: &Path, args: &[&str]) -> (ExitInfo, String, String) {
        let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
        let env = [
            ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
            ("ONLY".to_owned(), "this".to_owned()),
        ];
        let child = start(&Launch {
            program,
            args: &args,
            env: &env,
            cwd: dir,
        })
        .unwrap();
        let out = thread::spawn(move || read_all(child.stdout));
        let err = thread::spawn(move || read_all(child.stderr));
        let exit = child.reaper.wait();
        (exit, out.join().unwrap(), err.join().unwrap())
    }

    #[test]
    fn it_gets_exactly_its_arguments_environment_and_directory() {
        let temp = TempDir::new("child-env");
        let program = script(
            &temp.0,
            r#"for arg in "$@"; do echo "arg:$arg"; done
env | sort
pwd
echo to-stderr >&2
exit 3"#,
        );
        let (exit, out, err) = run(&temp.0, &program, &["run", "a b", "c'd", "$(x)"]);
        assert_eq!(
            exit,
            ExitInfo {
                code: Some(3),
                signal: None
            }
        );
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(&lines[..4], ["arg:run", "arg:a b", "arg:c'd", "arg:$(x)"]);
        // sh adds PWD, SHLVL and `_` itself; nothing came from the test's
        // own environment.
        let env: Vec<&str> = lines[4..lines.len() - 1]
            .iter()
            .copied()
            .filter(|line| {
                !line.starts_with("PWD=") && !line.starts_with("SHLVL=") && !line.starts_with("_=")
            })
            .collect();
        assert_eq!(env, ["ONLY=this", "PATH=/usr/bin:/bin"]);
        assert_eq!(
            Path::new(lines[lines.len() - 1]).canonicalize().unwrap(),
            temp.0.canonicalize().unwrap()
        );
        assert_eq!(err, "to-stderr\n");
    }

    /// stdin is /dev/null, and the only descriptors open are 0, 1 and 2.
    #[test]
    fn it_inherits_no_descriptor_but_its_three() {
        let temp = TempDir::new("child-fds");
        // An extra descriptor in this process, not close-on-exec.
        let extra = fs::File::open("/dev/null").unwrap();
        // SAFETY: clears FD_CLOEXEC on a descriptor this test owns.
        unsafe { libc::fcntl(extra.as_raw_fd(), libc::F_SETFD, 0) };
        // Each descriptor is looked up in /dev/fd rather than redirected
        // to: the extra one is often 10 or above when other tests hold
        // descriptors, where dash refuses a redirection outright and bash
        // keeps the ones it saves while redirecting.
        let program = script(
            &temp.0,
            r#"cat
for fd in 3 4 5 6 7 8 9 "$1"; do
  if [ -e "/dev/fd/$fd" ]; then echo "open:$fd"; fi
done
exit 0"#,
        );
        let extra_fd = extra.as_raw_fd().to_string();
        let (exit, out, _) = run(&temp.0, &program, &[&extra_fd]);
        assert_eq!(exit.code, Some(0));
        if cfg!(target_os = "macos") {
            assert_eq!(out, "", "stdin was empty, and no extra descriptor was open");
        } else {
            // Linux has no POSIX_SPAWN_CLOEXEC_DEFAULT: only stdin is
            // checked there.
            assert!(!out.contains("open:0"), "{out}");
        }
        drop(extra);
    }

    #[test]
    fn a_relative_program_or_a_nul_is_refused() {
        let temp = TempDir::new("child-bad");
        let launch = |program: &Path, args: &[String]| {
            spawn(&Launch {
                program,
                args,
                env: &[],
                cwd: &temp.0,
            })
            .map(|_| ())
            .unwrap_err()
            .kind()
        };
        assert_eq!(
            launch(Path::new("sing-box"), &[]),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            launch(Path::new("/bin/true"), &["a\0b".to_owned()]),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            launch(Path::new("/nonexistent/sing-box"), &[]),
            io::ErrorKind::NotFound
        );
    }

    /// The helper blocks SIGTERM in itself; sing-box still gets it, and
    /// dies of it.
    #[test]
    fn it_starts_with_default_signals_and_stops_on_sigterm() {
        let temp = TempDir::new("child-term");
        let program = script(&temp.0, "echo up\nexec sleep 30");
        let blocked = super::super::signals::set_of(&[libc::SIGTERM]).unwrap();
        let mut old = super::super::signals::set_of(&[]).unwrap();
        // SAFETY: valid sets; this thread's mask only, restored below.
        unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, &mut old) };
        let child = start(&Launch {
            program: &program,
            args: &[],
            env: &[],
            cwd: &temp.0,
        });
        // SAFETY: restores this thread's mask.
        unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &old, std::ptr::null_mut()) };
        let child = child.unwrap();
        let mut stdout = child.stdout;
        let mut up = [0u8; 3];
        stdout.read_exact(&mut up).unwrap();
        let reaper = Arc::new(child.reaper);
        let began = Instant::now();
        assert!(reaper.signal(libc::SIGTERM));
        let exit = reaper.wait();
        assert_eq!(
            exit,
            ExitInfo {
                code: None,
                signal: Some(libc::SIGTERM)
            }
        );
        assert!(began.elapsed() < Duration::from_secs(10));
        // Reaped: nothing is sent to its PID any more.
        assert!(!reaper.signal(libc::SIGKILL));
        assert_eq!(reaper.wait(), exit);
    }

    /// sing-box stays in the helper's process group (launchd ends the
    /// group with the helper).
    #[test]
    fn it_stays_in_this_process_group() {
        let temp = TempDir::new("child-pgid");
        let program = script(&temp.0, "exec sleep 30");
        let child = start(&Launch {
            program: &program,
            args: &[],
            env: &[],
            cwd: &temp.0,
        })
        .unwrap();
        let pid = child.reaper.pid() as libc::pid_t;
        // SAFETY: getpgid takes a PID; this one is our unreaped child.
        let group = unsafe { libc::getpgid(pid) };
        // SAFETY: getpgrp has no preconditions.
        assert_eq!(group, unsafe { libc::getpgrp() });
        assert!(child.reaper.signal(libc::SIGKILL));
        assert_eq!(child.reaper.wait().signal, Some(libc::SIGKILL));
    }

    /// A stop racing the exit never signals a reaped PID.
    #[test]
    fn signals_after_the_exit_go_nowhere() {
        let temp = TempDir::new("child-race");
        let program = script(&temp.0, "exit 0");
        for _ in 0..20 {
            let child = start(&Launch {
                program: &program,
                args: &[],
                env: &[],
                cwd: &temp.0,
            })
            .unwrap();
            let reaper = Arc::new(child.reaper);
            let signaller = {
                let reaper = reaper.clone();
                thread::spawn(move || {
                    for _ in 0..50 {
                        reaper.signal(libc::SIGTERM);
                    }
                })
            };
            let exit = reaper.wait();
            signaller.join().unwrap();
            assert!(!reaper.signal(libc::SIGTERM));
            assert!(
                exit == ExitInfo {
                    code: Some(0),
                    signal: None
                } || exit
                    == ExitInfo {
                        code: None,
                        signal: Some(libc::SIGTERM)
                    },
                "{exit:?}"
            );
        }
    }
}
