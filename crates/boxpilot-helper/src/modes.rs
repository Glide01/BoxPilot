//! Who may change what the macOS helper trusts (ADR 0006 rules 3 and 7):
//! the judgement over a file's owner, group and mode bits, as pure data, so
//! it is tested on every OS. The POSIX layer (`posix::verify`) reads the
//! real metadata with `lstat` / `fstat` and asks here. It is `acl`'s
//! counterpart for Windows.
//!
//! The helper runs only its own sing-box and keeps its state in its own
//! tree, so both must be beyond any non-administrator's reach, along the
//! whole path to them:
//!
//! - **no symbolic link** anywhere in the path, except the system's own
//!   (`/var`, `/tmp` and `/etc` point into `/private`), which are resolved
//!   first ([`explained_by_system_links`]) and the real path judged;
//! - **every directory** ([`Role::Dir`]: the helper's own directories and
//!   every one above them) owned by root, never writable by others, and
//!   writable by its group only when that group is wheel or admin:
//!   administrators are trusted, as on Windows. (Unlike a UAC-filtered
//!   Windows token, an admin account's processes hold the admin group
//!   without a prompt; the helper's own directories are root:wheel 0755
//!   anyway, so this matters only for a system directory above them.)
//! - **the state directory and each directory in it** ([`Role::Private`]):
//!   owned by root, and no permission at all for the group or others: it
//!   holds every account's `cache.db` and Tailscale node keys, and the
//!   helper's log;
//! - **each file the helper runs or reads** ([`Role::File`]: sing-box, the
//!   manifest, the helper itself, the owner record): a regular file owned
//!   by root, writable by neither its group nor others, whatever the group,
//!   and neither set-user-ID nor set-group-ID.

#![forbid(unsafe_code)]

use std::fmt;
use std::path::{Component, Path};

/// `S_IFMT` and the file types the helper tells apart (`sys/stat.h`; the
/// same values on macOS and Linux).
pub const S_IFMT: u32 = 0o170_000;
pub const S_IFDIR: u32 = 0o040_000;
pub const S_IFREG: u32 = 0o100_000;
pub const S_IFLNK: u32 = 0o120_000;

/// Group and others' write bits.
const GROUP_WRITE: u32 = 0o020;
const OTHERS_WRITE: u32 = 0o002;
/// Every permission bit of the group and others.
const GROUP_OR_OTHERS: u32 = 0o077;
/// Set-user-ID and set-group-ID.
const SET_ID: u32 = 0o6000;

/// macOS's `wheel` and `admin` groups.
pub const WHEEL_GID: u32 = 0;
pub const ADMIN_GID: u32 = 80;

/// The symbolic links macOS itself puts at the root, each to the same name
/// under `/private`: the only links a path the helper trusts may go through.
pub const SYSTEM_SYMLINKS: &[&str] = &["/var", "/tmp", "/etc"];

/// What `lstat` / `fstat` say about a file: its owner, its group, and its
/// `st_mode`, file type bits included.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Meta {
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
}

/// Who may own what the helper trusts, and which groups may write the
/// directories above it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trust {
    owners: Vec<u32>,
    groups: Vec<u32>,
}

impl Trust {
    /// The installed helper's: root owns everything; wheel and admin may
    /// write directories.
    pub fn root() -> Self {
        Self {
            owners: vec![0],
            groups: vec![WHEEL_GID, ADMIN_GID],
        }
    }

    /// Root's, and also the account `uid`'s files: for the tests, which
    /// build their trees unprivileged.
    pub fn root_and(uid: u32) -> Self {
        let mut trust = Self::root();
        if uid != 0 {
            trust.owners.push(uid);
        }
        trust
    }

    pub fn owners(&self) -> &[u32] {
        &self.owners
    }
}

/// What a path is to the helper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// A directory of the helper's or one above it.
    Dir,
    /// The state directory, or a directory in it.
    Private,
    /// A file the helper runs or reads.
    File,
}

/// Why a path was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModeRefusal {
    Symlink,
    NotADirectory,
    NotARegularFile,
    /// Owned by this uid, which isn't trusted.
    Owner(u32),
    WritableByOthers,
    /// A directory writable by its group, this gid, which is neither wheel
    /// nor admin.
    WritableByGroup(u32),
    /// A file writable by its group, whichever it is.
    FileWritableByGroup,
    /// A private directory with these permission bits for its group or
    /// others.
    NotPrivate(u32),
    SetId,
}

impl fmt::Display for ModeRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModeRefusal::Symlink => f.write_str("is a symbolic link"),
            ModeRefusal::NotADirectory => f.write_str("is not a directory"),
            ModeRefusal::NotARegularFile => f.write_str("is not a regular file"),
            ModeRefusal::Owner(uid) => write!(f, "is owned by uid {uid}, not root"),
            ModeRefusal::WritableByOthers => f.write_str("is writable by others"),
            ModeRefusal::WritableByGroup(gid) => write!(
                f,
                "is writable by its group (gid {gid}), which is neither wheel nor admin"
            ),
            ModeRefusal::FileWritableByGroup => f.write_str("is writable by its group"),
            ModeRefusal::NotPrivate(bits) => write!(
                f,
                "gives its group or others access (mode {bits:04o}); it must be root's alone"
            ),
            ModeRefusal::SetId => f.write_str("is set-user-ID or set-group-ID"),
        }
    }
}

