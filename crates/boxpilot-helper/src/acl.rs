//! Who may change what the helper trusts (ADR 0006 rules 3 and 7): the
//! judgement over an object's owner and DACL, as pure data, so it is tested
//! on every OS. The Windows layer reads the real security descriptors and
//! asks here.
//!
//! The helper runs only files from its own directory and keeps its state in
//! its own tree, so both must be beyond any non-administrator's reach, along
//! the whole path to them:
//!
//! - **no reparse point** anywhere in the chain: a junction or a symlink can
//!   point the helper at a tree someone else controls;
//! - **every owner trusted**: SYSTEM, Administrators or TrustedInstaller. An
//!   owner may always rewrite the DACL;
//! - **the helper directory and each file the helper runs or reads there**
//!   ([`Role::Object`]): no ACE that applies to it grants a
//!   non-administrator any write-class right. That includes `FILE_ADD_FILE`:
//!   the application directory comes first in the DLL search order, so a
//!   user who can add a file there can plant a DLL into a SYSTEM process.
//!   Reading is fine: these are the installed binaries;
//! - **the state directory and each directory in it** ([`Role::Private`]):
//!   no ACE grants a non-administrator any right at all, reading included.
//!   It holds every caller's cache file and Tailscale node keys, so Program
//!   Files' inherited "Users: Read & execute" is refused there; only a
//!   protected SYSTEM + Administrators DACL passes;
//! - **each ancestor**: no ACE grants a non-administrator `DELETE`,
//!   `WRITE_DAC`, `WRITE_OWNER`, `FILE_DELETE_CHILD`, `GENERIC_ALL` or
//!   `GENERIC_WRITE`, any of which would let them swap out or re-permission
//!   what lies below. Adding files beside it is fine: users may create
//!   folders in `C:\`.
//!
//! Inherit-only ACEs don't apply to the object itself. On an ancestor they
//! are skipped: what the helper uses below it is judged on its own. On the
//! helper's own directories they are not: the helper and sing-box create
//! files there (the log, the cache file, Tailscale state), and those files
//! inherit them. Only `CREATOR OWNER` is skipped there, since it becomes
//! the creator, SYSTEM. Deny ACEs only take rights away, so they never make
//! an object unsafe. An allow ACE of a type the helper doesn't read fails
//! closed. Generic rights count both as themselves and as the file rights
//! they map to.

#![forbid(unsafe_code)]

use std::fmt;

/// Well-known SID strings.
pub mod sid {
    pub const SYSTEM: &str = "S-1-5-18";
    pub const ADMINISTRATORS: &str = "S-1-5-32-544";
    pub const TRUSTED_INSTALLER: &str =
        "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464";
    pub const USERS: &str = "S-1-5-32-545";
    pub const NETWORK_CONFIGURATION_OPERATORS: &str = "S-1-5-32-556";
    pub const AUTHENTICATED_USERS: &str = "S-1-5-11";
    pub const INTERACTIVE: &str = "S-1-5-4";
    pub const EVERYONE: &str = "S-1-1-0";
    pub const CREATOR_OWNER: &str = "S-1-3-0";
    pub const OWNER_RIGHTS: &str = "S-1-3-4";
    pub const ALL_APPLICATION_PACKAGES: &str = "S-1-15-2-1";
    pub const ALL_RESTRICTED_APPLICATION_PACKAGES: &str = "S-1-15-2-2";
}

/// Access rights of file and directory objects (`winnt.h`). For a
/// directory, `FILE_WRITE_DATA` is `FILE_ADD_FILE` and `FILE_APPEND_DATA`
/// is `FILE_ADD_SUBDIRECTORY`.
pub mod right {
    pub const FILE_READ_DATA: u32 = 0x0001;
    pub const FILE_WRITE_DATA: u32 = 0x0002;
    pub const FILE_APPEND_DATA: u32 = 0x0004;
    pub const FILE_READ_EA: u32 = 0x0008;
    pub const FILE_WRITE_EA: u32 = 0x0010;
    pub const FILE_EXECUTE: u32 = 0x0020;
    pub const FILE_DELETE_CHILD: u32 = 0x0040;
    pub const FILE_READ_ATTRIBUTES: u32 = 0x0080;
    pub const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
    pub const DELETE: u32 = 0x0001_0000;
    pub const READ_CONTROL: u32 = 0x0002_0000;
    pub const WRITE_DAC: u32 = 0x0004_0000;
    pub const WRITE_OWNER: u32 = 0x0008_0000;
    pub const SYNCHRONIZE: u32 = 0x0010_0000;
    pub const GENERIC_ALL: u32 = 0x1000_0000;
    pub const GENERIC_EXECUTE: u32 = 0x2000_0000;
    pub const GENERIC_WRITE: u32 = 0x4000_0000;
    pub const GENERIC_READ: u32 = 0x8000_0000;
    /// `FA` in SDDL, "Full control".
    pub const FILE_ALL_ACCESS: u32 = 0x001F_01FF;
    pub const FILE_GENERIC_READ: u32 = 0x0012_0089;
    pub const FILE_GENERIC_WRITE: u32 = 0x0012_0116;
    pub const FILE_GENERIC_EXECUTE: u32 = 0x0012_00A0;
    /// "Modify" (icacls `M`).
    pub const MODIFY: u32 = 0x0013_01BF;
    /// "Read & execute" (icacls `RX`).
    pub const READ_EXECUTE: u32 = 0x0012_00A9;
}

