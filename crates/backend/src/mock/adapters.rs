//! A scripted adapter list. The default is one wired Sega-OUI NIC: DHCP `192.168.0.5/24`, gateway and DNS
//! `192.168.0.1`, link up.

use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::{AdapterConfig, AdapterError, AdapterId, Adapters, DhcpApply, InterfaceKind, Nic};

/// MAC prefix of the Sega NIC (`0x140318B80`).
const SEGA_OUI: [u8; 3] = [0x00, 0xD0, 0xF1];

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    nic: Nic,
    config: AdapterConfig,
}

#[derive(Debug)]
struct State {
    entries: Vec<Entry>,
    /// [`Adapters::list`] returns nothing.
    fail_list: bool,
    /// [`Adapters::read`] reports a duplicate address.
    duplicate: bool,
    /// [`Adapters::apply_dhcp`] and [`Adapters::apply_static`] return this instead of changing the config.
    apply_result: Result<(), AdapterError>,
    applied: Vec<Apply>,
    /// Address [`Adapters::local_address`] reports. `None` until [`SimAdapters::set_local`].
    local: Option<Ipv4Addr>,
}

/// One `apply_*` call, successful or not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Apply {
    /// [`Adapters::apply_dhcp`].
    Dhcp {
        /// The adapter.
        id: AdapterId,
        /// Which half of the DHCP change was requested.
        how: DhcpApply,
    },
    /// [`Adapters::apply_static`].
    Static {
        /// The adapter.
        id: AdapterId,
        /// Address.
        ip: Ipv4Addr,
        /// Mask.
        mask: Ipv4Addr,
        /// Gateway, when the command carried one.
        gateway: Option<Ipv4Addr>,
        /// DNS servers, in the order they would be added.
        dns: Vec<Ipv4Addr>,
    },
}

/// Scripted adapters. Applies update the stored config unless [`SimAdapters::set_apply_result`] is an error.
#[derive(Debug, Clone)]
pub struct SimAdapters {
    state: Arc<Mutex<State>>,
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|err| err.into_inner())
}

fn prefix_of(mask: Ipv4Addr) -> u8 {
    u32::from(mask).leading_ones() as u8
}

fn default_entry() -> Entry {
    let ip = Ipv4Addr::new(192, 168, 0, 5);
    let mask = Ipv4Addr::new(255, 255, 255, 0);
    let gateway = Ipv4Addr::new(192, 168, 0, 1);
    Entry {
        nic: Nic {
            index: 1,
            name: "eth0".to_owned(),
            mac: Some([SEGA_OUI[0], SEGA_OUI[1], SEGA_OUI[2], 0x00, 0x00, 0x01]),
            kind: InterfaceKind::Other,
            ip,
            mask,
            up: true,
        },
        config: AdapterConfig {
            dhcp: true,
            ip: Some(ip),
            prefix: 24,
            mask,
            gateway: Some(gateway),
            dns: vec![gateway],
            link_up: true,
            duplicate: false,
        },
    }
}

impl SimAdapters {
    /// One wired Sega adapter, DHCP `192.168.0.5/24`.
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                entries: vec![default_entry()],
                fail_list: false,
                duplicate: false,
                apply_result: Ok(()),
                applied: Vec::new(),
                local: None,
            })),
        }
    }

    /// Replaces the enumeration. Each NIC's read-back is its address and link, with DHCP off and no gateway.
    pub fn set_adapters(&self, nics: Vec<Nic>) {
        let entries = nics
            .into_iter()
            .map(|nic| {
                let prefix = prefix_of(nic.mask);
                let config = AdapterConfig {
                    dhcp: false,
                    ip: (nic.ip != Ipv4Addr::UNSPECIFIED).then_some(nic.ip),
                    prefix,
                    mask: nic.mask,
                    gateway: None,
                    dns: Vec::new(),
                    link_up: nic.up,
                    duplicate: false,
                };
                Entry { nic, config }
            })
            .collect();
        lock(&self.state).entries = entries;
    }

    /// Replaces the read-back of the NIC with this `index`, and mirrors the address and link onto the enumeration.
    pub fn set_config(&self, index: u32, config: AdapterConfig) {
        let mut state = lock(&self.state);
        let Some(entry) = state.entries.iter_mut().find(|entry| entry.nic.index == index) else { return };
        if let Some(ip) = config.ip {
            entry.nic.ip = ip;
            entry.nic.mask = if config.prefix == 0 { Ipv4Addr::BROADCAST } else { config.mask };
        }
        entry.nic.up = config.link_up;
        entry.config = config;
    }

    /// When `fail` is set, [`Adapters::list`] returns nothing.
    pub fn fail_enumeration(&self, fail: bool) {
        lock(&self.state).fail_list = fail;
    }

    /// When set, [`Adapters::read`] reports [`AdapterConfig::duplicate`].
    pub fn set_duplicate(&self, duplicate: bool) {
        lock(&self.state).duplicate = duplicate;
    }

    /// Later `apply_*` calls return `result` and, on `Err`, leave the config unchanged.
    pub fn set_apply_result(&self, result: Result<(), AdapterError>) {
        lock(&self.state).apply_result = result;
    }

    /// Every `apply_*` call so far, including ones that failed.
    pub fn applied(&self) -> Vec<Apply> {
        lock(&self.state).applied.clone()
    }

    /// The address [`Adapters::local_address`] reports (`ip=` of the power-on request).
    pub fn set_local(&self, address: Ipv4Addr) {
        lock(&self.state).local = Some(address);
    }
}

impl Default for SimAdapters {
    fn default() -> Self {
        Self::new()
    }
}

impl Adapters for SimAdapters {
    fn list(&self) -> Vec<Nic> {
        let state = lock(&self.state);
        if state.fail_list { Vec::new() } else { state.entries.iter().map(|entry| entry.nic.clone()).collect() }
    }

    fn read(&self, id: &AdapterId) -> Result<AdapterConfig, AdapterError> {
        let state = lock(&self.state);
        let mut config =
            state.entries.iter().find(|entry| entry.nic.index == id.index).ok_or(AdapterError::Failed)?.config.clone();
        if state.duplicate {
            config.duplicate = true;
        }
        Ok(config)
    }

    fn apply_dhcp(&self, id: &AdapterId, how: DhcpApply) -> Result<(), AdapterError> {
        let mut state = lock(&self.state);
        state.applied.push(Apply::Dhcp { id: id.clone(), how });
        state.apply_result?;
        if let Some(entry) = state.entries.iter_mut().find(|entry| entry.nic.index == id.index) {
            entry.config.dhcp = true;
        }
        Ok(())
    }

    fn apply_static(
        &self,
        id: &AdapterId,
        ip: Ipv4Addr,
        mask: Ipv4Addr,
        gateway: Option<Ipv4Addr>,
        dns: &[Ipv4Addr],
    ) -> Result<(), AdapterError> {
        let mut state = lock(&self.state);
        state.applied.push(Apply::Static { id: id.clone(), ip, mask, gateway, dns: dns.to_vec() });
        state.apply_result?;
        if let Some(entry) = state.entries.iter_mut().find(|entry| entry.nic.index == id.index) {
            entry.nic.ip = ip;
            entry.nic.mask = mask;
            entry.config.dhcp = false;
            entry.config.ip = Some(ip);
            entry.config.prefix = prefix_of(mask);
            entry.config.mask = mask;
            entry.config.gateway = gateway;
            entry.config.dns = dns.to_vec();
            entry.config.duplicate = false;
        }
        Ok(())
    }

    fn local_address(&self) -> Option<Ipv4Addr> {
        lock(&self.state).local
    }
}
