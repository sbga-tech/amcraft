//! Runtime state of `amdaemon::arcade::Manager` (`App+0x48`, `0x438` bytes) as seen by controllers.
//!
//! Evidence: `spec/controllers/lifecycle.md` §M2–§M4, §M7. The libam lifecycle (`initialize`,
//! `execute`, `finalize`) and the event hub live in the daemon crate; this type holds the fields
//! controllers read and the helpers they call.

use libam::sequence::ProcessState;

/// `Manager+0x430`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u32)]
pub enum ManagerState {
    /// Constructed, finalized, or libam `ProcessState` 0.
    #[default]
    Idle = 0,
    /// libam `ProcessState` 1 (setup game) or 2 (setup test).
    Setup = 1,
    /// libam `ProcessState` 3 (advertise), 4 (play) or 5 (test).
    Running = 2,
    /// `Manager::kill` (`0x14000A6D0`).
    Terminating = 3,
}

/// Manager fields shared with controllers.
#[derive(Debug, Default)]
pub struct Manager {
    state: ManagerState,
    test_mode: bool,
    debug_dump: bool,
    terminated: bool,
    app_state_events: Vec<u32>,
}

impl Manager {
    /// Constructor state (`0x14000A020`): idle, flags clear.
    pub fn new() -> Self {
        Self::default()
    }

    /// `Manager+0x430`.
    pub fn state(&self) -> ManagerState {
        self.state
    }

    /// `Manager+0x434`.
    pub fn test_mode(&self) -> bool {
        self.test_mode
    }

    /// `Manager+0x435` (copied from process config byte `+0xA4`, `0x14000D870`).
    pub fn debug_dump(&self) -> bool {
        self.debug_dump
    }

    /// Config callback `lambda_9470` (`0x14000D870`).
    pub fn set_debug_dump(&mut self, value: bool) {
        self.debug_dump = value;
    }

    /// Default `Controller::isReady` (`0x14002B210`): state ∈ {Running, Terminating}.
    pub fn controllers_ready(&self) -> bool {
        matches!(self.state, ManagerState::Running | ManagerState::Terminating)
    }

    /// App loop predicate (`0x1400025C0`): state ∈ {Setup, Running}.
    pub fn loop_active(&self) -> bool {
        matches!(self.state, ManagerState::Setup | ManagerState::Running)
    }

    /// `Manager::onChangeState` (`0x14000AED0`): maps unconditionally (from-state ignored); values above 5 leave
    /// both fields unchanged.
    pub fn on_change_state(&mut self, state: ProcessState) {
        tracing::info!(state = state.name(), "libam process state changed");
        let (manager, test) = match state.0 {
            0 => (ManagerState::Idle, false),
            1 => (ManagerState::Setup, false),
            2 => (ManagerState::Setup, true),
            3 | 4 => (ManagerState::Running, false),
            5 => (ManagerState::Running, true),
            _ => return,
        };
        self.state = manager;
        self.test_mode = test;
    }

    /// `Manager::kill` (`0x14000A6D0`).
    pub fn kill(&mut self) {
        self.state = ManagerState::Terminating;
    }

    /// State write performed by `Manager::finalize` (`0x14000A690`); flags are kept.
    pub fn mark_finalized(&mut self) {
        self.state = ManagerState::Idle;
    }

    /// A condition the original reported as `Fatal`/`Unknown` and ended the process on with `__debugbreak`: logs
    /// `reason` and latches [`Self::terminated`]; the App stops before running anything else.
    pub fn terminate(&mut self, reason: &str) {
        tracing::error!(reason, "fatal condition, the daemon stops");
        self.terminated = true;
    }

    /// Whether [`Self::terminate`] was called.
    pub fn terminated(&self) -> bool {
        self.terminated
    }

    /// Application-state broadcast (`0x140006130`, emitted only by `ApmController::checkResult`). The App
    /// delivers queued states to every controller in registry order before the next controller runs.
    pub fn emit_app_state(&mut self, state: u32) {
        self.app_state_events.push(state);
    }

    /// Drains queued application-state broadcasts.
    pub fn take_app_state_events(&mut self) -> Vec<u32> {
        std::mem::take(&mut self.app_state_events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager() -> Manager {
        Manager::new()
    }

    #[test]
    fn unknown_process_state_keeps_fields() {
        let mut m = manager();
        m.on_change_state(ProcessState(3));
        m.on_change_state(ProcessState(9));
        assert_eq!(m.state(), ManagerState::Running);
    }

    #[test]
    fn change_state_overrides_kill() {
        let mut m = manager();
        m.kill();
        m.on_change_state(ProcessState(1));
        assert_eq!(m.state(), ManagerState::Setup);
    }

    #[test]
    fn terminating_still_counts_as_ready_but_stops_loop() {
        let mut m = manager();
        m.kill();
        assert_eq!((m.controllers_ready(), m.loop_active()), (true, false));
    }
}
