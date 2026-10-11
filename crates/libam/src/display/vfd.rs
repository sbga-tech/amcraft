//! The VFD device protocol (`can.md` 3.10): the two-line, 20-column panel behind an RS-232C port.
//!
//! [`Vfd`] is the panel driver: packet building, the scroll rules, the CP932 conversion and the one-packet-in-flight
//! write. It opens a [`backend::SerialPort`] at 115200 baud, 8 data bits, no parity, one stop bit, CTS flow
//! control and DTR asserted, and a write older than 3000 ms is a timeout.
//!
//! The packet layout is specific to this panel firmware, so it is hand-written here (the byte values are
//! [vfd.command_table], the shapes [vfd.plain_message] and [vfd.scroll_message]); only the CP932 conversion uses a
//! crate (`encoding_rs`, `shift_jis`).

use std::io;
use std::time::Duration;

use encoding_rs::{EncoderResult, SHIFT_JIS};

use backend::{FlowControl, SerialPort, SerialPorts, SerialSettings};

/// Packet buffer capacity [vfd.packet_buffer]: appending past it fails with [`VfdError::PacketOverflow`].
const PACKET_CAPACITY: usize = 300;
/// Longest text that stays a plain message (CP932 bytes); longer text always scrolls [vfd.set_message].
const STATIC_LIMIT: usize = 20;
/// Messages are cut at this many bytes.
const MESSAGE_LIMIT: usize = 128;
/// Line buffer size of the original: a longer conversion result fails and leaves the line empty.
const LINE_BUFFER: usize = 256;
/// How long `Init` waits for its synchronous first packet [vfd.serial.write_path].
const INIT_WAIT_MS: u32 = 3010;
/// Trailing spaces of a scrolling message that is set through the "scroll" entry point.
pub(super) const SCROLL_PAD: usize = 6;

const RESET: [u8; 2] = [0x1B, 0x0B];
const POWER_ON: [u8; 3] = [0x1B, 0x21, 0x01];
const LANGUAGE: [u8; 3] = [0x1B, 0x32, 0x02];
const CLEAR_MESSAGE: [u8; 2] = [0x1B, 0x0C];
const STOP_SCROLL: [u8; 2] = [0x1B, 0x52];
const SET_MESSAGE: [u8; 4] = [0x1B, 0x30, 0x00, 0x00];
const SET_SCROLL_MESSAGE: [u8; 4] = [0x1B, 0x40, 0x00, 0x00];
const BOX_SIZE: [u8; 3] = [0x00, 0x9F, 0x02];
const SPEED: [u8; 3] = [0x1B, 0x41, 0x00];
const SCROLL_MESSAGE: [u8; 2] = [0x1B, 0x50];
const START_SCROLL: [u8; 2] = [0x1B, 0x51];

/// State of the last asynchronous write as the serial layer reports it (`amcUpdate`, [vfd.serial.write_path]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendStatus {
    /// No write outstanding and none to report.
    Idle,
    /// The write is still in flight.
    InFlight,
    /// The write finished with every byte written (reported once, then back to [`SendStatus::Idle`]).
    Done,
    /// The write was still pending after the 3000 ms send timeout.
    TimedOut,
    /// The write failed or was short.
    Failed,
}

/// 115200 baud, 8N1, CTS flow control, DTR asserted, a 3000 ms write timeout.
fn line_settings() -> SerialSettings {
    SerialSettings {
        baud: 115_200,
        flow_control: FlowControl::Hardware,
        dtr: true,
        write_timeout: Duration::from_millis(3_000),
        ..SerialSettings::default()
    }
}

/// The driver's negative result codes [vfd.driver.result_codes].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum VfdError {
    /// -1: the port opened but the init packet failed (the port was closed again), or a scroll block did not fit.
    #[error("generic failure (-1)")]
    Generic,
    /// -3: the packet buffer would overflow.
    #[error("packet buffer overflow (-3)")]
    PacketOverflow,
    /// -4: invalid port number, line, or text length.
    #[error("invalid argument (-4)")]
    InvalidArgument,
    /// -5: the port could not be opened or configured.
    #[error("open failed (-5)")]
    OpenFailed,
    /// -6: the port is not open.
    #[error("port not open (-6)")]
    NotOpen,
}

