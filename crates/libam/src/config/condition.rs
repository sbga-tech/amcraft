//! The per-file `condition` object: which board a file applies to (`spec/libam/config.md` "Board selector and
//! file selection"). It is not a config section and is never merged.

use super::json::Json;
use super::reader::{ConfigError, Obj, Res, bad_value};

/// The board a config is loaded for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Board {
    pub(super) dip_switch: u16,
    pub(super) model: u8,
    /// `0..=3`.
    pub(super) region: u32,
}

/// A parsed `condition`; all zeros (no object) always matches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Condition {
    model_type: u16,
    dip_value: u16,
    dip_mask: u32,
    region: u32,
    /// `0..=4`; not a filter, the merge uses it.
    pub(super) jvs_node: u32,
}

impl Condition {
    /// Reads the `condition` object of a file's root value.
    pub(super) fn parse(root: &Json) -> Res<Self> {
        let Json::Object(map) = root else {
            // The root getter asserts `is<object>()`; this is the assert text.
            return Err(ConfigError(
                "\"type mismatch! call is<type>() before get<type>()\" && is<object>()".to_owned(),
            ));
        };
        let Some(condition) = Obj::section(map, "condition")? else {
            return Ok(Self::default());
        };
        let mut out = Self::default();
        if let Some(v) = condition
            .number_with("model_type", |v| if (0.0..=98.0).contains(&v) { Ok(v as u16) } else { Err("out of range") })?
        {
            out.model_type = v;
        }
        if let Some(dipsw) = condition.object("dipsw")? {
            let byte = |v: f64| if (0.0..=255.0).contains(&v) { Ok(v as u32) } else { Err("out of range") };
            out.dip_value = dipsw.require_number_with("value", byte)? as u16;
            out.dip_mask = dipsw.require_number_with("mask", byte)?;
        }
        if let Some(region) = condition.string("region")? {
            out.region = match region {
                "" => 0,
                "JPN" => 1,
                "CHN" => 2,
                "EXP" => 3,
                other => {
                    return Err(bad_value(
                        &condition.key_path("region"),
                        "invalid region",
                        &Json::String(other.to_owned()),
                    ));
                }
            };
        }
        if let Some(v) = condition
            .number_with("jvs_node", |v| if (0.0..=4.0).contains(&v) { Ok(v as u32) } else { Err("out of range") })?
        {
            out.jvs_node = v;
        }
        Ok(out)
    }

    /// All four checks ANDed against `board`.
    pub(super) fn matches(&self, board: &Board) -> bool {
        (self.model_type == 0 || u16::from(board.model) == self.model_type)
            && (self.dip_mask == 0
                || (self.dip_mask & u32::from(self.dip_value)) == (self.dip_mask & u32::from(board.dip_switch)))
            && (self.region == 0 || self.region == board.region)
    }
}
