//! The configuration the cabinet-type event hands to amdaemon's controllers.
//!
//! `amdaemon::process::Config` (0x50CE8 bytes, built by the config loader at the libam cabinet-type event,
//! `0x140005B40`) and libam `am::AppConfig`; these types carry exactly the parts the registered controllers
//! consume (`spec/controllers/lifecycle.md` §M6, §M7.2).

use crate::aime::AimeAppConfig;
use crate::aime_pay::AimePayAppConfig;
use crate::allnet::AirProcessConfig;
use crate::can::CanAppConfig;
use crate::credit::CreditAppConfig;
use crate::emoney::{EMoneyAppConfig, EMoneyProcessConfig};
use crate::input::InputConfig;
use crate::network::{NetworkAppConfig, NetworkProcessConfig};
use crate::output::OutputConfig;
use crate::sequence::SequenceConfig;
use crate::usb_device::UsbDeviceAppConfig;
use crate::usb_io::{UsbIoAppConfig, UsbIoProcessConfig};

/// Parts of `amdaemon::process::Config` read by `configCbs` registrants, in registration order.
#[derive(Debug, Clone, Default)]
pub struct ProcessConfig {
    /// `Config+0xA4`: Manager request-trace flag (`0x14000D870`).
    pub debug_dump: bool,
    /// Sequence (`@0x1400436B9`).
    pub sequence: SequenceConfig,
    /// USBIO (`@0x14004A556`).
    pub usb_io: UsbIoProcessConfig,
    /// Input (`@0x140035F3D`).
    pub input: InputConfig,
    /// Output (`@0x1400406AD`).
    pub output: OutputConfig,
    /// Network (`@0x14003E8BA`).
    pub network: NetworkProcessConfig,
    /// ALL.Net Air (`@0x14004FEEC`).
    pub allnet_air: AirProcessConfig,
    /// EMoney (`@0x14002F037`).
    pub emoney: EMoneyProcessConfig,
}

/// Parts of libam `am::AppConfig` touched by `appConfigCbs` registrants, in registration order.
#[derive(Debug, Clone, Default)]
pub struct AppConfig {
    /// CAN (`@0x1400262F9`).
    pub can: CanAppConfig,
    /// USBIO (`@0x14004A5B8`).
    pub usb_io: UsbIoAppConfig,
    /// USB device (`@0x1400481B9`).
    pub usb_device: UsbDeviceAppConfig,
    /// Credit (`@0x14002B584`).
    pub credit: CreditAppConfig,
    /// Network (`@0x14003E8F5`).
    pub network: NetworkAppConfig,
    /// Aime (`@0x1400151B0`).
    pub aime: AimeAppConfig,
    /// EMoney (`@0x14002F072`).
    pub emoney: EMoneyAppConfig,
    /// AimePay (`@0x14001A04A`).
    pub aime_pay: AimePayAppConfig,
}
