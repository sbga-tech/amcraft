//! The JVS, input and output facades (`am::jvs`, `jvs.md` 4) and libam's handles over an empty node list.
//!
//! * [`JvsPort`]: JVS nodes, inputs and general outputs ([`JvsHandle`]). The switch, analog and rotary input states
//!   are shared with [`crate::usbio`], whose nodes report the same kinds of input with smaller capacities.
//! * [`InputPort`] and [`OutputPort`] over [`JvsNodePort`] ([`JvsNodeHandle`]): the JVS node count for the relation
//!   maps. The input and output controllers publish *relation maps*: logical ids the game looks up, each bound to a
//!   device. The process config carries one map per side, made of five blocks
//!   (`process::config::InputOutputConfig`): the common block plus one profile per detected JVS node count `1..=4`.
//!   [`RelationMap`] is the plain logical model of that config, instantiated as [`InputConfig`] and
//!   [`OutputConfig`].

use crate::result::{AmError, AmResult};
use crate::shared::Shared;

/// Number of leading elements a libam count selects out of a `capacity`-element container
/// (`min(count, capacity)`).
pub(crate) fn clamp_len(count: u64, capacity: usize) -> usize {
    usize::try_from(count).map_or(capacity, |count| count.min(capacity))
}

/// libam switch input state (`Node+0x118` for JVS, `Node+0x2E0` for USBIO: system byte at `+0`, player count
/// `u64` at `+8`, player masks `u64` from `+0x10`). Only the first `PLAYERS` masks are ever consumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwitchState<const PLAYERS: usize> {
    /// System byte (`+0x00`).
    pub system: u8,
    /// Player count as libam stores it; not clamped to `PLAYERS`.
    pub player_count: u64,
    /// Per-player 64-bit switch masks.
    pub player_masks: [u64; PLAYERS],
}

impl<const PLAYERS: usize> Default for SwitchState<PLAYERS> {
    fn default() -> Self {
        Self { system: 0, player_count: 0, player_masks: [0; PLAYERS] }
    }
}

impl<const PLAYERS: usize> SwitchState<PLAYERS> {
    /// The players amdaemon copies, `0..min(player_count, PLAYERS)`, with their masks. The rest of the matrix
    /// is left untouched (the original loop breaks once the player index reaches the matrix height).
    pub fn processed_players(&self) -> impl Iterator<Item = (usize, u64)> + '_ {
        self.player_masks.iter().copied().enumerate().take(clamp_len(self.player_count, PLAYERS))
    }
}

/// One libam analog channel (24-byte element: `u16` value at `+0`, `u64` valid-bit count at `+8`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AnalogChannel {
    /// Sampled value.
    pub value: u16,
    /// Valid-bit count (amdaemon keeps its low 16 bits).
    pub valid_bits: u64,
}

/// libam analog input state (`{u64 count; AnalogChannel[]}`); only the first `CHANNELS` channels are ever
/// consumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnalogState<const CHANNELS: usize> {
    /// Channel count as libam stores it; not clamped to `CHANNELS`.
    pub count: u64,
    /// Channels.
    pub channels: [AnalogChannel; CHANNELS],
}

impl<const CHANNELS: usize> Default for AnalogState<CHANNELS> {
    fn default() -> Self {
        Self { count: 0, channels: [AnalogChannel::default(); CHANNELS] }
    }
}

impl<const CHANNELS: usize> AnalogState<CHANNELS> {
    /// The channels amdaemon copies, `0..min(count, CHANNELS)`.
    pub fn processed(&self) -> &[AnalogChannel] {
        &self.channels[..clamp_len(self.count, CHANNELS)]
    }
}

/// libam rotary input state (`{u64 count; u16 values[]}`); only the first `CHANNELS` values are ever
/// consumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RotaryState<const CHANNELS: usize> {
    /// Channel count as libam stores it; not clamped to `CHANNELS`.
    pub count: u64,
    /// Channel values.
    pub values: [u16; CHANNELS],
}

impl<const CHANNELS: usize> Default for RotaryState<CHANNELS> {
    fn default() -> Self {
        Self { count: 0, values: [0; CHANNELS] }
    }
}

impl<const CHANNELS: usize> RotaryState<CHANNELS> {
    /// The values amdaemon copies, `0..min(count, CHANNELS)`.
    pub fn processed(&self) -> &[u16] {
        &self.values[..clamp_len(self.count, CHANNELS)]
    }
}

