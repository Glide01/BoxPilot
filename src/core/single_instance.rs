//! Single-instance plumbing for the URL scheme (Windows, Linux, macOS).
//!
//! When the browser opens a `sing-box://` / `boxpilot://` link on Windows or
//! Linux, the OS always launches a **new** BoxPilot process with the URI as
//! argv[1] — it never reuses the running instance (Windows' shell handler
//! and Linux's `xdg-open` behave the same here). macOS doesn't: link clicks
//! and Dock / Finder launches reach the running app as Apple events (`main`
//! forwards those itself), so only a second process started from a terminal
//! (or `open -n`) goes through here. So:
//!
//! 1. Every fresh process first calls [`try_forward`]: if a primary
//!    instance is already listening, the URI is handed over and the new
//!    process exits.
//! 2. The process that finds no listener becomes the primary: it takes the
//!    instance lock and runs the server thread, feeding received payloads
//!    into the UI as [`LaunchAttempt`]s via the callback (`main` wires it to
//!    a futures channel drained by `AppState`). The empty-string "no URI"
//!    sentinel is a wire-protocol detail and is decoded away here — see
//!    [`LaunchAttempt::from_wire`].
//!
//! The wire format is the same on both platforms: the client connects,
//! writes the URI (or nothing, for a plain launch) and closes; the server
//! reads to EOF.
//!
//! **Windows** — a named pipe plus a session-local mutex.
//!
//! Pipe names are machine-wide, so the name carries the session
//! (`BoxPilot.DeepLink.<session id>`): another account, in another session
//! (fast user switching, Remote Desktop), runs a BoxPilot of its own without
//! meeting this one. Any process may still make a pipe of that name first,
//! so the primary makes its first instance with
//! `FILE_FLAG_FIRST_PIPE_INSTANCE` (never joining a pipe someone else made)
//! and always has the next instance open before it closes the current one:
//! the name is never free while it runs. A sender writes a link, which may
//! carry a subscription's token, only to a server in its own session.
//!
//! The pipe carries an explicit DACL: read/write for the current user's own
//! SID (from the process token) and full control for SYSTEM, nobody else.
//! BoxPilot never elevates itself any more (ADR 0006), but the user may run
//! it as Administrator; the token's user SID is the same either way, so a
//! browser-spawned, non-elevated sender reaches an elevated primary too,
//! which the default DACL of an elevated token (Administrators + SYSTEM)
//! would refuse. No `Everyone`, no low-integrity label. The pipe also
//! refuses remote clients (`PIPE_REJECT_REMOTE_CLIENTS`), and one client may
//! send at most [`MAX_PAYLOAD`] bytes: more is dropped, and the attempt
//! still surfaces the window (ADR 0001). The sender opens it with an
//! identification-only security QoS, so whatever holds the name can learn
//! who forwarded a link, never act as them.
//!
//! **Linux and macOS** — a Unix socket plus an `flock` on a lock file, both
//! in `$XDG_RUNTIME_DIR`, or else in a `boxpilot-<uid>` directory in the
//! temp dir (`$TMPDIR`, which macOS makes per-user, else `/tmp`): made
//! owner-only, and used only if this user owns it, since in a shared temp
//! dir another user can make the name first.
//! The lock, not the socket, decides who is primary: a crashed instance
//! leaves its socket file behind, but the kernel drops its lock. The winner
//! deletes any stale socket, binds a fresh one and makes it owner-only
//! (0600). No elevation is involved, so sender and server always run as the
//! same user.
//!
//! Other platforms get no-op stubs so `main` can call unconditionally. The
//! Windows half can't be compile-checked off Windows (see CLAUDE.md) — CI's
//! MSVC build is the verifier.

use crate::core::deeplink::LaunchAttempt;

/// The most one client may send through the Windows pipe: an import link is
/// a few hundred bytes, and a sender gets no more of this process's memory.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
const MAX_PAYLOAD: usize = 64 * 1024;

/// The launch attempt one Windows pipe client made. Whatever it sent, it
/// is an attempt and surfaces the window (ADR 0001): a payload over
/// [`MAX_PAYLOAD`] (`overflowed`) or one that isn't UTF-8 is dropped, and
/// counts as a plain launch. Pure so it is tested on every platform.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn attempt_from_payload(data: &[u8], overflowed: bool) -> LaunchAttempt {
    if overflowed {
        return LaunchAttempt::Plain;
    }
    match std::str::from_utf8(data) {
        Ok(text) => LaunchAttempt::from_wire(text),
        Err(_) => LaunchAttempt::Plain,
    }
}

