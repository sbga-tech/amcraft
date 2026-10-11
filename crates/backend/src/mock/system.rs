//! The simulated system actions.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::{SystemActions, SystemError, TimeZoneInformation, WallTime};

/// What the simulated system actions recorded.
#[derive(Debug, Default)]
pub struct SimSystemState {
    /// Number of reboot requests.
    pub reboots: u32,
    /// System-time changes in order.
    pub system_times: Vec<WallTime>,
    /// Local-time changes in order.
    pub local_times: Vec<WallTime>,
    /// Time-zone changes in order.
    pub time_zones: Vec<TimeZoneInformation>,
    /// The computer name reported.
    pub computer_name: Option<String>,
    /// The process environment: only the variables set here exist (see [`SimSystem::set_env`]).
    pub env: HashMap<String, String>,
    /// Makes every action fail.
    pub fail: bool,
}

/// Recording system actions for tests.
#[derive(Debug, Clone, Default)]
pub struct SimSystem {
    /// Shared record.
    pub state: Rc<RefCell<SimSystemState>>,
}

impl SimSystem {
    /// A system that records and succeeds.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the process environment variable `name` that [`SystemActions::env_var`] reports.
    pub fn set_env(&self, name: &str, value: &str) {
        self.state.borrow_mut().env.insert(name.to_owned(), value.to_owned());
    }
}

impl SystemActions for SimSystem {
    fn reboot(&mut self) -> Result<(), SystemError> {
        let mut state = self.state.borrow_mut();
        state.reboots += 1;
        if state.fail { Err(SystemError::Failed(5)) } else { Ok(()) }
    }

    fn set_system_time(&mut self, time: &WallTime) -> Result<(), SystemError> {
        let mut state = self.state.borrow_mut();
        if state.fail {
            return Err(SystemError::Failed(5));
        }
        state.system_times.push(*time);
        Ok(())
    }

    fn set_local_time(&mut self, time: &WallTime) -> Result<(), SystemError> {
        let mut state = self.state.borrow_mut();
        if state.fail {
            return Err(SystemError::Failed(5));
        }
        state.local_times.push(*time);
        Ok(())
    }

    fn set_time_zone(&mut self, zone: &TimeZoneInformation) -> Result<(), SystemError> {
        let mut state = self.state.borrow_mut();
        if state.fail {
            return Err(SystemError::Failed(5));
        }
        state.time_zones.push(*zone);
        Ok(())
    }

    fn computer_name(&mut self) -> Option<String> {
        self.state.borrow().computer_name.clone()
    }

    fn env_var(&self, name: &str) -> Option<String> {
        self.state.borrow().env.get(name).cloned()
    }
}
