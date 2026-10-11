//! The sequence manager (`am::sequence::Manager`, `spec/libam/sequence.md`): the process state, the play slots
//! and the bookkeeping, plus the `SequencePort` handle.
//!
//! # Cross-module methods
//!
//! * [`Sequence::enter`] (`0x1402355B0`): `CheckingCabinetTypeState::enter`; fires `ChangeState(1)`.
//! * [`Sequence::configure`] (`0x140235910`): `OperatingState::enter`, takes the sequence config and the player
//!   count once.
//! * [`Sequence::update`] (`0x140235770`): once per Operating frame, with the dependency flag and the dongle
//!   values the flow passes.
//! * [`Sequence::set_state`] (`0x140235A80`, sequence.md 3.5): the one place the process state changes; raises
//!   `ChangeState` before the field is stored. [`Sequence::process_state`] is read by the flow, the credit rule
//!   and the idle/busy refresh.
//! * [`Sequence::reset_for_error`] (`0x140235D50`, sequence.md 3.7): what a newly pending error does to the plays.
//! * [`Sequence::store_history`] (`0x140236010`): the end-of-setup History write after a first launch.
//!
//! Time is the backend clock's monotonic millisecond tick; every timer is kept in milliseconds and differences
//! are taken in whole seconds (`book::elapsed`).

mod book;
mod port;
mod refresh;

use libam_api::am_config::SequenceCfg;
use libam_api::backup::GameId;
use libam_api::interface::LibamEvent;
use libam_api::sequence::{BookkeepingTotals, HistogramRow, ProcessState};

use crate::module::{Cx, ErrorCell, EventQueue, Lifecycle, StateWord};
use crate::nvram::{NvramContext, Record, RecordStatus};
use book::{CountedPlay, Published, Work, elapsed};

pub use port::SequenceHandle;

/// Number of play slots.
const SLOTS: usize = 8;
/// Seconds between two periodic flushes (`0x140235770`).
const FLUSH_INTERVAL_SECONDS: u32 = 10;
/// History image: the four game-id characters.
const HISTORY_ID: usize = 8;
/// History image: the region code byte.
const HISTORY_REGION: usize = 12;
/// The game id of a default History record.
const DEFAULT_GAME_ID: [u8; 4] = *b"____";

/// What `setPlayerState` does (`0x140235B80`, sequence.md 3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlayChange {
    /// Mode 1.
    Begin,
    /// Mode 2, always counted.
    Continue,
    /// Mode 0 with the play counted for the statistics.
    End,
    /// Mode 0 without counting.
    Cancel,
}

/// One play slot (`seq+0x4AC+20*i`).
#[derive(Debug, Clone, Copy, Default)]
struct Slot {
    playing: bool,
    start_ms: u64,
    end_ms: u64,
}

/// `am::sequence::Manager` (`ctx+133552`).
#[derive(Debug, Default)]
pub struct Sequence {
    life: Lifecycle,
    /// `+0x008`: the configuration was copied.
    configured: bool,
    config: SequenceCfg,
    /// `max_player` of the common block (`+0xB0`).
    player_count: u64,
    /// `+0x4A8`: `am::sequence::ProcessState` 0..5.
    process_state: u32,
    /// `+0x4A4`.
    first_launch: bool,
    slots: [Slot; SLOTS],
    work: Work,
    published: Published,
    /// `totalTimer` and `playTimer` (ms), `None` until first needed.
    total_timer: Option<u64>,
    play_timer: u64,
    /// The latest clock reading the manager saw; used by the paths that have no clock at hand.
    now_ms: u64,
    /// The loaded History image (`None` = not loaded).
    history: Option<Vec<u8>>,
    /// The loaded PlayInfo image (`None` = not loaded).
    play_image: Option<Vec<u8>>,
    stage_history: bool,
    stage_play: bool,
    stage_checked: bool,
    stage_handled: bool,
    restore_ok: bool,
    /// The platform develop byte: selects error 87 over 84.
    develop: bool,
    /// [`NvramContext::app_generation`] the PlayInfo cache belongs to.
    generation: u32,
}