/// The pipe's name for this session (see the module docs).
#[cfg(target_os = "windows")]
fn pipe_path() -> String {
    match current_session_id() {
        Some(session) => format!(r"\\.\pipe\BoxPilot.DeepLink.{session}"),
        None => r"\\.\pipe\BoxPilot.DeepLink".to_string(),
    }
}

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
/// = plain second launch, which surfaces the primary's window) and return
/// `true` — the caller must then exit. Returns `false` when no instance is
/// running.
#[cfg(target_os = "windows")]
pub fn try_forward(uri: Option<&str>) -> bool {
    try_forward_to(&pipe_path(), uri)
}

#[cfg(target_os = "windows")]
fn try_forward_to(pipe_path: &str, uri: Option<&str>) -> bool {
    use std::io::Write;
    use std::os::windows::fs::OpenOptionsExt;
    use windows::Win32::Storage::FileSystem::SECURITY_IDENTIFICATION;

    let payload = uri.unwrap_or("");
    for _attempt in 0..5 {
        // Identification only: a process that took the pipe name can't
        // impersonate this sender.
        match std::fs::OpenOptions::new()
            .write(true)
            .security_qos_flags(SECURITY_IDENTIFICATION.0)
            .open(pipe_path)
        {
            // Another session's process made the name: it isn't this
            // session's BoxPilot, and gets no link. Start up normally.
            Ok(pipe) if !served_from_this_session(&pipe) => {
                eprintln!(
                    "The deep-link pipe is served from another session; not forwarding to it."
                );
                return false;
            }
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

#[cfg(unix)]
pub fn try_forward(uri: Option<&str>) -> bool {
    unix::try_forward_at(&unix::InstancePaths::from_env(), uri)
}

#[cfg(not(any(target_os = "windows", unix)))]
pub fn try_forward(_uri: Option<&str>) -> bool {
    false
}

/// Claim the single-instance mutex and start the pipe server thread.
/// `on_attempt` is invoked on the pipe thread for every received payload —
/// it must be cheap and thread-safe (main wires it to a channel send).
#[cfg(target_os = "windows")]
pub fn start_server(on_attempt: Box<dyn Fn(LaunchAttempt) + Send>) -> ServerStart {
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

    let pipe_path = pipe_path();
    std::thread::spawn(move || pipe_server_loop(&pipe_path, on_attempt));
    ServerStart::Primary
}

#[cfg(unix)]
pub fn start_server(on_attempt: Box<dyn Fn(LaunchAttempt) + Send>) -> ServerStart {
    unix::start_server_at(&unix::InstancePaths::from_env(), on_attempt)
}

#[cfg(not(any(target_os = "windows", unix)))]
pub fn start_server(_on_attempt: Box<dyn Fn(LaunchAttempt) + Send>) -> ServerStart {
    ServerStart::Primary
}

/// The pipe's SDDL: read/write for `user_sid` (the current user's SID
/// string), full control for SYSTEM, protected from inheritance; no label.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn pipe_sddl(user_sid: &str) -> String {
    format!("D:P(A;;GRGW;;;{user_sid})(A;;GA;;;SY)")
}

/// Whether `pipe`'s server runs in this process's session (see the module
/// docs). `false` if either can't be read.
#[cfg(target_os = "windows")]
fn served_from_this_session(pipe: &std::fs::File) -> bool {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Pipes::GetNamedPipeServerSessionId;

    let mut server = 0u32;
    // SAFETY: an open pipe handle, valid for the call; `server` receives
    // the server's session id.
    if unsafe { GetNamedPipeServerSessionId(HANDLE(pipe.as_raw_handle()), &mut server) }.is_err() {
        return false;
    }
    current_session_id() == Some(server)
}

/// This process's session, from its token.
#[cfg(target_os = "windows")]
fn current_session_id() -> Option<u32> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Security::{GetTokenInformation, TokenSessionId, TOKEN_QUERY};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token = HANDLE::default();
    // SAFETY: the pseudo-handle of this process; `token` receives a new
    // handle, owned right after.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.ok()?;
    // SAFETY: a valid token handle, just opened, owned by nobody else.
    let token = unsafe { OwnedHandle::from_raw_handle(token.0) };
    let mut session = 0u32;
    let mut size = 0u32;
    // SAFETY: `session` is the DWORD TokenSessionId fills, and outlives
    // the call.
    unsafe {
        GetTokenInformation(
            HANDLE(token.as_raw_handle()),
            TokenSessionId,
            Some((&mut session as *mut u32).cast()),
            std::mem::size_of::<u32>() as u32,
            &mut size,
        )
    }
    .ok()?;
    Some(session)
}

/// The current user's SID as a string (`S-1-5-21-…`), from the process
/// token: the same user whether or not the token is elevated.
#[cfg(target_os = "windows")]
fn current_user_sid() -> Option<String> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows::core::PWSTR;
    use windows::Win32::Foundation::{LocalFree, HANDLE, HLOCAL};
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token = HANDLE::default();
    // SAFETY: the pseudo-handle of this process; `token` receives a new
    // handle, owned right after.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.ok()?;
    // SAFETY: a valid token handle, just opened, owned by nobody else.
    let token = unsafe { OwnedHandle::from_raw_handle(token.0) };
    let token = HANDLE(token.as_raw_handle());
    let mut size = 0u32;
    // SAFETY: a size probe: no buffer, `size` receives what is needed.
    let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut size) };
    // u64s, so the TOKEN_USER at its start is aligned.
    let mut buf = vec![0u64; (size as usize).div_ceil(8)];
    // SAFETY: `buf` holds at least `size` bytes.
    unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr().cast()),
            size,
            &mut size,
        )
    }
    .ok()?;
    // SAFETY: GetTokenInformation wrote a TOKEN_USER (and the SID it points
    // into) at the start of the aligned `buf`, which outlives this read.
    let sid = unsafe { (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut text = PWSTR::null();
    // SAFETY: a valid SID in `buf`; `text` receives a LocalAlloc'd string,
    // read once and freed.
    unsafe {
        ConvertSidToStringSidW(sid, &mut text).ok()?;
        let sid = text.to_string().ok();
        let _ = LocalFree(HLOCAL(text.0.cast()));
        sid
    }
}

/// Blocking accept loop on `pipe_path`, one client at a time. A client
/// connects, writes one URI, closes; we read to EOF (at most
/// [`MAX_PAYLOAD`]) and pass the payload on. Sequential accepts are plenty —
/// deep links are human-paced. The name stays this process's throughout
/// (see the module docs).
#[cfg(target_os = "windows")]
fn pipe_server_loop(pipe_path: &str, on_attempt: Box<dyn Fn(LaunchAttempt) + Send>) {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    // PIPE_ACCESS_INBOUND(FILE_FLAGS_AND_ATTRIBUTES)定义在 FileSystem,不在 Pipes。
    use windows::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_INBOUND};
    use windows::Win32::System::Pipes::{
        CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
        PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };

    const SDDL_REVISION_1: u32 = 1;

    // The current user and SYSTEM only (see module docs). The descriptor
    // is intentionally never freed: it must outlive every CreateNamedPipeW
    // call and this thread runs until process exit.
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    let security_attributes = current_user_sid().and_then(|sid| {
        let sddl: Vec<u16> = pipe_sddl(&sid)
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: `sddl` is NUL-terminated; `descriptor` receives a
        // LocalAlloc'd descriptor, kept for the life of the process.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR::from_raw(sddl.as_ptr()),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
        }
        .ok()
        .map(|()| SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: false.into(),
        })
    });
    if security_attributes.is_none() {
        // Degraded mode: the token's default DACL. That still admits this
        // user when BoxPilot isn't elevated; an elevated primary refuses
        // non-elevated senders (browser-launched imports) then.
        eprintln!(
            "Failed to build the deep-link pipe's security descriptor; using the default one."
        );
    }

    let pipe_name: Vec<u16> = pipe_path.encode_utf16().chain(std::iter::once(0)).collect();
    // One instance. `first`: only if the name is free, so this process
    // never joins a pipe someone else made.
    let create = |first: bool| {
        let open_mode = if first {
            PIPE_ACCESS_INBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE
        } else {
            PIPE_ACCESS_INBOUND
        };
        // SAFETY: `pipe_name` is NUL-terminated, and the security
        // attributes (and the descriptor they point to) outlive the call.
        let pipe = unsafe {
            CreateNamedPipeW(
                PCWSTR::from_raw(pipe_name.as_ptr()),
                open_mode,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                0,
                4096,
                0,
                security_attributes
                    .as_ref()
                    .map(|sa| sa as *const SECURITY_ATTRIBUTES),
            )
        };
        (!pipe.is_invalid()).then_some(pipe)
    };
    // The instance for the next client, made while the current one is
    // still open: the name is never free between two clients.
    let mut next = None;
    loop {
        let Some(pipe) = next.take().or_else(|| create(true)) else {
            // Another process holds the name (another account's, say) and
            // is never joined, or resources ran out; don't spin.
            std::thread::sleep(std::time::Duration::from_secs(1));
            continue;
        };

        let connected = wait_for_client(pipe);
        next = create(false);

        if connected {
            let (data, overflowed) = read_payload(pipe);
            let _ = unsafe { DisconnectNamedPipe(pipe) };
            on_attempt(attempt_from_payload(&data, overflowed));
        }
        unsafe {
            let _ = CloseHandle(pipe);
        }
    }
}

