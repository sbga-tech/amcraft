//! The ABaaS request/response cipher (`spec/libam/storage.md` 6.9.4-6.9.6).
//!
//! Hand-written only where specific to this binary: the key mask, the 16-byte header (which is also the CBC IV) and
//! the 1.05x output bound of the compressor. zlib, AES, SHA-256 and CRC-32 come from the usual crates.

use std::io::{Read, Write};

use aes::Aes256;
use aes::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit, block_padding::NoPadding, block_padding::Pkcs7};
use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use sha2::{Digest, Sha256};

/// The mask of `0x1403684F0`: `key = SHA256(GAMEID) XOR K`.
const MASK: [u8; 32] = [
    0x58, 0xB6, 0x7D, 0x7E, 0x2C, 0xAA, 0xF2, 0xCF, 0xCC, 0xAC, 0x73, 0xEA, 0xC4, 0x05, 0x86, 0x90, 0x8A, 0xFA, 0x9A,
    0x67, 0x59, 0x73, 0x72, 0x25, 0xBA, 0xEA, 0x4B, 0x99, 0xAD, 0x52, 0x13, 0xBE,
];

/// Length of the request and response header.
pub const HEADER_LEN: usize = 16;

/// Why a request could not be encoded or a response not decoded; each is a lib message of the original.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WireError {
    /// `cannot compress`: the zlib stream exceeds 1.05 x the JSON length (or the encoder failed).
    #[error("cannot compress")]
    Compress,
    /// `response header size`: fewer than 16 bytes.
    #[error("response header size")]
    ResponseHeader,
    /// `response code`: the server header carries a non-zero code.
    #[error("response code {0}")]
    ResponseCode(u16),
    /// `cannot decrypt`.
    #[error("cannot decrypt")]
    Decrypt,
    /// `cannot uncompress`.
    #[error("cannot uncompress")]
    Uncompress,
    /// `cannot get crc` / `ERROR: CRC`.
    #[error("cannot get crc")]
    Crc,
}

/// `key[32]`: the AES-256 key of one game id (upper-case, four characters).
pub fn derive_key(game_id: &str) -> [u8; 32] {
    let mut key: [u8; 32] = Sha256::digest(game_id.as_bytes()).into();
    for (byte, mask) in key.iter_mut().zip(MASK) {
        *byte ^= mask;
    }
    key
}

/// One request body (6.9.5): `H || AES-256-CBC(key, IV = H, PKCS#7(zlib(json)))`. `nonce` is `rand_s()`.
pub fn encode_request(
    key: &[u8; 32],
    version: (u8, u8),
    game_id: &str,
    json: &[u8],
    nonce: u32,
) -> Result<Vec<u8>, WireError> {
    let mut header = [0u8; HEADER_LEN];
    header[0] = 2;
    header[1] = 1;
    header[2] = version.0;
    header[3] = version.1;
    for (slot, byte) in header[4..8].iter_mut().zip(game_id.bytes()) {
        *slot = byte;
    }
    header[8..12].copy_from_slice(&crc32fast::hash(json).to_be_bytes());
    header[12..16].copy_from_slice(&nonce.to_ne_bytes());

    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::new(6));
    encoder.write_all(json).map_err(|_| WireError::Compress)?;
    let compressed = encoder.finish().map_err(|_| WireError::Compress)?;
    // The output buffer is `(u32)(len * 1.05)` bytes; a longer stream is Z_BUF_ERROR.
    let bound = (json.len() as f64 * 1.05) as u32 as usize;
    if compressed.len() > bound {
        return Err(WireError::Compress);
    }
    let cipher = cbc::Encryptor::<Aes256>::new(key.into(), &header.into());
    let mut body = header.to_vec();
    body.extend(cipher.encrypt_padded_vec_mut::<Pkcs7>(&compressed));
    Ok(body)
}

/// One response body (6.9.6) to the JSON text it carries; empty when the server says the plain length is 0.
pub fn decode_response(key: &[u8; 32], body: &[u8]) -> Result<Vec<u8>, WireError> {
    if body.len() < HEADER_LEN {
        return Err(WireError::ResponseHeader);
    }
    let code = u16::from_be_bytes([body[2], body[3]]);
    if code != 0 {
        return Err(WireError::ResponseCode(code));
    }
    let plain_len = u32::from_be_bytes([body[8], body[9], body[10], body[11]]) as usize;
    if plain_len == 0 {
        return Ok(Vec::new());
    }
    let iv: [u8; HEADER_LEN] = body[..HEADER_LEN].try_into().map_err(|_| WireError::ResponseHeader)?;
    let mut data = body[HEADER_LEN..].to_vec();
    cbc::Decryptor::<Aes256>::new(key.into(), &iv.into())
        .decrypt_padded_mut::<NoPadding>(&mut data)
        .map_err(|_| WireError::Decrypt)?;
    let pad = usize::from(*data.last().ok_or(WireError::Decrypt)?);
    if pad > 16 || pad > data.len() {
        return Err(WireError::Decrypt);
    }
    data.truncate(data.len() - pad);
    let mut json = Vec::new();
    ZlibDecoder::new(data.as_slice())
        .take(plain_len as u64 + 1)
        .read_to_end(&mut json)
        .map_err(|_| WireError::Uncompress)?;
    if json.len() > plain_len {
        return Err(WireError::Uncompress);
    }
    if crc32fast::hash(&json).to_be_bytes() != body[4..8] {
        return Err(WireError::Crc);
    }
    Ok(json)
}

