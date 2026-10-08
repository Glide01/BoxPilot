//! The Windows [`Supervisor`]: the helper's trees verified when it starts
//! and again before every spawn, private run directories, and sing-box
//! started from the hashed binary and watched until it exits.

use super::adapters::remove_sing_tun_adapters;
use super::folders;
use super::security::{create_dir, SecurityDescriptor};
use super::spawn::{self, Job, Launch};
use super::sys::wait_handle;
use super::verify::{self, Refused};
use crate::acl::Trusted;
use crate::exit;
use crate::helper::{HelperError, Installed, Process, RunEvents, Supervisor};
use crate::helper_log;
use crate::lines::read_lines;
use crate::log::{self, MAX_LOG_BYTES};
use crate::manifest::{self, Manifest, MAX_MANIFEST_BYTES};
use crate::paths::{self, Layout};
use crate::rundir::RunDir;
use crate::spawnplan;
use boxpilot_policy::Placement;
use boxpilot_protocol::ExitInfo;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::windows::io::OwnedHandle;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

/// How the helper is set up: the service's fixed install, or the console
/// seam's temporary tree.
pub(crate) struct Setup {
    pub(crate) layout: Layout,
    pub(crate) trusted: Trusted,
    /// The SDDL of every directory the helper creates.
    pub(crate) dir_sddl: String,
    /// Remove sing-tun adapters at start and after each run. The console
    /// seam, unprivileged, can't.
    pub(crate) clean_adapters: bool,
    /// The helper's own executable, verified with the files beside it.
    pub(crate) own_exe: Option<PathBuf>,
}

pub(crate) struct WinSupervisor {
    setup: Setup,
    manifest: Manifest,
    installed: Installed,
    system_root: String,
    max_log_line: usize,
}

/// Why the helper won't start: its exit code and what to log.
pub(crate) type StartError = (i32, String);

impl WinSupervisor {
    /// Verify everything before serving anyone: the helper directory and
    /// its files, the manifest and the binaries it names, and the state
    /// directory (created, protected, if missing). Then open the log, clear
    /// the run directories a crash left, and remove stale adapters.
    pub(crate) fn start(setup: Setup, max_log_line: usize) -> Result<Self, StartError> {
        let helper_refused = |refused: Refused| (exit::HELPER_DIR_REFUSED, refused.to_string());
        let layout = &setup.layout;
        verify::dir_chain(layout.helper_dir(), &setup.trusted).map_err(helper_refused)?;
        if let Some(exe) = &setup.own_exe {
            verify::open_file(exe, &setup.trusted).map_err(helper_refused)?;
        }
        let manifest = read_manifest(layout, &setup.trusted)?;
        let installed = Installed {
            sing_box_version: manifest.sing_box.version.clone(),
            sing_box_sha256: manifest.sing_box.sha256.clone(),
        };
        let supervisor = Self {
            system_root: folders::windows_dir()
                .map_err(|error| (exit::INTERNAL, format!("no Windows directory: {error}")))?,
            setup,
            manifest,
            installed,
            max_log_line,
        };
        supervisor
            .open_binaries()
            .map_err(|error| (exit::MANIFEST_REFUSED, error.0))?;
        supervisor
            .prepare_state()
            .map_err(|error| (exit::STATE_DIR_REFUSED, error.0))?;
        let layout = &supervisor.setup.layout;
        if let Err(error) = log::open_file(&layout.log_file(), MAX_LOG_BYTES) {
            return Err((exit::STATE_DIR_REFUSED, format!("no log file: {error}")));
        }
        // Whatever a crash left: the runs are over, and their sing-box died
        // with the helper's job handle.
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
        if supervisor.setup.clean_adapters {
            remove_sing_tun_adapters();
        }
        helper_log!(
            "helper {} started: sing-box {} ({})",
            env!("CARGO_PKG_VERSION"),
            supervisor.installed.sing_box_version,
            supervisor.installed.sing_box_sha256
        );
        Ok(supervisor)
    }

    /// The helper directory verified, and every file the manifest names
    /// opened sharing only reads, verified and hashed: sing-box first. The
    /// handles keep the files as they were hashed while they are open.
    fn open_binaries(&self) -> Result<Vec<File>, HelperError> {
        let layout = &self.setup.layout;
        let trusted = &self.setup.trusted;
        verify::dir_chain(layout.helper_dir(), trusted)
            .map_err(|refused| HelperError::new(refused.to_string()))?;
        let mut held = Vec::new();
        for (name, sha256) in self.manifest.files() {
            let path = layout.helper_file(name);
            let file = verify::open_file(&path, trusted)
                .map_err(|refused| HelperError::new(refused.to_string()))?;
            manifest::verify(name, sha256, &file)
                .map_err(|error| HelperError::new(format!("{}: {error}", path.display())))?;
            held.push(file);
        }
        Ok(held)
    }

    /// The state directory: its parent created if missing, itself created
    /// protected if missing, then the whole chain verified. A folder a user
    /// created first is refused here, by its owner.
    fn prepare_state(&self) -> Result<(), HelperError> {
        let state = self.setup.layout.state_dir();
        if let Some(parent) = state.parent() {
            if !parent.exists() {
                self.create(parent)?;
            }
        }
        self.create(state)?;
        self.verify_state()
    }

    fn verify_state(&self) -> Result<(), HelperError> {
        verify::dir_chain(self.setup.layout.state_dir(), &self.setup.trusted)
            .map_err(|refused| HelperError::new(refused.to_string()))
    }

