//! The two tokens the helper controls on Windows, as pure data (ADR 0006,
//! "Defense in depth"): sing-box's, a restricted copy of the helper's
//! ([`SING_BOX_TOKEN`]), and the helper's own, from which it removes every
//! privilege it doesn't need when it starts ([`HELPER_TOKEN`]).
//!
//! Both are compile-time constants. Nothing at run time (no setting,
//! environment variable, file, service configuration or protocol field)
//! can widen them: the Windows layer passes these and nothing else, and
//! checks each token it ends up with against its plan ([`excess`]).
//!
//! What each keeps was measured, not guessed: `examples/token_probe.rs`
//! starts the installed sing-box under one token after another on CI's
//! Windows runner, on every run, and the smoke test reads both tokens from
//! outside.

#![forbid(unsafe_code)]

use crate::acl::sid;
use crate::authority::{SE_GROUP_ENABLED, SE_GROUP_USE_FOR_DENY_ONLY};

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

/// A token, as what it keeps of the token it is made from. Deny by
/// default: what `privileges` doesn't name is removed, including any
/// privilege a later Windows adds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenPlan<'a> {
    /// The privileges kept, if held, with the attributes they had (a
    /// process may enable a disabled one). Every other one is removed:
    /// deleted from sing-box's copy (`PrivilegesToDelete`), removed from
    /// the helper's own token (`SE_PRIVILEGE_REMOVED`).
    pub privileges: &'a [&'a str],
    /// The highest integrity level the token may have (a label RID from
    /// [`integrity`]); a token above it is lowered, never raised. `None`
    /// keeps the level it had.
    pub max_integrity: Option<u32>,
    /// Groups made deny-only (`SidsToDisable`): they still match deny ACEs,
    /// and never grant anything.
    pub deny_only: &'a [&'a str],
}

/// The privileges sing-box keeps: only `SeChangeNotifyPrivilege`, which
/// every account holds (it only skips traverse checks on the folders above
/// a path). Measured on Windows Server 2025 with sing-box 1.14.0: with it
/// alone, wintun installs its driver on a machine's first TUN start, loads
/// it on later ones, and TUN (`auto_route`, `strict_route`, DNS hijacking,
/// `stack: mixed`) carries traffic. Not even `SeLoadDriverPrivilege`, which
/// WireGuard's tunnel service keeps for wintun: presumably because Windows'
/// device installation, not sing-box's process, installs and loads the
/// driver.
pub const SING_BOX_PRIVILEGES: &[&str] = &["SeChangeNotifyPrivilege"];

/// sing-box's token: the helper's, with only [`SING_BOX_PRIVILEGES`] left,
/// and lowered from System to High integrity, which TUN doesn't need
/// either (measured as above), so sing-box can't write to anything
/// labelled System.
///
/// Administrators stays enabled. Made deny-only, sing-box's TUN start fails
/// where `strict_route` adds its WFP sublayer (`FwpmSubLayerAdd0: invalid
/// argument`): the filtering engine wants an administrator to add one.
pub const SING_BOX_TOKEN: TokenPlan<'static> = TokenPlan {
    privileges: SING_BOX_PRIVILEGES,
    max_integrity: Some(integrity::HIGH),
    deny_only: &[],
};

/// The privileges the helper keeps of its own token when it starts:
/// `SeChangeNotifyPrivilege`, and `SeLoadDriverPrivilege`, which it may
/// need to uninstall a crashed sing-box's stale adapter. Measured: with
/// the SCM giving it only these two (`sc.exe privs`), the helper serves the
/// pipe, reads callers' tokens (identification-level, so no
/// `SeImpersonatePrivilege`), makes sing-box's restricted token, starts it
/// with `CreateProcessAsUserW` (no `SeAssignPrimaryTokenPrivilege` or
/// `SeIncreaseQuotaPrivilege`) and removes its adapter afterwards. Whether
/// it needs `SeLoadDriverPrivilege` at all is being measured (CI's helper
/// token trial with `SeChangeNotifyPrivilege` alone).
pub const HELPER_PRIVILEGES: &[&str] = &["SeChangeNotifyPrivilege", "SeLoadDriverPrivilege"];

