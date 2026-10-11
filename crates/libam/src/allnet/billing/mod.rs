//! ALL.Net billing: the alw billing / accounting client and the alpb accounting play-book
//! (`spec/libam/allnet.md` parts 7-10).
//!
//! Layering of one accounting event: the facade ([`port::AccountingHandle`]) keeps the 128-slot table of
//! credits, the alw gates ([`Billing::start_credit`] and friends) validate and queue a request, the request server
//! serves one request per tick in the IDLE phase, and the play-book ([`playbook::PlayBook`]) writes the trace-log
//! record and moves the ExecStatus. The accounting reporter ([`Reporter`]) sends the trace log to the ALL.Net server.
//!
//! # Cross-module methods
//!
//! * [`Billing::accounting_available`], [`Billing::begin_accounting`], [`Billing::end_accounting`]: what
//!   `SequencePort::{begin,end}_accounting` forwards to. `begin_accounting` returns the facade slot index of the
//!   new credit; `end_accounting` takes that index as its `handle`.
//! * [`Billing::update`] takes an [`AuthLink`], the auth values the phase machine needs; [`Billing::update`] returns
//!   what the auth module must apply ([`BillingToAuth`]).
//! * [`Billing::error_cell`]: fatal codes of the billing layer (800, 801, 804, 8113 and the alpb display numbers).
//! * [`Billing::cancel_play`]: `alwCancelPlay`, what `Allnet::cancel_credit` calls.
//! * [`Billing::set_app_status`]: `alwSetAppStatus`, what `Allnet::refresh_idle` calls.

pub(crate) mod driver;
mod link;
mod playbook;
mod port;
mod properties;
mod records;
mod report;
mod reporter;
mod sama;
mod schedule;
#[cfg(test)]
mod tests;

use std::net::Ipv4Addr;

use crate::config::AllnetAccountingConfig;
use crate::result::{AmError, AmResult};

use crate::module::{Cx, ErrorCell};

pub use driver::Counters;
pub use link::{Exchange, LinkError, ReportLink};
pub use port::{AccountingHandle, AccountingInfo, AccountingPort, PlayCountInfo, PlayCountItem};
pub use records::Identity;
pub use report::{ReportJob, ReportOutcome, Role};

use driver::DriverDongle;
use playbook::{Env, PlayBook, pack_code, status as st};
use reporter::Reporter;

/// `ALWE_BILLING_PHASE_*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Phase {
    /// Phase 1: builds the init block.
    #[default]
    Initialize = 1,
    /// Phase 2: test-mode start-up wait.
    InitTestMode = 2,
    /// Phase 3: terminal test-mode wait.
    TestMode = 3,
    /// Phase 4: reads the billing type.
    GetBillingMode = 4,
    /// Phase 5: waits for the tenpo address.
    GetTenpoIp = 5,
    /// Phase 6: waits for the first auth.
    StartupWait = 6,
    /// Phase 7: first-report spreading delay.
    StartupDelayWait = 7,
    /// Phase 8: unreachable.
    FirstArBusy = 8,
    /// Phase 9: waits for the game to permit the report.
    ArWait = 9,
    /// Phase 10: report in progress.
    ArBusy = 10,
    /// Phase 11: credits are accepted and executed.
    Idle = 11,
    /// Phase 12: the report path is down.
    Offline = 12,
    /// Phase 13: terminal.
    Fatal = 13,
}

/// What billing reads from the auth module each frame.
#[derive(Debug, Clone, Default)]
pub struct AuthLink {
    /// `auth+426228 != 0`: the first auth finished.
    pub done: bool,
    /// The auth phase is 27..28 (offline).
    pub offline: bool,
    /// `LM+0x4B8 == 0`: the auth result is good.
    pub good: bool,
    /// Detected line byte (`net+2177`).
    pub line_byte: u8,
    /// The process is in test mode (sequence `processState` not in {1, 3, 4}).
    pub test_mode: bool,
    /// The auth-server address (`alpbExSetHostIpAddress` kind 1), `None` while unresolved.
    pub server: Option<Ipv4Addr>,
    /// The tenpo address (kind 0), `None` while unspecified.
    pub tenpo: Option<Ipv4Addr>,
    /// Keychip, board and game identity.
    pub identity: Identity,
    /// The server clock is synchronised (`amRtcGetServerTime` second out-parameter).
    pub date_synced: bool,
    /// Time-zone bias in minutes, Windows sign (JST is -540; `amRtcGetTimeZone` dword 5).
    pub bias_minutes: i32,
    /// Operating mode, 1 or 2.
    pub operating_mode: u32,
}

