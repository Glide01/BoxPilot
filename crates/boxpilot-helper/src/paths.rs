//! Where the helper keeps what it owns. Two trees, side by side in
//! `%ProgramFiles%\BoxPilot` ([`Layout::installed`]), both verified
//! admin-only before the helper trusts anything in them:
//!
//! - **the helper directory** (`%ProgramFiles%\BoxPilot\Helper`, the helper
//!   executable's own directory): the helper, its sing-box, the files
//!   beside it and the install manifest. Fixed at install, never chosen by a
//!   user (ADR 0006 rule 7). Users may read it: these are the binaries;
//! - **the state directory** (`%ProgramFiles%\BoxPilot\HelperState`),
//!   private to SYSTEM and Administrators ([`STATE_DIR_DACL`]: nobody else
//!   may even read it):
//!   - `runs\<random>`: one private directory per start, removed when that
//!     sing-box has exited;
//!   - `users\<SID>`: each caller's lasting state, its `cache.db` and its
//!     Tailscale logins (node keys included), so they survive reconnects
//!     without one account seeing another's;
//!   - `helper.log`: the helper's own log.
//!
//! Why Program Files and not ProgramData: every user may create entries
//! in `C:\ProgramData`, so a user could create `ProgramData\BoxPilot`
//! before the install, as a folder (the helper would then refuse to run)
//! or as a junction (the installer, as SYSTEM, would then apply the
//! state directory's descriptor to wherever it points). Users can create
//! nothing under `C:\Program Files`. The state directory is the helper
//! directory's sibling, never inside it, so nothing written as state can
//! land among the binaries.
//!
//! **macOS** ([`Layout::installed_macos`], `endpoint::macos`) has the same
//! two trees, side by side in `/Library/Application Support/BoxPilot
//! Helper`, root:wheel and verified root-only before use: `bin` (sing-box
//! and the manifest; the helper itself is in `/Library/PrivilegedHelperTools`)
//! and `state` (0700), which also holds the [`OWNER_FILE`] the install
//! wrote and the [`RUN_MARKER`]. Accounts are named by their uid there
//! (`users/501`) rather than by a SID.

#![forbid(unsafe_code)]

use crate::manifest::MANIFEST_FILE;
use boxpilot_policy::Placement;
use boxpilot_protocol::endpoint::macos;
use std::fmt;
use std::path::{Path, PathBuf, MAIN_SEPARATOR};

/// The folder under `%ProgramFiles%` that holds both trees.
pub const PRODUCT_DIR: &str = "BoxPilot";
/// The helper directory, in [`PRODUCT_DIR`].
pub const HELPER_DIR: &str = "Helper";
/// The state directory, in [`PRODUCT_DIR`] beside [`HELPER_DIR`].
pub const STATE_DIR: &str = "HelperState";

/// The DACL of the state directory and of every directory the helper
/// creates in it, as SDDL: protected (nothing inherited, so not Program
/// Files' "Users: Read & execute"), full control for SYSTEM and
/// Administrators, nothing for anyone else. The MSI creates the state
/// directory with this DACL, owned by SYSTEM.
pub const STATE_DIR_DACL: &str = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";

/// The per-run directories, under the state directory.
pub const RUNS_DIR: &str = "runs";
/// The per-caller state, under the state directory.
pub const USERS_DIR: &str = "users";
/// The helper's own log, in the state directory.
pub const LOG_FILE: &str = "helper.log";
/// sing-box's cache file, in a caller's state directory.
pub const CACHE_FILE: &str = "cache.db";
/// The Tailscale endpoints' state, in a caller's state directory.
pub const TAILSCALE_DIR: &str = "tailscale";
/// macOS: the owner record the install writes in the state directory
/// (`endpoint::macos::OWNER_FILE`).
pub const OWNER_FILE: &str = "owner";
/// macOS: written in the state directory while a sing-box runs, saying
/// what it may leave behind if it can't undo it itself (`cleanup`), and
/// removed once it has exited and been cleaned up after. Found when the
/// helper starts, it means the helper died with a run.
pub const RUN_MARKER: &str = "running";

