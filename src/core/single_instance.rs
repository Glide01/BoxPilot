//! Windows single-instance plumbing for the URL scheme.
//!
//! When the browser opens a `sing-box://` / `boxpilot://` link, Windows
//! always launches a **new** `box-pilot.exe` with the URI as argv[1] — it
//! never reuses the running instance. So:
//!
//! 1. Every fresh process first calls [`try_forward`] **before** the
//!    elevation check in `main`: if a primary instance is already listening
//!    on the named pipe, the URI is handed over and the new process exits —
//!    no UAC prompt at all on the common path.
//! 2. The (elevated) process that finds no pipe becomes the primary: it
//!    takes the instance mutex and runs the pipe server thread, feeding
//!    received URIs into the UI via the callback (`main` wires it to a
//!    futures channel drained by `AppState`).
//!
//! DACL note: the pipe carries an explicit SDDL security descriptor
//! (`D:(A;;GRGW;;;WD)` + low-integrity label). The default DACL of an
//! elevated process's token grants access to BUILTIN\Administrators and
//! SYSTEM only — and in the browser-spawned *non-elevated* sender the
//! Administrators group is deny-only, so with default security the forward
//! would fail with ERROR_ACCESS_DENIED. World-writable is fine here: the
//! pipe only carries import-link strings, and every import goes through an
//! explicit user confirmation dialog before anything is fetched.
//!
//! All of this is `#[cfg(target_os = "windows")]`; other platforms get
//! no-op stubs so `main` can call unconditionally. None of it can be
//! compile-checked on macOS (see CLAUDE.md) — CI's MSVC build is the
//! verifier.

#[cfg(target_os = "windows")]
const PIPE_PATH: &str = r"\\.\pipe\BoxPilot.DeepLink";
/// Session-local (not `Global\`) on purpose: every BoxPilot instance is
/// launched from the interactive user session, and the local namespace
/// avoids cross-IL ACL surprises on the mutex itself.
#[cfg(target_os = "windows")]
const MUTEX_NAME: &str = "BoxPilot.SingleInstance";

pub enum ServerStart {
    /// We own the instance mutex; the pipe server thread is running.
    Primary,
    /// Another primary won the race (two cold starts at once). Caller
    /// should exit.
    LostRace,
}

/// If a primary instance is already listening, hand it `uri` (empty string
/// = plain second launch, primary ignores it) and return `true` — the
/// caller must then exit. Returns `false` when no instance is running.
#[cfg(target_os = "windows")]
pub fn try_forward(uri: Option<&str>) -> bool {
    use std::io::Write;

    let payload = uri.unwrap_or("");
    for _attempt in 0..5 {
        match std::fs::OpenOptions::new().write(true).open(PIPE_PATH) {
            Ok(mut pipe) => {
                let _ = pipe.write_all(payload.as_bytes());
                let _ = pipe.flush();
                return true;
            }
            // No pipe ⇒ no running instance ⇒ we should start up normally.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return false,
            // Anything else (ERROR_PIPE_BUSY between server accepts, or a
            // transient state) — retry briefly.
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(120)),
        }
    }
    // The pipe exists but never let us in. Treat it as "an instance is
    // running" anyway: a duplicate primary would fight over sing-box and
    // the system proxy, which is worse than a dropped import link.
    eprintln!("Deep-link pipe exists but is unreachable; exiting duplicate instance.");
    true
}

#[cfg(not(target_os = "windows"))]
pub fn try_forward(_uri: Option<&str>) -> bool {
    false
}