/// The helper's own token after it has dropped what it doesn't need. Only
/// privileges: it keeps its integrity level and groups, which are what make
/// it SYSTEM, and what lets it create its pipe under
/// `ProtectedPrefix\Administrators`.
pub const HELPER_TOKEN: TokenPlan<'static> = TokenPlan {
    privileges: HELPER_PRIVILEGES,
    max_integrity: None,
    deny_only: &[],
};

/// Privileges that make a process as strong as the kernel or as any
/// account: debugging any process, acting as part of the OS, impersonating
/// or creating tokens, reading or writing any file regardless of its ACL,
/// taking ownership, the security log, raw volumes, firmware variables.
/// Neither allowlist names one (a unit test holds that); if TUN or the
/// helper ever needs one, that is a decision for ADR 0006, not a list edit.
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

/// The Administrators group, which `strict_route` needs enabled.
pub const ADMINISTRATORS: &str = sid::ADMINISTRATORS;

/// Whether two privilege names are the same privilege: Windows looks them
/// up ignoring ASCII case.
pub fn same_privilege(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// The privileges to remove from a token that holds `held`, so that it
/// keeps only those `keep` names: every held one not on the allowlist. An
/// unknown or unreadable name is never on it, so it is removed too.
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
/// within the plan. Fewer privileges than allowed is within it: the helper
/// can't keep what the SCM didn't give it. The Windows layer checks every
/// token it ends up with by this, and refuses to go on with one that isn't
/// within its plan.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// What SYSTEM held on CI's Windows Server 2025 runner (the token
    /// probe's record), plus SeCreateTokenPrivilege, which LSASS holds.
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
            "SeCreateTokenPrivilege",
        ]
        .map(String::from)
        .to_vec()
    }

    const BA: &str = "S-1-5-32-544";

    #[test]
    fn neither_allowlist_holds_a_privilege_that_equals_the_kernel() {
        assert_eq!(SING_BOX_TOKEN.privileges, SING_BOX_PRIVILEGES);
        assert_eq!(HELPER_TOKEN.privileges, HELPER_PRIVILEGES);
        for kept in SING_BOX_PRIVILEGES.iter().chain(HELPER_PRIVILEGES) {
            assert!(
                !NEVER_FOR_SING_BOX
                    .iter()
                    .any(|never| same_privilege(kept, never)),
                "{kept} is on an allowlist"
            );
        }
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

    /// What the probe measured and ADR 0006 records: sing-box keeps the
    /// bypass of traverse checking and nothing else, runs at High
    /// integrity, and keeps Administrators, which WFP needs. The helper
    /// can't hand sing-box a privilege it dropped itself, so its own list
    /// holds sing-box's.
    #[test]
    fn the_plans_are_what_was_measured() {
        assert_eq!(SING_BOX_PRIVILEGES, ["SeChangeNotifyPrivilege"]);
        assert_eq!(SING_BOX_TOKEN.max_integrity, Some(integrity::HIGH));
        assert!(SING_BOX_TOKEN.deny_only.is_empty());
        assert!(!names_sid(SING_BOX_TOKEN.deny_only, ADMINISTRATORS));
        for kept in SING_BOX_PRIVILEGES {
            assert!(names_privilege(HELPER_PRIVILEGES, kept), "{kept}");
        }
        assert_eq!(HELPER_TOKEN.max_integrity, None);
        assert!(HELPER_TOKEN.deny_only.is_empty());
    }

    #[test]
    fn every_privilege_not_on_the_allowlist_is_deleted() {
        let held = system_privileges();
        let deleted = privileges_to_delete(&held, SING_BOX_PRIVILEGES);
        assert_eq!(deleted.len(), held.len() - 1);
        assert!(!deleted.contains(&"SeChangeNotifyPrivilege"));
        assert!(deleted.contains(&"SeLoadDriverPrivilege"));
        for never in NEVER_FOR_SING_BOX {
            assert!(
                !held.iter().any(|name| name == never) || deleted.contains(never),
                "{never} is kept"
            );
        }
        let removed = privileges_to_delete(&held, HELPER_PRIVILEGES);
        assert_eq!(removed.len(), held.len() - 2);
        assert!(!removed.contains(&"SeLoadDriverPrivilege"));
        // Compared as Windows compares them; anything unknown goes.
        let odd: Vec<String> = ["sechangenotifyprivilege", "SeNextYearPrivilege", "#0:99"]
            .map(String::from)
            .to_vec();
        assert_eq!(
            privileges_to_delete(&odd, SING_BOX_PRIVILEGES),
            ["SeNextYearPrivilege", "#0:99"]
        );
        // A standard user's token (the console seam).
        let user: Vec<String> = [
            "SeShutdownPrivilege",
            "SeChangeNotifyPrivilege",
            "SeUndockPrivilege",
            "SeIncreaseWorkingSetPrivilege",
            "SeTimeZonePrivilege",
        ]
        .map(String::from)
        .to_vec();
        let expected = [
            "SeShutdownPrivilege",
            "SeUndockPrivilege",
            "SeIncreaseWorkingSetPrivilege",
            "SeTimeZonePrivilege",
        ];
        assert_eq!(privileges_to_delete(&user, SING_BOX_PRIVILEGES), expected);
        assert_eq!(privileges_to_delete(&user, HELPER_PRIVILEGES), expected);
        // An empty allowlist removes everything.
        assert_eq!(privileges_to_delete(&user, &[]).len(), user.len());
    }

    #[test]
    fn the_integrity_level_is_lowered_never_raised() {
        use integrity::{HIGH, MEDIUM, SYSTEM};
        assert_eq!(integrity_to_set(SYSTEM, Some(HIGH)), Some(HIGH));
        assert_eq!(integrity_to_set(HIGH, Some(HIGH)), None);
        // The console seam's standard user stays at Medium.
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
        assert!(sids_to_disable(&groups, SING_BOX_TOKEN.deny_only).is_empty());
        // Already deny-only (an unelevated administrator), or absent (a
        // standard user): nothing to do.
        let filtered = vec![("S-1-5-32-544".to_owned(), SE_GROUP_USE_FOR_DENY_ONLY)];
        assert!(sids_to_disable(&filtered, &[BA]).is_empty());
        assert!(sids_to_disable(&groups[1..], &[BA]).is_empty());
    }

    #[test]
    fn a_token_beyond_its_plan_is_named() {
        let plan = TokenPlan {
            privileges: &["SeChangeNotifyPrivilege", "SeLoadDriverPrivilege"],
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
    }

    /// sing-box's token as the helper makes it is within its plan; the one
    /// it had before this ADR's measurement (LoadDriver, System) is not.
    #[test]
    fn sing_box_and_the_helper_are_judged_by_their_plans() {
        let sing_box = ObservedToken {
            privileges: vec![("SeChangeNotifyPrivilege".into(), 3)],
            integrity: Some(integrity::HIGH),
            groups: vec![(BA.into(), 0xe)],
        };
        assert!(excess(&sing_box, &SING_BOX_TOKEN).is_empty());
        let before = ObservedToken {
            privileges: vec![
                ("SeChangeNotifyPrivilege".into(), 3),
                ("SeLoadDriverPrivilege".into(), 0),
            ],
            integrity: Some(integrity::SYSTEM),
            ..sing_box.clone()
        };
        assert_eq!(excess(&before, &SING_BOX_TOKEN).len(), 2);

        // The helper's own token: SYSTEM's integrity and groups stay; only
        // privileges count.
        let helper = ObservedToken {
            privileges: vec![
                ("SeLoadDriverPrivilege".into(), 0),
                ("SeChangeNotifyPrivilege".into(), 3),
            ],
            integrity: Some(integrity::SYSTEM),
            groups: vec![(BA.into(), 0xe)],
        };
        assert!(excess(&helper, &HELPER_TOKEN).is_empty());
        let undropped = ObservedToken {
            privileges: system_privileges()
                .into_iter()
                .map(|name| (name, 0))
                .collect(),
            ..helper
        };
        assert_eq!(
            excess(&undropped, &HELPER_TOKEN).len(),
            system_privileges().len() - 2
        );
    }
}
