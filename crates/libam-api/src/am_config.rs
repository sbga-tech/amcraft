//! The full typed libam `am::AppConfig` (`0x3A10` bytes, `spec/libam/core.md` §1-§4) as values.
//!
//! The original is a plain aggregate of 25 sub-structs built back to back by the default constructor
//! `0x14029E620`; this module has one type per sub-struct, in the same order, with the same defaults
//! ([`AmConfig::default`], spec §2). Fixed arrays with a `u64` count become [`BoundedList`]s whose `push`
//! fails with `am::Result` code 2 when full (the "append" setters); `wchar_t[N]` buffers become [`WideStr`]s
//! whose constructor fails with code 1 when the text does not fit (the "setter fails status 1" rows);
//! `Optional` objects are `Option`s; listener pointers are `bool`s ("installed").
//!
//! Padding and indeterminate bytes are not represented: nothing in libam reads them (spec §0).
//!
//! The parts of the configuration the daemon's controllers touch have their own types in the facade modules
//! (`UsbIoAppConfig`, `CreditAppConfig`, ...); the sub-structs embed them in their `app` field, so there is
//! one type per libam configuration. [`CabinetAppConfig::effective`](crate::interface::CabinetAppConfig::effective)
//! folds the values the daemon wrote beside [`AmConfig`] into one configuration.
//!
//! [`AmConfig::validate`] is the `AppConfigValidator::isValid` chain (spec Part V), first failing rule wins.

use std::ops::Deref;

use crate::aime::AimeAppConfig;
use crate::aime_pay::AimePayAppConfig;
use crate::can::CanAppConfig;
use crate::credit::CreditAppConfig;
use crate::emoney::EMoneyAppConfig;
use crate::network::{NetworkAppConfig, NetworkProperty};
use crate::result::{AmError, AmResult};
use crate::usb_device::UsbDeviceAppConfig;
use crate::usb_io::UsbIoAppConfig;

/// The `0xFFFFFFFF` "unset" sentinel of the `u32` credit rule values (`dword_1406A14B8`).
pub const UNSET_U32: u32 = u32::MAX;
/// The `0xFFFFFFFFFFFFFFFF` "unset" sentinel of the `u64` values (`qword_1406A15F8`).
pub const UNSET_U64: u64 = u64::MAX;

/// A fixed-capacity list: the `{u64 count; T items[N]}` pairs of the configuration, appended one by one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedList<T, const N: usize>(Vec<T>);

impl<T, const N: usize> Default for BoundedList<T, N> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<T, const N: usize> BoundedList<T, N> {
    /// Capacity of the original's array.
    pub const CAPACITY: usize = N;

    /// The append setters: `am::Result` code 2 ("full") when `N` items are present.
    pub fn push(&mut self, item: T) -> AmResult<()> {
        if self.0.len() >= N {
            return Err(AmError::with_message(AmError::NOT_INITIALIZED, "list is full"));
        }
        self.0.push(item);
        Ok(())
    }

    /// The items appended so far (`count` of them).
    pub fn as_slice(&self) -> &[T] {
        &self.0
    }

    /// Number of appended items (the `count` word).
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// No item appended.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Iterates the appended items.
    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.0.iter()
    }
}

impl<'a, T, const N: usize> IntoIterator for &'a BoundedList<T, N> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// A `wchar_t[CAP]` string: at most `CAP - 1` UTF-16 code units plus the terminator.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WideStr<const CAP: usize>(String);

impl<const CAP: usize> WideStr<CAP> {
    /// Setter semantics: `am::Result` code 1 when the text needs `CAP` or more UTF-16 code units.
    pub fn new(text: &str) -> AmResult<Self> {
        if text.encode_utf16().count() >= CAP {
            return Err(AmError::with_message(AmError::INVALID_ARGUMENT, "string does not fit the buffer"));
        }
        Ok(Self(text.to_owned()))
    }

    /// The text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Number of UTF-16 code units (`wcslen`).
    pub fn units(&self) -> usize {
        self.0.encode_utf16().count()
    }
}

impl<const CAP: usize> Deref for WideStr<CAP> {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl<const CAP: usize> TryFrom<&str> for WideStr<CAP> {
    type Error = AmError;

    fn try_from(text: &str) -> AmResult<Self> {
        Self::new(text)
    }
}

/// An `OptPair` (`{u8 valid; u64 a; u64 b}`, spec §4.6) payload.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Pair {
    /// First value (`a`).
    pub a: u64,
    /// Second value (`b`).
    pub b: u64,
}

/// `common` (`am::common::AppConfig`, `0x0`, 696 bytes, spec §3.1).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommonConfig {
    /// Number of play slots (`max_player`, validated `<= 8`).
    pub max_player: u64,
    /// The resource block.
    pub resource: CommonResource,
}

/// `CommonResource` (688 bytes, spec §4.1).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommonResource {
    /// Resource folder (`<exeDir>\am_resources\` for the daemon).
    pub resource_path: WideStr<260>,
    /// Language name (`japanese`, `chinese`, ...), at most 15 code units.
    pub language: WideStr<16>,
    /// Custom resource ids (`WStr16[4]`), appended by `0x140214BD0`.
    pub custom_resource: BoundedList<WideStr<16>, 4>,
}

/// `error` (`am::error::Config`, `0x2B8`, 2 bytes, spec §3.2).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ErrorConfig {
    /// `auto_reset`.
    pub auto_reset: bool,
    /// `raise_in_test`.
    pub raise_in_test: bool,
}

/// `Bytes12` (spec §4.3): `{u8 b0; u8 b2_10[8]; u16 w10}`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Bytes12 {
    /// Byte at `+0`.
    pub b0: u8,
    /// Bytes at `+2..+10`.
    pub b2_10: [u8; 8],
    /// Word at `+10`.
    pub w10: u16,
}

/// `bnam` (`am::bnam::Config`, `0x2BA`, spec §3.3). The daemon never fills it; the validator requires it
/// disabled.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BnamConfig {
    /// `enable`; must be `false`.
    pub enable: bool,
    /// 12-byte value of setter `0x1402AC2D0` (`[U]`, no caller).
    pub id12: Bytes12,
    /// Platform id, at most 30 code units.
    pub platform_id: WideStr<31>,
    /// Main id, at most 30 code units.
    pub main_id: WideStr<31>,
    /// NVRAM / system-record base folder, used only when `enable`.
    pub path: WideStr<260>,
}

