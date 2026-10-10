//! What the tests share: temporary directories, a fake supervisor that
//! stands in for sing-box, and a client on the far end of an in-memory
//! connection.

use crate::conn::{serve, Budget, ConnConfig, Ended};
use crate::helper::{
    Caller, ConnId, HelperCore, HelperError, Installed, Process, RunEvents, Supervisor,
};
use crate::paths::{self, Layout};
use crate::rundir::RunDir;
use crate::transport::memory::{pair, MemoryEnd};
use crate::transport::Transport;
use boxpilot_policy::Placement;
use boxpilot_protocol::{
    decode_to_client, encode_request, Authority, Event, ExitInfo, FrameDecoder, HelloReply, Limits,
    Reply, Request, ToClient,
};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// A fresh directory under the system temp dir, removed on drop.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        Self::under(&std::env::temp_dir(), tag)
    }

    /// One beside the test binary, in the build directory: unlike the
    /// system temp dir (`/tmp`, writable by everyone; under `/var/folders`
    /// on macOS, through a link), a chain of directories that the POSIX
    /// verification can pass.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub fn in_build_dir(tag: &str) -> Self {
        let exe = std::env::current_exe().unwrap();
        Self::under(exe.parent().unwrap(), tag)
    }

    fn under(parent: &Path, tag: &str) -> Self {
        let mut random = [0u8; 8];
        getrandom::fill(&mut random).unwrap();
        let name: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let path = parent.join(format!("boxpilot-helper-{tag}-{name}"));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The names in `dir`, sorted; empty if it doesn't exist.
pub fn names(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// What the fake sing-box does once spawned.
#[derive(Debug, Clone, Default)]
pub struct Script {
    /// How many log lines it prints, at once.
    pub lines: usize,
    /// Exit with this code after the lines; `None`: run until stopped.
    pub exit_code: Option<i32>,
    /// Fail the spawn.
    pub fail_spawn: bool,
    /// How long it takes to exit once asked to stop.
    pub stop_delay: Duration,
}

/// What one spawn was given.
#[derive(Debug, Clone)]
pub struct Spawned {
    pub run_dir: PathBuf,
    pub config: Value,
    /// Every other file in the run directory, by name.
    pub files: BTreeMap<String, Vec<u8>>,
}

/// A supervisor whose "sing-box" is a thread: it prints the script's lines,
/// listens on the config's `api` port as sing-box would, and exits when
/// stopped or when the script says.
pub struct FakeSupervisor {
    /// Holds the fake's tree, removed on drop.
    _root: TempDir,
    pub layout: Layout,
    installed: Installed,
    pub script: Mutex<Script>,
    pub spawned: Mutex<Vec<Spawned>>,
    pub stops: Arc<AtomicUsize>,
}

impl FakeSupervisor {
    pub fn new() -> Self {
        let root = TempDir::new("fake");
        let layout = Layout::new(root.0.join("helper"), root.0.join("state"));
        fs::create_dir_all(layout.runs_dir()).unwrap();
        fs::create_dir_all(layout.users_dir()).unwrap();
        Self {
            _root: root,
            layout,
            installed: Installed {
                sing_box_version: "1.14.2".into(),
                sing_box_sha256: "ab".repeat(32),
            },
            script: Mutex::new(Script::default()),
            spawned: Mutex::new(Vec::new()),
            stops: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn set_script(&self, script: Script) {
        *self.script.lock().unwrap() = script;
    }

    pub fn spawned(&self) -> Vec<Spawned> {
        self.spawned.lock().unwrap().clone()
    }
}

impl Supervisor for FakeSupervisor {
    fn installed(&self) -> &Installed {
        &self.installed
    }

    fn prepare_run(&self, user: &str) -> Result<(RunDir, Placement), HelperError> {
        let user_dir = self
            .layout
            .user_dir(user)
            .ok_or_else(|| HelperError::new("not a SID"))?;
        fs::create_dir_all(&user_dir).unwrap();
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).unwrap();
        let run_dir = self.layout.run_dir(&paths::run_name(&random));
        fs::create_dir(&run_dir).unwrap();
        let placement = paths::placement(&run_dir, &user_dir).unwrap();
        Ok((RunDir::adopt(run_dir), placement))
    }

    fn spawn(
        &self,
        run: RunDir,
        events: Arc<dyn RunEvents>,
    ) -> Result<Box<dyn Process>, HelperError> {
        let script = self.script.lock().unwrap().clone();
        if script.fail_spawn {
            return Err(HelperError::new("the fake refuses to spawn"));
        }
        let config: Value =
            serde_json::from_str(&fs::read_to_string(run.config_path()).unwrap()).unwrap();
        let files = names(run.path())
            .into_iter()
            .filter(|name| name != "config.json")
            .map(|name| {
                let content = fs::read(run.path().join(&name)).unwrap();
                (name, content)
            })
            .collect();
        self.spawned.lock().unwrap().push(Spawned {
            run_dir: run.path().to_owned(),
            config: config.clone(),
            files,
        });
        // Listen where the `api` service would: the port is free, and the
        // GUI could reach it.
        let api = config["services"]
            .as_array()
            .unwrap()
            .iter()
            .find(|service| service["tag"] == "boxpilot-api")
            .unwrap();
        let port = api["listen_port"].as_u64().unwrap() as u16;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
            .map_err(|error| HelperError::new(format!("api port taken: {error}")))?;

        let control = Arc::new(Control::default());
        let thread_control = control.clone();
        thread::spawn(move || {
            for n in 0..script.lines {
                events.line(format!("INFO[0000] line {n}"), false);
            }
            let code = match script.exit_code {
                Some(code) => code,
                None => {
                    thread_control.wait_for(|state| state.stop_requested);
                    thread::sleep(script.stop_delay);
                    1
                }
            };
            drop(listener);
            drop(run);
            events.exited(ExitInfo {
                code: Some(code),
                signal: None,
            });
            thread_control.set(|state| state.done = true);
        });
        Ok(Box::new(FakeProcess {
            control,
            stops: self.stops.clone(),
        }))
    }
}

#[derive(Default)]
struct ControlState {
    stop_requested: bool,
    done: bool,
}

#[derive(Default)]
struct Control {
    state: Mutex<ControlState>,
    changed: Condvar,
}

impl Control {
    fn set(&self, change: impl FnOnce(&mut ControlState)) {
        change(&mut self.state.lock().unwrap());
        self.changed.notify_all();
    }

    fn wait_for(&self, ready: impl Fn(&ControlState) -> bool) {
        let mut state = self.state.lock().unwrap();
        while !ready(&state) {
            state = self.changed.wait(state).unwrap();
        }
    }
}

struct FakeProcess {
    control: Arc<Control>,
    stops: Arc<AtomicUsize>,
}

impl Process for FakeProcess {
    fn stop(&mut self) {
        self.stops.fetch_add(1, Ordering::SeqCst);
        self.control.set(|state| state.stop_requested = true);
        self.control.wait_for(|state| state.done);
    }
}

pub const ADMIN_SID: &str = "S-1-5-21-1-2-3-1001";
pub const OTHER_ADMIN_SID: &str = "S-1-5-21-1-2-3-1003";
pub const USER_SID: &str = "S-1-5-21-1-2-3-1002";

pub fn admin() -> Caller {
    Caller {
        authority: Authority::MayStart,
        user: Some(ADMIN_SID.into()),
    }
}

pub fn other_admin() -> Caller {
    Caller {
        authority: Authority::MayStart,
        user: Some(OTHER_ADMIN_SID.into()),
    }
}

pub fn user() -> Caller {
    Caller {
        authority: Authority::ReadOnly,
        user: Some(USER_SID.into()),
    }
}

/// The core over a fake supervisor, and connections to it.
pub struct Harness {
    pub core: Arc<HelperCore<FakeSupervisor>>,
    pub budget: Arc<Budget>,
    pub config: ConnConfig,
    /// Unread bytes each direction of a connection holds.
    pub pipe_capacity: usize,
    next_conn: AtomicU64,
}

impl Harness {
    pub fn new() -> Self {
        Self::with(ConnConfig::default(), Budget::DEFAULT_LIMIT)
    }

    pub fn with(config: ConnConfig, budget: usize) -> Self {
        Self {
            core: Arc::new(HelperCore::new(FakeSupervisor::new(), config.limits)),
            budget: Arc::new(Budget::new(budget)),
            config,
            pipe_capacity: 64 * 1024,
            next_conn: AtomicU64::new(1),
        }
    }

    pub fn fake(&self) -> &FakeSupervisor {
        self.core.supervisor()
    }

    /// A connection from `caller`, served on its own thread.
    pub fn connect(&self, caller: Caller) -> (Client, JoinHandle<Ended>) {
        let (client_end, server_end) = pair(self.pipe_capacity);
        let conn: ConnId = self.next_conn.fetch_add(1, Ordering::SeqCst);
        let core = self.core.clone();
        let budget = self.budget.clone();
        let config = self.config;
        let served =
            thread::spawn(move || serve(&server_end, conn, &caller, &*core, &config, &budget));
        (Client::new(client_end), served)
    }
}

/// The GUI's side of a connection.
pub struct Client {
    end: MemoryEnd,
    decoder: FrameDecoder,
    buf: Vec<u8>,
    unread: Vec<u8>,
}

/// How long a test waits for the helper before it fails.
pub const PATIENCE: Duration = Duration::from_secs(10);

impl Client {
    fn new(end: MemoryEnd) -> Self {
        Self {
            end,
            decoder: FrameDecoder::new(Limits::default().to_gui_caps()),
            buf: vec![0; 4096],
            unread: Vec::new(),
        }
    }

    pub fn send(&self, request: &Request) {
        self.send_bytes(&encode_request(request, &Limits::default()).unwrap());
    }

    pub fn send_bytes(&self, bytes: &[u8]) {
        self.end
            .write_all(bytes, Instant::now() + PATIENCE)
            .expect("the helper takes what is sent");
    }

    /// Hang up.
    pub fn close(&self) {
        self.end.shutdown_write();
    }

    /// The next message, or `None` at the end of the stream.
    pub fn recv(&mut self) -> Option<ToClient> {
        self.recv_by(Instant::now() + PATIENCE)
            .expect("the helper answers in time")
    }

    /// The next message by `deadline`: `Err` when none came.
    pub fn recv_by(&mut self, deadline: Instant) -> Result<Option<ToClient>, ()> {
        loop {
            let taken = self.decoder.feed(&self.unread);
            self.unread.drain(..taken);
            if let Some(frame) = self.decoder.next_frame().unwrap() {
                return Ok(Some(decode_to_client(&frame).unwrap()));
            }
            match self.end.read(&mut self.buf, Some(deadline)) {
                Ok(0) => return Ok(None),
                Ok(n) => self.unread.extend_from_slice(&self.buf[..n]),
                Err(error) if error.kind() == std::io::ErrorKind::TimedOut => return Err(()),
                Err(_) => return Ok(None),
            }
        }
    }

    /// The next message, which must be a reply.
    pub fn reply(&mut self) -> Reply {
        match self.recv() {
            Some(ToClient::Reply(reply)) => reply,
            other => panic!("expected a reply, got {other:?}"),
        }
    }

    /// The next message, which must be an event.
    pub fn event(&mut self) -> Event {
        match self.recv() {
            Some(ToClient::Event(event)) => event,
            other => panic!("expected an event, got {other:?}"),
        }
    }

    /// Send `request` and take its reply.
    pub fn ask(&mut self, request: &Request) -> Reply {
        self.send(request);
        self.reply()
    }

    pub fn hello(&mut self) -> HelloReply {
        match self.ask(&Request::hello()) {
            Reply::Hello(hello) => hello,
            other => panic!("expected hello, got {other:?}"),
        }
    }

    /// Every message until the end of the stream.
    pub fn rest(&mut self) -> Vec<ToClient> {
        std::iter::from_fn(|| self.recv()).collect()
    }
}
