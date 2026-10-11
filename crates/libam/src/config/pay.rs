//! Sections `aime`, `aimepay`, `emoney` and `abaas_log` (`Config+0x4FFA0`, `+0x50B14`, `+0x507E0`, `+0x50790`;
//! `spec/libam/config.md` Part J §0-§3, §7).
//!
//! All four are enabled when their object exists without `enable`; `enable: false` stops the reading of every
//! other key. Path strings are stored as the validators leave them: `/` turned into `\`, `resource_path` joined to
//! the executable directory when it is relative. Lengths are counted in UTF-16 units, the unit of the original's
//! fixed `wchar_t` buffers.

use super::json::{Json, Map};
use super::reader::{Arr, Obj, Res, bad_value, utf16_len};

const TOO_LARGE: &str = "too large length.";

/// `/` replaced by `\`, the original's path normaliser.
fn backslashes(text: &str) -> String {
    text.replace('/', "\\")
}

/// `X:...` with an ASCII drive letter, or a leading separator. `path` has its slashes normalised already.
fn is_absolute(path: &str) -> bool {
    let b = path.as_bytes();
    (b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic()) || b.first().is_some_and(|&c| c == b'\\' || c == b'/')
}

/// A path key that must be relative: `/` to `\`, then `too large length.` at `limit` units, then
/// `path is absolute.`. The error value is the original text.
fn relative_path(object: &Obj<'_>, key: &str, limit: usize) -> Res<Option<String>> {
    let Some(text) = object.string(key)? else { return Ok(None) };
    let path = backslashes(text);
    if utf16_len(&path) >= limit {
        return Err(object.bad(key, TOO_LARGE, &Json::String(text.to_owned())));
    }
    if is_absolute(&path) {
        return Err(object.bad(key, "path is absolute.", &Json::String(text.to_owned())));
    }
    Ok(Some(path))
}

/// `resource_path` of `aimepay` and `emoney`: a non-empty relative value is prefixed with `exe_dir`.
fn resource_path(object: &Obj<'_>, exe_dir: &str) -> Res<Option<String>> {
    let Some(text) = object.string("resource_path")? else { return Ok(None) };
    if text.is_empty() {
        return Ok(Some(String::new()));
    }
    let mut path = backslashes(text);
    if !is_absolute(&path) {
        path.insert_str(0, exe_dir);
    }
    if utf16_len(&path) >= 201 {
        return Err(object.bad("resource_path", TOO_LARGE, &Json::String(text.to_owned())));
    }
    Ok(Some(path))
}

/// `display_port`: `0..=256`.
fn display_port(object: &Obj<'_>) -> Res<u32> {
    let check = |v: f64| if (0.0..=256.0).contains(&v) { Ok(v as u32) } else { Err("out of range") };
    Ok(object.number_with("display_port", check)?.unwrap_or(0))
}

/// A string element of an array, with its own path in a failed check.
fn element_error(list: &Arr<'_>, index: usize, thrown: &str, text: &str) -> super::reader::ConfigError {
    bad_value(&list.element_path(index), thrown, &Json::String(text.to_owned()))
}

/// One `aime.unit[]` entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AimeUnit {
    /// COM port, `1..=256`.
    pub port: u32,
    /// `1..=6`.
    pub id: u32,
    pub required: bool,
}

/// Section `aime`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Aime {
    pub enable: bool,
    /// `1..=4` entries while enabled.
    pub units: Vec<AimeUnit>,
    /// At most three entries; slashes normalised, no `exe_dir` prefix.
    pub firmware_path: Vec<String>,
    pub high_baudrate: bool,
    /// `1..=2147483647`; defaults to `100`.
    pub polling_interval: u32,
    /// `log.enable`: `true` when the `log` object exists without the key.
    pub log_enable: bool,
    /// `log.root_path`: relative, at most 200 units.
    pub log_root_path: String,
}

impl Default for Aime {
    fn default() -> Self {
        Self {
            enable: false,
            units: Vec::new(),
            firmware_path: Vec::new(),
            high_baudrate: false,
            polling_interval: 100,
            log_enable: false,
            log_root_path: String::new(),
        }
    }
}

impl Aime {
    pub(super) fn parse(root: &Map) -> Res<Self> {
        let mut out = Self::default();
        let Some(section) = Obj::section(root, "aime")? else { return Ok(out) };
        out.enable = section.enable()?;
        if !out.enable {
            return Ok(out);
        }

        let units = section.require_array("unit")?;
        if units.len() == 0 {
            return Err(units.bad("array is empty."));
        }
        if units.len() > 4 {
            return Err(units.bad("too many units."));
        }
        for i in 0..units.len() {
            let unit = units.object(i)?;
            let port = unit.require_number_with("port", |v| {
                if (1.0..=256.0).contains(&v) { Ok(v as u32) } else { Err("out of range") }
            })?;
            let id = unit.require_number_with("id", |v| {
                if (1.0..=6.0).contains(&v) { Ok(v as u32) } else { Err("out of range") }
            })?;
            let required = unit.bool("required")?.unwrap_or(false);
            out.units.push(AimeUnit { port, id, required });
        }

        if let Some(paths) = section.array("firmware_path")? {
            if paths.len() > 3 {
                return Err(paths.bad("too many firmware paths."));
            }
            for i in 0..paths.len() {
                let text = paths.string(i)?;
                let path = backslashes(text);
                if utf16_len(&path) >= 261 {
                    return Err(element_error(&paths, i, TOO_LARGE, text));
                }
                out.firmware_path.push(path);
            }
        }
        out.high_baudrate = section.bool("high_baudrate")?.unwrap_or(false);
        if let Some(v) = section.number_with("polling_interval", |v| {
            if (1.0..=2_147_483_647.0).contains(&v) { Ok(v as u32) } else { Err("out of range") }
        })? {
            out.polling_interval = v;
        }
        if let Some(log) = section.object("log")? {
            out.log_enable = log.bool("enable")?.unwrap_or(true);
            if let Some(path) = relative_path(&log, "root_path", 201)? {
                out.log_root_path = path;
            }
        }
        Ok(out)
    }
}

