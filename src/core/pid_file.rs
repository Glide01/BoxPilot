//! The pid file that lets a start stop a sing-box orphaned by a BoxPilot
//! crash (Linux and macOS).
//!
//! sing-box holds the local proxy port, the system proxy it set and, in TUN
//! mode, the TUN device and routes. If BoxPilot dies without stopping it,
//! the next start fails on all of them. Linux usually avoids the orphan with
//! `PR_SET_PDEATHSIG` (set in `start_sing_box`), but the kernel clears that
//! when it execs the granted copy with file capabilities (ADR 0003); macOS
//! has no such signal at all (ADR 0005). So every start records the pid in
//! `<data dir>/sing-box.pid`, and the next start stops a recorded process
//! that is still ours ([`stop_stale_sing_box`]).
//!
//! "Ours" is decided from the process itself, never from the pid alone (pids
//! are reused): a pure rule per platform, unit-tested on every Unix, over
//! what `/proc` (Linux) or `proc_pidpath` + `KERN_PROCARGS2` (macOS) say.

use crate::core::paths::get_install_dir;
use crate::core::settings::SING_EXECUTABLE;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const PID_FILENAME: &str = "sing-box.pid";

/// Note a freshly started sing-box. Best effort: without the file only the
/// crash recovery is lost.
pub fn record_sing_box_pid(working_dir: &Path, pid: u32) {
    let _ = std::fs::write(working_dir.join(PID_FILENAME), format!("{}\n", pid));
}

/// Drop the record after a stop has reaped `pid`. Leaves a file naming some
/// other pid alone (a newer start already wrote its own).
pub fn forget_sing_box_pid(working_dir: &Path, pid: u32) {
    let path = working_dir.join(PID_FILENAME);
    let recorded = std::fs::read_to_string(&path).ok();
    if recorded.as_deref().and_then(parse_pid) == Some(pid as libc::pid_t) {
        let _ = std::fs::remove_file(path);
    }
}

/// Before a start: stop the sing-box a crashed BoxPilot left behind, if the
/// recorded pid is still one of ours — SIGTERM, up to `grace` to exit, then
/// SIGKILL. Never signals a pid that isn't (pid reuse). Blocking: call on
/// the background executor. The orphan runs as the same uid, so it can be
/// signalled without privilege.
pub fn stop_stale_sing_box(working_dir: &Path, grace: Duration) {
    let path = working_dir.join(PID_FILENAME);
    let Some(pid) = std::fs::read_to_string(&path).ok().as_deref().and_then(parse_pid) else {
        let _ = std::fs::remove_file(&path);
        return;
    };
    let candidates = candidates();
    let ours = || is_ours(pid, &candidates, working_dir);
    let wait_gone = |limit: Duration| {
        let deadline = Instant::now() + limit;
        while ours() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
    };

    if ours() {
        eprintln!("Stopping sing-box (pid {}) left running by a previous BoxPilot", pid);
        // SAFETY: plain syscalls; `pid` is positive and was just confirmed
        // to be our sing-box.
        unsafe { libc::kill(pid, libc::SIGTERM) };
        wait_gone(grace);
        if ours() {
            unsafe { libc::kill(pid, libc::SIGKILL) };
            // Let the kernel release its TUN device and ports before the
            // new sing-box asks for them.
            wait_gone(Duration::from_secs(1));
        }
    }
    let _ = std::fs::remove_file(&path);
}

/// The sing-box binaries BoxPilot starts: the bundled one next to its own
/// executable and, on Linux, the granted TUN copy.
fn candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    #[cfg(target_os = "linux")]
    candidates.push(PathBuf::from(crate::core::privilege::PRIVILEGED_COPY_PATH));
    if let Ok(install_dir) = get_install_dir() {
        candidates.push(install_dir.join(SING_EXECUTABLE));
    }
    candidates
}

#[cfg(target_os = "linux")]
fn is_ours(pid: libc::pid_t, candidates: &[PathBuf], working_dir: &Path) -> bool {
    let exe = std::fs::read_link(format!("/proc/{}/exe", pid)).ok();
    let cmdline = std::fs::read(format!("/proc/{}/cmdline", pid)).unwrap_or_default();
    is_our_sing_box_linux(exe.as_deref(), &cmdline, candidates, working_dir)
}

