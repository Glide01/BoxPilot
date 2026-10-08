//! The POSIX supervisor over a real tree and a real child: a shell script
//! standing in for sing-box (tests only; the helper never runs a shell),
//! installed as the install script would, with its manifest, and another
//! standing in for `/usr/bin/sandbox-exec`, which records what it was given
//! and executes sing-box as the real one does. On macOS, one test runs the
//! real sandbox-exec with the shipped profile.

use super::*;
use crate::helper::RunEvents;
use crate::manifest::sha256_hex;
use crate::runcfg::{self, SystemProxy};
use crate::testing::TempDir;
use boxpilot_protocol::{ExitInfo, StartRequest, TunOptions};
use serde_json::json;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;

fn me() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// A fake sing-box: prints its arguments, environment and directory, says
/// it started, then waits; SIGTERM ends it with 0, as sing-box's own close
/// does.
const POLITE: &str = r#"for arg in "$@"; do echo "arg=$arg"; done
echo "env=HOME=$HOME"
echo "env=PATH=$PATH"
echo "env=TMPDIR=$TMPDIR"
echo "pwd=$(pwd)"
echo "sing-box started"
trap 'echo stopping; exit 0' TERM
while :; do sleep 0.1; done"#;

/// One that ignores SIGTERM: only SIGKILL ends it.
const STUBBORN: &str = r#"trap '' TERM
echo "sing-box started"
while :; do sleep 0.1; done"#;

/// One that exits on its own, badly.
const CRASHING: &str = r#"echo "sing-box started"
exit 7"#;

/// A stand-in for `/usr/bin/sandbox-exec`: records its arguments,
/// NUL-separated, in the file put in place of `"$RECORD"`, then executes
/// the command after `--`, keeping its PID, as the real one does once the
/// profile is applied.
const SANDBOX_EXEC: &str = r#"for arg in "$@"; do printf '%s\0' "$arg"; done >"$RECORD"
while [ "$#" -gt 0 ]; do
    case $1 in
        --) shift; exec "$@" ;;
        -p | -f | -n | -D) shift 2 ;;
        *) exec "$@" ;;
    esac
done
echo "sandbox-exec: no command" >&2
exit 64"#;

/// One that can't apply the profile: it says why, and never runs sing-box.
const SANDBOX_EXEC_REFUSING: &str = r#"echo "sandbox-exec: the profile was refused" >&2
exit 65"#;

/// A helper tree as the install leaves it: `bin` (sing-box, manifest) and
/// `state` (0700, the owner record), all this test's user's.
struct Install {
    _temp: TempDir,
    layout: Layout,
    trust: Trust,
    cleanups: Arc<Mutex<Vec<Cleanup>>>,
    starts: Arc<Mutex<Vec<AfterStart>>>,
    /// "after_start" and "cleanup", in the order the platform got them.
    order: Arc<Mutex<Vec<&'static str>>>,
    /// The stand-in for sandbox-exec, in a directory of its own.
    sandbox_exec: PathBuf,
    /// Where it records its arguments.
    sandbox_record: PathBuf,
}

impl Install {
    /// `None` when this machine's build directory can't pass the chain
    /// check (a group-writable home, say): the test then checks nothing.
    fn new(tag: &str, script: &str) -> Option<Self> {
        let temp = TempDir::in_build_dir(tag);
        fs::set_permissions(&temp.0, fs::Permissions::from_mode(0o755)).unwrap();
        let trust = Trust::root_and(me());
        if let Err(refused) = verify::dir_chain(&temp.0, Role::Dir, &trust) {
            eprintln!("note: skipped, the build directory's chain is refused: {refused}");
            return None;
        }
        let root = fs::canonicalize(&temp.0).unwrap();
        if !root.to_str().is_some_and(sandboxplan::is_plain_path) {
            eprintln!(
                "note: skipped, the build directory's path can't be a sandbox parameter: {}",
                root.display()
            );
            return None;
        }
        let layout = Layout::new(root.join("bin"), root.join("state"));
        DirBuilder::new()
            .mode(0o755)
            .create(layout.helper_dir())
            .unwrap();
        fs::set_permissions(layout.helper_dir(), fs::Permissions::from_mode(0o755)).unwrap();
        DirBuilder::new()
            .mode(0o700)
            .create(layout.state_dir())
            .unwrap();
        let system = root.join("system");
        DirBuilder::new().mode(0o755).create(&system).unwrap();
        fs::set_permissions(&system, fs::Permissions::from_mode(0o755)).unwrap();
        let install = Self {
            _temp: temp,
            layout,
            trust,
            cleanups: Arc::new(Mutex::new(Vec::new())),
            starts: Arc::new(Mutex::new(Vec::new())),
            order: Arc::new(Mutex::new(Vec::new())),
            sandbox_exec: system.join("sandbox-exec"),
            sandbox_record: root.join("sandbox-exec.args"),
        };
        install.put_sing_box(script);
        install.put_sandbox_exec(&SANDBOX_EXEC.replace(
            "\"$RECORD\"",
            &format!("'{}'", install.sandbox_record.display()),
        ));
        install.write_owner(&format!("{}\n", me()));
        Some(install)
    }

