//! The SCM entry: `StartServiceCtrlDispatcherW`, the service's main and its
//! control handler, and status reporting. The service is demand-start (the
//! MSI installs it so); it stops itself when idle, and on `stop` or system
//! shutdown stops sing-box first.
//!
//! Hand-rolled on the `windows` crate rather than the `windows-service`
//! crate: it is a few small calls, and a new dependency in a SYSTEM process
//! is more to review than they are.

use super::folders;
use super::own_privileges;
use super::pipe::PIPE_SDDL;
use super::security::protected_dir_sddl;
use super::server;
use super::supervisor::Setup;
use super::sys::{is_win32, pcwstr, wide, Event};
use crate::acl::Trusted;
use crate::cli::{SERVICE_NAME, SERVICE_PIPE, USAGE};
use crate::exit;
use crate::helper_log;
use crate::paths::Layout;
use crate::tokenplan;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicUsize, Ordering};
use std::sync::OnceLock;
use windows::core::PWSTR;
use windows::Win32::Foundation::{
    ERROR_CALL_NOT_IMPLEMENTED, ERROR_FAILED_SERVICE_CONTROLLER_CONNECT,
    ERROR_SERVICE_SPECIFIC_ERROR, NO_ERROR,
};
use windows::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SetServiceStatus, StartServiceCtrlDispatcherW,
    SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP, SERVICE_CONTROL_INTERROGATE,
    SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_RUNNING, SERVICE_START_PENDING,
    SERVICE_STATUS, SERVICE_STATUS_CURRENT_STATE, SERVICE_STATUS_HANDLE, SERVICE_STOPPED,
    SERVICE_STOP_PENDING, SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS,
};

/// How long a pending state may take, as the SCM is told.
const WAIT_HINT_MS: u32 = 30_000;

/// Set by the control handler; the accept loop stops on it.
static STOP: OnceLock<Event> = OnceLock::new();
/// The status handle, as an integer (a raw handle is neither Send nor
/// Sync). 0 until registered.
static STATUS: AtomicUsize = AtomicUsize::new(0);
static CHECKPOINT: AtomicU32 = AtomicU32::new(0);
static EXIT_CODE: AtomicI32 = AtomicI32::new(exit::OK);

/// Hand the process to the SCM; returns once the service has stopped.
pub(crate) fn run() -> i32 {
    let Ok(mut name) = wide(SERVICE_NAME) else {
        return exit::INTERNAL;
    };
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR(name.as_mut_ptr()),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW::default(),
    ];
    // SAFETY: `table` ends with a null entry, and it and `name` outlive the
    // call, which returns only once the service has stopped.
    match unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } {
        Ok(()) => EXIT_CODE.load(Ordering::SeqCst),
        Err(error) if is_win32(&error, ERROR_FAILED_SERVICE_CONTROLLER_CONNECT) => {
            eprintln!("boxpilot-helper: not started by the Service Control Manager\n{USAGE}");
            exit::USAGE
        }
        Err(error) => {
            eprintln!("boxpilot-helper: the service dispatcher failed: {error}");
            exit::INTERNAL
        }
    }
}

/// The service's main, on the dispatcher's thread for it.
extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    let code = serve();
    EXIT_CODE.store(code, Ordering::SeqCst);
    report(SERVICE_STOPPED, code);
}

fn serve() -> i32 {
    let stop = match Event::new() {
        Ok(event) => STOP.get_or_init(|| event),
        Err(_) => return exit::INTERNAL,
    };
    let Ok(name) = wide(SERVICE_NAME) else {
        return exit::INTERNAL;
    };
    // SAFETY: `name` is NUL-terminated for the call; `control` is a plain
    // function that lives as long as the process; no context pointer.
    let status = match unsafe { RegisterServiceCtrlHandlerExW(pcwstr(&name), Some(control), None) }
    {
        Ok(status) => status,
        Err(_) => return exit::INTERNAL,
    };
    STATUS.store(status.0 as usize, Ordering::SeqCst);
    report(SERVICE_START_PENDING, exit::OK);
    // Before anything else, whatever the SCM gave it: every privilege but
    // the few it needs, gone for good (ADR 0006, "Defense in depth").
    if let Err(error) = own_privileges::keep_only(&tokenplan::HELPER_TOKEN) {
        helper_log!("refusing to run: dropping its own privileges: {error}");
        return exit::PRIVILEGES_REFUSED;
    }
    let setup = match setup() {
        Ok(setup) => setup,
        Err((code, message)) => {
            helper_log!("refusing to run: {message}");
            return code;
        }
    };
    server::run(setup, SERVICE_PIPE, PIPE_SDDL, stop, || {
        report(SERVICE_RUNNING, exit::OK)
    })
}

