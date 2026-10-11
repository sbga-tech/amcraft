//! The simulated vendor video DLL.

use std::cell::RefCell;
use std::rc::Rc;

use crate::{DisplayRequest, VideoDriver, VideoError};

#[derive(Debug, Default)]
struct SimDriverState {
    /// What `apply` answers.
    outcome: Option<Result<u32, VideoError>>,
    /// The library names and requests received.
    calls: Vec<(String, DisplayRequest)>,
}

/// A vendor display DLL that answers what the test says (code 0 by default) and records what it was asked.
#[derive(Debug, Clone, Default)]
pub struct SimVideoDriver {
    state: Rc<RefCell<SimDriverState>>,
}

impl SimVideoDriver {
    /// A driver that applies every request.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the answer of the next requests.
    pub fn set_outcome(&self, outcome: Result<u32, VideoError>) {
        self.state.borrow_mut().outcome = Some(outcome);
    }

    /// The `(library, request)` pairs received so far.
    pub fn calls(&self) -> Vec<(String, DisplayRequest)> {
        self.state.borrow().calls.clone()
    }
}

impl VideoDriver for SimVideoDriver {
    fn apply(&mut self, library: &str, request: &DisplayRequest) -> Result<u32, VideoError> {
        let mut state = self.state.borrow_mut();
        state.calls.push((library.to_owned(), *request));
        state.outcome.unwrap_or(Ok(0))
    }
}
