//! The Aime module body (`spec/libam/aime.md` A.1-A.10): configuration into units, the start-up pass, the frame
//! update, the play log and the facade methods the port and the other modules call.

use std::rc::Rc;
use std::sync::Arc;

use libam_api::aime::{
    AccessCode, AimeEvent, CampaignInfoList, CampaignProgressList, CardInfo, LedPattern, LibamAimeId, ReaderInfo,
    RelatedSegaId,
};
use libam_api::backup::GameId;
use libam_api::result::{AmError, AmResult};
use libam_api::sequence::AimeLogStatus;
use libam_api::time::Tm;

use super::Aime;
use super::device::{ReaderConfig, UnitCommands};
use super::facade_error::record_error;
use super::facade_log::{CAPACITY, LogQueue};
use super::facade_unit::{Tick, Unit};
use super::lib::{AimeLib, BoardInfo, CampaignError, LibConfig, NetworkClient, PlayLogRecord, PlayLogStore};
use crate::clock::Clock;
use crate::module::{Cx, ErrorCell, StateWord};
use crate::nvram::{NvramContext, Record, RecordStatus};
use backend::aime_db::AimeDbTransport;
use backend::{Backend, SerialPorts};

/// The ErrorCode-less fatal numbers the module itself raises.
const FATAL_NOT_READY: u16 = 4105;
/// The fatal number of an unreachable Aime DB (`0x140257910`).
const FATAL_DB_DEAD: u16 = 6508;

/// A `Tick` over the fields of `$aime` (the caller keeps `$aime.units` free for the iteration).
macro_rules! tick {
    ($aime:expr, $backend:expr, $clock:expr, $now:expr) => {{
        let backend: &Backend = $backend;
        Tick {
            now_ms: $now,
            permit: $aime.permit && !$aime.suspended,
            db_alive: $aime.net.as_ref().is_some_and(NetworkClient::is_alive),
            develop: $aime.develop,
            rescan_ok: $aime.phase == 3,
            tm: crate::util::tm_from_date_time($clock.local_now()),
            paths: &*backend.fs,
            firmware: $aime.config.firmware_paths.as_slice().first().map(|path| path.as_str()),
            events: &mut $aime.events,
        }
    }};
}

/// `time_t` of a local `struct tm` (seconds since 1970 of the same civil time read as UTC).
fn time_t(tm: &Tm) -> i64 {
    let year = i64::from(tm.year) + 1900;
    let month = i64::from(tm.mon) + 1;
    let shifted = if month <= 2 { year - 1 } else { year };
    let era = shifted.div_euclid(400);
    let year_of_era = shifted - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + i64::from(tm.mday) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    days * 86_400 + i64::from(tm.hour) * 3600 + i64::from(tm.min) * 60 + i64::from(tm.sec)
}

impl Aime {
    /// The constructed manager.
    pub fn new() -> Self {
        // The reader unit (`+129`, `0x1402588E0`) and the network client (`+104`, `0x140259F40`) are constructed
        // permitted; only `0x14022A530` ever clears the byte.
        Self { permit: true, ..Self::default() }
    }

    /// The module is usable by the facade (A.2): `enable` and the state in 2, 3 or 4.
    fn usable(&self) -> bool {
        self.config.enable && (2..=4).contains(&self.phase)
    }

    fn unit(&self, index: usize) -> Option<&Unit> {
        if self.usable() { self.units.get(index) } else { None }
    }

    fn unit_mut(&mut self, index: usize) -> Option<&mut Unit> {
        if self.usable() { self.units.get_mut(index) } else { None }
    }

    /// The error cell takes the first error only while it is empty (`+2376`).
    fn raise(&mut self, number: u16, sub: u16) {
        if self.life.error.is_empty() {
            self.life.error.set(number, sub);
        }
    }

    fn fault(&mut self, number: u16, sub: u16) {
        self.raise(number, sub);
        self.phase = 4;
    }

