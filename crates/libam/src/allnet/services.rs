//! The keychip identity every ALL.Net request and accounting record carries.

/// The keychip identity every request and accounting record carries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeychipIdentity {
    /// 11-character keychip id (`serial=`).
    pub keychip: String,
    /// 11-character board id.
    pub board: String,
    /// 4-character game id (`game_id=`).
    pub game_id: String,
    /// 5-character game version (`ver=`).
    pub game_version: String,
    /// Function type byte.
    pub function_type: u8,
}
