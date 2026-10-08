use futures_channel::mpsc::{self, UnboundedReceiver, UnboundedSender};
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::Duration;

use crate::i18n::s;

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x08000000;

/// Forward each line of a sing-box pipe, raw, until EOF. Parsing (level,
/// ANSI colours) and dedup against the API stream happen in
/// `core::log_merge`. Lossy UTF-8, so one bad byte can't end the reader.
/// The channel wakes the UI-thread drain (`state::drain`); it closes once
/// every reader has hit EOF.
pub fn spawn_pipe_reader<R: Read + Send + 'static>(pipe: R, sender: UnboundedSender<String>) {
    thread::spawn(move || {
        let mut reader = BufReader::new(pipe);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let line = String::from_utf8_lossy(&buf);
                    let line = line.trim_end_matches(['\n', '\r']);
                    if sender.unbounded_send(line.to_string()).is_err() {
                        break;
                    }
                }
            }
        }
    });
}

#[cfg(target_os = "windows")]
pub fn flush_dns_windows() -> Result<String, String> {
    use std::os::windows::process::CommandExt;

    let output = Command::new("ipconfig")
        .arg("/flushdns")
        .creation_flags(CREATE_NO_WINDOW)
        .output();

    match output {
        Ok(output) => {
            if output.status.success() {
                Ok(s().errors.flush_dns_ok.to_string())
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                Err((s().errors.flush_dns_failed)(&stderr))
            }
        }
        Err(e) => Err((s().errors.flush_dns_run)(&e.to_string())),
    }
}

/// Undo the system proxy, if it is still ours: on, and pointing at the
/// loopback host BoxPilot's local proxy (or sing-box, which writes the same)
/// listens on. A proxy the user has set since is left alone, as on Linux
/// and macOS. Needs no elevation: it is the user's own WinINet setting
/// (HKCU), changed through WinINet, which also tells running programs.
///
/// Runs after every stop: a local sing-box is killed on Windows and can't
/// clear what it set, and the privileged helper's sing-box never sets it
/// (`enable_system_proxy` does, for the user).
#[cfg(target_os = "windows")]
pub fn disable_system_proxy() -> Result<(), String> {
    let Some((enabled, server)) = wininet::current() else {
        return Ok(());
    };
    if !is_our_windows_proxy(enabled, &server) {
        return Ok(());
    }
    wininet::set_direct().map_err(|e| (s().errors.disable_proxy)(&e.message()))
}

/// Point the user's system proxy at the local proxy on `port`, as sing-box's
/// `set_system_proxy` would. For TUN through the privileged helper: a
/// SYSTEM sing-box would write SYSTEM's proxy, not the user's, so the GUI
/// sets the user's itself, as the user (ADR 0006, "System proxy").
/// `disable_system_proxy` undoes it.
#[cfg(target_os = "windows")]
pub fn enable_system_proxy(port: u16) -> Result<(), String> {
    wininet::set_proxy(&format!("127.0.0.1:{port}"), WINDOWS_PROXY_BYPASS)
        .map_err(|e| (s().helper.proxy_failed)(&e.message()))
}

/// Never needed off Windows: only the Windows helper needs the GUI to set
/// the system proxy, and sing-box sets it itself everywhere else.
#[cfg(not(target_os = "windows"))]
pub fn enable_system_proxy(_port: u16) -> Result<(), String> {
    Ok(())
}

/// What the system proxy BoxPilot sets leaves alone: loopback and local
/// (dotless) names.
#[cfg(target_os = "windows")]
const WINDOWS_PROXY_BYPASS: &str = "localhost;127.*;[::1];<local>";

/// The current user's WinINet proxy settings for the LAN connection, read
/// and written through WinINet's per-connection options, which update
/// HKCU and are what Windows' own Settings app uses; each write is followed
/// by the notifications that make running programs reload them.
#[cfg(target_os = "windows")]
mod wininet {
    use std::ffi::c_void;
    use std::mem::size_of;
    use windows::core::PWSTR;
    use windows::Win32::Foundation::{GlobalFree, HGLOBAL};
    use windows::Win32::Networking::WinInet::{
        InternetQueryOptionW, InternetSetOptionW, INTERNET_OPTION_PER_CONNECTION_OPTION,
        INTERNET_OPTION_REFRESH, INTERNET_OPTION_SETTINGS_CHANGED, INTERNET_PER_CONN,
        INTERNET_PER_CONN_FLAGS, INTERNET_PER_CONN_OPTIONW, INTERNET_PER_CONN_OPTIONW_0,
        INTERNET_PER_CONN_OPTION_LISTW, INTERNET_PER_CONN_PROXY_BYPASS,
        INTERNET_PER_CONN_PROXY_SERVER, PROXY_TYPE_DIRECT, PROXY_TYPE_PROXY,
    };

