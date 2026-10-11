//! `amdaemon::server::NetworkController` (vtable `0x1405D43D8`, ctor `0x14003E7C0`, `App+0xB58`) and its
//! `NetworkEventListener` sub-object (vtable `0x1405D43C0`).
//!
//! Schema blocks: `ServerData.network` (`+0x38D0`) and `ClientData.network` (`+0x1808`)
//! ([`crate::schema::network`]). Evidence: `spec/controllers/network_install.md` §2, claims
//! `network_install.network.*`.
//!
//! Behaviour in one paragraph: the only task is `TestStart`; its acceptor starts libam's network test and the
//! executor acknowledges the *submission* one frame later (the checker is the constant `Succeeded`). The test
//! itself runs inside libam and reports through [`NetworkPort::take_test_events`], which this controller turns
//! into the published `powerOnTest` / `test` blocks, the network property and the availability bytes. The
//! events are applied whether or not the controller is ready, exactly like the libam callback in the original.

use crate::executor::{RequestExecutor, RequestState};
use crate::schema::network::{
    NetworkTask, NetworkTestResult, NetworkTestState, ServerNetworkMut, ServerNetworkPatch, ServerPropertyPatch,
    ServerTestItemPatch, ServerTestMut, ServerTestPatch, TEST_ITEM_COUNT,
};
use crate::schema::shared::ErrorDataPatch;
use libam_api::network::{NetworkPort, NetworkProperty, NetworkTestEvent, TestItem, TestStatus};
use libam_api::result::{AmError, AmResult};
use libam_api::sequence::ProcessState;

use crate::controllers::controller::{Controller, ControllerId, Ctx, commit_check};
use crate::controllers::error::publish_fatal_error;
use libam_api::config::{AppConfig, ProcessConfig};

/// `ServerTestData.items` slot of the ALL.Net Air pseudo item: it has no libam checker and is driven by the
/// process configuration (§2.6.3) and the test-start / test-finished events.
const AIR_SLOT: usize = 9;
/// Why a write to a scalar field cannot fail: an integer or `bool` value is always representable.
const SCALARS: &str = "scalar fields accept every value";
/// Why a write of a closed-enum value cannot fail.
const CLOSED_ENUM: &str = "a declared enum value is always valid";
/// Why an item slot lookup cannot fail: every slot is below [`TEST_ITEM_COUNT`].
const SLOT: &str = "item slots lie inside the item array";

/// The `state`/`result` numbers of a libam [`TestStatus`] as the published enumerations.
trait TestStatusExt {
    /// Typed state; `None` for numbers outside `0..=2`.
    fn state(&self) -> Option<NetworkTestState>;

    /// Typed result; `None` for numbers outside `0..=3`.
    fn result(&self) -> Option<NetworkTestResult>;

    /// Item 0 just started: running with no result yet (the test (re)start marker).
    fn is_start(&self) -> bool {
        self.state() == Some(NetworkTestState::Running) && self.result() == Some(NetworkTestResult::None)
    }

    /// Done (any result).
    fn is_done(&self) -> bool {
        self.state() == Some(NetworkTestState::Done)
    }

    /// Good or Bad: the results that refresh the published error.
    fn has_verdict(&self) -> bool {
        matches!(self.result(), Some(NetworkTestResult::Good | NetworkTestResult::Bad))
    }
}

impl TestStatusExt for TestStatus {
    fn state(&self) -> Option<NetworkTestState> {
        NetworkTestState::try_from(self.state).ok()
    }

    fn result(&self) -> Option<NetworkTestResult> {
        NetworkTestResult::try_from(self.result).ok()
    }
}

/// `amdaemon::server::NetworkController`.
pub struct NetworkController {
    port: Box<dyn NetworkPort>,
    /// Executor at `this+0x28`; acceptor [`accept_task`], checker constant `Succeeded` (`0x1400400B0`).
    executor: RequestExecutor,
}

impl std::fmt::Debug for NetworkController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetworkController").field("executor", &self.executor).finish_non_exhaustive()
    }
}

impl NetworkController {
    /// Constructs the controller (ctor `0x14003E7C0`).
    pub fn new(port: Box<dyn NetworkPort>) -> Self {
        Self { port, executor: RequestExecutor::default() }
    }

    /// `NetworkEventListener::onTestEvent` (`0x14003E1C0`).
    fn on_test_event(&self, event: &NetworkTestEvent, ctx: &mut Ctx<'_>) {
        let Ctx { server, .. } = ctx;
        let mut net = server.network_mut();
        track_test_progress(&mut net, event);
        if event.status.has_verdict() {
            publish_fatal_error(&event.error, &mut test_block(&mut net, event.is_power_on).error_mut());
        }
        let Some(slot) = event.item.slot() else { return };
        copy_item_status(&mut test_block(&mut net, event.is_power_on), slot, &event.status);
        self.update_network_property(&mut net, event);
        update_hops(&mut test_block(&mut net, event.is_power_on), event);
        self.update_network_available(&mut net, event);
    }

