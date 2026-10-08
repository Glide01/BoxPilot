//! `mac_smoke`: the installed macOS helper, driven from the outside on a
//! real Mac (ADR 0006, "Verification before shipping";
//! `docs/helper-macos-checklist.md`).
//!
//! The helper was built and unit-tested on Linux. This checks what only
//! macOS can say: that launchd's socket lets every account in and the uid
//! decides what each may do, that a TUN start really brings `utun` up as
//! root and takes it down with its connection, and that the helper holds
//! its connection limits and deadlines. CI runs it on GitHub's Apple-silicon
//! runner through `packaging/macos/helper-smoke.sh`, after installing the
//! helper from the DMG it built, as the runner's account (the owner), as
//! root, and as a fresh standard account. It is a test tool: nothing
//! installs it.
//!
//! It talks to the helper as the GUI will: a Unix socket at
//! `endpoint::macos::SOCKET_PATH`, requests built with the protocol crate's
//! public API, and a start checked first with the policy the GUI runs.
//! Never through the helper's own modules: they are what is under test.
//!
//! Each command checks one set of expectations. The exit code is 0 when
//! they held, 1 when one didn't (and what was seen is printed), 2 for a bad
//! command line.

#[cfg(not(unix))]
fn main() {
    eprintln!("mac_smoke drives the macOS helper; it runs on macOS only");
    std::process::exit(boxpilot_protocol::endpoint::exit::UNSUPPORTED_OS);
}

#[cfg(unix)]
fn main() {
    std::process::exit(smoke::main());
}

#[cfg(unix)]
mod smoke {
    use boxpilot_protocol::endpoint::macos::SOCKET_PATH;
    use boxpilot_protocol::{
        decode_to_client, encode_request, ErrorCode, Event, ExitInfo, FrameDecoder, HelloReply,
        Limits, RefusalCode, Reply, Request, RunState, StartRequest, Started, ToClient, TunOptions,
        WireRefusal, PROTOCOL_VERSION,
    };
    use serde_json::json;
    use std::fmt;
    use std::fs;
    use std::io::{self, Read, Write};
    use std::net::{
        IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream,
        ToSocketAddrs, UdpSocket,
    };
    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    const USAGE: &str = "\
usage: mac_smoke <command> [options] [--socket <path>]

as the owner (or root):
  hello --expect start [--sha256 <hex>] [--sing-box-version <version>]
                   [--ready-file <file>]
  refused          a start the policy refuses comes back `refused`, naming the fields
  slots            8 connections are served, a 9th waits until one ends
  write-deadline   a client that stops reading is dropped
  tun [--probes] [--system-proxy] [--ready-file <file> [--release-file <file>]]
      [--end stop|close|mid-frame|helper-killed]
                   a real TUN start: up, (probed,) then down with its connection

as another account:
  hello --expect readonly
  unauthorized     start and stop are refused as unauthorized; status is not
  readonly-slots [--ready-file <file> --release-file <file>]
                   4 read-only connections are served, a 5th is closed
  denied --dir <dir> --file <file>
                   this account can neither list <dir> nor open <file>

--ready-file: written (key=value lines) once the expectations so far held;
--release-file: then wait for it to appear (at most 3 minutes) before ending;
--socket: the helper's socket, if not the installed one's.";

    const HELD: i32 = 0;
    const FAILED: i32 = 1;
    const USAGE_ERROR: i32 = 2;

    /// How long the socket may take to appear, and to be answered: launchd
    /// starts the helper for the first client, and holds back a job that
    /// exited at once (a broken install, a moment ago) for up to 10 s.
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
    /// The first reply on a connection: launchd may be starting the helper.
    const FIRST_REPLY_TIMEOUT: Duration = Duration::from_secs(30);
    /// Any later reply.
    const REPLY_TIMEOUT: Duration = Duration::from_secs(10);
    /// A whole `start`, as the GUI allows.
    const START_TIMEOUT: Duration = Duration::from_secs(60);
    /// From `started` to sing-box's log saying TUN is up.
    const TUN_UP_TIMEOUT: Duration = Duration::from_secs(45);
    /// The longest wait for a `--release-file`.
    const HOLD_LIMIT: Duration = Duration::from_secs(180);
    /// The helper drops a client that stops reading after 10 s (its write
    /// deadline); this waits that out with room.
    const WRITE_DEADLINE_WAIT: Duration = Duration::from_secs(15);
    /// The longest any command runs.
    const WATCHDOG: Duration = Duration::from_secs(10 * 60);

