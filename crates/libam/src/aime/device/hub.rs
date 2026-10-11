//! The reader manager core (`spec/libam/aime.md` E.2, D.6): per-port command queue with one outstanding command,
//! response routing by `dst`, command timeouts and the unit state every stepper works on.

use std::collections::VecDeque;
use std::rc::Rc;
use std::time::Duration;

use super::wire::{self, Assembler, Response, cmd};
use super::{Card, CardKind, FollowUp, ReaderConfig, ReaderInfo, SendError};
use backend::{SerialPort, SerialPorts, SerialSettings};

/// Queue capacity (`add` fails above 29 queued entries).
const QUEUE_MAX: usize = 30;
/// A sent head command that did not complete within this time clears the queue (D.6.2 slot deadline).
const SLOT_TIMEOUT_MS: u64 = 1950;

/// A frame waiting for or in flight on a port.
struct Queued {
    frame: Vec<u8>,
    cmd: u8,
    /// Wire unit of the frame (`dst`, minus 8 for the extension board commands).
    unit: u8,
    /// Unit slot that waits for the response; `None` for LED frames (fire and forget).
    owner: Option<usize>,
    sent_at: Option<u64>,
}

/// One COM port (`0x41E0` byte port context).
struct Port {
    com: u16,
    open: bool,
    line: Option<Box<dyn SerialPort>>,
    asm: Assembler,
    seq: u8,
    queue: VecDeque<Queued>,
}

/// The state of one reader unit (`cAiMeNFCRW`, E.3.1).
pub(super) struct Unit {
    pub id: u32,
    pub port: usize,
    /// Wire unit address (`unit & 7`).
    pub addr: u8,
    pub config: ReaderConfig,
    /// Driver active (`+308`): commands other than LED colours are refused when clear.
    pub active: bool,
    /// Reader initialised (`+311`).
    pub initialised: bool,
    /// Unit error (`+305`).
    pub unit_error: bool,
    /// Stop requested (`+310`).
    pub stop: bool,
    /// rw flag bit of the port (`+16856`).
    pub rw: bool,
    /// LED queue cancelled (`+16850` family): only "all off" is sent.
    pub led_cancelled: bool,
    /// Response of the command in flight, delivered by the pump.
    pub response: Option<Response>,
    /// A command of this unit is queued or outstanding.
    pub pending: bool,
    /// Response timer duration armed for the next send (`+41908`).
    pub timer_ms: u32,
    /// Response deadline (`+344`), set when the armed command is sent.
    pub deadline: u64,
    /// The timer is running (`+41907`).
    pub timer_armed: bool,
    /// A failed enqueue arms an immediate timeout ("SEND COMMAND FAULT").
    pub forced_expiry: bool,
    /// Pseudo-random state for the FeliCa Lite challenge.
    pub rng: u64,
    pub info: ReaderInfo,

    // init sequence (`[324]`)
    pub init_state: u8,

    // polling (`[328]` and friends)
    pub seq: u8,
    pub filter: u8,
    pub amiibo_mode: bool,
    pub polling_started: bool,
    pub poll_again: bool,
    pub start_time: u64,
    pub restart_at: u64,
    pub scan_deadline: u64,
    pub has_time_limit: bool,
    pub post_stop_deadline: u64,
    pub detect_retry: u8,
    pub retry_counter: u8,
    pub key_index: u8,
    pub auth_kind: u8,
    pub card_count: u32,
    pub current: usize,
    pub cards: [Card; 2],
    pub block1: [u8; 16],
    pub follow_up: Option<FollowUp>,
    /// FeliCa step to run once polling stopped (`[41948]`): the first SEQ of the flow, 0 for none.
    pub felica_follow: u8,
    pub felica_class: u8,
    pub felica_error: u32,
    pub felica_retry: u8,
    pub mobile_os: u8,
    pub rc: [u8; 16],
    /// The FeliCa Lite ID block 0x82 (`+41930`).
    pub id_block: [u8; 16],

    // firmware (`[316]`, `[320]`)
    pub fw_state: u8,
    pub led_state: u8,
    pub force: bool,
    pub hex_lines: Vec<String>,
    pub file_hw: String,
    pub file_fw: String,
    pub images: [Option<Vec<u8>>; 2],
    /// File version bytes `[41300 + generation]`: LED/gen 1 at 1, gen 2 at 2, gen 3 at 3.
    pub file_version: [u8; 4],
    pub fw_index: u32,
    pub fw_restarted: bool,
    pub sleep_until: u64,
}

