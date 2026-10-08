//! sing-box's sandbox on macOS (ADR 0006 rule 2, "Defense in depth"), as
//! pure data: the profile it runs under ([`SING_BOX_PROFILE`]), the
//! parameters that profile is given ([`Params`]), the command line that
//! applies it ([`sandbox_exec_args`]), and the denials it is known to make
//! ([`KNOWN_DENIALS`]). `tokenplan` is Windows' counterpart.
//!
//! **Enforced, deny by default, and measured, not guessed.** The profile
//! denies every operation it doesn't allow, and what it allows is what CI
//! measured sing-box doing under a first profile that allowed and reported
//! everything (`(allow (with report) default)`, on CI's Apple-silicon
//! runner with sing-box 1.14.2): over seven TUN runs (`auto_route`, DNS
//! hijacking, the loopback probes, the `api` service, a rule set sent as
//! an attachment, the GUI client's start, the helper killed under it, the
//! system proxy on and off), 1,625 reports from sing-box itself:
//!
//! - **files:** reads of the system's libraries and dyld's shared cache,
//!   sing-box itself and its directory, its run directory, its account's
//!   `cache.db`, `/dev/null`, `/dev/autofs_nowait`; and, from CoreFoundation
//!   starting up, `/private/etc/master.passwd` and root's
//!   `.CFUserTextEncoding`. Writes only to `${USER_DIR}/cache.db`,
//!   `/dev/null` and `/dev/dtracehelper` (dyld's DTrace probes);
//! - **network:** sockets of `AF_ROUTE` (domain 17, the routes) and
//!   `AF_SYSTEM` (domain 32, utun's kernel control, whose connect carries no
//!   address), binds, inbound and outbound IP traffic;
//! - **system:** `sysctl` reads only (`net.routetable.*`, `hw.*`, `kern.*`,
//!   `machdep.cpu.*`), and the Mach services `com.apple.logd` and
//!   `com.apple.system.notification_center`;
//! - **programs:** `fork`, then `/usr/sbin/networksetup` (the system proxy)
//!   and `/usr/bin/dscacheutil` (sing-tun's DNS flush). `networksetup` in
//!   turn ran `/bin/sh`, `bash` and `cp`, and wrote
//!   `/Library/Preferences/SystemConfiguration`.
//!
//! So sing-box executes nothing: the helper sets the system proxy and
//! flushes DNS itself, outside the sandbox (`cleanup::after_start`,
//! `runcfg::SYSTEM_PROXY`). Letting a root sing-box run `networksetup`
//! would have meant letting it run a shell, or exempting `networksetup`
//! from the sandbox, an unsandboxed root exec on sing-box's word.
//!
//! **TLS and the `local` DNS server, measured too.** CI's smoke profile
//! verifies a certificate (DNS over HTTPS) and resolves through `local`.
//! A round that allowed what they were expected to need only `(with
//! report)` showed what they use: `trustd` and `cfprefsd`'s daemon (with
//! its shared memory) for the certificate, `mDNSResponder`'s socket for
//! `local`. The rest of the guess (`trustd.agent`, `cfprefsd.agent`,
//! `configd`'s DNS configuration) went unused, so it is gone.
//!
//! **How it is applied: `/usr/bin/sandbox-exec`.** The supervisor
//! `posix_spawn`s [`SANDBOX_EXEC`] by its absolute path, with the argv
//! [`sandbox_exec_args`] builds: `-p <profile> -D <NAME>=<value>… --
//! <sing-box> <its arguments>`. sandbox-exec applies the profile to itself,
//! then executes sing-box in its place.
//!
//! - **One process, as before.** An exec keeps the PID, the parent and the
//!   process group: the helper's `Reaper` waits for and signals the PID it
//!   spawned, which is sing-box's, and launchd still ends sing-box with the
//!   helper's process group. The environment built from nothing,
//!   `POSIX_SPAWN_CLOEXEC_DEFAULT`, the signal defaults and the working
//!   directory are set by the helper's `posix_spawn`, as before, and pass
//!   through the exec unchanged.
//! - **Fail-closed.** sandbox-exec runs the command only once the profile
//!   is applied. If it can't apply it (a profile or a parameter it doesn't
//!   take), it says why on stderr, which reaches the starting connection as
//!   a log line, and exits: the run ends before sing-box ever runs. The
//!   supervisor has no other way to start sing-box, so a sandbox-exec that
//!   is missing, or fails verification, fails the start.
//! - **What is verified is what runs.** The supervisor verifies sing-box's
//!   directory chain and hash, then names that same absolute path after
//!   `--`, and an absolute path is executed as it is, never searched for.
//!   It is also the one program the profile lets sandbox-exec execute.
//!   sandbox-exec itself is on the sealed system volume, which SIP
//!   protects, and the supervisor verifies its chain (root's, writable by
//!   nobody else) before every spawn as well. Between the hash and the exec
//!   only root could swap sing-box, as before (`posix::verify`).
//! - **The profile is a constant,** handed over with `-p`, never written to
//!   a file. Its parameters are the helper's own paths only: sing-box's and
//!   its directory, this run's directory and the starting account's state
//!   directory, from `endpoint::macos` and the run plan ([`Params::for_run`]
//!   checks each), never anything from a config or a client. The profile
//!   reads them as `(param "RUN_DIR")`: values are never spliced into its
//!   text, and none may hold a character that could end a profile string
//!   anyway ([`is_plain_path`]).
//!
//! **Not a trampoline.** The other way, the helper re-executing itself in a
//! mode that calls `sandbox_init_with_parameters` and then `execve`s
//! sing-box, would give the root daemon's binary a second entry point (it
//! takes no arguments at all now) and `unsafe` declarations of a function
//! that libsystem_sandbox exports but no SDK header declares, for the same
//! deprecated mechanism underneath. sandbox-exec is deprecated too, but
//! macOS still ships it, and while it does, Apple's own tool compiles and
//! applies the profile. If a macOS drops it, the start fails naming its
//! path, and the trampoline is the way left.

