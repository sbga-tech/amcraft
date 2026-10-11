//! One reader unit of the Aime facade (`AccountLoader`, `spec/libam/aime.md` A.1, A.4, A.5, A.8): the command
//! sequencer, the commands' bodies over the aimelib core, the result blocks and the unit's events.

use crate::aime::{
    AccessCode, AimeEvent, CardInfo, FinishedStatus, FinishedTask, LibamAimeId, ReaderInfo, RelatedSegaId,
    UnitTaskFinished,
};
use crate::error::FatalError;
use crate::result::{AmError, AmResult};
use crate::time::Tm;

use super::device::{Holder, UnitSlot};
use super::facade_error::{fatal_number, has_unit_sub_number};
use super::lib::{AimeLib, Op, PollKind, ResultBlock, StartError, flags};
use backend::Fs;

/// The auto re-detect runs after 60 s without a sequence (`0x140259900`, A.12).
const RESCAN_AFTER_MS: u64 = 60_000;
/// The libam `ErrorCode` the forced failure raises (`0x1402A1510`): a server access error.
const FORCED_FAILURE: u32 = 10;
/// Internal aimelib codes the check commands treat as "unknown to the server" (C 11.1).
const FELICA_ID_NOT_FOUND: u32 = 12;
const AIME_ID_NOT_FOUND: u32 = 15;

/// The commands of a unit sequencer (A.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cmd {
    Detect,
    Scan,
    CheckFelica,
    RegisterFelica,
    CheckAime,
    RegisterAime,
    ForceFailure,
    Firmware,
}

impl Cmd {
    fn task(self) -> FinishedTask {
        match self {
            Self::Detect => FinishedTask::DetectCardReader,
            Self::Scan => FinishedTask::ScanNfcChip,
            Self::CheckFelica => FinishedTask::CheckFelicaRegistered,
            Self::RegisterFelica => FinishedTask::RegisterFelica,
            Self::CheckAime => FinishedTask::CheckAimeIdRegistered,
            Self::RegisterAime => FinishedTask::RegisterAimeId,
            Self::ForceFailure => FinishedTask::None,
            Self::Firmware => FinishedTask::UpdateFirmware,
        }
    }
}

/// `am::CommandSequencer` state (A.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seq {
    Idle,
    Running,
    /// A command waits for `confirm` (state 2).
    Question,
    Cancelling,
    Done,
    Failed,
}

/// What one step of a command reports.
enum Poll {
    Pending,
    Question(u32),
    Ok,
    Fail(u32),
}

/// What the module hands a unit for one frame.
pub(super) struct Tick<'a> {
    pub now_ms: u64,
    /// Readers accept cards (`permit` and not `suspended`).
    pub permit: bool,
    /// The Aime DB answered the last alive check.
    pub db_alive: bool,
    /// The global flag byte that selects fatal 87 instead of 84.
    pub develop: bool,
    /// The module is operational: an idle unit may re-detect.
    pub rescan_ok: bool,
    pub tm: Tm,
    pub paths: &'a dyn Fs,
    /// The first firmware path of the configuration.
    pub firmware: Option<&'a str>,
    pub events: &'a mut Vec<AimeEvent>,
}

/// One reader unit.
#[derive(Debug)]
pub(super) struct Unit {
    index: usize,
    /// The unit config flag byte: gates the fatal numbers 6501/6506.
    flag: bool,
    lib: AimeLib,
    /// `+132`: 0 reset, 1 started, 2 finished.
    pub(super) phase: u8,
    /// `+128`: the sequence is an auto-rescan restart.
    auto_restart: bool,
    /// `+129`: auto re-detect enabled.
    auto_rescan: bool,
    kind: u32,
    seq: Seq,
    list: Vec<Cmd>,
    current: usize,
    started: bool,
    confirmed: bool,
    cancel_sent: bool,
    polling: Option<PollKind>,
    /// A command of an abandoned sequence still runs inside the core.
    draining: Option<PollKind>,
    /// `+5544`: the libam `ErrorCode` of the last sequence.
    error_code: u32,
    end_pending: bool,
    need_felica: bool,
    need_aime: bool,
    reader_ready: bool,
    led_capable: bool,
    reader: ReaderInfo,
    card: CardInfo,
    access: Option<AccessCode>,
    aime: LibamAimeId,
    sega: Option<RelatedSegaId>,
    /// The `(aime id, card id)` pair of the last result, until the module takes it for the user-info list.
    card_entry: Option<(u32, [u8; 8])>,
    pattern: u32,
    idle_since_ms: u64,
    /// The unit's owner slot (`module+2680+24*i`, `0x1402563B0` / `0x1402563D0` / `0x140259670`), shared with the
    /// e-money store-card reader.
    slot: UnitSlot,
}

