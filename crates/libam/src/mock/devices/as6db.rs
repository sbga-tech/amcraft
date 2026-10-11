//! A simulated AS-6DB on a simulated COM port: the device side of the protocol of `credit.md` part II 3.4 to 3.6,
//! built from the same framing functions, so a test can compare the bytes the driver puts on the wire with the
//! spec's vectors and script coins, faults and a silent device.

use std::any::Any;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::io;
use std::rc::Rc;

use crate::credit::as6db::frame::{self, Key};
use crate::mock::serial::SerialDevice;
use backend::serial::{SerialPort, SerialPorts, SerialSettings};

/// Coin kinds the selector counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Yen {
    /// 10 yen.
    Ten,
    /// 100 yen.
    Hundred,
    /// 500 yen.
    FiveHundred,
}

/// One frame on the simulated wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireFrame {
    /// `true` for a frame the driver sent, `false` for the device's answer.
    pub from_master: bool,
    /// The bytes on the wire (scrambled where the protocol scrambles).
    pub bytes: Vec<u8>,
}

#[derive(Debug)]
struct State {
    /// COM port number that exists; `None` = the port is missing.
    port: Option<u16>,
    queue: VecDeque<u8>,
    address: u8,
    scrambled: bool,
    key_state: u16,
    device_seq: u8,
    setup: [u8; 17],
    lockout: bool,
    event_counter: u8,
    coins: [u8; 4],
    reported: [u8; 4],
    cond: u8,
    err: u8,
    answering: bool,
    wire: Vec<WireFrame>,
    opens: u32,
}

/// The simulated selector; clones share the device.
#[derive(Debug, Clone)]
pub struct SimAs6db(Rc<RefCell<State>>);

impl Default for SimAs6db {
    fn default() -> Self {
        Self::new()
    }
}

impl SimAs6db {
    /// A selector at bus address 2 on COM port 1, not yet keyed.
    pub fn new() -> Self {
        Self::on_port(1)
    }

    /// A selector at bus address 2 on `com`, not yet keyed.
    pub fn on_port(com: u16) -> Self {
        let device = Self::new_state();
        device.0.borrow_mut().port = Some(com);
        device
    }

    fn new_state() -> Self {
        Self(Rc::new(RefCell::new(State {
            port: Some(1),
            queue: VecDeque::new(),
            address: 2,
            scrambled: false,
            key_state: 0,
            device_seq: 0x10,
            setup: [0; 17],
            lockout: false,
            event_counter: 0,
            coins: [0; 4],
            reported: [0; 4],
            cond: 0,
            err: 0,
            answering: true,
            wire: Vec::new(),
            opens: 0,
        })))
    }

    /// The COM port that exists (`None`: opening always fails).
    pub fn port(&self) -> Option<u16> {
        self.0.borrow().port
    }

    fn open_link(&self) -> io::Result<Box<dyn SerialPort>> {
        let mut state = self.0.borrow_mut();
        if state.port.is_none() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no such COM port"));
        }
        state.opens += 1;
        state.queue.clear();
        drop(state);
        Ok(Box::new(SimLink(self.clone())))
    }

    /// Which COM port exists (`None`: opening always fails, as for a missing or unplugged adapter).
    pub fn set_port(&self, port: Option<u16>) {
        self.0.borrow_mut().port = port;
    }

    /// Whether the device answers at all.
    pub fn set_answering(&self, answering: bool) {
        self.0.borrow_mut().answering = answering;
    }

    /// A coin passes the selector: it is counted until the master acknowledges the next STATUS.
    pub fn insert_coin(&self, coin: Yen) {
        let mut state = self.0.borrow_mut();
        let slot = match coin {
            Yen::Ten => 0,
            Yen::Hundred => 2,
            Yen::FiveHundred => 3,
        };
        state.coins[slot] = state.coins[slot].wrapping_add(1);
        state.event_counter = state.event_counter.wrapping_add(1);
    }

    /// Sets the STATUS COND (bits 4, 5, 7...) and ERR bytes; the lockout bit of COND follows CONTROL.
    pub fn set_faults(&self, cond: u8, err: u8) {
        let mut state = self.0.borrow_mut();
        state.cond = cond & !1;
        state.err = err;
        state.event_counter = state.event_counter.wrapping_add(1);
    }

    /// The lockout bit the device currently holds.
    pub fn lockout(&self) -> bool {
        self.0.borrow().lockout
    }

    /// The SETUP payload the device stored (`None` before the first SETUP).
    pub fn setup(&self) -> Option<[u8; 17]> {
        let state = self.0.borrow();
        (state.setup != [0; 17]).then_some(state.setup)
    }

    /// Every frame on the wire, oldest first.
    pub fn wire(&self) -> Vec<WireFrame> {
        self.0.borrow().wire.clone()
    }

    /// How many times the port was opened.
    pub fn opens(&self) -> u32 {
        self.0.borrow().opens
    }
}

