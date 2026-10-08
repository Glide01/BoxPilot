//! Starting sing-box (ADR 0006 rules 2 and 3), as SYSTEM, with nothing it
//! didn't need:
//!
//! - `CreateProcessW` with the full application path, so nothing is
//!   searched for, and sing-box's command line and environment from
//!   `spawnplan`, built from nothing rather than inherited;
//! - `CREATE_SUSPENDED`, so it runs no instruction before it is in its job;
//!   `CREATE_NO_WINDOW`; `CREATE_UNICODE_ENVIRONMENT`;
//! - `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` naming only the write ends of its
//!   stdout and stderr pipes: no other handle of the helper's is inherited,
//!   and stdin is none;
//! - a job object with `KILL_ON_JOB_CLOSE` (if the helper dies, sing-box
//!   dies with it), `DIE_ON_UNHANDLED_EXCEPTION`, and an active-process
//!   limit of 1, so sing-box can't start a program. It is assigned before
//!   the thread is resumed.

use super::sys::{io_error, own, pcwstr, raw, wide};
use crate::spawnplan;
use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::marker::PhantomData;
use std::mem::{size_of, size_of_val};
use std::os::windows::io::OwnedHandle;
use std::path::Path;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{SetHandleInformation, HANDLE, HANDLE_FLAGS, HANDLE_FLAG_INHERIT};
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
    InitializeProcThreadAttributeList, ResumeThread, TerminateProcess, UpdateProcThreadAttribute,
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW,
};

/// The exit code sing-box gets when the helper stops it.
pub(crate) const STOPPED_EXIT_CODE: u32 = 1;

/// What to start.
pub(crate) struct Launch<'a> {
    /// sing-box's full path, in the verified helper directory.
    pub(crate) program: &'a Path,
    pub(crate) args: Vec<String>,
    /// Its working directory: the run directory.
    pub(crate) cwd: &'a Path,
    /// Its whole environment (`spawnplan::environment`).
    pub(crate) environment: Vec<(String, String)>,
}

/// A started sing-box.
pub(crate) struct Child {
    pub(crate) process: OwnedHandle,
    pub(crate) job: Job,
    pub(crate) stdout: File,
    pub(crate) stderr: File,
}

/// The job sing-box runs in.
pub(crate) struct Job(OwnedHandle);

impl Job {
    fn new() -> io::Result<Self> {
        // SAFETY: no attributes and no name: an unnamed job that only this
        // handle reaches.
        let handle = unsafe { CreateJobObjectW(None, PCWSTR::null()) }.map_err(io_error)?;
        // SAFETY: the job was just created; nothing else owns its handle.
        let job = Self(unsafe { own(handle) });
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION
            | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
        limits.BasicLimitInformation.ActiveProcessLimit = 1;
        // SAFETY: `limits` is a JOBOBJECT_EXTENDED_LIMIT_INFORMATION of the
        // size passed, alive for the call.
        unsafe {
            SetInformationJobObject(
                raw(&job.0),
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast::<c_void>(),
                size_of_val(&limits) as u32,
            )
        }
        .map_err(io_error)?;
        Ok(job)
    }

    fn assign(&self, process: &OwnedHandle) -> io::Result<()> {
        // SAFETY: both handles are open for the call.
        unsafe { AssignProcessToJobObject(raw(&self.0), raw(process)) }.map_err(io_error)
    }

    /// End every process in the job. Harmless when none is left.
    pub(crate) fn terminate(&self) {
        // SAFETY: the job handle is open while `self` lives.
        let _ = unsafe { TerminateJobObject(raw(&self.0), STOPPED_EXIT_CODE) };
    }
}

/// An anonymous pipe: its read end for the helper, its write end
/// inheritable, for sing-box's stdout or stderr.
fn output_pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: true.into(),
    };
    let (mut read, mut write) = (HANDLE::default(), HANDLE::default());
    // SAFETY: the out-pointers are valid; `attributes` outlives the call.
    unsafe { CreatePipe(&mut read, &mut write, Some(&attributes), 0) }.map_err(io_error)?;
    // SAFETY: CreatePipe returned two new handles that nothing else owns.
    let (read, write) = unsafe { (own(read), own(write)) };
    // SAFETY: `read` is open; this only clears its inherit flag.
    unsafe { SetHandleInformation(raw(&read), HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0)) }
        .map_err(io_error)?;
    Ok((read, write))
}

/// A `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` attribute list. It points at the
/// handle array it was built from, so it borrows it.
struct HandleList<'a> {
    buf: Vec<u64>,
    _handles: PhantomData<&'a [HANDLE]>,
}

