//! The Windows half of the helper client: the helper's pipe, its
//! demand-start service, and the token check. Every decision is the pure
//! module's ([`ConnectPlan`]); this is only the system calls.
//!
//! The pipe is opened for `GENERIC_READ | FILE_WRITE_DATA` (its DACL grants
//! interactive users exactly that, not `GENERIC_WRITE`), with an
//! identification-only security QoS (the helper may learn who is asking,
//! never act as them), and overlapped, so the reading thread and a `stop`
//! can use it at once: a synchronous pipe handle serializes its I/O, and a
//! read that waits for sing-box's next line would hold a write back.

use super::client::HelperIo;
use super::{ConnectPlan, ConnectStep, OpenError, PipeOpen, ServiceStart, ServiceState};
use boxpilot_protocol::endpoint::{PIPE_NAME, SERVICE_NAME};
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Instant;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_NO_DATA, ERROR_PIPE_NOT_CONNECTED, GENERIC_READ,
    HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT, WIN32_ERROR,
};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAG_OVERLAPPED, FILE_SHARE_NONE, FILE_WRITE_DATA,
    OPEN_EXISTING, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
};
use windows::Win32::System::Pipes::WaitNamedPipeW;
use windows::Win32::System::Services::{
    CloseServiceHandle, OpenSCManagerW, OpenServiceW, QueryServiceStatus, StartServiceW, SC_HANDLE,
    SC_MANAGER_CONNECT, SERVICE_QUERY_STATUS, SERVICE_START, SERVICE_STATUS, SERVICE_STOPPED,
};
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, OpenProcessToken, ResetEvent, SetEvent,
    WaitForMultipleObjects, INFINITE,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

/// Whether this process runs with an elevated token: only the user can have
/// made it so (Run as administrator), since BoxPilot never asks. Read once;
/// a token's elevation doesn't change.
pub(super) fn process_is_elevated() -> bool {
    static ELEVATED: OnceLock<bool> = OnceLock::new();
    *ELEVATED.get_or_init(|| {
        let mut token = HANDLE::default();
        // SAFETY: the pseudo-handle of this process; `token` receives a new
        // handle, owned below.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.is_err() {
            return false;
        }
        // SAFETY: a valid token handle, just opened, owned by nobody else.
        let token = unsafe { OwnedHandle::from_raw_handle(token.0) };
        let mut elevation = TOKEN_ELEVATION::default();
        let mut size = 0u32;
        // SAFETY: `elevation` is a TOKEN_ELEVATION, the size passed is its
        // own, and the token stays open for the call.
        let read = unsafe {
            GetTokenInformation(
                HANDLE(token.as_raw_handle()),
                TokenElevation,
                Some(&mut elevation as *mut TOKEN_ELEVATION as *mut _),
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut size,
            )
        };
        read.is_ok() && elevation.TokenIsElevated != 0
    })
}

/// Connect to the helper, starting its service as [`ConnectPlan`] says.
pub(super) fn open() -> Result<Arc<dyn HelperIo>, OpenError> {
    let name = wide(PIPE_NAME);
    let mut plan = ConnectPlan::new(Instant::now());
    loop {
        let error = match open_pipe(&name) {
            Ok(pipe) => {
                let io = PipeIo::new(pipe).map_err(|e| OpenError::Os(e.to_string()))?;
                return Ok(Arc::new(io));
            }
            Err(code) => PipeOpen::from_win32(code),
        };
        match plan.pipe_failed(error, Instant::now())? {
            ConnectStep::StartService => plan.service_started(start_service())?,
            ConnectStep::CheckService => {
                if let Some(state) = service_state() {
                    if plan.service_state(state)? {
                        plan.service_started(start_service())?;
                    }
                }
            }
            ConnectStep::WaitBusy(wait) => {
                let millis = u32::try_from(wait.as_millis()).unwrap_or(INFINITE - 1);
                // SAFETY: `name` is NUL-terminated and outlives the call.
                let _ = unsafe { WaitNamedPipeW(PCWSTR(name.as_ptr()), millis) };
                continue;
            }
        }
        thread::sleep(plan.backoff(Instant::now())?);
    }
}

/// `text` as a NUL-terminated UTF-16 string.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The Win32 code of a failed call.
fn win32_code(error: &windows::core::Error) -> u32 {
    WIN32_ERROR::from_error(error)
        .map(|code| code.0)
        .unwrap_or(error.code().0 as u32)
}

