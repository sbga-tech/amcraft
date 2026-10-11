//! The environment of the e-money module (`spec/libam/payment.md` PayEmSvc section 4): the resource directory,
//! `env.json` and the obfuscated PKCS#12 password.

use md5::{Digest, Md5};
use serde_json::Value;

/// The 21 MD5 digests of `resource.xml` accepted in `%SEGA_AM_LOG%\tfps-res\` (4.5, table `0x1406C4910`).
const RESOURCE_DIGESTS: [&str; 21] = [
    "f9f43f54b29e03dfc9aba7b60ab8849d",
    "0a34318e15fca0738c82b327f7b57785",
    "99f594914cd17d4393d19a456c773747",
    "a89789444ebdf85e94095da0c43b04f5",
    "063d48d72476c504173b7e968af15f45",
    "741ca8ee87910be8df41c58a2814eaa1",
    "f8283a7516e8d37d34e7c93b7dfcf8e3",
    "144eac816d6210a7e0adf0725b36f4a8",
    "6459c5739b9d98786344a88446ed1b1f",
    "11919df0e3708068d85038ead6919d93",
    "95a0cc29a97fb239180fe9560d3ae106",
    "f3242611d9d0f246e10b5e96094e7ec6",
    "bb65b93f1583f75c21907d58fd840a08",
    "8a05a63e4717f5e47ed3987e23cb2075",
    "1b99dec6b70e560d1d6dc484244fefdd",
    "2ec356b63a1c645e9975d342f61577a2",
    "8edbac92119f0582227639ce3e2049b5",
    "cb81da6237ca9faccbd58a70fe14a239",
    "ad4eb0ecf8c02ec65e6a8ea4dd047963",
    "dba2efd26da454176a89c294843eda64",
    "9fc7075d6ef3818d3a640b314c924c9e",
];

/// Capacity of the URL field of the environment record (wchar[128]).
const URL_UNITS: usize = 127;

/// The environment record `E` (`M+28000`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Env {
    /// `tasms.root_endpoint`: the activation URL.
    pub root_endpoint: String,
    /// `thincapayment.ca_file`, de-obfuscated: the PKCS#12 password.
    pub password: String,
    /// `tasms.use_proxy`: selects the store-card type (3 instead of 2) and the TASMS flag.
    pub use_proxy: bool,
}

/// Why `env.json` was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum EnvError {
    /// Not JSON, or a mandatory object is missing or has another type.
    #[error("env.json is not valid")]
    Invalid,
}

/// `0x140273560`: parses the environment file (at most 4096 bytes are read).
pub fn parse_env(bytes: &[u8]) -> Result<Env, EnvError> {
    let bytes = &bytes[..bytes.len().min(4096)];
    let root: Value = serde_json::from_slice(bytes).map_err(|_| EnvError::Invalid)?;
    let tasms = root.get("tasms").and_then(Value::as_object).ok_or(EnvError::Invalid)?;
    let thinca = root.get("thincapayment").and_then(Value::as_object).ok_or(EnvError::Invalid)?;
    let root_endpoint = tasms.get("root_endpoint").and_then(Value::as_str).ok_or(EnvError::Invalid)?;
    if root_endpoint.encode_utf16().count() > URL_UNITS {
        return Err(EnvError::Invalid);
    }
    let use_proxy = match tasms.get("use_proxy") {
        None => false,
        Some(Value::Bool(flag)) => *flag,
        Some(_) => return Err(EnvError::Invalid),
    };
    let stored = thinca.get("ca_file").and_then(Value::as_str).ok_or(EnvError::Invalid)?;
    if stored.len() >= 16 {
        return Err(EnvError::Invalid);
    }
    let password = unshift(stored).ok_or(EnvError::Invalid)?;
    Ok(Env { root_endpoint: root_endpoint.to_owned(), password, use_proxy })
}

/// 4.3: `plain = ((c - 32 + 85) mod 95) + 32`; a character outside `0x20..=0x7E` rejects the text.
fn unshift(stored: &str) -> Option<String> {
    stored.bytes().map(|c| (0x20..=0x7E).contains(&c).then(|| char::from((c - 32 + 85) % 95 + 32))).collect()
}

#[cfg(test)]
/// The inverse of [`unshift`], used to write fixtures.
pub fn shift(plain: &str) -> String {
    plain.bytes().map(|c| char::from((c - 32 + 10) % 95 + 32)).collect()
}

/// Lower-case hexadecimal MD5 of `data`.
pub fn md5_hex(data: &[u8]) -> String {
    Md5::digest(data).iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 4.5: `resource.xml` is on the whitelist.
pub fn resource_xml_allowed(bytes: &[u8]) -> bool {
    let digest = md5_hex(bytes);
    RESOURCE_DIGESTS.contains(&digest.as_str())
}

/// 4.1 (`0x14027E910`): `%SEGA_AM_LOG%\tfps-res\` when it holds a whitelisted `resource.xml`, else the configured
/// resource path, else `.`; a trailing backslash is ensured. `read` returns the bytes of a cabinet path.
pub fn resource_directory(
    sega_am_log: Option<&str>,
    configured: &str,
    read: impl Fn(&str) -> Option<Vec<u8>>,
) -> String {
    let mut dir = String::new();
    if let Some(base) = sega_am_log.filter(|base| !base.is_empty()) {
        let candidate = format!("{}\\tfps-res\\", base.trim_end_matches('\\'));
        if read(&format!("{candidate}resource.xml")).is_some_and(|bytes| resource_xml_allowed(&bytes)) {
            dir = candidate;
        }
    }
    if dir.is_empty() {
        dir = if configured.is_empty() { ".".to_owned() } else { configured.to_owned() };
    }
    if !dir.ends_with('\\') {
        dir.push('\\');
    }
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_round_trips_through_the_shift_cipher() {
        assert_eq!(unshift(&shift("p@ss Word~")).as_deref(), Some("p@ss Word~"));
        assert_eq!(shift("~"), ")");
    }

    #[test]
    fn env_requires_both_objects_and_a_boolean_proxy_flag() {
        let ok = format!(
            r#"{{"tasms":{{"root_endpoint":"http://x/a","use_proxy":true}},"thincapayment":{{"ca_file":"{}"}}}}"#,
            shift("secret")
        );
        assert_eq!(
            parse_env(ok.as_bytes()),
            Ok(Env { root_endpoint: "http://x/a".into(), password: "secret".into(), use_proxy: true })
        );
        assert!(parse_env(br#"{"tasms":{"root_endpoint":"u"}}"#).is_err());
        assert!(parse_env(br#"{"tasms":{"root_endpoint":"u","use_proxy":1},"thincapayment":{"ca_file":""}}"#).is_err());
    }
}
