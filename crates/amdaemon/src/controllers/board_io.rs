//! `amdaemon::server::BoardIoController` (vtable `0x1405C82E8`, ctor `0x1400243A0`, registry index 11).
//!
//! Publishes the cabinet DIP switches (latched once per process) and the push switches (every update) and
//! applies the two tri-state LED commands of the single "LED" request to the libam SystemIo LED word.
//!
//! Evidence: `spec/controllers/can_boardio_usbdevice.md` §1 (claims `can_boardio_usbdevice.boardio.*`).
//! The executor response is generation-only (`checkResult` variant B `0x1400255E0`): there is no
//! `succeeded` byte, so a refused request (libam absent) and an accepted one look identical on the wire.

use crate::executor::{RequestExecutor, RequestState};
use crate::manager::Manager;
use crate::schema::board_io::{ClientLedRequestRef, LedCommand, ServerBoardIoPatch};
use libam_api::board_io::BoardIoPort;

use crate::controllers::controller::{Controller, ControllerId, Ctx, settle_check};

/// `amdaemon::server::BoardIoController` (object size `0x88`).
pub struct BoardIoController {
    port: Box<dyn BoardIoPort>,
    /// `+0x28`: set by the first successful `clearServerData`, never cleared.
    dip_latched: bool,
    /// `+0x30` `RequestExecutor<ServerLedResponse, ClientLedRequest>`.
    executor: RequestExecutor,
}

impl std::fmt::Debug for BoardIoController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoardIoController")
            .field("dip_latched", &self.dip_latched)
            .field("executor", &self.executor)
            .finish_non_exhaustive()
    }
}

impl BoardIoController {
    /// Constructs the controller (`0x1400243A0`).
    pub fn new(port: Box<dyn BoardIoPort>) -> Self {
        Self { port, dip_latched: false, executor: RequestExecutor::default() }
    }
}

impl Controller for BoardIoController {
    fn id(&self) -> ControllerId {
        ControllerId::BoardIo
    }

    /// vf3 `0x1400244A0`: an LED request is in flight.
    fn is_busy(&self, ctx: &Ctx<'_>) -> bool {
        let request = ctx.client.board_io().led_request();
        self.executor.is_busy(request.generation(), ctx.server.board_io().led_response().generation())
    }

    /// vf5 `0x1400244D0`: latches the DIP switches once per process.
    fn clear_server_data(&mut self, ctx: &mut Ctx<'_>) {
        if self.port.systemio_facade().is_err() {
            tracing::warn!("libam is not ready");
            return;
        }
        if !self.dip_latched {
            // Both published bytes are loaded from the same libam address (`0x140213E30`/`0x140213E40`).
            let dip = self.port.dip() as u8;
            let patch = ServerBoardIoPatch {
                dip_switch_bits: Some(dip),
                dip_switch_app_value: Some(dip),
                ..ServerBoardIoPatch::default()
            };
            ctx.server.board_io_mut().copy_from(&patch).expect("scalar fields accept every value");
            self.dip_latched = true;
        }
    }

    /// vf6 `0x140024600`: refreshes the push switches, then `checkResult` (skipped without libam).
    fn update_server_data(&mut self, ctx: &mut Ctx<'_>) {
        if self.port.systemio_facade().is_err() {
            tracing::warn!("libam is not ready");
            return;
        }
        let mut board_io = ctx.server.board_io_mut();
        board_io.push_switch_bits_mut().set(self.port.push() as u8).expect("a scalar accepts every value");

        // The checker is the constant lambda `0x1400253F0` returning `Succeeded`.
        let mut checker = || RequestState::Succeeded;
        let outcome = self.executor.check_result_generation_only(
            ctx.client.board_io().led_request().generation(),
            board_io.led_response().generation(),
            Some(&mut checker),
        );
        if let Some(completion) = settle_check(outcome) {
            board_io
                .led_response_mut()
                .generation_mut()
                .set(completion.generation)
                .expect("a scalar accepts every value");
        }
    }

    /// vf7 `0x140024710`: accept edge of the LED executor.
    fn accept_requests(&mut self, ctx: &mut Ctx<'_>) {
        let client = ctx.client.board_io();
        let request = client.led_request();
        let port = &mut *self.port;
        let manager = &mut *ctx.manager;
        self.executor.accept(request.generation(), ctx.server.board_io().led_response().generation(), || {
            accept_led(port, manager, &request)
        });
    }
}

/// `BoardIoController::accept` (`0x140024780`): applies the two LED commands to the libam LED word.
fn accept_led(port: &mut dyn BoardIoPort, manager: &mut Manager, request: &ClientLedRequestRef<'_>) -> RequestState {
    if manager.debug_dump() {
        let text = trace_led(request);
        tracing::info!("{text}");
    }
    if port.systemio_facade().is_err() {
        tracing::warn!("libam is not ready");
        return RequestState::Failed;
    }
    let mut led = port.led();
    for (bit, &raw) in (0u32..).zip(request.commands()) {
        if let Ok(command) = LedCommand::try_from(raw) {
            led = command.apply(led, bit);
        }
    }
    // The word is written back (and marked valid) even when no command changed a bit.
    port.set_led(led);
    RequestState::Pending
}

