//! Adapter enumeration, read-back and `netsh` for amNetwork (`spec/libam/network.md` P.4, P.8).
//!
//! `netdev` supplies ifIndex, the GUID name, MAC, type, operational state, IPv4 address and prefix, and the DHCP
//! flag. It is built without the `gateway` feature, which would ARP-resolve every gateway during an enumeration
//! (the choice runs on the frame thread). Gateways and DNS servers come from `ipconfig`. Neither reports the
//! duplicate-address state of an IPv4 address (`DadState`, result -17 of P.8), so `is_duplicate` asks for it
//! with one `GetUnicastIpAddressEntry` call, the only `unsafe` of this module.
//!
//! Writes are `netsh` (`0x1402F7020`): a hidden window and a 100 s limit. The process is not killed on timeout.

use std::net::{IpAddr, Ipv4Addr};
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use netdev::interface::state::OperState;
use netdev::interface::types::InterfaceType;
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::NetworkManagement::IpHelper::{GetUnicastIpAddressEntry, MIB_UNICASTIPADDRESS_ROW};
use windows_sys::Win32::Networking::WinSock::{AF_INET, IpDadStateDuplicate, SOCKADDR_IN};

use crate::{AdapterConfig, AdapterError, AdapterId, Adapters, DhcpApply, InterfaceKind, Nic};

/// `WaitForSingleObject(netsh, 100000)`.
const NETSH_TIMEOUT: Duration = Duration::from_secs(100);
/// `CREATE_NO_WINDOW | NORMAL_PRIORITY_CLASS`.
const NETSH_FLAGS: u32 = 0x0800_0020;

/// `netdev`'s enumeration, one [`Nic`] per interface. Ordering and the Sega-OUI cap are the engine's.
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsAdapters;

impl WindowsAdapters {
    fn kind(kind: InterfaceType) -> InterfaceKind {
        match kind {
            InterfaceType::Unknown => InterfaceKind::Unknown,
            InterfaceType::Loopback => InterfaceKind::Loopback,
            InterfaceType::Wireless80211 => InterfaceKind::Wireless,
            _ => InterfaceKind::Other,
        }
    }
}

/// Whether `address` of the adapter `index` is in the duplicate-address state (`DadState`).
fn is_duplicate(index: u32, address: Ipv4Addr) -> bool {
    let mut socket = SOCKADDR_IN { sin_family: AF_INET, ..Default::default() };
    socket.sin_addr.S_un.S_addr = u32::from_ne_bytes(address.octets());
    let mut row = MIB_UNICASTIPADDRESS_ROW { InterfaceIndex: index, ..Default::default() };
    row.Address.Ipv4 = socket;
    // SAFETY: `row` is a valid in/out structure whose interface index and address (with its family) are set, which
    // is what the call reads to find the entry.
    unsafe { GetUnicastIpAddressEntry(&mut row) == ERROR_SUCCESS && row.DadState == IpDadStateDuplicate }
}

/// `netsh <args>` (`0x1402F7020`): hidden window, 100 s limit (the process is not killed on timeout). With
/// `check`, a non-zero exit code is a failure.
fn netsh(args: &[&str], check: bool) -> bool {
    let child = Command::new("netsh")
        .args(args)
        .creation_flags(NETSH_FLAGS)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else { return false };
    let deadline = Instant::now() + NETSH_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return !check || status.success(),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            _ => return false,
        }
    }
}

fn v4(addresses: &[IpAddr]) -> Vec<Ipv4Addr> {
    addresses
        .iter()
        .filter_map(|address| match address {
            IpAddr::V4(address) => Some(*address),
            IpAddr::V6(_) => None,
        })
        .collect()
}

impl Adapters for WindowsAdapters {
    fn list(&self) -> Vec<Nic> {
        netdev::get_interfaces()
            .into_iter()
            .map(|interface| {
                let (ip, mask) = interface
                    .ipv4
                    .first()
                    .map_or((Ipv4Addr::UNSPECIFIED, Ipv4Addr::UNSPECIFIED), |net| (net.addr(), net.netmask()));
                Nic {
                    index: interface.index,
                    name: interface.name,
                    mac: interface.mac_addr.map(|mac| mac.octets()),
                    kind: Self::kind(interface.if_type),
                    ip,
                    mask,
                    up: interface.oper_state == OperState::Up,
                }
            })
            .collect()
    }

