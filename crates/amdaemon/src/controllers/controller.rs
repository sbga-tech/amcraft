//! `amdaemon::server::Controller` (vtable `0x1405CA408`) and the per-tick context.
//!
//! Original object: `{vptr, ServerData*, ClientData const*, Manager*, bool ready}` (`0x28` bytes, base ctor
//! `0x14002B0F0`). Controllers only borrow the roots and the Manager; here the borrows are explicit in
//! [`Ctx`] and the `ready` byte lives in [`ControllerSlot`].

use crate::executor::{CheckOutcome, Completion};
use crate::manager::Manager;
use crate::schema::image::{ClientImage, ServerImage, ServerIoImage};
use crate::schema::shared::ResponseMut;

use libam_api::config::{AppConfig, ProcessConfig};
use libam_api::error::FatalError;

/// Borrowed state every controller hook may touch.
#[derive(Debug)]
pub struct Ctx<'a> {
    /// Live `ServerData` (`Server::Impl+0x8`).
    pub server: &'a mut ServerImage,
    /// Live `ClientData` (`Server::Impl+0x1A5F0`), read-only for controllers.
    pub client: &'a ClientImage,
    /// ServerIO body (`*(Server::Impl+0x36690)+8`): written by JVS and USBIO only.
    pub server_io: &'a mut ServerIoImage,
    /// `arcade::Manager` runtime state.
    pub manager: &'a mut Manager,
}

/// Registry position (App construction order, `App::App` `0x140001000`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ControllerId {
    /// `App+0x488`.
    Process,
    /// `App+0x508`.
    Common,
    /// `App+0x588`.
    SystemInfo,
    /// `App+0x5B0`.
    Error,
    /// `App+0x668`.
    Sequence,
    /// `App+0x6E8`.
    Jvs,
    /// `App+0x738`.
    Can,
    /// `App+0x7D0`.
    UsbIo,
    /// `App+0x840`.
    Input,
    /// `App+0x870`.
    Output,
    /// `App+0x8A0`.
    Backup,
    /// `App+0x958`.
    BoardIo,
    /// `App+0x9E0`.
    UsbDevice,
    /// `App+0xA70`.
    AppImage,
    /// `App+0xA98`.
    Credit,
    /// `App+0xB58`.
    Network,
    /// `App+0xBF0`.
    LanInstall,
    /// `App+0xC18`.
    NetDelivery,
    /// `App+0xCD8`.
    AllnetAuth,
    /// `App+0xD00`.
    AllnetAccounting,
    /// `App+0xD80`.
    AllnetWiFi,
    /// `App+0xDA8`.
    AllnetAir,
    /// `App+0xE28`.
    Aime,
    /// `App+0xF30`.
    EMoney,
    /// `App+0x1040`.
    AimePay,
    /// `App+0x10F0`.
    AbaasLog,
    /// `App+0x1170`.
    Apm,
}

impl ControllerId {
    /// All controllers in registry (construction, tick and busy-scan) order.
    pub const ORDER: [Self; 27] = [
        Self::Process,
        Self::Common,
        Self::SystemInfo,
        Self::Error,
        Self::Sequence,
        Self::Jvs,
        Self::Can,
        Self::UsbIo,
        Self::Input,
        Self::Output,
        Self::Backup,
        Self::BoardIo,
        Self::UsbDevice,
        Self::AppImage,
        Self::Credit,
        Self::Network,
        Self::LanInstall,
        Self::NetDelivery,
        Self::AllnetAuth,
        Self::AllnetAccounting,
        Self::AllnetWiFi,
        Self::AllnetAir,
        Self::Aime,
        Self::EMoney,
        Self::AimePay,
        Self::AbaasLog,
        Self::Apm,
    ];
}

/// Virtual interface of `amdaemon::server::Controller` plus the Manager event-hub registrations.
///
/// Slot mapping: vf3 [`Self::is_busy`], vf4 [`Self::is_ready`], vf5 [`Self::clear_server_data`],
/// vf6 [`Self::update_server_data`], vf7 [`Self::accept_requests`]. vf1/vf2 are fixed in
/// [`ControllerSlot`]. Hub hooks are invoked for every controller in registry order, which equals the
/// registration order recovered for every hub vector (`spec/controllers/lifecycle.md` §M7.2); controllers
/// that did not register keep the no-op default.
pub trait Controller: std::fmt::Debug {
    /// Registry identity.
    fn id(&self) -> ControllerId;

