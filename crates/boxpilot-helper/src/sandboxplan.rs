//! sing-box's sandbox on macOS (ADR 0006 rule 2, "Defense in depth"), as
//! pure data: the profile it runs under ([`SING_BOX_PROFILE`]), the
//! parameters that profile is given ([`Params`]), and the command line that
//! applies it ([`sandbox_exec_args`]). `tokenplan` is Windows' counterpart.
//!
//! **Measuring; not enforced yet.** The profile this build ships allows
//! every operation and reports each one, sing-box's own and those of
//! anything it starts (`networksetup`, which inherits the sandbox); it
//! denies nothing ([`ENFORCED`] is false). CI's macOS job collects the
//! reports over its TUN runs, the GUI client's, the helper's crash and the
//! system proxy (`packaging/macos/helper-smoke.sh sandbox-reports`, read by
//! `sandboxreport`), so that the enforced profile is written from what
//! sing-box was seen to do: measured, not guessed, as sing-box's Windows
//! token was. That one denies by default: no file writes outside the run
//! directory and the starting account's own state, no execution but
//! sing-box itself and `/usr/sbin/networksetup`. Shipping the measuring
//! profile is acceptable only because no release ships the macOS helper
//! yet; the enforced one replaces it before one does.
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
//!   sandbox-exec itself is on the sealed system volume, which SIP
//!   protects, and the supervisor verifies its chain (root's, writable by
//!   nobody else) before every spawn as well. Between the hash and the exec
//!   only root could swap sing-box, as before (`posix::verify`).
//! - **The profile is a constant,** handed over with `-p`, never written to
//!   a file. Its parameters are the helper's own paths only: sing-box's,
//!   the state directory, this run's directory and the starting account's
//!   state directory, from `endpoint::macos` and the run plan
//!   ([`Params::for_run`] checks each), never anything from a config or a
//!   client. The profile reads them as `(param "RUN_DIR")`: values are never
//!   spliced into its text, and none may hold a character that could end a
//!   profile string anyway ([`is_plain_path`]).
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

/// Whether [`SING_BOX_PROFILE`] denies anything. Not yet: it measures.
pub const ENFORCED: bool = false;

/// What the helper's log says of the profile sing-box runs under.
pub const STATUS: &str = if ENFORCED {
    "enforced"
} else {
    "measuring; not enforced yet"
};

/// The profile sing-box runs under, in SBPL, the sandbox's own language
/// (Apple doesn't document it for third parties). Measuring: every
/// operation is allowed, and reported to the system log, where the kernel's
/// sandbox writes one line per report (`sandboxreport`).
pub const SING_BOX_PROFILE: &str = "\
(version 1)
; BoxPilot's privileged helper: sing-box's sandbox (ADR 0006).
; Measuring; not enforced yet: every operation is allowed, and reported.
(allow (with report) default)
";

/// sing-box's own path: the one program the enforced profile lets it
/// execute besides `networksetup`.
pub const PARAM_SING_BOX: &str = "SING_BOX";
/// The helper's state directory (`endpoint::macos::STATE_DIR`).
pub const PARAM_STATE_DIR: &str = "STATE_DIR";
/// This run's directory: its config and attachments, sing-box's `-D`,
/// `HOME` and `TMPDIR`.
pub const PARAM_RUN_DIR: &str = "RUN_DIR";
/// The starting account's state directory: its `cache.db` and its
/// Tailscale state.
pub const PARAM_USER_DIR: &str = "USER_DIR";

/// Every parameter the profile is given (`-D NAME=value`), in order; it
/// reads them as `(param "NAME")`.
pub const PARAMETERS: [&str; 4] = [
    PARAM_SING_BOX,
    PARAM_STATE_DIR,
    PARAM_RUN_DIR,
    PARAM_USER_DIR,
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
    state_dir: String,
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
            state_dir: text(PARAM_STATE_DIR, layout.state_dir())?,
            run_dir: text(PARAM_RUN_DIR, run_dir)?,
            user_dir: text(PARAM_USER_DIR, user_dir)?,
        })
    }

    /// Each parameter's name and value, in [`PARAMETERS`]' order.
    pub fn pairs(&self) -> [(&'static str, &str); 4] {
        [
            (PARAM_SING_BOX, &self.sing_box),
            (PARAM_STATE_DIR, &self.state_dir),
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

    /// The profile without its comments.
    fn profile_code() -> String {
        SING_BOX_PROFILE
            .lines()
            .map(|line| line.split(';').next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The installed daemon's parameters for a run of account 501.
    fn installed() -> (Layout, PathBuf, PathBuf, PathBuf) {
        let layout = Layout::installed_macos();
        let sing_box = PathBuf::from(macos::SING_BOX_PATH);
        let run = layout.run_dir(&run_name(&[0xab; 16]));
        let user = layout.user_dir("501").unwrap();
        (layout, sing_box, run, user)
    }

    /// What this round ships, and the ADR says: the profile reports every
    /// operation and denies none, so CI can measure what sing-box does.
    /// The enforced profile changes this test, with ADR 0006.
    #[test]
    fn measuring_reports_every_operation_and_denies_none_yet() {
        const { assert!(!ENFORCED) };
        assert_eq!(STATUS, "measuring; not enforced yet");
        assert!(SING_BOX_PROFILE.starts_with("(version 1)\n"));
        assert!(SING_BOX_PROFILE.contains("not enforced yet"));
        let code = profile_code();
        assert!(code.contains("(allow (with report) default)"), "{code}");
        assert!(!code.contains("deny"), "{code}");
        assert!(!code.contains("no-report"), "{code}");
    }

    /// The profile is a constant: balanced, reading only the parameters
    /// the helper gives it, and naming none of the helper's paths itself
    /// (they arrive as parameters).
    #[test]
    fn the_profile_reads_only_its_own_parameters() {
        let code = profile_code();
        let mut depth = 0i32;
        for c in code.chars() {
            match c {
                '(' => depth += 1,
                ')' => depth -= 1,
                _ => {}
            }
            assert!(depth >= 0, "{code}");
        }
        assert_eq!(depth, 0, "{code}");
        for (at, _) in code.match_indices("(param ") {
            let name = code[at..].split('"').nth(1).expect("(param \"NAME\")");
            assert!(PARAMETERS.contains(&name), "the profile reads {name}");
        }
        for path in [macos::SUPPORT_DIR, macos::STATE_DIR, macos::SING_BOX_PATH] {
            assert!(!code.contains(path), "{path}");
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
                ("STATE_DIR", macos::STATE_DIR),
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
            format!("STATE_DIR={}", macos::STATE_DIR),
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
        assert_eq!(error.name, PARAM_STATE_DIR);
        assert!(error.to_string().contains("STATE_DIR"), "{error}");
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
