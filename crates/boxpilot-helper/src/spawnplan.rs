//! What sing-box is started with on Windows, as pure data (ADR 0006 rule 2,
//! "Environment"): its command line and its environment block. The Windows
//! layer passes both to `CreateProcessW` with the full application path, so
//! nothing is searched for.
//!
//! - **Arguments**: `run -D <run dir> -c <run dir>\config.json
//!   --disable-color`, quoted by the rules the MSVC runtime and Go both
//!   parse by, so a path with spaces stays one argument.
//! - **Environment**: built from nothing, never inherited from the service:
//!   `SystemRoot` and `windir`, a `PATH` of only `System32` and the Windows
//!   directory (the naive outbound loads `libcronet.dll` from beside
//!   sing-box, then from `PATH`, and a user-writable `PATH` entry would let
//!   a user plant that DLL in a SYSTEM process), and `TEMP`, `TMP` and
//!   `USERPROFILE` inside the run directory.

#![forbid(unsafe_code)]

use std::fmt;

/// Why a command line or environment block could not be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// A value holds a NUL, which would end it early.
    Nul,
    /// A variable name is empty or holds `=`.
    BadName,
    /// The program path holds a `"`, which no Windows path can.
    QuoteInProgram,
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            PlanError::Nul => "a value holds a NUL character",
            PlanError::BadName => "an environment variable name is empty or holds `=`",
            PlanError::QuoteInProgram => "the program path holds a quote",
        })
    }
}

impl std::error::Error for PlanError {}

/// sing-box's arguments for a run in `run_dir`, its config at `config`.
pub fn sing_box_args(run_dir: &str, config: &str) -> Vec<String> {
    vec![
        "run".into(),
        "-D".into(),
        run_dir.into(),
        "-c".into(),
        config.into(),
        "--disable-color".into(),
    ]
}

/// The command line `CreateProcessW` takes: `program`, quoted as Windows
/// reads `argv[0]` (whole, between quotes, no escapes), then each of
/// `args` quoted by the MSVC rules.
pub fn command_line(program: &str, args: &[String]) -> Result<String, PlanError> {
    if program.contains('"') {
        return Err(PlanError::QuoteInProgram);
    }
    if program.contains('\0') || args.iter().any(|arg| arg.contains('\0')) {
        return Err(PlanError::Nul);
    }
    let mut line = format!("\"{program}\"");
    for arg in args {
        line.push(' ');
        quote(arg, &mut line);
    }
    Ok(line)
}

/// Append `arg` so `CommandLineToArgvW` and the MSVC runtime read it back
/// as exactly `arg`: bare when it has no space, tab, newline or quote;
/// otherwise in quotes, each `"` escaped, and the backslashes before a `"`
/// or the closing quote doubled.
fn quote(arg: &str, out: &mut String) {
    let plain = !arg.is_empty() && !arg.contains([' ', '\t', '\n', '\u{b}', '"']);
    if plain {
        out.push_str(arg);
        return;
    }
    out.push('"');
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            c => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
}

/// sing-box's whole environment: the Windows directory `system_root`,
/// `temp` for `TEMP` and `TMP`, `profile` for `USERPROFILE`. Sorted by
/// name, case-insensitively, as an environment block must be.
pub fn environment(system_root: &str, temp: &str, profile: &str) -> Vec<(String, String)> {
    let root = system_root.trim_end_matches('\\');
    let mut vars = vec![
        ("SystemRoot".to_owned(), root.to_owned()),
        ("windir".to_owned(), root.to_owned()),
        ("PATH".to_owned(), format!("{root}\\System32;{root}")),
        ("TEMP".to_owned(), temp.to_owned()),
        ("TMP".to_owned(), temp.to_owned()),
        ("USERPROFILE".to_owned(), profile.to_owned()),
    ];
    vars.sort_by_key(|(name, _)| name.to_uppercase());
    vars
}

