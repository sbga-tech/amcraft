//! The per-frame hand-over of `OperatingState::update` (`0x14029D590` line 215) to the log manager: the
//! `setupReady` conjunction, `appAllowed` and the `SystemInfo` pointers (`storage.md` 6.5.3), collected from the
//! modules that own them.

use super::LogInputs;
use crate::core::Core;
use crate::module::Cx;

impl Core {
    /// `LogManager::update(L, setupReady, appAllowed, &SystemInfo)`.
    pub(crate) fn update_abaas(&mut self) {
        // The ID objects of the platform (`ctx+0x6600`) and the keychip (`ctx+0x666A`); the board serial is owned.
        let main_id = self.platform.board_short_serial();
        let inputs = LogInputs {
            setup_ready: self.dongle.is_done()
                && self.platform.is_ready()
                && self.appimage.is_ready()
                && self.storage.is_ready()
                && self.allnet.is_ready(),
            // `allnet+0x4B8 == 0` (`0x14023D330`, the same field as `0x14023D350`).
            app_allowed: self.allnet.is_good(),
            game_id: self.dongle.game_id(),
            keychip: self.dongle.keychip_short_serial(),
            main_id: main_id.as_deref(),
            base_path: self.storage.path_a(),
            version: LogInputs::version_of(self.appimage.current_app()),
            // `0x14023D430`: the location answer's id, absent while it is 0.
            place_id: self.allnet.auth().location().map(|location| location.id).filter(|&id| id != 0),
        };
        let mut cx = Cx {
            backend: &mut self.backend,
            clock: &*self.clock,
            events: &mut self.events,
            config: &self.config,
            nvram: &self.nvram,
        };
        self.abaas.update(&mut cx, &inputs);
    }
}
