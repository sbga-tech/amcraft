//! `systemInfo` block: `ServerData+0x9050` (0x49C).
//!
//! Evidence: `spec/controllers/process_common_systeminfo.md` §3.2. The whole block is written by
//! `SystemInfoController` on each not-ready → ready edge; SystemInfo has no ClientData block.

use widestring::U16CStr;
use zero_schema::zero;

use crate::schema::server_block;

/// `amdaemon::SerialId` (`0x3A` bytes): a `StandardSerialId` followed by a `ShortSerialId`.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct SerialId<'b> {
    /// `StandardSerialId`: `wchar_t[17]`, at most 16 code units + NUL.
    #[zero(capacity = 17)]
    pub standard: &'b U16CStr,
    /// `ShortSerialId`: `wchar_t[12]`, at most 11 code units + NUL.
    #[zero(capacity = 12)]
    pub short: &'b U16CStr,
}

/// One video output resolution: two `u32` copied from a libam video descriptor (`+4`, `+8`).
#[zero(endian = "little")]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Resolution {
    /// First `u32` (libam descriptor `+4`, getter `0x14021F1C0`). Calling it the width is an inference.
    pub width: u32,
    /// Second `u32` (libam descriptor `+8`, getter `0x14021F1D0`). Calling it the height is an inference.
    pub height: u32,
}

/// `ServerData.systemInfo`.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerSystemInfo<'b> {
    /// Board serial (libam `app+0x6608`); zero when the board data is broken.
    pub board_id: SerialId<'b>,
    /// libam region (`app+0x66B0`): 1, 2 and 3 are copied, anything else (or absent) is 0.
    pub region_code: u32,
    /// Keychip serial (libam `app+0x6672`); zero when the dongle is disconnected.
    pub keychip_id: SerialId<'b>,
    /// libam model type (`app+0x66AE`), 2 when absent.
    pub model_type: u8,
    /// Game id: apm sub-game id if present, else dongle game id; at most 4 code units + NUL.
    #[zero(capacity = 5)]
    pub game_id: &'b U16CStr,
    /// Raw libam develop byte (`app+0x6664`), not normalised to 0/1.
    pub develop: u8,
    /// Video resolutions; slot 0 comes from libam video descriptor 1, slot 1 from descriptor 0.
    pub resolutions: [Resolution; 2],
    /// libam storage application root, at most 255 code units + NUL.
    #[zero(capacity = 256)]
    pub app_root_path: &'b U16CStr,
    /// libam storage RAM-disk path, at most 255 code units + NUL.
    #[zero(capacity = 256)]
    pub ram_disk_path: &'b U16CStr,
    /// Output status: slot 0 from video descriptor 1, slot 1 from descriptor 0; each is
    /// `byte(desc+14) != 0 && byte(desc+15) != 0`.
    pub output_status: [bool; 2],
}

server_block!(system_info, system_info_mut: ServerSystemInfo, ServerSystemInfoRef, ServerSystemInfoMut @ 0x9050);

const _: () = {
    assert!(SerialId::SCHEMA_SIZE == 0x3A && SerialId::SCHEMA_ALIGN == 2);
    assert!(Resolution::SCHEMA_SIZE == 8);
    assert!(ServerSystemInfo::SCHEMA_SIZE == 0x49C && ServerSystemInfo::SCHEMA_ALIGN == 4);
};