/// libam jvs node input state (`Node+0x118`): the switch state at `+0`, analog at `+0x48`, rotary at
/// `+0x1D0`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JvsNodeInput {
    /// System byte, player count and player masks (four players are consumed).
    pub switch: SwitchState<4>,
    /// Analog channels (sixteen are consumed).
    pub analog: AnalogState<16>,
    /// Rotary channels (sixteen are consumed).
    pub rotary: RotaryState<16>,
}

/// libam surface used by the JVS controller (`am::jvs`, statically linked into `amdaemon.exe`).
///
/// Node indices are the array index of the node controller (`0..4`). Calls for an index at or beyond libam's
/// node count fail with `am::Result` code 1.
pub trait JvsPort {
    /// `jvs::handle(facade, &res)` (`0x1402187B0`). `Err` covers both a failed `am::Result` (code 2: the libam
    /// context is absent) and a null handle; the original treats them identically.
    fn handle(&self) -> AmResult<()>;

    /// `jvs::nodeCount()` (`0x140216A50`): size of libam's node vector.
    fn node_count(&self) -> u64;

    /// `jvs::nodeAutoOutputZero(handle, index, &res)` (`0x140216A70`): `NodeConfig+1`.
    fn node_auto_output_zero(&self, node: usize) -> AmResult<bool>;

    /// General-output slot write: `jvs::nodeOutputSlot(handle, index, &res)` (`0x140216AF0`, `Node+0x100`)
    /// followed by `GeneralOutput::set(bits, 8)` (`0x140225640`). No readiness, mode or "function disabled"
    /// check exists on this path.
    fn set_general_output(&mut self, node: usize, bits: u64) -> AmResult<()>;

    /// `jvs::nodeInput(handle, index, &res)` (`0x140216B70`): `Err` when the index is out of range (the
    /// original also treats a null pointer as "skip").
    fn node_input(&self, node: usize) -> AmResult<JvsNodeInput>;
}

/// Largest JVS node count that has a profile.
pub const MAX_JVS_NODES: u64 = 4;

/// Blocks of one relation map: the common block plus the profiles for `1..=`[`MAX_JVS_NODES`] detected JVS
/// nodes.
pub const CONFIG_BLOCKS: usize = MAX_JVS_NODES as usize + 1;

/// Maximum number of relations in one relation set.
pub const MAX_RELATIONS: usize = 64;

/// Number of per-player sets following `system` in a block.
pub const PLAYER_SETS: usize = 8;

/// libam JVS surface read by the refresh of both map controllers (`spec/controllers/input_output.md` §5.2, §8).
pub trait JvsNodePort {
    /// `am::jvs::handle(Manager+8, &Result)` (`0x1402187B0`): the facade handle check. Fails with
    /// `AmError::NOT_INITIALIZED` (result code 2) when libam has not been initialised.
    fn jvs_handle(&mut self) -> AmResult<()>;

    /// `am::jvs::nodeCount` (`0x140216A50`): the JVS node count libam currently reports. Only queried
    /// after [`Self::jvs_handle`] succeeded.
    fn jvs_node_count(&mut self) -> u64;
}

/// libam port of the input controller.
pub trait InputPort: JvsNodePort {}

/// Pushing into a [`RelationSet`] that already holds [`MAX_RELATIONS`] relations (the original's config
/// validator throws "too many relation").
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("too many relation")]
pub struct TooManyRelations;

/// One relation list (`system`, or one of the `players`): at most [`MAX_RELATIONS`] relations in config
/// order. Duplicate ids are never detected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationSet<R> {
    relations: Vec<R>,
}

impl<R> Default for RelationSet<R> {
    fn default() -> Self {
        Self { relations: Vec::new() }
    }
}

impl<R> RelationSet<R> {
    /// Appends `relation` after the existing ones.
    ///
    /// # Errors
    /// [`TooManyRelations`] when the set is full; the relation is dropped.
    pub fn push(&mut self, relation: R) -> Result<(), TooManyRelations> {
        if self.relations.len() == MAX_RELATIONS {
            return Err(TooManyRelations);
        }
        self.relations.push(relation);
        Ok(())
    }

    /// The relations in config order.
    pub fn as_slice(&self) -> &[R] {
        &self.relations
    }
}