impl std::error::Error for ModeRefusal {}

/// Judge one path's metadata in `role`.
pub fn judge(meta: &Meta, role: Role, trust: &Trust) -> Result<(), ModeRefusal> {
    let kind = meta.mode & S_IFMT;
    if kind == S_IFLNK {
        return Err(ModeRefusal::Symlink);
    }
    match role {
        Role::Dir | Role::Private if kind != S_IFDIR => return Err(ModeRefusal::NotADirectory),
        Role::File if kind != S_IFREG => return Err(ModeRefusal::NotARegularFile),
        _ => {}
    }
    if !trust.owners.contains(&meta.uid) {
        return Err(ModeRefusal::Owner(meta.uid));
    }
    match role {
        Role::Dir => {
            if meta.mode & OTHERS_WRITE != 0 {
                return Err(ModeRefusal::WritableByOthers);
            }
            if meta.mode & GROUP_WRITE != 0 && !trust.groups.contains(&meta.gid) {
                return Err(ModeRefusal::WritableByGroup(meta.gid));
            }
        }
        Role::Private => {
            let bits = meta.mode & GROUP_OR_OTHERS;
            if bits != 0 {
                return Err(ModeRefusal::NotPrivate(meta.mode & 0o7777));
            }
        }
        Role::File => {
            if meta.mode & OTHERS_WRITE != 0 {
                return Err(ModeRefusal::WritableByOthers);
            }
            if meta.mode & GROUP_WRITE != 0 {
                return Err(ModeRefusal::FileWritableByGroup);
            }
            if meta.mode & SET_ID != 0 {
                return Err(ModeRefusal::SetId);
            }
        }
    }
    Ok(())
}