fn wide<const N: usize>(text: &str) -> [u16; N] {
    let mut out = [0u16; N];
    for (slot, unit) in out.iter_mut().take(N - 1).zip(text.encode_utf16()) {
        *slot = unit;
    }
    out
}

impl Unit {
    /// A unit over its aimelib object; the LED controller reports pattern 0 at construction (A.8).
    pub(super) fn new(index: usize, flag: bool, lib: AimeLib, now_ms: u64, events: &mut Vec<AimeEvent>) -> Self {
        events.push(AimeEvent::ChangeLedLightPattern { unit: index as u64, pattern: 0 });
        Self {
            index,
            flag,
            lib,
            phase: 0,
            auto_restart: false,
            auto_rescan: true,
            kind: 0,
            seq: Seq::Idle,
            list: Vec::new(),
            current: 0,
            started: false,
            confirmed: false,
            cancel_sent: false,
            polling: None,
            draining: None,
            error_code: 0,
            end_pending: false,
            need_felica: false,
            need_aime: false,
            reader_ready: false,
            led_capable: false,
            reader: Self::blank_reader(),
            card: CardInfo::default(),
            access: None,
            aime: LibamAimeId { id: u32::MAX, flag: false },
            sega: None,
            card_entry: None,
            pattern: 0,
            idle_since_ms: now_ms,
            slot: UnitSlot::default(),
        }
    }

    fn blank_reader() -> ReaderInfo {
        ReaderInfo { detected: false, hard_version: [0; 64], firm_version: [0; 64] }
    }

    /// The aimelib object (the module sets the board info and terminates it).
    pub(super) fn lib_mut(&mut self) -> &mut AimeLib {
        &mut self.lib
    }

    /// A sequence runs (`0x140256570`: state 1, 2 or 3).
    pub(super) fn busy(&self) -> bool {
        matches!(self.seq, Seq::Running | Seq::Question | Seq::Cancelling)
    }

    /// A finished sequence has not yet raised its whole-sequence event; the unit is not free until it did.
    pub(super) fn reporting(&self) -> bool {
        self.end_pending
    }

    pub(super) fn reader_info(&self) -> ReaderInfo {
        self.reader.clone()
    }

    pub(super) fn card_info(&self) -> Option<CardInfo> {
        (self.card.card_type != 0).then_some(self.card)
    }

    pub(super) fn access_code(&self) -> Option<AccessCode> {
        self.access
    }

    pub(super) fn aime_id(&self) -> Option<LibamAimeId> {
        (self.aime.id != u32::MAX).then_some(self.aime)
    }

    pub(super) fn related_sega_id(&self) -> Option<RelatedSegaId> {
        self.sega.clone()
    }

    /// The user-info record the last result produced: a FeliCa card's IDm under its Aime id (payment.md 5.13 reads
    /// the 8 card-id bytes of a kind-2 record; that the bytes are the IDm is [INFERENCE]).
    pub(super) fn take_card_entry(&mut self) -> Option<(u32, [u8; 8])> {
        self.card_entry.take()
    }

    fn emit(&self, t: &mut Tick<'_>, cmd_task: FinishedTask, status: FinishedStatus) {
        t.events.push(AimeEvent::UnitTaskFinished(Box::new(UnitTaskFinished {
            unit: self.index as u64,
            task: cmd_task,
            status,
            error_code: self.error_code,
            fatal: FatalError::default(),
        })));
    }

    /// `0x140259E00`: forgets the result blocks of the previous sequence.
    fn reset_results(&mut self) {
        self.card = CardInfo::default();
        self.access = None;
        self.aime = LibamAimeId { id: u32::MAX, flag: false };
        self.sega = None;
        self.need_felica = false;
        self.need_aime = false;
    }

    /// The owner slot of the unit.
    pub(super) fn slot(&self) -> UnitSlot {
        self.slot.clone()
    }

    /// `0x140259670`: the unit is not held by another module (the e-money store-card reader).
    pub(super) fn available(&self) -> bool {
        self.slot.available_to(Holder::Aime)
    }

    /// The slot follows the sequence: the aime module holds the unit while a sequence (or the command of an
    /// abandoned one) runs, and gives it back when the unit is idle.
    pub(super) fn sync_slot(&self) {
        if self.busy() || self.end_pending || self.draining.is_some() {
            self.slot.acquire(Holder::Aime);
        } else {
            self.slot.release(Holder::Aime);
        }
    }

