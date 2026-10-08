//! Where Windows keeps things, asked of Windows itself rather than of
//! environment variables.

use super::sys::io_error;
use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use windows::core::GUID;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::SystemInformation::GetSystemWindowsDirectoryW;
use windows::Win32::UI::Shell::{
    FOLDERID_ProgramData, FOLDERID_ProgramFiles, SHGetKnownFolderPath, KF_FLAG_DEFAULT,
};

fn known_folder(id: &GUID) -> io::Result<PathBuf> {
    // SAFETY: `id` is a known-folder GUID that outlives the call; no token
    // (the helper's own); on success the result is a CoTaskMemAlloc'd,
    // NUL-terminated string, freed below.
    let path = unsafe { SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, HANDLE::default()) }
        .map_err(io_error)?;
    // SAFETY: `path` is the NUL-terminated string just returned.
    let wide = unsafe { path.as_wide() }.to_vec();
    // SAFETY: `path` came from SHGetKnownFolderPath and is freed once.
    unsafe { CoTaskMemFree(Some(path.0 as *const _)) };
    Ok(PathBuf::from(OsString::from_wide(&wide)))
}

/// `C:\Program Files` (the 64-bit one).
pub(crate) fn program_files() -> io::Result<PathBuf> {
    known_folder(&FOLDERID_ProgramFiles)
}

/// `C:\ProgramData`.
pub(crate) fn program_data() -> io::Result<PathBuf> {
    known_folder(&FOLDERID_ProgramData)
}

/// The Windows directory, `C:\Windows`, as text for sing-box's environment.
pub(crate) fn windows_dir() -> io::Result<String> {
    let mut buf = vec![0u16; 260];
    loop {
        // SAFETY: `buf` is a writable buffer of the length passed.
        let len = unsafe { GetSystemWindowsDirectoryW(Some(&mut buf)) } as usize;
        if len == 0 {
            return Err(io::Error::last_os_error());
        }
        if len < buf.len() {
            return String::from_utf16(&buf[..len])
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "the Windows directory"));
        }
        buf.resize(len + 1, 0);
    }
}
