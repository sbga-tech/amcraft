//! `am::nvram::AppInfo`: the backup area the daemon's Backup controller reads and writes (`storage.md` 3.7).
//!
//! The area is the application file's `0x4000..` region with a mirror copy `0x3FE000` behind it. Each list entry
//! addresses a *slot* `{crc32, game-id word, payload}` of a 512-aligned size. `save` writes the mirror copy
//! first, bracketed by journal updates, so one valid copy always exists; the setup state machine replays an
//! unfinished save after a crash.

use libam_api::am_config::NvramConfig;
use libam_api::backup::GameId;

use super::device::Device;
use super::{APP_INFO_LOADED, NvramContext, Record, RecordStatus};
use crate::module::{Cx, ErrorCell, Lifecycle, StateWord};

/// Size of the File device (`0x3FE000`, `0x140249C90`).
const FILE_SIZE: u64 = 4_186_112;
/// Alignment of the File device.
const FILE_ALIGNMENT: u32 = 512;
/// Position of the area in `appfile.dat`.
const AREA_BASE: u32 = 0x4000;
/// Distance of the mirror copy.
const MIRROR_DELTA: u32 = 0x3FE000;
/// Maximum list length.
const MAX_ENTRIES: usize = 64;
/// Error latch for an unusable configuration or unsupported device (`0x1008`).
const LATCH_BAD_PARAMETER: u16 = 4104;
/// Error latch for a missing handle or game-id conversion failure (`0x1009`).
const LATCH_CONTEXT: u16 = 4105;

/// One list entry `{device 2, offset, size}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Entry {
    offset: u32,
    size: u32,
}

/// The crash-recovery record (`storage.md` 3.7.7): `[8]` active, `[9]` mirror, `[0xC]` count, `[0x10..]` entries.
#[derive(Debug, Default)]
struct Journal {
    image: Vec<u8>,
    loaded: bool,
}

impl Journal {
    fn active(&self) -> bool {
        self.image.get(8) == Some(&1)
    }

    fn mirror(&self) -> bool {
        self.image.get(9).is_some_and(|&flag| flag != 0)
    }

    fn entries(&self) -> Vec<Entry> {
        let Some(count) = self.image.get(0xC..0x10) else {
            return Vec::new();
        };
        let count = (u32::from_le_bytes([count[0], count[1], count[2], count[3]]) as usize).min(MAX_ENTRIES);
        (0..count)
            .filter_map(|index| {
                let at = 0x10 + index * 12;
                let raw = self.image.get(at..at + 12)?;
                Some(Entry {
                    offset: u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]),
                    size: u32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]]),
                })
            })
            .collect()
    }

    /// `set` (`0x14025DC10`): builds the new image; without I/O when the record is loaded and nothing changed,
    /// else both copies are written (mirror first) and the cache is updated.
    fn set(&mut self, nvram: &NvramContext, active: bool, mirror: bool, entries: &[Entry]) -> Result<(), RecordStatus> {
        let mut image = vec![0u8; Record::AppDataJournalInfo.size()];
        image[8] = u8::from(active);
        image[9] = u8::from(mirror);
        image[0xC..0x10].copy_from_slice(&(entries.len() as u32).to_le_bytes());
        for (index, entry) in entries.iter().enumerate() {
            let at = 0x10 + index * 12;
            image[at] = 2;
            image[at + 4..at + 8].copy_from_slice(&entry.offset.to_le_bytes());
            image[at + 8..at + 12].copy_from_slice(&entry.size.to_le_bytes());
        }
        if self.loaded && self.image.get(4..) == Some(&image[4..]) {
            return Ok(());
        }
        match nvram.store_record(Record::AppDataJournalInfo, &image) {
            RecordStatus::Ok => {
                self.image = image;
                self.loaded = true;
                Ok(())
            }
            status => Err(status),
        }
    }
}

/// What the per-frame setup reads from the rest of libam (`0x140249430`).
#[derive(Debug, Clone, Copy)]
pub struct SetupInputs {
    /// Sequence set up, and the dongle and platform done.
    pub ready: bool,
    /// The sequence's first-launch flag.
    pub first_launch: bool,
    /// The keychip region (raw 1, 2 or 3).
    pub region: Option<u32>,
    /// The configured game id (the keychip's).
    pub game_id: Option<GameId>,
}

