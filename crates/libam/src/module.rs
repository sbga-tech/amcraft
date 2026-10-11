//! What every libam module shares: the manager state words, the error cell, the hook context and the event queue.
//!
//! # How a module plugs in
//!
//! A module is a struct in its own directory `src/<module>/` of this crate that [`crate::core::Core`] owns as a field.
//! The main flow ([`crate::flow`]) drives it through three hooks, always in the order of `spec/libam/core.md` M.3
//! and M.4:
//!
//! * `initialize(&mut self, cx: &mut Cx<'_>)`: called once from `OperatingState::enter` (M.4); the module reads its
//!   configuration from `cx.config` (the adopted [`crate::config::AmConfig`]).
//! * `update(&mut self, cx: &mut Cx<'_>, ...)`: called once per frame (M.3). Extra arguments are the values the
//!   original passes from other modules (named in the doc comment of each `update`).
//! * `terminate(&mut self, cx: &mut Cx<'_>)`: called by the terminate order of M.3.
//!
//! plus the read side the flow needs: `state()`, `error_cell()` (the error aggregator, M.6) and `is_ready()` (the
//! `allReady` conjunction, M.2). A module's facades live in `<module>/port.rs`: the port traits, the values they
//! exchange and the handles that implement them over [`crate::shared::Shared`]. Events a module wants to deliver to
//! the daemon go to `cx.events`; the frame returns them in emission order. Blocking work runs on worker threads
//! whose results the module applies inside `update`, so a frame is deterministic.

use crate::config::AmConfig;
use crate::interface::LibamEvent;

use crate::clock::Clock;
use backend::Backend;

/// `am::*::Manager` state word as the original numbers it: 0 uninitialised, 1 initialised/setting up, 2 ready,
/// 3 ready with an error latched (modules differ in which values they use; see each module).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StateWord(pub u32);

impl StateWord {
    /// Not initialised.
    pub const UNINITIALIZED: Self = Self(0);
    /// Initialised, set-up pending.
    pub const INITIALIZED: Self = Self(1);
    /// Set up without error.
    pub const READY: Self = Self(2);
    /// Error latched.
    pub const FAILED: Self = Self(3);

    /// `state in {2, 3}`: the predicate most modules use for "done" (`allReady`, M.2).
    pub const fn is_done(self) -> bool {
        self.0 == 2 || self.0 == 3
    }
}

/// A module's `FatalError` cell reduced to what the aggregator reads: `number == 0` means empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ErrorCell {
    number: u16,
    sub_number: u16,
}

impl ErrorCell {
    /// The empty cell.
    pub const EMPTY: Self = Self { number: 0, sub_number: 0 };

    /// Stores an error (a later error replaces it, as the original's setter does).
    pub fn set(&mut self, number: u16, sub_number: u16) {
        *self = Self { number, sub_number };
    }

    /// Empties the cell.
    pub fn clear(&mut self) {
        *self = Self::EMPTY;
    }

    /// `(number, sub_number)` when the cell is not empty.
    pub fn get(self) -> Option<(u16, u16)> {
        (self.number != 0).then_some((self.number, self.sub_number))
    }

    /// Whether the cell holds no error.
    pub fn is_empty(self) -> bool {
        self.number == 0
    }
}

/// The libam events produced since they were last taken, in emission order.
#[derive(Debug, Default)]
pub struct EventQueue(Vec<LibamEvent>);

impl EventQueue {
    /// Appends an event.
    pub fn push(&mut self, event: LibamEvent) {
        self.0.push(event);
    }

    /// Takes every queued event.
    pub fn take(&mut self) -> Vec<LibamEvent> {
        std::mem::take(&mut self.0)
    }

    /// Number of queued events.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// No event queued.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// What a hook may touch besides its own module: external interfaces, internal time and shared libam state.
pub struct Cx<'a> {
    /// Everything outside the process.
    pub backend: &'a mut Backend,
    /// Libam's internal source of time.
    pub clock: &'a dyn Clock,
    /// Events for the daemon.
    pub events: &'a mut EventQueue,
    /// The configuration the main flow adopted (default until the cabinet-type load completes).
    pub config: &'a AmConfig,
    /// The NVRAM context; record I/O is positional and needs only `&`.
    pub nvram: &'a crate::nvram::NvramContext,
}

/// The state word and error cell every module has; modules embed it and add their own fields.
#[derive(Debug, Default, Clone, Copy)]
pub struct Lifecycle {
    /// `Manager::state`.
    pub state: StateWord,
    /// The module's own error cell, read by the aggregator (M.6).
    pub error: ErrorCell,
}

impl Lifecycle {
    /// `initialize` succeeded: state 1, error cell cleared.
    pub fn begin(&mut self) {
        self.state = StateWord::INITIALIZED;
        self.error.clear();
    }

    /// `terminate`: back to state 0 with an empty cell.
    pub fn end(&mut self) {
        *self = Self::default();
    }

    /// The setup step of a module that has nothing to set up: ready (2), or failed (3) when the error cell holds
    /// an error. Only acts in state 1.
    pub fn finish_setup(&mut self) {
        if self.state == StateWord::INITIALIZED {
            self.state = if self.error.is_empty() { StateWord::READY } else { StateWord::FAILED };
        }
    }
}