/// `JvsNode` (104 bytes, spec §4.5): one required function set of a JVS board.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JvsNode {
    /// `share_coin_slot` (`[I]`).
    pub share_coin_slot: bool,
    /// `auto_output_zero` (`nodeAutoOutputZero` reads it); default `true`.
    pub auto_output_zero: bool,
    /// Switch input: `a` = players, `b` = buttons.
    pub switch_input: Option<Pair>,
    /// Coin input: the slot.
    pub coin_input: Option<u64>,
    /// Analog input: `a` = channel.
    pub analog_input: Option<Pair>,
    /// Rotary input: the channel.
    pub rotary_input: Option<u64>,
    /// General driver output: the slot.
    pub general_driver: Option<u64>,
}

impl Default for JvsNode {
    fn default() -> Self {
        Self {
            share_coin_slot: false,
            auto_output_zero: true,
            switch_input: None,
            coin_input: None,
            analog_input: None,
            rotary_input: None,
            general_driver: None,
        }
    }
}

/// `JvsTotal` (432 bytes, spec §4.4): one board of the JVS configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JvsTotal {
    /// `valid`: at least one node was converted.
    pub valid: bool,
    /// The converted nodes (`node_count`, at most 4).
    pub nodes: BoundedList<JvsNode, 4>,
}

/// `jvs` (`am::jvs::Config`, `0x550`, spec §3.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JvsConfig {
    /// `enable`.
    pub enable: bool,
    /// Lowest total id (1..4) enabled in the process config; 0 if none.
    pub default_total: u64,
    /// Totals #1..#4. The default constructor marks total #1 valid.
    pub totals: [JvsTotal; 4],
    /// `detect_node_listener` installed (written by the cabinet-type callback).
    pub detect_node_listener: bool,
}

impl Default for JvsConfig {
    fn default() -> Self {
        let mut totals: [JvsTotal; 4] = Default::default();
        totals[0].valid = true;
        Self { enable: false, default_total: 0, totals, detect_node_listener: false }
    }
}

/// `CanTarget` (48 bytes, spec §4.10).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CanTarget {
    /// Target type `1..=5` (config type `0..4` maps to `1..5`, anything else to 0).
    pub kind: u32,
    /// Target id (`1..=29` once validated).
    pub id: u8,
    /// Product name, at most 8 code units.
    pub product: WideStr<9>,
    /// Second `wchar[9]` string (setter `0x14021C4F0`; `[U]`, not written by the converter).
    pub name2: WideStr<9>,
    /// Byte at `+42` (`[U]`, setter `0x14021C580`, no caller).
    pub b42: u8,
    /// Word at `+44` (`[U]`, set with `b42`).
    pub w44: u16,
}

/// `CanNode` (80 bytes, spec §4.9).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CanNode {
    /// Target identification.
    pub target: CanTarget,
    /// Auto-reply interval (`0..=255` once validated).
    pub interval: u64,
    /// Bit 0 switch input, 1 analog A, 2 analog B, 3 rotary.
    pub input_mask: u32,
    /// Coin slot total (`0..=2` once validated).
    pub coin_input: u64,
    /// `auto_reply` (`[I]`).
    pub auto_reply: bool,
}

/// `CanBoard` (1296 bytes, spec §4.8): one CAN port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanBoard {
    /// Port number (`1..=0x20` once validated); default 1.
    pub number: u64,
    /// The nodes (at most 16).
    pub nodes: BoundedList<CanNode, 16>,
}

impl Default for CanBoard {
    fn default() -> Self {
        Self { number: 1, nodes: BoundedList::default() }
    }
}

/// `can` (`am::can::Config`, `0xC28`, spec §3.5).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CanConfig {
    /// `enable`.
    pub enable: bool,
    /// The ports (at most 2).
    pub ports: BoundedList<CanBoard, 2>,
    /// Packet listeners installed by the CAN controller.
    pub app: CanAppConfig,
}

/// `UsbioNode` (352 bytes, spec §4.11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbIoNode {
    /// Node id.
    pub id: u8,
    /// `share_coin_slot` (`[I]`).
    pub share_coin_slot: bool,
    /// `auto_output_zero`; default `true`.
    pub auto_output_zero: bool,
    /// Communication timeout (`Node::sendSetCommTimeout`).
    pub comm_timeout: Option<u8>,
    /// Switch sampling (`Node::sendSetSwitchSampling`).
    pub switch_sampling: Option<u8>,
    /// PWM cycle (`Node::sendPwmOutput`).
    pub pwm_cycle: Option<u8>,
    /// Required functions.
    pub funcs: UsbIoFuncs,
}

impl Default for UsbIoNode {
    fn default() -> Self {
        Self {
            id: 0,
            share_coin_slot: false,
            auto_output_zero: true,
            comm_timeout: None,
            switch_sampling: None,
            pwm_cycle: None,
            funcs: UsbIoFuncs::default(),
        }
    }
}

/// `UsbioFuncs` (336 bytes, spec §4.13).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsbIoFuncs {
    /// Switch input: `a` = players, `b` = buttons.
    pub switch_input: Option<Pair>,
    /// Coin input: the slot.
    pub coin_input: Option<u64>,
    /// Analog input: `a` = channel.
    pub analog_input: Option<Pair>,
    /// Rotary input: the channel.
    pub rotary_input: Option<u64>,
    /// General output: the slot.
    pub general_output: Option<u64>,
    /// PWM output: the slot.
    pub pwm_output: Option<u64>,
    /// Unique I/O list (at most 9).
    pub unique_io: BoundedList<UniqueCmd, 9>,
}

/// `UniqueCmd` (24 bytes, spec §4.15).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UniqueCmd {
    /// `valid`: set by `0x14021EDE0(command)`.
    pub valid: bool,
    /// Command byte.
    pub command: u8,
    /// Parameters appended by `0x14021EDF0` (at most 3).
    pub params: [u8; 3],
    /// Number of parameters.
    pub param_count: u8,
}

