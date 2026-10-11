//! The store-card reader over a reader unit of the Aime module (`spec/libam/payment.md` PayEmSvc 6.1 and 6.4).
//!
//! The reader issues exactly the seven unit commands of 6.4, one at a time, in the order of the original:
//! `StartPolling`, `SetLedBlink` (blue 500/500), `CardDetect`, then the batch `MifareKeySetA` (`thinca`),
//! `MifareAuthenticationA` (sector 1) and `MifareRead` (1,0), one `MifareRead` per step for the other eight blocks
//! (an authentication whenever the sector changes), `StopPolling`, and the LED (off, or red for 3 s). The commands run
//! on the unit the Aime module drives ([`UnitCommands`]); the reader holds the unit's lease while it works, so the
//! module's own sequences do not interleave.

use std::collections::VecDeque;

use super::card::{CardPoll, CardRead, Lease, header_kind};
use crate::aime::device::{CommandError, CommandPoll, LedBlink, UnitCommand, UnitCommands};

/// Key A of every sector: ASCII `thinca` (6.1 state 1, stored XOR `0xA4` at `0x1406DF548`).
const KEY_A: [u8; 6] = *b"thinca";
/// The `(sector, block)` list of 6.1 (`0x1406DF590`): the header, then the eight ciphertext blocks.
const BLOCKS: [(u8, u8); 9] = [(1, 0), (1, 1), (1, 2), (2, 0), (2, 1), (2, 2), (3, 0), (3, 1), (3, 2)];
/// How long the red LED of a refused card stays on (6.1 state 5).
const RED_MS: u64 = 3000;
/// The card type of a MIFARE card in the answer of `CardDetect`.
const MIFARE: u8 = 0x10;
/// Commands one call may run back to back (every answer that is already there moves the sequence on).
const STEPS_PER_PUMP: usize = 32;

/// One command of the sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    StartPolling,
    Detect,
    KeySet,
    Auth(u8),
    /// The index into [`BLOCKS`].
    Read(usize),
    StopPolling,
}

/// What the answer of `CardDetect` holds.
#[derive(Debug, PartialEq, Eq)]
enum Detected {
    /// No card, or a card this reader does not read: ask again.
    Nothing,
    /// A MIFARE card with its ID.
    Mifare(Vec<u8>),
}

fn parse_detect(payload: &[u8]) -> Detected {
    let (count, kind, len) = (payload.first().copied(), payload.get(1).copied(), payload.get(2).copied());
    let (Some(1), Some(MIFARE), Some(len)) = (count, kind, len) else { return Detected::Nothing };
    match payload.get(3..3 + usize::from(len)) {
        Some(id) if id.len() >= 4 => Detected::Mifare(id.to_vec()),
        _ => Detected::Nothing,
    }
}

/// The command sequence, apart from the unit it runs on.
#[derive(Debug, Default)]
struct Sequence {
    queue: VecDeque<Step>,
    current: Option<Step>,
    /// The earliest tick of the next `CardDetect` (the unit's polling gap).
    detect_at: u64,
    /// `StartPolling` was issued and `StopPolling` has not finished.
    polling: bool,
    uid: Vec<u8>,
    blocks: Vec<[u8; 16]>,
    /// What the session ends with once `StopPolling` is through.
    outcome: Option<CardPoll>,
    /// The answer `poll` hands out next.
    answer: Option<CardPoll>,
    /// An LED frame still to send.
    led: Option<LedBlink>,
    /// Set by a stop: the LED (`true` red) once the commands are through.
    final_led: Option<bool>,
    /// The tick the red LED goes off.
    led_off_at: Option<u64>,
    /// The flow ended: the lease goes back once everything is sent.
    releasing: bool,
}

impl Sequence {
    fn begin(&mut self) {
        *self = Self { queue: VecDeque::from([Step::StartPolling]), polling: true, ..Self::default() };
    }

    fn command(&self, step: Step) -> UnitCommand {
        let uid4 = || {
            let mut uid4 = [0; 4];
            uid4.copy_from_slice(&self.uid[self.uid.len() - 4..]);
            uid4
        };
        match step {
            Step::StartPolling => UnitCommand::StartPolling,
            Step::Detect => UnitCommand::CardDetect,
            Step::KeySet => UnitCommand::MifareKeySetA(KEY_A),
            Step::Auth(sector) => UnitCommand::MifareAuthA { sector, uid4: uid4() },
            Step::Read(index) => {
                let (sector, block) = BLOCKS[index];
                UnitCommand::MifareRead { sector, block, uid4: uid4() }
            }
            Step::StopPolling => UnitCommand::StopPolling,
        }
    }

