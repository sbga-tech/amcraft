//! Backup blocks: `ServerData+0x268` (0x108) and `ClientData+0x0` (0x218).
//!
//! Evidence: `spec/controllers/backup_appimage.md` §1.2. Field names are inferred (the binary has no PDB); class
//! names `ClientSetupInfo`, `ClientWriteInfo`, `ServerSetupInfo` and `Record` come from strings embedded in
//! `BackupController` (`createRecordList` `0x1405C5EC0`, `save` `0x1405C5390`, `load` `0x1405C4D80`).
//!
//! The client block is the request header plus the 0x210-byte `ClientInfo` union as raw bytes. [`SetupInfo`] and
//! [`WriteInfo`] decode the two task payloads from those bytes; both accept every bit pattern, so decoding a
//! payload cannot fail.

use num_enum::TryFromPrimitive;
use zero_schema::zero;

use crate::schema::shared::Response;
use crate::schema::{client_block, server_block};

/// Capacity of every record array of the protocol (`Backup::setupRecords` rejects more, `0x14006E0A0`).
pub const MAX_RECORDS: usize = 64;

/// `ServerData.backup`: the shared response, then the per-record load statuses of the last Setup
/// (`ipc::backup::ServerResponse`).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerBackup {
    /// `taskNo` (generation) and `succeeded`, written by `checkResult` (`0x140023CD0`).
    pub response: Response,
    /// `ServerSetupInfo`, the only arm of the `ServerInfo` union: one status per record, written by
    /// `BackupController::load` for `i < record_count` only. `Write` publishes no payload.
    pub record_status: [BackupRecordStatus; MAX_RECORDS],
}

/// `ClientData.backup`: the request header and its per-task payload (`ipc::backup::ClientRequest`).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ClientBackup<'b> {
    /// Request generation (`taskNo`): accepted when it differs from the response's.
    pub generation: u32,
    /// Raw [`BackupTask`]; any value other than Setup/Write fails the request with libam code 5.
    pub task: u32,
    /// `ClientInfo` union (`0x210`, the `Setup` arm is its largest member), decoded by the
    /// [`ClientBackupRef`] payload accessors.
    pub info: &'b [u8; 0x210],
}

/// Client block placement: `ClientData+0x0`.
const CLIENT_BACKUP_OFFSET: usize = 0;

server_block!(backup, backup_mut: ServerBackup, ServerBackupRef, ServerBackupMut @ 0x268);
client_block!(backup, backup_mut: ClientBackup, ClientBackupRef, ClientBackupMut @ CLIENT_BACKUP_OFFSET);

/// `ClientData` offset of the `ClientInfo` union (the payloads start here).
const INFO_OFFSET: usize = CLIENT_BACKUP_OFFSET + ClientBackup::LAYOUT.fields()[2].offset();
/// `ClientData` offset of the Setup game id.
pub const SETUP_GAME_ID_OFFSET: usize = INFO_OFFSET + SetupInfo::LAYOUT.fields()[0].offset();
/// `ClientData` offset of the first Setup record. The controller never bounds-checks the record count, so
/// records past [`MAX_RECORDS`] are read from the `ClientData` fields that follow the block.
pub const SETUP_RECORDS_OFFSET: usize = INFO_OFFSET + SetupInfo::LAYOUT.fields()[2].offset();
/// `ClientData` offset of the first Write record index (also unchecked past [`MAX_RECORDS`]).
pub const WRITE_INDICES_OFFSET: usize = INFO_OFFSET + WriteInfo::LAYOUT.fields()[1].offset();

/// `ipc::backup::BackupRecordStatus`, one `u32` per record, stored by `BackupController::load` (`0x140020CF0`).
/// The names are inferred (no enum-to-string table exists for this enum).
#[zero]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum BackupRecordStatus {
    /// libam read the record and `Backup::setRecord` succeeded.
    Valid = 0,
    /// libam load code 4: the record belongs to a different game id.
    DiffApp = 1,
    /// Any other libam failure code or a `Backup::setRecord` failure.
    BrokenData = 2,
    /// Unresolved: named by the old documentation, but `amdaemon.exe` never stores it.
    InvalidCall = 0xFFFF_FFFF,
}

