//! Installing and removing the macOS privileged helper (ADR 0006 rule 7):
//! the one administrator prompt, as the command that shows it, and running
//! it (Settings › TUN, "Install" / "Remove", and the prompt before a TUN
//! start that needs the helper). The commands and the reading of a failure
//! are pure, built on every OS so their tests run everywhere; running one
//! ([`run_install`], [`run_remove`]) is a thin shell over
//! `std::process::Command`, with no shell of BoxPilot's own.
//!
//! - **Not `SMAppService`**: it needs macOS 13 and an Apple-issued signing
//!   identity BoxPilot doesn't have. One prompt instead:
//!   `osascript … with administrator privileges` runs a fixed script inside
//!   BoxPilot.app (`endpoint::macos::INSTALL_SCRIPT`, `UNINSTALL_SCRIPT`).
//! - **The AppleScript text is a constant** ([`INSTALL_APPLESCRIPT`],
//!   [`REMOVE_APPLESCRIPT`]). Every value (the prompt, the script's path, the
//!   app's `Contents` directory, the owner's uid) arrives through
//!   `on run argv`, and reaches the shell only as a positional argument
//!   wrapped in `quoted form of`: never inside the script text. That is the
//!   rule `privilege::grant_command` follows on Linux, and the same
//!   hostile-path test holds it here.
//! - **The password is the OS's**: BoxPilot never sees it; only the OS
//!   prompt does.
//! - **Install-time trust**: the payload is read from the app bundle,
//!   which the user can write; that one moment is the user's (ADR 0006,
//!   "Install-time trust").
//! - **The owner** is the account that runs BoxPilot (its real uid); the
//!   prompt is how an administrator authorizes it (rule 4).

use crate::i18n::{s, Fmt1};
use boxpilot_protocol::endpoint::macos::{BUNDLE_PAYLOAD_DIR, INSTALL_SCRIPT, UNINSTALL_SCRIPT};
use std::path::Path;
use std::process::{Command, Stdio};

/// `osascript`, by its absolute path.
pub const OSASCRIPT: &str = "/usr/bin/osascript";

/// The AppleScript that installs the helper: `argv` is the prompt, the
/// install script's path, the app's `Contents` directory and the owner's
/// uid, in that order. One `-e` per line.
pub const INSTALL_APPLESCRIPT_LINES: &[&str] = &[
    "on run argv",
    "if (count of argv) is not 4 then error \"BoxPilot: expected 4 arguments\"",
    "set thePrompt to item 1 of argv",
    "set theCommand to \"/bin/sh \" & quoted form of (item 2 of argv) & \" \" & quoted form of (item 3 of argv) & \" \" & quoted form of (item 4 of argv)",
    "do shell script theCommand with prompt thePrompt with administrator privileges",
    "end run",
];

/// The AppleScript that removes it: `argv` is the prompt, the uninstall
/// script's path and `--keep-state` or `--remove-state`.
pub const REMOVE_APPLESCRIPT_LINES: &[&str] = &[
    "on run argv",
    "if (count of argv) is not 3 then error \"BoxPilot: expected 3 arguments\"",
    "set thePrompt to item 1 of argv",
    "set theCommand to \"/bin/sh \" & quoted form of (item 2 of argv) & \" \" & quoted form of (item 3 of argv)",
    "do shell script theCommand with prompt thePrompt with administrator privileges",
    "end run",
];

/// [`INSTALL_APPLESCRIPT_LINES`] as one text.
pub const INSTALL_APPLESCRIPT: &str = "on run argv
if (count of argv) is not 4 then error \"BoxPilot: expected 4 arguments\"
set thePrompt to item 1 of argv
set theCommand to \"/bin/sh \" & quoted form of (item 2 of argv) & \" \" & quoted form of (item 3 of argv) & \" \" & quoted form of (item 4 of argv)
do shell script theCommand with prompt thePrompt with administrator privileges
end run";

/// [`REMOVE_APPLESCRIPT_LINES`] as one text.
pub const REMOVE_APPLESCRIPT: &str = "on run argv
if (count of argv) is not 3 then error \"BoxPilot: expected 3 arguments\"
set thePrompt to item 1 of argv
set theCommand to \"/bin/sh \" & quoted form of (item 2 of argv) & \" \" & quoted form of (item 3 of argv)
do shell script theCommand with prompt thePrompt with administrator privileges
end run";

