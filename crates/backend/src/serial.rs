//! The serial ports of a cabinet (`Host::serial`).
//!
//! One [`SerialPorts`] opens every COM port the process uses: the Aime readers, the AS-6DB coin selector and the
//! VFD. [`SerialPort`] is one open port. `open` returns at once; a write queues and [`SerialPort::drain`] waits
//! until the port has accepted it. A read returns only bytes that have already arrived.
//!
//! [`SerialportPorts`] is the default implementation, the `serialport` crate. A write is handed to a thread that
//! owns a duplicate of the port, so the caller returns while the operating system accepts the bytes. The thread's
//! queue length is what [`SerialPort::bytes_to_write`] reports. The port's own write timeout is
//! [`SerialSettings::write_timeout`]: zero is the crate's "no timeout", which on Windows is a `COMMTIMEOUTS` value
//! of zero (a synchronous `WriteFile` that does not time out).
//!
//! The device name of COM port `n` is backend configuration: [`SerialportPorts::new`] opens `\\.\COM<n>` (the
//! `\\.\` prefix is what `CreateFile` requires for COM10 and above; the reader used to open that form and the
//! selector and the panel accept it too), [`SerialportPorts::named`] takes any mapping (e.g. `/dev/ttyS<n-1>`).

use std::fmt::Debug;
use std::io::{self, Read, Write};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serialport::ClearBuffer;

/// Bits in one character.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DataBits {
    /// 8 data bits.
    #[default]
    Eight,
    /// 7 data bits.
    Seven,
}

/// Parity bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Parity {
    /// No parity bit.
    #[default]
    None,
    /// Even parity.
    Even,
    /// Odd parity.
    Odd,
}

/// Stop bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StopBits {
    /// One stop bit.
    #[default]
    One,
    /// Two stop bits.
    Two,
}

/// Output flow control.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FlowControl {
    /// No flow control.
    #[default]
    None,
    /// CTS/RTS hardware flow control.
    Hardware,
}

/// Line parameters applied when a port is opened.
///
/// The default is 9600 baud, 8 data bits, no parity, one stop bit, no flow control, DTR left alone and no write
/// timeout. Each device replaces the fields it does not share.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SerialSettings {
    /// Baud rate.
    pub baud: u32,
    /// Data bits.
    pub data_bits: DataBits,
    /// Parity.
    pub parity: Parity,
    /// Stop bits.
    pub stop_bits: StopBits,
    /// Output flow control.
    pub flow_control: FlowControl,
    /// Assert DTR while the port is open.
    pub dtr: bool,
    /// How long one queued write may take. [`Duration::ZERO`] does not time out.
    pub write_timeout: Duration,
}

impl Default for SerialSettings {
    fn default() -> Self {
        Self {
            baud: 9600,
            data_bits: DataBits::Eight,
            parity: Parity::None,
            stop_bits: StopBits::One,
            flow_control: FlowControl::None,
            dtr: false,
            write_timeout: Duration::ZERO,
        }
    }
}

/// Opens the COM ports of one cabinet. `open` is a factory: a driver keeps the returned port and reopens it
/// without borrowing the backend.
pub trait SerialPorts: Debug {
    /// Opens `COM<com>` exclusively with `settings` and purges both queues.
    fn open(&self, com: u16, settings: &SerialSettings) -> io::Result<Box<dyn SerialPort>>;
}

/// One open COM port.
///
/// `read` never waits: it returns bytes already received, or zero. `write` queues the bytes and returns.
/// `bytes_to_write` is what is still queued. `drain` waits until that queue is empty.
pub trait SerialPort: Debug {
    /// Bytes waiting to be read.
    fn bytes_to_read(&mut self) -> io::Result<usize>;

    /// Bytes accepted by [`SerialPort::write`] and not yet sent.
    fn bytes_to_write(&mut self) -> io::Result<usize>;

    /// Reads already-received bytes into `buf`. Never waits; `Ok(0)` means nothing is queued.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize>;

    /// Queues `data` to be written. Returns once the queue has accepted it, not once the peer has.
    fn write(&mut self, data: &[u8]) -> io::Result<()>;

    /// Waits until [`SerialPort::bytes_to_write`] is zero.
    ///
    /// `Ok(true)` is an empty queue. `Ok(false)` is `timeout` elapsed with bytes still queued.
    /// [`Duration::MAX`] waits without a limit. A write error is `Err`.
    fn drain(&mut self, timeout: Duration) -> io::Result<bool>;

    /// Discards both queues.
    fn purge(&mut self) -> io::Result<()>;
}

/// Opens COM ports through `serialport`, by the device name a mapping gives each COM number.
pub struct SerialportPorts {
    name: Box<dyn Fn(u16) -> String>,
}

impl SerialportPorts {
    /// The Windows COM ports, `\\.\COM<n>`.
    pub fn new() -> Self {
        Self::named(|com| format!(r"\\.\COM{com}"))
    }

    /// COM port `n` is the device `name(n)`.
    pub fn named(name: impl Fn(u16) -> String + 'static) -> Self {
        Self { name: Box::new(name) }
    }
}

impl Default for SerialportPorts {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for SerialportPorts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SerialportPorts").field("com1", &(self.name)(1)).finish()
    }
}

