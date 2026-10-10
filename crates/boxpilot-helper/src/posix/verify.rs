//! Verifying the macOS helper's trees before it trusts anything in them
//! (ADR 0006 rules 3 and 7): every directory from `/` down and each file
//! the helper runs or reads, judged by `modes::judge`. Done when the helper
//! starts and again before every spawn; `win::verify`'s counterpart.
//!
//! A path is first resolved with `realpath`, and must differ from its real
//! path only by the system's own links (`modes::explained_by_system_links`);
//! then every component of the real path is read with `lstat`, so a link
//! is seen as a link and refused, never followed. A file is opened with
//! `O_NOFOLLOW` and judged by `fstat` on what was opened, which is then what
//! the helper reads or hashes.
//!
//! Between the hash and `posix_spawn` sing-box could still be replaced (no
//! `fexecve` on macOS, and nothing like Windows' share modes), but only by
//! root or an administrator: everything the chain check passed is theirs
//! alone.

#![forbid(unsafe_code)]

use crate::modes::{self, Meta, ModeRefusal, Role, Trust};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};

/// Why a path was refused: where, and what was wrong.
#[derive(Debug)]
pub struct Refused {
    pub path: PathBuf,
    pub why: Why,
}

#[derive(Debug)]
pub enum Why {
    Mode(ModeRefusal),
    Io(io::Error),
    NotAbsolute,
    /// The path goes through a symbolic link other than the system's own.
    Link(PathBuf),
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = self.path.display();
        match &self.why {
            Why::Mode(refusal) => write!(f, "{path} {refusal}"),
            Why::Io(error) => write!(f, "{path} could not be checked: {error}"),
            Why::NotAbsolute => write!(f, "{path} is not a plain absolute path"),
            Why::Link(real) => write!(
                f,
                "{path} goes through a symbolic link (its real path is {})",
                real.display()
            ),
        }
    }
}

impl std::error::Error for Refused {}

fn refused(path: &Path, why: Why) -> Refused {
    Refused {
        path: path.to_owned(),
        why,
    }
}

fn meta_of(metadata: &fs::Metadata) -> Meta {
    Meta {
        uid: metadata.uid(),
        gid: metadata.gid(),
        mode: metadata.mode(),
    }
}

fn judge_path(path: &Path, role: Role, trust: &Trust) -> Result<(), Refused> {
    let metadata = fs::symlink_metadata(path).map_err(|error| refused(path, Why::Io(error)))?;
    modes::judge(&meta_of(&metadata), role, trust).map_err(|why| refused(path, Why::Mode(why)))
}

fn is_plain_absolute(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|part| matches!(part, Component::RootDir | Component::Normal(_)))
}

/// `path`'s real path, if it differs from `path` only by the system's own
/// links.
fn real_path(path: &Path) -> Result<PathBuf, Refused> {
    if !is_plain_absolute(path) {
        return Err(refused(path, Why::NotAbsolute));
    }
    let real = fs::canonicalize(path).map_err(|error| refused(path, Why::Io(error)))?;
    if !modes::explained_by_system_links(path, &real) {
        return Err(refused(path, Why::Link(real)));
    }
    Ok(real)
}

/// Verify directory `dir` in `role` and every directory above it, up to
/// `/`, as [`Role::Dir`]. Returns its real path.
pub fn dir_chain(dir: &Path, role: Role, trust: &Trust) -> Result<PathBuf, Refused> {
    let real = real_path(dir)?;
    for (depth, path) in real.ancestors().enumerate() {
        let role = if depth == 0 { role } else { Role::Dir };
        judge_path(path, role, trust)?;
    }
    Ok(real)
}

/// Verify directory `dir` alone, in `role`: for a directory the helper
/// created inside a tree whose chain it has just verified.
pub fn dir_only(dir: &Path, role: Role, trust: &Trust) -> Result<(), Refused> {
    judge_path(dir, role, trust)
}

