//! Crash-safe file replacement for everything BoxPilot persists: settings,
//! profile configs and the runtime config. A plain `fs::write` truncates the
//! target first, so a crash, power loss or full disk mid-write leaves it
//! empty or half-written; here the new content goes to a temp file next to
//! the target and is renamed over it only once it is fully on disk. A reader
//! sees either the old file or the new one, never a mix.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Who may read the written file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileAccess {
    /// A new file gets the default mode (umask applies); on Unix an existing
    /// file keeps its current mode across the replace. On Windows the file
    /// inherits the folder's ACL either way.
    Inherit,
    /// Owner-only (0600) on Unix, for files that carry a secret. The temp
    /// file is created with that mode, so the content is never readable by
    /// anyone else, not even briefly. On Windows it inherits the per-user
    /// data folder's ACL, same as `Inherit`.
    OwnerOnly,
}

/// A suffix no other call in this process (or a concurrent BoxPilot, by pid)
/// gets: `<pid>-<n>`. For temp files that concurrent writers must not share.
pub fn unique_suffix() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{}", std::process::id(), n)
}

/// Replace `path` with `contents`: write a unique temp file in the same
/// directory (so the rename stays on one filesystem), `sync_all` it, then
/// rename it over the target. `std::fs::rename` replaces an existing file on
/// Windows too (`MOVEFILE_REPLACE_EXISTING`). On any error the temp file is
/// removed and the target is left exactly as it was.
pub fn write_atomic(path: &Path, contents: &[u8], access: FileAccess) -> io::Result<()> {
    write_atomic_with(path, access, |file| file.write_all(contents))
}

/// The first half of `write_atomic`: `contents` is fully written and synced
/// to a temp file next to `path`, but not yet in place. Lets a background job
/// do the slow write while the UI thread decides whether it should land at
/// all (a profile fetch whose profile was deleted meanwhile must not).
pub fn stage_atomic(path: &Path, contents: &[u8], access: FileAccess) -> io::Result<StagedFile> {
    stage_with(path, access, |file| file.write_all(contents))
}

/// New content waiting next to its target; see `stage_atomic`. `commit`
/// renames it over the target. Dropped uncommitted (discarded, or its task
/// cancelled), it removes the temp file, so the target is never touched.
#[derive(Debug)]
pub struct StagedFile {
    /// `None` once committed.
    tmp: Option<PathBuf>,
    target: PathBuf,
}

impl StagedFile {
    /// The file this replaces once committed.
    pub fn target(&self) -> &Path {
        &self.target
    }

    /// Rename the staged content over the target. On error the temp file is
    /// removed and the target is left as it was.
    pub fn commit(mut self) -> io::Result<()> {
        let Some(tmp) = self.tmp.take() else {
            return Ok(());
        };
        let result = rename_into_place(&tmp, &self.target);
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        if let Some(tmp) = self.tmp.take() {
            let _ = fs::remove_file(tmp);
        }
    }
}

/// `write_atomic` with the content step supplied by the caller; tests use it
/// to fail halfway through.
fn write_atomic_with(
    path: &Path,
    access: FileAccess,
    write: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    stage_with(path, access, write)?.commit()
}

