//! `amdaemon::server::UsbDeviceController` (vtable `0x1405DEFA8`, ctor `0x1400480C0`, registry index 12).
//!
//! Forwards a client `Reconnect(VID, PID)` request to libam's USB device manager and publishes the
//! asynchronous completion (`reconnect.busy` / `reconnect.lastResult`) delivered by libam's
//! `CompleteTaskEventListener`. It also owns the typed "required USB device" configuration (`usb_device`
//! JSON section) that libam's validator currently rejects whenever it is non-empty.
//!
//! Evidence: `spec/controllers/can_boardio_usbdevice.md` §3 (claims `can_boardio_usbdevice.usbdevice.*`).
//! The executor checker is the constant `Succeeded` (`0x1400493C0`), so the wire ack means "libam accepted
//! the request", never "the device reconnected".

use std::fmt::Write as _;

use crate::executor::{RequestExecutor, RequestState};
use crate::manager::Manager;
use crate::schema::usb_device::{
    ClientUsbDeviceRef, ServerReconnectMut, ServerReconnectPatch, UsbDeviceReconnectResult, UsbDeviceTask,
};
use libam_api::result::{AmError, AmResult};
use libam_api::usb_device::{UsbDeviceAppConfig, UsbDeviceEvent, UsbDeviceId, UsbDevicePort, UsbRequired};

use crate::controllers::controller::{Controller, ControllerId, Ctx, commit_check};
use libam_api::config::AppConfig;

/// `amdaemon::server::UsbDeviceController` (object size `0x90`).
pub struct UsbDeviceController {
    port: Box<dyn UsbDevicePort>,
    /// `+0x28` `RequestExecutor<ServerResponse, ClientRequest>`.
    executor: RequestExecutor,
}

impl std::fmt::Debug for UsbDeviceController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UsbDeviceController").field("executor", &self.executor).finish_non_exhaustive()
    }
}

impl UsbDeviceController {
    /// Constructs the controller (`0x1400480C0`). The published block starts zeroed like the roots.
    pub fn new(port: Box<dyn UsbDevicePort>) -> Self {
        Self { port, executor: RequestExecutor::default() }
    }
}

impl Controller for UsbDeviceController {
    fn id(&self) -> ControllerId {
        ControllerId::UsbDevice
    }

    /// vf3 `0x140048260`: a reconnect request is in flight.
    fn is_busy(&self, ctx: &Ctx<'_>) -> bool {
        self.executor.is_busy(ctx.client.usb_device().generation(), ctx.server.usb_device().response().generation())
    }

    /// vf6 `0x140048290`: `checkResult` only (vf5 is the base no-op: nothing is published on the ready edge).
    fn update_server_data(&mut self, ctx: &mut Ctx<'_>) {
        let mut usb = ctx.server.usb_device_mut();
        // The checker is the constant lambda `0x1400493C0` returning `Succeeded`.
        let outcome =
            self.executor.check_result(ctx.client.usb_device().generation(), usb.response().generation(), || {
                RequestState::Succeeded
            });
        commit_check(outcome, &mut usb.response_mut());
    }

    /// vf7 `0x1400482A0`: accept edge of the reconnect executor.
    fn accept_requests(&mut self, ctx: &mut Ctx<'_>) {
        let request = ctx.client.usb_device();
        let response_generation = ctx.server.usb_device().response().generation();
        let port = &mut *self.port;
        let manager = &mut *ctx.manager;
        let server = &mut *ctx.server;
        self.executor.accept(request.generation(), response_generation, || {
            accept_reconnect(port, manager, &mut server.usb_device_mut().reconnect_mut(), &request)
        });
    }

    /// `UsbDeviceEventListener::vf1` (`0x140048080`) for every completion libam raised.
    fn on_libam_events(&mut self, ctx: &mut Ctx<'_>) {
        let mut usb = ctx.server.usb_device_mut();
        let mut reconnect = usb.reconnect_mut();
        for event in self.port.take_events() {
            match event {
                UsbDeviceEvent::ReconnectComplete { code } => {
                    let patch = ServerReconnectPatch {
                        busy: Some(false),
                        last_result: Some(UsbDeviceReconnectResult::from_completion_code(code).into()),
                    };
                    reconnect.copy_from(&patch).expect("scalar fields accept every value");
                }
            }
        }
    }