#[cfg(target_os = "macos")]
fn is_ours(pid: libc::pid_t, candidates: &[PathBuf], working_dir: &Path) -> bool {
    let exe = macos::executable_path(pid);
    let args = macos::arguments(pid).unwrap_or_default();
    let argv = parse_procargs2(&args).unwrap_or_default();
    is_our_sing_box_macos(exe.as_deref(), &argv, candidates, working_dir)
}

/// A positive pid, or nothing: 0 and negatives would make `kill` signal a
/// whole process group.
fn parse_pid(text: &str) -> Option<libc::pid_t> {
    text.trim().parse::<libc::pid_t>().ok().filter(|pid| *pid > 0)
}

/// Whether `args` (argv) carries `-D <working_dir>`: BoxPilot always passes
/// its data dir, so another data dir is another user's or install's.
fn runs_in(args: &[&[u8]], working_dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let dir = working_dir.as_os_str().as_bytes();
    args.windows(2).any(|w| w[0] == b"-D" && w[1] == dir)
}

/// Linux: whether a process is a sing-box BoxPilot started: its executable
/// is one of `candidates`. A process that gained file capabilities at exec
/// is non-dumpable, so its `/proc/<pid>/exe` is unreadable even to the same
/// user — exactly the granted copy this exists for. Then its command line
/// decides: argv[0] a candidate (`start_sing_box` spawns by full path) and
/// `-D` our data dir. A readable exe always decides alone.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn is_our_sing_box_linux(
    exe: Option<&Path>,
    cmdline: &[u8],
    candidates: &[PathBuf],
    working_dir: &Path,
) -> bool {
    use std::os::unix::ffi::OsStrExt;
    if let Some(exe) = exe {
        // The binary may have been replaced since (a re-grant reinstalls
        // the copy); the kernel then appends " (deleted)".
        let exe = exe.as_os_str().as_bytes();
        let exe = exe.strip_suffix(b" (deleted)").unwrap_or(exe);
        return candidates.iter().any(|c| c.as_os_str().as_bytes() == exe);
    }
    let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
    let Some(argv0) = args.first() else {
        return false;
    };
    candidates.iter().any(|c| c.as_os_str().as_bytes() == *argv0) && runs_in(&args, working_dir)
}

/// macOS: whether a process is a sing-box BoxPilot started. Its executable
/// path (`proc_pidpath`, readable for the same user) is one of `candidates`;
/// or, for a BoxPilot that has moved since — dragged elsewhere, or run
/// from a randomized App Translocation path, which changes every launch —
/// it is some `sing-box` running in our data dir (`-D`). No executable
/// path: gone, or not ours to inspect.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn is_our_sing_box_macos(
    exe: Option<&Path>,
    args: &[&[u8]],
    candidates: &[PathBuf],
    working_dir: &Path,
) -> bool {
    let Some(exe) = exe else {
        return false;
    };
    if candidates.iter().any(|c| c == exe) {
        return true;
    }
    exe.file_name().is_some_and(|name| name == SING_EXECUTABLE) && runs_in(args, working_dir)
}