impl VfdError {
    /// The numeric code of the original driver.
    pub fn code(self) -> i32 {
        match self {
            Self::Generic => -1,
            Self::PacketOverflow => -3,
            Self::InvalidArgument => -4,
            Self::OpenFailed => -5,
            Self::NotOpen => -6,
        }
    }
}

/// `Update` state `S` of [vfd.update_state_machine]: idle (0), write in flight (1) or error (-1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Phase {
    #[default]
    Idle,
    InFlight,
    Error,
}

/// One line buffer of the driver: the converted text and its scroll bookkeeping.
#[derive(Debug, Default)]
struct LineBuffer {
    text: Vec<u8>,
    scrolling: bool,
    started: bool,
}

impl LineBuffer {
    fn clear(&mut self) {
        self.text.clear();
        self.scrolling = false;
        self.started = false;
    }
}

/// The position byte of a line: line 0 is `0x00`, line 1 is `0x02`, anything else `0x00`.
fn position(line: usize) -> u8 {
    if line == 1 { 0x02 } else { 0x00 }
}

/// UTF-16 to CP932 as `WideCharToMultiByte(932, 0)` does it: unmappable characters become `?` [vfd.set_message].
fn encode_cp932(text: &str) -> Vec<u8> {
    let mut encoder = SHIFT_JIS.new_encoder();
    let mut out = Vec::with_capacity(text.len() * 2);
    let mut rest = text;
    let mut chunk = [0u8; 128];
    loop {
        let (result, read, written) = encoder.encode_from_utf8_without_replacement(rest, &mut chunk, true);
        out.extend_from_slice(&chunk[..written]);
        rest = &rest[read..];
        match result {
            EncoderResult::InputEmpty => return out,
            EncoderResult::OutputFull => {}
            EncoderResult::Unmappable(_) => out.push(b'?'),
        }
    }
}

/// The panel driver (SEGA `ampdGd1232a01a` API): state is the open flag, the one-in-flight phase, the two line
/// buffers and the packet under construction.
#[derive(Debug, Default)]
pub struct Vfd {
    open: bool,
    phase: Phase,
    lines: [LineBuffer; 2],
    packet: Vec<u8>,
    port: Option<Box<dyn SerialPort>>,
    /// A write has been queued and not yet reaped.
    in_flight: bool,
    /// Backend-clock milliseconds when the write in flight was queued.
    started_ms: u64,
    /// A finished write the next poll reports once.
    finished: Option<SendStatus>,
}

impl Vfd {
    /// `Init` (`0x1403AFAD0`) [vfd.driver.init]: closes a previous session, opens the port and sends the init
    /// packet `1B 0B 1B 21 01 1B 32 02` synchronously.
    ///
    /// `Err(Generic)` means the port opened but the init packet failed; the port is already closed again.
    pub fn init(&mut self, ports: &dyn SerialPorts, port: u8, now_ms: u64) -> Result<(), VfdError> {
        if port == 0 {
            return Err(VfdError::InvalidArgument);
        }
        if self.open {
            self.exit();
        }
        let line = ports.open(u16::from(port), &line_settings()).map_err(|_| VfdError::OpenFailed)?;
        self.port = Some(line);
        self.open = true;
        self.in_flight = false;
        self.finished = None;
        self.packet.clear();
        for line in &mut self.lines {
            line.clear();
        }
        let mut init = [0u8; 8];
        init[..2].copy_from_slice(&RESET);
        init[2..5].copy_from_slice(&POWER_ON);
        init[5..].copy_from_slice(&LANGUAGE);
        if self.send(&init, now_ms).is_err() || self.wait_sent(INIT_WAIT_MS) != SendStatus::Done {
            self.exit();
            return Err(VfdError::Generic);
        }
        Ok(())
    }

