//! `amdaemon::server::CanController` (vtable `0x1405C95B8`, ctor `0x140026070`, registry index 6).
//!
//! Two CAN ports, each with a `CanPortRequestExecutor` (client `Send` requests → libam) and a
//! `CanPortAckPacketPusher` (frames received by libam → client, with ACK back-pressure). On the ready edge it
//! publishes libam's port and target tables.
//!
//! Evidence: `spec/controllers/can_boardio_usbdevice.md` §2 (claims `can_boardio_usbdevice.can.*`).
//!
//! The statically linked libam of this build contains only the *disabled* CAN manager: its port count is 0,
//! every `send` fails and no listener callback is ever invoked. [`libam_api::can::DisabledCanPort`] reproduces
//! that backend through [`libam_api::can::CanPort`]; a real backend only has to implement the trait.

use std::collections::VecDeque;
use std::fmt::Write as _;

use crate::executor::{RequestExecutor, RequestState};
use crate::manager::Manager;
use crate::schema::can::{
    CanTargetType, CanTask, ClientRequestRef, ClientSendInfoRef, MAX_ACK_PACKETS, MAX_FRAME_DATA, MAX_SEND_COMMANDS,
    MAX_TARGETS, PORT_COUNT, ServerAckDataMut, ServerAckDataPatch, ServerAckPacketPatch, ServerTargetMut,
    ServerTargetPatch,
};
use crate::schema::text::fit_wide_str;
use libam_api::can::{CanEvent, CanPacket, CanPort, CanTargetInfo};
use libam_api::result::{AmError, AmResult};

use crate::controllers::controller::{Controller, ControllerId, Ctx, commit_check};
use libam_api::config::AppConfig;

/// Why indexing the two per-port records of the blocks cannot fail.
const PORT_RECORD: &str = "both blocks hold one record per CAN port";

/// `CanPortRequestExecutor::sendState` (`+0x80`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum SendState {
    /// `0`: no send started yet.
    #[default]
    Idle,
    /// `3`: set at the start of `accept`, waiting for a successful send-complete callback.
    Awaiting,
    /// `2`: a send-complete callback with `ok` arrived.
    Completed,
}

/// One received frame in the queue of a [`AckPacketPusher`]; `ServerAckPacket` is its published form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReceivedFrame {
    target_index: u16,
    command: u8,
    data_size: u8,
    data: [u8; MAX_FRAME_DATA],
}

impl ReceivedFrame {
    fn patch(&self) -> ServerAckPacketPatch<'_> {
        ServerAckPacketPatch {
            target_index: Some(self.target_index),
            command: Some(self.command),
            data_size: Some(self.data_size),
            data: Some(&self.data),
        }
    }
}

/// `CanPortAckPacketPusher` (`0x28` bytes, ctor `0x140025B30`): the queue of received frames of one port.
#[derive(Debug, Default)]
struct AckPacketPusher {
    queue: VecDeque<ReceivedFrame>,
}

impl AckPacketPusher {
    /// `push` (`0x140025CB0`): oversize frames are dropped; an older frame with the same
    /// `(targetIndex, command)` is replaced, so the newest data wins and moves to the tail.
    fn push(&mut self, target_index: u16, packet: &CanPacket) {
        let Some(size) = u8::try_from(packet.size).ok().filter(|&size| usize::from(size) <= MAX_FRAME_DATA) else {
            return;
        };
        let len = usize::from(size);
        let mut frame =
            ReceivedFrame { target_index, command: packet.command, data_size: size, data: [0; MAX_FRAME_DATA] };
        frame.data[..len].copy_from_slice(&packet.data[..len]);
        self.queue.retain(|queued| !(queued.target_index == target_index && queued.command == packet.command));
        self.queue.push_back(frame);
    }

    /// Flush inline in vf6 (`0x140025BE0`): publishes at most 16 queued frames, and only when the client
    /// has echoed the previous batch number. Slots beyond the new `packet_count` keep stale data.
    fn flush(&mut self, ack: &mut ServerAckDataMut<'_>, client_ack: u32) {
        let task_no = ack.task_no();
        if task_no != client_ack || self.queue.is_empty() {
            return;
        }
        let batch = self.queue.len().min(MAX_ACK_PACKETS);
        let mut packets = ack.packets_mut();
        for (index, frame) in self.queue.drain(..batch).enumerate() {
            let mut slot = packets.get_mut(index).expect("a batch holds at most MAX_ACK_PACKETS frames");
            slot.copy_from(&frame.patch()).expect("scalar and byte fields accept every value");
        }
        let patch = ServerAckDataPatch {
            task_no: Some(task_no.wrapping_add(1)),
            packet_count: Some(batch as u32),
            ..ServerAckDataPatch::default()
        };
        ack.copy_from(&patch).expect("scalar fields accept every value");
    }
}