/// ACE types (`AceType`).
pub mod ace_type {
    pub const ALLOWED: u8 = 0x0;
    pub const DENIED: u8 = 0x1;
    pub const ALLOWED_OBJECT: u8 = 0x5;
    pub const DENIED_OBJECT: u8 = 0x6;
    pub const ALLOWED_CALLBACK: u8 = 0x9;
    pub const DENIED_CALLBACK: u8 = 0xA;
    pub const ALLOWED_CALLBACK_OBJECT: u8 = 0xB;
    pub const DENIED_CALLBACK_OBJECT: u8 = 0xC;
}

/// ACE flags (`AceFlags`).
pub mod ace_flag {
    pub const OBJECT_INHERIT: u8 = 0x01;
    pub const CONTAINER_INHERIT: u8 = 0x02;
    pub const NO_PROPAGATE_INHERIT: u8 = 0x04;
    pub const INHERIT_ONLY: u8 = 0x08;
    pub const INHERITED: u8 = 0x10;
}

use right::*;

/// The rights no non-administrator may hold on the object itself: any that
/// writes its data, its contents (a new file or folder in a directory), its
/// attributes or extended attributes, its security, or deletes it or
/// something in it.
pub const OBJECT_FORBIDDEN: u32 = FILE_WRITE_DATA
    | FILE_APPEND_DATA
    | FILE_WRITE_EA
    | FILE_DELETE_CHILD
    | FILE_WRITE_ATTRIBUTES
    | DELETE
    | WRITE_DAC
    | WRITE_OWNER
    | GENERIC_WRITE
    | GENERIC_ALL;

/// The rights no non-administrator may hold on the state directory or a
/// directory in it: every right but `SYNCHRONIZE`, which grants nothing on
/// its own. Reading is refused as well as writing: the tree holds each
/// caller's cache file and Tailscale node keys.
pub const PRIVATE_FORBIDDEN: u32 = !SYNCHRONIZE;

/// The rights no non-administrator may hold on an ancestor: any that lets
/// them replace, rename or re-permission what lies below it.
pub const ANCESTOR_FORBIDDEN: u32 =
    DELETE | WRITE_DAC | WRITE_OWNER | FILE_DELETE_CHILD | GENERIC_WRITE | GENERIC_ALL;

/// `mask` with each generic right also mapped to the file rights it stands
/// for (`IoFileObjectType`'s generic mapping).
pub fn map_generic(mask: u32) -> u32 {
    let mut mapped = mask;
    if mask & GENERIC_READ != 0 {
        mapped |= FILE_GENERIC_READ;
    }
    if mask & GENERIC_WRITE != 0 {
        mapped |= FILE_GENERIC_WRITE;
    }
    if mask & GENERIC_EXECUTE != 0 {
        mapped |= FILE_GENERIC_EXECUTE;
    }
    if mask & GENERIC_ALL != 0 {
        mapped |= FILE_ALL_ACCESS;
    }
    mapped
}

/// One ACE of a DACL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ace {
    pub ace_type: u8,
    pub flags: u8,
    pub mask: u32,
    /// The trustee's SID string. Empty for an ACE type whose SID the
    /// Windows layer doesn't read (object ACEs), which never get as far as
    /// a SID check.
    pub sid: String,
}

impl Ace {
    pub fn allow(flags: u8, mask: u32, sid: &str) -> Self {
        Self {
            ace_type: ace_type::ALLOWED,
            flags,
            mask,
            sid: sid.to_owned(),
        }
    }

    pub fn deny(flags: u8, mask: u32, sid: &str) -> Self {
        Self {
            ace_type: ace_type::DENIED,
            flags,
            mask,
            sid: sid.to_owned(),
        }
    }
}

