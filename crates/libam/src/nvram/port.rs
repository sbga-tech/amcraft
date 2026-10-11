//! Port handles of the nvram module and the game-id switch.

use libam_api::backup::{BackupPort, GameId, NvramDevice};
use libam_api::result::{AmError, AmResult};

use crate::core::Core;
use crate::shared::Shared;

/// `BackupPort` handle over the shared libam state.
#[derive(Debug)]
pub struct BackupHandle(pub(crate) Shared);

impl Core {
    /// The game-id switch (`0x140253940`, `storage.md` 3.8): `AppInfo` lets go of the application file, the file
    /// of `target` is opened (a failure is error 4105; every record owner notices the new
    /// [`crate::nvram::NvramContext::app_generation`] and reloads its application-file records), the id word is
    /// recomputed and `AppInfo` re-creates its context (journal reload).
    pub(crate) fn switch_game_id(&mut self, target: GameId) {
        self.nvram.close_app_file();
        self.app_info.set_game_id(&target);
        if self.nvram.open_app_file(&mut self.backend, Some(target)).is_err() {
            self.raise_error(4105, 0);
        }
        self.app_info.create_context(&self.nvram);
    }

    /// `set_default_game_id` target (`0x140217310`): the APM sub-game id when valid, else the configured one.
    pub(crate) fn default_game_id(&self) -> Option<GameId> {
        self.apm.sub_game_id().or_else(|| self.nvram.configured_game_id())
    }
}

impl BackupPort for BackupHandle {
    fn acquire_nvram(&self) -> AmResult {
        self.0.guard()
    }

    fn device_size(&self, device: NvramDevice) -> u64 {
        self.0.read(|core| if device == NvramDevice::File { core.app_info.file_size() } else { 0 })
    }

    fn device_alignment(&self, device: NvramDevice) -> u32 {
        self.0.read(|core| if device == NvramDevice::File { core.app_info.file_alignment() } else { 0 })
    }

    fn set_game_id(&mut self, game_id: &GameId) {
        if self.0.guard().is_ok() {
            self.0.with(|core| core.switch_game_id(*game_id));
        }
    }

    fn set_default_game_id(&mut self) {
        if self.0.guard().is_ok() {
            self.0.with(|core| {
                if let Some(target) = core.default_game_id() {
                    core.switch_game_id(target);
                }
            });
        }
    }

    fn load_record(&mut self, device: NvramDevice, offset: u32, buffer: &mut [u8]) -> AmResult {
        self.0.guard()?;
        let (status, loaded) = self.0.with(|core| {
            core.app_info.clear_list();
            let size = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
            if core.app_info.add_record(false, device as u32, offset, size, vec![0; buffer.len()]) != 0 {
                return (None, Vec::new());
            }
            let (status, mut buffers) = core.app_info.load(&core.nvram);
            (Some(status), buffers.pop().unwrap_or_default())
        });
        if loaded.len() == buffer.len() {
            buffer.copy_from_slice(&loaded);
        }
        match status {
            None => Err(AmError::code(AmError::INVALID_ARGUMENT)),
            Some(1) => Err(AmError::code(AmError::FAILED)),
            Some(2) => Err(AmError::code(AmError::UNKNOWN_TASK)),
            Some(3) => Err(AmError::code(4)),
            Some(_) => Ok(()),
        }
    }

    fn add_record(&mut self, device: NvramDevice, offset: u32, buffer: &[u8]) -> AmResult {
        self.0.guard()?;
        let size = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
        let rejected =
            self.0.with(|core| core.app_info.add_record(false, device as u32, offset, size, buffer.to_vec()));
        if rejected == 0 { Ok(()) } else { Err(AmError::code(AmError::INVALID_ARGUMENT)) }
    }

    fn commit_records(&mut self) -> AmResult {
        self.0.guard()?;
        let status = self.0.with(|core| core.app_info.save(&core.nvram));
        if status == 0 { Ok(()) } else { Err(AmError::code(AmError::FAILED)) }
    }
}