    /// The connections the helper serves at once, and of those, the ones
    /// from callers that may not start (`posix::server`).
    const MAX_CONNECTIONS: usize = 8;
    const MAX_READ_ONLY_CONNECTIONS: usize = 4;

    /// A public address every runner reaches.
    const PUBLIC_ADDRESS: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(1, 1, 1, 1), 443);
    /// A public name, for DNS through TUN and a by-name proxy request.
    const PUBLIC_NAME: &str = "one.one.one.one";

    /// The attachment the TUN profile's local rule set travels as.
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
                eprintln!("mac_smoke: {message}\n{USAGE}");
                return USAGE_ERROR;
            }
        };
        watchdog(
            WATCHDOG,
            format!("mac_smoke {command}: FAILED: still running after {WATCHDOG:?}"),
        );
        let result = match command.as_str() {
            "hello" => hello(&options),
            "refused" => refused(&options),
            "slots" => slots(&options),
            "write-deadline" => write_deadline(&options),
            "tun" => tun(&options),
            "unauthorized" => unauthorized(&options),
            "readonly-slots" => readonly_slots(&options),
            "denied" => denied(&options),
            other => Err(Failure::Usage(format!("unknown command {other:?}"))),
        };
        match result {
            Ok(()) => {
                println!("mac_smoke {command}: every expectation held");
                HELD
            }
            Err(Failure::Usage(message)) => {
                eprintln!("mac_smoke {command}: {message}\n{USAGE}");
                USAGE_ERROR
            }
            Err(Failure::Check(message)) => {
                eprintln!("mac_smoke {command}: FAILED: {message}");
                FAILED
            }
        }
    }

    fn watchdog(after: Duration, message: String) {
        thread::spawn(move || {
            thread::sleep(after);
            eprintln!("{message}");
            std::process::exit(FAILED);
        });
    }

    // ---- The command line ----

    enum Failure {
        Usage(String),
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
        socket: Option<PathBuf>,
        probes: bool,
        system_proxy: bool,
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
                    "--socket" => options.socket = Some(value()?.into()),
                    "--probes" => options.probes = true,
                    "--system-proxy" => options.system_proxy = true,
                    other => return Err(format!("unexpected argument {other:?}")),
                }
            }
            Ok(options)
        }

        fn expect(&self, allowed: &[&str]) -> Outcome<&str> {
            match self.expect.as_deref() {
                Some(value) if allowed.contains(&value) => Ok(value),
                _ => Err(Failure::Usage(format!(
                    "--expect must be one of {allowed:?}"
                ))),
            }
        }

        fn socket(&self) -> PathBuf {
            self.socket
                .clone()
                .unwrap_or_else(|| PathBuf::from(SOCKET_PATH))
        }
    }

    // ---- The socket ----

    /// The helper's PID, as the kernel says (a test tool may ask; the
    /// helper itself never uses a peer's PID).
    #[cfg(target_os = "macos")]
    fn peer_pid(stream: &UnixStream) -> Option<u32> {
        use std::os::fd::AsRawFd;
        let mut pid: libc::pid_t = 0;
        let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
        // SAFETY: the descriptor is open while `stream` is borrowed; `pid`
        // and `len` are valid for writes, and `len` is `pid`'s size.
        let result = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                (&mut pid as *mut libc::pid_t).cast(),
                &mut len,
            )
        };
        (result == 0).then_some(pid as u32)
    }

    #[cfg(not(target_os = "macos"))]
    fn peer_pid(_stream: &UnixStream) -> Option<u32> {
        None
    }

    /// Connect to the helper's socket, retrying while it isn't there yet.
    fn connect(path: &Path) -> Result<UnixStream, String> {
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        loop {
            match UnixStream::connect(path) {
                Ok(stream) => return Ok(stream),
                Err(error) if Instant::now() < deadline => {
                    if !matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                    ) {
                        return Err(format!("connecting to {}: {error}", path.display()));
                    }
                    thread::sleep(Duration::from_millis(200));
                }
                Err(error) => {
                    return Err(format!(
                        "{} didn't accept within {}s: {error}",
                        path.display(),
                        CONNECT_TIMEOUT.as_secs()
                    ))
                }
            }
        }
    }

    // ---- One connection ----

    enum Next {
        Message(ToClient),
        Eof,
        TimedOut,
    }

    /// One connection to the helper. sing-box's lines are printed as they
    /// come and kept, and its exit is kept.
    struct Conn {
        stream: UnixStream,
        decoder: FrameDecoder,
        unfed: Vec<u8>,
        limits: Limits,
        lines: Vec<String>,
        exited: Option<ExitInfo>,
        replies: usize,
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
        fn open(options: &Options) -> Result<Self, String> {
            let limits = Limits::default();
            Ok(Self {
                stream: connect(&options.socket())?,
                decoder: FrameDecoder::new(limits.to_gui_caps()),
                unfed: Vec::new(),
                limits,
                lines: Vec::new(),
                exited: None,
                replies: 0,
            })
        }

        fn send(&mut self, request: &Request) -> Result<(), String> {
            let name = request_name(request);
            let bytes = encode_request(request, &self.limits)
                .map_err(|error| format!("encoding {name}: {error}"))?;
            self.stream
                .set_write_timeout(Some(REPLY_TIMEOUT))
                .map_err(|error| error.to_string())?;
            self.stream
                .write_all(&bytes)
                .map_err(|error| format!("sending {name}: {error}"))
        }

        /// The next message, the end of the stream, or nothing by `deadline`.
        fn next(&mut self, deadline: Instant) -> Result<Next, String> {
            let mut buf = vec![0u8; 64 * 1024];
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
                            ToClient::Reply(_) => self.replies += 1,
                        }
                        return Ok(Next::Message(message));
                    }
                    Ok(None) => {}
                    Err(error) => return Err(format!("the helper broke the protocol: {error}")),
                }
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Ok(Next::TimedOut);
                }
                self.stream
                    .set_read_timeout(Some(left))
                    .map_err(|error| error.to_string())?;
                match self.stream.read(&mut buf) {
                    Ok(0) => return Ok(Next::Eof),
                    Ok(n) => self.unfed.extend_from_slice(&buf[..n]),
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock
                                | io::ErrorKind::TimedOut
                                | io::ErrorKind::Interrupted
                        ) => {}
                    // A reset is how a closed connection may read.
                    Err(error) if error.kind() == io::ErrorKind::ConnectionReset => {
                        return Ok(Next::Eof)
                    }
                    Err(error) => return Err(format!("reading the socket: {error}")),
                }
            }
        }

        /// The reply to the outstanding request, within `timeout` (longer
        /// for a connection's first). Events on the way are kept.
        fn reply(&mut self, timeout: Duration) -> Result<Reply, String> {
            let timeout = if self.replies == 0 {
                timeout.max(FIRST_REPLY_TIMEOUT)
            } else {
                timeout
            };
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
            match self.reply(REPLY_TIMEOUT)? {
                Reply::Hello(hello) if hello.protocol_version == PROTOCOL_VERSION => Ok(hello),
                other => Err(format!("hello was answered with {other:?}")),
            }
        }

        fn status(&mut self) -> Result<(RunState, Option<ExitInfo>), String> {
            self.send(&Request::Status)?;
            match self.reply(REPLY_TIMEOUT)? {
                Reply::Status { state, last_exit } => Ok((state, last_exit)),
                other => Err(format!("status was answered with {other:?}")),
            }
        }

        /// Read events until `until`; sing-box must keep running and the
        /// connection stay open.
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

    // ---- Files the shell side waits on ----

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
    /// caller's uid (ADR 0006 rule 4), and `status`, which every caller may
    /// ask.
    fn hello(options: &Options) -> Outcome {
        let may_start = options.expect(&["start", "readonly"])? == "start";
        let began = Instant::now();
        let mut conn = Conn::open(options)?;
        let hello = conn.hello()?;
        let pid = peer_pid(&conn.stream);
        println!(
            "helper {} (pid {}), sing-box {} sha256 {}, may_start {}; answered in {} ms",
            hello.helper_version,
            pid.map_or("?".into(), |pid| pid.to_string()),
            hello.sing_box_version,
            hello.sing_box_sha256,
            hello.may_start,
            began.elapsed().as_millis()
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
        if let Some(ready) = &options.ready_file {
            write_ready(
                ready,
                &[("helper_pid", pid.map_or("0".into(), |pid| pid.to_string()))],
            )?;
        }
        Ok(())
    }

    // ---- Starts ----

    fn free_port() -> Result<u16, String> {
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .and_then(|listener| listener.local_addr())
            .map(|address| address.port())
            .map_err(|error| format!("no free loopback port: {error}"))
    }

    fn tun_options(proxy_port: u16, system_proxy: bool) -> TunOptions {
        TunOptions {
            ipv6: false,
            proxy_port,
            allow_lan: false,
            system_proxy,
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
    /// network: one direct outbound bound to the real interface, DNS
    /// hijacked to a public resolver, and a local rule set that travels as
    /// an attachment, as the GUI sends it.
    fn tun_start(proxy_port: u16, system_proxy: bool) -> Result<StartRequest, String> {
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
            options: tun_options(proxy_port, system_proxy),
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

    /// A profile with fields the privileged path refuses: the tor outbound
    /// runs a program, and NTP `write_to_system` sets the clock.
    fn refused_config() -> String {
        json!({
            "ntp": {"enabled": true, "server": "time.apple.com", "write_to_system": true},
            "outbounds": [
                {"type": "direct", "tag": "direct"},
                {"type": "tor", "tag": "tor", "executable_path": "/bin/sh"}
            ]
        })
        .to_string()
    }

    const EXPECTED_REFUSALS: &[(&str, RefusalCode)] = &[
        ("/ntp/write_to_system", RefusalCode::SystemChange),
        ("/outbounds/1/executable_path", RefusalCode::RunsProgram),
        ("/outbounds/1/type", RefusalCode::RunsProgram),
    ];

    /// A start whose config the policy refuses is answered `refused`, with
    /// the refusals the GUI's own run of the policy predicts, and starts
    /// nothing.
    fn refused(options: &Options) -> Outcome {
        let mut conn = Conn::open(options)?;
        if !conn.hello()?.may_start {
            return Err(Failure::Usage(
                "this account may not start; run this as the owner".into(),
            ));
        }
        let request = StartRequest {
            config: refused_config(),
            attachments: Vec::new(),
            options: tun_options(free_port()?, false),
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
        let (state, _) = conn.status()?;
        if state != RunState::Stopped {
            return Err(format!("after the refusal the helper reports {state:?}").into());
        }
        Ok(())
    }

    /// A caller that may not start gets `unauthorized` for `start` and
    /// `stop`, before anything is checked or run, and `status` all the same.
    fn unauthorized(options: &Options) -> Outcome {
        let mut conn = Conn::open(options)?;
        if conn.hello()?.may_start {
            return Err(Failure::Usage(
                "this account may start; run this as another account".into(),
            ));
        }
        let start = tun_start(free_port()?, false)?;
        if let Err(error) = conn.send(&Request::Start(start)) {
            println!("note: {error} (the helper closed after the header)");
        }
        expect_unauthorized(&mut conn, "start")?;
        conn.wait_eof(Duration::from_secs(10))?;

        let mut conn = Conn::open(options)?;
        conn.hello()?;
        conn.send(&Request::Stop)?;
        expect_unauthorized(&mut conn, "stop")?;

        let mut conn = Conn::open(options)?;
        conn.hello()?;
        let (state, _) = conn.status()?;
        println!("ok: status is answered ({state:?})");
        Ok(())
    }

    fn expect_unauthorized(conn: &mut Conn, what: &str) -> Result<(), String> {
        match conn.reply(REPLY_TIMEOUT)? {
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
    /// ninth waits in the socket's backlog, and is served once one ends.
    fn slots(options: &Options) -> Outcome {
        let mut held = Vec::new();
        for n in 1..=MAX_CONNECTIONS {
            let mut conn = Conn::open(options)?;
            if !conn.hello()?.may_start {
                return Err(Failure::Usage(
                    "this account may not start; run this as the owner".into(),
                ));
            }
            println!("connection {n}: served");
            held.push(conn);
        }
        let mut ninth = Conn::open(options)?;
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
        match ninth.reply(REPLY_TIMEOUT)? {
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
    /// file appears, so the owner can show it still gets in.
    fn readonly_slots(options: &Options) -> Outcome {
        let mut held = Vec::new();
        for n in 1..=MAX_READ_ONLY_CONNECTIONS {
            let mut conn = Conn::open(options)?;
            if conn.hello()?.may_start {
                return Err(Failure::Usage(
                    "this account may start; run this as another account".into(),
                ));
            }
            println!("read-only connection {n}: served");
            held.push(conn);
        }
        let mut fifth = Conn::open(options)?;
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
            for conn in &mut held {
                conn.status()?;
            }
        }
        Ok(())
    }

    /// Requests sent without reading a reply: once the socket is full, the
    /// helper's write misses its deadline and it drops the connection.
    fn write_deadline(options: &Options) -> Outcome {
        const FLOOD: usize = 20_000;
        let mut conn = Conn::open(options)?;
        conn.hello()?;
        let one = encode_request(&Request::Status, &conn.limits)
            .map_err(|error| format!("encoding status: {error}"))?;
        let flood = one.repeat(FLOOD);
        watchdog(
            Duration::from_secs(60),
            "mac_smoke write-deadline: FAILED: a write to a client that stopped reading \
             still blocks after 60s: the helper neither reads nor drops it"
                .to_owned(),
        );
        let began = Instant::now();
        conn.stream
            .set_write_timeout(None)
            .map_err(|error| error.to_string())?;
        match conn.stream.write_all(&flood) {
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

    // ---- The state directory ----

    /// This account can't list `--dir` (the state directory) or open
    /// `--file` (the helper's log in it).
    fn denied(options: &Options) -> Outcome {
        let (Some(dir), Some(file)) = (&options.dir, &options.file) else {
            return Err(Failure::Usage("denied needs --dir and --file".into()));
        };
        expect_denied(&format!("listing {}", dir.display()), fs::read_dir(dir))?;
        expect_denied(&format!("opening {}", file.display()), fs::File::open(file))?;
        Ok(())
    }

    fn expect_denied<T>(what: &str, result: io::Result<T>) -> Outcome {
        match result {
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                println!("ok: {what}: {error}");
                Ok(())
            }
            Err(error) => Err(format!("{what} failed, but not as denied: {error}").into()),
            Ok(_) => Err(format!("{what} succeeded").into()),
        }
    }

    // ---- TUN ----

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum End {
        Stop,
        Close,
        MidFrame,
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

    /// A real TUN start as root: `started`, sing-box's log saying its TUN
    /// inbound and then sing-box itself started, its API listening on
    /// loopback, and, with `--probes`, the loopback rule and ordinary
    /// traffic checked through the proxy and TUN. With `--ready-file` it
    /// says so (for checks from outside: utun, the routes, sing-box's
    /// parent and listeners, the system proxy) and holds TUN up until the
    /// release file appears. Then it ends as `--end` says, and sing-box
    /// must be gone: the helper's status says stopped, and the machine's
    /// own route is back.
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
        let lan = lan_address();

        let mut conn = Conn::open(options)?;
        let hello = conn.hello()?;
        let helper_pid = peer_pid(&conn.stream);
        if !hello.may_start {
            return Err(Failure::Usage(
                "this account may not start; run this as the owner".into(),
            ));
        }
        let start = tun_start(proxy_port, options.system_proxy)?;
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
                conn.stream
                    .write_all(&[0, 0, 0])
                    .map_err(|error| format!("writing half a header: {error}"))?;
                drop(conn);
            }
            End::HelperKilled => {
                conn.wait_eof(Duration::from_secs(120))?;
                println!("ok: the connection ended with the helper");
                return network_after_tun().map_err(Failure::from);
            }
        }
        wait_stopped(options)?;
        network_after_tun()?;
        Ok(())
    }

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
        println!("ok: stopped, last exit {last_exit:?}");
        Ok(())
    }

    /// On a new connection, the helper reports no sing-box running within a
    /// few seconds: the one this run started stopped with its connection.
    fn wait_stopped(options: &Options) -> Result<(), String> {
        let mut conn = Conn::open(options)?;
        conn.hello()?;
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let (state, last_exit) = conn.status()?;
            if state == RunState::Stopped {
                println!("ok: the helper reports sing-box stopped (last exit {last_exit:?})");
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "sing-box still {state:?} 20s after its connection ended"
                ));
            }
            thread::sleep(Duration::from_millis(250));
        }
    }

    /// Once TUN is down, the machine's own route carries traffic again: a
    /// connection out doesn't start from the TUN interface's address.
    fn network_after_tun() -> Result<(), String> {
        let tun = IpAddr::V4(tun_address());
        let deadline = Instant::now() + Duration::from_secs(30);
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

    /// A TCP echo service on every IPv4 address and, if it can, on IPv6
    /// loopback, counting what reaches it.
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

    #[derive(Debug)]
    enum Probe {
        Reached,
        Refused(String),
    }

    impl Via {
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

    /// The loopback rule (ADR 0006, "Loopback, as hygiene") from outside,
    /// then the controls that make its refusals mean something: the proxy
    /// and TUN carry ordinary traffic, by address and by name.
    fn probes(proxy: SocketAddr, echo: &EchoServer, lan: Option<Ipv4Addr>) -> Result<(), String> {
        let mut loopback = vec![
            (Via::Socks5, Dest::Ip(Ipv4Addr::LOCALHOST.into()), echo.port),
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

        match lan {
            Some(address) => {
                let before = echo.accepted();
                let probe = Via::Socks5.probe(proxy, &Dest::Ip(address.into()), echo.port, true);
                match probe {
                    Ok(Probe::Reached) if echo.accepted() > before => println!(
                        "ok: SOCKS5 to this machine's {address}:{} reached the listener",
                        echo.port
                    ),
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
}
