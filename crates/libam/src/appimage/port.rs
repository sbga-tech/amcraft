//! Port handles of the appimage module (`storage.md` 4.9, 4.11): pure reads with the APM overrides.

use libam_api::app_image::{AppImagePort, AppImageProperty};
use libam_api::result::AmResult;

use crate::shared::Shared;

/// `AppImagePort` handle over the shared libam state.
#[derive(Debug)]
pub struct AppImageHandle(pub(crate) Shared);

impl AppImagePort for AppImageHandle {
    fn acquire_app_image(&self) -> AmResult {
        self.0.guard()
    }

    fn current_app(&self) -> Option<AppImageProperty> {
        self.0.read(
            |core| {
                if core.apm.sub_game_id().is_some() { core.apm.property() } else { core.appimage.current_app() }
            },
        )
    }

    fn option_count(&self) -> u32 {
        self.0.read(|core| if core.apm.sub_game_id().is_some() { 0 } else { core.appimage.option_count() })
    }

    fn option_property(&self, index: u32) -> Option<AppImageProperty> {
        self.0.read(
            |core| {
                if core.apm.sub_game_id().is_some() { core.apm.property() } else { core.appimage.option_at(index) }
            },
        )
    }

    fn option_root(&self) -> Option<[u16; 260]> {
        self.0.read(|core| core.appimage.option_root())
    }
}
