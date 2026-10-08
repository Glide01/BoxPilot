//! Verifying the helper's trees before it trusts anything in them (ADR 0006
//! rules 3 and 7): every directory from the volume root down, and each file
//! the helper runs or reads, judged by `acl::judge`. Done when the helper
//! starts and again before every spawn. The helper directory and its files
//! are judged as `Role::Object` (nobody else may write them), the state
//! directory and the directories in it as `Role::Private` (nobody else may
//! read them either).
//!
//! Each object is opened without following a reparse point
//! (`FILE_FLAG_OPEN_REPARSE_POINT`), so a link is seen as a link and
//! refused, never resolved; its owner, DACL and attributes are read through
//! that handle.

use super::security::read_security;
use crate::acl::{self, AclRefusal, Role, Trusted};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use windows::Win32::Foundation::GENERIC_READ;
use windows::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, READ_CONTROL,
};

/// Why a path was refused: where, and what was wrong.
#[derive(Debug)]
pub(crate) struct Refused {
    pub(crate) path: PathBuf,
    pub(crate) why: Why,
}

#[derive(Debug)]
pub(crate) enum Why {
    Acl(AclRefusal),
    Io(io::Error),
    NotAbsolute,
    NotAFile,
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = self.path.display();
        match &self.why {
            Why::Acl(refusal) => write!(f, "{path} {refusal}"),
            Why::Io(error) => write!(f, "{path} could not be checked: {error}"),
            Why::NotAbsolute => write!(f, "{path} is not a plain absolute path"),
            Why::NotAFile => write!(f, "{path} is not a regular file"),
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

/// Open a directory (or the volume root) to read its security, without
/// following a reparse point, and sharing everything: nothing is read from
/// it but its descriptor.
fn open_for_security(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .access_mode(READ_CONTROL.0 | FILE_READ_ATTRIBUTES.0)
        .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(path)
}

fn judge_open(path: &Path, file: &File, role: Role, trusted: &Trusted) -> Result<(), Refused> {
    let security = read_security(file).map_err(|error| refused(path, Why::Io(error)))?;
    acl::judge(&security, role, trusted).map_err(|why| refused(path, Why::Acl(why)))
}

/// Verify directory `dir` in `role` and every directory above it, up to the
/// volume root, as ancestors.
pub(crate) fn dir_chain(dir: &Path, role: Role, trusted: &Trusted) -> Result<(), Refused> {
    let plain = dir.is_absolute()
        && dir
            .components()
            .all(|part| !matches!(part, Component::CurDir | Component::ParentDir));
    if !plain {
        return Err(refused(dir, Why::NotAbsolute));
    }
    for (depth, path) in dir.ancestors().enumerate() {
        let role = if depth == 0 { role } else { Role::Ancestor };
        let handle = open_for_security(path).map_err(|error| refused(path, Why::Io(error)))?;
        judge_open(path, &handle, role, trusted)?;
    }
    Ok(())
}

/// Verify directory `dir` alone, in `role`: for a directory the helper
/// created inside a tree whose chain it has just verified.
pub(crate) fn dir_only(dir: &Path, role: Role, trusted: &Trusted) -> Result<(), Refused> {
    let handle = open_for_security(dir).map_err(|error| refused(dir, Why::Io(error)))?;
    judge_open(dir, &handle, role, trusted)
}

/// Open `path`, a file in a directory just verified, for reading, sharing
/// only reads: nobody can write, delete or rename it while the handle is
/// open. Verified as an object. The handle is how the helper then reads or
/// hashes it, and holding it until `CreateProcessW` returns keeps sing-box
/// the file that was hashed (ADR 0006 rule 3).
pub(crate) fn open_file(path: &Path, trusted: &Trusted) -> Result<File, Refused> {
    let file = OpenOptions::new()
        .access_mode(GENERIC_READ.0)
        .share_mode(FILE_SHARE_READ.0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(path)
        .map_err(|error| refused(path, Why::Io(error)))?;
    judge_open(path, &file, Role::Object, trusted)?;
    let metadata = file
        .metadata()
        .map_err(|error| refused(path, Why::Io(error)))?;
    if !metadata.is_file() {
        return Err(refused(path, Why::NotAFile));
    }
    Ok(file)
}
