//! `amdaemon::server::allnet::AirController` (vtable `0x1405E2868`, `App+0xDA8`, `0x80` bytes, new in
//! 2024).
//!
//! A placeholder: the controller never touches libam. It mirrors the process-config flag
//! `allnet_air.enable` into `ServerData+0x47A4`, publishes a fixed payload (`state = 1`, `error = 4`,
//! 32 × `X`) while that flag is set, and accepts the single client task `Clear` without doing anything.
//! The release build refuses `allnet_air.enable = true` at config load (spec `controllers/allnet.md`
//! §5.6), which this crate leaves to the config loader (evidence ids `allnet.air.*`).

use libam::allnet::AirPort;

use crate::executor::{RequestExecutor, RequestState};
use crate::manager::Manager;
use crate::schema::allnet::{AirTask, ClientAirRef, ServerAirDataPatch};
use crate::schema::text::fit_wide;

use crate::controllers::controller::{Controller, ControllerId, Ctx, commit_check};
use libam::config::ProcessConfig;

/// Header of the request trace.
const TRACE_HEADER: &str = "ALL.Net Air - task: ";
/// Label of the only task in the request logger's string table (`0x1405EEE18`).
const CLEAR_LABEL: &str = "Clear";
/// `ServerAirData.state` while Air is enabled.
const ENABLED_STATE: u32 = 1;
/// `ServerAirData.error` while Air is enabled.
const ENABLED_ERROR: u32 = 4;
/// `ServerAirData.text` while Air is enabled: 32 × `L'X'`.
const ENABLED_TEXT: [u16; 32] = [b'X' as u16; 32];

/// `AirController`.
pub struct AirController {
    executor: RequestExecutor,
}

impl std::fmt::Debug for AirController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AirController").field("executor", &self.executor).finish_non_exhaustive()
    }
}

impl AirController {
    /// Constructs the controller (ctor `0x14004FE10`): idle executor, acceptor = `accept_task`, checker
    /// = constant `Succeeded`. The config observer is delivered through
    /// [`Controller::on_process_config`].
    pub fn new(_port: Box<dyn AirPort>) -> Self {
        Self { executor: RequestExecutor::default() }
    }

    /// `RequestExecutor::checkResult` (`0x1400511D0`) with the constant-`Succeeded` checker.
    fn check_result(&mut self, ctx: &mut Ctx<'_>) {
        let generation = ctx.client.allnet_air().generation();
        let mut block = ctx.server.allnet_air_mut();
        let mut payload = block.payload_mut();
        let response_generation = payload.response().generation();
        let outcome = self.executor.check_result(generation, response_generation, || RequestState::Succeeded);
        commit_check(outcome, &mut payload.response_mut());
    }
}

/// The request trace text (`0x140050180`). A task other than `Clear` has no label: the original inserts
/// a null `wchar_t*` there and faults, this port logs the header alone.
fn request_trace(request: &ClientAirRef<'_>) -> String {
    let mut text = String::from(TRACE_HEADER);
    if request.task_kind() == Some(AirTask::Clear) {
        text.push_str(CLEAR_LABEL);
    }
    text
}

/// The acceptor (`0x140050080`): `Clear` is accepted without any effect, every other task fails
/// (`Result 5 "Requested unknown task."`).
fn accept_task(manager: &mut Manager, request: &ClientAirRef<'_>) -> RequestState {
    if manager.debug_dump() {
        let text = request_trace(request);
        tracing::info!("{text}");
    }
    match request.task_kind() {
        Some(AirTask::Clear) => RequestState::Pending,
        None => RequestState::Failed,
    }
}

impl Controller for AirController {
    fn id(&self) -> ControllerId {
        ControllerId::AllnetAir
    }

    /// vf3 `isBusy` (`0x14004FF50`).
    fn is_busy(&self, ctx: &Ctx<'_>) -> bool {
        self.executor
            .is_busy(ctx.client.allnet_air().generation(), ctx.server.allnet_air().payload().response().generation())
    }