    fn put_sandbox_exec(&self, script: &str) {
        fs::write(&self.sandbox_exec, format!("#!/bin/sh\n{script}\n")).unwrap();
        fs::set_permissions(&self.sandbox_exec, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// What the stand-in for sandbox-exec was last given.
    fn sandbox_args(&self) -> Vec<String> {
        let bytes = fs::read(&self.sandbox_record).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        let mut args: Vec<String> = text.split('\0').map(str::to_owned).collect();
        assert_eq!(args.pop().as_deref(), Some(""), "NUL-terminated");
        args
    }

    fn sing_box(&self) -> PathBuf {
        self.layout.helper_file("sing-box")
    }

    fn put_sing_box(&self, script: &str) {
        self.put_binary(format!("#!/bin/sh\n{script}\n").as_bytes());
    }

    /// sing-box's file, `content` whatever it is, and the manifest hashing
    /// it.
    fn put_binary(&self, content: &[u8]) {
        fs::write(self.sing_box(), content).unwrap();
        fs::set_permissions(self.sing_box(), fs::Permissions::from_mode(0o755)).unwrap();
        let manifest = json!({
            "manifest_version": 1,
            "sing_box": {
                "file": "sing-box",
                "version": "1.14.2",
                "sha256": sha256_hex(content).unwrap()
            },
            "extra_files": []
        });
        fs::write(self.layout.manifest_file(), manifest.to_string()).unwrap();
        fs::set_permissions(
            self.layout.manifest_file(),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
    }

    fn write_owner(&self, text: &str) {
        let path = self.layout.owner_file();
        fs::write(&path, text).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn setup(&self) -> Setup {
        let (cleanups, starts) = (self.cleanups.clone(), self.starts.clone());
        let (cleanup_order, start_order) = (self.order.clone(), self.order.clone());
        Setup {
            layout: self.layout.clone(),
            trust: self.trust.clone(),
            own_exe: None,
            after_start: Arc::new(move |plan: &AfterStart| {
                starts.lock().unwrap().push(*plan);
                start_order.lock().unwrap().push("after_start");
            }),
            cleanup: Arc::new(move |plan: &Cleanup| {
                cleanups.lock().unwrap().push(*plan);
                cleanup_order.lock().unwrap().push("cleanup");
            }),
            stop_grace: Duration::from_millis(300),
            sandbox_exec: self.sandbox_exec.clone(),
        }
    }

    fn start(&self) -> Result<PosixSupervisor, StartError> {
        PosixSupervisor::start(self.setup(), 8 * 1024)
    }

    fn cleanups(&self) -> Vec<Cleanup> {
        self.cleanups.lock().unwrap().clone()
    }

    fn starts(&self) -> Vec<AfterStart> {
        self.starts.lock().unwrap().clone()
    }

    fn order(&self) -> Vec<&'static str> {
        self.order.lock().unwrap().clone()
    }

    /// Wait (at most 10 s) until the platform was told sing-box is up.
    fn wait_for_start(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.starts().is_empty() {
            assert!(Instant::now() < deadline, "sing-box was never seen up");
            thread::sleep(Duration::from_millis(20));
        }
    }
}

/// What a run's events said.
#[derive(Default)]
struct Seen {
    lines: Mutex<Vec<String>>,
    exit: Mutex<Option<ExitInfo>>,
    changed: Condvar,
}

impl RunEvents for Seen {
    fn line(&self, line: String, _truncated: bool) {
        self.lines.lock().unwrap().push(line);
        self.changed.notify_all();
    }

    fn exited(&self, exit: ExitInfo) {
        let mut seen = self.exit.lock().unwrap();
        assert!(seen.is_none(), "exited twice");
        *seen = Some(exit);
        self.changed.notify_all();
    }
}

impl Seen {
    fn wait_for_line(&self, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut lines = self.lines.lock().unwrap();
        while !lines.iter().any(|line| line == text) {
            assert!(Instant::now() < deadline, "no line {text:?} in {lines:?}");
            lines = self
                .changed
                .wait_timeout(lines, Duration::from_millis(100))
                .unwrap()
                .0;
        }
    }

    fn exit(&self) -> Option<ExitInfo> {
        *self.exit.lock().unwrap()
    }

    fn wait_for_exit(&self) -> ExitInfo {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut exit = self.exit.lock().unwrap();
        loop {
            if let Some(exit) = *exit {
                return exit;
            }
            assert!(Instant::now() < deadline, "no exit");
            exit = self
                .changed
                .wait_timeout(exit, Duration::from_millis(100))
                .unwrap()
                .0;
        }
    }

    fn lines(&self) -> Vec<String> {
        self.lines.lock().unwrap().clone()
    }
}

/// A spawned run: its directory, what its events said, and the spawn's
/// result.
type Spawned = (PathBuf, Arc<Seen>, Result<Box<dyn Process>, HelperError>);

/// Prepare a run for `uid` with a minimal config, and spawn it. Its `api`
/// port is one nothing listens on: the fake sing-box never comes up.
fn run(supervisor: &PosixSupervisor, uid: &str, system_proxy: bool) -> Spawned {
    let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    run_on(supervisor, uid, system_proxy, port)
}

/// `run`, with an `api` port the returned listener holds: the fake sing-box
/// is up as soon as it runs, as the helper sees it.
fn run_up(supervisor: &PosixSupervisor, uid: &str, system_proxy: bool) -> (Spawned, TcpListener) {
    let api = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = api.local_addr().unwrap().port();
    (run_on(supervisor, uid, system_proxy, port), api)
}

fn run_on(supervisor: &PosixSupervisor, uid: &str, system_proxy: bool, api_port: u16) -> Spawned {
    let (mut run, placement) = supervisor.prepare_run(uid).unwrap();
    let start = StartRequest {
        config: json!({"outbounds": [{"type": "direct", "tag": "direct"}]}).to_string(),
        attachments: Vec::new(),
        options: TunOptions {
            ipv6: false,
            proxy_port: 7890,
            allow_lan: false,
            system_proxy,
        },
    };
    let prepared = runcfg::build(
        runcfg::check(start).unwrap(),
        &placement,
        SystemProxy::ByHelper,
        || Ok((api_port, ())),
        &[7; 32],
    )
    .unwrap();
    run.write(&prepared).unwrap();
    let path = run.path().to_owned();
    let seen = Arc::new(Seen::default());
    let process = supervisor.spawn(run, seen.clone());
    (path, seen, process)
}

#[test]
fn a_run_starts_with_its_plan_and_stops_cleanly_on_sigterm() {
    let Some(install) = Install::new("sup-run", POLITE) else {
        return;
    };
    let supervisor = install.start().unwrap();
    assert_eq!(supervisor.installed().sing_box_version, "1.14.2");
    let ((run_dir, seen, process), _api) = run_up(&supervisor, &me().to_string(), true);
    let mut process = process.unwrap();
    seen.wait_for_line("sing-box started");
    let run_text = run_dir.to_str().unwrap().to_owned();
    let lines = seen.lines();
    assert_eq!(
        lines[..6],
        [
            "arg=run".to_owned(),
            "arg=-D".to_owned(),
            format!("arg={run_text}"),
            "arg=-c".to_owned(),
            format!("arg={run_text}/config.json"),
            "arg=--disable-color".to_owned(),
        ]
    );
    assert!(
        lines.contains(&format!("env=HOME={run_text}/home")),
        "{lines:?}"
    );
    assert!(
        lines.contains(&format!("env=TMPDIR={run_text}/tmp")),
        "{lines:?}"
    );
    assert!(lines.contains(&"env=PATH=/usr/bin:/bin:/usr/sbin:/sbin".to_owned()));
    assert!(lines.contains(&format!("pwd={run_text}")), "{lines:?}");
    // It ran through sandbox-exec: the constant profile, the helper's own
    // paths as its parameters, then exactly the verified sing-box.
    let state = install.layout.state_dir().to_str().unwrap();
    let sing_box = install.sing_box().to_str().unwrap().to_owned();
    assert_eq!(
        install.sandbox_args(),
        [
            "-p".to_owned(),
            sandboxplan::SING_BOX_PROFILE.to_owned(),
            "-D".to_owned(),
            format!("SING_BOX={sing_box}"),
            "-D".to_owned(),
            format!("STATE_DIR={state}"),
            "-D".to_owned(),
            format!("RUN_DIR={run_text}"),
            "-D".to_owned(),
            format!("USER_DIR={state}/users/{}", me()),
            "--".to_owned(),
            sing_box,
            "run".to_owned(),
            "-D".to_owned(),
            run_text.clone(),
            "-c".to_owned(),
            format!("{run_text}/config.json"),
            "--disable-color".to_owned(),
        ]
    );
    // The marker says what this run may leave behind.
    assert_eq!(
        fs::read_to_string(install.layout.run_marker()).unwrap(),
        "system_proxy=7890\n"
    );
    // The account's own state, private.
    let user_dir = install.layout.user_dir(&me().to_string()).unwrap();
    assert!(verify::dir_only(&user_dir, Role::Private, &install.trust).is_ok());
    // Once it is up, the helper does what sing-box's sandbox denies it: the
    // system proxy, and the DNS flush.
    install.wait_for_start();
    assert_eq!(
        install.starts(),
        [AfterStart {
            set_proxy: Some(7890),
            flush_dns: true
        }]
    );

    process.stop();
    assert_eq!(
        seen.exit(),
        Some(ExitInfo {
            code: Some(0),
            signal: None
        })
    );
    assert!(seen.lines().contains(&"stopping".to_owned()));
    // The helper set the proxy, so it resets it, after a clean stop too.
    assert_eq!(
        install.cleanups(),
        [Cleanup {
            reset_proxy: Some(7890),
            flush_dns: true
        }]
    );
    assert_eq!(install.order(), ["after_start", "cleanup"]);
    assert!(!install.layout.run_marker().exists());
    assert!(!run_dir.exists());
}

#[test]
fn a_sing_box_that_ignores_sigterm_is_killed_and_its_proxy_reset() {
    let Some(install) = Install::new("sup-kill", STUBBORN) else {
        return;
    };
    let supervisor = install.start().unwrap();
    let (run_dir, seen, process) = run(&supervisor, &me().to_string(), true);
    let mut process = process.unwrap();
    seen.wait_for_line("sing-box started");
    let began = Instant::now();
    process.stop();
    assert!(began.elapsed() < Duration::from_secs(10));
    assert_eq!(
        seen.exit(),
        Some(ExitInfo {
            code: None,
            signal: Some(libc::SIGKILL)
        })
    );
    assert_eq!(
        install.cleanups(),
        [Cleanup {
            reset_proxy: Some(7890),
            flush_dns: true
        }]
    );
    // It never came up: nothing was set up after it, and the reset is
    // the conservative one either way.
    assert!(install.starts().is_empty());
    assert!(!run_dir.exists());
}

/// A stop while the helper still waits for sing-box to come up: nothing
/// is set up after it, and the cleanup still runs.
#[test]
fn a_run_stopped_before_it_is_up_sets_nothing_up() {
    let Some(install) = Install::new("sup-not-up", POLITE) else {
        return;
    };
    let supervisor = install.start().unwrap();
    let (_, seen, process) = run(&supervisor, &me().to_string(), true);
    let mut process = process.unwrap();
    seen.wait_for_line("sing-box started");
    let began = Instant::now();
    process.stop();
    assert!(began.elapsed() < Duration::from_secs(5));
    assert!(install.starts().is_empty());
    assert_eq!(install.order(), ["cleanup"]);
}

#[test]
fn a_sing_box_that_dies_on_its_own_is_cleaned_up_after() {
    let Some(install) = Install::new("sup-crash", CRASHING) else {
        return;
    };
    let supervisor = install.start().unwrap();
    let (run_dir, seen, process) = run(&supervisor, &me().to_string(), false);
    let mut process = process.unwrap();
    assert_eq!(
        seen.wait_for_exit(),
        ExitInfo {
            code: Some(7),
            signal: None
        }
    );
    // Stopping what has exited returns at once, and changes nothing.
    process.stop();
    // No proxy was asked for: DNS only.
    assert_eq!(
        install.cleanups(),
        [Cleanup {
            reset_proxy: None,
            flush_dns: true
        }]
    );
    assert!(install.starts().is_empty());
    assert!(!install.layout.run_marker().exists());
    assert!(!run_dir.exists());
}

/// sing-box runs under its sandbox or not at all: a sandbox-exec that is
/// missing or others may write fails the start, before anything runs; one
/// that can't apply the profile never runs sing-box, and the run ends with
/// its words and its exit code.
#[test]
fn sing_box_never_runs_without_its_sandbox() {
    let Some(install) = Install::new("sup-sandbox", POLITE) else {
        return;
    };
    let supervisor = install.start().unwrap();

    let refused_before_the_spawn = |what: &str| {
        let (run_dir, seen, process) = run(&supervisor, &me().to_string(), true);
        let error = process.err().expect(what);
        assert!(
            error
                .0
                .contains("sandbox can't be applied, so it doesn't start"),
            "{what}: {error}"
        );
        assert_eq!(seen.exit(), None, "{what}");
        assert!(!run_dir.exists(), "{what}");
        assert!(!install.layout.run_marker().exists(), "{what}");
        assert!(!install.sandbox_record.exists(), "{what}: nothing ran");
    };
    fs::set_permissions(&install.sandbox_exec, fs::Permissions::from_mode(0o775)).unwrap();
    refused_before_the_spawn("a group-writable sandbox-exec");
    fs::remove_file(&install.sandbox_exec).unwrap();
    refused_before_the_spawn("no sandbox-exec");

    install.put_sandbox_exec(SANDBOX_EXEC_REFUSING);
    let (run_dir, seen, process) = run(&supervisor, &me().to_string(), false);
    let mut process = process.unwrap();
    assert_eq!(
        seen.wait_for_exit(),
        ExitInfo {
            code: Some(65),
            signal: None
        }
    );
    process.stop();
    let lines = seen.lines();
    assert_eq!(
        lines,
        ["sandbox-exec: the profile was refused"],
        "{lines:?}"
    );
    assert!(!run_dir.exists());
    assert!(!install.layout.run_marker().exists());
    assert_eq!(
        install.cleanups(),
        [Cleanup {
            reset_proxy: None,
            flush_dns: true
        }]
    );
}

/// The real `/usr/bin/sandbox-exec` takes the shipped profile and its
/// parameters, and runs sing-box under them, in the PID it was spawned as
/// (unprivileged here: the measuring profile denies nothing).
#[cfg(target_os = "macos")]
#[test]
fn the_real_sandbox_exec_runs_sing_box_under_the_shipped_profile() {
    let Some(install) = Install::new("sup-real-sandbox", POLITE) else {
        return;
    };
    let mut setup = install.setup();
    setup.sandbox_exec = PathBuf::from(sandboxplan::SANDBOX_EXEC);
    let supervisor = PosixSupervisor::start(setup, 8 * 1024).unwrap();
    let (run_dir, seen, process) = run(&supervisor, &me().to_string(), false);
    let mut process = process.unwrap();
    seen.wait_for_line("sing-box started");
    let run_text = run_dir.to_str().unwrap().to_owned();
    assert_eq!(
        seen.lines()[..6],
        [
            "arg=run".to_owned(),
            "arg=-D".to_owned(),
            format!("arg={run_text}"),
            "arg=-c".to_owned(),
            format!("arg={run_text}/config.json"),
            "arg=--disable-color".to_owned(),
        ]
    );
    process.stop();
    assert_eq!(
        seen.exit(),
        Some(ExitInfo {
            code: Some(0),
            signal: None
        }),
        "{:?}",
        seen.lines()
    );
}

/// ADR 0006 rule 3: checked on every spawn, not just at start.
#[test]
fn a_sing_box_swapped_after_the_start_is_refused_at_the_spawn() {
    let Some(install) = Install::new("sup-swap", POLITE) else {
        return;
    };
    let supervisor = install.start().unwrap();
    fs::write(install.sing_box(), "#!/bin/sh\necho evil\n").unwrap();
    let (run_dir, seen, process) = run(&supervisor, &me().to_string(), false);
    let error = process.err().unwrap();
    assert!(
        error.0.contains("does not match the install manifest"),
        "{error}"
    );
    assert_eq!(seen.exit(), None);
    assert!(!run_dir.exists());
    assert!(!install.layout.run_marker().exists());
}

#[test]
fn a_broken_install_refuses_to_start_with_its_exit_code() {
    type Break = fn(&Install);
    let cases: [(&str, Break, i32); 6] = [
        (
            "sup-tampered",
            |install| {
                let mut content = fs::read(install.sing_box()).unwrap();
                content.push(b'\n');
                fs::write(install.sing_box(), content).unwrap();
            },
            exit::MANIFEST_REFUSED,
        ),
        (
            "sup-bin-writable",
            |install| {
                fs::set_permissions(
                    install.layout.helper_dir(),
                    fs::Permissions::from_mode(0o757),
                )
                .unwrap()
            },
            exit::HELPER_DIR_REFUSED,
        ),
        (
            "sup-sing-box-writable",
            |install| {
                fs::set_permissions(install.sing_box(), fs::Permissions::from_mode(0o775)).unwrap()
            },
            exit::HELPER_DIR_REFUSED,
        ),
        (
            "sup-state-readable",
            |install| {
                fs::set_permissions(
                    install.layout.state_dir(),
                    fs::Permissions::from_mode(0o704),
                )
                .unwrap()
            },
            exit::STATE_DIR_REFUSED,
        ),
        (
            "sup-manifest",
            |install| fs::write(install.layout.manifest_file(), "{}").unwrap(),
            exit::MANIFEST_REFUSED,
        ),
        (
            "sup-link",
            |install| {
                let real = install.layout.helper_dir().with_file_name("real-sing-box");
                fs::rename(install.sing_box(), &real).unwrap();
                std::os::unix::fs::symlink(&real, install.sing_box()).unwrap();
            },
            exit::HELPER_DIR_REFUSED,
        ),
    ];
    for (tag, break_it, code) in cases {
        let Some(install) = Install::new(tag, POLITE) else {
            return;
        };
        assert!(install.start().is_ok(), "{tag}: sound before");
        break_it(&install);
        let error = install.start().err().unwrap();
        assert_eq!(error.0, code, "{tag}: {}", error.1);
    }
}

#[test]
fn the_owner_record_is_read_strictly() {
    let Some(install) = Install::new("sup-owner", POLITE) else {
        return;
    };
    let supervisor = install.start().unwrap();
    assert_eq!(supervisor.owner(), Ok(me()));
    install.write_owner("501\n");
    assert_eq!(supervisor.owner(), Ok(501), "read again, every time");
    install.write_owner("not a uid\n");
    assert!(supervisor.owner().is_err());
    install.write_owner("501\n");
    fs::set_permissions(
        install.layout.owner_file(),
        fs::Permissions::from_mode(0o620),
    )
    .unwrap();
    assert!(supervisor
        .owner()
        .unwrap_err()
        .contains("writable by its group"));
    fs::remove_file(install.layout.owner_file()).unwrap();
    assert!(supervisor.owner().is_err());
}

/// A helper that died with a run cleans up after it when it next starts,
/// before serving anyone.
#[test]
fn a_crash_marker_is_cleaned_up_after_at_start() {
    let Some(install) = Install::new("sup-marker", POLITE) else {
        return;
    };
    fs::write(install.layout.run_marker(), "system_proxy=7890\n").unwrap();
    let stale_run = install.layout.run_dir("0123");
    fs::create_dir_all(&stale_run).unwrap();
    install.start().unwrap();
    assert_eq!(
        install.cleanups(),
        [Cleanup {
            reset_proxy: Some(7890),
            flush_dns: true
        }]
    );
    assert!(!install.layout.run_marker().exists());
    assert!(!stale_run.exists());

    // A marker it can't read: DNS only.
    fs::write(install.layout.run_marker(), "garbage").unwrap();
    install.start().unwrap();
    assert_eq!(
        install.cleanups()[1],
        Cleanup {
            reset_proxy: None,
            flush_dns: true
        }
    );
    // No marker, nothing to undo.
    install.start().unwrap();
    assert_eq!(install.cleanups().len(), 2);
}

#[test]
fn only_uids_get_a_state_directory() {
    let Some(install) = Install::new("sup-uid", POLITE) else {
        return;
    };
    let supervisor = install.start().unwrap();
    for bad in ["S-1-5-18", "../0", "0501", ""] {
        assert!(supervisor.prepare_run(bad).is_err(), "{bad}");
    }
    assert!(supervisor.prepare_run("501").is_ok());
}