/// `am::nvram::AppInfo` manager (`storage.md` 3.7).
#[derive(Debug, Default)]
pub struct AppInfoManager {
    life: Lifecycle,
    clear: NvramConfig,
    develop: bool,
    file_size: u64,
    region_code: u8,
    word: u32,
    entries: Vec<Entry>,
    buffers: Vec<Vec<u8>>,
    journal: Journal,
}

/// The game-id word (`0x1402497C0`): the four code-page-932 bytes big-endian, plus `(region code - 1) << 24`.
fn game_id_word(id: &GameId, region_code: u8) -> Option<u32> {
    let text = id.to_string();
    let (bytes, _, failed) = encoding_rs::SHIFT_JIS.encode(&text);
    if failed || bytes.len() != 4 {
        return None;
    }
    let packed = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    Some(packed.wrapping_add(u32::from(region_code.wrapping_sub(1)) << 24))
}

/// Raw keychip region to the `AppInfo` region code (`0x140284F60`).
fn region_code(raw: Option<u32>) -> u8 {
    match raw {
        Some(1) => 1,
        Some(2) => 8,
        Some(3) => 4,
        _ => 0,
    }
}

impl AppInfoManager {
    /// The constructed manager.
    pub fn new() -> Self {
        Self::default()
    }

    /// `AppInfo::configure` (`0x1402492C0`), from `OperatingState::enter`: stores the clear-data config and the
    /// platform develop byte; state 1.
    pub fn initialize(&mut self, config: NvramConfig, develop: bool) {
        self.clear = config;
        self.develop = develop;
        self.life.begin();
    }

    /// `0x140249430(ready, ...)`, once per Operating frame (`storage.md` 3.7.2): one setup state per call.
    pub fn update(&mut self, cx: &Cx<'_>, inputs: SetupInputs) {
        let nvram = cx.nvram;
        match self.life.state.0 {
            1 => self.setup_handles(nvram, inputs.game_id),
            2 => self.setup_journal(nvram),
            3 if inputs.ready => self.setup_game_id(inputs),
            4 => self.clear_app_data(nvram, inputs.first_launch),
            5 => self.replay(nvram),
            _ => return,
        }
        if self.latched() && self.life.state.0 != 0 {
            self.life.state = StateWord(APP_INFO_LOADED);
        }
    }

    /// State 1: the handles must exist (system file open, application file open for a valid game id).
    fn setup_handles(&mut self, nvram: &NvramContext, game_id: Option<GameId>) {
        if !nvram.is_open() || !nvram.app_file_open() || game_id.is_none() {
            self.life.error.set(LATCH_CONTEXT, 0);
        }
        self.file_size = FILE_SIZE;
        self.life.state = StateWord(2);
    }

    /// State 2: load the journal record (default written on a CRC failure).
    fn setup_journal(&mut self, nvram: &NvramContext) {
        if !self.journal.loaded {
            self.load_journal(nvram);
        }
        self.life.state = StateWord(3);
    }

    fn load_journal(&mut self, nvram: &NvramContext) {
        match nvram.load_record(Record::AppDataJournalInfo, || Record::AppDataJournalInfo.blank_image()) {
            Ok(image) => {
                self.journal.image = image;
                self.journal.loaded = true;
            }
            Err(status) => self.latch_status(status),
        }
    }

    /// State 3: region code and game-id word.
    fn setup_game_id(&mut self, inputs: SetupInputs) {
        let code = region_code(inputs.region);
        match inputs.game_id.as_ref().and_then(|id| game_id_word(id, code)) {
            Some(word) => {
                self.region_code = code;
                self.word = word;
                self.life.state = StateWord(4);
            }
            None => {
                self.life.error.set(LATCH_CONTEXT, 0);
                self.life.state = StateWord(APP_INFO_LOADED);
            }
        }
    }

    /// State 4, `clearAppData` (`0x140249FE0`): a clear-at-first-launch configuration is rejected; on the first
    /// launch the journal is cleared. No slot is erased.
    fn clear_app_data(&mut self, nvram: &NvramContext, first_launch: bool) {
        if self.clear.clear_eeprom_at_first_launch || self.clear.clear_busram_at_first_launch {
            self.life.error.set(LATCH_BAD_PARAMETER, 0);
        } else if first_launch && let Err(status) = self.journal.set(nvram, false, false, &[]) {
            self.latch_status(status);
        }
        self.life.state = StateWord(5);
    }

