//! The HTTP client interface, shared by the ALL.Net, network-delivery and ABaaS modules.
//!
//! The call blocks: modules run it on a worker thread and apply the answer at a frame boundary (contract:
//! worker threads). TLS with a custom trust bundle is [`TlsProvider`](super::TlsProvider), not this client.
//!
//! [`UreqHttp`] is the default implementation, blocking `ureq`: plain HTTP and HTTPS through `native-tls` with the
//! system root store (SChannel on Windows, like the original's WinHTTP). No proxy, no redirects and no automatic
//! decompression, like the original's transfers (`storage.md` 6.9: a 3xx is an answer).

use std::time::Duration;

/// HTTP method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// `GET`.
    Get,
    /// `POST`.
    Post,
    /// `PUT` (the sales-management-site status and counter reports).
    Put,
}

/// One request. Header order is the order of `headers`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    /// Method.
    pub method: Method,
    /// Absolute URL.
    pub url: String,
    /// Headers in wire order.
    pub headers: Vec<(String, String)>,
    /// Request body.
    pub body: Vec<u8>,
    /// Connect and total timeout.
    pub timeout: Duration,
}

/// The answer to a request that got an HTTP status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    /// Status code.
    pub status: u16,
    /// Headers as received.
    pub headers: Vec<(String, String)>,
    /// Response body.
    pub body: Vec<u8>,
}

/// A transfer that produced no HTTP status.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HttpError {
    /// The connection could not be established.
    #[error("could not connect")]
    Connect,
    /// The transfer timed out.
    #[error("operation timed out")]
    Timeout,
    /// Any other transfer error.
    #[error("transfer failed: {0}")]
    Transfer(String),
}

/// Blocking HTTP client.
pub trait HttpClient {
    /// Performs the request.
    fn request(&mut self, request: &HttpRequest) -> Result<HttpResponse, HttpError>;
}

/// Blocking HTTP(S) client on `ureq`: no proxy, no redirects (a 3xx is returned as the answer); HTTPS through the
/// system's TLS library with the system root store.
#[derive(Debug, Clone)]
pub struct UreqHttp {
    agent: ureq::Agent,
}

impl UreqHttp {
    /// A client with its own connection pool.
    pub fn new() -> Self {
        use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
        let tls = TlsConfig::builder().provider(TlsProvider::NativeTls).root_certs(RootCerts::PlatformVerifier).build();
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .proxy(None)
            .tls_config(tls)
            .build();
        Self { agent: ureq::Agent::new_with_config(config) }
    }
}

impl Default for UreqHttp {
    fn default() -> Self {
        Self::new()
    }
}

fn with_headers<B>(mut builder: ureq::RequestBuilder<B>, headers: &[(String, String)]) -> ureq::RequestBuilder<B> {
    for (name, value) in headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    builder
}

impl HttpClient for UreqHttp {
    fn request(&mut self, request: &HttpRequest) -> Result<HttpResponse, HttpError> {
        let timeout = Some(request.timeout);
        let mut response = match request.method {
            Method::Get => with_headers(self.agent.get(&request.url), &request.headers)
                .config()
                .timeout_connect(timeout)
                .timeout_global(timeout)
                .build()
                .call(),
            Method::Post => with_headers(self.agent.post(&request.url), &request.headers)
                .config()
                .timeout_connect(timeout)
                .timeout_global(timeout)
                .build()
                .send(&request.body[..]),
            Method::Put => with_headers(self.agent.put(&request.url), &request.headers)
                .config()
                .timeout_connect(timeout)
                .timeout_global(timeout)
                .build()
                .send(&request.body[..]),
        }
        .map_err(http_error)?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| (name.as_str().to_owned(), String::from_utf8_lossy(value.as_bytes()).into_owned()))
            .collect();
        let body = response.body_mut().with_config().limit(u64::MAX).read_to_vec().map_err(http_error)?;
        Ok(HttpResponse { status, headers, body })
    }
}

fn http_error(error: ureq::Error) -> HttpError {
    use std::io::ErrorKind;
    match error {
        ureq::Error::Timeout(_) => HttpError::Timeout,
        ureq::Error::HostNotFound | ureq::Error::ConnectionFailed => HttpError::Connect,
        ureq::Error::Io(io) => match io.kind() {
            ErrorKind::TimedOut | ErrorKind::WouldBlock => HttpError::Timeout,
            ErrorKind::ConnectionRefused
            | ErrorKind::HostUnreachable
            | ErrorKind::NetworkUnreachable
            | ErrorKind::AddrNotAvailable => HttpError::Connect,
            _ => HttpError::Transfer(io.to_string()),
        },
        other => HttpError::Transfer(other.to_string()),
    }
}