/// The argv in a `KERN_PROCARGS2` buffer: a native-endian `int` argc, the
/// executable path, NUL padding, then argc NUL-terminated arguments (the
/// environment follows; it is ignored). `None` if the buffer is malformed.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_procargs2(buf: &[u8]) -> Option<Vec<&[u8]>> {
    let argc = i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?);
    let argc = usize::try_from(argc).ok()?;
    let rest = &buf[4..];
    // Skip the executable path, then its padding.
    let path_end = rest.iter().position(|b| *b == 0)?;
    let mut rest = &rest[path_end..];
    let first_arg = rest.iter().position(|b| *b != 0)?;
    rest = &rest[first_arg..];
    let mut args = Vec::with_capacity(argc);
    for _ in 0..argc {
        let end = rest.iter().position(|b| *b == 0)?;
        args.push(&rest[..end]);
        rest = &rest[end + 1..];
    }
    Some(args)
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;

    /// The process's executable, as the kernel knows it.
    pub fn executable_path(pid: libc::pid_t) -> Option<PathBuf> {
        let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // SAFETY: `buf` is writable for the size passed, and outlives the
        // call.
        let len = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
        if len <= 0 {
            return None;
        }
        buf.truncate(len as usize);
        Some(PathBuf::from(OsStr::from_bytes(&buf)))
    }

    /// The raw `KERN_PROCARGS2` buffer of a process (argv and environment).
    /// Only readable for the same user's processes.
    pub fn arguments(pid: libc::pid_t) -> Option<Vec<u8>> {
        let mut arg_max: libc::c_int = 0;
        let mut size = std::mem::size_of::<libc::c_int>();
        let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
        // SAFETY: `mib` names a readable int sysctl, `arg_max` and `size`
        // match it.
        let ok = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as libc::c_uint,
                (&mut arg_max as *mut libc::c_int).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        } == 0;
        if !ok || arg_max <= 0 {
            return None;
        }
        let mut buf = vec![0u8; arg_max as usize];
        let mut size = buf.len();
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        // SAFETY: `buf` is writable for `size` bytes; the kernel writes at
        // most that and updates `size`.
        let ok = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as libc::c_uint,
                buf.as_mut_ptr().cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        } == 0;
        if !ok {
            return None;
        }
        buf.truncate(size);
        Some(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COPY: &str = "/usr/local/lib/boxpilot/sing-box";

    fn candidates() -> Vec<PathBuf> {
        vec![
            PathBuf::from(COPY),
            PathBuf::from("/tmp/.mount_BoxPiX/usr/bin/sing-box"),
        ]
    }

    fn cmdline(args: &[&str]) -> Vec<u8> {
        args.iter().flat_map(|a| a.bytes().chain([0])).collect()
    }

    const DATA: &str = "/home/u/.config/BoxPilot";

    #[test]
    fn linux_readable_exe_decides() {
        let data = Path::new(DATA);
        let ours = cmdline(&[COPY, "run", "-D", DATA, "-c", "x.json"]);
        for exe in [
            COPY,
            "/tmp/.mount_BoxPiX/usr/bin/sing-box",
            // Re-granted since: the running image's file was replaced.
            "/usr/local/lib/boxpilot/sing-box (deleted)",
        ] {
            assert!(
                is_our_sing_box_linux(Some(Path::new(exe)), b"", &candidates(), data),
                "{exe}"
            );
        }
        // Pid reused by something else: a matching-looking cmdline can't
        // outvote the exe.
        for exe in ["/usr/bin/bash", "/usr/local/lib/boxpilot/sing-box.bak", "/usr/bin/sing-box"] {
            assert!(
                !is_our_sing_box_linux(Some(Path::new(exe)), &ours, &candidates(), data),
                "{exe}"
            );
        }
    }

    #[test]
    fn linux_unreadable_exe_falls_back_to_cmdline() {
        let data = Path::new(DATA);
        let check =
            |args: &[&str]| is_our_sing_box_linux(None, &cmdline(args), &candidates(), data);
        assert!(check(&[COPY, "run", "-D", DATA, "-c", "x.json"]));
        // Another user's / data dir's sing-box.
        assert!(!check(&[COPY, "run", "-D", "/home/v/.config/BoxPilot"]));
        // Some other program, or another sing-box binary.
        assert!(!check(&["/usr/bin/sleep", "-D", DATA]));
        assert!(!check(&["sing-box", "run", "-D", DATA]));
        // `-D` must be followed by the dir, not just mention it.
        assert!(!check(&[COPY, "run", DATA]));
        // Gone, or a zombie (empty cmdline).
        assert!(!is_our_sing_box_linux(None, b"", &candidates(), data));
    }

    const MAC_DATA: &str = "/Users/u/Library/Application Support/BoxPilot";
    const MAC_BUNDLED: &str = "/Applications/BoxPilot.app/Contents/MacOS/sing-box";

    #[test]
    fn macos_candidate_exe_decides() {
        let data = Path::new(MAC_DATA);
        let candidates = vec![PathBuf::from(MAC_BUNDLED)];
        assert!(is_our_sing_box_macos(Some(Path::new(MAC_BUNDLED)), &[], &candidates, data));
        // No path: gone (or another user's).
        let args: Vec<&[u8]> = vec![MAC_BUNDLED.as_bytes(), b"run", b"-D", MAC_DATA.as_bytes()];
        assert!(!is_our_sing_box_macos(None, &args, &candidates, data));
    }

    #[test]
    fn macos_moved_app_counts_when_it_runs_in_our_data_dir() {
        let data = Path::new(MAC_DATA);
        let candidates = vec![PathBuf::from(MAC_BUNDLED)];
        let translocated =
            "/private/var/folders/xy/T/AppTranslocation/1234/d/BoxPilot.app/Contents/MacOS/sing-box";
        let ours: Vec<&[u8]> = vec![translocated.as_bytes(), b"run", b"-D", MAC_DATA.as_bytes()];
        assert!(is_our_sing_box_macos(Some(Path::new(translocated)), &ours, &candidates, data));
        // Another data dir, or no `-D` pair at all.
        let other: Vec<&[u8]> = vec![translocated.as_bytes(), b"run", b"-D", b"/Users/v/x"];
        assert!(!is_our_sing_box_macos(Some(Path::new(translocated)), &other, &candidates, data));
        let loose: Vec<&[u8]> = vec![translocated.as_bytes(), b"run", MAC_DATA.as_bytes()];
        assert!(!is_our_sing_box_macos(Some(Path::new(translocated)), &loose, &candidates, data));
        // Pid reused by another program that happens to look alike.
        let bash: Vec<&[u8]> = vec![b"/bin/bash", b"-D", MAC_DATA.as_bytes()];
        assert!(!is_our_sing_box_macos(Some(Path::new("/bin/bash")), &bash, &candidates, data));
    }

    fn procargs2(argc: i32, exec_path: &str, padding: usize, rest: &[&str]) -> Vec<u8> {
        let mut buf = argc.to_ne_bytes().to_vec();
        buf.extend(exec_path.bytes());
        buf.extend(std::iter::repeat_n(0, 1 + padding));
        for item in rest {
            buf.extend(item.bytes());
            buf.push(0);
        }
        buf
    }

    #[test]
    fn procargs2_yields_argv_without_the_environment() {
        let buf = procargs2(
            5,
            MAC_BUNDLED,
            3,
            &[MAC_BUNDLED, "run", "-D", MAC_DATA, "-c", "HOME=/Users/u", "TMPDIR=/x"],
        );
        let args = parse_procargs2(&buf).unwrap();
        assert_eq!(
            args,
            vec![
                MAC_BUNDLED.as_bytes(),
                b"run",
                b"-D",
                MAC_DATA.as_bytes(),
                b"-c"
            ]
        );
        assert_eq!(parse_procargs2(&procargs2(0, "/bin/x", 0, &["ENV=1"])).unwrap().len(), 0);
    }

    #[test]
    fn procargs2_rejects_malformed_buffers() {
        assert!(parse_procargs2(&[]).is_none());
        assert!(parse_procargs2(&[1, 0]).is_none());
        // argc says more arguments than the buffer holds.
        assert!(parse_procargs2(&procargs2(3, "/bin/x", 0, &["a", "b"])).is_none());
        assert!(parse_procargs2(&procargs2(-1, "/bin/x", 0, &["a"])).is_none());
    }

    #[test]
    fn pid_must_be_positive() {
        assert_eq!(parse_pid("1234\n"), Some(1234));
        assert_eq!(parse_pid(" 42 "), Some(42));
        assert_eq!(parse_pid("0"), None);
        assert_eq!(parse_pid("-1"), None);
        assert_eq!(parse_pid("-1234"), None);
        assert_eq!(parse_pid(""), None);
        assert_eq!(parse_pid("12ab"), None);
        assert_eq!(parse_pid("99999999999"), None);
    }

    #[test]
    fn pid_file_round_trip() {
        let dir = std::env::temp_dir().join(format!("boxpilot-pid-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(PID_FILENAME);

        record_sing_box_pid(&dir, 4321);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "4321\n");
        // A stop of some older run doesn't remove a newer record.
        forget_sing_box_pid(&dir, 1111);
        assert!(file.exists());
        forget_sing_box_pid(&dir, 4321);
        assert!(!file.exists());

        // A record whose pid is no sing-box of ours: nothing signalled, the
        // record is cleared. (pid 1 is never ours.)
        record_sing_box_pid(&dir, 1);
        stop_stale_sing_box(&dir, Duration::from_millis(100));
        assert!(!file.exists());
        std::fs::write(&file, "garbage").unwrap();
        stop_stale_sing_box(&dir, Duration::from_millis(100));
        assert!(!file.exists());

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