    /// State 5: replay of an unfinished save (`storage.md` 3.7.7).
    fn replay(&mut self, nvram: &NvramContext) {
        if self.journal.active() {
            let mirror = self.journal.mirror();
            self.entries.clear();
            self.buffers.clear();
            for entry in self.journal.entries() {
                self.add_record(true, 2, entry.offset, entry.size, vec![0; entry.size as usize]);
            }
            // The copy that was not being written holds the data.
            let loaded = self.load_copy(nvram, !mirror);
            if loaded != 1 {
                let rewritten = self.pass(nvram, mirror, false);
                if rewritten != 1 {
                    let entries = self.journal.entries();
                    self.clear_list();
                    if let Err(status) = self.journal.set(nvram, false, false, &entries) {
                        self.latch_status(status);
                    }
                }
            }
            self.clear_list();
        }
        self.life.state = StateWord(APP_INFO_LOADED);
    }

    /// `0x1402493D0`: back to state 0.
    pub fn terminate(&mut self) {
        *self = Self::default();
    }

    /// The module reset sweep (`0x1402544A0` / `0x1402554E0`, `storage.md` 3.7.9): resets when `force`, when the
    /// sequence is not set up, or when the latched error's class is 2 or 4 (`latch_class`).
    pub fn restart(&mut self, force: bool, sequence_set_up: bool, latch_class: Option<u32>) {
        let class_resets = self.latched() && matches!(latch_class, Some(2 | 4));
        if force || !sequence_set_up || class_resets {
            self.reset();
        }
    }

    /// `0x140249450` (state 6 only): latch, list, descriptors, region and id word cleared; state 1.
    fn reset(&mut self) {
        if self.life.state.0 != APP_INFO_LOADED {
            return;
        }
        self.clear_list();
        self.file_size = 0;
        self.region_code = 0;
        self.word = 0;
        self.journal.loaded = false;
        self.life.begin();
    }

    /// `AppInfo::setGameId` (`0x1402497C0`): recomputes the id word with the current region code; state 6 only.
    pub fn set_game_id(&mut self, id: &GameId) -> bool {
        if self.life.state.0 != APP_INFO_LOADED {
            return false;
        }
        match game_id_word(id, self.region_code) {
            Some(word) => {
                self.word = word;
                true
            }
            None => false,
        }
    }

    /// `AppInfo` create-context (`0x1402494E0`) after a game-id switch: the list is dropped and the journal of
    /// the new application file is loaded (a failure latches).
    pub fn create_context(&mut self, nvram: &NvramContext) {
        if self.life.state.0 != APP_INFO_LOADED {
            return;
        }
        self.clear_list();
        self.load_journal(nvram);
    }

    /// `allReady` term: state 6.
    pub fn is_ready(&self) -> bool {
        self.life.state.0 == APP_INFO_LOADED
    }

    /// The module's error cell, scanned by the aggregator (`+129032`).
    pub fn error_cell(&self) -> ErrorCell {
        self.life.error
    }

    /// The platform develop byte given at [`Self::initialize`]: selects error 87 over 84.
    pub fn develop(&self) -> bool {
        self.develop
    }

    /// Size of the File device descriptor (zero until the setup stage that creates it ran).
    pub fn file_size(&self) -> u64 {
        self.file_size
    }

    /// Alignment of the File device descriptor.
    pub fn file_alignment(&self) -> u32 {
        if self.file_size == 0 { 0 } else { FILE_ALIGNMENT }
    }

    fn latched(&self) -> bool {
        !self.life.error.is_empty()
    }

    fn latch_status(&mut self, status: RecordStatus) {
        if let Some((number, sub)) = NvramContext::status_error(status, self.develop) {
            self.life.error.set(number, sub);
        }
    }

    /// Latch 84, or 87 with the develop byte.
    fn latch_io(&mut self) {
        self.latch_status(RecordStatus::ReadFailed);
    }

    /// Empties the pending list (`0x1402494A0`).
    pub fn clear_list(&mut self) {
        self.entries.clear();
        self.buffers.clear();
    }

