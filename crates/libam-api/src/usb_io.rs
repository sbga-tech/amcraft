//! `am::usbio`: USB IO nodes, inputs, outputs and firmware update.

use crate::jvs::{AnalogState, RotaryState, SwitchState, clamp_len};
use crate::result::AmResult;

/// Payload bytes of one unique command (`UniqueCommand.data`, size constant `0x3E`).
pub const UNIQUE_DATA_LEN: usize = 62;
/// Bytes of the libam unique-data block (`Node+0x4D0`, `u64,u64,u64,u32,u8`).
pub const UNIQUE_INPUT_DATA_LEN: usize = 29;

/// libam node-info record (`Node+0x182`, 31 bytes): `wchar_t name[9]` at `+0`, `wchar_t code[6]` at `+0x12`
/// and the revision byte at `+0x1E`. The meaning of the three fields is inferred (board name, custom code,
/// firmware revision); amdaemon only copies them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsbIoNodeInfo {
    /// `wchar_t[9]` at `+0x00`.
    pub name: [u16; 9],
    /// `wchar_t[6]` at `+0x12`.
    pub code: [u16; 6],
    /// `u8` at `+0x1E`.
    pub revision: u8,
}

/// One libam unique-function descriptor (`{u16 flags; u16 code}`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UniqueDescriptor {
    /// Low byte of the board's per-slot reply word (bit 0 = flag0, bit 1 = flag1).
    pub flags: u16,
    /// High byte of the board's per-slot reply word.
    pub code: u16,
}

/// libam unique-descriptor list (`Node+0x310`: `{u64 count; UniqueDescriptor[]}`); only the first two
/// descriptors are ever consumed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UniqueDescriptors {
    /// Descriptor count as libam stores it (libam's setter keeps it `<= 2`); not clamped by amdaemon.
    pub count: u64,
    /// Descriptors.
    pub entries: [UniqueDescriptor; 2],
}

impl UniqueDescriptors {
    /// The descriptors amdaemon copies, `0..min(count, 2)`.
    pub fn processed(&self) -> &[UniqueDescriptor] {
        &self.entries[..clamp_len(self.count, self.entries.len())]
    }
}

/// `UniqueCommand` (`0x140226850`): `{u8 code; u64 size = 0x3E; u8 data[62]}`. The size is the constant
/// [`UNIQUE_DATA_LEN`], so libam's `size > 0x3E` rejection never triggers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UniqueCommand<'a> {
    /// Command code (the published `outputInfo.uniqueCommands[k]`).
    pub code: u8,
    /// Payload.
    pub data: &'a [u8; UNIQUE_DATA_LEN],
}

/// Status argument of `UpdateFirmwareEventListener::onNodeStatus` / `onUpdateState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirmwareStatus {
    /// `0`.
    Idle,
    /// `1`: a node starts updating / the update session entered firmware mode.
    Started,
    /// Any value `>= 2` (libam passes `2 + hadError`, or `4` when the update session is torn down).
    Finished,
}

impl FirmwareStatus {
    /// Classifies the raw status number libam passes (`0`, `1`, anything else).
    pub const fn from_raw(raw: u32) -> Self {
        match raw {
            0 => Self::Idle,
            1 => Self::Started,
            _ => Self::Finished,
        }
    }
}

/// A call libam makes on amdaemon's embedded `UsbIOEventListener`
/// (`am::usbio::UpdateFirmwareEventListener`, vtable `0x1405E0460`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirmwareEvent {
    /// vf1 `onProgress(node, percent)` (`0x140049920`): `percent` is the node's progress `0..=100`.
    Progress {
        /// Node index; ignored by amdaemon.
        node: u64,
        /// Node progress in percent.
        percent: u32,
    },
    /// vf2 `onNodeStatus(node, status)` (`0x140049900`).
    NodeStatus {
        /// Node index; ignored by amdaemon.
        node: u64,
        /// Node status.
        status: FirmwareStatus,
    },
    /// vf3 `onUpdateState(status)` (`0x1400498D0`).
    UpdateState {
        /// Update-session status.
        status: FirmwareStatus,
    },
}

/// libam surface used by the USBIO controller (`am::usbio`, statically linked into `amdaemon.exe`).
///
/// Node indices are the array index of the node controller (`0..4`). Calls for an index at or beyond libam's
/// node count fail with `am::Result` code 1; `send_*` additionally fail with code 6 when the node refuses
/// the datum (not ready, firmware-update mode, node error, function disabled, empty data, PWM slot out of
/// range, unknown unique code). A successful send only queues the datum for libam's communication cycle.
pub trait UsbIoPort {
    /// `usbio::handle(facade, &res)` (`0x1402188F0`). `Err` covers both a failed `am::Result` (code 2: the
    /// libam context is absent) and a null handle; the original treats them identically.
    fn handle(&self) -> AmResult<()>;