    /// Create `path` with the protected DACL; `Ok(false)` if it existed.
    fn create(&self, path: &Path) -> Result<bool, HelperError> {
        let descriptor = SecurityDescriptor::from_sddl(&self.setup.dir_sddl)
            .map_err(|error| HelperError::new(format!("the directory DACL: {error}")))?;
        create_dir(path, &descriptor)
            .map_err(|error| HelperError::new(format!("{}: {error}", path.display())))
    }

    /// `path`, inside the verified state directory: created protected, or
    /// verified if it was already there.
    fn ensure_dir(&self, path: &Path) -> Result<(), HelperError> {
        self.create(path)?;
        verify::dir_only(path, &self.setup.trusted)
            .map_err(|refused| HelperError::new(refused.to_string()))
    }

    fn path_text(path: &Path) -> Result<&str, HelperError> {
        path.to_str()
            .ok_or_else(|| HelperError::new(format!("{} is not valid Unicode", path.display())))
    }
}

/// Read the manifest through a verified handle, at most its size limit.
fn read_manifest(layout: &Layout, trusted: &Trusted) -> Result<Manifest, StartError> {
    let path = layout.manifest_file();
    let file = verify::open_file(&path, trusted)
        .map_err(|refused| (exit::HELPER_DIR_REFUSED, refused.to_string()))?;
    let mut bytes = Vec::new();
    file.take(MAX_MANIFEST_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
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

impl Supervisor for WinSupervisor {
    fn installed(&self) -> &Installed {
        &self.installed
    }

    fn prepare_run(&self, user: &str) -> Result<(RunDir, Placement), HelperError> {
        let layout = &self.setup.layout;
        self.verify_state()?;
        let user_dir = layout
            .user_dir(user)
            .ok_or_else(|| HelperError::new("the caller's SID is not one the helper takes"))?;
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
        let run = RunDir::adopt(run_dir);
        verify::dir_only(run.path(), &self.setup.trusted)
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
        // Checked on every spawn (ADR 0006 rule 3), and the files held open
        // from the hash until CreateProcessW has returned.
        let held = self.open_binaries()?;
        self.verify_state()?;
        let io_error = |what: &str, error: io::Error| HelperError::new(format!("{what}: {error}"));
        let temp = run
            .create_dir("tmp")
            .map_err(|error| io_error("TEMP", error))?;
        let profile = run
            .create_dir("home")
            .map_err(|error| io_error("USERPROFILE", error))?;
        let program = self.setup.layout.helper_file(&self.manifest.sing_box.file);
        let config = run.config_path();
        let launch = Launch {
            program: &program,
            args: spawnplan::sing_box_args(Self::path_text(run.path())?, Self::path_text(&config)?),
            cwd: run.path(),
            environment: spawnplan::environment(
                &self.system_root,
                Self::path_text(&temp)?,
                Self::path_text(&profile)?,
            ),
        };
        let child =
            spawn::spawn(&launch).map_err(|error| io_error("sing-box did not start", error))?;
        drop(held);

        let job = Arc::new(child.job);
        let done = Arc::new(Done::default());
        let watch = Watch {
            process: child.process,
            stdout: child.stdout,
            stderr: child.stderr,
            job: job.clone(),
            run,
            events,
            done: done.clone(),
            clean_adapters: self.setup.clean_adapters,
            max_log_line: self.max_log_line,
        };
        thread::Builder::new()
            .name("sing-box".into())
            .spawn(move || watch.until_exit())
            .map_err(|error| {
                job.terminate();
                io_error("no thread to watch sing-box", error)
            })?;
        Ok(Box::new(WinProcess { job, done }))
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
struct WinProcess {
    job: Arc<Job>,
    done: Arc<Done>,
}

impl Process for WinProcess {
    fn stop(&mut self) {
        self.job.terminate();
        self.done.wait();
    }
}

/// Everything one run's watcher thread owns.
struct Watch {
    process: OwnedHandle,
    stdout: File,
    stderr: File,
    job: Arc<Job>,
    run: RunDir,
    events: Arc<dyn RunEvents>,
    done: Arc<Done>,
    clean_adapters: bool,
    max_log_line: usize,
}

impl Watch {
    /// Drain both pipes while sing-box runs; once it has exited, clean up
    /// (adapters, then the run directory) and only then report `exited`.
    fn until_exit(self) {
        let Watch {
            process,
            stdout,
            stderr,
            job,
            run,
            events,
            done,
            clean_adapters,
            max_log_line,
        } = self;
        let readers: Vec<_> = [stdout, stderr]
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
        let exited = wait_handle(&process, None);
        let code = match exited {
            Ok(_) => spawn::exit_code(&process).ok(),
            Err(_) => None,
        };
        // Nothing else can be in the job (its limit is one process), but
        // ending it is free and leaves nothing to hold the pipes open.
        job.terminate();
        if exited.is_err() {
            // The wait failed: make sure it is gone before going on.
            let _ = wait_handle(&process, None);
        }
        for reader in readers {
            let _ = reader.join();
        }
        if clean_adapters {
            remove_sing_tun_adapters();
        }
        drop(run);
        // Windows exit codes are 32-bit unsigned; they travel as the i32
        // with the same bits.
        events.exited(ExitInfo {
            code: code.map(|code| code as i32),
            signal: None,
        });
        done.set();
    }
}
