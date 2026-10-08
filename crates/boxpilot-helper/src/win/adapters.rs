//! Removing sing-box's stale wintun adapters (ADR 0006 rule 6, "Crash
//! cleanup belongs to the helper"): after every run, and when the helper
//! starts, in case the last run ended in a crash. Native SetupAPI, no child
//! process: the GUI's `process::remove_tun_adapter`, which stays there until
//! the GUI stops elevating. Which adapters is `tun::is_stale_sing_tun`: by
//! name, and only once no longer present, so another program's running
//! tunnel is never removed.
//!
//! Best effort: a failure is logged and the run goes on, since sing-box
//! creates its own adapter at start regardless.

use crate::helper_log;
use crate::tun::is_stale_sing_tun;
use std::mem::size_of;
use windows::core::PCWSTR;
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_DevNode_Status, DiUninstallDevice, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo,
    SetupDiGetClassDevsW, SetupDiGetDeviceRegistryPropertyW, CM_DEVNODE_STATUS_FLAGS, CM_PROB,
    CR_NO_SUCH_DEVINST, GUID_DEVCLASS_NET, HDEVINFO, SETUP_DI_GET_CLASS_DEVS_FLAGS,
    SPDRP_FRIENDLYNAME, SP_DEVINFO_DATA,
};
use windows::Win32::Foundation::{BOOL, HWND};

/// Every installed network-class device, present or not: flags 0, not
/// `DIGCF_PRESENT`, so the not-present "ghost" adapters a crash leaves
/// behind are found too.
struct DeviceSet(HDEVINFO);

impl DeviceSet {
    fn network() -> Option<Self> {
        // SAFETY: a class GUID that lives for the program, no enumerator and
        // no window; on success a new device information set, freed in Drop.
        match unsafe {
            SetupDiGetClassDevsW(
                Some(&GUID_DEVCLASS_NET),
                PCWSTR::null(),
                HWND::default(),
                SETUP_DI_GET_CLASS_DEVS_FLAGS(0),
            )
        } {
            Ok(set) => Some(Self(set)),
            Err(error) => {
                helper_log!("TUN cleanup: SetupDiGetClassDevsW failed: {error}");
                None
            }
        }
    }

    /// The device at `index`, or `None` past the end (or on an error).
    fn device(&self, index: u32) -> Option<SP_DEVINFO_DATA> {
        let mut data = SP_DEVINFO_DATA {
            cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
            ..Default::default()
        };
        // SAFETY: the set is open; `data` is a valid out-structure whose
        // `cbSize` is set.
        unsafe { SetupDiEnumDeviceInfo(self.0, index, &mut data) }.ok()?;
        Some(data)
    }

    /// A device's FriendlyName, if it has one.
    fn friendly_name(&self, device: &SP_DEVINFO_DATA) -> Option<String> {
        let mut needed = 0u32;
        // SAFETY: a size query with no buffer; `needed` receives the size.
        let _ = unsafe {
            SetupDiGetDeviceRegistryPropertyW(
                self.0,
                device,
                SPDRP_FRIENDLYNAME,
                None,
                None,
                Some(&mut needed),
            )
        };
        if needed == 0 {
            return None;
        }
        let mut buf = vec![0u8; needed as usize];
        // SAFETY: `buf` holds the `needed` bytes the query asked for.
        unsafe {
            SetupDiGetDeviceRegistryPropertyW(
                self.0,
                device,
                SPDRP_FRIENDLYNAME,
                None,
                Some(buf.as_mut_slice()),
                None,
            )
        }
        .ok()?;
        let utf16: Vec<u16> = buf
            .chunks_exact(2)
            .map(|pair| u16::from_ne_bytes([pair[0], pair[1]]))
            .collect();
        Some(
            String::from_utf16_lossy(&utf16)
                .trim_end_matches('\0')
                .to_owned(),
        )
    }

    /// Whether the device is present. Only `CR_NO_SUCH_DEVINST`, the
    /// answer for a device that is installed but not present (a "ghost"),
    /// says it isn't; any other answer counts as present, so a failure never
    /// removes a running program's adapter.
    fn present(device: &SP_DEVINFO_DATA) -> bool {
        let mut status = CM_DEVNODE_STATUS_FLAGS(0);
        let mut problem = CM_PROB(0);
        // SAFETY: both out-pointers are valid for the call; `DevInst` is the
        // device instance handle SetupDiEnumDeviceInfo filled in.
        let result = unsafe { CM_Get_DevNode_Status(&mut status, &mut problem, device.DevInst, 0) };
        result != CR_NO_SUCH_DEVINST
    }

    /// Uninstall the device (and its children). A non-null `NeedReboot`
    /// keeps it from ever showing a restart prompt; a virtual adapter never
    /// needs one.
    fn uninstall(&self, device: &SP_DEVINFO_DATA) -> windows::core::Result<()> {
        let mut need_reboot = BOOL(0);
        // SAFETY: the set is open and `device` belongs to it; no window.
        unsafe { DiUninstallDevice(HWND::default(), self.0, device, 0, Some(&mut need_reboot)) }
    }
}

impl Drop for DeviceSet {
    fn drop(&mut self) {
        // SAFETY: the set was returned by SetupDiGetClassDevsW and is freed
        // once, here.
        let _ = unsafe { SetupDiDestroyDeviceInfoList(self.0) };
    }
}

/// Uninstall every network adapter whose FriendlyName is sing-box's and
/// that is no longer present.
pub(crate) fn remove_sing_tun_adapters() {
    let Some(set) = DeviceSet::network() else {
        return;
    };
    // Uninstalling a device leaves its element in the in-memory set, so
    // walking by index stays valid across removals.
    let mut removed = 0u32;
    let mut index = 0u32;
    while let Some(device) = set.device(index) {
        index += 1;
        let Some(name) = set.friendly_name(&device) else {
            continue;
        };
        if !is_stale_sing_tun(&name, DeviceSet::present(&device)) {
            continue;
        }
        match set.uninstall(&device) {
            Ok(()) => removed += 1,
            Err(error) => helper_log!("TUN cleanup: could not remove {name:?}: {error}"),
        }
    }
    if removed > 0 {
        helper_log!("TUN cleanup: removed {removed} sing-tun adapter(s)");
    }
}
