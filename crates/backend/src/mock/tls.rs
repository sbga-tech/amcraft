//! The scripted in-memory TLS server: each `connect` consumes the next [`Script`] (a connect without one fails
//! with [`TlsError::ConnectFailed`]) and records the connection and the bytes written.

use std::collections::VecDeque;
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::tls::{AllnetTls, TlsError, TlsProvider};

/// The computation of a [`Responder`].
pub type Answer = dyn Fn(&[u8]) -> Vec<u8> + Send + Sync;

/// A server answer computed from the request bytes the client wrote (a power-on answer echoes the request token).
#[derive(Clone)]
pub struct Responder(pub Arc<Answer>);

impl std::fmt::Debug for Responder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Responder")
    }
}

/// What the scripted server does for one connection.
#[derive(Debug, Clone, Default)]
pub struct Script {
    /// The connect fails with this error.
    pub connect_error: Option<TlsError>,
    /// The bytes the server answers with once the request was written.
    pub reply: Vec<u8>,
    /// Computes `reply` from the request instead (the first read).
    pub responder: Option<Responder>,
    /// The certificate chain the server presents (DER, leaf first).
    pub chain: Vec<Vec<u8>>,
    /// The read fails with this error instead of returning `reply`.
    pub read_error: Option<TlsError>,
}

/// One connection the simulation saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimConnection {
    /// Target address.
    pub addr: Ipv4Addr,
    /// Target port.
    pub port: u16,
    /// Requested server name (empty for a plain session).
    pub server_name: String,
    /// Whether the session was TLS.
    pub tls: bool,
    /// Every byte the client wrote.
    pub written: Vec<u8>,
}

#[derive(Debug, Default)]
struct SimState {
    scripts: VecDeque<Script>,
    connections: Vec<SimConnection>,
    anchors: Option<Vec<Vec<u8>>>,
}

/// The scripted in-memory server: each `connect` consumes the next [`Script`] (a connect without one fails with
/// [`TlsError::ConnectFailed`]) and records the connection and the bytes written.
#[derive(Debug, Clone, Default)]
pub struct SimTls {
    state: Arc<Mutex<SimState>>,
}

impl SimTls {
    /// A simulation with no script.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, SimState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queues the behaviour of the next connection.
    pub fn push_script(&self, script: Script) {
        self.lock().scripts.push_back(script);
    }

    /// Sets the trust anchors (DER) the signer chain is checked against.
    pub fn set_anchors(&self, anchors: Vec<Vec<u8>>) {
        self.lock().anchors = Some(anchors);
    }

    /// Every connection so far, in order.
    pub fn connections(&self) -> Vec<SimConnection> {
        self.lock().connections.clone()
    }
}

impl TlsProvider for SimTls {
    fn open(&self) -> Box<dyn AllnetTls> {
        Box::new(SimSession { shared: self.clone(), index: None, script: Script::default(), sent: 0 })
    }

    fn trust_anchors(&self) -> Option<Vec<Vec<u8>>> {
        self.lock().anchors.clone()
    }
}

#[derive(Debug)]
struct SimSession {
    shared: SimTls,
    index: Option<usize>,
    script: Script,
    sent: usize,
}

impl SimSession {
    fn attach(&mut self, addr: Ipv4Addr, port: u16, server_name: &str, tls: bool) -> Result<(), TlsError> {
        let mut state = self.shared.lock();
        let script = state.scripts.pop_front().ok_or(TlsError::ConnectFailed)?;
        if let Some(error) = script.connect_error {
            return Err(error);
        }
        state.connections.push(SimConnection {
            addr,
            port,
            server_name: server_name.to_owned(),
            tls,
            written: Vec::new(),
        });
        self.index = Some(state.connections.len() - 1);
        self.script = script;
        self.sent = 0;
        Ok(())
    }
}

impl AllnetTls for SimSession {
    fn connect(
        &mut self,
        addr: Ipv4Addr,
        port: u16,
        server_name: &str,
        _trust_pem: &[u8],
        _timeout: Duration,
    ) -> Result<(), TlsError> {
        self.attach(addr, port, server_name, true)
    }

    fn connect_plain(&mut self, addr: Ipv4Addr, port: u16, _timeout: Duration) -> Result<(), TlsError> {
        self.attach(addr, port, "", false)
    }

    fn write(&mut self, data: &[u8]) -> Result<(), TlsError> {
        let index = self.index.ok_or(TlsError::Closed)?;
        self.shared.lock().connections[index].written.extend_from_slice(data);
        Ok(())
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<usize, TlsError> {
        let index = self.index.ok_or(TlsError::Closed)?;
        if let Some(error) = self.script.read_error {
            return Err(error);
        }
        if let Some(Responder(answer)) = self.script.responder.take() {
            let request = self.shared.lock().connections[index].written.clone();
            self.script.reply = answer(&request);
        }
        let rest = &self.script.reply[self.sent..];
        let n = rest.len().min(buf.len());
        buf[..n].copy_from_slice(&rest[..n]);
        self.sent += n;
        Ok(n)
    }

    fn peer_chain(&self) -> Vec<Vec<u8>> {
        self.script.chain.clone()
    }

    fn close(&mut self) {
        self.index = None;
    }
}
