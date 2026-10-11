//! `amdaemon::server::UsbIOController` (vtable `0x1405E0488`, App+`0x7D0`), its four `UsbIONodeController`s
//! and the embedded `UsbIOEventListener` (firmware-update progress).
//!
//! Like [`crate::controllers::jvs`], the controller owns no `ServerData` block: it publishes into the USBIO
//! half of the ServerIO body (`body+0x0`, [`ServerUsbIo`](crate::schema::usb_io::ServerUsbIo)) and consumes
//! `ClientData.usb_io` ([`ClientUsbIo`](crate::schema::usb_io::ClientUsbIo)). The detected node count lives in
//! the published body and changes only through the detect-node hub callback
//! ([`Controller::on_usb_io_detect_node`]).
//!
//! Evidence: `spec/controllers/jvs_usbio.md` (claims `jvs_usbio.usbio.*`, `jvs_usbio.exec.usbio.*`,
//! `jvs_usbio.event.usbio.*`, `jvs_usbio.serverio.usbio.*`).

use std::fmt::{self, Write as _};

use crate::executor::{RequestExecutor, RequestState};
use crate::manager::Manager;
use crate::schema::text::fit_wide;
use crate::schema::usb_io::{
    ClientUsbIoRequestRef, NODE_CAPACITY, ServerUsbIoNodeMut, ServerUsbIoPatch, UNIQUE_SLOTS, UsbIoAnalogInputMut,
    UsbIoAnalogInputPatch, UsbIoOutputInfoPatch, UsbIoRotaryInputMut, UsbIoRotaryInputPatch, UsbIoSwitchInputMut,
    UsbIoTask, UsbIoUniqueDescriptorPatch, UsbIoUniqueInputMut,
};
use libam::jvs::{AnalogState, RotaryState, SwitchState};
use libam::usbio::{
    FirmwareEvent, FirmwareStatus, UniqueCommand, UniqueDescriptors, UsbIoNodeInfo, UsbIoPort, UsbIoUniqueFunction,
};

use crate::controllers::controller::{Controller, ControllerId, Ctx, commit_check};
use crate::controllers::jvs::MAX_NODES;
use libam::config::{AppConfig, ProcessConfig};

/// Size argument of `sendGeneralOutput(bits, size)`: the value is always sent as eight bytes.
const GENERAL_OUTPUT_SIZE: u64 = 8;

/// `amdaemon::server::UsbIONodeController` (heap object `0x78`, ctor `0x140049C60`): node index plus the
/// request executor over `ClientData.usb_io.nodes[i]` / `ServerIO.usb_io.nodes[i]`.
#[derive(Debug)]
struct UsbIoNodeController {
    index: usize,
    executor: RequestExecutor,
}

impl UsbIoNodeController {
    fn new(index: usize) -> Self {
        Self { index, executor: RequestExecutor::default() }
    }

    /// `isBusy` contribution: `request.taskNo != response.taskNo && state != 0`.
    fn is_busy(&self, ctx: &Ctx<'_>) -> bool {
        let request = ctx.client.usb_io().request(self.index).generation();
        let response = ctx.server_io.usb_io().node_generation(self.index);
        self.executor.is_busy(request, response)
    }

    /// `UsbIONodeController::updateInput` (`0x14004A180`). The five libam getters are checked independently:
    /// a failing getter skips only its own region.
    fn update_input(&self, port: &dyn UsbIoPort, ctx: &mut Ctx<'_>) {
        if port.handle().is_err() {
            tracing::warn!("libam is not ready");
            return;
        }
        let index = self.index;
        let mut body = ctx.server_io.usb_io_mut();
        let mut nodes = body.nodes_mut();
        let mut node = nodes.get_mut(index).expect("a node index is below the node capacity");
        if let Ok(switch) = port.node_switch(index) {
            write_switch(node.switch_input_mut(), &switch);
        }
        if let Ok(analog) = port.node_analog(index) {
            write_analog(node.analog_input_mut(), &analog);
        }
        if let Ok(rotary) = port.node_rotary(index) {
            write_rotary(node.rotary_input_mut(), &rotary);
        }
        if let Ok(descriptors) = port.node_unique_descriptors(index) {
            write_unique_descriptors(node.unique_input_mut(), &descriptors);
        }
        if let Ok(data) = port.node_unique_data(index) {
            node.unique_input_mut().data_mut().set(&data).expect("the data block is exactly the field");
        }
    }

    /// Accept edge of the inlined `RequestExecutor` member (`vf7` body): runs the acceptor once per new
    /// request generation while the executor is idle.
    fn accept(&mut self, port: &mut dyn UsbIoPort, ctx: &mut Ctx<'_>) {
        let index = self.index;
        let client = ctx.client;
        let request = client.usb_io().request(index);
        let (response_generation, commands) = {
            let body = ctx.server_io.usb_io();
            let node = body.nodes().get(index).expect("a node index is below the node capacity");
            (node.response().generation(), *node.output_info().unique_commands())
        };
        let manager = &mut *ctx.manager;
        self.executor.accept(request.generation(), response_generation, || {
            accept_task(port, index, &request, &commands, manager)
        });
    }

    /// `RequestExecutor<usbio>::checkResult` (`0x14004CA50`) with the constant-`Succeeded` checker
    /// (`0x14004C5D0`).
    fn check_result(&mut self, ctx: &mut Ctx<'_>) {
        let request_generation = ctx.client.usb_io().request(self.index).generation();
        let mut body = ctx.server_io.usb_io_mut();
        let mut nodes = body.nodes_mut();
        let mut node = nodes.get_mut(self.index).expect("a node index is below the node capacity");
        let mut response = node.response_mut();
        let outcome = self.executor.check_result(request_generation, response.generation(), || RequestState::Succeeded);
        commit_check(outcome, &mut response);
    }
}

