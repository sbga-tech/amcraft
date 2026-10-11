//! The singleton `DisplayContext` (`can.md` 3.1.2, 3.3.2-3.3.4): the open VFD, its last result and its health tracker.

use super::vfd::{Vfd, VfdError};
use backend::SerialPorts;

/// Consecutive error samples that fail the tracker (`threshold = 2, ratio = 0`).
const FAIL_AFTER: u32 = 2;

/// `0x140292740/810/880`: counts consecutive error samples; once failed it ignores further samples.
#[derive(Debug, Default)]
struct Tracker {
    consecutive: u32,
    failed: bool,
}

impl Tracker {
    fn push(&mut self, error: bool) {
        if self.failed {
            return;
        }
        if error {
            self.consecutive += 1;
            self.failed = self.consecutive >= FAIL_AFTER;
        } else {
            self.consecutive = 0;
        }
    }
}

/// The context of the one VFD.
#[derive(Debug, Default)]
pub(super) struct Context {
    /// `opened`: the panel answered the init packet and has not failed since.
    pub opened: bool,
    /// `lastResult`: the driver's `GetResult` at the last health sample.
    pub last_result: i32,
    tracker: Tracker,
    pub driver: Vfd,
}

impl Context {
    /// `0x1402656E0`: opens the port and sends the init packet. `Err` carries the driver code of a failed open.
    ///
    /// A port that opens but does not answer (-1) is a success with `opened == false`.
    pub fn initialize(&mut self, ports: &dyn SerialPorts, port: u64, now: u64) -> Result<(), VfdError> {
        self.opened = false;
        self.last_result = 0;
        self.tracker = Tracker::default();
        // The driver takes the low byte of the configured port.
        match self.driver.init(ports, port.to_le_bytes()[0], now) {
            Ok(()) => {
                self.opened = true;
                Ok(())
            }
            Err(VfdError::Generic) => Ok(()),
            Err(error) => {
                tracing::error!(code = error.code(), "display: opening the panel failed");
                Err(error)
            }
        }
    }

    /// `0x1402657B0`: closes the port and forgets the device state.
    pub fn finalize(&mut self) {
        self.driver.exit();
        self.opened = false;
        self.last_result = 0;
        self.tracker = Tracker::default();
    }

    /// `0x140265890`: runs the driver update, samples its result and updates `opened`.
    pub fn track(&mut self, now: u64) {
        if !self.opened {
            return;
        }
        self.driver.update(now);
        let result = self.driver.result();
        self.last_result = result;
        match result {
            0 => self.tracker.push(false),
            1 => {}
            error => {
                self.tracker.push(true);
                tracing::warn!(code = error, "display: the panel did not acknowledge");
            }
        }
        self.opened = !self.tracker.failed;
    }
}