/// What the Windows layer reads about one file or directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Security {
    pub owner: String,
    /// `None` for a NULL DACL, which grants everyone everything.
    pub dacl: Option<Vec<Ace>>,
    pub reparse_point: bool,
}

/// Where in the chain an object stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The helper directory, or a file the helper runs or reads in it:
    /// nobody but administrators may write it.
    Object,
    /// The state directory, or a directory in it: nobody but
    /// administrators may write it, or read it either.
    Private,
    /// A directory above one.
    Ancestor,
}

/// The SIDs that count as administrators: they may own anything in the
/// chain and hold any right on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trusted(Vec<String>);

impl Trusted {
    /// SYSTEM, Administrators and TrustedInstaller: the service's set.
    pub fn administrators() -> Self {
        Self(vec![
            sid::SYSTEM.into(),
            sid::ADMINISTRATORS.into(),
            sid::TRUSTED_INSTALLER.into(),
        ])
    }

    /// The service's set and `user`: for the unprivileged `--console` test
    /// seam only, which runs as `user` against `user`'s own temporary tree
    /// and refuses to run elevated, so trusting `user` lends nothing.
    pub fn administrators_and(user: &str) -> Self {
        let mut trusted = Self::administrators();
        trusted.0.push(user.to_owned());
        trusted
    }

    pub fn contains(&self, sid: &str) -> bool {
        self.0.iter().any(|trusted| trusted == sid)
    }

    pub fn sids(&self) -> &[String] {
        &self.0
    }
}

/// Why an object is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AclRefusal {
    ReparsePoint,
    Owner {
        sid: String,
    },
    NullDacl,
    /// An ACE that applies to the object, of a type the helper doesn't
    /// read: it might grant anything.
    UnknownAce {
        ace_type: u8,
    },
    /// An ACE grants a non-administrator a right it may not hold there.
    Grants {
        sid: String,
        mask: u32,
    },
}

impl fmt::Display for AclRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AclRefusal::ReparsePoint => f.write_str("is a reparse point (a link or junction)"),
            AclRefusal::Owner { sid } => {
                write!(
                    f,
                    "is owned by {sid}, not SYSTEM, Administrators or TrustedInstaller"
                )
            }
            AclRefusal::NullDacl => f.write_str("has no DACL, so everyone may change it"),
            AclRefusal::UnknownAce { ace_type } => {
                write!(
                    f,
                    "has an ACE of type 0x{ace_type:02x} the helper can't judge"
                )
            }
            AclRefusal::Grants { sid, mask } => write!(
                f,
                "grants {sid}, who is not an administrator, rights it may not hold there \
                 (0x{mask:08x})"
            ),
        }
    }
}

impl std::error::Error for AclRefusal {}

/// Judge one object of the chain.
pub fn judge(security: &Security, role: Role, trusted: &Trusted) -> Result<(), AclRefusal> {
    if security.reparse_point {
        return Err(AclRefusal::ReparsePoint);
    }
    if !trusted.contains(&security.owner) {
        return Err(AclRefusal::Owner {
            sid: security.owner.clone(),
        });
    }
    let dacl = security.dacl.as_ref().ok_or(AclRefusal::NullDacl)?;
    let forbidden = match role {
        Role::Object => OBJECT_FORBIDDEN,
        Role::Private => PRIVATE_FORBIDDEN,
        Role::Ancestor => ANCESTOR_FORBIDDEN,
    };
    for ace in dacl {
        if !applies(ace, role) {
            continue;
        }
        match ace.ace_type {
            ace_type::ALLOWED | ace_type::ALLOWED_CALLBACK => {}
            ace_type::DENIED
            | ace_type::DENIED_OBJECT
            | ace_type::DENIED_CALLBACK
            | ace_type::DENIED_CALLBACK_OBJECT => continue,
            other => return Err(AclRefusal::UnknownAce { ace_type: other }),
        }
        if trusted.contains(&ace.sid) {
            continue;
        }
        let granted = map_generic(ace.mask) & forbidden;
        if granted != 0 {
            return Err(AclRefusal::Grants {
                sid: ace.sid.clone(),
                mask: ace.mask,
            });
        }
    }
    Ok(())
}

/// Whether `ace` counts for an object in `role`: every ACE that applies to
/// the object itself, and on one of the helper's own (not an ancestor),
/// also every ACE its new files and folders inherit, `CREATOR OWNER` aside
/// (it becomes their creator).
fn applies(ace: &Ace, role: Role) -> bool {
    if ace.flags & ace_flag::INHERIT_ONLY == 0 {
        return true;
    }
    let inherited = ace.flags & (ace_flag::OBJECT_INHERIT | ace_flag::CONTAINER_INHERIT) != 0;
    role != Role::Ancestor && inherited && ace.sid != sid::CREATOR_OWNER
}

