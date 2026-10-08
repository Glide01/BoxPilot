//! Who may start and stop sing-box through the helper (ADR 0006 rule 4),
//! decided from what the OS says about the caller's token, as pure data so
//! it is tested on every OS. The Windows layer reads the token by
//! impersonating the pipe client; the PID is never used.
//!
//! Members of Administrators and of Network Configuration Operators may
//! start and stop, as in WireGuard for Windows: who belongs to those groups
//! is the administrator's call. Administrators counts whether the caller
//! runs elevated or not: UAC's filtered token keeps the group as deny-only,
//! and an administrator's unelevated GUI is the expected caller. Network
//! Configuration Operators counts only when enabled.
//!
//! Everything else is read-only (`hello` and `status`), and so is any token
//! that is not a plain user's: a restricted token, an AppContainer, or one
//! below medium integrity (a sandboxed browser process of an administrator
//! still carries the deny-only Administrators group). The pipe's DACL and
//! default integrity label keep most such callers out already; this does
//! not rely on it.

#![forbid(unsafe_code)]

use crate::acl::sid;
use boxpilot_protocol::Authority;

/// `SE_GROUP_ENABLED`.
pub const SE_GROUP_ENABLED: u32 = 0x0000_0004;
/// `SE_GROUP_USE_FOR_DENY_ONLY`.
pub const SE_GROUP_USE_FOR_DENY_ONLY: u32 = 0x0000_0010;
/// `SECURITY_MANDATORY_MEDIUM_RID`: a normal user process.
pub const MEDIUM_INTEGRITY: u32 = 0x2000;

/// What the Windows layer reads from the caller's token.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenFacts {
    /// `TokenUser`, as a SID string.
    pub user: Option<String>,
    /// `TokenGroups`: each group's SID string and attributes.
    pub groups: Vec<(String, u32)>,
    /// The integrity level's RID (`TokenIntegrityLevel`).
    pub integrity: Option<u32>,
    /// The token has restricting SIDs (`TokenRestrictedSids`).
    pub restricted: bool,
    /// `TokenIsAppContainer`.
    pub app_container: bool,
}

/// Whether these groups make a caller one that may start: Administrators,
/// enabled or deny-only, or Network Configuration Operators, enabled.
pub fn groups_may_start(groups: &[(String, u32)]) -> bool {
    groups.iter().any(|(group, attributes)| {
        let enabled = attributes & SE_GROUP_ENABLED != 0;
        let deny_only = attributes & SE_GROUP_USE_FOR_DENY_ONLY != 0;
        match group.as_str() {
            sid::ADMINISTRATORS => enabled || deny_only,
            sid::NETWORK_CONFIGURATION_OPERATORS => enabled && !deny_only,
            _ => false,
        }
    })
}

