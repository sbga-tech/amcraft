//! USB device blocks (`ServerData+0x4794`, 0x10; `ClientData+0x161C`, 0xC).
//!
//! Evidence: `spec/controllers/can_boardio_usbdevice.md` §3.2 (claims `can_boardio_usbdevice.usbdevice.*`).

use num_enum::TryFromPrimitive;
use zero_schema::zero;

use crate::schema::shared::Response;
use crate::schema::{client_block, server_block};

/// `UsbDeviceReconnectResult` published in `reconnect.lastResult`.
///
/// Values are **[D]** (`UsbDeviceEventListener::vf1` `0x140048080`, libam producer `0x14024CEA0`); the enumerator
/// names are **[I]**: the binary has no enum-to-string table for this type.
#[zero]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u32)]
pub enum UsbDeviceReconnectResult {
    /// `0`: initial value, or any libam completion code other than `1..=3`.
    #[default]
    None = 0,
    /// `1`: the amMaster driver call returned 0 (reconnect succeeded).
    Success = 1,
    /// `2`: the driver returned a generic non-zero error code.
    Failure = 2,
    /// `3`: the driver returned error code `0x81000015`.
    DriverError = 3,
}

impl UsbDeviceReconnectResult {
    /// Listener mapping (`0x140048080`): libam completion code `1 → 1`, `2 → 2`, `3 → 3`, else `0`.
    pub const fn from_completion_code(code: i32) -> Self {
        match code {
            1 => Self::Success,
            2 => Self::Failure,
            3 => Self::DriverError,
            _ => Self::None,
        }
    }
}

/// `usbdevice::ServerReconnectData` (0x8): asynchronous reconnect state.
#[zero(endian = "little")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerReconnect {
    /// Set by `accept` after libam accepted a reconnect, cleared by the completion listener.
    pub busy: bool,
    /// Last libam completion result; written only by the listener.
    pub last_result: UsbDeviceReconnectResult,
}

/// `ServerData.usbDevice`: the executor response and the asynchronous reconnect state.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerUsbDevice {
    /// Executor response (`usbdevice::ServerResponse`): generation + `succeeded`.
    pub response: Response,
    /// Asynchronous reconnect state.
    pub reconnect: ServerReconnect,
}

/// `ipc::usbdevice::Task` (name table `0x1405EEF40`); any other value is rejected as unknown task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, TryFromPrimitive)]
#[repr(u32)]
pub enum UsbDeviceTask {
    /// Reconnect the USB device identified by `info.vendorId`/`info.productId`.
    Reconnect = 0,
}

impl UsbDeviceTask {
    /// Name from the enum-to-string table `0x1405EEF40`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Reconnect => "Reconnect",
        }
    }
}

/// `usbdevice::ClientReconnectInfo` (0x4), the only member of the `ClientInfo` union.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ClientReconnectInfo {
    /// USB vendor id (never validated by amdaemon).
    pub vendor_id: u16,
    /// USB product id (never validated by amdaemon).
    pub product_id: u16,
}

/// `ClientData.usbDevice`: the only request (`usbdevice::ClientRequest`).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ClientUsbDevice {
    /// Request generation (`taskNo` in the old docs).
    pub generation: u32,
    /// Raw [`UsbDeviceTask`]; only `Reconnect` is accepted.
    pub task: u32,
    /// Per-task info.
    pub info: ClientReconnectInfo,
}

server_block!(usb_device, usb_device_mut: ServerUsbDevice, ServerUsbDeviceRef, ServerUsbDeviceMut @ 0x4794);
client_block!(usb_device, usb_device_mut: ClientUsbDevice, ClientUsbDeviceRef, ClientUsbDeviceMut @ 0x161C);

impl ClientUsbDeviceRef<'_> {
    /// The request's task, `None` for an unknown value (the request fails).
    pub fn task_kind(&self) -> Option<UsbDeviceTask> {
        UsbDeviceTask::try_from(self.task()).ok()
    }
}

const _: () = {
    assert!(ServerReconnect::SCHEMA_SIZE == 0x8);
    assert!(ServerUsbDevice::SCHEMA_SIZE == 0x10);
    assert!(ClientReconnectInfo::SCHEMA_SIZE == 0x4);
    assert!(ClientUsbDevice::SCHEMA_SIZE == 0xC);
};