#[cfg(test)]
mod tests {
    //! The fixtures are the default ACLs of Windows 10 and 11 as `icacls`
    //! and `Get-Acl` show them, written from knowledge of those defaults,
    //! not measured on a machine for this test. The Windows checklist in the
    //! helper's report covers comparing them with a real install.

    use super::ace_flag::*;
    use super::sid::*;
    use super::*;

    const OI: u8 = OBJECT_INHERIT;
    const CI: u8 = CONTAINER_INHERIT;
    const IO: u8 = INHERIT_ONLY;
    const ID: u8 = INHERITED;
    const TI: &str = TRUSTED_INSTALLER;
    const ALICE: &str = "S-1-5-21-1004336348-1177238915-682003330-1001";

    fn object(owner: &str, dacl: Vec<Ace>) -> Security {
        Security {
            owner: owner.into(),
            dacl: Some(dacl),
            reparse_point: false,
        }
    }

    fn admins() -> Trusted {
        Trusted::administrators()
    }

    /// `C:\`: `O:TI D:PAI(A;OICI;FA;;;BA)(A;OICI;FA;;;SY)(A;OICI;RX;;;BU)
    /// (A;OICIIO;SDGXGWGR;;;AU)(A;;LC;;;AU)`. Authenticated Users may create
    /// folders in it (`AD`), and get Modify on what they create.
    fn drive_root() -> Security {
        object(
            TI,
            vec![
                Ace::allow(OI | CI, FILE_ALL_ACCESS, ADMINISTRATORS),
                Ace::allow(OI | CI, FILE_ALL_ACCESS, SYSTEM),
                Ace::allow(OI | CI, READ_EXECUTE, USERS),
                Ace::allow(
                    OI | CI | IO,
                    DELETE | GENERIC_EXECUTE | GENERIC_WRITE | GENERIC_READ,
                    AUTHENTICATED_USERS,
                ),
                Ace::allow(0, FILE_APPEND_DATA, AUTHENTICATED_USERS),
            ],
        )
    }

    /// `C:\Program Files`, owned by TrustedInstaller: SYSTEM and
    /// Administrators may modify it and get full control below; users and
    /// app containers read and execute.
    fn program_files() -> Security {
        object(
            TI,
            vec![
                Ace::allow(0, FILE_ALL_ACCESS, TI),
                Ace::allow(CI | IO, GENERIC_ALL, TI),
                Ace::allow(0, MODIFY, SYSTEM),
                Ace::allow(OI | CI | IO, GENERIC_ALL, SYSTEM),
                Ace::allow(0, MODIFY, ADMINISTRATORS),
                Ace::allow(OI | CI | IO, GENERIC_ALL, ADMINISTRATORS),
                Ace::allow(0, READ_EXECUTE, USERS),
                Ace::allow(OI | CI | IO, GENERIC_READ | GENERIC_EXECUTE, USERS),
                Ace::allow(OI | CI | IO, GENERIC_ALL, CREATOR_OWNER),
                Ace::allow(0, READ_EXECUTE, ALL_APPLICATION_PACKAGES),
                Ace::allow(
                    OI | CI | IO,
                    GENERIC_READ | GENERIC_EXECUTE,
                    ALL_APPLICATION_PACKAGES,
                ),
                Ace::allow(0, READ_EXECUTE, ALL_RESTRICTED_APPLICATION_PACKAGES),
                Ace::allow(
                    OI | CI | IO,
                    GENERIC_READ | GENERIC_EXECUTE,
                    ALL_RESTRICTED_APPLICATION_PACKAGES,
                ),
            ],
        )
    }

    /// A folder the MSI creates under `C:\Program Files`
    /// (`BoxPilot`, `BoxPilot\Helper`), inheriting from it.
    fn program_files_child(owner: &str) -> Security {
        object(
            owner,
            vec![
                Ace::allow(ID, FILE_ALL_ACCESS, TI),
                Ace::allow(ID | CI | IO, GENERIC_ALL, TI),
                Ace::allow(ID, FILE_ALL_ACCESS, SYSTEM),
                Ace::allow(ID | OI | CI | IO, GENERIC_ALL, SYSTEM),
                Ace::allow(ID, FILE_ALL_ACCESS, ADMINISTRATORS),
                Ace::allow(ID | OI | CI | IO, GENERIC_ALL, ADMINISTRATORS),
                Ace::allow(ID, READ_EXECUTE, USERS),
                Ace::allow(ID | OI | CI | IO, GENERIC_READ | GENERIC_EXECUTE, USERS),
                Ace::allow(ID | OI | CI | IO, GENERIC_ALL, CREATOR_OWNER),
                Ace::allow(ID, READ_EXECUTE, ALL_APPLICATION_PACKAGES),
                Ace::allow(ID, READ_EXECUTE, ALL_RESTRICTED_APPLICATION_PACKAGES),
            ],
        )
    }

