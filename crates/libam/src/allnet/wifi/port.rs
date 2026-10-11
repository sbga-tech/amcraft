//! The ALL.Net Wi-Fi facade: [`WiFiPort`], [`WifiUnit`] and libam's [`WifiHandle`].

use crate::result::{AmError, AmResult};
use crate::shared::Shared;

/// libam `WifiUnit` (`0x70` bytes at `LM+0x1A48`, stride `0x70`, spec §8.5) as the controller reads it. The
/// texts are libam's NUL-terminated (or array-filling) `wchar_t` arrays.
///
/// libam's getters for `serial` and `firm` never return null (they point into the unit), so the
/// controller's empty-string fallbacks for them cannot trigger and are not modelled.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WifiUnit {
    /// `+0x00` serial (`0x140227C40`); an empty serial marks an unpopulated unit.
    pub serial: [u16; 13],
    /// `+0x1A` libam's "auth good" flag: never read by amdaemon.
    pub auth_good: bool,
    /// `+0x1C` firmware version (`0x140227C60`).
    pub firm: [u16; 21],
    /// `+0x46` "server alive" flag (`0x140227C70`).
    pub server_alive: bool,
    /// `+0x48` access count (`0x140227C80`; libam stores a `u8`).
    pub access_count: u64,
    /// `+0x50` master serial; empty means none (`0x140227C90` returns null).
    pub master: [u16; 13],
}

/// libam port of the WiFi controller (spec §4.4; evidence `allnet.wifi.port.table`).
pub trait WiFiPort {
    /// WiFi facade handle (`0x140219250`): `Err` (code 2) when the libam singleton does not exist yet.
    fn acquire_handle(&self) -> AmResult<()>;

    /// `0x140215DA0`: `AuthConfig.enable && WifiConfig.enable`, a static configuration flag.
    fn available(&self) -> bool;

    /// `0x140215DC0`: number of units libam found.
    fn unit_count(&self) -> u64;

    /// `0x140215DE0`: a copy of unit `index`; `Err` (code 1) when `index >= unit_count()`.
    fn unit(&self, index: u64) -> AmResult<WifiUnit>;
}

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