/// `ipc::backup::Task` (name table `0x1405EEE30` via `0x14006D7A0`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, TryFromPrimitive)]
#[repr(u32)]
pub enum BackupTask {
    /// Build the record list, select the game id and load every record (`L"Setup"`).
    Setup = 0,
    /// Save the listed records back to NVRAM (`L"Write"`).
    Write = 1,
}

/// `ipc::backup::BackupDevice` (name table `0x1405EBCD0` via `0x140062560`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, TryFromPrimitive)]
#[repr(u32)]
pub enum BackupDevice {
    /// EEPROM (`L"Eeprom"`).
    Eeprom = 0,
    /// Battery-backed SRAM (`L"Sram"`).
    Sram = 1,
    /// File device (`L"File"`).
    File = 2,
}

/// `ipc::backup::Record`: one requested record of a Setup, with a recognised device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record {
    /// Target device.
    pub device: BackupDevice,
    /// Payload size in bytes.
    pub size: u32,
}

/// `ipc::backup::ClientSetupInfo` (`0x210`): the `Setup` payload.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct SetupInfo {
    /// Game id: UTF-16, four significant characters. Not necessarily terminated within the array.
    pub game_id: [u16; 5],
    /// Number of records; **not bounds-checked** against [`MAX_RECORDS`] by the controller.
    pub record_count: u32,
    /// Requested records.
    pub records: [SetupRecord; MAX_RECORDS],
}

/// One requested record of a Setup as the game wrote it (`ipc::backup::Record`, 8 bytes).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct SetupRecord {
    /// Raw [`BackupDevice`].
    pub device: u32,
    /// Payload size in bytes.
    pub size: u32,
}

/// `ipc::backup::ClientWriteInfo` (`0x104`): the `Write` payload.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct WriteInfo {
    /// Number of indices; **not bounds-checked** against [`MAX_RECORDS`] by the controller.
    pub record_index_count: u32,
    /// Indices into the record list built by the last Setup.
    pub record_indices: [u32; MAX_RECORDS],
}

impl SetupRecordRef<'_> {
    /// The record's device, `None` for a value that names no device.
    pub fn device_kind(&self) -> Option<BackupDevice> {
        BackupDevice::try_from(self.device()).ok()
    }
}

impl<'w> ClientBackupRef<'w> {
    /// The request's task, `None` for an unknown value (the request fails).
    pub fn task_kind(&self) -> Option<BackupTask> {
        BackupTask::try_from(self.task()).ok()
    }

    /// The `Setup` payload.
    pub fn setup(&self) -> SetupInfoRef<'w> {
        let (setup, _) = SetupInfo::access_prefix(self.info()).expect("an integer payload accepts every bit pattern");
        setup
    }

    /// The `Write` payload.
    pub fn write(&self) -> WriteInfoRef<'w> {
        let (write, _) = WriteInfo::access_prefix(self.info()).expect("an integer payload accepts every bit pattern");
        write
    }
}

const _: () = {
    assert!(ServerBackup::SCHEMA_SIZE == 0x108);
    assert!(ClientBackup::SCHEMA_SIZE == 0x218);
    assert!(SetupInfo::SCHEMA_SIZE == 0x210);
    assert!(SetupRecord::SCHEMA_SIZE == 0x8);
    assert!(WriteInfo::SCHEMA_SIZE == 0x104);
    assert!(SETUP_GAME_ID_OFFSET == 0x8);
    assert!(SETUP_RECORDS_OFFSET == 0x18);
    assert!(WRITE_INDICES_OFFSET == 0xC);
    // The Setup records end with the union, so record `MAX_RECORDS` is the first byte after the block.
    assert!(
        SETUP_RECORDS_OFFSET + MAX_RECORDS * SetupRecord::SCHEMA_SIZE
            == CLIENT_BACKUP_OFFSET + ClientBackup::SCHEMA_SIZE
    );
};