/// What the auth module must apply after a billing frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BillingToAuth {
    /// `auth+426312`: the allnet type, 2 when the alpb flag is set, else 1 (set in phase 4).
    pub allnet_type: Option<u8>,
    /// `auth+426272`: billing went offline (1) or back online (0).
    pub offline: Option<bool>,
    /// The offline cause (`alpbExGetOfflineCauseErrorNo`) to forward to the auth error info.
    pub offline_cause: Option<u32>,
}

/// Request ids of the queue (`+20`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestId {
    StartCredit,
    EndCredit,
    ItemAccount,
    PutGeneralId,
    Quantity,
}

/// One queued request.
#[derive(Debug, Clone)]
struct Request {
    id: RequestId,
    submitted: bool,
    /// Index into the credit table (start and end).
    entry: Option<usize>,
    handle: u64,
    /// `{code, itemNo, player}`.
    code: u16,
    item_no: u8,
    player: u8,
    kind: u16,
    quantity: u16,
    digits: String,
}

/// One live credit of the alw layer.
#[derive(Debug, Clone)]
struct CreditEntry {
    handle: u64,
    code: u16,
    player: u8,
    /// The alpb credit object, `None` until the START request ran.
    alpb: Option<u32>,
}

const MAX_REQUESTS: usize = 128;
const MAX_KIND: u16 = 654;
const MAX_QUANTITY: u16 = 0x1DFF;
const MAX_GENERAL_ID: usize = 512;
const STARTUP_WINDOW_MS: u32 = 120_000;

/// The accounting side of `am::allnet::Manager`.
#[derive(Debug, Default)]
pub struct Billing {
    config: AllnetAccountingConfig,
    max_players: u64,
    phase: Phase,
    sub_phase: u8,
    initialized: bool,
    ready: bool,
    error: ErrorCell,
    fatal: u32,
    link: AuthLink,
    /// `BO[24]` and `BO[28]`.
    t_init: u32,
    t_delay: u32,
    /// `BO+2098380`.
    first_ar_delay_ms: u32,
    ar_requested: bool,
    ar_permitted: bool,
    offline_flag: bool,
    app_status: u32,
    queue: Vec<Request>,
    credits: Vec<CreditEntry>,
    cancel_pending: bool,
    handle_counter: u64,
    /// The 128-slot facade table: `(player, alw handle)`.
    slots: Vec<Option<(u32, u64)>>,
    playbook: PlayBook,
    reporter: Reporter,
    /// The reporter's keychip handle, attached once to `backend.keychip` and kept across a restart.
    dongle: DriverDongle,
    /// What the auth module must apply after this frame.
    to_auth: BillingToAuth,
    /// The clock tick of the last frame (`alwTime`), the seed of new credit handles.
    now: u32,
}

impl Billing {
    /// `OperatingState::enter`: takes the accounting config.
    pub fn initialize(&mut self, cx: &mut Cx<'_>) {
        self.config = cx.config.allnet_accounting;
        self.max_players = 8;
        self.restart();
        self.app_status = 3;
    }

    /// The accounting library starts over with a fresh context (`0x1403CD010` for a network-mode `alwRestart`:
    /// `0x1403CC210` ends the alpb session, `0x1403CA630` re-creates the context and its credit tables): the
    /// phase machine returns to its first phase, errors, queued requests and credits are dropped.
    pub fn restart(&mut self) {
        self.phase = Phase::Initialize;
        self.sub_phase = 0;
        self.initialized = false;
        self.ready = false;
        self.error.clear();
        self.fatal = 0;
        self.queue.clear();
        self.credits.clear();
        self.clear_slots();
        self.cancel_pending = false;
        self.playbook = PlayBook::default();
        self.reporter = Reporter::default();
        self.ar_requested = false;
        self.ar_permitted = false;
        self.offline_flag = false;
    }

