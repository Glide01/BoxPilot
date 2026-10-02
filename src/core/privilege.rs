//! TUN permission on Linux. BoxPilot runs as a normal user, but sing-box
//! needs CAP_NET_ADMIN to open a TUN device. The bundled sing-box can't be
//! granted it in place — an AppImage is a read-only, nosuid squashfs mount —
//! so a copy is installed to a root-owned fixed path and given file
//! capabilities there, once, through the system password prompt (pkexec).
//! The copy is re-granted whenever its version stops matching the bundled
//! sing-box (an AppImage update). Proxy mode never needs any of this.
//!
//! The decision and the command are pure fns, unit-tested; the rest is a
//! thin shell over `getxattr`, `geteuid` and `pkexec`. No gpui dependency.

use crate::core::process::query_sing_box_version;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The granted copy. Root-owned file and directory, so a user process can't
/// swap a different program in under the capabilities.
pub const PRIVILEGED_COPY_PATH: &str = "/usr/local/lib/boxpilot/sing-box";

/// The capabilities granted to the copy: TUN + routes (`net_admin`), ports
/// below 1024 (`net_bind_service`), raw sockets for ICMP (`net_raw`).
const GRANTED_CAPS: &str = "cap_net_admin,cap_net_bind_service,cap_net_raw+ep";

/// `security.capability` layout constants (`linux/capability.h`).
const VFS_CAP_REVISION_MASK: u32 = 0xFF00_0000;
const VFS_CAP_REVISION_1: u32 = 0x0100_0000;
const VFS_CAP_REVISION_2: u32 = 0x0200_0000;
const VFS_CAP_REVISION_3: u32 = 0x0300_0000;
const VFS_CAP_FLAGS_EFFECTIVE: u32 = 0x0000_0001;
const CAP_NET_ADMIN: u32 = 12;

/// Which sing-box a TUN-mode start runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunPlan {
    /// BoxPilot itself runs as root: the bundled sing-box inherits it.
    UseBundled(PathBuf),
    /// The granted copy is current and still holds its capabilities.
    UsePrivilegedCopy(PathBuf),
    /// No usable copy: ask the user to grant (install + setcap) first.
    NeedsGrant,
}

/// What is at [`PRIVILEGED_COPY_PATH`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopyState {
    Missing,
    Present {
        /// Its `sing-box version`; `None` if the probe failed.
        version: Option<String>,
        has_net_admin: bool,
    },
}

/// Pure decision for a TUN-mode start. The copy is only trusted when its
/// version matches the bundled one: an unknown bundled version can't be
/// matched, so it asks for a grant rather than run a possibly stale copy.
pub fn tun_launch_plan(
    is_root: bool,
    bundled: &Path,
    bundled_version: Option<&str>,
    copy_state: CopyState,
) -> TunPlan {
    if is_root {
        return TunPlan::UseBundled(bundled.to_path_buf());
    }
    match (bundled_version, copy_state) {
        (
            Some(bundled_version),
            CopyState::Present {
                version: Some(version),
                has_net_admin: true,
            },
        ) if version == bundled_version => {
            TunPlan::UsePrivilegedCopy(PathBuf::from(PRIVILEGED_COPY_PATH))
        }
        _ => TunPlan::NeedsGrant,
    }
}

/// Probe everything [`tun_launch_plan`] needs and decide. Blocking (runs
/// `sing-box version` up to twice): call on the background executor.
/// `bundled_version` is the startup probe's result; `None` (not landed yet,
/// or failed) probes again here.
pub fn evaluate_tun_plan(bundled: &Path, bundled_version: Option<String>) -> TunPlan {
    // SAFETY: geteuid has no preconditions and cannot fail.
    let is_root = unsafe { libc::geteuid() } == 0;
    if is_root {
        return tun_launch_plan(true, bundled, None, CopyState::Missing);
    }
    let bundled_version = bundled_version.or_else(|| query_sing_box_version(bundled));
    let copy = Path::new(PRIVILEGED_COPY_PATH);
    let copy_state = if copy.is_file() {
        CopyState::Present {
            version: query_sing_box_version(copy),
            has_net_admin: has_net_admin_cap(copy),
        }
    } else {
        CopyState::Missing
    };
    tun_launch_plan(false, bundled, bundled_version.as_deref(), copy_state)
}

