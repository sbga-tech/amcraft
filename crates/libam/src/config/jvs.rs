//! Section `jvs` (`Config+0x4F358`, `spec/libam/config.md` Part F §1).

use super::json::Map;
use super::reader::{Arr, Obj, Res, bad_described, range_u32};

/// `required_function.switch_input`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JvsSwitch {
    /// `0..=4`.
    pub player: u32,
    /// `0..=64`; non-zero only with a non-zero `player`.
    pub button: u32,
}

/// `required_function.analog_input`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JvsAnalog {
    /// `1..=16`.
    pub channel: u32,
    /// `0..=16`.
    pub bit: u32,
}

/// The functions a node must provide; a disabled one is `None`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JvsFunctions {
    pub switch_input: Option<JvsSwitch>,
    /// `coin_input.slot`, `1..=2`.
    pub coin_input: Option<u32>,
    pub analog_input: Option<JvsAnalog>,
    /// `rotary_input.channel`, `1..=16`.
    pub rotary_input: Option<u32>,
    /// `general_driver.slot`, `1..=64`.
    pub general_driver: Option<u32>,
}

/// One parsed `jvs.node[][]` object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JvsNode {
    /// Defaults to `true`.
    pub auto_output_zero: bool,
    pub share_coin_slot: bool,
    pub required_function: JvsFunctions,
}

/// Section `jvs`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Jvs {
    pub enable: bool,
    /// `lists[k]` is the node configuration for a cabinet with `k + 1` JVS nodes (`jvs.node[i]` is stored by its
    /// length, not by `i`; a later group of the same length replaces the earlier one). An empty list is unused.
    pub lists: [Vec<JvsNode>; 4],
}

impl Jvs {
    pub(super) fn parse(root: &Map) -> Res<Self> {
        let mut out = Self::default();
        let Some(section) = Obj::section(root, "jvs")? else { return Ok(out) };
        out.enable = section.enable()?;
        if !out.enable {
            return Ok(out);
        }

        let groups = section.require_array("node")?;
        if groups.len() > 4 {
            return Err(groups.bad("too many node configs."));
        }
        for i in 0..groups.len() {
            let group = groups.array(i)?;
            if group.len() == 0 {
                return Err(group.bad("node array is empty."));
            }
            if group.len() > 4 {
                return Err(group.bad("too many nodes."));
            }
            let mut nodes = Vec::with_capacity(group.len());
            for j in 0..group.len() {
                nodes.push(JvsNode::parse(&group, j)?);
            }
            out.lists[group.len() - 1] = nodes;
        }
        Ok(out)
    }
}

impl JvsNode {
    fn parse(group: &Arr<'_>, index: usize) -> Res<Self> {
        let node = group.object(index)?;
        let auto_output_zero = node.bool("auto_output_zero")?.unwrap_or(true);
        let share_coin_slot = node.bool("share_coin_slot")?.unwrap_or(false);
        let required = node.require_object("required_function")?;
        let required_function = JvsFunctions::parse(&required)?;
        Ok(Self { auto_output_zero, share_coin_slot, required_function })
    }
}

impl JvsFunctions {
    fn parse(required: &Obj<'_>) -> Res<Self> {
        let switch_input = required.function("switch_input", |o| {
            let (player, button) = o.switch_pair(4.0)?;
            Ok(JvsSwitch { player, button })
        })?;
        let coin_input = required.function("coin_input", |o| o.require_number_with("slot", range_u32(1.0, 2.0)))?;
        let analog_input = required.function("analog_input", |o| {
            let channel = o.require_number_with("channel", range_u32(1.0, 16.0))?;
            let bit = o.number_with("bit", range_u32(0.0, 16.0))?.unwrap_or(0);
            Ok(JvsAnalog { channel, bit })
        })?;
        let rotary_input =
            required.function("rotary_input", |o| o.require_number_with("channel", range_u32(1.0, 16.0)))?;
        let general_driver =
            required.function("general_driver", |o| o.require_number_with("slot", range_u32(1.0, 64.0)))?;

        let out = Self { switch_input, coin_input, analog_input, rotary_input, general_driver };
        if out == Self::default() {
            return Err(bad_described(required.path(), "All of the functions are disabled.", "object"));
        }
        Ok(out)
    }
}
