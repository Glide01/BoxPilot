//! Security descriptors: built from SDDL for what the helper creates, and
//! read from what it opens, as `acl::Security` for `acl::judge`.

use super::sys::{io_error, pcwstr, raw, wide};
use crate::acl::{self, Ace};
use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::os::windows::fs::MetadataExt;
use std::path::Path;
use windows::core::PWSTR;
use windows::Win32::Foundation::{LocalFree, ERROR_ALREADY_EXISTS, ERROR_SUCCESS, HLOCAL};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    ConvertStringSidToSidW, GetSecurityInfo, SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows::Win32::Security::{
    GetAce, IsValidSid, ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, DACL_SECURITY_INFORMATION,
    OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES,
};
use windows::Win32::Storage::FileSystem::{CreateDirectoryW, FILE_ATTRIBUTE_REPARSE_POINT};

/// Memory `LocalAlloc`'d by a Win32 call, freed when dropped.
struct Local(*mut c_void);

impl Drop for Local {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the pointer came from a call documented to return
            // `LocalAlloc`'d memory, and is freed exactly once, here.
            unsafe { LocalFree(HLOCAL(self.0)) };
        }
    }
}

/// A self-relative security descriptor parsed from SDDL.
pub(crate) struct SecurityDescriptor(Local);

impl SecurityDescriptor {
    pub(crate) fn from_sddl(sddl: &str) -> io::Result<Self> {
        let text = wide(sddl)?;
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: `text` is NUL-terminated and outlives the call; on
        // success `descriptor` receives `LocalAlloc`'d memory, which `Local`
        // frees.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                pcwstr(&text),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
        }
        .map_err(io_error)?;
        Ok(Self(Local(descriptor.0)))
    }

    /// Non-inheritable attributes pointing at this descriptor. Use them
    /// only while `self` lives.
    pub(crate) fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0 .0,
            bInheritHandle: false.into(),
        }
    }
}

/// A SID parsed from its string form (`S-1-5-32-544`), freed when dropped.
pub(crate) struct OwnedSid(Local);

impl OwnedSid {
    pub(crate) fn from_string(text: &str) -> io::Result<Self> {
        let text = wide(text)?;
        let mut sid = PSID::default();
        // SAFETY: `text` is NUL-terminated and outlives the call; on success
        // `sid` receives `LocalAlloc`'d memory, which `Local` frees.
        unsafe { ConvertStringSidToSidW(pcwstr(&text), &mut sid) }.map_err(io_error)?;
        Ok(Self(Local(sid.0)))
    }

    /// The SID, valid while `self` lives.
    pub(crate) fn psid(&self) -> PSID {
        PSID(self.0 .0)
    }
}

/// The DACL of every directory the helper creates in its state tree:
/// `paths::STATE_DIR_DACL` (protected, so nothing is inherited from
/// Program Files, whose users may read; SYSTEM and Administrators only),
/// and, for the unprivileged console seam, the user running it.
pub(crate) fn protected_dir_sddl(extra_sid: Option<&str>) -> String {
    let mut sddl = String::from(crate::paths::STATE_DIR_DACL);
    if let Some(sid) = extra_sid {
        sddl.push_str(&format!("(A;OICI;FA;;;{sid})"));
    }
    sddl
}

/// Create directory `path` with `descriptor`. `Ok(false)` when something
/// already exists there, which the caller must then verify.
pub(crate) fn create_dir(path: &Path, descriptor: &SecurityDescriptor) -> io::Result<bool> {
    let path = wide(path)?;
    let attributes = descriptor.attributes();
    // SAFETY: `path` is NUL-terminated; `attributes` and the descriptor it
    // points at outlive the call.
    match unsafe { CreateDirectoryW(pcwstr(&path), Some(&attributes)) } {
        Ok(()) => Ok(true),
        Err(error) if super::sys::is_win32(&error, ERROR_ALREADY_EXISTS) => Ok(false),
        Err(error) => Err(io_error(error)),
    }
}

