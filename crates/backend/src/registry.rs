//! The registry interface (`HKLM` only): the channel to the launcher and the platform values (`core.md` B.4).

use crate::Unsupported;

/// A registry value as libam reads and writes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegValue {
    /// `REG_DWORD`.
    Dword(u32),
    /// `REG_QWORD`.
    Qword(u64),
    /// `REG_SZ`.
    Text(String),
    /// `REG_BINARY`.
    Binary(Vec<u8>),
    /// `REG_MULTI_SZ`: the strings without the list terminator.
    MultiText(Vec<String>),
}

/// How a key is opened: the original's wrappers either open an existing key or create it volatile when missing
/// (`core.md` B.4.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyMode {
    /// `RegOpenKeyExW`: fails when the key does not exist.
    Open,
    /// `RegCreateKeyExW` with `REG_OPTION_VOLATILE`: the key disappears at reboot.
    CreateVolatile,
}

/// Why a registry access failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    /// Key or value does not exist.
    #[error("registry key or value not found")]
    NotFound,
    /// The value has another type than the caller reads.
    #[error("registry value has another type")]
    WrongType,
    /// The guard mutex stayed busy through every retry (`core.md` B.4.5: 6 attempts).
    #[error("registry guard mutex busy")]
    Busy,
    /// Any other failure, with the Win32 error code.
    #[error("registry access failed with code {0}")]
    Failed(u32),
}

/// `HKLM` registry access. Real implementations wrap each value access in the named mutex of `core.md` B.4.5.
pub trait Registry {
    /// Reads value `name` of `key` (a path below `HKLM`, backslash separated).
    fn read(&mut self, key: &str, mode: KeyMode, name: &str) -> Result<RegValue, RegistryError>;

    /// Writes value `name` of `key`.
    fn write(&mut self, key: &str, mode: KeyMode, name: &str, value: &RegValue) -> Result<(), RegistryError>;
}

/// No registry: every key and value is missing, nothing can be written.
impl Registry for Unsupported {
    fn read(&mut self, _key: &str, _mode: KeyMode, _name: &str) -> Result<RegValue, RegistryError> {
        Err(RegistryError::NotFound)
    }

    fn write(&mut self, _key: &str, _mode: KeyMode, _name: &str, _value: &RegValue) -> Result<(), RegistryError> {
        Err(RegistryError::NotFound)
    }
}
