//! Who is on the other end of the pipe (ADR 0006 rule 4): impersonate the
//! client, open the thread's token, revert, then read the token. The
//! decision itself is `authority::authority`, over what is read here. The
//! client's PID is never used: PIDs are reused, so a check on one races.

use super::security::sid_within;
use super::sys::{io_error, is_win32, own, raw};
use crate::authority::{self, TokenFacts};
use crate::helper::Caller;
use crate::spawnplan::ObservedToken;
use std::ffi::c_void;
use std::io;
use std::mem::{offset_of, size_of};
use std::os::windows::io::OwnedHandle;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, HANDLE, LUID};
use windows::Win32::Security::{
    GetTokenInformation, LookupPrivilegeNameW, RevertToSelf, TokenElevation, TokenGroups,
    TokenIntegrityLevel, TokenIsAppContainer, TokenPrivileges, TokenRestrictedSids, TokenUser,
    LUID_AND_ATTRIBUTES, SID_AND_ATTRIBUTES, TOKEN_ACCESS_MASK, TOKEN_ADJUST_DEFAULT,
    TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_ELEVATION, TOKEN_GROUPS, TOKEN_INFORMATION_CLASS,
    TOKEN_MANDATORY_LABEL, TOKEN_PRIVILEGES, TOKEN_QUERY, TOKEN_USER,
};
use windows::Win32::System::Pipes::ImpersonateNamedPipeClient;
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, OpenProcessToken, OpenThreadToken,
};

/// What the OS says about the client of the connected pipe `pipe`. Any
/// failure makes the caller read-only, with no account.
pub(crate) fn caller(pipe: &OwnedHandle) -> Caller {
    match client_token(pipe) {
        Ok(token) => {
            let facts = token.facts();
            Caller {
                authority: authority::authority(&facts),
                user: facts.user,
            }
        }
        Err(_) => Caller::read_only(),
    }
}

/// The client's token: opened while impersonating it, used after.
fn client_token(pipe: &OwnedHandle) -> io::Result<Token> {
    // Created before impersonating, so it reverts on every path out, the
    // failed impersonation included.
    let impersonating = Impersonating;
    // SAFETY: `pipe` is the server end of a connected named pipe, open for
    // the call.
    unsafe { ImpersonateNamedPipeClient(raw(pipe)) }.map_err(io_error)?;
    let token = Token::of_thread();
    drop(impersonating);
    token
}

/// Reverts the thread to the service's own identity when dropped.
struct Impersonating;

impl Drop for Impersonating {
    fn drop(&mut self) {
        // SAFETY: RevertToSelf takes nothing and only ends this thread's
        // impersonation, if any.
        if unsafe { RevertToSelf() }.is_err() {
            // A SYSTEM service thread left running as its client is the one
            // state the helper must never go on from.
            std::process::abort();
        }
    }
}

/// An access token, open for `TOKEN_QUERY`.
pub(crate) struct Token(OwnedHandle);

