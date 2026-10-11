//! `amdaemon::server::ProcessController` (`App+0x488`, vtable `0x1405D5BC8`).
//!
//! Process is the only controller that is always ready (it also runs in Idle, Setup and Teardown). Every tick
//! it publishes `managerRunning`, generates the daemon's `bootClock` once and commits finished requests. It
//! owns the `Kill` task of the request pair it shares with [`CommonController`](crate::controllers::common::CommonController)
//! (`spec/controllers/process_common_systeminfo.md` §1 and §4).

use libam_api::process::{NextProcessCode, ProcessPort};

use crate::executor::{RequestExecutor, RequestState};
use crate::manager::{Manager, ManagerState};
use crate::schema::process::{ClientProcessRef, NextProcess, ProcessTask, ServerProcessPatch};

use crate::controllers::controller::{Controller, ControllerId, Ctx, commit_check};

/// Trace prefix (`0x1405D59E0`).
const TRACE_PREFIX: &str = "Process - task: ";

/// Kill mapping (`0x140042A2D..0x140042A56`): wire 0 → 1, 1 → 2, 2 → 3, 3 → 4, ≥ 4 → 1.
fn next_process_code(next_process: u32) -> NextProcessCode {
    match NextProcess::try_from(next_process) {
        Ok(NextProcess::SegaBoot) => NextProcessCode::SegaBoot,
        Ok(NextProcess::SegaBootError) => NextProcessCode::SegaBootError,
        Ok(NextProcess::SystemTest) => NextProcessCode::SystemTest,
        Ok(NextProcess::Auto) | Err(_) => NextProcessCode::Auto,
    }
}

/// `amdaemon::server::ProcessController`.
pub struct ProcessController {
    port: Box<dyn ProcessPort>,
    /// Executor at `ctrl+0x28` over the shared process response/request pair (`0x14002A900`).
    exec: RequestExecutor,
}

impl std::fmt::Debug for ProcessController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessController").field("exec", &self.exec).finish_non_exhaustive()
    }
}

impl ProcessController {
    /// Constructs the controller (`0x140042650`): executor idle, no listeners.
    pub fn new(port: Box<dyn ProcessPort>) -> Self {
        Self { port, exec: RequestExecutor::default() }
    }

    /// First tick with `bootClock == 0` (`0x1400427FE..0x14004284F`): the raw counter value, or the local
    /// `DateTime` when the counter getter fails.
    fn boot_clock(&mut self) -> i64 {
        self.port.clock_ticks().unwrap_or_else(|| self.port.local_now().0)
    }
}

impl Controller for ProcessController {
    fn id(&self) -> ControllerId {
        ControllerId::Process
    }

    /// `0x140042750`: a Kill request never holds the App loop (its ack comes after `Manager::finalize`).
    fn is_busy(&self, ctx: &Ctx<'_>) -> bool {
        let request = ctx.client.process();
        request.task_kind() != Some(ProcessTask::Kill)
            && self.exec.is_busy(request.generation(), ctx.server.process().response().generation())
    }

    /// `0x1400427A0`: constant `true`.
    fn is_ready(&self, _ctx: &Ctx<'_>) -> bool {
        true
    }

    /// `0x1400427B0`: publish `managerRunning`, lazily create `bootClock`, then `checkResult`.
    fn update_server_data(&mut self, ctx: &mut Ctx<'_>) {
        let mut block = ctx.server.process_mut();
        let boot_clock = (block.boot_clock() == 0).then(|| self.boot_clock());
        let patch = ServerProcessPatch {
            manager_running: Some(ctx.manager.state() == ManagerState::Running),
            boot_clock,
            ..ServerProcessPatch::default()
        };
        block.copy_from(&patch).expect("scalar fields accept every value");
        check_result(&mut self.exec, ctx, |manager| {
            // Checker `0x140043420`: Kill stays pending until `Manager::finalize` wrote state 0.
            if manager.state() == ManagerState::Idle { RequestState::Succeeded } else { RequestState::Pending }
        });
    }

    /// `0x140042890`: the generic accept edge around `ProcessController::accept`.
    fn accept_requests(&mut self, ctx: &mut Ctx<'_>) {
        let request_generation = ctx.client.process().generation();
        let response_generation = ctx.server.process().response().generation();
        let Self { port, exec } = self;
        exec.accept(request_generation, response_generation, || accept(port.as_mut(), ctx));
    }
}