    fn option(
        option: INTERNET_PER_CONN,
        value: INTERNET_PER_CONN_OPTIONW_0,
    ) -> INTERNET_PER_CONN_OPTIONW {
        INTERNET_PER_CONN_OPTIONW {
            dwOption: option,
            Value: value,
        }
    }

    /// A list over `options` for the LAN connection (no connection name).
    fn list(options: &mut [INTERNET_PER_CONN_OPTIONW]) -> INTERNET_PER_CONN_OPTION_LISTW {
        INTERNET_PER_CONN_OPTION_LISTW {
            dwSize: size_of::<INTERNET_PER_CONN_OPTION_LISTW>() as u32,
            pszConnection: PWSTR::null(),
            dwOptionCount: options.len() as u32,
            dwOptionError: 0,
            pOptions: options.as_mut_ptr(),
        }
    }

    /// Whether a manual proxy is on, and its server string; `None` if the
    /// settings can't be read.
    pub(super) fn current() -> Option<(bool, String)> {
        let mut options = [
            option(
                INTERNET_PER_CONN_FLAGS,
                INTERNET_PER_CONN_OPTIONW_0 { dwValue: 0 },
            ),
            option(
                INTERNET_PER_CONN_PROXY_SERVER,
                INTERNET_PER_CONN_OPTIONW_0 {
                    pszValue: PWSTR::null(),
                },
            ),
        ];
        let mut query = list(&mut options);
        let mut size = size_of::<INTERNET_PER_CONN_OPTION_LISTW>() as u32;
        // SAFETY: `query` points at `options`, both alive for the call;
        // `size` is the list's own size.
        unsafe {
            InternetQueryOptionW(
                None,
                INTERNET_OPTION_PER_CONNECTION_OPTION,
                Some(&mut query as *mut INTERNET_PER_CONN_OPTION_LISTW as *mut c_void),
                &mut size,
            )
        }
        .ok()?;
        // SAFETY: WinINet filled both options with the kind each asked for.
        let (flags, server) = unsafe { (options[0].Value.dwValue, options[1].Value.pszValue) };
        let server_text = if server.is_null() {
            String::new()
        } else {
            // SAFETY: a NUL-terminated string WinINet allocated with
            // GlobalAlloc, read once, then freed as its docs say.
            unsafe {
                let text = server.to_string().unwrap_or_default();
                let _ = GlobalFree(HGLOBAL(server.0 as *mut c_void));
                text
            }
        };
        Some((flags & PROXY_TYPE_PROXY != 0, server_text))
    }

    fn apply(options: &mut [INTERNET_PER_CONN_OPTIONW]) -> windows::core::Result<()> {
        let settings = list(options);
        // SAFETY: `settings` points at `options`, both alive for the call;
        // the notifications take no buffer.
        unsafe {
            InternetSetOptionW(
                None,
                INTERNET_OPTION_PER_CONNECTION_OPTION,
                Some(&settings as *const INTERNET_PER_CONN_OPTION_LISTW as *const c_void),
                size_of::<INTERNET_PER_CONN_OPTION_LISTW>() as u32,
            )?;
            let _ = InternetSetOptionW(None, INTERNET_OPTION_SETTINGS_CHANGED, None, 0);
            let _ = InternetSetOptionW(None, INTERNET_OPTION_REFRESH, None, 0);
        }
        Ok(())
    }

    /// A manual proxy on `server`, bypassing `bypass`.
    pub(super) fn set_proxy(server: &str, bypass: &str) -> windows::core::Result<()> {
        let mut server: Vec<u16> = server.encode_utf16().chain(Some(0)).collect();
        let mut bypass: Vec<u16> = bypass.encode_utf16().chain(Some(0)).collect();
        let mut options = [
            option(
                INTERNET_PER_CONN_FLAGS,
                INTERNET_PER_CONN_OPTIONW_0 {
                    dwValue: PROXY_TYPE_PROXY | PROXY_TYPE_DIRECT,
                },
            ),
            option(
                INTERNET_PER_CONN_PROXY_SERVER,
                INTERNET_PER_CONN_OPTIONW_0 {
                    pszValue: PWSTR(server.as_mut_ptr()),
                },
            ),
            option(
                INTERNET_PER_CONN_PROXY_BYPASS,
                INTERNET_PER_CONN_OPTIONW_0 {
                    pszValue: PWSTR(bypass.as_mut_ptr()),
                },
            ),
        ];
        apply(&mut options)
    }

