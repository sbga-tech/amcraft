//! [`MemoryProtector`] on `CryptProtectMemory`.

use windows_sys::Win32::Security::Cryptography::{
    CRYPTPROTECTMEMORY_BLOCK_SIZE, CRYPTPROTECTMEMORY_SAME_LOGON, CryptProtectMemory, CryptUnprotectMemory,
};

use crate::protector::MemoryProtector;

/// `CryptProtectMemory` / `CryptUnprotectMemory` with `CRYPTPROTECTMEMORY_SAME_LOGON`.
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsMemoryProtector;

impl WindowsMemoryProtector {
    /// Runs the OS transform over `buf`; `false` when its length is not a multiple of 16 or the API fails.
    fn transform(buf: &mut [u8], protect: bool) -> bool {
        let Ok(len) = u32::try_from(buf.len()) else {
            return false;
        };
        if len % CRYPTPROTECTMEMORY_BLOCK_SIZE != 0 {
            return false;
        }
        let data = buf.as_mut_ptr().cast();
        // SAFETY: `data` addresses `len` initialised bytes that are exclusively borrowed for the duration of the call.
        let done = unsafe {
            if protect {
                CryptProtectMemory(data, len, CRYPTPROTECTMEMORY_SAME_LOGON)
            } else {
                CryptUnprotectMemory(data, len, CRYPTPROTECTMEMORY_SAME_LOGON)
            }
        };
        done != 0
    }
}

impl MemoryProtector for WindowsMemoryProtector {
    fn protect(&self, buf: &mut [u8]) -> bool {
        Self::transform(buf, true)
    }

    fn unprotect(&self, buf: &mut [u8]) -> bool {
        Self::transform(buf, false)
    }
}
