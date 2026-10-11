//! `amdaemon::server::JvsController` (vtable `0x1405D1898`, App+`0x6E8`) and its four
//! `JvsNodeController`s.
//!
//! The controller owns no `ServerData` block: everything it publishes goes to the JVS half of the ServerIO
//! body (`body+0x8A0`, [`ServerJvs`](crate::schema::jvs::ServerJvs)) and everything it consumes comes from
//! `ClientData.jvs` ([`ClientJvs`](crate::schema::jvs::ClientJvs)). Node controllers always exist for all four
//! slots; the *detected* node count lives in the published body and only the detect-node hub callback
//! ([`Controller::on_jvs_detect_node`]) changes it.
//!
//! Evidence: `spec/controllers/jvs_usbio.md` (claims `jvs_usbio.jvs.*`, `jvs_usbio.exec.jvs.*`,
//! `jvs_usbio.event.jvs.*`, `jvs_usbio.serverio.jvs.*`).

use std::fmt;

use crate::executor::{RequestExecutor, RequestState};
use crate::manager::Manager;
use crate::schema::jvs::{
    ClientJvsRequestRef, JvsAnalogInputMut, JvsAnalogInputPatch, JvsRotaryInputMut, JvsRotaryInputPatch,
    JvsSwitchInputMut, JvsTask, NODE_CAPACITY, SYSTEM_SWITCHES,
};
use libam::jvs::{AnalogState, JvsPort, RotaryState, SwitchState};
use libam::result::AmError;

use crate::controllers::controller::{Controller, ControllerId, Ctx, settle_check};

/// Largest node count the controllers publish, as libam reports it (`u64`).
pub(crate) const MAX_NODES: u64 = NODE_CAPACITY as u64;

/// `amdaemon::server::JvsNodeController` (heap object `0x78`, ctor `0x1400380D0`): node index plus the
/// request executor over `ClientData.jvs.nodes[i]` / `ServerIO.jvs.nodes[i]`.
#[derive(Debug)]
struct JvsNodeController {
    index: usize,
    executor: RequestExecutor,
}

impl JvsNodeController {
    fn new(index: usize) -> Self {
        Self { index, executor: RequestExecutor::default() }
    }

    /// `isBusy` contribution: `request.taskNo != response.taskNo && state != 0`.
    fn is_busy(&self, ctx: &Ctx<'_>) -> bool {
        let request = ctx.client.jvs().request(self.index).generation();
        let response = ctx.server_io.jvs().node_generation(self.index);
        self.executor.is_busy(request, response)
    }

    /// `JvsNodeController::updateInput` (`0x140038400`).
    fn update_input(&self, port: &dyn JvsPort, ctx: &mut Ctx<'_>) {
        if port.handle().is_err() {
            tracing::warn!("libam is not ready");
            return;
        }
        // A node index beyond libam's count (or a null input) is skipped silently.
        let Ok(input) = port.node_input(self.index) else { return };
        let mut body = ctx.server_io.jvs_mut();
        let mut nodes = body.nodes_mut();
        let mut node = nodes.get_mut(self.index).expect("a node index is below the node capacity");
        write_switch(node.switch_input_mut(), &input.switch);
        write_analog(node.analog_input_mut(), &input.analog);
        write_rotary(node.rotary_input_mut(), &input.rotary);
    }

    /// Accept edge of the inlined `RequestExecutor` member (`vf7` body): runs the acceptor once per new
    /// request generation while the executor is idle.
    fn accept(&mut self, port: &mut dyn JvsPort, ctx: &mut Ctx<'_>) {
        let index = self.index;
        let client = ctx.client;
        let request = client.jvs().request(index);
        let response_generation = ctx.server_io.jvs().node_generation(index);
        let manager = &mut *ctx.manager;
        self.executor.accept(request.generation(), response_generation, || accept_task(port, index, &request, manager));
    }