    /// No proxy: direct connections.
    pub(super) fn set_direct() -> windows::core::Result<()> {
        let mut options = [option(
            INTERNET_PER_CONN_FLAGS,
            INTERNET_PER_CONN_OPTIONW_0 {
                dwValue: PROXY_TYPE_DIRECT,
            },
        )];
        apply(&mut options)
    }
}

/// Undo the system proxy sing-box set, if it is still there. sing-box clears
/// it itself on SIGTERM, so this only matters after a crash or a SIGKILL.
/// Mirrors sing-box's `common/settings/proxy_linux.go`: GNOME via
/// `gsettings`, KDE via `kwriteconfig5`/`kwriteconfig6`. Each desktop is
/// reset only while it still points at 127.0.0.1, so a proxy the user has
/// set since is left alone. A missing tool means that desktop isn't in use
/// and is skipped silently.
#[cfg(target_os = "linux")]
pub fn disable_system_proxy() -> Result<(), String> {
    let mut errors = Vec::new();

    if let (Some(mode), Some(host)) = (
        read_command("gsettings", &["get", "org.gnome.system.proxy", "mode"]),
        read_command("gsettings", &["get", "org.gnome.system.proxy.http", "host"]),
    ) {
        if is_our_gnome_proxy(&mode, &host) {
            if let Err(e) = run_command(
                "gsettings",
                &["set", "org.gnome.system.proxy", "mode", "none"],
            ) {
                errors.push(e);
            }
        }
    }

    // Same preference order as sing-box; both versions edit the same
    // ~/.config/kioslaverc.
    let kde = ["5", "6"].into_iter().find_map(|v| {
        let read = format!("kreadconfig{v}");
        let proxy_type = read_command(&read, &kde_proxy_key("ProxyType"))?;
        let http_proxy = read_command(&read, &kde_proxy_key("httpProxy"))?;
        Some((format!("kwriteconfig{v}"), proxy_type, http_proxy))
    });
    if let Some((write, proxy_type, http_proxy)) = kde {
        if is_our_kde_proxy(&proxy_type, &http_proxy) {
            let mut args = kde_proxy_key("ProxyType");
            args.push("0");
            match run_command(&write, &args) {
                // Tell running KIO apps to re-read it, as sing-box does.
                Ok(()) => {
                    let _ = run_command(
                        "dbus-send",
                        &[
                            "--type=signal",
                            "/KIO/Scheduler",
                            "org.kde.KIO.Scheduler.reparseSlaveConfiguration",
                            "string:''",
                        ],
                    );
                }
                Err(e) => errors.push(e),
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err((s().errors.disable_proxy)(&errors.join("; ")))
    }
}

#[cfg(target_os = "linux")]
fn kde_proxy_key(key: &str) -> Vec<&str> {
    vec![
        "--file",
        "kioslaverc",
        "--group",
        "Proxy Settings",
        "--key",
        key,
    ]
}

/// Undo the system proxy sing-box set, if it is still there — same job and
/// same rule as on Linux. sing-box's `common/settings/proxy_darwin.go` sets
/// the web, secure web and SOCKS proxy of a network service through
/// `networksetup`; each one that is still on and points at 127.0.0.1 is
/// turned off, in every service, so a proxy the user set themselves is left
/// alone. A service whose state can't be read is skipped. The rule is
/// `boxpilot_runconfig::system_proxy`'s, which the privileged helper shares.
#[cfg(target_os = "macos")]
pub fn disable_system_proxy() -> Result<(), String> {
    use boxpilot_runconfig::system_proxy::{
        macos_proxy_is_ours, parse_network_services, LIST_SERVICES, MACOS_PROXY_KINDS, NETWORKSETUP,
    };
    let Some(services) = read_command(NETWORKSETUP, &[LIST_SERVICES]) else {
        return Ok(());
    };
    let mut errors = Vec::new();
    for service in parse_network_services(&services) {
        for (get, set_state) in MACOS_PROXY_KINDS {
            let Some(state) = read_command(NETWORKSETUP, &[get, service]) else {
                continue;
            };
            if macos_proxy_is_ours(&state, None) {
                if let Err(e) = run_command(NETWORKSETUP, &[set_state, service, "off"]) {
                    errors.push(e);
                }
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err((s().errors.disable_proxy)(&errors.join("; ")))
    }
}

/// Stdout of a successful run, or `None` if the tool is missing or fails.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn read_command(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_command(program: &str, args: &[&str]) -> Result<(), String> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| (s().errors.run_command)(program, &e.to_string()))?;
    if output.status.success() {
        Ok(())
    } else {
        Err((s().errors.command_failed)(
            program,
            String::from_utf8_lossy(&output.stderr).trim(),
        ))
    }
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
pub fn disable_system_proxy() -> Result<(), String> {
    Ok(())
}

/// The GNOME proxy is ours while it is still in manual mode on the loopback
/// host sing-box writes. Takes raw `gsettings get` output (GVariant text,
/// e.g. `'manual'`). Pure so it is tested on every platform.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn is_our_gnome_proxy(mode: &str, http_host: &str) -> bool {
    let unquote = |s: &str| s.trim().trim_matches('\'').to_string();
    unquote(mode) == "manual" && unquote(http_host) == "127.0.0.1"
}

/// The KDE proxy is ours while it is still manual (`ProxyType=1`) with an
/// HTTP proxy on the loopback host (sing-box writes
/// `http://127.0.0.1:<port>`). Takes raw `kreadconfig` output.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn is_our_kde_proxy(proxy_type: &str, http_proxy: &str) -> bool {
    proxy_type.trim() == "1" && http_proxy.contains("127.0.0.1")
}

/// The Windows proxy is ours while a manual proxy is still on and its
/// server is on the loopback host (BoxPilot writes `127.0.0.1:<port>`;
/// sing-box's own `set_system_proxy` writes the same host). Takes WinINet's
/// manual-proxy flag and server string. Pure so it is tested on every
/// platform.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn is_our_windows_proxy(enabled: bool, server: &str) -> bool {
    enabled
        && server.split(';').any(|entry| {
            // `127.0.0.1:7788`, `http://127.0.0.1:7788` or
            // `http=127.0.0.1:7788`: the host is what matters.
            let address = entry.rsplit(['=', '/']).next().unwrap_or("").trim();
            address.rsplit_once(':').map_or(address, |(host, _)| host) == "127.0.0.1"
        })
}

/// Match sing-box's wintun adapter by FriendlyName, case-insensitively. Pulled
/// out as a pure fn (no `cfg`) so the one bit of judgement here — *which*
/// adapters we uninstall — is unit-tested on every platform, even though the
/// caller is Windows-only and cannot run on the macOS dev box.
// Used by the Windows `remove_tun_adapter` and by tests on every platform; the
// only "unused" case is a non-Windows non-test build (macOS dev `cargo build`).
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn is_sing_tun_friendly_name(name: &str) -> bool {
    name.trim().to_ascii_lowercase().starts_with("sing-tun")
}

/// Remove stale sing-tun (wintun) network adapters left behind by a previous
/// run or an unclean exit. Done natively via SetupAPI + `DiUninstallDevice`,
/// in-process with no child-process launch — directly off the connect critical
/// path (it runs in prep, before sing-box spawns).
///
/// Requires Administrator (`DiUninstallDevice` returns ERROR_ACCESS_DENIED
/// otherwise), so it runs only when the user started BoxPilot elevated and
/// TUN runs a local sing-box. Without that, TUN goes through the privileged
/// helper, which removes stale adapters itself (ADR 0006 rule 6), and this
/// is skipped. Best-effort: any failure is logged and ignored, because
/// sing-box recreates its own adapter at startup regardless.
///
/// Cannot be compiled or exercised on macOS — verify via the CI MSVC build and
/// a Windows smoke test (connect/disconnect/restart in TUN + kill-recovery).
#[cfg(target_os = "windows")]
pub fn remove_tun_adapter() {
    use windows::core::PCWSTR;
    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        DiUninstallDevice, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo,
        SetupDiGetClassDevsW, SetupDiGetDeviceRegistryPropertyW, GUID_DEVCLASS_NET,
        SETUP_DI_GET_CLASS_DEVS_FLAGS, SPDRP_FRIENDLYNAME, SP_DEVINFO_DATA,
    };
    use windows::Win32::Foundation::{BOOL, HWND};

    if !crate::core::privileged_helper::process_is_elevated() {
        return;
    }
    eprintln!("TUN cleanup: removing sing-tun adapters (native SetupAPI)");

    unsafe {
        // Snapshot of every installed network-class device. Flags are 0 (NOT
        // DIGCF_PRESENT) on purpose: without the presence filter we also catch
        // not-present "ghost" sing-tun devnodes a crash can leave behind. The
        // binding maps INVALID_HANDLE_VALUE to Err, so a successful return is a
        // live set.
        let dev_info = match SetupDiGetClassDevsW(
            Some(&GUID_DEVCLASS_NET as *const _),
            PCWSTR::null(),
            HWND::default(),
            SETUP_DI_GET_CLASS_DEVS_FLAGS(0),
        ) {
            Ok(handle) => handle,
            Err(e) => {
                eprintln!("TUN cleanup: SetupDiGetClassDevsW failed: {e}");
                return;
            }
        };

        // Uninstalling a devnode leaves its element in this in-memory set, so
        // enumerating by incrementing index stays valid across removals.
        let mut removed = 0u32;
        let mut index = 0u32;
        loop {
            let mut data = SP_DEVINFO_DATA {
                cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
                ..Default::default()
            };
            // Err here is ERROR_NO_MORE_ITEMS (end of set) or a real error —
            // either way, stop.
            if SetupDiEnumDeviceInfo(dev_info, index, &mut data).is_err() {
                break;
            }
            index += 1;

            // Two-call FriendlyName read: probe size (null buffer), then fill.
            // A device with no FriendlyName leaves `needed` at 0
            // (ERROR_INVALID_DATA) and is skipped — same as the old filter.
            let mut needed = 0u32;
            let _ = SetupDiGetDeviceRegistryPropertyW(
                dev_info,
                &data,
                SPDRP_FRIENDLYNAME,
                None,
                None,
                Some(&mut needed as *mut u32),
            );
            if needed == 0 {
                continue;
            }
            let mut buf = vec![0u8; needed as usize];
            if SetupDiGetDeviceRegistryPropertyW(
                dev_info,
                &data,
                SPDRP_FRIENDLYNAME,
                None,
                Some(buf.as_mut_slice()),
                None,
            )
            .is_err()
            {
                continue;
            }

            // FriendlyName is a NUL-terminated UTF-16 string in the byte buffer.
            let utf16: Vec<u16> = buf
                .chunks_exact(2)
                .map(|c| u16::from_ne_bytes([c[0], c[1]]))
                .collect();
            let name = String::from_utf16_lossy(&utf16);
            let name = name.trim_end_matches('\0');
            if !is_sing_tun_friendly_name(name) {
                continue;
            }

            // Uninstall the devnode (+ child devnodes on Win8+). Pass a
            // non-null NeedReboot so it never pops a system-restart dialog (a
            // virtual adapter never needs one); the value is ignored.
            let mut need_reboot = BOOL(0);
            match DiUninstallDevice(
                HWND::default(),
                dev_info,
                &data,
                0,
                Some(&mut need_reboot as *mut BOOL),
            ) {
                Ok(()) => {
                    removed += 1;
                    eprintln!("TUN cleanup: removed '{name}'");
                }
                Err(e) => eprintln!("TUN cleanup: DiUninstallDevice('{name}') failed: {e}"),
            }
        }

        // HDEVINFO is a Copy handle with no Drop glue — free the set explicitly.
        let _ = SetupDiDestroyDeviceInfoList(dev_info);
        if removed == 0 {
            eprintln!("TUN cleanup: no sing-tun adapters found");
        }
    }
}