/// Claim the single-instance mutex and start the pipe server thread.
/// Call only after elevation (the primary must be the elevated process).
/// `on_message` is invoked on the pipe thread for every received payload —
/// it must be cheap and thread-safe (main wires it to a channel send).
#[cfg(target_os = "windows")]
pub fn start_server(on_message: Box<dyn Fn(String) + Send>) -> ServerStart {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;

    let mutex_name: Vec<u16> = MUTEX_NAME.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        match CreateMutexW(None, false, PCWSTR::from_raw(mutex_name.as_ptr())) {
            // The handle is intentionally leaked: the mutex must live
            // exactly as long as the process.
            Ok(_handle) => {
                if GetLastError() == ERROR_ALREADY_EXISTS {
                    return ServerStart::LostRace;
                }
            }
            // Couldn't even query the mutex — assume someone else owns the
            // role rather than risk a duplicate primary.
            Err(_) => return ServerStart::LostRace,
        }
    }

    std::thread::spawn(move || pipe_server_loop(on_message));
    ServerStart::Primary
}

#[cfg(not(target_os = "windows"))]
pub fn start_server(_on_message: Box<dyn Fn(String) + Send>) -> ServerStart {
    ServerStart::Primary
}

/// Blocking accept loop, one client at a time. A client connects, writes
/// one URI, closes; we read to EOF and pass the payload on. Sequential
/// accepts are plenty — deep links are human-paced.
#[cfg(target_os = "windows")]
fn pipe_server_loop(on_message: Box<dyn Fn(String) + Send>) {
    use windows::core::{w, HRESULT, PCWSTR};
    use windows::Win32::Foundation::{CloseHandle, ERROR_PIPE_CONNECTED};
    use windows::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    // ReadFile/ConnectNamedPipe 走 `Win32_System_IO` feature;
    // PIPE_ACCESS_INBOUND(FILE_FLAGS_AND_ATTRIBUTES)定义在 FileSystem,不在 Pipes。
    use windows::Win32::Storage::FileSystem::{ReadFile, PIPE_ACCESS_INBOUND};
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
        PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };

    const SDDL_REVISION_1: u32 = 1;

    // Allow Everyone read/write + low-integrity label, so the non-elevated
    // browser-spawned sender can reach this elevated server (see module
    // docs). The descriptor is intentionally never freed: it must outlive
    // every CreateNamedPipeW call and this thread runs until process exit.
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    let security_attributes = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            w!("D:(A;;GRGW;;;WD)S:(ML;;NW;;;LW)"),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
        .ok()
        .map(|()| SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: false.into(),
        })
    };
    if security_attributes.is_none() {
        // Degraded mode: the pipe still works for elevated senders (the
        // LostRace forward); browser-spawned imports will be refused.
        eprintln!("Failed to build pipe security descriptor; deep links from the browser may not reach this instance.");
    }

    let pipe_name: Vec<u16> = PIPE_PATH.encode_utf16().chain(std::iter::once(0)).collect();
    loop {
        let pipe = unsafe {
            CreateNamedPipeW(
                PCWSTR::from_raw(pipe_name.as_ptr()),
                PIPE_ACCESS_INBOUND,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                0,
                4096,
                0,
                security_attributes
                    .as_ref()
                    .map(|sa| sa as *const SECURITY_ATTRIBUTES),
            )
        };
        if pipe.is_invalid() {
            // Name taken or resources exhausted; don't spin.
            std::thread::sleep(std::time::Duration::from_secs(1));
            continue;
        }

        // ERROR_PIPE_CONNECTED = the client connected between create and
        // this call; that's a success for our purposes.
        let connected = match unsafe { ConnectNamedPipe(pipe, None) } {
            Ok(()) => true,
            Err(e) => e.code() == HRESULT::from_win32(ERROR_PIPE_CONNECTED.0),
        };

        if connected {
            let mut data = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let mut read: u32 = 0;
                match unsafe { ReadFile(pipe, Some(&mut buf), Some(&mut read), None) } {
                    Ok(()) if read > 0 => data.extend_from_slice(&buf[..read as usize]),
                    // 0-byte read or broken pipe — client is done.
                    _ => break,
                }
            }
            let _ = unsafe { DisconnectNamedPipe(pipe) };
            if let Ok(text) = String::from_utf8(data) {
                on_message(text.trim().to_string());
            }
        }
        unsafe {
            let _ = CloseHandle(pipe);
        }
    }
}