#![forbid(unsafe_code)]

use crate::manifest::is_plain_file_name;
use crate::paths::{is_run_name, is_uid, Layout};
use std::fmt;
use std::path::Path;

/// The tool that applies the profile: macOS's own, on the sealed system
/// volume, in one of `spawnplan::POSIX_PATH`'s SIP-protected directories.
pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// Whether [`SING_BOX_PROFILE`] denies what it doesn't allow. It does.
pub const ENFORCED: bool = true;

/// What the helper's log says of the profile sing-box runs under.
pub const STATUS: &str = if ENFORCED {
    "enforced"
} else {
    "measuring; not enforced yet"
};

/// The profile sing-box runs under, in SBPL, the sandbox's own language
/// (Apple doesn't document it for third parties). Later rules win over
/// earlier ones. Every denial is reported to the system log, where CI reads
/// them (`sandboxreport`); those sing-box is known to make are
/// [`KNOWN_DENIALS`].
pub const SING_BOX_PROFILE: &str = r#"(version 1)
; BoxPilot's privileged helper: sing-box's sandbox (ADR 0006, rule 2,
; "Defense in depth"). Enforced: what isn't allowed below is denied, and
; reported. What is allowed was measured in CI (sandboxplan).
(deny default)

; Programs: sandbox-exec executes sing-box, and nothing else runs. No fork,
; no other program, no shell.
(allow process-exec* (literal (param "SING_BOX")))
(allow signal (target self))

; Reading: any file's metadata (path walking); the contents of the system's
; own files, of sing-box and its directory, of this run and of this
; account's state. Not /Users, /private/var/root, /Library (its keychains,
; the helper's own state), /private/var/db but the time zones, or another
; account's state.
(allow file-read-metadata)
(allow file-read*
       (literal "/")
       (literal (param "SING_BOX"))
       (literal (param "HELPER_DIR"))
       (subpath (param "RUN_DIR"))
       (subpath (param "USER_DIR"))
       (subpath "/System/Library")
       (subpath "/System/Cryptexes")
       (subpath "/System/Volumes/Preboot/Cryptexes")
       (subpath "/usr/lib")
       (subpath "/usr/share")
       (subpath "/private/etc/ssl")
       (literal "/private/etc/hosts")
       (literal "/private/etc/services")
       (literal "/private/etc/protocols")
       (literal "/private/etc/passwd")
       (literal "/private/etc/group")
       (literal "/private/etc/resolv.conf")
       (literal "/private/var/run/resolv.conf")
       (literal "/private/etc/localtime")
       (subpath "/private/var/db/timezone")
       (literal "/dev/null")
       (literal "/dev/random")
       (literal "/dev/urandom")
       (literal "/dev/autofs_nowait"))

; Writing: this run, this account's state, and /dev/null.
(allow file-write*
       (subpath (param "RUN_DIR"))
       (subpath (param "USER_DIR"))
       (literal "/dev/null"))