/// Open `path`, a file in a directory whose chain was just verified, for
/// reading, without following a link, and verify what was opened as a
/// [`Role::File`]. The handle is how the helper then reads or hashes it.
pub fn open_file(path: &Path, trust: &Trust) -> Result<File, Refused> {
    if !is_plain_absolute(path) {
        return Err(refused(path, Why::NotAbsolute));
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| {
            // O_NOFOLLOW on a link fails with ELOOP: say what it is.
            if error.raw_os_error() == Some(libc::ELOOP) {
                refused(path, Why::Mode(ModeRefusal::Symlink))
            } else {
                refused(path, Why::Io(error))
            }
        })?;
    let metadata = file
        .metadata()
        .map_err(|error| refused(path, Why::Io(error)))?;
    modes::judge(&meta_of(&metadata), Role::File, trust)
        .map_err(|why| refused(path, Why::Mode(why)))?;
    Ok(file)
}

/// Verify the file at `path` and the chain of directories it is in.
pub fn file_chain(path: &Path, trust: &Trust) -> Result<File, Refused> {
    let parent = path
        .parent()
        .ok_or_else(|| refused(path, Why::NotAbsolute))?;
    let real_parent = dir_chain(parent, Role::Dir, trust)?;
    let name = path
        .file_name()
        .ok_or_else(|| refused(path, Why::NotAbsolute))?;
    open_file(&real_parent.join(name), trust)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;
    use std::os::unix::fs::{symlink, PermissionsExt};

    /// This process's uid: the owner of what it just created.
    fn me() -> u32 {
        let temp = TempDir::new("verify-me");
        fs::metadata(&temp.0).unwrap().uid()
    }

    /// A tree under the build directory, whose own chain this machine may
    /// not pass (a group-writable home, say): then the test says so and
    /// checks nothing, rather than fail on the machine.
    struct Tree {
        _temp: TempDir,
        root: PathBuf,
        trust: Trust,
    }

    fn tree(tag: &str) -> Option<Tree> {
        let temp = TempDir::in_build_dir(tag);
        fs::set_permissions(&temp.0, fs::Permissions::from_mode(0o755)).unwrap();
        let trust = Trust::root_and(me());
        let root = match dir_chain(&temp.0, Role::Dir, &trust) {
            Ok(real) => real,
            Err(refused) => {
                eprintln!("note: skipped, the build directory's chain is refused: {refused}");
                return None;
            }
        };
        Some(Tree {
            _temp: temp,
            root,
            trust,
        })
    }

    fn mkdir(path: &Path, mode: u32) {
        fs::create_dir(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn write(path: &Path, mode: u32) {
        fs::write(path, b"content").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn a_sound_tree_passes() {
        let Some(tree) = tree("verify-sound") else {
            return;
        };
        let bin = tree.root.join("bin");
        let state = tree.root.join("state");
        mkdir(&bin, 0o755);
        mkdir(&state, 0o700);
        write(&bin.join("sing-box"), 0o755);
        assert_eq!(dir_chain(&bin, Role::Dir, &tree.trust).unwrap(), bin);
        assert_eq!(
            dir_chain(&state, Role::Private, &tree.trust).unwrap(),
            state
        );
        let mut file = file_chain(&bin.join("sing-box"), &tree.trust).unwrap();
        let mut content = String::new();
        io::Read::read_to_string(&mut file, &mut content).unwrap();
        assert_eq!(content, "content");
    }

    #[test]
    fn writable_directories_and_files_are_refused() {
        let Some(tree) = tree("verify-writable") else {
            return;
        };
        let bin = tree.root.join("bin");
        mkdir(&bin, 0o757);
        let error = dir_chain(&bin, Role::Dir, &tree.trust).unwrap_err();
        assert!(
            matches!(error.why, Why::Mode(ModeRefusal::WritableByOthers)),
            "{error}"
        );
        // Below a directory others may write, nothing passes.
        let inner = bin.join("inner");
        mkdir(&inner, 0o755);
        assert!(dir_chain(&inner, Role::Dir, &tree.trust).is_err());
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(dir_chain(&inner, Role::Dir, &tree.trust).is_ok());

        let file = bin.join("sing-box");
        write(&file, 0o775);
        let error = file_chain(&file, &tree.trust).unwrap_err();
        assert!(
            matches!(error.why, Why::Mode(ModeRefusal::FileWritableByGroup)),
            "{error}"
        );
        fs::set_permissions(&file, fs::Permissions::from_mode(0o4755)).unwrap();
        assert!(matches!(
            file_chain(&file, &tree.trust).unwrap_err().why,
            Why::Mode(ModeRefusal::SetId)
        ));
        fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(file_chain(&file, &tree.trust).is_ok());
    }

    #[test]
    fn a_state_directory_others_may_read_is_refused() {
        let Some(tree) = tree("verify-private") else {
            return;
        };
        let state = tree.root.join("state");
        mkdir(&state, 0o704);
        let error = dir_chain(&state, Role::Private, &tree.trust).unwrap_err();
        assert!(
            matches!(error.why, Why::Mode(ModeRefusal::NotPrivate(0o704))),
            "{error}"
        );
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(dir_chain(&state, Role::Private, &tree.trust).is_ok());
    }

    /// A link anywhere in the path, to a directory or a file, is refused
    /// rather than followed.
    #[test]
    fn links_are_refused() {
        let Some(tree) = tree("verify-links") else {
            return;
        };
        let real = tree.root.join("real");
        mkdir(&real, 0o755);
        write(&real.join("sing-box"), 0o755);
        let linked_dir = tree.root.join("linked");
        symlink(&real, &linked_dir).unwrap();
        let error = dir_chain(&linked_dir, Role::Dir, &tree.trust).unwrap_err();
        assert!(matches!(error.why, Why::Link(_)), "{error}");
        assert!(file_chain(&linked_dir.join("sing-box"), &tree.trust).is_err());

        let linked_file = real.join("linked-sing-box");
        symlink(real.join("sing-box"), &linked_file).unwrap();
        let error = file_chain(&linked_file, &tree.trust).unwrap_err();
        assert!(
            matches!(error.why, Why::Mode(ModeRefusal::Symlink)),
            "{error}"
        );
        assert!(matches!(
            dir_only(&linked_dir, Role::Dir, &tree.trust)
                .unwrap_err()
                .why,
            Why::Mode(ModeRefusal::Symlink)
        ));
    }

    #[test]
    fn only_plain_absolute_paths_are_taken() {
        let trust = Trust::root();
        for path in ["relative/dir", "/a/../b", "/a/./b"] {
            let error = dir_chain(Path::new(path), Role::Dir, &trust).unwrap_err();
            assert!(
                matches!(error.why, Why::NotAbsolute | Why::Io(_)),
                "{path}: {error}"
            );
        }
        assert!(matches!(
            dir_chain(Path::new("/a/../b"), Role::Dir, &trust)
                .unwrap_err()
                .why,
            Why::NotAbsolute
        ));
        assert!(matches!(
            open_file(Path::new("x"), &trust).unwrap_err().why,
            Why::NotAbsolute
        ));
    }

    /// What isn't root's fails the installed helper's trust: here, the
    /// tests' own files, unless the tests run as root.
    #[test]
    fn the_installed_trust_is_roots_alone() {
        let Some(tree) = tree("verify-root") else {
            return;
        };
        let result = dir_chain(&tree.root, Role::Dir, &Trust::root());
        if me() == 0 {
            assert!(result.is_ok());
        } else {
            assert!(
                matches!(result.unwrap_err().why, Why::Mode(ModeRefusal::Owner(uid)) if uid == me())
            );
        }
    }
}
