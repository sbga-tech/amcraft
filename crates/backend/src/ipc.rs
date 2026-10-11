//! The factory of every OS object the IPC layer uses (`amdipc`, `spec/ipc/transport.md` §2, §4, §8).

use std::fmt::Debug;
use std::sync::Arc;

use crate::mutex::NamedMutex;
use crate::pipe::{PipeEndpoint, PipeRole};
use crate::protector::MemoryProtector;
use crate::section::Section;

/// Process-wide memory protector every IPC object needs after construction.
#[derive(Debug, Clone)]
pub struct Services {
    protector: Arc<dyn MemoryProtector>,
}

impl Services {
    /// Bundles the memory protector.
    pub fn new(protector: Arc<dyn MemoryProtector>) -> Self {
        Self { protector }
    }

    /// See [`MemoryProtector::protect`].
    pub fn protect(&self, buf: &mut [u8]) -> bool {
        self.protector.protect(buf)
    }

    /// See [`MemoryProtector::unprotect`].
    pub fn unprotect(&self, buf: &mut [u8]) -> bool {
        self.protector.unprotect(buf)
    }
}

/// Factory for every OS object the IPC layer uses.
pub trait IpcPlatform: Debug {
    /// Process-wide memory protector.
    fn services(&self) -> Services;

    /// `OpenMutexW(0x1F0001, FALSE, name)` falling back to `CreateMutexW(NULL, FALSE, name)`;
    /// `None` for the name creates an anonymous mutex. The mutex is not initially owned.
    fn open_mutex(&self, name: Option<&str>) -> Option<Box<dyn NamedMutex>>;

    /// `createMemoryMap` primitive (`0x140077440`): `OpenFileMappingW(FILE_MAP_WRITE)` of `name` (the full
    /// kernel name, e.g. `amdipc_shm_daemon_table_150805`), else `CreateFileMappingW(PAGE_READWRITE,
    /// mapping_len)` with a DACL granting only the current user mask `0x80000002`; then maps `mapping_len`
    /// bytes. Returns the view and whether this call created the section (`GetLastError() !=
    /// ERROR_ALREADY_EXISTS`). The new section content is zero. `ipc::shared_memory::SharedMemory` tries
    /// [`open_section`](Self::open_section) first, so this only runs when the section was absent a moment
    /// ago; an existing section must be returned with `created == false`, never replaced.
    fn create_section(&self, name: &str, mapping_len: usize) -> Option<(Box<dyn Section>, bool)>;

    /// `openCreatedMemoryMap` primitive (`0x1400779A0`): `OpenFileMappingW(FILE_MAP_WRITE)` of an existing
    /// section and maps all of it (the view may be rounded up to whole pages, so the header of the section,
    /// not [`Section::byte_len`], is authoritative for the payload size). `None` when it does not exist.
    fn open_section(&self, name: &str) -> Option<Box<dyn Section>>;

    /// Creates a pipe endpoint. `resource_name` is the name after `\\.\pipe\` (for example
    /// `SC0_amdmsg_CS`). A server endpoint creates the pipe instance (first instance, one instance
    /// maximum, inbound for a reader / outbound for a writer, current-user DACL) and starts listening; a
    /// client endpoint tries to connect. An unusable endpoint (bad name, `CreateNamedPipeW` refused) is still
    /// returned and reports `is_valid() == false`.
    fn open_pipe(&self, resource_name: &str, role: PipeRole, is_reader: bool) -> Box<dyn PipeEndpoint>;
}