    /// Starts a sequence of `kind` (A.3, A.4). `Err(3)` while another one runs or the e-money reader holds the unit.
    pub(super) fn start(&mut self, kind: u32, t: &mut Tick<'_>) -> AmResult {
        if !self.available() || (self.busy() && !self.auto_restart) {
            return Err(AmError::code(AmError::BUSY));
        }
        if self.busy() {
            self.abandon(t.now_ms);
        }
        self.auto_restart = kind == 1;
        self.reset_results();
        self.kind = kind;
        self.list = match kind {
            1 => vec![Cmd::Detect],
            2 => vec![Cmd::Detect, Cmd::Scan],
            5 => {
                let mut list = vec![Cmd::Detect, Cmd::Scan];
                if self.reader_ready {
                    list.extend([Cmd::CheckFelica, Cmd::RegisterFelica, Cmd::CheckAime, Cmd::RegisterAime]);
                } else {
                    list.push(Cmd::ForceFailure);
                }
                list
            }
            _ => vec![Cmd::Firmware, Cmd::Detect],
        };
        self.current = 0;
        self.started = false;
        self.confirmed = false;
        self.cancel_sent = false;
        self.polling = None;
        self.error_code = 0;
        self.end_pending = false;
        self.phase = 1;
        self.seq = Seq::Running;
        // The unit reset (`0x140259730`): every command reports "queued".
        for task in [
            FinishedTask::DetectCardReader,
            FinishedTask::ScanNfcChip,
            FinishedTask::CheckFelicaRegistered,
            FinishedTask::RegisterFelica,
            FinishedTask::CheckAimeIdRegistered,
            FinishedTask::RegisterAimeId,
            FinishedTask::UpdateFirmware,
        ] {
            self.emit(t, task, FinishedStatus::Queued);
        }
        self.sync_slot();
        Ok(())
    }

    /// `confirm`: a parked question continues (sequencer 2 -> 1).
    pub(super) fn confirm(&mut self) {
        if self.seq == Seq::Question {
            self.seq = Seq::Running;
            self.confirmed = true;
        }
    }

    /// `cancel`: a running sequence turns to cancelling (1/2 -> 3).
    pub(super) fn cancel(&mut self) {
        if matches!(self.seq, Seq::Running | Seq::Question) {
            self.seq = Seq::Cancelling;
        }
    }

    /// Drops the running sequence without events (terminate, restart, a replaced auto rescan).
    pub(super) fn abandon(&mut self, now_ms: u64) {
        if self.started
            && let Some(kind) = self.polling.take()
        {
            self.lib.cancel(now_ms);
            self.draining = Some(kind);
        }
        if self.led_capable && self.pattern != 0 {
            self.lib.set_led_color([0; 3]);
        }
        self.seq = Seq::Idle;
        self.started = false;
        self.end_pending = false;
        self.phase = 0;
        self.pattern = 0;
        self.sync_slot();
    }

    pub(super) fn set_led(&mut self, rgb: [u8; 3]) {
        if self.led_capable {
            self.lib.set_led_color(rgb);
        }
    }

    /// `0x1402A1CD0`: patterns above 4 are ignored; a change raises the listener (`0x1402A1E10`).
    pub(super) fn set_pattern(&mut self, pattern: u32, events: &mut Vec<AimeEvent>) {
        if pattern > 4 {
            return;
        }
        if pattern != self.pattern {
            self.pattern = pattern;
            events.push(AimeEvent::ChangeLedLightPattern { unit: self.index as u64, pattern });
        }
        if self.led_capable {
            self.lib.set_led_pattern(pattern as u8);
        }
    }

    /// One frame of the unit (`0x140259900`). Returns the fatal error a finished sequence reports.
    pub(super) fn step(&mut self, t: &mut Tick<'_>) -> Option<(u16, u16)> {
        if let Some(kind) = self.draining
            && self.lib.poll(kind, t.now_ms)
        {
            self.draining = None;
        }
        if self.draining.is_some() {
            return None;
        }
        match self.seq {
            Seq::Idle => {
                if self.end_pending {
                    return self.report_end(t);
                }
                None
            }
            Seq::Done | Seq::Failed => {
                if self.end_pending {
                    return self.report_end(t);
                }
                if self.auto_rescan
                    && t.rescan_ok
                    && self.phase == 2
                    && t.now_ms.saturating_sub(self.idle_since_ms) >= RESCAN_AFTER_MS
                {
                    let _ = self.start(1, t);
                }
                None
            }
            Seq::Question => None,
            Seq::Cancelling => {
                self.step_cancel(t);
                None
            }
            Seq::Running => {
                self.step_running(t);
                None
            }
        }
    }