/// The service's fixed trees (`Layout::installed`): the helper's own
/// directory, which must be `%ProgramFiles%\BoxPilot\Helper` (ADR 0006
/// rule 7: never a folder a user picked), and the private state directory
/// beside it, `%ProgramFiles%\BoxPilot\HelperState`.
fn setup() -> Result<Setup, (i32, String)> {
    let internal = |what: &str, error: std::io::Error| (exit::INTERNAL, format!("{what}: {error}"));
    let exe = std::env::current_exe().map_err(|error| internal("the helper's path", error))?;
    let helper_dir = exe
        .parent()
        .ok_or((
            exit::HELPER_DIR_REFUSED,
            "the helper has no directory".to_owned(),
        ))?
        .to_owned();
    let installed = Layout::installed(
        &folders::program_files().map_err(|error| internal("Program Files", error))?,
    );
    let expected = installed.helper_dir();
    if !same_path(&helper_dir, expected) {
        return Err((
            exit::HELPER_DIR_REFUSED,
            format!(
                "the helper runs from {}, not from {}",
                helper_dir.display(),
                expected.display()
            ),
        ));
    }
    Ok(Setup {
        layout: Layout::new(helper_dir, installed.state_dir().to_owned()),
        trusted: Trusted::administrators(),
        dir_sddl: protected_dir_sddl(None),
        clean_adapters: true,
        own_exe: Some(exe),
    })
}

/// Whether two absolute paths name the same place, as Windows compares
/// them: ignoring case and a trailing separator. A difference beyond that
/// (a short `PROGRA~1` name, a `\\?\` prefix) refuses rather than guesses.
fn same_path(a: &Path, b: &Path) -> bool {
    let text = |path: &Path| -> PathBuf {
        PathBuf::from(path.to_string_lossy().trim_end_matches('\\').to_lowercase())
    };
    text(a) == text(b)
}

/// The control handler, on the dispatcher's main thread.
extern "system" fn control(
    control: u32,
    _event_type: u32,
    _event_data: *mut c_void,
    _context: *mut c_void,
) -> u32 {
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            report(SERVICE_STOP_PENDING, exit::OK);
            if let Some(stop) = STOP.get() {
                stop.set();
            }
            NO_ERROR.0
        }
        SERVICE_CONTROL_INTERROGATE => NO_ERROR.0,
        _ => ERROR_CALL_NOT_IMPLEMENTED.0,
    }
}

/// Tell the SCM the service's state. A non-zero `code` on `SERVICE_STOPPED`
/// becomes the service-specific exit code (`crate::exit`).
fn report(state: SERVICE_STATUS_CURRENT_STATE, code: i32) {
    let handle = STATUS.load(Ordering::SeqCst);
    if handle == 0 {
        return;
    }
    let pending = state == SERVICE_START_PENDING || state == SERVICE_STOP_PENDING;
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: if state == SERVICE_RUNNING {
            SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN
        } else {
            0
        },
        dwWin32ExitCode: if code == exit::OK {
            NO_ERROR.0
        } else {
            ERROR_SERVICE_SPECIFIC_ERROR.0
        },
        dwServiceSpecificExitCode: code as u32,
        dwCheckPoint: if pending {
            CHECKPOINT.fetch_add(1, Ordering::SeqCst) + 1
        } else {
            0
        },
        dwWaitHint: if pending { WAIT_HINT_MS } else { 0 },
    };
    // SAFETY: the handle came from RegisterServiceCtrlHandlerExW and stays
    // valid until SERVICE_STOPPED, the last status reported; `status` is a
    // valid SERVICE_STATUS for the call.
    let _ = unsafe { SetServiceStatus(SERVICE_STATUS_HANDLE(handle as *mut c_void), &status) };
}
