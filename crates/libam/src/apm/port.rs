//! Port handles of the apm module (`storage.md` 5.3).

use libam_api::apm::{ApmPort, AppVersion};
use libam_api::backup::GameId;
use libam_api::result::AmResult;

use crate::shared::Shared;

/// `ApmPort` handle over the shared libam state.
#[derive(Debug)]
pub struct ApmHandle(pub(crate) Shared);

impl ApmPort for ApmHandle {
    fn apm_handle(&mut self) -> AmResult<()> {
        self.0.guard()
    }

    fn start_app(&mut self, game_id: &GameId, version: AppVersion, with_aime: bool) -> AmResult<()> {
        self.0.guard()?;
        self.0.with(|core| core.apm_start_app(*game_id, version, with_aime))
    }

    fn end_app(&mut self, test: bool) -> AmResult<()> {
        self.0.guard()?;
        self.0.with(|core| core.apm_end_app(test))
    }

    fn state(&mut self) -> u32 {
        self.0.read(|core| core.apm.state())
    }
}
