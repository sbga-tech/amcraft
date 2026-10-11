//! Sections `network`, `lan_install` and `net_delivery` (`Config+0x4FF34`, `+0x4FF58`, `+0x4FF60`;
//! `spec/libam/config.md` Part H §1-§3).
//!
//! The three share one rule: with `enable: false` no other key is read, not even type-checked. A section object
//! without `enable` is enabled; an absent section is disabled.

use super::json::{Json, Map};
use super::reader::{Obj, Res, range_u32};

/// `network.location_router_directly`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LocationRouterDirectly {
    /// `"deny"`.
    #[default]
    Deny = 0,
    /// `"require"`.
    Require = 1,
    /// `"allow"`.
    Allow = 2,
}

/// `network.property`: the adapter settings. Addresses are packed with the first octet in the low byte
/// (`192.168.0.1` is `0x0100A8C0`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetworkProperty {
    /// `save`: defaults to `true`.
    pub save: bool,
    /// `dhcp`: required. With `true` the five addresses are not read and stay `0`.
    pub dhcp: bool,
    pub ip_address: u32,
    pub subnet_mask: u32,
    pub gateway: u32,
    pub primary_dns: u32,
    pub secondary_dns: u32,
}

/// Section `network`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Network {
    pub enable: bool,
    pub support_standalone: bool,
    /// Defaults to `true`.
    pub use_gateway: bool,
    /// Defaults to `true`.
    pub use_location_router: bool,
    pub location_router_directly: LocationRouterDirectly,
    /// `None` when the `property` object is absent.
    pub property: Option<NetworkProperty>,
}

impl Default for Network {
    fn default() -> Self {
        Self {
            enable: false,
            support_standalone: false,
            use_gateway: true,
            use_location_router: true,
            location_router_directly: LocationRouterDirectly::Deny,
            property: None,
        }
    }
}

impl Network {
    pub(super) fn parse(root: &Map) -> Res<Self> {
        let mut out = Self::default();
        let Some(section) = Obj::section(root, "network")? else { return Ok(out) };
        out.enable = section.enable()?;
        if !out.enable {
            return Ok(out);
        }
        out.support_standalone = section.bool("support_standalone")?.unwrap_or(false);
        out.use_gateway = section.bool("use_gateway")?.unwrap_or(true);
        out.use_location_router = section.bool("use_location_router")?.unwrap_or(true);
        if let Some(text) = section.string("location_router_directly")? {
            out.location_router_directly = match text {
                "deny" => LocationRouterDirectly::Deny,
                "require" => LocationRouterDirectly::Require,
                "allow" => LocationRouterDirectly::Allow,
                other => {
                    return Err(section.bad("location_router_directly", "", &Json::String(other.to_owned())));
                }
            };
        }
        if let Some(property) = section.object("property")? {
            out.property = Some(NetworkProperty::parse(&property)?);
        }
        Ok(out)
    }
}

impl NetworkProperty {
    fn parse(property: &Obj<'_>) -> Res<Self> {
        let save = property.bool("save")?.unwrap_or(true);
        let dhcp = property.require_bool("dhcp")?;
        let mut out = Self { save, dhcp, ip_address: 0, subnet_mask: 0, gateway: 0, primary_dns: 0, secondary_dns: 0 };
        if dhcp {
            return Ok(out);
        }
        out.ip_address = address(property, "ip_address", false)?;
        out.subnet_mask = address(property, "subnet_mask", true)?;
        out.gateway = address(property, "gateway", true)?;
        out.primary_dns = address(property, "primary_dns", true)?;
        out.secondary_dns = address(property, "secondary_dns", true)?;
        Ok(out)
    }
}

/// A required IPv4 string key; `empty_is_zero` lets `""` through as `0` without validation.
fn address(object: &Obj<'_>, key: &str, empty_is_zero: bool) -> Res<u32> {
    let text = object.require_string(key)?;
    if empty_is_zero && text.is_empty() {
        return Ok(0);
    }
    parse_ipv4(text).ok_or_else(|| object.bad(key, "", &Json::String(text.to_owned())))
}

/// `a.b.c.d` with one to three ASCII digits per field, each `0..=255`; no sign, space or trailing text. The
/// original parses a NUL-terminated wide string, so text after an embedded NUL is not looked at. The result has
/// the first octet in the low byte.
pub(super) fn parse_ipv4(text: &str) -> Option<u32> {
    let text = text.split('\0').next().unwrap_or("");
    let mut fields = text.split('.');
    let mut packed = 0_u32;
    for shift in [0, 8, 16, 24] {
        let field = fields.next()?;
        if field.is_empty() || field.len() > 3 || !field.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let value: u32 = field.parse().ok()?;
        if value > 255 {
            return None;
        }
        packed |= value << shift;
    }
    if fields.next().is_some() { None } else { Some(packed) }
}

/// Section `lan_install`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanInstall {
    pub enable: bool,
    /// `true` is the LAN-install server, `false` the client. Required while enabled.
    pub server: bool,
    /// Seconds, `1..=99`; defaults to `20`.
    pub interval: u32,
}

impl Default for LanInstall {
    fn default() -> Self {
        Self { enable: false, server: false, interval: 20 }
    }
}

impl LanInstall {
    pub(super) fn parse(root: &Map) -> Res<Self> {
        let mut out = Self::default();
        let Some(section) = Obj::section(root, "lan_install")? else { return Ok(out) };
        out.enable = section.enable()?;
        if !out.enable {
            return Ok(out);
        }
        out.server = section.require_bool("server")?;
        if let Some(v) = section.number_with("interval", range_u32(1.0, 99.0))? {
            out.interval = v;
        }
        Ok(out)
    }
}

/// Section `net_delivery`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NetDelivery {
    pub enable: bool,
    pub local: bool,
    pub use_aws_signature: bool,
}

impl NetDelivery {
    pub(super) fn parse(root: &Map) -> Res<Self> {
        let mut out = Self::default();
        let Some(section) = Obj::section(root, "net_delivery")? else { return Ok(out) };
        out.enable = section.enable()?;
        if !out.enable {
            return Ok(out);
        }
        out.local = section.bool("local")?.unwrap_or(false);
        out.use_aws_signature = section.bool("use_aws_signature")?.unwrap_or(false);
        Ok(out)
    }
}