    fn read(&self, id: &AdapterId) -> Result<AdapterConfig, AdapterError> {
        let interface = netdev::get_interfaces()
            .into_iter()
            .find(|interface| interface.index == id.index)
            .ok_or(AdapterError::Failed)?;
        let link_up = interface.oper_state == OperState::Up;
        let dhcp = interface.dhcp_v4_enabled == Some(true);
        let (ip, prefix, mask, duplicate) = if let Some(net) = interface.ipv4.first() {
            let addr = net.addr();
            let prefix = u8::try_from(u32::from(net.prefix_len())).unwrap_or(0);
            (Some(addr), prefix, net.netmask(), is_duplicate(id.index, addr))
        } else {
            (None, 0, Ipv4Addr::UNSPECIFIED, false)
        };
        // A duplicate address is -17 before gateways are read; an `ipconfig` failure must not hide it.
        if duplicate {
            return Ok(AdapterConfig {
                dhcp,
                ip,
                prefix,
                mask,
                gateway: None,
                dns: Vec::new(),
                link_up,
                duplicate: true,
            });
        }
        let adapters = ipconfig::get_adapters().map_err(|_| AdapterError::Failed)?;
        let configured = adapters
            .iter()
            .find(|entry| entry.adapter_name().eq_ignore_ascii_case(&id.name))
            .ok_or(AdapterError::Failed)?;
        Ok(AdapterConfig {
            dhcp,
            ip,
            prefix,
            mask,
            gateway: v4(configured.gateways()).first().copied(),
            dns: v4(configured.dns_servers()),
            link_up,
            duplicate: false,
        })
    }

    fn apply_dhcp(&self, id: &AdapterId, how: DhcpApply) -> Result<(), AdapterError> {
        let index = id.index.to_string();
        let (set_address, strict) = match how {
            DhcpApply::Dns => (false, false),
            DhcpApply::Address => (true, true),
            DhcpApply::AddressBestEffort => (true, false),
        };
        if set_address {
            let args = ["interface", "ipv4", "set", "address", index.as_str(), "dhcp"];
            // When the link is down `strict` is false and the exit code is ignored.
            let ok = netsh(&args, strict);
            if strict && !ok {
                return Err(AdapterError::Failed);
            }
        }
        let args = ["interface", "ipv4", "set", "dns", index.as_str(), "validate=no", "dhcp"];
        netsh(&args, true).then_some(()).ok_or(AdapterError::Failed)
    }

    fn apply_static(
        &self,
        id: &AdapterId,
        ip: Ipv4Addr,
        mask: Ipv4Addr,
        gateway: Option<Ipv4Addr>,
        dns: &[Ipv4Addr],
    ) -> Result<(), AdapterError> {
        let index = id.index.to_string();
        let ip_text = ip.to_string();
        let mask_text = mask.to_string();
        let gateway_text = gateway.map(|addr| addr.to_string());
        let mut args =
            vec!["interface", "ipv4", "set", "address", index.as_str(), "static", ip_text.as_str(), mask_text.as_str()];
        if let Some(text) = gateway_text.as_deref() {
            args.push(text);
        }
        if !netsh(&args, true) {
            return Err(AdapterError::Failed);
        }
        let delete = ["interface", "ipv4", "delete", "dns", index.as_str(), "all", "validate=no"];
        if !netsh(&delete, true) {
            return Err(AdapterError::Failed);
        }
        for server in dns {
            let server = server.to_string();
            let add = ["interface", "ipv4", "add", "dns", index.as_str(), "validate=no", server.as_str()];
            if !netsh(&add, true) {
                return Err(AdapterError::Failed);
            }
        }
        Ok(())
    }

    fn local_address(&self) -> Option<Ipv4Addr> {
        // Connecting a UDP socket to the documentation range selects the default-route interface and sends
        // nothing (port 9 is discard).
        let probe = std::net::UdpSocket::bind("0.0.0.0:0").and_then(|socket| {
            socket.connect("192.0.2.1:9")?;
            socket.local_addr()
        });
        match probe.map(|addr| addr.ip()) {
            Ok(IpAddr::V4(address)) => Some(address),
            _ => None,
        }
    }
}
