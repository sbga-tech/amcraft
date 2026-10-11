//! The NetworkClient (`0x140259F40`, `spec/libam/aime.md` A.6): the reader-less [`AimeLib`] that runs the three
//! long-lived Aime DB commands (QueryCampaignInfo, SendPlayLog, QueryCampaignProgress) through a small command
//! sequencer, and the scheduler that decides every frame which one runs next (`0x14025A3A0`).
//!
//! # Scheduler (per frame, [`NetworkClient::update`])
//!
//! 1. The facade passes the `ready` flag (`0x140257D50`): the module's network feature is usable. A change to
//!    ready arms a restart; a change to not ready forgets the resolved server and the campaign info and
//!    reports the DB as not alive.
//! 2. While the sequencer runs it is stepped; when it ends the DB is reported alive when it succeeded, and a
//!    pending campaign-progress query raises the global `TaskFinished(SequenceFinished)` event.
//! 3. Otherwise, when ready: an armed restart runs QueryCampaignInfo; with valid campaign info and an alive DB,
//!    SendPlayLog runs when idle (or every 60 s) while play logs are pending, and QueryCampaignInfo is repeated
//!    every 900 s; with no valid info (or a dead DB) QueryCampaignInfo is retried when idle, alive, or after 60 s.
//!
//! The commands are the original's command classes (`0x1402A02A0`, `0x1402A0400`, `0x1402A0BF0`, `0x1402A1220`,
//! `0x1402A14C0`); their `now_ms` argument is the monotonic millisecond count of the reader layer.

use std::sync::Arc;

use crate::aime::{
    AimeEvent, CAMPAIGN_NAME_UNITS, CampaignInfoEntry, CampaignInfoList, CampaignProgressEntry, CampaignProgressList,
    FinishedStatus, FinishedTask, LibamAimeId,
};
use crate::time::Tm;

use super::{AimeLib, BoardInfo, LibConfig, LibManager, LogEntry, Op, PollKind, RawCampaignInfo};
use backend::aime_db::AimeDbTransport;

/// SendPlayLog is retried every 60 s while the previous run is finished (`0x14025A3A0`).
const LOG_RETRY_MS: u64 = 60_000;
/// QueryCampaignInfo is repeated every 900 s while everything works.
const INFO_REFRESH_MS: u64 = 900_000;
/// QueryCampaignInfo is retried after 60 s while the DB is dead or the info invalid.
const INFO_RETRY_MS: u64 = 60_000;
/// The sequencer holds at most 8 commands (`0x1402566C0`).
const MAX_COMMANDS: usize = 8;
/// One SendPlayLog request carries at most 20 entries (`0x1402A0A80`).
const LOG_BATCH: usize = 20;
/// The libam `ErrorCode` the ForceFailure command raises (`0x1402A1510`): a server access error.
const FORCED_FAILURE: u32 = 10;

/// A pending play-log record (the facade's log store entry, A.3 `send_play_log`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayLogRecord {
    /// Status 1..=4.
    pub status: u8,
    /// Region code (`module+45708`).
    pub region: u16,
    /// Aime id.
    pub aime_id: u32,
    /// Credit.
    pub credit: u32,
    /// `time_t` of the record.
    pub time: i64,
    /// The store byte (`store+81`).
    pub store_byte: u8,
    /// The 11-character serial.
    pub serial: [u8; 11],
}

/// The queue of pending play logs the facade owns (`AppContext+0x2E48`, appended by `0x140261630`).
pub trait PlayLogStore {
    /// Number of pending records.
    fn len(&self) -> usize;
    /// `true` when no record is pending.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Record `index` (0 = oldest).
    fn get(&self, index: usize) -> Option<PlayLogRecord>;
    /// Removes record `index` (accepted by the server).
    fn remove(&mut self, index: usize);
}