/// Wait on `pipe`, an instance of the server's, for a client; `false` if
/// none came. One may have come already, between the instance's creation
/// and this call: still connected (`ERROR_PIPE_CONNECTED`), or connected,
/// written and gone (`ERROR_NO_DATA`), as a sender that writes one link and
/// closes does whenever it reaches the next instance while the server is
/// still passing on the previous link. What it wrote is still there to
/// read then.
#[cfg(target_os = "windows")]
fn wait_for_client(pipe: windows::Win32::Foundation::HANDLE) -> bool {
    use windows::core::HRESULT;
    use windows::Win32::Foundation::{ERROR_NO_DATA, ERROR_PIPE_CONNECTED};
    use windows::Win32::System::Pipes::ConnectNamedPipe;

    // SAFETY: a pipe instance handle the caller made and still owns.
    match unsafe { ConnectNamedPipe(pipe, None) } {
        Ok(()) => true,
        Err(e) => [ERROR_PIPE_CONNECTED, ERROR_NO_DATA]
            .into_iter()
            .any(|code| e.code() == HRESULT::from_win32(code.0)),
    }
}

/// What the client on `pipe` sent, read to its end: at most
/// [`MAX_PAYLOAD`] bytes, and whether it sent more (disconnecting drops the
/// rest).
#[cfg(target_os = "windows")]
fn read_payload(pipe: windows::Win32::Foundation::HANDLE) -> (Vec<u8>, bool) {
    // ReadFile 走 `Win32_System_IO` feature。
    use windows::Win32::Storage::FileSystem::ReadFile;

    let mut data = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let mut read: u32 = 0;
        // SAFETY: a connected pipe instance of the caller's; `buf` and
        // `read` outlive this synchronous read.
        match unsafe { ReadFile(pipe, Some(&mut buf), Some(&mut read), None) } {
            Ok(()) if read > 0 => {
                if data.len() + read as usize > MAX_PAYLOAD {
                    return (data, true);
                }
                data.extend_from_slice(&buf[..read as usize]);
            }
            // 0-byte read or broken pipe — client is done.
            _ => return (data, false),
        }
    }
}

