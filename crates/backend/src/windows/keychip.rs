//! The production keychip (Windows): the driver `\??\FddDriver` and its registry store
//! (`spec/libam/platform.md` part B 5).
//!
//! Crates carry what they can: the keychip store is read through [`WindowsRegistry`] (`winreg` plus the per-value
//! named mutex) and the driver handle is a `std::fs::File` opened with `OpenOptionsExt` (access, share mode,
//! overlapped flag). What stays raw `windows-sys`, each in one `unsafe` block on buffers this module owns for the
//! duration of the call: `DeviceIoControl` (overlapped) and the overlapped result and cancel calls (no crate wraps
//! a driver request pipeline).

use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    ERROR_INVALID_FUNCTION, ERROR_IO_INCOMPLETE, ERROR_IO_PENDING, GENERIC_READ, GENERIC_WRITE,
};
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE};
use windows_sys::Win32::System::IO::{CancelIoEx, DeviceIoControl, GetOverlappedResult, OVERLAPPED};

use super::registry::WindowsRegistry;
use crate::keychip::{Failure, KeychipDriver, OpenError, Poll, Property, PropertyError, Request, SubmitError};
use crate::{KeyMode, RegValue, RegistryError};

/// The calling thread's last Win32 error, read straight after a failed call.
fn last_error() -> u32 {
    std::io::Error::last_os_error().raw_os_error().map_or(0, i32::cast_unsigned)
}

/// The device of the keychip driver.
const KEYCHIP_DEVICE: &str = r"\??\FddDriver";
/// The registry key of the keychip store, below `HKEY_LOCAL_MACHINE`.
const KEYCHIP_KEY: &str = r"System\Sega\SystemProperty\keychip";
/// Room for one registry value of the store (the largest is the 16 byte keychip id); a longer value fails.
const VALUE_CAPACITY: usize = 256;

/// The request in flight; the driver writes into these buffers until the request completes.
struct Pending {
    overlapped: OVERLAPPED,
    input: Vec<u8>,
    output: Vec<u8>,
}

/// The keychip driver (`amyDongleOpen`, the overlapped request pipeline) and its registry store.
///
/// The request has no event of its own: the overlapped record carries no `hEvent`, so the kernel signals the file
/// object and `GetOverlappedResult` without waiting reports `ERROR_IO_INCOMPLETE` while the request is pending.
#[derive(Default)]
pub struct WindowsKeychip {
    device: Option<File>,
    pending: Option<Box<Pending>>,
}

impl WindowsKeychip {
    /// A keychip that is not open.
    pub fn new() -> Self {
        Self::default()
    }

    /// Waits for the request in flight so the driver no longer touches its buffers.
    fn drain(&mut self) {
        let (Some(pending), Some(device)) = (self.pending.take(), self.device.as_ref()) else {
            return;
        };
        let mut transferred = 0u32;
        // SAFETY: the device and the overlapped record belong to the request that is still in flight; the cancel
        // and the waiting result both refer to exactly that request.
        unsafe {
            CancelIoEx(device.as_raw_handle(), &pending.overlapped);
            GetOverlappedResult(device.as_raw_handle(), &pending.overlapped, &mut transferred, 1);
        }
    }
}

impl KeychipDriver for WindowsKeychip {
    fn open(&mut self) -> Result<(), OpenError> {
        if self.device.is_some() {
            return Ok(());
        }
        let device = OpenOptions::new()
            .access_mode(GENERIC_READ | GENERIC_WRITE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OVERLAPPED)
            .open(KEYCHIP_DEVICE)
            .map_err(|_| OpenError)?;
        self.device = Some(device);
        Ok(())
    }

    fn close(&mut self) {
        self.drain();
        self.device = None;
    }

    fn submit(&mut self, request: Request) -> Result<(), SubmitError> {
        if self.device.is_none() {
            return Err(SubmitError::Failed(6));
        }
        self.drain();
        let Some(device) = self.device.as_ref() else {
            return Err(SubmitError::Failed(6));
        };
        let mut pending = Box::new(Pending {
            overlapped: OVERLAPPED::default(),
            input: request.input,
            output: vec![0; request.output_len],
        });
        let input = if pending.input.is_empty() { null() } else { pending.input.as_ptr().cast::<c_void>() };
        let output = if pending.output.is_empty() { null_mut() } else { pending.output.as_mut_ptr().cast::<c_void>() };
        // SAFETY: the buffers and the overlapped record live in the box stored in `self.pending` until the request
        // finishes or is cancelled by `drain`; the device is open for the whole time.
        let accepted = unsafe {
            DeviceIoControl(
                device.as_raw_handle(),
                request.code,
                input,
                pending.input.len() as u32,
                output,
                pending.output.len() as u32,
                null_mut(),
                &mut pending.overlapped,
            )
        };
        if accepted == 0 {
            let error = last_error();
            if error != ERROR_IO_PENDING {
                return Err(if error == ERROR_INVALID_FUNCTION {
                    SubmitError::InvalidFunction
                } else {
                    SubmitError::Failed(error)
                });
            }
        }
        self.pending = Some(pending);
        Ok(())
    }

    fn poll(&mut self) -> Poll {
        let (Some(pending), Some(device)) = (self.pending.as_ref(), self.device.as_ref()) else {
            return Poll::Done(Err(Failure { last_error: 0 }));
        };
        let mut transferred = 0u32;
        // SAFETY: the record is the one of the request in flight on this device; the call does not wait.
        let finished = unsafe { GetOverlappedResult(device.as_raw_handle(), &pending.overlapped, &mut transferred, 0) };
        if finished == 0 {
            let error = last_error();
            if error == ERROR_IO_INCOMPLETE {
                return Poll::Pending;
            }
            self.pending = None;
            return Poll::Done(Err(Failure { last_error: error }));
        }
        let Some(mut pending) = self.pending.take() else {
            return Poll::Done(Err(Failure { last_error: 0 }));
        };
        pending.output.truncate(transferred as usize);
        Poll::Done(Ok(std::mem::take(&mut pending.output)))
    }

    /// One attempt under the named guard mutex of `core.md` B.4.5 (the key path without `\` followed by the value
    /// name, truncated to 259 units keeping the tail): the keychip store reads with retry count 0 (`platform.md`
    /// 5.4), so a busy mutex is reported as `Busy` and the setup retries on its next poll.
    fn property(&mut self, name: &str) -> Result<Property, PropertyError> {
        match WindowsRegistry::read_once(KEYCHIP_KEY, KeyMode::Open, name) {
            Ok(RegValue::Binary(bytes)) if bytes.len() <= VALUE_CAPACITY => Ok(Property::Binary(bytes)),
            Ok(RegValue::Dword(value)) => Ok(Property::Dword(value)),
            Err(RegistryError::Busy) => Err(PropertyError::Busy),
            _ => Err(PropertyError::Failed),
        }
    }

    /// A second handle: the device is opened again by whoever uses it, with a request pipeline of its own.
    fn duplicate(&self) -> Box<dyn KeychipDriver> {
        Box::new(Self::new())
    }
}

impl Drop for WindowsKeychip {
    fn drop(&mut self) {
        // The driver must be done with the buffers before they go.
        self.close();
    }
}
