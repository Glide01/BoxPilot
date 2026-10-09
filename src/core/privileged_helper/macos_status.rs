//! The macOS helper's state (ADR 0006 rule 7), as Settings › TUN shows it
//! and TUN's availability follows it: installed or not, turned off, current
//! or not, and why it can't be reached.
//!
//! "Current" is judged against what this BoxPilot.app ships: the protocol
//! version and the hash of the helper's sing-box, which `hello` reports
//! (the ADR's rule), and the installed helper and launchd plist, which must
//! be the bundle's byte for byte. The ADR names only the first two; the
//! helper's own file is BoxPilot's task too (it is the privilege BoxPilot
//! lends), so a BoxPilot that ships a fixed helper asks to reinstall even
//! when its sing-box is unchanged.
//!
//! The judgements are pure and tested on every OS. [`probe`] is the I/O (a
//! stat, a connect and `hello`, reads of the bundle and the installed
//! files, and `launchctl print` for a helper that refuses to run), and
//! blocks, so it runs off the UI thread. Built everywhere; only macOS uses
//! it ([`super::HELPER_INSTALLED_BY_APP`]).

use super::client::{HelperConnection, HelperFailure};
use super::macos_install::install_script_path;
use super::{exit_code_message_on, open, HelperOs, OpenError};
use crate::i18n::s;
use boxpilot_protocol::endpoint::exit;
use boxpilot_protocol::endpoint::macos::{
    BUNDLE_HELPER, BUNDLE_PAYLOAD_DIR, HELPER_PATH, LABEL, PLIST_FILE, PLIST_PATH,
};
use boxpilot_protocol::{ErrorCode, HelloReply, PROTOCOL_VERSION};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// `launchctl`, by its absolute path.
pub const LAUNCHCTL: &str = "/bin/launchctl";

/// The install manifest's file name, in the bundle's
/// `endpoint::macos::BUNDLE_PAYLOAD_DIR` as in the helper's `bin`
/// (`endpoint::macos::MANIFEST_PATH`).
pub const MANIFEST_FILE: &str = "manifest.json";

/// How long [`probe`] waits for launchd to record the exit of a helper
/// that turned it away: it closes its waiting clients before it exits.
const EXIT_WAIT: Duration = Duration::from_secs(3);

/// The macOS helper's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelperStatus {
    /// Not looked at yet: the probe runs at startup, off the UI thread.
    Unknown,
    /// No helper on this Mac: no launchd plist.
    NotInstalled,
    /// Installed, but launchd doesn't run it: turned off in System Settings
    /// › General › Login Items, or unloaded (`launchctl bootout`). It counts
    /// as not installed (ADR 0006 rule 7).
    TurnedOff,
    /// Installed, what this BoxPilot ships, and this account may start TUN.
    Ready,
    /// Installed, but another account owns it (rule 4), so this one may not
    /// start. Installing it again makes this account the owner.
    OtherOwner,
    /// Installed, but not what this BoxPilot ships: another protocol
    /// version, sing-box, helper or plist. Reinstalling updates it.
    Stale,
    /// Installed, but it refuses to run: a broken install, with its exit
    /// code (`endpoint::exit`). Reinstalling repairs it.
    Broken(i32),
    /// Installed, but it couldn't be asked: the message.
    Unreachable(String),
}

/// What a TUN start through the helper does, by the helper's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartGate {
    /// Start. The helper is ready, or its state isn't known, and then the
    /// start itself says what went wrong.
    Start,
    /// Ask to install it (or reinstall it) first, as Linux asks for its
    /// TUN grant (ADR 0003): nothing starts until the user agrees.
    Ask,
    /// Start nothing, and say why.
    Refuse(String),
}

impl HelperStatus {
    /// Whether TUN can be chosen: the helper is installed and not turned
    /// off, or not known yet (a start looks again). A helper that is stale,
    /// owned by another account or broken still counts: a start asks to
    /// reinstall it.
    pub fn tun_available(&self) -> bool {
        !matches!(self, HelperStatus::NotInstalled | HelperStatus::TurnedOff)
    }