    /// `NetworkEventListener::updateNetworkProperty` (`0x14003E320`): item 0 Done captures the property.
    fn update_network_property(&self, net: &mut ServerNetworkMut<'_>, event: &NetworkTestEvent) {
        if event.item != TestItem::IpAddress || !event.status.is_done() {
            return;
        }
        if self.port.network_handle().is_err() {
            tracing::warn!("libam is not ready");
        } else if let Some(property) = self.port.property() {
            net.property_mut().copy_from(&property_patch(&property)).expect(SCALARS);
        }
    }

    /// `NetworkEventListener::updateNetworkAvailable` (`0x14003E4D0`): refreshes one availability byte when
    /// the item is Done (items 0, 3 and 5 only).
    fn update_network_available(&self, net: &mut ServerNetworkMut<'_>, event: &NetworkTestEvent) {
        if !event.status.is_done() {
            return;
        }
        if self.port.network_handle().is_err() {
            tracing::warn!("libam is not ready");
            return;
        }
        match event.item {
            TestItem::IpAddress => net.lan_available_mut().set(self.port.is_lan_available()).expect(SCALARS),
            TestItem::Hops => {
                net.location_lan_available_mut().set(self.port.is_location_lan_available()).expect(SCALARS);
            }
            TestItem::AllnetAuth => net.wan_available_mut().set(self.port.is_wan_available()).expect(SCALARS),
            _ => {}
        }
    }

    /// `NetworkController::loadAvailable` (`0x14003EC30`).
    fn load_available(&self, net: &mut ServerNetworkMut<'_>) {
        if self.port.network_handle().is_err() {
            tracing::warn!("libam is not ready");
        } else {
            net.available_mut().set(self.port.is_enabled()).expect(SCALARS);
        }
    }

    /// `NetworkController::loadTestTarget` (`0x14003ED20`): samples the per-checker enabled flags. The Air
    /// slot is skipped; without a handle every checker reads `false`.
    fn load_test_target(&self, block: &mut ServerTestMut<'_>) {
        let handle = self.port.network_handle();
        if handle.is_err() {
            tracing::warn!("libam is not ready");
        }
        for item in TestItem::CHECKERS {
            if let Some(slot) = item.slot() {
                set_available(block, slot, handle.is_ok() && self.port.is_item_enabled(item));
            }
        }
    }
}

impl Controller for NetworkController {
    fn id(&self) -> ControllerId {
        ControllerId::Network
    }

    /// `0x14003E980`: an accepted request is awaiting its acknowledgement.
    fn is_busy(&self, ctx: &Ctx<'_>) -> bool {
        self.executor.is_busy(ctx.client.network().generation(), ctx.server.network().response().generation())
    }

    /// `0x14003E9B0`: samples `available`, then both test targets (power-on first).
    fn clear_server_data(&mut self, ctx: &mut Ctx<'_>) {
        let mut net = ctx.server.network_mut();
        self.load_available(&mut net);
        self.load_test_target(&mut net.power_on_test_mut());
        self.load_test_target(&mut net.test_mut());
    }

    /// `0x14003EA10`: only `RequestExecutor::checkResult`; everything else is event driven.
    fn update_server_data(&mut self, ctx: &mut Ctx<'_>) {
        let request_generation = ctx.client.network().generation();
        let mut net = ctx.server.network_mut();
        let response_generation = net.response().generation();
        let outcome = self.executor.check_result(request_generation, response_generation, || RequestState::Succeeded);
        commit_check(outcome, &mut net.response_mut());
    }

    /// `0x14003EA20`: accept edge.
    fn accept_requests(&mut self, ctx: &mut Ctx<'_>) {
        let request_generation = ctx.client.network().generation();
        let response_generation = ctx.server.network().response().generation();
        let Self { port, executor } = self;
        executor.accept(request_generation, response_generation, || accept_task(port.as_mut(), ctx));
    }

    /// Delivers the libam `onTestEvent` calls; independent of readiness.
    fn on_libam_events(&mut self, ctx: &mut Ctx<'_>) {
        for event in self.port.take_test_events() {
            self.on_test_event(&event, ctx);
        }
    }

    /// `process::Config` observer `0x1400400C0`: publishes `allnet_air.enable` as the Air item's `available`.
    fn on_process_config(&mut self, config: &ProcessConfig, ctx: &mut Ctx<'_>) {
        let air = config.network.allnet_air_enable;
        let mut net = ctx.server.network_mut();
        set_available(&mut net.power_on_test_mut(), AIR_SLOT, air);
        set_available(&mut net.test_mut(), AIR_SLOT, air);
    }

    /// `am::AppConfig` callback `0x140040090`: installs the test event listener.
    fn on_app_config(&mut self, config: &mut AppConfig, _ctx: &mut Ctx<'_>) {
        config.network.test_event_listener = true;
    }
}

