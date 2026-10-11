//! The aimelib core of one unit (`spec/libam/aime.md` part C): the poll state machine (statuses 0..=28), the start
//! functions and their gate, cancel, the DB and log worker slots, and the firmware pollers.
//!
//! The original runs the machine on the caller's thread, one tick per poll, and the DB/log work on threads. Here the
//! machine ticks inside [`AimeLib::poll`]; the DB work runs as [`db`] jobs whose results are applied in later ticks.

use std::sync::Arc;
use std::time::Duration;

use super::db::{self, AimeReq, DbOp, DbOutput, Pending};
use super::proto::{self, DbParams};
use super::{
    BoardInfo, InternalCode, LibConfig, LibManager, Op, PollKind, ReaderStrings, ResultBlock, StartError, StartResult,
    flags,
};
use crate::aime::device::{
    CardKind, FelicaKind, FirmwareFiles, FirmwareStep, FollowUp, InitStep, LedFirmwareStep, PollStep, Reader,
};
use backend::Fs;
use backend::aime_db::{AimeDbTransport, ServerAddr};

/// Statuses used by the machine (`impl+2232`, C 6.2).
mod st {
    pub const IDLE: u32 = 0;
    pub const NFC_FW_START: u32 = 1;
    pub const NFC_FW_RUN: u32 = 2;
    pub const NFC_FW_DONE: u32 = 3;
    pub const LED_FW_START: u32 = 4;
    pub const LED_FW_RUN: u32 = 5;
    pub const LED_FW_DONE: u32 = 6;
    pub const NFC_INIT: u32 = 7;
    pub const NFC_INIT_ALIAS: u32 = 8;
    pub const CARD_POLL: u32 = 9;
    pub const STOP_RW: u32 = 10;
    pub const INTERRUPTED: u32 = 11;
    pub const DB_INIT: u32 = 12;
    pub const DB_RESOLVE: u32 = 13;
    pub const CHECK_FELICA_ID: u32 = 14;
    pub const CHECK_AIME_UID: u32 = 15;
    pub const FELICA_ID_GET: u32 = 16;
    pub const FELICA_ID_REGIST: u32 = 17;
    pub const ACCOUNT_GETEX: u32 = 18;
    pub const ACCOUNT_REGIST: u32 = 19;
    pub const CAMPAIGN_CLEAR: u32 = 20;
    pub const ACCOUNT_GET: u32 = 21;
    pub const ACCOUNT_GET_S: u32 = 22;
    pub const DB_CANCEL_WAIT: u32 = 25;
    pub const LED_OK: u32 = 26;
    pub const LED_NG: u32 = 27;
    pub const STOP: u32 = 28;
}

/// Wait of the pollers and of terminate for a worker (C 7.3).
const THREAD_WAIT: Duration = Duration::from_millis(5000);
/// Longest cancel loop (C 8: 5000 ticks of `Sleep(1)`).
const CANCEL_TICKS: u32 = 5000;
/// Default firmware list (C 9.1).
const DEFAULT_NFC_FIRMWARE: [&str; 3] =
    ["./firm/TN32MSEC003S_V12.hex", "./firm/update_15286_6691_94.bin", "./firm/update_15396_6728_94.bin"];
/// Default LED board firmware path (C 9.1).
const DEFAULT_LED_FIRMWARE: &str = "./firm/837_15084_1_01.hex";

/// The DB client object of a machine run (`0x14034F150`).
struct DbSession {
    server: Option<ServerAddr>,
    resolving: Option<Pending<Result<ServerAddr, InternalCode>>>,
    job: Option<Pending<DbOutput>>,
}

/// A worker slot: the DB slot (alive, campaign info) or the log slot (C 7.2, 9.5).
struct Slot {
    /// The operation waiting for its start condition.
    queued: Option<DbOp>,
    server: Option<ServerAddr>,
    running: Option<(DbOp, Pending<DbOutput>)>,
    /// The thread handle exists until a poller reaps it.
    handle: bool,
    /// The slot's work finished.
    done: bool,
    /// `dbResult` / `lsResult` / `logResult` (default 9).
    result: InternalCode,
    /// The finished output, kept until the slot's state is read.
    output: Option<DbOutput>,
}

impl Slot {
    fn new() -> Self {
        Self {
            queued: None,
            server: None,
            running: None,
            handle: false,
            done: true,
            result: InternalCode(9),
            output: None,
        }
    }

    fn begin(&mut self, op: DbOp, server: Option<ServerAddr>) {
        self.queued = Some(op);
        self.server = server;
        self.handle = true;
        self.done = false;
        self.result = InternalCode::SERVER_TIMEOUT;
        self.output = None;
    }

    /// A worker is running (`handle != 0 && !done && !waiting`).
    fn active(&self) -> bool {
        self.running.is_some()
    }
}

/// The aimelib object of one unit (`cAiMeLib` + impl).
pub struct AimeLib {
    unit_code: u32,
    manager: LibManager,
    generation: u64,
    transport: Arc<dyn AimeDbTransport>,
    reader: Option<Reader>,
    mode: u32,
    status: u32,
    status_changed: bool,
    status_snap: u32,
    scan_filter: u8,
    check_only: bool,
    board: BoardInfo,
    board_set: bool,
    r: ResultBlock,
    arg1: u32,
    follow_up: Option<FollowUp>,
    scan_timeout_sec: u32,
    keep_resolved: bool,
    server_rec: Option<ServerAddr>,
    terminating: bool,
    cancel_exec: bool,
    cancel_loop: bool,
    cancel_ticks: u32,
    strings: ReaderStrings,
    fw_kind: u8,
    fw_updating: bool,
    db: Option<DbSession>,
    db_slot: Slot,
    log_slot: Slot,
    log_type: u32,
    log_results: Vec<u8>,
    db_alive: bool,
}

