//! The CAN facade (`am::can`, `can.md` 2.3): ports, targets and packets, and libam's [`CanHandle`], the facade over a
//! manager with zero ports.
//!
//! The statically linked libam of `amdaemonExe Ver.91f83d14` contains only the *disabled* CAN manager: its port
//! count is 0, every `send` fails and no listener callback is ever invoked. [`DisabledCanPort`] is that backend
//! behind [`CanPort`]; a real backend only has to implement the trait.

use crate::result::{AmError, AmResult};
use crate::shared::Shared;

/// Payload bytes of one CAN frame (`am::can::Packet::data`).
pub const MAX_FRAME_DATA: usize = 8;

/// libam `am::can::Packet` (`0x14021C1F0`): `{u8 command; u64 size; u8 data[8]}`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CanPacket {
    /// CAN command byte.
    pub command: u8,
    /// Number of valid `data` bytes.
    pub size: u64,
    /// Payload, zero padded.
    pub data: [u8; MAX_FRAME_DATA],
}

impl CanPacket {
    /// `Packet::new(command, data, size)` (`0x14021C1F0`): with `size > 8` libam keeps the empty packet
    /// (command 0, size 0) and sets an ignored error; the send then still happens with that packet.
    pub fn new(command: u8, data: &[u8; MAX_FRAME_DATA], size: u8) -> Self {
        let len = usize::from(size);
        if len > MAX_FRAME_DATA {
            return Self::default();
        }
        let mut packet = Self { command, size: u64::from(size), data: [0; MAX_FRAME_DATA] };
        packet.data[..len].copy_from_slice(&data[..len]);
        packet
    }
}

/// libam `am::can::TargetInfo` as read by `clearServerData` (getters `0x14021C590..0x14021C5E0`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CanTargetInfo {
    /// libam node type: `1..=5` are the node kinds amdaemon publishes by name (host, motor, sensor, io, led);
    /// anything else is unknown to it.
    pub target_type: u32,
    /// Node id.
    pub id: u8,
    /// Product name (published truncated to 12 characters).
    pub product: String,
    /// Custom chip name (published truncated to 9 characters).
    pub custom_chip: String,
    /// Firmware revision (`TargetInfo+0x2A`).
    pub firm_revision: u8,
    /// Firmware checksum (`TargetInfo+0x2C`).
    pub firm_sum: u16,
}

/// Callback raised by libam through the two listener faces of `CanEventListener` (`controller+0x28`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanEvent {
    /// `SendPacketEventListener::vf1` (`0x140025FC0`). libam raises it synchronously from inside `send`;
    /// a late completion has no effect. The second (unused) argument of the original is not modelled.
    SendComplete {
        /// CAN port the frame was sent on.
        port: usize,
        /// Whether the backend sent the frame. A `false` completion is ignored.
        ok: bool,
    },
    /// `RecvPacketEventListener::vf1` (`0x140026010`): a frame arrived from `target_index`.
    Received {
        /// CAN port the frame arrived on.
        port: usize,
        /// Index of the sending target.
        target_index: u16,
        /// The frame.
        packet: CanPacket,
    },
}

/// The libam `am::can` surface used by amdaemon's CAN controller.
pub trait CanPort {
    /// `can` facade accessor `0x140218850` on `Manager+8`: `Err` (code 2) when libam is not initialised.
    fn can_facade(&self) -> AmResult<()>;

    /// `can.portCount()` (`0x140216080`).
    fn port_count(&self) -> usize;

    /// `can.targetCount(port)` (`0x1402160A0`): 0 for `port >= port_count()`.
    fn target_count(&self, port: usize) -> usize;

    /// `can.target(port, index, &res)` (`0x1402160D0`): `Err` (code 1) when out of range.
    fn target(&self, port: usize, index: usize) -> AmResult<CanTargetInfo>;

    /// `can.send(port, target, priority, packet, &res)` (`0x140215F40`). libam fails with code 1 unless
    /// `port < portCount && target < targetCount && priority <= 15`, and with code 2 when the backend
    /// refuses. A successful send raises [`CanEvent::SendComplete`] from inside this call.
    fn send(&mut self, port: usize, target_index: usize, priority: u8, packet: &CanPacket) -> AmResult<()>;