    fn step_cancel(&mut self, t: &mut Tick<'_>) {
        let cmd = self.list[self.current];
        if self.started
            && let Some(kind) = self.polling
        {
            if !self.cancel_sent {
                self.lib.cancel(t.now_ms);
                self.cancel_sent = true;
            }
            if !self.lib.poll(kind, t.now_ms) {
                return;
            }
            self.polling = None;
        }
        self.emit(t, cmd.task(), FinishedStatus::Cancelled);
        self.seq = Seq::Failed;
        self.end_pending = true;
    }

    fn step_running(&mut self, t: &mut Tick<'_>) {
        let cmd = self.list[self.current];
        if !self.started {
            if cmd == Cmd::Scan && !t.permit {
                return;
            }
            self.emit(t, cmd.task(), FinishedStatus::Started);
            self.started = true;
            self.confirmed = false;
            let first = self.start_cmd(cmd, t);
            self.settle(cmd, first, t);
            return;
        }
        let outcome = match self.polling {
            Some(kind) => {
                if self.lib.poll(kind, t.now_ms) {
                    self.polling = None;
                    self.finish_lib(cmd)
                } else {
                    if cmd == Cmd::Firmware {
                        t.events.push(AimeEvent::FirmwareUpdatingProgress {
                            unit: self.index as u64,
                            progress: self.lib.firmware_progress(),
                        });
                    }
                    Poll::Pending
                }
            }
            None if self.confirmed => self.start_register(cmd),
            None => Poll::Pending,
        };
        self.settle(cmd, outcome, t);
    }

    /// Applies the outcome of a command step (A.4: success advances, failure skips the rest).
    fn settle(&mut self, cmd: Cmd, outcome: Poll, t: &mut Tick<'_>) {
        match outcome {
            Poll::Pending => {}
            Poll::Question(question) => {
                t.events.push(AimeEvent::NeedAcceptation { unit: self.index as u64, question });
                self.seq = Seq::Question;
            }
            Poll::Ok => {
                self.emit(t, cmd.task(), FinishedStatus::Succeeded);
                self.current += 1;
                self.started = false;
                if self.current >= self.list.len() {
                    self.current = self.list.len() - 1;
                    self.seq = Seq::Done;
                    self.end_pending = true;
                }
            }
            Poll::Fail(code) => {
                self.error_code = code;
                self.emit(t, cmd.task(), FinishedStatus::Failed);
                self.seq = Seq::Failed;
                self.end_pending = true;
            }
        }
    }

    /// The sequence ended: the whole-sequence event (task 9) with the fatal error of the error code.
    fn report_end(&mut self, t: &mut Tick<'_>) -> Option<(u16, u16)> {
        self.end_pending = false;
        self.phase = 2;
        self.idle_since_ms = t.now_ms;
        let status = if self.seq == Seq::Done { FinishedStatus::Succeeded } else { FinishedStatus::Failed };
        let number = fatal_number(self.error_code, t.develop);
        let sub = if has_unit_sub_number(number) { self.index as u16 + 1 } else { 0 };
        let suppressed = (matches!(number, 6501 | 6506) && !self.flag) || (number == 6508 && t.db_alive);
        let reported = number != 0 && !suppressed;
        let fatal = if reported { FatalError::new(number, sub, t.tm, "", 0) } else { FatalError::default() };
        t.events.push(AimeEvent::UnitTaskFinished(Box::new(UnitTaskFinished {
            unit: self.index as u64,
            task: FinishedTask::SequenceFinished,
            status,
            error_code: self.error_code,
            fatal,
        })));
        reported.then_some((number, sub))
    }

    fn lib_start(&mut self, op: Op, kind: PollKind, now_ms: u64) -> Poll {
        match self.lib.start(op, now_ms) {
            Ok(()) => {
                self.polling = Some(kind);
                Poll::Pending
            }
            // 4: "NFC R/W Access Error", the nearest libam code of a refused start [INFERENCE].
            Err(StartError::Refused | StartError::NotInitialized) => Poll::Fail(4),
            Err(_) => Poll::Fail(4),
        }
    }