impl std::fmt::Debug for AimeLib {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AimeLib")
            .field("unit_code", &self.unit_code)
            .field("mode", &self.mode)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

fn scan_timeout(seconds: i64) -> u32 {
    if seconds > 0 { seconds.min(2_147_483) as u32 } else { 0 }
}

impl AimeLib {
    /// The SubContext sequence (`0x1402557F0`): constructor, nfc cfg 2000, LED off, alive cache 0, polling
    /// interval. `unit_code` = `com + 1000 * unit`; `reader` is `None` for the NetworkClient's instance.
    ///
    /// The lib LED (`0x14032D8B0`, libLedOff) is always off here: the SubContext switches it off and nothing
    /// switches it on, so the cosmetic LED states 26/27 only move the status.
    pub fn new(
        unit_code: u32,
        config: LibConfig,
        manager: &LibManager,
        transport: &Arc<dyn AimeDbTransport>,
        reader: Option<Reader>,
    ) -> Self {
        Self {
            unit_code,
            generation: manager.generation(),
            manager: manager.clone(),
            transport: Arc::clone(transport),
            reader,
            mode: 0,
            status: st::IDLE,
            status_changed: false,
            status_snap: st::IDLE,
            scan_filter: 0,
            check_only: false,
            board: BoardInfo::default(),
            board_set: false,
            r: ResultBlock::default(),
            arg1: 0,
            follow_up: None,
            scan_timeout_sec: scan_timeout(i64::from(config.scan_timeout_sec)),
            keep_resolved: false,
            server_rec: None,
            terminating: false,
            cancel_exec: false,
            cancel_loop: false,
            cancel_ticks: 0,
            strings: ReaderStrings::default(),
            fw_kind: 0,
            fw_updating: false,
            db: None,
            db_slot: Slot::new(),
            log_slot: Slot::new(),
            log_type: 0,
            log_results: Vec::new(),
            db_alive: false,
        }
    }

    /// `setBoardInfo` (`0x14032D170`).
    pub fn set_board_info(&mut self, board: &BoardInfo) {
        self.board = board.clone();
        self.board_set = true;
    }

    /// `getBoardInfo` (`0x14032A0E0`).
    pub fn board_info(&self) -> BoardInfo {
        self.board.clone()
    }

    /// `setScanTimeout` (`0x14032DD30`): `> 0` -> `min(sec, 2147483)`, else 0.
    pub fn set_scan_timeout(&mut self, seconds: i64) {
        self.scan_timeout_sec = scan_timeout(seconds);
    }

    // -----------------------------------------------------------------------------------------------------
    // Gate and start functions
    // -----------------------------------------------------------------------------------------------------

    /// The resource check (`0x140327740`): a replaced manager invalidates the unit for good.
    fn resource_ok(&mut self) -> bool {
        if self.generation == self.manager.generation() {
            return true;
        }
        self.clear_resolve();
        false
    }

    /// The start gate (`0x140329BD0`).
    fn gate(&mut self) -> StartResult {
        if !self.resource_ok() {
            return Err(StartError::Busy);
        }
        if self.cancel_loop {
            return Err(StartError::CancelRunning);
        }
        if !matches!(self.status, st::IDLE | st::STOP | st::NFC_FW_DONE | st::LED_FW_DONE) {
            return Err(StartError::NotIdle);
        }
        if let Some(reader) = self.reader.as_mut() {
            reader.set_amiibo_mode(false);
        }
        Ok(())
    }

    fn set_status(&mut self, status: u32) {
        self.status = status;
        self.status_changed = true;
    }

    /// Common tail of a gated start: mode, status and the server record to use.
    fn begin(&mut self, mode: u32, status: u32) {
        self.cancel_exec = false;
        self.mode = mode;
        self.server_rec = self.manager.resolved();
        self.set_status(status);
    }

    fn begin_scan(&mut self, mode: u32, filter: u8) {
        self.scan_filter = filter;
        self.r = ResultBlock::default();
        self.begin(mode, st::NFC_INIT);
    }

    /// Starts an operation. Result codes as in C 9.
    pub fn start(&mut self, op: Op, _now_ms: u64) -> StartResult {
        let result = match op {
            Op::DbAliveCheck => self.start_db_slot(DbOp::Alive),
            Op::CampaignInfoGet => self.start_db_slot(DbOp::CampaignInfo),
            Op::SendLogStatus { a, b } => self.start_log_slot(1, DbOp::LogStatus(a, b)),
            Op::SendAimeLog(log) => self.start_log_slot(2, DbOp::SingleLog(log)),
            Op::SendAimeLogex(entries) => self.start_log_slot(3, DbOp::MultiLog(entries)),
            Op::KeepResolveServer => {
                self.r.error = InternalCode::NONE;
                if let Some(addr) = self.manager.resolved() {
                    self.server_rec = Some(addr);
                    Err(StartError::Refused)
                } else {
                    self.mode = 15;
                    self.keep_resolved = true;
                    self.set_status(st::STOP);
                    self.server_rec = None;
                    Ok(())
                }
            }
            op => self.start_gated(op),
        };
        self.pump_slots();
        result
    }