    /// Mirrors the module state (A.7) in the state word the aggregator reads.
    fn sync_state(&mut self) {
        self.life.state = match self.phase {
            0 => StateWord::UNINITIALIZED,
            1 => StateWord::INITIALIZED,
            4 => StateWord::FAILED,
            _ => StateWord::READY,
        };
    }

    /// The reader id of a configured unit (`0x140257EB0`): `port + 1000 * unit number`.
    fn reader_id(com_port: u64, unit_index: u64) -> u32 {
        (com_port + 1000 * unit_index) as u32
    }

    /// `OperatingState::enter` (`0x140228B70`): takes `AppConfig.aime` and the platform record flag
    /// (`0x140256C90`, error 87 instead of 84), creates the readers and the network client.
    pub fn initialize(&mut self, cx: &mut Cx<'_>, develop: bool) {
        let now = cx.clock.tick_ms();
        self.shutdown(now);
        self.config = cx.config.aime.clone();
        self.develop = develop;
        self.life.begin();
        self.events.clear();
        self.user_info.clear();
        self.log = LogQueue::default();
        self.log_loaded = false;
        self.pending = false;
        self.stopped = false;
        self.board_set = false;
        self.game_id.clear();
        self.serial.clear();
        self.terminate = false;
        self.step = 0;
        self.index = 0;
        self.now_ms = now;
        self.phase = if self.config.enable { 1 } else { 3 };
        if self.config.enable {
            self.create_units(&cx.backend.serial, &cx.backend.aime_db, now);
        }
        self.sync_state();
    }

    /// The units, the reader manager and the network client (`0x140255970`).
    fn create_units(&mut self, serial: &Rc<dyn SerialPorts>, db: &Arc<dyn AimeDbTransport>, now: u64) {
        let ports = super::device::ReaderPorts::new(Rc::clone(serial), self.config.high_baudrate);
        let ids: Vec<u32> =
            self.config.units.iter().map(|unit| Self::reader_id(unit.com_port, unit.unit_index)).collect();
        ports.open(&ids);
        let lib_config = LibConfig {
            scan_timeout_sec: self.config.card_accept_time,
            polling_interval: self.config.polling_interval,
        };
        let reader_config = ReaderConfig { repoll_wait_ms: self.config.polling_interval, ..ReaderConfig::default() };
        for (index, (&id, unit)) in ids.iter().zip(self.config.units.iter()).enumerate() {
            let reader = ports.bind(id, reader_config.clone());
            let lib = AimeLib::new(id, lib_config, &self.manager, db, reader);
            self.units.push(Unit::new(index, unit.flag, lib, now, &mut self.events));
        }
        self.net = Some(NetworkClient::new(lib_config, &self.manager, db, now));
        self.ports = Some(ports);
        self.apply_board();
    }

    /// Closes the readers and drops every object (`0x140228E60`).
    fn shutdown(&mut self, now: u64) {
        for unit in &mut self.units {
            unit.abandon(now);
            unit.lib_mut().terminate(now);
        }
        self.units.clear();
        self.net = None;
        self.db_dead = false;
        if let Some(ports) = self.ports.take() {
            ports.close();
        }
        self.manager = super::lib::LibManager::default();
    }

    /// `0x1402293D0`: the game id (4 characters) and the keychip serial (11 characters, the dashes of the standard
    /// form dropped) every DB request carries.
    pub(crate) fn set_board(&mut self, game_id: GameId, serial: &str) {
        self.game_id = game_id.to_string();
        self.serial = serial.chars().filter(|&c| c != '-').take(11).collect();
        self.board_set = true;
        self.apply_board();
    }

    /// Re-keys the network client and every unit with the current board info.
    fn apply_board(&mut self) {
        let board = BoardInfo { game_id: self.game_id.clone(), serial: self.serial.clone(), arg: self.region };
        for unit in &mut self.units {
            unit.lib_mut().set_board_info(&board);
        }
        if let Some(net) = &mut self.net {
            net.set_board(&board);
        }
    }

