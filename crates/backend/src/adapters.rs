//! Adapter enumeration, read-back and address configuration (`network.md` P.4, P.8).
//!
//! The Sega-OUI grouping, the eight-adapter cap and the property-word encoding stay with the network engine.
//! This is only what the operating system reports and what `netsh` changes.

use std::fmt;
use std::net::Ipv4Addr;

use crate::Unsupported;

/// An adapter the engine has chosen: interface index, the GUID name `ipconfig` and `netsh` use, and its MAC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterId {
    /// `ifIndex`, also the `netsh` interface argument.
    pub index: u32,
    /// Adapter GUID (`ipconfig`'s adapter name).
    pub name: String,
    /// Six-byte MAC.
    pub mac: [u8; 6],
}

/// Interface type, as the Eth0 rules group it (P.4 step 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterfaceKind {
    /// Skipped.
    Unknown,
    /// Skipped.
    Loopback,
    /// The wireless group.
    Wireless,
    /// Everything else: the wired group.
    Other,
}

/// One interface from the enumeration, before the Eth0 rules drop or order it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nic {
    /// `ifIndex`.
    pub index: u32,
    /// Adapter GUID.
    pub name: String,
    /// MAC; `None` when the interface has none (the Eth0 rules skip it).
    pub mac: Option<[u8; 6]>,
    /// Wired, wireless, loopback or unknown.
    pub kind: InterfaceKind,
    /// First IPv4 address (`0.0.0.0` when unset).
    pub ip: Ipv4Addr,
    /// Netmask of `ip` (`0.0.0.0` when unset).
    pub mask: Ipv4Addr,
    /// The operational state is up.
    pub up: bool,
}

/// The chosen adapter's IPv4 configuration, read back for a property request (P.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterConfig {
    /// DHCP is enabled for IPv4.
    pub dhcp: bool,
    /// First IPv4 address, when the adapter has one.
    pub ip: Option<Ipv4Addr>,
    /// Prefix length of [`ip`](Self::ip). `0` with an address is the all-ones mask (the x86 shift wrap).
    pub prefix: u8,
    /// Netmask the enumeration reported for [`ip`](Self::ip). Unused when [`prefix`](Self::prefix) is 0.
    pub mask: Ipv4Addr,
    /// First IPv4 gateway.
    pub gateway: Option<Ipv4Addr>,
    /// IPv4 DNS servers, in the order the stack reports them.
    pub dns: Vec<Ipv4Addr>,
    /// The operational state is up.
    pub link_up: bool,
    /// The address is in the duplicate-address state (`DadState`); the request result is then -17.
    pub duplicate: bool,
}

/// An enumeration entry was not there, or a read or `netsh` command failed. The engine maps this to request
/// result -1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AdapterError {
    /// The adapter is gone, or the command could not be completed.
    #[error("adapter operation failed")]
    Failed,
}

/// How [`Adapters::apply_dhcp`] changes an adapter that should be on DHCP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DhcpApply {
    /// The address is already DHCP; only the DNS servers switch to DHCP.
    Dns,
    /// The link is up: `set address dhcp` must succeed, then DNS switches to DHCP.
    Address,
    /// The link is down: a failed `set address dhcp` is ignored (DHCP is armed for the next connect), then DNS
    /// switches to DHCP. The caller writes `DisableDhcpOnConnect` before this.
    AddressBestEffort,
}

/// Enumeration, read-back and address changes. Blocking `netsh` calls run on the engine's worker thread.
/// Shared by that worker and by LAN install, so it is [`Sync`].
pub trait Adapters: Send + Sync + fmt::Debug {
    /// Every interface the operating system enumerates, including ones the Eth0 rules skip.
    fn list(&self) -> Vec<Nic>;

    /// The chosen adapter's address, DHCP flag, gateway, DNS, link and duplicate-address state.
    fn read(&self, id: &AdapterId) -> Result<AdapterConfig, AdapterError>;

    /// Switch the adapter to DHCP as `how` describes.
    fn apply_dhcp(&self, id: &AdapterId, how: DhcpApply) -> Result<(), AdapterError>;

    /// Set a static address and replace the DNS servers.
    ///
    /// `gateway` `None` omits the gateway (the command's `0` gateway). `dns` empty deletes the servers and adds
    /// none; otherwise they are added in order, after the previous servers were deleted.
    fn apply_static(
        &self,
        id: &AdapterId,
        ip: Ipv4Addr,
        mask: Ipv4Addr,
        gateway: Option<Ipv4Addr>,
        dns: &[Ipv4Addr],
    ) -> Result<(), AdapterError>;

    /// The IPv4 address of the active adapter (`ip=` of the power-on request).
    ///
    /// `None` when it cannot be determined. The caller then uses `0.0.0.0`, which is what a failed lookup
    /// reported before this lived on the adapter interface.
    fn local_address(&self) -> Option<Ipv4Addr>;
}

/// No adapter enumeration: no adapter, no read-back, no configuration.
impl Adapters for Unsupported {
    fn list(&self) -> Vec<Nic> {
        Vec::new()
    }

    fn read(&self, _id: &AdapterId) -> Result<AdapterConfig, AdapterError> {
        Err(AdapterError::Failed)
    }

    fn apply_dhcp(&self, _id: &AdapterId, _how: DhcpApply) -> Result<(), AdapterError> {
        Err(AdapterError::Failed)
    }

    fn apply_static(
        &self,
        _id: &AdapterId,
        _ip: Ipv4Addr,
        _mask: Ipv4Addr,
        _gateway: Option<Ipv4Addr>,
        _dns: &[Ipv4Addr],
    ) -> Result<(), AdapterError> {
        Err(AdapterError::Failed)
    }

    fn local_address(&self) -> Option<Ipv4Addr> {
        None
    }
}