/// `osascript`'s arguments for `lines`, then `argv`: one `-e` per line,
/// and every value after the script, where `on run argv` takes them.
fn osascript_args(lines: &[&str], argv: Vec<String>) -> Vec<String> {
    lines
        .iter()
        .flat_map(|line| ["-e".to_owned(), (*line).to_owned()])
        .chain(argv)
        .collect()
}

/// The install script inside the app, from its `Contents` directory.
pub fn install_script_path(contents: &Path) -> std::path::PathBuf {
    contents.join(BUNDLE_PAYLOAD_DIR).join(INSTALL_SCRIPT)
}

/// The uninstall script inside the app, from its `Contents` directory.
pub fn uninstall_script_path(contents: &Path) -> std::path::PathBuf {
    contents.join(BUNDLE_PAYLOAD_DIR).join(UNINSTALL_SCRIPT)
}

/// The command that installs the helper from the app whose `Contents`
/// directory is `contents`, and makes `owner` its owner, behind one
/// administrator prompt saying `prompt`. `prompt` is BoxPilot's own text,
/// and never begins with `-`, which osascript would take for an option.
pub fn install_command(prompt: &str, contents: &Path, owner: u32) -> (String, Vec<String>) {
    let argv = vec![
        prompt.to_owned(),
        install_script_path(contents).to_string_lossy().into_owned(),
        contents.to_string_lossy().into_owned(),
        owner.to_string(),
    ];
    (
        OSASCRIPT.to_owned(),
        osascript_args(INSTALL_APPLESCRIPT_LINES, argv),
    )
}

/// The command that removes the helper with the uninstall script of the
/// app whose `Contents` directory is `contents`, keeping its state
/// directory unless `remove_state`.
pub fn remove_command(prompt: &str, contents: &Path, remove_state: bool) -> (String, Vec<String>) {
    let flag = if remove_state {
        "--remove-state"
    } else {
        "--keep-state"
    };
    let argv = vec![
        prompt.to_owned(),
        uninstall_script_path(contents)
            .to_string_lossy()
            .into_owned(),
        flag.to_owned(),
    ];
    (
        OSASCRIPT.to_owned(),
        osascript_args(REMOVE_APPLESCRIPT_LINES, argv),
    )
}

/// Why an administrator prompt changed nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptError {
    /// The user cancelled it.
    Dismissed,
    /// It ran and failed: what osascript, or the script, said.
    Failed(String),
}

impl PromptError {
    /// The message for the user; `failed` words a failure.
    pub fn message(&self, failed: Fmt1) -> String {
        match self {
            PromptError::Dismissed => s().helper.prompt_dismissed.to_string(),
            PromptError::Failed(detail) => failed(detail),
        }
    }
}

/// What a failed `osascript` run means. A cancelled prompt is AppleScript's
/// error -128 ("User canceled."). Anything else is the first line osascript
/// wrote, which for a script that refused is the script's own
/// `helper-install: …`, without AppleScript's `0:123: execution error: `
/// prefix and `(1)` error number; or, with no words, the exit code.
pub fn prompt_error(code: Option<i32>, stderr: &str) -> PromptError {
    if stderr.contains("(-128)") {
        return PromptError::Dismissed;
    }
    let line = stderr.lines().map(str::trim).find(|line| !line.is_empty());
    let detail = line.map(|line| {
        let text = line
            .split_once("execution error: ")
            .map_or(line, |(_, rest)| rest);
        match text.rsplit_once(" (") {
            Some((words, number))
                if number
                    .strip_suffix(')')
                    .is_some_and(|n| n.parse::<i32>().is_ok()) =>
            {
                words
            }
            _ => text,
        }
    });
    let h = &s().helper;
    PromptError::Failed(match (detail, code) {
        (Some(detail), _) => detail.to_string(),
        (None, Some(code)) => (h.exit_unknown)(&code.to_string()),
        (None, None) => h.prompt_terminated.to_string(),
    })
}