fn stage_with(
    path: &Path,
    access: FileAccess,
    write: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<StagedFile> {
    let tmp = temp_path_for(path)?;
    // Built first, so an error below drops it and removes the temp file.
    let staged = StagedFile {
        tmp: Some(tmp.clone()),
        target: path.to_path_buf(),
    };
    write_temp(path, &tmp, access, write)?;
    Ok(staged)
}

fn write_temp(
    path: &Path,
    tmp: &Path,
    access: FileAccess,
    write: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if access == FileAccess::OwnerOnly {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(tmp)?;
    #[cfg(unix)]
    if access == FileAccess::Inherit {
        // Set explicitly rather than through `mode()`, which the umask
        // would narrow.
        if let Ok(meta) = fs::metadata(path) {
            file.set_permissions(meta.permissions())?;
        }
    }
    #[cfg(not(unix))]
    let _ = (path, access);
    write(&mut file)?;
    file.sync_all()?;
    Ok(())
}

fn rename_into_place(tmp: &Path, path: &Path) -> io::Result<()> {
    fs::rename(tmp, path)?;
    // Persist the rename itself (the directory entry). Best effort: some
    // filesystems refuse to sync a directory, and the content is already safe.
    #[cfg(unix)]
    if let Some(dir) = path.parent() {
        let dir = if dir.as_os_str().is_empty() {
            Path::new(".")
        } else {
            dir
        };
        if let Ok(dir) = File::open(dir) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

/// `.<file name>.<unique suffix>.tmp` next to `path`.
fn temp_path_for(path: &Path) -> io::Result<PathBuf> {
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("not a file path: {}", path.display()),
        )
    })?;
    let tmp_name = format!(".{}.{}.tmp", name.to_string_lossy(), unique_suffix());
    Ok(path.with_file_name(tmp_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("box_pilot_atomic_{}_{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn creates_and_replaces_without_leaving_temp_files() {
        let dir = temp_dir("replace");
        let path = dir.join("settings.json");
        write_atomic(&path, b"first", FileAccess::Inherit).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "first");
        write_atomic(&path, b"second", FileAccess::Inherit).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "second");
        assert_eq!(entries(&dir), vec!["settings.json"]);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A write that fails halfway (disk full, I/O error) must leave the old
    /// file untouched and clean up its temp file.
    #[test]
    fn failed_write_leaves_target_intact() {
        let dir = temp_dir("fail");
        let path = dir.join("p1.json");
        fs::write(&path, "good config").unwrap();
        let err = write_atomic_with(&path, FileAccess::Inherit, |file| {
            file.write_all(b"half a con")?;
            Err(io::Error::other("disk full"))
        })
        .unwrap_err();
        assert_eq!(err.to_string(), "disk full");
        assert_eq!(fs::read_to_string(&path).unwrap(), "good config");
        assert_eq!(entries(&dir), vec!["p1.json"]);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A staged file is invisible until committed, and discarding it (a
    /// fetch for a deleted profile) leaves the old target and no temp file.
    #[test]
    fn staged_file_lands_only_on_commit() {
        let dir = temp_dir("staged");
        let path = dir.join("p1.json");
        fs::write(&path, "old").unwrap();

        let discarded = stage_atomic(&path, b"discarded", FileAccess::Inherit).unwrap();
        assert_eq!(discarded.target(), path.as_path());
        assert_eq!(entries(&dir).len(), 2, "temp file beside the target");
        drop(discarded);
        assert_eq!(fs::read_to_string(&path).unwrap(), "old");
        assert_eq!(entries(&dir), vec!["p1.json"]);

        let staged = stage_atomic(&path, b"new", FileAccess::Inherit).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "old");
        staged.commit().unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(entries(&dir), vec!["p1.json"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn temp_paths_are_unique_and_beside_the_target() {
        let path = Path::new("/data/BoxPilot/configs/p1.json");
        let a = temp_path_for(path).unwrap();
        let b = temp_path_for(path).unwrap();
        assert_ne!(a, b);
        assert_eq!(a.parent(), path.parent());
        assert!(a
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(".p1.json."));
    }

    #[cfg(unix)]
    #[test]
    fn owner_only_is_0600_and_inherit_keeps_the_existing_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("mode");
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;

        let secret = dir.join("running_config.json");
        fs::write(&secret, "old").unwrap();
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o644)).unwrap();
        write_atomic(&secret, b"new", FileAccess::OwnerOnly).unwrap();
        assert_eq!(mode(&secret), 0o600);

        let kept = dir.join("box_pilot_settings.json");
        fs::write(&kept, "old").unwrap();
        fs::set_permissions(&kept, fs::Permissions::from_mode(0o640)).unwrap();
        write_atomic(&kept, b"new", FileAccess::Inherit).unwrap();
        assert_eq!(fs::read_to_string(&kept).unwrap(), "new");
        assert_eq!(mode(&kept), 0o640);
        let _ = fs::remove_dir_all(&dir);
    }
}