    /// The unit went away (the Aime module restarted): nothing more can be sent.
    fn lost(&mut self) {
        let active = self.polling || self.current.is_some() || !self.queue.is_empty();
        self.queue.clear();
        self.current = None;
        self.polling = false;
        self.led = None;
        self.final_led = None;
        self.led_off_at = None;
        self.releasing = false;
        if active && self.outcome.is_none() && self.answer.is_none() {
            self.answer = Some(CardPoll::Failed);
        }
    }

    /// The detection failed: stop polling, then report [`CardPoll::Failed`].
    fn fail(&mut self) {
        self.outcome = Some(CardPoll::Failed);
        self.queue = VecDeque::from([Step::StopPolling]);
    }

    /// A key, authentication or read failed: the detection restarts with the LED left blue.
    fn read_failed(&mut self, now: u64) {
        self.blocks.clear();
        self.queue = VecDeque::from([Step::Detect]);
        self.detect_at = now;
        self.answer = Some(CardPoll::ReadFailed);
    }

    fn done(&mut self, step: Step, payload: &[u8], now: u64, gap: u64) {
        match step {
            Step::StartPolling => {
                self.led = Some(LedBlink::BLUE);
                self.queue.push_back(Step::Detect);
            }
            Step::Detect => match parse_detect(payload) {
                Detected::Nothing => {
                    self.detect_at = now + gap;
                    self.queue.push_back(Step::Detect);
                }
                Detected::Mifare(id) => {
                    self.uid = id;
                    self.blocks.clear();
                    self.queue = VecDeque::from([Step::KeySet, Step::Auth(BLOCKS[0].0), Step::Read(0)]);
                }
            },
            Step::KeySet | Step::Auth(_) => {}
            Step::Read(index) => {
                let Some(block) = payload.get(..16).and_then(|bytes| <[u8; 16]>::try_from(bytes).ok()) else {
                    return self.read_failed(now);
                };
                if index == 0 && header_kind(&block).is_none() {
                    return self.fail();
                }
                self.blocks.push(block);
                match BLOCKS.get(index + 1) {
                    None => {
                        let read = CardRead { uid: self.uid.clone(), blocks: std::mem::take(&mut self.blocks) };
                        self.outcome = Some(CardPoll::Read(read));
                        self.queue.push_back(Step::StopPolling);
                    }
                    Some(&(sector, _)) => {
                        if sector != BLOCKS[index].0 {
                            self.queue.push_back(Step::Auth(sector));
                        }
                        self.queue.push_back(Step::Read(index + 1));
                    }
                }
            }
            Step::StopPolling => self.stopped(),
        }
    }

    fn failed(&mut self, step: Step, now: u64) {
        match step {
            Step::StartPolling | Step::Detect => self.fail(),
            Step::KeySet | Step::Auth(_) | Step::Read(_) => self.read_failed(now),
            Step::StopPolling => self.stopped(),
        }
    }

    fn stopped(&mut self) {
        self.polling = false;
        if let Some(outcome) = self.outcome.take() {
            self.answer = Some(outcome);
        }
    }

    /// Runs the sequence as far as the unit lets it.
    fn drive(&mut self, unit: &mut UnitCommands, now: u64) {
        if !unit.bound() {
            return self.lost();
        }
        if !unit.held() {
            self.releasing = false;
            return;
        }
        for _ in 0..STEPS_PER_PUMP {
            match unit.poll(now) {
                CommandPoll::Pending => return,
                CommandPoll::Done(payload) => {
                    if let Some(step) = self.current.take() {
                        self.done(step, &payload, now, unit.repoll_wait_ms());
                    }
                }
                CommandPoll::Failed => {
                    if let Some(step) = self.current.take() {
                        self.failed(step, now);
                    }
                }
                CommandPoll::Idle => {
                    if !self.send_next(unit, now) {
                        return;
                    }
                }
            }
        }
    }