/// Run an administrator prompt's command: the program by its absolute
/// path, the values as arguments, no shell. Blocks until the user has
/// answered the prompt and the script has run.
fn run_prompt((program, args): (String, Vec<String>)) -> Result<(), PromptError> {
    let output = Command::new(&program)
        .args(&args)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| PromptError::Failed(error.to_string()))?;
    if output.status.success() {
        return Ok(());
    }
    Err(prompt_error(
        output.status.code(),
        &String::from_utf8_lossy(&output.stderr),
    ))
}

/// The account that runs BoxPilot: its real uid, which the install makes
/// the helper's owner.
fn real_uid() -> Option<u32> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        // SAFETY: getuid has no preconditions and cannot fail.
        Some(unsafe { libc::getuid() })
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

/// Install the helper from the app whose `Contents` directory is
/// `contents`, owned by the account that runs BoxPilot, behind macOS's
/// administrator prompt; a reinstall replaces it. Blocking: run it off the
/// UI thread. `Err` is the message for the user.
pub fn run_install(contents: &Path) -> Result<(), String> {
    let h = &s().helper;
    let Some(owner) = real_uid() else {
        return Err(h.unsupported.to_string());
    };
    run_prompt(install_command(h.install_prompt, contents, owner))
        .map_err(|error| error.message(h.install_failed))
}