    /// `RequestExecutor<jvs>::checkResult` (`0x140039F10`) with the constant-`Succeeded` checker
    /// (`0x140039C20`).
    fn check_result(&mut self, ctx: &mut Ctx<'_>) {
        let request_generation = ctx.client.jvs().request(self.index).generation();
        let mut body = ctx.server_io.jvs_mut();
        let mut nodes = body.nodes_mut();
        let mut response = nodes.get_mut(self.index).expect("a node index is below the node capacity");
        let outcome = self.executor.check_result_generation_only(
            request_generation,
            response.generation(),
            Some(&mut || RequestState::Succeeded),
        );
        if let Some(completion) = settle_check(outcome) {
            response.generation_mut().set(completion.generation).expect("u32 accepts every value");
        }
    }
}

/// `JvsNodeController::accept` (`0x140038230`): request trace, handle check, then the single `GeneralOutput`
/// task. Returns `Pending` (1) or `Failed` (3); it never returns `Succeeded`.
fn accept_task(
    port: &mut dyn JvsPort,
    index: usize,
    request: &ClientJvsRequestRef<'_>,
    manager: &mut Manager,
) -> RequestState {
    if manager.debug_dump() {
        let trace = request_trace(index, request);
        tracing::info!("{trace}");
    }
    if port.handle().is_err() {
        tracing::warn!("libam is not ready");
        return RequestState::Failed;
    }
    // Unknown tasks fail with `am::Result` code 5 and are not sent to libam.
    let result = match request.task_kind() {
        Some(JvsTask::GeneralOutput) => port.set_general_output(index, request.bits()),
        None => Err(AmError::code(AmError::UNKNOWN_TASK)),
    };
    if result.is_ok() { RequestState::Pending } else { RequestState::Failed }
}

/// Request trace line (`0x140038CE0`). An unknown task has no name (null pointer), which puts the stream in
/// failbit state and drops the rest of the line.
fn request_trace(index: usize, request: &ClientJvsRequestRef<'_>) -> String {
    match request.task_kind() {
        Some(task) => format!("JVS - node[{index}] task: {}, bits: 0x{:016X}", task.name(), request.bits()),
        None => format!("JVS - node[{index}] task: "),
    }
}

/// `sub_140038AA0`: eight system switches, the unclamped player count, then the player matrix.
fn write_switch(mut dst: JvsSwitchInputMut<'_>, src: &SwitchState<4>) {
    let mut systems = dst.systems_mut();
    for bit in 0..SYSTEM_SWITCHES {
        systems
            .get_mut(bit)
            .expect("a system bit selects one of the system switches")
            .update((src.system >> bit) & 1 != 0);
    }
    // Truncation to `u32` is what the original stores.
    dst.player_count_mut().set(src.player_count as u32).expect("u32 accepts every value");
    let mut players = dst.players_mut();
    for (player, mask) in src.processed_players() {
        players.get_mut(player).expect("a processed player has a switch row").update(mask);
    }
}

/// `sub_140038BC0`: the valid-bit count is rewritten for every processed channel, so the last one wins.
fn write_analog(mut dst: JvsAnalogInputMut<'_>, src: &AnalogState<16>) {
    let processed = src.processed();
    let mut values = dst.values().copy_into();
    for (value, channel) in values.iter_mut().zip(processed) {
        *value = channel.value;
    }
    let patch = JvsAnalogInputPatch {
        // Truncation to `u32` is what the original stores.
        channel_count: Some(src.count as u32),
        values: Some(values),
        valid_bit_count: processed.last().map(|channel| channel.valid_bits as u16),
    };
    dst.copy_from(&patch).expect("scalar fields accept every value");
}

/// `sub_140038C60`.
fn write_rotary(mut dst: JvsRotaryInputMut<'_>, src: &RotaryState<16>) {
    let mut values = dst.values().copy_into();
    for (value, &sample) in values.iter_mut().zip(src.processed()) {
        *value = sample;
    }
    let patch = JvsRotaryInputPatch { channel_count: Some(src.count as u32), values: Some(values) };
    dst.copy_from(&patch).expect("scalar fields accept every value");
}

/// `amdaemon::server::JvsController` (`0x14002B0F0` base + `0x140038540`).
pub struct JvsController {
    port: Box<dyn JvsPort>,
    nodes: [JvsNodeController; NODE_CAPACITY],
}

impl fmt::Debug for JvsController {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JvsController").field("nodes", &self.nodes).finish_non_exhaustive()
    }
}