/// `NetworkController::accept` (`0x14003EA80`): returns `Pending` (1) when libam started the test, otherwise
/// `Failed` (3).
fn accept_task(port: &mut dyn NetworkPort, ctx: &mut Ctx<'_>) -> RequestState {
    let task = ctx.client.network().task_kind();
    if ctx.manager.debug_dump() {
        let name = task.map_or("", task_name);
        tracing::info!("Network - task: {name}");
    }
    if port.network_handle().is_err() {
        tracing::warn!("libam is not ready");
        return RequestState::Failed;
    }
    let result = match task {
        Some(NetworkTask::TestStart) => start_test(port),
        None => Err(AmError::with_message(AmError::UNKNOWN_TASK, "Requested unknown task.")),
    };
    if result.is_ok() { RequestState::Pending } else { RequestState::Failed }
}

/// `ipc::network::Task` name (`0x14006D920`).
const fn task_name(task: NetworkTask) -> &'static str {
    match task {
        NetworkTask::TestStart => "TestStart",
    }
}

/// libam `startTest` (`0x140217140`): only valid while the sequence is in process state 5 (test).
fn start_test(port: &mut dyn NetworkPort) -> AmResult<()> {
    if port.sequence_process_state() != ProcessState(5) {
        return Err(AmError::with_message(AmError::NOT_INITIALIZED, "Invalid sequence."));
    }
    port.begin_test();
    Ok(())
}

/// The block an event belongs to (`isPowerOn ? net+0x28 : net+0x248`).
fn test_block<'a>(net: &'a mut ServerNetworkMut<'_>, is_power_on: bool) -> ServerTestMut<'a> {
    if is_power_on { net.power_on_test_mut() } else { net.test_mut() }
}

/// Publishes `available` of the item in `slot` of a test block.
fn set_available(block: &mut ServerTestMut<'_>, slot: usize, available: bool) {
    block.items_mut().get_mut(slot).expect(SLOT).available_mut().set(available).expect(SCALARS);
}

/// Step 1 of `onTestEvent`: the final event completes the block, an item-0 start event restarts it.
fn track_test_progress(net: &mut ServerNetworkMut<'_>, event: &NetworkTestEvent) {
    match event.item {
        TestItem::AllFinished => {
            let mut block = test_block(net, event.is_power_on);
            let patch = ServerTestPatch { running: Some(false), completed: Some(true), ..ServerTestPatch::default() };
            block.copy_from(&patch).expect(SCALARS);
            let mut items = block.items_mut();
            for slot in 0..TEST_ITEM_COUNT {
                let mut item = items.get_mut(slot).expect(SLOT);
                if item.state() == NetworkTestState::Running {
                    item.state_mut().set(NetworkTestState::Done).expect(CLOSED_ENUM);
                }
            }
        }
        TestItem::IpAddress if event.status.is_start() => {
            let mut block = test_block(net, event.is_power_on);
            let patch = ServerTestPatch {
                running: Some(true),
                completed: Some(false),
                hops: Some(0),
                error: Some(ErrorDataPatch { number: Some(0), ..ErrorDataPatch::default() }),
                ..ServerTestPatch::default()
            };
            block.copy_from(&patch).expect(SCALARS);
            let mut items = block.items_mut();
            let mut air = items.get_mut(AIR_SLOT).expect(SLOT);
            if air.available() {
                let patch = ServerTestItemPatch {
                    state: Some(NetworkTestState::Done.into()),
                    result: Some(NetworkTestResult::Good.into()),
                    ..ServerTestItemPatch::default()
                };
                air.copy_from(&patch).expect(CLOSED_ENUM);
            }
            // The availability bytes are shared by the power-on and the ordinary test.
            let patch = ServerNetworkPatch {
                lan_available: Some(false),
                location_lan_available: Some(false),
                wan_available: Some(false),
                ..ServerNetworkPatch::default()
            };
            net.copy_from(&patch).expect(SCALARS);
        }
        _ => {}
    }
}

/// State/result copy (`0x14003E6B0`) into the item in `slot`: numbers outside libam's enumerations leave the
/// published value unchanged.
fn copy_item_status(block: &mut ServerTestMut<'_>, slot: usize, status: &TestStatus) {
    let patch = ServerTestItemPatch {
        state: status.state().map(Into::into),
        result: status.result().map(Into::into),
        ..ServerTestItemPatch::default()
    };
    block.items_mut().get_mut(slot).expect(SLOT).copy_from(&patch).expect(CLOSED_ENUM);
}

/// `ServerProperty` update from the libam property (`0x14003E750`); `valid` latches to 1.
fn property_patch(property: &NetworkProperty) -> ServerPropertyPatch {
    ServerPropertyPatch {
        valid: Some(true),
        dhcp_enabled: Some(property.dhcp),
        address: Some(property.address),
        subnet_mask: Some(property.subnet_mask),
        gateway: Some(property.gateway),
        primary_dns: Some(property.primary_dns),
        secondary_dns: Some(property.secondary_dns),
    }
}

/// `updateHops` (`0x14003E430`): the HOPS item's Done/Good text is the hop count.
fn update_hops(block: &mut ServerTestMut<'_>, event: &NetworkTestEvent) {
    let status = &event.status;
    if event.item == TestItem::Hops && status.is_done() && status.result() == Some(NetworkTestResult::Good) {
        let hops = parse_c_unsigned(status.text_units()).unwrap_or(0);
        block.hops_mut().set(hops).expect(SCALARS);
    }
}