/// `0x140284F60`: the raw region value as the region code stored in History.
fn region_code(raw: Option<u32>) -> u8 {
    match raw {
        Some(1) => 0x01,
        Some(2) => 0x08,
        Some(3) => 0x04,
        _ => 0,
    }
}

/// The game id as the four ASCII bytes History stores; `None` when a character does not fit (code page 932
/// would need two bytes).
fn game_id_bytes(id: GameId) -> Option<[u8; 4]> {
    let mut out = [0; 4];
    for (slot, &unit) in out.iter_mut().zip(id.units()) {
        *slot = u8::try_from(unit).ok().filter(u8::is_ascii)?;
    }
    Some(out)
}

fn default_history() -> Vec<u8> {
    let mut image = Record::History.blank_image();
    image[HISTORY_ID..HISTORY_ID + 4].copy_from_slice(&DEFAULT_GAME_ID);
    image
}

fn default_play_image() -> Vec<u8> {
    let mut image = Record::PlayInfo.blank_image();
    Work::default().write_into(&mut image);
    image
}

impl Sequence {
    /// The constructed manager.
    pub fn new() -> Self {
        Self::default()
    }

    /// `0x1402355B0`: `configured` cleared, state 1; when the process state is not 1 the slots are cleared,
    /// `ChangeState(1)` is raised first and then the process state stored.
    pub fn enter(&mut self, events: &mut EventQueue) {
        self.configured = false;
        self.life.begin();
        if self.process_state != 1 {
            self.total_timer = None;
            self.slots = [Slot::default(); SLOTS];
            events.push(LibamEvent::ChangeState(ProcessState(1)));
            self.process_state = 1;
        }
    }

    /// `0x140235910`, from `OperatingState::enter`: copies the sequence config and the player count once.
    pub fn configure(&mut self, cx: &Cx<'_>) {
        if !self.configured {
            self.config = cx.config.sequence.clone();
            self.player_count = cx.config.common.max_player;
            self.configured = true;
        }
    }

    /// `0x140235770`, once per Operating frame. `deps_ready` is `platform{2,3} && dongle{2,3}`; the region and the
    /// game id come from the keychip, `develop` is its develop byte.
    pub fn update(
        &mut self,
        cx: &mut Cx<'_>,
        deps_ready: bool,
        region: Option<u32>,
        game_id: Option<GameId>,
        develop: bool,
    ) {
        self.now_ms = cx.clock.tick_ms();
        self.develop = develop;
        self.follow_generation(cx);
        match self.life.state {
            StateWord::INITIALIZED => self.staged_init(cx, deps_ready, region, game_id),
            state if state.is_done() => {
                let now = self.now_ms;
                let seconds = elapsed(self.total_timer.unwrap_or(now), now);
                let in_flush_state = matches!(self.process_state, 1 | 3 | 4);
                let saving = self.process_state != 4 || self.config.save_total_time_on_playing;
                if in_flush_state && saving && seconds >= FLUSH_INTERVAL_SECONDS {
                    self.flush(None);
                }
                self.store_play(cx.nvram);
            }
            _ => {}
        }
    }

    /// The PlayInfo cache belongs to one `appfile.dat`; a game-id switch drops it and re-reads the record
    /// (`0x140235FF0` -> `0x1402367E0`, storage.md 3.8 step 7) when the manager is set up.
    fn follow_generation(&mut self, cx: &Cx<'_>) {
        let generation = cx.nvram.app_generation();
        if generation == self.generation {
            return;
        }
        self.generation = generation;
        self.play_image = None;
        self.stage_play = false;
        if self.life.state.is_done() {
            self.load_play(cx.nvram);
            self.rebuild(cx.nvram);
        }
    }

    /// Latches the error of a failed record operation into the manager's own cell.
    fn latch(&mut self, status: RecordStatus) {
        if let Some((number, sub)) = NvramContext::status_error(status, self.develop) {
            self.life.error.set(number, sub);
        }
    }

    fn load_history(&mut self, nvram: &NvramContext) {
        match nvram.load_record(Record::History, default_history) {
            Ok(image) => self.history = Some(image),
            Err(status) => self.latch(status),
        }
    }

