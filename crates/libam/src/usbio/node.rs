//! `am::usbio::Node` (`usbio.md` 1.4.2-1.4.5, 1.6): the handshake, the input decoding, the output queue and the
//! command sequencer of the firmware update.

use std::collections::VecDeque;
use std::rc::Rc;

use crate::config::UsbIoNode;
use crate::jvs::{AnalogChannel, AnalogState, RotaryState, SwitchState};
use crate::usbio::{UNIQUE_INPUT_DATA_LEN, UniqueDescriptor, UniqueDescriptors, UsbIoNodeInfo};

use super::board::{BoardDesc, satisfies};
use super::firmware::FirmwareFile;
use crate::module::ErrorCell;
use backend::hid::{HidDevice, REPORT_LEN, Report};

/// Error numbers of the node and manager records (`usbio.md` 1.9).
pub mod error {
    /// Transport or board info unavailable.
    pub const CONTEXT: u16 = 0x1009;
    /// Board id not found, enumeration failure, communication error.
    pub const NOT_FOUND: u16 = 0x1901;
    /// Configured functions not satisfied, status handshake failure.
    pub const FUNCTION: u16 = 0x1902;
    /// Firmware file unusable, unexpected mode, transfer failed.
    pub const FIRMWARE: u16 = 0x1903;
}

/// Node modes (`Node+0x17C`).
pub mod mode {
    /// Reset: no board.
    pub const RESET: u8 = 0;
    /// Normal operation.
    pub const NORMAL: u8 = 1;
    /// Firmware update needed; still in application mode.
    pub const UPDATE_NEEDED: u8 = 2;
    /// Boot-loader (firmware update) mode.
    pub const FIRMWARE: u8 = 3;
}

/// Milliseconds a ChangeBootMode / Reset command waits for the board.
const SETTLE_MS: u64 = 5000;
const RING_LEN: usize = 600;

/// `0x140292740`: a ring of 600 flags plus the consecutive count.
#[derive(Debug, Clone)]
pub struct FailureCounter {
    consecutive_limit: usize,
    percent: usize,
    ring: VecDeque<bool>,
    consecutive: usize,
    set: usize,
}

impl FailureCounter {
    /// A counter that trips on `consecutive_limit` consecutive failures or `percent` % of the last 600.
    pub fn new(consecutive_limit: usize, percent: usize) -> Self {
        Self { consecutive_limit, percent, ring: VecDeque::new(), consecutive: 0, set: 0 }
    }

    /// Clears the history.
    pub fn reset(&mut self) {
        self.ring.clear();
        self.consecutive = 0;
        self.set = 0;
    }

    /// Records one tick; ignored once the counter is exceeded.
    pub fn push(&mut self, failed: bool) {
        if self.exceeded() {
            return;
        }
        if self.ring.len() == RING_LEN && self.ring.pop_front() == Some(true) {
            self.set -= 1;
        }
        self.ring.push_back(failed);
        if failed {
            self.set += 1;
            self.consecutive += 1;
        } else {
            self.consecutive = 0;
        }
    }

