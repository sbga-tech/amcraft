//! JVS blocks: `ClientData+0xB78` (0x40) and the JVS half of the ServerIO body at `body+0x8A0` (0xDC8).
//!
//! Evidence: `spec/controllers/jvs_usbio.md` §2.5–§2.6 (ServerIO) and §3.1 (requests); claims
//! `jvs_usbio.serverio.jvs.*` and `jvs_usbio.client.jvs.*`. The switch records ([`SwitchUnit`],
//! [`PlayerSwitches`]) are shared with the USBIO block ([`crate::schema::usb_io`]).

use num_enum::TryFromPrimitive;
use zero_schema::zero;

use crate::schema::{client_block, server_io_block};

/// Node slots in both the request array and the response array. The count is fixed (`4 × 0x10` requests,
/// `4 × 0x370` responses) no matter how many nodes libam detects.
pub const NODE_CAPACITY: usize = 4;
/// System switches of a JVS node (one per bit of libam's system byte).
pub const SYSTEM_SWITCHES: usize = 8;
/// Switches of one player row (one per bit of libam's 64-bit player mask).
pub const PLAYER_SWITCHES: usize = 64;
/// Players of the JVS switch matrix.
pub const JVS_PLAYERS: usize = 4;
/// Analog and rotary channels of a JVS node.
pub const JVS_CHANNELS: usize = 16;

/// `jvs::ClientRequest` (`0x10`), one per node: the request generation, the raw task and the task payload.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ClientJvsRequest<'b> {
    /// Request generation (`taskNo`); the client changes it to submit.
    pub generation: u32,
    /// Raw [`JvsTask`].
    pub task: u32,
    /// Task payload (`jvs::ClientInfo`, a one-member union), decoded as [`GeneralOutputInfo`].
    pub info: &'b [u8; 8],
}

/// `ClientData.jvs` (`0x40` bytes): `jvs::ClientData { ClientNodeData nodes[4] }`.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ClientJvs<'b> {
    /// Request of node *i* (node index = array index).
    pub nodes: [ClientJvsRequest<'b>; NODE_CAPACITY],
}

/// `ipc::jvs::Task` (name table `0x1405EEEE0`): `GeneralOutput` is the only task that exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, TryFromPrimitive)]
#[repr(u32)]
pub enum JvsTask {
    /// Writes the node's libam general-output slot ([`GeneralOutputInfo`]).
    GeneralOutput = 0,
}

impl JvsTask {
    /// Name printed in the request trace (`0x1405EEEE0[0]`).
    pub const fn name(self) -> &'static str {
        match self {
            Self::GeneralOutput => "GeneralOutput",
        }
    }
}

/// `GeneralOutput` payload.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct GeneralOutputInfo {
    /// Value handed to libam's general-output slot (`set(bits, 8)`).
    pub bits: u64,
}

impl<'w> ClientJvsRef<'w> {
    /// The request of node `index`.
    ///
    /// # Panics
    /// If `index` is not below [`NODE_CAPACITY`].
    pub fn request(&self, index: usize) -> ClientJvsRequestRef<'w> {
        self.nodes().get(index).expect("a node index is below the node capacity")
    }
}

impl ClientJvsRequestRef<'_> {
    /// The request's task, `None` for an unknown value (the request fails).
    pub fn task_kind(&self) -> Option<JvsTask> {
        JvsTask::try_from(self.task()).ok()
    }

    /// `GeneralOutput` payload: the bits for libam's general-output slot.
    pub fn bits(&self) -> u64 {
        let (payload, _) =
            GeneralOutputInfo::access_prefix(self.info()).expect("an integer payload accepts every bit pattern");
        payload.bits()
    }
}

/// `SwitchUnit` `{u8 on; u8 countToOn; u8 countToOff}` (3 bytes), shared by the JVS and USBIO input regions.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct SwitchUnit {
    /// Current level.
    pub on: bool,
    /// Rising-edge counter (wraps at 256).
    pub count_to_on: u8,
    /// Falling-edge counter (wraps at 256).
    pub count_to_off: u8,
}

impl SwitchUnitMut<'_> {
    /// Update rule `U(cur, new)` (`0x140038AA0`, `0x14004A850`): on a level change store the new level and
    /// count the edge (`u8` wraparound); an unchanged level touches nothing.
    pub fn update(&mut self, on: bool) {
        if on == self.on() {
            return;
        }
        let patch = if on {
            SwitchUnitPatch {
                on: Some(true),
                count_to_on: Some(self.count_to_on().wrapping_add(1)),
                ..SwitchUnitPatch::default()
            }
        } else {
            SwitchUnitPatch {
                on: Some(false),
                count_to_off: Some(self.count_to_off().wrapping_add(1)),
                ..SwitchUnitPatch::default()
            }
        };
        self.copy_from(&patch).expect("scalar fields accept every value");
    }
}

/// One player's row of the switch matrix: `SwitchUnit units[64]`.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct PlayerSwitches {
    /// Switch *k* is bit *k* of libam's 64-bit player mask.
    pub units: [SwitchUnit; PLAYER_SWITCHES],
}

