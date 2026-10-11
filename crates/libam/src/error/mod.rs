//! The error manager (`am::error`, `spec/libam/error.md` part II) and the `ErrorPort` handle.
//!
//! The manager owns the language and resource sets ([`resource`]), the current and pending error, the two
//! history rings and the occurrence table in memory, and the two persisted records behind them ([`log`]): the
//! init stage machine (13), `set` / commit / reset (14), the rings (17), persistence (18), the `OccurFatalError`
//! event (19) and the master hand-off of the current error.
//!
//! The raise path with its side effects on the sequence and the other modules (`0x140253370`) is
//! [`crate::core::Core::raise_error`]; this type only holds the manager's own part (`0x140233090`).

mod log;
mod resource;

use libam_api::am_config::ErrorConfig;
use libam_api::backup::GameId;
use libam_api::error::{ErrorOccurrence, ErrorPort, FatalError, HistoryRing};
use libam_api::interface::LibamEvent;
use libam_api::result::{AmError, AmResult};
use libam_api::time::Tm;

use crate::master::Master;
use crate::module::{Cx, ErrorCell, Lifecycle, StateWord};
use crate::nvram::{NvramContext, RecordStatus};
use crate::shared::Shared;
use crate::util::{tm_from_date_time, wide_to_string};
use log::{AppLog, ErrorLog, from_filetime, to_filetime};
use resource::{Filters, ResourceSet};

/// Capacity of each history ring.
const RING_CAPACITY: usize = 15;
/// Rows of the occurrence table.
const TABLE_CAPACITY: usize = 222;
/// Reset type of the compiled-in records: shutdown.
const RESET_SHUTDOWN: u32 = 1;
/// The error raised when the resource set cannot be initialised or the configuration is invalid.
const ERROR_BAD_CONFIG: u16 = 4104;
/// The error raised when the master handle or the record context is missing.
const ERROR_UNEXPECTED: u16 = 4105;
/// The unknown-error substitute (`error.md` 14.2).
const ERROR_UNKNOWN: u16 = 4103;
/// The text of the unknown-error substitute when even the resources lack it.
const UNKNOWN_TEXT: &str = "Unexpected Error Occurred";
/// The game id stamped when the keychip has none (`0x1406FC858`).
const NO_GAME_ID: [u8; 4] = *b"----";

/// The seven compiled-in fallback records (error.md 7.1), looked up by number only.
const BUILTIN: [(u16, &str); 7] = [
    (23, "Main board Malfunctioning"),
    (84, "Storage Device Malfunctioning"),
    (87, "Storage Device Malfunctioning"),
    (949, "Keychip Not Found"),
    (4103, "Unexpected Error Occurred"),
    (4104, "Unexpected Error Occurred"),
    (4105, "Unexpected Error Occurred"),
];

/// The compiled-in record of `number`: sub number 0, reset type shutdown.
fn builtin(number: u16) -> Option<FatalError> {
    BUILTIN
        .iter()
        .find(|(n, _)| *n == number)
        .map(|&(number, text)| FatalError::new(number, 0, Tm::default(), text, RESET_SHUTDOWN))
}

/// `am::error::Manager`.
#[derive(Debug)]
pub struct ErrorManager {
    life: Lifecycle,
    /// Init sub-stage 0..4 (error.md 13).
    stage: u8,
    config: ErrorConfig,
    config_done: bool,
    current: FatalError,
    pending: FatalError,
    system_ring: Vec<FatalError>,
    app_ring: Vec<FatalError>,
    table: Vec<(u16, u32)>,
    /// The language and resource sets (`app+24`).
    resources: ResourceSet,
    /// The resource set could not be initialised: error 4104 is raised with the first update.
    resource_failed: bool,
    error_log: ErrorLog,
    app_log: AppLog,
    /// `+11772`: `AppLoaderCount`, read at init stage 4.
    loader_count: u32,
    /// `+11469`: the boot restore ran.
    restored: bool,
    /// The platform develop byte: selects error 87 over 84.
    develop: bool,
    /// [`NvramContext::app_generation`] the application-file record cache belongs to.
    generation: u32,
}