    /// AppConfig lambda `0x1400493D0`: hands the completion listener to libam (`AppConfig+0x2518`).
    fn on_app_config(&mut self, config: &mut AppConfig, _ctx: &mut Ctx<'_>) {
        config.usb_device.listener_installed = true;
    }
}

/// `UsbDeviceController::accept` (`0x140048300`).
fn accept_reconnect(
    port: &mut dyn UsbDevicePort,
    manager: &mut Manager,
    reconnect: &mut ServerReconnectMut<'_>,
    request: &ClientUsbDeviceRef<'_>,
) -> RequestState {
    if manager.debug_dump() {
        let text = trace_reconnect(request);
        tracing::info!("{text}");
    }
    if port.usbdevice_facade().is_err() {
        tracing::warn!("libam is not ready");
        return RequestState::Failed;
    }
    let result = match request.task_kind() {
        Some(UsbDeviceTask::Reconnect) => {
            // VID/PID are passed through unvalidated.
            let info = request.info();
            let result = port.usb_reconnect(info.vendor_id(), info.product_id());
            if result.is_ok() {
                reconnect.busy_mut().set(true).expect("bool accepts both values");
            }
            result
        }
        None => Err(AmError::code(AmError::UNKNOWN_TASK)),
    };
    if result.is_ok() { RequestState::Pending } else { RequestState::Failed }
}

/// Request trace `0x140048510`: `USB device - task: Reconnect, VID: 0x<hex4>, PID: 0x<hex4>` (uppercase
/// hex: the stream carries `std::hex` and `std::uppercase`). The ids are only printed for task 0.
///
/// Deviation: the original passes a null task name for unknown tasks into the stream insertion and
/// faults; here an unknown task prints an empty name.
fn trace_reconnect(request: &ClientUsbDeviceRef<'_>) -> String {
    let task = request.task_kind();
    let mut text = format!("USB device - task: {}", task.map_or("", UsbDeviceTask::name));
    if task.is_some() {
        let info = request.info();
        // Writing to a `String` cannot fail.
        let _ = write!(text, ", VID: 0x{:04X}, PID: 0x{:04X}", info.vendor_id(), info.product_id());
    }
    text
}

/// Error of the `usb_device` configuration section or of the libam validation of its conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum UsbDeviceConfigError {
    /// More than 9 `required` definitions (`0x1401D3BD0`).
    #[error("too many required definitions.")]
    TooManyRequired,
    /// A `required` element has an empty `id` array.
    #[error("id is empty.")]
    NoIds,
    /// A `required` element has more than 32 ids.
    #[error("too many ids.")]
    TooManyIds,
    /// An id does not match `([0-9a-fA-F]{1,4}):([0-9a-fA-F]{1,4})` (`0x1401D3770`).
    #[error("invalid id format.")]
    InvalidIdFormat,
    /// `count` is outside `1.0..=4294967295.0` (`0x1401D3DF0`).
    #[error("out of range.")]
    CountOutOfRange,
    /// libam's `AppConfigValidator` rule for `am::usbdevice::Config` (`0x1402AFF80`) rejects any non-empty
    /// list; libam setup fails with it.
    #[error("Unsupport check USB device count.")]
    UnsupportedRequired,
}

/// Parses `"VVVV:PPPP"`: two groups of one to four hexadecimal digits, matching the whole string.
pub fn parse_usb_device_id(text: &str) -> Result<UsbDeviceId, UsbDeviceConfigError> {
    fn group(digits: &str) -> Option<u16> {
        if (1..=4).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_hexdigit()) {
            u16::from_str_radix(digits, 16).ok()
        } else {
            None
        }
    }
    text.split_once(':')
        .and_then(|(vendor, product)| Some(UsbDeviceId { vendor_id: group(vendor)?, product_id: group(product)? }))
        .ok_or(UsbDeviceConfigError::InvalidIdFormat)
}

