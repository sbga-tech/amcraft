//! `amdaemon::ipc::SharedMemory`: a named, size-stamped section (`spec/ipc/transport.md` §4).
//!
//! Kernel object names and layout:
//!
//! | Object | Name |
//! |---|---|
//! | section | `amdipc_shm_` + name |
//! | creation/open serialisation mutex | `amdipc_shmmtx_` + name |
//!
//! The mapping is `payload + 8` bytes: `u32 magic 0x15F90CE2`, `u32 payload size`, then the payload. Creation and
//! attachment run under the serialisation mutex so that a newly created section is stamped and zero-filled
//! before anybody else maps it.
//!
//! The message-channel use of `SharedMemory` (Messenger mode 1, `<msg>_amdmsg_{CS,SC}_shm`, 0x60034-byte
//! two-slot ring) is never instantiated by the daemon and is not implemented.

use crate::ipc::mutex::MutexHolder;
use backend::ipc::IpcPlatform;
use backend::section::Section;

/// Magic stamped at offset 0 of every section (`0x15F90CE2`).
pub const MAGIC: u32 = 0x15F9_0CE2;
/// Size of the `{magic, size}` header in front of the payload.
pub const HEADER_LEN: usize = 8;
/// Prefix of every section name (`0x1405F4528`).
const SECTION_PREFIX: &str = "amdipc_shm_";
/// Prefix of every serialisation mutex name (`0x1405F4540`).
const MUTEX_PREFIX: &str = "amdipc_shmmtx_";
/// Chunk used to zero large payloads without allocating them whole.
const ZERO_CHUNK: [u8; 4096] = [0; 4096];

/// Why a section could not be created or attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SharedMemoryError {
    /// `CreateFileMappingW` / `MapViewOfFile` failed.
    #[error("CreateFileMappingW failed.")]
    CreateFailed,
    /// The section does not exist.
    #[error("SharedMemory is not created.")]
    NotCreated,
    /// The magic is wrong: the object belongs to somebody else.
    #[error("SharedMemory is created by another.")]
    ForeignObject,
    /// The existing section has another payload size.
    #[error("SharedMemory is already created with a different size. The library version might be different.")]
    SizeMismatch,
    /// The mapping is smaller than its header claims.
    #[error("Cannot create memory map view.")]
    ViewFailed,
}

/// An attached section. Dropping unmaps the view and closes the handle; the section itself lives while any
/// process still holds a handle.
#[derive(Debug)]
pub struct SharedMemory {
    section: Box<dyn Section>,
    size: u32,
}

impl SharedMemory {
    /// Creates the section or attaches to an existing one (`Impl::Impl(size, name, zeroOnOpen)`, `0x1400782E0`).
    ///
    /// `size == 0` is bumped to 1. A newly created section is stamped and (it is already zero from the OS)
    /// left empty; an attached one is zero-filled only when `zero_on_open` is set. All of it happens under
    /// the serialisation mutex.
    pub fn create(
        platform: &dyn IpcPlatform,
        size: u32,
        name: &str,
        zero_on_open: bool,
    ) -> Result<Self, SharedMemoryError> {
        let size = size.max(1);
        let _guard = SerialisationLock::acquire(platform, name);
        // `createMemoryMap` (`0x140077440`) opens an existing section first and only creates it when absent.
        let full_name = section_name(name);
        let existing = platform.open_section(&full_name).map(|section| (section, false));
        let Some((mut section, created)) =
            existing.or_else(|| platform.create_section(&full_name, size as usize + HEADER_LEN))
        else {
            tracing::error!("creating the shared memory section failed");
            return Err(SharedMemoryError::CreateFailed);
        };
        if created {
            let mut header = [0u8; HEADER_LEN];
            header[..4].copy_from_slice(&MAGIC.to_le_bytes());
            header[4..].copy_from_slice(&size.to_le_bytes());
            if !section.write(0, &header) {
                tracing::error!("mapping the shared memory view failed");
                return Err(SharedMemoryError::CreateFailed);
            }
        } else {
            let (magic, existing) = read_header(section.as_ref()).ok_or_else(|| {
                tracing::error!("mapping the shared memory view failed");
                SharedMemoryError::ViewFailed
            })?;
            if magic != MAGIC {
                tracing::error!("shared memory section already belongs to another owner");
                return Err(SharedMemoryError::ForeignObject);
            }
            if existing != size {
                tracing::error!(
                    requested = size,
                    existing,
                    "shared memory section already exists with a different size"
                );
                return Err(SharedMemoryError::SizeMismatch);
            }
        }
        if section.byte_len() < size as usize + HEADER_LEN {
            // `MapViewOfFile` of `n + 8` bytes fails when the section is smaller (transport §4.2).
            tracing::error!("mapping the shared memory view failed");
            return Err(SharedMemoryError::ViewFailed);
        }
        let mut memory = Self { section, size };
        if !created && zero_on_open {
            memory.zero_payload();
        }
        Ok(memory)
    }

