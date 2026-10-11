//! `am::usbdevice`: USB device reconnects.

use crate::result::{AmError, AmResult};

/// Event raised by libam's `am::usbdevice::CompleteTaskEventListener` (`UsbDeviceEventListener::vf1`
/// `0x140048080`). libam fires it from `am::usbdevice::Manager::update` (`0x14024CEA0`) inside the libam
/// update pump.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsbDeviceEvent {
    /// The pending reconnect finished; `code` is libam's completion code (`1` success, `2` failure,
    /// `3` driver error `0x81000015`).
    ReconnectComplete {
        /// libam completion code.
        code: i32,
    },
}

/// The libam `am::usbdevice` surface used by amdaemon's USB device controller.
pub trait UsbDevicePort {
    /// `usbdevice` facade accessor `0x140218D50` on `Manager+8`: `Err` (code 2) when libam is not
    /// initialised.
    fn usbdevice_facade(&self) -> AmResult<()>;

    /// `reconnect(facade, vid, pid)` (`0x140217AA0` → `am::usbdevice::Manager::reconnect` `0x14024CBB0`).
    /// libam refuses (code 2) when not ready, without listener, without amMaster, while a task is active
    /// or when the asynchronous driver call fails to start.
    fn usb_reconnect(&mut self, vendor_id: u16, product_id: u16) -> AmResult<()>;

    /// Completion events raised since the last call; amdaemon delivers them from its libam event pump
    /// (`on_libam_events`).
    fn take_events(&mut self) -> Vec<UsbDeviceEvent>;
}

/// A USB `VID:PID` pair (`{u16 vid, u16 pid}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UsbDeviceId {
    /// USB vendor id.
    pub vendor_id: u16,
    /// USB product id.
    pub product_id: u16,
}

/// `am::usbdevice::Required` (`0x90` bytes): a counted set of `VID:PID` pairs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsbRequired {
    count: u64,
    pairs: Vec<UsbDeviceId>,
}

impl UsbRequired {
    /// Capacity of the pair array.
    pub const MAX_PAIRS: usize = 32;

    /// `addPair` (`0x14021E8A0`): fails with code 2 once 32 pairs exist.
    pub fn add_pair(&mut self, vendor_id: u16, product_id: u16) -> AmResult<()> {
        if self.pairs.len() >= Self::MAX_PAIRS {
            return Err(AmError::with_message(AmError::NOT_INITIALIZED, "Pair list is full"));
        }
        self.pairs.push(UsbDeviceId { vendor_id, product_id });
        Ok(())
    }

    /// `setCount` (`0x14021E890`): stores the `u32` into the first `u64`.
    pub fn set_count(&mut self, count: u32) {
        self.count = u64::from(count);
    }

    /// Number of devices required.
    pub fn count(&self) -> u64 {
        self.count
    }

    /// The pairs added so far.
    pub fn pairs(&self) -> &[UsbDeviceId] {
        &self.pairs
    }
}

/// The part of `am::AppConfig` owned by the USB device controller: `am::usbdevice::Config` at
/// `AppConfig+0x2000` (`count`, `Required sets[9]`, `listener*` at `+0x518` = `AppConfig+0x2518`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsbDeviceAppConfig {
    required: Vec<UsbRequired>,
    /// `AppConfig+0x2518` holds the controller's completion listener (set by the USB device controller's
    /// AppConfig callback).
    pub listener_installed: bool,
}

impl UsbDeviceAppConfig {
    /// Capacity of the `Required` array.
    pub const MAX_REQUIRED: usize = 9;

    /// `Config::addRequired` (`0x14021EAE0`): fails with code 2 ("Required list is full") at 9 sets.
    pub fn add_required(&mut self, required: UsbRequired) -> AmResult<()> {
        if self.required.len() >= Self::MAX_REQUIRED {
            return Err(AmError::with_message(AmError::NOT_INITIALIZED, "Required list is full"));
        }
        self.required.push(required);
        Ok(())
    }

    /// The required sets.
    pub fn required(&self) -> &[UsbRequired] {
        &self.required
    }

    /// libam's validator for `am::usbdevice::Config` (`0x1402AFF80`): only an empty list is valid. libam's
    /// setup fails with `Unsupport check USB device count.` otherwise.
    pub fn is_valid(&self) -> bool {
        self.required.is_empty()
    }
}