/// `ProcessController::accept` (`0x140042910`).
fn accept(port: &mut dyn ProcessPort, ctx: &mut Ctx<'_>) -> RequestState {
    let request = ctx.client.process();
    if ctx.manager.debug_dump() {
        let line = format_trace(TRACE_PREFIX, &request);
        tracing::info!("{line}");
    }
    match request.task_kind() {
        Some(ProcessTask::Kill) => {
            // The libam gate is only released through a valid facade; the Teardown transition follows
            // either way.
            if port.process_facade().is_ok() {
                port.set_next_process(next_process_code(request.next_process()));
            } else {
                tracing::warn!("libam is not ready");
            }
            ctx.manager.kill();
            RequestState::Pending
        }
        // Left for Common: no error, no side effect, retried on the next tick.
        Some(ProcessTask::Language | ProcessTask::Reboot | ProcessTask::Preload) => RequestState::Idle,
        // `Result` code 5 "Requested unknown task."
        None => RequestState::Failed,
    }
}

/// `RequestExecutor<process::ServerResponse, process::ClientRequest>::checkResult` (`0x14002AF30`) for the
/// shared process request/response pair, used by both Process (vf6 tail) and Common (vf6).
#[track_caller]
pub(crate) fn check_result(
    exec: &mut RequestExecutor,
    ctx: &mut Ctx<'_>,
    checker: impl FnOnce(&Manager) -> RequestState,
) {
    let request_generation = ctx.client.process().generation();
    let mut block = ctx.server.process_mut();
    let response_generation = block.response().generation();
    let manager = &*ctx.manager;
    let outcome = exec.check_result(request_generation, response_generation, || checker(manager));
    commit_check(outcome, &mut block.response_mut());
}

/// Request trace line of Process (`0x140042B60`) and Common (`0x14002A2D0`): `<prefix><Task>` plus the
/// task's payload. A value without a name (unknown task, `NextProcess` ≥ 4) ends the line early because the
/// original's null wide-string insertion puts its stream in the fail state.
pub(crate) fn format_trace(prefix: &str, request: &ClientProcessRef<'_>) -> String {
    use std::fmt::Write as _;

    let mut line = String::from(prefix);
    let Some(task) = request.task_kind() else { return line };
    line.push_str(task.name());
    match task {
        ProcessTask::Kill => {
            line.push_str(", next process: ");
            if let Ok(next) = NextProcess::try_from(request.next_process()) {
                line.push_str(next.name());
            }
        }
        ProcessTask::Language => {
            line.push_str(", value: ");
            if let Some(language) = request.requested_language() {
                line.push_str(&String::from_utf16_lossy(language.as_slice()));
            }
        }
        ProcessTask::Reboot => {}
        ProcessTask::Preload => {
            // Writing into a `String` cannot fail.
            let _ = write!(line, ", processID: {}", request.process_id());
        }
    }
    line
}