/// Why `query_campaign_progress` refused (A.3: 1 bad id, 3 already running).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CampaignError {
    /// Result 1: bad Aime id.
    BadId,
    /// Result 3: a query is already running (network phase 8).
    Busy,
}

/// A command or sequencer state (`am::Command`, M.2.1). Paused and cancelling are never entered by the client.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum State {
    #[default]
    Idle,
    Running,
    Succeeded,
    Failed,
}

impl State {
    fn running(self) -> bool {
        self == Self::Running
    }

    fn finished(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed)
    }
}

/// The commands of the client, in the order of their objects (`client+2872 ..`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CmdId {
    ResolveServer,
    SendPlayLog,
    QueryCampaignInfo,
    QueryCampaignProgress,
    ForceFailure,
}

/// `am::CommandSequencer` (`0x140256650`): up to 8 commands run in order; a failure skips the rest.
#[derive(Debug, Default)]
struct Sequencer {
    state: State,
    list: Vec<CmdId>,
    current: usize,
}

impl Sequencer {
    /// `0x1402566E0`: drops the command list (the state is kept).
    fn clear(&mut self) {
        self.list.clear();
        self.current = 0;
    }

    /// `0x1402566C0`: `false` when the list is full.
    fn add(&mut self, id: CmdId) -> bool {
        if self.list.len() >= MAX_COMMANDS {
            return false;
        }
        self.list.push(id);
        true
    }
}

/// SendPlayLog (`0x1402A0400`): the batch of the current run and the board info it temporarily replaced.
#[derive(Debug, Default)]
struct SendPlayLog {
    state: State,
    /// The entries chosen from the store by [`NetworkClient::prepare_batch`].
    batch: Vec<LogEntry>,
    /// Serial and region every entry of the batch shares.
    key: Option<([u8; 11], u16)>,
    /// Number of entries in the running request.
    entries: usize,
    /// The lib accepted the request (`results` come from it).
    started: bool,
    /// One byte per entry: non-zero = the server accepted the entry.
    results: Vec<u8>,
    /// The board info to put back when the run finishes.
    restore: Option<BoardInfo>,
}

impl SendPlayLog {
    /// `0x1402A0520` (without the state): forgets the batch of the run.
    fn clear(&mut self) {
        self.batch.clear();
        self.key = None;
        self.entries = 0;
        self.started = false;
        self.results.clear();
        self.restore = None;
    }
}

/// The NetworkClient, driven by the facade once per frame through [`NetworkClient::update`].
#[derive(Debug)]
pub struct NetworkClient {
    lib: AimeLib,
    /// The module's network feature is usable (`worker+97`).
    ready: bool,
    /// `ready` changed in this frame (`worker+98`).
    changed: bool,
    /// A restart of QueryCampaignInfo is armed (`client+105`).
    restart_pending: bool,
    /// Periodic work is enabled (`client+104`).
    periodic: bool,
    /// Time of the last QueryCampaignInfo start (`client+108`).
    info_started_ms: u64,
    /// Time of the last SendPlayLog start (`client+116`).
    log_started_ms: u64,
    /// A campaign-progress query waits for its sequence to end (the owner slot's phase 8).
    query_pending: bool,
    /// libam `ErrorCode` of the last finished command (`client+2768`).
    error_code: u32,
    /// The DB is alive (`EventListener+616`).
    alive: bool,
    /// Every `setAlive` call since the last [`Self::take_alive_changes`].
    alive_changes: Vec<bool>,
    events: Vec<AimeEvent>,
    info: CampaignInfoList,
    progress: CampaignProgressList,
    seq: Sequencer,
    resolve_state: State,
    send_log: SendPlayLog,
    info_state: State,
    /// The campaign info is valid (`QueryCampaignInfo+32`).
    info_valid: bool,
    progress_state: State,
    /// The Aime id of the progress query (`QueryCampaignProgress+32`).
    progress_aime_id: u32,
    force_state: State,
}

