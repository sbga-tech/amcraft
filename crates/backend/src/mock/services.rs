//! The simulated service control.

use std::cell::RefCell;
use std::rc::Rc;

use crate::{ServiceControl, ServiceError};

/// Recording service control for tests: every request is stored; `fail_with` makes them fail.
#[derive(Debug, Default)]
pub struct SimServiceState {
    /// `(service, code)` of every request.
    pub requests: Vec<(String, u32)>,
    /// Error returned by every request when set.
    pub fail_with: Option<ServiceError>,
}

/// Simulated service control.
#[derive(Debug, Clone, Default)]
pub struct SimServices {
    /// Shared record.
    pub state: Rc<RefCell<SimServiceState>>,
}

impl SimServices {
    /// A service control that records and succeeds.
    pub fn new() -> Self {
        Self::default()
    }
}

impl ServiceControl for SimServices {
    fn control(&mut self, service: &str, code: u32) -> Result<(), ServiceError> {
        let mut state = self.state.borrow_mut();
        state.requests.push((service.to_owned(), code));
        state.fail_with.map_or(Ok(()), Err)
    }
}