    /// `consecutive >= n` or the flags set in the ring reach the percentage of 600.
    pub fn exceeded(&self) -> bool {
        self.consecutive >= self.consecutive_limit || self.set >= RING_LEN * self.percent / 100
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cmd {
    Delay,
    ChangeBootMode,
    UpdateFirmware,
    Reset,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum SeqState {
    #[default]
    Idle,
    Running,
    Done,
    Failed,
}

/// The command sequencer (`0x140256740`).
#[derive(Debug, Default)]
struct Sequencer {
    cmds: Vec<Cmd>,
    index: usize,
    state: SeqState,
    started: bool,
    cancel: bool,
    timer: u64,
    file: Option<Rc<FirmwareFile>>,
    lines_sent: usize,
    progress: u32,
}

impl Sequencer {
    fn start(&mut self, cmds: Vec<Cmd>, file: Option<Rc<FirmwareFile>>) {
        *self = Self { cmds, state: SeqState::Running, file, ..Self::default() };
    }

    fn finished(&self) -> bool {
        matches!(self.state, SeqState::Done | SeqState::Failed)
    }
}

/// One of the four nodes.
#[derive(Debug)]
pub struct Node {
    cfg: UsbIoNode,
    index: usize,
    append_sub: bool,
    /// `Node+0x17C`.
    pub mode: u8,
    board_mode: u8,
    /// Board info of the matched board.
    pub info: UsbIoNodeInfo,
    phase: u8,
    step: u8,
    timeout_state: u8,
    /// The node error record.
    pub error: ErrorCell,
    comm_ok: bool,
    /// Switch input.
    pub switch: SwitchState<4>,
    /// Coin slots.
    pub coin: UniqueDescriptors,
    /// Analog input.
    pub analog: AnalogState<16>,
    /// Rotary input.
    pub rotary: RotaryState<16>,
    /// Raw unique input data.
    pub unique: [u8; UNIQUE_INPUT_DATA_LEN],
    system_status: u8,
    usb_status: u8,
    gout: Option<(usize, u64)>,
    pwm_duty: [u8; 8],
    pwm_pending: bool,
    ring: Vec<(u8, [u8; 62])>,
    counter_a: FailureCounter,
    counter_b: FailureCounter,
    seq: Sequencer,
    dev: Option<Box<dyn HidDevice>>,
}

fn wide<const N: usize>(text: &str) -> [u16; N] {
    let mut out = [0; N];
    for (slot, unit) in out.iter_mut().zip(text.encode_utf16()) {
        *slot = unit;
    }
    out
}

fn align8(bits: u64) -> u64 {
    bits.div_ceil(8) * 8
}

impl Node {
    /// A node in the reset state.
    pub fn new(index: usize, cfg: UsbIoNode, append_sub: bool) -> Self {
        Self {
            cfg,
            index,
            append_sub,
            mode: mode::RESET,
            board_mode: 0,
            info: UsbIoNodeInfo::default(),
            phase: 0,
            step: 0,
            timeout_state: 0,
            error: ErrorCell::EMPTY,
            comm_ok: false,
            switch: SwitchState { system: 0, player_count: 0, player_masks: [0; 4] },
            coin: UniqueDescriptors::default(),
            analog: AnalogState { count: 0, channels: [AnalogChannel::default(); 16] },
            rotary: RotaryState { count: 0, values: [0; 16] },
            unique: [0; UNIQUE_INPUT_DATA_LEN],
            system_status: 0,
            usb_status: 0,
            gout: None,
            pwm_duty: [0; 8],
            pwm_pending: false,
            ring: Vec::new(),
            counter_a: FailureCounter::new(3, 10),
            counter_b: FailureCounter::new(60, 20),
            seq: Sequencer::default(),
            dev: None,
        }
    }

    /// The configuration.
    pub fn config(&self) -> &UsbIoNode {
        &self.cfg
    }

    /// Sets the node error, with the sub number `index + 1` when `append_sub`.
    pub fn raise(&mut self, code: u16) {
        self.error.set(code, if self.append_sub { self.index as u16 + 1 } else { 0 });
    }

    fn settled(&self) -> bool {
        self.phase == 2 && self.step == 0
    }

    /// `0x14024D360`: binds the board and starts the handshake (mode 1) or the command sequence.
    pub fn initialize(&mut self, dev: Box<dyn HidDevice>, desc: &BoardDesc, mode: u8, force_outputs: bool) {
        self.phase = 2;
        self.step = 0;
        self.counter_b.reset();
        self.mode = mode;
        self.dev = Some(dev);
        self.board_mode = desc.mode;
        self.info =
            UsbIoNodeInfo { name: wide(&desc.board_no), code: wide(&desc.custom_chip), revision: desc.revision };
        let funcs = &self.cfg.funcs;
        if mode == mode::NORMAL {
            if !satisfies(&desc.functions, funcs) {
                self.raise(error::FUNCTION);
                return;
            }
            let players = funcs.switch_input.map_or(0, |p| p.a);
            self.switch.player_count = if players > 4 { 0 } else { players };
            let coins = funcs.coin_input.unwrap_or(0);
            self.coin.count = if coins > 2 { 0 } else { coins };
            let channels = funcs.analog_input.map_or(0, |p| p.a);
            self.analog.count = if channels > 16 { 0 } else { channels };
            let bits = funcs.analog_input.map_or(0, |p| p.b).min(16);
            for channel in &mut self.analog.channels {
                channel.valid_bits = bits;
            }
            let rotary = funcs.rotary_input.unwrap_or(0);
            self.rotary.count = if rotary > 16 { 0 } else { rotary };
        }
        if mode == mode::FIRMWARE && desc.mode != 0 {
            self.raise(error::FIRMWARE);
            return;
        }
        self.seq = Sequencer::default();
        self.reset_outputs(force_outputs);
        self.phase = 1;
        self.step = 0;
        match mode {
            mode::NORMAL => {
                self.phase = 0;
                self.step = 1;
            }
            mode::UPDATE_NEEDED => {
                let mut cmds = vec![Cmd::Delay];
                if desc.mode != 0 {
                    cmds.push(Cmd::ChangeBootMode);
                }
                self.seq.start(cmds, None);
            }
            _ => self.seq.start(vec![Cmd::Delay], None),
        }
    }

    /// `0x14024D8E0`: leaves a firmware mode cleanly, releases the transport and resets every field.
    pub fn finalize(&mut self, now: u64) {
        if self.mode != mode::NORMAL && self.seq.state == SeqState::Running {
            self.seq.cancel = true;
            for _ in 0..4 {
                self.run_sequencer(now);
                if self.seq.finished() {
                    break;
                }
            }
        }
        let (cfg, index, append_sub) = (self.cfg.clone(), self.index, self.append_sub);
        *self = Self::new(index, cfg, append_sub);
    }

    /// `0x14024DE80`.
    pub fn reset_outputs(&mut self, force: bool) {
        if !(self.cfg.auto_output_zero || force) || self.mode != mode::NORMAL || !self.error.is_empty() {
            return;
        }
        if self.cfg.funcs.general_output.is_some() {
            self.gout = Some((8, 0));
        }
        if let Some(slots) = self.cfg.funcs.pwm_output {
            for duty in self.pwm_duty.iter_mut().take(slots.min(8) as usize) {
                *duty = 0;
            }
            self.pwm_pending = true;
        }
    }

    /// Progress of the update (`Node::progress`).
    pub fn progress(&self) -> u32 {
        if !self.settled() {
            return 0;
        }
        match self.mode {
            mode::NORMAL => 100,
            mode::FIRMWARE => self.seq.progress,
            _ => 0,
        }
    }

    /// `Node::isUpdateFinished`.
    pub fn is_update_finished(&self) -> bool {
        self.settled()
            && (self.mode == mode::NORMAL
                || (self.mode == mode::FIRMWARE && (self.seq.finished() || self.board_mode != 0)))
    }

    /// The node reached its steady state or has no board.
    pub fn is_settled(&self) -> bool {
        self.mode == mode::RESET || self.settled()
    }

    /// `Node::updateFirmware` (`0x14024DC40`): `false` when rejected.
    pub fn update_firmware(&mut self, file: Option<Rc<FirmwareFile>>) -> bool {
        if !self.settled() || self.mode != mode::FIRMWARE || !self.error.is_empty() {
            return false;
        }
        match file {
            Some(file) => self.seq.start(vec![Cmd::Delay, Cmd::UpdateFirmware, Cmd::Reset], Some(file)),
            None => {
                self.seq.start(vec![Cmd::Delay, Cmd::Reset], None);
                self.seq.progress = 100;
            }
        }
        true
    }

    // --- facade sends (1.6.2) ---

    fn can_send(&self) -> bool {
        self.settled() && self.mode == mode::NORMAL && self.error.is_empty()
    }

    fn arm_timeout(&mut self) {
        if self.cfg.comm_timeout.is_some() && self.timeout_state == 0 {
            self.timeout_state = 1;
        }
    }

    /// `Node::sendGeneralOutput`.
    pub fn send_general_output(&mut self, bits: u64, size: u64) -> bool {
        if !self.can_send() || self.cfg.funcs.general_output.is_none() || size == 0 {
            return false;
        }
        self.gout = Some((size.min(8) as usize, bits));
        self.arm_timeout();
        true
    }

    /// `Node::sendPwmOutput`.
    pub fn send_pwm_output(&mut self, slot: usize, duty: u8) -> bool {
        let Some(slots) = self.cfg.funcs.pwm_output else { return false };
        if !self.can_send() || self.cfg.pwm_cycle.is_none() || slot as u64 >= slots {
            return false;
        }
        if let Some(entry) = self.pwm_duty.get_mut(slot) {
            *entry = duty;
        }
        self.pwm_pending = true;
        self.arm_timeout();
        true
    }

    /// `Node::sendUniqueCommand`.
    pub fn send_unique_command(&mut self, code: u8, data: &[u8; 62]) -> bool {
        if !self.can_send() || !self.cfg.funcs.unique_io.iter().any(|u| u.command == code) {
            return false;
        }
        if let Some(entry) = self.ring.iter_mut().find(|(c, _)| *c == code) {
            entry.1 = *data;
        } else {
            if self.ring.len() == 9 {
                self.ring.remove(0);
            }
            self.ring.push((code, *data));
        }
        self.arm_timeout();
        true
    }

    // --- device conversation ---

    fn send(&mut self, cmd: u8, data: &[u8]) -> bool {
        let mut report: Report = [0; REPORT_LEN];
        report[0] = 0x10;
        report[1] = cmd;
        report[2..2 + data.len()].copy_from_slice(data);
        let Some(dev) = self.dev.as_mut() else { return false };
        let result = if cmd & 0x80 != 0 { dev.set_output_report(&report) } else { dev.write_interrupt(&report) };
        if result.is_err() {
            self.comm_ok = false;
        }
        result.is_ok()
    }

    /// `0x14024FED0`: drains the interrupt-IN pipe and decodes the newest report.
    fn receive(&mut self) -> bool {
        let Some(dev) = self.dev.as_mut() else { return false };
        let mut newest = None;
        for _ in 0..256 {
            match dev.read_interrupt() {
                Ok(Some(report)) => newest = Some(report),
                Ok(None) => break,
                Err(_) => return false,
            }
        }
        match newest {
            Some(report) if report[0] == 1 => {
                self.decode(&report);
                true
            }
            _ => false,
        }
    }

    fn decode(&mut self, report: &Report) {
        let p = &report[1..];
        let analog = (self.analog.count as usize).min(8);
        for k in 0..analog {
            self.analog.channels[k].value = u16::from_le_bytes([p[2 * k], p[2 * k + 1]]);
        }
        let rotary = (self.rotary.count as usize).min(4);
        for k in 0..rotary {
            self.rotary.values[k] = u16::from_le_bytes([p[16 + 2 * k], p[17 + 2 * k]]);
        }
        for j in 0..(self.coin.count as usize).min(2) {
            self.coin.entries[j] = UniqueDescriptor { flags: u16::from(p[24 + 2 * j]), code: u16::from(p[25 + 2 * j]) };
        }
        self.switch.system = p[29] & 3;
        let buttons = self.cfg.funcs.switch_input.map_or(0, |s| s.b);
        for k in 0..(self.switch.player_count as usize).min(4) {
            let mut bytes = [0u8; 8];
            bytes[0] = p[28 + 2 * k];
            bytes[1] = p[29 + 2 * k] & 0xFC;
            let edge = (buttons >> 3) as usize;
            if edge < 8 {
                bytes[edge] &= (0xFFu32 << (align8(buttons) - buttons)) as u8;
                for byte in &mut bytes[edge + 1..] {
                    *byte = 0;
                }
            }
            self.switch.player_masks[k] = u64::from_le_bytes(bytes);
        }
        self.system_status = p[32];
        self.usb_status = p[33];
        self.unique.copy_from_slice(&p[34..34 + UNIQUE_INPUT_DATA_LEN]);
    }

    fn send_pending(&mut self) {
        if let Some((len, data)) = self.gout.take() {
            let bits = self.cfg.funcs.general_output.unwrap_or(0).min(8 * len as u64);
            let count = bits.div_ceil(8) as usize;
            let mut bytes = data.to_le_bytes();
            if count > 0 {
                bytes[count - 1] &= (0xFFu32 << (align8(bits) - bits)) as u8;
                self.send(0x04, &bytes[..count]);
            }
        }
        if self.pwm_pending {
            self.pwm_pending = false;
            let mut data = [0u8; 10];
            data[0] = self.cfg.pwm_cycle.unwrap_or(1);
            data[1..9].copy_from_slice(&self.pwm_duty);
            self.send(0x05, &data);
        }
        for (code, data) in std::mem::take(&mut self.ring) {
            self.send(code, &data);
        }
    }

    fn steady(&mut self) {
        if self.timeout_state == 1 {
            let timeout = self.cfg.comm_timeout.unwrap_or(0);
            self.send(0x01, &[timeout]);
            self.timeout_state = 2;
        }
        self.send_pending();
    }

    fn handshake(&mut self) {
        match (self.phase, self.step) {
            (0, 1) => {
                self.send(0x03, &[]);
                self.counter_a.reset();
                (self.phase, self.step) = (1, 1);
            }
            (1, 1) => {
                self.counter_a.push(self.system_status != 0);
                if self.system_status == 0 {
                    (self.phase, self.step) = (0, 2);
                } else if self.counter_a.exceeded() {
                    self.raise(error::FUNCTION);
                    (self.phase, self.step) = (2, 0);
                }
            }
            (0, 2) => {
                self.send(0x01, &[0]);
                self.send(0x02, &[self.cfg.switch_sampling.unwrap_or(6)]);
                self.counter_a.reset();
                (self.phase, self.step) = (1, 2);
            }
            (1, 2) => {
                self.counter_a.push(self.system_status & 0x30 != 0x30);
                let done = self.system_status & 0x30 == 0x30;
                if !done && self.counter_a.exceeded() {
                    self.raise(error::FUNCTION);
                    (self.phase, self.step) = (2, 0);
                } else if done {
                    let outputs = self.cfg.funcs.general_output.is_some() || self.cfg.funcs.pwm_output.is_some();
                    (self.phase, self.step) = if outputs { (0, 3) } else { (2, 0) };
                }
            }
            (0, 3) => {
                if self.cfg.auto_output_zero && self.mode == mode::NORMAL && self.error.is_empty() {
                    self.reset_outputs(false);
                }
                self.send_pending();
                (self.phase, self.step) = (1, 3);
            }
            _ => (self.phase, self.step) = (2, 0),
        }
    }

    /// `0x14024DB50`, once per manager update.
    pub fn update(&mut self, now: u64) {
        if self.phase == 0 && self.step == 0 {
            return;
        }
        self.comm_ok = true;
        let ok = if self.mode == mode::NORMAL { self.receive() } else { true };
        self.comm_ok = self.comm_ok && ok;
        if self.comm_ok {
            if self.seq.state == SeqState::Running {
                self.run_sequencer(now);
            }
            if self.mode != mode::NORMAL {
                if self.seq.finished() {
                    (self.phase, self.step) = (2, 0);
                }
            } else if self.settled() {
                self.steady();
            } else {
                self.handshake();
            }
        }
        self.counter_b.push(!self.comm_ok);
        if self.counter_b.exceeded() && self.error.is_empty() {
            self.raise(error::NOT_FOUND);
            if !self.settled() {
                (self.phase, self.step) = (2, 0);
            }
        }
    }

    // --- sequencer ---

    fn firmware_tick(&mut self) -> SeqState {
        let Some(file) = self.seq.file.clone() else { return SeqState::Failed };
        let status = match self.dev.as_mut().map(|dev| dev.get_input_report()) {
            Some(Ok(r)) if r[0] == 1 && r[1] == 0x85 => r[2],
            _ => return SeqState::Running,
        };
        match status {
            0xE0 => return SeqState::Failed,
            0xF0 => {
                self.seq.progress = 100;
                return SeqState::Done;
            }
            0 => self.seq.lines_sent = 0,
            _ => {}
        }
        let total = file.lines.len();
        self.seq.progress = (100 * self.seq.lines_sent / total) as u32;
        if self.seq.progress >= 100 {
            return SeqState::Done;
        }
        let ok = if self.seq.cancel || status == 0 {
            self.send(0x85, &[0])
        } else {
            let line = file.lines[self.seq.lines_sent].as_bytes();
            if line.len() + 1 > 62 {
                return SeqState::Failed;
            }
            let mut data = vec![line.len() as u8];
            data.extend_from_slice(line);
            self.seq.lines_sent += 1;
            self.send(0x85, &data)
        };
        if !ok || self.seq.cancel { SeqState::Failed } else { SeqState::Running }
    }

    fn run_sequencer(&mut self, now: u64) {
        for _ in 0..8 {
            if self.seq.state != SeqState::Running {
                return;
            }
            let Some(&cmd) = self.seq.cmds.get(self.seq.index) else {
                self.seq.state = SeqState::Done;
                return;
            };
            if self.seq.cancel && cmd != Cmd::UpdateFirmware {
                self.seq.state = SeqState::Failed;
                return;
            }
            let result = if !self.seq.started {
                self.seq.started = true;
                self.seq.timer = now;
                match cmd {
                    Cmd::Delay => SeqState::Done,
                    Cmd::ChangeBootMode => self.start_timed(0x84, &[]),
                    Cmd::Reset => self.start_timed(0x88, &[0xD9]),
                    Cmd::UpdateFirmware => {
                        self.seq.progress = 0;
                        self.seq.lines_sent = 0;
                        if self.send(0x85, &[0]) { SeqState::Running } else { SeqState::Failed }
                    }
                }
            } else {
                match cmd {
                    Cmd::Delay => SeqState::Done,
                    Cmd::ChangeBootMode | Cmd::Reset if now.saturating_sub(self.seq.timer) >= SETTLE_MS => {
                        SeqState::Done
                    }
                    Cmd::ChangeBootMode | Cmd::Reset => SeqState::Running,
                    Cmd::UpdateFirmware => self.firmware_tick(),
                }
            };
            match result {
                SeqState::Running => return,
                SeqState::Done => {}
                _ if cmd == Cmd::UpdateFirmware => {
                    if self.error.is_empty() {
                        self.raise(error::FIRMWARE);
                    }
                }
                _ => {
                    self.seq.state = SeqState::Failed;
                    return;
                }
            }
            self.seq.index += 1;
            self.seq.started = false;
            if self.seq.index >= self.seq.cmds.len() {
                self.seq.state = SeqState::Done;
            }
        }
    }

    fn start_timed(&mut self, cmd: u8, data: &[u8]) -> SeqState {
        if self.send(cmd, data) { SeqState::Running } else { SeqState::Failed }
    }
}
