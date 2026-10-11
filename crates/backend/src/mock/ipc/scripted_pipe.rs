//! An always-connected [`PipeEndpoint`] double with captured writes and manually supplied input.
//!
//! Writes complete immediately. A read stays pending until its entire requested length is available
//! in the shared input queue; completing it consumes exactly that many bytes.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use crate::pipe::{PipeEndpoint, WaitResult};

/// Shared record of write payloads, in call order.
pub type Written = Rc<RefCell<Vec<Vec<u8>>>>;
/// Shared input queue into which the caller appends bytes for pending reads.
pub type Input = Rc<RefCell<VecDeque<u8>>>;

/// Always-connected endpoint with immediate captured writes or scripted input reads.
#[derive(Debug)]
pub struct ScriptedPipe {
    written: Written,
    input: Input,
    is_reader: bool,
    pending_read: Option<usize>,
    data: Vec<u8>,
}

impl ScriptedPipe {
    /// A write endpoint and the shared record of every payload it receives.
    pub fn writer() -> (Box<dyn PipeEndpoint>, Written) {
        let written = Written::default();
        let pipe = Self {
            written: Rc::clone(&written),
            input: Input::default(),
            is_reader: false,
            pending_read: None,
            data: vec![],
        };
        (Box::new(pipe), written)
    }

    /// A read endpoint and its shared input queue, initially empty.
    pub fn reader() -> (Box<dyn PipeEndpoint>, Input) {
        let input = Input::default();
        let pipe = Self {
            written: Written::default(),
            input: Rc::clone(&input),
            is_reader: true,
            pending_read: None,
            data: vec![],
        };
        (Box::new(pipe), input)
    }
}

impl PipeEndpoint for ScriptedPipe {
    fn is_valid(&self) -> bool {
        true
    }
    fn pump(&mut self) -> bool {
        self.wait(0);
        true
    }
    fn is_connected(&mut self) -> bool {
        true
    }
    fn is_io_pending(&mut self) -> bool {
        self.pending_read.is_some()
    }
    fn last_io_ok(&mut self) -> bool {
        true
    }
    fn can_read(&mut self) -> bool {
        self.is_reader && self.pending_read.is_none()
    }
    fn can_write(&mut self) -> bool {
        !self.is_reader
    }
    fn read(&mut self, len: usize) -> bool {
        self.pending_read = Some(len);
        true
    }
    fn read_data(&self) -> &[u8] {
        &self.data
    }
    fn write(&mut self, data: &[u8]) -> bool {
        self.written.borrow_mut().push(data.to_vec());
        true
    }
    fn wait(&mut self, _timeout_ms: u32) -> WaitResult {
        let Some(len) = self.pending_read else { return WaitResult::Idle };
        let mut input = self.input.borrow_mut();
        if input.len() < len {
            return WaitResult::Timeout;
        }
        self.data = input.drain(..len).collect();
        self.pending_read = None;
        WaitResult::Completed
    }
}
