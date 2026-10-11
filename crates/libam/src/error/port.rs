//! The error facade (`am::FatalError`): the error records libam raises and keeps in its history rings,
//! [`ErrorPort`], and libam's [`ErrorHandle`] over the error manager.

use crate::result::{AmError, AmResult};
use crate::shared::Shared;
use crate::time::Tm;

/// Units of the libam message buffer (`wchar_t[128]` at item `+0x28`).
pub const FATAL_MESSAGE_UNITS: usize = 128;

/// `am::FatalError` (libam item, 0x12C bytes): one raised error as delivered by libam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FatalError {
    /// Item `+0x00` error number.
    pub number: u16,
    /// Item `+0x02` error sub number.
    pub sub_number: u16,
    /// Item `+0x04` local time the error was raised.
    pub time: Tm,
    /// Item `+0x28` localised message buffer, NUL-terminated or filling the array.
    pub message: [u16; FATAL_MESSAGE_UNITS],
    /// Item `+0x128` raw libam reset type (1 shutdown, 2 operation, 3 test, 4 any).
    pub reset_type: u32,
}

impl Default for FatalError {
    fn default() -> Self {
        Self { number: 0, sub_number: 0, time: Tm::default(), message: [0; FATAL_MESSAGE_UNITS], reset_type: 0 }
    }
}

impl FatalError {
    /// Builds an item from its parts; `message` is truncated to the 127 UTF-16 units libam stores.
    pub fn new(number: u16, sub_number: u16, time: Tm, message: &str, reset_type: u32) -> Self {
        let mut item = Self { number, sub_number, time, reset_type, ..Self::default() };
        let units = message.encode_utf16().take_while(|&unit| unit != 0).take(FATAL_MESSAGE_UNITS - 1);
        for (slot, unit) in item.message.iter_mut().zip(units) {
            *slot = unit;
        }
        item
    }

    /// The message units before the first NUL (the whole buffer when unterminated).
    pub fn message_units(&self) -> &[u16] {
        let end = self.message.iter().position(|&unit| unit == 0).unwrap_or(FATAL_MESSAGE_UNITS);
        &self.message[..end]
    }
}

/// Selector of the two libam error-history rings (`0x1402169B0(err, flag)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HistoryRing {
    /// Flag 1 (`ctx+0x4930`), published as `ServerError::history` ("application" in the old docs [I]).
    Application,
    /// Flag 0 (`ctx+0x3788`), published as `ServerError::system_history` ("system" in the old docs [I]).
    System,
}

/// One row of libam's error occurrence table (`get(i)`, `0x140224A70`): one occurrence counter (`number` only, no
/// sub number).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorOccurrence {
    /// Error number.
    pub number: u16,
    /// Occurrence count.
    pub times: u32,
}

/// libam surface used by the daemon's `ErrorController` (the `am::error` facade, `Manager+8` accessor
/// `0x140218530`).
///
/// libam raises fatal errors asynchronously to the daemon through the Manager hub, which delivers them to the
/// error controller's fatal-error handler (`Controller::on_fatal_error`); `set` may therefore cause such a delivery
/// before it returns.
pub trait ErrorPort {
    /// `Am::errorFacade()` (`0x140218530`): `false` when the status is an error or the facade is null. The
    /// controller reports its `isReady()` precondition failure in that case.
    fn error_facade_available(&self) -> bool;

    /// `Am::language()` (`0x140218440` → `0x140253D20`, libam `ctx+0xAE8`): the first 16 UTF-16 units of the
    /// language string (the only units the daemon ever reads), NUL-terminated or filling the array; `None` when
    /// libam has no context.
    fn language(&self) -> Option<[u16; 16]>;

    /// `error.reset()` (`0x140216950` → `0x1402533F0`): clears the current error if its libam reset type
    /// allows it; no-op without a current error.
    fn reset(&mut self);

    /// `error.set(number, subNumber)` (`0x140216920` → `0x140253370`): raises an error unless one is pending.
    fn set(&mut self, number: u16, sub_number: u16);

    /// `error.clearLogs()` (`0x140216970` → `0x1402331B0`): clears persisted logs, both rings and the
    /// occurrence table.
    fn clear_logs(&mut self);

    /// Ring `count()` (`0x140224690`). libam guarantees `<= 15`.
    fn history_count(&self, ring: HistoryRing) -> u32;

    /// Ring `get(i)` (`0x1402246A0`): row `i`, newest first; status 1 ("invalid num") when `i >= count`.
    fn history_item(&self, ring: HistoryRing, index: u32) -> AmResult<FatalError>;

    /// Occurrence table `count()` (`0x140224A60`, accessor `0x1402169D0`). libam guarantees `<= 222`.
    fn times_count(&self) -> u32;

    /// Occurrence table `get(i)` (`0x140224A70`).
    fn times_item(&self, index: u32) -> AmResult<ErrorOccurrence>;
}

/// `ErrorPort` over the shared state.
#[derive(Debug)]
pub struct ErrorHandle(pub(crate) Shared);

impl ErrorPort for ErrorHandle {
    fn error_facade_available(&self) -> bool {
        self.0.guard().is_ok()
    }

    fn language(&self) -> Option<[u16; 16]> {
        self.0.guard().ok()?;
        Some(self.0.read(|core| core.error.language()))
    }

    fn reset(&mut self) {
        self.0.with(|core| core.reset_error());
    }

    fn set(&mut self, number: u16, sub_number: u16) {
        self.0.with(|core| core.raise_error(number, sub_number));
    }

    fn clear_logs(&mut self) {
        self.0.with(|core| core.error.clear_logs(&core.nvram));
    }

    fn history_count(&self, ring: HistoryRing) -> u32 {
        self.0.read(|core| core.error.ring(ring).len() as u32)
    }

    fn history_item(&self, ring: HistoryRing, index: u32) -> AmResult<FatalError> {
        self.0
            .read(|core| core.error.ring(ring).get(index as usize).cloned())
            .ok_or_else(|| AmError::with_message(1, "Invalid num"))
    }

    fn times_count(&self) -> u32 {
        self.0.read(|core| core.error.table.len() as u32)
    }

    fn times_item(&self, index: u32) -> AmResult<ErrorOccurrence> {
        self.0
            .read(|core| core.error.table.get(index as usize).map(|&(number, times)| ErrorOccurrence { number, times }))
            .ok_or_else(|| AmError::with_message(1, "Invalid num"))
    }
}
