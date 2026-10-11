//! `am::Interface`: the libam lifecycle amdaemon drives, the listener callbacks libam fires back, and the
//! configuration loader behind the cabinet-type callback.
//!
//! * [`AmInterface`]: `am::Interface::initialize` / `execute` / `finalize` plus the few handle calls the
//!   Manager makes itself (`isDevelop`, app root path).
//! * [`LibamEvent`]: the listener callbacks libam fires from inside `initialize` and `execute`, in emission
//!   order (`spec/controllers/lifecycle.md` §M5).
//! * [`ConfigLoader`]: `anon::load` (`0x140006230`) and the Config → `am::AppConfig` conversion
//!   (`0x140006380`) behind the cabinet-type callback.
//!
//! libam's diagnostics are `tracing` events in the daemon's process, not listener callbacks: the original's
//! `OutputLogEventListener` and `setDebugLogFilePath` have no counterpart here.

use std::fmt::Debug;

use crate::am_config::{AmConfig, WideStr};
use crate::config::{AppConfig, ProcessConfig};
use crate::error::FatalError;
use crate::result::AmResult;
use crate::sequence::ProcessState;

/// libamw start-up configuration (`Manager+0x38`, copied verbatim into the libam context by `0x14022CF50`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LibamwConfig {
    /// `wchar_t logDir[128]` (setter `0x140212730`): the `-l` directory with its trailing `\`; empty when `-l` is
    /// absent.
    pub log_dir: String,
    /// `wchar_t appName[32]` (setter `0x140212850`).
    pub app_name: String,
    /// `wchar_t appVersion[32]` (setter `0x140212930`).
    pub app_version: String,
}

/// Listener sinks the cabinet-type callback hands to libam through its `AppConfig` (steps 8 and 10 of
/// `0x140005B40`). A sink that is not installed never receives events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ListenerSinks {
    /// `AppConfig+0x2558`: `am::display::CompleteTask` → `EventListener+0x28`.
    pub display_complete: bool,
    /// `AppConfig+0x1FF8`: `am::sequence::NeedToExit` → `EventListener+0x18`.
    pub need_to_exit: bool,
    /// `AppConfig+0xC20`: `am::jvs::DetectNode` → `EventListener+0x20`.
    pub jvs_detect_node: bool,
    /// `AppConfig+0x1E00`: `am::usbio::DetectNode` → `UsbIOEventListener` (`EventListener+0x1A0`).
    pub usb_io_detect_node: bool,
}

/// The libam `am::AppConfig` (0x3A10 bytes) as the daemon sees it: the parts the callback itself writes plus
/// the parts the registered controllers touch.
#[derive(Debug, Clone, Default)]
pub struct CabinetAppConfig {
    /// Parts touched by the `appConfigCbs` registrants.
    pub controllers: AppConfig,
    /// `AppConfig+0x2548` (u8): `Config[0x507E0] != 0 || Config[0x50B14] != 0`, written in step 8.
    pub display_flag: bool,
    /// `AppConfig+0x2550` (u32): `max(Config[0x50974], Config[0x50CA8])`, written in step 8.
    pub display_value: u32,
    /// `AppConfig+0x37F8`: optional `abaas_log.root_path`, filled by the Config conversion.
    pub abaas_log_root: Option<String>,
    /// Sinks installed by steps 8 and 10.
    pub sinks: ListenerSinks,
    /// The full libam configuration the Config conversion (`0x140006380`) filled, every sub-struct of
    /// `am::AppConfig`. The daemon only carries it from [`ConfigLoader::convert`] to
    /// [`AmInterface::load_cabinet_type_result`]; it never reads it. The parts above and in
    /// [`Self::controllers`] are the same libam values written by the daemon itself; libam folds them in with
    /// [`Self::effective`].
    pub am: AmConfig,
}

