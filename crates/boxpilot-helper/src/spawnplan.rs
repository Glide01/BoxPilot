//! What sing-box is started with on Windows, as pure data (ADR 0006 rule 2,
//! "Environment", and "Defense in depth"): its command line, its
//! environment block, its process mitigations and its token. The Windows
//! layer passes them to `CreateProcessAsUserW` with the full application
//! path, so nothing is searched for.
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
//! - **Mitigations** ([`SING_BOX_MITIGATIONS`]): no image from a remote
//!   share or with a low integrity label, no legacy extension points.
//! - **Token** ([`SING_BOX_TOKEN`]): the helper's own process token,
//!   restricted (`CreateRestrictedToken`): every privilege not on a
//!   compile-time allowlist deleted, not merely disabled, so sing-box can't
//!   enable it again. Nothing at run time (no setting, environment
//!   variable, file or protocol field) can widen it: the helper passes this
//!   constant and nothing else.

#![forbid(unsafe_code)]

use crate::authority::{SE_GROUP_ENABLED, SE_GROUP_USE_FOR_DENY_ONLY};
use std::fmt;

/// `PROCESS_CREATION_MITIGATION_POLICY_EXTENSION_POINT_DISABLE_ALWAYS_ON`
/// (winbase.h): no AppInit DLLs, Winsock LSPs, global window hooks or IMEs
/// are loaded into the process. Windows 8 and later.
pub const MITIGATION_EXTENSION_POINT_DISABLE: u64 = 1 << 32;
/// `PROCESS_CREATION_MITIGATION_POLICY_IMAGE_LOAD_NO_REMOTE_ALWAYS_ON`: no
/// image from a remote device (a UNC share). Windows 10 1511 and later.
pub const MITIGATION_IMAGE_LOAD_NO_REMOTE: u64 = 1 << 52;
/// `PROCESS_CREATION_MITIGATION_POLICY_IMAGE_LOAD_NO_LOW_LABEL_ALWAYS_ON`:
/// no image a low-integrity process could have written (one with a low
/// mandatory label). Windows 10 1511 and later.
pub const MITIGATION_IMAGE_LOAD_NO_LOW_LABEL: u64 = 1 << 56;

/// The process mitigations sing-box starts with
/// (`PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY`, one DWORD64): only ones that
/// can't stop a sing-box from working. Not `IMAGE_LOAD_PREFER_SYSTEM32`
/// (bit 60), which would change where `libcronet.dll` is looked for.
pub const SING_BOX_MITIGATIONS: u64 = MITIGATION_EXTENSION_POINT_DISABLE
    | MITIGATION_IMAGE_LOAD_NO_REMOTE
    | MITIGATION_IMAGE_LOAD_NO_LOW_LABEL;

/// Mandatory integrity levels: the RID of the label SID `S-1-16-<rid>`.
pub mod integrity {
    /// `SECURITY_MANDATORY_MEDIUM_RID`: a standard user's process.
    pub const MEDIUM: u32 = 0x2000;
    /// `SECURITY_MANDATORY_HIGH_RID`: an elevated administrator's process.
    pub const HIGH: u32 = 0x3000;
    /// `SECURITY_MANDATORY_SYSTEM_RID`: a service's, SYSTEM's.
    pub const SYSTEM: u32 = 0x4000;

    /// The label SID of `rid`, as a string.
    pub fn label_sid(rid: u32) -> String {
        format!("S-1-16-{rid}")
    }
}

/// `SE_GROUP_INTEGRITY`: the attribute of a token's integrity label.
pub const SE_GROUP_INTEGRITY: u32 = 0x0000_0020;

/// The token sing-box runs with, as a restriction of the helper's own
/// process token. Deny by default: what `privileges` doesn't name is
/// deleted, including any privilege a later Windows adds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenPlan<'a> {
    /// The privileges kept, if the helper holds them, with the attributes
    /// they had (sing-box may enable a disabled one, as it may today).
    /// Every other one is deleted (`PrivilegesToDelete`).
    pub privileges: &'a [&'a str],
    /// The highest integrity level sing-box may run at (a label RID from
    /// [`integrity`]); a token above it is lowered, never raised. `None`
    /// keeps the helper's.
    pub max_integrity: Option<u32>,
    /// Groups made deny-only (`SidsToDisable`): they still match deny ACEs,
    /// and never grant anything.
    pub deny_only: &'a [&'a str],
}

/// The privileges sing-box keeps. PROVISIONAL, a best estimate until the
/// CI token probe (`examples/token_probe.rs`) has measured what TUN needs
/// on Windows: `SeChangeNotifyPrivilege`, which every account holds (it
/// only skips traverse checks on the folders above a path), and
/// `SeLoadDriverPrivilege`, the one WireGuard's tunnel service keeps for
/// wintun.
pub const SING_BOX_PRIVILEGES: &[&str] = &["SeChangeNotifyPrivilege", "SeLoadDriverPrivilege"];