    /// vf5 clear (`0x14004FF80`), on the ready edge: `state` and `error` to 0, `text` to the empty string.
    /// `enabled`, the response and the padding are not touched, and the text keeps its stale units behind
    /// the terminator.
    fn clear_server_data(&mut self, ctx: &mut Ctx<'_>) {
        let mut block = ctx.server.allnet_air_mut();
        let mut payload = block.payload_mut();
        let patch = ServerAirDataPatch { state: Some(0), error: Some(0), ..ServerAirDataPatch::default() };
        payload.copy_from(&patch).expect("scalar fields accept every value");
        let mut text = payload.text_mut();
        let empty = fit_wide(&[], text.capacity());
        text.set(&empty).expect("fitted to the field");
    }

    /// vf6 update (`0x14004FFC0`): the placeholder payload while enabled, then `checkResult`.
    fn update_server_data(&mut self, ctx: &mut Ctx<'_>) {
        let mut block = ctx.server.allnet_air_mut();
        if block.enabled() {
            let mut payload = block.payload_mut();
            let patch = ServerAirDataPatch {
                state: Some(ENABLED_STATE),
                error: Some(ENABLED_ERROR),
                ..ServerAirDataPatch::default()
            };
            payload.copy_from(&patch).expect("scalar fields accept every value");
            let mut text = payload.text_mut();
            let fitted = fit_wide(&ENABLED_TEXT, text.capacity());
            text.set(&fitted).expect("fitted to the field");
        }
        self.check_result(ctx);
    }

    /// vf7 accept edge (`0x140050020`).
    fn accept_requests(&mut self, ctx: &mut Ctx<'_>) {
        let request = ctx.client.allnet_air();
        let response_generation = ctx.server.allnet_air().payload().response().generation();
        let manager = &mut *ctx.manager;
        self.executor.accept(request.generation(), response_generation, || accept_task(manager, &request));
    }

    /// Config observer (`0x140050EF0`): `ServerData[0x47A4] = Config[0x4FF9E]`. The original runs it
    /// inside the config load, before the release gate that refuses `enable = true`.
    fn on_process_config(&mut self, config: &ProcessConfig, ctx: &mut Ctx<'_>) {
        ctx.server.allnet_air_mut().enabled_mut().set(config.allnet_air.enable).expect("bool accepts true and false");
    }
}

#[cfg(test)]
mod tests {
    use libam::allnet::AirProcessConfig;

    use crate::schema::allnet::{ClientAirPatch, ServerAirPatch};
    use crate::schema::shared::ResponsePatch;
    use crate::schema::text::fit_wide_str;

    use super::*;
    use crate::controllers::controller::ControllerSlot;
    use crate::controllers::controller::testing::Harness;

    /// `ServerData` offset of the Air block.
    const BLOCK: usize = 0x47A4;
    /// Size of the Air block.
    const BLOCK_SIZE: usize = 0x58;
    /// Block offset of `payload.text`.
    const TEXT: usize = 0x14;

    struct NoLibam;
    impl AirPort for NoLibam {}

    struct Rig {
        h: Harness,
        slot: ControllerSlot,
    }

    impl Rig {
        fn new() -> Self {
            Self { h: Harness::running(), slot: ControllerSlot::new(Box::new(AirController::new(Box::new(NoLibam)))) }
        }

        fn pre(&mut self) {
            self.slot.pre_execute(&mut self.h.ctx());
        }

        fn post(&mut self) {
            self.slot.post_execute(&mut self.h.ctx());
        }

        fn busy(&mut self) -> bool {
            self.slot.controller().is_busy(&self.h.ctx())
        }

        fn configure(&mut self, enable: bool) {
            let config = ProcessConfig { allnet_air: AirProcessConfig { enable }, ..ProcessConfig::default() };
            self.slot.controller_mut().on_process_config(&config, &mut self.h.ctx());
        }

        fn submit(&mut self, raw_task: u32) {
            let generation = self.h.client.allnet_air().generation() + 1;
            let patch = ClientAirPatch { generation: Some(generation), task: Some(raw_task) };
            self.h.client.allnet_air_mut().copy_from(&patch).expect("plain fields");
        }

        /// The published response `(generation, succeeded)`.
        fn response(&self) -> (u32, bool) {
            let response = self.h.server.allnet_air().payload().response();
            (response.generation(), response.succeeded())
        }

        /// The raw bytes of the Air block, to see padding and the units behind a terminator.
        fn raw(&self) -> &[u8] {
            &self.h.server.bytes()[BLOCK..BLOCK + BLOCK_SIZE]
        }
    }

