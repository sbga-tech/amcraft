//! The simulated preload worker.

use std::cell::RefCell;
use std::rc::Rc;

use crate::{PreloadError, Preloader};

/// The simulated preloader: only the listed processes exist (process 0 always does).
#[derive(Debug, Clone, Default)]
pub struct SimPreloader {
    processes: Rc<RefCell<Vec<u32>>>,
    requests: Rc<RefCell<Vec<u32>>>,
}

impl SimPreloader {
    /// A system with no process besides this one.
    pub fn new() -> Self {
        Self::default()
    }

    /// Makes a process id exist.
    pub fn add_process(&self, process_id: u32) {
        self.processes.borrow_mut().push(process_id);
    }

    /// The process ids asked for so far.
    pub fn requests(&self) -> Vec<u32> {
        self.requests.borrow().clone()
    }
}

impl Preloader for SimPreloader {
    fn preload(&mut self, process_id: u32) -> Result<(), PreloadError> {
        self.requests.borrow_mut().push(process_id);
        if process_id == 0 || self.processes.borrow().contains(&process_id) { Ok(()) } else { Err(PreloadError) }
    }
}