// Nothing to do on Linux: the tun device goes away with sing-box's fd, and
// sing-box removes its routes itself on SIGTERM.
#[cfg(not(target_os = "windows"))]
pub fn remove_tun_adapter() {}

/// Best-effort `resolvectl flush-caches` (systemd-resolved). Without it, or
/// without permission to flush, the cache is simply kept.
#[cfg(target_os = "linux")]
pub fn flush_dns_linux() {
    let _ = Command::new("resolvectl")
        .arg("flush-caches")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Best-effort `dscacheutil -flushcache`. mDNSResponder's own cache needs
/// root to flush (`killall -HUP`), so that one is kept.
#[cfg(target_os = "macos")]
pub fn flush_dns_macos() {
    let _ = Command::new("/usr/bin/dscacheutil")
        .arg("-flushcache")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Pre-start prep (TUN cleanup + DNS flush). Run on a background thread.
/// Nothing here needs elevation except the Windows adapter cleanup, which
/// skips itself without it: `ipconfig /flushdns` works for a standard user.
pub fn prepare_process_start(is_tun_mode: bool) {
    if is_tun_mode {
        remove_tun_adapter();
    }
    #[cfg(target_os = "windows")]
    {
        let _ = flush_dns_windows();
    }
    #[cfg(target_os = "linux")]
    flush_dns_linux();
    #[cfg(target_os = "macos")]
    flush_dns_macos();
}

/// Post-stop cleanup (disable system proxy + TUN removal). Fire-and-forget on a thread.
pub fn cleanup_after_process_stop(was_system_proxy: bool, was_tun_mode: bool) {
    if was_system_proxy {
        if let Err(e) = disable_system_proxy() {
            eprintln!("Warning: {}", e);
        }
    }
    if was_tun_mode {
        remove_tun_adapter();
    }
}

/// Validate a config file by invoking `sing-box check`. This only parses and
/// schema-checks the config — it does not start tunnels, touch the registry,
/// or require elevation, so it is safe to run even while sing-box is connected.
/// Returns `Ok(())` if the config is valid, or `Err` with a trimmed summary of
/// sing-box's diagnostic output if not. Callers should skip this when the
/// binary is absent (see `perform_update`).
pub fn validate_config(
    sing_path: &Path,
    working_dir: &Path,
    config_path: &Path,
) -> Result<(), String> {
    let mut cmd = Command::new(sing_path);
    cmd.arg("check")
        .arg("-D")
        .arg(working_dir)
        .arg("-c")
        .arg(config_path)
        .current_dir(working_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let output = cmd
        .output()
        .map_err(|e| (s().errors.check_run_failed)(&e.to_string()))?;

    if output.status.success() {
        return Ok(());
    }

    // sing-box writes diagnostics to stderr; fall back to stdout if empty.
    let mut detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if detail.is_empty() {
        detail = String::from_utf8_lossy(&output.stdout).trim().to_string();
    }
    // Keep the toast readable: first line, char-capped (byte slicing could
    // split a multi-byte UTF-8 sequence and panic).
    let summary: String = detail.lines().next().unwrap_or("").chars().take(300).collect();
    Err((s().errors.validation_failed)(&summary))
}

/// Parse the version out of `sing-box version` output. The first line looks
/// like `sing-box version 1.11.15`; later lines (Environment/Tags/…) are
/// ignored. Pure so the one bit of judgement — what counts as a version
/// line — is unit-tested off-Windows.
pub fn parse_sing_box_version(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let version = line.trim().strip_prefix("sing-box version ")?.trim();
        (!version.is_empty()).then(|| version.to_string())
    })
}

/// Ask the bundled binary for its version (`sing-box version`, hidden
/// window). `None` when the binary is missing or the output is
/// unrecognizable. Blocking — run on the background executor.
pub fn query_sing_box_version(sing_path: &Path) -> Option<String> {
    let mut cmd = Command::new(sing_path);
    cmd.arg("version")
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let output = cmd.output().ok()?;
    parse_sing_box_version(&String::from_utf8_lossy(&output.stdout))
}

/// Both pipe readers run on dedicated threads. Called from
/// `ProcessSession::spawn_child` after the prep task completes.
pub fn start_sing_box(
    sing_path: &Path,
    config_path: &Path,
    working_dir: &Path,
) -> std::io::Result<(Child, UnboundedReceiver<String>)> {
    let mut cmd = Command::new(sing_path);
    cmd.arg("run")
        .arg("-D")
        .arg(working_dir)
        .arg("-c")
        .arg(config_path)
        .current_dir(working_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    // Have the kernel SIGTERM sing-box if BoxPilot dies, so a crash doesn't
    // orphan it with its routes and system proxy still in place. It fires
    // when the spawning *thread* exits; this runs on the UI thread, which
    // lives as long as the app. The kernel clears it on exec of a binary
    // with file capabilities, so a setcap'd sing-box isn't covered.
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        let parent = std::process::id() as libc::pid_t;
        // SAFETY: runs between fork and exec; only async-signal-safe
        // syscalls (prctl, getppid) and no allocation on the success path.
        unsafe {
            cmd.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                    return Err(io::Error::last_os_error());
                }
                // BoxPilot may have died before prctl took effect.
                if libc::getppid() != parent {
                    return Err(io::Error::from_raw_os_error(libc::ESRCH));
                }
                Ok(())
            });
        }
    }

    let mut child = cmd.spawn()?;
    let (sender, receiver) = mpsc::unbounded();

    if let Some(stdout) = child.stdout.take() {
        spawn_pipe_reader(stdout, sender.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_pipe_reader(stderr, sender);
    }

    Ok((child, receiver))
}

