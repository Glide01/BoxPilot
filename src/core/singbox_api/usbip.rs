//! USB/IP servers (`services[]` of type `usbip-server`):
//! `SubscribeUSBIPServerStatus`, read-only. `ProvideUSBDevices` (a
//! bidirectional stream that lends this machine's devices to a dynamic
//! server) is out of reach over gRPC-Web — see the ADR.
//!
//! Only servers with `"provider": "dynamic"` are reported: their devices
//! are the ones some client lends them through `ProvideUSBDevices`. A
//! default-provider server shares the host's matching devices itself and
//! has no status in the API.

use super::transport::{ApiError, IDLE_STREAM_READ_TIMEOUT};
use super::{pb, SingBoxApi};

impl SingBoxApi {
    /// Stream `SubscribeUSBIPServerStatus`: on subscribe, one update with
    /// every dynamic USB/IP server and its shared devices (an empty list
    /// when there is none), then a full update whenever a device is added,
    /// removed, attached or detached. Idle otherwise: `TimedOut` after
    /// `IDLE_STREAM_READ_TIMEOUT`; re-subscribe. A sing-box built without
    /// `with_usbip` answers `NOT_FOUND` at once.
    pub fn stream_usbip_server_status(
        &self,
        mut on_update: impl FnMut(Vec<UsbipServerStatus>) -> bool,
    ) -> Result<(), ApiError> {
        self.stream(
            "SubscribeUSBIPServerStatus",
            &(),
            IDLE_STREAM_READ_TIMEOUT,
            |update: pb::UsbipServerStatusUpdate| {
                on_update(
                    update
                        .servers
                        .into_iter()
                        .map(UsbipServerStatus::from_proto)
                        .collect(),
                )
            },
        )
    }
}

/// One dynamic USB/IP server.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UsbipServerStatus {
    pub server_tag: String,
    pub devices: Vec<UsbSharedDevice>,
}

impl UsbipServerStatus {
    fn from_proto(status: pb::UsbipServerStatus) -> Self {
        Self {
            server_tag: status.server_tag,
            devices: status
                .devices
                .into_iter()
                .map(UsbSharedDevice::from_proto)
                .collect(),
        }
    }
}

/// A device a USB/IP server offers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UsbSharedDevice {
    /// USB/IP bus id clients import it by (`1-2`).
    pub bus_id: String,
    pub stable_id: String,
    pub backend: UsbBackend,
    pub state: UsbDeviceState,
    pub vendor_id: u16,
    pub product_id: u16,
    /// Product string; may be empty.
    pub product: String,
    pub serial: String,
    /// Linux `usb_device_speed` (see `usb_speed_label`).
    pub speed: u32,
    pub device_class: u8,
}

impl UsbSharedDevice {
    fn from_proto(device: pb::UsbSharedDevice) -> Self {
        let descriptor = device.descriptor.unwrap_or_default();
        Self {
            bus_id: device.bus_id,
            stable_id: device.stable_id,
            backend: UsbBackend::from_proto(device.backend),
            state: UsbDeviceState::from_proto(device.state),
            vendor_id: descriptor.vendor_id as u16,
            product_id: descriptor.product_id as u16,
            product: descriptor.product,
            serial: descriptor.serial,
            speed: descriptor.speed,
            device_class: descriptor.device_class as u8,
        }
    }

    /// `vvvv:pppp`, lowercase hex — how USB ids are usually written.
    pub fn usb_id(&self) -> String {
        format!("{:04x}:{:04x}", self.vendor_id, self.product_id)
    }

    /// The product name, else "USB device".
    pub fn display_name(&self) -> &str {
        let product = self.product.trim();
        if product.is_empty() {
            "USB device"
        } else {
            product
        }
    }
}

/// Whether a client is using the device (`USBDeviceState`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UsbDeviceState {
    /// Offered, not attached by any client.
    #[default]
    Idle,
    /// A client has imported it.
    Attached,
    /// Known but can't be offered right now.
    Unavailable,
    Unknown(i32),
}