    /// A file the MSI installs there (`sing-box.exe`).
    fn program_files_file() -> Security {
        object(
            ADMINISTRATORS,
            vec![
                Ace::allow(ID, FILE_ALL_ACCESS, SYSTEM),
                Ace::allow(ID, FILE_ALL_ACCESS, ADMINISTRATORS),
                Ace::allow(ID, READ_EXECUTE, USERS),
                Ace::allow(ID, READ_EXECUTE, ALL_APPLICATION_PACKAGES),
                Ace::allow(ID, READ_EXECUTE, ALL_RESTRICTED_APPLICATION_PACKAGES),
            ],
        )
    }

    /// The state directory (`Program Files\BoxPilot\HelperState`) as the
    /// MSI and the helper create it, and every directory the helper
    /// creates in it: `O:SY D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)`.
    fn protected(owner: &str) -> Security {
        object(
            owner,
            vec![
                Ace::allow(OI | CI, FILE_ALL_ACCESS, SYSTEM),
                Ace::allow(OI | CI, FILE_ALL_ACCESS, ADMINISTRATORS),
            ],
        )
    }

    #[test]
    fn the_default_program_files_chain_passes() {
        let trusted = admins();
        assert_eq!(judge(&drive_root(), Role::Ancestor, &trusted), Ok(()));
        assert_eq!(judge(&program_files(), Role::Ancestor, &trusted), Ok(()));
        for owner in [ADMINISTRATORS, SYSTEM, TI] {
            let child = program_files_child(owner);
            assert_eq!(judge(&child, Role::Ancestor, &trusted), Ok(()));
            assert_eq!(judge(&child, Role::Object, &trusted), Ok(()));
        }
        assert_eq!(judge(&program_files_file(), Role::Object, &trusted), Ok(()));
    }

    /// `C:\`, `Program Files`, `Program Files\BoxPilot`, then the
    /// protected `HelperState`.
    #[test]
    fn the_state_chain_passes_with_a_protected_state_dir() {
        let trusted = admins();
        assert_eq!(judge(&drive_root(), Role::Ancestor, &trusted), Ok(()));
        assert_eq!(judge(&program_files(), Role::Ancestor, &trusted), Ok(()));
        assert_eq!(
            judge(
                &program_files_child(ADMINISTRATORS),
                Role::Ancestor,
                &trusted
            ),
            Ok(())
        );
        for owner in [SYSTEM, ADMINISTRATORS] {
            for role in [Role::Private, Role::Object] {
                assert_eq!(judge(&protected(owner), role, &trusted), Ok(()));
            }
        }
    }

    /// The state directory holds every caller's cache file and Tailscale
    /// node keys: what Program Files would hand down to a plain folder
    /// (Users and app containers may read and execute) is fine for the
    /// helper's binaries, and refused there.
    #[test]
    fn program_files_inherited_read_is_refused_for_the_state_dir() {
        let trusted = admins();
        let plain = program_files_child(ADMINISTRATORS);
        assert_eq!(judge(&plain, Role::Object, &trusted), Ok(()));
        assert_eq!(
            judge(&plain, Role::Private, &trusted),
            Err(AclRefusal::Grants {
                sid: USERS.into(),
                mask: READ_EXECUTE
            })
        );
        for (trustee, mask) in [
            (USERS, FILE_READ_DATA),
            (AUTHENTICATED_USERS, FILE_READ_ATTRIBUTES),
            (EVERYONE, READ_CONTROL),
            (ALL_APPLICATION_PACKAGES, GENERIC_READ),
            (ALICE, FILE_EXECUTE),
        ] {
            let mut security = protected(SYSTEM);
            security
                .dacl
                .as_mut()
                .unwrap()
                .push(Ace::allow(0, mask, trustee));
            assert_eq!(
                judge(&security, Role::Private, &trusted),
                Err(AclRefusal::Grants {
                    sid: trustee.into(),
                    mask
                }),
                "{trustee} {mask:#x}"
            );
        }
        // What files and folders created in it would inherit counts too.
        let mut inherited = protected(SYSTEM);
        inherited
            .dacl
            .as_mut()
            .unwrap()
            .push(Ace::allow(OI | CI | IO, GENERIC_READ, USERS));
        assert!(judge(&inherited, Role::Private, &trusted).is_err());
        // `SYNCHRONIZE` alone grants nothing.
        let mut synchronize = protected(SYSTEM);
        synchronize
            .dacl
            .as_mut()
            .unwrap()
            .push(Ace::allow(0, SYNCHRONIZE, USERS));
        assert_eq!(judge(&synchronize, Role::Private, &trusted), Ok(()));
    }