/// Linux and macOS backend. Paths are parameters rather than read from the
/// environment inside, so the tests run against a temp dir instead of the
/// real `$XDG_RUNTIME_DIR` (where they would collide with a running
/// BoxPilot).
#[cfg(unix)]
mod unix {
    use super::ServerStart;
    use crate::core::deeplink::LaunchAttempt;
    use std::fs::{File, OpenOptions, TryLockError};
    use std::io::{ErrorKind, Read, Write};
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    /// Where the instance lock and the forwarding socket live.
    #[derive(Debug, Clone, PartialEq)]
    pub(super) struct InstancePaths {
        pub lock: PathBuf,
        pub socket: PathBuf,
        /// In a shared temp dir, the directory both are in, and the uid it
        /// must belong to: made, or checked, before either is used
        /// ([`private_dir`]). `None` in `$XDG_RUNTIME_DIR`, which is
        /// per-user and 0700 already.
        pub private_dir: Option<(PathBuf, u32)>,
    }

    impl InstancePaths {
        pub fn from_env() -> Self {
            // SAFETY: getuid has no preconditions and cannot fail.
            let uid = unsafe { libc::getuid() };
            // `temp_dir` is `$TMPDIR`, else `/tmp`. macOS sets `$TMPDIR` to
            // a per-user dir for GUI and terminal launches alike (it has no
            // `$XDG_RUNTIME_DIR`), so both kinds of launch meet there.
            Self::resolve(
                std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
                &std::env::temp_dir(),
                uid,
            )
        }

        /// `$XDG_RUNTIME_DIR` is per-user and 0700 already, so plain names
        /// suffice there. In a shared temp dir they go in a directory of
        /// this user's own, named with the uid so that two users on one
        /// machine don't fight over one name.
        pub fn resolve(runtime_dir: Option<PathBuf>, temp_dir: &Path, uid: u32) -> Self {
            let (dir, private_dir) = match runtime_dir.filter(|dir| !dir.as_os_str().is_empty()) {
                Some(dir) => (dir, None),
                None => {
                    let dir = temp_dir.join(format!("boxpilot-{uid}"));
                    (dir.clone(), Some((dir, uid)))
                }
            };
            Self {
                lock: dir.join("boxpilot.lock"),
                socket: dir.join("boxpilot.sock"),
                private_dir,
            }
        }
    }