/// `process::config::UsbDeviceRequired` (`0x88` bytes): one required device set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbRequiredConfig {
    ids: Vec<UsbDeviceId>,
    count: u32,
}

impl UsbRequiredConfig {
    /// Maximum ids per set.
    pub const MAX_IDS: usize = 32;

    /// Validates one `{"id": [...], "count": N}` element. The ids are checked before the count (order
    /// between the two key validators is unresolved; it only matters when both are wrong).
    pub fn parse<S: AsRef<str>>(ids: &[S], count: f64) -> Result<Self, UsbDeviceConfigError> {
        if ids.is_empty() {
            return Err(UsbDeviceConfigError::NoIds);
        }
        if ids.len() > Self::MAX_IDS {
            return Err(UsbDeviceConfigError::TooManyIds);
        }
        let ids = ids.iter().map(|id| parse_usb_device_id(id.as_ref())).collect::<Result<Vec<_>, _>>()?;
        if !(1.0..=f64::from(u32::MAX)).contains(&count) {
            return Err(UsbDeviceConfigError::CountOutOfRange);
        }
        // The original truncates the validated double (`cvttsd2si`).
        Ok(Self { ids, count: count as u32 })
    }

    /// The `VID:PID` pairs of the set.
    pub fn ids(&self) -> &[UsbDeviceId] {
        &self.ids
    }

    /// How many devices of the set are required.
    pub fn count(&self) -> u32 {
        self.count
    }
}

/// `process::config::UsbDeviceConfig`: the parsed `usb_device` section (`required` may be absent).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsbDeviceProcessConfig {
    required: Vec<UsbRequiredConfig>,
}

impl UsbDeviceProcessConfig {
    /// Maximum `required` elements.
    pub const MAX_REQUIRED: usize = 9;

    /// Builds the section from validated elements; more than 9 are rejected.
    pub fn new(required: Vec<UsbRequiredConfig>) -> Result<Self, UsbDeviceConfigError> {
        if required.len() > Self::MAX_REQUIRED {
            return Err(UsbDeviceConfigError::TooManyRequired);
        }
        Ok(Self { required })
    }

    /// The required sets.
    pub fn required(&self) -> &[UsbRequiredConfig] {
        &self.required
    }
}

/// `UsbDeviceConverter::doExecute` (`0x140011D30`): converts the process section. Fails (code 2) only
/// if a count limit is exceeded, which a validated section never does.
pub fn convert_process_config(config: &UsbDeviceProcessConfig) -> AmResult<UsbDeviceAppConfig> {
    let mut app = UsbDeviceAppConfig::default();
    for set in config.required() {
        let mut required = UsbRequired::default();
        for id in set.ids() {
            required.add_pair(id.vendor_id, id.product_id)?;
        }
        required.set_count(set.count());
        app.add_required(required)?;
    }
    Ok(app)
}

