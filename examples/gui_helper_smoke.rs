//! `gui_helper_smoke`: the GUI's own helper client against the macOS helper
//! installed on this Mac (ADR 0006, phase 2; `docs/helper-macos-checklist.md`).
//!
//! `mac_smoke`, in the helper's crate, drives the daemon with a client of
//! its own. This drives it with the code BoxPilot ships
//! (`core::privileged_helper`): `open()` and its socket stream,
//! `HelperConnection`, `start_profile` (the profile's local files read as
//! the user, the policy run first), and the look at the helper that
//! Settings › TUN shows (`macos_status::probe`). CI runs it on GitHub's
//! Apple-silicon runner through `packaging/macos/helper-smoke.sh
//! gui-client`, as the runner's account (the owner), once the helper is
//! installed from the DMG. It is a test tool built from the GUI's crate,
//! never bundled: `build-dmg.sh` takes the release binary only.
//!
//! Each command checks one set of expectations. The exit code is 0 when
//! they held, 1 when one didn't (and what was seen is printed), 2 for a bad
//! command line.

#[cfg(not(unix))]
fn main() {
    eprintln!("gui_helper_smoke drives the macOS helper; it runs on macOS only");
    std::process::exit(2);
}

#[cfg(unix)]
fn main() {
    std::process::exit(smoke::main());
}

#[cfg(unix)]
mod smoke {
    use box_pilot_gui::core::privileged_helper::macos_status::{probe, HelperStatus};
    use box_pilot_gui::core::privileged_helper::{
        open, start_profile, HelperConnection, HelperEvent, HelperIo, RunningStart,
    };
    use box_pilot_gui::core::settings::RUNTIME_CONFIG_FILENAME;
    use boxpilot_protocol::TunOptions;
    use futures_channel::mpsc::{TryRecvError, UnboundedReceiver};
    use serde_json::json;
    use std::fs;
    use std::io;
    use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
    use std::path::{Path, PathBuf};
    use std::thread;
    use std::time::{Duration, Instant};

    const USAGE: &str = "\
usage: gui_helper_smoke <command>

as the helper's owner:
  hello            open() and hello: this account may start
  status --contents <BoxPilot.app/Contents> [--expect <state>]
                   the state Settings › TUN shows, printed as `status: <state>`
                   (ready, not-installed, turned-off, other-owner, stale,
                   broken <code>, unreachable, unknown)
  tun --app-dir <dir>
                   a real TUN start through start_profile: up, then stopped
  gone-peer        the helper ends the connection; writing and stopping
                   after that fail cleanly, at once";

    const HELD: i32 = 0;
    const FAILED: i32 = 1;
    const USAGE_ERROR: i32 = 2;

    /// From the start to sing-box's log saying TUN is up.
    const TUN_UP_TIMEOUT: Duration = Duration::from_secs(45);
    /// How long `stop` may wait for the helper's `stopped`, as the GUI
    /// allows (`process_session::HELPER_STOP_TIMEOUT`).
    const STOP_TIMEOUT: Duration = Duration::from_secs(10);
    /// The longest any command runs.
    const WATCHDOG: Duration = Duration::from_secs(5 * 60);

    /// The local rule set the TUN profile reads, which travels as an
    /// attachment, as a real profile's would.
    const RULE_SET_FILE: &str = "gui-smoke-rules.json";
    const RULE_SET: &str = r#"{"version": 1, "rules": [{"domain_suffix": ["gui-smoke.invalid"]}]}"#;

    enum Failure {
        Usage(String),
        Check(String),
    }

    impl From<String> for Failure {
        fn from(message: String) -> Self {
            Failure::Check(message)
        }
    }

    type Outcome = Result<(), Failure>;

