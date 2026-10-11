//! The hardware monitor driver `\\.\sghwmonitor` (`spec/libam/platform.md` part D 3.2): the CPU temperature
//! sensor behind `amyHm*`. The sampling policy is libam's (`libam::platform::hwm`).

use crate::Unsupported;

/// The driver `\\.\sghwmonitor` could not be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HwmError {
    /// `amyHmOpen` failed (status `0x81000008`).
    #[error("the hardware monitor driver cannot be opened")]
    Open,
    /// The read IOCTL failed or returned a short answer (status `0x81000002`).
    #[error("the temperature cannot be read")]
    Read,
}

/// The hardware monitor driver.
pub trait HwMonitor {
    /// `amyHmOpen`: `CreateFileW(L"\\\\.\\sghwmonitor", GENERIC_READ, FILE_SHARE_READ, OPEN_EXISTING)`.
    fn open(&mut self) -> Result<(), HwmError>;

    /// `amyHmClose`.
    fn close(&mut self);

    /// The raw reading of IOCTL `0x80006000` (a signed 32-bit integer, degrees Celsius \[I\]); the answer must be
    /// exactly four bytes long. The range check is the caller's.
    fn read_cpu_temperature(&mut self) -> Result<i32, HwmError>;
}

/// No hardware monitor driver.
impl HwMonitor for Unsupported {
    fn open(&mut self) -> Result<(), HwmError> {
        Err(HwmError::Open)
    }

    fn close(&mut self) {}

    fn read_cpu_temperature(&mut self) -> Result<i32, HwmError> {
        Err(HwmError::Read)
    }
}
