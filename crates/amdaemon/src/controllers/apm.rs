//! `amdaemon::server::ApmController` (vtable `0x1405C3978`, ctor `0x14001D5B0`).
//!
//! Starts and ends sub-applications through libam. The acceptor only validates the task; the libam work runs
//! in the *checker* and is retried every tick while libam answers BUSY. Every success is announced on the
//! application-state bus. Evidence: `spec/controllers/abaas_apm.md` §2 (claims `abaas_apm.apm.*`).

use std::fmt::Write as _;

use widestring::U16CStr;

use crate::executor::{RequestExecutor, RequestState};
use crate::manager::Manager;
use crate::schema::apm::{ApmGameExitReason, ApmTask, ClientApmRef, GameStartInfoRef};
use crate::schema::image::ClientImage;
use libam_api::apm::{ApmPort, AppVersion, version_parts};
use libam_api::backup::GameId;
use libam_api::result::{AmError, AmResult};

use crate::controllers::controller::{Controller, ControllerId, Ctx, commit_check};

/// `amdaemon::server::ApmController`.
pub struct ApmController {
    port: Box<dyn ApmPort>,
    executor: RequestExecutor,
}

impl std::fmt::Debug for ApmController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApmController").field("executor", &self.executor).finish_non_exhaustive()
    }
}

impl ApmController {
    /// Constructs the controller (`0x14001D5B0`).
    pub fn new(port: Box<dyn ApmPort>) -> Self {
        Self { port, executor: RequestExecutor::default() }
    }
}

impl Controller for ApmController {
    fn id(&self) -> ControllerId {
        ControllerId::Apm
    }

    fn is_busy(&self, ctx: &Ctx<'_>) -> bool {
        self.executor.is_busy(ctx.client.apm().generation(), ctx.server.apm().response().generation())
    }

    // vf5 is not overridden: nothing is cleared on the ready edge.

    fn update_server_data(&mut self, ctx: &mut Ctx<'_>) {
        // vf6 consists of `RequestExecutor::checkResult` only; nothing is published here.
        let Self { port, executor } = self;
        let client = ctx.client;
        let manager = &mut *ctx.manager;
        let mut block = ctx.server.apm_mut();
        let response_generation = block.response().generation();
        let outcome = executor.check_result(client.apm().generation(), response_generation, || {
            check_request(port.as_mut(), client, manager)
        });
        commit_check(outcome, &mut block.response_mut());
    }

    fn accept_requests(&mut self, ctx: &mut Ctx<'_>) {
        let request = ctx.client.apm();
        let manager = &mut *ctx.manager;
        let response_generation = ctx.server.apm().response().generation();
        self.executor.accept(request.generation(), response_generation, || accept(&request, manager));
    }

    fn on_app_state(&mut self, state: u32, ctx: &mut Ctx<'_>) {
        // Listener `0x14001EF50`, the last entry of the bus.
        ctx.server
            .apm_mut()
            .last_game_exit_reason_mut()
            .set(exit_reason(state))
            .expect("a closed enum accepts every declared value");
    }
}

/// `am::apm::State` → published `lastGameExitReason` (`0x14001EF50`).
///
/// | state | meaning | reason |
/// |---:|---|---|
/// | 1 | sub-application running | running |
/// | 2 | exit, not in test mode, `test = 0` | normal exit |
/// | 3 | exit in test mode, `test = 1` | test-mode exit with test flag |
/// | 4 | exit in test mode, `test = 0` | test-mode exit |
/// | 5 | exit, not in test mode, `test = 1` | exit with test flag |
/// | other | none / idle | unset |
fn exit_reason(state: u32) -> ApmGameExitReason {
    match state {
        1 => ApmGameExitReason::Running,
        2 => ApmGameExitReason::NormalExit,
        3 => ApmGameExitReason::TestModeTestFlagExit,
        4 => ApmGameExitReason::TestModeExit,
        5 => ApmGameExitReason::TestFlagExit,
        _ => ApmGameExitReason::Unset,
    }
}