    fn start_gated(&mut self, op: Op) -> StartResult {
        self.gate()?;
        match op {
            Op::UnitCheck => self.begin_scan(4, 0),
            Op::Scan { filter } => self.begin_scan(5, filter),
            Op::MailerOpen(text) => {
                self.follow_up = Some(FollowUp::Mail(text));
                self.begin_scan(1, 2);
            }
            Op::WebOpen(text) => {
                self.follow_up = Some(FollowUp::Web(text));
                self.begin_scan(2, 2);
            }
            Op::AmiiboInfoGet => self.begin_scan(3, 1),
            Op::AccountCheck(block) => {
                self.r = *block;
                self.check_only = true;
                self.begin(7, st::DB_INIT);
            }
            Op::AccountScan { filter } => {
                self.check_only = false;
                self.begin_scan(9, filter);
            }
            Op::FelicaIdCheck => {
                self.check_only = true;
                self.begin_scan(8, 2);
            }
            Op::AccountGet => {
                let mode = if self.r.flags & flags::LITE_S != 0 { 18 } else { 17 };
                self.begin(mode, st::DB_INIT);
            }
            Op::AccountGetS => self.begin(18, st::DB_INIT),
            Op::AccountGetex => {
                let mode = if self.r.flags & flags::LITE_S != 0 { 18 } else { 12 };
                self.begin(mode, st::DB_INIT);
            }
            Op::AccountRegist => self.begin(13, st::DB_INIT),
            Op::FelicaIdGet => {
                self.scan_filter = 2;
                self.begin(10, st::DB_INIT);
            }
            Op::FelicaIdRegist => self.begin(11, st::DB_INIT),
            Op::CampaignClearInfoGet { aime_id } => {
                self.arg1 = aime_id;
                self.begin(16, st::DB_INIT);
            }
            Op::DbAliveCheck
            | Op::CampaignInfoGet
            | Op::SendLogStatus { .. }
            | Op::SendAimeLog(_)
            | Op::SendAimeLogex(_)
            | Op::KeepResolveServer => unreachable!("handled by start"),
        }
        Ok(())
    }

    /// `startDBAliveCheck` / `startCampaignInfoGet` (no gate).
    fn start_db_slot(&mut self, op: DbOp) -> StartResult {
        let _ = self.resource_ok();
        if self.db_slot.handle {
            return Err(StartError::Busy);
        }
        if !self.board_set {
            return Err(StartError::NotIdle);
        }
        if self.terminating {
            return Err(StartError::Refused);
        }
        let server = self.manager.resolved();
        self.db_slot.begin(op, server);
        Ok(())
    }

    /// The three log starts (C 9.1, 9.5).
    fn start_log_slot(&mut self, log_type: u32, op: DbOp) -> StartResult {
        let _ = self.resource_ok();
        let allowed = match log_type {
            1 => self.log_type <= 1,
            2 => self.log_type & 0xFFFD == 0,
            _ => matches!(self.log_type, 0 | 3),
        };
        if !allowed {
            return Err(StartError::LogTypeConflict);
        }
        if self.log_slot.handle {
            return Err(StartError::Busy);
        }
        if !self.board_set {
            return Err(StartError::NotIdle);
        }
        if self.terminating {
            return Err(StartError::Refused);
        }
        let server = self.manager.resolved();
        self.log_slot.begin(op, server);
        self.log_type = log_type;
        Ok(())
    }

    /// `startNFCFirmUpdater` (`list`: `\n`-separated paths, `None` = the three default `./firm/` files) or, with
    /// `led`, `startLedFirmUpdater` (`list` = the one path, default `./firm/837_15084_1_01.hex`). The files are read
    /// through `paths`. Result 8 (refused) is `Err(StartError::Refused)`.
    pub fn start_firmware(&mut self, paths: &dyn Fs, list: Option<&str>, force: bool, led: bool) -> StartResult {
        self.gate()?;
        if self.terminating {
            return Err(StartError::Refused);
        }
        let Some(reader) = self.reader.as_mut() else {
            return Err(StartError::Refused);
        };
        let read = |path: &str| std::fs::read(paths.map(path)).ok();
        if led {
            let path = list.unwrap_or(DEFAULT_LED_FIRMWARE);
            let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
            let Some(hex) = read(path) else {
                return Err(StartError::Refused);
            };
            reader.load_led_firmware(name, &hex, force).map_err(|_| StartError::Refused)?;
            self.cancel_exec = false;
            self.set_status(st::LED_FW_START);
            return Ok(());
        }
        let mut files = FirmwareFiles::default();
        let owned: Vec<String> = match list {
            Some(text) => text.split('\n').filter(|part| !part.is_empty()).map(str::to_owned).collect(),
            None => DEFAULT_NFC_FIRMWARE.iter().map(|path| (*path).to_owned()).collect(),
        };
        for path in &owned {
            let Some(bytes) = read(path) else { continue };
            if path.ends_with(".hex") {
                files.hex = Some(bytes);
            } else if path.contains("15396") {
                files.bin_gen3 = Some(bytes);
            } else if path.contains("15286") || files.bin_gen2.is_none() {
                files.bin_gen2 = Some(bytes);
            } else {
                files.bin_gen3 = Some(bytes);
            }
        }
        reader.load_firmware(&files, force).map_err(|_| StartError::Refused)?;
        self.cancel_exec = false;
        self.set_status(st::NFC_FW_START);
        Ok(())
    }

    // -----------------------------------------------------------------------------------------------------
    // Polling
    // -----------------------------------------------------------------------------------------------------

