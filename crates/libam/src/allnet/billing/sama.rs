//! SAMA: verification of the signatures the server attaches to `playlimit=` and `nearfull=` (`allnet.md` part 10,
//! section 3). The signed message is `SHA-1(LE32(value) || keychip serial[0..11])`; the keychip's RSA public key
//! checks the PKCS#1 v1.5 signature.

use rsa::pkcs8::DecodePublicKey as _;
use rsa::{Pkcs1v15Sign, RsaPublicKey};
use sha1::{Digest as _, Sha1};

/// DigestInfo prefix of SHA-1 (15 bytes; with the 20-byte digest the decrypted block is 35 bytes).
const SHA1_PREFIX: [u8; 15] =
    [0x30, 0x21, 0x30, 0x09, 0x06, 0x05, 0x2B, 0x0E, 0x03, 0x02, 0x1A, 0x05, 0x00, 0x04, 0x14];

/// Why a signature was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SamaError {
    /// Hex text of the wrong length or with a non-hex character, or an unusable key (`-1`).
    Failed,
    /// The digest does not match (`-7`).
    Mismatch,
}

/// The 20-byte digest of `value` and the first 11 characters of `serial`.
pub fn message_digest(value: u32, serial: &str) -> [u8; 20] {
    let mut message = Vec::with_capacity(15);
    message.extend_from_slice(&value.to_le_bytes());
    message.extend(serial.bytes().chain(std::iter::repeat(0)).take(11));
    Sha1::digest(&message).into()
}

/// Decodes the 256-character hex signature into 128 bytes.
pub fn decode_signature(hex: &str) -> Result<Vec<u8>, SamaError> {
    if hex.len() != 256 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(SamaError::Failed);
    }
    (0..hex.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).map_err(|_| SamaError::Failed))
        .collect()
}

/// Verifies `signature_hex` over `value` for the keychip `serial` with the DER SubjectPublicKeyInfo `public_key`.
pub fn verify(public_key: &[u8], serial: &str, value: u32, signature_hex: &str) -> Result<(), SamaError> {
    let signature = decode_signature(signature_hex)?;
    let key = RsaPublicKey::from_public_key_der(public_key).map_err(|_| SamaError::Failed)?;
    let scheme = Pkcs1v15Sign { hash_len: Some(20), prefix: SHA1_PREFIX.to_vec().into_boxed_slice() };
    key.verify(scheme, &message_digest(value, serial), &signature).map_err(|_| SamaError::Mismatch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_covers_the_little_endian_value_and_eleven_serial_characters() {
        let expected: [u8; 20] = Sha1::digest(b"\x01\x00\x00\x00ABCD1234567").into();
        assert_eq!(message_digest(1, "ABCD12345678"), expected);
    }

    #[test]
    fn short_or_non_hex_signatures_fail() {
        assert_eq!(decode_signature("00"), Err(SamaError::Failed));
        assert_eq!(decode_signature(&"g".repeat(256)), Err(SamaError::Failed));
        assert_eq!(decode_signature(&"0f".repeat(128)).map(|bytes| bytes.len()), Ok(128));
    }
}