impl UniqueCmd {
    /// `0x14021EDE0(command)`: a valid command without parameters.
    pub fn new(command: u8) -> Self {
        Self { valid: true, command, params: [0; 3], param_count: 0 }
    }

    /// `0x14021EDF0`: appends one parameter; code 1 when three are present.
    pub fn push_param(&mut self, value: u8) -> AmResult<()> {
        let index = usize::from(self.param_count);
        if index >= self.params.len() {
            return Err(AmError::with_message(AmError::INVALID_ARGUMENT, "parameter list is full"));
        }
        self.params[index] = value;
        self.param_count += 1;
        Ok(())
    }
}

/// `usbio` (`am::usbio::Config`, `0x1668`, spec §3.6).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsbIoConfig {
    /// `enable`.
    pub enable: bool,
    /// `append_sub_number_to_device_error`.
    pub append_sub_number_to_device_error: bool,
    /// The nodes (at most 4).
    pub nodes: BoundedList<UsbIoNode, 4>,
    /// USB-IO firmware file path.
    pub firmware_path: WideStr<260>,
    /// `detect_node_listener` installed (written by the cabinet-type callback).
    pub detect_node_listener: bool,
    /// Firmware update listener installed by the USBIO controller.
    pub app: UsbIoAppConfig,
}

/// `GenericIoEntry` (32 bytes, spec §4.16).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GenericIoEntry {
    /// Flag byte (`[U]`).
    pub flag: bool,
    /// `U32List4` (spec §4.17), appended by `0x1402C5470`.
    pub list: BoundedList<u32, 4>,
}

/// `genericio` (`am::genericio::Config`, `0x1E10`, spec §3.7). The daemon never fills it; the validator
/// requires it disabled.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GenericIoConfig {
    /// `enable`; must be `false`.
    pub enable: bool,
    /// The entries (at most 4).
    pub entries: BoundedList<GenericIoEntry, 4>,
}

/// `nvram` (`am::nvram::Config`, `0x1EA0`, spec §3.8): the `backup` flags.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NvramConfig {
    /// `clear_eeprom_at_first_launch`.
    pub clear_eeprom_at_first_launch: bool,
    /// `clear_busram_at_first_launch`.
    pub clear_busram_at_first_launch: bool,
}

/// `CreditRule` (64 bytes, spec §4.18). [`UNSET_U32`] means "unset".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreditRule {
    /// Coin chute share type: 1 common, 2 individual.
    pub chute_share_type: u32,
    /// Service share type: 1 common, 2 individual.
    pub service_share_type: u32,
    /// Operation: 1 normal, 2 free play.
    pub operation: u32,
    /// Per chute multiplier.
    pub coin_chute_multiplier: [u32; 2],
    /// Coins per credit.
    pub coin_to_credit: u32,
    /// Bonus adder.
    pub bonus_adder: u32,
    /// Game cost per play slot.
    pub game_cost: [u32; 8],
    /// Coin amount (set only by libam's credit module).
    pub coin_amount: u32,
}

impl Default for CreditRule {
    fn default() -> Self {
        Self {
            chute_share_type: 0,
            service_share_type: 0,
            operation: 0,
            coin_chute_multiplier: [UNSET_U32; 2],
            coin_to_credit: UNSET_U32,
            bonus_adder: UNSET_U32,
            game_cost: [UNSET_U32; 8],
            coin_amount: UNSET_U32,
        }
    }
}

/// `CreditAs6db` (32 bytes, spec §4.19): the coin selector AS-6DB block.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CreditAs6db {
    /// Present / enabled.
    pub enable: bool,
    /// COM port (the `u16` of the process config, `[I]`).
    pub arg: u64,
    /// Entry types appended by `0x14021D400` (values 1, 2, 3, 0), at most 2.
    pub types: BoundedList<u32, 2>,
}

/// `credit` (`am::credit::Config`, `0x1EA8`, spec §3.9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreditConfig {
    /// `enable`.
    pub enable: bool,
    /// `max_credit` ([`UNSET_U32`] unset).
    pub max_credit: u32,
    /// Coin box per play slot ([`UNSET_U64`] = unassigned).
    pub coin_box_assign: [u64; 8],
    /// Credit rule block.
    pub rule: CreditRule,
    /// Polls batched before the insert-coin listener is invoked (`[I]`); default 1.
    pub notify_interval: u32,
    /// Coin selector AS-6DB block.
    pub coin_selector_as6db: CreditAs6db,
    /// Insert-coin listener installed by the credit controller.
    pub app: CreditAppConfig,
}

impl Default for CreditConfig {
    fn default() -> Self {
        Self {
            enable: false,
            max_credit: UNSET_U32,
            coin_box_assign: [UNSET_U64; 8],
            rule: CreditRule::default(),
            notify_interval: 1,
            coin_selector_as6db: CreditAs6db::default(),
            app: CreditAppConfig::default(),
        }
    }
}

/// `sequence` (`am::sequence::Config`, `0x1F60`, spec §3.10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequenceCfg {
    /// `save_total_time_on_playing`; default `true`.
    pub save_total_time_on_playing: bool,
    /// Expanded histogram bucket widths (`histogram_rule` width repeated count times, at most 32).
    pub histogram: BoundedList<u32, 32>,
    /// `first_launch_listener` installed (nothing installs it).
    pub first_launch_listener: bool,
    /// `need_to_exit_listener` installed (written by the cabinet-type callback).
    pub need_to_exit_listener: bool,
}

impl Default for SequenceCfg {
    fn default() -> Self {
        Self {
            save_total_time_on_playing: true,
            histogram: BoundedList::default(),
            first_launch_listener: false,
            need_to_exit_listener: false,
        }
    }
}

/// `usbdevice` (`am::usbdevice::Config`, `0x2000`, spec §3.11): the required sets and the completion
/// listener, both of which the USB device controller's types already carry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsbDeviceConfig {
    /// Required sets (`required_count` of them) and the `CompleteTask` listener.
    pub app: UsbDeviceAppConfig,
}

/// `Resolution` (8 bytes, spec §4.21).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Resolution {
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
}

