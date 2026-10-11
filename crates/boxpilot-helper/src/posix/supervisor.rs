//! The POSIX [`Supervisor`] (macOS): the helper's trees verified when it
//! starts and again before every spawn, private run directories, and
//! sing-box started from the hashed binary, watched until it exits, and
//! cleaned up after (ADR 0006 rules 2, 3, 6 and 7). `win::supervisor`'s
//! counterpart.
//!
//! - **Verification** (`verify`, `modes`): the helper directory's chain and
//!   the helper's own executable, then the manifest and every file it names,
//!   by hash, when the helper starts and before every spawn; the state
//!   directory's chain, private, before anything is created in it.
//! - **A broken install** stops the helper before it serves anyone, with
//!   the matching `endpoint::exit` code ([`StartError`]).
//! - **The owner record** is read again for each connection ([`owner`]),
//!   so a new install's owner counts at once.
//! - **Each run** gets a fresh 0700 directory under a random name, its
//!   `HOME` and `TMPDIR` inside it, and sing-box's lines go to the starting
//!   connection while it runs. Before the spawn the helper writes the run
//!   marker (`cleanup`). Once sing-box is up (its `api` service, the last
//!   thing it starts, accepts) the helper carries out `cleanup::after_start`
//!   through [`Setup::after_start`]: the DNS flush and the system proxy
//!   sing-box's sandbox denies it. Once sing-box has exited it waits for
//!   that to finish, carries out `cleanup::after_run` through
//!   [`Setup::cleanup`], removes the marker and the run directory, and only
//!   then reports `exited`.
//! - **sing-box runs only under its sandbox** (`sandboxplan`): spawned
//!   through [`Setup::sandbox_exec`], verified like the binaries before
//!   every spawn, which applies the profile and then executes the verified
//!   sing-box in its place, same PID. There is no other way to start it: a
//!   sandbox that can't be applied fails the start, or the run.
//! - **Stopping** is SIGTERM, then SIGKILL once [`Setup::stop_grace`] has
//!   passed, always to sing-box's own PID while it is unreaped (`child`).
//!
//! [`owner`]: PosixSupervisor::owner

use super::child::{self, Launch, Reaper};
use super::verify::{self, Refused, Why};
use crate::cleanup::{self, AfterStart, Cleanup, Marker, MAX_MARKER_BYTES};
use crate::exit;
use crate::helper::{HelperError, Installed, Process, RunEvents, Supervisor};
use crate::helper_log;
use crate::lines::read_lines;
use crate::log::{self, MAX_LOG_BYTES};
use crate::manifest::{self, Manifest, MAX_MANIFEST_BYTES};
use crate::modes::{Role, Trust};
use crate::owner::{self, MAX_OWNER_BYTES};
use crate::paths::{self, is_uid, Layout};
use crate::rundir::RunDir;
use crate::sandboxplan;
use crate::spawnplan;
use boxpilot_policy::Placement;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Between SIGTERM and SIGKILL: room for sing-box to remove its routes.
pub const STOP_GRACE: Duration = Duration::from_secs(5);

/// How long sing-box may take to come up before the helper stops waiting
/// to set things up after it: as long as the GUI waits for a start.
const UP_TIMEOUT: Duration = Duration::from_secs(60);

/// How often the helper looks whether sing-box is up.
const UP_POLL: Duration = Duration::from_millis(100);

/// How long, once sing-box has exited, its output is still read: a process
/// it started may hold its pipes a little longer.
const DRAIN_GRACE: Duration = Duration::from_secs(5);

/// The platform's cleanup after a run, or after a crash (`cleanup`).
pub type CleanupFn = Arc<dyn Fn(&Cleanup) + Send + Sync>;

/// What the platform does once sing-box is up (`cleanup::after_start`).
pub type AfterStartFn = Arc<dyn Fn(&AfterStart) + Send + Sync>;