impl State {
    /// Queues the device's answer: `body` is `code, data...`.
    fn respond(&mut self, body: &[u8]) {
        if !self.answering {
            return;
        }
        let seq = self.device_seq;
        self.device_seq = self.device_seq.wrapping_add(1);
        let key = if self.scrambled { Key::Scrambled(&mut self.key_state) } else { Key::Plain };
        let mut bytes = frame::build(frame::MASTER, seq, body, key);
        bytes[3] = self.address;
        self.queue.extend(bytes.iter().copied());
        self.wire.push(WireFrame { from_master: false, bytes });
    }

    fn ack(&mut self, echo: u8) {
        self.respond(&[frame::RESP_ACK, echo, 0, 0]);
    }

    /// A frame the master sent.
    fn receive(&mut self, bytes: &[u8]) {
        self.wire.push(WireFrame { from_master: true, bytes: bytes.to_vec() });
        if bytes.len() < 7 || bytes[..2] != [frame::SYNC0, frame::SYNC1] || bytes[3] != frame::MASTER {
            return;
        }
        if bytes[2] != self.address {
            return;
        }
        let mut frame_bytes = bytes.to_vec();
        if bytes[4] == 0 && bytes.len() == 9 {
            // NEW_SEED: plain frame carrying the seed, the answer already uses the new key.
            let seed = u16::from_le_bytes([bytes[6], bytes[7]]);
            if frame::new_seed(self.address, bytes[5], seed) != bytes {
                return;
            }
            self.scrambled = seed != 0;
            self.key_state = seed.swap_bytes();
            self.ack(bytes[5]);
            return;
        }
        let opened = if self.scrambled {
            frame::open(&mut frame_bytes, Some(&mut self.key_state))
        } else {
            frame::open(&mut frame_bytes, None)
        };
        if !opened {
            return;
        }
        let seq = frame_bytes[5];
        let body = &frame_bytes[6..frame_bytes.len() - 1];
        match body.first().copied() {
            Some(frame::CMD_SETUP) if body.len() == 18 => {
                self.setup.copy_from_slice(&body[1..]);
                self.ack(seq);
            }
            Some(frame::CMD_CONTROL) if body.len() == 3 => {
                self.lockout = body[1] & 1 != 0;
                self.ack(seq);
            }
            Some(frame::CMD_INFO) => {
                let mut data = vec![frame::RESP_INFO, 0x6D, 0x01, 0, 0, 0, 1, 0x01, 0x00];
                data.extend_from_slice(&self.setup[1..]);
                self.respond(&data);
            }
            Some(frame::CMD_STATUS) => {
                self.reported = self.coins;
                let cond = self.cond | u8::from(self.lockout);
                let [c10, c50, c100, c500] = self.coins;
                self.respond(&[frame::RESP_STATUS, self.event_counter, 0, 0, c10, c50, c100, c500, cond, self.err]);
            }
            Some(frame::CMD_ACK) => {
                for (coin, reported) in self.coins.iter_mut().zip(self.reported) {
                    *coin = coin.wrapping_sub(reported);
                }
                self.reported = [0; 4];
            }
            _ => {}
        }
    }
}

impl SerialPorts for SimAs6db {
    fn open(&self, com: u16, _settings: &SerialSettings) -> io::Result<Box<dyn SerialPort>> {
        if self.port() != Some(com) {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no such COM port"));
        }
        self.open_link()
    }
}

impl SerialDevice for SimAs6db {
    fn open(&self, _settings: &SerialSettings) -> io::Result<Box<dyn SerialPort>> {
        self.open_link()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
struct SimLink(SimAs6db);

impl SerialPort for SimLink {
    fn bytes_to_read(&mut self) -> io::Result<usize> {
        Ok((self.0).0.borrow().queue.len())
    }

    fn bytes_to_write(&mut self) -> io::Result<usize> {
        Ok(0)
    }

    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut state = (self.0).0.borrow_mut();
        let count = buf.len().min(state.queue.len());
        for (slot, byte) in buf.iter_mut().zip(state.queue.drain(..count)) {
            *slot = byte;
        }
        Ok(count)
    }

    fn write(&mut self, data: &[u8]) -> io::Result<()> {
        (self.0).0.borrow_mut().receive(data);
        Ok(())
    }

    fn drain(&mut self, _timeout: std::time::Duration) -> io::Result<bool> {
        Ok(true)
    }

    fn purge(&mut self) -> io::Result<()> {
        (self.0).0.borrow_mut().queue.clear();
        Ok(())
    }
}
