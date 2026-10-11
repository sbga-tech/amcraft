//! The HID transport of the USB I/O boards (`usbio.md` 1.3.4).
//!
//! A board speaks 64-byte HID reports: output report id `0x10` (byte 1 = command), input report id `1`
//! (byte 1 = payload). Commands with bit 7 set travel on the control pipe (`HidD_SetOutputReport` /
//! `HidD_GetInputReport`), the others on the interrupt pipes (`WriteFile` / `ReadFile`).

use crate::Unsupported;

/// Length of every report.
pub const REPORT_LEN: usize = 64;

/// One HID report: byte 0 = report id, byte 1 = command / status, bytes 2..63 = data.
pub type Report = [u8; REPORT_LEN];

/// A transfer that failed (`-1` of `amUsbioSendRequest` / `amUsbioRecvAcknowledge`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("usb i/o transfer failed")]
pub struct HidError;

/// A HID interface with the SEGA vendor id as the enumeration lists it, before the product string is parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HidBoard {
    /// The device path.
    pub path: String,
    /// `HidD_GetManufacturerString`.
    pub manufacturer: String,
    /// `HidD_GetProductString`.
    pub product: String,
}

/// An opened board.
pub trait HidDevice: std::fmt::Debug {
    /// Interrupt-OUT write of a 64-byte report (`WriteFile`).
    fn write_interrupt(&mut self, report: &Report) -> Result<(), HidError>;

    /// Interrupt-IN read (`ReadFile`): `Ok(None)` when no report is pending (`ERROR_IO_INCOMPLETE`).
    fn read_interrupt(&mut self) -> Result<Option<Report>, HidError>;

    /// Control-pipe output report (`HidD_SetOutputReport`).
    fn set_output_report(&mut self, report: &Report) -> Result<(), HidError>;

    /// Control-pipe input report (`HidD_GetInputReport`, report id 1).
    fn get_input_report(&mut self) -> Result<Report, HidError>;
}

/// The USB bus: enumeration and opening.
pub trait HidBus: std::fmt::Debug {
    /// Every present HID interface whose device path carries the SEGA vendor id `0x0CA3`, in enumeration
    /// order, that could be opened and answered both strings. `Err` is an enumeration failure.
    fn enumerate(&mut self) -> Result<Vec<HidBoard>, HidError>;

    /// Opens the board at `path`.
    fn open(&mut self, path: &str) -> Result<Box<dyn HidDevice>, HidError>;
}

/// A HID bus without USB I/O boards.
impl HidBus for Unsupported {
    fn enumerate(&mut self) -> Result<Vec<HidBoard>, HidError> {
        Ok(Vec::new())
    }

    fn open(&mut self, _path: &str) -> Result<Box<dyn HidDevice>, HidError> {
        Err(HidError)
    }
}