    /// Make `dir` a directory only `uid` may enter, or check that it is
    /// one: not a link, and owned by `uid` (a looser mode of its own is
    /// tightened). In a shared temp dir any user can make the name first;
    /// then nothing of ours goes there.
    fn private_dir(dir: &Path, uid: u32) -> std::io::Result<()> {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt};
        match std::fs::DirBuilder::new().mode(0o700).create(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        let meta = std::fs::symlink_metadata(dir)?;
        if !meta.file_type().is_dir() || meta.uid() != uid {
            return Err(std::io::Error::new(
                ErrorKind::PermissionDenied,
                format!("{} isn't a directory of this user's own", dir.display()),
            ));
        }
        if meta.mode() & 0o077 != 0 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    impl InstancePaths {
        /// [`private_dir`], where there is one to make or check.
        fn prepare(&self) -> std::io::Result<()> {
            match &self.private_dir {
                Some((dir, uid)) => private_dir(dir, *uid),
                None => Ok(()),
            }
        }
    }

    pub(super) fn try_forward_at(paths: &InstancePaths, uri: Option<&str>) -> bool {
        // Not this user's directory: whatever listens there gets no link.
        // Start up normally.
        if let Err(e) = paths.prepare() {
            eprintln!("Not forwarding the launch: {e}.");
            return false;
        }
        let payload = uri.unwrap_or("");
        for _attempt in 0..5 {
            match UnixStream::connect(&paths.socket) {
                Ok(mut stream) => {
                    let _ = stream.write_all(payload.as_bytes());
                    let _ = stream.flush();
                    return true;
                }
                // The lock, not the socket, says whether an instance is
                // running: a crashed one leaves a stale socket file behind
                // (ECONNREFUSED), and a socket that can't be used at all
                // (e.g. a path past the `sun_path` limit: 108 bytes on
                // Linux, 104 on macOS) must not
                // stop the first launch from starting. No lock holder ⇒
                // start up normally.
                Err(_) if !lock_is_held(&paths.lock) => return false,
                // A primary that hasn't bound yet (the LostRace path lands
                // here) — retry briefly.
                Err(_) => std::thread::sleep(Duration::from_millis(120)),
            }
        }
        // Same call as on Windows: a primary exists but never let us in.
        // A duplicate primary would fight over sing-box and the system
        // proxy, which is worse than a dropped import link.
        eprintln!("Deep-link socket is unreachable but an instance holds the lock; exiting duplicate instance.");
        true
    }

    /// Probe with a *shared* lock, released at once: it only fails while
    /// a primary holds the exclusive one.
    fn lock_is_held(lock: &Path) -> bool {
        match File::open(lock) {
            Ok(file) => matches!(file.try_lock_shared(), Err(TryLockError::WouldBlock)),
            Err(_) => false,
        }
    }

    pub(super) fn start_server_at(
        paths: &InstancePaths,
        on_attempt: Box<dyn Fn(LaunchAttempt) + Send>,
    ) -> ServerStart {
        // As with a broken lock below: refusing to start would let another
        // user keep BoxPilot from starting at all.
        if let Err(e) = paths.prepare() {
            eprintln!("{e}; running without single-instance protection.");
            return ServerStart::Primary;
        }
        match claim_lock(&paths.lock) {
            // The file is intentionally leaked: the lock must live exactly
            // as long as the process, and the kernel releases it on exit
            // (or crash).
            Ok(file) => std::mem::forget(file),
            Err(TryLockError::WouldBlock) => return ServerStart::LostRace,
            // Unlike a held lock this isn't someone else being primary,
            // it's a broken lock (unwritable dir, a filesystem without
            // flock). Refusing to start would brick BoxPilot on every
            // launch, so run unguarded instead.
            Err(TryLockError::Error(e)) => {
                eprintln!(
                    "Failed to take the single-instance lock {}: {e}; running without single-instance protection.",
                    paths.lock.display()
                );
                return ServerStart::Primary;
            }
        }

        match bind_socket(&paths.socket) {
            Ok(listener) => {
                std::thread::spawn(move || socket_server_loop(listener, on_attempt));
            }
            // Still the primary — the lock says so — just deaf to later
            // launch attempts; they give up after their forward retries.
            Err(e) => eprintln!(
                "Failed to bind the deep-link socket {}: {e}; later launches can't reach this instance.",
                paths.socket.display()
            ),
        }
        ServerStart::Primary
    }

    fn claim_lock(lock: &Path) -> Result<File, TryLockError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(lock)
            .map_err(TryLockError::Error)?;
        file.try_lock()?;
        Ok(file)
    }

    /// Only the lock holder calls this, so any existing socket file is a
    /// leftover from a crashed instance and safe to delete.
    fn bind_socket(socket: &Path) -> std::io::Result<UnixListener> {
        match std::fs::remove_file(socket) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let listener = UnixListener::bind(socket)?;
        std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
        Ok(listener)
    }