impl Token {
    /// The token of the thread, which is impersonating the client. Opened
    /// as the service itself (`OpenAsSelf`), so the client's rights on the
    /// thread don't matter. Fails for an anonymous client.
    fn of_thread() -> io::Result<Self> {
        let mut token = HANDLE::default();
        // SAFETY: the pseudo-handle of the current thread needs no closing;
        // `token` receives a new handle, owned below.
        unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, true, &mut token) }
            .map_err(io_error)?;
        // SAFETY: `token` was just opened and nothing else owns it.
        Ok(Self(unsafe { own(token) }))
    }

    /// The helper's own token.
    pub(crate) fn of_process() -> io::Result<Self> {
        Self::open_process(current_process(), TOKEN_QUERY)
    }

    /// The helper's own primary token, opened to make a restricted copy of
    /// it for sing-box (`restrict`): `CreateRestrictedToken` needs
    /// `TOKEN_DUPLICATE`; the copy gets this handle's access, and
    /// `CreateProcessAsUserW` needs `TOKEN_QUERY`, `TOKEN_DUPLICATE` and
    /// `TOKEN_ASSIGN_PRIMARY` on it, and lowering its integrity level
    /// `TOKEN_ADJUST_DEFAULT`. Never a thread's (impersonation) token.
    pub(crate) fn of_process_to_restrict() -> io::Result<Self> {
        Self::open_process(
            current_process(),
            TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_ASSIGN_PRIMARY | TOKEN_ADJUST_DEFAULT,
        )
    }

    /// The primary token of `process`, a process handle with at least
    /// `PROCESS_QUERY_LIMITED_INFORMATION`, for reading.
    pub(crate) fn of_process_handle(process: HANDLE) -> io::Result<Self> {
        Self::open_process(process, TOKEN_QUERY)
    }

    fn open_process(process: HANDLE, access: TOKEN_ACCESS_MASK) -> io::Result<Self> {
        let mut token = HANDLE::default();
        // SAFETY: `process` is open for the call, or the current process's
        // pseudo-handle; `token` receives a new handle, owned below.
        unsafe { OpenProcessToken(process, access, &mut token) }.map_err(io_error)?;
        // SAFETY: `token` was just opened and nothing else owns it.
        Ok(Self(unsafe { own(token) }))
    }

    /// A token a Win32 call has just created, such as a restricted copy.
    pub(crate) fn from_owned(handle: OwnedHandle) -> Self {
        Self(handle)
    }

    /// The handle, for a call that borrows it while `self` lives.
    pub(crate) fn handle(&self) -> HANDLE {
        raw(&self.0)
    }

    /// Every privilege the token holds (`TokenPrivileges`), with its LUID
    /// and attributes. A LUID with no name gets `#<high>:<low>`, which no
    /// allowlist names.
    pub(crate) fn privileges(&self) -> io::Result<Vec<Privilege>> {
        let (buf, len) = self.query(TokenPrivileges)?;
        if len < size_of::<u32>() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "short TOKEN_PRIVILEGES",
            ));
        }
        // SAFETY: GetTokenInformation wrote a TOKEN_PRIVILEGES at the start
        // of `buf`, 8-aligned, and its leading count lies within the
        // `len` ≥ 4 bytes written.
        let count = unsafe { (*(buf.as_ptr() as *const TOKEN_PRIVILEGES)).PrivilegeCount } as usize;
        let entries_at = offset_of!(TOKEN_PRIVILEGES, Privileges);
        let end = count
            .checked_mul(size_of::<LUID_AND_ATTRIBUTES>())
            .and_then(|bytes| bytes.checked_add(entries_at));
        if end.is_none_or(|end| end > len) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "TOKEN_PRIVILEGES overruns",
            ));
        }
        // SAFETY: the `count` entries start at `entries_at` and end within
        // the `len` bytes written, as just checked; `buf` is 8-aligned, more
        // than LUID_AND_ATTRIBUTES needs.
        let entries = unsafe {
            std::slice::from_raw_parts(
                (buf.as_ptr() as *const u8).add(entries_at) as *const LUID_AND_ATTRIBUTES,
                count,
            )
        };
        Ok(entries
            .iter()
            .map(|entry| Privilege {
                name: privilege_name(&entry.Luid).unwrap_or_else(|| {
                    format!("#{:x}:{:x}", entry.Luid.HighPart, entry.Luid.LowPart)
                }),
                luid: entry.Luid,
                attributes: entry.Attributes.0,
            })
            .collect())
    }

    /// Privileges, integrity level and groups, as `spawnplan` judges them.
    /// Any of them unreadable is an error: a token that can't be read back
    /// is never one sing-box starts under.
    pub(crate) fn observed(&self) -> io::Result<ObservedToken> {
        Ok(ObservedToken {
            privileges: self
                .privileges()?
                .into_iter()
                .map(|privilege| (privilege.name, privilege.attributes))
                .collect(),
            integrity: Some(self.integrity()?),
            groups: self.groups()?,
        })
    }

    /// One `TOKEN_INFORMATION_CLASS`, in an 8-aligned buffer (enough for
    /// every TOKEN_* structure), with its length in bytes.
    fn query(&self, class: TOKEN_INFORMATION_CLASS) -> io::Result<(Vec<u64>, usize)> {
        let mut needed = 0u32;
        // SAFETY: a size query: no buffer, `needed` receives the size. It
        // fails with ERROR_INSUFFICIENT_BUFFER by design.
        let _ = unsafe { GetTokenInformation(raw(&self.0), class, None, 0, &mut needed) };
        if needed == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buf = vec![0u64; (needed as usize).div_ceil(size_of::<u64>())];
        let capacity = (buf.len() * size_of::<u64>()) as u32;
        // SAFETY: `buf` holds `capacity` ≥ `needed` writable bytes.
        unsafe {
            GetTokenInformation(
                raw(&self.0),
                class,
                Some(buf.as_mut_ptr().cast::<c_void>()),
                capacity,
                &mut needed,
            )
        }
        .map_err(io_error)?;
        Ok((buf, needed as usize))
    }

    /// The SID of `entry`, which lies in `buf` (`len` bytes written).
    fn sid_of(buf: &[u64], len: usize, entry: &SID_AND_ATTRIBUTES) -> io::Result<String> {
        let start = buf.as_ptr() as usize;
        let sid = entry.Sid.0 as usize;
        if sid < start || sid >= start + len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "a token SID outside its buffer",
            ));
        }
        // SAFETY: `sid` lies in `buf`, and the `start + len - sid` bytes
        // after it are within the bytes GetTokenInformation wrote.
        unsafe { sid_within(sid as *const u8, start + len - sid) }
    }

    pub(crate) fn user(&self) -> io::Result<String> {
        let (buf, len) = self.query(TokenUser)?;
        if len < size_of::<TOKEN_USER>() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "short TokenUser",
            ));
        }
        // SAFETY: GetTokenInformation(TokenUser) wrote a TOKEN_USER at the
        // start of `buf`, which is 8-aligned and at least that long.
        let user = unsafe { &*(buf.as_ptr() as *const TOKEN_USER) };
        Self::sid_of(&buf, len, &user.User)
    }

    pub(crate) fn groups(&self) -> io::Result<Vec<(String, u32)>> {
        self.group_list(TokenGroups)
    }

    /// A TOKEN_GROUPS class: each group's SID and attributes.
    fn group_list(&self, class: TOKEN_INFORMATION_CLASS) -> io::Result<Vec<(String, u32)>> {
        let (buf, len) = self.query(class)?;
        if len < size_of::<u32>() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "short TOKEN_GROUPS",
            ));
        }
        // SAFETY: GetTokenInformation wrote a TOKEN_GROUPS at the start of
        // `buf`, 8-aligned, and its leading count lies within the `len` ≥ 4
        // bytes written.
        let count = unsafe { (*(buf.as_ptr() as *const TOKEN_GROUPS)).GroupCount } as usize;
        // An empty list (no restricting SIDs, say) may be written as the
        // count alone.
        if count == 0 {
            return Ok(Vec::new());
        }
        let entries_at = offset_of!(TOKEN_GROUPS, Groups);
        let end = count
            .checked_mul(size_of::<SID_AND_ATTRIBUTES>())
            .and_then(|bytes| bytes.checked_add(entries_at));
        if end.is_none_or(|end| end > len) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "TOKEN_GROUPS overruns",
            ));
        }
        // SAFETY: the `count` entries start at `entries_at` and end within
        // the `len` bytes written, as just checked; `buf` is 8-aligned, as
        // SID_AND_ATTRIBUTES requires.
        let entries = unsafe {
            std::slice::from_raw_parts(
                (buf.as_ptr() as *const u8).add(entries_at) as *const SID_AND_ATTRIBUTES,
                count,
            )
        };
        entries
            .iter()
            .map(|entry| Ok((Self::sid_of(&buf, len, entry)?, entry.Attributes)))
            .collect()
    }

    /// The integrity level's RID: the last sub-authority of the label SID.
    pub(crate) fn integrity(&self) -> io::Result<u32> {
        let (buf, len) = self.query(TokenIntegrityLevel)?;
        if len < size_of::<TOKEN_MANDATORY_LABEL>() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "short integrity label",
            ));
        }
        // SAFETY: GetTokenInformation(TokenIntegrityLevel) wrote a
        // TOKEN_MANDATORY_LABEL at the start of `buf`, 8-aligned.
        let label = unsafe { &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL) };
        let sid = Self::sid_of(&buf, len, &label.Label)?;
        sid.rsplit('-')
            .next()
            .and_then(|rid| rid.parse().ok())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no integrity RID"))
    }

    /// Whether the token has restricting SIDs.
    pub(crate) fn restricted(&self) -> io::Result<bool> {
        Ok(!self.group_list(TokenRestrictedSids)?.is_empty())
    }

    pub(crate) fn app_container(&self) -> io::Result<bool> {
        let (buf, len) = self.query(TokenIsAppContainer)?;
        if len < size_of::<u32>() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "short TokenIsAppContainer",
            ));
        }
        Ok(buf[0] as u32 != 0)
    }

    pub(crate) fn elevated(&self) -> io::Result<bool> {
        let (buf, len) = self.query(TokenElevation)?;
        if len < size_of::<TOKEN_ELEVATION>() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "short TokenElevation",
            ));
        }
        // SAFETY: GetTokenInformation(TokenElevation) wrote a
        // TOKEN_ELEVATION at the start of `buf`, 8-aligned.
        Ok(unsafe { (*(buf.as_ptr() as *const TOKEN_ELEVATION)).TokenIsElevated } != 0)
    }

    /// Everything `authority::authority` decides on. What can't be read is
    /// left out, which makes the caller read-only.
    pub(crate) fn facts(&self) -> TokenFacts {
        TokenFacts {
            user: self.user().ok(),
            groups: self.groups().unwrap_or_default(),
            integrity: self.integrity().ok(),
            restricted: self.restricted().unwrap_or(true),
            app_container: self.app_container().unwrap_or(true),
        }
    }
}

