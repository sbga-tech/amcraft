//! `amdaemon::server::BackupController` (vtable `0x1405C7678`): NVRAM backup records shared with the game
//! through the protected `amdaemon::ipc::Backup` memory object.
//!
//! * **Setup** builds the record list (device offsets, 512-byte slots), selects the game id, publishes the
//!   record layout to the shared memory and loads every record from libam NVRAM into it, publishing one
//!   [`BackupRecordStatus`] per record.
//! * **Write** copies the listed records out of the shared memory and commits them to NVRAM.
//!
//! All NVRAM I/O happens synchronously inside the accept edge (frame *N* `postExecute`); the response is
//! published by the next `preExecute` because the checker always reports success.
//!
//! Evidence: `spec/controllers/backup_appimage.md` §1, §2, §4 (claim ids `backup_appimage.backup.*`,
//! `backup_appimage.device.*`, `backup_appimage.port.*`).

use std::fmt::Write as _;

use crate::executor::{RequestExecutor, RequestState};
use crate::manager::Manager;
use crate::schema::backup::{
    BackupDevice, BackupRecordStatus, BackupTask, MAX_RECORDS, Record, SETUP_GAME_ID_OFFSET, SETUP_RECORDS_OFFSET,
    SetupRecord, SetupRecordRef, WRITE_INDICES_OFFSET,
};
use crate::schema::image::{ClientImage, ServerImage};
use crate::schema::text::fit_wide;
use libam::nvram::{BackupPort, GameId, NvramDevice};
use libam::result::{AmError, AmResult};

use crate::controllers::controller::{Controller, ControllerId, Ctx, commit_check};

/// `am::Result` codes produced by this controller (`spec` §1.4.4, §1.5).
mod code {
    /// Invalid device type in a Setup record; libam rejected a record add.
    pub(super) const INVALID: u32 = 1;
    /// `Backup::setupRecords` failed.
    pub(super) const SETUP_RECORDS: u32 = 2;
    /// A device is too small for the requested records; unknown task.
    pub(super) const REJECTED: u32 = 5;
    /// libam load result: the record belongs to another game id.
    pub(super) const OTHER_GAME_ID: u32 = 4;
}

/// Number of NVRAM devices (`BackupDevice` values `0..=2`).
const DEVICE_COUNT: usize = 3;
/// Size of the libam record header kept in front of every payload (`data+8`).
const RECORD_HEADER_SIZE: usize = 8;

impl From<BackupDevice> for NvramDevice {
    /// The libam device a schema [`BackupDevice`] names (`Eeprom = 0`, `Sram = 1`, `File = 2` on both sides).
    fn from(device: BackupDevice) -> Self {
        match device {
            BackupDevice::Eeprom => Self::Eeprom,
            BackupDevice::Sram => Self::Sram,
            BackupDevice::File => Self::File,
        }
    }
}

/// `amdaemon::ipc::Backup` (`spec/controllers/backup_appimage.md` §3): the protected shared memory the
/// controller mirrors records into. Implemented by `crate::ipc::backup::Backup` through the daemon.
pub trait BackupMemory {
    /// `ipc::Backup::create` (`0x14006DD00`): create or attach the shared memory for `relation_index`
    /// with `size` bytes of record body. `false` on failure.
    fn create(&mut self, relation_index: i32, size: u32) -> bool;

    /// `ipc::Backup::setupRecords` (`0x14006E0A0`): publishes the record sizes into the encrypted header.
    /// `false` when `records.len() > 0x40`, the shared memory is not created, the slots do not fit, or the
    /// header cannot be encrypted.
    fn setup_records(&mut self, records: &[Record]) -> bool;

    /// `ipc::Backup::recordCount` (`0x14006E720`): record count of the current header.
    fn record_count(&self) -> u32;

    /// `ipc::Backup::bodyCapacity` (`0x14006E6D0`): `0` while the shared memory is absent.
    fn body_capacity(&self) -> u32;

    /// `ipc::Backup::getRecordSize` (`0x14006E730`): `None` when `index >= record count`.
    fn record_size(&self, index: u32) -> Option<u32>;

    /// `ipc::Backup::getRecord` (`0x14006E7C0`): decrypts record `index` into `out[..size]`. `false` on a bad
    /// index, when `out` is shorter than the record, or on a decrypt/CRC failure.
    fn get_record(&self, index: u32, out: &mut [u8]) -> bool;

    /// `ipc::Backup::setRecord` (`0x14006E8D0`): encrypts `data[..size]` into slot `index`. `false` on a bad
    /// index, when `data` is shorter than the record, or when encryption fails.
    fn set_record(&mut self, index: u32, data: &[u8]) -> bool;
}

/// One entry of the record list built by the last Setup (`RecordEntry`, 40 bytes in the original).
#[derive(Debug)]
struct RecordEntry {
    /// Device the record lives on.
    device: BackupDevice,
    /// Device-relative offset assigned by the Setup (running sum of the previous slots on the same device).
    offset: u32,
    /// Zero-filled slot of `alignedSize` bytes: the 8-byte libam header followed by the payload.
    data: Vec<u8>,
}

/// Everything the acceptor touches (the executor is kept apart so the acceptor can borrow it mutably).
struct Store {
    port: Box<dyn BackupPort>,
    /// `this+0x88`: the `ipc::Backup` object.
    memory: Box<dyn BackupMemory>,
    /// `this+0x80`: Server relation index, the suffix of the shared-memory name.
    relation_index: i32,
    /// `this+0x90`: record list of the last Setup.
    records: Vec<RecordEntry>,
    /// `this+0xA8`: game id text last applied by Setup (`wchar_t[5]`).
    game_id: [u16; 5],
}

