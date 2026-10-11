//! `process` facade: next process, clock.

use crate::result::AmResult;
use crate::time::DateTime;

/// libam "next process" code stored by `set_next_process` (`0x140217700`) and later tested against 3
/// (`0x140234270`, `0x140234AF0`). The wire-name ↔ code pairing is an inference; the numbers are direct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum NextProcessCode {
    /// Code 1: wire `Auto`, and the target of every wire value ≥ 4.
    Auto = 1,
    /// Code 2: wire `SegaBoot`.
    SegaBoot = 2,
    /// Code 3: wire `SegaBootError`.
    SegaBootError = 3,
    /// Code 4: wire `SystemTest`.
    SystemTest = 4,
}

/// libam and operating-system calls made by the process controller.
pub trait ProcessPort {
    /// `facade.process_token` (`0x140218C10`): the libam "process" facade handle. `Err` stands for a NULL
    /// token with `Result` code 2 ("amw is uninitialized"), i.e. the libam singleton is NULL.
    fn process_facade(&mut self) -> AmResult<()>;

    /// `libam.set_next_process` (`0x140217700`, target `0x140233050`): stores `code` in libam's next-process
    /// global (`0x140957F44`) iff libam's kill-guard byte (`app+0x61D8`) is set. Only called with a valid
    /// facade.
    fn set_next_process(&mut self, code: NextProcessCode);

    /// `Clock::tryGet` (`0x1400533C0`, constructor `0x140053280`): a machine-wide counter that only increases
    /// from machine start, `None` when there is none. The original reads `QueryPerformanceCounter`; the value is
    /// the boot clock, which the game's client library only compares for equality (`chusanApp` `0x5DC620`), so its
    /// unit does not matter.
    fn clock_ticks(&mut self) -> Option<i64>;

    /// `DateTime::now(local = 0)` (`0x14005D0F0`): microseconds since 0001-01-01, used when
    /// [`Self::clock_ticks`] fails.
    fn local_now(&mut self) -> DateTime;
}
