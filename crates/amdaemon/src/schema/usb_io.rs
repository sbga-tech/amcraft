//! USBIO blocks: `ClientData+0x218` (0x960) and the USBIO half of the ServerIO body at `body+0x0` (0x8A0).
//!
//! Evidence: `spec/controllers/jvs_usbio.md` §2.1–§2.4 (ServerIO) and §3.2 (requests); claims
//! `jvs_usbio.serverio.usbio.*` and `jvs_usbio.client.usbio.*`. The switch records come from the JVS block
//! ([`crate::schema::jvs`]).

use num_enum::TryFromPrimitive;
use widestring::U16CStr;
use zero_schema::zero;

use crate::schema::jvs::{PlayerSwitches, SwitchUnit};
use crate::schema::shared::Response;
use crate::schema::{client_block, server_io_block};

/// Node slots in both the request array and the response array; fixed no matter how many nodes libam
/// detects.
pub const NODE_CAPACITY: usize = 4;
/// Unique-command slots per node (`uniques[9]` in the request, `uniqueCommands[9]` in the response).
pub const UNIQUE_SLOTS: usize = 9;
/// Payload bytes of one unique command (`UniqueCommand.data`, size constant `0x3E`).
pub const UNIQUE_DATA_LEN: usize = 62;
/// PWM slots addressed by one PWM request.
pub const PWM_SLOTS: usize = 8;
/// Bytes of the libam unique-data block copied into `uniqueInput.data` (`u64,u64,u64,u32,u8`).
pub const UNIQUE_INPUT_DATA_LEN: usize = 29;
/// Bytes of the request's task payload union (`ClientOutputInfo`).
pub const OUTPUT_INFO_SIZE: usize = 0x250;

/// `usbio::ClientRequest` (`0x258`), one per node: the request generation, the raw task and the task payload.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ClientUsbIoRequest<'b> {
    /// Request generation (`taskNo`); the client changes it to submit.
    pub generation: u32,
    /// Raw [`UsbIoTask`].
    pub task: u32,
    /// Task payload (`usbio::ClientInfo`, a one-member union), decoded as [`ClientOutputInfo`].
    pub info: &'b [u8; OUTPUT_INFO_SIZE],
}

/// `ClientData.usb_io` (`0x960` bytes): `usbio::ClientData { ClientNodeData nodes[4] }`.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ClientUsbIo<'b> {
    /// Request of node *i* (node index = array index).
    pub nodes: [ClientUsbIoRequest<'b>; NODE_CAPACITY],
}

/// `ipc::usbio::Task` (name table `0x1405EEF48`): `Output` is the only task that exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, TryFromPrimitive)]
#[repr(u32)]
pub enum UsbIoTask {
    /// Sends general, PWM and unique output ([`ClientOutputInfo`]).
    Output = 0,
}

impl UsbIoTask {
    /// Name printed in the request trace (`0x1405EEF48[0]`).
    pub const fn name(self) -> &'static str {
        match self {
            Self::Output => "Output",
        }
    }
}

/// `ClientOutputInfo.general` (`0x10` bytes).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct GeneralOutputRequest {
    /// Gate for the general output.
    pub requested: bool,
    /// Value for `sendGeneralOutput(bits, 8)`.
    pub bits: u64,
}

/// `ClientOutputInfo.pwm` (9 bytes).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct PwmOutputRequest<'b> {
    /// Gate for the PWM output.
    pub requested: bool,
    /// Duty for each of the eight slots (`sendPwmOutput(slot i, duties[i])`).
    pub duties: &'b [u8; PWM_SLOTS],
}

/// One `ClientOutputInfo.uniques[k]` entry (`0x3F` bytes).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct UniqueOutputRequest<'b> {
    /// Gate for this unique command.
    pub requested: bool,
    /// Command payload.
    pub data: &'b [u8; UNIQUE_DATA_LEN],
}

/// The `Output` payload, `usbio::ClientInfo.output` (`0x250` bytes).
///
/// The `requested` gates are `bool`s, so a payload whose gate byte is neither 0 nor 1 does not decode and
/// the request fails (the original treated every non-zero byte as set).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ClientOutputInfo<'b> {
    /// General output.
    pub general: GeneralOutputRequest,
    /// PWM output.
    pub pwm: PwmOutputRequest<'b>,
    /// Unique commands.
    pub uniques: [UniqueOutputRequest<'b>; UNIQUE_SLOTS],
}

