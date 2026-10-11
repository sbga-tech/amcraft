//! The Windows registry: `HKLM` values through `winreg` with the per-value named mutex.
//!
//! `winreg` supports `REG_OPTION_VOLATILE` creation and raw typed values, and shares `windows-sys` 0.61 with this
//! crate. It has no named-mutex support and `named-lock` prefixes `Global\` to every name, so the per-value mutex
//! of `core.md` B.4.5 stays a thin `windows-sys` helper (the exact names matter: other processes lock the same
//! values). [`WindowsRegistry`] is a `Send + Sync` unit struct, so every production user that needs the registry
//! goes through it, from any thread.
//!
//! Registry access masks: the trait carries only the key mode, so the mask is derived from the operation. Reads
//! use `KEY_READ` (0x20019) for both modes; the original opens its create-volatile keys with 0x2001F even to read
//! them, which only differs when the key denies write access (the read then succeeds here, as it does for the
//! `Static` key). Writes use `KEY_WRITE` (0x20006, the `sgworker` key) when the key must exist and
//! `KEY_READ | KEY_WRITE` (0x2001F) when it is created.

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::null;

use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
use windows_sys::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
use winreg::RegKey;
use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WRITE, REG_OPTION_VOLATILE, RegType};

use crate::{KeyMode, RegValue, Registry, RegistryError};

/// Total mutex attempts per value access (`core.md` B.4.5: retry count 5).
const MUTEX_ATTEMPTS: u32 = 6;
/// Longest mutex name in UTF-16 units; longer names lose their front (`core.md` B.4.5).
const MUTEX_NAME_MAX: usize = 259;

/// A NUL-terminated UTF-16 copy of `text` for the `W` APIs.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The mutex name of one value: the key path without backslashes followed by the value name, case as given.
fn mutex_name(key: &str, value: &str) -> Vec<u16> {
    let mut name: Vec<u16> =
        key.encode_utf16().filter(|&unit| unit != u16::from(b'\\')).chain(value.encode_utf16()).collect();
    if name.len() > MUTEX_NAME_MAX {
        name.drain(..name.len() - MUTEX_NAME_MAX);
    }
    name.push(0);
    name
}

/// A held value mutex. Dropping it releases and closes the mutex whether or not it was acquired (the original
/// calls `ReleaseMutex` + `CloseHandle` after every attempt).
struct ValueMutex(OwnedHandle);

impl ValueMutex {
    /// One attempt: `CreateMutexW(NULL, FALSE, name)` and `WaitForSingleObject(h, 0)`. Only `WAIT_OBJECT_0` counts as
    /// acquired; abandoned and timed-out waits do not.
    fn try_acquire(name: &[u16]) -> Option<Self> {
        debug_assert_eq!(name.last(), Some(&0));
        // SAFETY: `name` is NUL-terminated and outlives the call; null security attributes select the defaults.
        let raw = unsafe { CreateMutexW(null(), 0, name.as_ptr()) };
        if raw.is_null() {
            return None;
        }
        // SAFETY: `CreateMutexW` returned a valid handle that nothing else owns.
        let mutex = Self(unsafe { OwnedHandle::from_raw_handle(raw) });
        // SAFETY: the handle in `mutex` is open for the duration of the call.
        let wait = unsafe { WaitForSingleObject(mutex.0.as_raw_handle(), 0) };
        (wait == WAIT_OBJECT_0).then_some(mutex)
    }
}

impl Drop for ValueMutex {
    fn drop(&mut self) {
        // SAFETY: the handle is still open (the `OwnedHandle` field is dropped after this body). Releasing a mutex
        // this thread does not own fails harmlessly, as in the original.
        unsafe { ReleaseMutex(self.0.as_raw_handle()) };
    }
}

/// Runs `op` while holding the mutex of `key`/`value`; `Busy` when it stays taken for `attempts` attempts.
fn with_value_mutex<T>(attempts: u32, key: &str, value: &str, op: impl FnOnce() -> T) -> Result<T, RegistryError> {
    let name = mutex_name(key, value);
    for _ in 0..attempts {
        if let Some(_held) = ValueMutex::try_acquire(&name) {
            return Ok(op());
        }
        std::thread::yield_now();
    }
    Err(RegistryError::Busy)
}

fn registry_error(error: &std::io::Error) -> RegistryError {
    match error.raw_os_error().map(i32::cast_unsigned) {
        // ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND.
        Some(2 | 3) => RegistryError::NotFound,
        Some(code) => RegistryError::Failed(code),
        None => RegistryError::Failed(0),
    }
}

