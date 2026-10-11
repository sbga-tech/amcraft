//! `amdaemon::server::CommonController` (`App+0x508`, vtable `0x1405CA128`).
//!
//! Common publishes the player total and the libam language when it becomes ready and executes the
//! `Language`, `Reboot` and `Preload` tasks of the request pair it shares with
//! [`ProcessController`](crate::controllers::process::ProcessController) (`spec/controllers/process_common_systeminfo.md`
//! §2 and §4). Both controllers keep their own executor over the same request/response blocks and split
//! the task space by answering `Idle` ("not mine") for the tasks they do not own.

use libam_api::common::CommonPort;

use crate::executor::{RequestExecutor, RequestState};
use crate::schema::process::ProcessTask;
use crate::schema::text::fit_wide;

use crate::controllers::controller::{Controller, ControllerId, Ctx};
use crate::controllers::process::{check_result, format_trace};

/// Trace prefix (`0x1405C9EB0`).
const TRACE_PREFIX: &str = "Common - task: ";

/// `amdaemon::server::CommonController`.
pub struct CommonController {
    port: Box<dyn CommonPort>,
    /// Executor at `ctrl+0x28` over the shared process response/request pair (`0x14002A900`).
    exec: RequestExecutor,
}

impl std::fmt::Debug for CommonController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommonController").field("exec", &self.exec).finish_non_exhaustive()
    }
}

impl CommonController {
    /// Constructs the controller (`0x140029D40`): executor idle, no listeners.
    pub fn new(port: Box<dyn CommonPort>) -> Self {
        Self { port, exec: RequestExecutor::default() }
    }
}

impl Controller for CommonController {
    fn id(&self) -> ControllerId {
        ControllerId::Common
    }

    /// `0x140029E40`: a request is in flight on this executor.
    fn is_busy(&self, ctx: &Ctx<'_>) -> bool {
        self.exec.is_busy(ctx.client.process().generation(), ctx.server.process().response().generation())
    }

    /// `0x140029E70`: `updatePlayerTotal`, then `updateLanguage`, once per not-ready → ready edge.
    fn clear_server_data(&mut self, ctx: &mut Ctx<'_>) {
        update_player_total(self.port.as_mut(), ctx);
        update_language(self.port.as_mut(), ctx);
    }

    /// `0x140029EA0`: `checkResult` only; player count and language are not refreshed periodically.
    fn update_server_data(&mut self, ctx: &mut Ctx<'_>) {
        // Checker `0x14002AD50`: constant 2, so an accepted task succeeds in the next update.
        check_result(&mut self.exec, ctx, |_| RequestState::Succeeded);
    }

    /// `0x140029EB0`: the generic accept edge around the acceptor `0x140029F10`.
    fn accept_requests(&mut self, ctx: &mut Ctx<'_>) {
        let request_generation = ctx.client.process().generation();
        let response_generation = ctx.server.process().response().generation();
        let Self { port, exec } = self;
        exec.accept(request_generation, response_generation, || accept(port.as_mut(), ctx));
    }
}

/// `CommonController::updatePlayerTotal` (`0x14002A0C0`).
fn update_player_total(port: &mut dyn CommonPort, ctx: &mut Ctx<'_>) {
    if port.process_facade().is_ok() {
        // Only the low 32 bits of libam's qword are published.
        ctx.server.common_mut().player_count_mut().set(port.player_total() as u32).expect("u32 accepts every value");
    } else {
        tracing::warn!("libam is not ready");
    }
}

/// `CommonController::updateLanguage` (`0x14002A1C0`): at most 14 code units plus NUL are published
/// (`wcsncpy_s(dst, 15, src, 14)`).
fn update_language(port: &mut dyn CommonPort, ctx: &mut Ctx<'_>) {
    match port.language() {
        Ok(language) => {
            let mut block = ctx.server.common_mut();
            let mut field = block.language_mut();
            let fitted = fit_wide(&language, field.capacity());
            field.set(&fitted).expect("fitted to the field");
        }
        Err(_) => tracing::warn!("libam is not ready"),
    }
}

