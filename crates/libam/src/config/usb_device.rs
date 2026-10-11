//! Section `usb_device` (`Config+0x4EE88`, `spec/libam/config.md` Part F §3).
//!
//! The `VID:PID` ids are parsed by hand: the original's pattern `([0-9a-fA-F]{1,4}):([0-9a-fA-F]{1,4})` has to
//! match the whole string, which is shorter to state in code than through a regular-expression crate.

use super::json::Map;
use super::reader::{Obj, Res};

/// One `usb_device.required[]` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbDeviceRequirement {
    /// `(vid, pid)` per `id[]` element, `1..=32` of them.
    pub ids: Vec<(u16, u16)>,
    /// `1..=4294967295`.
    pub count: u32,
}

/// Section `usb_device`: there is no `enable` key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsbDevice {
    /// At most nine.
    pub required: Vec<UsbDeviceRequirement>,
}

impl UsbDevice {
    pub(super) fn parse(root: &Map) -> Res<Self> {
        let mut out = Self::default();
        let Some(section) = Obj::section(root, "usb_device")? else { return Ok(out) };
        let Some(list) = section.array("required")? else { return Ok(out) };
        if list.len() > 9 {
            return Err(list.bad("too many required definitions."));
        }
        for i in 0..list.len() {
            let entry = list.object(i)?;
            let ids = entry.require_array("id")?;
            if ids.len() == 0 {
                return Err(ids.bad("id is empty."));
            }
            if ids.len() > 32 {
                return Err(ids.bad("too many ids."));
            }
            let mut parsed = Vec::with_capacity(ids.len());
            for k in 0..ids.len() {
                parsed.push(parse_id(ids.string(k)?).ok_or_else(|| ids.bad("invalid id format."))?);
            }
            let count = entry.require_number_with("count", |v| {
                if (1.0..=4_294_967_295.0).contains(&v) { Ok(v as u32) } else { Err("out of range.") }
            })?;
            out.required.push(UsbDeviceRequirement { ids: parsed, count });
        }
        Ok(out)
    }
}

/// `<1-4 hex digits>:<1-4 hex digits>` as `(vid, pid)`.
fn parse_id(text: &str) -> Option<(u16, u16)> {
    let (vid, pid) = text.split_once(':')?;
    let hex = |s: &str| {
        if (1..=4).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit()) {
            u16::from_str_radix(s, 16).ok()
        } else {
            None
        }
    };
    Some((hex(vid)?, hex(pid)?))
}