    /// Blocking accept loop, one client at a time, mirroring the pipe
    /// server. The read timeout keeps a client that connects and never
    /// closes from wedging every later launch attempt.
    fn socket_server_loop(listener: UnixListener, on_attempt: Box<dyn Fn(LaunchAttempt) + Send>) {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let mut data = Vec::new();
            if stream.read_to_end(&mut data).is_err() {
                continue;
            }
            if let Ok(text) = String::from_utf8(data) {
                on_attempt(LaunchAttempt::from_wire(&text));
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::mpsc;

        fn temp_paths(tag: &str) -> InstancePaths {
            let dir =
                std::env::temp_dir().join(format!("boxpilot-si-test-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            InstancePaths {
                lock: dir.join("boxpilot.lock"),
                socket: dir.join("boxpilot.sock"),
                private_dir: None,
            }
        }

        /// A fresh base directory for [`private_dir`] tests. Short: a
        /// socket goes in one, and macOS's `$TMPDIR` is long already, with
        /// 104 bytes for a socket's whole path.
        fn temp_base(tag: &str) -> PathBuf {
            let base = std::env::temp_dir().join(format!("bp-si-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(&base).unwrap();
            base
        }

        fn mode_of(path: &Path) -> u32 {
            std::fs::symlink_metadata(path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        }

        fn my_uid() -> u32 {
            // SAFETY: getuid has no preconditions and cannot fail.
            unsafe { libc::getuid() }
        }

        type Callback = Box<dyn Fn(LaunchAttempt) + Send>;

        fn channel_callback() -> (Callback, mpsc::Receiver<LaunchAttempt>) {
            let (tx, rx) = mpsc::channel();
            let callback: Callback = Box::new(move |attempt| {
                let _ = tx.send(attempt);
            });
            (callback, rx)
        }

        #[test]
        fn paths_prefer_xdg_runtime_dir() {
            let paths = InstancePaths::resolve(
                Some(PathBuf::from("/run/user/1000")),
                Path::new("/tmp"),
                1000,
            );
            assert_eq!(paths.lock, PathBuf::from("/run/user/1000/boxpilot.lock"));
            assert_eq!(paths.socket, PathBuf::from("/run/user/1000/boxpilot.sock"));
            assert_eq!(paths.private_dir, None);
        }

        #[test]
        fn paths_fall_back_to_a_private_dir_in_temp_dir() {
            for runtime_dir in [None, Some(PathBuf::new())] {
                let paths = InstancePaths::resolve(runtime_dir, Path::new("/tmp"), 1234);
                assert_eq!(
                    paths.lock,
                    PathBuf::from("/tmp/boxpilot-1234/boxpilot.lock")
                );
                assert_eq!(
                    paths.socket,
                    PathBuf::from("/tmp/boxpilot-1234/boxpilot.sock")
                );
                assert_eq!(
                    paths.private_dir,
                    Some((PathBuf::from("/tmp/boxpilot-1234"), 1234))
                );
            }
        }

        #[test]
        fn the_private_dir_is_made_owner_only() {
            let dir = temp_base("made").join("boxpilot-x");
            private_dir(&dir, my_uid()).unwrap();
            assert!(dir.is_dir());
            assert_eq!(mode_of(&dir), 0o700);
            // Again, as every launch does.
            private_dir(&dir, my_uid()).unwrap();
        }

        #[test]
        fn a_private_dir_others_may_enter_is_closed_to_them() {
            let dir = temp_base("open").join("boxpilot-x");
            std::fs::create_dir(&dir).unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
            private_dir(&dir, my_uid()).unwrap();
            assert_eq!(mode_of(&dir), 0o700);
        }

        /// What another user could have made first in a shared temp dir is
        /// never used: a link, a file, or a directory of theirs.
        #[test]
        fn what_another_user_made_there_is_refused() {
            let base = temp_base("refused");
            let target = base.join("theirs");
            std::fs::create_dir(&target).unwrap();
            let link = base.join("boxpilot-link");
            std::os::unix::fs::symlink(&target, &link).unwrap();
            assert!(private_dir(&link, my_uid()).is_err());
            let file = base.join("boxpilot-file");
            std::fs::write(&file, b"").unwrap();
            assert!(private_dir(&file, my_uid()).is_err());
            // A directory whose owner isn't the user asking.
            assert!(private_dir(&target, my_uid().wrapping_add(1)).is_err());
        }

        /// Then a launch neither forwards there nor takes the lock: it starts
        /// as the primary, without single-instance protection, and nothing
        /// of it lands in that directory.
        #[test]
        fn a_refused_dir_gets_neither_a_link_nor_the_lock() {
            let base = temp_base("unused");
            let theirs = base.join("theirs");
            std::fs::create_dir(&theirs).unwrap();
            let paths = InstancePaths {
                lock: theirs.join("boxpilot.lock"),
                socket: theirs.join("boxpilot.sock"),
                private_dir: Some((theirs.clone(), my_uid().wrapping_add(1))),
            };
            let listener = UnixListener::bind(&paths.socket).unwrap();
            listener.set_nonblocking(true).unwrap();
            assert!(!try_forward_at(&paths, Some("sing-box://x")));
            assert!(listener.accept().is_err(), "the link went to their socket");
            let (callback, _rx) = channel_callback();
            assert!(matches!(
                start_server_at(&paths, callback),
                ServerStart::Primary
            ));
            assert!(!paths.lock.exists());
        }

        #[test]
        fn forward_without_an_instance_returns_false() {
            let paths = temp_paths("none");
            assert!(!try_forward_at(&paths, Some("sing-box://x")));
        }

        #[test]
        fn stale_socket_without_a_lock_holder_returns_false() {
            let paths = temp_paths("stale");
            // A crashed instance leaves its socket file behind.
            drop(UnixListener::bind(&paths.socket).unwrap());
            assert!(paths.socket.exists());
            assert!(!try_forward_at(&paths, Some("sing-box://x")));
        }

        #[test]
        fn unusable_socket_path_still_lets_the_first_launch_start() {
            let mut paths = temp_paths("long");
            // Past the `sun_path` limit (108 bytes on Linux, 104 on macOS):
            // connect and bind both fail.
            paths.socket = paths.socket.with_file_name("s".repeat(120));
            assert!(!try_forward_at(&paths, Some("sing-box://x")));
            let (on_attempt, _rx) = channel_callback();
            assert!(matches!(
                start_server_at(&paths, on_attempt),
                ServerStart::Primary
            ));
        }

        #[test]
        fn round_trip_and_a_second_primary_loses() {
            let paths = temp_paths("roundtrip");
            // A stale socket must not stop the primary from binding.
            drop(UnixListener::bind(&paths.socket).unwrap());

            let (on_attempt, rx) = channel_callback();
            assert!(matches!(
                start_server_at(&paths, on_attempt),
                ServerStart::Primary
            ));
            let mode = std::fs::metadata(&paths.socket)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);

            let uri = "sing-box://import-remote-profile?url=http%3A%2F%2F127.0.0.1%2Fx&name=t";
            assert!(try_forward_at(&paths, Some(uri)));
            assert_eq!(
                rx.recv_timeout(Duration::from_secs(5)).unwrap(),
                LaunchAttempt::DeepLink(uri.to_string())
            );
            assert!(try_forward_at(&paths, None));
            assert_eq!(
                rx.recv_timeout(Duration::from_secs(5)).unwrap(),
                LaunchAttempt::Plain
            );

            let (on_attempt, _rx) = channel_callback();
            assert!(matches!(
                start_server_at(&paths, on_attempt),
                ServerStart::LostRace
            ));
        }

        #[test]
        fn forward_waits_for_a_lock_holder_that_has_not_bound_yet() {
            // The LostRace path: the winner holds the lock, but its socket
            // isn't up yet when the loser forwards.
            let paths = temp_paths("late_bind");
            let _lock = claim_lock(&paths.lock).unwrap();
            let (on_attempt, rx) = channel_callback();
            let socket = paths.socket.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(150));
                let listener = bind_socket(&socket).unwrap();
                socket_server_loop(listener, on_attempt);
            });

            assert!(try_forward_at(&paths, Some("boxpilot://late")));
            assert_eq!(
                rx.recv_timeout(Duration::from_secs(5)).unwrap(),
                LaunchAttempt::DeepLink("boxpilot://late".to_string())
            );
        }
    }
}

/// The Windows pipe's pure parts, tested on every platform.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pipe_payload_is_one_launch_attempt() {
        let uri = "sing-box://import-remote-profile?url=https%3A%2F%2Fexample.com%2Fsub";
        assert_eq!(
            attempt_from_payload(uri.as_bytes(), false),
            LaunchAttempt::DeepLink(uri.to_string())
        );
        assert_eq!(attempt_from_payload(b"", false), LaunchAttempt::Plain);
        // What can't be a link still surfaces the window (ADR 0001).
        assert_eq!(
            attempt_from_payload(b"\xff\xfe", false),
            LaunchAttempt::Plain
        );
        assert_eq!(
            attempt_from_payload(uri.as_bytes(), true),
            LaunchAttempt::Plain
        );
    }

    /// The current user and SYSTEM, nobody else: no `Everyone`, no
    /// low-integrity label, nothing inherited.
    #[test]
    fn the_pipe_admits_the_user_and_system_only() {
        let sddl = pipe_sddl("S-1-5-21-1-2-3-1001");
        assert_eq!(sddl, "D:P(A;;GRGW;;;S-1-5-21-1-2-3-1001)(A;;GA;;;SY)");
        assert!(!sddl.contains("WD"));
        assert!(!sddl.contains("S:"));
    }
}