    /// `0x140229520(module, location id)`: while the module is up and its client is idle, a changed ALL.Net
    /// location id (0 when the authentication has none) re-keys the network client and the units.
    fn set_region(&mut self, region: u32) {
        let idle = self.net.as_ref().is_some_and(|net| !net.play_log_running());
        if !idle || !self.usable() {
            return;
        }
        if region != self.region {
            self.region = region;
            self.apply_board();
        }
    }

    /// `0x140228FD0` (+ `0x140229520`), once per Operating frame. `auth_good` is the ALL.Net authentication result
    /// (`allnet+1208 == 0`), `location_id` the u32 of the authentication's location answer (0 when absent).
    pub fn update(&mut self, cx: &mut Cx<'_>, auth_good: bool, location_id: u32) {
        if self.life.state == StateWord::UNINITIALIZED {
            return;
        }
        let now = cx.clock.tick_ms();
        self.now_ms = now;
        self.set_region(location_id);
        if !self.config.enable {
            self.sync_state();
            return;
        }
        if matches!(self.phase, 1 | 2) {
            self.run_step(cx, now);
        }
        if self.log_loaded && self.log_generation != cx.nvram.app_generation() {
            self.load_log(cx.nvram);
        }
        if let Some(ports) = &self.ports {
            ports.tick(now);
        }
        self.update_network(now, auth_good && self.region != 0);
        self.update_units(&*cx.backend, cx.clock, now);
        self.persist_log(cx.nvram);
        self.sync_state();
    }

    /// The step of the start-up pass (`0x14022A8D0`, A.7).
    fn run_step(&mut self, cx: &mut Cx<'_>, now: u64) {
        match self.step {
            // Step 0: wait until `set_board` stored the strings; stored but empty is a fault.
            0 => {
                if !self.board_set {
                    return;
                }
                if self.game_id.is_empty() || self.serial.is_empty() {
                    self.fault(FATAL_NOT_READY, 0);
                    return;
                }
                self.step = 1;
            }
            // Step 1: the play-log record (`0x14025AD90`), then the module is "running" (state 2).
            1 => {
                if let Err(status) = self.load_log_record(cx.nvram) {
                    let (number, sub) = record_error(status, self.develop).unwrap_or((FATAL_NOT_READY, 0));
                    self.fault(number, sub);
                    return;
                }
                self.phase = 2;
                self.step = 2;
            }
            // Step 2: wait until no unit owns a sequencer; no stored firmware record is resumed.
            2 => {
                if self.units.iter().all(|unit| !unit.busy()) {
                    self.step = 4;
                    self.index = 0;
                }
            }
            // Step 4: the initial detect of every unit in order; state 3 after the last one.
            _ => {
                while self.index < self.units.len() {
                    if self.units[self.index].busy() || self.units[self.index].reporting() {
                        return;
                    }
                    if self.units[self.index].phase == 2 {
                        self.index += 1;
                        continue;
                    }
                    let kind = if self.config.has_firmware_path() && !self.terminate { 7 } else { 1 };
                    let mut tick = tick!(self, &*cx.backend, cx.clock, now);
                    let _ = self.units[self.index].start(kind, &mut tick);
                    return;
                }
                self.phase = 3;
            }
        }
    }

    /// The network client's frame (`0x140257D50` then the scheduler `0x14025A3A0`).
    fn update_network(&mut self, now: u64, ready: bool) {
        let Some(net) = &mut self.net else { return };
        net.update(ready, &mut self.log, now);
        self.events.extend(net.take_events());
        let changes = net.take_alive_changes();
        for alive in changes {
            self.db_dead = !alive;
            if alive {
                if self.life.error.get().is_some_and(|(number, _)| number == FATAL_DB_DEAD) {
                    self.life.error.clear();
                }
            } else {
                self.raise(FATAL_DB_DEAD, 0);
            }
        }
    }

    fn update_units(&mut self, backend: &Backend, clock: &dyn Clock, now: u64) {
        let mut tick = tick!(self, backend, clock, now);
        let mut fatals = Vec::new();
        for unit in &mut self.units {
            if let Some(fatal) = unit.step(&mut tick) {
                fatals.push(fatal);
            }
            unit.sync_slot();
            if let Some((aime_id, card_id)) = unit.take_card_entry() {
                match self.user_info.iter_mut().find(|(id, _)| *id == aime_id) {
                    Some(entry) => entry.1 = card_id,
                    None => self.user_info.push((aime_id, card_id)),
                }
            }
        }
        for (number, sub) in fatals {
            self.raise(number, sub);
        }
    }