/// `amdaemon::server::BackupController`.
pub struct BackupController {
    exec: RequestExecutor,
    store: Store,
}

impl std::fmt::Debug for BackupController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let game_id = &self.store.game_id;
        let length = game_id.iter().position(|&unit| unit == 0).unwrap_or(game_id.len());
        f.debug_struct("BackupController")
            .field("exec", &self.exec)
            .field("relation_index", &self.store.relation_index)
            .field("records", &self.store.records.len())
            .field("game_id", &String::from_utf16_lossy(&game_id[..length]))
            .finish_non_exhaustive()
    }
}

impl BackupController {
    /// Constructs the controller (`0x140020580`). The original also registers the APM-state listener; here
    /// [`Controller::on_app_state`] is that listener.
    ///
    /// `relation_index` is the Server relation index (`0..32`); it is reinterpreted as the original's `i32`.
    pub fn new(port: Box<dyn BackupPort>, memory: Box<dyn BackupMemory>, relation_index: u32) -> Self {
        Self {
            exec: RequestExecutor::default(),
            store: Store { port, memory, relation_index: relation_index as i32, records: Vec::new(), game_id: [0; 5] },
        }
    }
}

/// Record `index` of a Setup request read straight from the `ClientData` image.
///
/// The controller never bounds-checks `recordCount`, so indices past `records[64]` read the fields that
/// follow the block. `None` once the read leaves the `ClientData` root.
fn setup_record(client: &ClientImage, index: usize) -> Option<SetupRecordRef<'_>> {
    let start = SETUP_RECORDS_OFFSET.checked_add(index.checked_mul(SetupRecord::SCHEMA_SIZE)?)?;
    let (record, _) = SetupRecord::access_prefix(client.bytes().get(start..)?).ok()?;
    Some(record)
}

/// Record index `index` of a Write request, read like [`setup_record`] (`recordIndexCount` is unchecked too).
fn write_index(client: &ClientImage, index: usize) -> Option<u32> {
    let start = WRITE_INDICES_OFFSET.checked_add(index.checked_mul(size_of::<u32>())?)?;
    let (word, _) = client.bytes().get(start..)?.split_first_chunk()?;
    Some(u32::from_le_bytes(*word))
}

/// `taskName` (`0x14006D7A0`).
fn task_name(task: BackupTask) -> &'static str {
    match task {
        BackupTask::Setup => "Setup",
        BackupTask::Write => "Write",
    }
}

/// `deviceName` (`0x140062560`).
fn device_name(device: BackupDevice) -> &'static str {
    match device {
        BackupDevice::Eeprom => "Eeprom",
        BackupDevice::Sram => "Sram",
        BackupDevice::File => "File",
    }
}

/// Request trace text (`0x140021A80`). Printing the null name of an unknown task or device sets the badbit of
/// the original `wostringstream`, which silently drops everything after it.
fn request_text(client: &ClientImage) -> String {
    let request = client.backup();
    let mut text = String::from("Backup - task: ");
    let Some(task) = request.task_kind() else {
        return text;
    };
    text.push_str(task_name(task));
    match task {
        BackupTask::Setup => {
            // The id is printed as a C string: the five-unit array need not be terminated.
            let units: Vec<u16> = client
                .bytes()
                .get(SETUP_GAME_ID_OFFSET..)
                .unwrap_or_default()
                .chunks_exact(size_of::<u16>())
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .take_while(|&unit| unit != 0)
                .collect();
            let _ = write!(text, ", GameID: {}, , record: ", String::from_utf16_lossy(&units));
            for index in 0..request.setup().record_count() as usize {
                let Some(record) = setup_record(client, index) else {
                    break;
                };
                let _ = write!(text, "\n  record[{index}] - device: ");
                let Some(device) = record.device_kind() else {
                    break;
                };
                let _ = write!(text, "{}, size: {}", device_name(device), record.size());
            }
        }
        BackupTask::Write => {
            text.push_str(", record: ");
            for index in 0..request.write().record_index_count() as usize {
                let Some(value) = write_index(client, index) else {
                    break;
                };
                if index != 0 {
                    text.push_str(", ");
                }
                let _ = write!(text, "{value}");
            }
        }
    }
    text
}

impl Store {
    /// Facade check shared by every entry point. Returns libam's result code.
    fn acquire(&self) -> AmResult {
        self.port.acquire_nvram()
    }

    /// `getSize` (`0x140020B10`): the total capacity of the three devices, summed in 32 bits.
    fn size(&self) -> u32 {
        if self.acquire().is_err() {
            tracing::warn!("libam is not ready");
            return 0;
        }
        [NvramDevice::File, NvramDevice::Sram, NvramDevice::Eeprom]
            .into_iter()
            .fold(0u32, |sum, device| sum.wrapping_add(self.port.device_size(device) as u32))
    }

    /// vf5 body (`0x140020760`): create the shared memory once, then re-apply the stored game id.
    fn clear(&mut self) {
        if self.memory.body_capacity() == 0 {
            let size = self.size();
            if !self.memory.create(self.relation_index, size) {
                tracing::warn!("creating the backup memory failed");
            }
        }
        if !self.records.is_empty() {
            let game_id = self.game_id;
            // The result is ignored by the original.
            let _ = self.set_game_id(&game_id);
        }
    }

    /// `BackupController::accept` (`0x1400208F0`).
    fn accept(&mut self, manager: &mut Manager, client: &ClientImage, server: &mut ServerImage) -> RequestState {
        if manager.debug_dump() {
            let text = request_text(client);
            tracing::info!("{text}");
        }
        if self.acquire().is_err() {
            tracing::warn!("libam is not ready");
            return RequestState::Failed;
        }
        let result = match client.backup().task_kind() {
            Some(BackupTask::Setup) => self.setup(client).and_then(|()| self.load(server)),
            Some(BackupTask::Write) => self.save(manager, client),
            None => Err(AmError::code(code::REJECTED)),
        };
        if result.is_ok() { RequestState::Pending } else { RequestState::Failed }
    }