#[cfg(test)]
pub(crate) mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use libam_api::result::AmResult;
    use libam_api::sequence::ProcessState;
    use libam_api::time::DateTime;

    use crate::schema::process::ClientProcessPatch;

    use super::*;
    use crate::controllers::controller::ControllerSlot;
    use crate::controllers::controller::testing::Harness;

    /// What the libam/OS layer was asked to do, plus what it answers.
    struct PortState {
        facade_ok: bool,
        facade_calls: u32,
        next_process: Vec<NextProcessCode>,
        ticks: Option<i64>,
        clock_calls: u32,
        local_now: i64,
    }

    struct TestPort(Rc<RefCell<PortState>>);

    impl ProcessPort for TestPort {
        fn process_facade(&mut self) -> AmResult<()> {
            let mut state = self.0.borrow_mut();
            state.facade_calls += 1;
            if state.facade_ok { Ok(()) } else { Err(libam_api::result::AmError::code(2)) }
        }

        fn set_next_process(&mut self, code: NextProcessCode) {
            self.0.borrow_mut().next_process.push(code);
        }

        fn clock_ticks(&mut self) -> Option<i64> {
            let mut state = self.0.borrow_mut();
            state.clock_calls += 1;
            state.ticks
        }

        fn local_now(&mut self) -> DateTime {
            DateTime(self.0.borrow().local_now)
        }
    }

    fn port_state() -> Rc<RefCell<PortState>> {
        Rc::new(RefCell::new(PortState {
            facade_ok: true,
            facade_calls: 0,
            next_process: Vec::new(),
            ticks: Some(0x5000),
            clock_calls: 0,
            local_now: 0x7777,
        }))
    }

    fn process_slot(state: &Rc<RefCell<PortState>>) -> ControllerSlot {
        ControllerSlot::new(Box::new(ProcessController::new(Box::new(TestPort(Rc::clone(state))))))
    }

    /// One App frame without the IPC exchange: every `preExecute`, then every `postExecute`.
    fn tick(harness: &mut Harness, slot: &mut ControllerSlot) {
        slot.pre_execute(&mut harness.ctx());
        slot.post_execute(&mut harness.ctx());
    }

    /// First frame, then the client echoes the published boot clock.
    fn boot(harness: &mut Harness, slot: &mut ControllerSlot) {
        slot.pre_execute(&mut harness.ctx());
        echo_boot_clock(harness);
    }

    /// The client copies the published boot clock into its echo.
    fn echo_boot_clock(harness: &mut Harness) {
        let boot_clock = harness.server.process().boot_clock();
        set_boot_clock_echo(harness, boot_clock);
    }

    pub(crate) fn set_boot_clock_echo(harness: &mut Harness, boot_clock: i64) {
        let patch = ClientProcessPatch { server_boot_clock: Some(boot_clock), ..ClientProcessPatch::default() };
        harness.client.process_mut().copy_from(&patch).expect("plain fields");
    }

    /// The client writes a request (what the IPC exchange between pre and post delivers).
    pub(crate) fn request(harness: &mut Harness, generation: u32, task: u32, info: [u8; 32]) {
        let patch = ClientProcessPatch {
            generation: Some(generation),
            task: Some(task),
            info: Some(&info),
            ..ClientProcessPatch::default()
        };
        harness.client.process_mut().copy_from(&patch).expect("plain fields");
    }

    /// Info bytes whose first word is `value` (Kill, Preload).
    pub(crate) fn word_info(value: u32) -> [u8; 32] {
        let mut info = [0; 32];
        info[..4].copy_from_slice(&value.to_le_bytes());
        info
    }

    /// Info bytes holding up to 16 UTF-16 units (Language); `units` is not terminated by this helper.
    pub(crate) fn language_info(units: &[u16]) -> [u8; 32] {
        let mut info = [0; 32];
        for (pair, unit) in info.chunks_exact_mut(2).zip(units) {
            pair.copy_from_slice(&unit.to_le_bytes());
        }
        info
    }

    /// The published response `(generation, succeeded)`.
    pub(crate) fn response(harness: &Harness) -> (u32, bool) {
        let response = harness.server.process().response();
        (response.generation(), response.succeeded())
    }

    fn is_busy(harness: &mut Harness, slot: &ControllerSlot) -> bool {
        slot.controller().is_busy(&harness.ctx())
    }

    #[test]
    fn kill_releases_the_libam_gate_with_the_mapped_code_and_enters_teardown() {
        let cases = [
            (0, NextProcessCode::Auto),
            (1, NextProcessCode::SegaBoot),
            (2, NextProcessCode::SegaBootError),
            (3, NextProcessCode::SystemTest),
            (4, NextProcessCode::Auto),
            (u32::MAX, NextProcessCode::Auto),
        ];
        for (wire, code) in cases {
            let state = port_state();
            let mut slot = process_slot(&state);
            let mut harness = Harness::running();
            boot(&mut harness, &mut slot);
            request(&mut harness, 1, 0, word_info(wire));

            slot.post_execute(&mut harness.ctx());

            assert_eq!(state.borrow().next_process, [code], "wire next process {wire}");
            assert_eq!(harness.manager.state(), ManagerState::Terminating);
        }
    }

    #[test]
    fn kill_is_acknowledged_only_after_the_manager_finalized() {
        let state = port_state();
        let mut slot = process_slot(&state);
        let mut harness = Harness::running();
        boot(&mut harness, &mut slot);
        request(&mut harness, 1, 0, word_info(NextProcess::SegaBootError as u32));

        slot.post_execute(&mut harness.ctx());
        assert!(!is_busy(&mut harness, &slot), "a pending Kill must not hold the App loop");

        // Teardown ticks: the checker keeps answering "pending".
        for _ in 0..3 {
            tick(&mut harness, &mut slot);
            assert_eq!(response(&harness), (0, false));
            assert!(!harness.server.process().manager_running());
        }

        // `Manager::finalize` writes state 0, then only Process.preExecute runs.
        harness.manager.mark_finalized();
        slot.pre_execute(&mut harness.ctx());
        assert_eq!(response(&harness), (1, true));
    }

    #[test]
    fn kill_is_honoured_before_the_manager_is_running() {
        let state = port_state();
        let mut slot = process_slot(&state);
        let mut harness = Harness::new();
        boot(&mut harness, &mut slot);
        assert!(slot.is_ready(), "Process is ready in every Manager state");
        request(&mut harness, 1, 0, [0; 32]);

        slot.post_execute(&mut harness.ctx());

        assert_eq!(harness.manager.state(), ManagerState::Terminating);
        assert_eq!(state.borrow().next_process, [NextProcessCode::Auto]);
    }

    #[test]
    fn kill_without_a_facade_still_terminates() {
        let state = port_state();
        state.borrow_mut().facade_ok = false;
        let mut slot = process_slot(&state);
        let mut harness = Harness::running();
        boot(&mut harness, &mut slot);
        request(&mut harness, 1, 0, word_info(NextProcess::SystemTest as u32));

        slot.post_execute(&mut harness.ctx());

        assert!(state.borrow().next_process.is_empty(), "the gate is not released without a facade");
        assert_eq!(harness.manager.state(), ManagerState::Terminating, "the request still tears the daemon down");

        harness.manager.mark_finalized();
        slot.pre_execute(&mut harness.ctx());
        assert!(response(&harness).1, "and is still acknowledged");
    }

    #[test]
    fn requests_are_ignored_until_the_client_echoes_the_boot_clock() {
        let state = port_state();
        let mut slot = process_slot(&state);
        let mut harness = Harness::running();
        slot.pre_execute(&mut harness.ctx());
        request(&mut harness, 1, 0, [0; 32]);

        let stale = harness.server.process().boot_clock() - 1;
        set_boot_clock_echo(&mut harness, stale);
        slot.post_execute(&mut harness.ctx());
        assert_eq!(harness.manager.state(), ManagerState::Running);

        echo_boot_clock(&mut harness);
        slot.post_execute(&mut harness.ctx());
        assert_eq!(harness.manager.state(), ManagerState::Terminating);
    }

    #[test]
    fn tasks_owned_by_common_are_declined_and_retried_every_tick() {
        for task in [1, 2, 3] {
            let state = port_state();
            let mut slot = process_slot(&state);
            let mut harness = Harness::running();
            boot(&mut harness, &mut slot);
            request(&mut harness, 5, task, [0; 32]);

            for _ in 0..3 {
                tick(&mut harness, &mut slot);
            }

            assert_eq!(response(&harness), (0, false), "task {task}");
            assert!(!is_busy(&mut harness, &slot), "Process never holds a task it declined");
            assert_eq!(state.borrow().facade_calls, 0);
            assert_eq!(harness.manager.state(), ManagerState::Running);
        }
    }

    #[test]
    fn unknown_task_fails_and_is_busy_until_committed() {
        let state = port_state();
        let mut slot = process_slot(&state);
        let mut harness = Harness::running();
        boot(&mut harness, &mut slot);
        request(&mut harness, 3, 4, [0; 32]);
        assert!(!is_busy(&mut harness, &slot), "nothing is owned before the accept edge");

        slot.post_execute(&mut harness.ctx());
        assert!(is_busy(&mut harness, &slot));
        assert_eq!(harness.manager.state(), ManagerState::Running);
        assert!(state.borrow().next_process.is_empty());

        slot.pre_execute(&mut harness.ctx());
        assert_eq!(response(&harness), (3, false));
        assert!(!is_busy(&mut harness, &slot));
    }

    #[test]
    fn manager_running_follows_the_manager_state_every_tick() {
        let state = port_state();
        let mut slot = process_slot(&state);
        let mut harness = Harness::new();
        let mut observed = Vec::new();
        for step in 0..5 {
            match step {
                1 => harness.manager.on_change_state(ProcessState(1)),
                2 => harness.manager.on_change_state(ProcessState(4)),
                3 => harness.manager.kill(),
                4 => harness.manager.mark_finalized(),
                _ => {}
            }
            slot.pre_execute(&mut harness.ctx());
            observed.push(harness.server.process().manager_running());
        }
        // Idle, Setup, Running, Teardown, Idle.
        assert_eq!(observed, [false, false, true, false, false]);
    }

    #[test]
    fn boot_clock_is_taken_once_from_the_port_counter() {
        let state = port_state();
        let mut slot = process_slot(&state);
        let mut harness = Harness::new();
        assert_eq!(harness.server.process().boot_clock(), 0);

        slot.pre_execute(&mut harness.ctx());
        assert_eq!(harness.server.process().boot_clock(), 0x5000);

        state.borrow_mut().ticks = Some(0x9999);
        for _ in 0..3 {
            tick(&mut harness, &mut slot);
        }
        assert_eq!(harness.server.process().boot_clock(), 0x5000, "the boot clock never changes afterwards");
        assert_eq!(state.borrow().clock_calls, 1, "the clock is not even read again");
    }

    #[test]
    fn boot_clock_falls_back_to_the_local_date_time_when_the_counter_fails() {
        let state = port_state();
        state.borrow_mut().ticks = None;
        let mut slot = process_slot(&state);
        let mut harness = Harness::new();

        slot.pre_execute(&mut harness.ctx());

        assert_eq!(harness.server.process().boot_clock(), 0x7777);
    }
}