    fn load_log_record(&mut self, nvram: &NvramContext) -> Result<(), RecordStatus> {
        let image = nvram.load_record(Record::AimePlayLogInfo, || Record::AimePlayLogInfo.blank_image())?;
        self.log = LogQueue::from_image(&image);
        self.log_loaded = true;
        self.log_generation = nvram.app_generation();
        Ok(())
    }

    /// The application file was reopened (a game-id switch): the queue of the new game id is loaded.
    fn load_log(&mut self, nvram: &NvramContext) {
        if let Err(status) = self.load_log_record(nvram) {
            let (number, sub) = record_error(status, self.develop).unwrap_or((FATAL_NOT_READY, 0));
            self.fault(number, sub);
        }
    }

    fn persist_log(&mut self, nvram: &NvramContext) {
        if !self.log_loaded || !self.log.take_dirty() {
            return;
        }
        let mut image = Record::AimePlayLogInfo.blank_image();
        self.log.write_image(&mut image);
        let status = nvram.store_record(Record::AimePlayLogInfo, &image);
        if let Some((number, sub)) = record_error(status, self.develop) {
            self.raise(number, sub);
        }
    }

    /// `0x140228E60`.
    pub fn terminate(&mut self, cx: &mut Cx<'_>) {
        let now = cx.clock.tick_ms();
        self.persist_log(cx.nvram);
        self.shutdown(now);
        self.phase = 0;
        self.life.end();
    }

    /// Module restart (A.10, `0x140229200(module, flag)`): `flag` is stored as the terminate flag and the readers
    /// are permitted again (`0x14022A530(module, 1)`, the next idle/busy refresh sets the real value). In the
    /// states 2 to 4 the sequences are cancelled, the fatal error and the campaign data forgotten, the start-up
    /// pass runs again.
    pub fn restart(&mut self, terminate: bool) {
        self.terminate = terminate;
        if self.config.enable && self.net.is_some() {
            self.set_permit(true, false);
        }
        if !(2..=4).contains(&self.phase) {
            return;
        }
        let now = self.now_ms;
        for unit in &mut self.units {
            unit.abandon(now);
        }
        self.db_dead = false;
        if let Some(net) = &mut self.net {
            net.reset();
        }
        self.life.begin();
        self.phase = if self.config.enable { 1 } else { 3 };
        self.step = 0;
        self.index = 0;
        self.sync_state();
    }

    /// The network test's read of the module (`0x1402968C0`, network.md T.8.7): `None` while the gate is closed
    /// (the module state is 2 to 4 and the network client finished its resolve failure or the campaign query),
    /// else the error record of the client's event listener (`AIME+46044`): the DB-dead fatal 6508 or empty.
    pub fn network_test(&self) -> Option<ErrorCell> {
        let gate = (2..=4).contains(&self.phase) && self.net.as_ref().is_some_and(NetworkClient::is_done);
        gate.then(|| {
            let mut record = ErrorCell::EMPTY;
            if self.db_dead {
                record.set(FATAL_DB_DEAD, 0);
            }
            record
        })
    }

    /// `0x14022A530(permit, suspended)`: `suspended` wins.
    pub fn set_permit(&mut self, permit: bool, suspended: bool) {
        self.permit = permit && !suspended;
        self.suspended = suspended;
    }

    /// `aime.log(status, id, tm, credit)` (`0x14022A0D0`).
    pub fn log(&mut self, status: AimeLogStatus, aime_id: u32, time: &Tm, credit: u32) -> AmResult {
        self.enqueue_log(status.libam_code(), aime_id, time, credit)
    }