/// Opens or creates (volatile) `key` below `HKLM`.
fn open_key(key: &str, mode: KeyMode, access: u32) -> Result<RegKey, RegistryError> {
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    match mode {
        KeyMode::Open => hklm.open_subkey_with_flags(key, access),
        KeyMode::CreateVolatile => {
            hklm.create_subkey_with_options_flags(key, REG_OPTION_VOLATILE, access).map(|(key, _)| key)
        }
    }
    .map_err(|error| registry_error(&error))
}

/// `HKLM` registry access with the per-value named mutex.
#[derive(Debug, Clone, Copy, Default)]
pub struct WindowsRegistry;

const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<WindowsRegistry>();
};

impl WindowsRegistry {
    /// One read with `attempts` tries of the value mutex.
    fn read_with(attempts: u32, key: &str, mode: KeyMode, name: &str) -> Result<RegValue, RegistryError> {
        let handle = open_key(key, mode, KEY_READ)?;
        let raw = with_value_mutex(attempts, key, name, || handle.get_raw_value(name))?
            .map_err(|error| registry_error(&error))?;
        decode_value(raw.vtype, &raw.bytes)
    }

    /// A read with a single try of the value mutex: the keychip store's `amyRegistry` reads have retry count 0
    /// (`platform.md` B 5.4), so a busy mutex is reported at once and the caller retries on its next poll.
    pub(crate) fn read_once(key: &str, mode: KeyMode, name: &str) -> Result<RegValue, RegistryError> {
        Self::read_with(1, key, mode, name)
    }
}

impl Registry for WindowsRegistry {
    fn read(&mut self, key: &str, mode: KeyMode, name: &str) -> Result<RegValue, RegistryError> {
        Self::read_with(MUTEX_ATTEMPTS, key, mode, name)
    }

    fn write(&mut self, key: &str, mode: KeyMode, name: &str, value: &RegValue) -> Result<(), RegistryError> {
        let access = match mode {
            KeyMode::Open => KEY_WRITE,
            KeyMode::CreateVolatile => KEY_READ | KEY_WRITE,
        };
        let handle = open_key(key, mode, access)?;
        let raw = encode_value(value);
        with_value_mutex(MUTEX_ATTEMPTS, key, name, || handle.set_raw_value(name, &raw))?
            .map_err(|error| registry_error(&error))
    }
}

fn utf16_units(bytes: &[u8]) -> Vec<u16> {
    bytes.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect()
}

/// Little-endian integer of at most `N` bytes (the original accepts shorter data); `None` when longer.
fn le_integer<const N: usize>(bytes: &[u8]) -> Option<[u8; N]> {
    let mut out = [0_u8; N];
    out.get_mut(..bytes.len())?.copy_from_slice(bytes);
    Some(out)
}

fn decode_value(vtype: RegType, bytes: &[u8]) -> Result<RegValue, RegistryError> {
    match vtype {
        RegType::REG_DWORD => le_integer::<4>(bytes).map(|b| RegValue::Dword(u32::from_le_bytes(b))),
        RegType::REG_QWORD => le_integer::<8>(bytes).map(|b| RegValue::Qword(u64::from_le_bytes(b))),
        RegType::REG_SZ => {
            let units = utf16_units(bytes);
            let end = units.iter().position(|&unit| unit == 0).unwrap_or(units.len());
            Some(RegValue::Text(String::from_utf16_lossy(&units[..end])))
        }
        RegType::REG_BINARY => Some(RegValue::Binary(bytes.to_vec())),
        RegType::REG_MULTI_SZ => {
            let units = utf16_units(bytes);
            let strings = units
                .split(|&unit| unit == 0)
                .take_while(|part| !part.is_empty())
                .map(String::from_utf16_lossy)
                .collect();
            Some(RegValue::MultiText(strings))
        }
        _ => None,
    }
    .ok_or(RegistryError::WrongType)
}

fn encode_value(value: &RegValue) -> winreg::RegValue<'static> {
    let (bytes, vtype) = match value {
        RegValue::Dword(v) => (v.to_le_bytes().to_vec(), RegType::REG_DWORD),
        RegValue::Qword(v) => (v.to_le_bytes().to_vec(), RegType::REG_QWORD),
        // `REG_SZ` data includes its terminating NUL (`core.md` B.4.1: size `2 * (len + 1)`).
        RegValue::Text(text) => (units_to_bytes(&wide(text)), RegType::REG_SZ),
        RegValue::Binary(bytes) => (bytes.clone(), RegType::REG_BINARY),
        RegValue::MultiText(strings) => {
            let mut units: Vec<u16> = strings.iter().flat_map(|text| wide(text)).collect();
            units.push(0);
            (units_to_bytes(&units), RegType::REG_MULTI_SZ)
        }
    };
    winreg::RegValue { bytes: bytes.into(), vtype }
}

fn units_to_bytes(units: &[u16]) -> Vec<u8> {
    units.iter().flat_map(|unit| unit.to_le_bytes()).collect()
}
