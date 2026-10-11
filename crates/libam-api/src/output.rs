//! The output relation map facade and configuration.

use crate::input::{JvsNodePort, RelationMap};

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