/// `CanEventListener` (`controller+0x28`, `0x50` bytes): routes libam callbacks to the per-port objects.
#[derive(Debug, Default)]
struct CanEventListener {
    send_states: [SendState; PORT_COUNT],
    pushers: [AckPacketPusher; PORT_COUNT],
}

impl CanEventListener {
    /// Dispatches one callback; ports ≥ 2 resolve to no object (`0x140028FE0`, `0x140028FC0`).
    fn dispatch(&mut self, event: CanEvent) {
        match event {
            CanEvent::SendComplete { port, ok } => {
                if ok && let Some(state) = self.send_states.get_mut(port) {
                    *state = SendState::Completed;
                }
            }
            CanEvent::Received { port, target_index, packet } => {
                if let Some(pusher) = self.pushers.get_mut(port) {
                    pusher.push(target_index, &packet);
                }
            }
        }
    }
}

/// `amdaemon::server::CanController` (object size `0x98`).
pub struct CanController {
    port: Box<dyn CanPort>,
    /// `+0x78` `CanPortRequestExecutor::executor` of ports 0 and 1.
    executors: [RequestExecutor; PORT_COUNT],
    /// `+0x28` listener plus the objects it addresses (`+0x78`/`+0x88` sendState and pushers).
    listener: CanEventListener,
}

impl std::fmt::Debug for CanController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CanController")
            .field("executors", &self.executors)
            .field("listener", &self.listener)
            .finish_non_exhaustive()
    }
}

impl CanController {
    /// Constructs the controller (`0x140026070`); both ports exist unconditionally.
    pub fn new(port: Box<dyn CanPort>) -> Self {
        Self { port, executors: Default::default(), listener: CanEventListener::default() }
    }
}

impl Controller for CanController {
    fn id(&self) -> ControllerId {
        ControllerId::Can
    }

    // vf3 `isBusy` is not overridden: a CAN request never keeps the App alive.

    /// vf5 `0x1400263D0`: publishes libam's port and target tables. Targets are never refreshed later.
    fn clear_server_data(&mut self, ctx: &mut Ctx<'_>) {
        let mut can = ctx.server.can_mut();
        if self.port.can_facade().is_err() {
            tracing::warn!("libam is not ready");
            can.port_count_mut().set(0).expect("a scalar accepts every value");
            return;
        }
        let mut ports = self.port.port_count();
        if ports > PORT_COUNT {
            tracing::error!("too many units");
            ports = PORT_COUNT;
        }
        can.port_count_mut().set(ports as u32).expect("a scalar accepts every value");
        let mut records = can.ports_mut();
        for port in 0..ports {
            let mut data = records.get_mut(port).expect("the port count is clamped to PORT_COUNT");
            let mut targets = self.port.target_count(port);
            if targets > MAX_TARGETS {
                tracing::error!("too many units");
                targets = MAX_TARGETS;
            }
            data.target_count_mut().set(targets as u16).expect("a scalar accepts every value");
            let mut slots = data.targets_mut();
            for index in 0..targets {
                let mut slot = slots.get_mut(index).expect("the target count is clamped to MAX_TARGETS");
                // The original replaces the whole 56-byte record: reset it, then fill in what libam knows.
                slot.clear();
                match self.port.target(port, index) {
                    Ok(info) => publish_target(&mut slot, &info),
                    Err(_) => {
                        tracing::warn!("libam version does not match");
                    }
                }
            }
        }
    }

    /// vf6 `0x140026810`: `checkResult` of both ports, then the ACK publication of both ports.
    fn update_server_data(&mut self, ctx: &mut Ctx<'_>) {
        let client = ctx.client.can();
        let client_ports = client.ports();
        let mut server = ctx.server.can_mut();
        let mut server_ports = server.ports_mut();
        for (index, executor) in self.executors.iter_mut().enumerate() {
            let request_generation = client_ports.get(index).expect(PORT_RECORD).request().generation();
            let mut port = server_ports.get_mut(index).expect(PORT_RECORD);
            let response_generation = port.response().generation();
            // The checker is the constant lambda `0x140028FB0` returning `Succeeded`.
            let outcome = executor.check_result(request_generation, response_generation, || RequestState::Succeeded);
            commit_check(outcome, &mut port.response_mut());
        }
        for (index, pusher) in self.listener.pushers.iter_mut().enumerate() {
            let client_ack = client_ports.get(index).expect(PORT_RECORD).ack().task_no();
            let mut port = server_ports.get_mut(index).expect(PORT_RECORD);
            pusher.flush(&mut port.ack_mut(), client_ack);
        }
    }

    /// vf7 `0x1400269B0`: accept edge of port 0, then port 1.
    fn accept_requests(&mut self, ctx: &mut Ctx<'_>) {
        let Self { port, executors, listener } = self;
        let client = ctx.client.can();
        let client_ports = client.ports();
        let server = ctx.server.can();
        let server_ports = server.ports();
        for (index, executor) in executors.iter_mut().enumerate() {
            let request = client_ports.get(index).expect(PORT_RECORD).request();
            let response_generation = server_ports.get(index).expect(PORT_RECORD).response().generation();
            executor.accept(request.generation(), response_generation, || {
                accept_send(index, &mut **port, listener, &mut *ctx.manager, &request)
            });
        }
    }