impl<'w> ClientUsbIoRef<'w> {
    /// The request of node `index`.
    ///
    /// # Panics
    /// If `index` is not below [`NODE_CAPACITY`].
    pub fn request(&self, index: usize) -> ClientUsbIoRequestRef<'w> {
        self.nodes().get(index).expect("a node index is below the node capacity")
    }
}

impl<'w> ClientUsbIoRequestRef<'w> {
    /// The request's task, `None` for an unknown value (the request fails).
    pub fn task_kind(&self) -> Option<UsbIoTask> {
        UsbIoTask::try_from(self.task()).ok()
    }

    /// `Output` payload, `None` when the payload bytes do not decode (a gate byte other than 0 or 1).
    pub fn output(&self) -> Option<ClientOutputInfoRef<'w>> {
        let info: &'w [u8; OUTPUT_INFO_SIZE] = self.info();
        ClientOutputInfo::access_prefix(info).ok().map(|(payload, _)| payload)
    }
}

/// USBIO `switchInput` (node+`0x028`, `0x18C` bytes), written by `sub_14004A850`.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct UsbIoSwitchInput {
    /// Test switch, bit 1 of libam's system byte.
    pub test: SwitchUnit,
    /// Tilt switch, bit 0 of libam's system byte.
    pub tilt: SwitchUnit,
    /// libam player count truncated to `u32`; **not** clamped (can exceed 2).
    pub player_count: u32,
    /// Per-player switch matrix; only the first two libam players are ever copied.
    pub players: [PlayerSwitches; 2],
}

/// USBIO `analogInput` (node+`0x1B4`, `0x18` bytes), written by `sub_14004A970`. The record size includes two
/// bytes of tail padding that are never written.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct UsbIoAnalogInput {
    /// libam channel count truncated to `u32`; not clamped.
    pub channel_count: u32,
    /// Channel values; only `min(channel_count, 8)` are written.
    pub values: [u16; 8],
    /// Valid-bit count rewritten for every processed channel, so it equals the **last** processed channel's
    /// value (untouched when no channel is processed).
    pub valid_bit_count: u16,
}

/// USBIO `rotaryInput` (node+`0x1CC`, `0x0C` bytes), written by `sub_14004AA10`.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct UsbIoRotaryInput {
    /// libam channel count truncated to `u32`; not clamped.
    pub channel_count: u32,
    /// Channel values; only `min(channel_count, 4)` are written.
    pub values: [u16; 4],
}

/// One unique-function descriptor of `uniqueInput` (6 bytes with its tail padding), written by `sub_14004AA90`.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct UsbIoUniqueDescriptor {
    /// libam descriptor code.
    pub code: u16,
    /// libam `flags & 1` ("input enable" [I]; amdaemon never interprets it).
    pub flag0: bool,
    /// libam `(flags >> 1) & 1` ("output enable" [I]; amdaemon never interprets it).
    pub flag1: bool,
    /// Always written `0`.
    pub reserved_04: u8,
}

/// USBIO `uniqueInput` (node+`0x1D8`).
///
/// The written part is `0x36` bytes; the C++ struct has `u32` alignment, so its size is `0x38` and the two
/// tail bytes are the "never written" padding at node+`0x20E`.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct UsbIoUniqueInput<'b> {
    /// libam descriptor count truncated to `u32` (libam's setter keeps it `<= 2`).
    pub reported_count: u32,
    /// Descriptors; only `min(reported_count, 2)` are written.
    pub descriptors: [UsbIoUniqueDescriptor; 2],
    /// Codes of the configured unique functions flagged *input*, written by the process-config observer
    /// (`0x14004B010`).
    pub commands: &'b [u8; UNIQUE_SLOTS],
    /// Raw copy of the 29 bytes at libam `Node+0x4D0`.
    pub data: &'b [u8; UNIQUE_INPUT_DATA_LEN],
}