    /// Whether there is a helper to reinstall or remove.
    pub fn installed(&self) -> bool {
        !matches!(self, HelperStatus::Unknown | HelperStatus::NotInstalled)
    }

    /// What a TUN start does. `can_install`: BoxPilot runs from its app
    /// bundle, which carries what an install needs.
    pub fn start_gate(&self, can_install: bool) -> StartGate {
        let h = &s().helper;
        match self {
            HelperStatus::Ready | HelperStatus::Unknown => StartGate::Start,
            HelperStatus::Unreachable(message) => StartGate::Refuse(message.clone()),
            _ if can_install => StartGate::Ask,
            HelperStatus::NotInstalled => StartGate::Refuse(h.mac_no_bundle.to_string()),
            HelperStatus::TurnedOff => StartGate::Refuse(h.mac_turned_off.to_string()),
            HelperStatus::OtherOwner => StartGate::Refuse(h.mac_not_allowed.to_string()),
            HelperStatus::Stale => StartGate::Refuse(h.mac_version_mismatch.to_string()),
            HelperStatus::Broken(code) => {
                StartGate::Refuse(exit_code_message_on(*code, HelperOs::MacOs))
            }
        }
    }

    /// The words of the prompt before installing it from this state: what
    /// installing does here (a first install, a reinstall, taking it over
    /// from another account, or turning it back on).
    pub fn install_prompt(&self) -> InstallPrompt {
        let (d, t) = (&s().dialogs, &s().settings);
        let (title, body) = match self {
            HelperStatus::Unknown | HelperStatus::NotInstalled => {
                (d.helper_install_title, d.helper_install_body)
            }
            HelperStatus::OtherOwner => (d.helper_install_title, d.helper_take_over_body),
            HelperStatus::TurnedOff => (d.helper_reinstall_title, d.helper_turn_on_body),
            HelperStatus::Ready
            | HelperStatus::Stale
            | HelperStatus::Broken(_)
            | HelperStatus::Unreachable(_) => (d.helper_reinstall_title, d.helper_reinstall_body),
        };
        InstallPrompt {
            title,
            body,
            ok: if self.installed() {
                t.reinstall_helper
            } else {
                t.install_helper
            },
        }
    }

    /// The state in the user's words (Settings › TUN).
    pub fn message(&self) -> String {
        let t = &s().settings;
        match self {
            HelperStatus::Unknown => t.helper_checking.to_string(),
            HelperStatus::NotInstalled => t.helper_not_installed.to_string(),
            HelperStatus::TurnedOff => t.helper_turned_off.to_string(),
            HelperStatus::Ready => t.helper_ready.to_string(),
            HelperStatus::OtherOwner => t.helper_other_owner.to_string(),
            HelperStatus::Stale => t.helper_stale.to_string(),
            HelperStatus::Broken(code) => {
                (t.helper_broken)(&exit_code_message_on(*code, HelperOs::MacOs))
            }
            HelperStatus::Unreachable(message) => message.clone(),
        }
    }
}

/// The prompt before installing the helper: its title, its text, and its
/// OK button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstallPrompt {
    pub title: &'static str,
    pub body: &'static str,
    pub ok: &'static str,
}

/// What this BoxPilot.app ships, to judge the installed helper by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shipped {
    /// Its sing-box's SHA-256 (the bundle's manifest), lowercase hex.
    pub sing_box_sha256: String,
    /// The installed helper and plist are the bundle's, byte for byte.
    pub files_match: bool,
}

/// The state of a helper that answered `hello`. `shipped`: what this
/// BoxPilot.app ships; without a bundle (`None`), currency can't be judged,
/// and a helper that serves this account is ready.
pub fn judge(hello: &HelloReply, shipped: Option<&Shipped>) -> HelperStatus {
    if hello.protocol_version != PROTOCOL_VERSION {
        return HelperStatus::Stale;
    }
    // First: installing again would take it over from another account,
    // which the user must hear before anything else.
    if !hello.may_start {
        return HelperStatus::OtherOwner;
    }
    if let Some(shipped) = shipped {
        if !hello
            .sing_box_sha256
            .eq_ignore_ascii_case(&shipped.sing_box_sha256)
            || !shipped.files_match
        {
            return HelperStatus::Stale;
        }
    }
    HelperStatus::Ready
}