impl NetworkClient {
    /// The constructor (`0x140259F40`): a reader-less lib, periodic work on, both timers at `now_ms`.
    pub fn new(config: LibConfig, manager: &LibManager, transport: &Arc<dyn AimeDbTransport>, now_ms: u64) -> Self {
        Self {
            lib: AimeLib::new(0, config, manager, transport, None),
            ready: false,
            changed: false,
            restart_pending: false,
            periodic: true,
            info_started_ms: now_ms,
            log_started_ms: now_ms,
            query_pending: false,
            error_code: 0,
            alive: false,
            alive_changes: Vec::new(),
            events: Vec::new(),
            info: blank_campaign_info(),
            progress: CampaignProgressList::default(),
            seq: Sequencer::default(),
            resolve_state: State::Idle,
            send_log: SendPlayLog::default(),
            info_state: State::Idle,
            info_valid: false,
            progress_state: State::Idle,
            progress_aime_id: u32::MAX,
            force_state: State::Idle,
        }
    }

    /// `0x140257CF0`: the board info the DB requests carry (game id, serial, region).
    pub fn set_board(&mut self, board: &BoardInfo) {
        self.lib.set_board_info(board);
    }

    /// `0x14025A140`: enables or disables the periodic QueryCampaignInfo / SendPlayLog scheduling.
    pub fn set_periodic(&mut self, enabled: bool) {
        self.periodic = enabled;
    }

    /// `0x14025A110`: forgets the campaign info and progress lists.
    pub fn reset(&mut self) {
        self.info = blank_campaign_info();
        self.progress = CampaignProgressList::default();
    }

    /// `0x14025A150`: a play log was queued; SendPlayLog goes back to idle unless it is running, so the
    /// scheduler may start it at once.
    pub fn wake_play_log(&mut self) {
        if !self.send_log.state.running() {
            self.send_log.clear();
            self.send_log.state = State::Idle;
        }
    }

    /// `0x14025A310`: SendPlayLog is running.
    pub fn play_log_running(&self) -> bool {
        self.send_log.state.running()
    }

    /// `0x14025A2B0`: the ResolveServer command ended in failure, or QueryCampaignInfo ended.
    pub fn is_done(&self) -> bool {
        self.resolve_state == State::Failed || self.info_state.finished()
    }

    /// `0x14025A330`: the campaign info list.
    pub fn campaign_info(&self) -> CampaignInfoList {
        self.info
    }

    /// `0x14025A340`: the campaign progress list.
    pub fn campaign_progress(&self) -> CampaignProgressList {
        self.progress
    }

    /// libam `ErrorCode` of the last finished command (`client+2768`, the module's `ErrorCode`).
    pub fn error_code(&self) -> u32 {
        self.error_code
    }

    /// `0x140257AD0`: the DB answered the last sequence.
    pub fn is_alive(&self) -> bool {
        self.alive
    }

    /// Every `setAlive` call of the listener (`0x1402579D0`) since the last call, in order. `false` records
    /// fatal error 6508 with the current time in the listener's record, `true` clears the record.
    pub fn take_alive_changes(&mut self) -> Vec<bool> {
        std::mem::take(&mut self.alive_changes)
    }

    /// The global listener callbacks raised since the last call, in order.
    pub fn take_events(&mut self) -> Vec<AimeEvent> {
        std::mem::take(&mut self.events)
    }

    /// Runs the ResolveServer command (`0x1402A0350` / `0x1402A03A0`) as a sequence of its own: forgets the
    /// resolved server and resolves it again. The scheduler never queues it; `false` when the sequencer runs.
    pub fn resolve_server(&mut self, now_ms: u64) -> bool {
        if self.seq.state.running() {
            return false;
        }
        self.begin_sequence(CmdId::ResolveServer, now_ms);
        true
    }

