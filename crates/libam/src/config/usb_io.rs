//! Section `usb_io` (`Config+0x4FAAC`, `spec/libam/config.md` Part F §2).

use super::json::{Json, Map};
use super::reader::{Arr, Obj, Res, bad_described, range_u32, utf16_len};

/// `required_function.switch_input`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsbIoSwitch {
    /// `0..=2`.
    pub player: u32,
    /// `0..=64`; non-zero only with a non-zero `player`.
    pub button: u32,
}

/// `required_function.analog_input`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsbIoAnalog {
    /// `1..=8`.
    pub channel: u32,
    /// `0..=16`.
    pub bit: u32,
}

/// One `required_function.unique_io[]` entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UniqueIo {
    /// `65..=79` (ASCII `A`..`O`), unique per node.
    pub command: u8,
    /// Missing entries are `0`.
    pub params: [u8; 3],
    pub input: bool,
    pub output: bool,
}

/// `required_function` of a node; a disabled function is `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsbIoFunctions {
    pub switch_input: Option<UsbIoSwitch>,
    /// `coin_input.slot`, `1..=2`.
    pub coin_input: Option<u32>,
    pub analog_input: Option<UsbIoAnalog>,
    /// `rotary_input.channel`, `1..=4`.
    pub rotary_input: Option<u32>,
    /// `general_output.slot`, `1..=64`.
    pub general_output: Option<u32>,
    /// `pwm_output.slot`, `1..=8`.
    pub pwm_output: Option<u32>,
    /// At most nine, in config order.
    pub unique_io: Vec<UniqueIo>,
}

/// One `usb_io.node[]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbIoNode {
    /// `0..=15`, unique across the section.
    pub id: u8,
    /// `timeout` in `0..=255`; `None` when absent or `-1`.
    pub timeout: Option<u8>,
    /// `switch_sampling`, same rule as `timeout`.
    pub switch_sampling: Option<u8>,
    /// `0..=255`; only read when `pwm_output` is enabled, else the default node image's `1` (`0x140088C40`).
    pub pwm_cycle: u8,
    /// Defaults to `true`.
    pub auto_output_zero: bool,
    pub share_coin_slot: bool,
    pub required_function: UsbIoFunctions,
}

/// Section `usb_io`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsbIo {
    pub enable: bool,
    /// One to four nodes, in array order.
    pub nodes: Vec<UsbIoNode>,
    /// `firmware_path` with every `/` turned into `\`; at most 260 UTF-16 units. Not joined with the executable
    /// directory (F3).
    pub firmware_path: String,
    pub append_sub_number_to_device_error: bool,
}

impl UsbIo {
    pub(super) fn parse(root: &Map) -> Res<Self> {
        let mut out = Self::default();
        let Some(section) = Obj::section(root, "usb_io")? else { return Ok(out) };
        out.enable = section.enable()?;
        if !out.enable {
            return Ok(out);
        }

        let nodes = section.require_array("node")?;
        if nodes.len() == 0 {
            return Err(nodes.bad("node array is empty."));
        }
        if nodes.len() > 4 {
            return Err(nodes.bad("too many nodes."));
        }
        for i in 0..nodes.len() {
            out.nodes.push(UsbIoNode::parse(&nodes.object(i)?)?);
        }
        for i in 0..out.nodes.len() {
            for j in i + 1..out.nodes.len() {
                if out.nodes[i].id == out.nodes[j].id {
                    return Err(bad_described(
                        section.path(),
                        &format!("node[{i}] and node[{j}] are the same id."),
                        "object",
                    ));
                }
            }
        }

        if let Some(path) = section.string("firmware_path")? {
            if utf16_len(path) >= 261 {
                return Err(section.bad("firmware_path", "too large length.", &Json::String(path.to_owned())));
            }
            out.firmware_path = path.replace('/', "\\");
        }
        out.append_sub_number_to_device_error = section.bool("append_sub_number_to_device_error")?.unwrap_or(false);
        Ok(out)
    }
}

/// `timeout` / `switch_sampling`: `0..=255` or exactly `-1` (unset).
fn byte_or_unset(v: f64) -> Result<Option<u8>, &'static str> {
    if v == -1.0 {
        Ok(None)
    } else if (0.0..=255.0).contains(&v) {
        Ok(Some(v as u8))
    } else {
        Err("out of range.")
    }
}

