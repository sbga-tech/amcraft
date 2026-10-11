//! The simulated HID bus of the USB I/O boards: scripted boards and their reports.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use crate::hid::{HidBoard, HidBus, HidDevice, HidError, REPORT_LEN, Report};

/// Everything a scripted board remembers.
#[derive(Debug, Clone)]
pub struct SimBoardState {
    /// Manufacturer string.
    pub manufacturer: String,
    /// Product string (`I/O CONTROL BD ; ...`).
    pub product: String,
    /// The input report the board streams once per receive drain.
    pub latest: Report,
    /// Reports delivered before the streamed one.
    pub queue: VecDeque<Report>,
    /// Stream `latest` once per drain.
    pub streaming: bool,
    /// The board reacts to ClearBoardStatus (status 0) and SetSamplingCount (status `0x30`).
    pub auto_status: bool,
    /// Interrupt-OUT reports written by libam, in order.
    pub interrupt_writes: Vec<Report>,
    /// Control-pipe output reports written by libam, in order.
    pub control_writes: Vec<Report>,
    /// Status byte of the firmware-update reply (`0x85`, status).
    pub firmware_status: u8,
    /// Writes fail.
    pub fail_writes: bool,
    /// Control-pipe reads fail.
    pub fail_control_reads: bool,
    /// The board is on the bus (a removed board is not enumerated).
    pub present: bool,
    served: bool,
}

/// The ASCII product string a board reports.
pub fn product_string(
    board_no: &str,
    mode: u8,
    revision: u8,
    sum: u16,
    chip: &str,
    config: u8,
    functions: &str,
) -> String {
    format!("I/O CONTROL BD ;{board_no};{mode:X};{revision:X};{sum:X};{chip};{config:X};{functions}")
}

impl SimBoardState {
    fn new(manufacturer: &str, product: &str) -> Self {
        let mut latest = [0; REPORT_LEN];
        latest[0] = 1;
        Self {
            manufacturer: manufacturer.to_owned(),
            product: product.to_owned(),
            latest,
            queue: VecDeque::new(),
            streaming: true,
            auto_status: true,
            interrupt_writes: Vec::new(),
            control_writes: Vec::new(),
            firmware_status: 1,
            fail_writes: false,
            fail_control_reads: false,
            present: true,
            served: false,
        }
    }

    fn react(&mut self, report: &Report) {
        if !self.auto_status {
            return;
        }
        match report[1] {
            0x03 => self.latest[33] = 0,
            0x02 => self.latest[33] |= 0x30,
            _ => {}
        }
    }
}

#[derive(Debug, Default)]
struct SimHidBusState {
    boards: Vec<SimBoardState>,
    paths: Vec<String>,
    fail_enumerate: bool,
    fail_open: bool,
}

/// Handle to script the simulated bus (shared with the backend).
#[derive(Debug, Clone, Default)]
pub struct SimHid {
    state: Rc<RefCell<SimHidBusState>>,
}

impl SimHid {
    /// Plugs in a board and returns its index.
    pub fn add_board(&self, manufacturer: &str, product: &str) -> usize {
        let mut state = self.state.borrow_mut();
        let index = state.boards.len();
        state.paths.push(format!("\\\\?\\hid#vid_0ca3&pid_0021#{index}"));
        state.boards.push(SimBoardState::new(manufacturer, product));
        index
    }

    /// Runs `f` on a board's scripted state.
    pub fn with_board<R>(&self, index: usize, f: impl FnOnce(&mut SimBoardState) -> R) -> R {
        f(&mut self.state.borrow_mut().boards[index])
    }

    /// Replaces the streamed input report of a board.
    pub fn set_input(&self, index: usize, report: Report) {
        self.with_board(index, |board| board.latest = report);
    }

    /// The interrupt-OUT reports written to a board so far.
    pub fn interrupt_writes(&self, index: usize) -> Vec<Report> {
        self.with_board(index, |board| board.interrupt_writes.clone())
    }

    /// The control-pipe output reports written to a board so far.
    pub fn control_writes(&self, index: usize) -> Vec<Report> {
        self.with_board(index, |board| board.control_writes.clone())
    }

    /// Enumeration fails from now on.
    pub fn set_fail_enumerate(&self, fail: bool) {
        self.state.borrow_mut().fail_enumerate = fail;
    }

    /// Opening a board fails from now on.
    pub fn set_fail_open(&self, fail: bool) {
        self.state.borrow_mut().fail_open = fail;
    }

    /// The bus a [`crate::Backend`] hands to the USB I/O manager; it shares the boards scripted through this
    /// handle.
    pub fn bus(&self) -> SimHidBus {
        SimHidBus { state: Rc::clone(&self.state) }
    }
}

/// The simulated [`HidBus`].
#[derive(Debug)]
pub struct SimHidBus {
    state: Rc<RefCell<SimHidBusState>>,
}

#[derive(Debug)]
struct SimHidDevice {
    state: Rc<RefCell<SimHidBusState>>,
    index: usize,
}

impl HidBus for SimHidBus {
    fn enumerate(&mut self) -> Result<Vec<HidBoard>, HidError> {
        let state = self.state.borrow();
        if state.fail_enumerate {
            return Err(HidError);
        }
        Ok(state
            .boards
            .iter()
            .zip(&state.paths)
            .filter(|(board, _)| board.present)
            .map(|(board, path)| HidBoard {
                path: path.clone(),
                manufacturer: board.manufacturer.clone(),
                product: board.product.clone(),
            })
            .collect())
    }

    fn open(&mut self, path: &str) -> Result<Box<dyn HidDevice>, HidError> {
        let state = self.state.borrow();
        if state.fail_open {
            return Err(HidError);
        }
        let index = state.paths.iter().position(|p| p == path).ok_or(HidError)?;
        Ok(Box::new(SimHidDevice { state: Rc::clone(&self.state), index }))
    }
}

impl SimHidDevice {
    fn board<R>(&self, f: impl FnOnce(&mut SimBoardState) -> R) -> R {
        f(&mut self.state.borrow_mut().boards[self.index])
    }
}

impl HidDevice for SimHidDevice {
    fn write_interrupt(&mut self, report: &Report) -> Result<(), HidError> {
        self.board(|board| {
            if !board.present || board.fail_writes {
                return Err(HidError);
            }
            board.interrupt_writes.push(*report);
            board.react(report);
            Ok(())
        })
    }

    fn read_interrupt(&mut self) -> Result<Option<Report>, HidError> {
        self.board(|board| {
            if !board.present {
                return Err(HidError);
            }
            if let Some(report) = board.queue.pop_front() {
                return Ok(Some(report));
            }
            if board.streaming && !board.served {
                board.served = true;
                return Ok(Some(board.latest));
            }
            board.served = false;
            Ok(None)
        })
    }

    fn set_output_report(&mut self, report: &Report) -> Result<(), HidError> {
        self.board(|board| {
            if !board.present || board.fail_writes {
                return Err(HidError);
            }
            board.control_writes.push(*report);
            Ok(())
        })
    }

    fn get_input_report(&mut self) -> Result<Report, HidError> {
        self.board(|board| {
            if !board.present || board.fail_control_reads {
                return Err(HidError);
            }
            let mut report = [0; REPORT_LEN];
            report[0] = 1;
            report[1] = 0x85;
            report[2] = board.firmware_status;
            Ok(report)
        })
    }
}