    /// Attaches to an existing section (`Impl::Impl(name)`, `0x1400785C0`): checks the magic, reads the payload
    /// size from the header and never initialises anything.
    pub fn open(platform: &dyn IpcPlatform, name: &str) -> Result<Self, SharedMemoryError> {
        let _guard = SerialisationLock::acquire(platform, name);
        let Some(section) = platform.open_section(&section_name(name)) else {
            tracing::error!("shared memory section does not exist");
            return Err(SharedMemoryError::NotCreated);
        };
        let (magic, size) = read_header(section.as_ref()).ok_or_else(|| {
            tracing::error!("mapping the shared memory view failed");
            SharedMemoryError::ViewFailed
        })?;
        if magic != MAGIC {
            tracing::error!("shared memory section belongs to another owner");
            return Err(SharedMemoryError::ForeignObject);
        }
        if section.byte_len() < size as usize + HEADER_LEN {
            tracing::error!("mapping the shared memory view failed");
            return Err(SharedMemoryError::ViewFailed);
        }
        Ok(Self { section, size })
    }

    /// Payload size in bytes (`Impl+0x18`, read from the header).
    pub fn size(&self) -> u32 {
        self.size
    }

    /// Copies `out.len()` payload bytes starting at payload offset `offset`; `false` when out of range.
    pub fn read(&self, offset: usize, out: &mut [u8]) -> bool {
        self.in_range(offset, out.len()) && self.section.read(HEADER_LEN + offset, out)
    }

    /// Copies `data` to payload offset `offset`; `false` when out of range.
    pub fn write(&mut self, offset: usize, data: &[u8]) -> bool {
        self.in_range(offset, data.len()) && self.section.write(HEADER_LEN + offset, data)
    }

    /// Zero-fills `len` payload bytes starting at `offset`; `false` when out of range.
    pub fn fill_zero(&mut self, offset: usize, len: usize) -> bool {
        if !self.in_range(offset, len) {
            return false;
        }
        let mut done = 0;
        while done < len {
            let chunk = ZERO_CHUNK.len().min(len - done);
            if !self.section.write(HEADER_LEN + offset + done, &ZERO_CHUNK[..chunk]) {
                return false;
            }
            done += chunk;
        }
        true
    }

    fn zero_payload(&mut self) {
        // The range is the whole payload, which `create` verified against the mapping.
        let _ = self.fill_zero(0, self.size as usize);
    }

    fn in_range(&self, offset: usize, len: usize) -> bool {
        offset.checked_add(len).is_some_and(|end| end <= self.size as usize)
            && HEADER_LEN + self.size as usize <= self.section.byte_len()
    }
}

/// Full kernel name of the section for `name` (`amdipc_shm_` + name).
pub fn section_name(name: &str) -> String {
    format!("{SECTION_PREFIX}{name}")
}