/// What launchd says of the helper's job (`launchctl print`, which any
/// account may read, root or not), for a helper that ended the connection
/// unanswered (`Lost`) or never answered (`TimedOut`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    /// It runs: alive, but it didn't answer in time (still cleaning up after
    /// a crash, say), or turned this connection away (its read-only
    /// connections all taken).
    Running,
    /// It doesn't run, and last exited with this code, if it has exited at
    /// all.
    NotRunning(Option<i32>),
    /// launchctl couldn't say.
    Unknown,
}

/// The state of a helper that didn't answer `hello`. `job`: what launchd
/// says of it, for a helper that ended the connection unanswered (`Lost`)
/// or never answered (`TimedOut`). One that refuses to run turns its
/// waiting clients away and exits with its code: broken. Not running, with
/// no such code, launchd doesn't run it: turned off. Still running, or
/// launchd can't say: neither, it just didn't answer this time, and the
/// next look (every TUN start takes one) asks again.
pub fn failure_status(failure: HelperFailure, job: JobState) -> HelperStatus {
    match failure {
        HelperFailure::Open(OpenError::NotInstalled) => HelperStatus::NotInstalled,
        HelperFailure::Open(OpenError::Disabled) => HelperStatus::TurnedOff,
        HelperFailure::Open(error) => HelperStatus::Unreachable(error.message_on(HelperOs::MacOs)),
        HelperFailure::NotAllowed => HelperStatus::OtherOwner,
        // Another protocol version, or a reply this build can't read: a
        // helper from another BoxPilot.
        HelperFailure::Error {
            code: ErrorCode::VersionMismatch,
            ..
        }
        | HelperFailure::BadReply(_) => HelperStatus::Stale,
        HelperFailure::Lost | HelperFailure::TimedOut => match job {
            JobState::NotRunning(Some(code)) if code != exit::OK => HelperStatus::Broken(code),
            JobState::NotRunning(_) => HelperStatus::TurnedOff,
            JobState::Running | JobState::Unknown => {
                HelperStatus::Unreachable(failure.message_on(HelperOs::MacOs))
            }
        },
        other => HelperStatus::Unreachable(other.message_on(HelperOs::MacOs)),
    }
}

/// The sing-box hash an install manifest names (`sing_box.sha256`, as
/// `packaging/macos/build-dmg.sh` writes it and the helper reads it),
/// lowercase; `None` unless it is 64 hex digits.
pub fn manifest_sha256(manifest: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(manifest).ok()?;
    let hash = value.get("sing_box")?.get("sha256")?.as_str()?;
    (hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| hash.to_ascii_lowercase())
}

/// What `launchctl print system/<label>` says of the helper's job: whether
/// it runs now (it has a `pid`), and its last exit code, if it has exited
/// (`last exit code = 12`, or `= 78: EX_CONFIG` on newer macOS; `(never
/// exited)` is none). The first of each counts, as the smoke test reads it.
pub fn parse_launchctl_print(text: &str) -> (bool, Option<i32>) {
    let mut running = None;
    let mut last_exit = None;
    for line in text.lines().map(str::trim) {
        if running.is_none() {
            if let Some(pid) = line.strip_prefix("pid = ") {
                running = Some(pid.trim().parse::<u32>().is_ok());
            }
        }
        if last_exit.is_none() {
            if let Some(rest) = line.strip_prefix("last exit code = ") {
                let digits: String = rest
                    .chars()
                    .enumerate()
                    .take_while(|(i, c)| c.is_ascii_digit() || (*i == 0 && *c == '-'))
                    .map(|(_, c)| c)
                    .collect();
                last_exit = Some(digits.parse::<i32>().ok());
            }
        }
    }
    (running.unwrap_or(false), last_exit.flatten())
}

/// `…/BoxPilot.app/Contents` from the path of the binary inside it,
/// `…/BoxPilot.app/Contents/MacOS/<binary>`; `None` for a binary that isn't
/// in an app bundle.
pub fn bundle_contents(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let app = contents.parent()?;
    let bundled = macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && app.extension()? == "app";
    bundled.then(|| contents.to_path_buf())
}

