//! `WiFiPort` handle.

use libam_api::allnet::{WiFiPort, WifiUnit};
use libam_api::result::{AmError, AmResult};

use crate::shared::Shared;

/// `WiFiPort` handle over the shared libam state.
#[derive(Debug)]
pub struct WifiHandle(pub(crate) Shared);

impl WiFiPort for WifiHandle {
    fn acquire_handle(&self) -> AmResult<()> {
        self.0.guard()
    }

    fn available(&self) -> bool {
        self.0.read(|core| core.config.allnet_auth.enable && core.config.allnet_wifi.enable)
    }

    fn unit_count(&self) -> u64 {
        self.0.read(|core| core.allnet.wifi.unit_count() as u64)
    }

    fn unit(&self, index: u64) -> AmResult<WifiUnit> {
        self.0.guard()?;
        let unit = usize::try_from(index).ok().and_then(|index| self.0.read(|core| core.allnet.wifi.unit(index)));
        unit.ok_or_else(|| AmError::code(1))
    }
}