impl<'a> HandleList<'a> {
    fn new(handles: &'a [HANDLE]) -> io::Result<Self> {
        let mut size = 0usize;
        // SAFETY: a size query with no list; it fails with
        // ERROR_INSUFFICIENT_BUFFER by design and sets `size`.
        let _ = unsafe {
            InitializeProcThreadAttributeList(
                LPPROC_THREAD_ATTRIBUTE_LIST::default(),
                1,
                0,
                &mut size,
            )
        };
        if size == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buf = vec![0u64; size.div_ceil(size_of::<u64>())];
        let list = LPPROC_THREAD_ATTRIBUTE_LIST(buf.as_mut_ptr().cast::<c_void>());
        // SAFETY: `buf` holds `size` writable, 8-aligned bytes, and its heap
        // allocation stays put for as long as the list lives.
        unsafe { InitializeProcThreadAttributeList(list, 1, 0, &mut size) }.map_err(io_error)?;
        let initialized = Self {
            buf,
            _handles: PhantomData,
        };
        // SAFETY: the list is initialized; `handles` outlives it (the
        // borrow `'a`), as the attribute keeps a pointer to it.
        unsafe {
            UpdateProcThreadAttribute(
                initialized.list(),
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                Some(handles.as_ptr().cast::<c_void>()),
                size_of_val(handles),
                None,
                None,
            )
        }
        .map_err(io_error)?;
        Ok(initialized)
    }

    fn list(&self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        LPPROC_THREAD_ATTRIBUTE_LIST(self.buf.as_ptr() as *mut c_void)
    }
}

impl Drop for HandleList<'_> {
    fn drop(&mut self) {
        // SAFETY: the list was initialized in `new` and is deleted once.
        unsafe { DeleteProcThreadAttributeList(self.list()) };
    }
}

/// Start sing-box suspended, put it in its job, then let it run. Every
/// verified file must stay open (by the caller) until this returns.
pub(crate) fn spawn(launch: &Launch<'_>) -> io::Result<Child> {
    let program = launch
        .program
        .to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "sing-box's path"))?;
    let application = wide(launch.program)?;
    let line = spawnplan::command_line(program, &launch.args)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let mut command_line = wide(&line)?;
    let environment = spawnplan::environment_block(&launch.environment)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let cwd = wide(launch.cwd)?;
    let (stdout, stdout_write) = output_pipe()?;
    let (stderr, stderr_write) = output_pipe()?;
    let job = Job::new()?;

    let inherited = [raw(&stdout_write), raw(&stderr_write)];
    let handle_list = HandleList::new(&inherited)?;
    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = HANDLE::default();
    startup.StartupInfo.hStdOutput = inherited[0];
    startup.StartupInfo.hStdError = inherited[1];
    startup.lpAttributeList = handle_list.list();
    let mut info = PROCESS_INFORMATION::default();
    // SAFETY: every string is NUL-terminated and outlives the call;
    // `command_line` is writable, as CreateProcessW requires; the
    // environment block is double-NUL-terminated UTF-16, as
    // CREATE_UNICODE_ENVIRONMENT says; `startup` is a STARTUPINFOEXW whose
    // `cb` says so (EXTENDED_STARTUPINFO_PRESENT), and its attribute list
    // and the handles it names are alive; `info` receives two new handles.
    unsafe {
        CreateProcessW(
            pcwstr(&application),
            PWSTR(command_line.as_mut_ptr()),
            None,
            None,
            true,
            CREATE_SUSPENDED
                | CREATE_NO_WINDOW
                | CREATE_UNICODE_ENVIRONMENT
                | EXTENDED_STARTUPINFO_PRESENT,
            Some(environment.as_ptr().cast::<c_void>()),
            pcwstr(&cwd),
            // The whole STARTUPINFOEXW, which begins with its STARTUPINFOW.
            (&startup as *const STARTUPINFOEXW).cast::<STARTUPINFOW>(),
            &mut info,
        )
    }
    .map_err(io_error)?;
    // SAFETY: CreateProcessW returned these two new handles, owned by
    // nobody else.
    let (process, thread) = unsafe { (own(info.hProcess), own(info.hThread)) };
    drop(handle_list);
    // Only sing-box holds the write ends now: its exit ends the readers.
    drop(stdout_write);
    drop(stderr_write);

    if let Err(error) = job.assign(&process) {
        terminate(&process);
        return Err(error);
    }
    // SAFETY: `thread` is the suspended main thread, open for the call.
    if unsafe { ResumeThread(raw(&thread)) } == u32::MAX {
        let error = io::Error::last_os_error();
        job.terminate();
        return Err(error);
    }
    Ok(Child {
        process,
        job,
        stdout: File::from(stdout),
        stderr: File::from(stderr),
    })
}

fn terminate(process: &OwnedHandle) {
    // SAFETY: `process` is open for the call.
    let _ = unsafe { TerminateProcess(raw(process), STOPPED_EXIT_CODE) };
}

/// The exit code of a process that has exited.
pub(crate) fn exit_code(process: &OwnedHandle) -> io::Result<u32> {
    let mut code = 0u32;
    // SAFETY: `process` is open for the call; `code` is a valid out-pointer.
    unsafe { GetExitCodeProcess(raw(process), &mut code) }.map_err(io_error)?;
    Ok(code)
}
