//! Port handle of the ABaaS log manager.

use libam_api::abaas::LogPort;
use libam_api::result::AmResult;

use crate::shared::Shared;
use crate::util::wide_array;

/// `LogPort` handle over the shared libam state (`storage.md` 6.2).
#[derive(Debug)]
pub struct LogHandle(pub(crate) Shared);

impl LogPort for LogHandle {
    fn abaas_handle(&mut self) -> AmResult<()> {
        self.0.guard()
    }

    fn is_alive(&mut self) -> bool {
        self.0.read(|core| core.abaas.is_alive())
    }

    fn file_count(&mut self) -> i32 {
        self.0.read(|core| core.abaas.file_count())
    }

    fn upload_idle(&mut self) -> bool {
        self.0.read(|core| core.abaas.upload_idle())
    }

    /// The sub-application folder while the APM has a sub-game id, else the main folder (`0x140215120`).
    fn current_log_root(&mut self) -> Option<[u16; 260]> {
        self.0.read(|core| {
            let folder =
                if core.apm.sub_game_id().is_some() { core.abaas.sub_folder() } else { core.abaas.main_folder() };
            folder.map(wide_array::<260>)
        })
    }

    fn send_error_message(&mut self, message: &[u16]) {
        self.0.with(|core| core.abaas.send_error_message(message, core.clock.as_ref()));
    }
}
