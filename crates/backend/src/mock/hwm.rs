//! The simulated hardware monitor driver.

use std::cell::RefCell;
use std::rc::Rc;

use crate::{HwMonitor, HwmError};

#[derive(Debug, Default)]
struct SimHwState {
    open_fails: bool,
    opened: bool,
    reading: Option<i32>,
}

/// A hardware monitor driver that answers what the test says.
#[derive(Debug, Clone)]
pub struct SimHwMonitor {
    state: Rc<RefCell<SimHwState>>,
}

impl Default for SimHwMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl SimHwMonitor {
    /// A driver reading 40 degrees.
    pub fn new() -> Self {
        Self { state: Rc::new(RefCell::new(SimHwState { reading: Some(40), ..SimHwState::default() })) }
    }

    /// Sets the reading; `None` makes the read fail.
    pub fn set_temperature(&self, reading: Option<i32>) {
        self.state.borrow_mut().reading = reading;
    }

    /// Makes the driver refuse to open.
    pub fn set_open_fails(&self, fails: bool) {
        self.state.borrow_mut().open_fails = fails;
    }

    /// Whether the driver is open.
    pub fn is_open(&self) -> bool {
        self.state.borrow().opened
    }
}

impl HwMonitor for SimHwMonitor {
    fn open(&mut self) -> Result<(), HwmError> {
        let mut state = self.state.borrow_mut();
        if state.open_fails {
            return Err(HwmError::Open);
        }
        state.opened = true;
        Ok(())
    }

    fn close(&mut self) {
        self.state.borrow_mut().opened = false;
    }

    fn read_cpu_temperature(&mut self) -> Result<i32, HwmError> {
        let state = self.state.borrow();
        if !state.opened {
            return Err(HwmError::Read);
        }
        state.reading.ok_or(HwmError::Read)
    }
}
