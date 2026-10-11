//! CAN blocks: `ServerData+0x3EE0` (`0x8AC`) and `ClientData+0x1628` (`0x1E0`).
//!
//! Evidence: `spec/controllers/can_boardio_usbdevice.md` §2.2. Both blocks are two identical per-port records
//! (server stride `0x454`, client stride `0xF0`); the server block is prefixed with the published port count.

use num_enum::TryFromPrimitive;
use widestring::U16CStr;
use zero_schema::zero;

use crate::schema::shared::Response;
use crate::schema::{client_block, server_block};

/// Number of CAN ports in both blocks.
pub const PORT_COUNT: usize = 2;
/// Capacity of `ServerPort::targets` (and the clamp applied to libam's target count).
pub const MAX_TARGETS: usize = 16;
/// Capacity of `ServerAckData::packets` (one acknowledged batch).
pub const MAX_ACK_PACKETS: usize = 16;
/// Capacity of `ClientSendInfo::commands`.
pub const MAX_SEND_COMMANDS: usize = 16;
/// Payload bytes of one CAN frame.
pub const MAX_FRAME_DATA: usize = 8;

/// `CanTargetType` (name table `0x140749DB8`, six entries).
#[zero]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum CanTargetType {
    /// `"host"`.
    Host = 0,
    /// `"motor"`.
    Motor = 1,
    /// `"sensor"`.
    Sensor = 2,
    /// `"io"`.
    Io = 3,
    /// `"led"`.
    Led = 4,
    /// `"reserved"`: also the result of every unmatched name or libam type.
    Reserved = 5,
}

impl CanTargetType {
    /// Name from the table `0x140749DB8`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Motor => "motor",
            Self::Sensor => "sensor",
            Self::Io => "io",
            Self::Led => "led",
            Self::Reserved => "reserved",
        }
    }

    /// String → type lookup (`0x14040A350`): exact, case-sensitive match, [`Self::Reserved`] otherwise.
    pub fn from_name(name: &str) -> Self {
        match name {
            "host" => Self::Host,
            "motor" => Self::Motor,
            "sensor" => Self::Sensor,
            "io" => Self::Io,
            "led" => Self::Led,
            _ => Self::Reserved,
        }
    }
}

/// `ipc::can::Task` (name table `0x1405EEE58`); any other value is rejected as unknown task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, TryFromPrimitive)]
#[repr(u32)]
pub enum CanTask {
    /// Hand a list of CAN frames to libam.
    Send = 0,
}

impl CanTask {
    /// Name from the enum-to-string table `0x1405EEE58`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Send => "Send",
        }
    }
}

/// `ServerAckPacket` (`0xC`): one received frame of the acknowledged batch.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerAckPacket<'b> {
    /// Index of the sending target.
    pub target_index: u16,
    /// CAN command byte.
    pub command: u8,
    /// Number of valid `data` bytes (≤ 8).
    pub data_size: u8,
    /// Payload, zero padded.
    pub data: &'b [u8; MAX_FRAME_DATA],
}

/// `ServerAckData` (`0xC8`): the acknowledged batch of received frames.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerAckData<'b> {
    /// Batch counter (`taskNo` in the old docs); incremented on every publication. The client echoes it in
    /// [`ClientAckData::task_no`] once it consumed the batch.
    pub task_no: u32,
    /// Valid entries of `packets` (0..=16); not cleared between batches.
    pub packet_count: u32,
    /// Frames; slots beyond `packet_count` keep stale data.
    pub packets: [ServerAckPacket<'b>; MAX_ACK_PACKETS],
}

/// `ServerTargetData` (`0x38`): one CAN node of the libam target list.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerTarget<'b> {
    /// Node type mapped from libam (`1..=5 → 0..=4`, anything else → 5).
    pub target_type: CanTargetType,
    /// Node id.
    pub id: u8,
    /// Firmware revision **[I name]**, libam `TargetInfo+0x2A`.
    pub firm_revision: u8,
    /// Firmware checksum **[I name]**, libam `TargetInfo+0x2C`.
    pub firm_sum: u16,
    /// Product name, at most 12 characters (`wcsncpy_s(dst, 13, src, 12)`).
    #[zero(capacity = 13)]
    pub product: &'b U16CStr,
    /// Custom chip name, at most 9 characters (`wcsncpy_s(dst, 10, src, 9)`).
    #[zero(capacity = 10)]
    pub custom_chip: &'b U16CStr,
}