    /// `SendPacketEventListener` / `RecvPacketEventListener` callbacks raised by libam's update pump.
    fn on_libam_events(&mut self, _ctx: &mut Ctx<'_>) {
        for event in self.port.take_events() {
            self.listener.dispatch(event);
        }
    }

    /// AppConfig lambda `0x140029000`: hands both listener faces to libam (`AppConfig+0x1658`/`+0x1660`).
    fn on_app_config(&mut self, config: &mut AppConfig, _ctx: &mut Ctx<'_>) {
        config.can.send_listener_installed = true;
        config.can.recv_listener_installed = true;
    }
}

/// `CanPortRequestExecutor::accept` (`0x140025910`).
fn accept_send(
    index: usize,
    port: &mut dyn CanPort,
    listener: &mut CanEventListener,
    manager: &mut Manager,
    request: &ClientRequestRef<'_>,
) -> RequestState {
    if manager.debug_dump() {
        let text = trace_send(index, request);
        tracing::info!("{text}");
    }
    if port.can_facade().is_err() {
        tracing::warn!("libam is not ready");
        return RequestState::Failed;
    }
    let result = match request.task_kind() {
        Some(CanTask::Send) => send_commands(index, port, listener, &request.send_info()),
        None => Err(AmError::code(AmError::UNKNOWN_TASK)),
    };
    if result.is_ok() { RequestState::Pending } else { RequestState::Failed }
}

/// The `Send` task: every command is handed to libam; only the **last** send result (plus the
/// send-complete callback seen during the whole loop) decides the outcome.
fn send_commands(
    index: usize,
    port: &mut dyn CanPort,
    listener: &mut CanEventListener,
    info: &ClientSendInfoRef<'_>,
) -> AmResult<()> {
    // The original does not bound `commandCount` and would read past the 16-entry array into adjacent
    // client data; such a request is rejected as an invalid argument instead.
    let Some(count) = usize::try_from(info.command_count()).ok().filter(|&count| count <= MAX_SEND_COMMANDS) else {
        return Err(AmError::code(AmError::INVALID_ARGUMENT));
    };
    listener.send_states[index] = SendState::Awaiting;
    let mut result = Ok(());
    for command in info.commands().iter().take(count) {
        let packet = CanPacket::new(command.command(), command.data(), command.data_size());
        result = port.send(index, usize::from(command.target_index()), command.priority(), &packet);
        // libam raises the send-complete callback synchronously, from inside `send`.
        for event in port.take_events() {
            listener.dispatch(event);
        }
    }
    if listener.send_states[index] == SendState::Awaiting {
        // No send-complete callback arrived (always the case with zero commands).
        result = Err(AmError::code(AmError::FAILED));
    }
    result
}

/// libam node type → [`CanTargetType`] (`1..=5 → 0..=4`, anything else `Reserved`).
fn libam_target_type(raw: u32) -> CanTargetType {
    match raw {
        1 => CanTargetType::Host,
        2 => CanTargetType::Motor,
        3 => CanTargetType::Sensor,
        4 => CanTargetType::Io,
        5 => CanTargetType::Led,
        _ => CanTargetType::Reserved,
    }
}

/// `ServerTargetData` builder (`0x140026A70`) over a record reset to zero: text fields are truncated like
/// `wcsncpy_s`.
fn publish_target(slot: &mut ServerTargetMut<'_>, info: &CanTargetInfo) {
    let product = fit_wide_str(&info.product, slot.product_mut().capacity());
    let custom_chip = fit_wide_str(&info.custom_chip, slot.custom_chip_mut().capacity());
    let patch = ServerTargetPatch {
        target_type: Some(libam_target_type(info.target_type).into()),
        id: Some(info.id),
        firm_revision: Some(info.firm_revision),
        firm_sum: Some(info.firm_sum),
        product: Some(&product),
        custom_chip: Some(&custom_chip),
    };
    slot.copy_from(&patch).expect("text fitted to the fields");
}