    /// vf3 `isBusy` (default `0x14002B1C0`: false). Keeps the App loop alive while the Manager is idle or
    /// terminating.
    fn is_busy(&self, ctx: &Ctx<'_>) -> bool {
        let _ = ctx;
        false
    }

    /// vf4 `isReady` (default `0x14002B210`: Manager state ∈ {Running, Terminating}).
    fn is_ready(&self, ctx: &Ctx<'_>) -> bool {
        ctx.manager.controllers_ready()
    }

    /// vf5: runs on the not-ready → ready edge, before [`Self::update_server_data`].
    fn clear_server_data(&mut self, ctx: &mut Ctx<'_>) {
        let _ = ctx;
    }

    /// vf6: runs every tick while ready and once more on the ready → not-ready edge.
    fn update_server_data(&mut self, ctx: &mut Ctx<'_>) {
        let _ = ctx;
    }

    /// vf7: runs after `Server::execute` while ready and the client echoes the boot clock.
    fn accept_requests(&mut self, ctx: &mut Ctx<'_>) {
        let _ = ctx;
    }

    /// Delivers libam listener callbacks registered directly with libam by this controller (they fire
    /// inside `Manager::execute`, before any `preExecute`).
    fn on_libam_events(&mut self, ctx: &mut Ctx<'_>) {
        let _ = ctx;
    }

    /// `configCbs` (`EventListener+0xB0`): process configuration accepted at the cabinet-type event.
    fn on_process_config(&mut self, config: &ProcessConfig, ctx: &mut Ctx<'_>) {
        let _ = (config, ctx);
    }

    /// `appConfigCbs` (`EventListener+0x98`): libam `AppConfig` stage (listener installation).
    fn on_app_config(&mut self, config: &mut AppConfig, ctx: &mut Ctx<'_>) {
        let _ = (config, ctx);
    }

    /// `displayDoneCbs` (`EventListener+0x1C0`): `am::display::CompleteTask`.
    fn on_display_complete(&mut self, ctx: &mut Ctx<'_>) {
        let _ = ctx;
    }

    /// `jvsDetectCbs` (`EventListener+0x188`): `am::jvs::DetectNode`.
    fn on_jvs_detect_node(&mut self, ctx: &mut Ctx<'_>) {
        let _ = ctx;
    }

    /// `UsbIOEventListener` vector (`EventListener+0x1A8`): `am::usbio::DetectNode`.
    fn on_usb_io_detect_node(&mut self, ctx: &mut Ctx<'_>) {
        let _ = ctx;
    }

    /// `needToExit[reason]` (`EventListener+0x128`): only reasons 1 (LAN install) and 2 (NetDelivery) have
    /// registrants.
    fn on_need_to_exit(&mut self, reason: u32, ctx: &mut Ctx<'_>) {
        let _ = (reason, ctx);
    }

    /// `fatalErrorCb` (`EventListener+0x1F8`): `am::error::OccurFatalError`.
    fn on_fatal_error(&mut self, error: &FatalError, ctx: &mut Ctx<'_>) {
        let _ = (error, ctx);
    }

    /// `apmCbs` (`EventListener+0x238`): application-state broadcast (`0x140006130`).
    fn on_app_state(&mut self, state: u32, ctx: &mut Ctx<'_>) {
        let _ = (state, ctx);
    }
}

/// `RequestExecutor<..>::checkResult` epilogue: the completion to write into the response, `None` while
/// nothing finished.
///
/// A checker that answered `Idle` for a request still pending is logged as a warning naming the caller; the
/// request then completes as failed. Callers whose response carries a `succeeded` byte use [`commit_check`];
/// this form serves the generation-only responses (BoardIO LED, JVS).
#[track_caller]
pub fn settle_check(outcome: CheckOutcome) -> Option<Completion> {
    match outcome {
        CheckOutcome::Idle | CheckOutcome::Pending => None,
        CheckOutcome::Completed(completion) => Some(completion),
        CheckOutcome::CompletedAfterInvalidCheck(completion) => {
            tracing::warn!(caller = %std::panic::Location::caller(), "request checker answered idle for a pending request");
            Some(completion)
        }
    }
}

/// [`settle_check`], then [`ResponseMut::complete`]: the whole `checkResult` epilogue for responses that carry a
/// `succeeded` byte.
#[track_caller]
pub fn commit_check(outcome: CheckOutcome, response: &mut ResponseMut<'_>) {
    if let Some(completion) = settle_check(outcome) {
        response.complete(completion);
    }
}