fn open_pipe(name: &[u16]) -> Result<OwnedHandle, u32> {
    // SAFETY: `name` is NUL-terminated; no security attributes, no
    // template. The handle returned is owned below.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(name.as_ptr()),
            GENERIC_READ.0 | FILE_WRITE_DATA.0,
            FILE_SHARE_NONE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
            None,
        )
    }
    .map_err(|error| win32_code(&error))?;
    // SAFETY: a valid pipe handle, just opened, owned by nobody else.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle.0) })
}

/// A service-control handle, closed on drop.
struct ScHandle(SC_HANDLE);

impl Drop for ScHandle {
    fn drop(&mut self) {
        // SAFETY: an open handle that only this value owns.
        let _ = unsafe { CloseServiceHandle(self.0) };
    }
}

/// The helper's service, opened with `SERVICE_START | SERVICE_QUERY_STATUS`
/// only, the rights its DACL grants interactive users; the manager with
/// `SC_MANAGER_CONNECT` only.
fn open_service() -> Result<(ScHandle, ScHandle), u32> {
    // SAFETY: the local machine's active database; the handle is owned.
    let manager = unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT) }
        .map(ScHandle)
        .map_err(|error| win32_code(&error))?;
    let name = wide(SERVICE_NAME);
    // SAFETY: an open manager handle and a NUL-terminated name; the handle
    // returned is owned.
    let service = unsafe {
        OpenServiceW(
            manager.0,
            PCWSTR(name.as_ptr()),
            SERVICE_START | SERVICE_QUERY_STATUS,
        )
    }
    .map(ScHandle)
    .map_err(|error| win32_code(&error))?;
    Ok((manager, service))
}

fn start_service() -> ServiceStart {
    let result = open_service().and_then(|(_manager, service)| {
        // SAFETY: an open service handle with SERVICE_START; no arguments.
        unsafe { StartServiceW(service.0, None) }.map_err(|error| win32_code(&error))
    });
    ServiceStart::from_win32(result.err())
}

/// The service's status; `None` if it can't be read.
fn service_state() -> Option<ServiceState> {
    let (_manager, service) = open_service().ok()?;
    let mut status = SERVICE_STATUS::default();
    // SAFETY: an open service handle with SERVICE_QUERY_STATUS, and a
    // SERVICE_STATUS to fill.
    unsafe { QueryServiceStatus(service.0, &mut status) }.ok()?;
    Some(if status.dwCurrentState == SERVICE_STOPPED {
        ServiceState::Stopped {
            win32_exit: status.dwWin32ExitCode,
            service_exit: status.dwServiceSpecificExitCode,
        }
    } else {
        ServiceState::Active
    })
}

/// A manual-reset event, closed on drop.
struct Event(OwnedHandle);

impl Event {
    fn new() -> io::Result<Self> {
        // SAFETY: no security attributes and no name.
        let handle = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }
            .map_err(|error| io::Error::from_raw_os_error(win32_code(&error) as i32))?;
        // SAFETY: a valid event handle, just created, owned by nobody else.
        Ok(Self(unsafe { OwnedHandle::from_raw_handle(handle.0) }))
    }

    fn handle(&self) -> HANDLE {
        HANDLE(self.0.as_raw_handle())
    }
}

/// Wait until one of `events` is signaled (its index), or `deadline`
/// passes (`None`).
fn wait_any(events: &[&Event], deadline: Option<Instant>) -> Option<usize> {
    let handles: Vec<HANDLE> = events.iter().map(|event| event.handle()).collect();
    let millis = match deadline {
        None => INFINITE,
        Some(deadline) => {
            let left = deadline
                .saturating_duration_since(Instant::now())
                .as_millis();
            u32::try_from(left)
                .unwrap_or(INFINITE - 1)
                .min(INFINITE - 1)
        }
    };
    // SAFETY: every handle belongs to an event borrowed for the whole call.
    let result = unsafe { WaitForMultipleObjects(&handles, false, millis) };
    if result == WAIT_TIMEOUT {
        return None;
    }
    let index = result.0.wrapping_sub(WAIT_OBJECT_0.0) as usize;
    // A failed wait reads as "not done": the caller cancels and waits for
    // the operation itself.
    (index < handles.len()).then_some(index)
}

/// The helper's pipe as a [`HelperIo`]: overlapped reads and writes, each
/// waited for together with the `closed` event, so a deadline or
/// [`HelperIo::close`] ends it at once.
struct PipeIo {
    pipe: OwnedHandle,
    read_done: Event,
    write_done: Event,
    closed: Event,
    /// One read and one write at a time: each direction has one event.
    reading: Mutex<()>,
    writing: Mutex<()>,
}

