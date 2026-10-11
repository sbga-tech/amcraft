//! The AS-6DB driver: one board (bus address 2), the non-blocking transfer state machine, the command
//! dispatcher and the response handlers (`credit.md` part II 3.5 to 3.11).
//!
//! The driver is clocked by the caller: [`Driver::frame`] is called once per libam frame with the millisecond
//! tick and runs at most two transfer steps. RESET, the three broadcasts and the "priority" requests are never
//! issued by the credit module (3.8), so only NEW_SEED, SETUP, CONTROL, INFO and STATUS exist here.

use super::frame::{self, CMD_ACK, CMD_CONTROL, CMD_INFO, CMD_STATUS, Key, RESP_ACK, RESP_INFO, RESP_NAK, RESP_STATUS};
use super::frame::{RxKey, RxVerdict};
use backend::{SerialPort, SerialPorts, SerialSettings};

/// Bus address of the single board.
pub const BOARD_ADDRESS: u8 = 2;
/// Connect timeout and busy timeout (ms).
const CONNECT_TIMEOUT: u32 = 4000;
/// Busy countdown (ms).
const BUSY_TIMEOUT: u32 = 4000;
/// Wait after a failed open (ms).
const REOPEN_BACKOFF: u32 = 2000;
/// Start delay before the first dispatch (ms).
const START_DELAY: u32 = 1000;
/// Automatic re-key interval (ms).
const REKEY_INTERVAL: u32 = 600_000;
/// Longest wait for the transmit queue to drain (ms).
const TX_WAIT: u32 = 40;
/// Longest wait for the first response byte (ms).
const RX_START_WAIT: u32 = 100;
/// Longest silence inside a response (ms).
const RX_IDLE_WAIT: u32 = 20;
/// SETUP OUTPUT byte: bit 0 and the five pulse outputs.
const SETUP_OUTPUT: u8 = 0x7D;
/// SETUP TIMEOUT1 and TIMEOUT2.
const SETUP_TIMEOUTS: [u8; 2] = [10, 50];

/// What the driver sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cmd {
    NewSeed,
    Setup,
    Control,
    Info,
    Status,
}

impl Cmd {
    /// Response length the validator expects (a 4-byte NAK is always accepted).
    fn response_len(self) -> u8 {
        match self {
            Self::NewSeed | Self::Setup | Self::Control => 4,
            Self::Info => 25,
            Self::Status => 10,
        }
    }

    /// INFO and STATUS data are acknowledged by the master.
    fn acknowledges(self) -> bool {
        matches!(self, Self::Info | Self::Status)
    }
}

/// The transfer sub-states, numbered as in `credit.md` 3.7.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Start = 0,
    WaitSendStart = 1,
    Send = 2,
    WaitSendEnd = 3,
    WaitRecvStart = 4,
    Recv = 5,
    WaitAckStart = 6,
    SendAck = 7,
    WaitAckEnd = 8,
}

enum Poll {
    Pending,
    Success,
    Failure,
}

/// The ten fault conditions the special device watches (`0x140403F70`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Faults {
    /// The board never answered within the connect timeout.
    pub comm_timeout: bool,
    /// The board reported BUSY for the whole busy timeout.
    pub busy_timeout: bool,
    /// A coin was rejected (rising edge of ERR bit 0).
    pub rejected: bool,
    /// COND bit 4.
    pub hardware_fail: bool,
    /// COND bit 5.
    pub setup_fail: bool,
    /// ERR bit 1.
    pub reject_fail: bool,
    /// ERR bit 2.
    pub accept_coin_fail: bool,
    /// ERR bit 3.
    pub accept_stocked_coin_fail: bool,
    /// ERR bit 4.
    pub coin_jam: bool,
    /// ERR bit 5.
    pub reverse: bool,
}

/// Coins counted by the STATUS processed this frame (income mode), zero in every other frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// 10 yen coins.
    pub yen_10: u32,
    /// 100 yen coins.
    pub yen_100: u32,
    /// 500 yen coins.
    pub yen_500: u32,
}

/// Requests still to be sent, each 0..=2 (`B[130..140]`).
#[derive(Debug, Clone, Copy, Default)]
struct Pending {
    new_seed: u8,
    control: u8,
    control_reject: u8,
    control_clear: u8,
    control_verify: u8,
    setup: u8,
    setup_verify: u8,
    info_after: u8,
}