impl Default for ErrorManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ErrorManager {
    /// The constructed, uninitialised manager.
    pub fn new() -> Self {
        Self {
            life: Lifecycle::default(),
            stage: 0,
            config: ErrorConfig::default(),
            config_done: false,
            current: FatalError::default(),
            pending: FatalError::default(),
            system_ring: Vec::new(),
            app_ring: Vec::new(),
            table: Vec::new(),
            resources: ResourceSet::default(),
            resource_failed: false,
            error_log: ErrorLog::default(),
            app_log: AppLog::default(),
            loader_count: 0,
            restored: false,
            develop: false,
            generation: 0,
        }
    }

    /// `0x140232BF0`, run by `CheckingCabinetTypeState::enter`: attaches the master handle (A.5), state 1,
    /// stage 0, current item empty.
    pub fn enter(&mut self, master: &mut Master) {
        master.attach();
        self.current = FatalError::default();
        master.copy_fatal_error(0, "");
        self.life.state = StateWord::INITIALIZED;
        self.stage = 0;
    }

    /// `0x140233010`, run by `OperatingState::enter`: stores `ErrorConfig` and initialises the resource set
    /// (`0x140283F80`, called by `0x14029CF30` with the exclusion filters). A failure raises 4104 with the first
    /// update and leaves the language empty for the rest of the run.
    pub fn configure(&mut self, cx: &Cx<'_>) {
        self.config = cx.config.error;
        self.config_done = true;
        let resource = &cx.config.common.resource;
        let custom_ids =
            resource.custom_resource.iter().map(|id| id.as_str().to_owned()).filter(|id| !id.is_empty()).collect();
        let result = self.resources.initialize(
            &*cx.backend.fs,
            resource.resource_path.as_str(),
            resource.language.as_str(),
            custom_ids,
            Filters::from_config(cx.config),
        );
        self.resource_failed = result != 0;
    }

    /// `0x140232F60(dongle ready, gameId)`, once per Operating frame: one init stage while the state is not 2/3.
    /// `game_id` is the keychip's current game id (`ctx+26206`), read when the stamp is made.
    pub fn update(
        &mut self,
        cx: &mut Cx<'_>,
        master: &mut Master,
        dongle_ready: bool,
        develop: bool,
        game_id: Option<GameId>,
    ) {
        if self.life.state == StateWord::UNINITIALIZED {
            return;
        }
        self.develop = develop;
        self.follow_generation(cx.nvram);
        if self.life.state.is_done() {
            return;
        }
        match self.stage {
            0 => {
                if std::mem::take(&mut self.resource_failed) {
                    let time = tm_from_date_time(cx.clock.local_now());
                    self.raise(cx, master, ERROR_BAD_CONFIG, 0, time);
                }
                if master.is_attached() {
                    self.stage = 1;
                } else {
                    self.life.error.set(ERROR_UNEXPECTED, 0);
                    self.life.state = StateWord::FAILED;
                }
            }
            1 => {
                self.load_error_log(cx.nvram);
                self.stage = 2;
            }
            2 => {
                self.load_app_log(cx.nvram);
                self.stage = 3;
            }
            3 => {
                if self.config_done && dongle_ready {
                    self.stage = 4;
                }
            }
            _ => self.finish_init(cx, master, game_id),
        }
    }

    /// Init stage 4: the rings and the table from the records, the stamp, the boot restore, then the pending
    /// error is committed or the current item announced.
    fn finish_init(&mut self, cx: &mut Cx<'_>, master: &mut Master, game_id: Option<GameId>) {
        self.reload_rings();
        self.setup_to_save(cx, game_id);
        self.restore(cx.nvram);
        self.restored = true;
        if self.pending.number != 0 {
            self.commit(cx, master);
        } else {
            self.notify(cx, master);
        }
        self.life.state = if self.life.error.is_empty() { StateWord::READY } else { StateWord::FAILED };
    }

    /// `0x140233D70`: reads the loader count and stamps the `ErrorLog` record with it and the game id.
    fn setup_to_save(&mut self, cx: &mut Cx<'_>, game_id: Option<GameId>) {
        self.loader_count = Master::app_loader_count(&mut *cx.backend.registry);
        let game_id = game_id.map_or(NO_GAME_ID, |id| {
            let mut out = [b'?'; 4];
            for (slot, &unit) in out.iter_mut().zip(id.units()) {
                if let Ok(byte) = u8::try_from(unit)
                    && byte.is_ascii()
                {
                    *slot = byte;
                }
            }
            out
        });
        self.error_log.stamp(cx.nvram, self.loader_count as u8, game_id);
    }