; Networking is sing-box's job: any IP traffic, the routing socket, and
; utun's kernel control socket, whose connect carries no address. No local
; service's Unix socket but mDNSResponder's (the local DNS server's).
(allow network-bind network-inbound (local ip))
(allow network-outbound)
(deny network-outbound (remote unix-socket))
(allow network-outbound
       (remote unix-socket (path-literal "/private/var/run/mDNSResponder"))
       (remote unix-socket (path-literal "/var/run/mDNSResponder")))
(allow system-socket (socket-domain 17))
(allow system-socket (socket-domain 32))

; The system's state, read only.
(allow sysctl-read)

; Services: the log and notifications.
(allow mach-lookup
       (global-name "com.apple.logd")
       (global-name "com.apple.system.notification_center"))
(allow ipc-posix-shm-read-data (ipc-posix-name "apple.shm.notification_center"))

; TLS certificate verification: Security.framework asks trustd, and reads
; its preferences through cfprefsd (measured on CI's DNS over HTTPS).
(allow mach-lookup
       (global-name "com.apple.trustd")
       (global-name "com.apple.cfprefsd.daemon"))
(allow ipc-posix-shm-read-data (ipc-posix-name "apple.cfprefs.daemonv1"))
"#;

/// sing-box's own path: the one program the profile lets sandbox-exec
/// execute, and the one file of the helper directory sing-box may read.
pub const PARAM_SING_BOX: &str = "SING_BOX";
/// The helper directory, sing-box's: listed (not read) as it starts.
pub const PARAM_HELPER_DIR: &str = "HELPER_DIR";
/// This run's directory: its config and attachments, sing-box's `-D`,
/// `HOME` and `TMPDIR`. Read and written.
pub const PARAM_RUN_DIR: &str = "RUN_DIR";
/// The starting account's state directory: its `cache.db` and its
/// Tailscale state. Read and written; no other account's.
pub const PARAM_USER_DIR: &str = "USER_DIR";

/// Every parameter the profile is given (`-D NAME=value`), in order; it
/// reads them as `(param "NAME")`.
pub const PARAMETERS: [&str; 4] = [
    PARAM_SING_BOX,
    PARAM_HELPER_DIR,
    PARAM_RUN_DIR,
    PARAM_USER_DIR,
];

/// A denial sing-box is known to meet, harmlessly: CI fails on any other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KnownDenial {
    /// The sandbox operation, as reports name it.
    pub operation: &'static str,
    /// Its target as reports write it, exactly, or a prefix of it when it
    /// ends with `*`; empty for an operation without one.
    pub target: &'static str,
    /// Why it is denied, and why that is harmless.
    pub why: &'static str,
}

impl KnownDenial {
    /// Whether this is the denial of `operation` on `target`.
    pub fn matches(&self, operation: &str, target: &str) -> bool {
        operation == self.operation
            && match self.target.strip_suffix('*') {
                Some(prefix) => target.starts_with(prefix),
                None => target == self.target,
            }
    }
}

/// The denials sing-box meets under the profile, each harmless: the
/// sandbox-reports step of CI fails on any other.
pub const KNOWN_DENIALS: &[KnownDenial] = &[
    KnownDenial {
        operation: "process-fork",
        target: "",
        why: "sing-tun flushes DNS with dscacheutil when TUN starts and stops, and ignores \
              the error; the helper flushes DNS itself",
    },
    KnownDenial {
        operation: "file-read-data",
        target: "/private/etc/master.passwd",
        why: "CoreFoundation looks up root's account as it starts; the file holds password \
              hashes",
    },
    KnownDenial {
        operation: "mach-lookup",
        target: "com.apple.system.opendirectoryd.libinfo",
        why: "the same account lookup through opendirectoryd, once the file is denied",
    },
    KnownDenial {
        operation: "mach-lookup",
        target: "com.apple.system.DirectoryService.libinfo_v1",
        why: "the same lookup, by its older name",
    },
    KnownDenial {
        operation: "file-read-data",
        target: "/private/var/root/.CFUserTextEncoding",
        why: "CoreFoundation's default text encoding, in root's home; it falls back to its own",
    },
    KnownDenial {
        operation: "user-preference-read",
        target: "kcfpreferencesanyapplication",
        why: "Security.framework reads the preferences every application shares as it \
              verifies a certificate; denied, it keeps its defaults, and verification \
              succeeds (CI's DNS over HTTPS)",
    },
    KnownDenial {
        operation: "file-read-data",
        target: "/dev/dtracehelper",
        why: "dyld registers DTrace probes; the kernel's DOF parser stays out of reach",
    },
    KnownDenial {
        operation: "file-write-data",
        target: "/dev/dtracehelper",
        why: "the same",
    },
    KnownDenial {
        operation: "file-ioctl",
        target: "path:/dev/dtracehelper *",
        why: "the same",
    },
];

/// The longest path the profile is given: macOS's `PATH_MAX` (1024) less
/// its NUL.
const MAX_PATH: usize = 1023;
/// Whether `path` may be given to the profile: absolute, every component a
/// plain name (no `.` or `..`, no empty one, so no `//` and no trailing
/// slash), shorter than `PATH_MAX`, and made only of ASCII letters and digits,
/// space, `.`, `-`, `_` and `/`. Every path the helper names is
/// (`endpoint::macos`, a run directory's hex name, a decimal uid), and
/// none of these characters can end a profile string (`"`, `\`), start a
/// comment or an expression, or break a line.
pub fn is_plain_path(path: &str) -> bool {
    path.len() > 1
        && path.len() <= MAX_PATH
        && path.starts_with('/')
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b' ' | b'.' | b'-' | b'_' | b'/'))
        && path[1..]
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

/// A parameter refused, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamError {
    pub name: &'static str,
    pub why: &'static str,
}

impl fmt::Display for ParamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sing-box's sandbox parameter {} {}", self.name, self.why)
    }
}

impl std::error::Error for ParamError {}

/// The profile's parameters for one run: the helper's own paths, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Params {
    sing_box: String,
    helper_dir: String,
    run_dir: String,
    user_dir: String,
}