impl SerialPorts for SerialportPorts {
    fn open(&self, com: u16, settings: &SerialSettings) -> io::Result<Box<dyn SerialPort>> {
        let mut builder = serialport::new((self.name)(com), settings.baud)
            .data_bits(data_bits(settings.data_bits))
            .parity(parity(settings.parity))
            .stop_bits(stop_bits(settings.stop_bits))
            .flow_control(flow(settings.flow_control))
            .timeout(settings.write_timeout);
        if settings.dtr {
            builder = builder.dtr_on_open(true);
        }
        let port = builder.open().map_err(io::Error::from)?;
        port.clear(ClearBuffer::All).map_err(io::Error::from)?;
        let writer = port.try_clone().map_err(io::Error::from)?;
        let shared = Arc::new((Mutex::new(Shared::default()), Condvar::new()));
        let (sender, receiver) = std::sync::mpsc::channel();
        let queue = Arc::clone(&shared);
        thread::spawn(move || writer_loop(writer, receiver, queue));
        Ok(Box::new(Port { read: port, jobs: Some(sender), shared }))
    }
}

/// Bytes the writer thread has not finished sending, and the first error it hit.
#[derive(Debug, Default)]
struct Shared {
    queued: usize,
    error: Option<io::Error>,
}

/// One open port: reads use `read`, writes go through `jobs`.
struct Port {
    read: Box<dyn serialport::SerialPort>,
    jobs: Option<std::sync::mpsc::Sender<Vec<u8>>>,
    shared: Arc<(Mutex<Shared>, Condvar)>,
}

impl std::fmt::Debug for Port {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let queued = self.shared.0.lock().map(|guard| guard.queued).unwrap_or(0);
        f.debug_struct("SerialportPort").field("queued", &queued).finish()
    }
}

impl Drop for Port {
    fn drop(&mut self) {
        let _ = self.read.clear(ClearBuffer::Output);
        // Closing the channel lets the writer finish its current write and exit. It is not joined: a stuck
        // `WriteFile` must not hold the thread that dropped the port.
        self.jobs.take();
    }
}

impl SerialPort for Port {
    fn bytes_to_read(&mut self) -> io::Result<usize> {
        Ok(self.read.bytes_to_read().map_err(io::Error::from)? as usize)
    }

    fn bytes_to_write(&mut self) -> io::Result<usize> {
        let mut guard = self.shared.0.lock().unwrap_or_else(|poison| poison.into_inner());
        if let Some(error) = guard.error.take() {
            return Err(error);
        }
        Ok(guard.queued)
    }

    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let waiting = self.bytes_to_read()?;
        let want = waiting.min(buf.len());
        if want == 0 {
            return Ok(0);
        }
        self.read.read(&mut buf[..want])
    }

    fn write(&mut self, data: &[u8]) -> io::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        {
            let mut guard = self.shared.0.lock().unwrap_or_else(|poison| poison.into_inner());
            guard.queued += data.len();
        }
        let sender = self.jobs.as_ref().ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "writer stopped"))?;
        sender.send(data.to_vec()).map_err(|_| {
            let mut guard = self.shared.0.lock().unwrap_or_else(|poison| poison.into_inner());
            guard.queued = guard.queued.saturating_sub(data.len());
            io::Error::new(io::ErrorKind::BrokenPipe, "writer stopped")
        })
    }

    fn drain(&mut self, timeout: Duration) -> io::Result<bool> {
        let (lock, cv) = &*self.shared;
        let started = Instant::now();
        let mut guard = lock.lock().unwrap_or_else(|poison| poison.into_inner());
        loop {
            if let Some(error) = guard.error.take() {
                return Err(error);
            }
            if guard.queued == 0 {
                return Ok(true);
            }
            if timeout.is_zero() {
                return Ok(false);
            }
            if timeout == Duration::MAX {
                guard = cv.wait(guard).unwrap_or_else(|poison| poison.into_inner());
                continue;
            }
            let left = timeout.saturating_sub(started.elapsed());
            if left.is_zero() {
                return Ok(false);
            }
            let (next, wait) = cv.wait_timeout(guard, left).unwrap_or_else(|poison| poison.into_inner());
            guard = next;
            if wait.timed_out() && guard.queued != 0 && guard.error.is_none() {
                return Ok(false);
            }
        }
    }

    fn purge(&mut self) -> io::Result<()> {
        self.read.clear(ClearBuffer::All).map_err(io::Error::from)
    }
}

/// Writes each queued buffer and then tells whoever is waiting in `drain`.
fn writer_loop(
    mut port: Box<dyn serialport::SerialPort>,
    jobs: std::sync::mpsc::Receiver<Vec<u8>>,
    shared: Arc<(Mutex<Shared>, Condvar)>,
) {
    while let Ok(data) = jobs.recv() {
        let result = port.write_all(&data).and_then(|()| port.flush());
        let mut guard = shared.0.lock().unwrap_or_else(|poison| poison.into_inner());
        guard.queued = guard.queued.saturating_sub(data.len());
        if let Err(error) = result
            && guard.error.is_none()
        {
            guard.error = Some(error);
        }
        shared.1.notify_all();
    }
}

fn data_bits(bits: DataBits) -> serialport::DataBits {
    match bits {
        DataBits::Eight => serialport::DataBits::Eight,
        DataBits::Seven => serialport::DataBits::Seven,
    }
}

fn parity(parity: Parity) -> serialport::Parity {
    match parity {
        Parity::None => serialport::Parity::None,
        Parity::Even => serialport::Parity::Even,
        Parity::Odd => serialport::Parity::Odd,
    }
}

fn stop_bits(bits: StopBits) -> serialport::StopBits {
    match bits {
        StopBits::One => serialport::StopBits::One,
        StopBits::Two => serialport::StopBits::Two,
    }
}

fn flow(flow: FlowControl) -> serialport::FlowControl {
    match flow {
        FlowControl::None => serialport::FlowControl::None,
        FlowControl::Hardware => serialport::FlowControl::Hardware,
    }
}
