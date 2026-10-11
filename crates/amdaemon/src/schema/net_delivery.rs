//! NetDelivery blocks (`ServerData+0x370`, 0x60, and `ClientData+0x1810`, 0x20C).
//!
//! Evidence: `spec/controllers/network_install.md` §4.2 (`ipc::install::*`), claims `network_install.netdelivery.*`.

use num_enum::TryFromPrimitive;
use widestring::U16CStr;
use zero_schema::zero;

use crate::schema::shared::Response;
use crate::schema::{client_block, server_block};

/// A pair of libam download counters; the binary only fixes the order (`first` at the lower offset).
#[zero(endian = "little")]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProgressPair {
    /// First counter (which of total/current it is stays unresolved in the binary).
    pub first: u32,
    /// Second counter.
    pub second: u32,
}

/// `ServerNetDeliveryAppInfo` (0x28) at `netDelivery+0x10` (`SD+0x380`).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerNetDeliveryAppInfo {
    /// Order time from the libam time-info record (`struct tm` #0) [I name], `DateTime` microseconds.
    pub order_time: i64,
    /// Release time (`struct tm` #1) [I name], `DateTime` microseconds.
    pub release_time: i64,
    /// `mapStatus(image.rawStatus)` (`0x14003B0F0`); the published values `0..=7` have no names in the binary.
    pub status: u32,
    /// Image `pairA.pair1`.
    pub progress: ProgressPair,
    /// `amdaemon::Version` from `AppImageProperty::convert`.
    pub version: u32,
    /// Creation time from `AppImageProperty::convert`, `DateTime` microseconds.
    pub creation_time: i64,
}

/// `ServerNetDeliveryOptionInfo` (0x28) at `netDelivery+0x38` (`SD+0x3A8`).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerNetDeliveryOptionInfo {
    /// Order time [I name], `DateTime` microseconds.
    pub order_time: i64,
    /// Release time [I name], `DateTime` microseconds.
    pub release_time: i64,
    /// `mapStatus(image.rawStatus)`.
    pub status: u32,
    /// `(pairA - pairB).pair1`.
    pub progress: ProgressPair,
    /// `pairB.pair1`.
    pub progress2: ProgressPair,
}

/// `ServerData.netDelivery`: availability, the `ipc::install::ServerNetDeliveryResponse`, the NeedToExit flag
/// and the published state of the two libam images.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerNetDelivery {
    /// libam NetDeliver manager available flag, sampled at the ready edge.
    pub available: bool,
    /// `ipc::install::ServerNetDeliveryResponse` (abs. `SD+0x374`).
    pub response: Response,
    /// Set by NeedToExit reason 2 (`*(u8*)(SD+0x37C)`); no clearing path exists.
    pub exit_needed: bool,
    /// Application image.
    pub app: ServerNetDeliveryAppInfo,
    /// Option image.
    pub option: ServerNetDeliveryOptionInfo,
}

/// Capacity of each request path (`wchar_t[129]`: 128 characters plus NUL).
pub const PATH_CAPACITY: usize = 129;

/// Bytes of the `DownloadInstructionUri` payload: both paths back to back.
pub const INFO_SIZE: usize = 2 * PATH_CAPACITY * size_of::<u16>();

/// `ClientData.netDelivery`: `ipc::install::ClientNetDeliveryRequest`.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ClientNetDelivery<'b> {
    /// Request generation (`requestId`).
    pub generation: u32,
    /// Raw [`NetDeliveryTask`].
    pub task: u32,
    /// Per-task info bytes (abs. `CD+0x1818`), decoded by the [`ClientNetDeliveryRef`] payload accessor.
    pub info: &'b [u8; INFO_SIZE],
}

server_block!(net_delivery, net_delivery_mut: ServerNetDelivery, ServerNetDeliveryRef, ServerNetDeliveryMut @ 0x370);
client_block!(net_delivery, net_delivery_mut: ClientNetDelivery, ClientNetDeliveryRef, ClientNetDeliveryMut @ 0x1810);

/// `ipc::install` NetDelivery task (name table `0x1405EEED8`): only `DownloadInstructionUri` exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, TryFromPrimitive)]
#[repr(u32)]
pub enum NetDeliveryTask {
    /// Hand `appPath|optionPath` to libam as the local delivery source (`"DownloadInstructionUri"`, `0x1405EECF0`).
    DownloadInstructionUri = 0,
}

impl NetDeliveryTask {
    /// Name from the task table (`0x14006D8C0`).
    pub const fn name(self) -> &'static str {
        match self {
            Self::DownloadInstructionUri => "DownloadInstructionUri",
        }
    }
}

/// `DownloadInstructionUri` payload: instruction URIs / paths of the application and the option image, each
/// terminated within its 129 units.
#[zero]
#[derive(Debug)]
pub struct DownloadInstructionUri<'b> {
    /// Application image (abs. `CD+0x1818`).
    #[zero(capacity = 129)]
    pub app_path: &'b U16CStr,
    /// Option image (abs. `CD+0x191A`).
    #[zero(capacity = 129)]
    pub option_path: &'b U16CStr,
}

impl<'w> ClientNetDeliveryRef<'w> {
    /// The request's task, `None` for an unknown value (the request fails).
    pub fn task_kind(&self) -> Option<NetDeliveryTask> {
        NetDeliveryTask::try_from(self.task()).ok()
    }

    /// `DownloadInstructionUri` payload, `None` when a path is not terminated within its 129 units.
    pub fn download_instruction(&self) -> Option<DownloadInstructionUriRef<'w>> {
        let info: &'w [u8; INFO_SIZE] = self.info();
        DownloadInstructionUri::access_prefix(info).ok().map(|(payload, _)| payload)
    }
}

const _: () = {
    assert!(ServerNetDeliveryAppInfo::SCHEMA_SIZE == 0x28 && ServerNetDeliveryAppInfo::SCHEMA_ALIGN == 8);
    assert!(ServerNetDeliveryOptionInfo::SCHEMA_SIZE == 0x28 && ServerNetDeliveryOptionInfo::SCHEMA_ALIGN == 8);
    assert!(ServerNetDelivery::SCHEMA_SIZE == 0x60);
    assert!(ClientNetDelivery::SCHEMA_SIZE == 0x20C);
    assert!(DownloadInstructionUri::SCHEMA_SIZE == INFO_SIZE);
};