    /// One tick of a poll wrapper; `true` = finished (C 9.2).
    pub fn poll(&mut self, kind: PollKind, now_ms: u64) -> bool {
        if self.terminating {
            return false;
        }
        self.pump_slots();
        if self.cancel_loop {
            self.cancel_tick(now_ms);
            return false;
        }
        match kind {
            PollKind::UnitCheck => self.machine(4, now_ms),
            PollKind::Scan => self.machine(5, now_ms),
            PollKind::MailerOpen => self.machine(1, now_ms),
            PollKind::WebOpen => self.machine(2, now_ms),
            PollKind::AmiiboInfoGet => self.machine(3, now_ms),
            PollKind::AccountCheck => self.machine(7, now_ms),
            PollKind::FelicaIdCheck => self.machine(8, now_ms),
            PollKind::AccountScan => self.machine(9, now_ms),
            PollKind::FelicaIdGet => self.machine(10, now_ms),
            PollKind::FelicaIdRegist => self.machine(11, now_ms),
            PollKind::AccountGetex => {
                let mode = if matches!(self.mode, 12 | 18) { self.mode } else { 12 };
                self.machine(mode, now_ms)
            }
            PollKind::AccountRegist => self.machine(13, now_ms),
            PollKind::AccountGet => {
                if matches!(self.mode, 17 | 18) {
                    self.machine(self.mode, now_ms)
                } else {
                    true
                }
            }
            PollKind::AccountGetS => self.machine(18, now_ms),
            PollKind::CampaignClearInfoGet => self.machine(16, now_ms),
            PollKind::KeepResolveServer => self.server_rec.is_some() || self.machine(15, now_ms),
            PollKind::CampaignInfoGet | PollKind::DbAliveCheck => self.reap_db_slot(),
            PollKind::SendLogStatus => self.reap_log_slot(1),
            PollKind::SendAimeLog => self.reap_log_slot(2),
            PollKind::SendAimeLogex => self.reap_log_slot(3),
            PollKind::NfcFirmware => self.firmware_poll(now_ms, false),
            PollKind::LedFirmware => self.firmware_poll(now_ms, true),
        }
    }

    fn finish(&mut self) -> bool {
        self.mode = 0;
        self.status = st::STOP;
        true
    }

    /// `0x140327860`: the tick prologue (C 6.3) and the status switch.
    fn machine(&mut self, mode: u32, now: u64) -> bool {
        if mode != self.mode
            && self.mode == 0
            && matches!(self.status_snap, st::STOP | st::LED_FW_DONE | st::NFC_FW_DONE)
        {
            return self.finish();
        }
        if !self.board_set && mode > 5 {
            self.r.error = InternalCode::BOARD_INFO_NOT_SET;
            return self.finish();
        }
        self.status_snap = self.status;
        self.step(mode, now)
    }

    fn step(&mut self, mode: u32, now: u64) -> bool {
        // States 12 stays while a worker holds the DB (C 6.4, 7.2); it keeps its "first tick" for later.
        if self.status == st::DB_INIT && (self.db_slot.active() || self.log_slot.active()) {
            return false;
        }
        let first = std::mem::take(&mut self.status_changed);
        match self.status {
            st::NFC_INIT | st::NFC_INIT_ALIAS => self.state_init(mode, first, now),
            st::CARD_POLL => self.state_card_poll(mode, first, now),
            st::STOP_RW => {
                self.set_status(if matches!(mode, 8 | 9) { st::DB_INIT } else { st::STOP });
            }
            st::INTERRUPTED => {
                if self.r.error.is_ok() {
                    self.r.error = InternalCode::NFC_ACCESS_ERROR;
                }
                self.set_status(st::STOP);
            }
            st::DB_INIT => self.state_db_init(mode, first),
            st::DB_RESOLVE => self.state_db_resolve(mode, first),
            st::CHECK_FELICA_ID..=st::ACCOUNT_GET_S => self.state_db_op(first),
            st::DB_CANCEL_WAIT => self.state_cancel_wait(first),
            st::LED_OK | st::LED_NG => self.set_status(st::STOP),
            st::STOP => return self.state_stop(),
            _ => {}
        }
        false
    }

    fn state_init(&mut self, mode: u32, first: bool, now: u64) {
        let Some(reader) = self.reader.as_mut() else {
            self.r.error = InternalCode::NFC_INIT_FAULT;
            self.set_status(st::STOP);
            return;
        };
        if first {
            if mode == 3 {
                reader.set_amiibo_mode(true);
            }
            return;
        }
        match reader.init_step(now) {
            InitStep::Running => {}
            InitStep::Interrupted => self.set_status(st::INTERRUPTED),
            InitStep::Failed => {
                self.r.error = InternalCode::NFC_INIT_FAULT;
                self.set_status(st::STOP);
            }
            InitStep::Done => {
                self.copy_reader_info();
                if mode == 4 {
                    self.r.error = InternalCode::NONE;
                    self.set_status(st::STOP);
                } else {
                    self.set_status(st::CARD_POLL);
                }
            }
        }
    }

    fn copy_reader_info(&mut self) {
        let Some(reader) = self.reader.as_ref() else { return };
        let info = reader.info();
        self.fw_kind = info.fw_kind;
        self.strings = ReaderStrings {
            hw: info.hw_version,
            fw: info.fw_version,
            led_fw: info.led_board_name,
            led_version: info.led_fw_version,
            led_present: info.led_present,
            generation: info.generation,
        };
    }

    fn clear_unit_error(&mut self) {
        if let Some(reader) = self.reader.as_mut() {
            reader.clear_unit_error();
        }
    }

    fn state_card_poll(&mut self, mode: u32, first: bool, now: u64) {
        let timeout_ms = 1000 * u64::from(self.scan_timeout_sec);
        let filter = self.scan_filter;
        let follow_up = if matches!(mode, 1 | 2) { self.follow_up.take() } else { None };
        let Some(reader) = self.reader.as_mut() else {
            self.r.error = InternalCode::NFC_INIT_FAULT;
            self.set_status(st::STOP);
            return;
        };
        if first {
            if matches!(mode, 1 | 2) {
                let refused = match follow_up {
                    Some(action) => reader.set_follow_up(action).is_err(),
                    None => true,
                };
                if refused {
                    self.r.error = InternalCode::NFC_ACCESS_ERROR;
                    self.set_status(st::STOP);
                    return;
                }
            }
            reader.start_polling(filter, timeout_ms, now);
            return;
        }
        match reader.poll_step(now) {
            PollStep::Running => {}
            PollStep::Stopped => self.set_status(st::INTERRUPTED),
            PollStep::UnitError => {
                self.r.error = InternalCode::CARD_DETECT_ERROR;
                self.set_status(st::LED_NG);
            }
            PollStep::Finished => self.on_card(mode),
        }
    }

