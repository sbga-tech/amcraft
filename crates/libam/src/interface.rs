//! The `AmInterface` handle (`am::Interface`, `spec/libam/core.md` M.1).
//!
//! * `initialize`: result code 2 when libam is already initialised; otherwise the main flow starts and the events
//!   of `CheckingCabinetTypeState::enter` come back.
//! * `execute`: exactly one frame; the events it produced (and any queued since) in emission order.
//! * `load_cabinet_type_result`: the answer to `LoadCabinetType`; the Checking -> Operating transition happens
//!   inside the call, events it raises are queued for the next `execute`.

use libam_api::interface::{AmInterface, CabinetAppConfig, LibamEvent, LibamwConfig};
use libam_api::result::AmResult;

use crate::shared::Shared;

/// `AmInterface` over the shared libam state.
#[derive(Debug)]
pub struct InterfaceHandle(pub(crate) Shared);

impl AmInterface for InterfaceHandle {
    fn initialize(&mut self, _config: &LibamwConfig) -> AmResult<Vec<LibamEvent>> {
        // `log_dir` and `app_name` configured the original debug log, which this crate does not keep.
        self.0.with(|core| core.initialize())
    }

    fn execute(&mut self) -> Vec<LibamEvent> {
        self.0.with(|core| core.execute())
    }

    fn load_cabinet_type_result(&mut self, accepted: bool, app_config: CabinetAppConfig) {
        self.0.with(|core| core.answer_cabinet_type(accepted, app_config));
    }

    fn finalize(&mut self) {
        self.0.with(|core| core.finalize());
    }

    fn is_develop(&self) -> AmResult<bool> {
        self.0.guard()?;
        Ok(self.0.read(|core| core.dongle.develop()))
    }

    fn app_root_path(&self) -> AmResult<Option<String>> {
        self.0.guard()?;
        Ok(self.0.read(|core| core.storage.app_root_path(core.apm.sub_game_id().is_some()).map(str::to_owned)))
    }
}
