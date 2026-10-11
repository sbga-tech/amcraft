//! The Aime DB transport: name resolution and one TCP connection per request
//! (`spec/libam/aime.md` B.1).
//!
//! [`TcpTransport`] is the default implementation, real TCP: [`TcpTransport::new`] resolves the name libam asks
//! for, [`TcpTransport::fixed`] answers one fixed address (a server on another host or a local test server).

use std::fmt;
use std::io::{Read, Write};
use std::net::{SocketAddr, SocketAddrV4, TcpStream, ToSocketAddrs};
use std::time::Duration;

/// A resolved server address (the 16-byte record of B.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerAddr {
    /// IPv4 address and port.
    pub addr: SocketAddrV4,
}

/// One TCP connection to the Aime DB (B.1: one connection per request).
pub trait AimeDbStream: Send {
    /// Sends the bytes.
    fn send(&mut self, data: &[u8]) -> std::io::Result<()>;
    /// Receives up to `buf.len()` bytes, waiting at most `timeout`. `Ok(0)` = the peer closed; a timeout is
    /// `Err(WouldBlock/TimedOut)`.
    fn recv(&mut self, buf: &mut [u8], timeout: Duration) -> std::io::Result<usize>;
}

/// The Aime DB transport: name resolution and connections.
pub trait AimeDbTransport: Send + Sync + fmt::Debug {
    /// `getaddrinfo(host, port)`, IPv4 only (B.1; libam asks for `aime.naominet.jp:22345`). Blocking.
    fn resolve(&self, host: &str, port: u16) -> std::io::Result<ServerAddr>;
    /// Opens a connection to `addr`, giving up after `timeout`.
    fn connect(&self, addr: ServerAddr, timeout: Duration) -> std::io::Result<Box<dyn AimeDbStream>>;
}

/// Real TCP: connects to the resolved address, or to a fixed one.
#[derive(Debug, Clone, Default)]
pub struct TcpTransport {
    fixed: Option<SocketAddrV4>,
}

impl TcpTransport {
    /// Resolves the requested name (`getaddrinfo`, IPv4 only).
    pub fn new() -> Self {
        Self::default()
    }

    /// A transport whose [`AimeDbTransport::resolve`] answers `server` without DNS.
    pub fn fixed(server: SocketAddrV4) -> Self {
        Self { fixed: Some(server) }
    }
}

struct TcpIo(TcpStream);

impl AimeDbStream for TcpIo {
    fn send(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.0.write_all(data)
    }

    fn recv(&mut self, buf: &mut [u8], timeout: Duration) -> std::io::Result<usize> {
        self.0.set_read_timeout(Some(timeout.max(Duration::from_millis(1))))?;
        self.0.read(buf)
    }
}

impl AimeDbTransport for TcpTransport {
    fn resolve(&self, host: &str, port: u16) -> std::io::Result<ServerAddr> {
        if let Some(addr) = self.fixed {
            return Ok(ServerAddr { addr });
        }
        (host, port)
            .to_socket_addrs()?
            .find_map(|addr| match addr {
                SocketAddr::V4(v4) => Some(ServerAddr { addr: v4 }),
                SocketAddr::V6(_) => None,
            })
            .ok_or_else(|| std::io::Error::other("no IPv4 address"))
    }

    fn connect(&self, addr: ServerAddr, timeout: Duration) -> std::io::Result<Box<dyn AimeDbStream>> {
        let stream = TcpStream::connect_timeout(&SocketAddr::V4(addr.addr), timeout.max(Duration::from_millis(1)))?;
        stream.set_nodelay(true)?;
        Ok(Box::new(TcpIo(stream)))
    }
}