impl JvsController {
    /// Constructs the controller with its four node controllers. Like the original, nothing is published
    /// until the first detect-node callback.
    pub fn new(port: Box<dyn JvsPort>) -> Self {
        Self { port, nodes: std::array::from_fn(JvsNodeController::new) }
    }

    /// `JvsController::updateNodeInfo` (`0x140038880`): refreshes the published node count and every node's
    /// `autoOutputZero`.
    fn update_node_info(&self, ctx: &mut Ctx<'_>) {
        let mut body = ctx.server_io.jvs_mut();
        if self.port.handle().is_err() {
            tracing::warn!("libam is not ready");
            body.node_count_mut().set(0).expect("u32 accepts every value");
            return;
        }
        let detected = self.port.node_count();
        let node_count = if detected > MAX_NODES {
            tracing::error!(detected, "too many nodes");
            NODE_CAPACITY as u32
        } else {
            detected as u32
        };
        body.node_count_mut().set(node_count).expect("u32 accepts every value");
        let mut nodes = body.nodes_mut();
        for index in 0..node_count as usize {
            let auto_output_zero = match self.port.node_auto_output_zero(index) {
                Ok(value) => value,
                Err(_) => {
                    tracing::warn!(node = index, "unexpected error");
                    false
                }
            };
            nodes
                .get_mut(index)
                .expect("a detected node is below the node capacity")
                .auto_output_zero_mut()
                .set(auto_output_zero)
                .expect("bool accepts both values");
        }
    }
}

impl Controller for JvsController {
    fn id(&self) -> ControllerId {
        ControllerId::Jvs
    }

    /// vf3 (`0x140038730`): OR over **all four** nodes, not only the detected ones, so a node whose index is
    /// at or beyond the current `nodeCount` can stay busy forever.
    fn is_busy(&self, ctx: &Ctx<'_>) -> bool {
        self.nodes.iter().any(|node| node.is_busy(ctx))
    }

    /// vf6 (`0x140038780`): for each detected node, refresh its input and then run `checkResult`.
    fn update_server_data(&mut self, ctx: &mut Ctx<'_>) {
        let detected = ctx.server_io.jvs().node_count() as usize;
        for node in self.nodes.iter_mut().take(detected) {
            node.update_input(self.port.as_ref(), ctx);
            node.check_result(ctx);
        }
    }

    /// vf7 (`0x1400387E0`): accept edge for each detected node.
    fn accept_requests(&mut self, ctx: &mut Ctx<'_>) {
        let detected = ctx.server_io.jvs().node_count() as usize;
        for node in self.nodes.iter_mut().take(detected) {
            node.accept(self.port.as_mut(), ctx);
        }
    }

