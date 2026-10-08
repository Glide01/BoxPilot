//! The POSIX supervisor over a real tree and a real child: a shell script
//! standing in for sing-box (tests only; the helper never runs a shell),
//! installed as the install script would, with its manifest.

use super::*;
use crate::helper::RunEvents;
use crate::manifest::sha256_hex;
use crate::runcfg::{self, SystemProxy};
use crate::testing::TempDir;
use boxpilot_protocol::{ExitInfo, StartRequest, TunOptions};
use serde_json::json;
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

/// A helper tree as the install leaves it: `bin` (sing-box, manifest) and
/// `state` (0700, the owner record), all this test's user's.
struct Install {
    _temp: TempDir,
    layout: Layout,
    trust: Trust,
    cleanups: Arc<Mutex<Vec<Cleanup>>>,
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
        let install = Self {
            _temp: temp,
            layout,
            trust,
            cleanups: Arc::new(Mutex::new(Vec::new())),
        };
        install.put_sing_box(script);
        install.write_owner(&format!("{}\n", me()));
        Some(install)
    }

    fn sing_box(&self) -> PathBuf {
        self.layout.helper_file("sing-box")
    }

    fn put_sing_box(&self, script: &str) {
        let content = format!("#!/bin/sh\n{script}\n");
        fs::write(self.sing_box(), &content).unwrap();
        fs::set_permissions(self.sing_box(), fs::Permissions::from_mode(0o755)).unwrap();
        let manifest = json!({
            "manifest_version": 1,
            "sing_box": {
                "file": "sing-box",
                "version": "1.14.2",
                "sha256": sha256_hex(content.as_bytes()).unwrap()
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
        let cleanups = self.cleanups.clone();
        Setup {
            layout: self.layout.clone(),
            trust: self.trust.clone(),
            own_exe: None,
            cleanup: Arc::new(move |plan: &Cleanup| cleanups.lock().unwrap().push(*plan)),
            stop_grace: Duration::from_millis(300),
        }
    }

    fn start(&self) -> Result<PosixSupervisor, StartError> {
        PosixSupervisor::start(self.setup(), 8 * 1024)
    }

    fn cleanups(&self) -> Vec<Cleanup> {
        self.cleanups.lock().unwrap().clone()
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

/// Prepare a run for `uid` with a minimal config, and spawn it.
fn run(
    supervisor: &PosixSupervisor,
    uid: &str,
    system_proxy: bool,
) -> (PathBuf, Arc<Seen>, Result<Box<dyn Process>, HelperError>) {
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
        SystemProxy::AsRequested,
        || Ok((41234, ())),
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
    let (run_dir, seen, process) = run(&supervisor, &me().to_string(), true);
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
    // The marker says what this run may leave behind.
    assert_eq!(
        fs::read_to_string(install.layout.run_marker()).unwrap(),
        "system_proxy=7890\n"
    );
    // The account's own state, private.
    let user_dir = install.layout.user_dir(&me().to_string()).unwrap();
    assert!(verify::dir_only(&user_dir, Role::Private, &install.trust).is_ok());

    process.stop();
    assert_eq!(
        seen.exit(),
        Some(ExitInfo {
            code: Some(0),
            signal: None
        })
    );
    assert!(seen.lines().contains(&"stopping".to_owned()));
    // sing-box unset its proxy itself: DNS only.
    assert_eq!(
        install.cleanups(),
        [Cleanup {
            reset_proxy: None,
            flush_dns: true
        }]
    );
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
    assert!(!run_dir.exists());
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
    // No proxy was set: DNS only.
    assert_eq!(
        install.cleanups(),
        [Cleanup {
            reset_proxy: None,
            flush_dns: true
        }]
    );
    assert!(!install.layout.run_marker().exists());
    assert!(!run_dir.exists());
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