/// The string form (`S-1-5-18`) of the SID at `sid`, after checking it lies
/// within `room` bytes and is well formed. It reads the SID's 8-byte head,
/// then no more than the length that head gives.
///
/// # Safety
/// `sid` must be valid for reads of `room` bytes, or of the whole SID its
/// head describes if that is shorter (as for a SID the OS returned).
pub(crate) unsafe fn sid_within(sid: *const u8, room: usize) -> io::Result<String> {
    let malformed = || io::Error::new(io::ErrorKind::InvalidData, "a malformed SID");
    // A SID: revision, sub-authority count, a 6-byte authority, then 4
    // bytes per sub-authority, at most 15 of them.
    if room < 8 {
        return Err(malformed());
    }
    // SAFETY: `room` ≥ 8, so byte 1 is readable.
    let count = usize::from(unsafe { *sid.add(1) });
    if count > 15 || 8 + 4 * count > room {
        return Err(malformed());
    }
    let psid = PSID(sid as *mut c_void);
    // SAFETY: the whole SID, as its header sizes it, lies within `room`.
    if !unsafe { IsValidSid(psid) }.as_bool() {
        return Err(malformed());
    }
    let mut text = PWSTR::null();
    // SAFETY: `psid` is a valid SID; on success `text` is a `LocalAlloc`'d,
    // NUL-terminated string, freed by `Local`.
    unsafe { ConvertSidToStringSidW(psid, &mut text) }.map_err(io_error)?;
    let _free = Local(text.0.cast());
    // SAFETY: `text` is the NUL-terminated string just returned.
    unsafe { text.to_string() }.map_err(|_| malformed())
}

/// The owner, DACL and reparse-point flag of the object `file` is open on.
/// `file` must be open with `READ_CONTROL` and `FILE_READ_ATTRIBUTES`.
pub(crate) fn read_security(file: &File) -> io::Result<acl::Security> {
    let mut owner = PSID::default();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: `file` is open for the call; the out-pointers are valid. On
    // success `descriptor` is `LocalAlloc`'d memory that `owner` and `dacl`
    // point into, freed by `_free` only after both have been read.
    let status = unsafe {
        GetSecurityInfo(
            raw(file),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            Some(&mut dacl),
            None,
            Some(&mut descriptor),
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status.0 as i32));
    }
    let _free = Local(descriptor.0);
    if owner.0.is_null() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "no owner"));
    }
    // SAFETY: `owner` points at the owner SID inside `descriptor`, which is
    // alive; a SID is at most 68 bytes, and `sid_within` reads no further
    // than its own header says.
    let owner = unsafe { sid_within(owner.0.cast(), 68) }?;
    let dacl = if dacl.is_null() {
        None
    } else {
        // SAFETY: `dacl` points at the DACL inside `descriptor`, alive.
        Some(unsafe { read_dacl(dacl) }?)
    };
    let attributes = file.metadata()?.file_attributes();
    Ok(acl::Security {
        owner,
        dacl,
        reparse_point: attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0,
    })
}

/// Every ACE of `dacl`. The trustee is read for the ACE types whose layout
/// is `ACCESS_ALLOWED_ACE`'s (allow, deny and their callback forms); other
/// types get no SID, and `acl::judge` refuses any of them that applies.
///
/// # Safety
/// `dacl` must point at a valid ACL that stays alive during the call.
unsafe fn read_dacl(dacl: *const ACL) -> io::Result<Vec<Ace>> {
    // SAFETY: the caller guarantees `dacl` is a valid ACL.
    let count = unsafe { (*dacl).AceCount };
    let mut aces = Vec::with_capacity(usize::from(count));
    for index in 0..u32::from(count) {
        let mut ace: *mut c_void = std::ptr::null_mut();
        // SAFETY: `index` < the ACL's ACE count; `ace` receives a pointer
        // into the ACL.
        unsafe { GetAce(dacl, index, &mut ace) }.map_err(io_error)?;
        // SAFETY: every ACE starts with an `ACE_HEADER`.
        let header = unsafe { std::ptr::read_unaligned(ace as *const ACE_HEADER) };
        let size = usize::from(header.AceSize);
        let sid_offset = std::mem::offset_of!(ACCESS_ALLOWED_ACE, SidStart);
        if size < sid_offset {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "a truncated ACE",
            ));
        }
        // Every standard ACE type has its mask right after the header.
        // SAFETY: `size` ≥ `sid_offset` > the mask's offset + 4.
        let mask = unsafe {
            std::ptr::read_unaligned(
                (ace as *const u8).add(std::mem::size_of::<ACE_HEADER>()) as *const u32
            )
        };
        let sid = match header.AceType {
            acl::ace_type::ALLOWED
            | acl::ace_type::DENIED
            | acl::ace_type::ALLOWED_CALLBACK
            | acl::ace_type::DENIED_CALLBACK => {
                // SAFETY: the ACE is `size` bytes long, and its SID starts
                // at `sid_offset`, within it.
                unsafe { sid_within((ace as *const u8).add(sid_offset), size - sid_offset) }?
            }
            _ => String::new(),
        };
        aces.push(Ace {
            ace_type: header.AceType,
            flags: header.AceFlags,
            mask,
            sid,
        });
    }
    Ok(aces)
}