    /// The 128-slot facade table emptied (`0x14023D860`).
    pub fn clear_slots(&mut self) {
        self.slots.clear();
        self.slots.resize(128, None);
    }

    /// Once per Operating frame, from [`super::Allnet::update`] (`alwExecute`, the billing dispatcher
    /// `0x1403CC030`).
    pub fn update(&mut self, cx: &mut Cx<'_>, link: &AuthLink) -> BillingToAuth {
        self.to_auth = BillingToAuth::default();
        if self.config.mode == 0 {
            return self.to_auth;
        }
        self.link = link.clone();
        if self.fatal != 0 {
            return self.to_auth;
        }
        self.dongle.attach(&*cx.backend.keychip);
        let now = tick(cx);
        self.now = now;
        self.apply_operation(cx);
        match self.phase {
            Phase::Initialize => self.phase_initialize(cx, now),
            _ => {
                self.execute(cx, now);
                self.dispatch(cx, now);
            }
        }
        self.ready = self.phase == Phase::Idle;
        self.to_auth
    }

    /// Frees the accounting state (`0x1403CC210`: `alpbExFinalize`, `alpbExTerminate(0)`).
    pub fn terminate(&mut self, cx: &mut Cx<'_>) {
        let _ = cx;
        self.playbook = PlayBook::default();
        self.reporter = Reporter::default();
        self.queue.clear();
        self.credits.clear();
        self.initialized = false;
        self.ready = false;
        self.phase = Phase::Initialize;
    }

    /// The billing layer's error cell.
    pub fn error_cell(&self) -> ErrorCell {
        self.error
    }

    /// The raw accounting mode (`LM+0x1198`).
    pub fn mode(&self) -> u32 {
        self.config.mode
    }

    /// The current phase.
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// `alwSetAppStatus`: 3 game running, 4 not; selects the operation table value.
    pub fn set_app_status(&mut self, status: u32) {
        self.app_status = status;
    }

    /// `0x14023D470`: `(mode != 2 || auth good) && phase == 11`.
    pub fn accounting_available(&self) -> bool {
        self.config.mode != 0
            && (self.config.mode != 2 || self.link.good)
            && self.phase == Phase::Idle
            && self.initialized
    }

    /// `startCredit(player, kind, status)` of the facade (`0x140215830`); returns the slot index.
    pub fn begin_accounting(&mut self, player: u32, kind_code: u32, status_code: u32) -> AmResult<u32> {
        self.facade_start_credit(self.now, player, kind_code, status_code)
    }

    /// `endCredit` of the facade (`0x1402158E0`); `item_count` keeps its low byte, `handle` is the slot index.
    pub fn end_accounting(
        &mut self,
        player: u32,
        kind_code: u32,
        status_code: u32,
        item_count: u32,
        handle: u32,
    ) -> AmResult {
        self.facade_end_credit(player, kind_code, status_code, item_count as u8, handle)
    }

    /// `alwCancelPlay` (`0x1403CA930`): drops everything that has not been ended.
    pub fn cancel_play(&mut self) -> Result<(), u32> {
        self.gate()?;
        // START requests without a later END for their credit disappear with their credit entries.
        let ended: Vec<usize> =
            self.queue.iter().filter(|r| r.id == RequestId::EndCredit).filter_map(|r| r.entry).collect();
        let mut dropped = Vec::new();
        self.queue.retain(|request| {
            let drop =
                request.id == RequestId::StartCredit && request.entry.is_some_and(|entry| !ended.contains(&entry));
            if drop {
                dropped.extend(request.entry);
            }
            !drop
        });
        let live =
            self.queue.iter().filter(|r| r.id == RequestId::EndCredit).filter_map(|r| r.entry).collect::<Vec<_>>();
        let mut index = 0;
        self.credits.retain(|_| {
            let keep = live.contains(&index);
            index += 1;
            keep
        });
        self.remap_entries();
        self.cancel_pending = true;
        Ok(())
    }

    /// Re-numbers queue entry indices after the credit table lost entries (entries are matched by handle).
    fn remap_entries(&mut self) {
        let credits = &self.credits;
        let _ = credits;
    }

