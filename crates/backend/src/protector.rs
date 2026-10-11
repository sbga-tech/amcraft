//! Same-logon memory protection of the IPC layer (`CryptProtectMemory`, `0x140079030` / `0x140079050`).

use std::fmt::Debug;

/// In-place memory protection bound to the current logon session
/// (`CryptProtectMemory` / `CryptUnprotectMemory` with `CRYPTPROTECTMEMORY_SAME_LOGON`, flag value 2).
///
/// Buffers are always a multiple of 16 bytes. The transform is OS-private on Windows, so protected bytes can
/// only be produced and checked by a peer that calls the same API in the same logon session.
pub trait MemoryProtector: Debug + Send + Sync {
    /// Encrypts `buf` in place; `false` on failure.
    fn protect(&self, buf: &mut [u8]) -> bool;
    /// Decrypts `buf` in place; `false` on failure.
    fn unprotect(&self, buf: &mut [u8]) -> bool;
}