/// One block of a relation map: the `system` set followed by the per-player sets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationBlock<R> {
    /// Machine-wide relations.
    pub system: RelationSet<R>,
    /// Per-player relations, `players[0]` = player 1.
    pub players: [RelationSet<R>; PLAYER_SETS],
}

impl<R> Default for RelationBlock<R> {
    fn default() -> Self {
        Self { system: RelationSet::default(), players: std::array::from_fn(|_| RelationSet::default()) }
    }
}

/// `process::config::InputOutputConfig`: the relation map of one side as the config loader produces it.
///
/// Block 0 is the *common* block (non-JVS relations); block `k` (`1..=`[`MAX_JVS_NODES`]) holds the JVS
/// relations that apply when libam detects `k` JVS nodes. A default map is all-empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationMap<R> {
    /// The [`CONFIG_BLOCKS`] blocks: common first, then the profiles for one to four nodes.
    pub blocks: [RelationBlock<R>; CONFIG_BLOCKS],
}

impl<R> Default for RelationMap<R> {
    fn default() -> Self {
        Self { blocks: std::array::from_fn(|_| RelationBlock::default()) }
    }
}

/// The input side of the process config (`process::Config+0xA8`).
pub type InputConfig = RelationMap<InputRelation>;

/// An input relation: a logical id bound to an input device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputRelation {
    /// Logical id the game looks the relation up by (config key `id`). The published field holds at most 23
    /// UTF-16 units; a longer id is cut there, and the id ends at its first NUL.
    pub id: String,
    /// Device the id is bound to.
    pub device: InputDevice,
}

/// `input::Device`: an input device type with its parameters (config string table `0x14074AC30`).
///
/// `index` is the index inside the device, in the range given per variant; `node`, `port`, `target` and `player`
/// are the type's info keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputDevice {
    /// `pushsw`: push switch, `index` `0..=1`.
    PushSwitch {
        /// Switch index.
        index: u16,
    },
    /// `jvs_switch`: `index` `0..=63` for a player switch, `0..=7` for a system switch.
    JvsSwitch {
        /// Switch index.
        index: u16,
        /// `jvs_node`, `0..=3`.
        node: u8,
        /// `jvs_player`, `0..=3`; `None` selects the system switches (config value `-1`).
        player: Option<u8>,
    },
    /// `jvs_analog`: `index` `0..=15`.
    JvsAnalog {
        /// Channel index.
        index: u16,
        /// `jvs_node`, `0..=3`.
        node: u8,
    },
    /// `jvs_rotary`: `index` `0..=15`.
    JvsRotary {
        /// Channel index.
        index: u16,
        /// `jvs_node`, `0..=3`.
        node: u8,
    },
    /// `can_switch`: `index` `0..=31`.
    CanSwitch {
        /// Switch index.
        index: u16,
        /// `can_port`, `0..=1`.
        port: u8,
        /// `can_target`, `0..=15`.
        target: u8,
    },
    /// `can_analog`: `index` `0..=7`.
    CanAnalog {
        /// Channel index.
        index: u16,
        /// `can_port`, `0..=1`.
        port: u8,
        /// `can_target`, `0..=15`.
        target: u8,
    },
    /// `can_rotary`: `index` `0..=3`.
    CanRotary {
        /// Channel index.
        index: u16,
        /// `can_port`, `0..=1`.
        port: u8,
        /// `can_target`, `0..=15`.
        target: u8,
    },
    /// `usb_switch`: `index` `0..=63` for a player switch, `0..=1` for a system switch.
    UsbSwitch {
        /// Switch index.
        index: u16,
        /// `usb_node`, `0..=3`.
        node: u8,
        /// `usb_player`, `0..=1`; `None` selects the system switches (config value `-1`).
        player: Option<u8>,
    },
    /// `usb_analog`: `index` `0..=7`.
    UsbAnalog {
        /// Channel index.
        index: u16,
        /// `usb_node`, `0..=3`.
        node: u8,
    },
    /// `usb_rotary`: `index` `0..=3`.
    UsbRotary {
        /// Channel index.
        index: u16,
        /// `usb_node`, `0..=3`.
        node: u8,
    },
}

/// libam port of the output controller.
pub trait OutputPort: JvsNodePort {}

/// The output side of the process config (`process::Config+0x26082`).
pub type OutputConfig = RelationMap<OutputRelation>;