    /// `0x140233F10`, once per process: an error that is still unresolved in the `ErrorLog` is either dropped or
    /// raised again as the pending error (`error.md` 18.1).
    fn restore(&mut self, nvram: &NvramContext) {
        if self.restored || !self.error_log.unresolved() {
            return;
        }
        let Some(entry) = self.error_log.entry(0) else { return };
        let mut item = match self.resources.errors() {
            Some(_) => self.localised(entry.number, entry.sub_number, from_filetime(entry.time)),
            None => builtin(entry.number).unwrap_or_default(),
        };
        if item.reset_type == 0 {
            item.reset_type = 4;
        }
        let drop_it = (matches!(item.reset_type, 1 | 3 | 4) && self.loader_count < 2)
            || (matches!(item.reset_type, 3 | 4) && self.config.auto_reset);
        let result = if drop_it {
            self.error_log.resolve(nvram, true)
        } else {
            self.pending = item;
            self.error_log.rewrite_loader(nvram)
        };
        if let Err(status) = result {
            self.latch(status);
        }
    }

    /// `0x140233090(number, sub)`: stores the pending error when neither a current nor a pending one exists,
    /// and commits it at once when the manager is set up (14.1).
    pub fn raise(&mut self, cx: &mut Cx<'_>, master: &mut Master, number: u16, sub_number: u16, time: Tm) {
        if self.current.number != 0 || self.pending.number != 0 {
            return;
        }
        self.pending = FatalError { number, sub_number, time, ..FatalError::default() };
        if self.life.state.is_done() {
            self.commit(cx, master);
        }
    }

    /// Fills the message and reset type of an unresolved item from the resources, or the compiled-in records
    /// (14.2). Items that already carry a reset type are left alone.
    fn resolve(&self, item: &mut FatalError) {
        if item.reset_type != 0 {
            return;
        }
        match self.resources.errors() {
            Some(resource) => {
                let found = resource.lookup(item.number, item.sub_number, false).filter(|found| found.reset_type != 0);
                let found = found.cloned().unwrap_or_else(|| {
                    resource.lookup(ERROR_UNKNOWN, 0, false).cloned().unwrap_or_else(|| {
                        FatalError::new(ERROR_UNKNOWN, 0, Tm::default(), UNKNOWN_TEXT, RESET_SHUTDOWN)
                    })
                });
                if found.number != 0 && (found.number != item.number || found.sub_number != item.sub_number) {
                    item.number = found.number;
                    item.sub_number = found.sub_number;
                }
                item.message = found.message;
                item.reset_type = found.reset_type;
            }
            None => {
                if let Some(record) = builtin(item.number) {
                    item.sub_number = record.sub_number;
                    item.message = record.message;
                    item.reset_type = record.reset_type;
                }
            }
        }
    }

    /// `0x140234270`: resolves the pending item, logs it into the rings, makes it current and notifies the
    /// listener (14.2).
    fn commit(&mut self, cx: &mut Cx<'_>, master: &mut Master) {
        let mut item = std::mem::take(&mut self.pending);
        self.resolve(&mut item);
        self.log_error(cx.nvram, &item);
        self.current = item;
        self.notify(cx, master);
        master.copy_fatal_error(self.current.number, &wide_to_string(self.current.message_units()));
    }

    /// `0x140234AF0`: the `OccurFatalError` listener with the live current item, then NeedToExit(0) for a real
    /// error while the launcher runs the boot-error mode.
    fn notify(&self, cx: &mut Cx<'_>, master: &Master) {
        cx.events.push(LibamEvent::FatalError(Box::new(self.current.clone())));
        if self.current.number != 0 && master.wants_boot_error() {
            cx.events.push(LibamEvent::NeedToExit(0));
        }
    }

    /// Latches a failed record operation into the manager's own cell when it holds no error yet.
    fn latch(&mut self, status: RecordStatus) {
        if self.life.error.is_empty()
            && let Some((number, sub_number)) = NvramContext::status_error(status, self.develop)
        {
            self.life.error.set(number, sub_number);
        }
    }