    /// Folders users may add to are fine above the helper's, never as its
    /// own.
    #[test]
    fn a_folder_users_may_add_to_is_no_helper_dir() {
        let trusted = admins();
        // What its new folders inherit (Modify for Authenticated Users)
        // refuses it first; its own `AD` would too.
        assert_eq!(
            judge(&drive_root(), Role::Object, &trusted),
            Err(AclRefusal::Grants {
                sid: AUTHENTICATED_USERS.into(),
                mask: DELETE | GENERIC_EXECUTE | GENERIC_WRITE | GENERIC_READ
            })
        );
        let mut without_inheritance = drive_root();
        without_inheritance
            .dacl
            .as_mut()
            .unwrap()
            .retain(|ace| ace.flags & IO == 0);
        assert_eq!(
            judge(&without_inheritance, Role::Object, &trusted),
            Err(AclRefusal::Grants {
                sid: AUTHENTICATED_USERS.into(),
                mask: FILE_APPEND_DATA
            })
        );
    }

    /// A folder a user created, wherever they could, is theirs: its owner
    /// gives it away, whatever its DACL says.
    #[test]
    fn a_users_folder_is_refused_by_its_owner() {
        for role in [Role::Object, Role::Private, Role::Ancestor] {
            assert_eq!(
                judge(&protected(ALICE), role, &admins()),
                Err(AclRefusal::Owner { sid: ALICE.into() })
            );
            assert_eq!(
                judge(&program_files_child(ALICE), role, &admins()),
                Err(AclRefusal::Owner { sid: ALICE.into() })
            );
        }
    }

    #[test]
    fn a_reparse_point_is_refused_wherever_it_stands() {
        let mut link = program_files_child(ADMINISTRATORS);
        link.reparse_point = true;
        for role in [Role::Object, Role::Private, Role::Ancestor] {
            assert_eq!(judge(&link, role, &admins()), Err(AclRefusal::ReparsePoint));
        }
    }

    #[test]
    fn a_null_dacl_is_refused() {
        let open = Security {
            owner: SYSTEM.into(),
            dacl: None,
            reparse_point: false,
        };
        assert_eq!(
            judge(&open, Role::Ancestor, &admins()),
            Err(AclRefusal::NullDacl)
        );
        // An empty DACL grants nobody anything.
        assert_eq!(
            judge(&object(SYSTEM, vec![]), Role::Object, &admins()),
            Ok(())
        );
    }