    fn load_play(&mut self, nvram: &NvramContext) {
        match nvram.load_record(Record::PlayInfo, default_play_image) {
            Ok(image) => self.play_image = Some(image),
            Err(status) => self.latch(status),
        }
    }

    /// `0x140236230`: one stage per frame (sequence.md 5.2).
    fn staged_init(&mut self, cx: &mut Cx<'_>, deps_ready: bool, region: Option<u32>, game_id: Option<GameId>) {
        if !self.configured {
            return;
        }
        if !self.stage_history && self.history.is_none() {
            self.load_history(cx.nvram);
            self.stage_history = true;
            return;
        }
        if !self.stage_play && self.play_image.is_none() {
            self.load_play(cx.nvram);
            self.stage_play = true;
            return;
        }
        if !deps_ready {
            return;
        }
        if !self.stage_checked {
            self.first_launch = self.detect(region, game_id);
            self.stage_checked = true;
        }
        if !self.stage_handled {
            if self.first_launch {
                self.restore_ok = self.reset_play(cx.nvram);
            }
            self.stage_handled = true;
            return;
        }
        self.rebuild(cx.nvram);
        self.life.state = if self.life.error.is_empty() { StateWord::READY } else { StateWord::FAILED };
    }

    /// `0x1402363B0` (sequence.md 5.4): the keychip's game id or region differ from the ones History remembers.
    fn detect(&self, region: Option<u32>, game_id: Option<GameId>) -> bool {
        let code = region_code(region);
        let (Some(id), Some(history)) = (game_id, self.history.as_deref()) else { return false };
        let Some(id) = game_id_bytes(id) else { return false };
        code != 0 && (code != history[HISTORY_REGION] || id != history[HISTORY_ID..HISTORY_ID + 4])
    }

    /// `0x1402364A0`: replaces the PlayInfo record by its default when the stored History is not the default.
    /// Returns whether it succeeded (the History write needs it).
    fn reset_play(&mut self, nvram: &NvramContext) -> bool {
        let Some(history) = self.history.as_deref() else { return false };
        let stored_default = history[HISTORY_REGION] == 0 || history[HISTORY_ID..HISTORY_ID + 4] == DEFAULT_GAME_ID;
        if stored_default {
            return true;
        }
        self.work = Work::default();
        let image = default_play_image();
        match nvram.store_record(Record::PlayInfo, &image) {
            RecordStatus::Ok => {
                self.play_image = Some(image);
                true
            }
            status => {
                self.latch(status);
                false
            }
        }
    }

    /// `0x1402367E0` (sequence.md 3.4): copies the cached PlayInfo block, aligns it with the configured histogram
    /// and publishes the rows. Needs the record loaded.
    fn rebuild(&mut self, nvram: &NvramContext) {
        let Some(image) = self.play_image.as_deref() else { return };
        self.work = Work::from_image(image);
        let changed = book::rebuild(&mut self.work, &mut self.published, self.config.histogram.as_slice());
        if changed {
            self.store_play(nvram);
        }
    }

    /// `0x14025CE80(PlayInfo, work)` (sequence.md 5.3): writes the working block when the application file is
    /// open and it differs from the loaded record.
    fn store_play(&mut self, nvram: &NvramContext) {
        if !nvram.app_file_open() {
            return;
        }
        let mut image = match &self.play_image {
            Some(cached) if Work::from_image(cached) == self.work => return,
            Some(cached) => cached.clone(),
            None => Record::PlayInfo.blank_image(),
        };
        self.work.write_into(&mut image);
        match nvram.store_record(Record::PlayInfo, &image) {
            RecordStatus::Ok => self.play_image = Some(image),
            status => self.latch(status),
        }
    }

    /// `0x140236010` (sequence.md 5.4): stores the keychip's game id and region into History once, after a first
    /// launch whose PlayInfo reset succeeded. Skipped when History already holds them.
    pub fn store_history(&mut self, nvram: &NvramContext, region: Option<u32>, game_id: Option<GameId>) {
        let code = region_code(region);
        let Some(id) = game_id.and_then(game_id_bytes) else { return };
        if !self.first_launch || !self.restore_ok || code == 0 {
            return;
        }
        let mut image = self.history.clone().unwrap_or_else(default_history);
        if image[HISTORY_REGION] == code && image[HISTORY_ID..HISTORY_ID + 4] == id {
            return;
        }
        image[HISTORY_ID..HISTORY_ID + 4].copy_from_slice(&id);
        image[HISTORY_REGION] = code;
        match nvram.store_record(Record::History, &image) {
            RecordStatus::Ok => self.history = Some(image),
            status => self.latch(status),
        }
    }