/// Ask sing-box to stop, without waiting. On Linux and macOS that's SIGTERM,
/// so it can remove its auto_route rules and system proxy on the way out;
/// on Windows it's `kill()`. Cheap enough for the UI thread. Follow with
/// `reap_child`.
pub fn signal_stop(child: &mut Child) -> io::Result<()> {
    #[cfg(unix)]
    {
        // A reaped child's pid may already belong to another process;
        // `kill()` has the same guard.
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        // SAFETY: a plain syscall on our own un-reaped child's pid.
        if unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        child.kill()
    }
}

/// Wait for a child that `signal_stop` was sent to. On Linux and macOS, give
/// it up to `grace` to exit on its own, then SIGKILL it. Blocking: keep it
/// off the UI thread.
pub fn reap_child(child: &mut Child, grace: Duration) -> io::Result<ExitStatus> {
    #[cfg(unix)]
    {
        const POLL: Duration = Duration::from_millis(50);
        let deadline = std::time::Instant::now() + grace;
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(status);
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            thread::sleep(POLL);
        }
        let _ = child.kill();
    }
    #[cfg(not(unix))]
    let _ = grace;
    child.wait()
}

/// `signal_stop` + `reap_child`: stop sing-box and wait at most `grace`
/// before killing it. Blocking.
pub fn terminate_child(child: &mut Child, grace: Duration) -> io::Result<ExitStatus> {
    let _ = signal_stop(child);
    reap_child(child, grace)
}