    /// Each write-class right alone refuses the object, for any
    /// non-administrator, generic rights included.
    #[test]
    fn any_write_right_on_the_object_is_refused() {
        for mask in [
            FILE_WRITE_DATA,
            FILE_APPEND_DATA,
            FILE_WRITE_EA,
            FILE_DELETE_CHILD,
            FILE_WRITE_ATTRIBUTES,
            DELETE,
            WRITE_DAC,
            WRITE_OWNER,
            GENERIC_WRITE,
            GENERIC_ALL,
            MODIFY,
            FILE_ALL_ACCESS,
        ] {
            for trustee in [USERS, AUTHENTICATED_USERS, EVERYONE, INTERACTIVE, ALICE] {
                let mut security = protected(SYSTEM);
                security
                    .dacl
                    .as_mut()
                    .unwrap()
                    .push(Ace::allow(0, mask, trustee));
                for role in [Role::Object, Role::Private] {
                    assert_eq!(
                        judge(&security, role, &admins()),
                        Err(AclRefusal::Grants {
                            sid: trustee.into(),
                            mask
                        }),
                        "{mask:#x} for {trustee} on {role:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn reading_and_running_are_fine() {
        for mask in [
            READ_EXECUTE,
            FILE_GENERIC_READ,
            GENERIC_READ | GENERIC_EXECUTE,
            READ_CONTROL | SYNCHRONIZE | FILE_READ_ATTRIBUTES | FILE_READ_EA,
        ] {
            let mut security = protected(SYSTEM);
            security
                .dacl
                .as_mut()
                .unwrap()
                .push(Ace::allow(0, mask, USERS));
            assert_eq!(
                judge(&security, Role::Object, &admins()),
                Ok(()),
                "{mask:#x}"
            );
            assert_eq!(
                judge(&security, Role::Ancestor, &admins()),
                Ok(()),
                "{mask:#x}"
            );
        }
    }

    #[test]
    fn an_ancestor_may_let_users_add_but_not_replace() {
        for mask in [
            FILE_WRITE_DATA,
            FILE_APPEND_DATA,
            FILE_WRITE_EA,
            FILE_WRITE_ATTRIBUTES,
        ] {
            let mut security = program_files();
            security
                .dacl
                .as_mut()
                .unwrap()
                .push(Ace::allow(0, mask, USERS));
            assert_eq!(
                judge(&security, Role::Ancestor, &admins()),
                Ok(()),
                "{mask:#x}"
            );
        }
        for mask in [
            DELETE,
            WRITE_DAC,
            WRITE_OWNER,
            FILE_DELETE_CHILD,
            GENERIC_WRITE,
            GENERIC_ALL,
            MODIFY,
        ] {
            let mut security = program_files();
            security
                .dacl
                .as_mut()
                .unwrap()
                .push(Ace::allow(CI, mask, USERS));
            assert_eq!(
                judge(&security, Role::Ancestor, &admins()),
                Err(AclRefusal::Grants {
                    sid: USERS.into(),
                    mask
                }),
                "{mask:#x}"
            );
        }
    }

    /// The owner check covers the owner's implicit right to rewrite the
    /// DACL; `OWNER RIGHTS` and `CREATOR OWNER` ACEs are not administrators.
    #[test]
    fn only_administrators_count_as_trusted() {
        for trustee in [CREATOR_OWNER, OWNER_RIGHTS, NETWORK_CONFIGURATION_OPERATORS] {
            let mut security = protected(SYSTEM);
            security
                .dacl
                .as_mut()
                .unwrap()
                .push(Ace::allow(0, FILE_ALL_ACCESS, trustee));
            assert!(
                judge(&security, Role::Object, &admins()).is_err(),
                "{trustee}"
            );
        }
    }

    #[test]
    fn deny_aces_grant_nothing() {
        let mut security = protected(SYSTEM);
        let dacl = security.dacl.as_mut().unwrap();
        dacl.push(Ace::deny(0, FILE_ALL_ACCESS, EVERYONE));
        dacl.push(Ace::deny(OI | CI | IO, FILE_ALL_ACCESS, EVERYONE));
        dacl.push(Ace {
            ace_type: ace_type::DENIED_OBJECT,
            flags: 0,
            mask: FILE_ALL_ACCESS,
            sid: String::new(),
        });
        assert_eq!(judge(&security, Role::Object, &admins()), Ok(()));
    }

    /// On an ancestor, what children inherit is judged where the helper
    /// uses them.
    #[test]
    fn an_ancestors_inherit_only_aces_grant_nothing_here() {
        for flags in [
            OI | IO,
            CI | IO,
            OI | CI | IO,
            OI | CI | IO | NO_PROPAGATE_INHERIT,
        ] {
            let mut security = program_files();
            security
                .dacl
                .as_mut()
                .unwrap()
                .push(Ace::allow(flags, GENERIC_ALL, USERS));
            assert_eq!(
                judge(&security, Role::Ancestor, &admins()),
                Ok(()),
                "{flags:#x}"
            );
        }
    }

    /// The state directory and each caller's are where the helper's log,
    /// the cache file and the Tailscale state are created: a right they
    /// would inherit counts as a right on the directory.
    #[test]
    fn what_the_objects_new_files_inherit_counts() {
        for flags in [
            OI | IO,
            CI | IO,
            OI | CI | IO,
            OI | IO | NO_PROPAGATE_INHERIT,
        ] {
            for mask in [
                GENERIC_WRITE,
                GENERIC_ALL,
                FILE_WRITE_DATA,
                DELETE,
                WRITE_DAC,
            ] {
                let mut security = protected(SYSTEM);
                security
                    .dacl
                    .as_mut()
                    .unwrap()
                    .push(Ace::allow(flags, mask, USERS));
                assert_eq!(
                    judge(&security, Role::Object, &admins()),
                    Err(AclRefusal::Grants {
                        sid: USERS.into(),
                        mask
                    }),
                    "{flags:#x} {mask:#x}"
                );
            }
        }
        // Reading and running are as fine for children as for the object.
        let mut readable = protected(SYSTEM);
        readable.dacl.as_mut().unwrap().push(Ace::allow(
            OI | CI | IO,
            GENERIC_READ | GENERIC_EXECUTE,
            USERS,
        ));
        assert_eq!(judge(&readable, Role::Object, &admins()), Ok(()));
        // An inherit-only ACE that names no child applies to nothing.
        let mut inert = protected(SYSTEM);
        inert
            .dacl
            .as_mut()
            .unwrap()
            .push(Ace::allow(IO, GENERIC_ALL, USERS));
        assert_eq!(judge(&inert, Role::Object, &admins()), Ok(()));
        // An ACE type the helper doesn't read fails closed here too.
        let mut unknown = protected(SYSTEM);
        unknown.dacl.as_mut().unwrap().push(Ace {
            ace_type: ace_type::ALLOWED_OBJECT,
            flags: OI | CI | IO,
            mask: FILE_GENERIC_READ,
            sid: String::new(),
        });
        assert_eq!(
            judge(&unknown, Role::Object, &admins()),
            Err(AclRefusal::UnknownAce {
                ace_type: ace_type::ALLOWED_OBJECT
            })
        );
    }

    /// `CREATOR OWNER` becomes whoever creates the child: in the helper's
    /// trees, the helper or sing-box, as SYSTEM.
    #[test]
    fn creator_owner_passes_to_the_creator() {
        let mut security = protected(SYSTEM);
        security
            .dacl
            .as_mut()
            .unwrap()
            .push(Ace::allow(OI | CI | IO, GENERIC_ALL, CREATOR_OWNER));
        assert_eq!(judge(&security, Role::Object, &admins()), Ok(()));
        // Not where it applies to the object itself.
        security.dacl.as_mut().unwrap().last_mut().unwrap().flags = OI | CI;
        assert!(judge(&security, Role::Object, &admins()).is_err());
    }

    #[test]
    fn an_allow_ace_the_helper_cant_read_fails_closed() {
        for ace_type in [
            ace_type::ALLOWED_OBJECT,
            ace_type::ALLOWED_CALLBACK_OBJECT,
            0x4,
            0x11,
        ] {
            let mut security = protected(SYSTEM);
            security.dacl.as_mut().unwrap().push(Ace {
                ace_type,
                flags: 0,
                mask: FILE_GENERIC_READ,
                sid: String::new(),
            });
            assert_eq!(
                judge(&security, Role::Ancestor, &admins()),
                Err(AclRefusal::UnknownAce { ace_type })
            );
            // Unless it doesn't apply here.
            security.dacl.as_mut().unwrap().last_mut().unwrap().flags = IO;
            assert_eq!(judge(&security, Role::Ancestor, &admins()), Ok(()));
        }
        // A conditional allow ACE is judged like a plain one.
        let mut security = protected(SYSTEM);
        security.dacl.as_mut().unwrap().push(Ace {
            ace_type: ace_type::ALLOWED_CALLBACK,
            flags: 0,
            mask: FILE_WRITE_DATA,
            sid: USERS.into(),
        });
        assert!(judge(&security, Role::Object, &admins()).is_err());
    }

    #[test]
    fn generic_rights_map_to_file_rights() {
        assert_eq!(map_generic(GENERIC_ALL) & DELETE, DELETE);
        assert_eq!(
            map_generic(GENERIC_WRITE) & FILE_WRITE_DATA,
            FILE_WRITE_DATA
        );
        assert_eq!(
            map_generic(GENERIC_READ | GENERIC_EXECUTE) & OBJECT_FORBIDDEN,
            0
        );
        assert_eq!(map_generic(READ_EXECUTE), READ_EXECUTE);
    }

    /// The console seam trusts its own user, and only it.
    #[test]
    fn the_console_seam_trusts_its_user_too() {
        let trusted = Trusted::administrators_and(ALICE);
        assert_eq!(judge(&protected(ALICE), Role::Object, &trusted), Ok(()));
        let mut security = protected(ALICE);
        security
            .dacl
            .as_mut()
            .unwrap()
            .push(Ace::allow(0, FILE_ALL_ACCESS, ALICE));
        assert_eq!(judge(&security, Role::Object, &trusted), Ok(()));
        security
            .dacl
            .as_mut()
            .unwrap()
            .push(Ace::allow(0, FILE_WRITE_DATA, USERS));
        assert!(judge(&security, Role::Object, &trusted).is_err());
        assert_eq!(trusted.sids().len(), 4);
    }
}
