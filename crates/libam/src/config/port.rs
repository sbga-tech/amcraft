//! The configuration facade behind the cabinet-type callback: [`ConfigLoader`] (`anon::load`, `0x140006230`, and
//! the Config -> `am::AppConfig` conversion, `0x140006380`), the values it exchanges, and libam's
//! [`FileConfigLoader`].
//!
//! [`ProcessConfig`] and [`AppConfig`] are the configuration the cabinet-type event hands to amdaemon's controllers:
//! `amdaemon::process::Config` (0x50CE8 bytes, built by the config loader at the libam cabinet-type event,
//! `0x140005B40`) and libam `am::AppConfig`; these types carry exactly the parts the registered controllers consume
//! (`spec/controllers/lifecycle.md` §M6, §M7.2).

use std::fmt::Debug;

use crate::aime::AimeAppConfig;
use crate::aimepay::AimePayAppConfig;
use crate::allnet::AirProcessConfig;
use crate::can::CanAppConfig;
use crate::credit::CreditAppConfig;
use crate::emoney::{EMoneyAppConfig, EMoneyProcessConfig};
use crate::interface::CabinetAppConfig;
use crate::jvs::{InputConfig, OutputConfig};
use crate::network::{NetworkAppConfig, NetworkProcessConfig};
use crate::sequence::SequenceConfig;
use crate::usbio::{UsbDeviceAppConfig, UsbIoAppConfig, UsbIoProcessConfig};

use super::{Config, ConvertError, LoadError, convert, load_config};

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

/// What `anon::load` hands to `ConfigFileLoader` (`0x140089110`).
#[derive(Debug, Clone, Copy)]
pub struct ConfigLoadRequest<'a> {
    /// `am::systemio::DipSwitch` raw value.
    pub dip_switch: u16,
    /// `am::ModelType` value.
    pub model: u8,
    /// `am::Region` mapped to `0..=3` (anything but 1, 2, 3 becomes 0).
    pub region: u32,
    /// `-c` values (`EventListener+0x40`).
    pub config_paths: &'a [String],
    /// Executable directory (`EventListener+0x58`).
    pub exe_dir: &'a str,
    /// `-m` value (`EventListener+0x78`): merged-config JSON dump path, empty for none.
    pub merged_dump_path: &'a str,
}

/// Bytes of the loaded `amdaemon::process::Config` (0x50CE8 bytes) that `0x140005B40` inspects itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ConfigBytes {
    /// `Config[0x4FF8C]` (u8): replaced by the maximum of the three bytes below in step 4.
    pub byte_4ff8c: u8,
    /// `Config[0x4FF98]` (u8).
    pub byte_4ff98: u8,
    /// `Config[0x4FF9D]` (u8).
    pub byte_4ff9d: u8,
    /// `Config[0x4FF9E]` (u8): ALL.Net Air enabled; the callback rejects the configuration when non-zero.
    pub air_enabled: u8,
    /// `Config[0x507E0]` (u8).
    pub display_flag_507e0: u8,
    /// `Config[0x50B14]` (u8).
    pub display_flag_50b14: u8,
    /// `Config[0x50974]` (u32).
    pub display_value_50974: u32,
    /// `Config[0x50CA8]` (u32).
    pub display_value_50ca8: u32,
}

/// A successfully loaded `amdaemon::process::Config`.
#[derive(Debug, Clone, Default)]
pub struct LoadedConfig {
    /// The parts the `configCbs` registrants consume.
    pub process: ProcessConfig,
    /// The raw bytes the callback inspects.
    pub bytes: ConfigBytes,
}

/// The configuration loader behind the cabinet-type callback.
pub trait ConfigLoader: Debug {
    /// `ConfigFileLoader` (`0x140089110`, called by `anon::load`): `None` when the loader reports failure.
    fn load(&mut self, request: &ConfigLoadRequest<'_>) -> Option<LoadedConfig>;

    /// Config → `am::AppConfig` conversion `0x140006380`: runs the 21 converters against `app_config`
    /// (including `abaas_log_root`); `false` when any converter fails.
    fn convert(&mut self, config: &LoadedConfig, exe_dir: &str, app_config: &mut CabinetAppConfig) -> bool;
}

/// The loader that reads the JSON config files from disk.
#[derive(Debug, Default)]
pub struct FileConfigLoader {
    document: Option<Config>,
    error: Option<LoadError>,
    convert_error: Option<ConvertError>,
}

impl FileConfigLoader {
    pub fn new() -> Self {
        Self::default()
    }

    /// The configuration of the last successful [`ConfigLoader::load`].
    pub fn document(&self) -> Option<&Config> {
        self.document.as_ref()
    }

    /// Why the last [`ConfigLoader::load`] failed; `None` after a success.
    pub fn error(&self) -> Option<&LoadError> {
        self.error.as_ref()
    }

    /// Why the last [`ConfigLoader::convert`] failed; `None` after a success.
    pub fn convert_error(&self) -> Option<&ConvertError> {
        self.convert_error.as_ref()
    }
}

impl ConfigLoader for FileConfigLoader {
    fn load(&mut self, request: &ConfigLoadRequest<'_>) -> Option<LoadedConfig> {
        match load_config(request) {
            Ok(config) => {
                let loaded = config.loaded();
                self.document = Some(config);
                self.error = None;
                Some(loaded)
            }
            Err(error) => {
                self.document = None;
                self.error = Some(error);
                None
            }
        }
    }

    fn convert(&mut self, config: &LoadedConfig, exe_dir: &str, app_config: &mut CabinetAppConfig) -> bool {
        let Some(document) = self.document.as_ref() else {
            self.convert_error =
                Some(ConvertError { converter: "config", reason: "no configuration is loaded".to_owned() });
            return false;
        };
        match convert::convert(document, config.bytes.byte_4ff8c, exe_dir, app_config) {
            Ok(()) => {
                self.convert_error = None;
                true
            }
            Err(error) => {
                self.convert_error = Some(error);
                false
            }
        }
    }
}