impl Unit {
    fn new(id: u32, port: usize, config: ReaderConfig, seed: u64) -> Self {
        Self {
            id,
            port,
            addr: ((id / 1000) & 7) as u8,
            config,
            active: true,
            initialised: false,
            unit_error: false,
            stop: false,
            rw: false,
            led_cancelled: false,
            response: None,
            pending: false,
            timer_ms: 0,
            deadline: 0,
            timer_armed: false,
            forced_expiry: false,
            rng: seed | 1,
            info: ReaderInfo { led_board_name: "No LED Board".into(), ..ReaderInfo::default() },
            init_state: 0,
            seq: 0,
            filter: 0,
            amiibo_mode: false,
            polling_started: false,
            poll_again: false,
            start_time: 0,
            restart_at: 0,
            scan_deadline: 0,
            has_time_limit: false,
            post_stop_deadline: 0,
            detect_retry: 0,
            retry_counter: 0,
            key_index: 0,
            auth_kind: 0,
            card_count: 0,
            current: 0,
            cards: [blank_card(), blank_card()],
            block1: [0; 16],
            follow_up: None,
            felica_follow: 0,
            felica_class: 0,
            felica_error: 0,
            felica_retry: 0,
            mobile_os: 0,
            rc: [0; 16],
            id_block: [0; 16],
            fw_state: 0,
            led_state: 0,
            force: false,
            hex_lines: Vec::new(),
            file_hw: String::new(),
            file_fw: String::new(),
            images: [None, None],
            file_version: [0xFF; 4],
            fw_index: 0,
            fw_restarted: false,
            sleep_until: 0,
        }
    }

    /// Arms the response timer for the next command (`0x14034B5A0`).
    pub fn arm(&mut self, ms: u32) {
        self.timer_ms = ms;
        self.timer_armed = false;
        self.forced_expiry = false;
    }

    /// The armed timer ran out (`0x140348850`).
    pub fn expired(&self, now: u64) -> bool {
        self.forced_expiry || (self.timer_armed && now > self.deadline)
    }

    /// The next pseudo-random byte (xorshift64*).
    pub fn random_byte(&mut self) -> u8 {
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        (self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 56) as u8
    }
}

/// A fresh card record (`kind` 4 = detect failure is the initial value in the original; the stepper resets it).
pub(super) fn blank_card() -> Card {
    Card { name: [b' '; 4], kind: CardKind::None, ..Card::default() }
}

/// The reader manager.
pub(super) struct Hub {
    serial: Rc<dyn SerialPorts>,
    baud: u32,
    ports: Vec<Port>,
    pub units: Vec<Unit>,
    /// A command was sent since the manager was created (`byte_1409CFD48`).
    pub any_sent: bool,
}

impl Hub {
    pub fn new(serial: Rc<dyn SerialPorts>, high_baud: bool) -> Self {
        Self {
            serial,
            baud: if high_baud { 115_200 } else { 38_400 },
            ports: Vec::new(),
            units: Vec::new(),
            any_sent: false,
        }
    }

    /// 8N1, no flow control, DTR asserted, a one-second write timeout (`spec/libam/aime.md` D.2).
    fn settings(&self) -> SerialSettings {
        SerialSettings {
            baud: self.baud,
            dtr: true,
            write_timeout: Duration::from_millis(1000),
            ..SerialSettings::default()
        }
    }

    /// Opens the distinct ports; returns `(opened, failed)`.
    pub fn open(&mut self, ids: &[u32]) -> (Vec<u16>, Vec<u16>) {
        let mut opened = Vec::new();
        let mut failed = Vec::new();
        let settings = self.settings();
        for id in ids {
            let com = (id % 1000) as u16;
            if self.ports.iter().any(|port| port.com == com) {
                continue;
            }
            let line = self.serial.open(com, &settings).ok();
            let open = line.is_some();
            self.ports.push(Port { com, open, line, asm: Assembler::default(), seq: 0, queue: VecDeque::new() });
            if open {
                opened.push(com);
            } else {
                failed.push(com);
            }
        }
        (opened, failed)
    }

    /// Binds (or rebinds) the unit of `id`; `None` when its port is not open.
    pub fn bind(&mut self, id: u32, mut config: ReaderConfig) -> Option<usize> {
        let com = (id % 1000) as u16;
        let port = self.ports.iter().position(|port| port.com == com && port.open)?;
        config.command_timeout_ms = config.command_timeout_ms.clamp(1000, 1900);
        config.board_timeout_ms = config.board_timeout_ms.max(2000);
        let seed = u64::from(id).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let unit = Unit::new(id, port, config, seed);
        match self.units.iter().position(|unit| unit.id == id) {
            Some(index) => {
                let rng = self.units[index].rng;
                self.units[index] = Unit { rng, ..unit };
                Some(index)
            }
            None => {
                self.units.push(unit);
                Some(self.units.len() - 1)
            }
        }
    }