/// How the helper is set up: the installed daemon's fixed paths, or a
/// test's tree.
pub struct Setup {
    pub layout: Layout,
    pub trust: Trust,
    /// The helper's own executable, verified with its directory chain.
    pub own_exe: Option<PathBuf>,
    /// Carries out an `AfterStart`: on macOS, sets the system proxy and
    /// flushes DNS.
    pub after_start: AfterStartFn,
    /// Carries out a `Cleanup`: on macOS, resets the system proxy and
    /// flushes DNS.
    pub cleanup: CleanupFn,
    /// Between SIGTERM and SIGKILL.
    pub stop_grace: Duration,
    /// What sing-box is started through, which applies its sandbox profile
    /// and then executes it (`sandboxplan`): `/usr/bin/sandbox-exec` for the
    /// installed daemon, a stand-in in the Linux tests. Verified as a file
    /// the helper runs, with its directory chain, before every spawn.
    pub sandbox_exec: PathBuf,
}

/// Why the helper won't start: its exit code and what to log.
pub type StartError = (i32, String);

pub struct PosixSupervisor {
    setup: Setup,
    manifest: Manifest,
    installed: Installed,
    max_log_line: usize,
}

/// The manifest, or a file it names, refused: missing or unreadable doesn't
/// match the manifest; its owner or mode, or a link, is the helper
/// directory's fault (`boxpilot_protocol::endpoint::exit`).
fn installed_file_refused(refused: Refused) -> StartError {
    let code = match refused.why {
        Why::Io(_) => exit::MANIFEST_REFUSED,
        Why::Mode(_) | Why::NotAbsolute | Why::Link(_) => exit::HELPER_DIR_REFUSED,
    };
    (code, refused.to_string())
}

/// At most `limit` bytes of `file`, and one more if there are: so a caller
/// can tell "too large" without reading all of it.
fn read_capped(file: File, limit: usize) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn path_text(path: &Path) -> Result<&str, HelperError> {
    path.to_str()
        .ok_or_else(|| HelperError::new(format!("{} is not valid Unicode", path.display())))
}

impl PosixSupervisor {
    /// Verify everything before serving anyone: the state directory
    /// (created, private, if missing) first, so the log there can say why
    /// anything else is refused; then the helper directory, the helper
    /// itself, the manifest and the binaries it names. Then clean up after
    /// a helper that died with a run, and clear the run directories it left.
    pub fn start(setup: Setup, max_log_line: usize) -> Result<Self, StartError> {
        let layout = &setup.layout;
        prepare_state(layout, &setup.trust).map_err(|error| (exit::STATE_DIR_REFUSED, error.0))?;
        if let Err(error) = log::open_file(&layout.log_file(), MAX_LOG_BYTES) {
            return Err((exit::STATE_DIR_REFUSED, format!("no log file: {error}")));
        }
        let helper_refused = |refused: Refused| (exit::HELPER_DIR_REFUSED, refused.to_string());
        verify::dir_chain(layout.helper_dir(), Role::Dir, &setup.trust).map_err(helper_refused)?;
        if let Some(exe) = &setup.own_exe {
            verify::file_chain(exe, &setup.trust).map_err(helper_refused)?;
        }
        let manifest = read_manifest(layout, &setup.trust)?;
        let installed = Installed {
            sing_box_version: manifest.sing_box.version.clone(),
            sing_box_sha256: manifest.sing_box.sha256.clone(),
        };
        let supervisor = Self {
            setup,
            manifest,
            installed,
            max_log_line,
        };
        supervisor.open_binaries()?;
        let layout = &supervisor.setup.layout;
        supervisor.clean_up_after_a_crash();
        let runs = layout.runs_dir();
        if let Err(error) = fs::remove_dir_all(&runs) {
            if error.kind() != io::ErrorKind::NotFound {
                return Err((
                    exit::STATE_DIR_REFUSED,
                    format!("{}: {error}", runs.display()),
                ));
            }
        }
        supervisor
            .ensure_dir(&runs)
            .map_err(|error| (exit::STATE_DIR_REFUSED, error.0))?;
        helper_log!(
            "helper {} started: sing-box {} ({})",
            env!("CARGO_PKG_VERSION"),
            supervisor.installed.sing_box_version,
            supervisor.installed.sing_box_sha256,
        );
        Ok(supervisor)
    }

