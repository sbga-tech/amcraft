//! `am::laninstall`: LAN install mode and servers.

use crate::result::AmResult;

/// libam LAN-install mode (first dword of the LanInstall manager, set by `LanInstallConverter` `0x1400116C0`).
///
/// libam only ever stores `0`, `1` or `2`; a port must map any other raw value to [`Self::Off`], which is what
/// the controller's two comparisons do with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanInstallMode {
    /// `lan_install.enable` is off.
    Off,
    /// Enabled and not `server`.
    Client,
    /// Enabled and `server`.
    Server,
}

/// libam calls made by amdaemon's `LanInstallController`.
pub trait LanInstallPort {
    /// LAN-install facade handle (`0x140219070`, `Mgr+8+19`).
    ///
    /// `Err` is the original's `Result != OK || token == NULL` ("libam singleton absent"); clear and update
    /// report the `isReady()` precondition exception and skip their body.
    fn lan_install_handle(&self) -> AmResult<()>;

    /// LAN-install mode (`0x140216C10` → `*(u32*)(libam+0x27478)`).
    fn mode(&self) -> LanInstallMode;

    /// Number of LAN-install servers (`0x140216C30` → `0x1402416D0`); `-1` when the mode is off or the manager
    /// failed to start (error code 4105).
    fn server_count(&self) -> i32;
}