    /// The card decision of state 9 after a finished scan (C 6.4).
    fn on_card(&mut self, mode: u32) {
        let Some(reader) = self.reader.as_ref() else { return };
        let scan = reader.scan();
        match scan.card_count {
            0 => {
                self.r.error = InternalCode::CARD_DETECT_TIMEOUT;
                self.clear_unit_error();
                self.set_status(st::STOP);
                return;
            }
            1 => {}
            _ => {
                self.r.error = InternalCode::TWO_CARDS;
                self.set_status(st::LED_NG);
                return;
            }
        }
        let Some(card) = scan.cards.first().filter(|card| card.valid) else {
            self.r.error = InternalCode::INCOMPATIBLE_CARD;
            self.clear_unit_error();
            self.set_status(st::LED_NG);
            return;
        };
        let r = &mut self.r;
        r.card_code = card.uid;
        r.game_code = card.name;
        r.card_value = card.value;
        r.card_uid = card.block0;
        r.card_kind = match card.kind {
            CardKind::None => 0,
            CardKind::Mifare => 1,
            CardKind::FeliCa => 2,
            CardKind::Amiibo => 3,
            CardKind::DetectFailure => 4,
        };
        let amiibo = |r: &mut ResultBlock| {
            let word = |at: usize| {
                u32::from_le_bytes([
                    card.uid_copy[at],
                    card.uid_copy[at + 1],
                    card.uid_copy[at + 2],
                    card.uid_copy[at + 3],
                ])
            };
            r.amiibo = [
                word(0),
                word(4),
                card.amiibo.head,
                u32::from(card.amiibo.byte3),
                u32::from(card.amiibo.word) | u32::from(card.amiibo.byte6) << 16,
            ];
        };
        match card.kind {
            CardKind::Amiibo => {
                amiibo(r);
                self.clear_unit_error();
                self.set_status(st::STOP);
                return;
            }
            CardKind::Mifare => {
                if mode == 8 {
                    r.error = InternalCode::NOT_DETECT_FELICA;
                    self.set_status(st::LED_NG);
                    return;
                }
                if mode == 3 {
                    amiibo(r);
                    self.clear_unit_error();
                    self.set_status(st::STOP);
                    return;
                }
                r.access_code = card.access_code;
                r.aux = scan.auth_kind;
            }
            CardKind::FeliCa => {
                r.flags |= flags::FELICA;
                r.felica_idm.copy_from_slice(&card.idm_pmm[..8]);
                r.felica_pmm.copy_from_slice(&card.idm_pmm[8..]);
                if scan.felica_error == 1 {
                    r.flags |= flags::MOBILE;
                } else if scan.felica_error == 14 {
                    r.aux = 0;
                    r.error = InternalCode::FELICA_CONTROL_ERROR;
                } else {
                    match scan.felica_kind.unwrap_or(FelicaKind::Other(0)) {
                        FelicaKind::LiteS => {
                            r.flags |= flags::LITE_S;
                            let lite = &card.lite_s;
                            r.lite_s_extra = [0; 75];
                            r.lite_s_extra[1..17].copy_from_slice(&lite.rc);
                            for (index, block) in lite.blocks.iter().enumerate() {
                                r.lite_s_extra[17 + 16 * index..33 + 16 * index].copy_from_slice(block);
                            }
                            r.lite_s_extra[65..73].copy_from_slice(&lite.block91);
                            r.lite_s_extra[73..75].copy_from_slice(&lite.dfc);
                            r.access_code2 = lite.value;
                        }
                        kind @ (FelicaKind::Lite | FelicaKind::Standard) => {
                            r.flags |=
                                if kind == FelicaKind::Lite { flags::FELICA_LITE } else { flags::FELICA_STANDARD };
                            r.access_code = card.access_code;
                            r.aux = scan.auth_kind;
                        }
                        FelicaKind::Mobile(_) => {
                            r.flags |= flags::MOBILE;
                            r.aux = 0;
                        }
                        FelicaKind::Other(_) => {
                            r.flags |= flags::UNSUPPORTED;
                            r.aux = 0;
                            r.error = InternalCode::INCOMPATIBLE_CARD;
                        }
                    }
                }
            }
            CardKind::None | CardKind::DetectFailure => {
                r.error = InternalCode::INCOMPATIBLE_CARD;
                self.clear_unit_error();
                self.set_status(st::LED_NG);
                return;
            }
        }
        if scan.felica_error > 1 {
            r.aux = 0;
            r.error = InternalCode::INCOMPATIBLE_CARD;
        }
        let failed = !r.error.is_ok();
        self.clear_unit_error();
        if mode == 5 {
            self.set_status(if failed { st::LED_NG } else { st::STOP });
        } else {
            self.set_status(st::STOP_RW);
        }
    }

    // ---- DB states ----------------------------------------------------------------------------------------

    fn params(&self) -> DbParams {
        DbParams::new(&self.board.game_id, &self.board.serial, self.board.arg, self.fw_kind)
    }

    fn state_db_init(&mut self, mode: u32, first: bool) {
        if first {
            self.db = Some(DbSession { server: None, resolving: None, job: None });
            return;
        }
        if let Some(addr) = self.server_rec {
            if let Some(db) = self.db.as_mut() {
                db.server = Some(addr);
            }
            self.after_db_ready(mode);
        } else {
            self.set_status(st::DB_RESOLVE);
        }
    }