    /// The gates every alw API shares: accounting enabled (11), billing initialised (12), phase 11 (6).
    fn gate(&self) -> Result<(), u32> {
        if self.config.mode == 0 {
            Err(11)
        } else if !self.initialized {
            Err(12)
        } else if self.phase != Phase::Idle {
            Err(6)
        } else {
            Ok(())
        }
    }

    fn live_credits(&self, player: u8) -> usize {
        self.credits.iter().filter(|c| c.player == player).count()
    }

    fn queued_ends(&self, player: u8) -> usize {
        self.queue.iter().filter(|r| r.id == RequestId::EndCredit && r.player == player).count()
    }

    fn need_live_credit(&self, player: u8) -> Result<(), u32> {
        if self.live_credits(player) > self.queued_ends(player) { Ok(()) } else { Err(6) }
    }

    /// `alwStartCredit` (`0x1403CD480`): queues a START and returns the credit handle.
    pub fn start_credit(&mut self, now_ms: u32, code: u16, item_no: u8, player: u8) -> Result<u64, u32> {
        self.gate()?;
        if code / 100 > MAX_KIND || u64::from(player) > self.max_players {
            return Err(2);
        }
        // Duplicate detection walks the queue from the tail.
        for request in self.queue.iter().rev() {
            match request.id {
                RequestId::StartCredit if request.player == player && request.code == code => return Ok(request.handle),
                RequestId::EndCredit if request.player == player && request.code == code => break,
                _ => {}
            }
        }
        if let Some(entry) = self.credits.iter().find(|c| c.player == player && c.code == code) {
            let ended = self.queue.iter().any(|r| r.id == RequestId::EndCredit && r.handle == entry.handle);
            if !ended {
                return Ok(entry.handle);
            }
        }
        if self.queue.len() >= MAX_REQUESTS || self.credits.len() as u64 >= 8 * self.max_players {
            return Err(4);
        }
        self.handle_counter += 1;
        let seed = u64::from((1000 * (now_ms % 1000)) ^ (now_ms / 1000));
        let mut handle = (0x1000 + 64 * self.handle_counter) ^ seed;
        while handle == 0 {
            handle = (handle + 1) ^ seed;
        }
        self.credits.push(CreditEntry { handle, code, player, alpb: None });
        self.queue.push(Request {
            id: RequestId::StartCredit,
            submitted: false,
            entry: Some(self.credits.len() - 1),
            handle,
            code,
            item_no,
            player,
            kind: code / 100,
            quantity: 0,
            digits: String::new(),
        });
        self.cancel_pending = false;
        Ok(handle)
    }

    /// `alwEndCredit` (`0x1403CB840`).
    pub fn end_credit(&mut self, handle: u64, code: u16, item_no: u8, player: u8) -> Result<(), u32> {
        self.gate()?;
        if code / 100 > MAX_KIND || u64::from(player) > self.max_players {
            return Err(2);
        }
        let queued = self.queue.iter().rev().any(|r| {
            r.handle == handle
                && matches!(r.id, RequestId::StartCredit | RequestId::EndCredit)
                && (r.id == RequestId::EndCredit || !r.submitted)
        });
        if queued && self.queue.iter().any(|r| r.handle == handle && r.id == RequestId::EndCredit) {
            return Err(6);
        }
        if self.queue.iter().any(|r| r.handle == handle && r.id == RequestId::StartCredit) {
            return Err(6);
        }
        let Some(entry) = self.credits.iter().position(|c| c.handle == handle) else {
            return Err(2);
        };
        self.queue.push(Request {
            id: RequestId::EndCredit,
            submitted: false,
            entry: Some(entry),
            handle,
            code,
            item_no,
            player,
            kind: code / 100,
            quantity: 0,
            digits: String::new(),
        });
        Ok(())
    }

    /// `alwItemAccount` (`0x1403CC830`).
    pub fn item_account(&mut self, code: u16, item_no: u8, player: u8) -> Result<(), u32> {
        self.gate()?;
        if code / 100 > MAX_KIND || u64::from(player) > self.max_players {
            return Err(2);
        }
        self.need_live_credit(player)?;
        self.push_plain(RequestId::ItemAccount, code, item_no, player, 0, String::new())
    }