/// Executor acceptor (`0x14001DA60`): traces, then only validates the task. No libam call happens here.
fn accept(request: &ClientApmRef<'_>, manager: &mut Manager) -> RequestState {
    if manager.debug_dump() {
        let text = trace_line(request);
        tracing::info!("{text}");
    }
    // `task >= 2` is `Result::setCode(5, "Requested unknown task.")` → Failed.
    match request.task_kind() {
        Some(_) => RequestState::Pending,
        None => RequestState::Failed,
    }
}

/// Executor checker `ApmController::checkResult` (`0x14001D780`), polled every tick until it leaves Pending.
///
/// The request is re-read from `ClientData` on every poll, not latched at accept.
fn check_request(port: &mut dyn ApmPort, client: &ClientImage, manager: &mut Manager) -> RequestState {
    if port.apm_handle().is_err() {
        tracing::warn!("libam is not ready");
        return RequestState::Failed;
    }
    let request = client.apm();
    // A payload that fails its proof (an Aime or test flag that is not a `bool`, a game id without terminator)
    // fails the request without reaching libam.
    let invalid_payload = || AmError::code(AmError::INVALID_ARGUMENT);
    let result = match request.task_kind() {
        Some(ApmTask::GameStart) => match request.game_start_info() {
            Some(info) => start_app(port, &info),
            None => Err(invalid_payload()),
        },
        Some(ApmTask::GameExit) => {
            request.game_exit_test().ok_or_else(invalid_payload).and_then(|test| port.end_app(test))
        }
        // The task changed after the acceptor let it through: `Result::setCode(5, ...)`.
        None => Err(AmError::code(AmError::UNKNOWN_TASK)),
    };
    match result {
        Ok(()) => {
            // Broadcast before the executor mirrors the generation: listeners see the new state first.
            let state = port.state();
            manager.emit_app_state(state);
            RequestState::Succeeded
        }
        Err(error) if error.code == AmError::BUSY => RequestState::Pending,
        Err(_) => RequestState::Failed,
    }
}

/// Decodes the request's sub game id for `am::GameId::parse` (`0x140223150`); failure is `am::Result` code 1.
///
/// The id must be exactly four ASCII letters. `GameId::parse` alone would also take other Unicode alphabetics,
/// but the CRT's `iswalpha` answer for non-ASCII characters is not recoverable from the binary, so the request
/// decoder keeps treating them as non-alphabetic.
fn parse_game_id(text: &U16CStr) -> AmResult<GameId> {
    let invalid = || AmError::code(AmError::INVALID_ARGUMENT);
    let [a, b, c, d] = <[u16; 4]>::try_from(text.as_slice()).map_err(|_| invalid())?;
    if ![a, b, c, d].iter().all(|&unit| u8::try_from(unit).is_ok_and(|letter| letter.is_ascii_alphabetic())) {
        return Err(invalid());
    }
    GameId::parse(&[a, b, c, d, 0]).ok_or_else(invalid)
}

/// `GameStart` branch of the checker: parse game id and version, then `am::Manager::startApp`.
///
/// A parse failure is logged and execution continues; `am::Result` stays at code 1.
fn start_app(port: &mut dyn ApmPort, info: &GameStartInfoRef<'_>) -> AmResult<()> {
    let game_id = parse_game_id(info.game_id()).inspect_err(|_| {
        tracing::warn!("invalid sub game id");
    })?;
    let version = AppVersion::from_raw(info.app_version()).inspect_err(|_| {
        tracing::warn!("invalid version");
    })?;
    port.start_app(&game_id, version, info.with_aime())
}