    /// `Exit`: closes the port. The in-flight phase is deliberately kept (it is not reset by `Init`/`Exit`).
    pub fn exit(&mut self) {
        if self.open {
            if let Some(port) = self.port.as_mut() {
                let _ = port.purge();
            }
            self.port = None;
            self.in_flight = false;
            self.finished = None;
            self.open = false;
        }
    }

    /// Whether the port is open.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// `GetResult` (`0x1403AFAB0`): 0 idle, 1 write in flight, -1 error, -6 when the port is closed.
    pub fn result(&self) -> i32 {
        if !self.open {
            return VfdError::NotOpen.code();
        }
        match self.phase {
            Phase::Idle => 0,
            Phase::InFlight => 1,
            Phase::Error => -1,
        }
    }

    /// `ClearMessage` (`0x1403AF9F0`) [vfd.clear_message]: drops both line buffers and queues `1B 0C 1B 52`.
    pub fn clear_message(&mut self) -> Result<(), VfdError> {
        if !self.open {
            return Err(VfdError::NotOpen);
        }
        for line in &mut self.lines {
            line.clear();
        }
        self.append(&CLEAR_MESSAGE)?;
        self.append(&STOP_SCROLL)
    }

    /// `SetMessage` (`0x1403AFCE0`, padding 0) [vfd.set_message].
    pub fn set_message(&mut self, text: &str, line: usize) -> Result<(), VfdError> {
        self.set_text(text, line, 0)
    }

    /// `SetMessageScroll` (padding 6): the same, with six spaces behind a scrolling text.
    pub fn set_message_scroll(&mut self, text: &str, line: usize) -> Result<(), VfdError> {
        self.set_text(text, line, SCROLL_PAD)
    }

    fn set_text(&mut self, text: &str, line: usize, pad: usize) -> Result<(), VfdError> {
        if !self.open {
            return Err(VfdError::NotOpen);
        }
        if text.is_empty() || line > 1 {
            return Err(VfdError::InvalidArgument);
        }
        let mut bytes = encode_cp932(text);
        if bytes.len() > LINE_BUFFER {
            bytes.clear();
        }
        bytes.truncate(MESSAGE_LIMIT);
        let len = bytes.len();
        let buffer = &mut self.lines[line];
        buffer.text = bytes;
        buffer.scrolling = false;
        buffer.started = false;
        if len <= STATIC_LIMIT {
            return Ok(());
        }
        buffer.scrolling = true;
        // `1B 40 00 00 <pos> 00 9F 02 1B 41 00 1B 50 <size> <text> <pad>`; the size byte is the low byte of
        // `len + pad` [vfd.scroll_message].
        let total = SET_SCROLL_MESSAGE.len() + 1 + BOX_SIZE.len() + SPEED.len() + SCROLL_MESSAGE.len() + 1 + len + pad;
        if self.packet.len() + total > PACKET_CAPACITY {
            return Err(VfdError::Generic);
        }
        self.packet.extend_from_slice(&SET_SCROLL_MESSAGE);
        self.packet.push(position(line));
        self.packet.extend_from_slice(&BOX_SIZE);
        self.packet.extend_from_slice(&SPEED);
        self.packet.extend_from_slice(&SCROLL_MESSAGE);
        self.packet.push((len + pad).to_le_bytes()[0]);
        self.packet.extend_from_slice(&self.lines[line].text);
        self.packet.resize(self.packet.len() + pad, b' ');
        Ok(())
    }