    fn state_db_resolve(&mut self, mode: u32, first: bool) {
        let transport = Arc::clone(&self.transport);
        let Some(db) = self.db.as_mut() else {
            self.r.error = InternalCode::SERVER_ERROR;
            self.set_status(st::DB_CANCEL_WAIT);
            return;
        };
        if first {
            db.resolving = Some(db::resolve(&transport));
            return;
        }
        let Some(pending) = db.resolving.as_mut() else { return };
        if !pending.is_done() {
            return;
        }
        let result = pending.take().unwrap_or(Err(InternalCode::RESOLVE_FAILED));
        db.resolving = None;
        match result {
            Err(code) => {
                self.r.error = code;
                self.set_status(st::DB_CANCEL_WAIT);
            }
            Ok(addr) => {
                db.server = Some(addr);
                if self.keep_resolved && self.manager.resolved() != Some(addr) {
                    self.manager.set_resolved(Some(addr));
                }
                self.after_db_ready(mode);
            }
        }
    }

    /// The mode switch after states 12 and 13 (C 6.4).
    fn after_db_ready(&mut self, mode: u32) {
        let next = match mode {
            10 => st::FELICA_ID_GET,
            11 => st::FELICA_ID_REGIST,
            12 => st::ACCOUNT_GETEX,
            13 => st::ACCOUNT_REGIST,
            16 => st::CAMPAIGN_CLEAR,
            17 => st::ACCOUNT_GET,
            18 => st::ACCOUNT_GET_S,
            _ => {
                let felica = self.r.flags & flags::FELICA != 0
                    && self.r.flags & (flags::FELICA_LITE | flags::FELICA_STANDARD) == 0;
                if felica { st::CHECK_FELICA_ID } else { st::CHECK_AIME_UID }
            }
        };
        self.set_status(next);
    }

    fn aime_req(&self) -> AimeReq {
        AimeReq { access_code: self.r.access_code, aux: self.r.aux, card_code: self.r.card_code }
    }

    fn state_op(&self) -> DbOp {
        let (idm, pmm) = (self.r.felica_idm, self.r.felica_pmm);
        match self.status {
            st::CHECK_FELICA_ID => DbOp::FelicaCheck { idm, pmm, register_missing: !self.check_only },
            st::CHECK_AIME_UID => DbOp::AimeCheck { req: self.aime_req(), register_missing: !self.check_only },
            st::FELICA_ID_GET => DbOp::FelicaGet { idm, pmm },
            st::FELICA_ID_REGIST => DbOp::FelicaRegist { idm, pmm },
            st::ACCOUNT_GETEX => DbOp::AimeGetex(self.aime_req()),
            st::ACCOUNT_REGIST => DbOp::AimeRegist(self.aime_req()),
            st::CAMPAIGN_CLEAR => DbOp::CampaignClear(self.arg1),
            st::ACCOUNT_GET => DbOp::AimeGet(self.aime_req()),
            _ => DbOp::AimeGetS { extra: self.r.lite_s_extra, aux: self.r.aux },
        }
    }

    fn state_db_op(&mut self, first: bool) {
        if first {
            let op = self.state_op();
            let server = self.db.as_ref().and_then(|db| db.server);
            let job = db::spawn(db::job(Arc::clone(&self.transport), server, self.params(), op));
            if let Some(db) = self.db.as_mut() {
                db.job = Some(job);
            }
            return;
        }
        let Some(pending) = self.db.as_mut().and_then(|db| db.job.as_mut()) else { return };
        if !pending.is_done() {
            return;
        }
        let output =
            pending.take().unwrap_or_else(|| DbOutput { code: InternalCode::SERVER_ERROR, ..DbOutput::default() });
        if let Some(db) = self.db.as_mut() {
            db.job = None;
        }
        self.apply(output);
    }

    fn forget_resolved(&mut self) {
        self.manager.set_resolved(None);
    }

    fn copy_reply(&mut self, output: &DbOutput, full: bool) {
        let Some(reply) = output.reply.as_ref() else { return };
        self.r.aime_id = reply.aime_id;
        self.r.aime_found = reply.found;
        if full {
            self.r.related_aime_ids = reply.related;
            self.r.sega_id = reply.sega_id;
        }
    }

    /// Applies a finished state operation to `R` and picks the next status (C 6.4).
    fn apply(&mut self, output: DbOutput) {
        let code = output.code;
        self.r.error = code;
        self.r.flags |= output.flags;
        match self.status {
            st::CHECK_FELICA_ID => {
                if let Some(access) = output.access_code {
                    self.r.access_code = access;
                }
                if !code.is_ok() {
                    self.forget_resolved();
                    self.set_status(st::DB_CANCEL_WAIT);
                } else if proto::is_bcd(&self.r.access_code) {
                    self.set_status(st::CHECK_AIME_UID);
                } else {
                    self.set_status(st::DB_CANCEL_WAIT);
                }
            }
            st::CHECK_AIME_UID => {
                if code.is_ok() {
                    self.copy_reply(&output, false);
                }
                self.set_status(st::STOP);
            }
            st::FELICA_ID_GET | st::FELICA_ID_REGIST => {
                if let Some(access) = output.access_code {
                    self.r.access_code = access;
                }
                if !code.is_ok() {
                    self.forget_resolved();
                }
                self.set_status(st::STOP);
            }
            st::ACCOUNT_GETEX | st::ACCOUNT_REGIST | st::ACCOUNT_GET | st::ACCOUNT_GET_S => {
                self.copy_reply(&output, matches!(self.status, st::ACCOUNT_GET | st::ACCOUNT_GET_S));
                if self.status == st::ACCOUNT_GET_S
                    && let Some(reply) = output.reply.as_ref()
                {
                    if let Some(aux) = reply.aux {
                        self.r.aux = aux;
                    }
                    if let Some(access) = reply.access_code {
                        self.r.access_code = access;
                    }
                }
                let lookup = self.status != st::ACCOUNT_REGIST;
                if code == InternalCode::AIME_ID_NOT_FOUND && lookup {
                    self.set_status(st::STOP);
                } else if code.is_ok() {
                    self.set_status(st::LED_OK);
                } else {
                    self.forget_resolved();
                    self.set_status(st::LED_NG);
                }
            }
            st::CAMPAIGN_CLEAR => {
                if !code.is_ok() {
                    self.forget_resolved();
                }
                if let Some(entries) = output.campaign_clear {
                    self.r.campaign_clear = entries;
                }
                self.set_status(st::STOP);
            }
            _ => self.set_status(st::STOP),
        }
    }