    /// Detect-node lambda (`0x140039C30`): bumps the detect counter, then refreshes the node info.
    fn on_jvs_detect_node(&mut self, ctx: &mut Ctx<'_>) {
        let mut body = ctx.server_io.jvs_mut();
        let counter = body.detect_counter().wrapping_add(1);
        body.detect_counter_mut().set(counter).expect("u32 accepts every value");
        self.update_node_info(ctx);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use libam::jvs::{AnalogChannel, JvsNodeInput};
    use libam::result::AmResult;

    use crate::schema::jvs::{ClientJvsRequestPatch, JvsAnalogInput};

    use super::*;
    use crate::controllers::controller::ControllerSlot;
    use crate::controllers::controller::testing::Harness;

    #[derive(Default)]
    struct FakeState {
        handle_missing: bool,
        node_count: u64,
        /// `None` = libam reports `am::Result` code 1 for that node.
        auto_output_zero: [Option<bool>; 4],
        inputs: [Option<JvsNodeInput>; 4],
        general_refused: bool,
        general_calls: Vec<(usize, u64)>,
    }

    struct FakeJvs(Rc<RefCell<FakeState>>);

    impl JvsPort for FakeJvs {
        fn handle(&self) -> AmResult<()> {
            if self.0.borrow().handle_missing { Err(AmError::code(AmError::NOT_INITIALIZED)) } else { Ok(()) }
        }

        fn node_count(&self) -> u64 {
            self.0.borrow().node_count
        }

        fn node_auto_output_zero(&self, node: usize) -> AmResult<bool> {
            self.0
                .borrow()
                .auto_output_zero
                .get(node)
                .copied()
                .flatten()
                .ok_or(AmError::code(AmError::INVALID_ARGUMENT))
        }

        fn set_general_output(&mut self, node: usize, bits: u64) -> AmResult<()> {
            let mut state = self.0.borrow_mut();
            state.general_calls.push((node, bits));
            if state.general_refused { Err(AmError::code(AmError::FAILED)) } else { Ok(()) }
        }

        fn node_input(&self, node: usize) -> AmResult<JvsNodeInput> {
            self.0.borrow().inputs.get(node).copied().flatten().ok_or(AmError::code(AmError::INVALID_ARGUMENT))
        }
    }

    fn fake() -> (Box<dyn JvsPort>, Rc<RefCell<FakeState>>) {
        let state = Rc::new(RefCell::new(FakeState::default()));
        (Box::new(FakeJvs(Rc::clone(&state))), state)
    }

    /// Controller whose libam reports `nodes` nodes (all with a readable config), already detected.
    fn detected(harness: &mut Harness, nodes: u64) -> (JvsController, Rc<RefCell<FakeState>>) {
        let (port, state) = fake();
        state.borrow_mut().node_count = nodes;
        state.borrow_mut().auto_output_zero = [Some(false); 4];
        let mut controller = JvsController::new(port);
        controller.on_jvs_detect_node(&mut harness.ctx());
        (controller, state)
    }

    fn submit(harness: &mut Harness, node: usize, generation: u32, task: u32, bits: u64) {
        let patch =
            ClientJvsRequestPatch { generation: Some(generation), task: Some(task), info: Some(&bits.to_le_bytes()) };
        let mut block = harness.client.jvs_mut();
        let mut nodes = block.nodes_mut();
        nodes.get_mut(node).expect("a node slot").copy_from(&patch).expect("plain fields");
    }

    /// The published response generation of `node`.
    fn generation(harness: &Harness, node: usize) -> u32 {
        harness.server_io.jvs().node_generation(node)
    }

    /// Published `autoOutputZero` of every node slot.
    fn auto_output_zero(harness: &Harness) -> Vec<bool> {
        harness.server_io.jvs().nodes().iter().map(|node| node.auto_output_zero()).collect()
    }

    /// `(on, countToOn, countToOff)` of system switch `bit` of `node`.
    fn system_switch(harness: &Harness, node: usize, bit: usize) -> (bool, u8, u8) {
        let body = harness.server_io.jvs();
        let node = body.nodes().get(node).expect("a node slot");
        let unit = node.switch_input().systems().get(bit).expect("a system switch");
        (unit.on(), unit.count_to_on(), unit.count_to_off())
    }

    /// `(on, countToOn, countToOff)` of switch `bit` of `player` on `node`.
    fn player_switch(harness: &Harness, node: usize, player: usize, bit: usize) -> (bool, u8, u8) {
        let body = harness.server_io.jvs();
        let node = body.nodes().get(node).expect("a node slot");
        let switches = node.switch_input();
        let row = switches.players().get(player).expect("a player row");
        let unit = row.units().get(bit).expect("a player switch");
        (unit.on(), unit.count_to_on(), unit.count_to_off())
    }

    fn player_count(harness: &Harness, node: usize) -> u32 {
        harness.server_io.jvs().nodes().get(node).expect("a node slot").switch_input().player_count()
    }

    fn analog(harness: &Harness, node: usize) -> JvsAnalogInput {
        harness.server_io.jvs().nodes().get(node).expect("a node slot").analog_input().copy_into()
    }

    #[test]
    fn detect_clamps_to_four_nodes() {
        let mut harness = Harness::running();
        let (_controller, _state) = detected(&mut harness, 7);
        assert_eq!(harness.server_io.jvs().node_count(), 4);
    }

    #[test]
    fn detect_publishes_auto_output_zero_and_stores_false_on_node_errors() {
        let mut harness = Harness::running();
        let (port, state) = fake();
        {
            let mut state = state.borrow_mut();
            state.node_count = 3;
            state.auto_output_zero = [Some(true), None, Some(true), Some(true)];
        }
        let mut controller = JvsController::new(port);
        controller.on_jvs_detect_node(&mut harness.ctx());

        // Node 1 failed (stored as false); node 3 is beyond the detected count and is not queried.
        assert_eq!(auto_output_zero(&harness), [true, false, true, false]);
    }

    #[test]
    fn detect_without_libam_handle_zeroes_the_count_but_still_counts_the_callback() {
        let mut harness = Harness::running();
        let (mut controller, state) = detected(&mut harness, 3);
        state.borrow_mut().handle_missing = true;
        controller.on_jvs_detect_node(&mut harness.ctx());

        let body = harness.server_io.jvs();
        assert_eq!((body.node_count(), body.detect_counter()), (0, 2));
    }

    #[test]
    fn update_polls_only_detected_nodes_and_skips_unreadable_ones_silently() {
        let mut harness = Harness::running();
        let (mut controller, state) = detected(&mut harness, 2);
        {
            let mut state = state.borrow_mut();
            let mut input = JvsNodeInput::default();
            input.switch.system = 0x01;
            state.inputs = [None, Some(input), Some(input), Some(input)];
        }
        controller.update_server_data(&mut harness.ctx());

        assert!(!system_switch(&harness, 0, 0).0, "node 0 has no libam input");
        assert!(system_switch(&harness, 1, 0).0);
        assert!(!system_switch(&harness, 2, 0).0, "node 2 is beyond nodeCount");
    }

    #[test]
    fn switch_edges_are_counted_and_only_four_players_are_processed() {
        let mut harness = Harness::running();
        let (mut controller, state) = detected(&mut harness, 1);
        let mut input = JvsNodeInput::default();
        input.switch.system = 0b0000_0101;
        input.switch.player_count = 6;
        input.switch.player_masks = [1, 1 << 63, 0, 0b10];
        state.borrow_mut().inputs[0] = Some(input);
        controller.update_server_data(&mut harness.ctx());

        let systems: Vec<bool> = (0..8).map(|bit| system_switch(&harness, 0, bit).0).collect();
        assert_eq!(systems, [true, false, true, false, false, false, false, false]);
        assert_eq!(player_count(&harness, 0), 6, "player count is published unclamped");
        assert!(player_switch(&harness, 0, 0, 0).0);
        assert!(player_switch(&harness, 0, 1, 63).0);
        assert!(player_switch(&harness, 0, 3, 1).0);

        // Releasing everything counts one falling edge per previously-set unit; unchanged units count nothing.
        state.borrow_mut().inputs[0] = Some(JvsNodeInput::default());
        controller.update_server_data(&mut harness.ctx());
        let (_, to_on, to_off) = system_switch(&harness, 0, 0);
        assert_eq!((to_on, to_off), (1, 1));
        let (_, to_on, to_off) = system_switch(&harness, 0, 1);
        assert_eq!((to_on, to_off), (0, 0));
        assert_eq!(player_switch(&harness, 0, 1, 63).2, 0, "players beyond playerCount=0 are left untouched");
    }

    #[test]
    fn analog_publishes_unclamped_count_and_the_last_channels_valid_bits() {
        let mut harness = Harness::running();
        let (mut controller, state) = detected(&mut harness, 1);
        let mut input = JvsNodeInput::default();
        input.analog.count = 20;
        for (k, channel) in input.analog.channels.iter_mut().enumerate() {
            *channel = AnalogChannel { value: 100 + k as u16, valid_bits: 0x1_0000 + k as u64 };
        }
        state.borrow_mut().inputs[0] = Some(input);
        controller.update_server_data(&mut harness.ctx());

        let published = analog(&harness, 0);
        assert_eq!(published.channel_count, 20);
        assert_eq!(published.values[0], 100);
        assert_eq!(published.values[15], 115);
        assert_eq!(published.valid_bit_count, 15, "low 16 bits of the last processed channel");

        let mut input = JvsNodeInput::default();
        input.analog.channels[0].valid_bits = 77;
        state.borrow_mut().inputs[0] = Some(input);
        controller.update_server_data(&mut harness.ctx());
        let published = analog(&harness, 0);
        assert_eq!((published.channel_count, published.valid_bit_count), (0, 15), "no channel processed: untouched");
    }

    #[test]
    fn general_output_is_sent_in_post_execute_and_acknowledged_in_the_next_pre_execute() {
        let mut harness = Harness::running();
        let (controller, state) = detected(&mut harness, 2);
        let mut slot = ControllerSlot::new(Box::new(controller));
        slot.pre_execute(&mut harness.ctx());

        submit(&mut harness, 1, 5, 0, 0xDEAD_BEEF_0123_4567);
        slot.pre_execute(&mut harness.ctx());
        assert_eq!(generation(&harness, 1), 0, "not accepted before post_execute");
        assert!(state.borrow().general_calls.is_empty());

        slot.post_execute(&mut harness.ctx());
        assert_eq!(state.borrow().general_calls, [(1, 0xDEAD_BEEF_0123_4567)]);
        assert!(slot.controller().is_busy(&harness.ctx()));
        assert_eq!(generation(&harness, 1), 0, "completes one tick later");

        slot.pre_execute(&mut harness.ctx());
        assert_eq!(generation(&harness, 1), 5);
        assert!(!slot.controller().is_busy(&harness.ctx()));

        // The same generation is never accepted twice.
        slot.post_execute(&mut harness.ctx());
        assert_eq!(state.borrow().general_calls.len(), 1);
    }

    #[test]
    fn unknown_task_never_reaches_libam_yet_is_acknowledged_like_a_success() {
        let mut harness = Harness::running();
        let (controller, state) = detected(&mut harness, 1);
        let mut slot = ControllerSlot::new(Box::new(controller));
        slot.pre_execute(&mut harness.ctx());

        submit(&mut harness, 0, 1, 1, 7);
        slot.post_execute(&mut harness.ctx());
        slot.pre_execute(&mut harness.ctx());

        assert!(state.borrow().general_calls.is_empty());
        assert_eq!(generation(&harness, 0), 1);
    }

    #[test]
    fn libam_refusal_still_acknowledges_the_generation() {
        let mut harness = Harness::running();
        let (controller, state) = detected(&mut harness, 1);
        state.borrow_mut().general_refused = true;
        let mut slot = ControllerSlot::new(Box::new(controller));
        slot.pre_execute(&mut harness.ctx());

        submit(&mut harness, 0, 3, 0, 1);
        slot.post_execute(&mut harness.ctx());
        assert!(slot.controller().is_busy(&harness.ctx()));
        slot.pre_execute(&mut harness.ctx());
        assert_eq!(generation(&harness, 0), 3);
    }

    #[test]
    fn requests_for_undetected_nodes_are_never_accepted() {
        let mut harness = Harness::running();
        let (controller, state) = detected(&mut harness, 1);
        let mut slot = ControllerSlot::new(Box::new(controller));
        slot.pre_execute(&mut harness.ctx());

        submit(&mut harness, 2, 1, 0, 9);
        slot.post_execute(&mut harness.ctx());
        slot.pre_execute(&mut harness.ctx());

        assert!(state.borrow().general_calls.is_empty());
        assert_eq!(generation(&harness, 2), 0);
        assert!(!slot.controller().is_busy(&harness.ctx()), "never accepted, so never busy");
    }

    #[test]
    fn busy_scan_covers_nodes_past_a_shrunken_node_count() {
        let mut harness = Harness::running();
        let (mut controller, state) = detected(&mut harness, 3);
        controller.update_server_data(&mut harness.ctx());
        submit(&mut harness, 2, 1, 0, 9);
        controller.accept_requests(&mut harness.ctx());
        assert!(controller.is_busy(&harness.ctx()));

        // A re-detect shrinks nodeCount: node 2 is no longer completed but still counts as busy.
        state.borrow_mut().node_count = 1;
        controller.on_jvs_detect_node(&mut harness.ctx());
        controller.update_server_data(&mut harness.ctx());
        assert!(controller.is_busy(&harness.ctx()));
        assert_eq!(generation(&harness, 2), 0);

        state.borrow_mut().node_count = 3;
        controller.on_jvs_detect_node(&mut harness.ctx());
        controller.update_server_data(&mut harness.ctx());
        assert!(!controller.is_busy(&harness.ctx()));
        assert_eq!(generation(&harness, 2), 1);
    }
}
