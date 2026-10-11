//! Schemas and decoding vocabulary shared by several blocks.

use widestring::U16CStr;
use zero_schema::zero;

use crate::executor::Completion;

/// Executor response root (8 bytes): the request generation it answers and whether it succeeded.
#[zero(endian = "little")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Response {
    /// Mirrors the request generation on completion.
    pub generation: u32,
    /// The request completed with `RequestState::Succeeded`.
    pub succeeded: bool,
}

impl ResponseMut<'_> {
    /// Writes a finished request like `checkResult` (`0x140019410`): the outcome (when the response carries
    /// one) and the mirrored generation.
    pub fn complete(&mut self, completion: Completion) {
        let patch = ResponsePatch { generation: Some(completion.generation), succeeded: completion.succeeded };
        self.copy_from(&patch).expect("scalar fields accept every value");
    }
}

/// `ErrorData.resetType`, produced from the libam reset type by `0x1400524E0` (names inferred).
#[zero]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u32)]
pub enum ErrorResetType {
    /// Unknown libam reset type.
    #[default]
    None = 0,
    /// libam type 4.
    Any = 1,
    /// libam type 1.
    Shutdown = 2,
    /// libam type 2 (cleared by the `Reset` task).
    Operation = 3,
    /// libam type 3.
    Test = 4,
}

/// `ipc::ErrorData` (`0x1A0` bytes): one published error (Error block, Aime units, network tests).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ErrorData<'b> {
    /// libam item `number`.
    pub number: u16,
    /// libam item `subNumber`.
    pub sub_number: u16,
    /// Localised message (`wcsncpy_s(dst, 200, src, 199)`).
    #[zero(capacity = 200)]
    pub message: &'b U16CStr,
    /// Mapped libam reset type.
    pub reset_type: ErrorResetType,
    /// `util::DateTime` microseconds of the error.
    pub time: i64,
}

const _: () = {
    assert!(Response::SCHEMA_SIZE == 8 && Response::SCHEMA_ALIGN == 4);
    assert!(ErrorData::SCHEMA_SIZE == 0x1A0 && ErrorData::SCHEMA_ALIGN == 8);
};
