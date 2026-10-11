//! `am::systemio`: DIP switches, push switches and the LED word.

use crate::result::AmResult;

/// The libam `am::systemio` surface used by amdaemon's board I/O controller.
pub trait BoardIoPort {
    /// `systemio` facade accessor `0x140218CB0` on `Manager+8`: `Err` (code 2) when libam is not
    /// initialised. The token itself is never dereferenced by amdaemon.
    fn systemio_facade(&self) -> AmResult<()>;

    /// DIP switch word (`&SystemIo.dip` `0x140217A60`, `value()` `0x140213E30`). libam stores it once, in
    /// `am::systemio::Manager::initialize` (`0x1402ABF70`).
    fn dip(&self) -> u64;

    /// Push switch word (`&dip + 8`, `value()` `0x140213E90`). libam only ever zero-initialises it.
    fn push(&self) -> u64;

    /// LED word (`&SystemIo.led` `0x140217A40`, `value()` `0x140213EE0`); persists between requests.
    fn led(&self) -> u64;

    /// `led.set(v)` (`0x140213ED0`): stores the word and marks it valid.
    fn set_led(&mut self, value: u64);
}