/// A registered controller with the base-class `ready` byte (`Controller+0x20`).
#[derive(Debug)]
pub struct ControllerSlot {
    controller: Box<dyn Controller>,
    ready: bool,
}

impl ControllerSlot {
    /// Wraps a controller (`ready` starts false).
    pub fn new(controller: Box<dyn Controller>) -> Self {
        Self { controller, ready: false }
    }

    /// The wrapped controller.
    pub fn controller(&self) -> &dyn Controller {
        self.controller.as_ref()
    }

    /// The wrapped controller, mutably.
    pub fn controller_mut(&mut self) -> &mut dyn Controller {
        self.controller.as_mut()
    }

    /// Stored readiness.
    pub fn is_ready(&self) -> bool {
        self.ready
    }

    /// vf1 `preExecute` (`0x14002B140`).
    pub fn pre_execute(&mut self, ctx: &mut Ctx<'_>) {
        let previous = self.ready;
        self.ready = self.controller.is_ready(ctx);
        if self.ready && !previous {
            self.controller.clear_server_data(ctx);
        }
        if self.ready || previous {
            self.controller.update_server_data(ctx);
        }
    }

    /// vf2 `postExecute` (`0x14002B190`): requests are accepted only while ready and once the client has
    /// echoed this boot's clock (`ServerData+0x38C8 == ClientData+0x1A50`).
    pub fn post_execute(&mut self, ctx: &mut Ctx<'_>) {
        if self.ready && ctx.server.process().boot_clock() == ctx.client.process().server_boot_clock() {
            self.controller.accept_requests(ctx);
        }
    }
}

/// Test harness shared by every controller module's unit tests.
#[cfg(test)]
pub(crate) mod testing {
    use crate::manager::Manager;
    use crate::schema::image::{ClientImage, ServerImage, ServerIoImage};
    use libam_api::sequence::ProcessState;
    use libam_api::time::DateTime;

    use super::Ctx;

    /// Zeroed roots plus an idle Manager.
    #[derive(Debug)]
    pub(crate) struct Harness {
        pub(crate) server: ServerImage,
        pub(crate) client: ClientImage,
        pub(crate) server_io: ServerIoImage,
        pub(crate) manager: Manager,
    }

    impl Harness {
        /// Idle Manager (state 0), zeroed roots.
        pub(crate) fn new() -> Self {
            Self {
                server: ServerImage::new(),
                client: ClientImage::new(),
                server_io: ServerIoImage::new(),
                manager: Manager::new(),
            }
        }

        /// Manager in `Running` (libam ProcessState 3), so default `isReady` is true.
        pub(crate) fn running() -> Self {
            let mut harness = Self::new();
            harness.manager.on_change_state(ProcessState(3));
            harness
        }

        /// Borrowed context.
        pub(crate) fn ctx(&mut self) -> Ctx<'_> {
            Ctx {
                server: &mut self.server,
                client: &self.client,
                server_io: &mut self.server_io,
                manager: &mut self.manager,
            }
        }
    }

    /// `DateTime::make` of a valid civil time without sub-seconds, for expected values.
    pub(crate) fn civil(year: i32, month: i32, day: i32, hour: i32, minute: i32, second: i32) -> DateTime {
        DateTime::make(year, month, day, hour, minute, second, 0).expect("valid civil time")
    }

    /// `items` as a fixed libam buffer: zero-padded when shorter than `N`, cut at `N` otherwise. A text that
    /// reaches `N` fills the array without a terminator, a shape the controllers must accept from libam.
    pub(crate) fn buffer<T: Copy + Default, const N: usize>(items: &[T]) -> [T; N] {
        let mut buffer = [T::default(); N];
        let len = items.len().min(N);
        buffer[..len].copy_from_slice(&items[..len]);
        buffer
    }
}

#[cfg(test)]
mod tests {
    use super::testing::Harness;
    use super::*;

    #[test]
    fn commit_check_completes_an_idle_checked_request_as_failed() {
        let mut h = Harness::new();
        let mut block = h.server.process_mut();
        let mut response = block.response_mut();
        response.complete(Completion { generation: 2, succeeded: Some(true) });
        let completion = Completion { generation: 4, succeeded: Some(false) };
        commit_check(CheckOutcome::CompletedAfterInvalidCheck(completion), &mut response);
        let response = h.server.process().response();
        assert_eq!((response.generation(), response.succeeded()), (4, false));
    }
}