    pub fn layout(&self) -> &Layout {
        &self.setup.layout
    }

    /// The owner's uid, from the owner record: in the verified state
    /// directory, root's, and well-formed (`owner`). `Err` says why not,
    /// for the log; then nobody may start.
    pub fn owner(&self) -> Result<u32, String> {
        self.verify_state().map_err(|error| error.0)?;
        let path = self.setup.layout.owner_file();
        let file =
            verify::open_file(&path, &self.setup.trust).map_err(|error| error.to_string())?;
        let bytes = read_capped(file, MAX_OWNER_BYTES)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        owner::parse_owner(&bytes).map_err(|error| format!("{}: {error}", path.display()))
    }

    /// The helper directory verified, and every file the manifest names
    /// opened without following a link, verified and hashed: sing-box first.
    fn open_binaries(&self) -> Result<Vec<File>, StartError> {
        let layout = &self.setup.layout;
        let trust = &self.setup.trust;
        verify::dir_chain(layout.helper_dir(), Role::Dir, trust)
            .map_err(|refused| (exit::HELPER_DIR_REFUSED, refused.to_string()))?;
        let mut held = Vec::new();
        for (name, sha256) in self.manifest.files() {
            let path = layout.helper_file(name);
            let file = verify::open_file(&path, trust).map_err(installed_file_refused)?;
            manifest::verify(name, sha256, &file).map_err(|error| {
                (
                    exit::MANIFEST_REFUSED,
                    format!("{}: {error}", path.display()),
                )
            })?;
            held.push(file);
        }
        Ok(held)
    }

    fn verify_state(&self) -> Result<(), HelperError> {
        verify_state(&self.setup.layout, &self.setup.trust)
    }

    fn create(&self, path: &Path) -> Result<bool, HelperError> {
        create_private(path)
    }

    /// `path`, inside the verified state directory: created private, or
    /// verified if it was already there.
    fn ensure_dir(&self, path: &Path) -> Result<(), HelperError> {
        self.create(path)?;
        verify::dir_only(path, Role::Private, &self.setup.trust)
            .map_err(|refused| HelperError::new(refused.to_string()))
    }

    /// A helper that finds the run marker died with a sing-box running:
    /// undo what that run may have left (`cleanup::after_crash`), then
    /// remove the marker.
    fn clean_up_after_a_crash(&self) {
        let path = self.setup.layout.run_marker();
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return,
            Err(error) => {
                helper_log!("the run marker could not be read: {error}");
                return;
            }
            Ok(_) => {}
        }
        let marker = verify::open_file(&path, &self.setup.trust)
            .ok()
            .and_then(|file| read_capped(file, MAX_MARKER_BYTES).ok())
            .and_then(|bytes| cleanup::parse_marker(&bytes));
        let plan = cleanup::after_crash(marker);
        helper_log!(
            "the last helper stopped while sing-box ran ({marker:?}): cleaning up after it \
             ({plan:?})"
        );
        (self.setup.cleanup)(&plan);
        if let Err(error) = fs::remove_file(&path) {
            helper_log!("the run marker could not be removed: {error}");
        }
    }

    fn write_marker(&self, marker: &Marker) -> Result<(), HelperError> {
        let path = self.setup.layout.run_marker();
        let written = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .and_then(|mut file| {
                file.write_all(cleanup::marker_text(marker).as_bytes())?;
                file.sync_all()
            });
        written.map_err(|error| {
            HelperError::new(format!("the run marker could not be written: {error}"))
        })
    }
}