/// `wcstoul(text, &end, 0)` accepted only when the whole string was consumed (`*end == 0`).
///
/// Mirrors the C runtime: leading white space, an optional sign (a minus negates modulo 2^32), base detection
/// (`0x`/`0X` hex, leading `0` octal, otherwise decimal) and saturation to `ULONG_MAX` on overflow. The empty
/// string consumes entirely and yields 0. `None` means unconsumed trailing text or no digits at all, for which
/// the caller publishes 0 either way.
fn parse_c_unsigned(text: &[u16]) -> Option<u32> {
    if text.is_empty() {
        return Some(0);
    }
    let mut rest = text;
    while let [first, tail @ ..] = rest {
        if !matches!(*first, 0x09..=0x0D | 0x20) {
            break;
        }
        rest = tail;
    }
    let negative = match rest {
        [0x2D, tail @ ..] => {
            rest = tail;
            true
        }
        [0x2B, tail @ ..] => {
            rest = tail;
            false
        }
        _ => false,
    };
    let (radix, digits) = match rest {
        [0x30, x, tail @ ..] if matches!(*x, 0x78 | 0x58) => (16, tail),
        [0x30, ..] => (8, rest),
        _ => (10, rest),
    };
    if digits.is_empty() {
        return None;
    }
    let mut value: u64 = 0;
    for &unit in digits {
        let digit = char::from_u32(u32::from(unit)).and_then(|c| c.to_digit(radix))?;
        value = (value * u64::from(radix) + u64::from(digit)).min(u64::from(u32::MAX) + 1);
    }
    Some(match u32::try_from(value) {
        Ok(value) if negative => value.wrapping_neg(),
        Ok(value) => value,
        Err(_) => u32::MAX,
    })
}