    /// `0x14025A190` after the id check of `0x14022A340`: starts the campaign-progress query of `aime_id`. The
    /// sequence appends to a running one; without a usable network the forced failure runs instead.
    pub fn query_campaign_progress(&mut self, aime_id: LibamAimeId, now_ms: u64) -> Result<(), CampaignError> {
        if aime_id.id == u32::MAX {
            return Err(CampaignError::BadId);
        }
        if self.query_pending {
            return Err(CampaignError::Busy);
        }
        self.progress = CampaignProgressList::default();
        self.error_code = 0;
        self.query_pending = true;
        self.progress_aime_id = aime_id.id;
        if !self.seq.state.running() {
            self.seq.clear();
        }
        self.seq.add(if self.ready { CmdId::QueryCampaignProgress } else { CmdId::ForceFailure });
        if !self.seq.state.running() {
            self.start_sequence(now_ms);
        }
        if self.ready {
            self.emit(FinishedTask::QueryCampaignProgress, FinishedStatus::Queued);
        }
        Ok(())
    }

    /// The module's per-frame call: `0x140257D50` (the ready flag) then the scheduler `0x14025A3A0`.
    pub fn update(&mut self, ready: bool, store: &mut dyn PlayLogStore, now_ms: u64) {
        self.changed = ready != self.ready;
        self.ready = ready;
        if self.changed {
            if ready {
                self.restart_pending = true;
            } else {
                let idle = !self.seq.state.running();
                self.lib.clear_resolve();
                if idle {
                    self.resolve_state = State::Idle;
                }
                self.info_valid = false;
                if idle {
                    self.info_state = State::Idle;
                }
                self.set_alive(false);
                self.restart_pending = false;
            }
        }
        if self.seq.state.running() {
            self.step_sequence(store, now_ms);
            if self.seq.state.finished() {
                self.set_alive(self.seq.state == State::Succeeded);
                if self.query_pending {
                    let status = if self.seq.state == State::Succeeded {
                        FinishedStatus::Succeeded
                    } else {
                        FinishedStatus::Failed
                    };
                    self.emit(FinishedTask::SequenceFinished, status);
                    self.query_pending = false;
                }
            }
            return;
        }
        if !self.ready {
            return;
        }
        if self.restart_pending {
            self.begin_info_sequence(now_ms);
            self.restart_pending = false;
            return;
        }
        if self.info_valid && self.alive {
            let log_state = self.send_log.state;
            let log_due = (!log_state.running() && !log_state.finished())
                || now_ms.saturating_sub(self.log_started_ms) >= LOG_RETRY_MS;
            if log_due && !log_state.running() && !store.is_empty() {
                self.begin_log_sequence(store, now_ms);
                return;
            }
            if self.periodic && now_ms.saturating_sub(self.info_started_ms) >= INFO_REFRESH_MS {
                self.begin_info_sequence(now_ms);
            }
        } else if self.periodic {
            let idle = !self.info_state.running() && !self.info_state.finished();
            if idle || self.alive || now_ms.saturating_sub(self.info_started_ms) >= INFO_RETRY_MS {
                self.begin_info_sequence(now_ms);
            }
        }
    }

    /// Terminates the lib's worker threads (the destructor `0x140332710`); returns its bit mask, 0 = clean.
    pub fn terminate(&mut self, now_ms: u64) -> u32 {
        self.lib.terminate(now_ms)
    }

    fn set_alive(&mut self, alive: bool) {
        self.alive = alive;
        self.alive_changes.push(alive);
    }

    fn emit(&mut self, task: FinishedTask, status: FinishedStatus) {
        self.events.push(AimeEvent::TaskFinished { task, status });
    }

    // -----------------------------------------------------------------------------------------------------
    // Sequencer
    // -----------------------------------------------------------------------------------------------------

    /// `0x14025A6E0`: the QueryCampaignInfo sequence.
    fn begin_info_sequence(&mut self, now_ms: u64) {
        self.begin_sequence(CmdId::QueryCampaignInfo, now_ms);
        self.info_started_ms = now_ms;
    }

