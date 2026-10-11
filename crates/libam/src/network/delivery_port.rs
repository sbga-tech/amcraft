//! The LAN-install and net-delivery facades of the network module and libam's handles.
//!
//! * `am::laninstall`: [`LanInstallPort`], the LAN install mode and servers ([`LanInstallHandle`]).
//! * `am::netdeliver`: network delivery images and the local delivery source ([`NetDeliveryHandle`]). The
//!   NetDeliver manager downloads the application and option images; [`NetDeliveryPort`] exposes its time and image
//!   records and takes the local delivery source. The image property is the `am::appimage` record of
//!   [`crate::appimage`].

use crate::appimage::AppImageProperty;
use crate::result::AmResult;
use crate::shared::Shared;
use crate::time::Tm;

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

/// Capacity of the delivery source buffer (`wchar_t[258]`): `128 + 1 + 128` characters plus NUL.
pub const DELIVERY_SOURCE_CAPACITY: usize = 258;

/// The two image slots of the libam NetDeliver manager (kind 2 is rejected by libam with `"Invalid Param"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    /// Application image (kind 0).
    App,
    /// Option image (kind 1).
    Option,
}

/// libam time-info record (two 36-byte `struct tm`, `libam+0x27610+0x16C+0x84C*kind`).
#[derive(Debug, Clone)]
pub struct TimeInfo {
    /// `struct tm` at record `+0x00`, published as `orderTime` [I name].
    pub order: Tm,
    /// `struct tm` at record `+0x24`, published as `releaseTime` [I name].
    pub release: Tm,
}

/// A pair of libam download counters; the binary only fixes the order (`first` at the lower offset).
///
/// `u32::MAX` in either counter marks the pair unset.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DownloadProgress {
    /// First counter (which of total/current it is stays unresolved in the binary).
    pub first: u32,
    /// Second counter.
    pub second: u32,
}

impl DownloadProgress {
    /// A pair is set when neither counter is `-1` (`u32::MAX`).
    pub const fn is_set(self) -> bool {
        self.first != u32::MAX && self.second != u32::MAX
    }
}

/// The parts of libam's 704-byte image-info record that amdaemon reads.
#[derive(Debug, Clone)]
pub struct ImageInfo {
    /// `+0x00` raw ALL.Net download status (`DownloadStatus` int).
    pub raw_status: i32,
    /// `+0x08` embedded `am::appimage::Property`.
    pub property: AppImageProperty,
    /// `pairA.pair1` (`+0x298`); `u32::MAX` in either counter marks the pair unset.
    pub progress_a: DownloadProgress,
    /// `pairB.pair1` (`+0x2A8`). The pairs 0 and 2 of both records are computed by amdaemon but never published.
    pub progress_b: DownloadProgress,
}

/// libam calls made by amdaemon's `NetDeliveryController`.
pub trait NetDeliveryPort {
    /// NetDeliver facade handle (`0x1402192F0`, `Mgr+8+23`).
    ///
    /// `Err` is the original's `Result != OK || token == NULL` ("libam singleton absent"); every caller reports
    /// the `isReady()` precondition exception for it.
    fn netdeliver_handle(&self) -> AmResult<()>;

    /// NetDeliver manager available flag (`0x140216CC0` → `*(u8*)(libam+0x27610)`).
    fn is_available(&self) -> bool;

    /// Time-info record of `kind` (`0x140216D00`); `None` unless the manager is available and in state 2.
    fn time_info(&self, kind: ImageKind) -> Option<TimeInfo>;

    /// Image-info record of `kind` (`0x140216D50`); `None` unless the manager is available and in state 2.
    fn image_info(&self, kind: ImageKind) -> Option<ImageInfo>;

    /// Stores the delivery source (`appPath|optionPath` in a zero-padded `wchar_t[258]`) into the NetDeliver
    /// `LocalParam` (`0x140216C70` → `0x140291480`).
    ///
    /// `Err` when the manager is not available or its local-mode byte is clear (`Result(6, "unsupport
    /// function\n")`); the request then fails.
    fn set_local_param(&mut self, source: &[u16; DELIVERY_SOURCE_CAPACITY]) -> AmResult<()>;
}

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