/// sing-box's token: the helper's, with only [`SING_BOX_PRIVILEGES`] left.
/// Its integrity level and groups stay the helper's until the probe shows
/// TUN still works without them (lowering to High; Administrators
/// deny-only).
pub const SING_BOX_TOKEN: TokenPlan<'static> = TokenPlan {
    privileges: SING_BOX_PRIVILEGES,
    max_integrity: None,
    deny_only: &[],
};

/// Privileges that make a process as strong as the kernel or as any
/// account: debugging any process, acting as part of the OS, impersonating
/// or creating tokens, reading or writing any file regardless of its ACL,
/// taking ownership, the security log, raw volumes, firmware variables.
/// sing-box's allowlist never names one (a unit test holds that); if TUN
/// ever needs one, that is a decision for ADR 0006, not a list edit.
pub const NEVER_FOR_SING_BOX: &[&str] = &[
    "SeAssignPrimaryTokenPrivilege",
    "SeBackupPrivilege",
    "SeCreateTokenPrivilege",
    "SeDebugPrivilege",
    "SeDelegateSessionUserImpersonatePrivilege",
    "SeEnableDelegationPrivilege",
    "SeImpersonatePrivilege",
    "SeManageVolumePrivilege",
    "SeRelabelPrivilege",
    "SeRestorePrivilege",
    "SeSecurityPrivilege",
    "SeSystemEnvironmentPrivilege",
    "SeTakeOwnershipPrivilege",
    "SeTcbPrivilege",
    "SeTrustedCredManAccessPrivilege",
];

/// Whether two privilege names are the same privilege: Windows looks them
/// up ignoring ASCII case.
pub fn same_privilege(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// The privileges to delete from a token that holds `held`, so that it
/// keeps only those `keep` names: every held one not on the allowlist. An
/// unknown or unreadable name is never on it, so it is deleted too.
pub fn privileges_to_delete<'h>(held: &'h [String], keep: &[&str]) -> Vec<&'h str> {
    held.iter()
        .map(String::as_str)
        .filter(|name| !names_privilege(keep, name))
        .collect()
}

/// Whether `list` names the privilege `name`.
fn names_privilege(list: &[&str], name: &str) -> bool {
    list.iter().any(|listed| same_privilege(listed, name))
}

/// Whether `list` names the SID `sid` (SID strings compare ignoring case:
/// `S-1-5-32-544` and `s-1-5-32-544` are the same SID).
fn names_sid(list: &[&str], sid: &str) -> bool {
    list.iter().any(|listed| listed.eq_ignore_ascii_case(sid))
}

/// The integrity level to set on the restricted token: `max` when the
/// helper's own level `current` is above it. Never a raise, which Windows
/// would refuse anyway.
pub fn integrity_to_set(current: u32, max: Option<u32>) -> Option<u32> {
    max.filter(|&max| current > max)
}

/// Of `deny_only`, the groups the token has that can still grant access
/// (not deny-only yet): the ones `SidsToDisable` must name.
pub fn sids_to_disable<'g>(groups: &'g [(String, u32)], deny_only: &[&str]) -> Vec<&'g str> {
    groups
        .iter()
        .filter(|(group, attributes)| {
            names_sid(deny_only, group) && attributes & SE_GROUP_USE_FOR_DENY_ONLY == 0
        })
        .map(|(group, _)| group.as_str())
        .collect()
}

/// What the Windows layer reads back from a token.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObservedToken {
    /// `TokenPrivileges`: each privilege's name and attributes.
    pub privileges: Vec<(String, u32)>,
    /// The integrity level's RID, if it could be read.
    pub integrity: Option<u32>,
    /// `TokenGroups`: each group's SID string and attributes.
    pub groups: Vec<(String, u32)>,
}