    /// Sends the next frame; `false` when there is nothing to send now.
    fn send_next(&mut self, unit: &mut UnitCommands, now: u64) -> bool {
        if let Some(led) = self.led {
            return match unit.send(UnitCommand::SetLedBlink(led)) {
                Ok(()) => {
                    self.led = None;
                    self.led_off_at = (led == LedBlink::RED).then_some(now + RED_MS);
                    true
                }
                Err(error) => self.refused(error),
            };
        }
        if let Some(&step) = self.queue.front() {
            if step == Step::Detect && now < self.detect_at {
                return false;
            }
            return match unit.send(self.command(step)) {
                Ok(()) => {
                    self.queue.pop_front();
                    self.current = Some(step);
                    true
                }
                Err(error) => self.refused(error),
            };
        }
        if let Some(red) = self.final_led.take() {
            self.led = Some(if red { LedBlink::RED } else { LedBlink::OFF });
            return true;
        }
        if let Some(at) = self.led_off_at {
            if now < at {
                return false;
            }
            self.led_off_at = None;
            self.led = Some(LedBlink::OFF);
            return true;
        }
        if self.releasing {
            self.releasing = false;
            unit.release();
        }
        false
    }

    /// The unit did not take a frame: a lost unit ends the session, a busy port is retried at the next frame.
    fn refused(&mut self, error: CommandError) -> bool {
        if error == CommandError::NotHeld {
            self.lost();
        }
        false
    }
}

/// The store-card reader of the cabinet: the e-money module's use of an Aime reader unit.
#[derive(Debug, Default)]
pub(crate) struct AimeStoreCardReader {
    unit: Option<UnitCommands>,
    now_ms: u64,
    seq: Sequence,
}

impl AimeStoreCardReader {
    /// A reader that waits for its unit (the flow hands it over once the Aime module has opened its ports).
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn pump(&mut self) {
        if let Some(unit) = self.unit.as_mut() {
            self.seq.drive(unit, self.now_ms);
        }
    }
    /// Starts polling and the blue blink LED.
    pub(crate) fn start(&mut self) {
        let held = self.unit.as_ref().is_some_and(UnitCommands::acquire);
        if !held {
            self.seq = Sequence { answer: Some(CardPoll::Failed), ..Sequence::default() };
            return;
        }
        self.seq.begin();
        self.pump();
    }

    /// The state of the detection.
    pub(crate) fn poll(&mut self) -> CardPoll {
        self.pump();
        self.seq.answer.take().unwrap_or(CardPoll::Waiting)
    }

    /// Stops polling; `red` blinks the LED red for a rejected card (3 s, then off), otherwise the LED goes off.
    pub(crate) fn stop(&mut self, red: bool) {
        if let Some(unit) = self.unit.as_mut() {
            unit.cancel();
        }
        let seq = &mut self.seq;
        seq.current = None;
        seq.queue.clear();
        seq.outcome = None;
        seq.answer = None;
        seq.led = None;
        if seq.polling {
            seq.queue.push_back(Step::StopPolling);
        }
        seq.final_led = Some(red);
        self.pump();
    }

    /// Once per module frame, before the flows: runs the reader commands of a detection or of a stop that is still
    /// on its way. `now_ms` is the frame's millisecond tick.
    pub(crate) fn tick(&mut self, now_ms: u64) {
        self.now_ms = now_ms;
        self.pump();
    }

    /// The reader lease the module sees.
    pub(crate) fn lease(&self) -> Lease {
        match &self.unit {
            Some(unit) if unit.bound() => {
                Lease { free: unit.available(), present: unit.probed(), ready: unit.usable() }
            }
            _ => Lease::ABSENT,
        }
    }

    /// Takes the reader lease for a flow (`0x1402563B0`); `false` while an Aime sequence holds the reader.
    pub(crate) fn acquire(&mut self) -> bool {
        self.seq.releasing = false;
        self.unit.as_ref().is_some_and(UnitCommands::acquire)
    }

    /// Gives the reader lease back at the end of a flow (`0x1402563D0`). Commands still on their way (the stop of a
    /// detection, the LED) finish first.
    pub(crate) fn release(&mut self) {
        if self.seq.polling && self.seq.final_led.is_none() {
            self.stop(false);
        }
        self.seq.releasing = true;
        self.pump();
    }

    /// Whether the reader needs the unit of the Aime module handed to it (it has none, or the Aime module restarted).
    pub(crate) fn wants_unit(&self) -> bool {
        self.unit.as_ref().is_none_or(|unit| !unit.bound())
    }

    /// Hands the reader the unit of the Aime module the e-money flows run on.
    pub(crate) fn attach(&mut self, unit: UnitCommands) {
        self.unit = Some(unit);
        self.seq = Sequence::default();
    }
}
