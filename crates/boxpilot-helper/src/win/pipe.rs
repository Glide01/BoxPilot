//! The helper's named pipe (ADR 0006 rule 5): instances, accepting a
//! client, and [`PipeTransport`], the connection's byte stream with
//! deadlines.
//!
//! Every instance is created byte-mode, overlapped (so every read and write
//! can be given up on at a deadline), with `PIPE_REJECT_REMOTE_CLIENTS` (a
//! remotely reachable service pipe was OpenVPN's CVE-2024-24974), and the
//! pipe's security descriptor. The first one also has
//! `FILE_FLAG_FIRST_PIPE_INSTANCE`, which fails if anyone else already holds
//! the name; after that the server always keeps one instance listening, so
//! the name is never free to take.

use super::security::SecurityDescriptor;
use super::sys::{io_error, is_win32, own, pcwstr, raw, wait_events, Event};
use crate::transport::Transport;
use std::io;
use std::os::windows::io::OwnedHandle;
use std::sync::Mutex;
use std::time::Instant;
use windows::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_NO_DATA, ERROR_PIPE_CONNECTED,
    ERROR_PIPE_NOT_CONNECTED, INVALID_HANDLE_VALUE,
};
use windows::Win32::Storage::FileSystem::{
    ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_FLAG_FIRST_PIPE_INSTANCE,
    FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

/// The pipe's DACL: SYSTEM and Administrators full control; interactive
/// users may read and write data, and nothing else. No mandatory label, so
/// the default (medium, no write-up) keeps low-integrity processes out.
///
/// ADR 0006 writes the users' ACE as `GRGW`. `GENERIC_WRITE` on a pipe
/// maps to `FILE_GENERIC_WRITE`, which includes `FILE_APPEND_DATA`, and for
/// a pipe that is `FILE_CREATE_PIPE_INSTANCE`: the right to create another
/// server instance of this pipe, wait for the next client, and impersonate
/// it. So users get exactly `FILE_GENERIC_READ | FILE_WRITE_DATA`
/// (0x0012008B) instead, and the GUI opens the pipe with
/// `GENERIC_READ | FILE_WRITE_DATA`, not `GENERIC_WRITE`.
pub(crate) const PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x12008b;;;IU)";

/// The pipe's DACL in the console seam: the service's, plus full control
/// for the unprivileged user running it, who creates every instance after
/// the first and so needs `FILE_CREATE_PIPE_INSTANCE`. That user is the
/// server there; it lends nothing.
pub(crate) fn console_pipe_sddl(user: &str) -> String {
    format!("{PIPE_SDDL}(A;;GA;;;{user})")
}

/// Each direction's buffer.
const BUFFER: u32 = 64 * 1024;

/// Create an instance of the pipe `name` (NUL-terminated UTF-16). `first`
/// adds `FILE_FLAG_FIRST_PIPE_INSTANCE`: then `ERROR_ACCESS_DENIED` means
/// someone else holds the name.
pub(crate) fn create_instance(
    name: &[u16],
    descriptor: &SecurityDescriptor,
    first: bool,
    max_instances: u32,
) -> io::Result<OwnedHandle> {
    let mut open_mode = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
    if first {
        open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }
    let attributes = descriptor.attributes();
    // SAFETY: `name` is NUL-terminated; `attributes` and the descriptor it
    // points at outlive the call, which copies the descriptor.
    let handle = unsafe {
        CreateNamedPipeW(
            pcwstr(name),
            FILE_FLAGS_AND_ATTRIBUTES(open_mode.0),
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            max_instances,
            BUFFER,
            BUFFER,
            0,
            Some(&attributes),
        )
    };
    if handle == INVALID_HANDLE_VALUE || handle.is_invalid() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a valid pipe handle, just created, owned by nobody else.
    Ok(unsafe { own(handle) })
}

/// How waiting for a client ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Accepted {
    Client,
    Stopped,
    TimedOut,
}

/// Wait for a client on `pipe`, an instance from [`create_instance`],
/// until `stop` is set or `deadline` passes.
pub(crate) fn accept(pipe: &OwnedHandle, stop: &Event, deadline: Instant) -> io::Result<Accepted> {
    let connected = Event::new()?;
    let mut overlapped = OVERLAPPED {
        hEvent: connected.handle(),
        ..Default::default()
    };
    // SAFETY: `pipe` was opened with FILE_FLAG_OVERLAPPED; `overlapped`
    // stays where it is, unmoved, until the operation has completed or been
    // cancelled and waited for below.
    match unsafe { ConnectNamedPipe(raw(pipe), Some(&mut overlapped)) } {
        Ok(()) => return Ok(Accepted::Client),
        Err(error) if is_win32(&error, ERROR_PIPE_CONNECTED) => return Ok(Accepted::Client),
        // A client connected and already closed its end: served like any
        // other, it reads as the end of the stream at once. Treating it as
        // a failure would let anyone wear the accept loop down.
        Err(error) if is_win32(&error, ERROR_NO_DATA) => return Ok(Accepted::Client),
        Err(error) if is_win32(&error, ERROR_IO_PENDING) => {}
        Err(error) => return Err(io_error(error)),
    }
    // Whatever the wait says, even if it failed, the operation is canceled
    // (unless it completed) and waited for before `overlapped` goes away.
    let woke = wait_events(&[&connected, stop], Some(deadline));
    let outcome = finish(pipe, &mut overlapped, !matches!(woke, Ok(Some(0))));
    match (woke, outcome) {
        // Stopping wins over a client that connected at the same moment.
        (Ok(Some(1)), _) => Ok(Accepted::Stopped),
        (Err(error), _) => Err(error),
        (_, Ok(_)) => Ok(Accepted::Client),
        (Ok(None), Err(_)) => Ok(Accepted::TimedOut),
        (_, Err(error)) => Err(error),
    }
}