/// `video` (`am::video::Config`, `0x2520`, spec §3.12).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VideoConfig {
    /// `enable`.
    pub enable: bool,
    /// Display mode 0/1/2.
    pub mode: u32,
    /// Primary monitor resolution.
    pub resolution: Resolution,
    /// Secondary monitor resolution.
    pub secondary_resolution: Resolution,
    /// Primary monitor angle (0, 90, 180 or 270 once validated).
    pub angle: u16,
    /// Secondary monitor angle.
    pub secondary_angle: u16,
    /// `ignore_unconnected`.
    pub ignore_unconnected: bool,
}

/// `storage` (`am::storage::Config`, `0x2540`, spec §3.13). The daemon never fills it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StorageConfig {
    /// Enables the FixAppRootFolderPath step of the storage manager (`[I]`).
    pub fix_app_root_folder: bool,
}

/// `display` (`am::display::Config`, `0x2548`, spec §3.14); written by the cabinet-type callback.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DisplayConfig {
    /// `enable`: `emoney.enable || aimepay.enable`.
    pub enable: bool,
    /// Display COM port (`max(emoney.display_port, aimepay.display_port)`).
    pub display_port: u64,
    /// `complete_task_listener` installed.
    pub complete_task_listener: bool,
    /// `message_listener` installed (nothing installs it).
    pub message_listener: bool,
}

/// `network` (`am::network::Config`, `0x2568`, spec §3.15).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetworkConfig {
    /// `enable`.
    pub enable: bool,
    /// `support_standalone` ("network mass", `[I]`).
    pub support_standalone: bool,
    /// `use_gateway`; default `true`.
    pub use_gateway: bool,
    /// `use_location_router`.
    pub use_location_router: bool,
    /// `location_router_directly`: 1 or 2, any other value maps to 0.
    pub location_router_directly: u32,
    /// The static IP property (`has_property` + `property`).
    pub property: Option<NetworkProperty>,
    /// `dhcp_if_unset` (`[I]`).
    pub dhcp_if_unset: bool,
    /// Test event listener installed by the network controller.
    pub app: NetworkAppConfig,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            enable: false,
            support_standalone: false,
            use_gateway: true,
            use_location_router: false,
            location_router_directly: 0,
            property: None,
            dhcp_if_unset: false,
            app: NetworkAppConfig::default(),
        }
    }
}

/// `AllnetVersion` (48 bytes, spec §4.23): the application version reported to ALL.Net.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllnetVersion {
    /// `valid`: set by `0x14021F550` when the packed value is valid.
    pub valid: bool,
    /// Packed value: byte 0 third component, byte 1 minor, `u16` at bytes 2..3 major.
    pub packed: u32,
    /// `"%04u.%02u.%02u"` text of the packed value; default `----.--.--`.
    pub long_text: String,
    /// Short text of the packed value; default `----.--`.
    pub short_text: String,
}

impl Default for AllnetVersion {
    fn default() -> Self {
        Self { valid: false, packed: 0, long_text: "----.--.--".to_owned(), short_text: "----.--".to_owned() }
    }
}

/// `allnet_auth` (`am::allnet::AuthConfig`, `0x2598`, spec §3.16).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AllnetAuthConfig {
    /// `enable`.
    pub enable: bool,
    /// Line support bits: 1 broadband, 2 mobile, 3 xdsl (`0x140012F40`).
    pub support_line: u32,
    /// `develop_version` (`[I]`).
    pub develop_version: bool,
    /// Application version reported to ALL.Net.
    pub version: AllnetVersion,
    /// `multi_auth_method` `0..=3`.
    pub multi_auth_method: u32,
    /// `lite_auth_key`.
    pub lite_auth_key: [u8; 16],
    /// `type`: 0 is auth 1.0, 1 is auth 2.0 (2 and above fail validation).
    pub auth_type: u32,
}

/// `allnet_accounting` (`am::allnet::AccountingConfig`, `0x25EC`, spec §3.17).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AllnetAccountingConfig {
    /// `mode`: 0 disabled, `1..=3` accounting type.
    pub mode: u32,
    /// Interval in seconds; default 120.
    pub interval: u32,
}

impl Default for AllnetAccountingConfig {
    fn default() -> Self {
        Self { mode: 0, interval: 120 }
    }
}

/// `allnet_wifi` (`am::allnet::WifiConfig`, `0x25F4`, spec §3.18).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AllnetWifiConfig {
    /// `enable`.
    pub enable: bool,
}

/// `laninstall` (`am::laninstall::Config`, `0x25F8`, spec §3.19).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanInstallConfig {
    /// `mode`: `lan_install.server != 0` as 0/1 (1 is the client).
    pub mode: u32,
    /// `interval`; default 20.
    pub interval: u64,
}

impl Default for LanInstallConfig {
    fn default() -> Self {
        Self { mode: 0, interval: 20 }
    }
}

/// `netdeliver` (`am::netdeliver::Config`, `0x2608`, spec §3.20).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NetDeliverConfig {
    /// `enable`.
    pub enable: bool,
    /// `local`.
    pub local: bool,
    /// Constant 1 the converter stores (`[I]`).
    pub const_one: bool,
    /// `use_aws_signature`.
    pub use_aws_signature: bool,
}

/// `AimeUnit` (24 bytes, spec §4.24): one card reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AimeUnit {
    /// COM port (`1..=0x100` once validated); default [`UNSET_U64`].
    pub com_port: u64,
    /// Unit index (`1..=6` once validated); default 1.
    pub unit_index: u64,
    /// Third unit parameter (process-config unit key `required`, `[I]`).
    pub flag: bool,
}

impl Default for AimeUnit {
    fn default() -> Self {
        Self { com_port: UNSET_U64, unit_index: 1, flag: false }
    }
}

/// `aime` (`am::aime::Config`, `0x2610`, spec §3.21).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AimeConfig {
    /// `enable`.
    pub enable: bool,
    /// Second flag (`[U]`, read by the account loader).
    pub b1: bool,
    /// `high_baudrate`.
    pub high_baudrate: bool,
    /// `polling_interval`; default 100.
    pub polling_interval: u32,
    /// Card accept time in seconds (`1..=0x20C49B`); default 30.
    pub card_accept_time: u32,
    /// Firmware paths, at most three of at most 259 code units (the original joins them into one
    /// `wchar[780]`).
    pub firmware_paths: BoundedList<WideStr<260>, 3>,
    /// The reader units (at most 8).
    pub units: BoundedList<AimeUnit, 8>,
    /// `log_enable`.
    pub log_enable: bool,
    /// Log root path (absolute).
    pub log_root_path: WideStr<260>,
    /// Listeners installed by the Aime controller.
    pub app: AimeAppConfig,
}

