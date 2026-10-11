//! Sections `input` and `output` (`Config+0xA8`, `Config+0x26082`; `spec/libam/config.md` Part G).
//!
//! Both are one template: `{"system": [relation...], "players": [[relation...]...]}`. The result is the shared
//! [`RelationMap`] of [`crate::jvs`]: block 0 holds every relation that is not tied to a JVS node count, block
//! `1..=4` the JVS relations of the profile for that node count.

use crate::jvs::{
    CONFIG_BLOCKS, InputDevice, InputRelation, OutputDevice, OutputRelation, PLAYER_SETS, RelationBlock, RelationMap,
    RelationSet,
};

use super::json::{Json, Map};
use super::reader::{Arr, Obj, Res, bad_described, utf16_len};

/// Longest relation id, in UTF-16 units (the published field is `wchar_t[24]`).
const MAX_ID_UNITS: usize = 23;

/// `0 <= v < limit`, stored as the truncating cast.
fn below(limit: f64) -> impl Fn(f64) -> Result<u32, &'static str> {
    move |v| if (0.0..limit).contains(&v) { Ok(v as u32) } else { Err("out of range") }
}

/// `0 <= v <= limit`.
fn at_most(limit: f64) -> impl Fn(f64) -> Result<u32, &'static str> {
    move |v| if (0.0..=limit).contains(&v) { Ok(v as u32) } else { Err("out of range") }
}

fn parse_id(relation: &Obj<'_>, invalid: &str) -> Res<String> {
    let id = relation.require_string("id")?;
    let units = utf16_len(id);
    if units == 0 || units > MAX_ID_UNITS {
        return Err(relation.bad("id", invalid, &Json::String(id.to_owned())));
    }
    Ok(id.to_owned())
}

/// `<player key>`: `-1` (no player, the system switches) or `0..limit`.
fn player(relation: &Obj<'_>, key: &str, limit: f64) -> Res<Option<u8>> {
    Ok(relation
        .number_with(key, |v| {
            if v == -1.0 {
                Ok(None)
            } else if (0.0..limit).contains(&v) {
                Ok(Some(v as u8))
            } else {
                Err("out of range")
            }
        })?
        .flatten())
}

fn node(relation: &Obj<'_>, key: &str, limit: f64) -> Res<u8> {
    Ok(relation.number_with(key, below(limit))?.unwrap_or(0) as u8)
}

/// One input relation and whether it is a JVS relation (routed to the node-count profiles).
pub(super) fn parse_input(relation: &Obj<'_>) -> Res<(InputRelation, bool)> {
    let id = parse_id(relation, "invalid input ID")?;
    let kind = relation.require_string("type")?;
    let index = |ok: &dyn Fn(f64) -> bool| -> Res<u16> {
        relation.require_number_with("index", |v| if ok(v) { Ok(v as u16) } else { Err("out of range") })
    };
    let (device, jvs) = match kind {
        "pushsw" => (InputDevice::PushSwitch { index: index(&|v| (0.0..=1.0).contains(&v))? }, false),
        "jvs_switch" => {
            let node = node(relation, "jvs_node", 4.0)?;
            let player = player(relation, "jvs_player", 4.0)?;
            let limit = if player.is_some() { 64.0 } else { 8.0 };
            (InputDevice::JvsSwitch { index: index(&|v| (0.0..limit).contains(&v))?, node, player }, true)
        }
        "jvs_analog" => {
            let node = node(relation, "jvs_node", 4.0)?;
            (InputDevice::JvsAnalog { index: index(&|v| (0.0..16.0).contains(&v))?, node }, true)
        }
        "jvs_rotary" => {
            let node = node(relation, "jvs_node", 4.0)?;
            (InputDevice::JvsRotary { index: index(&|v| (0.0..16.0).contains(&v))?, node }, true)
        }
        "can_switch" | "can_analog" | "can_rotary" => {
            let port = node(relation, "can_port", 2.0)?;
            let target = node(relation, "can_target", 16.0)?;
            let device = match kind {
                "can_switch" => InputDevice::CanSwitch { index: index(&|v| (0.0..32.0).contains(&v))?, port, target },
                "can_analog" => InputDevice::CanAnalog { index: index(&|v| (0.0..8.0).contains(&v))?, port, target },
                _ => InputDevice::CanRotary { index: index(&|v| (0.0..4.0).contains(&v))?, port, target },
            };
            (device, false)
        }
        "usb_switch" => {
            let node = node(relation, "usb_node", 4.0)?;
            let player = player(relation, "usb_player", 2.0)?;
            let limit = if player.is_some() { 64.0 } else { 2.0 };
            (InputDevice::UsbSwitch { index: index(&|v| (0.0..limit).contains(&v))?, node, player }, false)
        }
        "usb_analog" => {
            let node = node(relation, "usb_node", 4.0)?;
            (InputDevice::UsbAnalog { index: index(&|v| (0.0..8.0).contains(&v))?, node }, false)
        }
        "usb_rotary" => {
            let node = node(relation, "usb_node", 4.0)?;
            (InputDevice::UsbRotary { index: index(&|v| (0.0..4.0).contains(&v))?, node }, false)
        }
        other => {
            return Err(relation.bad("type", "invalid input device type", &Json::String(other.to_owned())));
        }
    };
    Ok((InputRelation { id, device }, jvs))
}