    /// `flush(player)` (`0x140236AC0`, sequence.md 3.3) at the manager's latest clock reading.
    fn flush(&mut self, player: Option<usize>) {
        let now = self.now_ms;
        let play = player.and_then(|index| self.slots.get(index)).map(|slot| CountedPlay {
            play_elapsed: elapsed(self.play_timer, now),
            duration: elapsed(slot.start_ms, slot.end_ms),
        });
        if play.is_some() {
            self.play_timer = now;
        }
        let mut total = self.total_timer.unwrap_or(now);
        book::flush(&mut self.work, &mut self.published, &mut total, now, play);
        self.total_timer = Some(total);
    }

    /// `0x140235A80` (sequence.md 3.5): ignored when `new` equals the current state or is 0, and, once set up,
    /// unless it is in 1..=5. State 1 restarts the total timer; state 2 flushes the bookkeeping (the caller stores
    /// it with [`Self::store_now`]). Clears the slots, raises `ChangeState(new)`, then stores the state. Returns
    /// whether the state changed.
    pub fn set_state(&mut self, new: u32, events: &mut EventQueue) -> bool {
        if new == self.process_state || new == 0 {
            return false;
        }
        if self.life.state.is_done() && !(1..=5).contains(&new) {
            return false;
        }
        match new {
            1 => self.total_timer = Some(self.now_ms),
            2 => self.flush(None),
            _ => {}
        }
        self.slots = [Slot::default(); SLOTS];
        events.push(LibamEvent::ChangeState(ProcessState(new)));
        self.process_state = new;
        true
    }

    /// Writes the working block right away (`0x140236A60`), the second half of `setState(2)`.
    pub fn store_now(&mut self, nvram: &NvramContext) {
        self.store_play(nvram);
    }

    /// Records the clock reading for the paths that run without a clock at hand ([`Self::set_state`],
    /// [`Self::reset_for_error`]).
    pub fn touch(&mut self, now_ms: u64) {
        self.now_ms = now_ms;
    }

    /// `0x140235D50` (sequence.md 3.7): flushes in states 1, 3, 4, clears the slots; in state 4 raises
    /// `ChangeState(3)`.
    pub fn reset_for_error(&mut self, events: &mut EventQueue) {
        if matches!(self.process_state, 1 | 3 | 4) {
            self.flush(None);
        }
        self.slots = [Slot::default(); SLOTS];
        if self.process_state == 4 {
            events.push(LibamEvent::ChangeState(ProcessState(3)));
            self.process_state = 3;
        }
    }

    /// `setPlayerState(idx, mode, count)` (`0x140235B80`, sequence.md 3.2). A silent no-op unless the manager is
    /// set up, not in a test state, free of its own error and `index` is below the player count.
    pub(crate) fn change_play(&mut self, index: usize, change: PlayChange, now_ms: u64, events: &mut EventQueue) {
        let allowed = self.life.state.is_done()
            && !matches!(self.process_state, 2 | 5)
            && self.life.error.is_empty()
            && (index as u64) < self.player_count
            && index < SLOTS;
        if !allowed {
            return;
        }
        self.now_ms = now_ms;
        match change {
            PlayChange::Begin => {
                self.slots[index] = Slot { playing: true, start_ms: now_ms, end_ms: self.slots[index].end_ms };
                if self.process_state == 3 {
                    events.push(LibamEvent::ChangeState(ProcessState(4)));
                    self.process_state = 4;
                    self.flush(None);
                    self.play_timer = now_ms;
                }
            }
            PlayChange::Continue => {
                self.slots[index].playing = false;
                self.slots[index].end_ms = now_ms;
                self.flush(Some(index));
                self.slots[index].playing = true;
                self.slots[index].start_ms = now_ms;
            }
            PlayChange::End | PlayChange::Cancel => {
                self.slots[index].playing = false;
                self.slots[index].end_ms = now_ms;
                self.flush((change == PlayChange::End).then_some(index));
                if self.slots.iter().all(|slot| !slot.playing) && self.process_state != 3 {
                    events.push(LibamEvent::ChangeState(ProcessState(3)));
                    self.process_state = 3;
                }
            }
        }
    }