fn bump(counter: &mut u8) {
    *counter = (*counter + 1).min(2);
}

fn drop_one(counter: &mut u8) {
    *counter = counter.saturating_sub(1);
}

/// The one board record.
#[derive(Debug)]
struct Board {
    online: bool,
    cmd: Option<Cmd>,
    stage: Stage,
    /// Time of the last transfer activity.
    t: u32,
    connect_left: u32,
    fresh: u32,
    rekey_left: u32,
    rx: Vec<u8>,
    tx: Vec<u8>,
    tx_seed: u16,
    rx_seed: u16,
    state: u16,
    seq: u8,
    req_seq: u8,
    pending: Pending,
    setup: [u8; 17],
    control_flags: u8,
    control_coins: u8,
    default_control_coins: u8,
    resp_code: u8,
    resp_seq: u8,
    resp_flags: u8,
    status: [u8; 9],
    prev_status: [u8; 9],
    info: [u8; 24],
    rejected: bool,
    busy_grace: u32,
    busy_left: u32,
    stats: Stats,
}

impl Board {
    fn new(coins: u8) -> Self {
        let mut setup = [0; 17];
        setup[1] = coins;
        setup[2] = SETUP_OUTPUT;
        setup[15..17].copy_from_slice(&SETUP_TIMEOUTS);
        let mut status = [0; 9];
        status[0] = 0xFF;
        Self {
            online: false,
            cmd: None,
            stage: Stage::Start,
            t: 0,
            connect_left: CONNECT_TIMEOUT,
            fresh: 5,
            rekey_left: REKEY_INTERVAL,
            rx: Vec::new(),
            tx: Vec::new(),
            tx_seed: 0,
            rx_seed: 0,
            state: 0,
            seq: 0,
            req_seq: 0,
            pending: Pending { new_seed: 1, control: 1, setup: 1, info_after: 1, ..Pending::default() },
            setup,
            control_flags: 1,
            control_coins: 0,
            default_control_coins: 0,
            resp_code: 0,
            resp_seq: 0x7F,
            resp_flags: 0,
            status,
            prev_status: [0; 9],
            info: [0; 24],
            rejected: false,
            busy_grace: 5,
            busy_left: BUSY_TIMEOUT,
            stats: Stats::default(),
        }
    }

    fn cond(&self) -> u8 {
        self.status[7]
    }

    fn err(&self) -> u8 {
        self.status[8]
    }
}

/// The driver for the credit module's single selector.
#[derive(Debug)]
pub struct Driver {
    port: u16,
    link: Option<Box<dyn SerialPort>>,
    start_delay: u32,
    last_time: u32,
    seed_gen: u32,
    board: Board,
}

impl Driver {
    /// `0x140404910`: the board configured for the counted coin `types` (1 = 10 yen, 2 = 100 yen, 3 = 500 yen),
    /// started at tick `now` with the seed generator at `seed` (non-zero, low 16 bits non-zero).
    pub fn new(port: u16, types: &[u32], now: u32, seed: u32) -> Self {
        let mut coins = 0;
        for &kind in types {
            coins |= match kind {
                1 => 0x01,
                2 => 0x04,
                3 => 0x08,
                _ => 0,
            };
        }
        Self { port, link: None, start_delay: START_DELAY, last_time: now, seed_gen: seed, board: Board::new(coins) }
    }

    /// The statistics of this frame.
    pub fn stats(&self) -> Stats {
        self.board.stats
    }

    /// The fault conditions of board 0 (`0x140403F70`).
    pub fn faults(&self) -> Faults {
        let b = &self.board;
        Faults {
            comm_timeout: !b.online && b.connect_left == 0 && b.fresh == 0,
            busy_timeout: b.busy_grace == 0 && b.busy_left == 0,
            rejected: b.rejected,
            hardware_fail: b.cond() & 0x10 != 0,
            setup_fail: b.cond() & 0x20 != 0,
            reject_fail: b.err() & 0x02 != 0,
            accept_coin_fail: b.err() & 0x04 != 0,
            accept_stocked_coin_fail: b.err() & 0x08 != 0,
            coin_jam: b.err() & 0x10 != 0,
            reverse: b.err() & 0x20 != 0,
        }
    }

