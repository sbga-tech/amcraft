//! The configuration loader behind the cabinet-type callback (`spec/libam/config.md`).
//!
//! [`FileConfigLoader`] implements [`ConfigLoader`]:
//!
//! 1. the `-c` files (default `<exe dir>config.json`) are parsed and their `condition` objects are evaluated
//!    against the board (model, DIP switch, region);
//! 2. the matching files are merged into one JSON tree ([`merge`]) and, when `-m` is given, the tree is dumped
//!    ([`json::dump_pretty`]);
//! 3. the tree is validated into the typed [`Config`] and the cross-section rules are checked ([`mixed`]);
//! 4. [`LoadedConfig`] is derived from it. The full [`Config`] stays inside the loader for the later conversion
//!    into libam's `AppConfig`.

pub mod allnet;
pub mod basic;
pub mod can;
pub mod credit;
pub mod jvs;
pub mod network;
pub mod pay;
pub mod usb_device;
pub mod usb_io;

mod condition;
mod convert;
mod json;
mod merge;
mod mixed;
mod reader;
mod relation;

use std::fmt;

pub use self::convert::ConvertError;
use std::path::Path;
use std::sync::LazyLock;
use std::time::Instant;

use libam_api::config::ProcessConfig;
use libam_api::input::{InputConfig, InputRelation};
use libam_api::interface::{CabinetAppConfig, ConfigBytes, ConfigLoadRequest, ConfigLoader, LoadedConfig};
use libam_api::output::{OutputConfig, OutputRelation};
use libam_api::sequence::SequenceConfig;
use libam_api::usb_io::{UsbIoNodeFunctions, UsbIoProcessConfig, UsbIoUniqueFunction};

use self::allnet::{Accounting, Auth, Wifi};
use self::basic::{Backup, Common, ErrorConfig, Video};
use self::can::Can;
use self::condition::{Board, Condition};
use self::credit::Credit;
use self::json::{Json, Map};
use self::jvs::Jvs;
use self::network::{LanInstall, NetDelivery, Network};
use self::pay::{AbaasLog, Aime, AimePay, EMoney};
use self::usb_device::UsbDevice;
use self::usb_io::UsbIo;

/// Highest valid `am::ModelType` (`0x62`, `spec/libam/platform.md`).
const MAX_MODEL: u8 = 0x62;

/// Why a load failed. The first three are the file loader's result codes 2, 3 and 4; the others end the load
/// before such a code exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadError {
    /// A config file could not be opened (`CannotOpen`, 2): `Cannot open "<file>".`
    CannotOpen(String),
    /// A config file is not JSON (`InvalidFormat`, 3): `Cannot parse "<file>"...`
    InvalidFormat(String),
    /// A `condition`, a section value or a cross-section rule is invalid (`InvalidValue`, 4); the text is the
    /// message the original logs.
    InvalidValue(String),
    /// The model type is not valid (`invalid model type.`, fatal in the original).
    InvalidModel(u8),
    /// A merge exception (`Config merge error (<file>) at "<path>". <what>`), fatal in the original and never
    /// converted into a result code. The `-m` dump is not written for it.
    Merge(String),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CannotOpen(file) => write!(f, "Cannot open \"{file}\"."),
            Self::InvalidFormat(text) => write!(f, "Cannot parse {text}"),
            Self::InvalidValue(text) | Self::Merge(text) => f.write_str(text),
            Self::InvalidModel(model) => write!(f, "invalid model type. ({model})"),
        }
    }
}

impl std::error::Error for LoadError {}

impl From<reader::ConfigError> for LoadError {
    fn from(error: reader::ConfigError) -> Self {
        Self::InvalidValue(error.0)
    }
}

/// The typed `amdaemon::process::Config` (`0x50CE8` bytes in the original): every section, validated, with the
/// loader's defaults and fix-ups applied.
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub common: Common,
    pub error: ErrorConfig,
    pub sequence: SequenceConfig,
    pub video: Video,
    pub usb_device: UsbDevice,
    pub backup: Backup,
    pub jvs: Jvs,
    pub can: Can,
    pub usb_io: UsbIo,
    pub input: InputConfig,
    pub output: OutputConfig,
    pub credit: Credit,
    pub network: Network,
    pub lan_install: LanInstall,
    pub net_delivery: NetDelivery,
    pub auth: Auth,
    pub accounting: Accounting,
    pub wifi: Wifi,
    /// `allnet_air.enable`: `true` when the section object exists without the key.
    pub air: bool,
    pub aime: Aime,
    pub abaas_log: AbaasLog,
    pub emoney: EMoney,
    pub aimepay: AimePay,
    /// `apm.enable`: `true` when the section object exists without the key.
    pub apm: bool,
}

/// The name of the synthetic key the merge adds to every appended `input` / `output` device object. Only its
/// uniqueness matters (the validators read back the same string); it is visible in the `-m` dump.
fn tag_key() -> String {
    static START: LazyLock<Instant> = LazyLock::new(Instant::now);
    let ticks = START.elapsed().as_nanos();
    format!("__jvs_node_count.{ticks}")
}