/// Toast text for a sing-box that exited on its own. `signal` is the Unix
/// signal that killed it, `code` its exit code if it exited normally.
pub fn exit_message(code: Option<i32>, signal: Option<i32>) -> String {
    match (signal, code) {
        (Some(signal), _) => (s().errors.killed_by_signal)(&signal.to_string()),
        (None, Some(code)) => (s().errors.exited_with_code)(&code.to_string()),
        (None, None) => s().messages.sing_box_exited.to_string(),
    }
}

pub fn describe_exit(status: &ExitStatus) -> String {
    #[cfg(unix)]
    let signal = std::os::unix::process::ExitStatusExt::signal(status);
    #[cfg(not(unix))]
    let signal = None;
    exit_message(status.code(), signal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_message_prefers_signal_then_code() {
        assert_eq!(
            exit_message(None, Some(9)),
            "sing-box was killed by signal 9."
        );
        assert_eq!(exit_message(Some(1), None), "sing-box exited with code 1.");
        assert_eq!(exit_message(Some(0), None), "sing-box exited with code 0.");
        assert_eq!(exit_message(None, None), "sing-box exited.");
    }

    #[cfg(unix)]
    #[test]
    fn describe_exit_reads_unix_wait_status() {
        use std::os::unix::process::ExitStatusExt;
        // Raw wait(2) status: low 7 bits = signal, else code in bits 8..16.
        assert_eq!(
            describe_exit(&ExitStatus::from_raw(9)),
            "sing-box was killed by signal 9."
        );
        assert_eq!(
            describe_exit(&ExitStatus::from_raw(1 << 8)),
            "sing-box exited with code 1."
        );
    }

    /// Only a proxy still pointing where sing-box put it gets reset; one the
    /// user changed since (another host, or mode off/auto) is left alone.
    #[test]
    fn gnome_proxy_is_ours_only_when_manual_on_loopback() {
        assert!(is_our_gnome_proxy("'manual'\n", "'127.0.0.1'\n"));
        assert!(is_our_gnome_proxy("manual", "127.0.0.1"));
        assert!(!is_our_gnome_proxy("'none'\n", "'127.0.0.1'\n"));
        assert!(!is_our_gnome_proxy("'auto'\n", "'127.0.0.1'\n"));
        assert!(!is_our_gnome_proxy("'manual'\n", "'proxy.corp.example'\n"));
        assert!(!is_our_gnome_proxy("'manual'\n", "''\n"));
        assert!(!is_our_gnome_proxy("'manual'\n", "'127.0.0.10'\n"));
    }

    #[test]
    fn kde_proxy_is_ours_only_when_manual_on_loopback() {
        assert!(is_our_kde_proxy("1\n", "http://127.0.0.1:7788\n"));
        assert!(!is_our_kde_proxy("0\n", "http://127.0.0.1:7788\n"));
        assert!(!is_our_kde_proxy("2\n", "http://127.0.0.1:7788\n"));
        assert!(!is_our_kde_proxy("1\n", "http://proxy.corp.example:3128\n"));
        assert!(!is_our_kde_proxy("1\n", "\n"));
        assert!(!is_our_kde_proxy("\n", "\n"));
    }

    /// A child that ignores nothing: SIGTERM ends it well before the grace
    /// period, and the status says so.
    #[cfg(unix)]
    #[test]
    fn terminate_child_stops_with_sigterm() {
        use std::os::unix::process::ExitStatusExt;
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let started = std::time::Instant::now();
        let status = terminate_child(&mut child, Duration::from_secs(3)).unwrap();
        assert_eq!(status.signal(), Some(libc::SIGTERM));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    /// A child that ignores SIGTERM is SIGKILLed once the grace runs out.
    #[cfg(unix)]
    #[test]
    fn terminate_child_escalates_to_sigkill() {
        use std::os::unix::process::ExitStatusExt;
        let mut child = Command::new("sh")
            .args(["-c", "trap '' TERM; exec sleep 30"])
            .spawn()
            .unwrap();
        // Let sh install the trap before we signal it.
        thread::sleep(Duration::from_millis(200));
        let started = std::time::Instant::now();
        let status = terminate_child(&mut child, Duration::from_millis(300)).unwrap();
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    /// Same rule on Windows: only a manual proxy still on the loopback host
    /// is cleared, in whichever spelling WinINet holds it.
    #[test]
    fn windows_proxy_is_ours_only_when_on_and_on_loopback() {
        assert!(is_our_windows_proxy(true, "127.0.0.1:7788"));
        assert!(is_our_windows_proxy(true, "http://127.0.0.1:7788"));
        assert!(is_our_windows_proxy(
            true,
            "http=127.0.0.1:7788;https=127.0.0.1:7788"
        ));
        assert!(is_our_windows_proxy(true, "127.0.0.1"));
        assert!(!is_our_windows_proxy(false, "127.0.0.1:7788"));
        assert!(!is_our_windows_proxy(true, "proxy.corp.example:3128"));
        assert!(!is_our_windows_proxy(true, "127.0.0.10:7788"));
        assert!(!is_our_windows_proxy(true, ""));
    }

    /// The native SetupAPI path uninstalls only adapters whose FriendlyName
    /// begins with "sing-tun" (case-insensitive). Getting this wrong would
    /// uninstall real NICs, so it is the one piece of the Windows-only fn we can
    /// and must test off-Windows.
    #[test]
    fn sing_tun_name_matches_only_singbox_adapters() {
        assert!(is_sing_tun_friendly_name("sing-tun"));
        assert!(is_sing_tun_friendly_name("sing-tun0"));
        assert!(is_sing_tun_friendly_name("Sing-Tun Tunnel"));
        assert!(is_sing_tun_friendly_name("SING-TUN"));
        assert!(is_sing_tun_friendly_name("  sing-tun0  "));
    }

    #[test]
    fn sing_tun_name_rejects_real_nics() {
        assert!(!is_sing_tun_friendly_name("Intel(R) Wi-Fi 6 AX201"));
        assert!(!is_sing_tun_friendly_name("Realtek PCIe GbE Family Controller"));
        assert!(!is_sing_tun_friendly_name("WireGuard Tunnel"));
        assert!(!is_sing_tun_friendly_name("TAP-Windows Adapter V9"));
        assert!(!is_sing_tun_friendly_name("my sing-tun clone")); // prefix only
        assert!(!is_sing_tun_friendly_name(""));
    }

    /// Real `sing-box version` output: version on the first line, then
    /// Environment/Tags/Revision lines we must not mistake for versions.
    #[test]
    fn parses_version_from_typical_output() {
        let out = "sing-box version 1.11.15\n\nEnvironment: go1.24.4 windows/amd64\nTags: with_gvisor,with_quic\n";
        assert_eq!(parse_sing_box_version(out).as_deref(), Some("1.11.15"));
    }

    #[test]
    fn parses_prerelease_versions() {
        assert_eq!(
            parse_sing_box_version("sing-box version 1.12.0-beta.5\n").as_deref(),
            Some("1.12.0-beta.5")
        );
    }

    #[test]
    fn rejects_output_without_a_version_line() {
        assert_eq!(parse_sing_box_version(""), None);
        assert_eq!(parse_sing_box_version("not a sing-box binary"), None);
        assert_eq!(parse_sing_box_version("sing-box version "), None);
        assert_eq!(parse_sing_box_version("Environment: go1.24.4"), None);
    }

    #[test]
    fn pipe_reader_forwards_raw_lines() {
        let (sender, mut receiver) = mpsc::unbounded();
        let input: &[u8] = b"\x1b[36mINFO\x1b[0m[0000] started\r\nbad \xff byte\nlast";
        spawn_pipe_reader(input, sender);

        // `Some(line)`, or `None` once the channel has closed.
        let mut recv = || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                match receiver.try_recv() {
                    Ok(line) => return Some(line),
                    Err(mpsc::TryRecvError::Closed) => return None,
                    Err(mpsc::TryRecvError::Empty) if std::time::Instant::now() < deadline => {
                        thread::sleep(std::time::Duration::from_millis(5))
                    }
                    Err(mpsc::TryRecvError::Empty) => panic!("pipe reader stalled"),
                }
            }
        };
        assert_eq!(recv().unwrap(), "\x1b[36mINFO\x1b[0m[0000] started");
        assert_eq!(recv().unwrap(), "bad \u{fffd} byte");
        assert_eq!(recv().unwrap(), "last", "an unterminated last line still arrives");

        // Pipe exhausted -> reader thread exits -> channel disconnects.
        assert!(recv().is_none());
    }
}