impl UsbIoNode {
    fn parse(node: &Obj<'_>) -> Res<Self> {
        let id =
            node.require_number_with(
                "id",
                |v| {
                    if (0.0..=15.0).contains(&v) { Ok(v as u8) } else { Err("out of range.") }
                },
            )?;
        let timeout = node.number_with("timeout", byte_or_unset)?.flatten();
        let switch_sampling = node.number_with("switch_sampling", byte_or_unset)?.flatten();
        let auto_output_zero = node.bool("auto_output_zero")?.unwrap_or(true);
        let share_coin_slot = node.bool("share_coin_slot")?.unwrap_or(false);
        let required_function = UsbIoFunctions::parse(&node.require_object("required_function")?)?;
        let pwm_cycle = if required_function.pwm_output.is_some() {
            node.require_number_with("pwm_cycle", |v| {
                if (0.0..=255.0).contains(&v) { Ok(v as u8) } else { Err("out of range.") }
            })?
        } else {
            1
        };
        Ok(Self { id, timeout, switch_sampling, pwm_cycle, auto_output_zero, share_coin_slot, required_function })
    }
}

impl UsbIoFunctions {
    fn parse(required: &Obj<'_>) -> Res<Self> {
        let mut out = Self {
            switch_input: required.function("switch_input", |o| {
                let (player, button) = o.switch_pair(2.0)?;
                Ok(UsbIoSwitch { player, button })
            })?,
            coin_input: required.function("coin_input", |o| o.require_number_with("slot", range_u32(1.0, 2.0)))?,
            analog_input: required.function("analog_input", |o| {
                let channel = o.require_number_with("channel", range_u32(1.0, 8.0))?;
                let bit = o.number_with("bit", range_u32(0.0, 16.0))?.unwrap_or(0);
                Ok(UsbIoAnalog { channel, bit })
            })?,
            rotary_input: required
                .function("rotary_input", |o| o.require_number_with("channel", range_u32(1.0, 4.0)))?,
            general_output: required
                .function("general_output", |o| o.require_number_with("slot", range_u32(1.0, 64.0)))?,
            pwm_output: required.function("pwm_output", |o| o.require_number_with("slot", range_u32(1.0, 8.0)))?,
            unique_io: Vec::new(),
        };
        if let Some(list) = required.array("unique_io")? {
            if list.len() > 9 {
                return Err(list.bad("too many definitions."));
            }
            for i in 0..list.len() {
                let entry = UniqueIo::parse(&list, i)?;
                if let Some(j) = out.unique_io.iter().position(|earlier| earlier.command == entry.command) {
                    return Err(list.bad(&format!("\"unique_io[{j}].command\" is same value ({}).", entry.command)));
                }
                out.unique_io.push(entry);
            }
        }

        let all_disabled = out.switch_input.is_none()
            && out.coin_input.is_none()
            && out.analog_input.is_none()
            && out.rotary_input.is_none()
            && out.general_output.is_none()
            && out.pwm_output.is_none()
            && out.unique_io.is_empty();
        if all_disabled {
            return Err(bad_described(required.path(), "All of the functions are disabled.", "object"));
        }
        Ok(out)
    }
}

impl UniqueIo {
    fn parse(list: &Arr<'_>, index: usize) -> Res<Self> {
        let entry = list.object(index)?;
        let command = entry.require_number_with("command", |v| {
            if (65.0..=79.0).contains(&v) { Ok(v as u8) } else { Err("out of range.") }
        })?;
        let mut params = [0; 3];
        if let Some(values) = entry.array("param")? {
            if values.len() > 3 {
                return Err(values.bad("too many params."));
            }
            for (i, slot) in params.iter_mut().enumerate().take(values.len()) {
                let v = values.number(i)?;
                if !(0.0..=255.0).contains(&v) {
                    return Err(values.bad_at(i, "out of range."));
                }
                *slot = v as u8;
            }
        }
        let input = entry.bool("input")?.unwrap_or(false);
        let output = entry.bool("output")?.unwrap_or(false);
        if !input && !output {
            return Err(list.bad("Both \"input\" and \"output\" are false."));
        }
        Ok(Self { command, params, input, output })
    }
}
