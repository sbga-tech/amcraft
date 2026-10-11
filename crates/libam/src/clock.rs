//! Libam's internal time seam: monotonic ticks, the performance counter and the local wall clock.
//!
//! [`SystemClock`] uses `std`, `chrono` and `quanta` on every platform. Time is app-owned state, not an external
//! backend interface; callers provide the clock separately when constructing [`crate::Libam`].

use std::sync::LazyLock;
use std::time::{Duration, Instant};

use chrono::{Datelike, Local, Timelike};
use libam_api::time::DateTime;

/// Time as libam reads it (spec `platform.md` part D, `core.md` M.3): modules never call the OS directly.
pub trait Clock {
    /// Monotonic milliseconds (`GetTickCount` style); only differences are meaningful.
    fn tick_ms(&self) -> u64;

    /// A machine-wide counter that only increases from machine start (the original reads
    /// `QueryPerformanceCounter`). Its unit is unspecified: consumers use it as an opaque value that differs between
    /// readings and between processes. `None` when there is no such counter.
    fn performance_counter(&self) -> Option<i64>;

    /// The local wall clock (`DateTime::now(local)`).
    fn local_now(&self) -> DateTime;

    /// Blocks for `ms` milliseconds (the platform reboot wait, `core.md` C.7). The simulated clock advances
    /// itself instead of sleeping.
    fn sleep_ms(&self, ms: u64);
}

/// The clocks of libam on every operating system. `chrono::Local` is the local wall clock (the original's
/// `DateTime::now(local)` is the system clock at 100 ns resolution in local time, `platform.md` A.3.2; chrono reads
/// the same clock through `std` and the active zone from the operating system). `tick_ms` is a `std::time::Instant`
/// count since the first call: every caller (`abaas`, `aime`, `allnet`, `network`, `platform`, `sequence`, `usbio`,
/// ...) only subtracts two readings and nothing persists or sends one. `sleep_ms` is `std::thread::sleep`.
///
/// [`Clock::performance_counter`] is `quanta::Clock::raw`: the CPU's invariant timestamp counter, or the operating
/// system's monotonic counter scaled to nanoseconds where the CPU has none. The original reads
/// `QueryPerformanceCounter`; the value differs in unit but has the properties its two consumers need. It counts up
/// from machine start, so it is non-zero and every daemon process publishes a different boot clock, which is all
/// the game's client library checks (it only compares the echoed value for equality, `chusanApp` `0x5DC620`); and
/// its low bits vary between readings, which is what the AS-6DB seed takes from it.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn tick_ms(&self) -> u64 {
        static ORIGIN: LazyLock<Instant> = LazyLock::new(Instant::now);
        u64::try_from(ORIGIN.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn performance_counter(&self) -> Option<i64> {
        static COUNTER: LazyLock<quanta::Clock> = LazyLock::new(quanta::Clock::new);
        i64::try_from(COUNTER.raw()).ok()
    }

    fn local_now(&self) -> DateTime {
        let now = Local::now();
        DateTime::make(
            now.year(),
            now.month().cast_signed(),
            now.day().cast_signed(),
            now.hour().cast_signed(),
            now.minute().cast_signed(),
            now.second().cast_signed(),
            (now.nanosecond() / 1000).cast_signed(),
        )
        .unwrap_or_default()
    }

    fn sleep_ms(&self, ms: u64) {
        // `std::thread::sleep` sleeps at least the requested time, which is what the original's re-sleeping loop
        // guarantees (`core.md` C.7).
        std::thread::sleep(Duration::from_millis(ms));
    }
}