    /// `0x140235DF0` (sequence.md 3.6): resets the totals and the hit counts when set up. The next
    /// [`Self::update`] stores the block.
    pub fn clear_bookkeeping(&mut self) {
        if self.life.state.is_done() {
            self.work = self.work.cleared();
            self.published.clear_counts();
        }
    }

    /// The six published totals.
    pub fn totals(&self) -> BookkeepingTotals {
        self.published.totals()
    }

    /// The published histogram row count.
    pub fn histogram_count(&self) -> u32 {
        self.published.count()
    }

    /// Histogram row `index`.
    pub fn histogram_row(&self, index: u32) -> Option<HistogramRow> {
        self.published.row(index)
    }

    /// Module restart (`0x140235860`, sequence.md 4.7): slots cleared, state 4 becomes 3 silently, the stage
    /// flags cleared (the loaded records stay), back to state 1.
    pub fn restart(&mut self) {
        if self.life.state.is_done() {
            // Only the eight playing bytes are cleared; the timestamps stay (`0x140235860`).
            for slot in &mut self.slots {
                slot.playing = false;
            }
            if self.process_state == 4 {
                self.process_state = 3;
            }
            self.first_launch = false;
            self.stage_history = false;
            self.stage_play = false;
            self.stage_checked = false;
            self.stage_handled = false;
            self.restore_ok = false;
            self.life.begin();
        }
    }

    /// `0x140235650`: flushes and stores the bookkeeping of a set-up manager, then back to the constructed state.
    pub fn terminate(&mut self, cx: &mut Cx<'_>) {
        if self.life.state == StateWord::UNINITIALIZED {
            return;
        }
        self.now_ms = cx.clock.tick_ms();
        // An unfinished init never rebuilt the working block from the record: storing it would overwrite the
        // saved bookkeeping with the defaults.
        if self.life.state.is_done() && matches!(self.process_state, 1 | 3 | 4) {
            self.flush(None);
            self.store_play(cx.nvram);
        }
        *self = Self::new();
    }

    /// `processState` (`+0x4A8`).
    pub fn process_state(&self) -> u32 {
        self.process_state
    }

    /// `processState` in {2, 5}: the test side.
    pub fn is_test(&self) -> bool {
        matches!(self.process_state, 2 | 5)
    }

    /// `processState` in {3, 4}: advertise or play.
    pub fn is_game(&self) -> bool {
        matches!(self.process_state, 3 | 4)
    }

    /// `isGameMode` (`0x14028F180`): `processState` in {1, 3, 4}; every other state (including 0) is "test" for the
    /// ALL.Net restart and the alw test flag.
    pub fn is_game_mode(&self) -> bool {
        matches!(self.process_state, 1 | 3 | 4)
    }

    /// `firstLaunch` (`ctx+134740`).
    pub fn first_launch(&self) -> bool {
        self.first_launch
    }

    /// `is_playing(i)` (`0x140236140`).
    pub fn is_playing(&self, index: usize) -> bool {
        self.slots.get(index).is_some_and(|slot| slot.playing)
    }

    /// `playerTotal`: the player count once set up, else 0 (sequence.md 4.9).
    pub fn player_total(&self) -> u64 {
        if self.life.state.is_done() { self.player_count } else { 0 }
    }

    /// The configuration copied at `configure`.
    pub fn config(&self) -> &SequenceCfg {
        &self.config
    }

    /// `ctx+134736`.
    pub fn state(&self) -> StateWord {
        self.life.state
    }

    /// `allReady` term: state in {2, 3}.
    pub fn is_ready(&self) -> bool {
        self.life.state.is_done()
    }

    /// The manager's own error holder (`+0x368`), raised by the aggregator.
    pub fn error_cell(&self) -> ErrorCell {
        self.life.error
    }
}