    /// `0x140234910` and `0x1402349B0`: the system log and the system ring; only an entry that the system log
    /// stored as new reaches the application log.
    fn log_error(&mut self, nvram: &NvramContext, item: &FatalError) {
        if !self.error_log.is_loaded() {
            return;
        }
        let time = to_filetime(&item.time);
        match self.error_log.log(nvram, item.number, item.sub_number, time) {
            Err(status) => self.latch(status),
            Ok(()) if self.error_log.wrote_new() => {
                push_ring(&mut self.system_ring, item.clone());
                self.log_app(nvram, item, time);
            }
            Ok(()) => {}
        }
    }

    /// `0x1402349B0`: the application log, the application ring and the counter of the item's number.
    fn log_app(&mut self, nvram: &NvramContext, item: &FatalError, time: u64) {
        if !self.app_log.is_loaded() {
            return;
        }
        let times = self.app_log.times_of(item.number).map_or(1, |times| times.saturating_add(1));
        if let Err(status) = self.app_log.log(nvram, item.number, item.sub_number, time, times) {
            self.latch(status);
            return;
        }
        push_ring(&mut self.app_ring, item.clone());
        let row = match self.table.iter().position(|(number, _)| *number == item.number) {
            Some(row) => Some(row),
            None if self.table.len() < TABLE_CAPACITY => {
                self.table.push((item.number, 0));
                Some(self.table.len() - 1)
            }
            None => None,
        };
        if let (Some(row), Some(times)) = (row, self.app_log.times_of(item.number)) {
            self.table[row].1 = times;
        }
    }

    /// The item of `(number, sub)` with the message and reset type of the resources when they know it.
    fn localised(&self, number: u16, sub_number: u16, time: Tm) -> FatalError {
        let mut item = FatalError { number, sub_number, time, ..FatalError::default() };
        if let Some(found) = self.resources.errors().and_then(|resource| resource.lookup(number, sub_number, false)) {
            item.message = found.message;
            item.reset_type = found.reset_type;
        }
        item
    }

    /// `0x140233850`: the system ring from the `ErrorLog`, oldest entry first.
    fn reload_system_ring(&mut self) {
        let items: Vec<FatalError> = (0..self.error_log.count())
            .rev()
            .filter_map(|index| self.error_log.entry(index))
            .map(|entry| self.localised(entry.number, entry.sub_number, from_filetime(entry.time)))
            .collect();
        self.system_ring.clear();
        for item in items {
            push_ring(&mut self.system_ring, item);
        }
    }

    /// `0x140233A40`: the occurrence table (one row per distinct number of the error resource, the compiled-in
    /// numbers without resources) and the application ring from `AppErrorLogInfo`. Needs the record loaded.
    fn reload_application(&mut self) {
        if !self.app_log.is_loaded() {
            return;
        }
        self.app_ring.clear();
        self.table.clear();
        let numbers: Vec<u16> = match self.resources.errors() {
            Some(resource) => resource.items().iter().map(|item| item.number).collect(),
            None => BUILTIN.iter().map(|&(number, _)| number).collect(),
        };
        let mut previous = 0;
        for number in numbers {
            if number != previous && self.table.len() < TABLE_CAPACITY {
                self.table.push((number, self.app_log.times_of(number).unwrap_or(0)));
            }
            previous = number;
        }
        let items: Vec<FatalError> = (0..self.app_log.count())
            .rev()
            .filter_map(|index| self.app_log.entry(index))
            .map(|entry| self.localised(entry.number, entry.sub_number, from_filetime(entry.time)))
            .collect();
        for item in items {
            push_ring(&mut self.app_ring, item);
        }
    }

    fn reload_rings(&mut self) {
        self.reload_system_ring();
        self.reload_application();
    }

    fn load_error_log(&mut self, nvram: &NvramContext) {
        if !self.error_log.is_loaded()
            && let Err(status) = self.error_log.load(nvram)
        {
            self.latch(status);
        }
    }

    fn load_app_log(&mut self, nvram: &NvramContext) {
        if !self.app_log.is_loaded()
            && let Err(status) = self.app_log.load(nvram)
        {
            self.latch(status);
        }
    }