/// Whether `path`'s file capabilities grant CAP_NET_ADMIN as permitted +
/// effective. Any read error (no xattr, no file) is `false`.
pub fn has_net_admin_cap(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    let mut buf = [0u8; 32];
    // SAFETY: both strings are NUL-terminated and `buf` outlives the call,
    // which writes at most `buf.len()` bytes.
    let len = unsafe {
        libc::getxattr(
            c_path.as_ptr(),
            c"security.capability".as_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    };
    if len < 0 {
        return false;
    }
    caps_grant_net_admin(&buf[..len as usize])
}

/// Parse a `security.capability` value (`struct vfs_cap_data`, revision
/// 1/2/3: `magic_etc`, then little-endian permitted/inheritable u32 pairs,
/// v3 adds a trailing `rootid`). CAP_NET_ADMIN sits in the first permitted
/// word; without the effective flag it isn't raised at exec.
fn caps_grant_net_admin(data: &[u8]) -> bool {
    let word = |i: usize| {
        data.get(i * 4..i * 4 + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    let Some(magic_etc) = word(0) else {
        return false;
    };
    let expected_len = match magic_etc & VFS_CAP_REVISION_MASK {
        VFS_CAP_REVISION_1 => 12,
        VFS_CAP_REVISION_2 => 20,
        VFS_CAP_REVISION_3 => 24,
        _ => return false,
    };
    if data.len() != expected_len || magic_etc & VFS_CAP_FLAGS_EFFECTIVE == 0 {
        return false;
    }
    let Some(permitted) = word(1) else {
        return false;
    };
    permitted & (1 << CAP_NET_ADMIN) != 0
}

/// `pkexec` argv installing `bundled` as the copy and granting it. The copy
/// is `root:<gid>` mode 0750: only the granting user's group can run a
/// CAP_NET_ADMIN sing-box, not every local account. (Another user, without
/// access, fails the version probe and gets offered its own grant.) The
/// path and gid go in as `$1` / `$2`, never into the script text, so no
/// quoting of a user-controlled path can change what runs as root.
pub fn grant_command(bundled: &Path, gid: u32) -> (String, Vec<String>) {
    let script = format!(
        "install -D -m 0750 -o root -g \"$2\" \"$1\" {copy} && setcap {caps} {copy}",
        copy = PRIVILEGED_COPY_PATH,
        caps = GRANTED_CAPS,
    );
    (
        "pkexec".to_string(),
        vec![
            "sh".to_string(),
            "-c".to_string(),
            script,
            "sh".to_string(),
            bundled.to_string_lossy().into_owned(),
            gid.to_string(),
        ],
    )
}

/// Install and grant the copy through pkexec, for the caller's primary
/// group. Blocks until the user has answered the password prompt: call on
/// the background executor. `Err` is a short message for a toast.
pub fn run_grant(bundled: &Path) -> Result<(), String> {
    // SAFETY: getgid has no preconditions and cannot fail.
    let gid = unsafe { libc::getgid() };
    let (program, args) = grant_command(bundled, gid);
    let output = match Command::new(&program).args(&args).output() {
        Ok(output) => output,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(
                "pkexec not found. Install polkit to grant TUN permission.".to_string(),
            );
        }
        Err(e) => return Err(format!("Failed to run pkexec: {}", e)),
    };
    if output.status.success() {
        return Ok(());
    }
    Err(grant_error_message(
        output.status.code(),
        &String::from_utf8_lossy(&output.stderr),
    ))
}

/// pkexec exits 126 when the prompt was dismissed and 127 when it could not
/// authorize (denied, no polkit agent); anything else is the script's own
/// exit code (e.g. `setcap` failing). The first stderr line says more than
/// the number — except pkexec's own "Not authorized", which reads better
/// rephrased.
fn grant_error_message(code: Option<i32>, stderr: &str) -> String {
    let detail = stderr.lines().map(str::trim).find(|l| !l.is_empty());
    match (code, detail) {
        (Some(126), _) => {
            "TUN permission was not granted: the password prompt was dismissed.".to_string()
        }
        (Some(127), None) => "TUN permission was not granted: not authorized.".to_string(),
        (Some(127), Some(detail)) if detail.contains("Not authorized") => {
            "TUN permission was not granted: not authorized.".to_string()
        }
        (_, Some(detail)) => format!("Failed to grant TUN permission: {}", detail),
        (Some(code), None) => format!("Failed to grant TUN permission (exit code {}).", code),
        (None, None) => "Failed to grant TUN permission: pkexec was terminated.".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps_bytes(magic_etc: u32, permitted: u32, rootid: Option<u32>) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend(magic_etc.to_le_bytes());
        out.extend(permitted.to_le_bytes());
        out.extend(0u32.to_le_bytes()); // inheritable[0]
        if magic_etc & VFS_CAP_REVISION_MASK != VFS_CAP_REVISION_1 {
            out.extend(0u32.to_le_bytes()); // permitted[1]
            out.extend(0u32.to_le_bytes()); // inheritable[1]
        }
        if let Some(rootid) = rootid {
            out.extend(rootid.to_le_bytes());
        }
        out
    }

    /// What `setcap cap_net_admin,cap_net_bind_service,cap_net_raw+ep` writes.
    const GRANTED: u32 = (1 << 10) | (1 << 12) | (1 << 13);

    #[test]
    fn parses_v2_with_net_admin_effective() {
        let data = caps_bytes(VFS_CAP_REVISION_2 | VFS_CAP_FLAGS_EFFECTIVE, GRANTED, None);
        assert_eq!(data.len(), 20);
        assert!(caps_grant_net_admin(&data));
    }

    #[test]
    fn parses_v3_with_net_admin_effective() {
        let data = caps_bytes(VFS_CAP_REVISION_3 | VFS_CAP_FLAGS_EFFECTIVE, GRANTED, Some(0));
        assert_eq!(data.len(), 24);
        assert!(caps_grant_net_admin(&data));
    }

    #[test]
    fn parses_v1() {
        let data = caps_bytes(VFS_CAP_REVISION_1 | VFS_CAP_FLAGS_EFFECTIVE, GRANTED, None);
        assert_eq!(data.len(), 12);
        assert!(caps_grant_net_admin(&data));
    }

    #[test]
    fn rejects_missing_effective_flag() {
        // `setcap cap_net_admin+p`: permitted but never raised at exec.
        let data = caps_bytes(VFS_CAP_REVISION_2, GRANTED, None);
        assert!(!caps_grant_net_admin(&data));
        let data = caps_bytes(VFS_CAP_REVISION_3, GRANTED, Some(0));
        assert!(!caps_grant_net_admin(&data));
    }

    #[test]
    fn rejects_missing_net_admin_bit() {
        let only_bind_and_raw = (1 << 10) | (1 << 13);
        let data = caps_bytes(
            VFS_CAP_REVISION_2 | VFS_CAP_FLAGS_EFFECTIVE,
            only_bind_and_raw,
            None,
        );
        assert!(!caps_grant_net_admin(&data));
    }

    #[test]
    fn rejects_malformed() {
        assert!(!caps_grant_net_admin(&[]));
        assert!(!caps_grant_net_admin(&[1, 0, 0]));
        // Unknown revision.
        let data = caps_bytes(0x0400_0000 | VFS_CAP_FLAGS_EFFECTIVE, GRANTED, None);
        assert!(!caps_grant_net_admin(&data));
        // v2 magic with a v3-sized value.
        let data = caps_bytes(VFS_CAP_REVISION_2 | VFS_CAP_FLAGS_EFFECTIVE, GRANTED, Some(0));
        assert!(!caps_grant_net_admin(&data));
        // Truncated v3.
        let data = caps_bytes(VFS_CAP_REVISION_3 | VFS_CAP_FLAGS_EFFECTIVE, GRANTED, Some(0));
        assert!(!caps_grant_net_admin(&data[..20]));
    }

    #[test]
    fn missing_file_has_no_cap() {
        assert!(!has_net_admin_cap(Path::new("/nonexistent/boxpilot/sing-box")));
    }

    /// Reads capabilities the kernel itself wrote. Needs root and `setcap`
    /// (libcap2-bin): `cargo test -- --ignored real_file_capabilities`.
    #[test]
    #[ignore]
    fn real_file_capabilities() {
        let dir = std::env::temp_dir().join(format!("boxpilot-caps-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let setcap = |caps: &str, file: &Path| {
            let status = Command::new("setcap").arg(caps).arg(file).status().unwrap();
            assert!(status.success(), "setcap {} failed", caps);
        };
        let file = |name: &str| {
            let path = dir.join(name);
            std::fs::write(&path, b"#!/bin/sh\n").unwrap();
            path
        };

        let plain = file("plain");
        assert!(!has_net_admin_cap(&plain));

        let granted = file("granted");
        setcap(GRANTED_CAPS, &granted);
        assert!(has_net_admin_cap(&granted));

        let not_effective = file("not-effective");
        setcap("cap_net_admin+p", &not_effective);
        assert!(!has_net_admin_cap(&not_effective));

        let other_cap = file("other-cap");
        setcap("cap_net_raw+ep", &other_cap);
        assert!(!has_net_admin_cap(&other_cap));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn present(version: &str, has_net_admin: bool) -> CopyState {
        CopyState::Present {
            version: Some(version.to_string()),
            has_net_admin,
        }
    }

    #[test]
    fn plan_root_uses_bundled() {
        let bundled = Path::new("/tmp/.mount_x/usr/bin/sing-box");
        assert_eq!(
            tun_launch_plan(true, bundled, None, CopyState::Missing),
            TunPlan::UseBundled(bundled.to_path_buf())
        );
        assert_eq!(
            tun_launch_plan(true, bundled, Some("1.14.0"), present("1.13.0", false)),
            TunPlan::UseBundled(bundled.to_path_buf())
        );
    }

    #[test]
    fn plan_uses_current_granted_copy() {
        let bundled = Path::new("/opt/boxpilot/sing-box");
        assert_eq!(
            tun_launch_plan(false, bundled, Some("1.14.0"), present("1.14.0", true)),
            TunPlan::UsePrivilegedCopy(PathBuf::from(PRIVILEGED_COPY_PATH))
        );
    }

    #[test]
    fn plan_needs_grant_otherwise() {
        let bundled = Path::new("/opt/boxpilot/sing-box");
        let cases = [
            (Some("1.14.0"), CopyState::Missing),
            // An update: the copy is the previous version.
            (Some("1.14.0"), present("1.13.0", true)),
            // Copied but the setcap half failed / caps stripped.
            (Some("1.14.0"), present("1.14.0", false)),
            (
                Some("1.14.0"),
                CopyState::Present {
                    version: None,
                    has_net_admin: true,
                },
            ),
            // Bundled version unknown: can't vouch for the copy.
            (None, present("1.14.0", true)),
        ];
        for (bundled_version, copy_state) in cases {
            assert_eq!(
                tun_launch_plan(false, bundled, bundled_version, copy_state.clone()),
                TunPlan::NeedsGrant,
                "{:?} / {:?}",
                bundled_version,
                copy_state
            );
        }
    }

    #[test]
    fn grant_command_passes_path_and_gid_as_positional_args() {
        let bundled = Path::new("/tmp/.mount_Box it'\"$(x)/usr/bin/sing-box");
        let (program, args) = grant_command(bundled, 1000);
        assert_eq!(program, "pkexec");
        assert_eq!(
            args,
            vec![
                "sh",
                "-c",
                "install -D -m 0750 -o root -g \"$2\" \"$1\" /usr/local/lib/boxpilot/sing-box \
                 && setcap cap_net_admin,cap_net_bind_service,cap_net_raw+ep \
                 /usr/local/lib/boxpilot/sing-box",
                "sh",
                "/tmp/.mount_Box it'\"$(x)/usr/bin/sing-box",
                "1000",
            ]
        );
    }

    #[test]
    fn grant_errors_are_short() {
        assert_eq!(
            grant_error_message(Some(126), ""),
            "TUN permission was not granted: the password prompt was dismissed."
        );
        assert_eq!(
            grant_error_message(
                Some(127),
                "Error executing command as another user: Not authorized\n\n\
                 This incident has been reported.\n"
            ),
            "TUN permission was not granted: not authorized."
        );
        assert_eq!(
            grant_error_message(Some(127), ""),
            "TUN permission was not granted: not authorized."
        );
        assert_eq!(
            grant_error_message(
                Some(127),
                "Error executing command as another user: No authentication agent found.\n"
            ),
            "Failed to grant TUN permission: Error executing command as another user: \
             No authentication agent found."
        );
        assert_eq!(
            grant_error_message(Some(127), "sh: 1: setcap: not found\n"),
            "Failed to grant TUN permission: sh: 1: setcap: not found"
        );
        assert_eq!(
            grant_error_message(Some(1), "Failed to set capabilities on file\n"),
            "Failed to grant TUN permission: Failed to set capabilities on file"
        );
        assert_eq!(
            grant_error_message(Some(2), ""),
            "Failed to grant TUN permission (exit code 2)."
        );
        assert_eq!(
            grant_error_message(None, ""),
            "Failed to grant TUN permission: pkexec was terminated."
        );
    }
}