/// The longest SID string Windows writes: `S-1-`, a 48-bit authority (15
/// digits) and 15 sub-authorities of up to 10 digits, with their dashes.
const MAX_SID_LEN: usize = 184;

/// The helper's two trees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    helper_dir: PathBuf,
    state_dir: PathBuf,
}

impl Layout {
    pub fn new(helper_dir: PathBuf, state_dir: PathBuf) -> Self {
        Self {
            helper_dir,
            state_dir,
        }
    }

    /// The installed service's trees, under `program_files` (the 64-bit
    /// `%ProgramFiles%`, as Windows reports it): `BoxPilot\Helper` and,
    /// beside it, `BoxPilot\HelperState`.
    pub fn installed(program_files: &Path) -> Self {
        let product = program_files.join(PRODUCT_DIR);
        Self::new(product.join(HELPER_DIR), product.join(STATE_DIR))
    }

    /// The macOS install's trees: `endpoint::macos::BIN_DIR` and, beside
    /// it, `endpoint::macos::STATE_DIR`.
    pub fn installed_macos() -> Self {
        Self::new(
            PathBuf::from(macos::BIN_DIR),
            PathBuf::from(macos::STATE_DIR),
        )
    }

    pub fn helper_dir(&self) -> &Path {
        &self.helper_dir
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// `manifest.json`, beside the helper.
    pub fn manifest_file(&self) -> PathBuf {
        self.helper_dir.join(MANIFEST_FILE)
    }

    /// A file the manifest names, beside the helper. The manifest's names
    /// are plain file names (`manifest::is_plain_file_name`), so this never
    /// leaves the helper directory.
    pub fn helper_file(&self, name: &str) -> PathBuf {
        self.helper_dir.join(name)
    }

    pub fn runs_dir(&self) -> PathBuf {
        self.state_dir.join(RUNS_DIR)
    }

    /// The run directory called `name` (from [`run_name`]).
    pub fn run_dir(&self, name: &str) -> PathBuf {
        self.runs_dir().join(name)
    }

    pub fn users_dir(&self) -> PathBuf {
        self.state_dir.join(USERS_DIR)
    }

    /// The state directory of `account`, or `None` when `account` is neither
    /// a SID string ([`is_sid`], Windows) nor a uid ([`is_uid`], macOS): a
    /// name from the OS, but checked anyway before it becomes a path.
    pub fn user_dir(&self, account: &str) -> Option<PathBuf> {
        (is_sid(account) || is_uid(account)).then(|| self.users_dir().join(account))
    }

    pub fn log_file(&self) -> PathBuf {
        self.state_dir.join(LOG_FILE)
    }

    /// macOS: the owner record, in the state directory.
    pub fn owner_file(&self) -> PathBuf {
        self.state_dir.join(OWNER_FILE)
    }

    /// macOS: the marker of a running sing-box, in the state directory.
    pub fn run_marker(&self) -> PathBuf {
        self.state_dir.join(RUN_MARKER)
    }
}

/// Whether `text` is a uid as the helper writes one: decimal, 1 to 10
/// digits, no leading zero (but `0` itself), and below `u32::MAX`, which is
/// `(uid_t)-1`, no account. Only ASCII digits, so it is a safe directory
/// name.
pub fn is_uid(text: &str) -> bool {
    (1..=10).contains(&text.len())
        && text.bytes().all(|b| b.is_ascii_digit())
        && (text == "0" || !text.starts_with('0'))
        && text.parse::<u32>().is_ok_and(|uid| uid != u32::MAX)
}

/// Whether `text` is a SID string as Windows writes it: `S-1-`, a decimal
/// identifier authority, then 1 to 15 decimal sub-authorities, all
/// separated by single dashes. Only ASCII digits, `S` and `-`, so it is a
/// safe directory name.
pub fn is_sid(text: &str) -> bool {
    let Some(rest) = text.strip_prefix("S-1-") else {
        return false;
    };
    let parts: Vec<&str> = rest.split('-').collect();
    text.len() <= MAX_SID_LEN
        && (2..=16).contains(&parts.len())
        && parts
            .iter()
            .all(|part| (1..=15).contains(&part.len()) && part.bytes().all(|b| b.is_ascii_digit()))
}

/// A run directory's name: 16 random bytes in hex, so no two runs share one
/// and nobody can predict the next.
pub fn run_name(random: &[u8; 16]) -> String {
    random.iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether `name` is one [`run_name`] makes: 32 lowercase hex digits.
pub fn is_run_name(name: &str) -> bool {
    name.len() == 32
        && name
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A path the policy's `Placement` can't carry: not valid Unicode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotUnicode;

impl fmt::Display for NotUnicode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a helper path is not valid Unicode")
    }
}

impl std::error::Error for NotUnicode {}

/// Where one start's helper-owned fields point: the run directory for its
/// attachments, and the caller's own state directory for the cache file and
/// the Tailscale state.
pub fn placement(run_dir: &Path, user_dir: &Path) -> Result<Placement, NotUnicode> {
    let text = |path: &Path| path.to_str().map(str::to_owned).ok_or(NotUnicode);
    Ok(Placement {
        run_dir: text(run_dir)?,
        cache_file: text(&user_dir.join(CACHE_FILE))?,
        tailscale_dir: text(&user_dir.join(TAILSCALE_DIR))?,
        separator: MAIN_SEPARATOR,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sids_are_checked_before_they_become_paths() {
        for sid in [
            "S-1-5-18",
            "S-1-5-32-544",
            "S-1-5-21-3623811015-3361044348-30300820-1013",
        ] {
            assert!(is_sid(sid), "{sid}");
        }
        for not_sid in [
            "",
            "S-1-5",
            "S-1-",
            "s-1-5-18",
            "S-2-5-18",
            "S-1-5--18",
            "S-1-5-18-",
            "S-1-0x5-18",
            "S-1-5-18\\..",
            "S-1-5-18/x",
            "S-1-5-١٨",
            "S-1-5-1234567890123456",
            "S-1-5-1-2-3-4-5-6-7-8-9-10-11-12-13-14-15-16",
        ] {
            assert!(!is_sid(not_sid), "{not_sid}");
        }
        assert!(is_sid("S-1-5-1-2-3-4-5-6-7-8-9-10-11-12-13-14-15"));
    }

    /// Side by side under `Program Files\BoxPilot`: neither tree is in the
    /// other, so state never lands among the binaries.
    #[test]
    fn the_installed_trees_are_siblings_in_program_files() {
        let program_files = Path::new("/Program Files");
        let layout = Layout::installed(program_files);
        let product = program_files.join("BoxPilot");
        assert_eq!(layout.helper_dir(), product.join("Helper"));
        assert_eq!(layout.state_dir(), product.join("HelperState"));
        assert_eq!(layout.helper_dir().parent(), layout.state_dir().parent());
        assert!(!layout.state_dir().starts_with(layout.helper_dir()));
        assert!(!layout.helper_dir().starts_with(layout.state_dir()));
        for place in [
            layout.runs_dir(),
            layout.users_dir(),
            layout.log_file(),
            layout.user_dir("S-1-5-21-1-2-3-1001").unwrap(),
        ] {
            assert!(place.starts_with(layout.state_dir()), "{place:?}");
            assert!(!place.starts_with(layout.helper_dir()), "{place:?}");
        }
    }

    #[test]
    fn uids_are_checked_before_they_become_paths() {
        for uid in ["0", "501", "4294967294"] {
            assert!(is_uid(uid), "{uid}");
        }
        for not_uid in [
            "",
            "0501",
            "00",
            "+501",
            "-1",
            "501 ",
            "5_01",
            "4294967295",
            "4294967296",
            "99999999999",
            "../501",
            "٥٠١",
        ] {
            assert!(!is_uid(not_uid), "{not_uid}");
        }
        let layout = Layout::new(PathBuf::from("/h"), PathBuf::from("/s"));
        assert_eq!(layout.user_dir("501"), Some(PathBuf::from("/s/users/501")));
        assert_eq!(layout.user_dir("0501"), None);
    }

    /// Side by side under `/Library/Application Support/BoxPilot Helper`,
    /// as `endpoint::macos` names them for the scripts and the GUI.
    #[test]
    fn the_macos_trees_are_the_endpoints() {
        let layout = Layout::installed_macos();
        assert_eq!(layout.helper_dir(), Path::new(macos::BIN_DIR));
        assert_eq!(layout.state_dir(), Path::new(macos::STATE_DIR));
        assert_eq!(layout.manifest_file(), Path::new(macos::MANIFEST_PATH));
        assert_eq!(
            layout.helper_file("sing-box"),
            Path::new(macos::SING_BOX_PATH)
        );
        assert_eq!(layout.owner_file(), Path::new(macos::OWNER_FILE));
        assert_eq!(layout.log_file(), Path::new(macos::LOG_FILE));
        assert_eq!(layout.helper_dir().parent(), layout.state_dir().parent());
        assert_eq!(
            layout.state_dir().parent(),
            Some(Path::new(macos::SUPPORT_DIR))
        );
        assert!(!layout.state_dir().starts_with(layout.helper_dir()));
        assert!(layout.run_marker().starts_with(layout.state_dir()));
    }

    #[test]
    fn the_layout_names_every_place() {
        let layout = Layout::new(PathBuf::from("/h"), PathBuf::from("/s"));
        assert_eq!(layout.manifest_file(), Path::new("/h/manifest.json"));
        assert_eq!(
            layout.helper_file("sing-box.exe"),
            Path::new("/h/sing-box.exe")
        );
        assert_eq!(layout.run_dir("ab"), Path::new("/s/runs/ab"));
        assert_eq!(
            layout.user_dir("S-1-5-21-1-2-3-1001"),
            Some(PathBuf::from("/s/users/S-1-5-21-1-2-3-1001"))
        );
        assert_eq!(layout.user_dir("../x"), None);
        assert_eq!(layout.log_file(), Path::new("/s/helper.log"));
    }

    #[test]
    fn run_names_are_hex() {
        let mut bytes = [0u8; 16];
        bytes[0] = 0xab;
        bytes[15] = 0x01;
        assert_eq!(run_name(&bytes), "ab000000000000000000000000000001");
        assert!(is_run_name(&run_name(&bytes)));
        assert!(is_run_name(&run_name(&[0xff; 16])));
        for not_run in [
            "",
            "ab",
            "AB000000000000000000000000000001",
            "ab00000000000000000000000000000g",
            "ab0000000000000000000000000000001",
            "../00000000000000000000000000000",
        ] {
            assert!(!is_run_name(not_run), "{not_run}");
        }
    }

    #[test]
    fn the_placement_points_into_the_run_and_the_callers_state() {
        let run = Path::new("/s/runs/ab");
        let user = Path::new("/s/users/S-1-5-18");
        let placement = placement(run, user).unwrap();
        let sep = MAIN_SEPARATOR;
        assert_eq!(placement.run_dir, "/s/runs/ab");
        assert_eq!(
            placement.cache_file,
            format!("/s/users/S-1-5-18{sep}cache.db")
        );
        assert_eq!(
            placement.tailscale_dir,
            format!("/s/users/S-1-5-18{sep}tailscale")
        );
        assert_eq!(placement.separator, sep);
    }

    #[cfg(unix)]
    #[test]
    fn a_path_that_isnt_unicode_is_refused() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let bad = Path::new(OsStr::from_bytes(b"/s/\xff"));
        assert_eq!(placement(bad, Path::new("/u")), Err(NotUnicode));
        assert_eq!(placement(Path::new("/r"), bad), Err(NotUnicode));
    }
}