    /// Task 0 front half (`0x140020C40`).
    fn setup(&mut self, client: &ClientImage) -> AmResult {
        let requested = self.create_record_list(client).inspect_err(|_| self.records.clear())?;
        self.set_game_id(&client.backup().setup().game_id().copy_into())?;
        if self.memory.setup_records(&requested) {
            Ok(())
        } else {
            Err(AmError::with_message(code::SETUP_RECORDS, "Failed to setup records."))
        }
    }

    /// `createRecordList` (`0x1400213A0`). Returns the validated records the Setup publishes to the shared memory.
    ///
    /// A device that overflows is logged before the entry is allocated; the caller clears the list on any
    /// error, so the unallocated entry is unobservable.
    fn create_record_list(&mut self, client: &ClientImage) -> AmResult<Vec<Record>> {
        self.records.clear();
        let acquired = self.acquire();
        if acquired.is_err() {
            tracing::warn!("libam is not ready");
        }
        acquired?;
        let count = client.backup().setup().record_count() as usize;
        let mut requested = Vec::with_capacity(count.min(MAX_RECORDS));
        let mut running = [0u32; DEVICE_COUNT];
        for index in 0..count {
            let record = setup_record(client, index)
                .and_then(|record| Some(Record { device: record.device_kind()?, size: record.size() }));
            let Some(Record { device, size }) = record else {
                // A record past the end of `ClientData` fails like an unknown device.
                tracing::error!("invalid device type");
                return Err(AmError::code(code::INVALID));
            };
            let alignment = self.port.device_alignment(device.into());
            // `roundUp(size + 8, alignment)` for a power-of-two alignment, 0 for a zero alignment (32-bit).
            let aligned = !alignment.wrapping_sub(1) & alignment.wrapping_add(size).wrapping_add(7);
            let running_offset = &mut running[device as usize];
            let offset = *running_offset;
            *running_offset = offset.wrapping_add(aligned);
            if self.port.device_size(device.into()) < u64::from(*running_offset) {
                tracing::warn!(device = device_name(device), "device is too small for the records");
                return Err(AmError::code(code::REJECTED));
            }
            self.records.push(RecordEntry { device, offset, data: vec![0; aligned as usize] });
            requested.push(Record { device, size });
        }
        Ok(requested)
    }

    /// `setGameId` (`0x1400218B0`). `game_id` is the raw `wchar_t[5]` text of the request (or the stored one).
    fn set_game_id(&mut self, game_id: &[u16; 5]) -> AmResult {
        let acquired = self.acquire();
        if acquired.is_err() {
            tracing::warn!("libam is not ready");
        }
        acquired?;
        if game_id[0] == 0 {
            self.port.set_default_game_id();
        } else {
            let Some(parsed) = GameId::parse(game_id) else {
                tracing::error!("invalid game id");
                return Err(AmError::code(code::INVALID));
            };
            self.port.set_game_id(&parsed);
        }
        // `wcsncpy_s(this+0xA8, 5, gameId, 4)`: the original (not upper-cased) text is kept.
        let fitted = fit_wide(game_id, self.game_id.len());
        let units = fitted.as_slice_with_nul();
        self.game_id[..units.len()].copy_from_slice(units);
        Ok(())
    }

    /// Payload window of a record slot: `data+8` for the size the shared memory knows for `index`. Empty when
    /// the index is unknown or the slot is smaller than the stored size, which both ports treat as a failure.
    fn payload<'a>(memory: &dyn BackupMemory, index: u32, data: &'a mut [u8]) -> &'a mut [u8] {
        let Some(size) = memory.record_size(index) else {
            return Default::default();
        };
        let end = RECORD_HEADER_SIZE.saturating_add(size as usize);
        data.get_mut(RECORD_HEADER_SIZE..end).unwrap_or_default()
    }

    /// `load` (`0x140020CF0`): reads every record of the list from libam and stores it into the shared memory.
    ///
    /// Only libam code 1 fails the request; code 4 marks the record [`BackupRecordStatus::DiffApp`], every other
    /// failure [`BackupRecordStatus::BrokenData`].
    fn load(&mut self, server: &mut ServerImage) -> AmResult {
        let acquired = self.acquire();
        if acquired.is_err() {
            tracing::warn!("libam is not ready");
        }
        acquired?;
        let count = self.memory.record_count() as usize;
        if self.records.len() < count {
            // `r` stays untouched: the request still succeeds.
            tracing::warn!("record count does not match");
            return Ok(());
        }
        let mut result = Ok(());
        let mut block = server.backup_mut();
        let mut statuses = block.record_status_mut();
        for (index, entry) in self.records.iter_mut().enumerate().take(count.min(MAX_RECORDS)) {
            let published = match self.port.load_record(entry.device.into(), entry.offset, &mut entry.data) {
                Ok(()) => {
                    let index = index as u32;
                    let payload = Self::payload(&*self.memory, index, &mut entry.data);
                    if self.memory.set_record(index, payload) {
                        BackupRecordStatus::Valid
                    } else {
                        BackupRecordStatus::BrokenData
                    }
                }
                Err(error) if error.code == code::INVALID => {
                    result = Err(AmError::code(code::INVALID));
                    BackupRecordStatus::BrokenData
                }
                Err(error) if error.code == code::OTHER_GAME_ID => BackupRecordStatus::DiffApp,
                Err(_) => BackupRecordStatus::BrokenData,
            };
            statuses.set(index, published).expect("a closed enum value within the status array");
        }
        result
    }

    /// `save` (`0x140020FB0`): copies the listed records out of the shared memory and commits them.
    ///
    /// Per-record failures are logged and skipped; only the final commit result decides the request. An index
    /// outside the list ended the original process (`std::_Xout_of_range`); here the manager terminates.
    fn save(&mut self, manager: &mut Manager, client: &ClientImage) -> AmResult {
        let acquired = self.acquire();
        if acquired.is_err() {
            tracing::warn!("libam is not ready");
        }
        acquired?;
        for position in 0..client.backup().write().record_index_count() as usize {
            let index = write_index(client, position);
            let Some((index, entry)) =
                index.and_then(|index| Some((index, self.records.get_mut(usize::try_from(index).ok()?)?)))
            else {
                manager.terminate("record index is out of range");
                return Err(AmError::code(AmError::FAILED));
            };
            let out = Self::payload(&*self.memory, index, &mut entry.data);
            if !self.memory.get_record(index, out) {
                tracing::error!("reading the record failed");
            }
            if let Err(error) = self.port.add_record(entry.device.into(), entry.offset, &entry.data) {
                tracing::warn!(code = error.code, "adding the record failed");
            }
        }
        // The commit overwrites the result of every earlier add.
        let result = self.port.commit_records();
        if let Err(error) = &result {
            tracing::warn!(code = error.code, "saving the records failed");
        }
        result
    }
}