/// `UsbIONodeController::accept` (`0x140049E90`): request trace, handle check, then the single `Output`
/// task. Returns `Pending` (1) or `Failed` (3); it never returns `Succeeded`.
///
/// General output, PWM slots `0..8` and the requested unique commands are **all** attempted even after an
/// earlier send failed; the original keeps the first error code, which only matters as "failed or not".
fn accept_task(
    port: &mut dyn UsbIoPort,
    index: usize,
    request: &ClientUsbIoRequestRef<'_>,
    commands: &[u8; UNIQUE_SLOTS],
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
    // Unknown tasks fail the request and are not sent to libam.
    let Some(UsbIoTask::Output) = request.task_kind() else { return RequestState::Failed };
    // A payload that does not decode (a gate byte other than 0 or 1) fails the request before libam is called.
    let Some(info) = request.output() else { return RequestState::Failed };
    let mut failed = false;
    let general = info.general();
    if general.requested() {
        failed |= port.send_general_output(index, general.bits(), GENERAL_OUTPUT_SIZE).is_err();
    }
    let pwm = info.pwm();
    if pwm.requested() {
        for (slot, &duty) in pwm.duties().iter().enumerate() {
            failed |= port.send_pwm_output(index, slot, duty).is_err();
        }
    }
    for (unique, &code) in info.uniques().iter().zip(commands) {
        if unique.requested() {
            let command = UniqueCommand { code, data: unique.data() };
            failed |= port.send_unique_command(index, &command).is_err();
        }
    }
    if failed { RequestState::Failed } else { RequestState::Pending }
}

/// Request trace text (`0x14004AB90`). An unknown task has no name (null pointer), which puts the stream in
/// failbit state and drops the rest of the line. A payload that does not decode has no readable gates, so its
/// detail lines are omitted (the request fails).
fn request_trace(index: usize, request: &ClientUsbIoRequestRef<'_>) -> String {
    let mut text = format!("USB I/O - node[{index}] task: ");
    let Some(task) = request.task_kind() else { return text };
    text.push_str(task.name());
    match task {
        UsbIoTask::Output => {
            let Some(info) = request.output() else { return text };
            let general = info.general();
            if general.requested() {
                let _ = write!(text, "\n  general - bits: 0x{:016X}", general.bits());
            }
            let pwm = info.pwm();
            if pwm.requested() {
                text.push_str("\n  pwm     - duties:");
                for &duty in pwm.duties() {
                    let _ = write!(text, " 0x{duty:02X}");
                }
            }
            for (slot, unique) in info.uniques().iter().enumerate() {
                if unique.requested() {
                    let _ = write!(text, "\n  unique{slot} -  data:");
                    for &byte in unique.data() {
                        let _ = write!(text, " 0x{byte:02X}");
                    }
                }
            }
        }
    }
    text
}

/// `sub_14004A850`: test (bit 1) and tilt (bit 0) switches, the unclamped player count, then two players.
fn write_switch(mut dst: UsbIoSwitchInputMut<'_>, src: &SwitchState<2>) {
    dst.test_mut().update(src.system & 0x02 != 0);
    dst.tilt_mut().update(src.system & 0x01 != 0);
    // Truncation to `u32` is what the original stores.
    dst.player_count_mut().set(src.player_count as u32).expect("u32 accepts every value");
    let mut players = dst.players_mut();
    for (player, mask) in src.processed_players() {
        players.get_mut(player).expect("a processed player has a switch row").update(mask);
    }
}

/// `sub_14004A970`: the valid-bit count is rewritten for every processed channel, so the last one wins.
fn write_analog(mut dst: UsbIoAnalogInputMut<'_>, src: &AnalogState<8>) {
    let processed = src.processed();
    let mut values = dst.values().copy_into();
    for (value, channel) in values.iter_mut().zip(processed) {
        *value = channel.value;
    }
    let patch = UsbIoAnalogInputPatch {
        // Truncation to `u32` is what the original stores.
        channel_count: Some(src.count as u32),
        values: Some(values),
        valid_bit_count: processed.last().map(|channel| channel.valid_bits as u16),
    };
    dst.copy_from(&patch).expect("scalar fields accept every value");
}

/// `sub_14004AA10`.
fn write_rotary(mut dst: UsbIoRotaryInputMut<'_>, src: &RotaryState<4>) {
    let mut values = dst.values().copy_into();
    for (value, &sample) in values.iter_mut().zip(src.processed()) {
        *value = sample;
    }
    let patch = UsbIoRotaryInputPatch { channel_count: Some(src.count as u32), values: Some(values) };
    dst.copy_from(&patch).expect("scalar fields accept every value");
}

/// `sub_14004AA90`: the reported count, then the first `min(count, 2)` descriptors.
fn write_unique_descriptors(mut dst: UsbIoUniqueInputMut<'_>, src: &UniqueDescriptors) {
    // Truncation to `u32` is what the original stores.
    dst.reported_count_mut().set(src.count as u32).expect("u32 accepts every value");
    let mut descriptors = dst.descriptors_mut();
    for (index, descriptor) in src.processed().iter().enumerate() {
        let patch = UsbIoUniqueDescriptorPatch {
            code: Some(descriptor.code),
            flag0: Some(descriptor.flags & 1 != 0),
            flag1: Some(descriptor.flags & 2 != 0),
            reserved_04: Some(0),
        };
        descriptors
            .get_mut(index)
            .expect("a processed descriptor has a slot")
            .copy_from(&patch)
            .expect("scalar fields accept every value");
    }
}

/// `sub_14004A7A0`: board name, code and revision. A missing record (null pointer) publishes empty strings
/// and revision `0`.
fn write_node_info(mut node: ServerUsbIoNodeMut<'_>, info: Option<UsbIoNodeInfo>) {
    let info = info.unwrap_or_default();
    let mut name = node.name_mut();
    let fitted = fit_wide(&info.name, name.capacity());
    name.set(&fitted).expect("fitted to the field");
    let mut code = node.code_mut();
    let fitted = fit_wide(&info.code, code.capacity());
    code.set(&fitted).expect("fitted to the field");
    node.revision_mut().set(info.revision).expect("u8 accepts every value");
}

/// `UsbIOEventListener::updateNodeCount` (`0x140049970`): publishes libam's node count clamped to four. A
/// missing handle is logged and **leaves the count unchanged**.
fn update_node_count(port: &dyn UsbIoPort, ctx: &mut Ctx<'_>) {
    if port.handle().is_err() {
        tracing::warn!("libam is not ready");
        return;
    }
    let detected = port.node_count();
    let node_count = if detected > MAX_NODES {
        tracing::error!(detected, "too many nodes");
        NODE_CAPACITY as u32
    } else {
        detected as u32
    };
    ctx.server_io.usb_io_mut().node_count_mut().set(node_count).expect("u32 accepts every value");
}