    /// The application-file records belong to one `appfile.dat`: a game-id switch re-reads them and, once set up,
    /// rebuilds the rings and the table (`0x140233290`, storage.md 3.8 step 8).
    fn follow_generation(&mut self, nvram: &NvramContext) {
        let generation = nvram.app_generation();
        if generation == self.generation {
            return;
        }
        self.generation = generation;
        if self.stage < 3 {
            return;
        }
        self.error_log.unload();
        self.app_log.unload();
        self.load_error_log(nvram);
        self.load_app_log(nvram);
        if self.life.state.is_done() {
            self.relocalise();
        }
    }

    /// `0x140233290`: only when set up, the rings and the table are rebuilt and the text of the current error is
    /// looked up again.
    fn relocalise(&mut self) {
        if !self.life.state.is_done() {
            return;
        }
        self.reload_rings();
        if self.current.number != 0
            && let Some(found) =
                self.resources.errors().and_then(|r| r.lookup(self.current.number, self.current.sub_number, false))
        {
            self.current.message = found.message;
        }
    }

    /// The resource set's language switch (`ResourceSet::changeLanguage` `0x140284350`, then `0x140233290`).
    /// 0 on success (also for the language already set); 1 when the files could not be loaded or the resource
    /// set never initialised; 2 for an empty name. The old language and texts stay on a failure.
    pub fn change_language(&mut self, cx: &mut Cx<'_>, language: &[u16]) -> u32 {
        let result = self.resources.change_language(&*cx.backend.fs, language);
        if result == 0 {
            self.relocalise();
        }
        result
    }

    /// `0x1402336E0`: clears the current error when its reset type allows it. `from_mode_change` selects the
    /// type-3/4 auto-reset of a mode switch; the facade clears types 2 and 4 (14.3). Notifies the listener with
    /// the empty item. Returns whether an error was cleared.
    pub fn reset(&mut self, cx: &mut Cx<'_>, master: &mut Master, from_mode_change: bool) -> bool {
        if self.current.number == 0 || (!from_mode_change && !self.life.state.is_done()) {
            return false;
        }
        let allowed = if from_mode_change {
            self.config.auto_reset && matches!(self.current.reset_type, 3 | 4)
        } else {
            matches!(self.current.reset_type, 2 | 4)
        };
        if !allowed {
            return false;
        }
        if self.error_log.unresolved()
            && let Err(status) = self.error_log.resolve(cx.nvram, false)
        {
            self.latch(status);
        }
        let time = tm_from_date_time(cx.clock.local_now());
        self.current = FatalError { time, ..FatalError::default() };
        master.copy_fatal_error(0, "");
        self.notify(cx, master);
        true
    }

    /// `0x140232F90`, the re-initialisation of a mode change (`testFlag` is the sequence's test state): the
    /// type-3/4 auto-reset, then back to init stage 0 with the rings, the table, the pending error and the own
    /// cell cleared. The current error stays unless the auto-reset removed it.
    pub fn restart(&mut self, cx: &mut Cx<'_>, master: &mut Master, test_flag: bool) {
        master.set_test_flag(test_flag);
        self.reset(cx, master, true);
        self.life.state = StateWord::INITIALIZED;
        self.stage = 0;
        self.pending = FatalError::default();
        self.system_ring.clear();
        self.app_ring.clear();
        self.table.clear();
        self.life.error.clear();
    }

    /// `0x1402331B0`: only when set up; writes the default images, clears both rings and rebuilds the table
    /// with zero counters.
    pub fn clear_logs(&mut self, nvram: &NvramContext) {
        if !self.life.state.is_done() {
            return;
        }
        if self.error_log.is_loaded() {
            match self.error_log.clear(nvram) {
                Ok(()) => self.system_ring.clear(),
                Err(status) => self.latch(status),
            }
        }
        if self.app_log.is_loaded() {
            match self.app_log.clear(nvram) {
                Ok(()) => self.reload_application(),
                Err(status) => self.latch(status),
            }
        }
    }

    /// `0x140232E30`: hands the exit values to the launcher and returns to state 0.
    pub fn terminate(&mut self, cx: &mut Cx<'_>, master: &mut Master) {
        if self.life.state == StateWord::UNINITIALIZED {
            return;
        }
        // A language switch re-localises the current error without the master at hand.
        master.copy_fatal_error(self.current.number, &wide_to_string(self.current.message_units()));
        master.finalize(cx.backend);
        *self = Self::new();
    }

    /// Manager state word (`+11460`).
    pub fn state(&self) -> StateWord {
        self.life.state
    }