impl Controller for BackupController {
    fn id(&self) -> ControllerId {
        ControllerId::Backup
    }

    /// vf3 `0x140020730`: a request is accepted and its response not yet published.
    fn is_busy(&self, ctx: &Ctx<'_>) -> bool {
        self.exec.is_busy(ctx.client.backup().generation(), ctx.server.backup().response().generation())
    }

    /// vf5 `0x140020760` (ready rising edge). Despite its name it does not touch `ServerData`.
    fn clear_server_data(&mut self, _ctx: &mut Ctx<'_>) {
        self.store.clear();
    }

    /// vf6 `0x140020870`: `checkResult`; the checker (`0x140023310`) always answers success.
    fn update_server_data(&mut self, ctx: &mut Ctx<'_>) {
        let request_generation = ctx.client.backup().generation();
        let mut block = ctx.server.backup_mut();
        let response_generation = block.response().generation();
        let outcome = self.exec.check_result(request_generation, response_generation, || RequestState::Succeeded);
        commit_check(outcome, &mut block.response_mut());
    }

    /// vf7 `0x140020880`: runs the acceptor synchronously when a new request generation is visible.
    fn accept_requests(&mut self, ctx: &mut Ctx<'_>) {
        let request_generation = ctx.client.backup().generation();
        let response_generation = ctx.server.backup().response().generation();
        let Ctx { server, client, manager, .. } = ctx;
        self.exec.accept(request_generation, response_generation, || self.store.accept(manager, client, server));
    }

    /// APM listener (lambda `6d3f0272`, `0x1400232E0`): any state drops the record list; the shared memory, the
    /// stored game id and the response are kept.
    fn on_app_state(&mut self, _state: u32, _ctx: &mut Ctx<'_>) {
        self.store.records.clear();
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    use crate::schema::backup::ClientBackupPatch;

    use super::*;
    use crate::controllers::controller::ControllerSlot;
    use crate::controllers::controller::testing::Harness;

    const FILE_SIZE: u64 = 4_186_112;

    /// Observable libam / `ipc::Backup` interaction.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        SetGameId([u16; 4]),
        SetDefaultGameId,
        Load { device: BackupDevice, offset: u32, len: usize },
        Add { device: BackupDevice, offset: u32, data: Vec<u8> },
        Commit,
        Create { relation_index: i32, size: u32 },
        SetupRecords(Vec<Record>),
        SetRecord { index: u32, data: Vec<u8> },
        GetRecord(u32),
    }

    /// Scripted libam NVRAM plus a model of the protected `ipc::Backup` memory.
    #[derive(Default)]
    struct Libam {
        down: bool,
        eeprom_size: u64,
        sram_size: u64,
        file_size: u64,
        load_results: VecDeque<AmResult>,
        add_results: VecDeque<AmResult>,
        commit_error: Option<u32>,
        refuse_set_record: bool,
        capacity: u32,
        sizes: Vec<u32>,
        slots: Vec<Option<Vec<u8>>>,
        loads: u8,
        calls: Vec<Call>,
    }

    type Shared = Rc<RefCell<Libam>>;

    struct FakePort(Shared);

    /// The schema device a libam device came from, so that the recorded calls name schema devices.
    fn schema_device(device: NvramDevice) -> BackupDevice {
        match device {
            NvramDevice::Eeprom => BackupDevice::Eeprom,
            NvramDevice::Sram => BackupDevice::Sram,
            NvramDevice::File => BackupDevice::File,
        }
    }

    impl BackupPort for FakePort {
        fn acquire_nvram(&self) -> AmResult {
            if self.0.borrow().down { Err(AmError::code(2)) } else { Ok(()) }
        }

        fn device_size(&self, device: NvramDevice) -> u64 {
            let libam = self.0.borrow();
            match device {
                NvramDevice::Eeprom => libam.eeprom_size,
                NvramDevice::Sram => libam.sram_size,
                NvramDevice::File => libam.file_size,
            }
        }

        fn device_alignment(&self, device: NvramDevice) -> u32 {
            if device == NvramDevice::File { 512 } else { 0 }
        }

        fn set_game_id(&mut self, game_id: &GameId) {
            self.0.borrow_mut().calls.push(Call::SetGameId(*game_id.units()));
        }

        fn set_default_game_id(&mut self) {
            self.0.borrow_mut().calls.push(Call::SetDefaultGameId);
        }

