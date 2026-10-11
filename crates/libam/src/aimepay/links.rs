//! The wiring of the Aime Pay module to the rest of libam: what the main flow reads from the other modules for
//! [`AimePay::update`] (`0x14029D590`, `payment.apmgr.life.exec_inputs`) and the modules a user task uses
//! (display, credit, Aime user-info list, sequence manager).
//!
//! The flow calls [`update`] and [`set_shutdown`]; the port handle goes through [`drive`].

use libam_api::system_info::LibamSerial;

use super::{AimePay, Inputs, Links};
use crate::aime::Aime;
use crate::core::{Core, cx};
use crate::credit::Credit;
use crate::display::Display;
use crate::module::{Cx, StateWord};
use crate::sequence::Sequence;
use crate::util::wide_to_string;

/// The modules a task of the Aime Pay module touches, borrowed from [`Core`] next to the module itself.
struct CoreLinks<'a> {
    display: &'a mut Display,
    credit: &'a mut Credit,
    aime: &'a Aime,
    sequence: &'a Sequence,
}

impl Links for CoreLinks<'_> {
    fn display(&mut self) -> &mut Display {
        self.display
    }

    fn display_ready(&self) -> bool {
        self.display.ready()
    }

    fn test_mode(&self) -> bool {
        self.sequence.is_test()
    }

    fn ignore_coin(&self) -> bool {
        self.credit.ignore_coin()
    }

    fn addable(&self, player: u32) -> u32 {
        self.credit.addable(player)
    }

    fn add_coin(&mut self, player: u32, coin: u32) -> bool {
        self.credit.add_coin(player, coin)
    }

    fn card_id(&self, aime_id: u32) -> Option<[u8; 8]> {
        self.aime.card_id(aime_id)
    }
}

/// Runs `f` on the module with the hook context and the links it needs; every field is borrowed separately.
pub fn drive<R>(core: &mut Core, f: impl FnOnce(&mut AimePay, &mut Cx<'_>, &mut dyn Links) -> R) -> R {
    let mut cx = cx!(core);
    let mut links =
        CoreLinks { display: &mut core.display, credit: &mut core.credit, aime: &core.aime, sequence: &core.sequence };
    f(&mut core.aimepay, &mut cx, &mut links)
}

/// The text of a serial (`XXXX-XXXNNNNNNNN`).
fn serial_text(serial: LibamSerial) -> String {
    wide_to_string(&serial.standard)
}

/// The values `0x14029D590` passes to the module's `execute`.
fn inputs(core: &Core) -> Inputs {
    Inputs {
        prereq: core.credit.state() == StateWord::READY
            && core.platform.is_ready()
            && core.dongle.is_done()
            && core.display.is_ready(),
        net_ready: core.allnet.is_good(),
        board: core.platform.board_serial().map(serial_text),
        keychip: core.dongle.serial().map(serial_text),
        game_id: core.dongle.game_id().map(|id| id.to_string()),
        client_id: core.allnet.client_id(),
        emoney_registered: core.emoney.is_registered(),
        develop: core.platform.sub_storage(),
    }
}

/// `AimePay::update` with its inputs: the module's M.3 step.
pub fn update(core: &mut Core) {
    let inputs = inputs(core);
    drive(core, |module, cx, links| module.update(cx, &inputs, links));
}

/// `setShutdown(flag)` with `flag` = the sequence manager is in test mode, from the module-stop sequence
/// (`0x1402544A0`).
pub fn set_shutdown(core: &mut Core) {
    let test = core.sequence.is_test();
    drive(core, |module, cx, links| module.set_shutdown(test, cx, links));
}