    /// `Update` (`0x1403AFEC0`) [vfd.update_state_machine]: reaps the previous write and, when nothing is in
    /// flight, builds and starts the next packet. At most one packet is in flight.
    pub fn update(&mut self, now_ms: u64) {
        if !self.open {
            return;
        }
        let status = self.poll(now_ms);
        if status == SendStatus::InFlight {
            return;
        }
        if self.phase == Phase::InFlight {
            self.phase = if status == SendStatus::Done { Phase::Idle } else { Phase::Error };
            return;
        }
        if self.build().is_err() {
            self.phase = Phase::Error;
            return;
        }
        if self.packet.is_empty() {
            self.phase = Phase::Idle;
            return;
        }
        let packet = std::mem::take(&mut self.packet);
        let started = self.send(&packet, now_ms);
        for line in &mut self.lines {
            line.clear();
        }
        self.phase = if started.is_ok() { Phase::InFlight } else { Phase::Error };
    }

    /// Queues `packet` and returns without waiting for the port to accept it.
    fn send(&mut self, packet: &[u8], now_ms: u64) -> io::Result<()> {
        let port = self.port.as_mut().ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "port not open"))?;
        port.write(packet)?;
        self.finished = None;
        self.in_flight = true;
        self.started_ms = now_ms;
        Ok(())
    }

    /// The write state. A finished state is reported once, then the line is idle.
    fn poll(&mut self, now_ms: u64) -> SendStatus {
        if let Some(status) = self.finished.take() {
            return status;
        }
        if !self.in_flight {
            return SendStatus::Idle;
        }
        let Some(port) = self.port.as_mut() else {
            self.in_flight = false;
            return SendStatus::Failed;
        };
        match port.bytes_to_write() {
            Ok(0) => {
                self.in_flight = false;
                SendStatus::Done
            }
            Ok(_) if now_ms.wrapping_sub(self.started_ms) >= 3_000 => {
                self.in_flight = false;
                SendStatus::TimedOut
            }
            Ok(_) => SendStatus::InFlight,
            Err(error) if error.kind() == io::ErrorKind::TimedOut => {
                self.in_flight = false;
                SendStatus::TimedOut
            }
            Err(_) => {
                self.in_flight = false;
                SendStatus::Failed
            }
        }
    }

    /// Blocks for the write in flight, at most `timeout_ms`. `Done` stays pending for the next poll.
    fn wait_sent(&mut self, timeout_ms: u32) -> SendStatus {
        if !self.in_flight {
            return SendStatus::Idle;
        }
        let status = match self.port.as_mut().map(|port| port.drain(Duration::from_millis(u64::from(timeout_ms)))) {
            Some(Ok(true)) => SendStatus::Done,
            Some(Ok(false)) => SendStatus::TimedOut,
            Some(Err(error)) if error.kind() == io::ErrorKind::TimedOut => SendStatus::TimedOut,
            _ => SendStatus::Failed,
        };
        self.in_flight = false;
        self.finished = Some(status);
        status
    }

    /// Appends the plain messages (line 0 then 1) and then `1B 51` per scrolling line [vfd.packet_order].
    fn build(&mut self) -> Result<(), VfdError> {
        for index in 0..self.lines.len() {
            let line = &self.lines[index];
            if line.scrolling || line.text.is_empty() {
                continue;
            }
            if line.text.len() > STATIC_LIMIT {
                return Err(VfdError::InvalidArgument);
            }
            let total = SET_MESSAGE.len() + 1 + line.text.len();
            if self.packet.len() + total > PACKET_CAPACITY {
                return Err(VfdError::PacketOverflow);
            }
            self.packet.extend_from_slice(&SET_MESSAGE);
            self.packet.push(position(index));
            self.packet.extend_from_slice(&self.lines[index].text);
            self.lines[index].text.clear();
        }
        for index in 0..self.lines.len() {
            if self.lines[index].scrolling && !self.lines[index].started {
                self.append(&START_SCROLL)?;
                self.lines[index].started = true;
            }
        }
        Ok(())
    }

    fn append(&mut self, bytes: &[u8]) -> Result<(), VfdError> {
        if self.packet.len() + bytes.len() > PACKET_CAPACITY {
            return Err(VfdError::PacketOverflow);
        }
        self.packet.extend_from_slice(bytes);
        Ok(())
    }
}
