//! The small sections: `common`, `error`, `sequence`, `video`, `backup`, `apm`, `allnet_air`
//! (`spec/libam/config.md` Part D).

use libam_api::sequence::{HistogramRule, PlayAccountingTiming, SequenceConfig};

use super::json::{Json, Map};
use super::reader::{Obj, Res, bad_described, utf16_len};

/// Section `common` (`Config+0x0`, 0xA8 bytes).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Common {
    /// `common.max_player`, `0..=8`.
    pub max_player: u32,
    /// `common.language` (at most 14 UTF-16 units). Empty after parsing when absent; the loader's prelude fills it
    /// from the region (`japanese`, `chinese`, `english`).
    pub language: String,
    /// `common.custom_resource_id`: up to four ids of at most 15 UTF-16 units each. Empty strings are kept in
    /// place; the converter skips them.
    pub custom_resource_ids: Vec<String>,
    /// `common.log.output_request`: log every incoming server request.
    pub log_output_request: bool,
}

impl Common {
    pub(super) fn parse(root: &Map) -> Res<Self> {
        let mut out = Self::default();
        let Some(section) = Obj::section(root, "common")? else { return Ok(out) };

        if let Some(v) = section.number_in("max_player", 0.0, 8.0, "out of range")? {
            out.max_player = v as u32;
        }
        if let Some(language) = section.string("language")? {
            if utf16_len(language) >= 15 {
                return Err(section.bad("language", "too large length", &Json::String(language.to_owned())));
            }
            out.language = language.to_owned();
        }
        if let Some(ids) = section.array("custom_resource_id")? {
            if ids.len() > 4 {
                return Err(ids.bad("too many custom resource ids"));
            }
            for i in 0..ids.len() {
                let id = ids.string(i)?;
                if utf16_len(id) >= 16 {
                    return Err(ids.bad("too large length"));
                }
                out.custom_resource_ids.push(id.to_owned());
            }
        }
        if let Some(log) = section.object("log")? {
            out.log_output_request = log.bool("output_request")?.unwrap_or(false);
        }
        Ok(out)
    }
}

/// Section `error` (`Config+0x4ED5C`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ErrorConfig {
    /// `error.auto_reset`.
    pub auto_reset: bool,
    /// `error.raise_in_test`.
    pub raise_in_test: bool,
}

impl ErrorConfig {
    pub(super) fn parse(root: &Map) -> Res<Self> {
        let mut out = Self::default();
        let Some(section) = Obj::section(root, "error")? else { return Ok(out) };
        out.auto_reset = section.bool("auto_reset")?.unwrap_or(false);
        out.raise_in_test = section.bool("raise_in_test")?.unwrap_or(false);
        Ok(out)
    }
}

/// Largest number of `sequence.histogram_rule` entries, and largest sum of their `count`.
const HISTOGRAM_LIMIT: usize = 31;

/// Parses section `sequence` (`Config+0x4ED60`) into the shared API type.
pub(super) fn parse_sequence(root: &Map) -> Res<SequenceConfig> {
    let mut out = SequenceConfig { save_total_time_on_playing: true, ..SequenceConfig::default() };
    let Some(section) = Obj::section(root, "sequence")? else { return Ok(out) };

    if let Some(timing) = section.string("play_accounting_timing")? {
        out.play_accounting_timing = match timing {
            "normal" => PlayAccountingTiming::Normal,
            "begin" => PlayAccountingTiming::Begin,
            "end" => PlayAccountingTiming::End,
            other => {
                return Err(section.bad("play_accounting_timing", "invalid value", &Json::String(other.to_owned())));
            }
        };
    }
    if let Some(save) = section.bool("save_total_time_on_playing")? {
        out.save_total_time_on_playing = save;
    }
    if let Some(rules) = section.array("histogram_rule")? {
        if rules.len() > HISTOGRAM_LIMIT {
            return Err(rules.bad("too many histogram rules"));
        }
        for i in 0..rules.len() {
            let rule = rules.object(i)?;
            let width = rule.require_number_with("width", |v| {
                if (1.0..=86400.0).contains(&v) { Ok(v as u32) } else { Err("out of range") }
            })?;
            let count = rule.require_number_with("count", |v| {
                if (1.0..=31.0).contains(&v) { Ok(v as u32) } else { Err("out of range") }
            })?;
            out.histogram_rules.push(HistogramRule { width, count });
        }
        let total: u32 = out.histogram_rules.iter().map(|r| r.count).sum();
        if total as usize > HISTOGRAM_LIMIT {
            return Err(bad_described(section.path(), "total of \"histogram_rule[].count\" is too many", "object"));
        }
    }
    Ok(out)
}