    /// `0x14025A740`: the SendPlayLog sequence over a batch taken from the store.
    fn begin_log_sequence(&mut self, store: &dyn PlayLogStore, now_ms: u64) {
        self.prepare_batch(store);
        self.begin_sequence(CmdId::SendPlayLog, now_ms);
        self.log_started_ms = now_ms;
    }

    fn begin_sequence(&mut self, id: CmdId, now_ms: u64) {
        self.seq.clear();
        self.seq.add(id);
        self.start_sequence(now_ms);
    }

    /// `0x1402564D0`: the sequencer runs and its first command starts.
    fn start_sequence(&mut self, now_ms: u64) {
        self.seq.state = State::Running;
        self.seq.current = 0;
        let first = self.seq.list[0];
        self.begin(first, now_ms);
    }

    /// `0x140256740`: one step of the running command, then the transition to the next one.
    fn step_sequence(&mut self, store: &mut dyn PlayLogStore, now_ms: u64) {
        let id = self.seq.list[self.seq.current];
        if self.state(id).running() {
            self.step(id, store, now_ms);
        }
        let state = self.state(id);
        if !state.finished() {
            return;
        }
        self.finish(id);
        if state != State::Succeeded {
            self.seq.state = State::Failed;
        } else if self.seq.current + 1 < self.seq.list.len() {
            self.seq.current += 1;
            let next = self.seq.list[self.seq.current];
            self.begin(next, now_ms);
        } else {
            self.seq.state = State::Succeeded;
        }
    }

    fn state(&self, id: CmdId) -> State {
        match id {
            CmdId::ResolveServer => self.resolve_state,
            CmdId::SendPlayLog => self.send_log.state,
            CmdId::QueryCampaignInfo => self.info_state,
            CmdId::QueryCampaignProgress => self.progress_state,
            CmdId::ForceFailure => self.force_state,
        }
    }

    fn set_state(&mut self, id: CmdId, state: State) {
        match id {
            CmdId::ResolveServer => self.resolve_state = state,
            CmdId::SendPlayLog => self.send_log.state = state,
            CmdId::QueryCampaignInfo => self.info_state = state,
            CmdId::QueryCampaignProgress => self.progress_state = state,
            CmdId::ForceFailure => self.force_state = state,
        }
    }

    // -----------------------------------------------------------------------------------------------------
    // Commands: start (vtable +8), step (+24), finish (+16)
    // -----------------------------------------------------------------------------------------------------

    /// The start bodies ignore the start result (`0x1402564D0` discards it): a refused start leaves the command
    /// running, and its poll then reports whatever the lib holds.
    fn begin(&mut self, id: CmdId, now_ms: u64) {
        self.set_state(id, State::Running);
        match id {
            CmdId::ResolveServer => {
                self.lib.clear_resolve();
                let _ = self.lib.start(Op::KeepResolveServer, now_ms);
            }
            CmdId::SendPlayLog => self.start_send_play_log(now_ms),
            CmdId::QueryCampaignInfo => {
                self.info_valid = false;
                let _ = self.lib.start(Op::CampaignInfoGet, now_ms);
            }
            CmdId::QueryCampaignProgress => {
                self.emit(FinishedTask::QueryCampaignProgress, FinishedStatus::Started);
                let _ = self.lib.start(Op::CampaignClearInfoGet { aime_id: self.progress_aime_id }, now_ms);
            }
            // The original also succeeds when the owner slot's phase is 3 and an access code is set; the
            // client's slot only ever holds phase 8, so the command always fails.
            CmdId::ForceFailure => {
                self.error_code = FORCED_FAILURE;
                self.force_state = State::Failed;
            }
        }
    }