// ---- The I/O ----

/// Whether the helper is installed: its launchd plist is there. One stat.
pub fn plist_installed() -> bool {
    Path::new(PLIST_PATH).exists()
}

/// BoxPilot.app's `Contents` directory, when BoxPilot runs from an app
/// bundle that carries the helper's payload: what the helper installs from,
/// and what "current" means. `None` for a bare binary (`cargo run`), which
/// can't install the helper.
pub fn app_bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let contents = bundle_contents(&exe)?;
    install_script_path(&contents).is_file().then_some(contents)
}

/// One look at the helper, as the GUI takes it: find BoxPilot.app, then
/// [`probe`] the helper against it. Blocking.
pub fn look() -> (Option<PathBuf>, HelperStatus) {
    let bundle = app_bundle();
    let status = probe(bundle.as_deref());
    (bundle, status)
}

/// Look at the helper: is it installed (its plist), and if so, ask it
/// `hello` and judge the answer against what `bundle` (BoxPilot.app's
/// `Contents`) ships. Blocking: a connect, `hello` (launchd may be starting
/// the helper, which verifies its install first; at most `HELLO_TIMEOUT`),
/// then reads of the bundle and the installed files. Never `Unknown`.
pub fn probe(bundle: Option<&Path>) -> HelperStatus {
    if !plist_installed() {
        return HelperStatus::NotInstalled;
    }
    let hello = match open() {
        Ok(io) => {
            let (connection, _events) = HelperConnection::new(io);
            // Dropping the connection closes it; the helper exits when idle.
            connection.hello()
        }
        Err(error) => Err(HelperFailure::Open(error)),
    };
    match hello {
        Ok(hello) => judge(&hello, bundle.and_then(shipped).as_ref()),
        Err(failure @ (HelperFailure::Lost | HelperFailure::TimedOut)) => {
            failure_status(failure, job_state())
        }
        Err(failure) => failure_status(failure, JobState::Unknown),
    }
}

/// What the bundle at `contents` ships, compared with what is installed.
/// `None` if its manifest can't be read: nothing to judge by.
fn shipped(contents: &Path) -> Option<Shipped> {
    let payload = contents.join(BUNDLE_PAYLOAD_DIR);
    let manifest = fs::read_to_string(payload.join(MANIFEST_FILE)).ok()?;
    let sing_box_sha256 = manifest_sha256(&manifest)?;
    let same = |installed: &str, bundled: PathBuf| match (fs::read(installed), fs::read(bundled)) {
        (Ok(installed), Ok(bundled)) => installed == bundled,
        _ => false,
    };
    Some(Shipped {
        sing_box_sha256,
        files_match: same(HELPER_PATH, contents.join(BUNDLE_HELPER))
            && same(PLIST_PATH, payload.join(PLIST_FILE)),
    })
}