/// `UsbIOEventListener::updateNodeInfo` (`0x140049AA0`): for each published node, `autoOutputZero` and the
/// name/code/revision record. The record is copied even when the preceding `autoOutputZero` call failed.
fn update_node_info(port: &dyn UsbIoPort, ctx: &mut Ctx<'_>) {
    if port.handle().is_err() {
        tracing::warn!("libam is not ready");
        return;
    }
    let mut body = ctx.server_io.usb_io_mut();
    let published = body.node_count() as usize;
    let mut nodes = body.nodes_mut();
    for index in (0..NODE_CAPACITY).take(published) {
        let mut node = nodes.get_mut(index).expect("a node index is below the node capacity");
        let auto_output_zero = match port.node_auto_output_zero(index) {
            Ok(value) => value,
            Err(_) => {
                tracing::warn!(node = index, "unexpected error");
                false
            }
        };
        node.output_info_mut().auto_output_zero_mut().set(auto_output_zero).expect("bool accepts both values");
        write_node_info(node, port.node_info(index));
    }
}

/// `amdaemon::server::UsbIOEventListener` state (embedded in the controller at `+0x28`): the firmware
/// progress accumulators `curProgress` (`+0x40`) and `doneProgress` (`+0x44`).
#[derive(Debug, Default)]
struct UsbIoEventListener {
    current_progress: u32,
    done_progress: u32,
}

impl UsbIoEventListener {
    /// vf1 `onProgress` (`0x140049920`): `firmwareProgress = (percent + doneProgress) / nodeCount` with
    /// unsigned 32-bit arithmetic, where `nodeCount` is the published count.
    ///
    /// Deviation: the original has no zero guard, so `nodeCount == 0` raises a divide fault and ends the
    /// process. The port keeps the previous `firmwareProgress` in that case (the faulting instruction
    /// precedes the store); `curProgress` has already been updated at that point, as in the original.
    fn on_progress(&mut self, percent: u32, ctx: &mut Ctx<'_>) {
        self.current_progress = percent;
        let mut body = ctx.server_io.usb_io_mut();
        if let Some(progress) = percent.wrapping_add(self.done_progress).checked_div(body.node_count()) {
            body.firmware_progress_mut().set(progress).expect("u32 accepts every value");
        }
    }

    /// vf2 `onNodeStatus` (`0x140049900`): a node start resets `curProgress`, a node finish folds it into
    /// `doneProgress` first.
    fn on_node_status(&mut self, status: FirmwareStatus) {
        match status {
            FirmwareStatus::Idle => {}
            FirmwareStatus::Started => self.current_progress = 0,
            FirmwareStatus::Finished => {
                self.done_progress = self.done_progress.wrapping_add(self.current_progress);
                self.current_progress = 0;
            }
        }
    }

    /// vf3 `onUpdateState` (`0x1400498D0`). `curProgress` is deliberately not reset by `Started`. `Finished`
    /// refreshes names and `autoOutputZero` only; it touches neither `nodeCount` nor `detectCounter`.
    fn on_update_state(&mut self, status: FirmwareStatus, port: &dyn UsbIoPort, ctx: &mut Ctx<'_>) {
        let mut body = ctx.server_io.usb_io_mut();
        match status {
            FirmwareStatus::Idle => body.firmware_updating_mut().set(false).expect("bool accepts both values"),
            FirmwareStatus::Started => {
                let patch = ServerUsbIoPatch {
                    firmware_updating: Some(true),
                    firmware_progress: Some(0),
                    ..ServerUsbIoPatch::default()
                };
                body.copy_from(&patch).expect("scalar fields accept every value");
                self.done_progress = 0;
            }
            FirmwareStatus::Finished => {
                body.firmware_updating_mut().set(false).expect("bool accepts both values");
                update_node_info(port, ctx);
            }
        }
    }
}

/// `amdaemon::server::UsbIOController` (`0x14002B0F0` base + `0x14004A3A0`).
pub struct UsbIoController {
    port: Box<dyn UsbIoPort>,
    listener: UsbIoEventListener,
    nodes: [UsbIoNodeController; NODE_CAPACITY],
}

impl fmt::Debug for UsbIoController {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UsbIoController")
            .field("listener", &self.listener)
            .field("nodes", &self.nodes)
            .finish_non_exhaustive()
    }
}

impl UsbIoController {
    /// Constructs the controller with its four node controllers and the embedded firmware listener. Like the
    /// original, nothing is published until the first detect-node callback.
    pub fn new(port: Box<dyn UsbIoPort>) -> Self {
        Self { port, listener: UsbIoEventListener::default(), nodes: std::array::from_fn(UsbIoNodeController::new) }
    }
}

/// Codes of the configured unique functions selected by `flagged`, in config order, packed into the nine
/// published slots (the rest is zero). The original appends without a bound (the config validator limits
/// the list to nine functions); extra functions are dropped here instead of overflowing the array.
fn function_codes(functions: &[UsbIoUniqueFunction], flagged: fn(&UsbIoUniqueFunction) -> bool) -> [u8; UNIQUE_SLOTS] {
    let mut codes = [0; UNIQUE_SLOTS];
    for (slot, function) in codes.iter_mut().zip(functions.iter().filter(|function| flagged(function))) {
        *slot = function.code;
    }
    codes
}

impl Controller for UsbIoController {
    fn id(&self) -> ControllerId {
        ControllerId::UsbIo
    }

    /// vf3 (`0x14004A670`): OR over **all four** nodes, not only the detected ones, so a node whose index is
    /// at or beyond the current `nodeCount` can stay busy forever.
    fn is_busy(&self, ctx: &Ctx<'_>) -> bool {
        self.nodes.iter().any(|node| node.is_busy(ctx))
    }

    /// vf6 (`0x14004A6C0`): for each detected node, refresh its input and then run `checkResult`.
    fn update_server_data(&mut self, ctx: &mut Ctx<'_>) {
        let detected = ctx.server_io.usb_io().node_count() as usize;
        for node in self.nodes.iter_mut().take(detected) {
            node.update_input(self.port.as_ref(), ctx);
            node.check_result(ctx);
        }
    }

    /// vf7 (`0x14004A720`): accept edge for each detected node.
    fn accept_requests(&mut self, ctx: &mut Ctx<'_>) {
        let detected = ctx.server_io.usb_io().node_count() as usize;
        for node in self.nodes.iter_mut().take(detected) {
            node.accept(self.port.as_mut(), ctx);
        }
    }