/// Remove the helper with the uninstall script of the app whose `Contents`
/// directory is `contents`, behind macOS's administrator prompt. Its state
/// directory stays (each account's cache and Tailscale state), as Windows'
/// uninstall keeps `HelperState`. Blocking: run it off the UI thread.
/// `Err` is the message for the user.
pub fn run_remove(contents: &Path) -> Result<(), String> {
    let h = &s().helper;
    run_prompt(remove_command(h.remove_prompt, contents, false))
        .map_err(|error| error.message(h.remove_failed))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROMPT: &str = "BoxPilot wants to install its privileged helper for TUN mode.";

    #[test]
    fn the_lines_are_the_script() {
        assert_eq!(INSTALL_APPLESCRIPT_LINES.join("\n"), INSTALL_APPLESCRIPT);
        assert_eq!(REMOVE_APPLESCRIPT_LINES.join("\n"), REMOVE_APPLESCRIPT);
    }

    /// The app's path, quotes, `$(…)`, backticks, a newline: every value is
    /// passed through argv as it is, and the script text never changes.
    #[test]
    fn install_passes_values_as_positional_args() {
        let contents =
            Path::new("/Users/eve/Box it'\"$(touch /tmp/x)`id`\n; rm -rf ~/BoxPilot.app/Contents");
        let (program, args) = install_command(PROMPT, contents, 501);
        assert_eq!(program, "/usr/bin/osascript");
        let script = INSTALL_APPLESCRIPT_LINES.len() * 2;
        for (pair, line) in args[..script].chunks(2).zip(INSTALL_APPLESCRIPT_LINES) {
            assert_eq!(pair, ["-e", *line]);
        }
        assert_eq!(
            args[script..],
            [
                PROMPT.to_owned(),
                // Joined as the platform joins paths: the tests run on
                // Windows too, where `join` adds a backslash.
                contents
                    .join("Resources/Helper")
                    .join("helper-install.sh")
                    .to_string_lossy()
                    .into_owned(),
                contents.to_string_lossy().into_owned(),
                "501".to_owned(),
            ]
        );
        // The values never reach the script text.
        for line in INSTALL_APPLESCRIPT_LINES {
            assert!(!line.contains("eve") && !line.contains("501"), "{line}");
        }
    }

    #[test]
    fn remove_passes_values_as_positional_args() {
        let contents = Path::new("/Applications/Box Pilot'$(x).app/Contents");
        for (remove_state, flag) in [(false, "--keep-state"), (true, "--remove-state")] {
            let (program, args) = remove_command(PROMPT, contents, remove_state);
            assert_eq!(program, "/usr/bin/osascript");
            let script = REMOVE_APPLESCRIPT_LINES.len() * 2;
            assert_eq!(
                args[..script],
                REMOVE_APPLESCRIPT_LINES
                    .iter()
                    .flat_map(|line| ["-e".to_owned(), (*line).to_owned()])
                    .collect::<Vec<_>>()[..]
            );
            assert_eq!(
                args[script..],
                [
                    PROMPT.to_owned(),
                    contents
                        .join("Resources/Helper")
                        .join("helper-uninstall.sh")
                        .to_string_lossy()
                        .into_owned(),
                    flag.to_owned(),
                ]
            );
        }
    }

    /// Each value reaches the shell only inside `quoted form of`, and the
    /// shell runs nothing but `/bin/sh <script> <args>`; the prompt is no
    /// shell text at all.
    #[test]
    fn the_shell_sees_only_quoted_values() {
        for (text, values) in [(INSTALL_APPLESCRIPT, 4), (REMOVE_APPLESCRIPT, 3)] {
            let command = text
                .lines()
                .find(|line| line.starts_with("set theCommand to "))
                .unwrap();
            assert!(command.starts_with("set theCommand to \"/bin/sh \" & "));
            assert_eq!(command.matches("quoted form of (item ").count(), values - 1);
            assert!(
                !command.contains("item 1 of argv"),
                "the prompt is not shell text"
            );
            for n in 2..=values {
                assert!(command.contains(&format!("quoted form of (item {n} of argv)")));
            }
            // Nothing but those items and literal spaces is concatenated.
            let rest = command
                .trim_start_matches("set theCommand to \"/bin/sh \" & ")
                .replace("quoted form of (item 2 of argv)", "")
                .replace("quoted form of (item 3 of argv)", "")
                .replace("quoted form of (item 4 of argv)", "")
                .replace(" & \" \" & ", "");
            assert_eq!(rest, "", "{command}");
            assert!(text.contains(&format!("(count of argv) is not {values}")));
            assert!(text.contains("with prompt thePrompt with administrator privileges"));
        }
    }

    #[test]
    fn the_scripts_are_the_bundles() {
        let contents = Path::new("/Applications/BoxPilot.app/Contents");
        assert_eq!(
            install_script_path(contents),
            Path::new("/Applications/BoxPilot.app/Contents/Resources/Helper/helper-install.sh")
        );
        assert_eq!(
            uninstall_script_path(contents),
            Path::new("/Applications/BoxPilot.app/Contents/Resources/Helper/helper-uninstall.sh")
        );
        assert!(!PROMPT.starts_with('-'));
    }

    /// The prompts BoxPilot passes are its own text, never taken for an
    /// osascript option.
    #[test]
    fn the_prompts_are_not_options() {
        for t in [&crate::i18n::EN, &crate::i18n::ZH_CN] {
            for prompt in [t.helper.install_prompt, t.helper.remove_prompt] {
                assert!(!prompt.trim().is_empty());
                assert!(!prompt.starts_with('-'), "{prompt}");
            }
        }
    }

    #[test]
    fn a_failed_prompt_says_why() {
        assert_eq!(
            prompt_error(Some(1), "0:298: execution error: User canceled. (-128)\n"),
            PromptError::Dismissed
        );
        assert_eq!(
            prompt_error(
                Some(1),
                "0:298: execution error: helper-install: no account has uid 4242 (1)\n"
            ),
            PromptError::Failed("helper-install: no account has uid 4242".into())
        );
        // Words that merely end in parentheses keep them.
        assert_eq!(
            prompt_error(Some(1), "launchctl bootstrap failed (try again)\n"),
            PromptError::Failed("launchctl bootstrap failed (try again)".into())
        );
        assert_eq!(
            prompt_error(Some(3), "\n  \n"),
            PromptError::Failed("exit code 3".into())
        );
        assert_eq!(
            prompt_error(None, ""),
            PromptError::Failed("osascript was terminated".into())
        );
        assert_eq!(
            PromptError::Dismissed.message(crate::i18n::EN.helper.install_failed),
            "The administrator prompt was cancelled; nothing changed."
        );
        assert_eq!(
            PromptError::Failed("x".into()).message(crate::i18n::EN.helper.remove_failed),
            "Couldn't remove the privileged helper: x"
        );
    }
}