    /// The lockout bit of the last STATUS (`B[190]` bit 0).
    pub fn device_lockout(&self) -> bool {
        self.board.cond() & 1 != 0
    }

    /// `0x140407A10`: sets the lockout bit of the next CONTROL and queues it.
    pub fn set_lockout(&mut self, on: bool) {
        let b = &mut self.board;
        b.control_flags = (b.control_flags & !1) | u8::from(on);
        bump(&mut b.pending.control);
    }

    /// One libam frame (`0x140408660` / tick `0x1404086A0`).
    pub fn frame(&mut self, ports: &dyn SerialPorts, now: u32) {
        self.board.stats = Stats::default();
        if now == self.last_time {
            return;
        }
        let dt = now.wrapping_sub(self.last_time);
        self.last_time = now;
        if self.start_delay != 0 {
            if dt < self.start_delay {
                self.start_delay -= dt;
                return;
            }
            self.start_delay = 0;
        }
        if self.link.is_none() {
            // 9600 baud, 8N1, no flow control, DTR left alone, no write timeout (`credit.md` part II 3.3).
            match ports.open(self.port, &SerialSettings::default()) {
                Ok(link) => self.link = Some(link),
                Err(_) => {
                    self.start_delay = REOPEN_BACKOFF;
                    return;
                }
            }
        }
        let b = &mut self.board;
        b.connect_left = b.connect_left.saturating_sub(dt);
        b.fresh = b.fresh.saturating_sub(1);
        if self.seed_gen != 0 && b.rekey_left != 0 {
            b.rekey_left = b.rekey_left.saturating_sub(dt);
            if b.rekey_left == 0 {
                bump(&mut b.pending.new_seed);
            }
        }
        if b.fresh != 0 && b.connect_left != 0 && b.busy_left != 0 {
            b.busy_left = b.busy_left.saturating_sub(dt);
        }
        self.advance_seed();
        for _ in 0..2 {
            if !self.dispatch(now) {
                break;
            }
        }
    }

    /// `0x1404086A0` end: 16 Galois steps, repeated until the low half is non-zero.
    fn advance_seed(&mut self) {
        if self.seed_gen == 0 {
            return;
        }
        let mut s = self.seed_gen;
        loop {
            for _ in 0..16 {
                s = (s >> 1) ^ if s & 1 != 0 { 0xD000_0001 } else { 0 };
            }
            if s == 0 {
                s = 0xDEAD_BEEF;
            }
            if s & 0xFFFF != 0 {
                break;
            }
        }
        self.seed_gen = s;
    }

    /// `0x1404082C0`: continues the running command or picks the next one. `true` when a command completed.
    fn dispatch(&mut self, now: u32) -> bool {
        let cmd = match self.board.cmd {
            Some(cmd) => cmd,
            None => {
                let cmd = self.pick();
                self.board.cmd = Some(cmd);
                self.board.stage = Stage::Start;
                cmd
            }
        };
        match self.transfer(cmd, now) {
            Poll::Pending => false,
            Poll::Success => {
                let b = &mut self.board;
                b.online = true;
                b.fresh = 5;
                b.connect_left = CONNECT_TIMEOUT;
                b.cmd = None;
                self.acked(cmd);
                true
            }
            Poll::Failure => {
                let b = &mut self.board;
                b.online = false;
                b.cmd = None;
                if cmd != Cmd::NewSeed {
                    bump(&mut b.pending.new_seed);
                }
                true
            }
        }
    }

    /// The priority table of 3.8, reduced to the requests the credit module can raise.
    fn pick(&mut self) -> Cmd {
        let p = &mut self.board.pending;
        if p.new_seed != 0 {
            p.new_seed = 1;
            self.board.tx_seed = self.seed_gen as u16;
            return Cmd::NewSeed;
        }
        if p.setup_verify != 0 || p.control_verify != 0 {
            p.setup_verify = p.setup_verify.min(1);
            p.control_verify = p.control_verify.min(1);
            return Cmd::Status;
        }
        if p.setup != 0 {
            p.setup = 1;
            return Cmd::Setup;
        }
        if p.control != 0 {
            p.control = 1;
            p.control_reject = u8::from(p.control_reject != 0);
            p.control_clear = u8::from(p.control_clear != 0);
            return Cmd::Control;
        }
        if p.info_after != 0 {
            p.info_after = 1;
            return Cmd::Info;
        }
        Cmd::Status
    }