/// What `token` holds beyond `plan`: a privilege not on the allowlist, an
/// integrity level above the cap (or none readable when there is a cap), a
/// group that should be deny-only and still grants. Empty when the token is
/// within the plan; the helper checks the token it made with this, and
/// refuses to start sing-box under one that isn't.
pub fn excess(token: &ObservedToken, plan: &TokenPlan<'_>) -> Vec<String> {
    let mut excess: Vec<String> = token
        .privileges
        .iter()
        .filter(|(name, _)| !names_privilege(plan.privileges, name))
        .map(|(name, _)| format!("the privilege {name}"))
        .collect();
    if let Some(max) = plan.max_integrity {
        match token.integrity {
            Some(level) if level <= max => {}
            Some(level) => excess.push(format!("integrity level 0x{level:x}, above 0x{max:x}")),
            None => excess.push("an unreadable integrity level".to_owned()),
        }
    }
    for (group, attributes) in &token.groups {
        if names_sid(plan.deny_only, group)
            && (attributes & SE_GROUP_USE_FOR_DENY_ONLY == 0 || attributes & SE_GROUP_ENABLED != 0)
        {
            excess.push(format!("the group {group} (attributes 0x{attributes:x})"));
        }
    }
    excess
}

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

    /// The winbase.h values (mingw-w64 and the Windows SDK agree), and
    /// nothing else: "prefer System32" (bit 60) stays off.
    #[test]
    fn sing_box_starts_with_exactly_these_mitigations() {
        assert_eq!(MITIGATION_EXTENSION_POINT_DISABLE, 0x0000_0001_0000_0000);
        assert_eq!(MITIGATION_IMAGE_LOAD_NO_REMOTE, 0x0010_0000_0000_0000);
        assert_eq!(MITIGATION_IMAGE_LOAD_NO_LOW_LABEL, 0x0100_0000_0000_0000);
        assert_eq!(SING_BOX_MITIGATIONS, 0x0110_0001_0000_0000);
        assert_eq!(SING_BOX_MITIGATIONS & (1 << 60), 0);
    }

    /// What `whoami /priv` lists for SYSTEM on Windows Server 2022: the
    /// shape of what the service holds.
    fn system_privileges() -> Vec<String> {
        [
            "SeAssignPrimaryTokenPrivilege",
            "SeLockMemoryPrivilege",
            "SeIncreaseQuotaPrivilege",
            "SeTcbPrivilege",
            "SeSecurityPrivilege",
            "SeTakeOwnershipPrivilege",
            "SeLoadDriverPrivilege",
            "SeSystemProfilePrivilege",
            "SeSystemtimePrivilege",
            "SeProfileSingleProcessPrivilege",
            "SeIncreaseBasePriorityPrivilege",
            "SeCreatePagefilePrivilege",
            "SeCreatePermanentPrivilege",
            "SeBackupPrivilege",
            "SeRestorePrivilege",
            "SeShutdownPrivilege",
            "SeDebugPrivilege",
            "SeAuditPrivilege",
            "SeSystemEnvironmentPrivilege",
            "SeChangeNotifyPrivilege",
            "SeUndockPrivilege",
            "SeManageVolumePrivilege",
            "SeImpersonatePrivilege",
            "SeCreateGlobalPrivilege",
            "SeIncreaseWorkingSetPrivilege",
            "SeTimeZonePrivilege",
            "SeCreateSymbolicLinkPrivilege",
            "SeDelegateSessionUserImpersonatePrivilege",
        ]
        .map(String::from)
        .to_vec()
    }

    const BA: &str = "S-1-5-32-544";

    #[test]
    fn the_allowlist_never_holds_a_privilege_that_equals_the_kernel() {
        assert_eq!(SING_BOX_TOKEN.privileges, SING_BOX_PRIVILEGES);
        for kept in SING_BOX_PRIVILEGES {
            assert!(
                !NEVER_FOR_SING_BOX
                    .iter()
                    .any(|never| same_privilege(kept, never)),
                "{kept} is on sing-box's allowlist"
            );
        }
        // At least the bypass of traverse checking, which every account has.
        assert!(SING_BOX_PRIVILEGES
            .iter()
            .any(|kept| same_privilege(kept, "SeChangeNotifyPrivilege")));
        // The ones the task names, among others.
        for dangerous in [
            "SeDebugPrivilege",
            "SeTcbPrivilege",
            "SeImpersonatePrivilege",
            "SeAssignPrimaryTokenPrivilege",
            "SeBackupPrivilege",
            "SeRestorePrivilege",
            "SeTakeOwnershipPrivilege",
            "SeCreateTokenPrivilege",
        ] {
            assert!(NEVER_FOR_SING_BOX.contains(&dangerous), "{dangerous}");
        }
    }

    #[test]
    fn every_privilege_not_on_the_allowlist_is_deleted() {
        let held = system_privileges();
        let deleted = privileges_to_delete(&held, SING_BOX_PRIVILEGES);
        assert_eq!(deleted.len(), held.len() - 2);
        assert!(!deleted.contains(&"SeChangeNotifyPrivilege"));
        assert!(!deleted.contains(&"SeLoadDriverPrivilege"));
        for never in NEVER_FOR_SING_BOX {
            assert!(
                !held.iter().any(|name| name == never) || deleted.contains(never),
                "{never} is kept"
            );
        }
        // Compared as Windows compares them; anything unknown goes.
        let odd: Vec<String> = ["sechangenotifyprivilege", "SeNextYearPrivilege", "#0:99"]
            .map(String::from)
            .to_vec();
        assert_eq!(
            privileges_to_delete(&odd, SING_BOX_PRIVILEGES),
            ["SeNextYearPrivilege", "#0:99"]
        );
        // A standard user's token (the console seam) has no
        // SeLoadDriverPrivilege to keep: the rest still goes.
        let user: Vec<String> = [
            "SeShutdownPrivilege",
            "SeChangeNotifyPrivilege",
            "SeUndockPrivilege",
            "SeIncreaseWorkingSetPrivilege",
            "SeTimeZonePrivilege",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(
            privileges_to_delete(&user, SING_BOX_PRIVILEGES),
            [
                "SeShutdownPrivilege",
                "SeUndockPrivilege",
                "SeIncreaseWorkingSetPrivilege",
                "SeTimeZonePrivilege"
            ]
        );
        // An empty allowlist deletes everything.
        assert_eq!(privileges_to_delete(&user, &[]).len(), user.len());
    }

    #[test]
    fn the_integrity_level_is_lowered_never_raised() {
        use integrity::{HIGH, MEDIUM, SYSTEM};
        assert_eq!(integrity_to_set(SYSTEM, Some(HIGH)), Some(HIGH));
        assert_eq!(integrity_to_set(HIGH, Some(HIGH)), None);
        assert_eq!(integrity_to_set(MEDIUM, Some(HIGH)), None);
        assert_eq!(integrity_to_set(SYSTEM, None), None);
        assert_eq!(integrity::label_sid(HIGH), "S-1-16-12288");
        assert_eq!(integrity::label_sid(SYSTEM), "S-1-16-16384");
    }

    #[test]
    fn only_groups_that_still_grant_are_made_deny_only() {
        let groups = vec![
            ("S-1-5-32-544".to_owned(), 0xe),
            ("S-1-1-0".to_owned(), 0x7),
            ("S-1-5-11".to_owned(), 0x7),
        ];
        assert_eq!(sids_to_disable(&groups, &[BA]), [BA]);
        assert!(sids_to_disable(&groups, &[]).is_empty());
        // Already deny-only (an unelevated administrator), or absent (a
        // standard user): nothing to do.
        let filtered = vec![("S-1-5-32-544".to_owned(), SE_GROUP_USE_FOR_DENY_ONLY)];
        assert!(sids_to_disable(&filtered, &[BA]).is_empty());
        assert!(sids_to_disable(&groups[1..], &[BA]).is_empty());
    }

    #[test]
    fn a_token_beyond_its_plan_is_named() {
        let plan = TokenPlan {
            privileges: SING_BOX_PRIVILEGES,
            max_integrity: Some(integrity::HIGH),
            deny_only: &[BA],
        };
        let within = ObservedToken {
            privileges: vec![
                ("SeChangeNotifyPrivilege".into(), 3),
                ("SeLoadDriverPrivilege".into(), 0),
            ],
            integrity: Some(integrity::HIGH),
            groups: vec![
                (BA.into(), SE_GROUP_USE_FOR_DENY_ONLY),
                ("S-1-1-0".into(), 7),
            ],
        };
        assert!(excess(&within, &plan).is_empty());
        // Fewer privileges than allowed is within it.
        let fewer = ObservedToken {
            privileges: vec![("SeChangeNotifyPrivilege".into(), 3)],
            ..within.clone()
        };
        assert!(excess(&fewer, &plan).is_empty());

        let mut wide = within.clone();
        wide.privileges.push(("SeDebugPrivilege".into(), 0));
        wide.integrity = Some(integrity::SYSTEM);
        wide.groups[0].1 = 0xe;
        let found = excess(&wide, &plan);
        assert_eq!(found.len(), 3, "{found:?}");
        assert!(found[0].contains("SeDebugPrivilege"));
        assert!(found[1].contains("0x4000"));
        assert!(found[2].contains(BA));

        let unreadable = ObservedToken {
            integrity: None,
            ..within.clone()
        };
        assert_eq!(excess(&unreadable, &plan).len(), 1);
        // With no cap and no deny-only groups, only privileges count.
        assert!(excess(&unreadable, &SING_BOX_TOKEN).is_empty());
        let system = ObservedToken {
            integrity: Some(integrity::SYSTEM),
            groups: vec![(BA.into(), 0xe)],
            ..within
        };
        assert!(excess(&system, &SING_BOX_TOKEN).is_empty());
    }

    #[test]
    fn sing_box_gets_exactly_its_arguments() {
        let run = r"C:\Program Files\BoxPilot\HelperState\runs\0123abcd";
        let config = format!(r"{run}\config.json");
        let args = sing_box_args(run, &config);
        let line = command_line(r"C:\Program Files\BoxPilot\Helper\sing-box.exe", &args).unwrap();
        assert_eq!(
            line,
            r#""C:\Program Files\BoxPilot\Helper\sing-box.exe" run -D "C:\Program Files\BoxPilot\HelperState\runs\0123abcd" -c "C:\Program Files\BoxPilot\HelperState\runs\0123abcd\config.json" --disable-color"#
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
