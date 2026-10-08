//! Where the helper keeps what it owns. Two trees, both verified admin-only
//! before the helper trusts anything in them:
//!
//! - **the helper directory** (`%ProgramFiles%\BoxPilot\Helper`, the helper
//!   executable's own directory): the helper, its sing-box, the files
//!   beside it and the install manifest. Fixed at install, never chosen by a
//!   user (ADR 0006 rule 7);
//! - **the state directory** (`%ProgramData%\BoxPilot\Helper`):
//!   - `runs\<random>`: one private directory per start, removed when that
//!     sing-box has exited;
//!   - `users\<SID>`: each caller's lasting state, its `cache.db` and its
//!     Tailscale logins, so they survive reconnects without one account
//!     seeing another's;
//!   - `helper.log`: the helper's own log.

#![forbid(unsafe_code)]

use crate::manifest::MANIFEST_FILE;
use boxpilot_policy::Placement;
use std::fmt;
use std::path::{Path, PathBuf, MAIN_SEPARATOR};

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

    /// The state directory of the account `sid`, or `None` when `sid` isn't
    /// a SID string ([`is_sid`]): a name from the OS, but checked anyway
    /// before it becomes a path.
    pub fn user_dir(&self, sid: &str) -> Option<PathBuf> {
        is_sid(sid).then(|| self.users_dir().join(sid))
    }

    pub fn log_file(&self) -> PathBuf {
        self.state_dir.join(LOG_FILE)
    }
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
