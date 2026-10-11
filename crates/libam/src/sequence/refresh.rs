//! The module refresh sweep (`0x1402544A0`, `error.md` 13 / `sequence.md` 4.7): every module that is not ready for
//! the current situation, or holds a resettable error, is re-armed.

use crate::core::{Core, cx};
use crate::module::ErrorCell;
use crate::network::networktest::PeerView;

impl Core {
    /// `0x1402554E0(force, block, cell)`: true when `force` or `block`, else when the module's own error cell
    /// holds an error whose reset type is 2 (operation) or 4 (any).
    fn refresh_needed(&self, force: bool, block: bool, cell: ErrorCell) -> bool {
        force
            || block
            || cell
                .get()
                .is_some_and(|(number, sub_number)| matches!(self.error.reset_type_for(number, sub_number), 2 | 4))
    }

    /// `0x1402544A0(force)`: the modules in the fixed order of the original. The readiness terms are read when
    /// the module's turn comes, so a module re-armed earlier in the sweep blocks the later ones that depend on it.
    /// `force` is set by the mode toggle; the error reset runs the sweep unforced.
    pub fn refresh_modules(&mut self, force: bool) {
        if self.refresh_needed(force, false, self.dongle.error_cell()) {
            self.dongle.restart();
        }
        if self.refresh_needed(force, false, self.hwm.error_cell()) {
            self.hwm.restart();
        }
        if self.refresh_needed(force, false, self.platform.error_cell()) {
            self.platform.refresh();
        }
        let block = !self.platform.is_ready() || !self.dongle.is_done();
        if self.refresh_needed(force, block, self.sequence.error_cell()) {
            self.sequence.restart();
        }
        let test = self.sequence.is_test();
        if self.refresh_needed(force, false, self.jvs.error_cell()) {
            self.jvs.restart();
        }
        if self.refresh_needed(force, false, self.can.error_cell()) {
            self.can.restart();
        }
        if self.refresh_needed(force, false, self.usbio.error_cell()) {
            let now = self.clock.tick_ms();
            self.usbio.restart(test, now);
        }
        if self.refresh_needed(force, false, self.video.error_cell()) {
            self.video.restart();
        }
        if self.refresh_needed(force, false, self.display.error_cell()) {
            self.display.restart();
        }
        // The catalogue has no error cell the sweep can see: it is re-armed by `force` only.
        if force {
            self.appimage.restart();
        }
        let block = !self.jvs.state().is_done() || !self.can.is_ready() || !self.usbio.is_ready();
        if self.refresh_needed(force, block, self.credit.error_cell()) {
            self.credit.restart();
        }
        let block = !self.dongle.is_done();
        if self.refresh_needed(force, block, self.error.own_cell()) {
            self.error.restart(&mut cx!(self), &mut self.master, test);
        }
        if self.refresh_needed(force, block, self.network.error_cell()) {
            self.network.restart_module(&mut cx!(self));
        }
        let block = !self.dongle.is_done() || !self.sequence.is_ready();
        if self.refresh_needed(force, block, self.storage.error_cell()) {
            self.storage.restart();
        }
        let block = !self.error.is_ready();
        if self.refresh_needed(force, block, self.usbdevice.error_cell()) {
            self.usbdevice.restart();
        }
        let latch_class =
            self.app_info.error_cell().get().map(|(number, sub_number)| self.error.reset_type_for(number, sub_number));
        self.app_info.restart(force, self.sequence.is_ready(), latch_class);
        let block = !self.sequence.is_ready() || !self.network.is_ready() || !self.appimage.is_ready();
        if self.refresh_needed(force, block, self.allnet.error_cell()) {
            self.allnet.restart(self.sequence.is_game_mode());
        }
        let block = !self.sequence.is_ready() || !self.network.is_ready() || !self.allnet.is_ready();
        if self.refresh_needed(force, block, self.laninstall.error_cell()) {
            self.laninstall.restart();
        }
        let block = !self.dongle.is_done() || !self.allnet.is_ready();
        if self.refresh_needed(force, block, self.aime.error_cell()) {
            self.aime.restart(test);
        }
        let block = !self.dongle.is_done()
            || !self.credit.is_ready()
            || !self.storage.is_ready()
            || !self.platform.is_ready()
            || !self.aime.is_ready()
            || !self.display.is_ready();
        if self.refresh_needed(force, block, self.emoney.error_cell()) {
            self.emoney.notify_module(test);
        }
        let block = !self.dongle.is_done()
            || !self.platform.is_ready()
            || !self.appimage.is_ready()
            || !self.storage.is_ready()
            || !self.allnet.is_ready();
        if self.refresh_needed(force, block, self.abaas.error_cell()) {
            let now = self.clock.tick_ms();
            self.abaas.restart(now);
        }
        let block =
            !self.credit.is_ready() || !self.platform.is_ready() || !self.dongle.is_done() || !self.display.is_ready();
        if self.refresh_needed(force, block, self.aimepay.error_cell()) {
            // `setShutdown(sequence in test mode)` (`0x140238D90`, payment.md PayApMgr).
            crate::aimepay::set_shutdown(self);
        }
        let block = !self.dongle.is_done()
            || !self.sequence.is_ready()
            || !self.appimage.is_ready()
            || !self.storage.is_ready()
            || !self.app_info.is_ready()
            || !self.allnet.is_ready()
            || !self.aime.is_ready()
            || !self.abaas.is_ready()
            || !self.emoney.is_ready()
            || !self.aimepay.is_ready();
        if self.refresh_needed(force, block, self.apm.error_cell()) {
            self.apm.restart();
        }
        let block = !self.network.is_ready()
            || !self.allnet.is_ready()
            || !self.aime.is_ready()
            || !self.emoney.is_ready()
            || !self.aimepay.is_ready();
        if force || block {
            let peers = PeerView::of(self);
            self.networktest.forced_stop(&self.network, &peers);
        }
    }
}