    /// The checks and the append of `send_play_log` (A.3).
    pub(super) fn enqueue_log(&mut self, status: u32, aime_id: u32, time: &Tm, credit: u32) -> AmResult {
        if !self.usable() || !self.log_loaded {
            return Err(AmError::code(AmError::NOT_INITIALIZED));
        }
        if aime_id == u32::MAX || !(1..=4).contains(&status) {
            return Err(AmError::code(AmError::INVALID_ARGUMENT));
        }
        if self.log.len() >= CAPACITY {
            return Err(AmError::code(AmError::FAILED));
        }
        let mut serial = [0u8; 11];
        for (slot, byte) in serial.iter_mut().zip(self.serial.bytes()) {
            *slot = byte;
        }
        self.log.push(PlayLogRecord {
            status: status as u8,
            region: self.region as u16,
            aime_id,
            credit,
            time: time_t(time),
            store_byte: 0,
            serial,
        });
        if let Some(net) = &mut self.net {
            net.wake_play_log();
        }
        Ok(())
    }

    /// `0x14022A5E0`: clears the Aime play-log record (written by the next frame).
    pub fn clear_play_log(&mut self) {
        self.log.clear();
    }

    /// The FeliCa card id of an Aime id (the Aime user-info list; `None` when the card was not a FeliCa card).
    pub fn card_id(&self, aime_id: u32) -> Option<[u8; 8]> {
        self.user_info.iter().find(|(id, _)| *id == aime_id).map(|&(_, card_id)| card_id)
    }

    // -----------------------------------------------------------------------------------------------------
    // APM hand-over (storage.md 5.4 steps 5, 7, 8, 11)
    // -----------------------------------------------------------------------------------------------------

    /// Step 5: every unit's running sequence is cancelled (the result is ignored).
    pub(crate) fn apm_prepare(&mut self) {
        if self.usable() {
            for unit in &mut self.units {
                unit.cancel();
            }
        }
    }

    /// Step 7, `0x14022A670`: 2 when already pending; otherwise 0 after the stop of the network work was
    /// requested, 2 module not running, 6 a unit still busy, 4 no unit.
    pub(crate) fn apm_begin_change(&mut self) -> u32 {
        if self.pending {
            return 2;
        }
        self.pending = true;
        if !self.usable() {
            return 2;
        }
        if self.units.is_empty() {
            return 4;
        }
        if self.units.iter().any(Unit::busy) {
            return 6;
        }
        if let Some(net) = &mut self.net {
            net.set_periodic(false);
        }
        self.stopped = true;
        0
    }

    /// Step 8, `0x140229390`: every unit is idle and the network work has stopped.
    pub(crate) fn apm_stopped(&self) -> bool {
        self.units.iter().all(|unit| !unit.busy()) && self.net.as_ref().is_none_or(|net| !net.play_log_running())
    }

    /// Step 11, `0x1402293D0`: the new game id and serial, the pending flag cleared, the network work resumed.
    pub(crate) fn apm_finish_change(&mut self, game_id: GameId, serial: &str) {
        self.set_board(game_id, serial);
        self.pending = false;
        if self.stopped {
            self.stopped = false;
            if let Some(net) = &mut self.net {
                net.set_periodic(true);
            }
        }
    }

    // -----------------------------------------------------------------------------------------------------
    // Port methods (A.3)
    // -----------------------------------------------------------------------------------------------------

    pub(super) fn unit_count(&self) -> u64 {
        if self.usable() { self.units.len() as u64 } else { 0 }
    }

    pub(super) fn is_db_alive(&self) -> bool {
        self.net.as_ref().is_some_and(NetworkClient::is_alive)
    }

    /// `start_scan_offline` (kind 2) and `start_scan` (kind 5): 1 no unit, 3 busy or the start-up pass runs.
    pub(super) fn start_scan(&mut self, unit: usize, kind: u32, backend: &Backend, clock: &dyn Clock) -> AmResult {
        let now = clock.tick_ms();
        let starting = self.phase == 2;
        if self.unit(unit).is_none() {
            return Err(AmError::code(AmError::INVALID_ARGUMENT));
        }
        if starting {
            return Err(AmError::code(AmError::BUSY));
        }
        let mut tick = tick!(self, backend, clock, now);
        self.units[unit].start(kind, &mut tick)
    }