/// libam's validator for `am::usbdevice::Config` (`0x1402AFF80`): only an empty list is valid.
pub fn validate_app_config(config: &UsbDeviceAppConfig) -> Result<(), UsbDeviceConfigError> {
    if config.is_valid() { Ok(()) } else { Err(UsbDeviceConfigError::UnsupportedRequired) }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use libam_api::sequence::ProcessState;

    use super::*;
    use crate::controllers::controller::ControllerSlot;
    use crate::controllers::controller::testing::Harness;
    use crate::schema::usb_device::{ClientReconnectInfoPatch, ClientUsbDevicePatch, ServerUsbDevice};

    #[derive(Debug, Default)]
    struct Usb {
        /// libam task slot (`mgr+0x65C`): a second reconnect while set is refused ("busy. Task = 1.").
        active: bool,
        reconnects: Vec<(u16, u16)>,
        events: Vec<UsbDeviceEvent>,
    }

    #[derive(Debug, Clone, Default)]
    struct FakePort(Rc<RefCell<Usb>>);

    impl UsbDevicePort for FakePort {
        fn usbdevice_facade(&self) -> AmResult<()> {
            Ok(())
        }
        fn usb_reconnect(&mut self, vendor_id: u16, product_id: u16) -> AmResult<()> {
            let mut usb = self.0.borrow_mut();
            usb.reconnects.push((vendor_id, product_id));
            if usb.active {
                return Err(AmError::code(AmError::NOT_INITIALIZED));
            }
            usb.active = true;
            Ok(())
        }
        fn take_events(&mut self) -> Vec<UsbDeviceEvent> {
            std::mem::take(&mut self.0.borrow_mut().events)
        }
    }

    struct Rig {
        harness: Harness,
        slot: ControllerSlot,
        usb: Rc<RefCell<Usb>>,
    }

    impl Rig {
        fn new() -> Self {
            let port = FakePort::default();
            let usb = Rc::clone(&port.0);
            let mut rig = Self {
                harness: Harness::running(),
                slot: ControllerSlot::new(Box::new(UsbDeviceController::new(Box::new(port)))),
                usb,
            };
            rig.tick();
            rig
        }

        /// One App frame: libam events, `preExecute`, `postExecute`.
        fn tick(&mut self) {
            let mut ctx = self.harness.ctx();
            self.slot.controller_mut().on_libam_events(&mut ctx);
            self.slot.pre_execute(&mut ctx);
            self.slot.post_execute(&mut ctx);
        }

        fn request(&mut self, generation: u32, task: u32, vid: u16, pid: u16) {
            let patch = ClientUsbDevicePatch {
                generation: Some(generation),
                task: Some(task),
                info: Some(ClientReconnectInfoPatch { vendor_id: Some(vid), product_id: Some(pid) }),
            };
            self.harness.client.usb_device_mut().copy_from(&patch).expect("plain fields");
        }

        fn reconnect(&mut self, generation: u32, vid: u16, pid: u16) {
            self.request(generation, 0, vid, pid);
            self.tick(); // accepted in postExecute
            self.tick(); // acknowledged in the next preExecute
        }

        fn complete(&mut self, code: i32) {
            let mut usb = self.usb.borrow_mut();
            usb.active = false;
            usb.events.push(UsbDeviceEvent::ReconnectComplete { code });
        }

        fn block(&self) -> ServerUsbDevice {
            self.harness.server.usb_device().copy_into()
        }
    }

    #[test]
    fn reconnect_is_handed_to_libam_and_acknowledged_one_frame_later() {
        let mut rig = Rig::new();
        rig.request(1, 0, 0x04CA, 0x0123);

        let mut ctx = rig.harness.ctx();
        rig.slot.pre_execute(&mut ctx);
        assert_eq!(ctx.server.usb_device().response().generation(), 0);
        rig.slot.post_execute(&mut ctx);
        assert!(ctx.server.usb_device().reconnect().busy(), "busy is set at accept time");
        assert_eq!(ctx.server.usb_device().response().generation(), 0, "not acknowledged yet");
        assert!(rig.slot.controller().is_busy(&ctx));
        rig.slot.pre_execute(&mut ctx);
        assert_eq!(ctx.server.usb_device().response().generation(), 1);
        assert!(ctx.server.usb_device().response().succeeded());
        assert!(!rig.slot.controller().is_busy(&ctx));
        assert_eq!(rig.usb.borrow().reconnects, [(0x04CA, 0x0123)]);
    }

    #[test]
    fn second_reconnect_while_one_is_active_fails_and_keeps_busy() {
        let mut rig = Rig::new();
        rig.reconnect(1, 1, 1);
        rig.reconnect(2, 2, 2);
        let block = rig.block();
        assert_eq!(block.response.generation, 2);
        assert!(!block.response.succeeded);
        assert!(block.reconnect.busy, "first reconnect still pending");
    }

    #[test]
    fn unknown_task_fails_without_calling_libam() {
        let mut rig = Rig::new();
        rig.request(1, 1, 0x04CA, 0x0123);
        rig.tick();
        rig.tick();
        let block = rig.block();
        assert_eq!(block.response.generation, 1);
        assert!(!block.response.succeeded);
        assert!(rig.usb.borrow().reconnects.is_empty());
    }

    #[test]
    fn vid_and_pid_are_passed_through_unvalidated() {
        let mut rig = Rig::new();
        rig.reconnect(1, 0, 0);
        assert_eq!(rig.usb.borrow().reconnects, [(0, 0)]);
        assert!(rig.block().response.succeeded);
    }

    #[test]
    fn completion_events_clear_busy_and_publish_the_mapped_result() {
        for (code, expected) in [
            (1, UsbDeviceReconnectResult::Success),
            (2, UsbDeviceReconnectResult::Failure),
            (3, UsbDeviceReconnectResult::DriverError),
            (0, UsbDeviceReconnectResult::None),
            (7, UsbDeviceReconnectResult::None),
            (-1, UsbDeviceReconnectResult::None),
        ] {
            let mut rig = Rig::new();
            rig.reconnect(1, 1, 2);
            assert!(rig.block().reconnect.busy);
            rig.complete(code);
            rig.tick();
            let block = rig.block();
            assert!(!block.reconnect.busy, "code {code}");
            assert_eq!(block.reconnect.last_result, expected, "code {code}");
        }
    }

    #[test]
    fn last_result_survives_a_new_accepted_reconnect() {
        let mut rig = Rig::new();
        rig.reconnect(1, 1, 2);
        rig.complete(3);
        rig.tick();
        rig.reconnect(2, 1, 2);
        let block = rig.block();
        assert!(block.reconnect.busy);
        assert_eq!(block.reconnect.last_result, UsbDeviceReconnectResult::DriverError);
        rig.complete(1);
        rig.tick();
        assert_eq!(rig.block().reconnect.last_result, UsbDeviceReconnectResult::Success);
    }

    #[test]
    fn completion_events_are_applied_even_when_the_controller_is_not_ready() {
        let mut rig = Rig::new();
        rig.reconnect(1, 1, 2);
        rig.harness.manager.on_change_state(ProcessState(0));
        rig.complete(2);
        let mut ctx = rig.harness.ctx();
        rig.slot.controller_mut().on_libam_events(&mut ctx);
        assert!(!ctx.server.usb_device().reconnect().busy());
        assert_eq!(ctx.server.usb_device().reconnect().last_result(), UsbDeviceReconnectResult::Failure);
    }

    #[test]
    fn required_set_validation_follows_the_validator_limits() {
        assert_eq!(UsbRequiredConfig::parse::<&str>(&[], 1.0), Err(UsbDeviceConfigError::NoIds));
        let ids: Vec<String> = (0..33).map(|i| format!("{i:x}:1")).collect();
        assert_eq!(UsbRequiredConfig::parse(&ids, 1.0), Err(UsbDeviceConfigError::TooManyIds));
        assert!(UsbRequiredConfig::parse(&ids[..32], 1.0).is_ok());
        assert_eq!(UsbRequiredConfig::parse(&["1:1", "zz"], 1.0), Err(UsbDeviceConfigError::InvalidIdFormat));
        for count in [0.0, 0.99, -1.0, 4_294_967_296.0] {
            assert_eq!(
                UsbRequiredConfig::parse(&["1:1"], count),
                Err(UsbDeviceConfigError::CountOutOfRange),
                "{count}"
            );
        }
        assert_eq!(UsbRequiredConfig::parse(&["1:1"], 1.0).map(|r| r.count()), Ok(1));
        assert_eq!(UsbRequiredConfig::parse(&["1:1"], 4_294_967_295.0).map(|r| r.count()), Ok(u32::MAX));
        assert_eq!(UsbRequiredConfig::parse(&["1:1"], 2.9).map(|r| r.count()), Ok(2), "fraction truncates");
    }

    #[test]
    fn libam_accepts_only_an_empty_required_list() {
        let empty = convert_process_config(&UsbDeviceProcessConfig::default()).expect("converts");
        assert_eq!(validate_app_config(&empty), Ok(()));
        let process =
            UsbDeviceProcessConfig::new(vec![UsbRequiredConfig::parse(&["1:1"], 1.0).expect("valid")]).expect("valid");
        let app = convert_process_config(&process).expect("converts");
        assert_eq!(validate_app_config(&app), Err(UsbDeviceConfigError::UnsupportedRequired));
        assert_eq!(UsbDeviceConfigError::UnsupportedRequired.to_string(), "Unsupport check USB device count.");
    }
}