    fn key(&mut self) -> Key<'_> {
        if self.board.tx_seed == 0 { Key::Plain } else { Key::Scrambled(&mut self.board.state) }
    }

    fn next_seq(&mut self) -> u8 {
        self.board.seq = self.board.seq.wrapping_add(1);
        self.board.seq
    }

    fn build(&mut self, cmd: Cmd) -> Vec<u8> {
        let seq = self.next_seq();
        self.board.req_seq = seq;
        let b = &self.board;
        let (control, setup, tx_seed) = ([CMD_CONTROL, b.control_flags, b.control_coins], b.setup, b.tx_seed);
        match cmd {
            Cmd::NewSeed => frame::new_seed(BOARD_ADDRESS, seq, tx_seed).to_vec(),
            Cmd::Setup => frame::build(BOARD_ADDRESS, seq, &frame::setup_body(&setup), self.key()),
            Cmd::Control => frame::build(BOARD_ADDRESS, seq, &control, self.key()),
            Cmd::Info => frame::build(BOARD_ADDRESS, seq, &[CMD_INFO], self.key()),
            Cmd::Status => frame::build(BOARD_ADDRESS, seq, &[CMD_STATUS], self.key()),
        }
    }

    fn build_ack(&mut self) -> Vec<u8> {
        let seq = self.next_seq();
        let body = [CMD_ACK, self.board.resp_seq, self.board.resp_code, 0];
        frame::build(BOARD_ADDRESS, seq, &body, self.key())
    }

    /// `ClearCommError.cbOutQue != 0`; `Err` closes nothing by itself.
    fn tx_busy(&mut self) -> Result<bool, ()> {
        let link = self.link.as_mut().ok_or(())?;
        link.bytes_to_write().map(|n| n != 0).map_err(|_| ())
    }

    fn rx_waiting(&mut self) -> Result<bool, ()> {
        let link = self.link.as_mut().ok_or(())?;
        link.bytes_to_read().map(|n| n != 0).map_err(|_| ())
    }

    fn write(&mut self, frame: &[u8]) -> Result<(), ()> {
        let link = self.link.as_mut().ok_or(())?;
        link.write(frame).map_err(|_| ())
    }
    fn read_byte(&mut self) -> Result<u8, ()> {
        let link = self.link.as_mut().ok_or(())?;
        let mut byte = [0];
        match link.read(&mut byte) {
            Ok(1) => Ok(byte[0]),
            _ => Err(()),
        }
    }

    /// An I/O error: the port is closed (the next frame reopens it) and the transfer fails.
    fn close_fail(&mut self) -> Poll {
        self.link = None;
        Poll::Failure
    }

    /// `0x140404D30`: one step of the transfer.
    fn transfer(&mut self, cmd: Cmd, now: u32) -> Poll {
        loop {
            match self.board.stage {
                Stage::Start => {
                    let b = &mut self.board;
                    b.t = now;
                    b.rx.clear();
                    if let Some(link) = self.link.as_mut() {
                        let _ = link.purge();
                    }
                    self.board.stage = Stage::WaitSendStart;
                }
                Stage::WaitSendStart => match self.tx_busy() {
                    Err(()) => return self.close_fail(),
                    Ok(true) if now.wrapping_sub(self.board.t) > TX_WAIT => return self.close_fail(),
                    Ok(true) => return Poll::Pending,
                    Ok(false) => {
                        self.board.tx = self.build(cmd);
                        self.board.stage = Stage::Send;
                    }
                },
                Stage::Send => {
                    let frame = std::mem::take(&mut self.board.tx);
                    let sent = self.write(&frame);
                    self.board.tx = frame;
                    if sent.is_err() {
                        return self.close_fail();
                    }
                    self.board.t = now;
                    self.board.stage = Stage::WaitSendEnd;
                    return Poll::Pending;
                }
                Stage::WaitSendEnd => match self.tx_busy() {
                    Err(()) => return Poll::Failure,
                    Ok(true) if now.wrapping_sub(self.board.t) > TX_WAIT => return Poll::Failure,
                    Ok(true) => return Poll::Pending,
                    Ok(false) => {
                        self.board.t = now;
                        self.board.stage = Stage::WaitRecvStart;
                        return Poll::Pending;
                    }
                },
                Stage::WaitRecvStart => match self.rx_waiting() {
                    Err(()) => return self.close_fail(),
                    Ok(false) if now.wrapping_sub(self.board.t) > RX_START_WAIT => return Poll::Failure,
                    Ok(false) => return Poll::Pending,
                    Ok(true) => {
                        self.board.stage = Stage::Recv;
                        return Poll::Pending;
                    }
                },
                Stage::Recv => return self.receive(cmd, now),
                Stage::WaitAckStart => match self.tx_busy() {
                    Err(()) => return Poll::Failure,
                    Ok(true) if now.wrapping_sub(self.board.t) > TX_WAIT => return Poll::Failure,
                    Ok(true) => return Poll::Pending,
                    Ok(false) => {
                        self.board.tx = self.build_ack();
                        self.board.stage = Stage::SendAck;
                    }
                },
                Stage::SendAck => {
                    let frame = std::mem::take(&mut self.board.tx);
                    let sent = self.write(&frame);
                    self.board.tx = frame;
                    if sent.is_err() {
                        return self.close_fail();
                    }
                    self.board.t = now;
                    self.board.stage = Stage::WaitAckEnd;
                    return Poll::Pending;
                }
                Stage::WaitAckEnd => match self.tx_busy() {
                    Err(()) => return Poll::Failure,
                    Ok(true) if now.wrapping_sub(self.board.t) > TX_WAIT => return Poll::Failure,
                    Ok(true) => return Poll::Pending,
                    Ok(false) => return Poll::Success,
                },
            }
        }
    }

    /// Sub-state 5: reads what is queued, one byte at a time, through the validator and the parser.
    fn receive(&mut self, cmd: Cmd, now: u32) -> Poll {
        let new_seed = (cmd == Cmd::NewSeed).then(|| u16::from_le_bytes([self.board.tx[6], self.board.tx[7]]));
        let mut complete = false;
        loop {
            match self.rx_waiting() {
                Err(()) => return self.close_fail(),
                Ok(false) => break,
                Ok(true) => {}
            }
            let byte = match self.read_byte() {
                Ok(byte) => byte,
                Err(()) => return self.close_fail(),
            };
            let b = &mut self.board;
            b.t = now;
            let key = RxKey { seed: b.rx_seed, state: b.state };
            let verdict = frame::feed_rx(&mut b.rx, byte, BOARD_ADDRESS, cmd.response_len(), key, new_seed);
            if let RxVerdict::Complete(commit) = verdict {
                if let Some(commit) = commit {
                    b.rx_seed = commit.seed;
                    b.state = commit.state;
                }
                if self.parse(cmd) {
                    complete = true;
                    break;
                }
                self.board.rx.clear();
            }
        }
        if !complete {
            return if now.wrapping_sub(self.board.t) > RX_IDLE_WAIT { Poll::Failure } else { Poll::Pending };
        }
        if self.board.resp_code == RESP_NAK || !cmd.acknowledges() {
            return Poll::Success;
        }
        self.board.t = now;
        self.board.stage = Stage::WaitAckStart;
        Poll::Pending
    }

    /// The response parsers (`0x1404045D0`, `0x1404046A0`, `0x1404047C0`). `false` leaves the transfer waiting.
    fn parse(&mut self, cmd: Cmd) -> bool {
        let b = &mut self.board;
        let len = b.rx[4];
        let code = b.rx[6];
        let nak = len == 4 && code == RESP_NAK;
        match cmd {
            Cmd::NewSeed | Cmd::Setup | Cmd::Control => {
                if code != RESP_ACK && code != RESP_NAK || b.rx[7] != b.req_seq {
                    return false;
                }
            }
            Cmd::Info if len == 25 && code == RESP_INFO => {
                b.info.copy_from_slice(&b.rx[7..31]);
            }
            Cmd::Status if len == 10 && code == RESP_STATUS => {
                b.prev_status = b.status;
                b.status.copy_from_slice(&b.rx[7..16]);
                b.rejected = b.prev_status[8] & 1 == 0 && b.status[8] & 1 == 1;
            }
            Cmd::Info | Cmd::Status if nak => {}
            _ => return false,
        }
        b.resp_code = code;
        b.resp_seq = b.rx[5];
        b.resp_flags = if len == 4 { b.rx[9] } else { 0 };
        true
    }

    /// The response handlers (`3.10`) and the STATUS processing (`3.11`) for a completed command.
    fn acked(&mut self, cmd: Cmd) {
        let b = &mut self.board;
        if b.resp_code == RESP_NAK {
            if cmd != Cmd::NewSeed {
                bump(&mut b.pending.new_seed);
            }
            return;
        }
        let gs = b.resp_flags & 1 != 0;
        match cmd {
            Cmd::NewSeed => {
                drop_one(&mut b.pending.new_seed);
                bump(&mut b.pending.info_after);
                b.rekey_left = REKEY_INTERVAL;
            }
            Cmd::Setup if gs => bump(&mut b.pending.setup_verify),
            Cmd::Setup => {
                drop_one(&mut b.pending.setup);
                bump(&mut b.pending.info_after);
            }
            Cmd::Control if gs => {
                drop_one(&mut b.pending.control);
                bump(&mut b.pending.control_verify);
            }
            Cmd::Control => self.control_acked(),
            Cmd::Info => self.info_received(),
            Cmd::Status => self.process_status(),
        }
    }

    /// CONTROL acknowledged without GET_STATUS (also the "verified" case of STATUS processing).
    fn control_acked(&mut self) {
        let b = &mut self.board;
        drop_one(&mut b.pending.control);
        if b.pending.control_reject != 0 {
            drop_one(&mut b.pending.control_reject);
            if b.pending.control == 0 && b.pending.control_reject == 0 {
                b.control_flags &= !2;
            }
        }
        if b.pending.control_clear != 0 {
            drop_one(&mut b.pending.control_clear);
            if b.pending.control_clear == 0 {
                b.control_flags &= !4;
            }
        }
        if b.pending.control == 0 {
            b.control_coins = 0;
        }
    }

    /// INFO data against the configured SETUP (`0x140405CB0`).
    fn info_received(&mut self) {
        let b = &mut self.board;
        if b.info[8..24] != b.setup[1..17] {
            bump(&mut b.pending.setup);
            if matches!(b.setup[3] % 10, 1..=9) {
                b.control_coins = b.default_control_coins;
                bump(&mut b.pending.control);
            }
        }
        drop_one(&mut b.pending.info_after);
    }

    /// `0x140406E40`.
    fn process_status(&mut self) {
        let b = &mut self.board;
        if b.prev_status[0] == b.status[0] {
            return;
        }
        let [_, _, _, c10, c50, c100, c500, cond, err] = b.status;
        b.stats = Stats { yen_10: u32::from(c10), yen_100: u32::from(c100), yen_500: u32::from(c500) };
        if b.pending.control_verify != 0 {
            if err & 0x40 != 0 {
                if cond & 0x20 != 0 {
                    bump(&mut b.pending.new_seed);
                    bump(&mut b.pending.setup);
                }
            } else {
                self.control_acked();
            }
            self.board.pending.control_verify = 0;
        } else if b.pending.setup_verify != 0 {
            if err & 0x40 != 0 {
                b.pending.setup_verify = 0;
                if cond & 0x20 != 0 {
                    bump(&mut b.pending.new_seed);
                }
            } else {
                drop_one(&mut b.pending.setup);
                if b.control_flags & 1 == 0 {
                    bump(&mut b.pending.control);
                }
                bump(&mut b.pending.info_after);
                b.pending.setup_verify = 0;
            }
        }
        let b = &mut self.board;
        if cond & 0x80 == 0 {
            b.busy_grace = 5;
            b.busy_left = BUSY_TIMEOUT;
        } else {
            b.busy_grace = b.busy_grace.saturating_sub(1);
        }
        if cond & 0x10 != 0 || err & 0x3E != 0 {
            if cond & 1 == 0 {
                b.control_flags |= 1;
                bump(&mut b.pending.control);
            }
            if b.status[1] != 0 && (c10 | c50 | c100 | c500) != 0 {
                b.control_flags |= 2;
                bump(&mut b.pending.control);
                bump(&mut b.pending.control_reject);
            }
            if err & 0x08 != 0 {
                b.control_flags |= 4;
                bump(&mut b.pending.control);
                bump(&mut b.pending.control_clear);
            }
        }
    }
}