        fn load_record(&mut self, device: NvramDevice, offset: u32, buffer: &mut [u8]) -> AmResult {
            let mut libam = self.0.borrow_mut();
            libam.calls.push(Call::Load { device: schema_device(device), offset, len: buffer.len() });
            let result = libam.load_results.pop_front().unwrap_or(Ok(()));
            if result.is_ok() {
                // Distinct payload per load, behind the 8-byte libam header.
                let fill = 0x40 + libam.loads;
                if let Some(payload) = buffer.get_mut(RECORD_HEADER_SIZE..) {
                    payload.fill(fill);
                }
            }
            libam.loads += 1;
            result
        }

        fn add_record(&mut self, device: NvramDevice, offset: u32, buffer: &[u8]) -> AmResult {
            let mut libam = self.0.borrow_mut();
            libam.calls.push(Call::Add { device: schema_device(device), offset, data: buffer.to_vec() });
            libam.add_results.pop_front().unwrap_or(Ok(()))
        }

        fn commit_records(&mut self) -> AmResult {
            let mut libam = self.0.borrow_mut();
            libam.calls.push(Call::Commit);
            libam.commit_error.map_or(Ok(()), |code| Err(AmError::code(code)))
        }
    }

    struct FakeMemory(Shared);

    impl BackupMemory for FakeMemory {
        fn create(&mut self, relation_index: i32, size: u32) -> bool {
            let mut libam = self.0.borrow_mut();
            libam.calls.push(Call::Create { relation_index, size });
            if size == 0 {
                return false;
            }
            libam.capacity = size;
            true
        }

        fn setup_records(&mut self, records: &[Record]) -> bool {
            let mut libam = self.0.borrow_mut();
            libam.calls.push(Call::SetupRecords(records.to_vec()));
            if records.len() > 0x40 {
                return false;
            }
            libam.sizes = records.iter().map(|record| record.size).collect();
            libam.slots = vec![None; records.len()];
            true
        }

        fn record_count(&self) -> u32 {
            self.0.borrow().sizes.len() as u32
        }

        fn body_capacity(&self) -> u32 {
            self.0.borrow().capacity
        }

        fn record_size(&self, index: u32) -> Option<u32> {
            self.0.borrow().sizes.get(index as usize).copied()
        }

        fn get_record(&self, index: u32, out: &mut [u8]) -> bool {
            let mut libam = self.0.borrow_mut();
            libam.calls.push(Call::GetRecord(index));
            let Some(Some(stored)) = libam.slots.get(index as usize) else {
                return false;
            };
            let Some(window) = out.get_mut(..stored.len()) else {
                return false;
            };
            window.copy_from_slice(stored);
            true
        }

        fn set_record(&mut self, index: u32, data: &[u8]) -> bool {
            let mut libam = self.0.borrow_mut();
            let payload = data.to_vec();
            libam.calls.push(Call::SetRecord { index, data: payload.clone() });
            let size = libam.sizes.get(index as usize).copied().unwrap_or(u32::MAX) as usize;
            if libam.refuse_set_record || data.len() < size {
                return false;
            }
            libam.slots[index as usize] = Some(payload[..size].to_vec());
            true
        }
    }

    fn libam() -> Shared {
        Rc::new(RefCell::new(Libam { file_size: FILE_SIZE, ..Libam::default() }))
    }

    fn controller(libam: &Shared) -> BackupController {
        BackupController::new(Box::new(FakePort(Rc::clone(libam))), Box::new(FakeMemory(Rc::clone(libam))), 3)
    }

    fn calls(libam: &Shared) -> Vec<Call> {
        libam.borrow().calls.clone()
    }

    fn loads(libam: &Shared) -> Vec<(BackupDevice, u32, usize)> {
        let calls = calls(libam);
        calls
            .iter()
            .filter_map(|call| match call {
                Call::Load { device, offset, len } => Some((*device, *offset, *len)),
                _ => None,
            })
            .collect()
    }

    /// `ClientInfo` bytes of a Setup request: the game id, the record count and the raw `(device, size)` pairs.
    fn setup_info(game_id: &str, record_count: u32, records: &[(u32, u32)]) -> [u8; 0x210] {
        let mut info = [0; 0x210];
        for (pair, unit) in info[..10].chunks_exact_mut(2).zip(game_id.encode_utf16().take(4)) {
            pair.copy_from_slice(&unit.to_le_bytes());
        }
        info[0x0C..0x10].copy_from_slice(&record_count.to_le_bytes());
        for (slot, (device, size)) in info[0x10..].chunks_exact_mut(8).zip(records) {
            slot[..4].copy_from_slice(&device.to_le_bytes());
            slot[4..].copy_from_slice(&size.to_le_bytes());
        }
        info
    }

    /// `ClientInfo` bytes of a Write request: the index count, then the indices.
    fn write_info(indices: &[u32]) -> [u8; 0x210] {
        let mut info = [0; 0x210];
        info[..4].copy_from_slice(&(indices.len() as u32).to_le_bytes());
        for (slot, index) in info[4..].chunks_exact_mut(4).zip(indices) {
            slot.copy_from_slice(&index.to_le_bytes());
        }
        info
    }

    /// The game writes a request (what the IPC exchange between pre and post delivers).
    fn request(h: &mut Harness, generation: u32, task: u32, info: [u8; 0x210]) {
        let patch = ClientBackupPatch { generation: Some(generation), task: Some(task), info: Some(&info) };
        h.client.backup_mut().copy_from(&patch).expect("plain fields");
    }

    fn setup_request(h: &mut Harness, generation: u32, game_id: &str, records: &[Record]) {
        let raw: Vec<(u32, u32)> = records.iter().map(|record| (record.device as u32, record.size)).collect();
        request(h, generation, BackupTask::Setup as u32, setup_info(game_id, raw.len() as u32, &raw));
    }