    pub fn main() -> i32 {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let Some((command, rest)) = args.split_first() else {
            eprintln!("{USAGE}");
            return USAGE_ERROR;
        };
        thread::spawn(|| {
            thread::sleep(WATCHDOG);
            eprintln!("gui_helper_smoke: FAILED: still running after {WATCHDOG:?}");
            std::process::exit(FAILED);
        });
        let result = match command.as_str() {
            "hello" => no_options(rest).and_then(|()| hello()),
            "status" => status(rest),
            "tun" => option(rest, "--app-dir").and_then(|dir| tun(Path::new(&dir))),
            "gone-peer" => no_options(rest).and_then(|()| gone_peer()),
            other => Err(Failure::Usage(format!("unknown command {other:?}"))),
        };
        match result {
            Ok(()) => {
                println!("gui_helper_smoke {command}: every expectation held");
                HELD
            }
            Err(Failure::Usage(message)) => {
                eprintln!("gui_helper_smoke {command}: {message}\n{USAGE}");
                USAGE_ERROR
            }
            Err(Failure::Check(message)) => {
                eprintln!("gui_helper_smoke {command}: FAILED: {message}");
                FAILED
            }
        }
    }

    fn no_options(args: &[String]) -> Result<(), Failure> {
        match args.first() {
            None => Ok(()),
            Some(arg) => Err(Failure::Usage(format!("unexpected argument {arg:?}"))),
        }
    }

    /// The value of the one option `name`, which must be all of `args`.
    fn option(args: &[String], name: &str) -> Result<String, Failure> {
        match args {
            [flag, value] if flag == name => Ok(value.clone()),
            _ => Err(Failure::Usage(format!("expected {name} <value>"))),
        }
    }

    /// A state as one word, for the shell script.
    fn status_word(status: &HelperStatus) -> String {
        match status {
            HelperStatus::Unknown => "unknown".into(),
            HelperStatus::NotInstalled => "not-installed".into(),
            HelperStatus::TurnedOff => "turned-off".into(),
            HelperStatus::Ready => "ready".into(),
            HelperStatus::OtherOwner => "other-owner".into(),
            HelperStatus::Stale => "stale".into(),
            HelperStatus::Broken(code) => format!("broken {code}"),
            HelperStatus::Unreachable(_) => "unreachable".into(),
        }
    }

    // ---- hello ----

    /// `open()` reaches the daemon through launchd's socket, and `hello`
    /// says this account may start.
    fn hello() -> Outcome {
        let began = Instant::now();
        let io = open().map_err(|error| error.message())?;
        let (connection, _events) = HelperConnection::new(io);
        let hello = connection.hello().map_err(|failure| failure.message())?;
        println!(
            "helper {}, sing-box {} sha256 {}, may_start {}; answered in {} ms",
            hello.helper_version,
            hello.sing_box_version,
            hello.sing_box_sha256,
            hello.may_start,
            began.elapsed().as_millis()
        );
        if !hello.may_start {
            return Err(
                "hello says this account may not start; run this as the owner"
                    .to_owned()
                    .into(),
            );
        }
        Ok(())
    }

    // ---- status ----