/// `ServerPortData` (`0x454`): everything published for one CAN port.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerPort<'b> {
    /// Executor response (`can::ServerResponse`): generation + `succeeded`.
    pub response: Response,
    /// Received-frame batch.
    pub ack: ServerAckData<'b>,
    /// Valid entries of `targets` (≤ 16), written on the ready edge.
    pub target_count: u16,
    /// Node list, written on the ready edge only.
    pub targets: [ServerTarget<'b>; MAX_TARGETS],
}

/// `ServerData.can`: the published port count and the two per-port records.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerCan<'b> {
    /// Ports reported by libam (clamped to 2).
    pub port_count: u32,
    /// Per-port records.
    pub ports: [ServerPort<'b>; PORT_COUNT],
}

/// `ClientSendCommand` (`0xE`): one frame of a `Send` request.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ClientSendCommand<'b> {
    /// Destination target index.
    pub target_index: u16,
    /// CAN command byte.
    pub command: u8,
    /// Number of valid `data` bytes (> 8 makes libam build an empty packet).
    pub data_size: u8,
    /// Payload.
    pub data: &'b [u8; MAX_FRAME_DATA],
    /// libam priority (0..=15, otherwise libam rejects the send).
    pub priority: u8,
}

/// `ClientSendInfo` (`0xE4`), the `Send` payload and the only member of the `ClientInfo` union.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ClientSendInfo<'b> {
    /// Number of commands to send; the daemon does not bound it by 16.
    pub command_count: u32,
    /// Commands.
    pub commands: [ClientSendCommand<'b>; MAX_SEND_COMMANDS],
}

/// `can::ClientRequest` (`0xEC`): the request header and its per-task info bytes.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ClientRequest<'b> {
    /// Request generation (`taskNo` in the old docs).
    pub generation: u32,
    /// Raw [`CanTask`]; only [`CanTask::Send`] is accepted.
    pub task: u32,
    /// Per-task info union (`0xE4` bytes), decoded by [`ClientRequestRef::send_info`].
    pub info: &'b [u8; 0xE4],
}

/// `ClientAckData` (`0x4`): the client's acknowledgement of the last published batch.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ClientAckData {
    /// Echo of [`ServerAckData::task_no`].
    pub task_no: u32,
}

/// `ClientPortData` (`0xF0`).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ClientPort<'b> {
    /// The port's request.
    pub request: ClientRequest<'b>,
    /// Acknowledgement of the received-frame batch.
    pub ack: ClientAckData,
}

/// `ClientData.can`: the two per-port records.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ClientCan<'b> {
    /// Per-port records.
    pub ports: [ClientPort<'b>; PORT_COUNT],
}

server_block!(can, can_mut: ServerCan, ServerCanRef, ServerCanMut @ 0x3EE0);
client_block!(can, can_mut: ClientCan, ClientCanRef, ClientCanMut @ 0x1628);

impl<'w> ClientRequestRef<'w> {
    /// The request's task, `None` for an unknown value (the request fails).
    pub fn task_kind(&self) -> Option<CanTask> {
        CanTask::try_from(self.task()).ok()
    }

    /// `Send` payload. Integers and byte arrays only, so every bit pattern is a valid payload.
    pub fn send_info(&self) -> ClientSendInfoRef<'w> {
        let info: &'w [u8; 0xE4] = self.info();
        let (send, _) = ClientSendInfo::access_prefix(info).expect("an integer payload accepts every bit pattern");
        send
    }
}

const _: () = {
    assert!(ServerAckPacket::SCHEMA_SIZE == 0xC);
    assert!(ServerAckData::SCHEMA_SIZE == 0xC8);
    assert!(ServerTarget::SCHEMA_SIZE == 0x38);
    assert!(ServerPort::SCHEMA_SIZE == 0x454);
    assert!(ServerCan::SCHEMA_SIZE == 0x8AC);
    assert!(ClientSendCommand::SCHEMA_SIZE == 0xE);
    assert!(ClientSendInfo::SCHEMA_SIZE == 0xE4);
    assert!(ClientRequest::SCHEMA_SIZE == 0xEC);
    assert!(ClientAckData::SCHEMA_SIZE == 0x4);
    assert!(ClientPort::SCHEMA_SIZE == 0xF0);
    assert!(ClientCan::SCHEMA_SIZE == 0x1E0);
};
