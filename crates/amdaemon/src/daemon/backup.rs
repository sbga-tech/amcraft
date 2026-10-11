//! `amdaemon::ipc::Backup` as seen by `BackupController` (`BackupController+0x88`).
//!
//! The controller only knows the [`BackupMemory`] contract; the protected shared memory itself lives in
//! `crate::ipc::backup`. This adapter connects the two.

use crate::controllers::backup::BackupMemory;
use crate::ipc::backup::Backup;
use crate::schema::backup::Record;
use backend::ipc::IpcPlatform;

/// `ipc::Backup` owned by the Backup controller (ctor `0x14006DC30`).
#[derive(Debug)]
pub struct IpcBackupMemory(Backup);

impl IpcBackupMemory {
    /// Creates the (not yet mapped) backup object on `platform`.
    pub fn new(platform: Box<dyn IpcPlatform>) -> Self {
        Self(Backup::new(platform))
    }
}

impl BackupMemory for IpcBackupMemory {
    fn create(&mut self, relation_index: i32, size: u32) -> bool {
        self.0.create(relation_index, size)
    }

    fn setup_records(&mut self, records: &[Record]) -> bool {
        self.0.setup_records(records)
    }

    fn record_count(&self) -> u32 {
        self.0.record_count()
    }

    fn body_capacity(&self) -> u32 {
        self.0.body_capacity()
    }

    fn record_size(&self, index: u32) -> Option<u32> {
        self.0.record_size(index)
    }

    fn get_record(&self, index: u32, out: &mut [u8]) -> bool {
        self.0.get_record(index, out)
    }

    fn set_record(&mut self, index: u32, data: &[u8]) -> bool {
        self.0.set_record(index, data)
    }
}