/// Request trace `0x140026B50`.
///
/// `CAN - port[<port>] task: Send` and, for `Send`, one line per command:
/// `\n  command[<i>] - index: <u16>, code: <u8>, size: <u8>, data: 0x<HH> ×8, priority: <u8>`.
/// The stream receives `std::hex` and `std::uppercase` together with the first data byte and never resets
/// them, so every number printed after that point (the priority, and all of the following commands) is
/// uppercase hexadecimal without prefix, while the port and the first command's counters are decimal.
///
/// Deviations: the original passes a null task name for unknown tasks into the stream insertion and
/// faults (here the name is empty), and it walks `commandCount` entries unclamped (here at most 16).
fn trace_send(port: usize, request: &ClientRequestRef<'_>) -> String {
    let task = request.task_kind();
    let mut text = format!("CAN - port[{port}] task: {}", task.map_or("", CanTask::name));
    if task != Some(CanTask::Send) {
        return text;
    }
    let info = request.send_info();
    let count = usize::try_from(info.command_count()).unwrap_or(usize::MAX);
    let mut hex = false;
    for (index, command) in info.commands().iter().enumerate().take(count) {
        text.push_str("\n  command[");
        push_number(&mut text, index as u64, hex);
        text.push_str("] - index: ");
        push_number(&mut text, u64::from(command.target_index()), hex);
        text.push_str(", code: ");
        push_number(&mut text, u64::from(command.command()), hex);
        text.push_str(", size: ");
        push_number(&mut text, u64::from(command.data_size()), hex);
        text.push_str(", data:");
        for &byte in command.data() {
            // Writing to a `String` cannot fail.
            let _ = write!(text, " 0x{byte:02X}");
            hex = true;
        }
        text.push_str(", priority: ");
        push_number(&mut text, u64::from(command.priority()), hex);
    }
    text
}

