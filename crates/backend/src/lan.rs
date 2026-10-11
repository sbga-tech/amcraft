//! The LAN interface of LAN install: the beacon UDP socket and the sync channel (`spec/libam/network.md` L.9).
//! The adapter LAN install binds to is the Eth0 choice over [`Adapters`](crate::Adapters).
//!
//! [`StdLanNet`] is the default implementation, sockets from `std::net`: the datagram goes to a target address and
//! the sockets bind configurable ports; [`StdLanNet::real`] uses the beacon and sync ports of LAN install.

use std::fmt;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream, UdpSocket};
use std::time::Duration;

/// UDP port of the beacon (`0x9CB0`).
pub const BEACON_PORT: u16 = 40112;
/// TCP port of the sync channel.
pub const SYNC_PORT: u16 = 40110;
/// Largest beacon datagram (`1020` bytes of payload).
pub const MAX_DATAGRAM: usize = 1020;

/// The beacon UDP socket (non-blocking).
pub trait Datagram: fmt::Debug {
    /// Sends one datagram to the beacon destination.
    fn send(&mut self, bytes: &[u8]) -> io::Result<()>;
    /// The next received datagram, `None` when nothing is waiting.
    fn recv(&mut self) -> io::Result<Option<Vec<u8>>>;
}

/// The TCP sync listener of a server (non-blocking).
pub trait SyncListener: fmt::Debug {
    /// A client's 4-byte payload, `None` when no connection with data is waiting.
    fn accept(&mut self) -> io::Result<Option<[u8; 4]>>;
}

/// The sockets LAN install opens.
pub trait LanNet: fmt::Debug {
    /// Opens the beacon socket bound to `<local>:40112`; `broadcast` selects the sending role.
    fn open_udp(&mut self, local: Ipv4Addr, broadcast: bool) -> io::Result<Box<dyn Datagram>>;
    /// Listens on `<local>:40110`.
    fn listen_sync(&mut self, local: Ipv4Addr) -> io::Result<Box<dyn SyncListener>>;
    /// Connects to `<server>:40110` and sends the 4 bytes of `own` (`SendSyncData`).
    fn send_sync(&mut self, server: Ipv4Addr, own: Ipv4Addr) -> io::Result<()>;
}

/// Sockets from `std::net`. The datagram goes to `target`, the sockets bind `bind_port` / `sync_port`.
#[derive(Debug)]
pub struct StdLanNet {
    bind_port: u16,
    sync_port: u16,
    target: SocketAddrV4,
}

impl StdLanNet {
    /// The production ports: bind 40112, broadcast to `255.255.255.255:40112`, sync on 40110.
    pub fn real() -> Self {
        Self::new(BEACON_PORT, SocketAddrV4::new(Ipv4Addr::BROADCAST, BEACON_PORT), SYNC_PORT)
    }

    /// Custom ports (tests on `127.0.0.1`).
    pub fn new(bind_port: u16, target: SocketAddrV4, sync_port: u16) -> Self {
        Self { bind_port, sync_port, target }
    }
}

#[derive(Debug)]
struct StdDatagram {
    socket: UdpSocket,
    target: SocketAddrV4,
}

impl Datagram for StdDatagram {
    fn send(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.socket.send_to(bytes, self.target).map(drop)
    }

    fn recv(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut buffer = [0u8; MAX_DATAGRAM + 4];
        match self.socket.recv_from(&mut buffer) {
            Ok((len, _)) => Ok(Some(buffer[..len.min(MAX_DATAGRAM)].to_vec())),
            Err(error) if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::ConnectionReset) => {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }
}

#[derive(Debug)]
struct StdSyncListener(TcpListener);

impl SyncListener for StdSyncListener {
    fn accept(&mut self) -> io::Result<Option<[u8; 4]>> {
        let mut stream = match self.0.accept() {
            Ok((stream, _)) => stream,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(None),
            Err(error) => return Err(error),
        };
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(Duration::from_millis(50)))?;
        let mut payload = [0u8; 4];
        let read = stream.read(&mut payload).unwrap_or(0);
        Ok((read != 0).then_some(payload))
    }
}

impl LanNet for StdLanNet {
    fn open_udp(&mut self, local: Ipv4Addr, broadcast: bool) -> io::Result<Box<dyn Datagram>> {
        let socket = UdpSocket::bind(SocketAddr::from((local, self.bind_port)))?;
        socket.set_nonblocking(true)?;
        if broadcast {
            socket.set_broadcast(true)?;
        }
        Ok(Box::new(StdDatagram { socket, target: self.target }))
    }

    fn listen_sync(&mut self, local: Ipv4Addr) -> io::Result<Box<dyn SyncListener>> {
        let listener = TcpListener::bind(SocketAddr::from((local, self.sync_port)))?;
        listener.set_nonblocking(true)?;
        Ok(Box::new(StdSyncListener(listener)))
    }

    fn send_sync(&mut self, server: Ipv4Addr, own: Ipv4Addr) -> io::Result<()> {
        let mut stream =
            TcpStream::connect_timeout(&SocketAddr::from((server, self.sync_port)), Duration::from_millis(1000))?;
        stream.set_write_timeout(Some(Duration::from_millis(1000)))?;
        stream.write_all(&own.octets())
    }
}
