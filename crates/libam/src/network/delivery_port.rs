//! Port handles of the LAN-install and net-delivery parts of the network module.

use libam_api::lan_install::{LanInstallMode, LanInstallPort};
use libam_api::net_delivery::{DELIVERY_SOURCE_CAPACITY, ImageInfo, ImageKind, NetDeliveryPort, TimeInfo};
use libam_api::result::AmResult;

use crate::shared::Shared;

/// `LanInstallPort` handle over the shared libam state.
#[derive(Debug)]
pub struct LanInstallHandle(pub(crate) Shared);

/// `NetDeliveryPort` handle over the shared libam state.
#[derive(Debug)]
pub struct NetDeliveryHandle(pub(crate) Shared);

impl LanInstallPort for LanInstallHandle {
    fn lan_install_handle(&self) -> AmResult<()> {
        self.0.guard()
    }

    fn mode(&self) -> LanInstallMode {
        match self.0.read(|core| core.laninstall.mode()) {
            1 => LanInstallMode::Client,
            2 => LanInstallMode::Server,
            _ => LanInstallMode::Off,
        }
    }

    fn server_count(&self) -> i32 {
        self.0.read(|core| core.laninstall.server_count())
    }
}

impl NetDeliveryPort for NetDeliveryHandle {
    fn netdeliver_handle(&self) -> AmResult<()> {
        self.0.guard()
    }

    fn is_available(&self) -> bool {
        self.0.read(|core| core.laninstall.deliver.is_available())
    }

    fn time_info(&self, kind: ImageKind) -> Option<TimeInfo> {
        self.0.read(|core| core.laninstall.deliver.time_info(kind))
    }

    fn image_info(&self, kind: ImageKind) -> Option<ImageInfo> {
        self.0.read(|core| core.laninstall.deliver.image_info(kind))
    }

    fn set_local_param(&mut self, source: &[u16; DELIVERY_SOURCE_CAPACITY]) -> AmResult<()> {
        self.0.guard()?;
        self.0.with(|core| core.laninstall.deliver.set_local_param(source))
    }
}