/// Whether `real`, the real path of `path` (`realpath`), differs from it
/// only by the system's own links: equal, or `path` begins with one of
/// [`SYSTEM_SYMLINKS`] and `real` is the same path under `/private`. Both
/// must be absolute and plain (no `.` or `..`).
pub fn explained_by_system_links(path: &Path, real: &Path) -> bool {
    // `has_root`, which on Unix is `is_absolute`: the tests run on Windows
    // too, where a path without a drive isn't absolute.
    let plain = |p: &Path| {
        p.has_root()
            && p.components()
                .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
    };
    if !plain(path) || !plain(real) {
        return false;
    }
    if path == real {
        return true;
    }
    SYSTEM_SYMLINKS.iter().any(|link| {
        path.starts_with(link)
            && Path::new("/private").join(path.strip_prefix("/").unwrap_or(path)) == real
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: u32 = 501;

    fn meta(uid: u32, gid: u32, mode: u32) -> Meta {
        Meta { uid, gid, mode }
    }

    fn dir(uid: u32, gid: u32, perm: u32) -> Meta {
        meta(uid, gid, S_IFDIR | perm)
    }

    fn file(uid: u32, gid: u32, perm: u32) -> Meta {
        meta(uid, gid, S_IFREG | perm)
    }

    fn root() -> Trust {
        Trust::root()
    }

    /// The modes of the macOS system directories the helper's paths go
    /// through, and of what the install creates.
    #[test]
    fn the_installed_tree_passes() {
        for (what, meta) in [
            ("/", dir(0, WHEEL_GID, 0o755)),
            ("/Library", dir(0, WHEEL_GID, 0o755)),
            ("/Library/Application Support", dir(0, ADMIN_GID, 0o755)),
            (
                "an admin-writable system directory",
                dir(0, ADMIN_GID, 0o775),
            ),
            ("/Library/PrivilegedHelperTools", dir(0, WHEEL_GID, 0o1755)),
            ("BoxPilot Helper", dir(0, WHEEL_GID, 0o755)),
            ("bin", dir(0, WHEEL_GID, 0o755)),
        ] {
            assert_eq!(judge(&meta, Role::Dir, &root()), Ok(()), "{what}");
        }
        assert_eq!(
            judge(&dir(0, WHEEL_GID, 0o700), Role::Private, &root()),
            Ok(())
        );
        for perm in [0o755, 0o644, 0o600, 0o500] {
            assert_eq!(
                judge(&file(0, WHEEL_GID, perm), Role::File, &root()),
                Ok(())
            );
        }
    }

    #[test]
    fn links_and_the_wrong_kind_are_refused() {
        let link = meta(0, 0, S_IFLNK | 0o755);
        for role in [Role::Dir, Role::Private, Role::File] {
            assert_eq!(judge(&link, role, &root()), Err(ModeRefusal::Symlink));
        }
        assert_eq!(
            judge(&file(0, 0, 0o755), Role::Dir, &root()),
            Err(ModeRefusal::NotADirectory)
        );
        assert_eq!(
            judge(&file(0, 0, 0o700), Role::Private, &root()),
            Err(ModeRefusal::NotADirectory)
        );
        assert_eq!(
            judge(&dir(0, 0, 0o755), Role::File, &root()),
            Err(ModeRefusal::NotARegularFile)
        );
        for kind in [0o010_000, 0o020_000, 0o060_000, 0o140_000] {
            assert_eq!(
                judge(&meta(0, 0, kind | 0o644), Role::File, &root()),
                Err(ModeRefusal::NotARegularFile),
                "{kind:o}"
            );
        }
    }

    #[test]
    fn only_root_may_own_anything() {
        for role in [Role::Dir, Role::Private, Role::File] {
            let meta = if role == Role::File {
                file(ME, 20, 0o600)
            } else {
                dir(ME, 20, 0o700)
            };
            assert_eq!(judge(&meta, role, &root()), Err(ModeRefusal::Owner(ME)));
            assert_eq!(judge(&meta, role, &Trust::root_and(ME)), Ok(()));
        }
        assert_eq!(Trust::root_and(0), Trust::root());
    }

    #[test]
    fn directories_are_never_writable_by_others() {
        for perm in [0o757, 0o777, 0o1777, 0o702] {
            assert_eq!(
                judge(&dir(0, WHEEL_GID, perm), Role::Dir, &root()),
                Err(ModeRefusal::WritableByOthers),
                "{perm:o}"
            );
        }
    }

    /// staff (20), everyone (12), or any other group: an ordinary account
    /// may be in it.
    #[test]
    fn directories_are_group_writable_only_by_wheel_or_admin() {
        for gid in [WHEEL_GID, ADMIN_GID] {
            assert_eq!(judge(&dir(0, gid, 0o775), Role::Dir, &root()), Ok(()));
        }
        for gid in [20, 12, 1, 501] {
            assert_eq!(
                judge(&dir(0, gid, 0o775), Role::Dir, &root()),
                Err(ModeRefusal::WritableByGroup(gid))
            );
            assert_eq!(judge(&dir(0, gid, 0o755), Role::Dir, &root()), Ok(()));
        }
    }

    #[test]
    fn private_directories_give_nothing_to_anyone_else() {
        for perm in [0o701, 0o704, 0o710, 0o740, 0o750, 0o755, 0o770, 0o777] {
            assert_eq!(
                judge(&dir(0, WHEEL_GID, perm), Role::Private, &root()),
                Err(ModeRefusal::NotPrivate(perm)),
                "{perm:o}"
            );
        }
        assert_eq!(
            judge(&dir(0, ADMIN_GID, 0o700), Role::Private, &root()),
            Ok(())
        );
    }

    #[test]
    fn files_are_writable_by_root_alone_and_never_set_id() {
        for gid in [WHEEL_GID, ADMIN_GID, 20] {
            assert_eq!(
                judge(&file(0, gid, 0o775), Role::File, &root()),
                Err(ModeRefusal::FileWritableByGroup),
                "{gid}"
            );
        }
        assert_eq!(
            judge(&file(0, WHEEL_GID, 0o757), Role::File, &root()),
            Err(ModeRefusal::WritableByOthers)
        );
        assert_eq!(
            judge(&file(0, WHEEL_GID, 0o666), Role::File, &root()),
            Err(ModeRefusal::WritableByOthers)
        );
        for perm in [0o4755, 0o2755, 0o6755] {
            assert_eq!(
                judge(&file(0, WHEEL_GID, perm), Role::File, &root()),
                Err(ModeRefusal::SetId),
                "{perm:o}"
            );
        }
    }

    #[test]
    fn only_the_systems_links_are_resolved() {
        let p = Path::new;
        assert!(explained_by_system_links(
            p("/Library/Application Support/BoxPilot Helper/bin"),
            p("/Library/Application Support/BoxPilot Helper/bin")
        ));
        assert!(explained_by_system_links(
            p("/var/run/x.sock"),
            p("/private/var/run/x.sock")
        ));
        assert!(explained_by_system_links(p("/tmp/a"), p("/private/tmp/a")));
        assert!(explained_by_system_links(p("/var"), p("/private/var")));
        for (path, real) in [
            // A link of anyone else's, anywhere.
            (
                "/Library/Application Support/BoxPilot Helper/bin",
                "/Users/eve/bin",
            ),
            ("/Library/x", "/private/Library/x"),
            // Into /private, but not the same path.
            ("/var/run/x", "/private/var/folders/x"),
            ("/var/run/x", "/private/tmp/x"),
            ("/variable/x", "/private/variable/x"),
            // Not plain.
            ("/var/../Users/x", "/Users/x"),
            ("var/x", "/private/var/x"),
        ] {
            assert!(
                !explained_by_system_links(p(path), p(real)),
                "{path} -> {real}"
            );
        }
    }
}
