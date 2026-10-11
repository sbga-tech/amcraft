//! The USB I/O facades and libam's handles.
//!
//! * [`UsbIoPort`] (`am::usbio`): USB IO nodes, inputs, outputs and firmware update ([`UsbIoHandle`]).
//! * [`BoardIoPort`] (`am::systemio`): DIP switches, push switches and the LED word ([`BoardIoHandle`]).
//! * [`UsbDevicePort`] (`am::usbdevice`): USB device reconnects ([`UsbDeviceHandle`]).

use super::node::Node;
use crate::jvs::{AnalogChannel, AnalogState, RotaryState, SwitchState, clamp_len};
use crate::result::{AmError, AmResult};
use crate::shared::Shared;

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

/// The libam `am::systemio` surface used by amdaemon's board I/O controller.
pub trait BoardIoPort {
    /// `systemio` facade accessor `0x140218CB0` on `Manager+8`: `Err` (code 2) when libam is not
    /// initialised. The token itself is never dereferenced by amdaemon.
    fn systemio_facade(&self) -> AmResult<()>;

    /// DIP switch word (`&SystemIo.dip` `0x140217A60`, `value()` `0x140213E30`). libam stores it once, in
    /// `am::systemio::Manager::initialize` (`0x1402ABF70`).
    fn dip(&self) -> u64;

    /// Push switch word (`&dip + 8`, `value()` `0x140213E90`). libam only ever zero-initialises it.
    fn push(&self) -> u64;

    /// LED word (`&SystemIo.led` `0x140217A40`, `value()` `0x140213EE0`); persists between requests.
    fn led(&self) -> u64;

    /// `led.set(v)` (`0x140213ED0`): stores the word and marks it valid.
    fn set_led(&mut self, value: u64);
}

/// Event raised by libam's `am::usbdevice::CompleteTaskEventListener` (`UsbDeviceEventListener::vf1`
/// `0x140048080`). libam fires it from `am::usbdevice::Manager::update` (`0x14024CEA0`) inside the libam
/// update pump.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsbDeviceEvent {
    /// The pending reconnect finished; `code` is libam's completion code (`1` success, `2` failure,
    /// `3` driver error `0x81000015`).
    ReconnectComplete {
        /// libam completion code.
        code: i32,
    },
}

/// The libam `am::usbdevice` surface used by amdaemon's USB device controller.
pub trait UsbDevicePort {
    /// `usbdevice` facade accessor `0x140218D50` on `Manager+8`: `Err` (code 2) when libam is not
    /// initialised.
    fn usbdevice_facade(&self) -> AmResult<()>;

    /// `reconnect(facade, vid, pid)` (`0x140217AA0` → `am::usbdevice::Manager::reconnect` `0x14024CBB0`).
    /// libam refuses (code 2) when not ready, without listener, without amMaster, while a task is active
    /// or when the asynchronous driver call fails to start.
    fn usb_reconnect(&mut self, vendor_id: u16, product_id: u16) -> AmResult<()>;

    /// Completion events raised since the last call; amdaemon delivers them from its libam event pump
    /// (`on_libam_events`).
    fn take_events(&mut self) -> Vec<UsbDeviceEvent>;
}

/// A USB `VID:PID` pair (`{u16 vid, u16 pid}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UsbDeviceId {
    /// USB vendor id.
    pub vendor_id: u16,
    /// USB product id.
    pub product_id: u16,
}

/// `am::usbdevice::Required` (`0x90` bytes): a counted set of `VID:PID` pairs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsbRequired {
    count: u64,
    pairs: Vec<UsbDeviceId>,
}

impl UsbRequired {
    /// Capacity of the pair array.
    pub const MAX_PAIRS: usize = 32;