/// Trace line emitted before every accept when the Manager request trace is on (`0x14001DBA0`).
///
/// An unknown task has no name, which puts the original stream into the failed state right after the prefix.
/// Booleans are streamed with `boolalpha`, so they read `true`/`false`.
fn trace_line(request: &ClientApmRef<'_>) -> String {
    let mut line = String::from("Apm - task: ");
    if let Some(task) = request.task_kind() {
        line.push_str(task.name());
        // Writing to a `String` cannot fail.
        let _ = match task {
            ApmTask::GameStart => request.game_start_info().map_or(Ok(()), |info| {
                let (major, minor, patch) = version_parts(info.app_version());
                write!(
                    line,
                    ", version: {major}.{minor:02}.{patch:02}, GameID: {}, with Aime: {}",
                    String::from_utf16_lossy(info.game_id().as_slice()),
                    info.with_aime()
                )
            }),
            ApmTask::GameExit => request.game_exit_test().map_or(Ok(()), |test| write!(line, ", test: {test}")),
        };
    }
    line
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    use crate::schema::apm::ClientApmPatch;

    use super::*;
    use crate::controllers::controller::ControllerSlot;
    use crate::controllers::controller::testing::Harness;

    /// What the fake libam was asked to do.
    #[derive(Debug, PartialEq, Eq)]
    enum Call {
        Start { game_id: String, version: u32, with_aime: bool },
        End { test: bool },
    }

    /// State behind the fake libam facade.
    struct Libam {
        initialised: bool,
        /// Results of the next start/end calls; `Ok` once exhausted.
        results: VecDeque<AmResult<()>>,
        calls: Vec<Call>,
        state: u32,
    }

    struct FakePort(Rc<RefCell<Libam>>);

    impl FakePort {
        fn next_result(&self) -> AmResult<()> {
            self.0.borrow_mut().results.pop_front().unwrap_or(Ok(()))
        }
    }

    impl ApmPort for FakePort {
        fn apm_handle(&mut self) -> AmResult<()> {
            if self.0.borrow().initialised {
                Ok(())
            } else {
                Err(AmError::with_message(AmError::NOT_INITIALIZED, "amw is uninitialized\n"))
            }
        }

        fn start_app(&mut self, game_id: &GameId, version: AppVersion, with_aime: bool) -> AmResult<()> {
            self.0.borrow_mut().calls.push(Call::Start {
                game_id: game_id.to_string(),
                version: version.raw(),
                with_aime,
            });
            self.next_result()
        }

        fn end_app(&mut self, test: bool) -> AmResult<()> {
            self.0.borrow_mut().calls.push(Call::End { test });
            self.next_result()
        }

        fn state(&mut self) -> u32 {
            self.0.borrow().state
        }
    }

    struct Rig {
        harness: Harness,
        slot: ControllerSlot,
        libam: Rc<RefCell<Libam>>,
    }

    const fn version(major: u32, minor: u32, patch: u32) -> u32 {
        major << 16 | minor << 8 | patch
    }

    /// Up to five units, no terminator added: a five character text fills the field.
    fn game_id(text: &str) -> [u16; 5] {
        let mut units = [0; 5];
        for (slot, unit) in units.iter_mut().zip(text.encode_utf16()) {
            *slot = unit;
        }
        units
    }

    impl Rig {
        fn new() -> Self {
            let libam = Rc::new(RefCell::new(Libam {
                initialised: true,
                results: VecDeque::new(),
                calls: Vec::new(),
                state: 0,
            }));
            let slot = ControllerSlot::new(Box::new(ApmController::new(Box::new(FakePort(Rc::clone(&libam))))));
            Self { harness: Harness::running(), slot, libam }
        }

        fn pre(&mut self) {
            let mut ctx = self.harness.ctx();
            self.slot.pre_execute(&mut ctx);
        }

        fn post(&mut self) {
            let mut ctx = self.harness.ctx();
            self.slot.post_execute(&mut ctx);
        }

        fn is_busy(&mut self) -> bool {
            let ctx = self.harness.ctx();
            self.slot.controller().is_busy(&ctx)
        }

        /// Hands every queued bus event to the controller, like the App does after the checker.
        fn deliver_app_states(&mut self) -> Vec<u32> {
            let events = self.harness.manager.take_app_state_events();
            for &state in &events {
                let mut ctx = self.harness.ctx();
                self.slot.controller_mut().on_app_state(state, &mut ctx);
            }
            events
        }

        fn write_request(&mut self, generation: u32, task: u32, info: Option<&[u8; 0x10]>) {
            let patch = ClientApmPatch { generation: Some(generation), task: Some(task), info };
            self.harness.client.apm_mut().copy_from(&patch).expect("plain fields");
        }

        fn game_start(&mut self, generation: u32, info: [u8; 0x10]) {
            self.write_request(generation, ApmTask::GameStart as u32, Some(&info));
        }

        fn game_exit(&mut self, generation: u32, test: bool) {
            let mut info = [0; 0x10];
            info[0] = u8::from(test);
            self.write_request(generation, ApmTask::GameExit as u32, Some(&info));
        }

        fn raw_task(&mut self, generation: u32, task: u32) {
            self.write_request(generation, task, None);
        }

        fn response(&self) -> (u32, bool) {
            let block = self.harness.server.apm();
            let response = block.response();
            (response.generation(), response.succeeded())
        }

        fn exit_reason(&self) -> ApmGameExitReason {
            self.harness.server.apm().last_game_exit_reason()
        }

        fn calls(&self) -> Vec<Call> {
            std::mem::take(&mut self.libam.borrow_mut().calls)
        }

        fn queue_results(&self, results: impl IntoIterator<Item = AmResult<()>>) {
            self.libam.borrow_mut().results.extend(results);
        }

        fn set_state(&self, state: u32) {
            self.libam.borrow_mut().state = state;
        }
    }

    /// The `GameStart` info bytes: version, the five id units as given (no terminator is added), Aime flag.
    fn start_info_units(id: [u16; 5], raw_version: u32, with_aime: bool) -> [u8; 0x10] {
        let mut info = [0; 0x10];
        info[..4].copy_from_slice(&raw_version.to_le_bytes());
        for (bytes, unit) in info[4..14].chunks_exact_mut(2).zip(id) {
            bytes.copy_from_slice(&unit.to_le_bytes());
        }
        info[14] = u8::from(with_aime);
        info
    }

    fn start_info(id: &str, raw_version: u32, with_aime: bool) -> [u8; 0x10] {
        start_info_units(game_id(id), raw_version, with_aime)
    }

    fn busy() -> AmResult<()> {
        Err(AmError::code(AmError::BUSY))
    }

    #[test]
    fn game_start_runs_libam_in_the_tick_after_accept_and_broadcasts_the_new_state() {
        let mut rig = Rig::new();
        rig.set_state(1);
        rig.game_start(1, start_info("sdey", version(1, 2, 3), true));
        rig.pre();
        rig.post();
        assert!(rig.calls().is_empty(), "accept only validates the task");
        assert!(rig.is_busy());
        assert_eq!(rig.response(), (0, false));
        assert!(rig.harness.manager.take_app_state_events().is_empty());

        rig.pre();
        assert_eq!(
            rig.calls(),
            vec![Call::Start { game_id: "SDEY".into(), version: version(1, 2, 3), with_aime: true }]
        );
        assert_eq!(rig.response(), (1, true));
        assert!(!rig.is_busy());
        assert_eq!(rig.deliver_app_states(), vec![1]);
        assert_eq!(rig.exit_reason(), ApmGameExitReason::Running);
    }

    #[test]
    fn game_exit_passes_the_test_flag_and_publishes_the_exit_reason() {
        let mut rig = Rig::new();
        rig.set_state(5);
        rig.game_exit(1, true);
        rig.pre();
        rig.post();
        rig.pre();
        assert_eq!(rig.calls(), vec![Call::End { test: true }]);
        assert_eq!(rig.response(), (1, true));
        assert_eq!(rig.deliver_app_states(), vec![5]);
        assert_eq!(rig.exit_reason(), ApmGameExitReason::TestFlagExit);
    }

    #[test]
    fn tasks_from_two_upwards_are_rejected_at_accept_and_never_reach_libam() {
        for task in [2, 3, u32::MAX] {
            let mut rig = Rig::new();
            rig.game_exit(1, false);
            rig.pre();
            rig.post();
            rig.pre();
            rig.calls();
            rig.harness.manager.take_app_state_events();
            assert_eq!(rig.response(), (1, true));

            rig.raw_task(2, task);
            rig.post();
            assert!(rig.is_busy(), "task {task}: the rejected request still owns the executor until it is answered");
            rig.pre();
            assert_eq!(rig.response(), (2, false), "task {task}");
            assert!(rig.calls().is_empty(), "task {task}");
            assert!(rig.harness.manager.take_app_state_events().is_empty());
        }
    }

    #[test]
    fn libam_busy_keeps_the_request_pending_and_retries_every_tick() {
        let mut rig = Rig::new();
        rig.set_state(1);
        rig.queue_results([busy(), busy(), busy()]);
        rig.game_start(1, start_info("SDEY", version(1, 0, 0), false));
        rig.pre();
        rig.post();
        for attempt in 1..=3 {
            rig.pre();
            rig.post();
            assert_eq!(rig.calls().len(), 1, "attempt {attempt}");
            assert_eq!(rig.response(), (0, false), "attempt {attempt}");
            assert!(rig.is_busy());
            assert!(rig.harness.manager.take_app_state_events().is_empty());
        }

        rig.pre();
        assert_eq!(rig.calls().len(), 1);
        assert_eq!(rig.response(), (1, true));
        assert_eq!(rig.harness.manager.take_app_state_events(), vec![1]);
        assert!(!rig.is_busy());
    }

    #[test]
    fn request_fields_are_reread_on_every_poll() {
        let mut rig = Rig::new();
        rig.queue_results([busy()]);
        rig.game_start(1, start_info("SDEY", version(1, 0, 0), false));
        rig.pre();
        rig.post();
        rig.pre();
        rig.game_start(1, start_info("SDGA", version(2, 0, 1), true));
        rig.pre();
        assert_eq!(
            rig.calls(),
            vec![
                Call::Start { game_id: "SDEY".into(), version: version(1, 0, 0), with_aime: false },
                Call::Start { game_id: "SDGA".into(), version: version(2, 0, 1), with_aime: true },
            ]
        );
    }

    #[test]
    fn every_other_libam_failure_fails_the_request_without_a_broadcast() {
        for code in [1, 2, 5, 6, 7] {
            let mut rig = Rig::new();
            rig.queue_results([Err(AmError::code(code))]);
            rig.game_start(1, start_info("SDEY", version(1, 0, 0), true));
            rig.pre();
            rig.post();
            rig.pre();
            assert_eq!(rig.response(), (1, false), "code {code}");
            assert_eq!(rig.calls().len(), 1, "code {code}");
            assert!(rig.harness.manager.take_app_state_events().is_empty(), "code {code}");
            assert!(!rig.is_busy());
        }
    }

    #[test]
    fn failure_overwrites_an_earlier_success_flag() {
        let mut rig = Rig::new();
        rig.game_exit(1, false);
        rig.pre();
        rig.post();
        rig.pre();
        assert_eq!(rig.response(), (1, true));

        rig.queue_results([Err(AmError::code(AmError::FAILED))]);
        rig.game_exit(2, false);
        rig.post();
        rig.pre();
        assert_eq!(rig.response(), (2, false));
    }

    #[test]
    fn malformed_game_id_fails_without_calling_libam() {
        let unterminated = start_info_units([u16::from(b'a'); 5], version(1, 0, 0), false);
        // An id without a terminator within its five units already fails the payload proof.
        let cases = [
            start_info("abc", version(1, 0, 0), false),
            start_info("abcde", version(1, 0, 0), false),
            start_info("ab1d", version(1, 0, 0), false),
            start_info("ab d", version(1, 0, 0), false),
            start_info("", version(1, 0, 0), false),
            unterminated,
        ];
        for (index, info) in cases.into_iter().enumerate() {
            let mut rig = Rig::new();
            rig.game_start(1, info);
            rig.pre();
            rig.post();
            rig.pre();
            assert_eq!(rig.response(), (1, false), "case {index}");
            assert!(rig.calls().is_empty(), "case {index}");
            assert!(rig.harness.manager.take_app_state_events().is_empty());
        }
    }

    #[test]
    fn version_components_outside_their_ranges_fail() {
        for raw in [version(10_000, 0, 0), version(0, 100, 0), version(0, 0, 100), version(0, 255, 255), u32::MAX] {
            let mut rig = Rig::new();
            rig.game_start(1, start_info("SDEY", raw, false));
            rig.pre();
            rig.post();
            rig.pre();
            assert_eq!(rig.response(), (1, false), "version {raw:#x}");
            assert!(rig.calls().is_empty(), "version {raw:#x}");
        }
    }

    #[test]
    fn version_components_at_their_maximum_are_accepted() {
        let mut rig = Rig::new();
        rig.game_start(1, start_info("SDEY", version(9999, 99, 99), false));
        rig.pre();
        rig.post();
        rig.pre();
        assert_eq!(
            rig.calls(),
            vec![Call::Start { game_id: "SDEY".into(), version: version(9999, 99, 99), with_aime: false }]
        );
        assert_eq!(rig.response(), (1, true));
    }

    #[test]
    fn libam_uninitialised_at_poll_time_fails() {
        let mut rig = Rig::new();
        rig.game_exit(1, false);
        rig.pre();
        rig.post();
        rig.libam.borrow_mut().initialised = false;
        rig.pre();
        assert_eq!(rig.response(), (1, false));
        assert!(rig.calls().is_empty());
        assert!(rig.harness.manager.take_app_state_events().is_empty());
    }

    #[test]
    fn exit_reason_maps_every_application_state() {
        let expected = [
            (0, ApmGameExitReason::Unset),
            (1, ApmGameExitReason::Running),
            (2, ApmGameExitReason::NormalExit),
            (3, ApmGameExitReason::TestModeTestFlagExit),
            (4, ApmGameExitReason::TestModeExit),
            (5, ApmGameExitReason::TestFlagExit),
            (6, ApmGameExitReason::Unset),
            (7, ApmGameExitReason::Unset),
            (u32::MAX, ApmGameExitReason::Unset),
        ];
        for (state, reason) in expected {
            let mut rig = Rig::new();
            // A published value other than the expected one, so that every case proves the write.
            let stale = if reason == ApmGameExitReason::Running {
                ApmGameExitReason::NormalExit
            } else {
                ApmGameExitReason::Running
            };
            rig.harness
                .server
                .apm_mut()
                .last_game_exit_reason_mut()
                .set(stale)
                .expect("a closed enum accepts every declared value");
            let mut ctx = rig.harness.ctx();
            rig.slot.controller_mut().on_app_state(state, &mut ctx);
            assert_eq!(rig.exit_reason(), reason, "state {state}");
        }
    }

    #[test]
    fn exit_reason_is_published_even_while_the_controller_is_not_ready() {
        let mut rig = Rig::new();
        rig.harness.manager = Harness::new().manager;
        let mut ctx = rig.harness.ctx();
        rig.slot.controller_mut().on_app_state(1, &mut ctx);
        assert_eq!(rig.exit_reason(), ApmGameExitReason::Running);
    }
}
