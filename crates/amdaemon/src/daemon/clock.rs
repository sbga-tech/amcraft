//! The clock of the App's main loop: the tick counter that paces it and sleep (`spec/controllers/lifecycle.md`
//! §3.6-§3.8).
//!
//! Not a backend interface: `std` serves it the same way on every platform. [`Clock`] is the seam the App's tests
//! use to script the tick count instead of waiting for real time to pass; [`StdClock`] is the only real clock.

use std::fmt::Debug;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

/// The clock of the main loop.
pub trait Clock: Debug {
    /// Ticks per second of [`Self::now`]. Always positive.
    fn frequency(&self) -> i64;

    /// Current tick count. Monotonic: setting the wall clock does not move it.
    fn now(&self) -> i64;

    /// Sleeps for `milliseconds`; `0` gives up the rest of the time slice.
    fn sleep(&self, milliseconds: u32);
}

/// Ticks per second: nanoseconds of a `std::time::Instant`.
const TICKS_PER_SECOND: i64 = 1_000_000_000;

/// The main-loop clock (`Clock` ctor `0x140053280`).
///
/// Ticks are the nanoseconds of a `std::time::Instant` since the first reading. The main loop only subtracts two
/// readings; `Instant` is monotonic and on Windows reads `QueryPerformanceCounter`, the original's clock, at a finer
/// resolution than its 10 MHz, so the 66 Hz and 120 Hz periods come out as in the original.
///
/// `sleep` is `std::thread::sleep`. On Windows 10 1803 and later it waits on a high-resolution waitable timer,
/// which is as precise as the original's `Sleep` after `timeBeginPeriod(1)` (the `-f` option) without changing the
/// system timer resolution. A zero sleep yields the time slice, as `Sleep(0)` does.
#[derive(Debug, Clone, Copy, Default)]
pub struct StdClock;

impl Clock for StdClock {
    fn frequency(&self) -> i64 {
        TICKS_PER_SECOND
    }

    fn now(&self) -> i64 {
        static ORIGIN: LazyLock<Instant> = LazyLock::new(Instant::now);
        i64::try_from(ORIGIN.elapsed().as_nanos()).unwrap_or(i64::MAX)
    }

    fn sleep(&self, milliseconds: u32) {
        if milliseconds == 0 {
            std::thread::yield_now();
        } else {
            std::thread::sleep(Duration::from_millis(u64::from(milliseconds)));
        }
    }
}