    /// `addPair` (`0x14021E8A0`): fails with code 2 once 32 pairs exist.
    pub fn add_pair(&mut self, vendor_id: u16, product_id: u16) -> AmResult<()> {
        if self.pairs.len() >= Self::MAX_PAIRS {
            return Err(AmError::with_message(AmError::NOT_INITIALIZED, "Pair list is full"));
        }
        self.pairs.push(UsbDeviceId { vendor_id, product_id });
        Ok(())
    }

    /// `setCount` (`0x14021E890`): stores the `u32` into the first `u64`.
    pub fn set_count(&mut self, count: u32) {
        self.count = u64::from(count);
    }

    /// Number of devices required.
    pub fn count(&self) -> u64 {
        self.count
    }

    /// The pairs added so far.
    pub fn pairs(&self) -> &[UsbDeviceId] {
        &self.pairs
    }
}

/// The part of `am::AppConfig` owned by the USB device controller: `am::usbdevice::Config` at
/// `AppConfig+0x2000` (`count`, `Required sets[9]`, `listener*` at `+0x518` = `AppConfig+0x2518`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsbDeviceAppConfig {
    required: Vec<UsbRequired>,
    /// `AppConfig+0x2518` holds the controller's completion listener (set by the USB device controller's
    /// AppConfig callback).
    pub listener_installed: bool,
}

impl UsbDeviceAppConfig {
    /// Capacity of the `Required` array.
    pub const MAX_REQUIRED: usize = 9;

    /// `Config::addRequired` (`0x14021EAE0`): fails with code 2 ("Required list is full") at 9 sets.
    pub fn add_required(&mut self, required: UsbRequired) -> AmResult<()> {
        if self.required.len() >= Self::MAX_REQUIRED {
            return Err(AmError::with_message(AmError::NOT_INITIALIZED, "Required list is full"));
        }
        self.required.push(required);
        Ok(())
    }

    /// The required sets.
    pub fn required(&self) -> &[UsbRequired] {
        &self.required
    }

    /// libam's validator for `am::usbdevice::Config` (`0x1402AFF80`): only an empty list is valid. libam's
    /// setup fails with `Unsupport check USB device count.` otherwise.
    pub fn is_valid(&self) -> bool {
        self.required.is_empty()
    }
}

/// `UsbIoPort` handle over the shared libam state.
#[derive(Debug)]
pub struct UsbIoHandle(pub(crate) Shared);

/// `BoardIoPort` handle over the shared libam state.
#[derive(Debug)]
pub struct BoardIoHandle(pub(crate) Shared);

/// `UsbDevicePort` handle over the shared libam state.
#[derive(Debug)]
pub struct UsbDeviceHandle(pub(crate) Shared);

/// The failure of every indexed accessor: `index >= nodeCount` (`usbio.md` 1.1).
fn out_of_range() -> AmError {
    AmError::with_message(AmError::INVALID_ARGUMENT, "invalid num")
}

/// The failure of a refused `send_*` (`Node::send*` returned false, `usbio.md` 1.6.2).
fn refused() -> AmError {
    AmError::code(AmError::FAILED)
}

impl UsbIoHandle {
    /// Runs `f` on node `node` after the facade accessor check.
    fn node<R>(&self, node: usize, f: impl FnOnce(&Node) -> R) -> AmResult<R> {
        self.0.guard()?;
        self.0.read(|core| core.usbio.nodes().get(node).map(f)).ok_or_else(out_of_range)
    }

    /// Runs a send on node `node`: `Result 1` out of range, `Result 6` when the node refuses.
    fn send(&mut self, node: usize, f: impl FnOnce(&mut Node) -> bool) -> AmResult<()> {
        self.0.guard()?;
        let accepted = self.0.with(|core| core.usbio.nodes_mut().get_mut(node).map(f)).ok_or_else(out_of_range)?;
        if accepted { Ok(()) } else { Err(refused()) }
    }
}

impl UsbIoPort for UsbIoHandle {
    /// `0x1402188F0`: ok while libam is initialised, else result code 2.
    fn handle(&self) -> AmResult<()> {
        self.0.guard()
    }

