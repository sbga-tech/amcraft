//! Sections `allnet_auth`, `allnet_accounting` and `allnet_wifi` (`Config+0x4FF64`, `+0x4FF90`, `+0x4FF9C`;
//! `spec/libam/config.md` Part I §1-§3). `allnet_air` is a bare `enable` flag ([`super::basic`]).
//!
//! As in the network sections, a section object without `enable` is enabled and `enable: false` stops the
//! reading of every other key.

use super::json::{Json, Map};
use super::reader::{Obj, Res};

/// `allnet_auth.type`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AuthType {
    /// `"1.0"`.
    #[default]
    V1 = 0,
    /// `"2.0"`.
    V2 = 1,
}

/// `allnet_auth.support_line`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SupportLine {
    pub broadband: bool,
    pub mobile: bool,
    pub xdsl: bool,
}

impl SupportLine {
    /// At least one line type is enabled.
    pub fn any(&self) -> bool {
        self.broadband || self.mobile || self.xdsl
    }
}

/// Section `allnet_auth`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Auth {
    pub enable: bool,
    pub auth_type: AuthType,
    pub support_line: SupportLine,
    pub multi_auth_method: bool,
    /// `develop_version`: empty or `<1-2 digits>.<2 digits>`.
    pub develop_version: String,
    /// `lite_auth_key`: 16 raw bytes (all zero when absent).
    pub lite_auth_key: [u8; 16],
    /// `log.level`, `0..=4`.
    pub log_level: u8,
}

impl Auth {
    pub(super) fn parse(root: &Map) -> Res<Self> {
        let mut out = Self::default();
        let Some(section) = Obj::section(root, "allnet_auth")? else { return Ok(out) };
        out.enable = section.enable()?;
        if !out.enable {
            return Ok(out);
        }

        if let Some(text) = section.string("type")? {
            out.auth_type = match text {
                "1.0" => AuthType::V1,
                "2.0" => AuthType::V2,
                other => return Err(section.bad("type", "invalid auth type", &Json::String(other.to_owned()))),
            };
        }
        if let Some(lines) = section.object("support_line")? {
            out.support_line = SupportLine {
                broadband: lines.bool("broadband")?.unwrap_or(false),
                mobile: lines.bool("mobile")?.unwrap_or(false),
                xdsl: lines.bool("xdsl")?.unwrap_or(false),
            };
        }
        out.multi_auth_method = section.bool("multi_auth_method")?.unwrap_or(false);
        if let Some(text) = section.string("develop_version")? {
            if !text.is_empty() && !is_develop_version(text) {
                return Err(section.bad("develop_version", "invalid version format.", &Json::String(text.to_owned())));
            }
            out.develop_version = text.to_owned();
        }
        if let Some(key) = section.array("lite_auth_key")? {
            if key.len() != 16 {
                return Err(key.bad("unmatch data size."));
            }
            for i in 0..16 {
                let v = key.number(i)?;
                if !(0.0..=255.0).contains(&v) {
                    return Err(key.bad("out of range."));
                }
                out.lite_auth_key[i] = v as u8;
            }
        }
        out.log_level = parse_log_level(&section)?;
        Ok(out)
    }
}

/// `[0-9]{1,2}\.[0-9]{2}`, matched against the whole string.
fn is_develop_version(text: &str) -> bool {
    let Some((major, minor)) = text.split_once('.') else { return false };
    let digits =
        |s: &str, n: std::ops::RangeInclusive<usize>| n.contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit());
    digits(major, 1..=2) && digits(minor, 2..=2)
}

/// `log.level` of an allnet section: `0..=4`, `out of range.` otherwise; `0` without the `log` object.
fn parse_log_level(section: &Obj<'_>) -> Res<u8> {
    let Some(log) = section.object("log")? else { return Ok(0) };
    Ok(log
        .number_with("level", |v| if (0.0..=4.0).contains(&v) { Ok(v as u8) } else { Err("out of range.") })?
        .unwrap_or(0))
}

/// `allnet_accounting.mode`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AccountingMode {
    /// Not set (the section is disabled).
    #[default]
    None = 0,
    /// `"A"`.
    A = 1,
    /// `"B1"`.
    B1 = 2,
    /// `"B2"`.
    B2 = 3,
}

/// Section `allnet_accounting`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Accounting {
    pub enable: bool,
    /// Required while enabled.
    pub mode: AccountingMode,
    pub log_level: u8,
}

impl Accounting {
    pub(super) fn parse(root: &Map) -> Res<Self> {
        let mut out = Self::default();
        let Some(section) = Obj::section(root, "allnet_accounting")? else { return Ok(out) };
        out.enable = section.enable()?;
        if !out.enable {
            return Ok(out);
        }
        let mode = section.require_string("mode")?;
        out.mode = match mode {
            "A" => AccountingMode::A,
            "B1" => AccountingMode::B1,
            "B2" => AccountingMode::B2,
            other => return Err(section.bad("mode", "", &Json::String(other.to_owned()))),
        };
        out.log_level = parse_log_level(&section)?;
        Ok(out)
    }
}

/// Section `allnet_wifi`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Wifi {
    pub enable: bool,
    pub log_level: u8,
}

impl Wifi {
    pub(super) fn parse(root: &Map) -> Res<Self> {
        let mut out = Self::default();
        let Some(section) = Obj::section(root, "allnet_wifi")? else { return Ok(out) };
        out.enable = section.enable()?;
        if out.enable {
            out.log_level = parse_log_level(&section)?;
        }
        Ok(out)
    }
}
