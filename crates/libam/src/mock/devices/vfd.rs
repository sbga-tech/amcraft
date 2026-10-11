//! The simulated VFD line: records every packet and lets a test script the port.
//!
//! The panel driver polls [`SerialPort::bytes_to_write`] and waits in [`SerialPort::drain`]. A scripted stall
//! stays busy for that many polls, a hang stays busy until `drain` (which reports the timeout at once, the way
//! the old line's `wait_sent` did), and a write failure is an error on the next poll or drain.

use std::any::Any;
use std::cell::RefCell;
use std::io;
use std::rc::Rc;
use std::time::Duration;

use crate::mock::serial::SerialDevice;
use backend::serial::{SerialPort, SerialSettings};

#[derive(Debug, Default)]
struct State {
    /// COM number this device was attached to. The open port reports its low byte.
    com: u16,
    open: bool,
    open_port: Option<u8>,
    opens: u32,
    packets: Vec<Vec<u8>>,
    open_fails: bool,
    write_fails: bool,
    stall_polls: u32,
    hang: bool,
    /// Polls of [`SerialPort::bytes_to_write`] that still report the write as busy. `u32::MAX` never finishes.
    busy_polls: u32,
    held_error: bool,
    in_flight: bool,
}

/// The simulated serial line of the display module. Clones share the device.
#[derive(Debug, Clone, Default)]
pub struct SimVfd(Rc<RefCell<State>>);

impl SimVfd {
    /// A line with a panel that answers every packet at once.
    pub fn new() -> Self {
        Self::default()
    }

    /// A line attached to `com`.
    pub fn on_port(com: u16) -> Self {
        let line = Self::new();
        line.0.borrow_mut().com = com;
        line
    }

    /// Every packet handed to the line so far, the init packet first.
    pub fn packets(&self) -> Vec<Vec<u8>> {
        self.0.borrow().packets.clone()
    }

    /// Takes the packets recorded so far.
    pub fn take_packets(&self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.0.borrow_mut().packets)
    }

    /// The COM port currently open (port byte), if any.
    pub fn open_port(&self) -> Option<u8> {
        self.0.borrow().open_port
    }

    /// How many times the port was opened.
    pub fn open_count(&self) -> u32 {
        self.0.borrow().opens
    }

    /// Opening the port fails (the COM port does not exist).
    pub fn set_open_fails(&self, fails: bool) {
        self.0.borrow_mut().open_fails = fails;
    }

    /// Writes fail (the line reports a write error for each packet).
    pub fn set_write_fails(&self, fails: bool) {
        self.0.borrow_mut().write_fails = fails;
    }

    /// Every later write stays in flight for `polls` polls before it completes.
    pub fn set_stall_polls(&self, polls: u32) {
        self.0.borrow_mut().stall_polls = polls;
    }

    /// The panel does not answer: later writes never complete and time out.
    pub fn set_hang(&self, hang: bool) {
        self.0.borrow_mut().hang = hang;
    }
}

impl SerialDevice for SimVfd {
    fn open(&self, _settings: &SerialSettings) -> io::Result<Box<dyn SerialPort>> {
        let mut state = self.0.borrow_mut();
        if state.open_fails {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no such COM port"));
        }
        state.open = true;
        state.open_port = Some(state.com as u8);
        state.opens += 1;
        state.in_flight = false;
        state.held_error = false;
        state.busy_polls = 0;
        drop(state);
        Ok(Box::new(VfdPort(self.0.clone())))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

struct VfdPort(Rc<RefCell<State>>);

impl std::fmt::Debug for VfdPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VfdPort").field("open", &self.0.borrow().open_port).finish()
    }
}

impl Drop for VfdPort {
    fn drop(&mut self) {
        let mut state = self.0.borrow_mut();
        state.open = false;
        state.open_port = None;
        state.in_flight = false;
        state.held_error = false;
        state.busy_polls = 0;
    }
}

impl SerialPort for VfdPort {
    fn bytes_to_read(&mut self) -> io::Result<usize> {
        Ok(0)
    }

    fn bytes_to_write(&mut self) -> io::Result<usize> {
        let mut state = self.0.borrow_mut();
        if state.held_error {
            state.held_error = false;
            state.in_flight = false;
            return Err(io::Error::other("write failed"));
        }
        if !state.in_flight {
            return Ok(0);
        }
        if state.busy_polls == u32::MAX {
            return Ok(1);
        }
        if state.busy_polls > 0 {
            state.busy_polls -= 1;
            return Ok(1);
        }
        state.in_flight = false;
        Ok(0)
    }

    fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
        Ok(0)
    }

    fn write(&mut self, data: &[u8]) -> io::Result<()> {
        let mut state = self.0.borrow_mut();
        if !state.open {
            return Err(io::Error::new(io::ErrorKind::NotConnected, "port not open"));
        }
        state.packets.push(data.to_vec());
        state.in_flight = true;
        if state.hang {
            state.busy_polls = u32::MAX;
            state.held_error = false;
        } else if state.write_fails {
            state.busy_polls = 0;
            state.held_error = true;
        } else {
            state.busy_polls = state.stall_polls;
            state.held_error = false;
        }
        Ok(())
    }

    fn drain(&mut self, _timeout: Duration) -> io::Result<bool> {
        let mut state = self.0.borrow_mut();
        if state.held_error {
            state.held_error = false;
            state.in_flight = false;
            return Err(io::Error::other("write failed"));
        }
        if state.busy_polls == u32::MAX {
            return Ok(false);
        }
        state.busy_polls = 0;
        state.in_flight = false;
        Ok(true)
    }

    fn purge(&mut self) -> io::Result<()> {
        let mut state = self.0.borrow_mut();
        state.in_flight = false;
        state.held_error = false;
        state.busy_polls = 0;
        Ok(())
    }
}
