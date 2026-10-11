//! Input block (`ServerData+0x94EC`, 0x7992).
//!
//! `amdaemon::ipc::InputOutputServerData<InputOutputId<InputIdTag>, input::Device>`: one `system` set followed by
//! eight `players` sets, each set `{u16 count; Relation relations[64]}` and each relation
//! `{wchar_t id[24]; Device device}`. The output block ([`crate::schema::output`]) instantiates the same template
//! with its own device. Nothing in either block needs more than 2-byte alignment.
//!
//! Evidence: `spec/controllers/input_output.md` §2 (`input_output.*` claims).

use widestring::U16CStr;
use zero_schema::zero;

use crate::schema::server_block;

/// Capacity of a relation id in `wchar_t` units, terminator included.
pub const ID_CAPACITY: usize = 24;
/// Maximum number of relations in one relation set.
pub const MAX_RELATIONS: usize = 64;
/// Number of per-player sets following `system` in a block.
pub const PLAYER_SETS: usize = 8;

/// `input::DeviceType`, stored as a `u16` in [`InputDevice::kind`] (config string table `0x14074AC30`).
#[zero]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum InputDeviceType {
    /// Config string `none`; the config reader rejects it, zero in unused relation slots.
    Invalid = 0,
    /// `pushsw`: push switch, `index` `0..=1`.
    PushSwitch = 1,
    /// `jvs_switch`: `index` `0..=63` for a player switch, `0..=7` for a system switch.
    JvsSwitch = 2,
    /// `jvs_analog`: `index` `0..=15`.
    JvsAnalog = 3,
    /// `jvs_rotary`: `index` `0..=15`.
    JvsRotary = 4,
    /// `can_switch`: `index` `0..=31`.
    CanSwitch = 5,
    /// `can_analog`: `index` `0..=7`.
    CanAnalog = 6,
    /// `can_rotary`: `index` `0..=3`.
    CanRotary = 7,
    /// `usb_switch`: `index` `0..=63` for a player switch, `0..=1` for a system switch.
    UsbSwitch = 8,
    /// `usb_analog`: `index` `0..=7`.
    UsbAnalog = 9,
    /// `usb_rotary`: `index` `0..=3`.
    UsbRotary = 10,
}

/// `player` byte of a [`JvsInfo`] / [`UsbInfo`] that selects the *system* group (config value `-1`).
pub const SYSTEM_PLAYER: u8 = 0xFF;

/// Device info of `JvsSwitch` / `JvsAnalog` / `JvsRotary` (`info` bytes `+0`, `+1`).
#[zero]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JvsInfo {
    /// `jvs_node`, `0..=3` (default 0).
    pub node: u8,
    /// `jvs_player` (switches only), `0..=3` or [`SYSTEM_PLAYER`] (default); analog and rotary keep the default.
    pub player: u8,
}

/// Device info of `CanSwitch` / `CanAnalog` / `CanRotary` (`info` bytes `+0`, `+1`).
#[zero]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanInfo {
    /// `can_port`, `0..=1`.
    pub port: u8,
    /// `can_target`, `0..=15`.
    pub target: u8,
}

/// Device info of `UsbSwitch` / `UsbAnalog` / `UsbRotary` (`info` bytes `+0`, `+1`).
#[zero]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsbInfo {
    /// `usb_node`, `0..=3` (default 0).
    pub node: u8,
    /// `usb_player` (switches only), `0..=1` or [`SYSTEM_PLAYER`] (default); analog and rotary keep the default.
    pub player: u8,
}

/// The two-byte `info` union of an [`InputDevice`], selected by its device type.
#[zero]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputInfo {
    /// Unused relation slot: no info.
    #[zero(tag = InputDeviceType::Invalid)]
    Invalid,
    /// Push switch: no info.
    #[zero(tag = InputDeviceType::PushSwitch)]
    PushSwitch,
    /// JVS switch.
    #[zero(tag = InputDeviceType::JvsSwitch)]
    JvsSwitch(JvsInfo),
    /// JVS analog input.
    #[zero(tag = InputDeviceType::JvsAnalog)]
    JvsAnalog(JvsInfo),
    /// JVS rotary encoder.
    #[zero(tag = InputDeviceType::JvsRotary)]
    JvsRotary(JvsInfo),
    /// CAN switch.
    #[zero(tag = InputDeviceType::CanSwitch)]
    CanSwitch(CanInfo),
    /// CAN analog input.
    #[zero(tag = InputDeviceType::CanAnalog)]
    CanAnalog(CanInfo),
    /// CAN rotary encoder.
    #[zero(tag = InputDeviceType::CanRotary)]
    CanRotary(CanInfo),
    /// USB switch.
    #[zero(tag = InputDeviceType::UsbSwitch)]
    UsbSwitch(UsbInfo),
    /// USB analog input.
    #[zero(tag = InputDeviceType::UsbAnalog)]
    UsbAnalog(UsbInfo),
    /// USB rotary encoder.
    #[zero(tag = InputDeviceType::UsbRotary)]
    UsbRotary(UsbInfo),
}

