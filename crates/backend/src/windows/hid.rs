//! The production HID bus of the USB I/O boards (`usbio.md` 1.3.1, 1.3.4 and 1.10).
//!
//! Enumeration, the manufacturer and product strings and the interrupt pipes (`WriteFile` / `ReadFile`) go through
//! the mature `hidapi` crate (its pure-Rust Windows backend, no C toolchain). Two details of that backend do not
//! fit this board, which is why the control pipe is hand-rolled below (the only `unsafe` of the module):
//!
//! * the backend has no `get_input_report` (`HidD_GetInputReport`), which the firmware update needs (`0x85`
//!   status, `usbio.md` 1.8.3);
//! * its `send_output_report` sizes the buffer by the feature report length of the board, which is zero for a
//!   board without feature reports, so `HidD_SetOutputReport` would be called with no data.
//!
//! The control pipe therefore uses a second handle of the same device interface (a `std::fs::File` opened with the
//! share mode of the original and `GENERIC_READ | GENERIC_WRITE`; the `HidD_*` calls are synchronous).
//!
//! One difference from the original: `hidapi` waits up to a second for a pending interrupt write where the original
//! waits 16 ms and then cancels it; a write that completes in time behaves identically.

use std::ffi::CString;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;

use hidapi::{HidApi, HidDevice as ApiDevice};
use windows_sys::Win32::Devices::HumanInterfaceDevice::{HidD_GetInputReport, HidD_SetOutputReport};
use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

use crate::hid::{HidBoard, HidBus, HidDevice, HidError, REPORT_LEN, Report};

/// The SEGA vendor id as it appears in a device path (the product id is not examined).
const SEGA_VENDOR: &str = "vid_0ca3";

/// At most this many HID interfaces are examined, in enumeration order (`usbio.md` 1.3.1, open item 8).
const MAX_INTERFACES: usize = 32;

/// The report id of every input report.
const INPUT_REPORT_ID: u8 = 1;

/// The HID bus of the machine.
#[derive(Default)]
pub struct WindowsHid {
    api: Option<HidApi>,
}

impl WindowsHid {
    /// A bus that enumerates on the first [`HidBus::enumerate`].
    pub fn new() -> Self {
        Self::default()
    }
}

impl fmt::Debug for WindowsHid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WindowsHid").finish_non_exhaustive()
    }
}

impl HidBus for WindowsHid {
    fn enumerate(&mut self) -> Result<Vec<HidBoard>, HidError> {
        match &mut self.api {
            Some(api) => api.refresh_devices().map_err(|_| HidError)?,
            None => self.api = Some(HidApi::new().map_err(|_| HidError)?),
        }
        let api = self.api.as_ref().ok_or(HidError)?;
        Ok(api
            .device_list()
            .take(MAX_INTERFACES)
            .filter_map(|info| {
                let path = info.path().to_string_lossy().into_owned();
                if !path.to_ascii_lowercase().contains(SEGA_VENDOR) {
                    return None;
                }
                // A board that does not answer both strings is skipped, like a failed `setupNode`.
                Some(HidBoard {
                    path,
                    manufacturer: info.manufacturer_string()?.to_owned(),
                    product: info.product_string()?.to_owned(),
                })
            })
            .collect())
    }

    fn open(&mut self, path: &str) -> Result<Box<dyn HidDevice>, HidError> {
        let api = self.api.as_ref().ok_or(HidError)?;
        let device_path = CString::new(path).map_err(|_| HidError)?;
        let interrupt = api.open_path(&device_path).map_err(|_| HidError)?;
        interrupt.set_blocking_mode(false).map_err(|_| HidError)?;
        let control = ControlPipe::open(path)?;
        Ok(Box::new(Board { interrupt, control }))
    }
}

/// An opened board: the interrupt pipes through `hidapi`, the control pipe through its own handle.
#[derive(Debug)]
struct Board {
    interrupt: ApiDevice,
    control: ControlPipe,
}

impl HidDevice for Board {
    fn write_interrupt(&mut self, report: &Report) -> Result<(), HidError> {
        self.interrupt.write(report).map(drop).map_err(|_| HidError)
    }

    fn read_interrupt(&mut self) -> Result<Option<Report>, HidError> {
        let mut report = [0; REPORT_LEN];
        match self.interrupt.read(&mut report).map_err(|_| HidError)? {
            0 => Ok(None),
            _ => Ok(Some(report)),
        }
    }

    fn set_output_report(&mut self, report: &Report) -> Result<(), HidError> {
        self.control.set_output_report(report)
    }

    fn get_input_report(&mut self) -> Result<Report, HidError> {
        self.control.get_input_report()
    }
}

/// The control pipe of a board (`HidD_SetOutputReport` / `HidD_GetInputReport`); the handle closes with the file.
#[derive(Debug)]
struct ControlPipe(File);

impl ControlPipe {
    fn open(path: &str) -> Result<Self, HidError> {
        OpenOptions::new()
            .access_mode(GENERIC_READ | GENERIC_WRITE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(path)
            .map(Self)
            .map_err(|_| HidError)
    }

    fn set_output_report(&self, report: &Report) -> Result<(), HidError> {
        // SAFETY: the handle is open for the call; `report` is `REPORT_LEN` readable bytes and the call does not
        // retain the pointer.
        let sent = unsafe { HidD_SetOutputReport(self.0.as_raw_handle(), report.as_ptr().cast(), REPORT_LEN as u32) };
        if sent { Ok(()) } else { Err(HidError) }
    }

    fn get_input_report(&self) -> Result<Report, HidError> {
        let mut report = [0; REPORT_LEN];
        report[0] = INPUT_REPORT_ID;
        // SAFETY: the handle is open for the call; `report` is `REPORT_LEN` writable bytes and the call does not
        // retain the pointer.
        let received =
            unsafe { HidD_GetInputReport(self.0.as_raw_handle(), report.as_mut_ptr().cast(), REPORT_LEN as u32) };
        if received { Ok(report) } else { Err(HidError) }
    }
}