    fn state_cancel_wait(&mut self, first: bool) {
        if first {
            if let Some(db) = self.db.as_ref() {
                if let Some(job) = db.job.as_ref() {
                    job.abort();
                }
                if let Some(job) = db.resolving.as_ref() {
                    job.abort();
                }
            }
            return;
        }
        if let Some(db) = self.db.as_mut() {
            if db.job.as_mut().is_some_and(|job| !job.is_done()) {
                return;
            }
            db.job = None;
            if db.resolving.as_mut().is_some_and(|job| !job.is_done()) {
                return;
            }
            db.resolving = None;
        }
        if self.r.error.is_ok() {
            self.r.error = InternalCode::SERVER_ERROR;
        }
        self.set_status(st::STOP);
    }

    /// Status 28 (C 6.4): drop the DB client, ask the reader to stop; finished once both are gone.
    fn state_stop(&mut self) -> bool {
        self.status_changed = false;
        if let Some(db) = self.db.as_mut() {
            if db.job.as_mut().is_some_and(|job| !job.is_done())
                || db.resolving.as_mut().is_some_and(|job| !job.is_done())
            {
                return false;
            }
            self.db = None;
        }
        let busy = self.reader.as_ref().is_some_and(Reader::busy);
        if busy {
            if let Some(reader) = self.reader.as_mut() {
                reader.request_stop();
            }
            return false;
        }
        self.finish()
    }

    // ---- worker slots ---------------------------------------------------------------------------------------

    /// Starts queued slot workers whose condition holds and completes finished ones (C 7.2).
    fn pump_slots(&mut self) {
        let status = self.status;
        if self.db_slot.running.is_none()
            && self.db_slot.queued.is_some()
            && !(st::DB_INIT..=st::ACCOUNT_GET_S).contains(&status)
        {
            self.launch(false);
        }
        let db_ok = !self.db_slot.handle || self.db_slot.done;
        if self.log_slot.running.is_none()
            && self.log_slot.queued.is_some()
            && !(st::DB_INIT..23).contains(&status)
            && db_ok
        {
            self.launch(true);
        }
        for log in [false, true] {
            self.complete(log);
        }
    }

    fn launch(&mut self, log: bool) {
        let params = self.params();
        let transport = Arc::clone(&self.transport);
        let slot = if log { &mut self.log_slot } else { &mut self.db_slot };
        let Some(op) = slot.queued.take() else { return };
        let pending = db::spawn(db::job(transport, slot.server, params, op.clone()));
        slot.running = Some((op, pending));
    }

    fn complete(&mut self, log: bool) {
        let slot = if log { &mut self.log_slot } else { &mut self.db_slot };
        let Some((op, pending)) = slot.running.as_mut() else { return };
        if !pending.is_done() {
            return;
        }
        let output =
            pending.take().unwrap_or_else(|| DbOutput { code: InternalCode::SERVER_ERROR, ..DbOutput::default() });
        let op = op.clone();
        slot.running = None;
        slot.done = true;
        slot.result = output.code;
        if let Some(addr) = output.resolved
            && self.keep_resolved
        {
            self.manager.set_resolved(Some(addr));
        }
        if !output.code.is_ok() {
            self.manager.set_resolved(None);
        }
        match op {
            DbOp::Alive => self.db_alive = output.code.is_ok(),
            DbOp::CampaignInfo => {
                if let Some(entries) = output.campaign_info {
                    self.r.campaign_info = entries;
                }
            }
            DbOp::MultiLog(_) => self.log_results = output.log_bytes.clone(),
            _ => {}
        }
    }

    fn reap_db_slot(&mut self) -> bool {
        if self.db_slot.done && self.db_slot.handle {
            self.db_slot.handle = false;
            self.r.db_thread_error = self.db_slot.result;
        }
        self.db_slot.done
    }

    fn reap_log_slot(&mut self, log_type: u32) -> bool {
        if self.log_type != log_type {
            return true;
        }
        if self.log_slot.done && self.log_slot.handle {
            self.log_slot.handle = false;
            self.r.log_thread_error = self.log_slot.result;
        }
        self.log_slot.done
    }

    // ---- firmware pollers -----------------------------------------------------------------------------------

    fn firmware_poll(&mut self, now: u64, led: bool) -> bool {
        let (start, run, done) = if led {
            (st::LED_FW_START, st::LED_FW_RUN, st::LED_FW_DONE)
        } else {
            (st::NFC_FW_START, st::NFC_FW_RUN, st::NFC_FW_DONE)
        };
        let Some(reader) = self.reader.as_mut() else { return true };
        if self.status == start {
            self.status = run;
        } else if self.status == run {
            let error = if led {
                match reader.led_firmware_step(now) {
                    LedFirmwareStep::Running => None,
                    LedFirmwareStep::Done => Some(InternalCode::NONE),
                    LedFirmwareStep::Failed => Some(InternalCode::FIRMUP_ERROR),
                }
            } else {
                match reader.firmware_step(now) {
                    FirmwareStep::Running => None,
                    FirmwareStep::Done | FirmwareStep::AlreadyNew => Some(InternalCode::NONE),
                    FirmwareStep::Failed => Some(InternalCode::FIRMUP_ERROR),
                    FirmwareStep::VersionFault => Some(InternalCode::NFC_INIT_FAULT),
                }
            };
            match error {
                None => self.fw_updating = true,
                Some(code) => {
                    self.copy_reader_info();
                    self.status = done;
                    self.r.error = code;
                }
            }
        } else if self.status == done {
            self.fw_updating = false;
            return true;
        }
        false
    }

