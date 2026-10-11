//! `am::Manager::changeApp` (`0x140254D80`), `startApp` (`0x140253A30`) and `endApp` (`0x140253BC0`):
//! `spec/libam/storage.md` 5.4 and 5.5.
//!
//! The hand-over is re-entered on every retry (the daemon polls while the result is 3, busy), so every step is
//! idempotent by design. It works on [`Core`] because it touches the Aime, e-money, AimePay, ALL.Net, storage,
//! ABaaS, sequence and error modules and the NVRAM game-id switch in one call.

use libam_api::apm::AppVersion;
use libam_api::backup::GameId;
use libam_api::result::{AmError, AmResult};

use super::link::{self, LinkPaths};
use crate::core::Core;

fn failed() -> AmError {
    AmError::code(AmError::FAILED)
}

fn busy() -> AmError {
    AmError::code(AmError::BUSY)
}

fn game_id_text(game_id: &GameId) -> String {
    String::from_utf16_lossy(game_id.units())
}

impl Core {
    /// `0x14023FE10` with the `ready` conjunction of `0x14029D590` (storage.md 5.2).
    pub(crate) fn update_apm(&mut self) {
        let ready = self.dongle.is_done()
            && self.sequence.is_ready()
            && self.appimage.is_ready()
            && self.storage.is_ready()
            && self.app_info.is_ready()
            && self.allnet.is_ready()
            && self.aime.is_ready()
            && self.abaas.is_ready()
            && self.emoney.is_ready();
        self.apm.update(ready);
    }

    /// `am::Manager::startApp`: the hand-over, then `APM.startApp` with the log-folder link.
    pub(crate) fn apm_start_app(&mut self, game_id: GameId, version: AppVersion, with_aime: bool) -> AmResult<()> {
        self.change_app(Some(game_id), Some(version))?;
        if !self.apm.is_ready() || !self.apm.enabled() || !self.link_log_folders(&game_id) {
            return Err(failed());
        }
        if !self.apm.start(game_id, version.raw(), with_aime) {
            return Err(failed());
        }
        if !with_aime {
            let suspended = self.error.current().number != 0;
            self.aime.set_permit(false, suspended);
        }
        Ok(())
    }

    /// `am::Manager::endApp`: the hand-over with an invalid game id, then `APM.endApp`.
    pub(crate) fn apm_end_app(&mut self, test: bool) -> AmResult<()> {
        self.change_app(None, None)?;
        let in_test = self.sequence.is_test();
        if self.apm.end(in_test, test) { Ok(()) } else { Err(failed()) }
    }

    /// `linkLogFolder` with the folders evaluated after the hand-over: both present means a link is required.
    fn link_log_folders(&mut self, game_id: &GameId) -> bool {
        let (Some(main), Some(sub)) = (self.abaas.main_folder(), self.abaas.sub_folder()) else {
            return true;
        };
        let link_cabinet = format!("{main}{}", game_id_text(game_id));
        let target_cabinet = sub.to_owned();
        let link = self.backend.fs.map(&link_cabinet);
        let target = self.backend.fs.map(&target_cabinet);
        link::link_log_folder(
            self.backend.fs.as_ref(),
            LinkPaths { link_cabinet: &link_cabinet, target_cabinet: &target_cabinet, link: &link, target: &target },
        )
        .is_ok()
    }

    /// The shared precheck and hand-over (storage.md 5.4); `Err` code 3 is "try again next frame".
    fn change_app(&mut self, game_id: Option<GameId>, version: Option<AppVersion>) -> AmResult<()> {
        let start = game_id.is_some();
        // 1, 2: a disabled APM, a start while started, an end while not started.
        if !self.apm.enabled() || start == self.apm.is_started() {
            return Err(failed());
        }
        // 3: a latched error: an end is silently accepted, a start fails.
        if self.error.current().number != 0 {
            return if start { Err(failed()) } else { Ok(()) };
        }
        // 4.
        if !self.apm.is_ready() {
            return Err(busy());
        }
        let aime = self.aime.enabled();
        let emoney = self.emoney.is_enabled();
        let aimepay = self.aimepay.is_enabled();
        let allnet = self.allnet.auth_enabled();
        // 5, 6.
        if aime {
            self.aime.apm_prepare();
        }
        if emoney {
            self.emoney.cancel();
        }
        if aimepay {
            self.aimepay.cancel();
        }
        // 7: the first successful begin-change is a busy result; later calls see "already pending".
        if aime && self.aime.apm_begin_change() == 0 {
            return Err(busy());
        }
        // 8.
        if (allnet && !self.allnet.apm_change_ready())
            || (aime && !self.aime.apm_stopped())
            || (emoney && !self.emoney.apm_idle())
        {
            return Err(busy());
        }
        // 9: the game-id switch of the NVRAM files for the target.
        let target = game_id.or_else(|| self.dongle.game_id());
        if let Some(target) = target {
            self.switch_game_id(target);
        }
        let serial = self.dongle.keychip_short_serial().unwrap_or_default().to_owned();
        // 10: ALL.Net learns the sub-game id and version.
        if allnet {
            self.allnet.cancel_credit();
            let raw = version
                .map(AppVersion::raw)
                .or_else(|| self.appimage.current_app().filter(|property| property.is_valid()).map(|p| p.version))
                .unwrap_or(0);
            let changed = match target {
                Some(target) => self.allnet.apm_change(target, raw),
                None => Err(failed()),
            };
            if changed.is_err() {
                return Err(failed());
            }
        }
        // 11.
        if aime && let Some(target) = target {
            self.aime.apm_finish_change(target, &serial);
        }
        // 12.
        self.storage.set_apm_game_id(game_id);
        // 13.
        if self.abaas.enabled() && !self.abaas.change_app(self.backend.fs.as_ref(), game_id) {
            return Err(failed());
        }
        // 14.
        if let Some(target) = target {
            if emoney {
                self.emoney.apm_change(target);
            }
            if aimepay {
                self.aimepay.apm_change(target);
            }
        }
        // 15, 16: the sequence cleanup, then the idle refresh of every gated module when it is now in mode 3.
        self.sequence.reset_for_error(&mut self.events);
        if self.sequence.process_state() == 3 {
            self.refresh_idle_busy();
        }
        Ok(())
    }
}