    /// `usbio::nodeCount()` (`0x140217B30`): size of libam's node vector.
    fn node_count(&self) -> u64;

    /// `usbio::nodeAutoOutputZero(handle, index, &res)` (`0x140217B50`): `NodeConfig+2`.
    fn node_auto_output_zero(&self, node: usize) -> AmResult<bool>;

    /// `usbio::nodeInfo(handle, index, &res)` (`0x140217E10`, `Node+0x182`): `None` is the null pointer
    /// returned together with `am::Result` code 1 for an out-of-range index.
    fn node_info(&self, node: usize) -> Option<UsbIoNodeInfo>;

    /// `usbio::nodeSwitch` (`0x140217E90`, `Node+0x2E0`).
    fn node_switch(&self, node: usize) -> AmResult<SwitchState<2>>;

    /// `usbio::nodeAnalog` (`0x140217F90`, `Node+0x320`).
    fn node_analog(&self, node: usize) -> AmResult<AnalogState<8>>;

    /// `usbio::nodeRotary` (`0x140218010`, `Node+0x4A8`).
    fn node_rotary(&self, node: usize) -> AmResult<RotaryState<4>>;

    /// `usbio::nodeUniqueDesc` (`0x140217F10`, `Node+0x310`).
    fn node_unique_descriptors(&self, node: usize) -> AmResult<UniqueDescriptors>;

    /// `usbio::nodeUniqueData` (`0x140218090`, `Node+0x4D0`): the raw 29 bytes.
    fn node_unique_data(&self, node: usize) -> AmResult<[u8; UNIQUE_INPUT_DATA_LEN]>;

    /// `usbio::sendGeneral` (`0x140217BD0`) → `am::usbio::Node::sendGeneralOutput(u64, u64)` (`0x14024DF50`).
    fn send_general_output(&mut self, node: usize, bits: u64, size: u64) -> AmResult<()>;

    /// `usbio::sendPwm` (`0x140217C90`) → `am::usbio::Node::sendPwmOutput(u64 slot, u8 duty)` (`0x14024E210`).
    fn send_pwm_output(&mut self, node: usize, slot: usize, duty: u8) -> AmResult<()>;

    /// `usbio::sendUnique` (`0x140217D50`) → `am::usbio::Node::sendUniqueCommand` (`0x14024E510`).
    fn send_unique_command(&mut self, node: usize, command: &UniqueCommand<'_>) -> AmResult<()>;

    /// Firmware-update listener calls libam made since the previous drain, in call order. libam delivers
    /// them synchronously from its per-frame pump, but only once the controller's `AppConfig` observer has
    /// installed the listener ([`UsbIoAppConfig::firmware_listener`]).
    fn take_firmware_events(&mut self) -> Vec<FirmwareEvent>;
}

/// One unique function of a USBIO node as configured in the process config (6-byte entry: code at `+0`,
/// "input" flag at `+4`, "output" flag at `+5`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsbIoUniqueFunction {
    /// Command code (`entry[0]`).
    pub code: u8,
    /// Function is an input (`entry[4] != 0`): its code is published in `uniqueInput.commands`.
    pub input: bool,
    /// Function is an output (`entry[5] != 0`): its code is published in `outputInfo.uniqueCommands`.
    pub output: bool,
}

/// Function block of one USBIO node in the process config (`process::Config+0x4FAC0+0x80*i`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsbIoNodeFunctions {
    /// PWM function enabled (`+0x30`).
    pub pwm_enabled: bool,
    /// PWM slot count (`+0x34`).
    pub pwm_slots: u32,
    /// Configured unique functions in config order (`+0x38` count, `+0x3C` entries).
    pub uniques: Vec<UsbIoUniqueFunction>,
}

/// Part of `amdaemon::process::Config` read by the USBIO observer (`0x14004B010`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsbIoProcessConfig {
    /// One entry per configured node (`usbio.nodeCount` at `+0x4FAB0` is `nodes.len()`). The original does
    /// not bound the count by four (the config validator does); entries beyond the four node controllers
    /// are ignored by amdaemon instead of indexing past the node array.
    pub nodes: Vec<UsbIoNodeFunctions>,
}

/// Part of libam `am::AppConfig` touched by the USBIO observer (`0x14004C5F0`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsbIoAppConfig {
    /// `AppConfig+0x1E08` (`usbio::Config+0x7A0`): `UpdateFirmwareEventListener*`. `true` once the observer
    /// pointed it at the controller's embedded listener, which makes libam deliver
    /// [`UsbIoPort::take_firmware_events`].
    pub firmware_listener: bool,
}
