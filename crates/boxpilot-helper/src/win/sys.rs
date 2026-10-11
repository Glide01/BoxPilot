//! The small wrappers the rest of the platform layer builds on: handles,
//! events, waits, wide strings and Win32 errors.

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::time::Instant;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HANDLE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT, WIN32_ERROR};
use windows::Win32::System::Threading::{
    CreateEventW, ResetEvent, SetEvent, WaitForMultipleObjects, WaitForSingleObject, INFINITE,
};

/// The Win32 handle of a std handle owner, for a call that borrows it.
pub(crate) fn raw(handle: &impl AsRawHandle) -> HANDLE {
    HANDLE(handle.as_raw_handle())
}

/// Take ownership of a handle a Win32 call has just returned.
///
/// # Safety
/// `handle` must be a valid, open handle that nothing else owns or closes.
pub(crate) unsafe fn own(handle: HANDLE) -> OwnedHandle {
    // SAFETY: the caller guarantees `handle` is valid and unowned.
    unsafe { OwnedHandle::from_raw_handle(handle.0) }
}

/// A Win32 error as an `io::Error` with its Win32 code, so `kind()` works.
pub(crate) fn io_error(error: windows::core::Error) -> io::Error {
    match WIN32_ERROR::from_error(&error) {
        Some(code) => io::Error::from_raw_os_error(code.0 as i32),
        None => io::Error::other(error.message()),
    }
}

/// Whether `error` is the Win32 error `code`.
pub(crate) fn is_win32(error: &windows::core::Error, code: WIN32_ERROR) -> bool {
    WIN32_ERROR::from_error(error) == Some(code)
}

/// `text` as a NUL-terminated UTF-16 string, refusing an interior NUL that
/// would cut it short.
pub(crate) fn wide(text: impl AsRef<OsStr>) -> io::Result<Vec<u16>> {
    let mut wide: Vec<u16> = text.as_ref().encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a string holds a NUL character",
        ));
    }
    wide.push(0);
    Ok(wide)
}

/// A pointer to a string from [`wide`], valid while `wide` lives.
pub(crate) fn pcwstr(wide: &[u16]) -> PCWSTR {
    debug_assert_eq!(wide.last(), Some(&0));
    PCWSTR(wide.as_ptr())
}

/// A manual-reset event: stays signaled until reset.
#[derive(Debug)]
pub(crate) struct Event(OwnedHandle);

impl Event {
    pub(crate) fn new() -> io::Result<Self> {
        // SAFETY: no security attributes and no name; the call only creates
        // an event and returns its handle.
        let handle =
            unsafe { CreateEventW(None, true, false, PCWSTR::null()) }.map_err(io_error)?;
        // SAFETY: `handle` was just created, and only this event owns it.
        Ok(Self(unsafe { own(handle) }))
    }

    pub(crate) fn set(&self) {
        // SAFETY: the event handle is open for as long as `self` lives.
        let _ = unsafe { SetEvent(self.handle()) };
    }

    pub(crate) fn reset(&self) {
        // SAFETY: the event handle is open for as long as `self` lives.
        let _ = unsafe { ResetEvent(self.handle()) };
    }

    pub(crate) fn handle(&self) -> HANDLE {
        raw(&self.0)
    }

    pub(crate) fn is_set(&self) -> bool {
        matches!(wait_events(&[self], Some(Instant::now())), Ok(Some(0)))
    }
}

/// Milliseconds from now to `deadline` for a Win32 wait: 0 once it has
/// passed, `INFINITE` for none.
pub(crate) fn millis_until(deadline: Option<Instant>) -> u32 {
    match deadline {
        None => INFINITE,
        Some(deadline) => {
            let left = deadline
                .saturating_duration_since(Instant::now())
                .as_millis();
            u32::try_from(left)
                .unwrap_or(INFINITE - 1)
                .min(INFINITE - 1)
        }
    }
}

/// Wait until one of `events` is signaled (its index) or `deadline` passes
/// (`None`).
pub(crate) fn wait_events(
    events: &[&Event],
    deadline: Option<Instant>,
) -> io::Result<Option<usize>> {
    let handles: Vec<HANDLE> = events.iter().map(|event| event.handle()).collect();
    // SAFETY: every handle belongs to an `Event` borrowed for the whole call,
    // so each stays open while the wait uses it.
    let result = unsafe { WaitForMultipleObjects(&handles, false, millis_until(deadline)) };
    signaled(result, handles.len())
}

/// Wait until `handle` (a process, say) is signaled; `false` at `deadline`.
pub(crate) fn wait_handle(handle: &OwnedHandle, deadline: Option<Instant>) -> io::Result<bool> {
    // SAFETY: `handle` is borrowed for the whole call, so it stays open.
    let result = unsafe { WaitForSingleObject(raw(handle), millis_until(deadline)) };
    Ok(signaled(result, 1)?.is_some())
}

fn signaled(
    result: windows::Win32::Foundation::WAIT_EVENT,
    count: usize,
) -> io::Result<Option<usize>> {
    if result == WAIT_TIMEOUT {
        return Ok(None);
    }
    if result == WAIT_FAILED {
        return Err(io::Error::last_os_error());
    }
    let index = result.0.wrapping_sub(WAIT_OBJECT_0.0) as usize;
    if index < count {
        Ok(Some(index))
    } else {
        // WAIT_ABANDONED: only mutexes are abandoned, and none is waited on.
        Err(io::Error::other("an unexpected wait result"))
    }
}