    pub(super) fn confirm(&mut self, unit: usize) -> AmResult {
        self.unit_mut(unit).ok_or(AmError::code(AmError::INVALID_ARGUMENT))?.confirm();
        Ok(())
    }

    pub(super) fn cancel(&mut self, unit: usize) -> AmResult {
        let starting = self.phase == 2;
        let unit = self.unit_mut(unit).ok_or(AmError::code(AmError::INVALID_ARGUMENT))?;
        if !starting {
            unit.cancel();
        }
        Ok(())
    }

    pub(super) fn set_led(&mut self, unit: usize, rgb: [u8; 3]) -> AmResult {
        let unit = self.unit_mut(unit).ok_or(AmError::code(AmError::INVALID_ARGUMENT))?;
        if !unit.available() {
            return Err(AmError::code(AmError::BUSY));
        }
        unit.set_led(rgb);
        Ok(())
    }

    pub(super) fn set_led_pattern(&mut self, unit: usize, pattern: LedPattern) -> AmResult {
        if !self.usable() {
            return Err(AmError::code(AmError::INVALID_ARGUMENT));
        }
        let unit = self.units.get_mut(unit).ok_or(AmError::code(AmError::INVALID_ARGUMENT))?;
        if !unit.available() {
            return Err(AmError::code(AmError::BUSY));
        }
        unit.set_pattern(pattern as u32, &mut self.events);
        Ok(())
    }

    /// The raw unit-command access of unit `index` for the e-money store-card reader (`spec/libam/payment.md`
    /// PayEmSvc 6.4), sharing the unit's owner slot; `None` while the module is unusable or the unit is not bound
    /// to an open port.
    pub fn store_reader(&self, index: usize) -> Option<UnitCommands> {
        let unit = self.unit(index)?;
        let config = self.config.units.as_slice().get(index)?;
        let id = Self::reader_id(config.com_port, config.unit_index);
        self.ports.as_ref()?.unit_commands(id, unit.slot())
    }

    pub(super) fn reader_info(&self, unit: usize) -> Option<ReaderInfo> {
        self.unit(unit).map(Unit::reader_info)
    }

    pub(super) fn card_info(&self, unit: usize) -> Option<CardInfo> {
        self.unit(unit).and_then(Unit::card_info)
    }

    pub(super) fn access_code(&self, unit: usize) -> Option<AccessCode> {
        self.unit(unit).and_then(Unit::access_code)
    }

    pub(super) fn aime_id(&self, unit: usize) -> Option<LibamAimeId> {
        self.unit(unit).and_then(Unit::aime_id)
    }

    pub(super) fn related_sega_id(&self, unit: usize) -> Option<RelatedSegaId> {
        self.unit(unit).and_then(Unit::related_sega_id)
    }

    /// `query_campaign_progress` (A.3): 2 module unusable, 6 no network client, 1 bad id, 3 a query runs.
    pub(super) fn query_campaign_progress(&mut self, aime_id: LibamAimeId, now_ms: u64) -> AmResult {
        if !self.usable() {
            return Err(AmError::code(AmError::NOT_INITIALIZED));
        }
        let net = self.net.as_mut().ok_or(AmError::code(AmError::FAILED))?;
        net.query_campaign_progress(aime_id, now_ms).map_err(|error| match error {
            CampaignError::BadId => AmError::code(AmError::INVALID_ARGUMENT),
            CampaignError::Busy => AmError::code(AmError::BUSY),
        })
    }

    pub(super) fn campaign_info_list(&self) -> Option<CampaignInfoList> {
        self.usable().then(|| self.net.as_ref().map(NetworkClient::campaign_info)).flatten()
    }

    pub(super) fn campaign_progress_list(&self) -> Option<CampaignProgressList> {
        self.usable().then(|| self.net.as_ref().map(NetworkClient::campaign_progress)).flatten()
    }

    pub(super) fn take_events(&mut self) -> Vec<AimeEvent> {
        std::mem::take(&mut self.events)
    }
}
