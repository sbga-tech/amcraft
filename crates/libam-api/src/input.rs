//! JVS node count for the input relation map, and the input map configuration.
//!
//! The input and output controllers publish *relation maps*: logical ids the game looks up, each bound to a
//! device. The process config carries one map per side, made of five blocks (`process::config::InputOutputConfig`):
//! the common block plus one profile per detected JVS node count `1..=4`. This module holds the plain logical
//! model of that config ([`RelationMap`]); [`crate::output`] instantiates it with its own relation type.

use crate::result::AmResult;

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