/// `input::Device`: `{u16 type; u16 index; Info info}` (6 bytes).
#[zero(endian = "little")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputDevice {
    /// `input::DeviceType`; the zero value is the unused slot.
    pub kind: InputDeviceType,
    /// Index inside the device (range by type, see [`InputDeviceType`]); signedness is not observable.
    pub index: u16,
    /// Union bytes selected by [`Self::kind`].
    #[zero(tag_field = kind)]
    pub info: InputInfo,
}

/// `InputOutputRelation<InputOutputId<InputIdTag>, input::Device>` (0x36 bytes): a logical id bound to a device.
#[zero(endian = "little")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputRelation<'b> {
    /// Logical id the game looks the relation up by (at most 23 units; duplicates are never detected).
    #[zero(capacity = 24)]
    pub id: &'b U16CStr,
    /// Device the id is bound to.
    pub device: InputDevice,
}

/// `InputOutputRelationSet<Relation>` (0xD82 bytes). Entries at index `>= count` are all-zero in published data.
#[zero(endian = "little")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputRelationSet<'b> {
    /// Number of valid relations, `0..=64`.
    pub count: u16,
    /// Relation storage.
    pub relations: [InputRelation<'b>; MAX_RELATIONS],
}

/// `ServerData.input`: the `system` set followed by `players[8]`.
#[zero(endian = "little")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerInput<'b> {
    /// Machine-wide relations (test/service/coin-type inputs and similar).
    pub system: InputRelationSet<'b>,
    /// Per-player relations, `players[0]` = player 1.
    pub players: [InputRelationSet<'b>; PLAYER_SETS],
}

server_block!(input, input_mut: ServerInput, ServerInputRef, ServerInputMut @ 0x94EC);

const _: () = {
    assert!(InputDevice::SCHEMA_SIZE == 6);
    assert!(InputRelation::SCHEMA_SIZE == 0x36);
    assert!(InputRelationSet::SCHEMA_SIZE == 0xD82);
    assert!(ServerInput::SCHEMA_SIZE == 0x7992);
};

#[cfg(test)]
mod tests {
    use crate::schema::image::ServerImage;
    use crate::schema::text::fit_wide_str;

    use super::*;

    /// Bytes of a relation placed in `players[1]` slot 2 land at the spec's absolute offsets.
    #[test]
    fn relation_fields_serialise_at_spec_offsets() {
        let mut image = ServerImage::new();
        let id = fit_wide_str("START", ID_CAPACITY);
        let patch = InputRelationPatch {
            id: Some(&id),
            device: Some(InputDevicePatch {
                kind: None,
                index: Some(0x0102),
                info: Some(InputInfoPatch::JvsSwitch(JvsInfoPatch { node: Some(3), player: Some(SYSTEM_PLAYER) })),
            }),
        };
        let mut block = image.input_mut();
        let mut players = block.players_mut();
        let mut set = players.get_mut(1).expect("eight player sets");
        set.count_mut().set(3).expect("u16 accepts every value");
        set.relations_mut().get_mut(2).expect("slot below 64").copy_from(&patch).expect("id fits the field");

        let bytes = &image.bytes()[0x94EC..0x94EC + ServerInput::SCHEMA_SIZE];
        let set = 0x1B04;
        assert_eq!(&bytes[set..set + 2], &[3, 0]);
        let rel = set + 2 + 2 * 0x36;
        assert_eq!(&bytes[rel..rel + 12], b"S\0T\0A\0R\0T\0\0\0");
        assert_eq!(&bytes[rel + 0x30..rel + 0x36], &[2, 0, 0x02, 0x01, 3, 0xFF]);
    }
}