impl UsbDeviceState {
    fn from_proto(value: i32) -> Self {
        match value {
            0 => UsbDeviceState::Idle,
            1 => UsbDeviceState::Attached,
            2 => UsbDeviceState::Unavailable,
            other => UsbDeviceState::Unknown(other),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            UsbDeviceState::Idle => "Available",
            UsbDeviceState::Attached => "In use",
            UsbDeviceState::Unavailable => "Unavailable",
            UsbDeviceState::Unknown(_) => "Unknown",
        }
    }
}

/// Where the device physically lives (`USBBackend`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UsbBackend {
    #[default]
    Unspecified,
    LinuxSysfs,
    /// Lent by a client through `ProvideUSBDevices`.
    Dynamic,
    DarwinIoKit,
    WindowsVboxUsb,
    Unknown(i32),
}

impl UsbBackend {
    fn from_proto(value: i32) -> Self {
        match value {
            0 => UsbBackend::Unspecified,
            1 => UsbBackend::LinuxSysfs,
            2 => UsbBackend::Dynamic,
            3 => UsbBackend::DarwinIoKit,
            4 => UsbBackend::WindowsVboxUsb,
            other => UsbBackend::Unknown(other),
        }
    }
}

/// A Linux `usb_device_speed` as a link rate, or `None` when unknown.
pub fn usb_speed_label(speed: u32) -> Option<&'static str> {
    match speed {
        1 => Some("1.5 Mbps"),
        2 => Some("12 Mbps"),
        3 => Some("480 Mbps"),
        4 => Some("Wireless"),
        5 => Some("5 Gbps"),
        6 => Some("10 Gbps"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_status_maps_devices() {
        let status = UsbipServerStatus::from_proto(pb::UsbipServerStatus {
            server_tag: "usb".into(),
            devices: vec![pb::UsbSharedDevice {
                descriptor: Some(pb::UsbDeviceDescriptor {
                    device_id: "1-2".into(),
                    bus_num: 1,
                    dev_num: 2,
                    speed: 3,
                    vendor_id: 0x046d,
                    product_id: 0xc52b,
                    device_class: 0,
                    serial: "ABC".into(),
                    product: "USB Receiver".into(),
                    ..Default::default()
                }),
                bus_id: "1-2".into(),
                stable_id: "s1".into(),
                backend: 2,
                state: 1,
            }],
        });
        assert_eq!(status.server_tag, "usb");
        assert_eq!(
            status.devices,
            vec![UsbSharedDevice {
                bus_id: "1-2".into(),
                stable_id: "s1".into(),
                backend: UsbBackend::Dynamic,
                state: UsbDeviceState::Attached,
                vendor_id: 0x046d,
                product_id: 0xc52b,
                product: "USB Receiver".into(),
                serial: "ABC".into(),
                speed: 3,
                device_class: 0,
            }]
        );
        let device = &status.devices[0];
        assert_eq!(device.usb_id(), "046d:c52b");
        assert_eq!(device.display_name(), "USB Receiver");
    }

    #[test]
    fn missing_descriptor_and_unknown_enums_degrade_gracefully() {
        let device = UsbSharedDevice::from_proto(pb::UsbSharedDevice {
            descriptor: None,
            bus_id: "3-1".into(),
            stable_id: String::new(),
            backend: 9,
            state: 7,
        });
        assert_eq!(device.display_name(), "USB device");
        assert_eq!(device.usb_id(), "0000:0000");
        assert_eq!(device.backend, UsbBackend::Unknown(9));
        assert_eq!(device.state, UsbDeviceState::Unknown(7));
        assert_eq!(device.state.label(), "Unknown");
    }

    #[test]
    fn enums_map_upstream_values() {
        let states: Vec<UsbDeviceState> = (0..3).map(UsbDeviceState::from_proto).collect();
        assert_eq!(
            states,
            vec![
                UsbDeviceState::Idle,
                UsbDeviceState::Attached,
                UsbDeviceState::Unavailable
            ]
        );
        let backends: Vec<UsbBackend> = (0..5).map(UsbBackend::from_proto).collect();
        assert_eq!(
            backends,
            vec![
                UsbBackend::Unspecified,
                UsbBackend::LinuxSysfs,
                UsbBackend::Dynamic,
                UsbBackend::DarwinIoKit,
                UsbBackend::WindowsVboxUsb,
            ]
        );
        assert_eq!(usb_speed_label(3), Some("480 Mbps"));
        assert_eq!(usb_speed_label(0), None);
    }
}