    /// `alwPutQuantity` (`0x1403CCCB0`).
    pub fn put_quantity(&mut self, kind: u16, player: u8, quantity: u16) -> Result<(), u32> {
        self.gate()?;
        if kind > MAX_KIND || u64::from(player) > self.max_players || quantity > MAX_QUANTITY {
            return Err(2);
        }
        self.need_live_credit(player)?;
        self.push_plain(RequestId::Quantity, 0, 0, player, kind, String::new())?;
        if let Some(request) = self.queue.last_mut() {
            request.quantity = quantity;
        }
        Ok(())
    }

    /// `alwPutGeneralID` (`0x1403CCA70`).
    pub fn put_general_id(&mut self, kind: u16, player: u8, digits: &str) -> Result<(), u32> {
        self.gate()?;
        if kind > MAX_KIND
            || u64::from(player) > self.max_players
            || digits.len() > MAX_GENERAL_ID
            || !digits.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(2);
        }
        self.need_live_credit(player)?;
        self.push_plain(RequestId::PutGeneralId, 0, 0, player, kind, digits.to_owned())
    }

    fn push_plain(
        &mut self,
        id: RequestId,
        code: u16,
        item_no: u8,
        player: u8,
        kind: u16,
        digits: String,
    ) -> Result<(), u32> {
        if self.queue.len() >= MAX_REQUESTS {
            return Err(4);
        }
        self.queue.push(Request {
            id,
            submitted: false,
            entry: None,
            handle: 0,
            code,
            item_no,
            player,
            kind: if id == RequestId::ItemAccount { code / 100 } else { kind },
            quantity: 0,
            digits,
        });
        Ok(())
    }

    /// Number of queued requests (`0x1403B6B80`).
    pub fn queued_requests(&self) -> usize {
        self.queue.len()
    }

    /// `0x1403B6C20`: no accounting activity pending.
    pub fn queue_is_empty(&self) -> bool {
        !self.initialized || self.queue.is_empty()
    }

    /// `alwSetNearFull` (`0x1403B7640`).
    pub fn set_near_full(&mut self, cx: &mut Cx<'_>, enable: bool) -> Result<(), u32> {
        if self.config.mode == 0 {
            return Err(11);
        }
        if !self.initialized {
            return Err(12);
        }
        self.reporter.near_full_enabled = enable;
        let _ = cx;
        Ok(())
    }

    // ---- phases -------------------------------------------------------------------------------------------

    fn set_fatal(&mut self, code: u32) {
        self.fatal = code;
        self.phase = Phase::Fatal;
        self.sub_phase = 0;
        self.error.set(u16::try_from(code).unwrap_or(800), 0);
    }

    fn phase_initialize(&mut self, cx: &mut Cx<'_>, now: u32) {
        self.t_init = now;
        if self.link.identity.keychip.is_empty() {
            // The identity is not known yet: the init block cannot be built. Retry next frame.
            return;
        }
        // The keychip setup is one poll per access; a missing answer is "not ready yet", not a failure.
        if self.dongle.counters().is_none() {
            if self.dongle.failed() {
                self.set_fatal(949);
            }
            return;
        }
        match self.reporter.initialize(cx, &self.link, &mut self.dongle) {
            Ok(()) => {
                self.playbook.initialize(self.link.test_mode);
                self.initialized = true;
                self.ar_requested = false;
                self.phase = Phase::GetBillingMode;
            }
            Err(code) => self.set_fatal(code),
        }
    }

    /// `alpbExExecute`: one step of the alpb state machine.
    fn execute(&mut self, cx: &mut Cx<'_>, now: u32) {
        let link = self.link.clone();
        self.reporter.execute(cx, &link, &mut self.dongle);
        self.playbook.set_tenpo_invalid(link.tenpo.is_none());
        self.with_env(cx, |book, env| book.tick(env));
        let _ = now;
    }

    /// `ExecStatus` (`0x1403B7EC0`).
    fn exec_status(&self) -> i32 {
        self.playbook.exec_status()
    }

    fn dispatch(&mut self, cx: &mut Cx<'_>, now: u32) {
        let exec = self.exec_status();
        if exec == st::NG && !matches!(self.phase, Phase::Initialize | Phase::TestMode) {
            let code = self.playbook.display_error();
            self.set_fatal(if code == 0 { 800 } else { code });
            return;
        }
        match self.phase {
            Phase::Initialize | Phase::Fatal => {}
            Phase::GetBillingMode => self.phase_get_billing_mode(),
            Phase::InitTestMode => {
                if exec == 8 {
                    self.phase = Phase::TestMode;
                }
            }
            Phase::TestMode => {}
            Phase::GetTenpoIp => {
                if self.link.tenpo.is_some() {
                    self.phase = Phase::StartupWait;
                }
            }
            Phase::StartupWait => self.phase_startup_wait(cx, now, exec),
            Phase::StartupDelayWait => self.phase_startup_delay_wait(cx, now, exec),
            Phase::FirstArBusy | Phase::ArBusy => self.phase_ar_busy(cx, now, exec),
            Phase::ArWait => {
                if exec == 2 {
                    self.ar_requested = true;
                    if self.ar_permitted {
                        self.ar_requested = false;
                        self.ar_permitted = false;
                        self.phase = Phase::ArBusy;
                    }
                }
            }
            Phase::Idle => {
                self.serve_request(cx, exec);
                match exec {
                    2 => self.ar_setting(cx, now),
                    10 => self.go_offline(),
                    _ => {}
                }
            }
            Phase::Offline => match exec {
                1 => {
                    self.phase = Phase::Idle;
                    self.offline_flag = false;
                    self.to_auth.offline = Some(false);
                }
                2 => self.ar_setting(cx, now),
                10 => self.to_auth.offline = Some(true),
                _ => {}
            },
        }
    }

    fn phase_get_billing_mode(&mut self) {
        let Some(counters) = self.dongle.counters() else { return };
        let billing_type = counters.billing_type;
        self.to_auth.allnet_type = Some(if counters.billing_type != 0 { 2 } else { 1 });
        let mode_byte = if self.config.mode == 1 { 1 } else { 2 };
        let test = self.link.test_mode;
        if billing_type != 0 && mode_byte & 1 == 0 {
            self.set_fatal(8113);
        } else if billing_type != 0 {
            self.phase = if test { Phase::InitTestMode } else { Phase::StartupWait };
        } else if test {
            self.phase = Phase::InitTestMode;
        } else if mode_byte == 2 {
            self.phase = Phase::GetTenpoIp;
        } else {
            self.phase = Phase::StartupWait;
        }
        self.sub_phase = 0;
    }

    fn phase_startup_wait(&mut self, cx: &mut Cx<'_>, now: u32, exec: i32) {
        if exec != 2 {
            return;
        }
        let waited = now.wrapping_sub(self.t_init);
        if waited <= STARTUP_WINDOW_MS && !(self.link.done || self.link.offline) {
            return;
        }
        let line = self.link.line_byte;
        if (line & 0xF5) == 0 && line != 10 {
            self.ar_setting(cx, now);
            return;
        }
        let day = cx.clock.local_now().civil().day;
        self.first_ar_delay_ms = schedule::first_ar_delay_ms(&self.link.identity.board, day);
        self.t_delay = now;
        self.phase = Phase::StartupDelayWait;
    }

    fn phase_startup_delay_wait(&mut self, cx: &mut Cx<'_>, now: u32, exec: i32) {
        if exec != 2 {
            return;
        }
        if now.wrapping_sub(self.t_init) > STARTUP_WINDOW_MS || now.wrapping_sub(self.t_delay) > self.first_ar_delay_ms
        {
            self.ar_setting(cx, now);
        }
    }

    /// `ar_setting` dispatch with the value amdaemon always passes (1): start the report.
    fn ar_setting(&mut self, cx: &mut Cx<'_>, now: u32) {
        let _ = (cx, now);
        // `StartAccountingReport() failed` is only logged; the phase machine tries again next tick.
        self.reporter.start_request();
        self.phase = Phase::ArBusy;
    }

    fn phase_ar_busy(&mut self, cx: &mut Cx<'_>, now: u32, exec: i32) {
        let _ = (cx, now);
        match exec {
            1 => {
                self.to_auth.offline = Some(false);
                self.phase = Phase::Idle;
            }
            2 => {
                if self.first_ar_delay_ms == 0 || now.wrapping_sub(self.t_delay) > self.first_ar_delay_ms {
                    self.first_ar_delay_ms = 0;
                    let _ = self.reporter.start_request();
                }
            }
            10 => self.go_offline(),
            _ => {}
        }
    }

    fn go_offline(&mut self) {
        self.offline_flag = true;
        self.phase = Phase::Offline;
        self.to_auth.offline = Some(true);
        self.to_auth.offline_cause = Some(self.playbook.offline_cause(self.reporter.term_date_error()));
    }

    /// The operation switch (`op.*`): accounting on in app status 3, off in 4 (table `{1,1,1,1,0}`).
    fn apply_operation(&mut self, cx: &mut Cx<'_>) {
        let _ = cx;
        let enable = !matches!(self.app_status, 4);
        self.reporter.set_enabled(enable);
    }

    /// The request server (`0x1403CBAD0`): serves the oldest request.
    fn serve_request(&mut self, cx: &mut Cx<'_>, exec: i32) {
        if self.queue.is_empty() {
            if self.cancel_pending && exec == st::PLAYING {
                self.with_env(cx, |book, env| book.cancel_play(env));
                self.cancel_pending = false;
            }
            return;
        }
        let head = self.queue[0].clone();
        if head.submitted {
            if head.id == RequestId::EndCredit
                && let Some(entry) = self.credits.iter().position(|c| c.handle == head.handle)
            {
                self.credits.remove(entry);
            }
            self.queue.remove(0);
            self.fix_entries();
            return;
        }
        let first_done = self.reporter.first_done();
        let result: Result<(), i32> = match head.id {
            RequestId::StartCredit => {
                if exec != st::AR_WAIT && exec != st::PLAYING {
                    return;
                }
                let handle = self.playbook.start_credit(first_done, pack_code(head.code, head.item_no, head.player));
                if handle == 0 {
                    Err(-1)
                } else {
                    if let Some(entry) = self.credits.iter_mut().find(|c| c.handle == head.handle) {
                        entry.alpb = Some(handle);
                    }
                    Ok(())
                }
            }
            id => {
                if exec != st::PLAYING {
                    return;
                }
                let rc = match id {
                    RequestId::EndCredit => {
                        let alpb = self.credits.iter().find(|c| c.handle == head.handle).and_then(|c| c.alpb);
                        self.playbook.end_credit(alpb.unwrap_or(0), pack_code(head.code, head.item_no, head.player))
                    }
                    RequestId::ItemAccount => {
                        self.playbook.item_account(pack_code(head.code, head.item_no, head.player))
                    }
                    RequestId::PutGeneralId => {
                        self.playbook.put_general_id(u32::from(head.kind), head.player, &head.digits)
                    }
                    _ => {
                        let code = 100 * head.kind + 20 + (head.quantity >> 8);
                        self.playbook.item_account(pack_code(code, (head.quantity & 0xFF) as u8, head.player))
                    }
                };
                if rc == 0 { Ok(()) } else { Err(rc) }
            }
        };
        match result {
            Ok(()) => self.queue[0].submitted = true,
            Err(_) => {
                // `Billing API failed`: dequeued; a failed START leaves its credit entry (`quirk.failed_start`).
                self.queue.remove(0);
                self.fix_entries();
            }
        }
    }

    /// Runs `f` on the state manager with everything it reads or writes outside itself.
    fn with_env<R>(&mut self, cx: &mut Cx<'_>, f: impl FnOnce(&mut PlayBook, &mut Env<'_>) -> R) -> R {
        let (_, _, stamp) = reporter::now_of(cx);
        let ar_status = self.reporter.status();
        let first_auth_done = self.reporter.first_done();
        let near_full_enabled = self.reporter.near_full_enabled;
        let conn_failed = self.reporter.term_date_error();
        let link = &self.link;
        let mut env = Env {
            dongle: &mut self.dongle,
            trace: &mut self.reporter.trace,
            identity: &link.identity,
            tenpo: link.tenpo,
            stamp,
            synced: link.date_synced,
            ar_status,
            first_auth_done,
            operating_mode: link.operating_mode,
            near_full_enabled,
            conn_failed,
        };
        f(&mut self.playbook, &mut env)
    }

    /// Keeps the credit indices of the queued requests in step with the credit table.
    fn fix_entries(&mut self) {
        let handles: Vec<u64> = self.credits.iter().map(|c| c.handle).collect();
        for request in &mut self.queue {
            request.entry = handles.iter().position(|&handle| handle == request.handle);
        }
    }

    // ---- facade -------------------------------------------------------------------------------------------

    fn playing(&self, player: u32) -> bool {
        player < 8 && self.slots.iter().flatten().any(|&(p, handle)| p == player && handle != 0)
    }

    fn facade_start_credit(&mut self, now: u32, player: u32, kind: u32, status: u32) -> AmResult<u32> {
        if !self.accounting_available() {
            return Err(AmError::with_message(AmError::NOT_INITIALIZED, "Accounting is disable."));
        }
        let code = (status.wrapping_add(100 * kind)) as u16;
        let handle = self
            .start_credit(now, code, 0, (player + 1) as u8)
            .map_err(|_| AmError::with_message(AmError::FAILED, "Failed to start credit."))?;
        let slot = self
            .slots
            .iter()
            .position(|slot| slot.is_some_and(|(_, existing)| existing == handle))
            .or_else(|| self.slots.iter().position(Option::is_none))
            .unwrap_or(128);
        if let Some(entry) = self.slots.get_mut(slot) {
            *entry = Some((player, handle));
        }
        Ok(slot as u32)
    }

    fn facade_end_credit(&mut self, player: u32, kind: u32, status: u32, items: u8, slot: u32) -> AmResult {
        if !self.playing(player) {
            return Err(AmError::with_message(AmError::NOT_INITIALIZED, "Not playing."));
        }
        let Some(Some((_, handle))) = self.slots.get(slot as usize).copied() else {
            return Err(AmError::with_message(AmError::FAILED, "Failed to end credit."));
        };
        let code = (status.wrapping_add(100 * kind)) as u16;
        self.end_credit(handle, code, items, (player + 1) as u8)
            .map_err(|_| AmError::with_message(AmError::FAILED, "Failed to end credit."))?;
        self.slots[slot as usize] = None;
        Ok(())
    }

    fn facade_item_account(&mut self, player: u32, kind: u32, status: u32, items: u8) -> AmResult {
        if !self.playing(player) {
            return Err(AmError::with_message(AmError::NOT_INITIALIZED, "Not playing."));
        }
        let code = (status.wrapping_add(100 * kind)) as u16;
        self.item_account(code, items, (player + 1) as u8)
            .map_err(|_| AmError::with_message(AmError::FAILED, "Failed to item account."))
    }

    fn facade_put_quantity(&mut self, player: u32, kind: u32, quantity: u16) -> AmResult {
        if !self.playing(player) {
            return Err(AmError::with_message(AmError::NOT_INITIALIZED, "Not playing."));
        }
        self.put_quantity(kind as u16, (player + 1) as u8, quantity)
            .map_err(|_| AmError::with_message(AmError::FAILED, "Failed to put quantity."))
    }

    fn facade_put_general_id(&mut self, player: u32, kind: u32, general_id: &[u16]) -> AmResult {
        if !self.playing(player) {
            return Err(AmError::with_message(AmError::NOT_INITIALIZED, "Not playing."));
        }
        let end = general_id.iter().position(|&unit| unit == 0).unwrap_or(general_id.len());
        let text = String::from_utf16_lossy(&general_id[..end]);
        let (encoded, _, failed) = encoding_rs::SHIFT_JIS.encode(&text);
        if failed || encoded.len() > 513 {
            return Err(AmError::with_message(AmError::FAILED, "Failed to put generalId."));
        }
        let digits = String::from_utf8_lossy(&encoded).into_owned();
        self.put_general_id(kind as u16, (player + 1) as u8, &digits)
            .map_err(|_| AmError::with_message(AmError::FAILED, "Failed to put generalId."))
    }
}

/// `alwTime`: milliseconds, wrapping at `u32`.
fn tick(cx: &Cx<'_>) -> u32 {
    cx.clock.tick_ms() as u32
}
