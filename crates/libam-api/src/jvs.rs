//! `am::jvs`: JVS nodes, inputs and general outputs.
//!
//! The switch, analog and rotary input states are shared with [`crate::usb_io`], whose nodes report the same
//! kinds of input with smaller capacities.

use crate::result::AmResult;

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