/// The Common acceptor (`0x140029F10`). Any non-OK libam `Result` makes the request fail; the `Result`
/// codes themselves never reach the wire.
fn accept(port: &mut dyn CommonPort, ctx: &mut Ctx<'_>) -> RequestState {
    let request = ctx.client.process();
    if ctx.manager.debug_dump() {
        let line = format_trace(TRACE_PREFIX, &request);
        tracing::info!("{line}");
    }
    let succeeded = match request.task_kind() {
        // Owned by Process: not mine, no side effect, retried on the next tick.
        Some(ProcessTask::Kill) => return RequestState::Idle,
        Some(ProcessTask::Language) => {
            // Facade `0x140218310` answers `Result` code 1 unless the value is NUL-terminated within the field.
            let changed =
                request.requested_language().is_some_and(|language| port.set_language(language.as_slice()).is_ok());
            // Published unconditionally, whether or not the change worked.
            update_language(port, ctx);
            changed
        }
        Some(ProcessTask::Reboot) => {
            if port.board_facade().is_ok() {
                port.reboot();
            }
            // `Result` code 6 "Cannot reboot." is set unconditionally: control only returns when the
            // machine did not reboot.
            false
        }
        Some(ProcessTask::Preload) => port.preload(request.process_id()).is_ok(),
        // `Result` code 5 "Requested unknown task."
        None => false,
    };
    if succeeded { RequestState::Pending } else { RequestState::Failed }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use crate::manager::ManagerState;
    use crate::schema::process::{NextProcess, ProcessTask, ServerProcessPatch};
    use libam_api::process::{NextProcessCode, ProcessPort};
    use libam_api::result::{AmError, AmResult};
    use libam_api::sequence::ProcessState;
    use libam_api::time::DateTime;

    use super::*;
    use crate::controllers::controller::ControllerSlot;
    use crate::controllers::controller::testing::Harness;
    use crate::controllers::process::ProcessController;
    use crate::controllers::process::tests::{language_info, request, response, set_boot_clock_echo, word_info};

    /// What libam is asked to do, plus what it answers.
    struct PortState {
        player_total: u64,
        language: String,
        set_language_ok: bool,
        set_language_calls: Vec<String>,
        language_reads: u32,
        reboot_calls: u32,
        preload_ok: bool,
        preload_calls: Vec<u32>,
    }

    struct TestPort(Rc<RefCell<PortState>>);

    impl CommonPort for TestPort {
        fn process_facade(&mut self) -> AmResult<()> {
            Ok(())
        }

        fn player_total(&mut self) -> u64 {
            self.0.borrow().player_total
        }

        fn language(&mut self) -> AmResult<[u16; 16]> {
            let mut state = self.0.borrow_mut();
            state.language_reads += 1;
            let mut language = [0; 16];
            for (unit, source) in language.iter_mut().zip(state.language.encode_utf16().take(15)) {
                *unit = source;
            }
            Ok(language)
        }

        fn set_language(&mut self, language: &[u16]) -> AmResult<()> {
            let mut state = self.0.borrow_mut();
            let text = String::from_utf16_lossy(language);
            state.set_language_calls.push(text.clone());
            if state.set_language_ok {
                state.language = text;
                Ok(())
            } else {
                Err(AmError::code(AmError::FAILED))
            }
        }

        fn board_facade(&mut self) -> AmResult<()> {
            Ok(())
        }

        fn reboot(&mut self) {
            self.0.borrow_mut().reboot_calls += 1;
        }

        fn preload(&mut self, process_id: u32) -> AmResult<()> {
            let mut state = self.0.borrow_mut();
            state.preload_calls.push(process_id);
            if state.preload_ok { Ok(()) } else { Err(AmError::code(AmError::FAILED)) }
        }
    }

    fn port_state() -> Rc<RefCell<PortState>> {
        Rc::new(RefCell::new(PortState {
            player_total: 3,
            language: "en-US".to_owned(),
            set_language_ok: true,
            set_language_calls: Vec::new(),
            language_reads: 0,
            reboot_calls: 0,
            preload_ok: true,
            preload_calls: Vec::new(),
        }))
    }

    fn common_slot(state: &Rc<RefCell<PortState>>) -> ControllerSlot {
        ControllerSlot::new(Box::new(CommonController::new(Box::new(TestPort(Rc::clone(state))))))
    }

    /// The client has echoed the boot clock, so accept edges fire (Common never touches the clock itself).
    fn echo_boot_clock(harness: &mut Harness) {
        let patch = ServerProcessPatch { boot_clock: Some(0x1234), ..ServerProcessPatch::default() };
        harness.server.process_mut().copy_from(&patch).expect("plain fields");
        set_boot_clock_echo(harness, 0x1234);
    }

    /// A Language request whose tag is `text` cut to 15 units and terminated.
    fn language_request(harness: &mut Harness, generation: u32, text: &str) {
        let units: Vec<u16> = text.encode_utf16().take(15).collect();
        request(harness, generation, ProcessTask::Language as u32, language_info(&units));
    }

    fn is_busy(harness: &mut Harness, slot: &ControllerSlot) -> bool {
        slot.controller().is_busy(&harness.ctx())
    }

    fn published_language(harness: &Harness) -> String {
        String::from_utf16_lossy(harness.server.common().language().as_slice())
    }

    fn player_count(harness: &Harness) -> u32 {
        harness.server.common().player_count()
    }

    #[test]
    fn language_request_is_applied_in_the_accept_edge_and_completes_in_the_next_update() {
        let state = port_state();
        let mut slot = common_slot(&state);
        let mut harness = Harness::running();
        echo_boot_clock(&mut harness);
        slot.pre_execute(&mut harness.ctx());
        assert_eq!(published_language(&harness), "en-US");

        language_request(&mut harness, 1, "ja-JP");
        assert!(!is_busy(&mut harness, &slot), "nothing is owned before the accept edge");
        slot.post_execute(&mut harness.ctx());

        assert_eq!(state.borrow().set_language_calls, ["ja-JP"]);
        assert_eq!(published_language(&harness), "ja-JP", "republished by the acceptor itself");
        assert_eq!(response(&harness), (0, false), "the response is only committed by the next update");
        assert!(is_busy(&mut harness, &slot));

        slot.pre_execute(&mut harness.ctx());
        assert_eq!(response(&harness), (1, true));
        assert!(!is_busy(&mut harness, &slot));
    }

    #[test]
    fn language_without_a_terminator_is_rejected_before_libam_but_still_republished() {
        let state = port_state();
        let mut slot = common_slot(&state);
        let mut harness = Harness::running();
        echo_boot_clock(&mut harness);
        slot.pre_execute(&mut harness.ctx());
        let reads_before = state.borrow().language_reads;
        request(&mut harness, 1, ProcessTask::Language as u32, language_info(&[u16::from(b'x'); 16]));

        slot.post_execute(&mut harness.ctx());
        slot.pre_execute(&mut harness.ctx());

        assert!(state.borrow().set_language_calls.is_empty());
        assert_eq!(state.borrow().language_reads, reads_before + 1);
        assert_eq!(response(&harness), (1, false));
        assert_eq!(published_language(&harness), "en-US");
    }

    #[test]
    fn fifteen_unit_language_is_accepted_and_published_cut_to_fourteen() {
        let state = port_state();
        let mut slot = common_slot(&state);
        let mut harness = Harness::running();
        echo_boot_clock(&mut harness);
        slot.pre_execute(&mut harness.ctx());

        language_request(&mut harness, 1, "abcdefghijklmno");
        slot.post_execute(&mut harness.ctx());
        slot.pre_execute(&mut harness.ctx());

        assert_eq!(state.borrow().set_language_calls, ["abcdefghijklmno"]);
        assert_eq!(response(&harness), (1, true));
        assert_eq!(published_language(&harness), "abcdefghijklmn");
    }

    #[test]
    fn language_refused_by_libam_fails_the_request_and_republishes_the_old_value() {
        let state = port_state();
        state.borrow_mut().set_language_ok = false;
        let mut slot = common_slot(&state);
        let mut harness = Harness::running();
        echo_boot_clock(&mut harness);
        slot.pre_execute(&mut harness.ctx());
        let reads_before = state.borrow().language_reads;

        language_request(&mut harness, 1, "xx-XX");
        slot.post_execute(&mut harness.ctx());
        assert!(is_busy(&mut harness, &slot), "a failed task is still committed one update later");
        slot.pre_execute(&mut harness.ctx());

        assert_eq!(state.borrow().set_language_calls, ["xx-XX"]);
        assert_eq!(state.borrow().language_reads, reads_before + 1);
        assert_eq!(response(&harness), (1, false));
        assert_eq!(published_language(&harness), "en-US");
    }

    #[test]
    fn reboot_that_returns_control_completes_as_failure() {
        let state = port_state();
        let mut slot = common_slot(&state);
        let mut harness = Harness::running();
        echo_boot_clock(&mut harness);
        slot.pre_execute(&mut harness.ctx());
        request(&mut harness, 4, ProcessTask::Reboot as u32, [0; 32]);

        slot.post_execute(&mut harness.ctx());
        assert_eq!(state.borrow().reboot_calls, 1);
        slot.pre_execute(&mut harness.ctx());

        assert_eq!(response(&harness), (4, false));
        assert_eq!(harness.manager.state(), ManagerState::Running);
    }

    #[test]
    fn preload_passes_the_process_id_and_mirrors_the_libam_result() {
        for (libam_ok, expected) in [(true, true), (false, false)] {
            let state = port_state();
            state.borrow_mut().preload_ok = libam_ok;
            let mut slot = common_slot(&state);
            let mut harness = Harness::running();
            echo_boot_clock(&mut harness);
            slot.pre_execute(&mut harness.ctx());
            request(&mut harness, 8, ProcessTask::Preload as u32, word_info(0xCAFE));

            slot.post_execute(&mut harness.ctx());
            slot.pre_execute(&mut harness.ctx());

            assert_eq!(state.borrow().preload_calls, [0xCAFE]);
            assert_eq!(response(&harness), (8, expected));
        }
    }

    #[test]
    fn requests_wait_for_the_running_manager() {
        let state = port_state();
        let mut slot = common_slot(&state);
        let mut harness = Harness::new();
        echo_boot_clock(&mut harness);
        harness.manager.on_change_state(ProcessState(1));
        language_request(&mut harness, 1, "ja-JP");

        for _ in 0..3 {
            slot.pre_execute(&mut harness.ctx());
            slot.post_execute(&mut harness.ctx());
        }
        assert!(state.borrow().set_language_calls.is_empty(), "Setup is not ready");
        assert_eq!(response(&harness), (0, false));

        harness.manager.on_change_state(ProcessState(3));
        for _ in 0..2 {
            slot.pre_execute(&mut harness.ctx());
            slot.post_execute(&mut harness.ctx());
        }
        assert_eq!(state.borrow().set_language_calls, ["ja-JP"]);
        assert_eq!(response(&harness), (1, true));
    }

    #[test]
    fn player_total_and_language_are_published_only_on_the_ready_edge() {
        let state = port_state();
        state.borrow_mut().player_total = 0x1_0000_0007;
        let mut slot = common_slot(&state);
        let mut harness = Harness::new();

        slot.pre_execute(&mut harness.ctx());
        assert_eq!(player_count(&harness), 0, "nothing is published while the Manager is idle");

        harness.manager.on_change_state(ProcessState(3));
        slot.pre_execute(&mut harness.ctx());
        assert_eq!(player_count(&harness), 7, "only the low 32 bits");
        assert_eq!(published_language(&harness), "en-US");

        {
            let mut state = state.borrow_mut();
            state.player_total = 9;
            state.language = "fr-FR".to_owned();
        }
        slot.pre_execute(&mut harness.ctx());
        assert_eq!(player_count(&harness), 7, "no periodic refresh");
        assert_eq!(published_language(&harness), "en-US");

        harness.manager.on_change_state(ProcessState(1));
        slot.pre_execute(&mut harness.ctx());
        harness.manager.on_change_state(ProcessState(3));
        slot.pre_execute(&mut harness.ctx());
        assert_eq!(player_count(&harness), 9, "republished on the next ready edge");
        assert_eq!(published_language(&harness), "fr-FR");
    }

    #[test]
    fn shorter_language_leaves_the_old_tail_behind_the_terminator() {
        let state = port_state();
        state.borrow_mut().language = "en-US".to_owned();
        let mut slot = common_slot(&state);
        let mut harness = Harness::running();
        slot.pre_execute(&mut harness.ctx());

        state.borrow_mut().language = "ja".to_owned();
        slot.controller_mut().clear_server_data(&mut harness.ctx());

        // `ServerData.common.language` (block `0x902C`, field `+0x04`), read raw to see behind the terminator.
        let field = &harness.server.bytes()[0x9030..0x9030 + 2 * 15];
        let units: Vec<u16> = field.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect();
        let text = |s: &str| s.encode_utf16().collect::<Vec<_>>();
        assert_eq!(&units[..3], &text("ja\0")[..]);
        assert_eq!(&units[3..5], &text("US")[..], "wcsncpy_s does not clear the rest of the destination");
    }

    /// The Process side of the shared request pair, for the cross-controller protocol tests.
    struct ProcessSide(Rc<RefCell<Vec<NextProcessCode>>>);

    impl ProcessPort for ProcessSide {
        fn process_facade(&mut self) -> AmResult<()> {
            Ok(())
        }

        fn set_next_process(&mut self, code: NextProcessCode) {
            self.0.borrow_mut().push(code);
        }

        fn clock_ticks(&mut self) -> Option<i64> {
            Some(0x5000)
        }

        fn local_now(&mut self) -> DateTime {
            DateTime(0)
        }
    }

    /// Registry order: Process then Common, sharing one request/response pair.
    struct Pair {
        process: ControllerSlot,
        common: ControllerSlot,
        harness: Harness,
        libam: Rc<RefCell<PortState>>,
        next_process: Rc<RefCell<Vec<NextProcessCode>>>,
    }

    impl Pair {
        fn running() -> Self {
            let libam = port_state();
            let next_process = Rc::new(RefCell::new(Vec::new()));
            let mut pair = Self {
                process: ControllerSlot::new(Box::new(ProcessController::new(Box::new(ProcessSide(Rc::clone(
                    &next_process,
                )))))),
                common: common_slot(&libam),
                harness: Harness::running(),
                libam,
                next_process,
            };
            pair.pre();
            let boot_clock = pair.harness.server.process().boot_clock();
            set_boot_clock_echo(&mut pair.harness, boot_clock);
            pair
        }

        fn pre(&mut self) {
            self.process.pre_execute(&mut self.harness.ctx());
            self.common.pre_execute(&mut self.harness.ctx());
        }

        fn post(&mut self) {
            self.process.post_execute(&mut self.harness.ctx());
            self.common.post_execute(&mut self.harness.ctx());
        }

        fn tick(&mut self) {
            self.pre();
            self.post();
        }

        fn busy(&mut self) -> (bool, bool) {
            let ctx = self.harness.ctx();
            (self.process.controller().is_busy(&ctx), self.common.controller().is_busy(&ctx))
        }
    }

    #[test]
    fn each_task_is_executed_by_exactly_one_of_the_two_controllers() {
        let mut pair = Pair::running();

        language_request(&mut pair.harness, 1, "ja-JP");
        pair.tick();
        pair.pre();
        assert_eq!(response(&pair.harness), (1, true));
        assert_eq!(pair.libam.borrow().set_language_calls, ["ja-JP"]);
        assert!(pair.next_process.borrow().is_empty());
        assert_eq!(pair.harness.manager.state(), ManagerState::Running);

        request(&mut pair.harness, 2, ProcessTask::Kill as u32, word_info(NextProcess::SegaBoot as u32));
        pair.post();
        assert_eq!(*pair.next_process.borrow(), [NextProcessCode::SegaBoot]);
        assert_eq!(pair.libam.borrow().set_language_calls, ["ja-JP"], "Common does not touch Kill");
        assert_eq!(pair.harness.manager.state(), ManagerState::Terminating);
        pair.pre();
        assert_eq!(response(&pair.harness), (1, true), "Kill is only acknowledged after finalize");

        pair.harness.manager.mark_finalized();
        pair.process.pre_execute(&mut pair.harness.ctx());
        assert_eq!(response(&pair.harness), (2, true));
    }

    #[test]
    fn unknown_task_leaves_common_stuck_and_fails_the_next_request_unexecuted() {
        let mut pair = Pair::running();

        request(&mut pair.harness, 1, 9, [0; 32]);
        pair.post();
        assert_eq!(pair.busy(), (true, true), "both executors failed the unknown task");
        pair.pre();
        assert_eq!(response(&pair.harness), (1, false), "Process commits first");
        assert_eq!(pair.busy(), (false, false));

        // Common never saw the commit, so its executor still holds state 3.
        language_request(&mut pair.harness, 2, "ja-JP");
        assert_eq!(pair.busy(), (false, true), "Common reports busy while stuck and the generations differ");
        pair.pre();
        assert_eq!(response(&pair.harness), (2, false), "committed as a failure by the stale state");
        pair.post();
        assert!(pair.libam.borrow().set_language_calls.is_empty(), "the task never ran");

        // The stale state is consumed by that commit: the following request is served normally.
        language_request(&mut pair.harness, 3, "ja-JP");
        pair.post();
        pair.pre();
        assert_eq!(pair.libam.borrow().set_language_calls, ["ja-JP"]);
        assert_eq!(response(&pair.harness), (3, true));
    }

    #[test]
    fn kill_after_an_unknown_task_is_acknowledged_as_failed_and_blocks_process_forever() {
        let mut pair = Pair::running();
        request(&mut pair.harness, 1, 9, [0; 32]);
        pair.post();
        pair.pre();

        request(&mut pair.harness, 2, ProcessTask::Kill as u32, [0; 32]);
        pair.post();
        assert_eq!(pair.harness.manager.state(), ManagerState::Terminating, "Process accepted the Kill");
        pair.pre();
        assert_eq!(response(&pair.harness), (2, false), "Common's stale failure is committed first");

        pair.harness.manager.mark_finalized();
        pair.process.pre_execute(&mut pair.harness.ctx());
        assert_eq!(response(&pair.harness), (2, false), "Process's executor never completes");

        // Process still holds the Kill, so it never accepts another one.
        request(&mut pair.harness, 3, ProcessTask::Kill as u32, [0; 32]);
        pair.post();
        assert_eq!(pair.harness.manager.state(), ManagerState::Idle);
        assert_eq!(pair.next_process.borrow().len(), 1);
        assert_eq!(response(&pair.harness), (2, false));
    }
}