impl Params {
    /// The parameters of a run in `layout`: `sing_box` a file in its helper
    /// directory (the manifest's), `run_dir` one of its run directories (a
    /// name `paths::run_name` made), `user_dir` an account's state
    /// directory (a uid). Each must be exactly that, and a plain path
    /// ([`is_plain_path`]); nothing else is taken.
    pub fn for_run(
        layout: &Layout,
        sing_box: &Path,
        run_dir: &Path,
        user_dir: &Path,
    ) -> Result<Self, ParamError> {
        let refuse = |name, why| ParamError { name, why };
        let in_dir = |path: &Path, dir: &Path, name_ok: fn(&str) -> bool| {
            path.parent() == Some(dir)
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(name_ok)
        };
        if !in_dir(sing_box, layout.helper_dir(), is_plain_file_name) {
            return Err(refuse(
                PARAM_SING_BOX,
                "is not a file in the helper directory",
            ));
        }
        if !in_dir(run_dir, &layout.runs_dir(), is_run_name) {
            return Err(refuse(
                PARAM_RUN_DIR,
                "is not one of the helper's run directories",
            ));
        }
        if !in_dir(user_dir, &layout.users_dir(), is_uid) {
            return Err(refuse(
                PARAM_USER_DIR,
                "is not an account's state directory",
            ));
        }
        let text = |name, path: &Path| {
            path.to_str()
                .filter(|text| is_plain_path(text))
                .map(str::to_owned)
                .ok_or_else(|| refuse(name, "is not a plain absolute path of safe characters"))
        };
        Ok(Self {
            sing_box: text(PARAM_SING_BOX, sing_box)?,
            helper_dir: text(PARAM_HELPER_DIR, layout.helper_dir())?,
            run_dir: text(PARAM_RUN_DIR, run_dir)?,
            user_dir: text(PARAM_USER_DIR, user_dir)?,
        })
    }

