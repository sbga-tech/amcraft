//! The TLS client of ALL.Net (`alssl`, `spec/libam/allnet.md` part 12): a blocking client session used by the
//! authentication (alAuth, alAbEx) and by the accounting reporter.
//!
//! [`NativeTls`] is the default implementation: the platform TLS client (SChannel through `native-tls` on Windows,
//! the system TLS library elsewhere).

use std::io::{Read, Write};
use std::net::Ipv4Addr;
use std::time::Duration;

/// Why a TLS (or plain TCP) session operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TlsError {
    /// The TCP connect did not finish in time (WSA 10060).
    #[error("connect timed out")]
    ConnectTimeout,
    /// The TCP connect failed.
    #[error("connect failed")]
    ConnectFailed,
    /// The TLS handshake failed.
    #[error("handshake failed")]
    Handshake,
    /// The server chain is not trusted or its common name does not match (`-10`).
    #[error("certificate rejected")]
    CertificateRejected,
    /// The server certificate is expired or not yet valid (`-11`).
    #[error("certificate date invalid")]
    CertificateDate,
    /// A send or receive timed out (WSA 10060).
    #[error("timed out")]
    Timeout,
    /// The session is not open.
    #[error("closed")]
    Closed,
    /// Any other socket error.
    #[error("i/o error")]
    Io,
}

impl TlsError {
    /// Whether this is the socket timeout (`WSA 10060`) the error maps single out.
    pub fn is_timeout(self) -> bool {
        matches!(self, Self::ConnectTimeout | Self::Timeout)
    }
}

/// One blocking client session. `trust_pem` is the CA bundle to trust; empty means the platform root store.
pub trait AllnetTls: Send {
    /// Connects with TLS to `addr:port`, checking the server certificate against `server_name`
    /// (a `*.domain` pattern is accepted) and `trust_pem`.
    fn connect(
        &mut self,
        addr: Ipv4Addr,
        port: u16,
        server_name: &str,
        trust_pem: &[u8],
        timeout: Duration,
    ) -> Result<(), TlsError>;
    /// Connects a plain TCP session.
    fn connect_plain(&mut self, addr: Ipv4Addr, port: u16, timeout: Duration) -> Result<(), TlsError>;
    /// Sends all of `data`.
    fn write(&mut self, data: &[u8]) -> Result<(), TlsError>;
    /// Reads at most `buf.len()` bytes; `Ok(0)` is the peer closing.
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, TlsError>;
    /// The certificate chain the server presented (DER, leaf first); empty for a plain session.
    fn peer_chain(&self) -> Vec<Vec<u8>>;
    /// Closes the session (idempotent).
    fn close(&mut self);
}

/// Creates sessions; the `tls` field of [`crate::Backend`].
pub trait TlsProvider: Send {
    /// A fresh, unconnected session.
    fn open(&self) -> Box<dyn AllnetTls>;
    /// The DER trust anchors the `auth_data` signer chain must end in. `None` means the transport validated the
    /// server chain against the platform store itself, so the presented chain is the anchor set.
    fn trust_anchors(&self) -> Option<Vec<Vec<u8>>> {
        None
    }
}

/// The platform TLS client (SChannel through `native-tls`).
pub struct NativeTls;

impl TlsProvider for NativeTls {
    fn open(&self) -> Box<dyn AllnetTls> {
        Box::new(NativeSession { stream: None, chain: Vec::new() })
    }
}

enum Stream {
    Plain(std::net::TcpStream),
    Tls(Box<native_tls::TlsStream<std::net::TcpStream>>),
}

struct NativeSession {
    stream: Option<Stream>,
    chain: Vec<Vec<u8>>,
}

fn io_error(error: &std::io::Error) -> TlsError {
    match error.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => TlsError::Timeout,
        _ => TlsError::Io,
    }
}

fn tcp(addr: Ipv4Addr, port: u16, timeout: Duration) -> Result<std::net::TcpStream, TlsError> {
    let socket = std::net::SocketAddr::from((addr, port));
    let stream = std::net::TcpStream::connect_timeout(&socket, timeout).map_err(|e| match e.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => TlsError::ConnectTimeout,
        _ => TlsError::ConnectFailed,
    })?;
    stream.set_read_timeout(Some(timeout)).map_err(|_| TlsError::Io)?;
    stream.set_write_timeout(Some(timeout)).map_err(|_| TlsError::Io)?;
    Ok(stream)
}

impl AllnetTls for NativeSession {
    fn connect(
        &mut self,
        addr: Ipv4Addr,
        port: u16,
        server_name: &str,
        trust_pem: &[u8],
        timeout: Duration,
    ) -> Result<(), TlsError> {
        let tcp = tcp(addr, port, timeout)?;
        let mut builder = native_tls::TlsConnector::builder();
        if !trust_pem.is_empty() {
            let root = native_tls::Certificate::from_pem(trust_pem).map_err(|_| TlsError::CertificateRejected)?;
            builder.add_root_certificate(root).disable_built_in_roots(true);
        }
        let connector = builder.build().map_err(|_| TlsError::Handshake)?;
        let domain = server_name.strip_prefix("*.").map_or_else(|| server_name.to_owned(), |rest| format!("at.{rest}"));
        let stream = connector.connect(&domain, tcp).map_err(|_| TlsError::CertificateRejected)?;
        self.chain = stream.peer_certificate().ok().flatten().and_then(|c| c.to_der().ok()).into_iter().collect();
        self.stream = Some(Stream::Tls(Box::new(stream)));
        Ok(())
    }

    fn connect_plain(&mut self, addr: Ipv4Addr, port: u16, timeout: Duration) -> Result<(), TlsError> {
        self.stream = Some(Stream::Plain(tcp(addr, port, timeout)?));
        Ok(())
    }

    fn write(&mut self, data: &[u8]) -> Result<(), TlsError> {
        let result = match self.stream.as_mut().ok_or(TlsError::Closed)? {
            Stream::Plain(s) => s.write_all(data),
            Stream::Tls(s) => s.write_all(data),
        };
        result.map_err(|e| io_error(&e))
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<usize, TlsError> {
        let result = match self.stream.as_mut().ok_or(TlsError::Closed)? {
            Stream::Plain(s) => s.read(buf),
            Stream::Tls(s) => s.read(buf),
        };
        result.map_err(|e| io_error(&e))
    }

    fn peer_chain(&self) -> Vec<Vec<u8>> {
        self.chain.clone()
    }

    fn close(&mut self) {
        self.stream = None;
    }
}
