//! [`MemoryProtector`] doubles.

use crate::protector::MemoryProtector;

/// [`MemoryProtector`] that leaves the bytes unchanged.
#[derive(Debug, Clone, Copy, Default)]
pub struct IdentityProtector;

/// [`MemoryProtector`] that XORs every byte with a key, so tests can see which bytes were protected.
/// The transform is its own inverse; like `CryptProtectMemory` it requires a multiple of 16 bytes.
#[derive(Debug, Clone, Copy)]
pub struct XorProtector(pub u8);

impl MemoryProtector for XorProtector {
    fn protect(&self, buf: &mut [u8]) -> bool {
        let aligned = buf.len().is_multiple_of(16);
        if aligned {
            buf.iter_mut().for_each(|byte| *byte ^= self.0);
        }
        aligned
    }

    fn unprotect(&self, buf: &mut [u8]) -> bool {
        self.protect(buf)
    }
}

impl MemoryProtector for IdentityProtector {
    fn protect(&self, buf: &mut [u8]) -> bool {
        buf.len().is_multiple_of(16)
    }

    fn unprotect(&self, buf: &mut [u8]) -> bool {
        buf.len().is_multiple_of(16)
    }
}
