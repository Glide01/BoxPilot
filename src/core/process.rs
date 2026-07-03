use crate::core::settings::{LogEntry, LogLevel};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x08000000;

pub fn classify_log_level(line: &str) -> LogLevel {
    if line.contains("\"error\"") || line.contains("\"fatal\"") {
        LogLevel::Error
    } else if line.contains("\"warn\"") {
        LogLevel::Warn
    } else {
        LogLevel::Info
    }
}

pub fn spawn_pipe_reader<R: Read + Send + 'static>(
    pipe: R,
    prefix: &str,
    sender: mpsc::Sender<LogEntry>,
) {
    let prefix = prefix.to_string();
    thread::spawn(move || {
        let reader = BufReader::new(pipe);
        for line in reader.lines() {
            if let Ok(line_content) = line {
                let level = classify_log_level(&line_content);
                let message = format!("[{}] {}", prefix, line_content);
                let _ = sender.send(LogEntry { message, level });
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
                Ok("Successfully flushed the DNS resolver cache.".to_string())
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                Err(format!("Failed to flush DNS cache. Error: {}", stderr))
            }
        }
        Err(e) => Err(format!("Failed to execute 'ipconfig /flushdns': {}", e)),
    }
}

#[cfg(target_os = "windows")]
pub fn disable_system_proxy() -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let output = Command::new("reg")
        .args([
            "add",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings",
            "/v", "ProxyEnable",
            "/t", "REG_DWORD",
            "/d", "0",
            "/f",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("Failed to run reg command: {}", e))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "Failed to disable system proxy: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

#[cfg(not(target_os = "windows"))]
pub fn disable_system_proxy() -> Result<(), String> {
    Ok(())
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
/// otherwise) — we always run elevated via `ensure_elevated()`. Best-effort:
/// any failure is logged and ignored, because sing-box recreates its own
/// adapter at startup regardless.
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

#[cfg(not(target_os = "windows"))]
pub fn remove_tun_adapter() {}

/// Pre-start prep (TUN cleanup + DNS flush). Run on a background thread.
pub fn prepare_process_start(is_tun_mode: bool) {
    if is_tun_mode {
        remove_tun_adapter();
    }
    #[cfg(target_os = "windows")]
    {
        let _ = flush_dns_windows();
    }
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
        .map_err(|e| format!("Failed to run sing-box check: {}", e))?;

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
    Err(format!("Config validation failed: {}", summary))
}

/// Both pipe readers run on dedicated threads. Called from
/// `ProcessSession::spawn_child` after the prep task completes.
pub fn start_sing_box(
    sing_path: &Path,
    config_path: &Path,
    working_dir: &Path,
) -> std::io::Result<(Child, mpsc::Receiver<LogEntry>)> {
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

    let mut child = cmd.spawn()?;
    let (sender, receiver) = mpsc::channel();

    if let Some(stdout) = child.stdout.take() {
        spawn_pipe_reader(stdout, "STDOUT", sender.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_pipe_reader(stderr, "STDERR", sender);
    }

    Ok((child, receiver))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// sing-box console lines carry the level as a quoted JSON-ish token,
    /// e.g. `... "level":"error" ...` — the classifier keys on the quoted
    /// word, so an unquoted mention (a URL containing "error") stays Info.
    #[test]
    fn classifies_quoted_levels() {
        assert_eq!(classify_log_level(r#"{"level":"error","msg":"x"}"#), LogLevel::Error);
        assert_eq!(classify_log_level(r#"{"level":"fatal","msg":"x"}"#), LogLevel::Error);
        assert_eq!(classify_log_level(r#"{"level":"warn","msg":"x"}"#), LogLevel::Warn);
        assert_eq!(classify_log_level(r#"{"level":"info","msg":"x"}"#), LogLevel::Info);
    }

    #[test]
    fn unquoted_keywords_do_not_escalate() {
        assert_eq!(classify_log_level("GET https://example.com/error/page"), LogLevel::Info);
        assert_eq!(classify_log_level("warning: something"), LogLevel::Info);
        assert_eq!(classify_log_level(""), LogLevel::Info);
    }

    #[test]
    fn error_takes_precedence_over_warn() {
        assert_eq!(
            classify_log_level(r#""level":"error" after a "warn" retry"#),
            LogLevel::Error
        );
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

    #[test]
    fn pipe_reader_forwards_lines_with_prefix_and_level() {
        let (sender, receiver) = mpsc::channel();
        let input: &[u8] = b"plain line\n\"error\" line\n";
        spawn_pipe_reader(input, "STDOUT", sender);

        let first = receiver.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert_eq!(first.message, "[STDOUT] plain line");
        assert_eq!(first.level, LogLevel::Info);

        let second = receiver.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert_eq!(second.message, "[STDOUT] \"error\" line");
        assert_eq!(second.level, LogLevel::Error);

        // Pipe exhausted -> reader thread exits -> channel disconnects.
        assert!(receiver.recv_timeout(std::time::Duration::from_secs(5)).is_err());
    }
}