    // -----------------------------------------------------------------------------------------------------
    // Cancel, resolve, terminate
    // -----------------------------------------------------------------------------------------------------

    /// `cancel` (`0x1403273F0`, C 8).
    pub fn cancel(&mut self, _now_ms: u64) {
        if self.cancel_exec {
            return;
        }
        self.cancel_exec = true;
        match self.status_snap {
            st::IDLE | st::STOP => {
                self.r.error =
                    if self.status < st::DB_INIT { InternalCode::NFC_ACCESS_ERROR } else { InternalCode::SERVER_ERROR };
                self.set_status(st::STOP);
                return;
            }
            st::NFC_FW_START..=st::LED_FW_DONE => return,
            st::NFC_INIT | st::NFC_INIT_ALIAS | st::CARD_POLL => match self.reader.as_mut() {
                None => {
                    self.r.error = InternalCode::NFC_ACCESS_ERROR;
                    self.set_status(st::STOP);
                }
                Some(reader) => reader.request_stop(),
            },
            st::DB_INIT..=st::ACCOUNT_GET_S => {
                self.r.error = InternalCode::SERVER_ERROR;
                self.set_status(st::DB_CANCEL_WAIT);
            }
            _ => {}
        }
        if !self.terminating {
            self.cancel_loop = true;
            self.cancel_ticks = 0;
        }
    }

    /// One tick of the cancel loop (`0x140334000`): the machine runs with `impl.mode` until it finishes.
    fn cancel_tick(&mut self, now: u64) {
        self.cancel_ticks += 1;
        let mode = self.mode;
        if self.machine(mode, now) || self.cancel_ticks >= CANCEL_TICKS {
            self.cancel_loop = false;
        }
    }

    /// `clearResolve` (`0x140329E30`): forgets the shared resolved server.
    pub fn clear_resolve(&mut self) {
        self.manager.set_resolved(None);
        self.server_rec = None;
        self.keep_resolved = false;
    }

    /// Destructor / terminate (`0x140332710`): returns the original's bit mask, 0 = clean.
    pub fn terminate(&mut self, now_ms: u64) -> u32 {
        self.terminating = true;
        let mut mask = 0;
        for (log, bit) in [(false, 0x2), (true, 0x8)] {
            let slot = if log { &mut self.log_slot } else { &mut self.db_slot };
            slot.queued = None;
            if let Some((_, pending)) = slot.running.as_mut() {
                pending.abort();
                if !pending.wait(THREAD_WAIT) {
                    mask |= bit;
                }
            }
            slot.running = None;
        }
        if let Some(db) = self.db.take() {
            if let Some(mut job) = db.job {
                job.abort();
                if !job.wait(THREAD_WAIT) {
                    mask |= 0x100;
                }
            }
            if let Some(job) = db.resolving.as_ref() {
                job.abort();
            }
        }
        if let Some(reader) = self.reader.as_mut() {
            reader.terminate(now_ms);
        }
        if self.generation != self.manager.generation() {
            mask |= 0x400;
        }
        mask
    }

    // -----------------------------------------------------------------------------------------------------
    // Getters
    // -----------------------------------------------------------------------------------------------------

    /// The result block `R` (`getR`).
    pub fn result(&self) -> ResultBlock {
        self.r.clone()
    }

    /// The per-entry results of the last `SendAimeLogex` (non-zero = the server accepted the entry).
    pub fn log_results(&self) -> Vec<u8> {
        self.log_results.clone()
    }

    /// The reader strings copied after a successful init.
    pub fn reader_strings(&self) -> ReaderStrings {
        self.strings.clone()
    }

    /// NFC firmware update progress 0..=100 (`0x14032AA50`).
    pub fn firmware_progress(&self) -> f32 {
        match self.status {
            st::NFC_FW_DONE => 100.0,
            st::NFC_FW_RUN => {
                let Some(reader) = self.reader.as_ref() else { return 0.0 };
                let (current, total) = reader.firmware_progress();
                let generation = reader.info().generation;
                if total == 0 && generation <= 1 {
                    return 0.0;
                }
                let denominator = if generation > 1 { 48.0 } else { total as f32 };
                current as f32 / denominator * 100.0
            }
            _ => 0.0,
        }
    }

    /// `kSTATE` (`impl+2232`), 0..=28.
    pub fn status(&self) -> u32 {
        self.status
    }

    /// `kPOLL` mode (`impl+1216`), 0 = idle.
    pub fn mode(&self) -> u32 {
        self.mode
    }

    /// `setLedColor` (`0x14032D6F0`): `true` if a packet was sent.
    pub fn set_led_color(&mut self, rgb: [u8; 3]) -> bool {
        let Some(reader) = self.reader.as_mut() else { return false };
        let present = reader.info().led_present;
        present && reader.set_led_colour(rgb).is_ok()
    }

    /// `setLedPattern` (`0x14032D7D0`): `true` if a packet was sent.
    pub fn set_led_pattern(&mut self, index: u8) -> bool {
        let Some(reader) = self.reader.as_mut() else { return false };
        let present = reader.info().led_present;
        present && reader.set_led_pattern(index).is_ok()
    }

    /// The unit idle check (`0x14032BF40`): no queued reader command.
    pub fn is_unit_idle(&self) -> bool {
        self.reader.as_ref().is_none_or(|reader| !reader.busy())
    }

    /// `true` after the last alive check (cmd 100/101) the server answered with status 1.
    pub fn db_alive(&self) -> bool {
        self.db_alive
    }
}