    /// What Settings › TUN shows, judged against the app at `--contents`.
    fn status(args: &[String]) -> Outcome {
        let mut contents = None;
        let mut expect = None;
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let value = args
                .next()
                .cloned()
                .ok_or_else(|| Failure::Usage(format!("{arg} needs a value")))?;
            match arg.as_str() {
                "--contents" => contents = Some(PathBuf::from(value)),
                "--expect" => expect = Some(value),
                other => return Err(Failure::Usage(format!("unexpected argument {other:?}"))),
            }
        }
        let Some(contents) = contents else {
            return Err(Failure::Usage("status needs --contents".into()));
        };
        let began = Instant::now();
        let status = probe(Some(&contents));
        let word = status_word(&status);
        println!(
            "status: {word} ({} ms): {}",
            began.elapsed().as_millis(),
            status.message()
        );
        match expect {
            Some(expected) if expected != word => {
                Err(format!("the helper's state is {word}, not {expected}").into())
            }
            _ => Ok(()),
        }
    }

    // ---- TUN ----

    fn free_port() -> Result<u16, String> {
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .and_then(|listener| listener.local_addr())
            .map(|address| address.port())
            .map_err(|error| format!("no free loopback port: {error}"))
    }

    /// The smallest profile a TUN start can run that keeps the runner's
    /// network (as `mac_smoke`'s), with a local rule set at a relative
    /// path: `start_profile` reads it as the user, against the data dir.
    fn write_profile(app_dir: &Path) -> Result<PathBuf, String> {
        fs::create_dir_all(app_dir).map_err(|error| format!("{}: {error}", app_dir.display()))?;
        let write = |name: &str, text: &str| {
            let path = app_dir.join(name);
            fs::write(&path, text)
                .map(|()| path.clone())
                .map_err(|error| format!("writing {}: {error}", path.display()))
        };
        write(RULE_SET_FILE, RULE_SET)?;
        let config = json!({
            "log": {"level": "info"},
            "dns": {"servers": [{"type": "udp", "tag": "public", "server": "1.1.1.1"}]},
            "outbounds": [{"type": "direct", "tag": "direct"}],
            "route": {
                "rule_set": [{
                    "type": "local", "tag": "gui-smoke", "format": "source",
                    "path": RULE_SET_FILE
                }],
                "rules": [
                    {"action": "sniff"},
                    {"protocol": "dns", "action": "hijack-dns"},
                    {"rule_set": "gui-smoke", "action": "reject"}
                ],
                "auto_detect_interface": true,
                "default_domain_resolver": "public",
                "final": "direct"
            }
        });
        write("profile.json", &config.to_string())
    }

    /// The next event within `timeout`; `None` once the channel has ended.
    fn next_event(
        events: &mut UnboundedReceiver<HelperEvent>,
        timeout: Duration,
    ) -> Result<Option<HelperEvent>, String> {
        let deadline = Instant::now() + timeout;
        loop {
            match events.try_recv() {
                Ok(event) => return Ok(Some(event)),
                Err(TryRecvError::Closed) => return Ok(None),
                Err(TryRecvError::Empty) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(20))
                }
                Err(TryRecvError::Empty) => {
                    return Err(format!("no event from the helper within {timeout:?}"))
                }
            }
        }
    }

    /// A real TUN start through the GUI's path: `start_profile` reads the
    /// profile and its rule set, runs the policy, writes the running view,
    /// connects and starts. sing-box's log says TUN and then sing-box
    /// itself started, its API listens where the helper said, and `stop`
    /// is answered (well before its deadline); the connection then ends.
    fn tun(app_dir: &Path) -> Outcome {
        let config = write_profile(app_dir)?;
        let options = TunOptions {
            ipv6: false,
            proxy_port: free_port()?,
            allow_lan: false,
            system_proxy: false,
        };
        let began = Instant::now();
        let RunningStart {
            connection,
            api,
            mut events,
        } = start_profile(&config, app_dir, options)?;
        println!(
            "started in {} ms: proxy 127.0.0.1:{}, API 127.0.0.1:{}",
            began.elapsed().as_millis(),
            options.proxy_port,
            api.port()
        );
        let view = app_dir.join(RUNTIME_CONFIG_FILENAME);
        let view_text =
            fs::read_to_string(&view).map_err(|error| format!("{}: {error}", view.display()))?;
        if !view_text.contains("\"tun\"") || view_text.contains(RULE_SET_FILE) {
            return Err(format!(
                "the running view {} lacks the TUN inbound or still names the local file",
                view.display()
            )
            .into());
        }

        let deadline = Instant::now() + TUN_UP_TIMEOUT;
        let (mut tun_up, mut sing_box_up) = (false, false);
        while !(tun_up && sing_box_up) {
            let left = deadline.saturating_duration_since(Instant::now());
            match next_event(&mut events, left)? {
                Some(HelperEvent::Log(line)) => {
                    println!("  sing-box | {line}");
                    tun_up |= line.contains("inbound/tun") && line.contains("started");
                    sing_box_up |= line.contains("sing-box started");
                }
                Some(other) => {
                    return Err(format!("{other:?} while sing-box should be starting").into())
                }
                None => return Err("the events ended while sing-box started".to_owned().into()),
            }
        }
        println!(
            "ok: TUN up {} ms after the start",
            began.elapsed().as_millis()
        );
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, api.port()));
        TcpStream::connect_timeout(&address, Duration::from_secs(5))
            .map_err(|error| format!("the helper's API at {address} doesn't accept: {error}"))?;
        println!("ok: the API listens at {address}");

        let stopping = Instant::now();
        connection.stop(STOP_TIMEOUT);
        let took = stopping.elapsed();
        if took >= STOP_TIMEOUT - Duration::from_secs(1) {
            return Err(format!("stop waited {took:?}: the helper didn't answer `stop`").into());
        }
        println!("ok: stopped in {} ms", took.as_millis());
        loop {
            match next_event(&mut events, Duration::from_secs(10))? {
                Some(HelperEvent::Log(line)) => println!("  sing-box | {line}"),
                Some(HelperEvent::Exited(exit)) => println!("ok: sing-box exited: {exit:?}"),
                Some(HelperEvent::Closed(reason)) => {
                    println!("ok: the connection ended ({reason})");
                    break;
                }
                None => return Err("the events ended without Closed".to_owned().into()),
            }
        }
        if let Some(event) = next_event(&mut events, Duration::from_secs(1))? {
            return Err(format!("an event after Closed: {event:?}").into());
        }
        Ok(())
    }

    // ---- A peer that is gone ----

    /// The helper ends the connection (a frame type the protocol doesn't
    /// have is answered with `error`, then closed). Writing after that
    /// fails as a write does, never with the `EINVAL` macOS gives the
    /// socket timeout once the peer is gone; stopping returns at once; and
    /// the next connection is served as usual.
    fn gone_peer() -> Outcome {
        let io: std::sync::Arc<dyn HelperIo> = open().map_err(|error| error.message())?;
        let (connection, mut events) = HelperConnection::new(io.clone());
        if !connection
            .hello()
            .map_err(|failure| failure.message())?
            .may_start
        {
            return Err("this account may not start; run this as the owner"
                .to_owned()
                .into());
        }
        let deadline = || Instant::now() + Duration::from_secs(5);
        io.write_all(&[0, 0, 0, 1, 0x7f, b'x'], deadline())
            .map_err(|error| format!("sending a malformed frame: {error}"))?;
        match next_event(&mut events, Duration::from_secs(10))? {
            Some(HelperEvent::Closed(reason)) => println!("ok: the helper ended it ({reason})"),
            other => return Err(format!("instead of the end of the connection: {other:?}").into()),
        }
        // A short pause: the helper's end is closed, not just shut down.
        thread::sleep(Duration::from_millis(200));
        match io.write_all(b"\0\0\0\x01", deadline()) {
            Err(error) if error.raw_os_error() == Some(libc::EINVAL) => {
                return Err(format!("a write to the gone peer failed with EINVAL: {error}").into())
            }
            Err(error) if error.kind() == io::ErrorKind::TimedOut => {
                return Err(
                    format!("a write to the gone peer waited for its deadline: {error}").into(),
                )
            }
            Err(error) => println!("ok: a write to the gone peer fails: {error}"),
            Ok(()) => println!("note: a write to the gone peer was taken by the socket"),
        }
        let stopping = Instant::now();
        connection.stop(STOP_TIMEOUT);
        let took = stopping.elapsed();
        if took >= Duration::from_secs(2) {
            return Err(format!("stopping a gone connection took {took:?}").into());
        }
        println!("ok: stopping it returned in {} ms", took.as_millis());
        io.close();
        hello()
    }
}