    /// `allReady` term: state in {2, 3}.
    pub fn is_ready(&self) -> bool {
        self.life.state.is_done()
    }

    /// The manager's own cell (`+11160`), scanned by the aggregator.
    pub fn own_cell(&self) -> ErrorCell {
        self.life.error
    }

    /// The current error, number 0 for none (`g+13916`).
    pub fn current(&self) -> &FatalError {
        &self.current
    }

    /// Whether an error is pending commit.
    pub fn has_pending(&self) -> bool {
        self.pending.number != 0
    }

    /// `ErrorConfig.raise_in_test` (`+17`).
    pub fn raise_in_test(&self) -> bool {
        self.config.raise_in_test
    }

    /// Reset type of the module error `(number, 0)`; see [`Self::reset_type_for`].
    pub fn reset_type_of(&self, number: u16) -> u32 {
        self.reset_type_for(number, 0)
    }

    /// Reset type of the error `(number, sub_number)` (`0x140280B70`, the lookup of the module refresh
    /// predicate `0x1402554E0`): from the resources with the sub-number fallback, from the compiled-in records
    /// without resources; 0 when unknown.
    pub fn reset_type_for(&self, number: u16, sub_number: u16) -> u32 {
        match self.resources.errors() {
            Some(resource) => resource.lookup(number, sub_number, false).map_or(0, |item| item.reset_type),
            None => builtin(number).map_or(0, |item| item.reset_type),
        }
    }

    /// The record of `(number, sub_number)` in the error resource (`warning == false`, with the compiled-in
    /// records as the fallback while no resources are loaded) or the warning resource (`warning == true`), the
    /// lookup behind the network error texts (`0x140280B70`, flag 0: a missing sub number falls back to 0).
    /// `None` when the table does not know the number.
    pub fn error_info(&self, number: u16, sub_number: u16, warning: bool) -> Option<FatalError> {
        let set = if warning { self.resources.warnings() } else { self.resources.errors() };
        match set {
            Some(resource) => resource.lookup(number, sub_number, false).cloned(),
            None if !warning => builtin(number),
            None => None,
        }
    }

    /// The language the resource set holds (`ctx+0xAE8`): empty until a load succeeded.
    pub fn language(&self) -> [u16; 16] {
        self.resources.language_array()
    }

    fn ring(&self, ring: HistoryRing) -> &[FatalError] {
        match ring {
            HistoryRing::Application => &self.app_ring,
            HistoryRing::System => &self.system_ring,
        }
    }
}

/// Newest first, at most [`RING_CAPACITY`] rows.
fn push_ring(ring: &mut Vec<FatalError>, item: FatalError) {
    ring.insert(0, item);
    ring.truncate(RING_CAPACITY);
}

/// `ErrorPort` over the shared state.
#[derive(Debug)]
pub struct ErrorHandle(pub(crate) Shared);

impl ErrorPort for ErrorHandle {
    fn error_facade_available(&self) -> bool {
        self.0.guard().is_ok()
    }

    fn language(&self) -> Option<[u16; 16]> {
        self.0.guard().ok()?;
        Some(self.0.read(|core| core.error.language()))
    }

    fn reset(&mut self) {
        self.0.with(|core| core.reset_error());
    }

    fn set(&mut self, number: u16, sub_number: u16) {
        self.0.with(|core| core.raise_error(number, sub_number));
    }

    fn clear_logs(&mut self) {
        self.0.with(|core| core.error.clear_logs(&core.nvram));
    }

    fn history_count(&self, ring: HistoryRing) -> u32 {
        self.0.read(|core| core.error.ring(ring).len() as u32)
    }

    fn history_item(&self, ring: HistoryRing, index: u32) -> AmResult<FatalError> {
        self.0
            .read(|core| core.error.ring(ring).get(index as usize).cloned())
            .ok_or_else(|| AmError::with_message(1, "Invalid num"))
    }

    fn times_count(&self) -> u32 {
        self.0.read(|core| core.error.table.len() as u32)
    }

    fn times_item(&self, index: u32) -> AmResult<ErrorOccurrence> {
        self.0
            .read(|core| core.error.table.get(index as usize).map(|&(number, times)| ErrorOccurrence { number, times }))
            .ok_or_else(|| AmError::with_message(1, "Invalid num"))
    }
}