    fn step(&mut self, id: CmdId, store: &mut dyn PlayLogStore, now_ms: u64) {
        match id {
            CmdId::ResolveServer => {
                if self.lib.poll(PollKind::KeepResolveServer, now_ms) {
                    let code = self.lib.result().error;
                    self.resolve_state = self.conclude(code.libam_code());
                }
            }
            CmdId::SendPlayLog => self.step_send_play_log(store, now_ms),
            CmdId::QueryCampaignInfo => {
                if self.lib.poll(PollKind::CampaignInfoGet, now_ms) {
                    let result = self.lib.result();
                    self.info_state = self.conclude(result.db_thread_error.libam_code());
                    if self.info_state == State::Succeeded {
                        self.info_valid = true;
                        self.info = campaign_info_list(&result.campaign_info);
                    }
                }
            }
            CmdId::QueryCampaignProgress => {
                if self.lib.poll(PollKind::CampaignClearInfoGet, now_ms) {
                    self.progress_aime_id = u32::MAX;
                    let result = self.lib.result();
                    self.progress_state = self.conclude(result.error.libam_code());
                    if self.progress_state == State::Succeeded {
                        for clear in result.campaign_clear.iter().filter(|clear| clear.id != 0) {
                            let entry =
                                CampaignProgressEntry { id: clear.id, flag: clear.flag != 0, value: clear.value };
                            push_progress(&mut self.progress, entry);
                        }
                    }
                }
            }
            CmdId::ForceFailure => {}
        }
    }

    fn finish(&mut self, id: CmdId) {
        match id {
            CmdId::QueryCampaignProgress => {
                let status = if self.progress_state == State::Succeeded {
                    FinishedStatus::Succeeded
                } else {
                    FinishedStatus::Failed
                };
                self.emit(FinishedTask::QueryCampaignProgress, status);
            }
            // `0x1402A0830`: puts the board info back and forgets the batch.
            CmdId::SendPlayLog => {
                if let Some(board) = self.send_log.restore.take() {
                    self.lib.set_board_info(&board);
                }
                self.send_log.clear();
            }
            CmdId::ResolveServer | CmdId::QueryCampaignInfo | CmdId::ForceFailure => {}
        }
    }

    /// The end of a command whose lib poll finished: the libam `ErrorCode` is stored (0 on success).
    fn conclude(&mut self, error_code: u32) -> State {
        self.error_code = error_code;
        if error_code == 0 { State::Succeeded } else { State::Failed }
    }

    /// The batch of the next SendPlayLog run (`0x1402A05E0` over `0x1402A0A80`): the oldest records that share
    /// the first record's serial and region, at most 20.
    fn prepare_batch(&mut self, store: &dyn PlayLogStore) {
        self.send_log.clear();
        for index in 0..store.len() {
            let Some(record) = store.get(index) else { break };
            if self.send_log.batch.len() >= LOG_BATCH {
                break;
            }
            match self.send_log.key {
                Some((serial, region)) if !same_serial(&serial, &record.serial) || region != record.region => break,
                Some(_) => {}
                None => self.send_log.key = Some((record.serial, record.region)),
            }
            self.send_log.batch.push(log_entry(&record));
        }
    }

    fn start_send_play_log(&mut self, now_ms: u64) {
        let log = &mut self.send_log;
        let Some((serial, region)) = log.key.filter(|_| !log.batch.is_empty()) else {
            // Nothing to send: the original logs "empty play log" and fails.
            log.state = State::Failed;
            return;
        };
        log.entries = log.batch.len();
        log.results = vec![0; log.entries];
        let current = self.lib.board_info();
        let text = serial_text(&serial);
        if !current.serial.bytes().take(11).eq(text.bytes()) || current.arg != u32::from(region) {
            let board = BoardInfo { game_id: current.game_id.clone(), serial: text, arg: u32::from(region) };
            self.send_log.restore = Some(current);
            self.lib.set_board_info(&board);
        }
        let batch = std::mem::take(&mut self.send_log.batch);
        self.send_log.started = self.lib.start(Op::SendAimeLogex(batch), now_ms).is_ok();
    }