const _: () = assert!(TEST_ITEM_COUNT == AIR_SLOT + 1);

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::ops::Range;
    use std::rc::Rc;

    use crate::schema::shared::ErrorData;

    use super::*;
    use crate::controllers::controller::ControllerSlot;
    use crate::controllers::controller::testing::Harness;
    use crate::schema::network::ClientNetworkPatch;
    use libam_api::error::FatalError;
    use libam_api::time::Tm;

    const IDLE: u32 = 0;
    const RUNNING: u32 = 1;
    const DONE: u32 = 2;
    const NO_RESULT: u32 = 0;
    const NOT_APPLICABLE: u32 = 1;
    const GOOD: u32 = 2;
    const BAD: u32 = 3;

    /// A valid libam timestamp, so `FatalError::publish` writes the whole record.
    const TIME: Tm = Tm { sec: 1, min: 2, hour: 3, mday: 4, mon: 5, year: 124, wday: 0, yday: 0, isdst: 0 };

    /// libam as seen through the port.
    struct Libam {
        handle_ok: bool,
        enabled: bool,
        enabled_items: [bool; 9],
        process_state: u32,
        begun: u32,
        property: Option<NetworkProperty>,
        lan: bool,
        location_lan: bool,
        wan: bool,
        events: Vec<NetworkTestEvent>,
    }

    impl Libam {
        fn new() -> Self {
            Self {
                handle_ok: true,
                enabled: false,
                enabled_items: [false; 9],
                process_state: 5,
                begun: 0,
                property: None,
                lan: false,
                location_lan: false,
                wan: false,
                events: Vec::new(),
            }
        }
    }

    struct FakePort(Rc<RefCell<Libam>>);

    impl NetworkPort for FakePort {
        fn network_handle(&self) -> AmResult<()> {
            if self.0.borrow().handle_ok { Ok(()) } else { Err(AmError::code(AmError::NOT_INITIALIZED)) }
        }
        fn is_enabled(&self) -> bool {
            self.0.borrow().enabled
        }
        fn is_item_enabled(&self, item: TestItem) -> bool {
            item.slot().is_some_and(|slot| self.0.borrow().enabled_items[slot])
        }
        fn sequence_process_state(&self) -> ProcessState {
            ProcessState(self.0.borrow().process_state)
        }
        fn begin_test(&mut self) {
            self.0.borrow_mut().begun += 1;
        }
        fn property(&self) -> Option<NetworkProperty> {
            self.0.borrow().property
        }
        fn is_lan_available(&self) -> bool {
            self.0.borrow().lan
        }
        fn is_location_lan_available(&self) -> bool {
            self.0.borrow().location_lan
        }
        fn is_wan_available(&self) -> bool {
            self.0.borrow().wan
        }
        fn take_test_events(&mut self) -> Vec<NetworkTestEvent> {
            std::mem::take(&mut self.0.borrow_mut().events)
        }
    }

    fn fixture() -> (NetworkController, Rc<RefCell<Libam>>, Harness) {
        let libam = Rc::new(RefCell::new(Libam::new()));
        let controller = NetworkController::new(Box::new(FakePort(Rc::clone(&libam))));
        (controller, libam, Harness::running())
    }

    fn status(state: u32, result: u32) -> TestStatus {
        TestStatus { state, result, text: [0; 16] }
    }

    fn event(is_power_on: bool, item: TestItem, state: u32, result: u32) -> NetworkTestEvent {
        NetworkTestEvent { is_power_on, item, status: status(state, result), error: FatalError::default() }
    }

    /// libam status text: up to 15 units, terminated.
    fn wide16(text: &str) -> [u16; 16] {
        let mut units = [0; 16];
        for (slot, unit) in units.iter_mut().zip(text.encode_utf16().take_while(|&unit| unit != 0).take(15)) {
            *slot = unit;
        }
        units
    }

    fn hops_event(state: u32, result: u32, text: &str) -> NetworkTestEvent {
        let mut e = event(false, TestItem::Hops, state, result);
        e.status.text = wide16(text);
        e
    }

    fn deliver(
        controller: &mut NetworkController,
        libam: &Rc<RefCell<Libam>>,
        h: &mut Harness,
        events: Vec<NetworkTestEvent>,
    ) {
        libam.borrow_mut().events.extend(events);
        controller.on_libam_events(&mut h.ctx());
    }

    fn request(h: &mut Harness, generation: u32, task: u32) {
        let patch = ClientNetworkPatch { generation: Some(generation), task: Some(task) };
        h.client.network_mut().copy_from(&patch).expect("integer fields accept every value");
    }

    /// Item `slot` of the power-on (`true`) or the ordinary test block as `(available, state, result)`.
    fn item(h: &Harness, power_on: bool, slot: usize) -> (bool, NetworkTestState, NetworkTestResult) {
        let net = h.server.network();
        let block = if power_on { net.power_on_test() } else { net.test() };
        let item = block.items().get(slot).expect("slot inside the item array");
        (item.available(), item.state(), item.result())
    }

    fn seed_available(h: &mut Harness, power_on: bool, slot: usize, available: bool) {
        let patch = ServerTestItemPatch { available: Some(available), ..ServerTestItemPatch::default() };
        let mut net = h.server.network_mut();
        let mut block = if power_on { net.power_on_test_mut() } else { net.test_mut() };
        block.items_mut().get_mut(slot).expect("slot inside the item array").copy_from(&patch).expect("a bool");
    }

    fn seed_state(h: &mut Harness, power_on: bool, slot: usize, state: NetworkTestState) {
        let patch = ServerTestItemPatch { state: Some(state.into()), ..ServerTestItemPatch::default() };
        let mut net = h.server.network_mut();
        let mut block = if power_on { net.power_on_test_mut() } else { net.test_mut() };
        block.items_mut().get_mut(slot).expect("slot inside the item array").copy_from(&patch).expect("a closed enum");
    }

    fn seed_block(h: &mut Harness, power_on: bool, patch: &ServerTestPatch<'_>) {
        let mut net = h.server.network_mut();
        let mut block = if power_on { net.power_on_test_mut() } else { net.test_mut() };
        block.copy_from(patch).expect("integer and bool fields accept every value");
    }

    /// Writes `error` into the `error` record of a test block, the way a Good/Bad event does.
    fn publish(h: &mut Harness, power_on: bool, error: &FatalError) {
        let mut net = h.server.network_mut();
        let mut block = if power_on { net.power_on_test_mut() } else { net.test_mut() };
        publish_fatal_error(error, &mut block.error_mut());
    }

    /// The bytes of the `error` record of a test block inside the server image: `ServerData.network`
    /// (`+0x38D0`), the block (`+0x28` power-on, `+0x248` ordinary), `error` (`+0x80`).
    fn error_span(power_on: bool) -> Range<usize> {
        let start = 0x38D0 + if power_on { 0x28 } else { 0x248 } + 0x80;
        start..start + ErrorData::SCHEMA_SIZE
    }

    fn error_bytes(h: &Harness, power_on: bool) -> Vec<u8> {
        h.server.bytes()[error_span(power_on)].to_vec()
    }

    /// What `FatalError::publish` leaves in the `error` record of a test block of a fresh image after the
    /// `errors` were published in order.
    fn published_error(power_on: bool, errors: &[&FatalError]) -> Vec<u8> {
        let mut reference = Harness::new();
        for error in errors {
            publish(&mut reference, power_on, error);
        }
        error_bytes(&reference, power_on)
    }

    #[test]
    fn test_start_is_accepted_only_in_sequence_state_5() {
        for (state, accepted) in [(0, false), (2, false), (3, false), (4, false), (5, true), (6, false)] {
            let (mut controller, libam, mut h) = fixture();
            libam.borrow_mut().process_state = state;
            request(&mut h, 1, 0);
            controller.accept_requests(&mut h.ctx());
            controller.update_server_data(&mut h.ctx());
            let response = h.server.network().response();
            assert_eq!(response.succeeded(), accepted, "sequence state {state}");
            assert_eq!(response.generation(), 1, "the rejection is acknowledged too (state {state})");
            assert_eq!(libam.borrow().begun, u32::from(accepted), "libam started iff accepted (state {state})");
        }
    }

    #[test]
    fn accepted_request_is_acknowledged_one_frame_after_the_accept_edge() {
        let (controller, _libam, mut h) = fixture();
        let mut slot = ControllerSlot::new(Box::new(controller));
        slot.pre_execute(&mut h.ctx());
        request(&mut h, 4, 0);
        slot.post_execute(&mut h.ctx());
        assert!(slot.controller().is_busy(&h.ctx()), "accepted but not yet acknowledged keeps the process alive");
        assert_eq!(h.server.network().response().generation(), 0);
        slot.pre_execute(&mut h.ctx());
        assert!(h.server.network().response().succeeded());
        assert_eq!(h.server.network().response().generation(), 4);
        assert!(!slot.controller().is_busy(&h.ctx()));
    }

    #[test]
    fn missing_libam_fails_the_request() {
        let (mut controller, libam, mut h) = fixture();
        libam.borrow_mut().handle_ok = false;
        request(&mut h, 1, 0);
        controller.accept_requests(&mut h.ctx());
        controller.update_server_data(&mut h.ctx());
        assert!(!h.server.network().response().succeeded());
        assert_eq!(libam.borrow().begun, 0);
    }

    #[test]
    fn ready_edge_samples_availability_and_checker_enablement_only() {
        let (mut controller, libam, mut h) = fixture();
        {
            let mut libam = libam.borrow_mut();
            libam.enabled = true;
            libam.enabled_items = [true, false, true, false, false, true, false, false, true];
        }
        seed_available(&mut h, true, AIR_SLOT, true);
        seed_state(&mut h, false, 3, NetworkTestState::Done);
        h.server.network_mut().property_mut().valid_mut().set(true).expect("a bool");
        controller.clear_server_data(&mut h.ctx());
        assert!(h.server.network().available());
        for power_on in [true, false] {
            let flags: Vec<bool> = (0..AIR_SLOT).map(|slot| item(&h, power_on, slot).0).collect();
            assert_eq!(flags, [true, false, true, false, false, true, false, false, true]);
        }
        assert!(item(&h, true, AIR_SLOT).0, "the Air slot is owned by the config observer");
        assert_eq!(item(&h, false, 3).1, NetworkTestState::Done, "state is not reset");
        assert!(h.server.network().property().valid(), "property is not reset");
    }

    #[test]
    fn availability_is_a_one_shot_snapshot() {
        let (mut controller, libam, mut h) = fixture();
        libam.borrow_mut().enabled = true;
        controller.clear_server_data(&mut h.ctx());
        libam.borrow_mut().enabled = false;
        controller.update_server_data(&mut h.ctx());
        assert!(h.server.network().available());
    }

    #[test]
    fn start_event_restarts_the_block_and_clears_the_shared_availability_bytes() {
        let (mut controller, libam, mut h) = fixture();
        seed_block(
            &mut h,
            false,
            &ServerTestPatch { completed: Some(true), hops: Some(7), ..ServerTestPatch::default() },
        );
        publish(&mut h, false, &FatalError::new(9, 0, TIME, "stale", 0));
        seed_available(&mut h, false, AIR_SLOT, true);
        seed_block(
            &mut h,
            true,
            &ServerTestPatch { completed: Some(true), hops: Some(5), ..ServerTestPatch::default() },
        );
        let shared = ServerNetworkPatch {
            lan_available: Some(true),
            location_lan_available: Some(true),
            wan_available: Some(true),
            ..ServerNetworkPatch::default()
        };
        h.server.network_mut().copy_from(&shared).expect("bool fields");
        deliver(&mut controller, &libam, &mut h, vec![event(false, TestItem::IpAddress, RUNNING, NO_RESULT)]);
        let net = h.server.network();
        assert!(net.test().running() && !net.test().completed());
        assert_eq!((net.test().hops(), net.test().error().number()), (0, 0));
        assert_eq!(item(&h, false, AIR_SLOT).1, NetworkTestState::Done);
        assert_eq!(item(&h, false, AIR_SLOT).2, NetworkTestResult::Good);
        assert_eq!(item(&h, false, 0).1, NetworkTestState::Running, "the item status is copied as usual");
        assert!(!net.lan_available() && !net.location_lan_available() && !net.wan_available());
        assert_eq!(
            (net.power_on_test().hops(), net.power_on_test().completed()),
            (5, true),
            "the other block is untouched"
        );
    }

    #[test]
    fn start_event_leaves_a_disabled_air_slot_idle() {
        let (mut controller, libam, mut h) = fixture();
        deliver(&mut controller, &libam, &mut h, vec![event(true, TestItem::IpAddress, RUNNING, NO_RESULT)]);
        let (_, state, result) = item(&h, true, AIR_SLOT);
        assert_eq!((state, result), (NetworkTestState::Idle, NetworkTestResult::None));
        assert!(h.server.network().power_on_test().running());
    }

    #[test]
    fn only_item_zero_running_without_result_restarts_a_test() {
        for (item, state, result) in [
            (TestItem::Gateway, RUNNING, NO_RESULT),
            (TestItem::IpAddress, RUNNING, GOOD),
            (TestItem::IpAddress, DONE, NO_RESULT),
            (TestItem::IpAddress, IDLE, NO_RESULT),
        ] {
            let (mut controller, libam, mut h) = fixture();
            seed_block(&mut h, false, &ServerTestPatch { hops: Some(3), ..ServerTestPatch::default() });
            deliver(&mut controller, &libam, &mut h, vec![event(false, item, state, result)]);
            assert!(!h.server.network().test().running(), "{item:?} {state} {result}");
            assert_eq!(h.server.network().test().hops(), 3, "{item:?} {state} {result}");
        }
    }

    #[test]
    fn final_event_completes_the_block_and_finishes_running_items() {
        let (mut controller, libam, mut h) = fixture();
        seed_block(&mut h, true, &ServerTestPatch { running: Some(true), ..ServerTestPatch::default() });
        seed_state(&mut h, true, 0, NetworkTestState::Done);
        seed_state(&mut h, true, 1, NetworkTestState::Running);
        seed_state(&mut h, true, AIR_SLOT, NetworkTestState::Running);
        seed_state(&mut h, false, 1, NetworkTestState::Running);
        deliver(&mut controller, &libam, &mut h, vec![event(true, TestItem::AllFinished, DONE, NOT_APPLICABLE)]);
        let net = h.server.network();
        assert!(!net.power_on_test().running() && net.power_on_test().completed());
        let states: Vec<_> = (0..TEST_ITEM_COUNT).map(|slot| item(&h, true, slot).1).collect();
        let (done, idle) = (NetworkTestState::Done, NetworkTestState::Idle);
        assert_eq!(
            states,
            [done, done, idle, idle, idle, idle, idle, idle, idle, done],
            "Running becomes Done, Idle stays Idle (including the Air slot)"
        );
        assert_eq!(item(&h, false, 1).1, NetworkTestState::Running, "the ordinary block is untouched");
    }

    #[test]
    fn item_status_follows_libam_numbers_and_ignores_unknown_ones() {
        let (mut controller, libam, mut h) = fixture();
        deliver(&mut controller, &libam, &mut h, vec![event(false, TestItem::Aime, DONE, BAD)]);
        let (_, state, result) = item(&h, false, 6);
        assert_eq!((state, result), (NetworkTestState::Done, NetworkTestResult::Bad));
        deliver(&mut controller, &libam, &mut h, vec![event(false, TestItem::Aime, 7, 9)]);
        let (_, state, result) = item(&h, false, 6);
        assert_eq!(
            (state, result),
            (NetworkTestState::Done, NetworkTestResult::Bad),
            "unknown libam numbers keep the previous published values"
        );
        deliver(&mut controller, &libam, &mut h, vec![event(false, TestItem::Aime, RUNNING, NO_RESULT)]);
        let (_, state, result) = item(&h, false, 6);
        assert_eq!((state, result), (NetworkTestState::Running, NetworkTestResult::None));
    }

    #[test]
    fn every_good_or_bad_event_replaces_the_error_and_other_results_keep_it() {
        let (mut controller, libam, mut h) = fixture();
        let failure = FatalError::new(7, 3, TIME, "link down", 1);
        let mut bad = event(false, TestItem::Gateway, DONE, BAD);
        bad.error = failure.clone();
        let before = error_bytes(&h, false);
        deliver(&mut controller, &libam, &mut h, vec![bad]);
        let published = error_bytes(&h, false);
        assert_ne!(published, before);
        assert_eq!(published, published_error(false, &[&failure]));
        assert_eq!(h.server.network().test().error().number(), 7);
        let message = h.server.network().test().error().message();
        assert_eq!(String::from_utf16_lossy(message.as_slice()), "link down");

        let mut not_applicable = event(false, TestItem::DnsLan, DONE, NOT_APPLICABLE);
        not_applicable.error = FatalError::new(8, 0, TIME, "other", 0);
        deliver(&mut controller, &libam, &mut h, vec![not_applicable]);
        assert_eq!(error_bytes(&h, false), published, "N/A events do not touch the error");
        assert_eq!(h.server.network().power_on_test().error().number(), 0, "the power-on block has its own error");

        deliver(&mut controller, &libam, &mut h, vec![event(false, TestItem::Aime, DONE, GOOD)]);
        assert_eq!(
            error_bytes(&h, false),
            published_error(false, &[&failure, &FatalError::default()]),
            "a Good event publishes its own (here empty) record over the previous one"
        );
        assert_eq!(h.server.network().test().error().number(), 0);
        assert!(h.server.network().test().error().message().is_empty());
    }

    #[test]
    fn item_zero_done_captures_the_property_and_latches_valid() {
        let (mut controller, libam, mut h) = fixture();
        libam.borrow_mut().property = Some(NetworkProperty {
            dhcp: true,
            address: 0x0100_A8C0,
            subnet_mask: 0x00FF_FFFF,
            gateway: 0x0100_A8C0,
            primary_dns: 0x0808_0808,
            secondary_dns: 0,
        });
        deliver(&mut controller, &libam, &mut h, vec![event(false, TestItem::Gateway, DONE, GOOD)]);
        assert!(!h.server.network().property().valid(), "only item 0 captures the property");
        deliver(&mut controller, &libam, &mut h, vec![event(true, TestItem::IpAddress, DONE, BAD)]);
        let property = h.server.network().property();
        assert!(property.valid() && property.dhcp_enabled());
        assert_eq!(
            [
                property.address(),
                property.subnet_mask(),
                property.gateway(),
                property.primary_dns(),
                property.secondary_dns()
            ],
            [0x0100_A8C0, 0x00FF_FFFF, 0x0100_A8C0, 0x0808_0808, 0],
            "raw libam words, no byte-order change"
        );
        libam.borrow_mut().property = None;
        deliver(&mut controller, &libam, &mut h, vec![event(false, TestItem::IpAddress, DONE, GOOD)]);
        assert!(h.server.network().property().valid(), "valid is never cleared and a missing property leaves it as is");
        assert_eq!(h.server.network().property().address(), 0x0100_A8C0);
    }

    #[test]
    fn hops_are_parsed_only_from_a_done_good_hops_item() {
        let (mut controller, libam, mut h) = fixture();
        let cases = [
            (hops_event(DONE, GOOD, "12"), 12),
            (hops_event(RUNNING, GOOD, "99"), 12),
            (hops_event(DONE, BAD, "99"), 12),
            (hops_event(DONE, GOOD, "0x1F"), 31),
            (hops_event(DONE, GOOD, "010"), 8),
            (hops_event(DONE, GOOD, "4 hops"), 0),
            (hops_event(DONE, GOOD, ""), 0),
            (hops_event(DONE, GOOD, "5"), 5),
        ];
        for (event, expected) in cases {
            deliver(&mut controller, &libam, &mut h, vec![event]);
            assert_eq!(h.server.network().test().hops(), expected);
        }
        let mut other_item = event(false, TestItem::LineType, DONE, GOOD);
        other_item.status.text = wide16("77");
        deliver(&mut controller, &libam, &mut h, vec![other_item]);
        assert_eq!(h.server.network().test().hops(), 5, "other items never touch the hop count");
    }

    #[test]
    fn availability_bytes_follow_the_done_events_of_items_0_3_and_5() {
        let (mut controller, libam, mut h) = fixture();
        {
            let mut libam = libam.borrow_mut();
            libam.lan = true;
            libam.location_lan = false;
            libam.wan = true;
        }
        let flags = |h: &Harness| {
            let net = h.server.network();
            [net.lan_available(), net.location_lan_available(), net.wan_available()]
        };
        deliver(&mut controller, &libam, &mut h, vec![event(false, TestItem::IpAddress, RUNNING, NO_RESULT)]);
        assert_eq!(flags(&h), [false, false, false], "not Done: nothing is sampled");
        deliver(&mut controller, &libam, &mut h, vec![event(false, TestItem::Gateway, DONE, GOOD)]);
        assert_eq!(flags(&h), [false, false, false]);
        deliver(&mut controller, &libam, &mut h, vec![event(false, TestItem::IpAddress, DONE, GOOD)]);
        assert_eq!(flags(&h), [true, false, false]);
        libam.borrow_mut().location_lan = true;
        deliver(&mut controller, &libam, &mut h, vec![event(false, TestItem::Hops, DONE, BAD)]);
        assert_eq!(flags(&h), [true, true, false]);
        deliver(&mut controller, &libam, &mut h, vec![event(true, TestItem::AllnetAuth, DONE, GOOD)]);
        assert_eq!(flags(&h), [true, true, true], "the bytes are shared by both tests");
    }

    #[test]
    fn events_are_applied_even_while_the_manager_is_not_ready() {
        let libam = Rc::new(RefCell::new(Libam::new()));
        let mut controller = NetworkController::new(Box::new(FakePort(Rc::clone(&libam))));
        let mut h = Harness::new();
        assert!(!controller.is_ready(&h.ctx()));
        deliver(&mut controller, &libam, &mut h, vec![event(false, TestItem::IpAddress, RUNNING, NO_RESULT)]);
        assert!(h.server.network().test().running());
        controller.on_libam_events(&mut h.ctx());
        assert!(h.server.network().test().running(), "drained events are not replayed");
    }

    #[test]
    fn process_config_publishes_the_air_flag_to_both_blocks() {
        let (mut controller, _libam, mut h) = fixture();
        let mut config = ProcessConfig::default();
        config.network.allnet_air_enable = true;
        controller.on_process_config(&config, &mut h.ctx());
        assert!(item(&h, true, AIR_SLOT).0);
        assert!(item(&h, false, AIR_SLOT).0);
        assert!(!item(&h, false, 0).0);
        controller.on_process_config(&ProcessConfig::default(), &mut h.ctx());
        assert!(!item(&h, false, AIR_SLOT).0);
    }
}