    /// Detect-node lambda (`0x14004C610`): bumps the detect counter, then refreshes the node count and the
    /// node info (in that order).
    fn on_usb_io_detect_node(&mut self, ctx: &mut Ctx<'_>) {
        let mut body = ctx.server_io.usb_io_mut();
        let counter = body.detect_counter().wrapping_add(1);
        body.detect_counter_mut().set(counter).expect("u32 accepts every value");
        update_node_count(self.port.as_ref(), ctx);
        update_node_info(self.port.as_ref(), ctx);
    }

    /// Process-config observer (`0x14004B010`): publishes the configured unique-function codes and the PWM
    /// slot count of every configured node.
    fn on_process_config(&mut self, config: &ProcessConfig, ctx: &mut Ctx<'_>) {
        let mut body = ctx.server_io.usb_io_mut();
        let mut nodes = body.nodes_mut();
        for (index, functions) in config.usb_io.nodes.iter().enumerate().take(NODE_CAPACITY) {
            let mut node = nodes.get_mut(index).expect("a node index is below the node capacity");
            let input_codes = function_codes(&functions.uniques, |function| function.input);
            node.unique_input_mut().commands_mut().set(&input_codes).expect("the codes are exactly the field");
            let patch = UsbIoOutputInfoPatch {
                pwm_slot_count: Some(if functions.pwm_enabled { functions.pwm_slots } else { 0 }),
                unique_commands: Some(&function_codes(&functions.uniques, |function| function.output)),
                ..UsbIoOutputInfoPatch::default()
            };
            node.output_info_mut().copy_from(&patch).expect("scalar fields accept every value");
        }
    }

    /// `AppConfig` observer (`0x14004C5F0`): installs the embedded `UsbIOEventListener` as libam's
    /// firmware-update listener (`AppConfig+0x1E08`).
    fn on_app_config(&mut self, config: &mut AppConfig, _ctx: &mut Ctx<'_>) {
        config.usb_io.firmware_listener = true;
    }

