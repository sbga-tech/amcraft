//! Section `can` (`Config+0x4F69C`, `spec/libam/config.md` Part E §2).

use super::json::{Json, Map};
use super::reader::{Obj, Res, bad_described, utf16_len};

/// `can.port[].target[].type`, in the order of the original's name table (`motor` is 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanTargetType {
    Motor = 1,
    Sensor = 2,
    Io = 3,
    Led = 4,
}

/// `auto_reply` of one target.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AutoReply {
    /// `interval` in half seconds (`ceil(seconds * 2)`), `0..=255`.
    pub interval: u8,
    pub switch_input: bool,
    pub analog_input_a: bool,
    pub analog_input_b: bool,
    pub rotary_input: bool,
    /// `coin_input.slot`, `0..=2` (`0`: no coin input).
    pub coin_slot: u8,
    pub coin_share_slot: bool,
}

/// One `can.port[].target[]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanTarget {
    /// At most 8 UTF-16 units.
    pub product: String,
    pub kind: CanTargetType,
    /// `1..=29`.
    pub id: u8,
    pub auto_reply: AutoReply,
}

/// One `can.port[]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanPort {
    /// `1..=32`.
    pub number: u16,
    /// `1..=16` targets.
    pub targets: Vec<CanTarget>,
}

/// Section `can`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Can {
    pub enable: bool,
    /// At most two ports.
    pub ports: Vec<CanPort>,
}

impl Can {
    pub(super) fn parse(root: &Map) -> Res<Self> {
        let mut out = Self::default();
        let Some(section) = Obj::section(root, "can")? else { return Ok(out) };
        out.enable = section.enable()?;
        if !out.enable {
            return Ok(out);
        }

        let ports = section.require_array("port")?;
        if ports.len() > 2 {
            return Err(ports.bad("too many ports."));
        }
        for i in 0..ports.len() {
            out.ports.push(CanPort::parse(&ports.object(i)?, &ports, i)?);
        }
        for i in 0..out.ports.len() {
            for j in i + 1..out.ports.len() {
                if out.ports[i].number == out.ports[j].number {
                    return Err(bad_described(
                        section.path(),
                        &format!("port[{i}] and port[{j}] are the same number."),
                        "object",
                    ));
                }
            }
        }
        Ok(out)
    }
}

impl CanPort {
    fn parse(port: &Obj<'_>, ports: &super::reader::Arr<'_>, index: usize) -> Res<Self> {
        let number = port.require_number_with("number", |v| {
            if (1.0..=32.0).contains(&v) { Ok(v as u16) } else { Err("out of range") }
        })?;
        let list = port.require_array("target")?;
        if list.len() == 0 {
            return Err(list.bad("target array is empty."));
        }
        if list.len() > 16 {
            return Err(list.bad("too many targets."));
        }
        let mut targets = Vec::with_capacity(list.len());
        for j in 0..list.len() {
            targets.push(CanTarget::parse(&list.object(j)?)?);
        }
        for i in 0..targets.len() {
            for j in i + 1..targets.len() {
                if targets[i].kind == targets[j].kind && targets[i].id == targets[j].id {
                    // The port element's index stays on the reported path; the value is the `port` array.
                    return Err(
                        ports.bad_at(index, &format!("target[{i}] and target[{j}] are the same type and the same id."))
                    );
                }
            }
        }
        Ok(Self { number, targets })
    }
}

impl CanTarget {
    fn parse(target: &Obj<'_>) -> Res<Self> {
        let product = match target.string("product")? {
            Some(s) if utf16_len(s) >= 9 => {
                return Err(target.bad("product", "too long product name.", &Json::String(s.to_owned())));
            }
            Some(s) => s.to_owned(),
            None => String::new(),
        };
        let kind = target.require_string("type")?;
        let kind = match kind {
            "motor" => CanTargetType::Motor,
            "sensor" => CanTargetType::Sensor,
            "io" => CanTargetType::Io,
            "led" => CanTargetType::Led,
            other => return Err(target.bad("type", "invalid target type.", &Json::String(other.to_owned()))),
        };
        let id = target.require_number_with("id", |v| {
            if (1.0..=29.0).contains(&v) { Ok(v as u8) } else { Err("invalid target ID.") }
        })?;
        let auto_reply = match target.object("auto_reply")? {
            Some(reply) => AutoReply::parse(&reply)?,
            None => AutoReply::default(),
        };
        Ok(Self { product, kind, id, auto_reply })
    }
}

impl AutoReply {
    fn parse(reply: &Obj<'_>) -> Res<Self> {
        let mut out = Self::default();
        if let Some(v) = reply.number_with("interval", |v| {
            let half_seconds = (v * 2.0).ceil();
            if (0.0..=255.0).contains(&half_seconds) { Ok(half_seconds as u8) } else { Err("out of range.") }
        })? {
            out.interval = v;
        }
        out.switch_input = reply.bool("switch_input")?.unwrap_or(false);
        out.analog_input_a = reply.bool("analog_input_a")?.unwrap_or(false);
        out.analog_input_b = reply.bool("analog_input_b")?.unwrap_or(false);
        out.rotary_input = reply.bool("rotary_input")?.unwrap_or(false);
        if let Some(coin) = reply.object("coin_input")? {
            if let Some(v) =
                coin.number_with("slot", |v| if (0.0..=2.0).contains(&v) { Ok(v as u8) } else { Err("out of range.") })?
            {
                out.coin_slot = v;
            }
            out.coin_share_slot = coin.bool("share_slot")?.unwrap_or(false);
        }
        Ok(out)
    }
}