    fn write_request(h: &mut Harness, generation: u32, indices: &[u32]) {
        request(h, generation, BackupTask::Write as u32, write_info(indices));
    }

    /// Accept edge of frame N followed by the `preExecute` of frame N+1.
    fn serve(controller: &mut BackupController, h: &mut Harness) {
        controller.accept_requests(&mut h.ctx());
        controller.update_server_data(&mut h.ctx());
    }

    fn file(size: u32) -> Record {
        Record { device: BackupDevice::File, size }
    }

    fn status(h: &Harness, index: usize) -> BackupRecordStatus {
        h.server.backup().record_status().get(index).expect("recovered status value")
    }

    fn response(h: &Harness) -> (u32, bool) {
        let block = h.server.backup();
        let response = block.response();
        (response.generation(), response.succeeded())
    }

    #[test]
    fn setup_publishes_valid_statuses_one_update_after_the_accept_edge() {
        let libam = libam();
        let mut controller = controller(&libam);
        let mut h = Harness::running();
        setup_request(&mut h, 1, "SBZZ", &[file(0x100), file(0x20)]);

        controller.accept_requests(&mut h.ctx());
        assert!(controller.is_busy(&h.ctx()));
        assert_eq!(response(&h), (0, false));

        controller.update_server_data(&mut h.ctx());
        assert!(!controller.is_busy(&h.ctx()));
        assert_eq!(response(&h), (1, true));
        assert_eq!((status(&h, 0), status(&h, 1)), (BackupRecordStatus::Valid, BackupRecordStatus::Valid));
        let game_id = [b'S', b'B', b'Z', b'Z'].map(u16::from);
        assert_eq!(
            calls(&libam),
            [
                Call::SetGameId(game_id),
                Call::SetupRecords(vec![file(0x100), file(0x20)]),
                Call::Load { device: BackupDevice::File, offset: 0, len: 512 },
                Call::SetRecord { index: 0, data: vec![0x40; 0x100] },
                Call::Load { device: BackupDevice::File, offset: 512, len: 512 },
                Call::SetRecord { index: 1, data: vec![0x41; 0x20] },
            ]
        );
    }

    #[test]
    fn file_slots_are_eight_header_bytes_plus_payload_rounded_up_to_512_and_packed_from_offset_zero() {
        let libam = libam();
        let mut controller = controller(&libam);
        let mut h = Harness::running();
        setup_request(&mut h, 1, "SBZZ", &[file(0x1F8), file(0x1F9), file(0), file(0x400)]);
        serve(&mut controller, &mut h);

        let dev = BackupDevice::File;
        assert_eq!(loads(&libam), [(dev, 0, 512), (dev, 512, 1024), (dev, 1536, 512), (dev, 2048, 1536)]);
        assert_eq!(response(&h), (1, true));
    }

    #[test]
    fn offsets_restart_at_zero_on_every_setup() {
        let libam = libam();
        let mut controller = controller(&libam);
        let mut h = Harness::running();
        setup_request(&mut h, 1, "SBZZ", &[file(0x10), file(0x10)]);
        serve(&mut controller, &mut h);
        setup_request(&mut h, 2, "SBZZ", &[file(0x10)]);
        serve(&mut controller, &mut h);

        let loads = loads(&libam);
        assert_eq!(loads[2], (BackupDevice::File, 0, 512));
    }

    #[test]
    fn eeprom_and_sram_records_fail_the_request_yet_later_records_still_load() {
        let libam = libam();
        libam.borrow_mut().load_results = VecDeque::from([Err(AmError::code(1)), Err(AmError::code(1))]);
        let mut controller = controller(&libam);
        let mut h = Harness::running();
        let records =
            [Record { device: BackupDevice::Eeprom, size: 4 }, Record { device: BackupDevice::Sram, size: 4 }, file(8)];
        setup_request(&mut h, 1, "SBZZ", &records);
        serve(&mut controller, &mut h);

        assert_eq!(response(&h), (1, false));
        assert_eq!(
            [status(&h, 0), status(&h, 1), status(&h, 2)],
            [BackupRecordStatus::BrokenData, BackupRecordStatus::BrokenData, BackupRecordStatus::Valid]
        );
        // Descriptors of unsupported devices are zero: zero-length slots at offset 0.
        assert_eq!(
            loads(&libam),
            [(BackupDevice::Eeprom, 0, 0), (BackupDevice::Sram, 0, 0), (BackupDevice::File, 0, 512)]
        );
    }

    #[test]
    fn only_libam_code_1_fails_setup_other_codes_become_record_statuses() {
        let libam = libam();
        libam.borrow_mut().load_results =
            [4, 5, 6, 2, 3].map(|code| Err(AmError::code(code))).into_iter().collect::<VecDeque<_>>();
        let mut controller = controller(&libam);
        let mut h = Harness::running();
        setup_request(&mut h, 1, "SBZZ", &[file(8); 5]);
        serve(&mut controller, &mut h);

        assert_eq!(response(&h), (1, true));
        let statuses: Vec<_> = (0..5).map(|i| status(&h, i)).collect();
        assert_eq!(
            statuses,
            [
                BackupRecordStatus::DiffApp,
                BackupRecordStatus::BrokenData,
                BackupRecordStatus::BrokenData,
                BackupRecordStatus::BrokenData,
                BackupRecordStatus::BrokenData,
            ]
        );
        // No record reached the protected memory.
        assert!(!calls(&libam).iter().any(|call| matches!(call, Call::SetRecord { .. })));
    }

