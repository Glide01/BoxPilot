//! `service_smoke`: the installed `BoxPilotHelper` service, driven from the
//! outside on a real Windows machine (ADR 0006, "Verification before
//! shipping"; `docs/helper-windows-checklist.md`).
//!
//! The helper was built and unit-tested on Linux. This checks what only
//! Windows can say: that the MSI's service and pipe let the right accounts in
//! and keep the others out, that a TUN start really brings TUN up as SYSTEM
//! and takes it down with its connection, and that the helper holds its
//! connection limits and deadlines. CI runs it on GitHub's `windows-latest`
//! runner through `packaging/windows/helper-smoke.ps1`, after installing the
//! MSI, as the runner's elevated administrator and as a fresh standard
//! account. It is a test tool: the MSI never installs it.
//!
//! It talks to the helper as the GUI does (`src/core/privileged_helper`): the
//! pipe opened for `GENERIC_READ | FILE_WRITE_DATA` with an
//! identification-only QoS, the service started on demand, requests built
//! with the protocol crate's public API, and a start checked first with the
//! policy the GUI runs. Never through the helper's own modules: they are what
//! is under test. (The `token` command compares sing-box's token with
//! `spawnplan::SING_BOX_TOKEN`, the constant that says what it may hold: the
//! specification, read from outside, not the code that applies it.)
//!
//! One synchronous pipe handle, polled with `PeekNamedPipe`, so every wait
//! has a deadline without the GUI's overlapped I/O. Only the network probes
//! run on a thread of their own, and they never touch the pipe.
//!
//! Each command checks one set of expectations. The exit code is 0 when they
//! held, 1 when one didn't (and what was seen is printed), 2 for a bad
//! command line, 3 when `token` could not read the token at all (a finding
//! about what an administrator may see, not about the helper).

#[cfg(not(windows))]
fn main() {
    eprintln!("service_smoke drives the BoxPilotHelper Windows service; it runs on Windows only");
    std::process::exit(boxpilot_protocol::endpoint::exit::UNSUPPORTED_OS);
}

#[cfg(windows)]
fn main() {
    std::process::exit(smoke::main());
}

#[cfg(windows)]
mod smoke {
    use boxpilot_helper::spawnplan::{integrity, SING_BOX_TOKEN};
    use boxpilot_protocol::endpoint::{PIPE_NAME, SERVICE_NAME};
    use boxpilot_protocol::{
        decode_to_client, encode_request, ErrorCode, Event, ExitInfo, FrameDecoder, HelloReply,
        Limits, RefusalCode, Reply, Request, RunState, StartRequest, Started, ToClient, TunOptions,
        WireRefusal, PROTOCOL_VERSION,
    };
    use serde_json::json;
    use std::collections::BTreeSet;
    use std::fmt;
    use std::fs::{self, File};
    use std::io::{self, Read, Write};
    use std::net::{
        IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream,
        ToSocketAddrs, UdpSocket,
    };
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{
        LocalFree, ERROR_ACCESS_DENIED, ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND,
        ERROR_INSUFFICIENT_BUFFER, ERROR_NO_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_NOT_CONNECTED,
        ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_SPECIFIC_ERROR, GENERIC_READ, GENERIC_WRITE,
        HANDLE, HLOCAL, LUID, WIN32_ERROR,
    };
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::Win32::Security::{
        AdjustTokenPrivileges, GetTokenInformation, LookupPrivilegeNameW, LookupPrivilegeValueW,
        TokenGroups, TokenIntegrityLevel, TokenPrivileges, TokenUser, LUID_AND_ATTRIBUTES, PSID,
        SE_PRIVILEGE_ENABLED, SID_AND_ATTRIBUTES, TOKEN_ADJUST_PRIVILEGES, TOKEN_GROUPS,
        TOKEN_INFORMATION_CLASS, TOKEN_MANDATORY_LABEL, TOKEN_PRIVILEGES, TOKEN_QUERY, TOKEN_USER,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_SHARE_NONE, FILE_WRITE_DATA,
        OPEN_EXISTING, PIPE_ACCESS_DUPLEX, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
    };
    use windows::Win32::System::Pipes::{
        CreateNamedPipeW, GetNamedPipeServerProcessId, PeekNamedPipe, WaitNamedPipeW,
        PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
        PIPE_WAIT,
    };
    use windows::Win32::System::Services::{
        CloseServiceHandle, OpenSCManagerW, OpenServiceW, QueryServiceStatus, StartServiceW,
        SC_HANDLE, SC_MANAGER_CONNECT, SERVICE_QUERY_STATUS, SERVICE_START, SERVICE_STATUS,
        SERVICE_STOPPED,
    };
    use windows::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    const USAGE: &str = "\
usage: service_smoke <command> [options]

as an administrator (the runner):
  hello --expect start [--sha256 <hex>] [--sing-box-version <version>]
  refused          a start the policy refuses comes back `refused`, naming the fields
  slots            8 connections are served, a 9th waits until one ends
  write-deadline   a client that stops reading is dropped
  tun [--probes] [--ready-file <file> [--release-file <file>]]
      [--end stop|close|mid-frame|helper-killed]
                   a real TUN start: up, (probed,) then down with its connection
  squat --hold --ready-file <file> --release-file <file>
                   hold the service's pipe name, so the service can't start
  token --pid <pid> --expect sing-box|privileges|print [--privileges <A,B,..>]
                   read a process's token from outside and print it;
                   sing-box: exactly the privileges, integrity level and
                   Administrators group spawnplan::SING_BOX_TOKEN plans;
                   privileges: exactly the privileges --privileges lists

as a standard account:
  hello --expect readonly
  unauthorized     start and stop are refused as unauthorized; status is not
  squat --expect denied
                   this account can neither create the pipe nor add an instance
  generic-write --expect denied
                   this account can't open the pipe for GENERIC_WRITE
  readonly-slots [--ready-file <file> --release-file <file>]
                   4 read-only connections are served, a 5th is closed
  denied --dir <dir> --file <file>
                   this account can neither list <dir> nor open <file>

--ready-file: written (key=value lines) once the expectations so far held;
--release-file: then wait for it to appear (at most 3 minutes) before ending.";

    /// The expectations held.
    const HELD: i32 = 0;
    /// One didn't.
    const FAILED: i32 = 1;
    /// A bad command line.
    const USAGE_ERROR: i32 = 2;
    /// `token` could not read the token: a finding, not a failure.
    const UNREADABLE: i32 = 3;

    /// How long the pipe may take to appear. The GUI allows 15 s; a runner's
    /// first start (the fresh binaries scanned as they are hashed) gets more.
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
    /// The helper's own deadline for `hello`, and the GUI's.
    const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
    /// A whole `start`, as the GUI allows.
    const START_TIMEOUT: Duration = Duration::from_secs(60);
    /// From `started` to sing-box's log saying TUN is up: wintun installs its
    /// driver on the machine's first start.
    const TUN_UP_TIMEOUT: Duration = Duration::from_secs(45);
    /// The longest wait for a `--release-file`.
    const HOLD_LIMIT: Duration = Duration::from_secs(180);
    /// How often an idle pipe is looked at.
    const POLL: Duration = Duration::from_millis(10);
    /// The helper drops a client that stops reading after 10 s (its write
    /// deadline, `conn::Timeouts::write`); this waits that out with room.
    const WRITE_DEADLINE_WAIT: Duration = Duration::from_secs(15);
    /// The longest any command runs. Every wait has its own deadline but
    /// one: a write to the pipe blocks for as long as the helper reads
    /// nothing, so a helper that never drops such a client would hang the
    /// tool, and CI with it, instead of failing it.
    const WATCHDOG: Duration = Duration::from_secs(10 * 60);

    /// What the GUI opens the pipe with, and everything the pipe's DACL
    /// grants interactive users: `GENERIC_WRITE` would include
    /// `FILE_CREATE_PIPE_INSTANCE` (ADR 0006 rule 5).
    const GUI_ACCESS: u32 = GENERIC_READ.0 | FILE_WRITE_DATA.0;

    /// The connections the helper serves at once, and of those, the ones
    /// from callers that may not start (`win/server.rs`).
    const MAX_CONNECTIONS: usize = 8;
    const MAX_READ_ONLY_CONNECTIONS: usize = 4;

    /// The pipe's instance limit as the helper creates it (`MAX_CONNECTIONS
    /// + 1`, one always listening). Asking to add an instance with any
    /// other limit could be refused for that alone, which would hide the
    /// access check the `squat` command is after.
    const HELPER_PIPE_INSTANCES: u32 = MAX_CONNECTIONS as u32 + 1;