    /// Closes every port (`0x140344440`); the original's bit mask is 0 for a clean close.
    pub fn close(&mut self) -> u32 {
        self.ports.clear();
        self.units.clear();
        0
    }

    /// Queues a request of unit `u` (`0x140349510`). Commands from `0x80` take the extension board selector from the
    /// first payload byte (`0xFF` -> `0x18`).
    pub fn send(&mut self, u: usize, command: u8, payload: &[u8]) -> Result<(), SendError> {
        let unit = &self.units[u];
        if !unit.active && command != cmd::LED_COLOUR {
            return Err(SendError::Inactive);
        }
        let (dst, body) = if command >= 0x80 {
            let selector = payload.first().copied().unwrap_or(0xFF);
            (if selector == 0xFF { 0x18 } else { wire::dst_for(command, selector) }, payload.get(1..).unwrap_or(&[]))
        } else {
            (unit.addr, payload)
        };
        let wire_unit = if command >= 0x80 { dst - 8 } else { dst };
        // 0x80 / 0x81 / 0x82 are LED frames: no reply is awaited. Only the colour frames replace a queued twin.
        let fire_and_forget = matches!(command, 0x80..=0x82);
        let led = command == 0x80 || command == cmd::LED_COLOUR;
        let owner = (!fire_and_forget).then_some(u);
        let port = &mut self.ports[unit.port];
        port.seq = port.seq.wrapping_add(1);
        let frame = wire::stuff(&wire::request(dst, port.seq, command, body));
        self.any_sent = true;
        if led
            && let Some(entry) = port
                .queue
                .iter_mut()
                .skip(1)
                .find(|entry| entry.cmd == command && entry.unit == wire_unit && entry.sent_at.is_none())
        {
            entry.frame = frame;
            return Ok(());
        }
        if port.queue.len() >= QUEUE_MAX {
            return Err(SendError::QueueFull);
        }
        port.queue.push_back(Queued { frame, cmd: command, unit: wire_unit, owner, sent_at: None });
        if owner.is_some() {
            self.units[u].pending = true;
        }
        Ok(())
    }

    /// Sends with the response timer armed to `ms`; a refused send arms an immediate timeout.
    pub fn send_armed(&mut self, u: usize, ms: u32, command: u8, payload: &[u8]) {
        self.units[u].arm(ms);
        if self.send(u, command, payload).is_err() {
            self.units[u].forced_expiry = true;
        }
    }

    /// Takes the pending response when it answers `command`.
    pub fn take_response(&mut self, u: usize, command: u8) -> Option<Response> {
        let unit = &mut self.units[u];
        if unit.response.as_ref().is_some_and(|response| response.cmd == command) { unit.response.take() } else { None }
    }

    /// Removes the queued or outstanding entries of `command` for unit `u` (`0x140347230`).
    pub fn clear_command(&mut self, u: usize, command: u8) {
        let port = self.units[u].port;
        self.ports[port].queue.retain(|entry| !(entry.owner == Some(u) && entry.cmd == command));
        self.units[u].pending = self.ports[port].queue.iter().any(|entry| entry.owner == Some(u));
    }

    /// Drops everything queued for unit `u` and the unit's rw flag.
    pub fn clear_unit(&mut self, u: usize) {
        let port = self.units[u].port;
        self.ports[port].queue.retain(|entry| entry.owner != Some(u));
        let unit = &mut self.units[u];
        unit.pending = false;
        unit.rw = false;
    }

    /// One manager cycle for every open port: send the queue head, drain the line, dispatch, time out.
    pub fn pump(&mut self, now: u64) {
        for index in 0..self.ports.len() {
            if self.ports[index].open {
                self.pump_port(index, now);
            }
        }
    }

    fn pump_port(&mut self, index: usize, now: u64) {
        // An LED head that was sent is popped on the next scan.
        while self.ports[index].queue.front().is_some_and(|head| head.owner.is_none() && head.sent_at.is_some()) {
            self.ports[index].queue.pop_front();
        }
        self.send_head(index, now);
        self.receive(index, now);
        self.time_out(index, now);
    }