/// The state directory: created, 0700, if missing (the install creates
/// it), then its whole chain verified, itself as private.
fn prepare_state(layout: &Layout, trust: &Trust) -> Result<(), HelperError> {
    let state = layout.state_dir();
    match fs::symlink_metadata(state) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = state.parent() {
                verify::dir_chain(parent, Role::Dir, trust)
                    .map_err(|refused| HelperError::new(refused.to_string()))?;
            }
            create_private(state)?;
        }
        Err(error) => return Err(HelperError::new(format!("{}: {error}", state.display()))),
    }
    verify_state(layout, trust)
}

fn verify_state(layout: &Layout, trust: &Trust) -> Result<(), HelperError> {
    verify::dir_chain(layout.state_dir(), Role::Private, trust)
        .map(|_| ())
        .map_err(|refused| HelperError::new(refused.to_string()))
}

/// Create `path`, 0700; `Ok(false)` if it existed.
fn create_private(path: &Path) -> Result<bool, HelperError> {
    match DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(HelperError::new(format!("{}: {error}", path.display()))),
    }
}

/// Read the manifest through a verified handle, at most its size limit.
fn read_manifest(layout: &Layout, trust: &Trust) -> Result<Manifest, StartError> {
    let path = layout.manifest_file();
    let file = verify::open_file(&path, trust).map_err(installed_file_refused)?;
    let bytes = read_capped(file, MAX_MANIFEST_BYTES).map_err(|error| {
        (
            exit::MANIFEST_REFUSED,
            format!("{}: {error}", path.display()),
        )
    })?;
    manifest::parse(&bytes).map_err(|error| {
        (
            exit::MANIFEST_REFUSED,
            format!("{}: {error}", path.display()),
        )
    })
}

impl Supervisor for PosixSupervisor {
    fn installed(&self) -> &Installed {
        &self.installed
    }

    fn prepare_run(&self, user: &str) -> Result<(RunDir, Placement), HelperError> {
        let layout = &self.setup.layout;
        self.verify_state()?;
        let user_dir = layout
            .user_dir(user)
            .filter(|_| is_uid(user))
            .ok_or_else(|| HelperError::new("the caller's uid is not one the helper takes"))?;
        self.ensure_dir(&layout.users_dir())?;
        self.ensure_dir(&user_dir)?;
        self.ensure_dir(&user_dir.join(paths::TAILSCALE_DIR))?;
        self.ensure_dir(&layout.runs_dir())?;
        let mut random = [0u8; 16];
        getrandom::fill(&mut random)
            .map_err(|error| HelperError::new(format!("the OS RNG failed: {error}")))?;
        let run_dir = layout.run_dir(&paths::run_name(&random));
        if !self.create(&run_dir)? {
            return Err(HelperError::new("a fresh run directory already existed"));
        }
        let run = RunDir::adopt(run_dir).with_account_dir(user_dir.clone());
        verify::dir_only(run.path(), Role::Private, &self.setup.trust)
            .map_err(|refused| HelperError::new(refused.to_string()))?;
        let placement = paths::placement(run.path(), &user_dir)
            .map_err(|error| HelperError::new(error.to_string()))?;
        Ok((run, placement))
    }