/// The UTF-16 environment block `CreateProcessW` takes with
/// `CREATE_UNICODE_ENVIRONMENT`: each `name=value` NUL-terminated, and one
/// more NUL at the end.
pub fn environment_block(vars: &[(String, String)]) -> Result<Vec<u16>, PlanError> {
    let mut block = Vec::new();
    for (name, value) in vars {
        if name.is_empty() || name.contains('=') {
            return Err(PlanError::BadName);
        }
        if name.contains('\0') || value.contains('\0') {
            return Err(PlanError::Nul);
        }
        block.extend(name.encode_utf16());
        block.push(u16::from(b'='));
        block.extend(value.encode_utf16());
        block.push(0);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    Ok(block)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The MSVC runtime's reading of a command line's arguments after
    /// `argv[0]` (the 2008 rules, which `CommandLineToArgvW` and Go share
    /// for these cases), to check `quote` against.
    fn parse_args(line: &str) -> Vec<String> {
        let mut args = Vec::new();
        let mut chars = line.chars().peekable();
        loop {
            while chars.peek().is_some_and(|c| *c == ' ' || *c == '\t') {
                chars.next();
            }
            if chars.peek().is_none() {
                return args;
            }
            let mut arg = String::new();
            let mut quoted = false;
            loop {
                match chars.peek().copied() {
                    None => break,
                    Some(' ' | '\t') if !quoted => break,
                    Some('\\') => {
                        let mut n = 0;
                        while chars.peek() == Some(&'\\') {
                            chars.next();
                            n += 1;
                        }
                        if chars.peek() == Some(&'"') {
                            arg.extend(std::iter::repeat_n('\\', n / 2));
                            if n % 2 == 1 {
                                arg.push('"');
                                chars.next();
                            }
                        } else {
                            arg.extend(std::iter::repeat_n('\\', n));
                        }
                    }
                    Some('"') => {
                        chars.next();
                        if quoted && chars.peek() == Some(&'"') {
                            arg.push('"');
                            chars.next();
                        } else {
                            quoted = !quoted;
                        }
                    }
                    Some(c) => {
                        arg.push(c);
                        chars.next();
                    }
                }
            }
            args.push(arg);
        }
    }

    /// The arguments of `line`, `argv[0]` skipped.
    fn args_of(line: &str) -> Vec<String> {
        let rest = line
            .strip_prefix('"')
            .and_then(|rest| rest.split_once('"'))
            .expect("argv[0] is quoted")
            .1;
        parse_args(rest)
    }

    #[test]
    fn sing_box_gets_exactly_its_arguments() {
        let run = r"C:\ProgramData\BoxPilot\Helper\runs\0123abcd";
        let config = format!(r"{run}\config.json");
        let args = sing_box_args(run, &config);
        let line = command_line(r"C:\Program Files\BoxPilot\Helper\sing-box.exe", &args).unwrap();
        assert_eq!(
            line,
            r#""C:\Program Files\BoxPilot\Helper\sing-box.exe" run -D C:\ProgramData\BoxPilot\Helper\runs\0123abcd -c C:\ProgramData\BoxPilot\Helper\runs\0123abcd\config.json --disable-color"#
        );
        assert_eq!(args_of(&line), args);
    }

    #[test]
    fn hard_arguments_round_trip() {
        for arg in [
            "",
            "a b",
            r"C:\Users\A B\Temp\",
            r"C:\dir with space\\",
            r#"say "hi""#,
            r#"\""#,
            r"\\server\share\x y",
            "tab\there",
            "new\nline",
            r#"a\\"b"#,
            "中文 路径",
        ] {
            let args = vec![arg.to_owned(), "next".to_owned()];
            let line = command_line(r"C:\x.exe", &args).unwrap();
            assert_eq!(args_of(&line), args, "{arg:?} as {line}");
        }
    }

    #[test]
    fn what_cant_be_passed_is_refused() {
        assert_eq!(
            command_line(r#"C:\a"b.exe"#, &[]),
            Err(PlanError::QuoteInProgram)
        );
        assert_eq!(
            command_line(r"C:\a.exe", &["x\0y".into()]),
            Err(PlanError::Nul)
        );
    }

    #[test]
    fn the_environment_is_built_from_nothing() {
        let vars = environment(r"C:\Windows\", r"C:\run\tmp", r"C:\run\home");
        assert_eq!(
            vars,
            [
                ("PATH", r"C:\Windows\System32;C:\Windows"),
                ("SystemRoot", r"C:\Windows"),
                ("TEMP", r"C:\run\tmp"),
                ("TMP", r"C:\run\tmp"),
                ("USERPROFILE", r"C:\run\home"),
                ("windir", r"C:\Windows"),
            ]
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
        );
    }

    #[test]
    fn the_block_is_nul_separated_and_double_terminated() {
        let block = environment_block(&[("A".into(), "1".into()), ("Bé".into(), "".into())]);
        assert_eq!(
            block.unwrap(),
            "A=1\0Bé=\0\0".encode_utf16().collect::<Vec<_>>()
        );
        assert_eq!(environment_block(&[]).unwrap(), [0, 0]);
        assert_eq!(
            environment_block(&[("A=B".into(), "1".into())]),
            Err(PlanError::BadName)
        );
        assert_eq!(
            environment_block(&[("".into(), "1".into())]),
            Err(PlanError::BadName)
        );
        assert_eq!(
            environment_block(&[("A".into(), "1\0B=2".into())]),
            Err(PlanError::Nul)
        );
    }
}