/// Reads `{magic, size}` from the front of a mapped section.
fn read_header(section: &dyn Section) -> Option<(u32, u32)> {
    let mut header = [0u8; HEADER_LEN];
    if !section.read(0, &mut header) {
        return None;
    }
    Some((
        u32::from_le_bytes([header[0], header[1], header[2], header[3]]),
        u32::from_le_bytes([header[4], header[5], header[6], header[7]]),
    ))
}

/// The `amdipc_shmmtx_<name>` mutex held while creating or attaching (`0x140078E10`). A mutex that cannot be
/// created is ignored like in the original (the lock result is never checked).
#[derive(Debug)]
struct SerialisationLock(Option<MutexHolder>);

impl SerialisationLock {
    fn acquire(platform: &dyn IpcPlatform, name: &str) -> Self {
        let mut holder = MutexHolder::open(platform, Some(&format!("{MUTEX_PREFIX}{name}")));
        if let Some(holder) = holder.as_mut() {
            holder.lock();
        }
        Self(holder)
    }
}

impl Drop for SerialisationLock {
    fn drop(&mut self) {
        if let Some(holder) = self.0.as_mut() {
            holder.unlock();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use backend::mock::ipc::LoopbackPlatform;

    fn raw(platform: &LoopbackPlatform, name: &str) -> Vec<u8> {
        let section = platform.open_section(&section_name(name)).unwrap();
        let mut bytes = vec![0; section.byte_len()];
        assert!(section.read(0, &mut bytes));
        bytes
    }

    #[test]
    fn created_section_is_stamped_with_magic_and_payload_size() {
        let platform = LoopbackPlatform::new();
        let mut shm = SharedMemory::create(&platform, 24, "demo", false).unwrap();
        assert_eq!(shm.size(), 24);
        assert!(shm.write(0, &[1, 2, 3, 4]));
        let bytes = raw(&platform, "demo");
        assert_eq!(bytes.len(), 32, "payload + 8-byte header");
        assert_eq!(bytes[..8], [0xE2, 0x0C, 0xF9, 0x15, 24, 0, 0, 0]);
        assert_eq!(bytes[8..12], [1, 2, 3, 4]);
    }

    #[test]
    fn attaching_keeps_content_unless_zero_on_open_is_requested() {
        let platform = LoopbackPlatform::new();
        let mut first = SharedMemory::create(&platform, 16, "keep", false).unwrap();
        assert!(first.write(0, &[9; 16]));
        let second = SharedMemory::create(&platform, 16, "keep", false).unwrap();
        let mut seen = [0u8; 16];
        assert!(second.read(0, &mut seen));
        assert_eq!(seen, [9; 16]);

        let third = SharedMemory::create(&platform, 16, "keep", true).unwrap();
        assert!(third.read(0, &mut seen));
        assert_eq!(seen, [0; 16], "zeroOnOpen clears an existing payload");
        assert!(first.read(0, &mut seen));
        assert_eq!(seen, [0; 16], "all handles see the same memory");
    }

    #[test]
    fn mismatching_or_foreign_sections_are_refused() {
        let platform = LoopbackPlatform::new();
        let _owner = SharedMemory::create(&platform, 16, "size", false).unwrap();
        assert_eq!(SharedMemory::create(&platform, 32, "size", false).unwrap_err(), SharedMemoryError::SizeMismatch);

        // A section somebody else created without our header.
        let (mut foreign, created) = platform.create_section(&section_name("alien"), 24).unwrap();
        assert!(created);
        assert!(foreign.write(0, &[0xFF; 8]));
        assert_eq!(SharedMemory::create(&platform, 16, "alien", false).unwrap_err(), SharedMemoryError::ForeignObject);
        assert_eq!(SharedMemory::open(&platform, "alien").unwrap_err(), SharedMemoryError::ForeignObject);
    }
}