/// A server answer to `json` (what the real server sends back); used by the tests to script the simulated HTTP.
#[cfg(test)]
pub fn encode_response(key: &[u8; 32], code: u16, json: &[u8]) -> Vec<u8> {
    encode_response_with_crc(key, code, crc32fast::hash(json), json)
}

/// [`encode_response`] with an explicit CRC field (a damaged answer).
#[cfg(test)]
pub fn encode_response_with_crc(key: &[u8; 32], code: u16, crc: u32, json: &[u8]) -> Vec<u8> {
    let mut header = [0u8; HEADER_LEN];
    header[2..4].copy_from_slice(&code.to_be_bytes());
    header[4..8].copy_from_slice(&crc.to_be_bytes());
    header[8..12].copy_from_slice(&(json.len() as u32).to_be_bytes());
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::new(6));
    encoder.write_all(json).unwrap();
    let compressed = encoder.finish().unwrap();
    let mut body = header.to_vec();
    body.extend(cbc::Encryptor::<Aes256>::new(key.into(), &header.into()).encrypt_padded_vec_mut::<Pkcs7>(&compressed));
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_the_masked_sha256_of_the_game_id() {
        let digest: [u8; 32] = Sha256::digest(b"SDEY").into();
        let key = derive_key("SDEY");
        assert_eq!(key[0], digest[0] ^ 0x58);
        assert_eq!(key[31], digest[31] ^ 0xBE);
    }

    #[test]
    fn request_header_and_cipher_round_trip() {
        let key = derive_key("SDEY");
        let json = br#"{"client":{"serial":"A","main_id":"B"}}"#.repeat(8);
        let wire = encode_request(&key, (1, 5), "SDEY", &json, 0x0102_0304).unwrap();
        assert_eq!(&wire[..8], &[2, 1, 1, 5, b'S', b'D', b'E', b'Y']);
        assert_eq!(&wire[8..12], &crc32fast::hash(&json).to_be_bytes());
        assert_eq!(&wire[12..16], &0x0102_0304u32.to_ne_bytes());
        assert_eq!((wire.len() - HEADER_LEN) % 16, 0);
        // The same bytes decrypt with the header as IV.
        let iv: [u8; 16] = wire[..16].try_into().unwrap();
        let mut ct = wire[16..].to_vec();
        let plain =
            cbc::Decryptor::<Aes256>::new(&key.into(), &iv.into()).decrypt_padded_mut::<Pkcs7>(&mut ct).unwrap();
        let mut inflated = Vec::new();
        ZlibDecoder::new(plain).read_to_end(&mut inflated).unwrap();
        assert_eq!(inflated, json);
    }

    #[test]
    fn response_decoding_checks_header_code_and_crc() {
        let key = derive_key("SDEY");
        let json = br#"{"settings":{"interval":1800}}"#;
        assert_eq!(decode_response(&key, &encode_response(&key, 0, json)).unwrap(), json);
        assert_eq!(decode_response(&key, &[0; 15]), Err(WireError::ResponseHeader));
        assert_eq!(decode_response(&key, &encode_response(&key, 7, json)), Err(WireError::ResponseCode(7)));
        let bad = encode_response_with_crc(&key, 0, crc32fast::hash(json) ^ 1, json);
        assert_eq!(decode_response(&key, &bad), Err(WireError::Crc));
        let mut empty = [0u8; 16];
        empty[8..12].copy_from_slice(&0u32.to_be_bytes());
        assert_eq!(decode_response(&key, &empty).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn incompressible_json_exceeds_the_compressor_bound() {
        let key = derive_key("SDEY");
        // 100 bytes of xorshift noise: the stored-block overhead alone (11 bytes) exceeds 5 % of the input.
        let mut state = 0x2545_F491_u32;
        let noise: Vec<u8> = (0..100)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect();
        assert_eq!(encode_request(&key, (1, 0), "SDEY", &noise, 0), Err(WireError::Compress));
    }
}