impl AimeConfig {
    /// `has_firmware_path`: at least one firmware path was appended.
    pub fn has_firmware_path(&self) -> bool {
        !self.firmware_paths.is_empty()
    }
}

impl Default for AimeConfig {
    fn default() -> Self {
        Self {
            enable: false,
            b1: false,
            high_baudrate: false,
            polling_interval: 100,
            card_accept_time: 30,
            firmware_paths: BoundedList::default(),
            units: BoundedList::default(),
            log_enable: false,
            log_root_path: WideStr::default(),
            app: AimeAppConfig::default(),
        }
    }
}

/// `emoney` (`am::emoney::Config`, `0x2F38`, spec §3.22).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EMoneyConfig {
    /// `enable`.
    pub enable: bool,
    /// Second flag (`[U]`, read by `emoney::Manager::setup`).
    pub b1: bool,
    /// Per brand slot enable; default all `true`.
    pub brand_enabled: [bool; 8],
    /// Executable directory with trailing backslash.
    pub root_path: WideStr<260>,
    /// Resource folder.
    pub resource_path: WideStr<260>,
    /// `log.level`.
    pub log_level: u32,
    /// `log.root_path` (absolute).
    pub log_root_path: WideStr<260>,
    /// Aime reader index used for e-money (validated against the Aime unit count).
    pub aime_unit: u64,
    /// Listeners installed by the e-money controller.
    pub app: EMoneyAppConfig,
}

impl Default for EMoneyConfig {
    fn default() -> Self {
        Self {
            enable: false,
            b1: false,
            brand_enabled: [true; 8],
            root_path: WideStr::default(),
            resource_path: WideStr::default(),
            log_level: 0,
            log_root_path: WideStr::default(),
            aime_unit: 0,
            app: EMoneyAppConfig::default(),
        }
    }
}

/// `aimepay` (`am::aimepay::Config`, `0x3590`, spec §3.23).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AimePayConfig {
    /// `enable`.
    pub enable: bool,
    /// Resource folder.
    pub resource_path: WideStr<260>,
    /// Coin item ids (`AimePayItemId[2]`, at most 13 code units each).
    pub coin_item_id: BoundedList<WideStr<14>, 2>,
    /// Listeners installed by the Aime Pay controller.
    pub app: AimePayAppConfig,
}

/// `abaas_log` (`am::abaas::LogConfig`, `0x37F8`, spec §3.24).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbaasLogConfig {
    /// `enable`.
    pub enable: bool,
    /// Log root path (the setter needs fewer than 200 code units).
    pub root_path: WideStr<200>,
    /// `use_network`; default `true`.
    pub use_network: bool,
    /// Log level `0..=3`.
    pub level: u32,
}

impl Default for AbaasLogConfig {
    fn default() -> Self {
        Self { enable: false, root_path: WideStr::default(), use_network: true, level: 0 }
    }
}

/// `apm` (`am::apm::Config`, `0x3A08`, spec §3.25).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ApmConfig {
    /// `enable`.
    pub enable: bool,
}

/// libam `am::AppConfig`: the 25 sub-structs in the order of the default constructor `0x14029E620`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AmConfig {
    /// `common` at `0x0`.
    pub common: CommonConfig,
    /// `error` at `0x2B8`.
    pub error: ErrorConfig,
    /// `bnam` at `0x2BA`.
    pub bnam: BnamConfig,
    /// `jvs` at `0x550`.
    pub jvs: JvsConfig,
    /// `can` at `0xC28`.
    pub can: CanConfig,
    /// `usbio` at `0x1668`.
    pub usbio: UsbIoConfig,
    /// `genericio` at `0x1E10`.
    pub genericio: GenericIoConfig,
    /// `nvram` at `0x1EA0`.
    pub nvram: NvramConfig,
    /// `credit` at `0x1EA8`.
    pub credit: CreditConfig,
    /// `sequence` at `0x1F60`.
    pub sequence: SequenceCfg,
    /// `usbdevice` at `0x2000`.
    pub usbdevice: UsbDeviceConfig,
    /// `video` at `0x2520`.
    pub video: VideoConfig,
    /// `storage` at `0x2540`.
    pub storage: StorageConfig,
    /// `display` at `0x2548`.
    pub display: DisplayConfig,
    /// `network` at `0x2568`.
    pub network: NetworkConfig,
    /// `allnet_auth` at `0x2598`.
    pub allnet_auth: AllnetAuthConfig,
    /// `allnet_accounting` at `0x25EC`.
    pub allnet_accounting: AllnetAccountingConfig,
    /// `allnet_wifi` at `0x25F4`.
    pub allnet_wifi: AllnetWifiConfig,
    /// `laninstall` at `0x25F8`.
    pub laninstall: LanInstallConfig,
    /// `netdeliver` at `0x2608`.
    pub netdeliver: NetDeliverConfig,
    /// `aime` at `0x2610`.
    pub aime: AimeConfig,
    /// `emoney` at `0x2F38`.
    pub emoney: EMoneyConfig,
    /// `aimepay` at `0x3590`.
    pub aimepay: AimePayConfig,
    /// `abaas_log` at `0x37F8`.
    pub abaas_log: AbaasLogConfig,
    /// `apm` at `0x3A08`.
    pub apm: ApmConfig,
}

/// The first rule of `AppConfigValidator::isValid` that failed (spec Part V). libam latches error 4104 for it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("app config rule {rule} failed: {reason}")]
pub struct AmConfigError {
    /// Spec rule id (`V01.1` .. `V20.1`).
    pub rule: &'static str,
    /// What is wrong.
    pub reason: String,
}

fn fail(rule: &'static str, reason: impl Into<String>) -> Result<(), AmConfigError> {
    Err(AmConfigError { rule, reason: reason.into() })
}

/// Highest COM port number of a VFD display, Aime reader or AS-6DB.
const MAX_COM_PORT: u64 = 0x100;
/// Largest Aime card accept time (`0x20C49B` seconds).
const MAX_CARD_ACCEPT_TIME: u32 = 0x20C49B;