    /// A public address every runner reaches: the proxy and TUN must still
    /// carry ordinary traffic while the loopback rule holds.
    const PUBLIC_ADDRESS: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(1, 1, 1, 1), 443);
    /// A public name, for DNS through TUN and a by-name proxy request: one
    /// nothing else on a runner resolves, so the answer can't come from the
    /// DNS client's cache (`helper-smoke.ps1` clears it before the run too).
    const PUBLIC_NAME: &str = "one.one.one.one";

    /// The attachment the TUN profile's local rule set travels as, and its
    /// content: a rule set sing-box must read from the run directory, or
    /// fail to start.
    const RULE_SET_ID: &str = "smoke-rules";
    const RULE_SET: &str = r#"{"version": 1, "rules": [{"domain_suffix": ["smoke.invalid"]}]}"#;

    pub fn main() -> i32 {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let Some((command, rest)) = args.split_first() else {
            eprintln!("{USAGE}");
            return USAGE_ERROR;
        };
        let options = match Options::parse(rest) {
            Ok(options) => options,
            Err(message) => {
                eprintln!("service_smoke: {message}\n{USAGE}");
                return USAGE_ERROR;
            }
        };
        watchdog(
            WATCHDOG,
            format!("service_smoke {command}: FAILED: still running after {WATCHDOG:?}"),
        );
        let result = match command.as_str() {
            "hello" => hello(&options),
            "refused" => refused(),
            "slots" => slots(),
            "write-deadline" => write_deadline(),
            "tun" => tun(&options),
            "squat" => squat(&options),
            "unauthorized" => unauthorized(),
            "generic-write" => generic_write(&options),
            "readonly-slots" => readonly_slots(&options),
            "denied" => denied(&options),
            "token" => token(&options),
            other => Err(Failure::Usage(format!("unknown command {other:?}"))),
        };
        match result {
            Ok(()) => {
                println!("service_smoke {command}: every expectation held");
                HELD
            }
            Err(Failure::Usage(message)) => {
                eprintln!("service_smoke {command}: {message}\n{USAGE}");
                USAGE_ERROR
            }
            Err(Failure::Unreadable(message)) => {
                eprintln!("service_smoke {command}: UNREADABLE: {message}");
                UNREADABLE
            }
            Err(Failure::Check(message)) => {
                eprintln!("service_smoke {command}: FAILED: {message}");
                FAILED
            }
        }
    }

    /// End the process as failed, saying `message`, unless it has ended by
    /// `after`.
    fn watchdog(after: Duration, message: String) {
        thread::spawn(move || {
            thread::sleep(after);
            eprintln!("{message}");
            std::process::exit(FAILED);
        });
    }

    // ---- The command line ----

    /// Why a command didn't hold.
    enum Failure {
        Usage(String),
        /// The token can't be read from outside at all.
        Unreadable(String),
        /// An expectation didn't hold, or the helper couldn't be asked.
        Check(String),
    }

    impl From<String> for Failure {
        fn from(message: String) -> Self {
            Failure::Check(message)
        }
    }

    type Outcome<T = ()> = Result<T, Failure>;

    #[derive(Default)]
    struct Options {
        expect: Option<String>,
        sha256: Option<String>,
        sing_box_version: Option<String>,
        end: Option<String>,
        ready_file: Option<PathBuf>,
        release_file: Option<PathBuf>,
        dir: Option<PathBuf>,
        file: Option<PathBuf>,
        pid: Option<u32>,
        privileges: Option<String>,
        probes: bool,
        hold: bool,
    }

    impl Options {
        fn parse(args: &[String]) -> Result<Self, String> {
            let mut options = Options::default();
            let mut args = args.iter();
            while let Some(arg) = args.next() {
                let mut value = || {
                    args.next()
                        .cloned()
                        .ok_or_else(|| format!("{arg} needs a value"))
                };
                match arg.as_str() {
                    "--expect" => options.expect = Some(value()?),
                    "--sha256" => options.sha256 = Some(value()?),
                    "--sing-box-version" => options.sing_box_version = Some(value()?),
                    "--end" => options.end = Some(value()?),
                    "--ready-file" => options.ready_file = Some(value()?.into()),
                    "--release-file" => options.release_file = Some(value()?.into()),
                    "--dir" => options.dir = Some(value()?.into()),
                    "--file" => options.file = Some(value()?.into()),
                    "--probes" => options.probes = true,
                    "--hold" => options.hold = true,
                    "--pid" => {
                        options.pid = Some(
                            value()?
                                .parse()
                                .map_err(|_| "--pid takes a process id".to_owned())?,
                        )
                    }
                    "--privileges" => options.privileges = Some(value()?),
                    other => return Err(format!("unexpected argument {other:?}")),
                }
            }
            if options.release_file.is_some() && options.ready_file.is_none() {
                return Err("--release-file goes with --ready-file".into());
            }
            Ok(options)
        }

        /// `--expect`, which must be one of `allowed`.
        fn expect(&self, allowed: &[&str]) -> Outcome<&str> {
            match self.expect.as_deref() {
                Some(value) if allowed.contains(&value) => Ok(value),
                _ => Err(Failure::Usage(format!(
                    "--expect must be one of {}",
                    allowed.join(", ")
                ))),
            }
        }
    }

    // ---- The service and its pipe ----

    /// `text` as a NUL-terminated UTF-16 string.
    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// The Win32 code of a failed call.
    fn win32_code(error: &windows::core::Error) -> u32 {
        WIN32_ERROR::from_error(error)
            .map(|code| code.0)
            .unwrap_or(error.code().0 as u32)
    }

    /// A Win32 code in words, with its number.
    fn describe(code: u32) -> String {
        format!(
            "{} (Win32 error {code})",
            io::Error::from_raw_os_error(code as i32)
        )
    }

    /// Whether a Win32 code means the helper closed its end.
    fn is_end_of_stream(code: u32) -> bool {
        [ERROR_BROKEN_PIPE, ERROR_PIPE_NOT_CONNECTED, ERROR_NO_DATA]
            .iter()
            .any(|end| end.0 == code)
    }

    /// The client end of the helper's pipe: a synchronous handle.
    struct Pipe(File);

    impl Pipe {
        /// One attempt to open the pipe with `access`, and the QoS the GUI
        /// uses: identification only, so the helper may learn who is asking,
        /// never act as them.
        fn open(access: u32) -> Result<Self, u32> {
            let name = wide(PIPE_NAME);
            // SAFETY: `name` is NUL-terminated and outlives the call; no
            // security attributes, no template. The handle is owned below.
            let handle = unsafe {
                CreateFileW(
                    PCWSTR(name.as_ptr()),
                    access,
                    FILE_SHARE_NONE,
                    None,
                    OPEN_EXISTING,
                    SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                    None,
                )
            }
            .map_err(|error| win32_code(&error))?;
            // SAFETY: a valid handle, just opened, owned by nobody else.
            Ok(Self(File::from(unsafe {
                OwnedHandle::from_raw_handle(handle.0)
            })))
        }

        fn handle(&self) -> HANDLE {
            HANDLE(self.0.as_raw_handle())
        }

        /// The bytes waiting to be read; `None` once the helper has closed
        /// its end and everything it wrote has been read.
        fn available(&self) -> Result<Option<usize>, String> {
            let mut available = 0u32;
            // SAFETY: the pipe is open for the call; no buffer is passed, and
            // `available` is a valid out-pointer.
            match unsafe { PeekNamedPipe(self.handle(), None, 0, None, Some(&mut available), None) }
            {
                Ok(()) => Ok(Some(available as usize)),
                Err(error) if is_end_of_stream(win32_code(&error)) => Ok(None),
                Err(error) => Err(format!(
                    "reading the pipe: {}",
                    describe(win32_code(&error))
                )),
            }
        }

        /// The process at the other end: the helper.
        fn server_pid(&self) -> Option<u32> {
            let mut pid = 0u32;
            // SAFETY: the pipe is open for the call; `pid` is a valid
            // out-pointer.
            unsafe { GetNamedPipeServerProcessId(self.handle(), &mut pid) }.ok()?;
            Some(pid)
        }
    }

    /// A service-control handle, closed on drop.
    struct ScHandle(SC_HANDLE);

    impl Drop for ScHandle {
        fn drop(&mut self) {
            // SAFETY: an open handle that only this value owns.
            let _ = unsafe { CloseServiceHandle(self.0) };
        }
    }

    /// The helper's service, opened with `access`; the manager with
    /// `SC_MANAGER_CONNECT` only, as the GUI does.
    fn open_service(access: u32) -> Result<(ScHandle, ScHandle), u32> {
        // SAFETY: the local machine's active database; the handle is owned.
        let manager = unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT) }
            .map(ScHandle)
            .map_err(|error| win32_code(&error))?;
        let name = wide(SERVICE_NAME);
        // SAFETY: an open manager handle and a NUL-terminated name; the
        // handle returned is owned.
        let service = unsafe { OpenServiceW(manager.0, PCWSTR(name.as_ptr()), access) }
            .map(ScHandle)
            .map_err(|error| win32_code(&error))?;
        Ok((manager, service))
    }

    /// Start the service with `SERVICE_START`, the right its DACL grants
    /// interactive users.
    fn start_service() -> Result<(), u32> {
        let (_manager, service) = open_service(SERVICE_START)?;
        // SAFETY: an open service handle with SERVICE_START; no arguments.
        unsafe { StartServiceW(service.0, None) }.map_err(|error| win32_code(&error))
    }

    fn service_status() -> Result<SERVICE_STATUS, u32> {
        let (_manager, service) = open_service(SERVICE_QUERY_STATUS)?;
        let mut status = SERVICE_STATUS::default();
        // SAFETY: an open service handle with SERVICE_QUERY_STATUS, and a
        // SERVICE_STATUS to fill.
        unsafe { QueryServiceStatus(service.0, &mut status) }
            .map_err(|error| win32_code(&error))?;
        Ok(status)
    }

    /// Connect as the GUI does (`privileged_helper::ConnectPlan`): no pipe
    /// means the service isn't running, so start it; once this connect has
    /// started it, a missing pipe means "not listening yet" or "it failed",
    /// which its status tells apart (a clean stop is an idle exit that raced
    /// the start, so start it again); every instance busy means wait.
    fn connect() -> Result<Pipe, String> {
        let name = wide(PIPE_NAME);
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        let mut started = false;
        let mut backoff = Duration::from_millis(50);
        loop {
            let code = match Pipe::open(GUI_ACCESS) {
                Ok(pipe) => return Ok(pipe),
                Err(code) => code,
            };
            if Instant::now() >= deadline {
                return Err(format!(
                    "{PIPE_NAME} didn't open within {}s: {}",
                    CONNECT_TIMEOUT.as_secs(),
                    describe(code)
                ));
            }
            if code == ERROR_FILE_NOT_FOUND.0 && !started {
                match start_service() {
                    Ok(()) => started = true,
                    Err(code) if code == ERROR_SERVICE_ALREADY_RUNNING.0 => {}
                    Err(code) => {
                        return Err(format!("starting {SERVICE_NAME}: {}", describe(code)))
                    }
                }
            } else if code == ERROR_FILE_NOT_FOUND.0 {
                let status = service_status()
                    .map_err(|code| format!("the status of {SERVICE_NAME}: {}", describe(code)))?;
                if status.dwCurrentState == SERVICE_STOPPED {
                    if status.dwWin32ExitCode == ERROR_SERVICE_SPECIFIC_ERROR.0 {
                        return Err(format!(
                            "{SERVICE_NAME} stopped with its exit code {} \
                             (boxpilot_protocol::endpoint::exit)",
                            status.dwServiceSpecificExitCode
                        ));
                    }
                    if status.dwWin32ExitCode != 0 {
                        return Err(format!(
                            "{SERVICE_NAME} stopped: {}",
                            describe(status.dwWin32ExitCode)
                        ));
                    }
                    started = false;
                }
            } else if code == ERROR_PIPE_BUSY.0 {
                let left = deadline.saturating_duration_since(Instant::now());
                let millis = left.min(Duration::from_secs(2)).as_millis() as u32;
                // SAFETY: `name` is NUL-terminated and outlives the call.
                let _ = unsafe { WaitNamedPipeW(PCWSTR(name.as_ptr()), millis) };
                continue;
            } else {
                return Err(format!("opening {PIPE_NAME}: {}", describe(code)));
            }
            thread::sleep(backoff);
            backoff = (backoff * 2).min(Duration::from_millis(500));
        }
    }

    /// Whether some instance of the service's pipe exists right now.
    fn pipe_exists() -> bool {
        let name = wide(PIPE_NAME);
        // SAFETY: `name` is NUL-terminated and outlives the call; 1 ms.
        let available = unsafe { WaitNamedPipeW(PCWSTR(name.as_ptr()), 1) }.as_bool();
        available
            || io::Error::last_os_error().raw_os_error() != Some(ERROR_FILE_NOT_FOUND.0 as i32)
    }

    /// Create a server instance of the service's pipe, with the default
    /// security descriptor; `first` adds `FILE_FLAG_FIRST_PIPE_INSTANCE`.
    fn create_instance(first: bool, max_instances: u32) -> Result<OwnedHandle, u32> {
        let name = wide(PIPE_NAME);
        let mut mode = PIPE_ACCESS_DUPLEX;
        if first {
            mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
        }
        // SAFETY: `name` is NUL-terminated and outlives the call; no
        // security attributes. The handle is owned below.
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(name.as_ptr()),
                mode,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                max_instances,
                4096,
                4096,
                0,
                None,
            )
        };
        if handle.is_invalid() {
            let code = io::Error::last_os_error().raw_os_error().unwrap_or(0);
            return Err(code as u32);
        }
        // SAFETY: a valid pipe handle, just created, owned by nobody else.
        Ok(unsafe { OwnedHandle::from_raw_handle(handle.0) })
    }

    // ---- One connection ----

    /// What reading a connection gave.
    enum Next {
        Message(ToClient),
        /// The helper closed the connection.
        Eof,
        TimedOut,
    }

    /// One connection to the helper, read by polling. sing-box's lines are
    /// printed as they come and kept, and its exit is kept.
    struct Conn {
        pipe: Pipe,
        decoder: FrameDecoder,
        /// Bytes read but not yet fed: `feed` stops at the end of a frame.
        unfed: Vec<u8>,
        limits: Limits,
        lines: Vec<String>,
        exited: Option<ExitInfo>,
    }

    fn request_name(request: &Request) -> &'static str {
        match request {
            Request::Hello { .. } => "hello",
            Request::Start(_) => "start",
            Request::Stop => "stop",
            Request::Status => "status",
        }
    }

    impl Conn {
        fn open() -> Result<Self, String> {
            Ok(Self::over(connect()?))
        }

        fn over(pipe: Pipe) -> Self {
            let limits = Limits::default();
            Self {
                pipe,
                decoder: FrameDecoder::new(limits.to_gui_caps()),
                unfed: Vec::new(),
                limits,
                lines: Vec::new(),
                exited: None,
            }
        }

        fn send(&mut self, request: &Request) -> Result<(), String> {
            let name = request_name(request);
            let bytes = encode_request(request, &self.limits)
                .map_err(|error| format!("encoding {name}: {error}"))?;
            self.pipe
                .0
                .write_all(&bytes)
                .map_err(|error| format!("sending {name}: {error}"))
        }

        /// The next message, the end of the stream, or nothing by `deadline`.
        fn next(&mut self, deadline: Instant) -> Result<Next, String> {
            loop {
                let taken = self.decoder.feed(&self.unfed);
                self.unfed.drain(..taken);
                match self.decoder.next_frame() {
                    Ok(Some(frame)) => {
                        let message = decode_to_client(&frame)
                            .map_err(|error| format!("the helper sent {frame:?}: {error}"))?;
                        match &message {
                            ToClient::Event(Event::Log { line, .. }) => {
                                println!("  sing-box | {line}");
                                self.lines.push(line.clone());
                            }
                            ToClient::Event(Event::Exited(exit)) => {
                                println!("  sing-box exited: {exit:?}");
                                self.exited = Some(*exit);
                            }
                            ToClient::Reply(_) => {}
                        }
                        return Ok(Next::Message(message));
                    }
                    // `feed` took everything: read more.
                    Ok(None) => {}
                    Err(error) => return Err(format!("the helper broke the protocol: {error}")),
                }
                match self.pipe.available()? {
                    None => return Ok(Next::Eof),
                    Some(0) => {
                        if Instant::now() >= deadline {
                            return Ok(Next::TimedOut);
                        }
                        thread::sleep(POLL);
                    }
                    Some(waiting) => {
                        let mut buf = vec![0u8; waiting.min(64 * 1024)];
                        let read = self
                            .pipe
                            .0
                            .read(&mut buf)
                            .map_err(|error| format!("reading the pipe: {error}"))?;
                        if read == 0 {
                            return Ok(Next::Eof);
                        }
                        self.unfed.extend_from_slice(&buf[..read]);
                    }
                }
            }
        }

        /// The reply to the outstanding request, within `timeout`. Events on
        /// the way are kept.
        fn reply(&mut self, timeout: Duration) -> Result<Reply, String> {
            let deadline = Instant::now() + timeout;
            loop {
                match self.next(deadline)? {
                    Next::Message(ToClient::Reply(reply)) => return Ok(reply),
                    Next::Message(ToClient::Event(_)) => {}
                    Next::Eof => {
                        return Err("the helper closed the connection before replying".into())
                    }
                    Next::TimedOut => {
                        return Err(format!("no reply within {}s", timeout.as_secs()))
                    }
                }
            }
        }

        fn hello(&mut self) -> Result<HelloReply, String> {
            self.send(&Request::hello())?;
            match self.reply(HELLO_TIMEOUT)? {
                Reply::Hello(hello) if hello.protocol_version == PROTOCOL_VERSION => Ok(hello),
                other => Err(format!("hello was answered with {other:?}")),
            }
        }

        fn status(&mut self) -> Result<(RunState, Option<ExitInfo>), String> {
            self.send(&Request::Status)?;
            match self.reply(HELLO_TIMEOUT)? {
                Reply::Status { state, last_exit } => Ok((state, last_exit)),
                other => Err(format!("status was answered with {other:?}")),
            }
        }

        /// Read events until `until`; sing-box must keep running and the
        /// connection stay open, and nothing but events may arrive.
        fn expect_running(&mut self, until: Instant) -> Result<(), String> {
            loop {
                match self.next(until)? {
                    Next::Message(ToClient::Event(Event::Exited(exit))) => {
                        return Err(format!("sing-box exited ({exit:?}) while it should run"))
                    }
                    Next::Message(ToClient::Event(Event::Log { .. })) => {}
                    Next::Message(ToClient::Reply(reply)) => {
                        return Err(format!("an unasked reply: {reply:?}"))
                    }
                    Next::Eof => {
                        return Err("the helper closed the connection while sing-box ran".into())
                    }
                    Next::TimedOut => return Ok(()),
                }
            }
        }

        /// Wait until sing-box has logged a line that `matches`.
        fn wait_for_line(
            &mut self,
            what: &str,
            matches: impl Fn(&str) -> bool,
            timeout: Duration,
        ) -> Result<(), String> {
            let deadline = Instant::now() + timeout;
            while !self.lines.iter().any(|line| matches(line)) {
                if Instant::now() >= deadline {
                    return Err(format!(
                        "sing-box logged no line {what} within {}s",
                        timeout.as_secs()
                    ));
                }
                self.expect_running(Instant::now() + Duration::from_millis(100))?;
            }
            Ok(())
        }

        /// Read until the helper closes the connection.
        fn wait_eof(&mut self, timeout: Duration) -> Result<(), String> {
            let deadline = Instant::now() + timeout;
            loop {
                match self.next(deadline)? {
                    Next::Eof => return Ok(()),
                    Next::Message(_) => {}
                    Next::TimedOut => {
                        return Err(format!(
                            "the helper kept the connection open past {}s",
                            timeout.as_secs()
                        ))
                    }
                }
            }
        }
    }

    // ---- Files the PowerShell side waits on ----

    /// Write `pairs` as `key=value` lines, all at once: written beside and
    /// renamed, so a reader never sees half.
    fn write_ready(path: &Path, pairs: &[(&str, String)]) -> Result<(), String> {
        let text: String = pairs
            .iter()
            .map(|(key, value)| format!("{key}={value}\n"))
            .collect();
        let partial = path.with_extension("partial");
        fs::write(&partial, text)
            .and_then(|()| fs::rename(&partial, path))
            .map_err(|error| format!("writing {}: {error}", path.display()))?;
        println!("wrote {}", path.display());
        Ok(())
    }

    fn wait_for_file(
        path: &Path,
        mut idle: impl FnMut() -> Result<(), String>,
    ) -> Result<(), String> {
        let deadline = Instant::now() + HOLD_LIMIT;
        println!("waiting for {}", path.display());
        while !path.exists() {
            if Instant::now() >= deadline {
                return Err(format!(
                    "{} didn't appear within {}s",
                    path.display(),
                    HOLD_LIMIT.as_secs()
                ));
            }
            idle()?;
        }
        Ok(())
    }

    fn sleep_a_little() -> Result<(), String> {
        thread::sleep(Duration::from_millis(100));
        Ok(())
    }

    // ---- hello ----

    /// The reply to `hello`, from the authority the helper decides from the
    /// caller's token (ADR 0006 rule 4), and `status`, which every caller
    /// may ask.
    fn hello(options: &Options) -> Outcome {
        let may_start = options.expect(&["start", "readonly"])? == "start";
        let began = Instant::now();
        let mut conn = Conn::open()?;
        let connected = began.elapsed();
        let hello = conn.hello()?;
        println!(
            "helper {} (pid {}), sing-box {} sha256 {}, may_start {}; connected in {} ms",
            hello.helper_version,
            conn.pipe
                .server_pid()
                .map_or("?".into(), |pid| pid.to_string()),
            hello.sing_box_version,
            hello.sing_box_sha256,
            hello.may_start,
            connected.as_millis()
        );
        if hello.may_start != may_start {
            return Err(format!(
                "the helper says may_start = {}, expected {may_start}",
                hello.may_start
            )
            .into());
        }
        if hello.helper_version != env!("CARGO_PKG_VERSION") {
            return Err(format!(
                "the installed helper is {}, this build is {}",
                hello.helper_version,
                env!("CARGO_PKG_VERSION")
            )
            .into());
        }
        let hex = &hello.sing_box_sha256;
        if hex.len() != 64
            || !hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(format!("sing_box_sha256 {hex:?} is not 64 lowercase hex digits").into());
        }
        if let Some(expected) = &options.sha256 {
            if !hex.eq_ignore_ascii_case(expected) {
                return Err(format!(
                    "the helper reports sing-box {hex}, the installed file is {expected}"
                )
                .into());
            }
        }
        if let Some(expected) = &options.sing_box_version {
            if &hello.sing_box_version != expected {
                return Err(format!(
                    "the helper reports sing-box {}, the manifest says {expected}",
                    hello.sing_box_version
                )
                .into());
            }
        }
        let (state, last_exit) = conn.status()?;
        println!("status: {state:?}, last exit {last_exit:?}");
        Ok(())
    }

    // ---- Starts ----

    fn free_port() -> Result<u16, String> {
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .and_then(|listener| listener.local_addr())
            .map(|address| address.port())
            .map_err(|error| format!("no free loopback port: {error}"))
    }

    fn tun_options(proxy_port: u16) -> TunOptions {
        TunOptions {
            ipv6: false,
            proxy_port,
            allow_lan: false,
            system_proxy: false,
        }
    }

    /// The TUN interface's address (`boxpilot_runconfig::TUN_IPV4_ADDRESS`).
    fn tun_address() -> Ipv4Addr {
        boxpilot_runconfig::TUN_IPV4_ADDRESS
            .split('/')
            .next()
            .and_then(|address| address.parse().ok())
            .expect("TUN_IPV4_ADDRESS is an IPv4 address with a prefix length")
    }

    /// The smallest profile a TUN start can run that keeps the runner's
    /// network: one direct outbound (bound to the real interface, so its
    /// own traffic doesn't loop through TUN), DNS hijacked to a public
    /// resolver (the system resolver points at TUN while it is up), and a
    /// local rule set, which travels as an attachment as the GUI sends it
    /// (`prepare_start`): read here, attached by id, checked by the same
    /// policy the helper runs.
    fn tun_start(proxy_port: u16) -> Result<StartRequest, String> {
        let mut config = json!({
            "log": {"level": "info"},
            "dns": {
                "servers": [{"type": "udp", "tag": "public", "server": "1.1.1.1"}]
            },
            "outbounds": [{"type": "direct", "tag": "direct"}],
            "route": {
                "rule_set": [{
                    "type": "local",
                    "tag": "smoke",
                    "format": "source",
                    "path": "smoke-rules.json"
                }],
                "rules": [
                    {"action": "sniff"},
                    {"protocol": "dns", "action": "hijack-dns"},
                    {"rule_set": "smoke", "action": "reject"}
                ],
                "auto_detect_interface": true,
                "default_domain_resolver": "public",
                "final": "direct"
            }
        });
        let fields = boxpilot_policy::local_file_fields(&config);
        let [field] = fields.as_slice() else {
            return Err(format!(
                "the smoke profile reads {} local files, not 1",
                fields.len()
            ));
        };
        boxpilot_policy::attach(&mut config, &field.pointer, RULE_SET_ID)
            .map_err(|error| format!("attaching {}: {error}", field.pointer))?;
        let request = StartRequest {
            config: config.to_string(),
            attachments: vec![(RULE_SET_ID.to_owned(), RULE_SET.as_bytes().to_vec())],
            options: tun_options(proxy_port),
        };
        boxpilot_policy::check(
            &request.config,
            &request.attachment_ids(),
            &Default::default(),
        )
        .map_err(|refusals| {
            format!("the policy refuses the smoke profile here already: {refusals:?}")
        })?;
        Ok(request)
    }

    /// A profile with fields the privileged path refuses, as a subscription
    /// might carry them: the tor outbound runs a program, and NTP
    /// `write_to_system` sets the clock.
    fn refused_config() -> String {
        json!({
            "ntp": {"enabled": true, "server": "time.windows.com", "write_to_system": true},
            "outbounds": [
                {"type": "direct", "tag": "direct"},
                {"type": "tor", "tag": "tor", "executable_path": "C:\\Windows\\System32\\cmd.exe"}
            ]
        })
        .to_string()
    }

    /// What the policy must name in [`refused_config`], at least.
    const EXPECTED_REFUSALS: &[(&str, RefusalCode)] = &[
        ("/ntp/write_to_system", RefusalCode::SystemChange),
        ("/outbounds/1/executable_path", RefusalCode::RunsProgram),
        ("/outbounds/1/type", RefusalCode::RunsProgram),
    ];

    /// A start whose config the policy refuses is answered `refused`, with
    /// the refusals the GUI's own run of the policy predicts, and starts
    /// nothing.
    fn refused() -> Outcome {
        let mut conn = Conn::open()?;
        if !conn.hello()?.may_start {
            return Err(Failure::Usage(
                "this account may not start, so its start is refused before the policy runs; \
                 run this as an administrator"
                    .into(),
            ));
        }
        let request = StartRequest {
            config: refused_config(),
            attachments: Vec::new(),
            options: tun_options(free_port()?),
        };
        let predicted: Vec<WireRefusal> = match boxpilot_policy::check(
            &request.config,
            &request.attachment_ids(),
            &Default::default(),
        ) {
            Ok(_) => {
                return Err("the policy passes the hostile profile here already"
                    .to_owned()
                    .into())
            }
            Err(refusals) => refusals.iter().map(WireRefusal::from).collect(),
        };
        conn.send(&Request::Start(request))?;
        let (refusals, omitted) = match conn.reply(START_TIMEOUT)? {
            Reply::Refused { refusals, omitted } => (refusals, omitted),
            other => return Err(format!("the start was answered {other:?}, not refused").into()),
        };
        for refusal in &refusals {
            println!(
                "refused: {} {} {}",
                refusal.pointer,
                refusal.code,
                refusal.detail.as_deref().unwrap_or("")
            );
        }
        for (pointer, code) in EXPECTED_REFUSALS {
            if !refusals
                .iter()
                .any(|r| r.pointer == *pointer && r.code == *code)
            {
                return Err(format!("the refusals don't name {pointer} ({code})").into());
            }
        }
        if refusals != predicted || omitted != 0 {
            return Err(format!(
                "the helper's refusals differ from the GUI's prediction: helper {refusals:?} \
                 (+{omitted} omitted), predicted {predicted:?}"
            )
            .into());
        }
        // A refusal isn't a protocol error: the connection stays open, and
        // nothing runs.
        let (state, _) = conn.status()?;
        if state != RunState::Stopped {
            return Err(format!("after the refusal the helper reports {state:?}").into());
        }
        Ok(())
    }

    /// A caller that may not start gets `unauthorized` for `start` and
    /// `stop`, before anything is checked or run, and `status` all the same.
    fn unauthorized() -> Outcome {
        let mut conn = Conn::open()?;
        if conn.hello()?.may_start {
            return Err(Failure::Usage(
                "this account may start; run this as a standard account".into(),
            ));
        }
        let start = tun_start(free_port()?)?;
        // The helper refuses the header and closes, so writing the blobs
        // after it may fail: what counts is the reply.
        if let Err(error) = conn.send(&Request::Start(start)) {
            println!("note: {error} (the helper closed after the header)");
        }
        expect_unauthorized(&mut conn, "start")?;
        conn.wait_eof(Duration::from_secs(10))?;

        let mut conn = Conn::open()?;
        conn.hello()?;
        conn.send(&Request::Stop)?;
        expect_unauthorized(&mut conn, "stop")?;

        let mut conn = Conn::open()?;
        conn.hello()?;
        let (state, _) = conn.status()?;
        println!("ok: status is answered ({state:?})");
        Ok(())
    }

    fn expect_unauthorized(conn: &mut Conn, what: &str) -> Result<(), String> {
        match conn.reply(HELLO_TIMEOUT)? {
            Reply::Error {
                code: ErrorCode::Unauthorized,
                message,
            } => {
                println!("ok: {what} refused as unauthorized ({message})");
                Ok(())
            }
            other => Err(format!(
                "{what} from a read-only caller was answered {other:?}"
            )),
        }
    }

    // ---- Connection limits and deadlines ----

    /// Eight connections from callers that may start are served at once; a
    /// ninth waits at the listening instance, and is served once one ends.
    fn slots() -> Outcome {
        let mut held = Vec::new();
        for n in 1..=MAX_CONNECTIONS {
            let mut conn = Conn::open()?;
            if !conn.hello()?.may_start {
                return Err(Failure::Usage(
                    "this account may not start; run this as an administrator".into(),
                ));
            }
            println!("connection {n}: served");
            held.push(conn);
        }
        let mut ninth = Conn::open()?;
        ninth.send(&Request::hello())?;
        match ninth.next(Instant::now() + Duration::from_secs(3))? {
            Next::TimedOut => println!("ok: connection 9 waits while 8 are served"),
            Next::Message(message) => {
                return Err(
                    format!("connection 9 was answered while 8 were served: {message:?}").into(),
                )
            }
            Next::Eof => {
                return Err("connection 9 was closed instead of waiting"
                    .to_owned()
                    .into())
            }
        }
        drop(held.remove(0));
        let began = Instant::now();
        match ninth.reply(HELLO_TIMEOUT)? {
            Reply::Hello(_) => println!(
                "ok: connection 9 served {} ms after connection 1 closed",
                began.elapsed().as_millis()
            ),
            other => return Err(format!("connection 9's hello was answered {other:?}").into()),
        }
        Ok(())
    }

    /// Four connections from callers that may not start are served; a fifth
    /// is closed at once, so other accounts can't hold every slot (ADR 0006
    /// rule 4). With `--ready-file`, the four stay open until the release
    /// file appears, so an administrator can show it still gets in.
    fn readonly_slots(options: &Options) -> Outcome {
        let mut held = Vec::new();
        for n in 1..=MAX_READ_ONLY_CONNECTIONS {
            let mut conn = Conn::open()?;
            if conn.hello()?.may_start {
                return Err(Failure::Usage(
                    "this account may start; run this as a standard account".into(),
                ));
            }
            println!("read-only connection {n}: served");
            held.push(conn);
        }
        let mut fifth = Conn::open()?;
        // The helper may already have closed it.
        if let Err(error) = fifth.send(&Request::hello()) {
            println!("note: {error}");
        }
        match fifth.next(Instant::now() + Duration::from_secs(5))? {
            Next::Eof => println!("ok: read-only connection 5 was closed unanswered"),
            Next::Message(message) => {
                return Err(format!("read-only connection 5 was answered: {message:?}").into())
            }
            Next::TimedOut => {
                return Err("read-only connection 5 was neither served nor closed"
                    .to_owned()
                    .into())
            }
        }
        if let Some(ready) = &options.ready_file {
            write_ready(ready, &[("held", held.len().to_string())])?;
            if let Some(release) = &options.release_file {
                wait_for_file(release, sleep_a_little)?;
            }
            // Still served: the slots were held, not lost.
            for conn in &mut held {
                conn.status()?;
            }
        }
        Ok(())
    }

    /// Requests in a stream sent without reading a reply: once the pipe is
    /// full, the helper's write misses its deadline and it drops the
    /// connection, rather than hold it (and its memory) for ever.
    fn write_deadline() -> Outcome {
        /// Twenty thousand `status` requests, 440 KB: far more than the
        /// pipe's buffers and the helper's read chunk hold together, so the
        /// helper stops reading long before the last.
        const FLOOD: usize = 20_000;
        let mut conn = Conn::open()?;
        conn.hello()?;
        let one = encode_request(&Request::Status, &conn.limits)
            .map_err(|error| format!("encoding status: {error}"))?;
        let flood = one.repeat(FLOOD);
        // The write below blocks until the helper either reads it all or
        // drops this client; neither within a minute is the failure.
        watchdog(
            Duration::from_secs(60),
            "service_smoke write-deadline: FAILED: a write to a client that stopped reading \
             still blocks after 60s: the helper neither reads nor drops it"
                .to_owned(),
        );
        let began = Instant::now();
        match conn.pipe.0.write_all(&flood) {
            Ok(()) => println!(
                "wrote {FLOOD} requests in {} ms",
                began.elapsed().as_millis()
            ),
            Err(error) => println!(
                "writing stopped after {} ms: {error}",
                began.elapsed().as_millis()
            ),
        }
        if let Some(left) = (began + WRITE_DEADLINE_WAIT).checked_duration_since(Instant::now()) {
            thread::sleep(left);
        }
        let mut replies = 0usize;
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match conn.next(deadline)? {
                Next::Message(ToClient::Reply(Reply::Status { .. })) => replies += 1,
                Next::Message(other) => {
                    return Err(format!("a flood of status requests got {other:?}").into())
                }
                Next::Eof => break,
                Next::TimedOut => {
                    return Err(format!(
                        "after {replies} replies the helper still holds a client that stopped \
                         reading"
                    )
                    .into())
                }
            }
        }
        if replies >= FLOOD {
            return Err(format!("all {FLOOD} requests were answered: nothing was dropped").into());
        }
        println!(
            "ok: dropped after {replies} of {FLOOD} replies, {} s after the flood began",
            began.elapsed().as_secs()
        );
        Ok(())
    }

    // ---- HelperState ----

    /// This account can't list `--dir` (HelperState) or open `--file` (the
    /// helper's log in it). Read from the error itself: `cmd /c dir` says
    /// "File Not Found" for a directory it may not list.
    fn denied(options: &Options) -> Outcome {
        let (Some(dir), Some(file)) = (&options.dir, &options.file) else {
            return Err(Failure::Usage("denied needs --dir and --file".into()));
        };
        expect_denied(&format!("listing {}", dir.display()), fs::read_dir(dir))?;
        expect_denied(&format!("opening {}", file.display()), File::open(file))?;
        Ok(())
    }

    fn expect_denied<T>(what: &str, result: io::Result<T>) -> Outcome {
        match result {
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                println!("ok: {what} is denied");
                Ok(())
            }
            Err(error) => Err(format!("{what} failed, but not as denied: {error}").into()),
            Ok(_) => Err(format!("{what} succeeded").into()),
        }
    }

    // ---- The pipe name ----

    /// `--expect denied`: this account can neither create the service's
    /// pipe name (only administrators create names under
    /// `ProtectedPrefix\Administrators`) nor add an instance to the
    /// service's pipe (its DACL grants users read and `FILE_WRITE_DATA`,
    /// never `FILE_CREATE_PIPE_INSTANCE`), with or without
    /// `FILE_FLAG_FIRST_PIPE_INSTANCE`.
    ///
    /// `--hold`: as an administrator, take the name while the service is
    /// stopped and hold it, so the service's start finds it squatted.
    fn squat(options: &Options) -> Outcome {
        if options.hold {
            let (Some(ready), Some(release)) = (&options.ready_file, &options.release_file) else {
                return Err(Failure::Usage(
                    "--hold needs --ready-file and --release-file".into(),
                ));
            };
            let _pipe = create_instance(true, PIPE_UNLIMITED_INSTANCES).map_err(|code| {
                format!(
                    "{PIPE_NAME} could not be created: {}; is the service still running, or \
                     is this not an administrator?",
                    describe(code)
                )
            })?;
            println!("holding {PIPE_NAME}");
            write_ready(ready, &[("held", "pipe".to_owned())])?;
            wait_for_file(release, sleep_a_little)?;
            return Ok(());
        }
        options.expect(&["denied"])?;
        if pipe_exists() {
            println!("the service's pipe exists: adding an instance");
        } else {
            println!("the service's pipe doesn't exist: creating the name");
        }
        for first in [false, true] {
            let how = if first {
                "with FILE_FLAG_FIRST_PIPE_INSTANCE"
            } else {
                "as one more instance"
            };
            match create_instance(first, HELPER_PIPE_INSTANCES) {
                Ok(pipe) => {
                    drop(pipe);
                    return Err(format!("this account created {PIPE_NAME} {how}").into());
                }
                Err(code) if code == ERROR_ACCESS_DENIED.0 => {
                    println!("ok: creating {PIPE_NAME} {how} is denied")
                }
                Err(code) => {
                    return Err(format!(
                        "creating {PIPE_NAME} {how} failed with {}, not access denied",
                        describe(code)
                    )
                    .into())
                }
            }
        }
        Ok(())
    }

    /// `--expect denied`: this account can't open the pipe for
    /// `GENERIC_WRITE`, which on a pipe includes `FILE_CREATE_PIPE_INSTANCE`;
    /// the GUI's access (held here meanwhile, which keeps the pipe there)
    /// is all it gets.
    fn generic_write(options: &Options) -> Outcome {
        options.expect(&["denied"])?;
        let mut held = Conn::open()?;
        if held.hello()?.may_start {
            return Err(Failure::Usage(
                "administrators get GENERIC_ALL on the pipe; run this as a standard account".into(),
            ));
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match Pipe::open(GENERIC_READ.0 | GENERIC_WRITE.0) {
                Ok(_) => {
                    return Err("this account opened the pipe for GENERIC_WRITE"
                        .to_owned()
                        .into())
                }
                Err(code) if code == ERROR_ACCESS_DENIED.0 => {
                    println!("ok: opening the pipe for GENERIC_WRITE is denied");
                    return Ok(());
                }
                Err(code) if code == ERROR_PIPE_BUSY.0 && Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(200))
                }
                Err(code) => {
                    return Err(format!(
                        "opening the pipe for GENERIC_WRITE failed with {}, not access denied",
                        describe(code)
                    )
                    .into())
                }
            }
        }
    }

    // ---- TUN ----

    /// How a `tun` run ends.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum End {
        /// The GUI's stop: `stop`, then `exited` and `stopped`.
        Stop,
        /// The connection closes between frames: a GUI that crashed.
        Close,
        /// The connection closes in the middle of a frame header.
        MidFrame,
        /// Something else kills the helper: the connection breaks.
        HelperKilled,
    }

    impl End {
        fn parse(text: Option<&str>) -> Outcome<Self> {
            match text.unwrap_or("stop") {
                "stop" => Ok(End::Stop),
                "close" => Ok(End::Close),
                "mid-frame" => Ok(End::MidFrame),
                "helper-killed" => Ok(End::HelperKilled),
                other => Err(Failure::Usage(format!("--end {other:?}"))),
            }
        }
    }

    /// A real TUN start as SYSTEM: `started`, sing-box's log saying its TUN
    /// inbound and then sing-box itself started, its API listening on
    /// loopback, and, with `--probes`, the loopback rule and ordinary
    /// traffic checked through the proxy and TUN. With `--ready-file` it
    /// says so (for checks from outside: the adapter, sing-box's parent, its
    /// listeners) and holds TUN up until the release file appears. Then it
    /// ends as `--end` says, and sing-box must be gone: the helper's status
    /// says stopped, and the machine's own route is back.
    fn tun(options: &Options) -> Outcome {
        let end = End::parse(options.end.as_deref())?;
        if end == End::HelperKilled
            && (options.ready_file.is_none() || options.release_file.is_some())
        {
            return Err(Failure::Usage(
                "--end helper-killed needs --ready-file and no --release-file".into(),
            ));
        }
        let proxy_port = free_port()?;
        let echo = if options.probes {
            Some(EchoServer::start()?)
        } else {
            None
        };
        // Before TUN is up, while the route to the internet is still the
        // machine's own: its address on its real network.
        let lan = lan_address();

        let mut conn = Conn::open()?;
        let helper_pid = conn.pipe.server_pid();
        if !conn.hello()?.may_start {
            return Err(Failure::Usage(
                "this account may not start; run this as an administrator".into(),
            ));
        }
        let start = tun_start(proxy_port)?;
        let began = Instant::now();
        conn.send(&Request::Start(start))?;
        let started: Started = match conn.reply(START_TIMEOUT)? {
            Reply::Started(started) => started,
            other => return Err(format!("the TUN start was answered {other:?}").into()),
        };
        println!(
            "started in {} ms: proxy 127.0.0.1:{proxy_port}, API 127.0.0.1:{}",
            began.elapsed().as_millis(),
            started.api_port
        );
        let secret = &started.api_secret;
        if secret.len() != 64 || !secret.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("the API secret is not 32 bytes of hex".to_owned().into());
        }
        conn.wait_for_line(
            "saying the TUN inbound started",
            |line| line.contains("inbound/tun") && line.contains("started"),
            TUN_UP_TIMEOUT,
        )?;
        conn.wait_for_line(
            "saying sing-box started",
            |line| line.contains("sing-box started"),
            TUN_UP_TIMEOUT,
        )?;
        println!(
            "ok: TUN up {} ms after the start",
            began.elapsed().as_millis()
        );
        let api = SocketAddr::from((Ipv4Addr::LOCALHOST, started.api_port));
        TcpStream::connect_timeout(&api, Duration::from_secs(5))
            .map_err(|error| format!("the helper's API at {api} doesn't accept: {error}"))?;
        println!("ok: the API listens at {api}");

        if let Some(echo) = &echo {
            let proxy = SocketAddr::from((Ipv4Addr::LOCALHOST, proxy_port));
            pump_while(&mut conn, || probes(proxy, echo, lan))??;
        }
        if let Some(ready) = &options.ready_file {
            write_ready(
                ready,
                &[
                    ("proxy_port", proxy_port.to_string()),
                    ("api_port", started.api_port.to_string()),
                    ("tun_address", tun_address().to_string()),
                    (
                        "tun_prefix",
                        boxpilot_runconfig::TUN_IPV4_ADDRESS.to_owned(),
                    ),
                    (
                        "helper_pid",
                        helper_pid.map_or("0".into(), |pid| pid.to_string()),
                    ),
                ],
            )?;
            if let Some(release) = &options.release_file {
                wait_for_file(release, || {
                    conn.expect_running(Instant::now() + Duration::from_millis(100))
                })?;
            }
        }

        match end {
            End::Stop => stop(&mut conn)?,
            End::Close => {
                println!("closing the connection");
                drop(conn);
            }
            End::MidFrame => {
                println!("closing the connection after 3 bytes of a frame header");
                conn.pipe
                    .0
                    .write_all(&[0, 0, 0])
                    .map_err(|error| format!("writing half a header: {error}"))?;
                drop(conn);
            }
            End::HelperKilled => {
                conn.wait_eof(Duration::from_secs(60))?;
                println!("ok: the connection ended with the helper");
                return network_after_tun().map_err(Failure::from);
            }
        }
        wait_stopped()?;
        network_after_tun()?;
        Ok(())
    }

    /// The GUI's stop: `stop` is answered `stopped` once sing-box has
    /// exited, and sing-box's `exited` goes to this connection, which
    /// started it. The helper queues what a request sets off after its
    /// reply (`outbox.rs`), so `exited` comes after `stopped`; the order
    /// isn't what is checked here, only that both arrive.
    fn stop(conn: &mut Conn) -> Result<(), String> {
        println!("stopping");
        conn.send(&Request::Stop)?;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match conn.next(deadline)? {
                Next::Message(ToClient::Event(_)) => {}
                Next::Message(ToClient::Reply(Reply::Stopped)) => break,
                Next::Message(ToClient::Reply(other)) => {
                    return Err(format!("stop was answered {other:?}"))
                }
                Next::Eof => return Err("the helper closed the connection on stop".into()),
                Next::TimedOut => return Err("no stopped within 30s".into()),
            }
        }
        let exited_first = conn.exited.is_some();
        let deadline = Instant::now() + Duration::from_secs(5);
        while conn.exited.is_none() {
            match conn.next(deadline)? {
                Next::Message(ToClient::Event(_)) => {}
                Next::Message(ToClient::Reply(other)) => {
                    return Err(format!("after stopped, an unasked reply {other:?}"))
                }
                Next::Eof => return Err("the connection ended without sing-box's exited".into()),
                Next::TimedOut => return Err("stopped came, but sing-box's exited didn't".into()),
            }
        }
        let (state, last_exit) = conn.status()?;
        if state != RunState::Stopped || last_exit.is_none() {
            return Err(format!("after stop: {state:?}, last exit {last_exit:?}"));
        }
        println!(
            "ok: stopped, sing-box's exited {} it, last exit {last_exit:?}",
            if exited_first { "before" } else { "after" }
        );
        Ok(())
    }

    /// On a new connection, the helper reports no sing-box running within a
    /// few seconds: the one this run started stopped with its connection.
    fn wait_stopped() -> Result<(), String> {
        let mut conn = Conn::open()?;
        conn.hello()?;
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let (state, last_exit) = conn.status()?;
            if state == RunState::Stopped {
                println!("ok: the helper reports sing-box stopped (last exit {last_exit:?})");
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "sing-box still {state:?} 15s after its connection ended"
                ));
            }
            thread::sleep(Duration::from_millis(250));
        }
    }

    /// Once TUN is down, the machine's own route carries traffic again: a
    /// connection out doesn't start from the TUN interface's address.
    fn network_after_tun() -> Result<(), String> {
        let tun = IpAddr::V4(tun_address());
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let seen =
                match TcpStream::connect_timeout(&PUBLIC_ADDRESS.into(), Duration::from_secs(5)) {
                    Ok(stream) => match stream.local_addr() {
                        Ok(local) if local.ip() != tun => {
                            println!("ok: after TUN, {PUBLIC_ADDRESS} is reached from {local}");
                            return Ok(());
                        }
                        Ok(local) => format!("still from the TUN address {local}"),
                        Err(error) => error.to_string(),
                    },
                    Err(error) => error.to_string(),
                };
            if Instant::now() >= deadline {
                return Err(format!(
                    "after TUN, no direct connection to {PUBLIC_ADDRESS}: {seen}"
                ));
            }
            thread::sleep(Duration::from_millis(500));
        }
    }

    /// Run `work` on a thread of its own while this one reads the
    /// connection, so sing-box's lines keep flowing (a client that stops
    /// reading is dropped, and its sing-box with it) and an early exit is
    /// seen.
    fn pump_while<T: Send>(conn: &mut Conn, work: impl FnOnce() -> T + Send) -> Result<T, String> {
        thread::scope(|scope| {
            let worker = scope.spawn(work);
            while !worker.is_finished() {
                conn.expect_running(Instant::now() + Duration::from_millis(50))?;
            }
            worker.join().map_err(|_| "the probes panicked".to_owned())
        })
    }

    // ---- Probes through the proxy and TUN ----

    /// This machine's address on its real network, if it has one: where it
    /// sends from towards the internet (no packet is sent).
    fn lan_address() -> Option<Ipv4Addr> {
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
        socket.connect(PUBLIC_ADDRESS).ok()?;
        match socket.local_addr().ok()?.ip() {
            IpAddr::V4(address) if !address.is_loopback() && !address.is_unspecified() => {
                Some(address)
            }
            _ => None,
        }
    }

    /// A TCP echo service on every IPv4 address (loopback and this
    /// machine's own) and, if it can, on IPv6 loopback, counting what
    /// reaches it: a probe through the proxy must never add to the count.
    struct EchoServer {
        port: u16,
        v6_port: Option<u16>,
        accepted: Arc<AtomicUsize>,
    }

    impl EchoServer {
        fn start() -> Result<Self, String> {
            let accepted = Arc::new(AtomicUsize::new(0));
            let v4 = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0))
                .map_err(|error| format!("the echo listener: {error}"))?;
            let port = v4.local_addr().map_err(|error| error.to_string())?.port();
            Self::serve(v4, accepted.clone());
            let v6_port = match TcpListener::bind((Ipv6Addr::LOCALHOST, 0)) {
                Ok(v6) => {
                    let port = v6.local_addr().map_err(|error| error.to_string())?.port();
                    Self::serve(v6, accepted.clone());
                    Some(port)
                }
                Err(error) => {
                    println!("note: no IPv6 loopback listener ({error}); ::1 isn't probed");
                    None
                }
            };
            Ok(Self {
                port,
                v6_port,
                accepted,
            })
        }

        fn serve(listener: TcpListener, accepted: Arc<AtomicUsize>) {
            thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    accepted.fetch_add(1, Ordering::SeqCst);
                    thread::spawn(move || {
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                        let mut buf = [0u8; 64];
                        if let Ok(n) = stream.read(&mut buf) {
                            let _ = stream.write_all(&buf[..n]);
                        }
                    });
                }
            });
        }

        fn accepted(&self) -> usize {
            self.accepted.load(Ordering::SeqCst)
        }
    }

    /// Where a probe asks the proxy to connect.
    #[derive(Debug, Clone)]
    enum Dest {
        Ip(IpAddr),
        Name(&'static str),
    }

    impl fmt::Display for Dest {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Dest::Ip(IpAddr::V6(address)) => write!(f, "[{address}]"),
                Dest::Ip(address) => write!(f, "{address}"),
                Dest::Name(name) => f.write_str(name),
            }
        }
    }

    /// Which of the mixed inbound's protocols a probe speaks.
    #[derive(Debug, Clone, Copy)]
    enum Via {
        Socks5,
        HttpConnect,
    }

    impl fmt::Display for Via {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(match self {
                Via::Socks5 => "SOCKS5",
                Via::HttpConnect => "HTTP CONNECT",
            })
        }
    }

    /// What a probe saw.
    #[derive(Debug)]
    enum Probe {
        /// The proxy connected (and, if asked, the echo came back).
        Reached,
        /// The proxy refused or closed: how.
        Refused(String),
    }

    impl Via {
        /// Ask the proxy at `proxy` for `dest:port`; with `echo`, also send
        /// a few bytes and expect them back.
        fn probe(
            self,
            proxy: SocketAddr,
            dest: &Dest,
            port: u16,
            echo: bool,
        ) -> Result<Probe, String> {
            let mut stream = TcpStream::connect_timeout(&proxy, Duration::from_secs(5))
                .map_err(|error| format!("the proxy at {proxy}: {error}"))?;
            let timeout = Some(Duration::from_secs(10));
            stream
                .set_read_timeout(timeout)
                .and_then(|()| stream.set_write_timeout(timeout))
                .map_err(|error| error.to_string())?;
            let connected = match self {
                Via::Socks5 => socks5_connect(&mut stream, dest, port)?,
                Via::HttpConnect => http_connect(&mut stream, dest, port)?,
            };
            match connected {
                Probe::Reached if echo => Ok(exchange(&mut stream)),
                other => Ok(other),
            }
        }
    }

    fn socks5_connect(stream: &mut TcpStream, dest: &Dest, port: u16) -> Result<Probe, String> {
        let io = |error: io::Error| format!("SOCKS5 handshake: {error}");
        stream.write_all(&[5, 1, 0]).map_err(io)?;
        let mut choice = [0u8; 2];
        stream.read_exact(&mut choice).map_err(io)?;
        if choice != [5, 0] {
            return Err(format!("the proxy chose SOCKS5 method {choice:?}"));
        }
        let mut request = vec![5, 1, 0];
        match dest {
            Dest::Ip(IpAddr::V4(address)) => {
                request.push(1);
                request.extend(address.octets());
            }
            Dest::Ip(IpAddr::V6(address)) => {
                request.push(4);
                request.extend(address.octets());
            }
            Dest::Name(name) => {
                request.push(3);
                request.push(name.len() as u8);
                request.extend(name.as_bytes());
            }
        }
        request.extend(port.to_be_bytes());
        stream.write_all(&request).map_err(io)?;
        let mut head = [0u8; 4];
        if let Err(error) = stream.read_exact(&mut head) {
            return Ok(Probe::Refused(format!("closed without a reply ({error})")));
        }
        if head[1] != 0 {
            return Ok(Probe::Refused(format!("SOCKS5 reply {}", head[1])));
        }
        let bound = match head[3] {
            1 => 4 + 2,
            4 => 16 + 2,
            3 => {
                let mut len = [0u8; 1];
                stream.read_exact(&mut len).map_err(io)?;
                usize::from(len[0]) + 2
            }
            other => return Err(format!("SOCKS5 reply with address type {other}")),
        };
        let mut rest = vec![0u8; bound];
        stream.read_exact(&mut rest).map_err(io)?;
        Ok(Probe::Reached)
    }

    fn http_connect(stream: &mut TcpStream, dest: &Dest, port: u16) -> Result<Probe, String> {
        let authority = format!("{dest}:{port}");
        let request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n");
        stream
            .write_all(request.as_bytes())
            .map_err(|error| format!("HTTP CONNECT: {error}"))?;
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            match stream.read(&mut byte) {
                Ok(1) if head.len() < 4096 => head.push(byte[0]),
                Ok(1) => return Err("an HTTP CONNECT response head over 4 KiB".into()),
                Ok(_) => return Ok(Probe::Refused("closed without a response".into())),
                Err(error) => return Ok(Probe::Refused(format!("no response ({error})"))),
            }
        }
        let status = String::from_utf8_lossy(&head);
        let status = status.lines().next().unwrap_or_default().to_owned();
        if status.split_whitespace().nth(1) == Some("200") {
            Ok(Probe::Reached)
        } else {
            Ok(Probe::Refused(status))
        }
    }

    /// Send a few bytes and expect them back.
    fn exchange(stream: &mut TcpStream) -> Probe {
        const PING: &[u8] = b"boxpilot-smoke";
        if let Err(error) = stream.write_all(PING) {
            return Probe::Refused(format!("closed after connecting ({error})"));
        }
        let mut back = [0u8; PING.len()];
        match stream.read_exact(&mut back) {
            Ok(()) if back == PING => Probe::Reached,
            Ok(()) => Probe::Refused(format!("a garbled echo {back:?}")),
            Err(error) => Probe::Refused(format!("no echo ({error})")),
        }
    }

    /// The loopback rule (ADR 0006, "Loopback, as hygiene") from outside:
    /// through the local proxy, no connection to `127.0.0.1`, `::1` or a
    /// `localhost` name reaches the echo service listening there. Then the
    /// controls that make those refusals mean something: the proxy and TUN
    /// carry ordinary traffic, by address and by name.
    ///
    /// The IP probes are the rule's alone: without it sing-box would dial
    /// the listener. A name might also fail to resolve upstream, so for
    /// names the check is the property itself: nothing reached loopback.
    fn probes(proxy: SocketAddr, echo: &EchoServer, lan: Option<Ipv4Addr>) -> Result<(), String> {
        let mut loopback = vec![
            (Via::Socks5, Dest::Ip(Ipv4Addr::LOCALHOST.into()), echo.port),
            (
                Via::Socks5,
                Dest::Ip(Ipv4Addr::new(127, 1, 2, 3).into()),
                echo.port,
            ),
            (Via::Socks5, Dest::Name("localhost"), echo.port),
            (
                Via::Socks5,
                Dest::Name("boxpilot-smoke.localhost"),
                echo.port,
            ),
            (
                Via::HttpConnect,
                Dest::Ip(Ipv4Addr::LOCALHOST.into()),
                echo.port,
            ),
            (Via::HttpConnect, Dest::Name("localhost"), echo.port),
        ];
        if let Some(v6_port) = echo.v6_port {
            loopback.push((Via::Socks5, Dest::Ip(Ipv6Addr::LOCALHOST.into()), v6_port));
        }
        for (via, dest, port) in &loopback {
            let before = echo.accepted();
            let probe = via.probe(proxy, dest, *port, true)?;
            // A connection the proxy opened would be counted by now.
            thread::sleep(Duration::from_millis(300));
            let reached = echo.accepted() - before;
            match probe {
                Probe::Refused(how) if reached == 0 => {
                    println!("ok: {via} to {dest}:{port} refused ({how}), nothing reached loopback")
                }
                Probe::Refused(how) => {
                    return Err(format!(
                        "{via} to {dest}:{port} was refused ({how}), but the loopback listener \
                         got {reached} connection(s)"
                    ))
                }
                Probe::Reached => {
                    return Err(format!(
                        "{via} to {dest}:{port} reached the loopback listener: the helper's \
                         loopback rule did not hold"
                    ))
                }
            }
        }

        // This machine's own address isn't loopback, so the proxy carries
        // it. Only a note if not: sing-box's outbound is bound to the real
        // interface, and Windows may not loop that back to its own address.
        match lan {
            Some(address) => {
                let before = echo.accepted();
                let probe = Via::Socks5.probe(proxy, &Dest::Ip(address.into()), echo.port, true);
                match probe {
                    Ok(Probe::Reached) if echo.accepted() > before => {
                        println!(
                            "ok: SOCKS5 to this machine's {address}:{} reached the listener",
                            echo.port
                        )
                    }
                    other => println!(
                        "note: SOCKS5 to this machine's {address}:{} didn't reach the listener \
                         ({other:?})",
                        echo.port
                    ),
                }
            }
            None => println!("note: no LAN address found; the proxy isn't probed with one"),
        }

        let public = Dest::Ip(IpAddr::V4(*PUBLIC_ADDRESS.ip()));
        expect_reached(Via::Socks5, proxy, &public, PUBLIC_ADDRESS.port())?;
        expect_reached(Via::Socks5, proxy, &Dest::Name(PUBLIC_NAME), 443)?;
        expect_reached(Via::HttpConnect, proxy, &Dest::Name(PUBLIC_NAME), 443)?;

        // Through TUN: auto_route sends this machine's own connections into
        // the tunnel, so one to the internet starts from the TUN address.
        let tun = IpAddr::V4(tun_address());
        let stream = TcpStream::connect_timeout(&PUBLIC_ADDRESS.into(), Duration::from_secs(10))
            .map_err(|error| format!("through TUN, {PUBLIC_ADDRESS}: {error}"))?;
        let local = stream.local_addr().map_err(|error| error.to_string())?;
        if local.ip() != tun {
            return Err(format!(
                "a connection to {PUBLIC_ADDRESS} started from {local}, not the TUN address {tun}: \
                 TUN doesn't carry this machine's traffic"
            ));
        }
        println!("ok: through TUN, {PUBLIC_ADDRESS} is reached from {local}");
        // DNS through TUN: the system resolver points at the tunnel, and the
        // profile hijacks DNS to its own resolver.
        let resolved: Vec<SocketAddr> = (PUBLIC_NAME, 443)
            .to_socket_addrs()
            .map_err(|error| format!("through TUN, resolving {PUBLIC_NAME}: {error}"))?
            .collect();
        if resolved.is_empty() {
            return Err(format!("through TUN, {PUBLIC_NAME} resolved to nothing"));
        }
        println!("ok: through TUN, {PUBLIC_NAME} resolves to {resolved:?}");
        Ok(())
    }

    /// A probe that must reach its destination: tried three times, a second
    /// apart, so one slow answer from the internet isn't a failure.
    fn expect_reached(via: Via, proxy: SocketAddr, dest: &Dest, port: u16) -> Result<(), String> {
        let mut last = String::new();
        for _ in 0..3 {
            match via.probe(proxy, dest, port, false) {
                Ok(Probe::Reached) => {
                    println!("ok: {via} to {dest}:{port} connected");
                    return Ok(());
                }
                Ok(Probe::Refused(how)) => last = how,
                Err(error) => last = error,
            }
            thread::sleep(Duration::from_secs(1));
        }
        Err(format!(
            "{via} to {dest}:{port} failed ({last}): the proxy doesn't carry ordinary traffic, \
             so the refusals above prove nothing"
        ))
    }

    // ---- Tokens, read from outside ----

    /// What a process's token says.
    struct ProcessToken {
        user: String,
        /// Each privilege's name and attributes.
        privileges: Vec<(String, u32)>,
        /// The integrity level's RID.
        integrity: u32,
        /// Each group's SID and attributes.
        groups: Vec<(String, u32)>,
    }

    /// `SE_GROUP_ENABLED` and `SE_GROUP_USE_FOR_DENY_ONLY`.
    const SE_GROUP_ENABLED: u32 = 0x4;
    const SE_GROUP_USE_FOR_DENY_ONLY: u32 = 0x10;
    const ADMINISTRATORS: &str = "S-1-5-32-544";
    const SYSTEM: &str = "S-1-5-18";

    /// `--pid`'s token, read as this account: `--expect sing-box` checks it
    /// against `spawnplan::SING_BOX_TOKEN`, `--expect privileges` against
    /// `--privileges`, `--expect print` only prints it.
    fn token(options: &Options) -> Outcome {
        let expect = options.expect(&["sing-box", "privileges", "print"])?;
        let Some(pid) = options.pid else {
            return Err(Failure::Usage("token needs --pid".into()));
        };
        let wanted: Option<Vec<String>> = match (expect, &options.privileges) {
            ("privileges", Some(list)) => Some(
                list.split(',')
                    .map(|name| name.trim().to_owned())
                    .filter(|name| !name.is_empty())
                    .collect(),
            ),
            ("privileges", None) => {
                return Err(Failure::Usage(
                    "--expect privileges needs --privileges".into(),
                ))
            }
            _ => None,
        };
        let token = read_process_token(pid).map_err(Failure::Unreadable)?;
        print_process_token(pid, &token);
        match expect {
            "sing-box" => expect_sing_box_token(&token)?,
            "privileges" => {
                let wanted = wanted.unwrap_or_default();
                expect_privileges(&token, &wanted)?;
                println!("ok: pid {pid} holds exactly {}", wanted.join(", "));
            }
            _ => {}
        }
        Ok(())
    }

    fn print_process_token(pid: u32, token: &ProcessToken) {
        println!(
            "pid {pid}: user {}, integrity 0x{:x}, {} privileges, {} groups",
            token.user,
            token.integrity,
            token.privileges.len(),
            token.groups.len()
        );
        for (name, attributes) in &token.privileges {
            println!("  privilege {name:<44} 0x{attributes:x}");
        }
        for (group, attributes) in &token.groups {
            println!("  group     {group:<60} 0x{attributes:08x}");
        }
    }

    fn privilege_set(names: impl IntoIterator<Item = impl AsRef<str>>) -> BTreeSet<String> {
        names
            .into_iter()
            .map(|name| name.as_ref().to_ascii_lowercase())
            .collect()
    }

    /// The token holds exactly `wanted`, compared as Windows compares
    /// privilege names.
    fn expect_privileges(token: &ProcessToken, wanted: &[impl AsRef<str>]) -> Result<(), String> {
        let have = privilege_set(token.privileges.iter().map(|(name, _)| name));
        let want = privilege_set(wanted);
        if have != want {
            return Err(format!(
                "the token holds privileges {:?} it shouldn't and lacks {:?}",
                have.difference(&want).collect::<Vec<_>>(),
                want.difference(&have).collect::<Vec<_>>()
            ));
        }
        Ok(())
    }

    /// sing-box's token is the helper's as `spawnplan::SING_BOX_TOKEN`
    /// restricts it: SYSTEM's, exactly the planned privileges (SYSTEM holds
    /// them all, so none is missing either), the planned integrity level
    /// (the cap, or the helper's System), and Administrators deny-only
    /// exactly when the plan says.
    fn expect_sing_box_token(token: &ProcessToken) -> Result<(), String> {
        let plan = SING_BOX_TOKEN;
        if token.user != SYSTEM {
            return Err(format!("sing-box runs as {}, not SYSTEM", token.user));
        }
        expect_privileges(token, plan.privileges)
            .map_err(|error| format!("sing-box's token: {error}"))?;
        let level = plan.max_integrity.unwrap_or(integrity::SYSTEM);
        if token.integrity != level {
            return Err(format!(
                "sing-box runs at integrity level 0x{:x}, its plan says 0x{level:x}",
                token.integrity
            ));
        }
        let deny_only = plan
            .deny_only
            .iter()
            .any(|group| group.eq_ignore_ascii_case(ADMINISTRATORS));
        let administrators = token
            .groups
            .iter()
            .find(|(group, _)| group.eq_ignore_ascii_case(ADMINISTRATORS))
            .map(|(_, attributes)| *attributes)
            .ok_or("sing-box's token has no Administrators group")?;
        let is_deny_only = administrators & SE_GROUP_USE_FOR_DENY_ONLY != 0
            && administrators & SE_GROUP_ENABLED == 0;
        if is_deny_only != deny_only {
            return Err(format!(
                "sing-box's Administrators group has attributes 0x{administrators:x}; its plan \
                 says {}",
                if deny_only { "deny-only" } else { "enabled" }
            ));
        }
        println!(
            "ok: sing-box holds exactly {}, at integrity 0x{level:x}, Administrators {}",
            plan.privileges.join(", "),
            if deny_only { "deny-only" } else { "enabled" }
        );
        Ok(())
    }

    /// Open `pid` for `PROCESS_QUERY_LIMITED_INFORMATION`.
    fn open_process(pid: u32) -> Result<OwnedHandle, u32> {
        // SAFETY: no handle is passed in; the one returned is owned below.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
            .map_err(|error| win32_code(&error))?;
        // SAFETY: a valid handle, just opened, owned by nobody else.
        Ok(unsafe { OwnedHandle::from_raw_handle(handle.0) })
    }

    /// Enable this process's SeDebugPrivilege, which an elevated
    /// administrator holds disabled: it opens any process, not any token.
    fn enable_debug_privilege() -> Result<(), String> {
        let mut own = HANDLE::default();
        // SAFETY: the current process's pseudo-handle; `own` receives a new
        // handle, owned below.
        unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
                &mut own,
            )
        }
        .map_err(|error| format!("this process's token: {error}"))?;
        // SAFETY: just opened, owned by nobody else.
        let own = unsafe { OwnedHandle::from_raw_handle(own.0) };
        let mut luid = LUID::default();
        let name = wide("SeDebugPrivilege");
        // SAFETY: a NUL-terminated name and a valid out-pointer.
        unsafe { LookupPrivilegeValueW(PCWSTR::null(), PCWSTR(name.as_ptr()), &mut luid) }
            .map_err(|error| format!("SeDebugPrivilege: {error}"))?;
        let privileges = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        // SAFETY: the token is open with TOKEN_ADJUST_PRIVILEGES; `privileges`
        // is a whole TOKEN_PRIVILEGES with one entry; no previous state.
        unsafe {
            AdjustTokenPrivileges(
                HANDLE(own.as_raw_handle()),
                false,
                Some(&privileges),
                0,
                None,
                None,
            )
        }
        .map_err(|error| format!("enabling SeDebugPrivilege: {error}"))?;
        // AdjustTokenPrivileges succeeds without the privilege, saying so
        // only in the last error.
        if io::Error::last_os_error().raw_os_error() == Some(1300) {
            return Err("this account doesn't hold SeDebugPrivilege".into());
        }
        Ok(())
    }

    /// One class of `token`'s information, 8-aligned, with its length.
    fn token_information(
        token: HANDLE,
        class: TOKEN_INFORMATION_CLASS,
    ) -> Result<(Vec<u64>, usize), String> {
        let mut needed = 0u32;
        // SAFETY: a size query with no buffer; it fails by design and sets
        // `needed`.
        let _ = unsafe { GetTokenInformation(token, class, None, 0, &mut needed) };
        if needed == 0 {
            return Err(format!(
                "GetTokenInformation({}): {}",
                class.0,
                io::Error::last_os_error()
            ));
        }
        let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
        let capacity = (buf.len() * 8) as u32;
        // SAFETY: `buf` holds `capacity` >= `needed` writable bytes.
        unsafe {
            GetTokenInformation(
                token,
                class,
                Some(buf.as_mut_ptr().cast()),
                capacity,
                &mut needed,
            )
        }
        .map_err(|error| format!("GetTokenInformation({}): {error}", class.0))?;
        Ok((buf, needed as usize))
    }

    /// A SID's string form. `sid` must point at a valid SID.
    fn sid_string(sid: PSID) -> Result<String, String> {
        let mut text = PWSTR::null();
        // SAFETY: the caller passes a valid SID; on success `text` is a
        // LocalAlloc'd string, freed below.
        unsafe { ConvertSidToStringSidW(sid, &mut text) }
            .map_err(|error| format!("ConvertSidToStringSidW: {error}"))?;
        // SAFETY: `text` is the NUL-terminated string just returned.
        let string = unsafe { text.to_string() };
        // SAFETY: allocated by ConvertSidToStringSidW, freed once.
        unsafe { LocalFree(HLOCAL(text.0.cast())) };
        string.map_err(|error| format!("a SID's text: {error}"))
    }

    /// A privilege's name, or its LUID if it has none.
    fn privilege_name(luid: &LUID) -> String {
        let mut buf = vec![0u16; 128];
        let mut len = buf.len() as u32;
        // SAFETY: `luid` is valid; `buf` holds `len` UTF-16 units.
        match unsafe {
            LookupPrivilegeNameW(PCWSTR::null(), luid, PWSTR(buf.as_mut_ptr()), &mut len)
        } {
            Ok(()) => String::from_utf16_lossy(&buf[..(len as usize).min(buf.len())]),
            Err(error) if WIN32_ERROR::from_error(&error) == Some(ERROR_INSUFFICIENT_BUFFER) => {
                format!("#{:x}:{:x} (a long name)", luid.HighPart, luid.LowPart)
            }
            Err(_) => format!("#{:x}:{:x}", luid.HighPart, luid.LowPart),
        }
    }

    /// The `count` entries of type `T` at `offset` in `buf` (`len` bytes
    /// written), checked to lie within them.
    fn entries<T>(buf: &[u64], len: usize, offset: usize, count: usize) -> Result<&[T], String> {
        let end = count
            .checked_mul(std::mem::size_of::<T>())
            .and_then(|bytes| bytes.checked_add(offset));
        if end.is_none_or(|end| end > len) {
            return Err("token information overruns its buffer".into());
        }
        // SAFETY: the `count` entries lie within the `len` bytes written, as
        // just checked, and `buf` is 8-aligned, enough for the TOKEN_*
        // entry types.
        Ok(unsafe {
            std::slice::from_raw_parts((buf.as_ptr() as *const u8).add(offset) as *const T, count)
        })
    }

    /// The token of `pid`, read as this account: the process opened for
    /// `PROCESS_QUERY_LIMITED_INFORMATION` (with SeDebugPrivilege enabled if
    /// that is denied), its token for `TOKEN_QUERY`.
    fn read_process_token(pid: u32) -> Result<ProcessToken, String> {
        let process = match open_process(pid) {
            Ok(process) => process,
            Err(code) if code == ERROR_ACCESS_DENIED.0 => {
                println!(
                    "note: OpenProcess({pid}, PROCESS_QUERY_LIMITED_INFORMATION) is denied to \
                     this administrator; again with SeDebugPrivilege enabled"
                );
                enable_debug_privilege()?;
                open_process(pid).map_err(|code| {
                    format!(
                        "OpenProcess({pid}) even with SeDebugPrivilege: {}",
                        describe(code)
                    )
                })?
            }
            Err(code) => return Err(format!("OpenProcess({pid}): {}", describe(code))),
        };
        let mut token = HANDLE::default();
        // SAFETY: `process` is open for the call; `token` receives a new
        // handle, owned below.
        unsafe { OpenProcessToken(HANDLE(process.as_raw_handle()), TOKEN_QUERY, &mut token) }
            .map_err(|error| {
                format!(
                    "OpenProcessToken({pid}, TOKEN_QUERY) as this administrator: {}",
                    describe(win32_code(&error))
                )
            })?;
        // SAFETY: just opened, owned by nobody else.
        let token = unsafe { OwnedHandle::from_raw_handle(token.0) };
        let token = HANDLE(token.as_raw_handle());

        let (buf, len) = token_information(token, TokenUser)?;
        if len < std::mem::size_of::<TOKEN_USER>() {
            return Err("short TokenUser".into());
        }
        // SAFETY: GetTokenInformation wrote a TOKEN_USER at the start of
        // `buf`, 8-aligned; its SID points into `buf`.
        let user = sid_string(unsafe { (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid })?;

        let (buf, len) = token_information(token, TokenPrivileges)?;
        if len < 4 {
            return Err("short TokenPrivileges".into());
        }
        // SAFETY: a TOKEN_PRIVILEGES starts `buf`; its count is within it.
        let count = unsafe { (*(buf.as_ptr() as *const TOKEN_PRIVILEGES)).PrivilegeCount } as usize;
        let privileges = entries::<LUID_AND_ATTRIBUTES>(
            &buf,
            len,
            std::mem::offset_of!(TOKEN_PRIVILEGES, Privileges),
            count,
        )?
        .iter()
        .map(|entry| (privilege_name(&entry.Luid), entry.Attributes.0))
        .collect();

        let (buf, len) = token_information(token, TokenIntegrityLevel)?;
        if len < std::mem::size_of::<TOKEN_MANDATORY_LABEL>() {
            return Err("short TokenIntegrityLevel".into());
        }
        // SAFETY: a TOKEN_MANDATORY_LABEL starts `buf`; its SID points into
        // `buf`.
        let label =
            sid_string(unsafe { (*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL)).Label.Sid })?;
        let integrity = label
            .rsplit('-')
            .next()
            .and_then(|rid| rid.parse().ok())
            .ok_or_else(|| format!("an integrity label {label}"))?;

        let (buf, len) = token_information(token, TokenGroups)?;
        if len < 4 {
            return Err("short TokenGroups".into());
        }
        // SAFETY: a TOKEN_GROUPS starts `buf`; its count is within it.
        let count = unsafe { (*(buf.as_ptr() as *const TOKEN_GROUPS)).GroupCount } as usize;
        let groups = entries::<SID_AND_ATTRIBUTES>(
            &buf,
            len,
            std::mem::offset_of!(TOKEN_GROUPS, Groups),
            count,
        )?
        .iter()
        .map(|entry| Ok((sid_string(entry.Sid)?, entry.Attributes)))
        .collect::<Result<Vec<_>, String>>()?;

        Ok(ProcessToken {
            user,
            privileges,
            integrity,
            groups,
        })
    }
}
