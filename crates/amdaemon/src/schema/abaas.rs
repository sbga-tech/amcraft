//! `abaas` log blocks: `ServerData+0x3D40` (`0x1A0`) and `ClientData+0x0E10` (`0x80C`).
//!
//! Evidence: `spec/controllers/abaas_apm.md` §1.2 (claims `abaas_apm.log.blocks`, `.sd.*`, `.cd.request`,
//! `.task.enum`). Field names are inferred; the binary has none.

use num_enum::TryFromPrimitive;
use widestring::U16CStr;
use zero_schema::zero;

use crate::schema::shared::Response;
use crate::schema::{client_block, server_block};

/// `ServerData.abaas_log`: `ipc::abaas::ServerLogResponse` plus the published libabaas state.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerAbaasLog<'b> {
    /// Response root (`checkResult` `0x14004E290`).
    pub response: Response,
    /// Active ABaaS log folder, at most 200 characters stored (`wcsncpy_s(dst, 0xC9, src, 0xC8)` at
    /// `0x14004D170`).
    #[zero(capacity = 201)]
    pub root_path: &'b U16CStr,
    /// libabaas "alive" flag (`LogManager::isAlive`, `0x1402150C0`).
    pub alive: bool,
    /// Upload in progress: `!LogManager.uploadIdle` (`0x140215100`).
    pub upload: bool,
    /// Pending upload records, `-1` when unavailable (`0x1402150E0`).
    pub file_count: i32,
}

/// `ClientData.abaas_log`: `ipc::abaas::ClientLogRequest`.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ClientAbaasLog<'b> {
    /// Request generation.
    pub generation: u32,
    /// Raw [`LogTask`] (`cd+3604`, read at `0x14004CFC0`).
    pub task: u32,
    /// Per-task info (`ClientLogInfo`), decoded by [`ClientAbaasLogRef::error_message`]. The extent of 2050 bytes
    /// is inferred from the block size (`spec/controllers/abaas_apm.md` §4, unresolved).
    pub info: &'b [u8; 2050],
}

server_block!(abaas_log, abaas_log_mut: ServerAbaasLog, ServerAbaasLogRef, ServerAbaasLogMut @ 0x3D40);
client_block!(abaas_log, abaas_log_mut: ClientAbaasLog, ClientAbaasLogRef, ClientAbaasLogMut @ 0x0E10);

/// `ipc::abaas::LogTask`. The name table `0x1405EED58` has exactly one entry; every other value is rejected as an
/// unknown task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, TryFromPrimitive)]
#[repr(u32)]
pub enum LogTask {
    /// Submit an error message to ABaaS (name `L"Error"` at `0x1405EE940`).
    Error = 0,
}

impl LogTask {
    /// Name inserted into the request trace line (`0x14006D650`).
    pub const fn name(self) -> &'static str {
        match self {
            Self::Error => "Error",
        }
    }
}

/// `Error` payload (`ClientLogInfo.message`): the NUL-terminated UTF-16 error text. The capacity of 1025 code units
/// is inferred from the block extent; amdaemon applies no other bound.
#[zero]
#[derive(Debug)]
pub struct ErrorLogInfo<'b> {
    /// Error text, terminated within 1025 units.
    #[zero(capacity = 1025)]
    pub message: &'b U16CStr,
}

impl<'w> ClientAbaasLogRef<'w> {
    /// The request's task, `None` for an unknown value (the request fails).
    pub fn task_kind(&self) -> Option<LogTask> {
        LogTask::try_from(self.task()).ok()
    }

    /// `Error` payload: the message, `None` when it is not terminated within its 1025 units.
    pub fn error_message(&self) -> Option<&'w U16CStr> {
        let info: &'w [u8; 2050] = self.info();
        ErrorLogInfo::access_prefix(info).ok().map(|(payload, _)| payload.message())
    }
}

const _: () = {
    assert!(ServerAbaasLog::SCHEMA_SIZE == 0x1A0);
    assert!(ClientAbaasLog::SCHEMA_SIZE == 0x80C);
    assert!(ErrorLogInfo::SCHEMA_SIZE == 2050);
};