impl AmConfig {
    /// `AppConfigValidator::isValid(AppConfig)` (`0x1402AC980`, spec Part V): the 20 overloads in table order,
    /// the first failing rule wins. Blocks the original does not validate (`error`, `nvram`, `sequence`,
    /// `storage`, `abaas_log`, `apm`) are not checked here either.
    pub fn validate(&self) -> Result<(), AmConfigError> {
        self.check_common_credit_accounting()?; // 1  0x1402ACBB0
        self.check_bnam()?; // 2  0x1402ACE70
        self.check_video()?; // 3  0x1402ACF30
        self.check_jvs()?; // 4  0x1402AD040
        self.check_can()?; // 5  0x1402AD4C0
        self.check_usbio()?; // 6  0x1402AD9E0
        self.check_genericio()?; // 7  0x1402AE020
        self.check_credit()?; // 8  0x1402AE0E0
        self.check_display()?; // 9  0x1402AE860
        self.check_network()?; // 10 0x1402AE990
        self.check_network_laninstall()?; // 11 0x1402AEA50
        self.check_network_auth()?; // 12 0x1402AEBF0
        self.check_auth_accounting()?; // 13 0x1402AECD0
        self.check_auth_wifi()?; // 14 0x1402AED90
        self.check_deliver()?; // 15 0x1402AEE50
        self.check_aime()?; // 16 0x1402AF2C0
        self.check_emoney()?; // 17 0x1402AF5B0
        self.check_aimepay()?; // 18 0x1402AF950
        self.check_credit_aime_display()?; // 19 0x1402AFCE0
        self.check_usbdevice() // 20 0x1402AFF80
    }

    fn check_common_credit_accounting(&self) -> Result<(), AmConfigError> {
        let players = self.common.max_player;
        if self.common.resource.language.is_empty() {
            return fail("V01.1", "language is empty");
        }
        if players > 8 {
            return fail("V01.2", format!("player total {players} exceeds 8"));
        }
        if self.credit.enable {
            let boxes = self.credit.coin_box_assign.iter().filter(|&&slot| slot != UNSET_U64 && slot < 4).count();
            if (players as u128) < boxes as u128 {
                return fail("V01.3", format!("coin box total {boxes} is more than player total {players}"));
            }
        }
        if self.allnet_accounting.mode != 0 && players == 0 {
            return fail("V01.4", "player total is 0 while ALL.Net accounting is enabled");
        }
        Ok(())
    }

    fn check_bnam(&self) -> Result<(), AmConfigError> {
        if self.bnam.enable {
            return fail("V02.1", "BNAM must be disabled");
        }
        Ok(())
    }

    fn check_video(&self) -> Result<(), AmConfigError> {
        if !self.video.enable {
            return Ok(());
        }
        for angle in [self.video.angle, self.video.secondary_angle] {
            if !matches!(angle, 0 | 90 | 180 | 270) {
                return fail("V03.1", format!("display angle {angle} is not 0, 90, 180 or 270"));
            }
        }
        Ok(())
    }

    fn check_jvs(&self) -> Result<(), AmConfigError> {
        if !self.jvs.enable {
            return Ok(());
        }
        for total in &self.jvs.totals {
            if !total.valid || total.nodes.is_empty() {
                continue;
            }
            for node in &total.nodes {
                let any = node.switch_input.is_some()
                    || node.coin_input.is_some()
                    || node.analog_input.is_some()
                    || node.rotary_input.is_some()
                    || node.general_driver.is_some();
                if !any {
                    return fail("V04.1", "all functions are disabled");
                }
                if let Some(switch) = node.switch_input
                    && switch.a == 0
                    && switch.b != 0
                {
                    return fail("V04.2", format!("switch button {} set while player is 0", switch.b));
                }
                if node.coin_input == Some(0) {
                    return fail("V04.3", "coin input slot is 0");
                }
                if node.analog_input.is_some_and(|analog| analog.a == 0) {
                    return fail("V04.4", "analog input channel is 0");
                }
                if node.rotary_input == Some(0) {
                    return fail("V04.5", "rotary input channel is 0");
                }
                if node.general_driver == Some(0) {
                    return fail("V04.6", "general driver output slot is 0");
                }
            }
        }
        Ok(())
    }

    fn check_can(&self) -> Result<(), AmConfigError> {
        let can = &self.can;
        if !can.enable {
            return Ok(());
        }
        if !can.app.listeners_assigned() {
            return fail("V05.1", "CAN event listeners are not assigned");
        }
        if can.ports.is_empty() {
            return fail("V05.2", "CAN port config is empty");
        }
        for (port_index, port) in can.ports.iter().enumerate() {
            if !(1..=0x20).contains(&port.number) {
                return fail("V05.3", format!("port {port_index}: number {} is out of range", port.number));
            }
            if port.nodes.is_empty() {
                return fail("V05.4", format!("port {port_index}: CAN node config is empty"));
            }
            for (node_index, node) in port.nodes.iter().enumerate() {
                if !(1..=0x1D).contains(&node.target.id) {
                    return fail(
                        "V05.5",
                        format!("port {port_index} node {node_index}: CAN id {} is out of range", node.target.id),
                    );
                }
                if !(2..=5).contains(&(node.target.kind as i32)) {
                    return fail(
                        "V05.6",
                        format!("port {port_index} node {node_index}: invalid CAN node type {}", node.target.kind),
                    );
                }
                if node.interval > 0xFF {
                    return fail(
                        "V05.7",
                        format!(
                            "port {port_index} node {node_index}: auto reply interval {} is out of range",
                            node.interval
                        ),
                    );
                }
                if node.coin_input > 2 {
                    return fail(
                        "V05.8",
                        format!(
                            "port {port_index} node {node_index}: coin slot total {} is out of range",
                            node.coin_input
                        ),
                    );
                }
            }
        }
        Ok(())
    }