    /// `0x140217B30`: the configured node count.
    fn node_count(&self) -> u64 {
        self.0.read(|core| core.usbio.nodes().len() as u64)
    }

    fn node_auto_output_zero(&self, node: usize) -> AmResult<bool> {
        self.node(node, |n| n.config().auto_output_zero)
    }

    fn node_info(&self, node: usize) -> Option<UsbIoNodeInfo> {
        self.node(node, |n| n.info).ok()
    }

    fn node_switch(&self, node: usize) -> AmResult<SwitchState<2>> {
        self.node(node, |n| SwitchState {
            system: n.switch.system,
            player_count: n.switch.player_count,
            player_masks: [n.switch.player_masks[0], n.switch.player_masks[1]],
        })
    }

    fn node_analog(&self, node: usize) -> AmResult<AnalogState<8>> {
        self.node(node, |n| {
            let mut channels = [AnalogChannel::default(); 8];
            channels.copy_from_slice(&n.analog.channels[..8]);
            AnalogState { count: n.analog.count, channels }
        })
    }

    fn node_rotary(&self, node: usize) -> AmResult<RotaryState<4>> {
        self.node(node, |n| {
            let mut values = [0; 4];
            values.copy_from_slice(&n.rotary.values[..4]);
            RotaryState { count: n.rotary.count, values }
        })
    }

    fn node_unique_descriptors(&self, node: usize) -> AmResult<UniqueDescriptors> {
        self.node(node, |n| n.coin)
    }

    fn node_unique_data(&self, node: usize) -> AmResult<[u8; UNIQUE_INPUT_DATA_LEN]> {
        self.node(node, |n| n.unique)
    }

    fn send_general_output(&mut self, node: usize, bits: u64, size: u64) -> AmResult<()> {
        self.send(node, |n| n.send_general_output(bits, size))
    }

    fn send_pwm_output(&mut self, node: usize, slot: usize, duty: u8) -> AmResult<()> {
        self.send(node, |n| n.send_pwm_output(slot, duty))
    }

    fn send_unique_command(&mut self, node: usize, command: &UniqueCommand<'_>) -> AmResult<()> {
        self.send(node, |n| n.send_unique_command(command.code, command.data))
    }

    fn take_firmware_events(&mut self) -> Vec<FirmwareEvent> {
        self.0.with(|core| core.usbio.take_firmware_events())
    }
}

impl BoardIoPort for BoardIoHandle {
    /// `0x140218CB0`: ok while libam is initialised, whatever the systemio state (3.4.1).
    fn systemio_facade(&self) -> AmResult<()> {
        self.0.guard()
    }

    fn dip(&self) -> u64 {
        self.0.read(|core| core.systemio.dip())
    }

    fn push(&self) -> u64 {
        self.0.read(|core| core.systemio.push())
    }

    fn led(&self) -> u64 {
        self.0.read(|core| core.systemio.led())
    }

    fn set_led(&mut self, value: u64) {
        self.0.with(|core| core.systemio.set_led(value));
    }
}

impl UsbDevicePort for UsbDeviceHandle {
    /// `0x140218D50`: ok while libam is initialised (4.1).
    fn usbdevice_facade(&self) -> AmResult<()> {
        self.0.guard()
    }

    /// `0x140217AA0` (4.2): result code 0 or 2 (no message); the instance is not tested here.
    fn usb_reconnect(&mut self, vendor_id: u16, product_id: u16) -> AmResult<()> {
        let accepted = self.0.with(|core| {
            let master_enabled = core.master.is_attached();
            core.usbdevice.reconnect(vendor_id, product_id, master_enabled)
        });
        if accepted { Ok(()) } else { Err(AmError::code(AmError::NOT_INITIALIZED)) }
    }

    fn take_events(&mut self) -> Vec<UsbDeviceEvent> {
        self.0.with(|core| core.usbdevice.take_events())
    }
}
