//! In-memory LAN sockets: datagrams queued with [`SimLan::push_datagram`], everything sent recorded. The adapter
//! LAN install binds to comes from the backend's adapter enumeration (`SimAdapters`).

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io;
use std::net::Ipv4Addr;
use std::rc::Rc;

use crate::lan::{Datagram, LanNet, SyncListener};

#[derive(Debug, Default)]
struct SimLanState {
    sent: Vec<Vec<u8>>,
    inbox: VecDeque<Vec<u8>>,
    sync_inbox: VecDeque<[u8; 4]>,
    sync_sent: Vec<(Ipv4Addr, Ipv4Addr)>,
    fail_open: bool,
}

/// In-memory LAN sockets: datagrams queued with [`SimLan::push_datagram`], everything sent recorded.
#[derive(Debug, Clone, Default)]
pub struct SimLan {
    state: Rc<RefCell<SimLanState>>,
}

impl SimLan {
    /// A LAN whose sockets open and stay silent.
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues a datagram for the next receive.
    pub fn push_datagram(&self, bytes: &[u8]) {
        self.state.borrow_mut().inbox.push_back(bytes.to_vec());
    }

    /// Queues a client's sync payload for the server listener.
    pub fn push_sync(&self, payload: [u8; 4]) {
        self.state.borrow_mut().sync_inbox.push_back(payload);
    }

    /// Every datagram sent so far.
    pub fn sent(&self) -> Vec<Vec<u8>> {
        self.state.borrow().sent.clone()
    }

    /// Every `(server, own)` sync connection made so far.
    pub fn sync_sent(&self) -> Vec<(Ipv4Addr, Ipv4Addr)> {
        self.state.borrow().sync_sent.clone()
    }

    /// Makes socket creation fail.
    pub fn fail_open(&self, fail: bool) {
        self.state.borrow_mut().fail_open = fail;
    }
}

impl Datagram for SimLan {
    fn send(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.state.borrow_mut().sent.push(bytes.to_vec());
        Ok(())
    }

    fn recv(&mut self) -> io::Result<Option<Vec<u8>>> {
        Ok(self.state.borrow_mut().inbox.pop_front())
    }
}

impl SyncListener for SimLan {
    fn accept(&mut self) -> io::Result<Option<[u8; 4]>> {
        Ok(self.state.borrow_mut().sync_inbox.pop_front())
    }
}

impl LanNet for SimLan {
    fn open_udp(&mut self, _local: Ipv4Addr, _broadcast: bool) -> io::Result<Box<dyn Datagram>> {
        if self.state.borrow().fail_open {
            return Err(io::ErrorKind::AddrInUse.into());
        }
        Ok(Box::new(self.clone()))
    }

    fn listen_sync(&mut self, _local: Ipv4Addr) -> io::Result<Box<dyn SyncListener>> {
        if self.state.borrow().fail_open {
            return Err(io::ErrorKind::AddrInUse.into());
        }
        Ok(Box::new(self.clone()))
    }

    fn send_sync(&mut self, server: Ipv4Addr, own: Ipv4Addr) -> io::Result<()> {
        self.state.borrow_mut().sync_sent.push((server, own));
        Ok(())
    }
}