    /// `addRecord` (`0x14024A600`): 0 appended, 1 rejected (device 0 and 1 also latch 4104).
    pub fn add_record(&mut self, bypass: bool, device: u32, offset: u32, size: u32, data: Vec<u8>) -> u32 {
        if (!bypass && self.life.state.0 != APP_INFO_LOADED) || self.word == 0 || self.entries.len() >= MAX_ENTRIES {
            return 1;
        }
        match device {
            0 | 1 => {
                self.life.error.set(LATCH_BAD_PARAMETER, 0);
                1
            }
            2 if u64::from(offset) + u64::from(size) <= self.file_size => {
                self.entries.push(Entry { offset, size });
                self.buffers.push(data);
                0
            }
            _ => 1,
        }
    }

    /// `AppInfo::load()` (`0x140249940`): status 0 ok, 1 failure, 2 CRC error, 3 other game id. Returns the
    /// buffers of the listed entries (the list is cleared).
    pub fn load(&mut self, nvram: &NvramContext) -> (u32, Vec<Vec<u8>>) {
        if self.life.state.0 != APP_INFO_LOADED || self.latched() {
            return (1, Vec::new());
        }
        if self.entries.is_empty() {
            return (0, Vec::new());
        }
        let mut status = self.load_copy(nvram, false);
        if status == 2 {
            status = self.load_copy(nvram, true);
            if status == 0 {
                status = self.pass(nvram, false, false);
            }
        }
        self.entries.clear();
        (status, std::mem::take(&mut self.buffers))
    }

    /// Per-copy load (`0x14024A940`).
    fn load_copy(&mut self, nvram: &NvramContext, mirror: bool) -> u32 {
        let (status, io_failed) = self.scan_copy(nvram, mirror);
        if io_failed {
            self.latch_io();
        }
        status
    }

    /// Reads every listed slot of one copy; the flag reports a device error (which latches).
    fn scan_copy(&mut self, nvram: &NvramContext, mirror: bool) -> (u32, bool) {
        let delta = if mirror { MIRROR_DELTA } else { 0 };
        let mut some_ok = false;
        let mut mismatch: Option<u32> = None;
        for (entry, buffer) in self.entries.iter().zip(self.buffers.iter_mut()) {
            let address = AREA_BASE + entry.offset + delta;
            match nvram.read_one(Device::Application, address, buffer, Some(self.word)) {
                0 => {
                    if mismatch.is_some() {
                        return (2, false);
                    }
                    some_ok = true;
                }
                -23 => return (2, false),
                -24 => {
                    if some_ok {
                        return (2, false);
                    }
                    let stored = u32::from_be_bytes([buffer[4], buffer[5], buffer[6], buffer[7]]);
                    match mismatch {
                        None => mismatch = Some(stored),
                        Some(first) if first != stored => return (2, false),
                        Some(_) => {}
                    }
                }
                _ => return (1, true),
            }
        }
        (if some_ok { 0 } else { 3 }, false)
    }

    /// `AppInfo::save()` (`0x140249AF0`): 0 ok, 1 failure. The mirror copy is written first.
    pub fn save(&mut self, nvram: &NvramContext) -> u32 {
        if self.life.state.0 != APP_INFO_LOADED || self.latched() {
            return 1;
        }
        if self.entries.is_empty() {
            return 0;
        }
        let mut result = self.pass(nvram, true, true);
        if result == 0 {
            result = self.pass(nvram, false, true);
        }
        if result == 0
            && let Err(status) = self.journal.set(nvram, false, false, &self.entries)
        {
            self.latch_status(status);
            return 1;
        }
        self.clear_list();
        result
    }

    /// One write pass over the list (`0x14024AD90`): journal "in flight" first when `journal`, then every slot
    /// with CRC and game-id word stamped.
    fn pass(&mut self, nvram: &NvramContext, mirror: bool, journal: bool) -> u32 {
        if journal && let Err(status) = self.journal.set(nvram, true, mirror, &self.entries) {
            self.latch_status(status);
            return 1;
        }
        let delta = if mirror { MIRROR_DELTA } else { 0 };
        let word = self.word;
        let failed = self.entries.iter().zip(self.buffers.iter_mut()).any(|(entry, buffer)| {
            let address = AREA_BASE + entry.offset + delta;
            nvram.write_one(Device::Application, address, buffer, Some(word)) != 0
        });
        if failed {
            self.latch_io();
            return 1;
        }
        0
    }
}