    fn start_cmd(&mut self, cmd: Cmd, t: &mut Tick<'_>) -> Poll {
        let now = t.now_ms;
        match cmd {
            Cmd::Detect => self.lib_start(Op::UnitCheck, PollKind::UnitCheck, now),
            Cmd::Scan => self.lib_start(Op::Scan { filter: 3 }, PollKind::Scan, now),
            Cmd::CheckFelica => {
                if self.lib.result().flags & flags::FELICA == 0 {
                    Poll::Ok
                } else {
                    self.lib_start(Op::FelicaIdGet, PollKind::FelicaIdGet, now)
                }
            }
            Cmd::RegisterFelica => {
                if self.need_felica {
                    Poll::Question(1)
                } else {
                    Poll::Ok
                }
            }
            Cmd::CheckAime => self.lib_start(Op::AccountGet, PollKind::AccountGet, now),
            Cmd::RegisterAime => {
                if self.need_aime {
                    Poll::Question(2)
                } else {
                    Poll::Ok
                }
            }
            Cmd::ForceFailure => Poll::Fail(FORCED_FAILURE),
            Cmd::Firmware => match self.lib.start_firmware(t.paths, t.firmware, false, false) {
                Ok(()) => {
                    self.polling = Some(PollKind::NfcFirmware);
                    Poll::Pending
                }
                Err(_) => Poll::Fail(22),
            },
        }
    }

    /// The question was confirmed: the registration runs.
    fn start_register(&mut self, cmd: Cmd) -> Poll {
        // The sequencer calls the time of the frame through the core's own clock; registrations are not timed here.
        let now = 0;
        match cmd {
            Cmd::RegisterFelica => self.lib_start(Op::FelicaIdRegist, PollKind::FelicaIdRegist, now),
            Cmd::RegisterAime => self.lib_start(Op::AccountRegist, PollKind::AccountRegist, now),
            _ => Poll::Pending,
        }
    }

    /// A polled aimelib command finished: its outcome and the result blocks it fills.
    fn finish_lib(&mut self, cmd: Cmd) -> Poll {
        let r = self.lib.result();
        let raw = r.error.raw();
        let code = r.error.libam_code();
        match cmd {
            Cmd::Detect => {
                if raw == 0 {
                    let strings = self.lib.reader_strings();
                    self.reader =
                        ReaderInfo { detected: true, hard_version: wide(&strings.hw), firm_version: wide(&strings.fw) };
                    self.reader_ready = true;
                    self.led_capable = strings.led_present;
                    Poll::Ok
                } else {
                    self.reader = Self::blank_reader();
                    self.reader_ready = false;
                    Poll::Fail(code)
                }
            }
            Cmd::CheckFelica if raw == FELICA_ID_NOT_FOUND => {
                self.need_felica = true;
                Poll::Ok
            }
            Cmd::CheckAime if raw == AIME_ID_NOT_FOUND => {
                self.absorb(&r);
                self.need_aime = true;
                Poll::Ok
            }
            Cmd::Firmware => {
                if raw == 0 {
                    Poll::Ok
                } else {
                    Poll::Fail(code)
                }
            }
            _ => {
                if raw == 0 {
                    self.absorb(&r);
                    Poll::Ok
                } else {
                    Poll::Fail(code)
                }
            }
        }
    }

    /// Copies the aimelib result block `R` into the unit's result blocks (`+3436 .. +3616`).
    fn absorb(&mut self, r: &ResultBlock) {
        let felica = r.flags & flags::FELICA != 0;
        self.card = CardInfo {
            card_type: if felica { CardInfo::FELICA } else { r.card_kind },
            idm: u64::from_le_bytes(r.felica_idm),
            pmm: u64::from_le_bytes(r.felica_pmm),
            mobile: r.flags & flags::MOBILE != 0,
        };
        self.access = AccessCode::parse(&r.access_code).filter(|code| *code != AccessCode::UNSET);
        self.aime = LibamAimeId { id: r.aime_id as u32, flag: r.flags & flags::KIND_2 != 0 };
        self.card_entry = (felica && r.aime_id != -1).then_some((r.aime_id as u32, r.felica_idm));
        if r.sega_id[0] == 0 {
            self.sega = None;
        } else {
            let length = r.sega_id.iter().position(|&byte| byte == 0).unwrap_or(r.sega_id.len());
            let mut auth_key = [0u16; 257];
            for (slot, &byte) in auth_key.iter_mut().zip(&r.sega_id[..length]) {
                *slot = u16::from(byte);
            }
            let id = |value: i32| (value != -1).then_some(value as u32);
            self.sega = Some(RelatedSegaId {
                auth_key,
                count: u64::from(r.aime_found),
                items: [id(r.aime_id), id(r.related_aime_ids[0]), id(r.related_aime_ids[1])],
            });
        }
    }
}