    #[test]
    fn a_refused_set_record_marks_only_that_record_broken() {
        let libam = libam();
        libam.borrow_mut().refuse_set_record = true;
        let mut controller = controller(&libam);
        let mut h = Harness::running();
        setup_request(&mut h, 1, "SBZZ", &[file(8)]);
        serve(&mut controller, &mut h);

        assert_eq!(status(&h, 0), BackupRecordStatus::BrokenData);
        assert_eq!(response(&h), (1, true));
    }

    #[test]
    fn statuses_beyond_the_record_count_keep_their_previous_values() {
        let libam = libam();
        libam.borrow_mut().load_results = (0..3).map(|_| Err(AmError::code(4))).collect();
        let mut controller = controller(&libam);
        let mut h = Harness::running();
        setup_request(&mut h, 1, "SBZZ", &[file(8); 3]);
        serve(&mut controller, &mut h);
        setup_request(&mut h, 2, "SBZZ", &[file(8)]);
        serve(&mut controller, &mut h);

        assert_eq!(status(&h, 0), BackupRecordStatus::Valid);
        assert_eq!((status(&h, 1), status(&h, 2)), (BackupRecordStatus::DiffApp, BackupRecordStatus::DiffApp));
    }

    #[test]
    fn invalid_device_fails_setup_before_any_libam_call() {
        let libam = libam();
        let mut controller = controller(&libam);
        let mut h = Harness::running();
        let raw = [(BackupDevice::File as u32, 8), (3, 4)];
        request(&mut h, 1, BackupTask::Setup as u32, setup_info("SBZZ", 2, &raw));
        serve(&mut controller, &mut h);

        assert_eq!(response(&h), (1, false));
        assert!(calls(&libam).is_empty());
    }

    #[test]
    fn device_overflow_fails_setup() {
        let libam = libam();
        libam.borrow_mut().file_size = 1024;
        let mut controller = controller(&libam);
        let mut h = Harness::running();

        // Two 512-byte slots fill the device exactly.
        setup_request(&mut h, 1, "SBZZ", &[file(0x1F8), file(0x1F8)]);
        serve(&mut controller, &mut h);
        assert_eq!(response(&h), (1, true));

        setup_request(&mut h, 2, "SBZZ", &[file(0x1F8), file(0x1F8), file(0)]);
        serve(&mut controller, &mut h);
        assert_eq!(response(&h), (2, false));
    }

    #[test]
    fn invalid_game_id_fails_setup_without_reaching_libam() {
        let libam = libam();
        let mut controller = controller(&libam);
        let mut h = Harness::running();
        setup_request(&mut h, 1, "SBZZ", &[file(8), file(8)]);
        serve(&mut controller, &mut h);

        setup_request(&mut h, 2, "AB1D", &[file(8)]);
        let before = calls(&libam).len();
        serve(&mut controller, &mut h);

        assert_eq!(response(&h), (2, false));
        assert_eq!(calls(&libam).len(), before, "neither game id nor records reach libam");
    }

    #[test]
    fn empty_game_id_selects_the_default_and_lowercase_ids_reach_libam_uppercased() {
        let libam = libam();
        let mut controller = controller(&libam);
        let mut h = Harness::running();

        setup_request(&mut h, 1, "", &[file(8)]);
        serve(&mut controller, &mut h);
        assert_eq!(calls(&libam)[0], Call::SetDefaultGameId);

        setup_request(&mut h, 2, "sbzz", &[file(8)]);
        serve(&mut controller, &mut h);
        assert!(calls(&libam).contains(&Call::SetGameId([b'S', b'B', b'Z', b'Z'].map(u16::from))));
    }

    #[test]
    fn game_id_needs_exactly_four_letters() {
        let id = |text: &str| {
            let mut units = [0u16; 5];
            for (slot, unit) in units.iter_mut().zip(text.encode_utf16()) {
                *slot = unit;
            }
            GameId::parse(&units)
        };
        assert_eq!(id("sbzZ").map(|id| *id.units()), Some([b'S', b'B', b'Z', b'Z'].map(u16::from)));
        assert_eq!(id("SBZ"), None);
        assert_eq!(id("SB1Z"), None);
        assert_eq!(id("S Z1"), None);
        // Five letters leave no terminator inside the array.
        assert_eq!(id("SBZZZ"), None);
    }

    #[test]
    fn record_count_above_64_reads_adjacent_client_data_and_is_rejected_by_the_protected_memory() {
        let libam = libam();
        let mut controller = controller(&libam);
        let mut h = Harness::running();
        // `records[64]` ends the block; the 65th record is the zeroed field that follows (Eeprom, size 0).
        request(&mut h, 1, BackupTask::Setup as u32, setup_info("SBZZ", 65, &[]));
        serve(&mut controller, &mut h);

        assert_eq!(response(&h), (1, false));
        let setup_len = calls(&libam).iter().find_map(|call| match call {
            Call::SetupRecords(records) => Some(records.len()),
            _ => None,
        });
        assert_eq!(setup_len, Some(65));
        assert!(loads(&libam).is_empty());
    }

    #[test]
    fn unknown_task_fails_without_touching_libam() {
        let libam = libam();
        let mut controller = controller(&libam);
        let mut h = Harness::running();
        request(&mut h, 4, 7, [0; 0x210]);
        serve(&mut controller, &mut h);

        assert_eq!(response(&h), (4, false));
        assert!(calls(&libam).is_empty());
    }

    #[test]
    fn request_fails_while_libam_is_down() {
        let libam = libam();
        libam.borrow_mut().down = true;
        let mut controller = controller(&libam);
        let mut h = Harness::running();
        setup_request(&mut h, 1, "SBZZ", &[file(8)]);
        serve(&mut controller, &mut h);

        assert_eq!(response(&h), (1, false));
        assert!(calls(&libam).is_empty());
    }