    fn spawn(
        &self,
        run: RunDir,
        events: Arc<dyn RunEvents>,
    ) -> Result<Box<dyn Process>, HelperError> {
        // Checked on every spawn (ADR 0006 rule 3).
        let held = self
            .open_binaries()
            .map_err(|(_, message)| HelperError::new(message))?;
        self.verify_state()?;
        // sing-box runs under its sandbox, or not at all.
        let sandbox_exec = &self.setup.sandbox_exec;
        let held_sandbox_exec =
            verify::file_chain(sandbox_exec, &self.setup.trust).map_err(|refused| {
                HelperError::new(format!(
                    "sing-box's sandbox can't be applied, so it doesn't start: {refused}"
                ))
            })?;
        let io_error = |what: &str, error: io::Error| HelperError::new(format!("{what}: {error}"));
        let tmp = run
            .create_dir("tmp")
            .map_err(|error| io_error("TMPDIR", error))?;
        let home = run
            .create_dir("home")
            .map_err(|error| io_error("HOME", error))?;
        let program = self.setup.layout.helper_file(&self.manifest.sing_box.file);
        let config = run.config_path();
        let sing_box_args = spawnplan::sing_box_args(path_text(run.path())?, path_text(&config)?);
        let account_dir = run
            .account_dir()
            .ok_or_else(|| HelperError::new("the run names no account directory"))?;
        let params =
            sandboxplan::Params::for_run(&self.setup.layout, &program, run.path(), account_dir)
                .map_err(|error| HelperError::new(error.to_string()))?;
        let args = sandboxplan::sandbox_exec_args(&params, &sing_box_args);
        let env = spawnplan::posix_environment(path_text(&home)?, path_text(&tmp)?);
        let system_proxy = run.system_proxy_port();
        self.write_marker(&Marker { system_proxy })?;
        let marker = self.setup.layout.run_marker();
        let child = child::spawn(&Launch {
            program: sandbox_exec,
            args: &args,
            env: &env,
            cwd: run.path(),
        })
        .map_err(|error| {
            let _ = fs::remove_file(&marker);
            io_error(
                &format!(
                    "sing-box did not start under its sandbox ({})",
                    sandbox_exec.display()
                ),
                error,
            )
        })?;
        drop(held);
        drop(held_sandbox_exec);

        let reaper = Arc::new(child.reaper);
        let done = Arc::new(Done::default());
        let stop_requested = Arc::new(AtomicBool::new(false));
        let watch = Watch {
            reaper: reaper.clone(),
            stdout: child.stdout,
            stderr: child.stderr,
            run,
            events,
            done: done.clone(),
            stop_requested: stop_requested.clone(),
            after_start: self.setup.after_start.clone(),
            cleanup: self.setup.cleanup.clone(),
            marker,
            max_log_line: self.max_log_line,
        };
        let pid = reaper.pid();
        let spawned = thread::Builder::new()
            .name("sing-box".into())
            .spawn(move || watch.until_exit());
        if let Err(error) = spawned {
            // Unwatched, it would run for nobody: end it here. The watch,
            // with the run directory, went with the failed spawn.
            reaper.signal(libc::SIGKILL);
            reaper.wait();
            let _ = fs::remove_file(self.setup.layout.run_marker());
            return Err(io_error("no thread to watch sing-box", error));
        }
        helper_log!(
            "sing-box started under its sandbox profile ({}), pid {pid}",
            sandboxplan::STATUS
        );
        Ok(Box::new(PosixProcess {
            reaper,
            done,
            stop_requested,
            grace: self.setup.stop_grace,
        }))
    }
}

/// Set once a run's `exited` has been delivered.
#[derive(Default)]
struct Done {
    done: Mutex<bool>,
    changed: Condvar,
}

impl Done {
    fn set(&self) {
        *self
            .done
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
        self.changed.notify_all();
    }

