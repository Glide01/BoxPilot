//! Installing and removing the macOS privileged helper (ADR 0006 rule 7):
//! the one administrator prompt, as the command that shows it. Pure, and
//! built on every OS so its tests run everywhere; the macOS GUI wires it up
//! in phase 2 (Settings › TUN, "Install helper" / "Remove helper").
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

#![allow(dead_code)] // Phase 2 wires it up.

use boxpilot_protocol::endpoint::macos::{BUNDLE_PAYLOAD_DIR, INSTALL_SCRIPT, UNINSTALL_SCRIPT};
use std::path::Path;

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
}
