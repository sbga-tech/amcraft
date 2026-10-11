//! The adapter facts the ALL.Net manager publishes (`allnet.md` object model `+0x494..+0x4B0`, refresh
//! `0x14023E110`; network class `0x1403B3F70`, classification at IP_ADDRESS END): the eth0 properties, taken once
//! the adapter answered, and the class of the address.

use crate::network::{AmNetwork, PropertyBuffer};
use backend::Backend;

/// `amNetworkGetResult`: the request is still pending.
const PENDING: i32 = 1;

/// The class of the adapter address (`N+236`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetClass {
    /// 0: neither of the others.
    Other,
    /// 1: an address in 10.0.0.0/8 while the router type is not the title router.
    Net10,
    /// 2: the same subnet as the keychip network.
    Keychip,
}

/// The eth0 properties and their class, filled once.
#[derive(Debug, Default)]
pub struct NetInfo {
    issued: bool,
    record: Option<(PropertyBuffer, NetClass)>,
}

impl NetInfo {
    /// One frame: asks the adapter for its properties, then reads them on success (`first success only`). A
    /// failed or link-down answer asks again on the next frame. `title_router` is the router type 2.
    pub fn step(&mut self, net: &mut AmNetwork, backend: &mut Backend, title_router: bool) {
        if self.record.is_some() {
            return;
        }
        if !self.issued {
            self.issued = net.start_get_property(&backend.adapters) != PENDING;
            return;
        }
        let result = net.result();
        if result == PENDING {
            return;
        }
        self.issued = false;
        if result != 0 {
            return;
        }
        let buffer = net.property();
        if buffer.flags & PropertyBuffer::LINK_DOWN != 0 || buffer.ip == 0 {
            return;
        }
        let class = if net.keychip_address(&mut *backend.registry) == (buffer.ip & buffer.mask) {
            NetClass::Keychip
        } else if buffer.ip.to_le_bytes()[0] == 10 && !title_router {
            NetClass::Net10
        } else {
            NetClass::Other
        };
        self.record = Some((buffer, class));
    }

    /// The properties (slot 3: `+1172` flag set).
    pub fn property(&self) -> Option<&PropertyBuffer> {
        self.record.as_ref().map(|(buffer, _)| buffer)
    }

    /// The class of the address.
    pub fn class(&self) -> Option<NetClass> {
        self.record.map(|(_, class)| class)
    }
}