    fn wait(&self) {
        let mut done = self
            .done
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while !*done {
            done = self
                .changed
                .wait(done)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }
}

/// A running sing-box. Dropping it neither blocks nor stops anything: the
/// watcher thread owns the run.
struct PosixProcess {
    reaper: Arc<Reaper>,
    done: Arc<Done>,
    stop_requested: Arc<AtomicBool>,
    grace: Duration,
}

impl Process for PosixProcess {
    fn stop(&mut self) {
        self.stop_requested.store(true, Ordering::SeqCst);
        if self.reaper.signal(libc::SIGTERM) && !self.reaper.wait_reaped(self.grace) {
            helper_log!(
                "sing-box didn't stop within {}s of SIGTERM: killing it",
                self.grace.as_secs()
            );
            self.reaper.signal(libc::SIGKILL);
        }
        self.done.wait();
    }
}

/// Everything one run's watcher thread owns.
struct Watch {
    reaper: Arc<Reaper>,
    stdout: io::PipeReader,
    stderr: io::PipeReader,
    run: RunDir,
    events: Arc<dyn RunEvents>,
    done: Arc<Done>,
    stop_requested: Arc<AtomicBool>,
    after_start: AfterStartFn,
    cleanup: CleanupFn,
    marker: PathBuf,
    max_log_line: usize,
}

impl Watch {
    /// Drain both pipes while sing-box runs, and set things up once it is
    /// up (in a thread of its own, so the exit is reaped at once); once it
    /// has exited, wait for that thread, clean up (the system proxy and
    /// DNS, the marker, then the run directory) and only then report
    /// `exited`. The set-up is over before the cleanup starts, so a proxy
    /// set late is still reset.
    fn until_exit(self) {
        let Watch {
            reaper,
            stdout,
            stderr,
            run,
            events,
            done,
            stop_requested,
            after_start,
            cleanup,
            marker,
            max_log_line,
        } = self;
        let over = Arc::new(AtomicBool::new(false));
        let set_up = run.api_port().and_then(|api_port| {
            let system_proxy = run.system_proxy_port();
            let over = over.clone();
            let stop_requested = stop_requested.clone();
            let spawned = thread::Builder::new()
                .name("sing-box-up".into())
                .spawn(move || {
                    let gone =
                        || over.load(Ordering::SeqCst) || stop_requested.load(Ordering::SeqCst);
                    if !wait_until_up(api_port, gone) {
                        return;
                    }
                    let plan = cleanup::after_start(system_proxy);
                    helper_log!("sing-box is up; setting up after it ({plan:?})");
                    after_start(&plan);
                });
            match spawned {
                Ok(thread) => Some(thread),
                Err(error) => {
                    helper_log!("no thread to set up after sing-box: {error}");
                    None
                }
            }
        });
        let readers: Vec<JoinHandle<()>> = [stdout, stderr]
            .into_iter()
            .map(|pipe| {
                let events = events.clone();
                thread::spawn(move || {
                    read_lines(pipe, max_log_line, |line, truncated| {
                        events.line(line, truncated)
                    })
                })
            })
            .collect();
        let exit = reaper.wait();
        over.store(true, Ordering::SeqCst);
        if let Some(set_up) = set_up {
            let _ = set_up.join();
        }
        join_for(readers, DRAIN_GRACE);
        let plan = cleanup::after_run(run.system_proxy_port());
        helper_log!("sing-box exited ({exit:?}); cleaning up ({plan:?})");
        cleanup(&plan);
        if let Err(error) = fs::remove_file(&marker) {
            helper_log!("the run marker could not be removed: {error}");
        }
        drop(run);
        events.exited(exit);
        done.set();
    }
}

/// Wait until sing-box's `api` service accepts on `api_port`: sing-box
/// starts it after its inbounds, so its TUN routes are up then. `false` if
/// `gone` says the run ended or is being stopped first, or after
/// [`UP_TIMEOUT`].
fn wait_until_up(api_port: u16, gone: impl Fn() -> bool) -> bool {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, api_port));
    let deadline = Instant::now() + UP_TIMEOUT;
    loop {
        if gone() {
            return false;
        }
        if TcpStream::connect_timeout(&address, UP_POLL).is_ok() {
            return true;
        }
        if Instant::now() >= deadline {
            helper_log!(
                "sing-box's api service didn't accept within {}s: nothing set up after it",
                UP_TIMEOUT.as_secs()
            );
            return false;
        }
        thread::sleep(UP_POLL);
    }
}

/// Join the threads that finish by `timeout`; leave the others be (a
/// process sing-box started may still hold its pipe; it gets nowhere once
/// the run's `exited` is out).
fn join_for(threads: Vec<JoinHandle<()>>, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    for thread in threads {
        while !thread.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        if thread.is_finished() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests;