    #[test]
    fn write_commits_header_and_payload_of_the_listed_records_in_request_order() {
        let libam = libam();
        let mut controller = controller(&libam);
        let mut h = Harness::running();
        setup_request(&mut h, 1, "SBZZ", &[file(0x100), file(0x20)]);
        serve(&mut controller, &mut h);
        let setup_calls = calls(&libam).len();

        // The game rewrote both records in the protected memory after the Setup.
        libam.borrow_mut().slots = vec![Some(vec![0x77; 0x100]), Some(vec![0x99; 0x20])];
        write_request(&mut h, 2, &[1, 0]);
        serve(&mut controller, &mut h);

        assert_eq!(response(&h), (2, true));
        // Header zero, rewritten payload at +8, the rest is what the Setup load left in the slot.
        let mut slot1 = vec![0x41u8; 512];
        slot1[..8].fill(0);
        slot1[8..8 + 0x20].fill(0x99);
        let mut slot0 = vec![0x40u8; 512];
        slot0[..8].fill(0);
        slot0[8..8 + 0x100].fill(0x77);
        assert_eq!(
            calls(&libam)[setup_calls..],
            [
                Call::GetRecord(1),
                Call::Add { device: BackupDevice::File, offset: 512, data: slot1 },
                Call::GetRecord(0),
                Call::Add { device: BackupDevice::File, offset: 0, data: slot0 },
                Call::Commit,
            ]
        );
    }

    #[test]
    fn failed_record_add_is_skipped_and_only_the_commit_decides_the_request() {
        let libam = libam();
        let mut controller = controller(&libam);
        let mut h = Harness::running();
        setup_request(&mut h, 1, "SBZZ", &[file(8)]);
        serve(&mut controller, &mut h);

        libam.borrow_mut().add_results = VecDeque::from([Err(AmError::code(1))]);
        write_request(&mut h, 2, &[0]);
        serve(&mut controller, &mut h);
        assert_eq!(response(&h), (2, true));

        libam.borrow_mut().commit_error = Some(6);
        write_request(&mut h, 3, &[0]);
        serve(&mut controller, &mut h);
        assert_eq!(response(&h), (3, false));
    }

    #[test]
    fn unreadable_record_still_commits_the_stale_buffer() {
        let libam = libam();
        libam.borrow_mut().load_results = VecDeque::from([Err(AmError::code(5))]);
        let mut controller = controller(&libam);
        let mut h = Harness::running();
        setup_request(&mut h, 1, "SBZZ", &[file(8)]);
        serve(&mut controller, &mut h);
        assert_eq!(status(&h, 0), BackupRecordStatus::BrokenData);

        write_request(&mut h, 2, &[0]);
        serve(&mut controller, &mut h);

        assert_eq!(response(&h), (2, true));
        let calls = calls(&libam);
        assert!(matches!(&calls[calls.len() - 2], Call::Add { data, .. } if data.iter().all(|&b| b == 0)));
        assert_eq!(calls.last(), Some(&Call::Commit));
    }

    #[test]
    fn write_index_outside_the_record_list_ends_the_process() {
        let libam = libam();
        let mut controller = controller(&libam);
        let mut h = Harness::running();
        setup_request(&mut h, 1, "SBZZ", &[file(8)]);
        serve(&mut controller, &mut h);

        write_request(&mut h, 2, &[5]);
        serve(&mut controller, &mut h);

        assert_eq!(response(&h), (2, false));
        assert!(h.manager.terminated());
        assert!(!calls(&libam).contains(&Call::Commit));
    }

    #[test]
    fn app_state_notification_drops_the_record_list_so_later_writes_have_nothing_to_index() {
        let libam = libam();
        let mut controller = controller(&libam);
        let mut h = Harness::running();
        setup_request(&mut h, 1, "SBZZ", &[file(8)]);
        serve(&mut controller, &mut h);
        assert_eq!(libam.borrow().sizes, [8], "header survives");

        controller.on_app_state(3, &mut h.ctx());
        assert_eq!(libam.borrow().sizes, [8]);

        write_request(&mut h, 2, &[0]);
        serve(&mut controller, &mut h);
        assert!(h.manager.terminated());
    }

    #[test]
    fn rising_edge_creates_the_protected_memory_once_with_the_total_device_capacity() {
        let libam = libam();
        {
            let mut libam = libam.borrow_mut();
            libam.sram_size = 0x100;
            libam.eeprom_size = 0x10;
        }
        let mut slot = ControllerSlot::new(Box::new(controller(&libam)));
        let mut h = Harness::running();

        slot.pre_execute(&mut h.ctx());
        assert_eq!(calls(&libam), [Call::Create { relation_index: 3, size: FILE_SIZE as u32 + 0x110 }]);

        // Falling and rising again finds the memory in place.
        h.manager.mark_finalized();
        slot.pre_execute(&mut h.ctx());
        h.manager.on_change_state(libam::sequence::ProcessState(3));
        slot.pre_execute(&mut h.ctx());
        assert_eq!(calls(&libam).len(), 1);
    }

    #[test]
    fn rising_edge_reapplies_the_stored_game_id_only_when_a_record_list_exists() {
        let libam = libam();
        let mut controller = controller(&libam);
        let mut h = Harness::running();
        controller.clear_server_data(&mut h.ctx());
        assert!(!calls(&libam).iter().any(|call| matches!(call, Call::SetGameId(_))));

        setup_request(&mut h, 1, "sbzz", &[file(8)]);
        serve(&mut controller, &mut h);
        let applied = calls(&libam).iter().filter(|call| matches!(call, Call::SetGameId(_))).count();
        controller.clear_server_data(&mut h.ctx());
        let reapplied = calls(&libam).iter().filter(|call| matches!(call, Call::SetGameId(_))).count();
        assert_eq!((applied, reapplied), (1, 2));
    }
}