fn push_number(text: &mut String, value: u64, hex: bool) {
    // Writing to a `String` cannot fail.
    let _ = if hex { write!(text, "{value:X}") } else { write!(text, "{value}") };
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::ops::Range;
    use std::rc::Rc;

    use crate::schema::can::ClientRequestPatch;
    use crate::schema::image::ServerImage;
    use libam_api::can::DisabledCanPort;

    use super::*;
    use crate::controllers::controller::ControllerSlot;
    use crate::controllers::controller::testing::Harness;

    /// libam CAN manager stand-in implementing libam's own argument validation.
    #[derive(Debug, Default)]
    struct Can {
        /// Targets per port; the port count is the length.
        targets: Vec<Vec<CanTargetInfo>>,
        /// Overrides `targetCount(port)` (clamp tests).
        declared: Vec<Option<usize>>,
        /// Callback raised by every accepted send.
        complete_on_send: Option<bool>,
        sent: Vec<(usize, usize, u8, CanPacket)>,
        events: Vec<CanEvent>,
    }

    #[derive(Debug, Clone, Default)]
    struct FakePort(Rc<RefCell<Can>>);

    impl CanPort for FakePort {
        fn can_facade(&self) -> AmResult<()> {
            Ok(())
        }
        fn port_count(&self) -> usize {
            self.0.borrow().targets.len()
        }
        fn target_count(&self, port: usize) -> usize {
            let can = self.0.borrow();
            can.declared.get(port).copied().flatten().unwrap_or_else(|| can.targets.get(port).map_or(0, Vec::len))
        }
        fn target(&self, port: usize, index: usize) -> AmResult<CanTargetInfo> {
            let can = self.0.borrow();
            can.targets
                .get(port)
                .and_then(|targets| targets.get(index))
                .cloned()
                .ok_or(AmError::code(AmError::INVALID_ARGUMENT))
        }
        fn send(&mut self, port: usize, target_index: usize, priority: u8, packet: &CanPacket) -> AmResult<()> {
            let mut can = self.0.borrow_mut();
            let known_target = can.targets.get(port).is_some_and(|targets| target_index < targets.len());
            if !known_target || priority > 15 {
                return Err(AmError::code(AmError::INVALID_ARGUMENT));
            }
            can.sent.push((port, target_index, priority, *packet));
            if let Some(ok) = can.complete_on_send {
                can.events.push(CanEvent::SendComplete { port, ok });
            }
            Ok(())
        }
        fn take_events(&mut self) -> Vec<CanEvent> {
            std::mem::take(&mut self.0.borrow_mut().events)
        }
    }

    fn node(target_type: u32, id: u8) -> CanTargetInfo {
        CanTargetInfo { target_type, id, ..CanTargetInfo::default() }
    }

    /// One frame of a `Send` request as the game lays it out in the request's info bytes.
    #[derive(Debug, Clone, Copy)]
    struct SendCommand {
        target_index: u16,
        command: u8,
        data_size: u8,
        data: [u8; MAX_FRAME_DATA],
        priority: u8,
    }

    fn command(target: u16, code: u8, data: &[u8], priority: u8) -> SendCommand {
        let mut bytes = [0; MAX_FRAME_DATA];
        bytes[..data.len()].copy_from_slice(data);
        SendCommand { target_index: target, command: code, data_size: data.len() as u8, data: bytes, priority }
    }

    /// The `0xE4` info bytes of a `Send` request: `u32` command count, then 16 frames of `0xE` bytes
    /// (`u16` target, `u8` code, `u8` size, `u8[8]` data, `u8` priority, 1 padding byte).
    fn send_info(count: u32, commands: &[SendCommand]) -> [u8; 0xE4] {
        let mut info = [0; 0xE4];
        info[..4].copy_from_slice(&count.to_le_bytes());
        for (bytes, command) in info[4..].chunks_exact_mut(0xE).zip(commands) {
            bytes[..2].copy_from_slice(&command.target_index.to_le_bytes());
            bytes[2] = command.command;
            bytes[3] = command.data_size;
            bytes[4..12].copy_from_slice(&command.data);
            bytes[12] = command.priority;
        }
        info
    }

    /// Bytes `range` (relative to the port record, stride `0x454`) of server port `port`, read behind the schema.
    fn port_bytes(image: &ServerImage, port: usize, range: Range<usize>) -> &[u8] {
        let base = 0x3EE0 + 4 + port * 0x454;
        &image.bytes()[base + range.start..base + range.end]
    }

    /// `ServerAckData` of a port record.
    const ACK_BYTES: Range<usize> = 0x08..0xD0;

    /// `ServerTargetData` `index` of a port record.
    fn target_bytes(index: usize) -> Range<usize> {
        0xD4 + index * 0x38..0xD4 + (index + 1) * 0x38
    }

    fn frame(port: usize, target_index: u16, command: u8, data: &[u8]) -> CanEvent {
        let mut bytes = [0; MAX_FRAME_DATA];
        bytes[..data.len()].copy_from_slice(data);
        CanEvent::Received { port, target_index, packet: CanPacket { command, size: data.len() as u64, data: bytes } }
    }

    struct Rig {
        harness: Harness,
        slot: ControllerSlot,
        can: Rc<RefCell<Can>>,
    }

    impl Rig {
        /// Two ports with two targets each; every accepted send completes successfully.
        fn new() -> Self {
            let port = FakePort::default();
            let can = Rc::clone(&port.0);
            {
                let mut state = can.borrow_mut();
                state.targets = vec![vec![node(1, 1), node(2, 2)], vec![node(3, 1), node(4, 2)]];
                state.complete_on_send = Some(true);
            }
            Self::with_port(Box::new(port), can)
        }

        fn with_port(port: Box<dyn CanPort>, can: Rc<RefCell<Can>>) -> Self {
            Self { harness: Harness::running(), slot: ControllerSlot::new(Box::new(CanController::new(port))), can }
        }

        /// Backend that only exists as the disabled libam CAN manager.
        fn disabled() -> Self {
            Self::with_port(Box::new(DisabledCanPort), Rc::default())
        }

        /// One App frame: libam events, `preExecute`, `postExecute`.
        fn tick(&mut self) {
            let mut ctx = self.harness.ctx();
            self.slot.controller_mut().on_libam_events(&mut ctx);
            self.slot.pre_execute(&mut ctx);
            self.slot.post_execute(&mut ctx);
        }

        fn write_request(&mut self, port: usize, generation: u32, task: u32, info: &[u8; 0xE4]) {
            let patch = ClientRequestPatch { generation: Some(generation), task: Some(task), info: Some(info) };
            let mut can = self.harness.client.can_mut();
            let mut ports = can.ports_mut();
            ports.get_mut(port).expect(PORT_RECORD).request_mut().copy_from(&patch).expect("plain fields");
        }

        fn request(&mut self, port: usize, generation: u32, commands: &[SendCommand]) {
            let info = send_info(commands.len() as u32, commands);
            self.write_request(port, generation, CanTask::Send as u32, &info);
        }

        /// Issues a request and runs the frame that accepts it and the frame that acknowledges it.
        fn exchange(&mut self, port: usize, generation: u32, commands: &[SendCommand]) {
            self.request(port, generation, commands);
            self.tick();
            self.tick();
        }

        /// The client echoes `task_no` as the last consumed batch of `port`.
        fn echo(&mut self, port: usize, task_no: u32) {
            let mut can = self.harness.client.can_mut();
            let mut ports = can.ports_mut();
            ports.get_mut(port).expect(PORT_RECORD).ack_mut().task_no_mut().set(task_no).expect("a plain field");
        }

        fn port_count(&self) -> u32 {
            self.harness.server.can().port_count()
        }

        /// The published `(generation, succeeded)` of `port`.
        fn response(&self, port: usize) -> (u32, bool) {
            let can = self.harness.server.can();
            let response = can.ports().get(port).expect(PORT_RECORD).response();
            (response.generation(), response.succeeded())
        }

        /// The published `(task_no, packet_count)` of `port`.
        fn ack(&self, port: usize) -> (u32, u32) {
            let can = self.harness.server.can();
            let ack = can.ports().get(port).expect(PORT_RECORD).ack();
            (ack.task_no(), ack.packet_count())
        }

        /// The published frame `slot` of `port`: `(target_index, command, data_size, data)`.
        fn packet(&self, port: usize, slot: usize) -> (u16, u8, u8, [u8; MAX_FRAME_DATA]) {
            let can = self.harness.server.can();
            let ack = can.ports().get(port).expect(PORT_RECORD).ack();
            let packet = ack.packets().get(slot).expect("a batch holds at most MAX_ACK_PACKETS frames");
            (packet.target_index(), packet.command(), packet.data_size(), *packet.data())
        }

        /// The published `target_count` of `port`.
        fn target_count(&self, port: usize) -> u16 {
            self.harness.server.can().ports().get(port).expect(PORT_RECORD).target_count()
        }

        /// The published `id` of target `index` of `port`.
        fn target_id(&self, port: usize, index: usize) -> u8 {
            let can = self.harness.server.can();
            let port = can.ports().get(port).expect(PORT_RECORD);
            port.targets().get(index).expect("sixteen targets").id()
        }

        fn sent(&self) -> Vec<(usize, usize, u8, CanPacket)> {
            self.can.borrow().sent.clone()
        }

        fn push_events(&self, events: impl IntoIterator<Item = CanEvent>) {
            self.can.borrow_mut().events.extend(events);
        }
    }

    // ---- Send requests -------------------------------------------------------------------------------

    #[test]
    fn send_reaches_libam_in_post_and_is_acknowledged_one_frame_later() {
        let mut rig = Rig::new();
        rig.tick();
        rig.request(0, 1, &[command(1, 0x30, &[1, 2, 3], 4), command(0, 0x31, &[], 0)]);

        rig.tick();
        let first = CanPacket::new(0x30, &[1, 2, 3, 0, 0, 0, 0, 0], 3);
        let second = CanPacket::new(0x31, &[0; 8], 0);
        assert_eq!(rig.sent(), [(0, 1, 4, first), (0, 0, 0, second)]);
        assert_eq!(rig.response(0).0, 0, "accepted in postExecute, not yet acknowledged");

        rig.tick();
        let (generation, succeeded) = rig.response(0);
        assert_eq!(generation, 1);
        assert!(succeeded);
        assert_eq!(rig.response(1).0, 0, "port 1 untouched");
    }

    #[test]
    fn disabled_backend_fails_every_send_with_a_terminal_ack() {
        let mut rig = Rig::disabled();
        rig.tick();
        assert_eq!(rig.port_count(), 0);
        rig.exchange(0, 1, &[command(0, 1, &[1], 0)]);
        let (generation, succeeded) = rig.response(0);
        assert_eq!(generation, 1, "the request is still acknowledged");
        assert!(!succeeded, "but as failed");
    }

    #[test]
    fn completion_callback_reporting_failure_is_ignored_so_the_send_fails() {
        let mut rig = Rig::new();
        rig.can.borrow_mut().complete_on_send = Some(false);
        rig.tick();
        rig.exchange(1, 1, &[command(0, 1, &[], 0)]);
        assert!(!rig.response(1).1);
    }

    #[test]
    fn completion_arriving_outside_accept_does_not_help_a_later_send() {
        let mut rig = Rig::new();
        rig.can.borrow_mut().complete_on_send = None;
        rig.tick();
        rig.push_events([CanEvent::SendComplete { port: 0, ok: true }]);
        rig.tick();
        rig.exchange(0, 1, &[command(0, 1, &[], 0)]);
        assert!(!rig.response(0).1, "sendState is armed again by accept");
    }

    #[test]
    fn only_the_last_send_result_decides_the_outcome() {
        let mut rig = Rig::new();
        rig.tick();
        // First send is refused by libam (target out of range), the last one is fine: success.
        rig.exchange(0, 1, &[command(9, 1, &[], 0), command(1, 2, &[], 0)]);
        assert!(rig.response(0).1, "earlier failure is masked");
        assert_eq!(rig.sent().len(), 1);

        // First send fine, last one has an invalid priority: failure, although a callback was seen.
        rig.exchange(0, 2, &[command(0, 3, &[], 0), command(1, 4, &[], 16)]);
        assert!(!rig.response(0).1);
        assert_eq!(rig.sent().len(), 2, "the first frame of the second request was still sent");
    }

    #[test]
    fn an_empty_command_list_fails() {
        let mut rig = Rig::new();
        rig.tick();
        rig.exchange(0, 1, &[]);
        assert!(rig.sent().is_empty());
        assert!(!rig.response(0).1);
        assert_eq!(rig.response(0).0, 1);
    }

    #[test]
    fn data_size_above_eight_sends_an_empty_command_zero_packet() {
        let mut rig = Rig::new();
        rig.tick();
        let oversize = SendCommand { data_size: 9, data: [7; 8], ..command(1, 0x55, &[], 3) };
        rig.exchange(0, 1, &[oversize]);
        assert_eq!(rig.sent(), [(0, 1, 3, CanPacket { command: 0, size: 0, data: [0; 8] })]);
        assert!(rig.response(0).1, "the send is not rejected");
    }

    #[test]
    fn more_than_sixteen_commands_are_rejected_without_sending() {
        let mut rig = Rig::new();
        rig.tick();
        rig.write_request(0, 1, CanTask::Send as u32, &send_info(17, &[command(0, 1, &[], 0)]));
        rig.tick();
        rig.tick();
        assert!(rig.sent().is_empty());
        assert!(!rig.response(0).1);
        assert_eq!(rig.response(0).0, 1);
    }

    #[test]
    fn unknown_task_fails_without_calling_libam() {
        let mut rig = Rig::new();
        rig.tick();
        rig.write_request(0, 1, 1, &send_info(1, &[command(0, 1, &[], 0)]));
        rig.tick();
        rig.tick();
        assert!(rig.sent().is_empty());
        assert!(!rig.response(0).1);
    }

    #[test]
    fn can_never_keeps_the_app_busy() {
        let mut rig = Rig::new();
        rig.tick();
        rig.request(0, 1, &[command(0, 1, &[], 0)]);
        let mut ctx = rig.harness.ctx();
        rig.slot.pre_execute(&mut ctx);
        rig.slot.post_execute(&mut ctx);
        let response = ctx.server.can().ports().get(0).expect(PORT_RECORD).response().generation();
        let request = ctx.client.can().ports().get(0).expect(PORT_RECORD).request().generation();
        assert_ne!(response, request);
        assert!(!rig.slot.controller().is_busy(&ctx), "request in flight, yet vf3 is the base default");
    }

    // ---- Ready-edge publication ----------------------------------------------------------------------

    #[test]
    fn ready_edge_publishes_ports_and_mapped_targets() {
        let mut rig = Rig::new();
        {
            let mut can = rig.can.borrow_mut();
            can.targets[0] = vec![
                CanTargetInfo {
                    target_type: 2,
                    id: 7,
                    product: "MOTOR-DRIVER-ABC".into(),
                    custom_chip: "CHIP-0123456789".into(),
                    firm_revision: 3,
                    firm_sum: 0xBEEF,
                },
                node(1, 1),
                node(5, 2),
                node(0, 3),
                node(6, 4),
            ];
            can.targets[1] = Vec::new();
        }
        rig.tick();
        let server = rig.harness.server.can();
        assert_eq!(server.port_count(), 2);
        let ports = server.ports();
        let port = ports.get(0).expect(PORT_RECORD);
        assert_eq!(port.target_count(), 5);
        assert_eq!(ports.get(1).expect(PORT_RECORD).target_count(), 0);
        let targets = port.targets();
        let first = targets.get(0).expect("sixteen targets");
        assert_eq!(first.target_type(), CanTargetType::Motor);
        assert_eq!((first.id(), first.firm_revision(), first.firm_sum()), (7, 3, 0xBEEF));
        assert_eq!(String::from_utf16_lossy(first.product().as_slice()), "MOTOR-DRIVER", "12 characters");
        assert_eq!(String::from_utf16_lossy(first.custom_chip().as_slice()), "CHIP-0123", "9 characters");
        let types: Vec<_> = (1..5).map(|index| targets.get(index).expect("sixteen targets").target_type()).collect();
        assert_eq!(types, [CanTargetType::Host, CanTargetType::Led, CanTargetType::Reserved, CanTargetType::Reserved]);
        let beyond = port_bytes(&rig.harness.server, 0, target_bytes(5));
        assert!(beyond.iter().all(|&byte| byte == 0), "slots beyond targetCount untouched");
    }

    #[test]
    fn targets_are_not_refreshed_after_the_ready_edge() {
        let mut rig = Rig::new();
        rig.tick();
        assert_eq!(rig.target_id(0, 0), 1);
        rig.can.borrow_mut().targets[0][0].id = 99;
        rig.tick();
        assert_eq!(rig.target_id(0, 0), 1);
    }

    #[test]
    fn too_many_ports_and_targets_are_clamped() {
        let mut rig = Rig::new();
        rig.can.borrow_mut().targets = vec![(0..17).map(|id| node(1, id)).collect(), Vec::new(), Vec::new()];
        rig.tick();
        assert_eq!(rig.port_count(), 2);
        assert_eq!(rig.target_count(0), 16);
        assert_eq!(rig.target_id(0, 15), 15);
    }

    #[test]
    fn unmatched_target_writes_a_zero_record() {
        let mut rig = Rig::new();
        rig.can.borrow_mut().declared = vec![Some(2)];
        rig.can.borrow_mut().targets = vec![vec![node(1, 5)]];
        {
            let mut can = rig.harness.server.can_mut();
            let mut ports = can.ports_mut();
            let mut port = ports.get_mut(0).expect(PORT_RECORD);
            port.targets_mut().get_mut(1).expect("sixteen targets").id_mut().set(0x77).expect("a plain field");
        }
        rig.tick();
        assert_eq!(rig.target_count(0), 2);
        assert_eq!(rig.target_id(0, 0), 5);
        assert!(port_bytes(&rig.harness.server, 0, target_bytes(1)).iter().all(|&byte| byte == 0));
    }

    // ---- Received frames and ACK back-pressure ---------------------------------------------------------

    #[test]
    fn received_frames_are_published_in_one_batch_and_wait_for_the_client_echo() {
        let mut rig = Rig::new();
        rig.tick();
        rig.push_events([frame(0, 1, 0x10, &[1]), frame(0, 2, 0x10, &[2, 3])]);
        rig.tick();
        assert_eq!(rig.ack(0), (1, 2));
        assert_eq!(rig.packet(0, 1), (2, 0x10, 2, [2, 3, 0, 0, 0, 0, 0, 0]));
        let other = port_bytes(&rig.harness.server, 1, ACK_BYTES);
        assert!(other.iter().all(|&byte| byte == 0), "port 1 has its own queue");

        // The client has not echoed batch 1: new frames accumulate and nothing is published.
        rig.push_events([frame(0, 3, 0x20, &[9])]);
        rig.tick();
        rig.tick();
        assert_eq!(rig.ack(0), (1, 2));

        rig.echo(0, 1);
        rig.tick();
        assert_eq!(rig.ack(0), (2, 1));
        assert_eq!(rig.packet(0, 0).0, 3);
        assert_eq!(rig.packet(0, 1).0, 2, "stale slot of the previous batch is not cleared");
    }

    #[test]
    fn a_client_echo_that_differs_from_the_server_number_stalls_the_queue() {
        let mut rig = Rig::new();
        rig.tick();
        rig.echo(1, 5);
        rig.push_events([frame(1, 1, 0x10, &[1])]);
        rig.tick();
        rig.tick();
        assert_eq!(rig.ack(1).0, 0);
        rig.echo(1, 0);
        rig.tick();
        assert_eq!(rig.ack(1), (1, 1));
    }

    #[test]
    fn frames_with_the_same_target_and_command_coalesce_to_the_newest() {
        let mut rig = Rig::new();
        rig.tick();
        rig.push_events([
            frame(0, 1, 0x10, &[1]),
            frame(0, 2, 0x10, &[2]),
            frame(0, 1, 0x11, &[3]),
            frame(0, 1, 0x10, &[4]),
        ]);
        rig.tick();
        let (_, count) = rig.ack(0);
        let published: Vec<_> = (0..count as usize)
            .map(|slot| {
                let (target_index, command, _, data) = rig.packet(0, slot);
                (target_index, command, data[0])
            })
            .collect();
        assert_eq!(published, [(2, 0x10, 2), (1, 0x11, 3), (1, 0x10, 4)], "newest data wins and moves to the tail");
    }

    #[test]
    fn frames_with_more_than_eight_data_bytes_are_dropped() {
        let mut rig = Rig::new();
        rig.tick();
        let too_big =
            CanEvent::Received { port: 0, target_index: 1, packet: CanPacket { command: 0x10, size: 9, data: [1; 8] } };
        let max =
            CanEvent::Received { port: 0, target_index: 2, packet: CanPacket { command: 0x10, size: 8, data: [2; 8] } };
        rig.push_events([too_big, max]);
        rig.tick();
        assert_eq!(rig.ack(0).1, 1);
        let (target_index, _, data_size, _) = rig.packet(0, 0);
        assert_eq!((target_index, data_size), (2, 8));
    }

    #[test]
    fn a_batch_holds_at_most_sixteen_frames_and_the_rest_follows_after_the_echo() {
        let mut rig = Rig::new();
        rig.tick();
        rig.push_events((0..20).map(|target| frame(0, target, 0x10, &[target as u8])));
        rig.tick();
        assert_eq!(rig.ack(0), (1, 16));
        assert_eq!(rig.packet(0, 15).0, 15);

        rig.echo(0, 1);
        rig.tick();
        assert_eq!(rig.ack(0), (2, 4));
        assert_eq!(rig.packet(0, 3).0, 19);
        assert_eq!(rig.packet(0, 4).0, 4, "slots after packetCount keep the previous batch");
    }

    #[test]
    fn frames_received_while_not_ready_are_queued_and_published_after_the_ready_edge() {
        let mut rig = Rig::new();
        rig.harness.manager.on_change_state(libam_api::sequence::ProcessState(1));
        rig.push_events([frame(0, 1, 0x10, &[1])]);
        rig.tick();
        assert_eq!(rig.ack(0).0, 0, "controller is not ready");
        rig.harness.manager.on_change_state(libam_api::sequence::ProcessState(3));
        rig.tick();
        assert_eq!(rig.ack(0).1, 1);
    }

    // ---- Configuration and trace -----------------------------------------------------------------------

    #[test]
    fn trace_switches_to_uppercase_hex_after_the_first_data_byte() {
        let mut rig = Rig::new();
        let sample = command(300, 10, &[1, 0xAB, 0xCD], 12);
        rig.request(1, 1, &[sample, sample]);
        let client = rig.harness.client.can();
        let ports = client.ports();
        let request = ports.get(1).expect(PORT_RECORD).request();
        let data = " 0x01 0xAB 0xCD 0x00 0x00 0x00 0x00 0x00";
        assert_eq!(
            trace_send(1, &request),
            format!(
                "CAN - port[1] task: Send\n  command[0] - index: 300, code: 10, size: 3, data:{data}, priority: C\n  \
                 command[1] - index: 12C, code: A, size: 3, data:{data}, priority: C"
            )
        );
    }
}