    fn check_usbio(&self) -> Result<(), AmConfigError> {
        let usbio = &self.usbio;
        if !usbio.enable {
            return Ok(());
        }
        let nodes = usbio.nodes.as_slice();
        for (i, first) in nodes.iter().enumerate() {
            for second in &nodes[i + 1..] {
                if first.id == second.id {
                    return fail("V06.1", format!("nodes {} and {} have the same id", i, i + 1));
                }
            }
        }
        for node in nodes {
            let funcs = &node.funcs;
            let mut has_function = false;
            if let Some(switch) = funcs.switch_input {
                if switch.a == 0 && switch.b != 0 {
                    return fail("V06.2", format!("switch button {} set while player is 0", switch.b));
                }
                has_function = true;
            }
            if let Some(slot) = funcs.coin_input {
                if slot == 0 {
                    return fail("V06.3", "coin input slot is 0");
                }
                has_function = true;
            }
            if let Some(analog) = funcs.analog_input {
                if analog.a == 0 {
                    return fail("V06.4", "analog input channel is 0");
                }
                has_function = true;
            }
            if let Some(channel) = funcs.rotary_input {
                if channel == 0 {
                    return fail("V06.5", "rotary input channel is 0");
                }
                has_function = true;
            }
            if let Some(slot) = funcs.general_output {
                if slot == 0 {
                    return fail("V06.6", "general output slot is 0");
                }
                has_function = true;
            }
            if let Some(slot) = funcs.pwm_output {
                if slot == 0 {
                    return fail("V06.7a", "PWM output slot is 0");
                }
                if node.pwm_cycle.is_none() {
                    return fail("V06.7b", "PWM cycle is not assigned");
                }
                has_function = true;
            }
            if funcs.unique_io.iter().any(|command| command.valid) {
                continue;
            }
            if !has_function {
                return fail("V06.8", "all functions are disabled");
            }
        }
        Ok(())
    }

    fn check_genericio(&self) -> Result<(), AmConfigError> {
        if self.genericio.enable {
            return fail("V07.1", "generic I/O must be disabled");
        }
        Ok(())
    }

    fn check_credit(&self) -> Result<(), AmConfigError> {
        let credit = &self.credit;
        if !credit.enable {
            return Ok(());
        }
        let rule = &credit.rule;
        if rule.chute_share_type > 2 {
            return fail("V08.1", format!("coin chute share type {} is invalid", rule.chute_share_type as i32));
        }
        if rule.service_share_type > 2 {
            return fail("V08.2", format!("service share type {} is invalid", rule.service_share_type as i32));
        }
        if rule.operation > 2 {
            return fail("V08.3", format!("operation {} is invalid", rule.operation as i32));
        }
        // The rule values are `<= 0xFF` or the "unset" sentinel, which the temporary rule stores as 2.
        let narrow = |value: u32| {
            (value <= 0xFF || value == UNSET_U32).then_some(if value == UNSET_U32 { 2 } else { value as u8 })
        };
        let mut multipliers = [0u8; 2];
        for (i, &value) in rule.coin_chute_multiplier.iter().enumerate() {
            match narrow(value) {
                Some(byte) => multipliers[i] = byte,
                None => return fail("V08.4", format!("coin chute multiplier {i} is {value}")),
            }
        }
        let Some(bonus_adder) = narrow(rule.bonus_adder) else {
            return fail("V08.5", format!("bonus adder {} is invalid", rule.bonus_adder));
        };
        let Some(coin_to_credit) = narrow(rule.coin_to_credit) else {
            return fail("V08.6", format!("coin to credit {} is invalid", rule.coin_to_credit));
        };
        let mut game_cost = [0u8; 8];
        for (j, &value) in rule.game_cost.iter().enumerate() {
            match narrow(value) {
                Some(byte) => game_cost[j] = byte,
                None => return fail("V08.7", format!("game cost {j} is {value}")),
            }
        }
        let Some(coin_amount) = narrow(rule.coin_amount) else {
            return fail("V08.8", format!("coin amount {} is invalid", rule.coin_amount));
        };
        // `0x1402DE600` on the temporary rule: multipliers 1..=9, bonus adder 0 or 2..=9, coin to credit
        // 1..=99, every game cost and the coin amount 1..=255.
        let out_of_range = multipliers.iter().any(|&m| !(1..=9).contains(&m))
            || bonus_adder > 9
            || bonus_adder == 1
            || !(1..=99).contains(&coin_to_credit)
            || game_cost.contains(&0)
            || coin_amount == 0;
        if out_of_range {
            return fail("V08.9", "a value of the credit rule is out of range");
        }
        if credit.max_credit > 0xFF && credit.max_credit != UNSET_U32 {
            return fail("V08.10", format!("max credit {} is invalid", credit.max_credit));
        }
        if credit.max_credit != UNSET_U32 && (credit.max_credit as u8).wrapping_sub(1) > 0x62 {
            return fail("V08.11", format!("max credit {} is out of range", credit.max_credit));
        }
        for (i, &slot) in credit.coin_box_assign.iter().enumerate() {
            if slot != UNSET_U64 && slot >= 4 {
                return fail("V08.12", format!("coin box {slot} bound to player {i} is invalid"));
            }
        }
        if credit.coin_selector_as6db.enable && credit.coin_selector_as6db.types.is_empty() {
            return fail("V08.13", "price config is empty");
        }
        Ok(())
    }

    fn check_display(&self) -> Result<(), AmConfigError> {
        let display = &self.display;
        if !display.enable {
            return Ok(());
        }
        if !display.complete_task_listener {
            return fail("V09.1", "display event listener is not assigned");
        }
        if display.display_port > MAX_COM_PORT {
            return fail("V09.2", format!("COM port {} of the VFD display is out of range", display.display_port));
        }
        Ok(())
    }

    fn check_network(&self) -> Result<(), AmConfigError> {
        let network = &self.network;
        if network.enable && !network.use_location_router && network.location_router_directly == 1 {
            return fail("V10.1", "config router and environment do not match");
        }
        Ok(())
    }

    fn check_network_laninstall(&self) -> Result<(), AmConfigError> {
        if self.laninstall.mode == 0 {
            return Ok(());
        }
        let network = &self.network;
        if !network.enable {
            return fail("V11.1", "network must be enabled if LAN install is enabled");
        }
        if network.location_router_directly == 1 {
            return fail("V11.2", "must be allowed to run under the title router if LAN install is enabled");
        }
        if !network.use_gateway {
            return fail("V11.3", "gateway must be used if LAN install is enabled");
        }
        Ok(())
    }