/// `video.mode`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum VideoMode {
    #[default]
    Clone,
    Single,
    Dual,
}

/// One `{width, height, angle}` resolution of the `video` section.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Resolution {
    pub width: u32,
    pub height: u32,
    /// 0, 90, 180 or 270.
    pub angle: u16,
}

impl Resolution {
    fn parse(object: &Obj<'_>) -> Res<Self> {
        let size = |v: f64| {
            if (0.0..=4_294_967_295.0).contains(&v) { Ok(v as u32) } else { Err("out of range") }
        };
        let mut out = Self::default();
        if let Some(v) = object.number_with("width", size)? {
            out.width = v;
        }
        if let Some(v) = object.number_with("height", size)? {
            out.height = v;
        }
        if let Some(v) = object.number_with("angle", |v| {
            if (0.0..=270.0).contains(&v) && matches!(v as u16, 0 | 90 | 180 | 270) {
                Ok(v as u16)
            } else {
                Err("invalid angle")
            }
        })? {
            out.angle = v;
        }
        Ok(out)
    }
}

/// Section `video` (`Config+0x4EE64`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Video {
    /// `video.enable`: `true` when the section object exists without the key.
    pub enable: bool,
    pub mode: VideoMode,
    /// Primary and secondary resolution (the `monitor` array fills both slots in order).
    pub resolutions: [Resolution; 2],
    pub ignore_unconnected: bool,
}

impl Video {
    pub(super) fn parse(root: &Map) -> Res<Self> {
        let mut out = Self::default();
        let Some(section) = Obj::section(root, "video")? else { return Ok(out) };
        out.enable = section.enable()?;
        if !out.enable {
            return Ok(out);
        }
        if let Some(mode) = section.string("mode")? {
            out.mode = match mode {
                "clone" => VideoMode::Clone,
                "single" => VideoMode::Single,
                "dual" => VideoMode::Dual,
                other => return Err(section.bad("mode", "invalid mode", &Json::String(other.to_owned()))),
            };
        }
        let mut monitor_given = false;
        if let Some(monitors) = section.array("monitor")? {
            if monitors.len() > 2 {
                return Err(monitors.bad("too many monitors"));
            }
            monitor_given = true;
            out.resolutions = [Resolution::default(); 2];
            for i in 0..monitors.len() {
                out.resolutions[i] = Resolution::parse(&monitors.object(i)?)?;
            }
        }
        for (slot, key) in ["resolution", "secondary_resolution"].into_iter().enumerate() {
            if let Some(object) = section.object(key)? {
                if monitor_given {
                    return Err(section.bad(key, "Cannot be set with \"video.monitor\"", &Json::Object(Map::new())));
                }
                out.resolutions[slot] = Resolution::parse(&object)?;
            }
        }
        // The secondary size falls back to the primary one when either side is 0; its angle does not.
        let [primary, secondary] = &mut out.resolutions;
        if secondary.width == 0 || secondary.height == 0 {
            secondary.width = primary.width;
            secondary.height = primary.height;
        }
        out.ignore_unconnected = section.bool("ignore_unconnected")?.unwrap_or(false);
        Ok(out)
    }
}

/// Section `backup` (`Config+0x4F354`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Backup {
    pub clear_eeprom_at_first_launch: bool,
    pub clear_busram_at_first_launch: bool,
}

impl Backup {
    pub(super) fn parse(root: &Map) -> Res<Self> {
        let mut out = Self::default();
        let Some(section) = Obj::section(root, "backup")? else { return Ok(out) };
        out.clear_eeprom_at_first_launch = section.bool("clear_eeprom_at_first_launch")?.unwrap_or(false);
        out.clear_busram_at_first_launch = section.bool("clear_busram_at_first_launch")?.unwrap_or(false);
        Ok(out)
    }
}

/// The sections that consist of one `enable` flag that is `true` when the section object exists (`apm`,
/// `allnet_air`); `false` when the section is absent.
pub(super) fn parse_enable_only(root: &Map, section: &str) -> Res<bool> {
    match Obj::section(root, section)? {
        Some(object) => object.enable(),
        None => Ok(false),
    }
}