/// Section `aimepay`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AimePay {
    pub enable: bool,
    /// Relative values carry the executable directory in front.
    pub resource_path: String,
    /// `0..=256`; `0` means "take the `emoney` value".
    pub display_port: u32,
    /// At most two ids of at most 13 UTF-16 units each.
    pub coin_item_id: Vec<String>,
}

impl AimePay {
    pub(super) fn parse(root: &Map, exe_dir: &str) -> Res<Self> {
        let mut out = Self::default();
        let Some(section) = Obj::section(root, "aimepay")? else { return Ok(out) };
        out.enable = section.enable()?;
        if !out.enable {
            return Ok(out);
        }
        if let Some(path) = resource_path(&section, exe_dir)? {
            out.resource_path = path;
        }
        out.display_port = display_port(&section)?;
        if let Some(ids) = section.array("coin_item_id")? {
            if ids.len() > 2 {
                return Err(ids.bad("too many item IDs for coin."));
            }
            for i in 0..ids.len() {
                let text = ids.string(i)?;
                if utf16_len(text) >= 14 {
                    return Err(element_error(&ids, i, TOO_LARGE, text));
                }
                out.coin_item_id.push(text.to_owned());
            }
        }
        Ok(out)
    }
}

/// The brands `emoney.ignore_brand` can name, in flag order.
pub const EMONEY_BRANDS: [&str; 7] = ["Nanaco", "Edy", "Transport", "Waon", "Paseli", "iD", "Sapica"];

/// Section `emoney`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EMoney {
    pub enable: bool,
    /// Relative values carry the executable directory in front.
    pub resource_path: String,
    /// `0..=256`; `0` means "take the `aimepay` value".
    pub display_port: u32,
    /// Index into `aime.unit[]`, `0..=3`.
    pub aime_unit: u8,
    /// `log.level`, `0..=4`.
    pub log_level: u8,
    /// `log.root_path`: relative, at most 200 units.
    pub log_root_path: String,
    /// `ignore_brand`: flag per entry of [`EMONEY_BRANDS`].
    pub ignore_brand: [bool; 7],
}

impl EMoney {
    pub(super) fn parse(root: &Map, exe_dir: &str) -> Res<Self> {
        let mut out = Self::default();
        let Some(section) = Obj::section(root, "emoney")? else { return Ok(out) };
        out.enable = section.enable()?;
        if !out.enable {
            return Ok(out);
        }
        if let Some(path) = resource_path(&section, exe_dir)? {
            out.resource_path = path;
        }
        out.display_port = display_port(&section)?;
        if let Some(v) = section
            .number_with("aime_unit", |v| if (0.0..4.0).contains(&v) { Ok(v as u8) } else { Err("out of range.") })?
        {
            out.aime_unit = v;
        }
        if let Some(log) = section.object("log")? {
            if let Some(v) =
                log.number_with("level", |v| if (0.0..=4.0).contains(&v) { Ok(v as u8) } else { Err("out of range.") })?
            {
                out.log_level = v;
            }
            if let Some(path) = relative_path(&log, "root_path", 201)? {
                out.log_root_path = path;
            }
        }
        if let Some(brands) = section.array("ignore_brand")? {
            for i in 0..brands.len() {
                let text = brands.string(i)?;
                let Some(n) = EMONEY_BRANDS.iter().position(|b| b.eq_ignore_ascii_case(text)) else {
                    return Err(element_error(&brands, i, "invalid brand ID.", text));
                };
                out.ignore_brand[n] = true;
            }
        }
        Ok(out)
    }
}

/// Section `abaas_log`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbaasLog {
    pub enable: bool,
    /// Relative, at most 36 units; resolved against the application root by the manager.
    pub root_path: String,
    /// Defaults to `true`, also for an absent section.
    pub use_network: bool,
    /// `log.level`, `0..=4`.
    pub log_level: u8,
}

impl Default for AbaasLog {
    fn default() -> Self {
        Self { enable: false, root_path: String::new(), use_network: true, log_level: 0 }
    }
}

impl AbaasLog {
    pub(super) fn parse(root: &Map) -> Res<Self> {
        let mut out = Self::default();
        let Some(section) = Obj::section(root, "abaas_log")? else { return Ok(out) };
        out.enable = section.enable()?;
        if !out.enable {
            return Ok(out);
        }
        if let Some(path) = relative_path(&section, "root_path", 37)? {
            out.root_path = path;
        }
        out.use_network = section.bool("use_network")?.unwrap_or(true);
        if let Some(log) = section.object("log")?
            && let Some(v) =
                log.number_with("level", |v| if (0.0..=4.0).contains(&v) { Ok(v as u8) } else { Err("out of range.") })?
        {
            out.log_level = v;
        }
        Ok(out)
    }
}