/// An output relation: a logical id bound to an output device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputRelation {
    /// Logical id the game looks the relation up by (config key `id`). The published field holds at most 23
    /// UTF-16 units; a longer id is cut there, and the id ends at its first NUL.
    pub id: String,
    /// Device the id is bound to.
    pub device: OutputDevice,
}

/// `output::Device`: an output device type with its parameters (config string table `0x14074ACF8`).
///
/// `index` is the bit, LED or PWM-channel index in the range given per variant; `count` is the number of
/// consecutive bits starting at `index`; `node`, `port`, `target`, `command` and `data_size` are the type's info
/// keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputDevice {
    /// `led`: `index` `0..=1`; the config does not carry a count.
    Led {
        /// LED index.
        index: u16,
    },
    /// `jvs_general`: bits `[index, index + count)` of the node's output word, `index` `0..=63`.
    JvsGeneral {
        /// First bit.
        index: u16,
        /// Number of bits, `0..=64 - index`.
        count: u16,
        /// `jvs_node`, `0..=3`.
        node: u8,
    },
    /// `can_request`: bits of the request data, `index` `0..=8 * data_size`.
    CanRequest {
        /// First bit.
        index: u16,
        /// Number of bits, `0..=8 * data_size - index`.
        count: u16,
        /// `can_port`, `0..=1`.
        port: u8,
        /// `can_target`, `0..=15`.
        target: u8,
        /// `can_command`, `0..=255`.
        command: u8,
        /// `can_data_size`, `0..=8`.
        data_size: u8,
    },
    /// `usb_general`: bits `[index, index + count)` of the node's output word, `index` `0..=63`.
    UsbGeneral {
        /// First bit.
        index: u16,
        /// Number of bits, `0..=64 - index`.
        count: u16,
        /// `usb_node`, `0..=3`.
        node: u8,
    },
    /// `usb_pwm`: PWM channel `index` `0..=7`; the config does not carry a count.
    UsbPwm {
        /// PWM channel.
        index: u16,
        /// `usb_node`, `0..=3`.
        node: u8,
    },
}

/// `JvsPort` handle over the shared libam state.
#[derive(Debug)]
pub struct JvsHandle(pub(crate) Shared);

/// `JvsNodePort` handle over the shared libam state; also serves `InputPort` and `OutputPort`.
#[derive(Debug)]
pub struct JvsNodeHandle(pub(crate) Shared);

/// Number of nodes in the node list (`0x140216A50`): always 0 [jvs.facade.node_count].
const NODE_COUNT: u64 = 0;

/// The failure of every indexed facade call: `index >= nodeCount` is always true, so the result is code 1 with the
/// message `invalid num` (`jvs.md` 4.3 to 4.5). None of these calls tests the libam context pointer; only `handle`
/// does [jvs.facade.handle].
fn invalid_num() -> AmError {
    AmError::with_message(AmError::INVALID_ARGUMENT, "invalid num")
}

impl JvsPort for JvsHandle {
    /// `0x1402187B0`: ok while libam is initialised, else result code 2 [jvs.facade.handle].
    fn handle(&self) -> AmResult<()> {
        self.0.guard()
    }

    fn node_count(&self) -> u64 {
        NODE_COUNT
    }

    /// `0x140216A70` [jvs.facade.auto_output_zero].
    fn node_auto_output_zero(&self, _node: usize) -> AmResult<bool> {
        Err(invalid_num())
    }

    /// `0x140216AF0`: the slot lookup fails, so `GeneralOutput::set` is never reached
    /// [jvs.facade.output_slot].
    fn set_general_output(&mut self, _node: usize, _bits: u64) -> AmResult<()> {
        Err(invalid_num())
    }

    /// `0x140216B70` [jvs.facade.node_input].
    fn node_input(&self, _node: usize) -> AmResult<JvsNodeInput> {
        Err(invalid_num())
    }
}

impl JvsNodePort for JvsNodeHandle {
    /// `0x1402187B0`, see [`JvsHandle::handle`].
    fn jvs_handle(&mut self) -> AmResult<()> {
        self.0.guard()
    }

    fn jvs_node_count(&mut self) -> u64 {
        NODE_COUNT
    }
}

impl InputPort for JvsNodeHandle {}

impl OutputPort for JvsNodeHandle {}
