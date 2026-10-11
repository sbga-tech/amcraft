//! Output block (`ServerData+0x10E7E`, 0x8292).
//!
//! Same template as the input block ([`crate::schema::input`]): a `system` set followed by eight `players` sets,
//! each set `{u16 count; Relation relations[64]}`, with the 58-byte output relation. The block starts at a
//! 2-byte-aligned address, and nothing in it needs more than 2-byte alignment.
//!
//! Evidence: `spec/controllers/input_output.md` §2.4–2.5 (`input_output.*` claims).

use widestring::U16CStr;
use zero_schema::zero;

use crate::schema::input::{MAX_RELATIONS, PLAYER_SETS};
use crate::schema::server_block;

/// `output::DeviceType`, stored as a `u16` in [`OutputDevice::kind`] (config string table `0x14074ACF8`).
#[zero]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum OutputDeviceType {
    /// Config string `none`; the config reader rejects it, zero in unused relation slots.
    Invalid = 0,
    /// `led`: `index` `0..=1`, `count` not read.
    Led = 1,
    /// `jvs_general`: bits `[index, index + count)` of the node's output word, `index` `0..=63`.
    JvsGeneral = 2,
    /// `can_request`: bits of the request data, `index` `0..=8 * data_size`.
    CanRequest = 3,
    /// `usb_general`: bits `[index, index + count)` of the node's output word, `index` `0..=63`.
    UsbGeneral = 4,
    /// `usb_pwm`: PWM channel `index` `0..=7`, `count` not read.
    UsbPwm = 5,
}

/// Device info of `JvsGeneral` (`info` byte `+0`).
#[zero]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputJvsInfo {
    /// `jvs_node`, `0..=3` (default 0).
    pub node: u8,
}

/// Device info of `CanRequest` (`info` bytes `+0..+3`).
#[zero]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputCanInfo {
    /// `can_port`, `0..=1` (default 0).
    pub port: u8,
    /// `can_target`, `0..=15` (default 0).
    pub target: u8,
    /// `can_command`, `0..=255`, required by the config reader.
    pub command: u8,
    /// `can_data_size`, `0..=8`, required by the config reader.
    pub data_size: u8,
}

/// Device info of `UsbGeneral` and `UsbPwm` (`info` byte `+0`).
#[zero]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputUsbInfo {
    /// `usb_node`, `0..=3` (default 0).
    pub node: u8,
}

/// The four-byte `info` union of an [`OutputDevice`], selected by its device type.
#[zero]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputInfo {
    /// Unused relation slot: no info.
    #[zero(tag = OutputDeviceType::Invalid)]
    Invalid,
    /// LED: no info.
    #[zero(tag = OutputDeviceType::Led)]
    Led,
    /// JVS general-purpose output.
    #[zero(tag = OutputDeviceType::JvsGeneral)]
    JvsGeneral(OutputJvsInfo),
    /// CAN request.
    #[zero(tag = OutputDeviceType::CanRequest)]
    CanRequest(OutputCanInfo),
    /// USB general-purpose output.
    #[zero(tag = OutputDeviceType::UsbGeneral)]
    UsbGeneral(OutputUsbInfo),
    /// USB PWM channel.
    #[zero(tag = OutputDeviceType::UsbPwm)]
    UsbPwm(OutputUsbInfo),
}

/// `output::Device`: `{u16 type; u16 index; u16 count; Info info}` (10 bytes).
#[zero(endian = "little")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputDevice {
    /// `output::DeviceType`; the zero value is the unused slot.
    pub kind: OutputDeviceType,
    /// Bit / LED / PWM-channel index (range by type, see [`OutputDeviceType`]).
    pub index: u16,
    /// Number of consecutive bits starting at `index` (JVS/USB general and CAN request only).
    pub count: u16,
    /// Union bytes selected by [`Self::kind`].
    #[zero(tag_field = kind)]
    pub info: OutputInfo,
}

/// `InputOutputRelation<InputOutputId<OutputIdTag>, output::Device>` (0x3A bytes): a logical id bound to a device.
#[zero(endian = "little")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputRelation<'b> {
    /// Logical id the game looks the relation up by (at most 23 units; duplicates are never detected).
    #[zero(capacity = 24)]
    pub id: &'b U16CStr,
    /// Device the id is bound to.
    pub device: OutputDevice,
}

/// `InputOutputRelationSet<Relation>` (0xE82 bytes). Entries at index `>= count` are all-zero in published data.
#[zero(endian = "little")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputRelationSet<'b> {
    /// Number of valid relations, `0..=64`.
    pub count: u16,
    /// Relation storage.
    pub relations: [OutputRelation<'b>; MAX_RELATIONS],
}

/// `ServerData.output`: the `system` set followed by `players[8]`.
#[zero(endian = "little")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerOutput<'b> {
    /// Machine-wide relations (LEDs and similar).
    pub system: OutputRelationSet<'b>,
    /// Per-player relations, `players[0]` = player 1.
    pub players: [OutputRelationSet<'b>; PLAYER_SETS],
}

server_block!(output, output_mut: ServerOutput, ServerOutputRef, ServerOutputMut @ 0x10E7E);

const _: () = {
    assert!(OutputDevice::SCHEMA_SIZE == 10);
    assert!(OutputRelation::SCHEMA_SIZE == 0x3A);
    assert!(OutputRelationSet::SCHEMA_SIZE == 0xE82);
    assert!(ServerOutput::SCHEMA_SIZE == 0x8292);
};

#[cfg(test)]
mod tests {
    use crate::schema::image::ServerImage;
    use crate::schema::input::ID_CAPACITY;
    use crate::schema::text::fit_wide_str;

    use super::*;

    /// Bytes of a CAN request relation in `system` slot 1 land at the spec's offsets.
    #[test]
    fn relation_fields_serialise_at_spec_offsets() {
        let mut image = ServerImage::new();
        let id = fit_wide_str("CAN", ID_CAPACITY);
        let patch = OutputRelationPatch {
            id: Some(&id),
            device: Some(OutputDevicePatch {
                kind: None,
                index: Some(0x0304),
                count: Some(0x0506),
                info: Some(OutputInfoPatch::CanRequest(OutputCanInfoPatch {
                    port: Some(1),
                    target: Some(15),
                    command: Some(0xAB),
                    data_size: Some(8),
                })),
            }),
        };
        let mut block = image.output_mut();
        let mut system = block.system_mut();
        system.count_mut().set(2).expect("u16 accepts every value");
        system.relations_mut().get_mut(1).expect("slot below 64").copy_from(&patch).expect("id fits the field");

        let bytes = &image.bytes()[0x10E7E..0x10E7E + ServerOutput::SCHEMA_SIZE];
        assert_eq!(&bytes[0..2], &[2, 0]);
        let rel = 2 + 0x3A;
        assert_eq!(&bytes[rel..rel + 8], b"C\0A\0N\0\0\0");
        assert_eq!(&bytes[rel + 0x30..rel + 0x3A], &[3, 0, 0x04, 0x03, 0x06, 0x05, 1, 15, 0xAB, 8]);
    }
}
