//! One HTTP exchange over an [`AllnetTls`] session, advanced one stage per frame (connect, send, receive: the
//! sub-steps 3 to 5 of alAuth and the job states 3 to 5 of alAbEx), and the response parser of ntHttp2/ntHttp3
//! (`spec/libam/allnet.md` part 6 section 4.4).

use std::net::Ipv4Addr;
use std::time::Duration;

use backend::tls::{AllnetTls, TlsError, TlsProvider};

/// Connect, send and receive timeout of both clients.
pub const TIMEOUT: Duration = Duration::from_millis(60_000);
/// Capacity of the body object.
pub const BODY_LIMIT: usize = 4096;
/// ntHttp's protocol errors: syntax (`-300`) and header (`-301`).
pub const SYNTAX: i32 = -300;
/// Header too long or too many headers.
pub const HEADER: i32 = -301;

/// Where an exchange goes.
#[derive(Debug, Clone)]
pub struct Target {
    /// The server address.
    pub addr: Ipv4Addr,
    /// The port (443 for TLS, 80 otherwise).
    pub port: u16,
    /// TLS or plain.
    pub tls: bool,
    /// The certificate name pattern (`*.auth.sys-all.net`); empty for a plain target.
    pub server_name: String,
}

/// Progress of an exchange.
#[derive(Debug)]
pub enum Poll {
    /// Call again next frame.
    Pending,
    /// The whole answer (until the peer closed).
    Done(Vec<u8>),
    /// The exchange failed.
    Failed(TlsError),
}

/// One request/answer exchange.
pub struct Exchange {
    session: Box<dyn AllnetTls>,
    target: Target,
    request: Vec<u8>,
    stage: u8,
    response: Vec<u8>,
}

impl Exchange {
    /// Creates the session; nothing is sent yet.
    pub fn new(provider: &dyn TlsProvider, target: Target, request: Vec<u8>) -> Self {
        Self { session: provider.open(), target, request, stage: 0, response: Vec::new() }
    }

    /// One stage: connect, then send, then receive to the end.
    pub fn step(&mut self) -> Poll {
        match self.stage {
            0 => {
                let t = &self.target;
                let result = if t.tls {
                    self.session.connect(t.addr, t.port, &t.server_name, &[], TIMEOUT)
                } else {
                    self.session.connect_plain(t.addr, t.port, TIMEOUT)
                };
                match result {
                    Ok(()) => {
                        self.stage = 1;
                        Poll::Pending
                    }
                    Err(e) => Poll::Failed(e),
                }
            }
            1 => match self.session.write(&self.request) {
                Ok(()) => {
                    self.stage = 2;
                    Poll::Pending
                }
                Err(e) => Poll::Failed(e),
            },
            _ => {
                let mut buf = [0u8; 1024];
                loop {
                    match self.session.read(&mut buf) {
                        Ok(0) => return Poll::Done(std::mem::take(&mut self.response)),
                        Ok(n) => {
                            self.response.extend_from_slice(&buf[..n]);
                            if self.response.len() > 4 * BODY_LIMIT {
                                return Poll::Failed(TlsError::Io);
                            }
                        }
                        Err(e) => return Poll::Failed(e),
                    }
                }
            }
        }
    }

    /// The chain the server presented.
    pub fn chain(&self) -> Vec<Vec<u8>> {
        self.session.peer_chain()
    }

    /// Closes the session.
    pub fn close(&mut self) {
        self.session.close();
    }
}

/// A parsed answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    /// Status code.
    pub status: u32,
    /// Header `(name, value)` pairs in order.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Vec<u8>,
}

impl Answer {
    /// The first header named `name` (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// Parses an HTTP/1.0 or 1.1 answer: at most 15 headers of at most 255 characters, `Content-Length`, chunked
/// or read-until-close bodies of at most 4096 bytes, and a `Connection` header that must be `Close`.
pub fn parse_http(raw: &[u8]) -> Result<Answer, i32> {
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or(SYNTAX)?;
    let (head, rest) = (&raw[..split], &raw[split + 4..]);
    let head = std::str::from_utf8(head).map_err(|_| SYNTAX)?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().ok_or(SYNTAX)?;
    let mut parts = status_line.splitn(3, ' ');
    if !matches!(parts.next(), Some("HTTP/1.0" | "HTTP/1.1")) {
        return Err(SYNTAX);
    }
    let status: u32 = parts.next().and_then(|s| s.parse().ok()).ok_or(SYNTAX)?;
    if parts.next().is_some_and(|r| r.len() > 255) {
        return Err(HEADER);
    }
    let mut headers = Vec::new();
    for line in lines {
        let (name, value) = line.split_once(':').ok_or(SYNTAX)?;
        let value = value.trim_start();
        if headers.len() >= 15 || name.len() > 255 || value.len() > 255 {
            return Err(HEADER);
        }
        headers.push((name.to_owned(), value.to_owned()));
    }
    let answer = Answer { status, headers, body: Vec::new() };
    if answer.header("Connection").is_some_and(|v| !v.eq_ignore_ascii_case("close")) {
        return Err(SYNTAX);
    }
    let body = if answer.header("Transfer-Encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        dechunk(rest)?
    } else if let Some(length) = answer.header("Content-Length").and_then(|v| v.trim().parse::<usize>().ok()) {
        rest[..length.min(rest.len())].to_vec()
    } else {
        rest.to_vec()
    };
    if body.len() > BODY_LIMIT {
        return Err(SYNTAX);
    }
    Ok(Answer { body, ..answer })
}

fn dechunk(mut rest: &[u8]) -> Result<Vec<u8>, i32> {
    let mut body = Vec::new();
    loop {
        let eol = rest.windows(2).position(|w| w == b"\r\n").ok_or(SYNTAX)?;
        let size = std::str::from_utf8(&rest[..eol])
            .ok()
            .and_then(|s| usize::from_str_radix(s.split(';').next()?.trim(), 16).ok());
        let size = size.ok_or(SYNTAX)?;
        rest = &rest[eol + 2..];
        if size == 0 {
            return Ok(body);
        }
        if rest.len() < size + 2 {
            return Err(SYNTAX);
        }
        body.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..];
    }
}