/// The current process's pseudo-handle, which needs no closing.
fn current_process() -> HANDLE {
    // SAFETY: GetCurrentProcess takes nothing and returns a constant.
    unsafe { GetCurrentProcess() }
}

/// One privilege of a token.
pub(crate) struct Privilege {
    /// Its name (`SeChangeNotifyPrivilege`), or `#<high>:<low>` if the LUID
    /// has none.
    pub(crate) name: String,
    pub(crate) luid: LUID,
    /// `SE_PRIVILEGE_ENABLED` and the like.
    pub(crate) attributes: u32,
}

/// The name of the privilege `luid`, if it has one.
fn privilege_name(luid: &LUID) -> Option<String> {
    let mut buf = vec![0u16; 64];
    loop {
        let mut len = buf.len() as u32;
        // SAFETY: `luid` is valid for the call; `buf` holds `len` writable
        // UTF-16 units, and `len` says so.
        match unsafe {
            LookupPrivilegeNameW(PCWSTR::null(), luid, PWSTR(buf.as_mut_ptr()), &mut len)
        } {
            // `len` is now the name's length, without the NUL.
            Ok(()) => return String::from_utf16(buf.get(..len as usize)?).ok(),
            // `len` is now the size needed, with the NUL.
            Err(error)
                if is_win32(&error, ERROR_INSUFFICIENT_BUFFER)
                    && len as usize > buf.len()
                    && len <= 1024 =>
            {
                buf.resize(len as usize, 0)
            }
            Err(_) => return None,
        }
    }
}