    fn send_head(&mut self, index: usize, now: u64) {
        if self.ports[index].queue.front().is_none_or(|head| head.sent_at.is_some()) {
            return;
        }
        let frame = self.ports[index].queue.front().expect("queue head").frame.clone();
        let mut line = self.ports[index].line.take();
        let result = match line.as_mut() {
            Some(port) => write_frame(port.as_mut(), &frame),
            None => Err(std::io::Error::new(std::io::ErrorKind::NotConnected, "COM port not open")),
        };
        self.ports[index].line = line;
        match result {
            Ok(()) => {
                let head = self.ports[index].queue.front_mut().expect("queue head");
                head.sent_at = Some(now);
                if let Some(owner) = head.owner {
                    let unit = &mut self.units[owner];
                    unit.deadline = now + u64::from(unit.timer_ms);
                    unit.timer_armed = true;
                }
            }
            Err(_) => self.fail_port(index),
        }
    }

    fn receive(&mut self, index: usize, now: u64) {
        let mut line = self.ports[index].line.take();
        let mut responses = Vec::new();
        let mut failed = false;
        if let Some(port) = line.as_mut() {
            loop {
                let waiting = match port.bytes_to_read() {
                    Ok(count) => count,
                    Err(_) => {
                        failed = true;
                        break;
                    }
                };
                if waiting == 0 {
                    break;
                }
                let mut buf = vec![0u8; waiting.min(512)];
                match port.read(&mut buf) {
                    Ok(0) => break,
                    Ok(count) => responses.extend(self.ports[index].asm.push(&buf[..count])),
                    Err(_) => {
                        failed = true;
                        break;
                    }
                }
            }
        }
        self.ports[index].line = line;
        if failed {
            self.fail_port(index);
            return;
        }
        for response in responses {
            self.dispatch(index, response, now);
        }
    }

    /// Accepts a response when it answers the sent head (cmd and unit match), else drops it ("DIFFICULT COMMAND").
    fn dispatch(&mut self, index: usize, response: Response, now: u64) {
        let port = &mut self.ports[index];
        let Some(head) = port.queue.front() else { return };
        let unit_of_response = if response.cmd >= 0x80 { response.dst.wrapping_sub(8) } else { response.dst };
        if head.sent_at.is_none() || head.cmd != response.cmd || head.unit != unit_of_response {
            return;
        }
        let Some(owner) = head.owner else { return };
        port.queue.pop_front();
        if response.status == 3 && response.cmd != 0x70 && response.cmd != 0x71 {
            self.reset_port(index, now);
        }
        let unit = &mut self.units[owner];
        unit.pending = false;
        unit.response = Some(response);
    }

    /// Status 3 ("COMPORT INITIALIZE"): recreate the port and clear the initialised flag of its units.
    fn reset_port(&mut self, index: usize, _now: u64) {
        self.ports[index].line = None;
        let com = self.ports[index].com;
        let settings = self.settings();
        let line = self.serial.open(com, &settings).ok();
        self.ports[index].open = line.is_some();
        self.ports[index].line = line;
        self.ports[index].asm = Assembler::default();
        for unit in self.units.iter_mut().filter(|unit| unit.port == index) {
            unit.initialised = false;
        }
    }

    /// The sent head did not complete within the slot deadline: every waiting unit gets the error flag and the queue
    /// is cleared (there is no resend, D.6.2).
    fn time_out(&mut self, index: usize, now: u64) {
        let port = &self.ports[index];
        let Some(head) = port.queue.front() else { return };
        let late = head.owner.is_some() && head.sent_at.is_some_and(|sent| now > sent + SLOT_TIMEOUT_MS);
        if late {
            self.fail_port(index);
        }
    }

    /// Clears the queue of a port and flags all waiting units.
    fn fail_port(&mut self, index: usize) {
        let port = &mut self.ports[index];
        for entry in port.queue.drain(..) {
            if let Some(owner) = entry.owner {
                let unit = &mut self.units[owner];
                unit.unit_error = true;
                unit.pending = false;
                unit.rw = false;
            }
        }
    }
}

/// The printable ASCII of a block, anything else as a space (`rec+16`).
pub(super) fn printable(bytes: &[u8]) -> [u8; 4] {
    let mut name = [b' '; 4];
    for (slot, &byte) in name.iter_mut().zip(bytes) {
        if (0x20..0x7F).contains(&byte) {
            *slot = byte;
        }
    }
    name
}

/// Writes a frame and waits up to a second for the port to accept it.
fn write_frame(port: &mut dyn SerialPort, frame: &[u8]) -> std::io::Result<()> {
    port.write(frame)?;
    match port.drain(Duration::from_millis(1000)) {
        Ok(true) => Ok(()),
        Ok(false) => Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "serial write timed out")),
        Err(error) => Err(error),
    }
}
