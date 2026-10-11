//! The simulated clock.

use std::cell::Cell;
use std::rc::Rc;

use crate::time::DateTime;

use crate::clock::Clock;

/// Deterministic clock for tests: starts at a fixed local time and moves only when told to (or when slept on).
#[derive(Debug, Clone)]
pub struct SimClock {
    ms: Rc<Cell<u64>>,
    start: DateTime,
}

impl SimClock {
    /// A clock at 2024-05-14 16:00:00 local time and tick 1000.
    pub fn new() -> Self {
        let start = DateTime::make(2024, 5, 14, 16, 0, 0, 0).unwrap_or_default();
        Self { ms: Rc::new(Cell::new(1000)), start }
    }

    /// Moves the clock forward.
    pub fn advance_ms(&self, ms: u64) {
        self.ms.set(self.ms.get() + ms);
    }
}

impl Default for SimClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SimClock {
    fn tick_ms(&self) -> u64 {
        self.ms.get()
    }

    fn performance_counter(&self) -> Option<i64> {
        Some(self.ms.get().cast_signed() * 10_000)
    }

    fn local_now(&self) -> DateTime {
        let elapsed = (self.ms.get() - 1000).cast_signed() * 1000;
        DateTime(self.start.0 + elapsed)
    }

    fn sleep_ms(&self, ms: u64) {
        self.advance_ms(ms);
    }
}
