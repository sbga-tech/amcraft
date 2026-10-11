//! The two POST bodies of the fetcher (`spec/libam/network.md` D.9.4): the signed-URL request and the state report.

use std::fmt::Write as _;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use backend::{HttpRequest, Method};

/// The signed-URL endpoint (`getSignedUrl`, `0x1402F0430`).
pub const SIGNED_URL_ENDPOINT: &str = "https://cf.auth.sys-all.net/net/cloudfront";
/// Marker in the answer that precedes the Base64 text of the signed URL.
const SIGNED_URL_MARKER: &str = "&signed_url=";
/// Most characters of the Base64 text that are used.
const SIGNED_URL_BASE64_MAX: usize = 1023;
/// Most bytes of the decoded URL.
const SIGNED_URL_MAX: usize = 1024;
/// The form content type of both POSTs.
const FORM: &str = "application/x-www-form-urlencoded";
/// Request timeout of the fetcher's HTTP client (60 s between attempts, `utilHttpDownload`).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// The POST that exchanges an image URL for a signed one: the body is not escaped.
pub fn signed_url_request(auth_data: &str, url: &str) -> HttpRequest {
    HttpRequest {
        method: Method::Post,
        url: SIGNED_URL_ENDPOINT.to_owned(),
        headers: vec![("Content-Type".to_owned(), FORM.to_owned())],
        body: format!("auth_data={auth_data}&uri={url}").into_bytes(),
        timeout: REQUEST_TIMEOUT,
    }
}

/// The URL in the answer of [`signed_url_request`]: the Base64 text after `&signed_url=`.
pub fn parse_signed_url(body: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(body);
    let start = text.find(SIGNED_URL_MARKER)? + SIGNED_URL_MARKER.len();
    let encoded: String =
        text[start..].chars().take_while(|c| !c.is_whitespace()).take(SIGNED_URL_BASE64_MAX).collect();
    let decoded = STANDARD.decode(encoded).ok()?;
    (decoded.len() <= SIGNED_URL_MAX).then(|| String::from_utf8(decoded).ok()).flatten()
}

/// Escapes a JSON string the way `0x1402EC3E0` does: `\b \t \n \f \r \" \/ \\`.
pub fn escape(text: &str, out: &mut String) {
    for c in text.chars() {
        match c {
            '\u{8}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            '"' => out.push_str("\\\""),
            '/' => out.push_str("\\/"),
            '\\' => out.push_str("\\\\"),
            c => out.push(c),
        }
    }
}

/// What one report carries (`0x1402EC530`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// The option-image report (`optimage`) rather than the application one (`appimage`).
    pub option: bool,
    /// Keychip serial (12 characters).
    pub serial: String,
    /// Record status of the kind.
    pub rf_state: u32,
    /// Authentication time (epoch seconds).
    pub auth_time: i64,
    /// DLI `ORDER_TIME` (epoch seconds).
    pub order_time: i64,
    /// DLI `RELEASE_TIME` (epoch seconds).
    pub release_time: i64,
    /// Record words 16 and 17.
    pub segments: (u32, u32),
    /// Newest application image waiting for release.
    pub dav: String,
    /// Newest application image otherwise.
    pub wdav: String,
    /// Newest OS image waiting for release.
    pub dov: String,
    /// Newest OS image otherwise.
    pub wdov: String,
    /// `GAME_DESC`.
    pub game_desc: String,
    /// File names of the images waiting for release.
    pub waiting_release: Vec<String>,
    /// File names of the other images.
    pub others: Vec<String>,
    /// The `auth_data` string, appended only when present.
    pub auth_data: String,
}

impl Report {
    /// The JSON text, without spaces or newlines.
    pub fn body(&self) -> String {
        let mut out = String::new();
        out.push_str(if self.option { "{\"optimage\":{" } else { "{\"appimage\":{" });
        out.push_str("\"serial\":\"");
        escape(&self.serial, &mut out);
        let auth_kind = if self.auth_time <= 0 { 1 } else { 2 };
        let _ = write!(
            out,
            "\",\"rf_state\":{},\"at\":{},\"as\":{},\"ot\":{},\"rt\":{},\"tsc\":{},\"tdsc\":{}",
            self.rf_state,
            self.auth_time,
            auth_kind,
            self.order_time,
            self.release_time,
            self.segments.0,
            self.segments.1
        );
        let mut version = |key: &str, value: &str| {
            let _ = write!(out, ",\"{key}\":\"");
            escape(value, &mut out);
            out.push('"');
        };
        version("dav", &self.dav);
        if !self.option {
            version("wdav", &self.wdav);
        }
        version("dov", &self.dov);
        if !self.option {
            version("wdov", &self.wdov);
        }
        version("gd", &self.game_desc);
        for (key, files) in [("dfl", &self.waiting_release), ("wfl", &self.others)] {
            let _ = write!(out, ",\"{key}\":[");
            for (index, file) in files.iter().enumerate() {
                if index != 0 {
                    out.push(',');
                }
                out.push('"');
                escape(file, &mut out);
                out.push('"');
            }
            out.push(']');
        }
        out.push('}');
        if !self.auth_data.is_empty() {
            out.push_str(",\"auth_data\":\"");
            escape(&self.auth_data, &mut out);
            out.push('"');
        }
        out.push('}');
        out
    }

    /// The POST request to `url`.
    pub fn request(&self, url: &str) -> HttpRequest {
        HttpRequest {
            method: Method::Post,
            url: url.to_owned(),
            headers: vec![("Content-Type".to_owned(), FORM.to_owned())],
            body: self.body().into_bytes(),
            timeout: REQUEST_TIMEOUT,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_url_is_base64_after_the_marker() {
        let encoded = STANDARD.encode("https://cdn.example/SDEZ_1.00.00.app?sig=1");
        let body = format!("result=ok&signed_url={encoded}\r\n");
        assert_eq!(parse_signed_url(body.as_bytes()).as_deref(), Some("https://cdn.example/SDEZ_1.00.00.app?sig=1"));
        assert_eq!(parse_signed_url(b"result=ng"), None);
    }

    #[test]
    fn app_report_body_has_no_whitespace_and_escapes_slashes() {
        let report = Report {
            serial: "A63E01A1234".into(),
            rf_state: 2,
            auth_time: 1_700_000_000,
            order_time: 10,
            release_time: 20,
            segments: (5, 3),
            dav: "1.02.03".into(),
            wdav: "0".into(),
            dov: "0".into(),
            wdov: "0".into(),
            game_desc: "a/b".into(),
            waiting_release: vec!["x.app".into()],
            others: vec![],
            auth_data: "tok".into(),
            ..Report::default()
        };
        assert_eq!(
            report.body(),
            "{\"appimage\":{\"serial\":\"A63E01A1234\",\"rf_state\":2,\"at\":1700000000,\"as\":2,\"ot\":10,\"rt\":20,\
             \"tsc\":5,\"tdsc\":3,\"dav\":\"1.02.03\",\"wdav\":\"0\",\"dov\":\"0\",\"wdov\":\"0\",\"gd\":\"a\\/b\",\
             \"dfl\":[\"x.app\"],\"wfl\":[]},\"auth_data\":\"tok\"}"
        );
    }

    #[test]
    fn option_report_omits_the_waiting_versions() {
        let report = Report { option: true, auth_time: 0, ..Report::default() };
        let body = report.body();
        assert!(body.starts_with("{\"optimage\":{"));
        assert!(!body.contains("wdav") && !body.contains("wdov") && !body.contains("auth_data"));
        assert!(body.contains("\"as\":1"));
    }
}