impl Config {
    /// Validates the merged tree: `common`, then the other sections in the original's fixed order, then the
    /// prelude and the cross-section rules.
    fn from_tree(tree: &Map, exe_dir: &str, region: u32, tag_key: &str) -> Result<Self, LoadError> {
        let common = Common::parse(tree)?;
        let max_player = common.max_player;
        let mut cfg = Self { common, ..Self::default() };

        cfg.error = ErrorConfig::parse(tree)?;
        cfg.sequence = basic::parse_sequence(tree)?;
        cfg.video = Video::parse(tree)?;
        cfg.usb_device = UsbDevice::parse(tree)?;
        cfg.backup = Backup::parse(tree)?;
        cfg.jvs = Jvs::parse(tree)?;
        cfg.can = Can::parse(tree)?;
        cfg.usb_io = UsbIo::parse(tree)?;
        cfg.input = relation::parse_map::<InputRelation>(tree, "input", max_player, tag_key, relation::parse_input)?;
        cfg.output =
            relation::parse_map::<OutputRelation>(tree, "output", max_player, tag_key, relation::parse_output)?;
        cfg.credit = Credit::parse(tree, max_player)?;
        cfg.network = Network::parse(tree)?;
        cfg.lan_install = LanInstall::parse(tree)?;
        cfg.net_delivery = NetDelivery::parse(tree)?;
        cfg.auth = Auth::parse(tree)?;
        cfg.accounting = Accounting::parse(tree)?;
        cfg.wifi = Wifi::parse(tree)?;
        cfg.air = basic::parse_enable_only(tree, "allnet_air")?;
        cfg.aime = Aime::parse(tree)?;
        cfg.abaas_log = AbaasLog::parse(tree)?;
        cfg.emoney = EMoney::parse(tree, exe_dir)?;
        cfg.aimepay = AimePay::parse(tree, exe_dir)?;
        cfg.apm = basic::parse_enable_only(tree, "apm")?;

        // Prelude: defaults the JSON does not give.
        if cfg.common.language.is_empty() {
            cfg.common.language = match region {
                1 => "japanese",
                2 => "chinese",
                _ => "english",
            }
            .to_owned();
        }
        cfg.credit.fill_coin_box_assign(max_player);

        mixed::validate(&mut cfg).map_err(LoadError::InvalidValue)?;
        Ok(cfg)
    }

    /// The parts of the configuration other components consume directly.
    fn loaded(&self) -> LoadedConfig {
        let nodes = self
            .usb_io
            .nodes
            .iter()
            .map(|node| {
                let function = &node.required_function;
                UsbIoNodeFunctions {
                    pwm_enabled: function.pwm_output.is_some(),
                    pwm_slots: function.pwm_output.unwrap_or(0),
                    uniques: function
                        .unique_io
                        .iter()
                        .map(|u| UsbIoUniqueFunction { code: u.command, input: u.input, output: u.output })
                        .collect(),
                }
            })
            .collect();

        let mut process = ProcessConfig {
            debug_dump: self.common.log_output_request,
            sequence: self.sequence.clone(),
            usb_io: UsbIoProcessConfig { nodes },
            input: self.input.clone(),
            output: self.output.clone(),
            ..ProcessConfig::default()
        };
        process.network.allnet_air_enable = self.air;
        process.allnet_air.enable = self.air;
        process.emoney.aime_unit = self.emoney.aime_unit;

        // `byte_4ff8c` is the raw `allnet_auth` level: the callback merges the three levels itself.
        let bytes = ConfigBytes {
            byte_4ff8c: self.auth.log_level,
            byte_4ff98: self.accounting.log_level,
            byte_4ff9d: self.wifi.log_level,
            air_enabled: u8::from(self.air),
            display_flag_507e0: u8::from(self.emoney.enable),
            display_flag_50b14: u8::from(self.aimepay.enable),
            display_value_50974: self.emoney.display_port,
            display_value_50ca8: self.aimepay.display_port,
        };
        LoadedConfig { process, bytes }
    }
}

/// Reads, parses and merges the selected files into `tree`.
fn merge_files(paths: &[String], board: &Board, tag_key: &str, tree: &mut Map) -> Result<(), LoadError> {
    let mut matched = false;
    for file in paths {
        let text = std::fs::read(Path::new(file)).map_err(|_| LoadError::CannotOpen(file.clone()))?;
        let root = Json::parse(&text).map_err(|error| LoadError::InvalidFormat(format!("\"{file}\": {error}")))?;
        let condition = Condition::parse(&root)?;
        if !condition.matches(board) {
            continue;
        }
        matched = true;
        merge::merge_file(tree, &root, condition.jvs_node, tag_key)
            .map_err(|error| LoadError::Merge(error.render(file)))?;
    }
    if matched {
        Ok(())
    } else {
        Err(LoadError::InvalidValue("There is no file which matched in current board info.".to_owned()))
    }
}

/// `-m`: the merged tree, pretty-printed. A path that cannot be written is silently skipped.
fn write_dump(path: &str, tree: &Map) {
    if !path.is_empty() {
        let _ = std::fs::write(path, json::dump_pretty(tree));
    }
}

/// The whole load: files, merge, dump, validation.
fn load_config(request: &ConfigLoadRequest<'_>) -> Result<Config, LoadError> {
    if !(1..=MAX_MODEL).contains(&request.model) {
        return Err(LoadError::InvalidModel(request.model));
    }
    let board = Board { dip_switch: request.dip_switch, model: request.model, region: request.region };
    let default_paths;
    let paths = if request.config_paths.is_empty() {
        default_paths = [format!("{}config.json", request.exe_dir)];
        &default_paths[..]
    } else {
        request.config_paths
    };

    let tag_key = tag_key();
    let mut tree = Map::new();
    let merged = merge_files(paths, &board, &tag_key, &mut tree);
    // The dump follows the file loop whatever its result, but not a merge exception.
    if !matches!(merged, Err(LoadError::Merge(_))) {
        write_dump(request.merged_dump_path, &tree);
    }
    merged?;
    Config::from_tree(&tree, request.exe_dir, request.region, &tag_key)
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