    /// Each parameter's name and value, in [`PARAMETERS`]' order.
    pub fn pairs(&self) -> [(&'static str, &str); 4] {
        [
            (PARAM_SING_BOX, &self.sing_box),
            (PARAM_HELPER_DIR, &self.helper_dir),
            (PARAM_RUN_DIR, &self.run_dir),
            (PARAM_USER_DIR, &self.user_dir),
        ]
    }

    /// sing-box's path, which sandbox-exec executes.
    pub fn sing_box(&self) -> &str {
        &self.sing_box
    }
}

/// sandbox-exec's arguments, after its own name, to run sing-box (at
/// `params`' path) with `sing_box_args` under [`SING_BOX_PROFILE`]: the
/// profile, each parameter, `--`, then sing-box's path and its arguments as
/// they are.
pub fn sandbox_exec_args(params: &Params, sing_box_args: &[String]) -> Vec<String> {
    let mut args = vec!["-p".to_owned(), SING_BOX_PROFILE.to_owned()];
    for (name, value) in params.pairs() {
        args.push("-D".to_owned());
        args.push(format!("{name}={value}"));
    }
    args.push("--".to_owned());
    args.push(params.sing_box.clone());
    args.extend(sing_box_args.iter().cloned());
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::run_name;
    use crate::spawnplan::POSIX_PATH;
    use boxpilot_protocol::endpoint::macos;
    use std::path::PathBuf;

    /// An s-expression of the profile.
    #[derive(Debug, Clone, PartialEq)]
    enum Sexp {
        Atom(String),
        Str(String),
        List(Vec<Sexp>),
    }

    /// The profile's top-level expressions, comments dropped.
    fn parse_profile() -> Vec<Sexp> {
        fn parse(chars: &mut std::iter::Peekable<std::str::Chars>) -> Vec<Sexp> {
            let mut items = Vec::new();
            while let Some(&c) = chars.peek() {
                match c {
                    ';' => while chars.next().is_some_and(|c| c != '\n') {},
                    '(' => {
                        chars.next();
                        items.push(Sexp::List(parse(chars)));
                    }
                    ')' => {
                        chars.next();
                        return items;
                    }
                    '"' => {
                        chars.next();
                        let text: String = chars.by_ref().take_while(|&c| c != '"').collect();
                        assert!(!text.contains('\\'), "an escape in {text:?}");
                        items.push(Sexp::Str(text));
                    }
                    c if c.is_whitespace() => {
                        chars.next();
                    }
                    _ => {
                        let mut atom = String::new();
                        while let Some(&c) = chars.peek() {
                            if c.is_whitespace() || c == '(' || c == ')' {
                                break;
                            }
                            atom.push(c);
                            chars.next();
                        }
                        items.push(Sexp::Atom(atom));
                    }
                }
            }
            items
        }
        parse(&mut SING_BOX_PROFILE.chars().peekable())
    }

    /// A rule: `allow` or `deny`, its operations, its filters, and whether
    /// it reports.
    #[derive(Debug)]
    struct Rule {
        action: String,
        operations: Vec<String>,
        filters: Vec<Sexp>,
        reports: bool,
    }

    fn rules() -> Vec<Rule> {
        parse_profile()
            .into_iter()
            .filter_map(|expression| {
                let Sexp::List(items) = expression else {
                    panic!("a bare atom at the top: {expression:?}")
                };
                let Sexp::Atom(action) = &items[0] else {
                    panic!("{items:?}")
                };
                if action == "version" {
                    return None;
                }
                assert!(action == "allow" || action == "deny", "{action}");
                let mut rule = Rule {
                    action: action.clone(),
                    operations: Vec::new(),
                    filters: Vec::new(),
                    reports: false,
                };
                for item in &items[1..] {
                    match item {
                        Sexp::Atom(operation) => rule.operations.push(operation.clone()),
                        Sexp::List(modifier)
                            if modifier.first() == Some(&Sexp::Atom("with".into())) =>
                        {
                            assert_eq!(
                                modifier,
                                &[Sexp::Atom("with".into()), Sexp::Atom("report".into())]
                            );
                            rule.reports = true;
                        }
                        filter => rule.filters.push(filter.clone()),
                    }
                }
                assert!(!rule.operations.is_empty(), "{rule:?}");
                Some(rule)
            })
            .collect()
    }

    /// Whether `operation` is `name` or a wildcard (`file-write*`,
    /// `default`) that covers it.
    fn covers(operation: &str, name: &str) -> bool {
        operation == "default"
            || operation == name
            || operation
                .strip_suffix('*')
                .is_some_and(|prefix| name.starts_with(prefix))
    }

    /// The allow rules covering `name`.
    fn allows(name: &str) -> Vec<Rule> {
        rules()
            .into_iter()
            .filter(|rule| rule.action == "allow")
            .filter(|rule| rule.operations.iter().any(|op| covers(op, name)))
            .collect()
    }

    fn list(items: &[&str]) -> Sexp {
        Sexp::List(
            items
                .iter()
                .map(|item| match item.strip_prefix('"') {
                    Some(text) => Sexp::Str(text.trim_end_matches('"').to_owned()),
                    None => Sexp::Atom((*item).to_owned()),
                })
                .collect(),
        )
    }

    fn param(kind: &str, name: &str) -> Sexp {
        Sexp::List(vec![
            Sexp::Atom(kind.into()),
            Sexp::List(vec![Sexp::Atom("param".into()), Sexp::Str(name.into())]),
        ])
    }

    /// A filter's fixed path, if it is `(literal "…")` or `(subpath "…")`.
    fn fixed_path(filter: &Sexp) -> Option<(&str, &str)> {
        match filter {
            Sexp::List(items) => match items.as_slice() {
                [Sexp::Atom(kind), Sexp::Str(path)] if kind == "literal" || kind == "subpath" => {
                    Some((kind, path))
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// The installed daemon's parameters for a run of account 501.
    fn installed() -> (Layout, PathBuf, PathBuf, PathBuf) {
        let layout = Layout::installed_macos();
        let sing_box = PathBuf::from(macos::SING_BOX_PATH);
        let run = layout.run_dir(&run_name(&[0xab; 16]));
        let user = layout.user_dir("501").unwrap();
        (layout, sing_box, run, user)
    }

    /// Enforced, deny by default, first; nothing allowed by default.
    #[test]
    fn the_profile_denies_by_default() {
        const { assert!(ENFORCED) };
        assert_eq!(STATUS, "enforced");
        assert!(SING_BOX_PROFILE.starts_with("(version 1)\n"));
        let expressions = parse_profile();
        assert_eq!(expressions[0], list(&["version", "1"]));
        assert_eq!(expressions[1], list(&["deny", "default"]));
        for rule in rules().iter().skip(1) {
            assert!(
                !rule.operations.iter().any(|op| op == "default"),
                "{rule:?}"
            );
        }
        assert!(!SING_BOX_PROFILE.contains("no-report"));
        assert!(!SING_BOX_PROFILE.contains("no-sandbox"));
    }

    /// No program but sing-box itself, which sandbox-exec executes: no
    /// fork, no shell, no `networksetup`.
    #[test]
    fn nothing_runs_but_sing_box() {
        let exec = allows("process-exec");
        assert_eq!(exec.len(), 1, "{exec:?}");
        assert_eq!(exec[0].filters, [param("literal", PARAM_SING_BOX)]);
        assert!(allows("process-fork").is_empty());
        assert!(allows("process-exec-interpreter").len() <= 1);
        for rule in rules() {
            for op in &rule.operations {
                assert!(
                    !(rule.action == "allow" && (op == "process*" || op == "process-fork")),
                    "{rule:?}"
                );
            }
        }
        assert!(!SING_BOX_PROFILE.contains("networksetup"));
        assert!(!SING_BOX_PROFILE.contains("/bin/"));
    }

    /// Writes go to this run, this account's state and `/dev/null`: never
    /// the state directory elsewhere (another account's state, the owner
    /// record, the helper's log), never outside the helper's tree.
    #[test]
    fn nothing_is_written_outside_the_run_and_the_accounts_state() {
        for write in [
            "file-write-data",
            "file-write-create",
            "file-write-unlink",
            "file-write-mode",
            "file-write-owner",
            "file-write-xattr",
        ] {
            let rules = allows(write);
            assert_eq!(rules.len(), 1, "{write}: {rules:?}");
            assert_eq!(
                rules[0].filters,
                [
                    param("subpath", PARAM_RUN_DIR),
                    param("subpath", PARAM_USER_DIR),
                    list(&["literal", "\"/dev/null\""]),
                ],
                "{write}"
            );
        }
        assert!(allows("sysctl-write").is_empty());
        assert!(allows("file-ioctl").is_empty());
    }

    /// Reads: the system's files, sing-box, its run and its account's
    /// state. Never anything that holds the user's data, root's home,
    /// keychains, password hashes, other accounts' state or the helper's
    /// own; nor the data volume's other way in, `/System/Volumes/Data`.
    #[test]
    fn nothing_private_is_read() {
        let rules = allows("file-read-data");
        assert_eq!(rules.len(), 1, "{rules:?}");
        let filters = &rules[0].filters;
        for kept in [PARAM_SING_BOX, PARAM_HELPER_DIR] {
            assert!(filters.contains(&param("literal", kept)), "{kept}");
        }
        for kept in [PARAM_RUN_DIR, PARAM_USER_DIR] {
            assert!(filters.contains(&param("subpath", kept)), "{kept}");
        }
        let private = [
            "/Users",
            "/private/var/root",
            "/var/root",
            "/Library/Keychains",
            "/Library/Application Support/BoxPilot Helper/state",
            "/private/var/db",
            "/private/etc/master.passwd",
            "/private/etc/sudoers",
            "/private/var/folders",
            "/System/Volumes/Data",
            "/Volumes",
        ];
        let exceptions = ["/private/var/db/timezone"];
        for filter in filters {
            if let Some((kind, path)) = fixed_path(filter) {
                for private in private {
                    let inside = path == private || path.starts_with(&format!("{private}/"));
                    assert!(
                        !inside || exceptions.contains(&path),
                        "{kind} {path} reads {private}"
                    );
                    if kind == "subpath" {
                        assert!(
                            !private.starts_with(&format!("{path}/")),
                            "subpath {path} reads {private}"
                        );
                    }
                }
            } else {
                assert!(
                    matches!(filter, Sexp::List(items) if matches!(items.as_slice(),
                        [Sexp::Atom(kind), Sexp::List(param)]
                            if (kind == "literal" || kind == "subpath")
                                && param.first() == Some(&Sexp::Atom("param".into())))),
                    "an unexpected read filter: {filter:?}"
                );
            }
        }
        // Metadata, path walking, is read anywhere; contents are not.
        assert!(allows("file-read-metadata")
            .iter()
            .any(|rule| rule.filters.is_empty()));
    }

    /// Network: IP traffic, routing and utun sockets; no Unix socket but
    /// mDNSResponder's.
    #[test]
    fn local_services_are_out_of_reach_but_the_resolver() {
        let rules = rules();
        let position = |filter: &[&str], action: &str| {
            rules.iter().position(|rule| {
                rule.action == action
                    && rule.operations == ["network-outbound"]
                    && rule.filters.first() == Some(&list(filter))
            })
        };
        let deny = rules
            .iter()
            .position(|rule| {
                rule.action == "deny"
                    && rule.operations == ["network-outbound"]
                    && rule.filters == [list(&["remote", "unix-socket"])]
            })
            .expect("Unix sockets are denied");
        let broad = rules
            .iter()
            .position(|rule| {
                rule.action == "allow"
                    && rule.operations == ["network-outbound"]
                    && rule.filters.is_empty()
            })
            .expect("outbound traffic is allowed");
        assert!(broad < deny, "the denial comes after, so it wins");
        let _ = position;
        let resolver = rules
            .iter()
            .position(|rule| {
                rule.action == "allow"
                    && rule.operations == ["network-outbound"]
                    && rule.filters.iter().all(|filter| {
                        let Sexp::List(items) = filter else {
                            return false;
                        };
                        let path = format!("{items:?}");
                        items.first() == Some(&Sexp::Atom("remote".into()))
                            && path.contains("mDNSResponder")
                    })
                    && !rule.filters.is_empty()
            })
            .expect("mDNSResponder's socket is allowed");
        assert!(deny < resolver);
        let sockets: Vec<&Rule> = rules
            .iter()
            .filter(|rule| rule.action == "allow" && rule.operations == ["system-socket"])
            .collect();
        let domains: Vec<&Sexp> = sockets.iter().flat_map(|rule| &rule.filters).collect();
        assert_eq!(
            domains,
            [
                &list(&["socket-domain", "17"]),
                &list(&["socket-domain", "32"])
            ]
        );
    }

    /// The Mach services allowed are the measured ones, and nothing is
    /// allowed only to be reported any more. None of them is the system
    /// keychain's server, or the account lookups' (`opendirectoryd`).
    #[test]
    fn mach_services_are_the_measured_ones() {
        let mut silent = Vec::new();
        let mut reported = Vec::new();
        for rule in allows("mach-lookup") {
            for filter in &rule.filters {
                let Sexp::List(items) = filter else {
                    panic!("{filter:?}")
                };
                let [Sexp::Atom(kind), Sexp::Str(name)] = items.as_slice() else {
                    panic!("{items:?}")
                };
                assert_eq!(kind, "global-name");
                if rule.reports {
                    reported.push(name.clone());
                } else {
                    silent.push(name.clone());
                }
            }
        }
        assert_eq!(
            silent,
            [
                "com.apple.logd",
                "com.apple.system.notification_center",
                "com.apple.trustd",
                "com.apple.cfprefsd.daemon",
            ]
        );
        assert!(reported.is_empty(), "{reported:?}");
        assert!(!SING_BOX_PROFILE.contains("(with report)"));
        assert!(!SING_BOX_PROFILE.contains("SecurityServer"));
        assert!(!SING_BOX_PROFILE.contains("opendirectoryd"));
    }

    /// The known denials are ones the profile makes: none of them is
    /// allowed.
    #[test]
    fn the_known_denials_are_the_profiles() {
        for known in KNOWN_DENIALS {
            assert!(!known.why.is_empty());
            let allowed = allows(known.operation);
            for rule in &allowed {
                for filter in &rule.filters {
                    if let Some((kind, path)) = fixed_path(filter) {
                        let target = known.target.trim_end_matches('*');
                        let hit = path == target
                            || (kind == "subpath" && target.starts_with(&format!("{path}/")));
                        assert!(!hit, "{known:?} is allowed by {rule:?}");
                    }
                }
                if known.operation == "mach-lookup" {
                    assert!(
                        !format!("{:?}", rule.filters).contains(known.target),
                        "{known:?}"
                    );
                }
            }
            // An operation allowed without a filter would never be denied.
            assert!(
                !allowed.iter().any(|rule| rule.filters.is_empty()),
                "{known:?}"
            );
        }
        let fork = &KNOWN_DENIALS[0];
        assert!(fork.matches("process-fork", ""));
        assert!(!fork.matches("process-exec*", ""));
        let ioctl = KNOWN_DENIALS
            .iter()
            .find(|known| known.operation == "file-ioctl")
            .unwrap();
        assert!(ioctl.matches(
            "file-ioctl",
            "path:/dev/dtracehelper ioctl-command:(_IO \"h\" 4)"
        ));
        assert!(!ioctl.matches("file-ioctl", "path:/dev/pf ioctl-command:(_IO \"D\" 1)"));
        let passwd = KNOWN_DENIALS[1];
        assert!(passwd.matches("file-read-data", "/private/etc/master.passwd"));
        assert!(!passwd.matches("file-read-data", "/private/etc/master.passwd.old"));
    }

    /// The profile is a constant: reading only the parameters the helper
    /// gives it, and naming none of the helper's paths itself (they arrive
    /// as parameters).
    #[test]
    fn the_profile_reads_only_its_own_parameters() {
        fn params(sexp: &Sexp, found: &mut Vec<String>) {
            if let Sexp::List(items) = sexp {
                if let [Sexp::Atom(head), Sexp::Str(name)] = items.as_slice() {
                    if head == "param" {
                        found.push(name.clone());
                    }
                }
                for item in items {
                    params(item, found);
                }
            }
        }
        let mut found = Vec::new();
        for expression in parse_profile() {
            params(&expression, &mut found);
        }
        for name in &found {
            assert!(
                PARAMETERS.contains(&name.as_str()),
                "the profile reads {name}"
            );
        }
        for name in PARAMETERS {
            assert!(found.iter().any(|found| found == name), "{name} is unused");
        }
        for path in [macos::SUPPORT_DIR, macos::STATE_DIR, macos::SING_BOX_PATH] {
            assert!(!SING_BOX_PROFILE.contains(path), "{path}");
        }
        assert!(!SING_BOX_PROFILE.contains('\0'));
        assert!(SING_BOX_PROFILE.is_ascii());
    }

    /// The tool is macOS's own, by its absolute path, in a SIP-protected
    /// system directory: never one under `/usr/local`, which Homebrew gives
    /// the user.
    #[test]
    fn sandbox_exec_is_the_systems() {
        assert_eq!(SANDBOX_EXEC, "/usr/bin/sandbox-exec");
        let dir = Path::new(SANDBOX_EXEC).parent().unwrap().to_str().unwrap();
        assert!(POSIX_PATH.split(':').any(|entry| entry == dir), "{dir}");
        assert!(is_plain_path(SANDBOX_EXEC));
    }

    /// The installed daemon's parameters are `endpoint::macos`'s paths and
    /// the run plan's, and nothing else. POSIX only: on Windows `Layout`
    /// joins the macOS paths with `\`, which no profile is given.
    #[cfg(unix)]
    #[test]
    fn the_installed_parameters_come_from_the_endpoint_and_the_run() {
        let (layout, sing_box, run, user) = installed();
        let params = Params::for_run(&layout, &sing_box, &run, &user).unwrap();
        let run_text = format!("{}/runs/{}", macos::STATE_DIR, "ab".repeat(16));
        let user_text = format!("{}/users/501", macos::STATE_DIR);
        assert_eq!(
            params.pairs(),
            [
                ("SING_BOX", macos::SING_BOX_PATH),
                ("HELPER_DIR", macos::BIN_DIR),
                ("RUN_DIR", run_text.as_str()),
                ("USER_DIR", user_text.as_str()),
            ]
        );
        assert_eq!(params.pairs().map(|(name, _)| name), PARAMETERS);
        assert_eq!(params.sing_box(), macos::SING_BOX_PATH);
        for (_, value) in params.pairs() {
            assert!(is_plain_path(value), "{value}");
            assert!(value.starts_with(macos::SUPPORT_DIR), "{value}");
        }
    }

    /// sandbox-exec gets the constant profile, the four parameters, `--`,
    /// then exactly the verified sing-box and its arguments. POSIX only, as
    /// above.
    #[cfg(unix)]
    #[test]
    fn the_command_applies_the_profile_then_runs_exactly_sing_box() {
        let (layout, sing_box, run, user) = installed();
        let params = Params::for_run(&layout, &sing_box, &run, &user).unwrap();
        let run_text = run.to_str().unwrap();
        let sing_box_args =
            crate::spawnplan::sing_box_args(run_text, &format!("{run_text}/config.json"));
        let args = sandbox_exec_args(&params, &sing_box_args);
        let mut expected = vec![
            "-p".to_owned(),
            SING_BOX_PROFILE.to_owned(),
            "-D".to_owned(),
            format!("SING_BOX={}", macos::SING_BOX_PATH),
            "-D".to_owned(),
            format!("HELPER_DIR={}", macos::BIN_DIR),
            "-D".to_owned(),
            format!("RUN_DIR={run_text}"),
            "-D".to_owned(),
            format!("USER_DIR={}/users/501", macos::STATE_DIR),
            "--".to_owned(),
            macos::SING_BOX_PATH.to_owned(),
        ];
        expected.extend(sing_box_args.iter().cloned());
        assert_eq!(args, expected);
        // The profile is the constant, whole: no value spliced into it.
        assert_eq!(args[1], SING_BOX_PROFILE);
        // No other way to name a profile.
        assert!(!args.iter().any(|arg| arg == "-f" || arg == "-n"));
        // Everything after `--` is sing-box's: its path, then its own
        // arguments, as they are.
        let end = args.iter().position(|arg| arg == "--").unwrap();
        assert_eq!(args[end + 1], macos::SING_BOX_PATH);
        assert_eq!(args[end + 2..], sing_box_args[..]);
    }

    /// No value that could leave a profile string, or a line, gets in.
    #[test]
    fn a_value_that_could_leave_a_profile_string_is_refused() {
        for good in [
            "/a",
            macos::STATE_DIR,
            macos::SING_BOX_PATH,
            "/Library/Application Support/BoxPilot Helper/state/users/501",
        ] {
            assert!(is_plain_path(good), "{good}");
        }
        for bad in [
            "",
            "/",
            "relative/path",
            "/a/",
            "/a//b",
            "/a/./b",
            "/a/../b",
            "/a/..",
        ] {
            assert!(!is_plain_path(bad), "{bad:?}");
        }
        for c in [
            '"', '\\', '(', ')', ';', '#', '|', '\'', '`', '$', '=', '\n', '\r', '\t', '\0',
            '\u{7f}', 'é', '\u{2028}', '*', '[', '{',
        ] {
            let path = format!("/Library/Application Support/x{c}y");
            assert!(!is_plain_path(&path), "{path:?}");
        }
        assert!(!is_plain_path(&format!("/{}", "a".repeat(MAX_PATH))));

        // Through `for_run`: a layout whose own paths hold one is refused.
        let layout = Layout::new(
            PathBuf::from("/h/bin"),
            PathBuf::from("/h/st\") (allow default"),
        );
        let run = layout.run_dir(&run_name(&[1; 16]));
        let user = layout.user_dir("501").unwrap();
        let error =
            Params::for_run(&layout, Path::new("/h/bin/sing-box"), &run, &user).unwrap_err();
        assert_eq!(error.name, PARAM_RUN_DIR);
        assert!(error.to_string().contains("RUN_DIR"), "{error}");
        let layout = Layout::new(PathBuf::from("/h/b\"n"), PathBuf::from("/h/state"));
        let run = layout.run_dir(&run_name(&[1; 16]));
        let user = layout.user_dir("501").unwrap();
        let error =
            Params::for_run(&layout, &layout.helper_file("sing-box"), &run, &user).unwrap_err();
        assert_eq!(error.name, PARAM_SING_BOX);
    }

    /// Each parameter is the helper's own path of its kind, or refused.
    #[test]
    fn a_path_outside_the_run_plan_is_refused() {
        let (layout, sing_box, run, user) = installed();
        let refused = |sing_box: &Path, run: &Path, user: &Path| {
            Params::for_run(&layout, sing_box, run, user)
                .unwrap_err()
                .name
        };
        let state = layout.state_dir().to_owned();
        for bad in [
            PathBuf::from("/usr/local/bin/sing-box"),
            layout.helper_dir().join("x").join("sing-box"),
            layout.helper_dir().join("../sing-box"),
            layout.helper_dir().to_owned(),
        ] {
            assert_eq!(refused(&bad, &run, &user), PARAM_SING_BOX, "{bad:?}");
        }
        for bad in [
            layout.runs_dir(),
            layout.run_dir("AB".repeat(16).as_str()),
            layout.run_dir("ab"),
            state.join("users").join("ab".repeat(16)),
            run.join("tmp"),
            PathBuf::from("/tmp").join("ab".repeat(16)),
        ] {
            assert_eq!(refused(&sing_box, &bad, &user), PARAM_RUN_DIR, "{bad:?}");
        }
        for bad in [
            layout.users_dir(),
            layout.users_dir().join("0501"),
            layout.users_dir().join("S-1-5-18"),
            layout.runs_dir().join("501"),
            user.join("tailscale"),
        ] {
            assert_eq!(refused(&sing_box, &run, &bad), PARAM_USER_DIR, "{bad:?}");
        }
    }
}