impl CabinetAppConfig {
    /// The configuration libam validates and adopts: [`Self::am`] with the values the daemon wrote beside it
    /// (the controllers' parts, steps 8 and 10 of the callback and the abaas root) taking precedence, as in
    /// the original where all of them are fields of one object.
    pub fn effective(&self) -> AmConfig {
        let mut am = self.am.clone();
        let parts = &self.controllers;
        am.can.app = parts.can;
        am.usbio.app = parts.usb_io;
        am.usbdevice.app = parts.usb_device.clone();
        am.credit.app = parts.credit;
        am.network.app = parts.network;
        am.aime.app = parts.aime;
        am.emoney.app = parts.emoney;
        am.aimepay.app = parts.aime_pay;
        am.display.enable = self.display_flag;
        am.display.display_port = u64::from(self.display_value);
        am.display.complete_task_listener = self.sinks.display_complete;
        am.sequence.need_to_exit_listener = self.sinks.need_to_exit;
        am.jvs.detect_node_listener = self.sinks.jvs_detect_node;
        am.usbio.detect_node_listener = self.sinks.usb_io_detect_node;
        if let Some(root) = self.abaas_log_root.as_deref().and_then(|root| WideStr::new(root).ok()) {
            am.abaas_log.root_path = root;
        }
        am
    }
}

/// Arguments of `LoadCabinetTypeEventListener::operator()` (`0x140005B40`).
#[derive(Debug, Clone)]
pub struct LoadCabinetTypeEvent {
    /// `am::ModelType` optional (dongle model); `None` when not read.
    pub model: Option<u8>,
    /// `am::systemio::DipSwitch` raw value.
    pub dip_switch: u16,
    /// `am::Region` optional; only 1, 2 and 3 are meaningful.
    pub region: Option<u32>,
    /// libam's default-constructed `AppConfig`, filled by the callback.
    pub app_config: CabinetAppConfig,
}

/// A libam listener callback.
#[derive(Debug)]
pub enum LibamEvent {
    /// `am::sequence::ChangeStateEventListener` (`0x140005EC0`).
    ChangeState(ProcessState),
    /// `am::LoadCabinetTypeEventListener` (`0x140005B40`). libam waits for the answer, which the daemon gives
    /// through [`AmInterface::load_cabinet_type_result`].
    LoadCabinetType(Box<LoadCabinetTypeEvent>),
    /// `am::error::OccurFatalErrorEventListener` (`0x140005F00`).
    FatalError(Box<FatalError>),
    /// `am::sequence::NeedToExitEventListener` (`0x140005F40`): the reason; only `0..3` is routed.
    NeedToExit(u32),
    /// `am::jvs::DetectNodeEventListener` (`0x140005F90`).
    JvsDetectNode,
    /// `am::usbio::DetectNodeEventListener` (`UsbIOEventListener`, `0x140005580`).
    UsbIoDetectNode,
    /// `am::display::CompleteTaskEventListener` (`0x140005FE0`).
    DisplayCompleteTask,
    /// `am::storage::FixAppRootFolderPathEventListener` (`0x140006030`): the app root folder, `None` when libam
    /// passes a null path.
    FixAppRootFolderPath(Option<String>),
}

/// `am::Interface` as used by `arcade::Manager`.
pub trait AmInterface: Debug {
    /// `am::Interface::initialize` (`0x1402181C0`) with the libamw configuration. On success libam has started
    /// its main-flow state machine and synchronously fired the listener callbacks returned here (normally
    /// `ChangeState(1)`). Failure codes: 2 already initialised, 6 object creation failed, 1 invalid config.
    fn initialize(&mut self, config: &LibamwConfig) -> AmResult<Vec<LibamEvent>>;

    /// `am::Interface::execute` (`0x1402182F0`, one main-flow step): the callbacks fired during the step, in
    /// emission order. Does nothing when libam is not initialised.
    fn execute(&mut self) -> Vec<LibamEvent>;

    /// The answer to a [`LibamEvent::LoadCabinetType`]: `accepted` is the callback's return value and
    /// `app_config` the configuration it filled. libam validates and adopts it, or latches error code 4104.
    fn load_cabinet_type_result(&mut self, accepted: bool, app_config: CabinetAppConfig);

    /// `am::Interface::finalize` (`0x1402182B0`): terminates libam; idempotent.
    fn finalize(&mut self);

    /// `Manager::isDevelop` handle call (accessor k = 4 + `0x1402165B0`): the developer-dongle flag. `Err` when
    /// libam is not initialised.
    fn is_develop(&self) -> AmResult<bool>;

    /// `Manager::generateAppRootPath` handle call (accessor k = 16 + `0x140217990`): the app root folder libam
    /// resolved, `None` while it has not resolved one. `Err` when libam is not initialised.
    fn app_root_path(&self) -> AmResult<Option<String>>;
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