    fn check_network_auth(&self) -> Result<(), AmConfigError> {
        if !self.allnet_auth.enable {
            return Ok(());
        }
        if !self.network.enable {
            return fail("V12.1", "network must be enabled if ALL.Net auth is enabled");
        }
        if self.allnet_auth.auth_type > 1 {
            return fail("V12.2", format!("ALL.Net auth type {} is invalid", self.allnet_auth.auth_type));
        }
        Ok(())
    }

    fn check_auth_accounting(&self) -> Result<(), AmConfigError> {
        if self.allnet_accounting.mode != 0 && !self.allnet_auth.enable {
            return fail("V13.1", "ALL.Net auth must be enabled if ALL.Net accounting is enabled");
        }
        Ok(())
    }

    fn check_auth_wifi(&self) -> Result<(), AmConfigError> {
        if self.allnet_wifi.enable && !self.allnet_auth.enable {
            return fail("V14.1", "ALL.Net auth must be enabled if ALL.Net Wi-Fi is enabled");
        }
        Ok(())
    }

    fn check_deliver(&self) -> Result<(), AmConfigError> {
        let deliver = &self.netdeliver;
        if !deliver.enable {
            return Ok(());
        }
        if !self.network.enable {
            return fail("V15.1", "network must be enabled if net delivery is enabled");
        }
        if !self.network.use_gateway {
            return fail("V15.2", "gateway must be used if net delivery is enabled");
        }
        if deliver.local {
            return fail("V15.3", "net delivery local mode is unsupported");
        }
        if !self.allnet_auth.enable {
            return fail("V15.4", "ALL.Net auth must be enabled if net delivery is enabled");
        }
        if self.laninstall.mode == 1 {
            return fail("V15.5", "cannot operate as LAN install client while net delivery is enabled");
        }
        if deliver.use_aws_signature && self.allnet_auth.auth_type != 1 {
            return fail("V15.6", "cannot use the AWS signature with ALL.Net auth 1.0");
        }
        Ok(())
    }

    fn check_aime(&self) -> Result<(), AmConfigError> {
        let aime = &self.aime;
        if !aime.enable {
            return Ok(());
        }
        if !(aime.app.task_finished_listener && aime.app.need_acceptation_listener) {
            return fail("V16.1", "Aime event listeners are not assigned");
        }
        if !(1..=MAX_CARD_ACCEPT_TIME).contains(&aime.card_accept_time) {
            return fail("V16.2", format!("card accept time {} is out of range", aime.card_accept_time));
        }
        for (i, unit) in aime.units.iter().enumerate() {
            if !(1..=MAX_COM_PORT).contains(&unit.com_port) {
                return fail("V16.3", format!("COM port {} of Aime reader {i} is out of range", unit.com_port));
            }
            if !(1..=6).contains(&unit.unit_index) {
                return fail("V16.4", format!("unit index {} of Aime reader {i} is out of range", unit.unit_index));
            }
        }
        Ok(())
    }

    fn check_emoney(&self) -> Result<(), AmConfigError> {
        let emoney = &self.emoney;
        if !emoney.enable {
            return Ok(());
        }
        let listeners = &emoney.app;
        if !(listeners.complete_task_listener
            && listeners.notify_deal_result_listener
            && listeners.play_sound_listener
            && listeners.notify_cancel_listener)
        {
            return fail("V17.1", "e-money event listeners are not assigned");
        }
        if emoney.resource_path.is_empty() {
            return fail("V17.2", "e-money resource folder is not assigned");
        }
        if !self.display.enable {
            return fail("V17.3", "display must be enabled if e-money is enabled");
        }
        if !self.network.enable {
            return fail("V17.4", "network must be enabled if e-money is enabled");
        }
        if !self.aime.enable {
            return fail("V17.5", "Aime must be enabled if e-money is enabled");
        }
        if !self.aime.high_baudrate {
            return fail("V17.6", "Aime reader must be high baud rate if e-money is enabled");
        }
        if emoney.aime_unit >= self.aime.units.len() as u64 {
            return fail("V17.7", format!("reader index {} for e-money is out of range", emoney.aime_unit));
        }
        Ok(())
    }

    fn check_aimepay(&self) -> Result<(), AmConfigError> {
        let aimepay = &self.aimepay;
        if !aimepay.enable {
            return Ok(());
        }
        let listeners = &aimepay.app;
        if !(listeners.complete_task_listener
            && listeners.notify_deal_result_listener
            && listeners.need_acceptation_listener)
        {
            return fail("V18.1", "Aime Pay event listeners are not assigned");
        }
        if aimepay.resource_path.is_empty() {
            return fail("V18.2", "Aime Pay resource folder is not assigned");
        }
        for item in &aimepay.coin_item_id {
            if item.len() != 13 || !item.bytes().all(|byte| byte.is_ascii_digit()) {
                return fail("V18.3", "Aime Pay coin item id is not valid");
            }
        }
        if !self.allnet_auth.enable {
            return fail("V18.4", "ALL.Net auth must be enabled if Aime Pay is enabled");
        }
        if !self.aime.enable {
            return fail("V18.5", "Aime must be enabled if Aime Pay is enabled");
        }
        if !self.display.enable {
            return fail("V18.6", "display must be enabled if Aime Pay is enabled");
        }
        Ok(())
    }

    fn check_credit_aime_display(&self) -> Result<(), AmConfigError> {
        let selector = self.credit.enable && self.credit.coin_selector_as6db.enable;
        let selector_port = self.credit.coin_selector_as6db.arg;
        if self.aime.enable {
            for unit in &self.aime.units {
                if selector && unit.com_port == selector_port {
                    return fail("V19.1", "COM port of Aime reader and coin selector AS-6DB is the same");
                }
                if self.display.enable && unit.com_port == self.display.display_port {
                    return fail("V19.2", "COM port of Aime reader and display is the same");
                }
            }
        }
        if self.display.enable && selector && self.display.display_port == selector_port {
            return fail("V19.3", "COM port of display and coin selector AS-6DB is the same");
        }
        Ok(())
    }

    fn check_usbdevice(&self) -> Result<(), AmConfigError> {
        if !self.usbdevice.app.is_valid() {
            return fail("V20.1", "USB device checks are unsupported");
        }
        Ok(())
    }
}
