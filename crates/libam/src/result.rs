//! `am::Result` (vtable `0x1406A0750`): `{vptr, u32 code}`; `isError` = `code != 0` (`0x1402148B0`).

/// Non-zero libam result code.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("am::Result code {code}{}", message.as_deref().map(|m| format!(": {m}")).unwrap_or_default())]
pub struct AmError {
    /// Raw code (never 0).
    pub code: u32,
    /// Optional message passed to `setCode` (only `OutputDebugStringW`'d by libam).
    pub message: Option<String>,
}

impl AmError {
    /// Invalid argument.
    pub const INVALID_ARGUMENT: u32 = 1;
    /// Module not initialised / precondition (e.g. "amw is uninitialized").
    pub const NOT_INITIALIZED: u32 = 2;
    /// Busy (APM keeps the request pending).
    pub const BUSY: u32 = 3;
    /// "Requested unknown task."
    pub const UNKNOWN_TASK: u32 = 5;
    /// Operation failed / "Requested invalid task."
    pub const FAILED: u32 = 6;

    /// Code without message.
    pub const fn code(code: u32) -> Self {
        Self { code, message: None }
    }

    /// Code with message.
    pub fn with_message(code: u32, message: impl Into<String>) -> Self {
        Self { code, message: Some(message.into()) }
    }
}

/// `am::Result` as a Rust result.
pub type AmResult<T = ()> = Result<T, AmError>;
