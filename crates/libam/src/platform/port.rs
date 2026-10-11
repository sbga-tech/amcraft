//! Port handles of the platform module: the facade tokens, the process hand-off, reboot, preload and the system
//! information reads (`spec/libam/platform.md` part A, B.1.1, C.2.10, C.4).

use libam_api::backup::GameId;
use libam_api::common::CommonPort;
use libam_api::process::{NextProcessCode, ProcessPort};
use libam_api::result::{AmError, AmResult};
use libam_api::system_info::{LibamSerial, SystemInfoPort, VideoResolution};
use libam_api::time::DateTime;

use crate::module::Cx;
use crate::shared::Shared;
use crate::util::wide_array;

/// The longest language name the facade accepts: `wcsnlen(str, 16) <= 15` (`error.md` 3).
const LANGUAGE_MAX_UNITS: usize = 15;

/// `SystemInfoPort` handle over the shared libam state.
#[derive(Debug)]
pub struct SystemInfoHandle(pub(crate) Shared);

/// `ProcessPort` handle over the shared libam state.
#[derive(Debug)]
pub struct ProcessHandle(pub(crate) Shared);

/// `CommonPort` handle over the shared libam state.
#[derive(Debug)]
pub struct CommonHandle(pub(crate) Shared);

/// A game id as the `wchar_t[5]` the facade returns: four characters and a NUL.
fn game_id_units(id: GameId) -> [u16; 5] {
    let mut units = [0u16; 5];
    units[..4].copy_from_slice(id.units());
    units
}

impl SystemInfoPort for SystemInfoHandle {
    fn board_facade(&mut self) -> AmResult<()> {
        self.0.guard()
    }

    fn dongle_facade(&mut self) -> AmResult<()> {
        self.0.guard()
    }

    fn video_facade(&mut self) -> AmResult<()> {
        self.0.guard()
    }

    fn storage_facade(&mut self) -> AmResult<()> {
        self.0.guard()
    }

    fn apm_facade(&mut self) -> AmResult<()> {
        self.0.guard()
    }

    /// The keychip region (`0x1402176C0`), 1 to 3.
    fn board_region(&mut self) -> Option<u32> {
        self.0.read(|core| core.dongle.region())
    }

    /// The platform manager's board block (`0x1402176A0`).
    fn board_serial(&mut self) -> Option<LibamSerial> {
        self.0.read(|core| core.platform.board_serial())
    }

    fn keychip_serial(&mut self) -> Option<LibamSerial> {
        self.0.read(|core| core.dongle.serial())
    }

    fn model_type(&mut self) -> Option<u8> {
        self.0.read(|core| core.dongle.model())
    }

    fn develop(&mut self) -> u8 {
        self.0.read(|core| u8::from(core.dongle.develop()))
    }

    fn dongle_game_id(&mut self) -> Option<[u16; 5]> {
        self.0.read(|core| core.dongle.game_id().map(game_id_units))
    }

    fn apm_game_id(&mut self) -> Option<[u16; 5]> {
        self.0.read(|core| core.apm.sub_game_id().map(game_id_units))
    }

    fn video_resolution(&mut self, index: u32) -> VideoResolution {
        self.0.read(|core| core.video.resolution(index))
    }

    /// `byte(desc+14) != 0 && byte(desc+15) != 0`: the flags are never set (C.2.10).
    fn video_output_active(&mut self, _index: u32) -> bool {
        false
    }

    fn app_root_path(&mut self) -> Option<[u16; 260]> {
        self.0.read(|core| core.storage.app_root_path(core.apm.sub_game_id().is_some()).map(wide_array::<260>))
    }

    fn ram_disk_path(&mut self) -> Option<[u16; 260]> {
        self.0.read(|core| core.storage.ram_disk().map(wide_array::<260>))
    }
}

impl ProcessPort for ProcessHandle {
    fn process_facade(&mut self) -> AmResult<()> {
        self.0.guard()
    }

    /// Stored only while the error manager is attached to the master context (the kill guard, A.3.1).
    fn set_next_process(&mut self, code: NextProcessCode) {
        self.0.with(|core| core.master.set_next_process(code as u32));
    }

    fn clock_ticks(&mut self) -> Option<i64> {
        self.0.read(|core| core.clock.performance_counter())
    }

    fn local_now(&mut self) -> DateTime {
        self.0.read(|core| core.clock.local_now())
    }
}

impl CommonPort for CommonHandle {
    fn process_facade(&mut self) -> AmResult<()> {
        self.0.guard()
    }

    fn player_total(&mut self) -> u64 {
        self.0.read(|core| core.sequence.player_total())
    }

    fn language(&mut self) -> AmResult<[u16; 16]> {
        self.0.guard()?;
        Ok(self.0.read(|core| core.error.language()))
    }

    /// `0x140218310`: a name longer than 15 units is refused with 1, a failed `changeLanguage` is 6.
    fn set_language(&mut self, language: &[u16]) -> AmResult<()> {
        self.0.guard()?;
        let length = language.iter().position(|&unit| unit == 0).unwrap_or(language.len());
        if length > LANGUAGE_MAX_UNITS {
            return Err(AmError::code(AmError::INVALID_ARGUMENT));
        }
        let changed = self.0.with(|core| {
            let mut cx = Cx {
                backend: &mut core.backend,
                clock: &*core.clock,
                events: &mut core.events,
                config: &core.config,
                nvram: &core.nvram,
            };
            core.error.change_language(&mut cx, &language[..length]) == 0
        });
        if changed { Ok(()) } else { Err(AmError::code(AmError::FAILED)) }
    }

    fn board_facade(&mut self) -> AmResult<()> {
        self.0.guard()
    }

    /// The platform reboot (`0x14024B8F0`): request, wait 20 s, latch 4105.
    fn reboot(&mut self) {
        self.0.with(|core| {
            let mut cx = Cx {
                backend: &mut core.backend,
                clock: &*core.clock,
                events: &mut core.events,
                config: &core.config,
                nvram: &core.nvram,
            };
            core.platform.reboot(&mut cx);
        });
    }

    /// `amWorkaroundPreloadDatasec(process_id, 0)`: `Result` 6 when the process cannot be preloaded.
    fn preload(&mut self, process_id: u32) -> AmResult<()> {
        self.0.guard()?;
        let preloaded = self.0.with(|core| {
            let outcome = core.backend.preload.preload(process_id);
            if outcome.is_err() {
                tracing::info!(process_id, "preloading the data sections failed");
            }
            outcome.is_ok()
        });
        if preloaded { Ok(()) } else { Err(AmError::code(AmError::FAILED)) }
    }
}