impl PipeIo {
    fn new(pipe: OwnedHandle) -> io::Result<Self> {
        Ok(Self {
            pipe,
            read_done: Event::new()?,
            write_done: Event::new()?,
            closed: Event::new()?,
            reading: Mutex::new(()),
            writing: Mutex::new(()),
        })
    }

    fn handle(&self) -> HANDLE {
        HANDLE(self.pipe.as_raw_handle())
    }

    fn is_closed(&self) -> bool {
        wait_any(&[&self.closed], Some(Instant::now())).is_some()
    }

    /// Run one overlapped operation that `start` begins with the given
    /// OVERLAPPED, until it completes, `deadline` passes or the pipe is
    /// closed; the bytes it transferred.
    fn io(
        &self,
        done: &Event,
        deadline: Option<Instant>,
        start: impl FnOnce(*mut OVERLAPPED) -> windows::core::Result<()>,
    ) -> io::Result<u32> {
        if self.is_closed() {
            return Err(io::ErrorKind::ConnectionAborted.into());
        }
        // SAFETY: the event handle is open while `done` lives.
        let _ = unsafe { ResetEvent(done.handle()) };
        let mut overlapped = OVERLAPPED {
            hEvent: done.handle(),
            ..Default::default()
        };
        match start(&mut overlapped) {
            Ok(()) => {}
            Err(error) if win32_code(&error) == ERROR_IO_PENDING.0 => {}
            Err(error) => return Err(io::Error::from_raw_os_error(win32_code(&error) as i32)),
        }
        let woke = wait_any(&[done, &self.closed], deadline);
        if woke != Some(0) {
            // SAFETY: `overlapped` is the in-flight operation's, on this
            // pipe. A failure means it already completed.
            let _ = unsafe { CancelIoEx(self.handle(), Some(&overlapped)) };
        }
        let mut transferred = 0u32;
        // SAFETY: `overlapped` belongs to an operation started on this
        // pipe; waiting (`true`) returns only once the system no longer
        // uses it, or the buffer the caller lent the operation.
        match unsafe { GetOverlappedResult(self.handle(), &overlapped, &mut transferred, true) } {
            Ok(()) => Ok(transferred),
            Err(error) => Err(match woke {
                None => io::ErrorKind::TimedOut.into(),
                Some(1) => io::ErrorKind::ConnectionAborted.into(),
                _ => io::Error::from_raw_os_error(win32_code(&error) as i32),
            }),
        }
    }
}

impl HelperIo for PipeIo {
    fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        let _one = self.reading.lock().unwrap_or_else(|p| p.into_inner());
        let len = buf.len().min(u32::MAX as usize);
        let pipe = self.handle();
        let result = self.io(&self.read_done, None, |overlapped| {
            // SAFETY: `buf` outlives `io`, which waits until the system is
            // done with it and with `overlapped`.
            unsafe { ReadFile(pipe, Some(&mut buf[..len]), None, Some(overlapped)) }
        });
        match result {
            Ok(n) => Ok(n as usize),
            // The helper closed its end: the end of the stream.
            Err(error)
                if [ERROR_BROKEN_PIPE, ERROR_PIPE_NOT_CONNECTED, ERROR_NO_DATA]
                    .iter()
                    .any(|code| error.raw_os_error() == Some(code.0 as i32)) =>
            {
                Ok(0)
            }
            Err(error) => Err(error),
        }
    }

    fn write_all(&self, mut buf: &[u8], deadline: Instant) -> io::Result<()> {
        let _one = self.writing.lock().unwrap_or_else(|p| p.into_inner());
        let pipe = self.handle();
        while !buf.is_empty() {
            let chunk = &buf[..buf.len().min(u32::MAX as usize)];
            let written = self.io(&self.write_done, Some(deadline), |overlapped| {
                // SAFETY: `chunk` outlives `io`, which waits until the
                // system is done with it and with `overlapped`.
                unsafe { WriteFile(pipe, Some(chunk), None, Some(overlapped)) }
            })?;
            if written == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            buf = &buf[written as usize..];
        }
        Ok(())
    }

    /// Ends every read and write under way and refuses later ones. The
    /// handle itself closes when the last owner drops this, which the
    /// helper sees as the end of the stream.
    fn close(&self) {
        // SAFETY: the event and the pipe are open while `self` lives;
        // cancelling another thread's I/O is what CancelIoEx is for, and
        // each canceled operation's owner still waits for its completion.
        unsafe {
            let _ = SetEvent(self.closed.handle());
            let _ = CancelIoEx(self.handle(), None);
        }
    }
}
