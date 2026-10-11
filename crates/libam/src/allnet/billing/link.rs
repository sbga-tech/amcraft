//! The accounting report's HTTPS link over the backend's TLS client (`allnet.md` part 10, "HTTPS utility").

use std::net::Ipv4Addr;
use std::time::Duration;

use backend::tls::{AllnetTls, TlsError};

/// Connect and every send/receive wait (`0x1403FE050`).
const TIMEOUT: Duration = Duration::from_secs(60);

/// Bytes read per `read` call.
const CHUNK: usize = 4096;

/// One HTTPS exchange: connect (when no session is open), send the request, read the whole response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exchange {
    /// The auth-server address (`alpbExSetHostIpAddress` kind 1).
    pub address: Ipv4Addr,
    /// TCP port; 8443 for the accounting server.
    pub port: u16,
    /// `Host:` header and TLS server name (property `accounting-report.host`).
    pub host: String,
    /// The PEM trust anchor (`cacert.pem`, the keychip's own CA certificate).
    pub ca_pem: Vec<u8>,
    /// Open a new session first (the previous one was closed or never existed).
    pub new_session: bool,
    /// The complete HTTP request, head and body.
    pub request: Vec<u8>,
}

/// Why an exchange failed; the reporter maps these to its status codes and messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkError {
    /// The connection attempt timed out (60 s).
    ConnectTimeout,
    /// The TCP connection was refused or broke while connecting.
    ConnectFailed,
    /// The TLS handshake failed.
    Tls,
    /// The server certificate does not verify against `cacert.pem` (X509 error other than a date error).
    CertificateRejected,
    /// The server certificate is not yet valid or expired.
    CertificateDate,
    /// Send or receive timed out (60 s).
    Timeout,
    /// Send or receive failed.
    Io,
}

impl std::fmt::Debug for ReportLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReportLink").finish_non_exhaustive()
    }
}

/// The HTTPS client of the accounting reporter: one TLS session, reopened whenever an [`Exchange`] asks for a new
/// one.
pub struct ReportLink {
    session: Box<dyn AllnetTls>,
}

impl ReportLink {
    /// A link over `session`.
    pub fn new(session: Box<dyn AllnetTls>) -> Self {
        Self { session }
    }

    /// Performs one exchange and returns the raw HTTP response.
    pub fn exchange(&mut self, exchange: &Exchange) -> Result<Vec<u8>, LinkError> {
        if exchange.new_session {
            self.session.close();
            self.session
                .connect(exchange.address, exchange.port, &exchange.host, &exchange.ca_pem, TIMEOUT)
                .map_err(map)?;
        }
        self.session.write(&exchange.request).map_err(map)?;
        let mut response = Vec::new();
        let mut chunk = [0u8; CHUNK];
        loop {
            if complete_length(&response).is_some_and(|length| response.len() >= length) {
                return Ok(response);
            }
            match self.session.read(&mut chunk).map_err(map)? {
                0 if response.is_empty() => return Err(LinkError::Io),
                0 => return Ok(response),
                read => response.extend_from_slice(&chunk[..read]),
            }
        }
    }
}

fn map(error: TlsError) -> LinkError {
    match error {
        TlsError::ConnectTimeout => LinkError::ConnectTimeout,
        TlsError::ConnectFailed => LinkError::ConnectFailed,
        TlsError::Handshake => LinkError::Tls,
        TlsError::CertificateRejected => LinkError::CertificateRejected,
        TlsError::CertificateDate => LinkError::CertificateDate,
        TlsError::Timeout => LinkError::Timeout,
        TlsError::Closed | TlsError::Io => LinkError::Io,
    }
}

/// Total length of the HTTP response in `bytes` once the head and the `Content-Length` body are complete.
fn complete_length(bytes: &[u8]) -> Option<usize> {
    let split = bytes.windows(4).position(|window| window == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&bytes[..split]);
    let length = head.split("\r\n").skip(1).find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim().eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().ok()).flatten()
    });
    // Without a length the response is parsed (and rejected) as it is.
    Some(split + 4 + length.unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_is_complete_after_the_content_length_body() {
        let response = b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nabcd";
        assert_eq!(complete_length(response), Some(response.len()));
        assert_eq!(complete_length(&response[..response.len() - 1]), Some(response.len()));
        assert_eq!(complete_length(b"HTTP/1.1 200 OK\r\nContent-Le"), None);
    }
}