/// Request trace `0x140024970`: `Board I/O - task: LED, <name(cmd0)>, <name(cmd1)>`.
///
/// Deviation: the original passes a null name for command bytes above 2 into the stream insertion and
/// faults; here such bytes print an empty name.
fn trace_led(request: &ClientLedRequestRef<'_>) -> String {
    let name = |index| request.command(index).map_or("", LedCommand::name);
    format!("Board I/O - task: LED, {}, {}", name(0), name(1))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use crate::schema::board_io::ClientLedRequestPatch;
    use libam_api::result::{AmError, AmResult};
    use libam_api::sequence::ProcessState;

    use super::*;
    use crate::controllers::controller::ControllerSlot;
    use crate::controllers::controller::testing::Harness;

    #[derive(Debug, Default)]
    struct SystemIo {
        absent: bool,
        dip: u64,
        push: u64,
        led: u64,
        led_valid: bool,
        led_writes: u32,
    }

    #[derive(Debug, Clone, Default)]
    struct FakePort(Rc<RefCell<SystemIo>>);

    impl BoardIoPort for FakePort {
        fn systemio_facade(&self) -> AmResult<()> {
            if self.0.borrow().absent { Err(AmError::code(AmError::NOT_INITIALIZED)) } else { Ok(()) }
        }
        fn dip(&self) -> u64 {
            self.0.borrow().dip
        }
        fn push(&self) -> u64 {
            self.0.borrow().push
        }
        fn led(&self) -> u64 {
            self.0.borrow().led
        }
        fn set_led(&mut self, value: u64) {
            let mut io = self.0.borrow_mut();
            io.led = value;
            io.led_valid = true;
            io.led_writes += 1;
        }
    }

    struct Rig {
        harness: Harness,
        slot: ControllerSlot,
        io: Rc<RefCell<SystemIo>>,
    }

    impl Rig {
        fn new() -> Self {
            let port = FakePort::default();
            let io = Rc::clone(&port.0);
            Self {
                harness: Harness::running(),
                slot: ControllerSlot::new(Box::new(BoardIoController::new(Box::new(port)))),
                io,
            }
        }

        /// One App frame: `preExecute` then `postExecute`.
        fn tick(&mut self) {
            let mut ctx = self.harness.ctx();
            self.slot.pre_execute(&mut ctx);
            self.slot.post_execute(&mut ctx);
        }

        fn request_led(&mut self, generation: u32, commands: [u8; 2]) {
            let patch = ClientLedRequestPatch { generation: Some(generation), commands: Some(&commands) };
            self.harness.client.board_io_mut().led_request_mut().copy_from(&patch).expect("plain fields");
        }

        /// Issues request `generation` and runs the two frames that accept and acknowledge it.
        fn send_led(&mut self, generation: u32, commands: [u8; 2]) {
            self.request_led(generation, commands);
            self.tick();
            self.tick();
            let acknowledged = self.harness.server.board_io().led_response().generation();
            assert_eq!(acknowledged, generation, "request acknowledged");
        }

        fn busy(&mut self) -> bool {
            let ctx = self.harness.ctx();
            self.slot.controller().is_busy(&ctx)
        }
    }

    #[test]
    fn led_commands_set_and_clear_only_the_low_two_bits() {
        let mut rig = Rig::new();
        rig.io.borrow_mut().led = 0b1111_0010;
        rig.tick();
        rig.send_led(1, [1, 2]);
        assert_eq!(rig.io.borrow().led, 0b1111_0001, "bit0 set, bit1 cleared, bits 2.. untouched");

        rig.send_led(2, [2, 1]);
        assert_eq!(rig.io.borrow().led, 0b1111_0010, "bit0 cleared, bit1 set");

        rig.send_led(3, [1, 1]);
        assert_eq!(rig.io.borrow().led, 0b1111_0011);
    }

    #[test]
    fn none_and_unknown_led_commands_leave_bits_unchanged() {
        let mut rig = Rig::new();
        rig.io.borrow_mut().led = 0b01;
        rig.tick();
        rig.send_led(1, [0, 3]);
        assert_eq!(rig.io.borrow().led, 0b01);
        rig.send_led(2, [0xFF, 0]);
        assert_eq!(rig.io.borrow().led, 0b01);
        rig.send_led(3, [3, 2]);
        assert_eq!(rig.io.borrow().led, 0b01, "unknown byte is ignored, the valid command still applies");
    }

    #[test]
    fn led_word_is_written_back_even_when_nothing_changes() {
        let mut rig = Rig::new();
        rig.tick();
        rig.request_led(1, [0, 0]);
        rig.tick();
        let io = rig.io.borrow();
        assert!(io.led_valid, "libam LED word is marked valid by every accepted request");
        assert_eq!(io.led_writes, 1);
    }

    #[test]
    fn request_is_accepted_in_post_and_acknowledged_in_next_pre_without_succeeded() {
        let mut rig = Rig::new();
        rig.tick();
        rig.request_led(5, [1, 0]);

        let mut ctx = rig.harness.ctx();
        rig.slot.pre_execute(&mut ctx);
        assert_eq!(ctx.server.board_io().led_response().generation(), 0, "pre of the request frame does not ack");
        rig.slot.post_execute(&mut ctx);
        assert_eq!(rig.io.borrow().led, 1, "accepted during postExecute");
        assert_eq!(ctx.server.board_io().led_response().generation(), 0);
        assert!(rig.slot.controller().is_busy(&ctx));

        rig.slot.pre_execute(&mut ctx);
        assert_eq!(ctx.server.board_io().led_response().generation(), 5);
        assert!(!rig.slot.controller().is_busy(&ctx));
        assert_eq!(ctx.server.board_io().dip_switch_bits(), 0);
    }

    #[test]
    fn dip_switches_are_latched_once_per_process() {
        let mut rig = Rig::new();
        rig.io.borrow_mut().dip = 0x1A5;
        rig.tick();
        assert_eq!(rig.harness.server.board_io().dip_switch_bits(), 0xA5, "(u8) of the 64-bit word");
        assert_eq!(rig.harness.server.board_io().dip_switch_app_value(), 0xA5, "same libam byte");

        // Not ready → ready again: clearServerData runs again but does not read the DIP word a second time.
        rig.io.borrow_mut().dip = 0x42;
        rig.harness.manager.on_change_state(ProcessState(0));
        rig.tick();
        rig.harness.manager.on_change_state(ProcessState(3));
        rig.tick();
        assert_eq!(rig.harness.server.board_io().dip_switch_bits(), 0xA5);
        assert_eq!(rig.harness.server.board_io().dip_switch_app_value(), 0xA5);
    }

    #[test]
    fn dip_is_not_latched_while_libam_is_absent() {
        let mut rig = Rig::new();
        rig.io.borrow_mut().absent = true;
        rig.io.borrow_mut().dip = 0x11;
        rig.tick();
        assert_eq!(rig.harness.server.board_io().dip_switch_bits(), 0);

        // libam comes up: the next ready edge latches the value that is current then.
        rig.io.borrow_mut().absent = false;
        rig.harness.manager.on_change_state(ProcessState(0));
        rig.tick();
        rig.harness.manager.on_change_state(ProcessState(3));
        rig.tick();
        assert_eq!(rig.harness.server.board_io().dip_switch_bits(), 0x11);
    }

    #[test]
    fn push_switches_follow_libam_on_every_update() {
        let mut rig = Rig::new();
        rig.tick();
        assert_eq!(rig.harness.server.board_io().push_switch_bits(), 0);
        rig.io.borrow_mut().push = 0x103;
        rig.tick();
        assert_eq!(rig.harness.server.board_io().push_switch_bits(), 0x03);
        rig.io.borrow_mut().push = 0;
        rig.tick();
        assert_eq!(rig.harness.server.board_io().push_switch_bits(), 0);
    }

    #[test]
    fn without_libam_the_request_fails_and_stays_in_flight_until_update_runs_again() {
        let mut rig = Rig::new();
        rig.tick();
        rig.io.borrow_mut().absent = true;
        rig.request_led(3, [1, 1]);
        rig.tick();
        assert_eq!(rig.io.borrow().led, 0, "LED word untouched");
        assert!(rig.busy(), "update returns before checkResult, so the request stays in flight");
        rig.tick();
        assert_eq!(rig.harness.server.board_io().led_response().generation(), 0);

        // The accept failure state (3) commits as soon as libam is back; the response carries only the generation.
        rig.io.borrow_mut().absent = false;
        rig.tick();
        assert_eq!(rig.harness.server.board_io().led_response().generation(), 3);
        assert_eq!(rig.io.borrow().led, 0, "failed accept never applied the command");
    }

    #[test]
    fn new_generation_while_in_flight_waits_and_mirrors_the_latest() {
        let mut rig = Rig::new();
        rig.tick();
        rig.request_led(1, [1, 0]);
        {
            let mut ctx = rig.harness.ctx();
            rig.slot.pre_execute(&mut ctx);
            rig.slot.post_execute(&mut ctx);
        }
        // Client bumps the generation before the daemon acknowledged the first request.
        rig.request_led(2, [0, 1]);
        {
            let mut ctx = rig.harness.ctx();
            rig.slot.post_execute(&mut ctx);
            assert_eq!(rig.io.borrow().led, 1, "second request is not accepted while the first is in flight");
            rig.slot.pre_execute(&mut ctx);
            assert_eq!(ctx.server.board_io().led_response().generation(), 2, "mirror takes the latest generation");
            rig.slot.post_execute(&mut ctx);
        }
        assert_eq!(rig.io.borrow().led, 1, "generations are equal now, the second command is lost");
    }
}