    /// Delivers the firmware-update listener calls libam made during its pump
    /// ([`UsbIoEventListener`] vf1/vf2/vf3).
    fn on_libam_events(&mut self, ctx: &mut Ctx<'_>) {
        for event in self.port.take_firmware_events() {
            match event {
                FirmwareEvent::Progress { percent, .. } => self.listener.on_progress(percent, ctx),
                FirmwareEvent::NodeStatus { status, .. } => self.listener.on_node_status(status),
                FirmwareEvent::UpdateState { status } => {
                    self.listener.on_update_state(status, self.port.as_ref(), ctx);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use widestring::U16CStr;

    use crate::schema::jvs::SwitchUnitRef;
    use crate::schema::text::fit_wide_str;
    use crate::schema::usb_io::{
        ClientOutputInfo, ClientOutputInfoMut, ClientUsbIoRequestPatch, GeneralOutputRequestPatch, OUTPUT_INFO_SIZE,
        PwmOutputRequestPatch, ServerUsbIoNodeRef, UniqueOutputRequestPatch,
    };
    use libam::result::{AmError, AmResult};
    use libam::usbio::{
        UNIQUE_DATA_LEN, UNIQUE_INPUT_DATA_LEN, UniqueDescriptor, UsbIoNodeFunctions, UsbIoProcessConfig,
    };

    use super::*;
    use crate::controllers::controller::ControllerSlot;
    use crate::controllers::controller::testing::Harness;
    use libam::jvs::AnalogChannel;

    /// Everything one fake node can report; `None` = libam answers `am::Result` code 1 for that getter.
    #[derive(Clone, Copy, Default)]
    struct FakeNode {
        auto_output_zero: Option<bool>,
        info: Option<UsbIoNodeInfo>,
        switch: Option<SwitchState<2>>,
        analog: Option<AnalogState<8>>,
        rotary: Option<RotaryState<4>>,
        descriptors: Option<UniqueDescriptors>,
        data: Option<[u8; UNIQUE_INPUT_DATA_LEN]>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Send {
        General { node: usize, bits: u64, size: u64 },
        Pwm { node: usize, slot: usize, duty: u8 },
        Unique { node: usize, code: u8, data: Vec<u8> },
    }

    #[derive(Default)]
    struct FakeState {
        handle_missing: bool,
        node_count: u64,
        nodes: [FakeNode; 4],
        sends: Vec<Send>,
        /// Sends matching this predicate are refused with code 6.
        refuse: Option<fn(&Send) -> bool>,
        events: Vec<FirmwareEvent>,
    }

    struct FakeUsbIo(Rc<RefCell<FakeState>>);

    impl FakeUsbIo {
        fn node<T>(&self, node: usize, pick: impl FnOnce(&FakeNode) -> Option<T>) -> AmResult<T> {
            let state = self.0.borrow();
            state.nodes.get(node).and_then(pick).ok_or(AmError::code(AmError::INVALID_ARGUMENT))
        }

        fn send(&mut self, send: Send) -> AmResult<()> {
            let mut state = self.0.borrow_mut();
            let refused = state.refuse.is_some_and(|refuse| refuse(&send));
            state.sends.push(send);
            if refused { Err(AmError::code(AmError::FAILED)) } else { Ok(()) }
        }
    }

    impl UsbIoPort for FakeUsbIo {
        fn handle(&self) -> AmResult<()> {
            if self.0.borrow().handle_missing { Err(AmError::code(AmError::NOT_INITIALIZED)) } else { Ok(()) }
        }

        fn node_count(&self) -> u64 {
            self.0.borrow().node_count
        }

        fn node_auto_output_zero(&self, node: usize) -> AmResult<bool> {
            self.node(node, |n| n.auto_output_zero)
        }

        fn node_info(&self, node: usize) -> Option<UsbIoNodeInfo> {
            self.0.borrow().nodes.get(node).and_then(|n| n.info)
        }

        fn node_switch(&self, node: usize) -> AmResult<SwitchState<2>> {
            self.node(node, |n| n.switch)
        }

        fn node_analog(&self, node: usize) -> AmResult<AnalogState<8>> {
            self.node(node, |n| n.analog)
        }

        fn node_rotary(&self, node: usize) -> AmResult<RotaryState<4>> {
            self.node(node, |n| n.rotary)
        }

        fn node_unique_descriptors(&self, node: usize) -> AmResult<UniqueDescriptors> {
            self.node(node, |n| n.descriptors)
        }

        fn node_unique_data(&self, node: usize) -> AmResult<[u8; UNIQUE_INPUT_DATA_LEN]> {
            self.node(node, |n| n.data)
        }

        fn send_general_output(&mut self, node: usize, bits: u64, size: u64) -> AmResult<()> {
            self.send(Send::General { node, bits, size })
        }

        fn send_pwm_output(&mut self, node: usize, slot: usize, duty: u8) -> AmResult<()> {
            self.send(Send::Pwm { node, slot, duty })
        }

        fn send_unique_command(&mut self, node: usize, command: &UniqueCommand<'_>) -> AmResult<()> {
            self.send(Send::Unique { node, code: command.code, data: command.data.to_vec() })
        }

        fn take_firmware_events(&mut self) -> Vec<FirmwareEvent> {
            std::mem::take(&mut self.0.borrow_mut().events)
        }
    }

    fn fake() -> (Box<dyn UsbIoPort>, Rc<RefCell<FakeState>>) {
        let state = Rc::new(RefCell::new(FakeState::default()));
        (Box::new(FakeUsbIo(Rc::clone(&state))), state)
    }

    /// Controller whose libam reports `nodes` ready nodes, already detected.
    fn detected(harness: &mut Harness, nodes: u64) -> (UsbIoController, Rc<RefCell<FakeState>>) {
        let (port, state) = fake();
        {
            let mut state = state.borrow_mut();
            state.node_count = nodes;
            state.nodes = [FakeNode { auto_output_zero: Some(false), ..FakeNode::default() }; 4];
        }
        let mut controller = UsbIoController::new(port);
        controller.on_usb_io_detect_node(&mut harness.ctx());
        (controller, state)
    }

    fn utf16<const N: usize>(text: &str) -> [u16; N] {
        let mut units = [0; N];
        for (unit, encoded) in units.iter_mut().zip(text.encode_utf16()) {
            *unit = encoded;
        }
        units
    }

    fn info(name: &str, code: &str, revision: u8) -> UsbIoNodeInfo {
        UsbIoNodeInfo { name: utf16(name), code: utf16(code), revision }
    }

    /// The payload union of a request with the 8-byte alignment it has inside `ClientData`.
    #[repr(align(8))]
    struct Payload([u8; OUTPUT_INFO_SIZE]);

    /// The game submits `task` on `node`; `fill` writes the `Output` payload through its schema.
    fn submit(
        harness: &mut Harness,
        node: usize,
        generation: u32,
        task: u32,
        fill: impl FnOnce(&mut ClientOutputInfoMut<'_>),
    ) {
        let mut payload = Payload([0; OUTPUT_INFO_SIZE]);
        fill(&mut ClientOutputInfo::access_mut(&mut payload.0).expect("an all-zero payload is valid"));
        let patch = ClientUsbIoRequestPatch { generation: Some(generation), task: Some(task), info: Some(&payload.0) };
        let mut block = harness.client.usb_io_mut();
        let mut nodes = block.nodes_mut();
        nodes.get_mut(node).expect("a node slot").copy_from(&patch).expect("plain fields");
    }

    /// Payload filler: the general output gate and bits.
    fn general(info: &mut ClientOutputInfoMut<'_>, requested: bool, bits: u64) {
        let patch = GeneralOutputRequestPatch { requested: Some(requested), bits: Some(bits) };
        info.general_mut().copy_from(&patch).expect("plain fields");
    }

    /// Payload filler: the PWM gate and duties.
    fn pwm(info: &mut ClientOutputInfoMut<'_>, requested: bool, duties: [u8; 8]) {
        let patch = PwmOutputRequestPatch { requested: Some(requested), duties: Some(&duties) };
        info.pwm_mut().copy_from(&patch).expect("plain fields");
    }

    /// Payload filler: unique command slot `slot`.
    fn unique(info: &mut ClientOutputInfoMut<'_>, slot: usize, requested: bool, data: [u8; UNIQUE_DATA_LEN]) {
        let patch = UniqueOutputRequestPatch { requested: Some(requested), data: Some(&data) };
        let mut uniques = info.uniques_mut();
        uniques.get_mut(slot).expect("a unique slot").copy_from(&patch).expect("plain fields");
    }

    /// The published node `index`.
    fn node(harness: &Harness, index: usize) -> ServerUsbIoNodeRef<'_> {
        harness.server_io.usb_io().nodes().get(index).expect("a node slot")
    }

    /// The published response `(generation, succeeded)` of node `index`.
    fn response(harness: &Harness, index: usize) -> (u32, bool) {
        let response = node(harness, index).response();
        (response.generation(), response.succeeded())
    }

    /// `(on, countToOn, countToOff)`.
    fn unit_state(unit: SwitchUnitRef<'_>) -> (bool, u8, u8) {
        (unit.on(), unit.count_to_on(), unit.count_to_off())
    }

    /// A published string as text.
    fn text(string: &U16CStr) -> String {
        String::from_utf16_lossy(string.as_slice())
    }

    /// Switch state of switch `bit` of `player` on node `index`.
    fn player_switch(harness: &Harness, index: usize, player: usize, bit: usize) -> (bool, u8, u8) {
        let switches = node(harness, index).switch_input();
        let row = switches.players().get(player).expect("a player row");
        unit_state(row.units().get(bit).expect("a player switch"))
    }

    /// Seeds `index` with a name, a revision and an `autoOutputZero` the way an earlier detect left them.
    fn seed_node(harness: &mut Harness, index: usize, name: &str, revision: u8, auto_output_zero: bool) {
        let mut body = harness.server_io.usb_io_mut();
        let mut nodes = body.nodes_mut();
        let mut node = nodes.get_mut(index).expect("a node slot");
        let mut field = node.name_mut();
        let fitted = fit_wide_str(name, field.capacity());
        field.set(&fitted).expect("fitted to the field");
        node.revision_mut().set(revision).expect("u8 accepts every value");
        node.output_info_mut().auto_output_zero_mut().set(auto_output_zero).expect("bool accepts both values");
    }

    fn config_with(nodes: Vec<UsbIoNodeFunctions>) -> ProcessConfig {
        ProcessConfig { usb_io: UsbIoProcessConfig { nodes }, ..ProcessConfig::default() }
    }

    fn function(code: u8, input: bool, output: bool) -> UsbIoUniqueFunction {
        UsbIoUniqueFunction { code, input, output }
    }

    #[test]
    fn detect_clamps_to_four_nodes() {
        let mut harness = Harness::running();
        let (_controller, _state) = detected(&mut harness, 9);
        assert_eq!(harness.server_io.usb_io().node_count(), 4);
    }

    #[test]
    fn detect_without_libam_handle_keeps_the_previous_count() {
        let mut harness = Harness::running();
        let (mut controller, state) = detected(&mut harness, 2);
        state.borrow_mut().handle_missing = true;
        controller.on_usb_io_detect_node(&mut harness.ctx());

        let body = harness.server_io.usb_io();
        assert_eq!((body.node_count(), body.detect_counter()), (2, 2));
    }

    #[test]
    fn detect_publishes_truncated_name_code_and_revision() {
        let mut harness = Harness::running();
        let (port, state) = fake();
        {
            let mut state = state.borrow_mut();
            state.node_count = 2;
            state.nodes[0].auto_output_zero = Some(true);
            state.nodes[0].info = Some(UsbIoNodeInfo { name: utf16("ABCDEFGHI"), code: utf16("12345"), revision: 7 });
            state.nodes[1].auto_output_zero = Some(false);
            state.nodes[1].info = Some(info("IO4", "6789", 2));
        }
        UsbIoController::new(port).on_usb_io_detect_node(&mut harness.ctx());

        let first = node(&harness, 0);
        assert_eq!(text(first.name()), "ABCDEFGH", "name keeps eight characters");
        assert_eq!(text(first.code()), "12345");
        assert_eq!((first.revision(), first.output_info().auto_output_zero()), (7, true));
        let second = node(&harness, 1);
        assert_eq!(text(second.name()), "IO4");
        assert_eq!(text(second.code()), "6789");
        assert_eq!((second.revision(), second.output_info().auto_output_zero()), (2, false));
    }

    #[test]
    fn missing_node_info_publishes_empty_strings_and_clears_auto_output_zero() {
        let mut harness = Harness::running();
        let (port, state) = fake();
        {
            let mut state = state.borrow_mut();
            state.node_count = 1;
            state.nodes[0] = FakeNode { auto_output_zero: None, info: None, ..FakeNode::default() };
        }
        seed_node(&mut harness, 0, "old", 9, true);
        UsbIoController::new(port).on_usb_io_detect_node(&mut harness.ctx());

        let published = node(&harness, 0);
        assert_eq!(
            (text(published.name()), published.revision(), published.output_info().auto_output_zero()),
            (String::new(), 0, false)
        );
    }

    #[test]
    fn update_polls_only_detected_nodes_and_each_getter_fails_independently() {
        let mut harness = Harness::running();
        let (mut controller, state) = detected(&mut harness, 2);
        {
            let mut state = state.borrow_mut();
            let switch = SwitchState::<2> { system: 0b11, ..SwitchState::default() };
            let mut rotary = RotaryState::<4> { count: 1, ..RotaryState::default() };
            rotary.values[0] = 5;
            for node in &mut state.nodes {
                node.switch = Some(switch);
                node.rotary = Some(rotary);
                node.analog = None;
                node.data = Some([0xAA; UNIQUE_INPUT_DATA_LEN]);
            }
        }
        controller.update_server_data(&mut harness.ctx());

        let polled = node(&harness, 1);
        let switches = polled.switch_input();
        assert_eq!((switches.test().on(), switches.tilt().on()), (true, true));
        assert_eq!(polled.rotary_input().values().get(0), Some(5));
        assert_eq!(polled.analog_input().channel_count(), 0, "failing getter skips only its own region");
        assert_eq!(polled.unique_input().data(), &[0xAA; UNIQUE_INPUT_DATA_LEN]);
        assert!(!node(&harness, 2).switch_input().test().on(), "node 2 is beyond nodeCount");
    }

    #[test]
    fn switch_publishes_test_tilt_and_only_two_players() {
        let mut harness = Harness::running();
        let (mut controller, state) = detected(&mut harness, 1);
        let mut switch = SwitchState::<2> { system: 0b10, player_count: 5, player_masks: [1 << 63, 0b100] };
        state.borrow_mut().nodes[0].switch = Some(switch);
        controller.update_server_data(&mut harness.ctx());

        let published = node(&harness, 0).switch_input();
        assert_eq!((published.test().on(), published.tilt().on()), (true, false), "test = bit 1, tilt = bit 0");
        assert_eq!(published.player_count(), 5, "player count is published unclamped");
        assert!(player_switch(&harness, 0, 0, 63).0);
        assert!(player_switch(&harness, 0, 1, 2).0);

        switch.system = 0b01;
        state.borrow_mut().nodes[0].switch = Some(switch);
        controller.update_server_data(&mut harness.ctx());
        let published = node(&harness, 0).switch_input();
        assert_eq!(unit_state(published.test()), (false, 1, 1));
        assert_eq!(unit_state(published.tilt()), (true, 1, 0));
        assert_eq!(player_switch(&harness, 0, 0, 63).1, 1, "unchanged level counts no edge");
    }

    #[test]
    fn analog_keeps_eight_channels_and_the_last_channels_valid_bits() {
        let mut harness = Harness::running();
        let (mut controller, state) = detected(&mut harness, 1);
        let mut analog = AnalogState::<8> { count: 12, ..AnalogState::default() };
        for (k, channel) in analog.channels.iter_mut().enumerate() {
            *channel = AnalogChannel { value: 10 * k as u16, valid_bits: 0x2_0000 + k as u64 };
        }
        state.borrow_mut().nodes[0].analog = Some(analog);
        controller.update_server_data(&mut harness.ctx());

        let published = node(&harness, 0).analog_input().copy_into();
        assert_eq!(published.channel_count, 12);
        assert_eq!(published.values, [0, 10, 20, 30, 40, 50, 60, 70]);
        assert_eq!(published.valid_bit_count, 7);
    }

    #[test]
    fn unique_descriptors_publish_the_count_and_up_to_two_decoded_entries() {
        let mut harness = Harness::running();
        let (mut controller, state) = detected(&mut harness, 1);
        let descriptors = UniqueDescriptors {
            count: 7,
            entries: [UniqueDescriptor { flags: 0b01, code: 0x31 }, UniqueDescriptor { flags: 0b10, code: 0x32 }],
        };
        state.borrow_mut().nodes[0].descriptors = Some(descriptors);
        controller.update_server_data(&mut harness.ctx());

        let unique = node(&harness, 0).unique_input();
        assert_eq!(unique.reported_count(), 7);
        let first = unique.descriptors().get(0).expect("a descriptor slot");
        assert_eq!((first.code(), first.flag0(), first.flag1()), (0x31, true, false));
        let second = unique.descriptors().get(1).expect("a descriptor slot");
        assert_eq!((second.code(), second.flag0(), second.flag1()), (0x32, false, true));
    }

    #[test]
    fn output_request_sends_general_then_all_pwm_slots_then_requested_uniques_and_acknowledges() {
        let mut harness = Harness::running();
        let (controller, state) = detected(&mut harness, 2);
        let mut slot = ControllerSlot::new(Box::new(controller));
        let config = config_with(vec![
            UsbIoNodeFunctions::default(),
            UsbIoNodeFunctions {
                pwm_enabled: true,
                pwm_slots: 8,
                uniques: vec![function(0x11, false, true), function(0x22, true, false), function(0x33, false, true)],
            },
        ]);
        slot.controller_mut().on_process_config(&config, &mut harness.ctx());
        slot.pre_execute(&mut harness.ctx());

        submit(&mut harness, 1, 6, 0, |info| {
            general(info, true, 0x0102_0304_0506_0708);
            pwm(info, true, [10, 11, 12, 13, 14, 15, 16, 17]);
            unique(info, 0, true, [0xA0; UNIQUE_DATA_LEN]);
            unique(info, 1, true, [0xB1; UNIQUE_DATA_LEN]);
        });

        slot.post_execute(&mut harness.ctx());
        let mut expected = vec![Send::General { node: 1, bits: 0x0102_0304_0506_0708, size: 8 }];
        expected.extend((0..8).map(|i| Send::Pwm { node: 1, slot: i, duty: 10 + i as u8 }));
        // Request slot k uses the k-th configured *output* function code: 0x11 then 0x33.
        expected.push(Send::Unique { node: 1, code: 0x11, data: vec![0xA0; UNIQUE_DATA_LEN] });
        expected.push(Send::Unique { node: 1, code: 0x33, data: vec![0xB1; UNIQUE_DATA_LEN] });
        assert_eq!(state.borrow().sends, expected);
        assert!(slot.controller().is_busy(&harness.ctx()));
        assert_eq!(response(&harness, 1).0, 0, "completes one tick later");

        slot.pre_execute(&mut harness.ctx());
        assert_eq!(response(&harness, 1), (6, true));
        assert!(!slot.controller().is_busy(&harness.ctx()));

        slot.post_execute(&mut harness.ctx());
        assert_eq!(state.borrow().sends.len(), expected.len(), "the same generation is never accepted twice");
    }

    #[test]
    fn requests_send_only_the_groups_they_ask_for() {
        let mut harness = Harness::running();
        let (mut controller, state) = detected(&mut harness, 1);
        submit(&mut harness, 0, 1, 0, |info| {
            pwm(info, true, [0; 8]);
            general(info, false, 99);
            unique(info, 4, false, [1; UNIQUE_DATA_LEN]);
        });
        controller.accept_requests(&mut harness.ctx());

        let sends = state.borrow().sends.clone();
        assert_eq!(sends.len(), 8);
        assert!(sends.iter().all(|s| matches!(s, Send::Pwm { .. })));
    }

    #[test]
    fn refused_send_does_not_stop_later_groups_and_fails_the_request() {
        let mut harness = Harness::running();
        let (controller, state) = detected(&mut harness, 1);
        state.borrow_mut().refuse =
            Some(|send| matches!(send, Send::General { .. }) || matches!(send, Send::Pwm { slot: 3, .. }));
        let mut slot = ControllerSlot::new(Box::new(controller));
        slot.pre_execute(&mut harness.ctx());

        submit(&mut harness, 0, 2, 0, |info| {
            general(info, true, 0);
            pwm(info, true, [0; 8]);
            unique(info, 0, true, [0; UNIQUE_DATA_LEN]);
        });
        slot.post_execute(&mut harness.ctx());

        // General (refused), all eight PWM slots (slot 3 refused) and the unique command were all attempted.
        let sends = state.borrow().sends.clone();
        assert_eq!(sends.len(), 1 + 8 + 1);
        assert!(matches!(sends[0], Send::General { .. }) && matches!(sends[9], Send::Unique { .. }));

        slot.pre_execute(&mut harness.ctx());
        assert_eq!(response(&harness, 0), (2, false));
    }

    #[test]
    fn unknown_task_never_reaches_libam_and_fails() {
        let mut harness = Harness::running();
        let (controller, state) = detected(&mut harness, 1);
        let mut slot = ControllerSlot::new(Box::new(controller));
        slot.pre_execute(&mut harness.ctx());

        submit(&mut harness, 0, 1, 3, |info| general(info, true, 0));
        slot.post_execute(&mut harness.ctx());
        slot.pre_execute(&mut harness.ctx());

        assert!(state.borrow().sends.is_empty());
        assert_eq!(response(&harness, 0), (1, false));
    }

    #[test]
    fn requests_for_undetected_nodes_are_never_accepted() {
        let mut harness = Harness::running();
        let (mut controller, state) = detected(&mut harness, 1);
        submit(&mut harness, 3, 1, 0, |info| general(info, true, 0));
        controller.accept_requests(&mut harness.ctx());
        controller.update_server_data(&mut harness.ctx());

        assert!(state.borrow().sends.is_empty());
        assert_eq!(response(&harness, 3).0, 0);
        assert!(!controller.is_busy(&harness.ctx()));
    }

    #[test]
    fn busy_scan_covers_nodes_past_a_shrunken_node_count() {
        let mut harness = Harness::running();
        let (mut controller, state) = detected(&mut harness, 4);
        submit(&mut harness, 3, 1, 0, |info| general(info, true, 0));
        controller.accept_requests(&mut harness.ctx());
        assert!(controller.is_busy(&harness.ctx()));

        // A re-detect shrinks nodeCount: node 3 is no longer completed but still counts as busy.
        state.borrow_mut().node_count = 2;
        controller.on_usb_io_detect_node(&mut harness.ctx());
        controller.update_server_data(&mut harness.ctx());
        assert!(controller.is_busy(&harness.ctx()));
        assert_eq!(response(&harness, 3).0, 0);

        state.borrow_mut().node_count = 4;
        controller.on_usb_io_detect_node(&mut harness.ctx());
        controller.update_server_data(&mut harness.ctx());
        assert!(!controller.is_busy(&harness.ctx()));
        assert_eq!(response(&harness, 3).0, 1);
    }

    #[test]
    fn process_config_publishes_unique_codes_and_pwm_slot_counts() {
        let mut harness = Harness::running();
        let (mut controller, _state) = detected(&mut harness, 2);
        {
            let mut body = harness.server_io.usb_io_mut();
            let mut nodes = body.nodes_mut();
            let mut seeded = nodes.get_mut(0).expect("a node slot");
            seeded.unique_input_mut().commands_mut().set(&[9; UNIQUE_SLOTS]).expect("the codes are exactly the field");
            seeded
                .output_info_mut()
                .unique_commands_mut()
                .set(&[9; UNIQUE_SLOTS])
                .expect("the codes are exactly the field");
        }
        let config = config_with(vec![
            UsbIoNodeFunctions {
                pwm_enabled: true,
                pwm_slots: 6,
                uniques: vec![function(1, true, false), function(2, false, true), function(3, true, true)],
            },
            UsbIoNodeFunctions { pwm_enabled: false, pwm_slots: 6, uniques: Vec::new() },
        ]);
        controller.on_process_config(&config, &mut harness.ctx());

        let first = node(&harness, 0);
        assert_eq!(
            first.unique_input().commands(),
            &[1, 3, 0, 0, 0, 0, 0, 0, 0],
            "input codes, previous contents cleared"
        );
        assert_eq!(first.output_info().unique_commands(), &[2, 3, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(first.output_info().pwm_slot_count(), 6);
        assert_eq!(node(&harness, 1).output_info().pwm_slot_count(), 0, "disabled PWM function publishes zero slots");
    }

    fn deliver(
        controller: &mut UsbIoController,
        state: &Rc<RefCell<FakeState>>,
        harness: &mut Harness,
        events: &[FirmwareEvent],
    ) {
        state.borrow_mut().events.extend_from_slice(events);
        controller.on_libam_events(&mut harness.ctx());
    }

    fn progress(percent: u32) -> FirmwareEvent {
        FirmwareEvent::Progress { node: 0, percent }
    }

    fn node_status(raw: u32) -> FirmwareEvent {
        FirmwareEvent::NodeStatus { node: 0, status: FirmwareStatus::from_raw(raw) }
    }

    fn update_state(raw: u32) -> FirmwareEvent {
        FirmwareEvent::UpdateState { status: FirmwareStatus::from_raw(raw) }
    }

    #[test]
    fn firmware_progress_averages_over_nodes_and_accumulates_finished_nodes() {
        let mut harness = Harness::running();
        let (mut controller, state) = detected(&mut harness, 2);
        deliver(&mut controller, &state, &mut harness, &[update_state(1)]);
        assert_eq!(
            (harness.server_io.usb_io().firmware_updating(), harness.server_io.usb_io().firmware_progress()),
            (true, 0)
        );

        deliver(&mut controller, &state, &mut harness, &[node_status(1), progress(50)]);
        assert_eq!(harness.server_io.usb_io().firmware_progress(), 25, "(50 + 0) / 2");

        deliver(&mut controller, &state, &mut harness, &[progress(100), node_status(2)]);
        assert_eq!(harness.server_io.usb_io().firmware_progress(), 50, "(100 + 0) / 2");

        // Node 1 starts: its own progress restarts while node 0's 100 % stays accumulated.
        deliver(&mut controller, &state, &mut harness, &[node_status(1), progress(40)]);
        assert_eq!(harness.server_io.usb_io().firmware_progress(), 70, "(40 + 100) / 2");

        deliver(&mut controller, &state, &mut harness, &[progress(100), node_status(3), update_state(2)]);
        assert_eq!(harness.server_io.usb_io().firmware_progress(), 100);
        assert!(!harness.server_io.usb_io().firmware_updating());
    }

    #[test]
    fn starting_a_new_update_session_clears_progress_and_accumulated_nodes() {
        let mut harness = Harness::running();
        let (mut controller, state) = detected(&mut harness, 1);
        deliver(
            &mut controller,
            &state,
            &mut harness,
            &[update_state(1), node_status(1), progress(100), node_status(2)],
        );
        deliver(&mut controller, &state, &mut harness, &[update_state(1), node_status(1), progress(10)]);
        assert_eq!(harness.server_io.usb_io().firmware_progress(), 10, "doneProgress was reset by the new session");
    }

    #[test]
    fn firmware_progress_without_detected_nodes_keeps_the_previous_value() {
        let mut harness = Harness::running();
        let (port, state) = fake();
        let mut controller = UsbIoController::new(port);
        harness.server_io.usb_io_mut().firmware_progress_mut().set(33).expect("u32 accepts every value");
        deliver(&mut controller, &state, &mut harness, &[progress(80)]);
        assert_eq!(harness.server_io.usb_io().firmware_progress(), 33, "no division by a zero node count");

        // The accumulator side effect of the original still happens before the division.
        state.borrow_mut().node_count = 1;
        controller.on_usb_io_detect_node(&mut harness.ctx());
        deliver(&mut controller, &state, &mut harness, &[node_status(2), progress(0)]);
        assert_eq!(harness.server_io.usb_io().firmware_progress(), 80, "curProgress=80 was folded into doneProgress");
    }

    #[test]
    fn finished_update_state_refreshes_node_info_but_not_counters() {
        let mut harness = Harness::running();
        let (mut controller, state) = detected(&mut harness, 2);
        {
            let mut state = state.borrow_mut();
            state.nodes[0].info = Some(info("NEWFW", "C1", 5));
            state.nodes[0].auto_output_zero = Some(true);
            // libam now reports more nodes, but a firmware refresh must not re-read the count.
            state.node_count = 4;
        }
        let (count, counter) = (harness.server_io.usb_io().node_count(), harness.server_io.usb_io().detect_counter());
        deliver(&mut controller, &state, &mut harness, &[update_state(4)]);

        let body = harness.server_io.usb_io();
        assert_eq!((body.node_count(), body.detect_counter()), (count, counter));
        let refreshed = node(&harness, 0);
        assert_eq!(text(refreshed.name()), "NEWFW");
        assert_eq!((refreshed.revision(), refreshed.output_info().auto_output_zero()), (5, true));
    }
}