/// The helper's job as launchd has it, from `launchctl print` (by its
/// absolute path, no shell): running, or not, with its last exit code. It
/// needs no privilege to read: CI's macOS job reads a broken install's code
/// this way as an account without root, and fails if it can't. A helper
/// that refuses to run turns its waiting clients away before it exits, so
/// this waits for the exit, briefly: still running after that, it is alive.
fn job_state() -> JobState {
    let deadline = Instant::now() + EXIT_WAIT;
    loop {
        let output = Command::new(LAUNCHCTL)
            .arg("print")
            .arg(format!("system/{LABEL}"))
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output();
        let output = match output {
            Ok(output) if output.status.success() => output,
            _ => return JobState::Unknown,
        };
        let (running, last_exit) = parse_launchctl_print(&String::from_utf8_lossy(&output.stdout));
        if !running {
            return JobState::NotRunning(last_exit);
        }
        if Instant::now() >= deadline {
            return JobState::Running;
        }
        thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boxpilot_protocol::endpoint::macos::MANIFEST_PATH;

    const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn hello(may_start: bool, sha: &str) -> HelloReply {
        HelloReply {
            protocol_version: PROTOCOL_VERSION,
            helper_version: "0.1.0".into(),
            sing_box_version: "1.14.2".into(),
            sing_box_sha256: sha.into(),
            may_start,
        }
    }

    fn shipped(files_match: bool) -> Shipped {
        Shipped {
            sing_box_sha256: SHA.into(),
            files_match,
        }
    }

    #[test]
    fn a_helper_is_current_when_everything_is_the_bundles() {
        assert_eq!(
            judge(&hello(true, SHA), Some(&shipped(true))),
            HelperStatus::Ready
        );
        // The helper reports its hash in lowercase; case doesn't matter.
        assert_eq!(
            judge(&hello(true, &SHA.to_uppercase()), Some(&shipped(true))),
            HelperStatus::Ready
        );
        // Without a bundle there is nothing to compare with.
        assert_eq!(
            judge(&hello(true, &"ab".repeat(32)), None),
            HelperStatus::Ready
        );
    }

    #[test]
    fn another_sing_box_helper_or_protocol_is_stale() {
        assert_eq!(
            judge(&hello(true, &"ab".repeat(32)), Some(&shipped(true))),
            HelperStatus::Stale
        );
        assert_eq!(
            judge(&hello(true, SHA), Some(&shipped(false))),
            HelperStatus::Stale
        );
        let mut other_protocol = hello(true, SHA);
        other_protocol.protocol_version = PROTOCOL_VERSION + 1;
        assert_eq!(
            judge(&other_protocol, Some(&shipped(true))),
            HelperStatus::Stale
        );
    }

    /// Another account's helper says so first: reinstalling takes it over.
    #[test]
    fn another_accounts_helper_is_said_first() {
        assert_eq!(
            judge(&hello(false, SHA), Some(&shipped(true))),
            HelperStatus::OtherOwner
        );
        assert_eq!(
            judge(&hello(false, &"ab".repeat(32)), Some(&shipped(false))),
            HelperStatus::OtherOwner
        );
    }

    #[test]
    fn failures_map_to_states() {
        use HelperFailure::*;
        use JobState::NotRunning;
        let unknown = JobState::Unknown;
        assert_eq!(
            failure_status(Open(OpenError::NotInstalled), unknown),
            HelperStatus::NotInstalled
        );
        assert_eq!(
            failure_status(Open(OpenError::Disabled), unknown),
            HelperStatus::TurnedOff
        );
        assert_eq!(
            failure_status(Open(OpenError::ConnectDenied), unknown),
            HelperStatus::Unreachable(OpenError::ConnectDenied.message_on(HelperOs::MacOs))
        );
        assert_eq!(
            failure_status(NotAllowed, unknown),
            HelperStatus::OtherOwner
        );
        assert_eq!(
            failure_status(
                Error {
                    code: ErrorCode::VersionMismatch,
                    message: "x".into()
                },
                unknown
            ),
            HelperStatus::Stale
        );
        assert_eq!(
            failure_status(BadReply("protocol version 2".into()), unknown),
            HelperStatus::Stale
        );
        // Turned away: the exit code says why; without one, launchd isn't
        // running it.
        assert_eq!(
            failure_status(Lost, NotRunning(Some(exit::MANIFEST_REFUSED))),
            HelperStatus::Broken(exit::MANIFEST_REFUSED)
        );
        assert_eq!(
            failure_status(TimedOut, NotRunning(Some(exit::STATE_DIR_REFUSED))),
            HelperStatus::Broken(exit::STATE_DIR_REFUSED)
        );
        assert_eq!(
            failure_status(Lost, NotRunning(Some(exit::OK))),
            HelperStatus::TurnedOff
        );
        assert_eq!(
            failure_status(TimedOut, NotRunning(None)),
            HelperStatus::TurnedOff
        );
        assert!(matches!(
            failure_status(Io("broken pipe".into()), unknown),
            HelperStatus::Unreachable(message) if message.contains("broken pipe")
        ));
    }

    /// A helper that runs but didn't answer (busy cleaning up after a
    /// crash, or its read-only connections all taken) isn't turned off, nor
    /// is one launchd can't say anything about: TUN stays available, and
    /// the next look asks again.
    #[test]
    fn a_live_helper_that_did_not_answer_is_not_turned_off() {
        use HelperFailure::*;
        for (failure, job) in [
            (TimedOut, JobState::Running),
            (Lost, JobState::Running),
            (TimedOut, JobState::Unknown),
            (Lost, JobState::Unknown),
        ] {
            let status = failure_status(failure.clone(), job);
            assert_eq!(
                status,
                HelperStatus::Unreachable(failure.message_on(HelperOs::MacOs)),
                "{failure:?} {job:?}"
            );
            assert!(status.tun_available(), "{failure:?} {job:?}");
        }
    }

    /// TUN can be chosen unless the helper is known to be missing or off.
    #[test]
    fn tun_is_available_unless_the_helper_is_missing_or_off() {
        for (status, available, installed) in [
            (HelperStatus::Unknown, true, false),
            (HelperStatus::NotInstalled, false, false),
            (HelperStatus::TurnedOff, false, true),
            (HelperStatus::Ready, true, true),
            (HelperStatus::OtherOwner, true, true),
            (HelperStatus::Stale, true, true),
            (HelperStatus::Broken(12), true, true),
            (HelperStatus::Unreachable("x".into()), true, true),
        ] {
            assert_eq!(status.tun_available(), available, "{status:?}");
            assert_eq!(status.installed(), installed, "{status:?}");
        }
    }

    /// A start asks to install or reinstall when BoxPilot can; otherwise it
    /// says what is wrong, in macOS's words.
    #[test]
    fn a_start_asks_or_says_why() {
        let h = &crate::i18n::EN.helper;
        assert_eq!(HelperStatus::Ready.start_gate(true), StartGate::Start);
        assert_eq!(HelperStatus::Unknown.start_gate(false), StartGate::Start);
        for status in [
            HelperStatus::NotInstalled,
            HelperStatus::TurnedOff,
            HelperStatus::OtherOwner,
            HelperStatus::Stale,
            HelperStatus::Broken(exit::MANIFEST_REFUSED),
        ] {
            assert_eq!(status.start_gate(true), StartGate::Ask, "{status:?}");
        }
        assert_eq!(
            HelperStatus::Unreachable("boom".into()).start_gate(true),
            StartGate::Refuse("boom".into())
        );
        assert_eq!(
            HelperStatus::NotInstalled.start_gate(false),
            StartGate::Refuse(h.mac_no_bundle.into())
        );
        assert_eq!(
            HelperStatus::TurnedOff.start_gate(false),
            StartGate::Refuse(h.mac_turned_off.into())
        );
        assert_eq!(
            HelperStatus::OtherOwner.start_gate(false),
            StartGate::Refuse(h.mac_not_allowed.into())
        );
        assert_eq!(
            HelperStatus::Stale.start_gate(false),
            StartGate::Refuse(h.mac_version_mismatch.into())
        );
        assert_eq!(
            HelperStatus::Broken(exit::MANIFEST_REFUSED).start_gate(false),
            StartGate::Refuse(exit_code_message_on(
                exit::MANIFEST_REFUSED,
                HelperOs::MacOs
            ))
        );
    }

    /// Each state's prompt says what installing does there.
    #[test]
    fn the_install_prompt_fits_the_state() {
        let (d, t) = (&crate::i18n::EN.dialogs, &crate::i18n::EN.settings);
        let prompt = |title, body, ok| InstallPrompt { title, body, ok };
        for (status, expected) in [
            (
                HelperStatus::NotInstalled,
                prompt(
                    d.helper_install_title,
                    d.helper_install_body,
                    t.install_helper,
                ),
            ),
            (
                HelperStatus::OtherOwner,
                prompt(
                    d.helper_install_title,
                    d.helper_take_over_body,
                    t.reinstall_helper,
                ),
            ),
            (
                HelperStatus::TurnedOff,
                prompt(
                    d.helper_reinstall_title,
                    d.helper_turn_on_body,
                    t.reinstall_helper,
                ),
            ),
            (
                HelperStatus::Stale,
                prompt(
                    d.helper_reinstall_title,
                    d.helper_reinstall_body,
                    t.reinstall_helper,
                ),
            ),
            (
                HelperStatus::Broken(exit::MANIFEST_REFUSED),
                prompt(
                    d.helper_reinstall_title,
                    d.helper_reinstall_body,
                    t.reinstall_helper,
                ),
            ),
        ] {
            assert_eq!(status.install_prompt(), expected, "{status:?}");
        }
    }

    #[test]
    fn every_state_reads() {
        let t = &crate::i18n::EN.settings;
        assert_eq!(HelperStatus::Ready.message(), t.helper_ready);
        assert_eq!(HelperStatus::NotInstalled.message(), t.helper_not_installed);
        assert_eq!(
            HelperStatus::Broken(exit::MANIFEST_REFUSED).message(),
            "Installed, but it refuses to run. The privileged helper stopped: its copy of \
             sing-box doesn't match what was installed; reinstall it in Settings › TUN."
        );
        assert_eq!(HelperStatus::Unreachable("why".into()).message(), "why");
    }

    /// The manifest as `build-dmg.sh` writes it.
    #[test]
    fn the_manifest_names_sing_boxs_hash() {
        let manifest = format!(
            "{{\n  \"manifest_version\": 1,\n  \"sing_box\": {{\n    \"file\": \"sing-box\",\n    \
             \"version\": \"1.14.2\",\n    \"sha256\": \"{}\"\n  }},\n  \"extra_files\": []\n}}\n",
            SHA.to_uppercase()
        );
        assert_eq!(manifest_sha256(&manifest).as_deref(), Some(SHA));
        for bad in [
            "",
            "{}",
            r#"{"sing_box": {"sha256": "abc"}}"#,
            r#"{"sing_box": {"sha256": 7}}"#,
            &format!(r#"{{"sing_box": {{"sha256": "{}"}}}}"#, "zz".repeat(32)),
        ] {
            assert_eq!(manifest_sha256(bad), None, "{bad}");
        }
        assert!(MANIFEST_PATH.ends_with(&format!("/{MANIFEST_FILE}")));
    }

    #[test]
    fn launchctl_print_says_running_and_the_last_exit() {
        let stopped = "system/io.github.glide01.boxpilot.helper = {\n\
                       \tactive count = 0\n\
                       \tpath = /Library/LaunchDaemons/io.github.glide01.boxpilot.helper.plist\n\
                       \tstate = not running\n\
                       \tlast exit code = 12\n\
                       }\n";
        assert_eq!(parse_launchctl_print(stopped), (false, Some(12)));
        let running = "\tstate = running\n\tpid = 4242\n\tlast exit code = 0\n";
        assert_eq!(parse_launchctl_print(running), (true, Some(0)));
        let named = "\tlast exit code = 78: EX_CONFIG\n";
        assert_eq!(parse_launchctl_print(named), (false, Some(78)));
        let never = "\tlast exit code = (never exited)\n";
        assert_eq!(parse_launchctl_print(never), (false, None));
        let negative = "\tlast exit code = -9\n";
        assert_eq!(parse_launchctl_print(negative), (false, Some(-9)));
        assert_eq!(parse_launchctl_print(""), (false, None));
    }

    #[test]
    fn only_a_binary_inside_an_app_bundle_has_one() {
        assert_eq!(
            bundle_contents(Path::new(
                "/Applications/BoxPilot.app/Contents/MacOS/BoxPilot"
            )),
            Some(PathBuf::from("/Applications/BoxPilot.app/Contents"))
        );
        // App Translocation runs a quarantined download from a random path:
        // still a bundle.
        assert_eq!(
            bundle_contents(Path::new(
                "/private/var/folders/x/AppTranslocation/1/d/BoxPilot.app/Contents/MacOS/BoxPilot"
            )),
            Some(PathBuf::from(
                "/private/var/folders/x/AppTranslocation/1/d/BoxPilot.app/Contents"
            ))
        );
        for bare in [
            "/home/me/BoxPilot/target/release/box_pilot_gui",
            "/Applications/BoxPilot/Contents/MacOS/BoxPilot",
            "/Applications/BoxPilot.app/MacOS/BoxPilot",
            "BoxPilot",
            "/",
        ] {
            assert_eq!(bundle_contents(Path::new(bare)), None, "{bare}");
        }
    }
}