    /// `0x1402A0950`: when the request ended, the accepted records leave the store.
    fn step_send_play_log(&mut self, store: &mut dyn PlayLogStore, now_ms: u64) {
        if !self.lib.poll(PollKind::SendAimeLogex, now_ms) {
            return;
        }
        let code = self.lib.result().log_thread_error.libam_code();
        self.send_log.state = self.conclude(code);
        if self.send_log.state != State::Succeeded {
            return;
        }
        if self.send_log.started {
            self.send_log.results = self.lib.log_results();
        }
        let mut index = 0;
        for entry in 0..self.send_log.entries {
            if self.send_log.results.get(entry).is_some_and(|&accepted| accepted != 0) {
                store.remove(index);
            } else {
                index += 1;
            }
        }
    }
}

/// The wire entry of a record (`0x1402A0A80`): a status outside 1..=4 counts as 1.
fn log_entry(record: &PlayLogRecord) -> LogEntry {
    LogEntry {
        aime_id: record.aime_id,
        status: if (2..=4).contains(&record.status) { u32::from(record.status) } else { 1 },
        credit: record.credit,
        time: record.time,
        record_byte: u32::from(record.store_byte),
        region: u32::from(record.region),
    }
}

/// `strncmp(a, b, 11) == 0`.
fn same_serial(a: &[u8; 11], b: &[u8; 11]) -> bool {
    a.iter().take_while(|&&byte| byte != 0).eq(b.iter().take_while(|&&byte| byte != 0))
}

/// The serial as text, up to the first NUL.
fn serial_text(serial: &[u8; 11]) -> String {
    let length = serial.iter().position(|&byte| byte == 0).unwrap_or(serial.len());
    String::from_utf8_lossy(&serial[..length]).into_owned()
}

/// An empty campaign-info list (`0x140221220`).
fn blank_campaign_info() -> CampaignInfoList {
    let entry = CampaignInfoEntry { id: 0, name: [0; CAMPAIGN_NAME_UNITS], times: [Tm::default(); 5] };
    CampaignInfoList { count: 0, entries: [entry; 3] }
}

/// `0x1402215F0`: appends to the progress list while it holds fewer than three entries.
fn push_progress(list: &mut CampaignProgressList, entry: CampaignProgressEntry) {
    if let Some(slot) = usize::try_from(list.count).ok().and_then(|index| list.entries.get_mut(index)) {
        *slot = entry;
        list.count += 1;
    }
}

/// The libam campaign-info list of the server's entries (`0x1402A0CA0`, 5.2): entries without an id are skipped.
fn campaign_info_list(raw: &[RawCampaignInfo; 3]) -> CampaignInfoList {
    let mut list = blank_campaign_info();
    for (slot, entry) in list.entries.iter_mut().zip(raw.iter().filter(|entry| entry.id != 0)) {
        *slot = CampaignInfoEntry { id: entry.id, name: wide_name(&entry.name), times: entry.stamps.map(stamp_time) };
        list.count += 1;
    }
    list
}

/// The ANSI campaign name (code page 932) as the zero-padded wide string, at most 127 units.
fn wide_name(name: &[u8; 128]) -> [u16; CAMPAIGN_NAME_UNITS] {
    let length = name.iter().position(|&byte| byte == 0).unwrap_or(name.len());
    let (text, _, _) = encoding_rs::SHIFT_JIS.decode(&name[..length]);
    let mut wide = [0; CAMPAIGN_NAME_UNITS];
    for (slot, unit) in wide.iter_mut().take(CAMPAIGN_NAME_UNITS - 1).zip(text.encode_utf16()) {
        *slot = unit;
    }
    wide
}

/// A decimal `YYMMDDHH` stamp as the libam `tm` (5.2): minute and second are 0.
fn stamp_time(stamp: u32) -> Tm {
    let field = |divisor: u32| (stamp / divisor % 100) as i32;
    Tm { hour: field(1), mday: field(100), mon: field(10_000) - 1, year: field(1_000_000) + 100, ..Tm::default() }
}
