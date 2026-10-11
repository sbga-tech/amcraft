//! The hardware monitor driver `\\.\sghwmonitor` (`spec/libam/platform.md` D 3.2).
//!
//! The driver handle is a `std::fs::File` opened with `OpenOptionsExt` (access and share mode); what stays raw
//! `windows-sys` is the synchronous `DeviceIoControl` on a 4-byte buffer this module owns for the duration of the
//! call (no crate wraps a driver request).

use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::GENERIC_READ;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows_sys::Win32::System::IO::DeviceIoControl;

use crate::{HwMonitor, HwmError};

/// `IOCTL` of the CPU temperature reading.
const HWM_READ_IOCTL: u32 = 0x8000_6000;

/// The hardware monitor driver.
#[derive(Default)]
pub struct WindowsHwMonitor {
    device: Option<File>,
}

impl WindowsHwMonitor {
    /// A monitor that is not open.
    pub fn new() -> Self {
        Self::default()
    }
}

impl HwMonitor for WindowsHwMonitor {
    fn open(&mut self) -> Result<(), HwmError> {
        if self.device.is_some() {
            return Ok(());
        }
        let device = OpenOptions::new()
            .access_mode(GENERIC_READ)
            .share_mode(FILE_SHARE_READ)
            .open(r"\\.\sghwmonitor")
            .map_err(|_| HwmError::Open)?;
        self.device = Some(device);
        Ok(())
    }

    fn close(&mut self) {
        self.device = None;
    }

    fn read_cpu_temperature(&mut self) -> Result<i32, HwmError> {
        let device = self.device.as_ref().ok_or(HwmError::Read)?;
        let mut reading = [0u8; 4];
        let mut returned = 0u32;
        // SAFETY: the output buffer is 4 bytes and `returned` receives the byte count; the call is synchronous.
        let done = unsafe {
            DeviceIoControl(
                device.as_raw_handle(),
                HWM_READ_IOCTL,
                std::ptr::null(),
                0,
                reading.as_mut_ptr().cast::<c_void>(),
                reading.len() as u32,
                &mut returned,
                null_mut(),
            )
        };
        if done == 0 || returned != 4 {
            return Err(HwmError::Read);
        }
        Ok(i32::from_le_bytes(reading))
    }
}