/// Complete an overlapped operation on `handle`: cancel it first if `cancel`,
/// then wait until the system is done with `overlapped` (and the buffer the
/// operation uses), and return what it transferred.
fn finish(handle: &OwnedHandle, overlapped: &mut OVERLAPPED, cancel: bool) -> io::Result<u32> {
    if cancel {
        // SAFETY: `overlapped` is the in-flight operation's, on `handle`.
        // A failure means it already completed, which the wait below sees.
        let _ = unsafe { CancelIoEx(raw(handle), Some(overlapped)) };
    }
    let mut transferred = 0u32;
    // SAFETY: `overlapped` belongs to an operation started on `handle`;
    // waiting (`true`) returns only once the system no longer uses it.
    unsafe { GetOverlappedResult(raw(handle), overlapped, &mut transferred, true) }
        .map_err(io_error)?;
    Ok(transferred)
}

/// A connected pipe instance as a connection's [`Transport`]. Reads and
/// writes are overlapped, each waited for together with the `closed`
/// event, so a deadline or [`Transport::close`] ends them at once.
pub(crate) struct PipeTransport {
    pipe: OwnedHandle,
    read_done: Event,
    write_done: Event,
    closed: Event,
    /// One read and one write at a time: each direction has one event.
    reading: Mutex<()>,
    writing: Mutex<()>,
}

impl PipeTransport {
    pub(crate) fn new(pipe: OwnedHandle) -> io::Result<Self> {
        Ok(Self {
            pipe,
            read_done: Event::new()?,
            write_done: Event::new()?,
            closed: Event::new()?,
            reading: Mutex::new(()),
            writing: Mutex::new(()),
        })
    }

    pub(crate) fn handle(&self) -> &OwnedHandle {
        &self.pipe
    }

    /// Run one overlapped operation that `start` begins (a ReadFile or
    /// WriteFile on the pipe with the given OVERLAPPED), until it completes,
    /// `deadline` passes, or the transport closes.
    fn io(
        &self,
        done: &Event,
        deadline: Option<Instant>,
        start: impl FnOnce(*mut OVERLAPPED) -> windows::core::Result<()>,
    ) -> io::Result<u32> {
        if self.closed.is_set() {
            return Err(io::ErrorKind::ConnectionAborted.into());
        }
        done.reset();
        let mut overlapped = OVERLAPPED {
            hEvent: done.handle(),
            ..Default::default()
        };
        match start(&mut overlapped) {
            Ok(()) => {}
            Err(error) if is_win32(&error, ERROR_IO_PENDING) => {}
            Err(error) => return Err(io_error(error)),
        }
        let woke = wait_events(&[done, &self.closed], deadline);
        let cancel = !matches!(woke, Ok(Some(0)));
        match finish(&self.pipe, &mut overlapped, cancel) {
            // Completed, even if a deadline or close raced it: what was
            // transferred was transferred.
            Ok(transferred) => Ok(transferred),
            Err(error) => Err(match woke {
                Ok(None) => io::ErrorKind::TimedOut.into(),
                Ok(Some(1)) => io::ErrorKind::ConnectionAborted.into(),
                _ => error,
            }),
        }
    }
}

/// Whether `error` means the client is gone: the end of the stream.
fn is_disconnect(error: &io::Error) -> bool {
    [ERROR_BROKEN_PIPE, ERROR_PIPE_NOT_CONNECTED, ERROR_NO_DATA]
        .iter()
        .any(|code| error.raw_os_error() == Some(code.0 as i32))
}

impl Transport for PipeTransport {
    fn read(&self, buf: &mut [u8], deadline: Option<Instant>) -> io::Result<usize> {
        let _one = self
            .reading
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let len = buf.len().min(u32::MAX as usize);
        let pipe = raw(&self.pipe);
        let result = self.io(&self.read_done, deadline, |overlapped| {
            // SAFETY: `buf` outlives `io`, which waits until the system is
            // done with it and with `overlapped`, even on a deadline.
            unsafe { ReadFile(pipe, Some(&mut buf[..len]), None, Some(overlapped)) }
        });
        match result {
            Ok(n) => Ok(n as usize),
            Err(error) if is_disconnect(&error) => Ok(0),
            Err(error) => Err(error),
        }
    }

    fn write_all(&self, mut buf: &[u8], deadline: Instant) -> io::Result<()> {
        let _one = self
            .writing
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let pipe = raw(&self.pipe);
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

    /// Ends every read and write under way and refuses later ones. The pipe
    /// itself stays connected until the transport is dropped, so a client
    /// still reads what was written before (an `error` reply) and then
    /// sees the end.
    fn close(&self) {
        self.closed.set();
        // SAFETY: the pipe handle is open while `self` lives; cancelling all
        // of its I/O from another thread is what CancelIoEx is for, and each
        // canceled operation's owner still waits for its completion.
        let _ = unsafe { CancelIoEx(raw(&self.pipe), None) };
    }
}