    #[test]
    fn the_config_observer_stores_allnet_air_enable_in_the_wrapper_byte() {
        let mut rig = Rig::new();
        rig.configure(true);
        rig.pre();
        rig.h.server.span_mut(BLOCK + 1, 3).fill(0x77);
        let before = rig.raw().to_vec();
        rig.configure(true);
        assert_eq!(rig.raw()[0], 1);
        rig.configure(false);
        assert_eq!(rig.raw()[0], 0);
        assert_eq!(&rig.raw()[1..], &before[1..], "padding and the payload are not touched");
    }

    #[test]
    fn enabled_air_publishes_the_fixed_payload() {
        let mut rig = Rig::new();
        rig.configure(true);
        rig.pre();
        let payload = rig.h.server.allnet_air().payload();
        assert_eq!((payload.state(), payload.error()), (1, 4));
        assert_eq!(String::from_utf16_lossy(payload.text().as_slice()), "X".repeat(32));
        assert_eq!(&rig.raw()[TEXT + 64..TEXT + 66], [0, 0], "terminated behind the 32nd unit");
    }

    #[test]
    fn disabled_air_keeps_the_cleared_payload() {
        let mut rig = Rig::new();
        let old = fit_wide_str("old", 33);
        let patch =
            ServerAirDataPatch { state: Some(9), error: Some(9), text: Some(&old), ..ServerAirDataPatch::default() };
        rig.h.server.allnet_air_mut().payload_mut().copy_from(&patch).expect("fits the field");
        rig.pre();
        let payload = rig.h.server.allnet_air().payload();
        assert_eq!((payload.state(), payload.error()), (0, 0));
        assert!(payload.text().is_empty());
    }

    #[test]
    fn clear_leaves_the_wrapper_response_padding_and_stale_text_tail_alone() {
        let mut rig = Rig::new();
        // Every value the clear step writes or must keep holds a sentinel, the padding bytes included.
        let stale = fit_wide(&[u16::from(b'X'); 32], 33);
        let sentinel = ServerAirPatch {
            enabled: Some(true),
            payload: Some(ServerAirDataPatch {
                response: Some(ResponsePatch { generation: Some(0xABAB_ABAB), succeeded: Some(true) }),
                state: Some(0xABAB_ABAB),
                error: Some(0xABAB_ABAB),
                text: Some(&stale),
            }),
        };
        rig.h.server.allnet_air_mut().copy_from(&sentinel).expect("fits the fields");
        rig.h.server.span_mut(BLOCK + 1, 3).fill(0xAB);
        rig.h.server.span_mut(BLOCK + 0x56, 2).fill(0xAB);
        let before = rig.raw().to_vec();

        rig.slot.controller_mut().clear_server_data(&mut rig.h.ctx());

        let air = rig.h.server.allnet_air();
        assert!(air.enabled());
        assert_eq!((air.payload().state(), air.payload().error()), (0, 0));
        assert!(air.payload().text().is_empty());
        let after = rig.raw();
        assert_eq!(after[..4], before[..4], "enabled and its padding");
        assert_eq!(after[4..12], before[4..12], "the response");
        assert_eq!(&after[TEXT..TEXT + 2], [0, 0], "empty string");
        assert_eq!(after[TEXT + 2..], before[TEXT + 2..], "units behind the terminator keep their value");
    }

    #[test]
    fn a_clear_request_is_accepted_acknowledged_and_changes_nothing_else() {
        let mut rig = Rig::new();
        rig.configure(true);
        rig.pre();
        let before = rig.raw().to_vec();

        rig.submit(0);
        rig.post();
        assert!(rig.busy(), "accepted but not yet retired");
        assert_eq!(rig.response().0, 0);

        rig.pre();
        assert_eq!(rig.response(), (1, true));
        assert!(!rig.busy());
        let after = rig.raw();
        // Only the response header (`+4..+0xC` of the wrapper) differs from before the request.
        assert_eq!(&after[..4], &before[..4]);
        assert_eq!(&after[12..], &before[12..]);
    }

    #[test]
    fn other_tasks_fail_and_are_retired_as_not_succeeded() {
        for raw in [1, 2, u32::MAX] {
            let mut rig = Rig::new();
            rig.pre();
            rig.submit(raw);
            rig.post();
            rig.pre();
            assert_eq!(rig.response(), (1, false), "task {raw}");
        }
    }
}