/// USBIO `outputInfo` (node+`0x210`, `0x14` bytes).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct UsbIoOutputInfo<'b> {
    /// libam `NodeConfig+2`, refreshed on detect and after a firmware update (`updateNodeInfo`).
    pub auto_output_zero: bool,
    /// PWM slot count from the process config (`0` when the PWM function is disabled).
    pub pwm_slot_count: u32,
    /// Codes of the configured unique functions flagged *output*; `sendUniqueCommand` takes
    /// `unique_commands[k]` as the command code of request slot `k`.
    pub unique_commands: &'b [u8; UNIQUE_SLOTS],
}

/// `ServerIO` USBIO node (`0x224` bytes), at `body+0x0C+0x224*i`.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerUsbIoNode<'b> {
    /// `response.taskNo` and `succeeded`; written only by `checkResult` (`0x14004CA50`).
    pub response: Response,
    /// Board name (`wcsncpy_s(dst, 9, src, 8)`) [I].
    #[zero(capacity = 9)]
    pub name: &'b U16CStr,
    /// Board code (`wcsncpy_s(dst, 6, src, 5)`) [I].
    #[zero(capacity = 6)]
    pub code: &'b U16CStr,
    /// Revision [I].
    pub revision: u8,
    /// Input switches.
    pub switch_input: UsbIoSwitchInput,
    /// Analog channels.
    pub analog_input: UsbIoAnalogInput,
    /// Rotary channels.
    pub rotary_input: UsbIoRotaryInput,
    /// Unique-function inputs.
    pub unique_input: UsbIoUniqueInput<'b>,
    /// Output description.
    pub output_info: UsbIoOutputInfo<'b>,
}

/// USBIO half of the ServerIO body (`body+0x0`, `0x8A0` bytes).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerUsbIo<'b> {
    /// A firmware update is in progress (`UpdateFirmwareEventListener::onUpdateState`).
    pub firmware_updating: bool,
    /// Firmware progress in percent (`(percent + done) / nodeCount`).
    pub firmware_progress: u32,
    /// Detected node count (`<= 4`).
    pub node_count: u32,
    /// Node slots.
    pub nodes: [ServerUsbIoNode<'b>; NODE_CAPACITY],
    /// Detect-callback counter (wraps at `2^32`).
    pub detect_counter: u32,
}

impl ServerUsbIoRef<'_> {
    /// The response generation of node `index`.
    ///
    /// # Panics
    /// If `index` is not below [`NODE_CAPACITY`].
    pub fn node_generation(&self, index: usize) -> u32 {
        self.nodes().get(index).expect("a node index is below the node capacity").response().generation()
    }
}

client_block!(usb_io, usb_io_mut: ClientUsbIo, ClientUsbIoRef, ClientUsbIoMut @ 0x218);
server_io_block!(usb_io, usb_io_mut: ServerUsbIo, ServerUsbIoRef, ServerUsbIoMut @ 0x0);

const _: () = {
    assert!(GeneralOutputRequest::SCHEMA_SIZE == 0x10);
    assert!(PwmOutputRequest::SCHEMA_SIZE == 0x09);
    assert!(UniqueOutputRequest::SCHEMA_SIZE == 0x3F);
    assert!(ClientOutputInfo::SCHEMA_SIZE == OUTPUT_INFO_SIZE && ClientOutputInfo::SCHEMA_ALIGN == 8);
    assert!(ClientUsbIoRequest::SCHEMA_SIZE == 0x258);
    assert!(ClientUsbIo::SCHEMA_SIZE == 0x960);
    assert!(UsbIoSwitchInput::SCHEMA_SIZE == 0x18C);
    assert!(UsbIoAnalogInput::SCHEMA_SIZE == 0x18);
    assert!(UsbIoRotaryInput::SCHEMA_SIZE == 0x0C);
    assert!(UsbIoUniqueDescriptor::SCHEMA_SIZE == 0x06);
    assert!(UsbIoUniqueInput::SCHEMA_SIZE == 0x38);
    assert!(UsbIoOutputInfo::SCHEMA_SIZE == 0x14);
    assert!(ServerUsbIoNode::SCHEMA_SIZE == 0x224);
    assert!(ServerUsbIo::SCHEMA_SIZE == 0x8A0);
};