/// The caller's authority. Anything missing or unusual is read-only.
pub fn authority(facts: &TokenFacts) -> Authority {
    let plain_token = facts.user.is_some()
        && !facts.restricted
        && !facts.app_container
        && facts
            .integrity
            .is_some_and(|level| level >= MEDIUM_INTEGRITY);
    if plain_token && groups_may_start(&facts.groups) {
        Authority::MayStart
    } else {
        Authority::ReadOnly
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANDATORY: u32 = 0x1;
    const ENABLED_BY_DEFAULT: u32 = 0x2;
    const OWNER: u32 = 0x8;
    const LOGON_ID: u32 = 0xC000_0000;
    const ALICE: &str = "S-1-5-21-1004336348-1177238915-682003330-1001";
    const HIGH: u32 = 0x3000;
    const LOW: u32 = 0x1000;

    /// The groups of an ordinary interactive user's token.
    fn user_groups() -> Vec<(String, u32)> {
        let on = MANDATORY | ENABLED_BY_DEFAULT | SE_GROUP_ENABLED;
        vec![
            ("S-1-5-21-1004336348-1177238915-682003330-513".into(), on),
            (sid::EVERYONE.into(), on),
            ("S-1-5-32-545".into(), on),
            (sid::INTERACTIVE.into(), on),
            ("S-1-2-1".into(), on),
            (sid::AUTHENTICATED_USERS.into(), on),
            ("S-1-5-15".into(), on),
            ("S-1-5-5-0-123456".into(), on | LOGON_ID),
            ("S-1-2-0".into(), on),
            ("S-1-5-64-10".into(), on),
        ]
    }

    fn facts(groups: Vec<(String, u32)>, integrity: u32) -> TokenFacts {
        TokenFacts {
            user: Some(ALICE.into()),
            groups,
            integrity: Some(integrity),
            restricted: false,
            app_container: false,
        }
    }

    fn with(group: &str, attributes: u32) -> Vec<(String, u32)> {
        let mut groups = user_groups();
        groups.push((group.into(), attributes));
        groups
    }

    #[test]
    fn a_standard_user_is_read_only() {
        assert_eq!(
            authority(&facts(user_groups(), MEDIUM_INTEGRITY)),
            Authority::ReadOnly
        );
    }

    /// An administrator's elevated token, and the filtered token UAC gives
    /// the same administrator unelevated (Administrators deny-only).
    #[test]
    fn an_administrator_may_start_elevated_or_not() {
        let elevated = with(
            sid::ADMINISTRATORS,
            MANDATORY | ENABLED_BY_DEFAULT | SE_GROUP_ENABLED | OWNER,
        );
        assert_eq!(authority(&facts(elevated, HIGH)), Authority::MayStart);
        let filtered = with(sid::ADMINISTRATORS, SE_GROUP_USE_FOR_DENY_ONLY);
        assert_eq!(
            authority(&facts(filtered, MEDIUM_INTEGRITY)),
            Authority::MayStart
        );
    }

    #[test]
    fn network_configuration_operators_may_start_only_when_enabled() {
        let enabled = with(
            sid::NETWORK_CONFIGURATION_OPERATORS,
            MANDATORY | ENABLED_BY_DEFAULT | SE_GROUP_ENABLED,
        );
        assert_eq!(
            authority(&facts(enabled, MEDIUM_INTEGRITY)),
            Authority::MayStart
        );
        for attributes in [
            0,
            SE_GROUP_USE_FOR_DENY_ONLY,
            SE_GROUP_ENABLED | SE_GROUP_USE_FOR_DENY_ONLY,
        ] {
            let groups = with(sid::NETWORK_CONFIGURATION_OPERATORS, attributes);
            assert_eq!(
                authority(&facts(groups, MEDIUM_INTEGRITY)),
                Authority::ReadOnly,
                "{attributes:#x}"
            );
        }
    }

    /// A group that is listed but neither enabled nor deny-only grants
    /// nothing; nor do the group's SID as the user, or look-alikes.
    #[test]
    fn only_the_exact_groups_count() {
        for (group, attributes) in [
            (sid::ADMINISTRATORS, 0),
            (sid::ADMINISTRATORS, MANDATORY),
            ("S-1-5-32-5440", SE_GROUP_ENABLED),
            ("s-1-5-32-544", SE_GROUP_ENABLED),
            ("S-1-5-32-544 ", SE_GROUP_ENABLED),
            ("S-1-5-21-1-2-3-500", SE_GROUP_ENABLED),
            ("S-1-5-32-547", SE_GROUP_ENABLED),
        ] {
            assert_eq!(
                authority(&facts(with(group, attributes), HIGH)),
                Authority::ReadOnly,
                "{group:?} {attributes:#x}"
            );
        }
        let mut as_user = facts(user_groups(), HIGH);
        as_user.user = Some(sid::ADMINISTRATORS.into());
        assert_eq!(authority(&as_user), Authority::ReadOnly);
    }

    /// An administrator's sandboxed or low-integrity process is read-only.
    #[test]
    fn unusual_tokens_are_read_only() {
        let admin = || {
            facts(
                with(sid::ADMINISTRATORS, SE_GROUP_USE_FOR_DENY_ONLY),
                MEDIUM_INTEGRITY,
            )
        };
        let mut restricted = admin();
        restricted.restricted = true;
        let mut app_container = admin();
        app_container.app_container = true;
        let mut low = admin();
        low.integrity = Some(LOW);
        let mut unknown_level = admin();
        unknown_level.integrity = None;
        let mut no_user = admin();
        no_user.user = None;
        for token in [restricted, app_container, low, unknown_level, no_user] {
            assert_eq!(authority(&token), Authority::ReadOnly, "{token:?}");
        }
        assert_eq!(authority(&TokenFacts::default()), Authority::ReadOnly);
        assert_eq!(authority(&admin()), Authority::MayStart);
    }

    /// SYSTEM's own tools (system integrity, Administrators enabled) count
    /// as administrators.
    #[test]
    fn system_integrity_is_above_medium() {
        let system = with(sid::ADMINISTRATORS, SE_GROUP_ENABLED | OWNER);
        assert_eq!(authority(&facts(system, 0x4000)), Authority::MayStart);
    }
}