/// One output relation and whether it is a JVS relation.
pub(super) fn parse_output(relation: &Obj<'_>) -> Res<(OutputRelation, bool)> {
    let id = parse_id(relation, "invalid output ID")?;
    let kind = relation.require_string("type")?;
    let optional = |key: &str, check: &dyn Fn(f64) -> bool| -> Res<u16> {
        Ok(relation.number_with(key, |v| if check(v) { Ok(v as u16) } else { Err("out of range") })?.unwrap_or(0))
    };
    // `jvs_general` and `usb_general`: 64 bits; `count` may reach the end of the word.
    let general = |node_key: &str| -> Res<(u16, u16, u8)> {
        let node = node(relation, node_key, 4.0)?;
        let index = optional("index", &|v| (0.0..64.0).contains(&v))?;
        let count = optional("count", &|v| (0.0..=f64::from(64 - index)).contains(&v))?;
        Ok((index, count, node))
    };
    let (device, jvs) = match kind {
        "led" => (OutputDevice::Led { index: optional("index", &|v| (0.0..=1.0).contains(&v))? }, false),
        "jvs_general" => {
            let (index, count, node) = general("jvs_node")?;
            (OutputDevice::JvsGeneral { index, count, node }, true)
        }
        "can_request" => {
            let port = node(relation, "can_port", 2.0)?;
            let target = node(relation, "can_target", 16.0)?;
            let command = relation.require_number_with("can_command", at_most(255.0))? as u8;
            let data_size = relation.require_number_with("can_data_size", at_most(8.0))? as u8;
            let bits = f64::from(u32::from(data_size) * 8);
            let index = optional("index", &|v| (0.0..=bits).contains(&v))?;
            let count = optional("count", &|v| (0.0..=bits - f64::from(index)).contains(&v))?;
            (OutputDevice::CanRequest { index, count, port, target, command, data_size }, false)
        }
        "usb_general" => {
            let (index, count, node) = general("usb_node")?;
            (OutputDevice::UsbGeneral { index, count, node }, false)
        }
        "usb_pwm" => {
            let node = node(relation, "usb_node", 4.0)?;
            (OutputDevice::UsbPwm { index: optional("index", &|v| (0.0..8.0).contains(&v))?, node }, false)
        }
        other => {
            return Err(relation.bad("type", "invalid output device type", &Json::String(other.to_owned())));
        }
    };
    Ok((OutputRelation { id, device }, jvs))
}

