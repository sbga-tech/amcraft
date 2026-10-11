//! Error blocks: `ServerData+0x56B0` (0x3970) and `ClientData+0x1A58` (0xC).
//!
//! Evidence: `spec/controllers/error_sequence.md` Part 1 §2 (`error_sequence.error.layout.*`).

use num_enum::TryFromPrimitive;
use zero_schema::zero;

use crate::schema::shared::{ErrorData, Response};
use crate::schema::{client_block, server_block};

/// Number of rows in a [`HistoryLog`] (libam ring size, `0x1402245B0`).
pub const HISTORY_CAPACITY: usize = 15;
/// Number of rows in a [`TimesLog`] (libam occurrence table, `0x140224800`).
pub const TIMES_CAPACITY: usize = 222;

/// `ErrorTimesItem`: one libam occurrence counter (`number` only, no subNumber).
#[zero(endian = "little")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorTimesItem {
    /// Error number.
    pub number: u16,
    /// Occurrence count.
    pub times: u32,
}

/// `ServerHistoryLogData` (0x1868 bytes): a libam error-history ring, newest row first.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct HistoryLog<'b> {
    /// Number of valid rows as reported by libam (0..=15).
    pub count: u32,
    /// Rows; row 0 is the newest.
    pub items: [ErrorData<'b>; HISTORY_CAPACITY],
}

/// `ServerTimesLogData` (0x6F4 bytes): libam occurrence counters.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct TimesLog {
    /// Number of valid rows as reported by libam (0..=222).
    pub count: u32,
    /// Rows.
    pub items: [ErrorTimesItem; TIMES_CAPACITY],
}

/// `ServerData.error`: the last fatal error and libam's error logs.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerError<'b> {
    /// Executor response (`checkResult`).
    pub response: Response,
    /// Last fatal error; written only by the fatal-error listener.
    pub data: ErrorData<'b>,
    /// libam history ring selected with flag 1 (`updateLog`).
    pub history: HistoryLog<'b>,
    /// libam occurrence counters (`updateLog`).
    pub times: TimesLog,
    /// libam history ring selected with flag 0 (`updateLog`).
    pub system_history: HistoryLog<'b>,
}

/// `ClientData.error`: the request header and its per-task info bytes.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ClientError<'b> {
    /// Request generation (`taskNo`).
    pub generation: u32,
    /// Raw [`ErrorTask`].
    pub task: u32,
    /// Per-task info union (`ClientInfo`, whose only member is [`SetInfo`]).
    pub info: &'b [u8; 4],
}

server_block!(error, error_mut: ServerError, ServerErrorRef, ServerErrorMut @ 0x56B0);
client_block!(error, error_mut: ClientError, ClientErrorRef, ClientErrorMut @ 0x1A58);

/// `ipc::error::Task` (name table `0x1405EEEC0` read by `0x14006D890`; values ≥ 3 have no name).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, TryFromPrimitive)]
#[repr(u32)]
pub enum ErrorTask {
    /// `libam.error.reset()` (`0x1402533F0`).
    Reset = 0,
    /// `libam.error.set(number, subNumber)` (`0x140253370`).
    Set = 1,
    /// `libam.error.clearLogs()` (`0x1402331B0`) followed by a log republish.
    LogClear = 2,
}

impl ErrorTask {
    /// Name printed by the request trace (`0x1405EECC0`, `0x1405EECD0`, `0x1405EECD8`).
    pub const fn name(self) -> &'static str {
        match self {
            Self::Reset => "Reset",
            Self::Set => "Set",
            Self::LogClear => "LogClear",
        }
    }
}

/// `Set` payload (`ClientSetInfo`).
#[zero(endian = "little")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetInfo {
    /// Error number.
    pub number: u16,
    /// Error sub number.
    pub sub_number: u16,
}

impl ClientErrorRef<'_> {
    /// The request's task, `None` for an unknown value (the request fails).
    pub fn task_kind(&self) -> Option<ErrorTask> {
        ErrorTask::try_from(self.task()).ok()
    }

    /// `Set` payload: the error number and sub number.
    pub fn set_info(&self) -> SetInfo {
        let (set, _) = SetInfo::access_prefix(self.info()).expect("an integer payload accepts every bit pattern");
        set.copy_into()
    }
}

const _: () = {
    assert!(ErrorTimesItem::SCHEMA_SIZE == 8);
    assert!(HistoryLog::SCHEMA_SIZE == 0x1868 && HistoryLog::SCHEMA_ALIGN == 8);
    assert!(TimesLog::SCHEMA_SIZE == 0x6F4);
    assert!(ServerError::SCHEMA_SIZE == 0x3970 && ServerError::SCHEMA_ALIGN == 8);
    assert!(SetInfo::SCHEMA_SIZE == 4);
    assert!(ClientError::SCHEMA_SIZE == 0xC);
};