    /// Listener callbacks raised since the last call, in order. amdaemon delivers them from its libam event
    /// pump (`on_libam_events`) and right after every `send`.
    fn take_events(&mut self) -> Vec<CanEvent>;
}

/// The CAN backend of the shipped libam (the disabled CAN manager): `setup` only logs
/// `CAN not support.`, the port count (`CANMGR+0xB78`) stays 0, `send()` is `xor al,al; ret` and no
/// callback ever fires. libam is considered initialised.
#[derive(Debug, Clone, Copy, Default)]
pub struct DisabledCanPort;

impl CanPort for DisabledCanPort {
    fn can_facade(&self) -> AmResult<()> {
        Ok(())
    }

    fn port_count(&self) -> usize {
        0
    }

    fn target_count(&self, _port: usize) -> usize {
        0
    }

    fn target(&self, _port: usize, _index: usize) -> AmResult<CanTargetInfo> {
        Err(AmError::code(AmError::INVALID_ARGUMENT))
    }

    fn send(&mut self, _port: usize, _target_index: usize, _priority: u8, _packet: &CanPacket) -> AmResult<()> {
        // `port < portCount (0)` never holds.
        Err(AmError::code(AmError::INVALID_ARGUMENT))
    }

    fn take_events(&mut self) -> Vec<CanEvent> {
        Vec::new()
    }
}

/// The part of `am::AppConfig` owned by the CAN controller: the two listener pointers stored in
/// `am::can::Config` (`AppConfig+0xC28`) at `+2608` (`AppConfig+0x1658`) and `+2616` (`AppConfig+0x1660`).
/// The rest of `am::can::Config` (enable flag, port and node lists) is produced by the CAN config converter
/// (`0x14000FBD0`) and is inert with the disabled backend.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CanAppConfig {
    /// `+0x1658`: `SendPacketEventListener` (the listener at `controller+0x28`).
    pub send_listener_installed: bool,
    /// `+0x1660`: `RecvPacketEventListener` (the listener at `controller+0x30`).
    pub recv_listener_installed: bool,
}

impl CanAppConfig {
    /// libam's validator (`0x1402AD4C0`, the `am::can::Config` overload) fails an enabled CAN configuration
    /// with `CAN event listener are not assigned.` unless both are set.
    pub fn listeners_assigned(&self) -> bool {
        self.send_listener_installed && self.recv_listener_installed
    }
}

/// `CanPort` handle over the shared libam state.
#[derive(Debug)]
pub struct CanHandle(pub(crate) Shared);

/// The port count (`manager+0xB78`): written only by the constructor, so 0 for the life of the process.
const PORT_COUNT: usize = 0;

impl CanPort for CanHandle {
    /// `0x140218850`: a token while libam is initialised, else result code 2.
    fn can_facade(&self) -> AmResult<()> {
        self.0.guard()
    }

    /// `0x140216080`.
    fn port_count(&self) -> usize {
        PORT_COUNT
    }

    /// `0x1402160A0`: `port < portCount ? backend.targetCount : 0`.
    fn target_count(&self, _port: usize) -> usize {
        0
    }

    /// `0x1402160D0`: `port >= portCount` gives result code 1.
    fn target(&self, _port: usize, _index: usize) -> AmResult<CanTargetInfo> {
        Err(AmError::code(AmError::INVALID_ARGUMENT))
    }

    /// `0x140215F40`: needs `portCount > port`, which never holds, so the backend is never reached and the
    /// result is code 1 (whatever the priority).
    fn send(&mut self, _port: usize, _target_index: usize, _priority: u8, _packet: &CanPacket) -> AmResult<()> {
        Err(AmError::code(AmError::INVALID_ARGUMENT))
    }

    /// No send-complete or received-packet event is ever dispatched (`can.md` 2.4).
    fn take_events(&mut self) -> Vec<CanEvent> {
        Vec::new()
    }
}