impl PlayerSwitchesMut<'_> {
    /// Applies libam switch bits: bit *k* of `mask` drives `units[k]`.
    pub fn update(&mut self, mask: u64) {
        let mut units = self.units_mut();
        for bit in 0..PLAYER_SWITCHES {
            units.get_mut(bit).expect("a mask bit selects one of the player's switches").update((mask >> bit) & 1 != 0);
        }
    }
}

/// JVS `switchInput` (node+`0x004`, `0x31C` bytes), written by `sub_140038AA0`.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct JvsSwitchInput {
    /// System switches, bit *k* of libam's system byte.
    pub systems: [SwitchUnit; SYSTEM_SWITCHES],
    /// libam player count truncated to `u32`; **not** clamped (can exceed 4).
    pub player_count: u32,
    /// Per-player switch matrix; only the first four libam players are ever copied.
    pub players: [PlayerSwitches; JVS_PLAYERS],
}

/// JVS `analogInput` (node+`0x320`, `0x28` bytes), written by `sub_140038BC0`. The record size includes two
/// bytes of tail padding that are never written.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct JvsAnalogInput {
    /// libam channel count truncated to `u32`; not clamped.
    pub channel_count: u32,
    /// Channel values; only `min(channel_count, 16)` are written.
    pub values: [u16; JVS_CHANNELS],
    /// Valid-bit count rewritten for every processed channel, so it equals the **last** processed channel's
    /// value (untouched when no channel is processed).
    pub valid_bit_count: u16,
}

/// JVS `rotaryInput` (node+`0x348`, `0x24` bytes), written by `sub_140038C60`.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct JvsRotaryInput {
    /// libam channel count truncated to `u32`; not clamped.
    pub channel_count: u32,
    /// Channel values; only `min(channel_count, 16)` are written.
    pub values: [u16; JVS_CHANNELS],
}

/// `ServerIO` JVS node (`0x370` bytes), at `body+0x8A4+0x370*i`.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerJvsNode {
    /// `response.taskNo`; written only by `checkResult` (`0x140039F10`). The JVS response has no
    /// `succeeded` byte.
    pub generation: u32,
    /// Input switches.
    pub switch_input: JvsSwitchInput,
    /// Analog channels.
    pub analog_input: JvsAnalogInput,
    /// Rotary channels.
    pub rotary_input: JvsRotaryInput,
    /// libam `NodeConfig+1`, refreshed on detect only (`updateNodeInfo`).
    pub auto_output_zero: bool,
}

/// JVS half of the ServerIO body (`body+0x8A0`, `0xDC8` bytes).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerJvs {
    /// Detected node count (`<= 4`; `0` when libam's jvs handle is unavailable).
    pub node_count: u32,
    /// Node slots.
    pub nodes: [ServerJvsNode; NODE_CAPACITY],
    /// Detect-callback counter (wraps at `2^32`).
    pub detect_counter: u32,
}

impl ServerJvsRef<'_> {
    /// The response generation of node `index`.
    ///
    /// # Panics
    /// If `index` is not below [`NODE_CAPACITY`].
    pub fn node_generation(&self, index: usize) -> u32 {
        self.nodes().get(index).expect("a node index is below the node capacity").generation()
    }
}

client_block!(jvs, jvs_mut: ClientJvs, ClientJvsRef, ClientJvsMut @ 0xB78);
server_io_block!(jvs, jvs_mut: ServerJvs, ServerJvsRef, ServerJvsMut @ 0x8A0);

const _: () = {
    assert!(ClientJvsRequest::SCHEMA_SIZE == 0x10);
    assert!(ClientJvs::SCHEMA_SIZE == 0x40);
    assert!(GeneralOutputInfo::SCHEMA_SIZE == 0x08 && GeneralOutputInfo::SCHEMA_ALIGN == 8);
    assert!(SwitchUnit::SCHEMA_SIZE == 3);
    assert!(PlayerSwitches::SCHEMA_SIZE == 0xC0);
    assert!(JvsSwitchInput::SCHEMA_SIZE == 0x31C);
    assert!(JvsAnalogInput::SCHEMA_SIZE == 0x28);
    assert!(JvsRotaryInput::SCHEMA_SIZE == 0x24);
    assert!(ServerJvsNode::SCHEMA_SIZE == 0x370);
    assert!(ServerJvs::SCHEMA_SIZE == 0xDC8);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switch_unit_counts_each_edge_once_and_wraps_at_256() {
        let mut bytes = [0_u8; SwitchUnit::SCHEMA_SIZE];
        let mut unit = SwitchUnit::access_mut(&mut bytes).expect("an all-zero switch unit is valid");
        unit.update(false);
        assert_eq!(
            (unit.on(), unit.count_to_on(), unit.count_to_off()),
            (false, 0, 0),
            "unchanged level must not count"
        );
        for _ in 0..257 {
            unit.update(true);
            unit.update(false);
        }
        assert_eq!((unit.on(), unit.count_to_on(), unit.count_to_off()), (false, 1, 1));
    }
}