/// The Windows pipe itself, on Windows: CI's `windows` job runs these.
#[cfg(all(test, target_os = "windows"))]
mod pipe_tests {
    use super::*;
    use std::sync::mpsc::{self, Receiver};
    use std::time::{Duration, Instant};
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_INBOUND};
    use windows::Win32::System::Pipes::{
        CreateNamedPipeW, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };

    /// A pipe name of this test's own.
    fn test_pipe(tag: &str) -> String {
        format!(
            r"\\.\pipe\BoxPilot.DeepLink.test-{tag}-{}",
            std::process::id()
        )
    }

    /// The first instance of `path`, made here with the default DACL, as
    /// any process could; `None` if the name is already taken.
    fn first_instance(path: &str) -> Option<HANDLE> {
        let name: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `name` is NUL-terminated; no security attributes.
        let pipe = unsafe {
            CreateNamedPipeW(
                PCWSTR::from_raw(name.as_ptr()),
                PIPE_ACCESS_INBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                0,
                4096,
                0,
                None,
            )
        };
        (!pipe.is_invalid()).then_some(pipe)
    }

    fn close(pipe: HANDLE) {
        // SAFETY: a pipe handle this test made and owns.
        unsafe {
            let _ = CloseHandle(pipe);
        }
    }

    fn serve(path: &str) -> Receiver<LaunchAttempt> {
        let (tx, rx) = mpsc::channel();
        let path = path.to_string();
        std::thread::spawn(move || {
            pipe_server_loop(
                &path,
                Box::new(move |attempt| {
                    let _ = tx.send(attempt);
                }),
            )
        });
        rx
    }

    /// Forward `uri` once the server listens, and see it arrive.
    fn deliver(path: &str, uri: &str, attempts: &Receiver<LaunchAttempt>) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !try_forward_to(path, Some(uri)) {
            assert!(Instant::now() < deadline, "the server never listened");
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(
            attempts.recv_timeout(Duration::from_secs(5)).unwrap(),
            LaunchAttempt::DeepLink(uri.to_string())
        );
    }

    /// A server in this session is one a link goes to, and while it runs
    /// the name is its own, between clients too: nobody else can make the
    /// pipe's first instance.
    #[test]
    fn the_name_stays_the_servers_between_clients() {
        let path = test_pipe("keep");
        let attempts = serve(&path);
        for n in 0..3 {
            deliver(&path, &format!("boxpilot://link-{n}"), &attempts);
            if let Some(pipe) = first_instance(&path) {
                close(pipe);
                panic!("the name was free after client {n}");
            }
        }
    }

    /// A name another process made first is never joined: what is sent to
    /// it meanwhile reaches that process, not the server, which takes the
    /// name once it is free.
    #[test]
    fn a_name_made_elsewhere_is_never_joined() {
        let path = test_pipe("squat");
        let squatter = first_instance(&path).expect("a fresh name");
        let attempts = serve(&path);
        assert!(try_forward_to(&path, Some("boxpilot://squatted")));
        assert!(
            attempts.recv_timeout(Duration::from_millis(1500)).is_err(),
            "the server joined a pipe it didn't make"
        );
        close(squatter);
        deliver(&path, "boxpilot://after", &attempts);
    }

    /// A sender that connects, writes and closes before the server waits
    /// on that instance, as one does that reaches the next instance while
    /// the previous link is being passed on, is still read.
    #[test]
    fn a_sender_gone_before_the_wait_is_still_read() {
        let path = test_pipe("early");
        let pipe = first_instance(&path).expect("a fresh name");
        assert!(try_forward_to(&path, Some("boxpilot://early")));
        assert!(
            wait_for_client(pipe),
            "the sender that already left wasn't counted"
        );
        assert_eq!(read_payload(pipe), (b"boxpilot://early".to_vec(), false));
        close(pipe);
    }

    #[test]
    fn this_sessions_name_carries_its_id() {
        let session = current_session_id().expect("the token's session");
        assert_eq!(
            pipe_path(),
            format!(r"\\.\pipe\BoxPilot.DeepLink.{session}")
        );
    }
}