/// The scratch blocks a section is parsed into: the common block and one block per JVS node count.
struct Scratch<R> {
    common: RelationBlock<R>,
    profiles: [RelationBlock<R>; 4],
}

impl<R: Clone> Scratch<R> {
    /// Routes a parsed relation into set `set` (`0` = `system`, `i + 1` = `players[i]`): non-JVS relations go to
    /// the common block; a JVS relation goes to every profile (`tag == 0`) or to profile `tag` only (`1..=4`),
    /// and is dropped for a larger tag.
    fn route(&mut self, set: usize, relation: R, jvs: bool, tag: u64) -> Result<(), ()> {
        fn set_of<R>(block: &mut RelationBlock<R>, set: usize) -> &mut RelationSet<R> {
            if set == 0 { &mut block.system } else { &mut block.players[set - 1] }
        }
        if !jvs {
            return set_of(&mut self.common, set).push(relation).map_err(|_| ());
        }
        for (b, profile) in self.profiles.iter_mut().enumerate() {
            if tag == 0 || tag == b as u64 + 1 {
                set_of(profile, set).push(relation.clone()).map_err(|_| ())?;
            }
        }
        Ok(())
    }

    fn parse_element(
        &mut self,
        element: &Obj<'_>,
        set: usize,
        tag_key: &str,
        parse: &impl Fn(&Obj<'_>) -> Res<(R, bool)>,
    ) -> Res<Result<(), ()>> {
        let (relation, jvs) = parse(element)?;
        let tag = if jvs { element.require_number_with(tag_key, |v| Ok::<_, &str>(v as u64))? } else { 0 };
        Ok(self.route(set, relation, jvs, tag))
    }
}

/// Parses the section `name` of the merged tree. `max_player` is `common.max_player`; `tag_key` is the synthetic
/// key the merge put into every appended element.
pub(super) fn parse_map<R: Clone>(
    root: &Map,
    name: &str,
    max_player: u32,
    tag_key: &str,
    parse: impl Fn(&Obj<'_>) -> Res<(R, bool)>,
) -> Res<RelationMap<R>> {
    let Some(section) = Obj::section(root, name)? else { return Ok(RelationMap::default()) };
    let mut scratch =
        Scratch::<R> { common: RelationBlock::default(), profiles: std::array::from_fn(|_| RelationBlock::default()) };

    if let Some(system) = section.array("system")? {
        for i in 0..system.len() {
            let element = system.object(i)?;
            scratch.parse_element(&element, 0, tag_key, &parse)?.map_err(|()| system.bad("too many relation"))?;
        }
    }
    if let Some(players) = section.array("players")? {
        if players.len() > max_player as usize {
            return Err(players.bad("too many players."));
        }
        for p in 0..players.len() {
            let group: Arr<'_> = players.array(p)?;
            for i in 0..group.len() {
                let element = group.object(i)?;
                scratch
                    .parse_element(&element, p + 1, tag_key, &parse)?
                    .map_err(|()| players.bad("too many relation"))?;
            }
        }
    }

    // A profile is merged behind the common block at run time, so every set must fit together.
    for profile in &scratch.profiles {
        let common_sets = std::iter::once(&scratch.common.system).chain(&scratch.common.players);
        let profile_sets = std::iter::once(&profile.system).chain(&profile.players);
        for (common, profile) in common_sets.zip(profile_sets) {
            if common.as_slice().len() + profile.as_slice().len() > crate::jvs::MAX_RELATIONS {
                return Err(bad_described(section.path(), "too many relation", "object"));
            }
        }
    }

    let [p1, p2, p3, p4] = scratch.profiles;
    let blocks: [RelationBlock<R>; CONFIG_BLOCKS] = [scratch.common, p1, p2, p3, p4];
    debug_assert_eq!(blocks[0].players.len(), PLAYER_SETS);
    Ok(RelationMap { blocks })
}
