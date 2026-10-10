//! The helper's command line. The SCM starts it with none; the
//! `--console --root <dir>` test seam runs the same loop as a console
//! process, unprivileged, against a temporary tree (ADR 0006,
//! "Verification before shipping").

#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::path::PathBuf;

/// The Windows service's name (`boxpilot_protocol::endpoint`, shared with
/// the GUI).
pub const SERVICE_NAME: &str = boxpilot_protocol::endpoint::SERVICE_NAME;

/// The service's pipe (`boxpilot_protocol::endpoint`, shared with the GUI).
pub const SERVICE_PIPE: &str = boxpilot_protocol::endpoint::PIPE_NAME;

/// The console seam's pipe by default. Unprivileged, it can't create the
/// service's name, and must not pretend to be the service.
pub const CONSOLE_PIPE: &str = r"\\.\pipe\BoxPilot\helper-console";

/// The prefix every pipe name has.
const PIPE_PREFIX: &str = r"\\.\pipe\";

pub const USAGE: &str = "\
usage: boxpilot-helper
           as the BoxPilotHelper service, started by the Service Control Manager
       boxpilot-helper --console --root <dir> [--pipe <\\\\.\\pipe\\name>]
           the same loop as an unprivileged console process, against <dir>\\helper
           (sing-box and manifest.json) and <dir>\\state, for testing";

/// How the helper runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Under the SCM, from its fixed install directory.
    Service,
    /// The test seam.
    Console { root: PathBuf, pipe: String },
}

/// Parse the arguments after the program name.
pub fn parse(args: &[OsString]) -> Result<Mode, String> {
    if args.is_empty() {
        return Ok(Mode::Service);
    }
    let mut console = false;
    let mut root = None;
    let mut pipe = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--console") if !console => console = true,
            Some("--root") if root.is_none() => {
                let value = args.next().ok_or("--root needs a directory")?;
                root = Some(PathBuf::from(value));
            }
            Some("--pipe") if pipe.is_none() => {
                let value = args.next().ok_or("--pipe needs a pipe name")?;
                let value = value.to_str().ok_or("--pipe is not valid Unicode")?;
                if !is_console_pipe_name(value) {
                    return Err(format!(
                        "--pipe must be a name under {PIPE_PREFIX} outside ProtectedPrefix"
                    ));
                }
                pipe = Some(value.to_owned());
            }
            _ => return Err(format!("unexpected argument {arg:?}")),
        }
    }
    if !console {
        return Err("--root and --pipe go with --console".into());
    }
    let root = root.ok_or("--console needs --root <dir>")?;
    Ok(Mode::Console {
        root,
        pipe: pipe.unwrap_or_else(|| CONSOLE_PIPE.to_owned()),
    })
}

/// A pipe name the console seam may use: `\\.\pipe\` and 1 to 200 of
/// `A-Z a-z 0-9 . _ - \`, not under `ProtectedPrefix`, where the service's
/// pipe lives.
pub fn is_console_pipe_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix(PIPE_PREFIX) else {
        return false;
    };
    (1..=200).contains(&rest.len())
        && rest
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'\\'))
        && !rest.to_ascii_lowercase().starts_with("protectedprefix")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn no_arguments_is_the_service() {
        assert_eq!(parse(&[]), Ok(Mode::Service));
    }

    #[test]
    fn the_console_seam_needs_a_root() {
        assert_eq!(
            parse(&args(&["--console", "--root", r"C:\t\root"])),
            Ok(Mode::Console {
                root: PathBuf::from(r"C:\t\root"),
                pipe: CONSOLE_PIPE.into()
            })
        );
        assert_eq!(
            parse(&args(&[
                "--root",
                "r",
                "--console",
                "--pipe",
                r"\\.\pipe\bp-test-1"
            ])),
            Ok(Mode::Console {
                root: PathBuf::from("r"),
                pipe: r"\\.\pipe\bp-test-1".into()
            })
        );
        for bad in [
            &["--console"][..],
            &["--root", "r"],
            &["--console", "--root"],
            &["--console", "--root", "a", "--root", "b"],
            &["--console", "--console", "--root", "a"],
            &["--console", "--root", "a", "--service"],
            &["run"],
        ] {
            assert!(parse(&args(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_console_can_never_take_the_services_pipe() {
        for bad in [
            SERVICE_PIPE,
            r"\\.\pipe\protectedprefix\Administrators\BoxPilot\helper",
            r"\\.\pipe\",
            r"\\server\pipe\x",
            r"\\.\pipe\a b",
            r"\\.\pipe\a/b",
            "helper",
        ] {
            assert!(!is_console_pipe_name(bad), "{bad}");
            assert!(parse(&args(&["--console", "--root", "r", "--pipe", bad])).is_err());
        }
        assert!(is_console_pipe_name(CONSOLE_PIPE));
    }
}
