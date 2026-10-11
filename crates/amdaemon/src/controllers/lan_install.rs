//! `amdaemon::server::LanInstallController` (vtable `0x1405D20F8`, ctor `0x14003A140`, `App+0xBF0`, size `0x28`).
//!
//! Schema block: `ServerData.lanInstall` (`+0x4BD8`, [`crate::schema::lan_install`]); no `ClientData` block and
//! no request executor. Evidence: `spec/controllers/network_install.md` §3, claims `network_install.laninstall.*`.
//!
//! The ready edge samples the libam LAN-install mode into `client`/`server`; `serverCount` is refreshed on
//! every ready frame; `exitNeeded` is raised by libam's NeedToExit reason 1 and never cleared.

use crate::schema::lan_install::ServerLanInstallPatch;
use libam_api::lan_install::{LanInstallMode, LanInstallPort};

use crate::controllers::controller::{Controller, ControllerId, Ctx};

/// Why a write to a scalar field cannot fail: an integer or `bool` value is always representable.
const SCALARS: &str = "scalar fields accept every value";

/// libam NeedToExit reason that LAN install raises when a new application was found (`0x140244780`).
pub const NEED_TO_EXIT_REASON: u32 = 1;

/// `amdaemon::server::LanInstallController`.
pub struct LanInstallController {
    port: Box<dyn LanInstallPort>,
}

impl std::fmt::Debug for LanInstallController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LanInstallController").finish_non_exhaustive()
    }
}

impl LanInstallController {
    /// Constructs the controller (ctor `0x14003A140`).
    pub fn new(port: Box<dyn LanInstallPort>) -> Self {
        Self { port }
    }

    /// Logs when libam is absent; `true` when the body may run.
    fn check_handle(&self) -> bool {
        let present = self.port.lan_install_handle().is_ok();
        if !present {
            tracing::warn!("libam is not ready");
        }
        present
    }
}

impl Controller for LanInstallController {
    fn id(&self) -> ControllerId {
        ControllerId::LanInstall
    }

    /// `0x14003A1F0`: samples mode and server count. `exitNeeded` is not touched.
    fn clear_server_data(&mut self, ctx: &mut Ctx<'_>) {
        if !self.check_handle() {
            return;
        }
        let mode = self.port.mode();
        let patch = ServerLanInstallPatch {
            client: Some(mode == LanInstallMode::Client),
            server: Some(mode == LanInstallMode::Server),
            server_count: Some(self.port.server_count()),
            ..ServerLanInstallPatch::default()
        };
        ctx.server.lan_install_mut().copy_from(&patch).expect(SCALARS);
    }

    /// `0x14003A310`: refreshes `serverCount` only.
    fn update_server_data(&mut self, ctx: &mut Ctx<'_>) {
        if self.check_handle() {
            ctx.server.lan_install_mut().server_count_mut().set(self.port.server_count()).expect(SCALARS);
        }
    }

    /// NeedToExit callback `0x14003A7A0` (registered for reason 1 at `0x14003A1A8`): "a new application was
    /// found". Raised level-triggered by libam, so it is idempotent; readiness is not consulted.
    fn on_need_to_exit(&mut self, reason: u32, ctx: &mut Ctx<'_>) {
        if reason == NEED_TO_EXIT_REASON {
            ctx.server.lan_install_mut().exit_needed_mut().set(true).expect(SCALARS);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use libam_api::result::{AmError, AmResult};

    use super::*;
    use crate::controllers::controller::ControllerSlot;
    use crate::controllers::controller::testing::Harness;

    struct Libam {
        handle_ok: bool,
        mode: LanInstallMode,
        server_count: i32,
    }

    struct FakePort(Rc<RefCell<Libam>>);

    impl LanInstallPort for FakePort {
        fn lan_install_handle(&self) -> AmResult<()> {
            if self.0.borrow().handle_ok { Ok(()) } else { Err(AmError::code(AmError::NOT_INITIALIZED)) }
        }
        fn mode(&self) -> LanInstallMode {
            self.0.borrow().mode
        }
        fn server_count(&self) -> i32 {
            self.0.borrow().server_count
        }
    }

    fn fixture(mode: LanInstallMode, server_count: i32) -> (LanInstallController, Rc<RefCell<Libam>>, Harness) {
        let libam = Rc::new(RefCell::new(Libam { handle_ok: true, mode, server_count }));
        (LanInstallController::new(Box::new(FakePort(Rc::clone(&libam)))), libam, Harness::running())
    }

    #[test]
    fn ready_edge_publishes_mode_as_client_or_server_flag() {
        for (mode, client, server) in [
            (LanInstallMode::Off, false, false),
            (LanInstallMode::Client, true, false),
            (LanInstallMode::Server, false, true),
        ] {
            let (mut controller, _libam, mut h) = fixture(mode, 3);
            let seed =
                ServerLanInstallPatch { client: Some(true), server: Some(true), ..ServerLanInstallPatch::default() };
            h.server.lan_install_mut().copy_from(&seed).expect(SCALARS);
            controller.clear_server_data(&mut h.ctx());
            let lan = h.server.lan_install();
            assert_eq!((lan.client(), lan.server()), (client, server), "{mode:?}");
            assert_eq!(lan.server_count(), 3, "{mode:?}");
        }
    }

    #[test]
    fn mode_is_sampled_once_but_server_count_follows_libam_every_frame() {
        let (controller, libam, mut h) = fixture(LanInstallMode::Client, -1);
        let mut slot = ControllerSlot::new(Box::new(controller));
        slot.pre_execute(&mut h.ctx());
        assert_eq!(h.server.lan_install().server_count(), -1);
        {
            let mut libam = libam.borrow_mut();
            libam.mode = LanInstallMode::Server;
            libam.server_count = 2;
        }
        slot.pre_execute(&mut h.ctx());
        let lan = h.server.lan_install();
        assert_eq!((lan.client(), lan.server()), (true, false));
        assert_eq!(lan.server_count(), 2);
    }

    #[test]
    fn clear_keeps_exit_needed_set() {
        let (mut controller, _libam, mut h) = fixture(LanInstallMode::Off, -1);
        controller.on_need_to_exit(1, &mut h.ctx());
        controller.clear_server_data(&mut h.ctx());
        assert!(h.server.lan_install().exit_needed());
    }

    #[test]
    fn only_need_to_exit_reason_1_sets_exit_needed() {
        for reason in [0, 2, 3, 99] {
            let (mut controller, _libam, mut h) = fixture(LanInstallMode::Off, -1);
            controller.on_need_to_exit(reason, &mut h.ctx());
            assert!(!h.server.lan_install().exit_needed(), "reason {reason}");
        }
        let (mut controller, _libam, mut h) = fixture(LanInstallMode::Off, -1);
        controller.on_need_to_exit(1, &mut h.ctx());
        assert!(h.server.lan_install().exit_needed());
        controller.on_need_to_exit(1, &mut h.ctx());
        assert!(h.server.lan_install().exit_needed(), "level-triggered: repeated calls are harmless");
    }

    #[test]
    fn need_to_exit_is_recorded_while_the_manager_is_not_ready() {
        let libam = Rc::new(RefCell::new(Libam { handle_ok: true, mode: LanInstallMode::Off, server_count: -1 }));
        let mut controller = LanInstallController::new(Box::new(FakePort(libam)));
        let mut h = Harness::new();
        assert!(!controller.is_ready(&h.ctx()));
        controller.on_need_to_exit(1, &mut h.ctx());
        assert!(h.server.lan_install().exit_needed());
    }
}
