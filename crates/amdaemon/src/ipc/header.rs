//! Application-level daemon message framing (`DaemonMessageHeader`).
//!
//! Every plaintext message exchanged on a daemon Messenger starts with a 16-byte header followed by
//! `body_size` bytes. Evidence: writers `0x140076C10`/`0x140076C90`, hello `0x140076580`, decoder
//! `0x14007CAA0`.

use crate::schema::image::{CLIENT_DATA_SIZE, SERVER_DATA_SIZE};

/// Size of the application header.
pub const HEADER_LEN: usize = 16;

/// Hello body: u64 `0x0001A5E8001A8801` (`0x1405F2410`).
///
/// Interpreted as `ClientData size << 8 | 1` in the low half and `ServerData size` in the high half.
pub const HELLO_TOKEN: u64 = 0x0001_A5E8_001A_8801;

/// Size of the hello body.
pub const HELLO_BODY_LEN: u32 = 8;

const _: () = assert!(HELLO_TOKEN == ((SERVER_DATA_SIZE as u64) << 32) | ((CLIENT_DATA_SIZE as u64) << 8) | 1);

/// Message kind (`u32` at header offset 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum Kind {
    /// Shape token handshake.
    Hello = 0,
    /// Leading `ServerData` prefix.
    ServerData = 1,
    /// Leading `ClientData` prefix.
    ClientData = 2,
    /// `ClientData` prefix followed by `ServerData` prefix.
    Combined = 3,
}

impl Kind {
    /// Maps a raw header kind.
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            0 => Some(Self::Hello),
            1 => Some(Self::ServerData),
            2 => Some(Self::ClientData),
            3 => Some(Self::Combined),
            _ => None,
        }
    }
}

/// Parsed header. `client_size`/`server_size` are only meaningful for [`Kind::Combined`]; for other kinds
/// the original writers leave whatever the reused send buffer held in those bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// Raw kind.
    pub kind: u32,
    /// Bytes following the header.
    pub body_size: u32,
    /// Combined: client prefix length.
    pub client_size: u32,
    /// Combined: server prefix length.
    pub server_size: u32,
}

impl Header {
    /// Parses the first 16 bytes of `message`.
    pub fn parse(message: &[u8]) -> Option<Self> {
        let raw: &[u8; HEADER_LEN] = message.get(..HEADER_LEN)?.try_into().ok()?;
        let word = |i: usize| u32::from_le_bytes([raw[i], raw[i + 1], raw[i + 2], raw[i + 3]]);
        Some(Self { kind: word(0), body_size: word(4), client_size: word(8), server_size: word(12) })
    }
}

/// Reason a message body was not applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// Plaintext shorter than the header.
    #[error("message shorter than the 16-byte header")]
    Truncated,
    /// Header kind is not a root-carrying kind.
    #[error("unsupported message kind {0}")]
    UnsupportedKind(u32),
    /// Sizes violate the decoder rules of `0x14007CAA0`.
    #[error("message sizes violate the root bounds")]
    BadSize,
    /// `body_size` exceeds the bytes actually received. The original `memcpy`s past the buffer here; this
    /// implementation rejects the message instead.
    #[error("body_size exceeds received plaintext")]
    BodyOverrun,
}

/// Applies a kind 1/2/3 message to the root images (`0x14007CAA0`).
///
/// `strict` requires full-root sizes (used by the client for the initial Combined image).
pub fn decode_into(message: &[u8], server: &mut [u8], client: &mut [u8], strict: bool) -> Result<Kind, DecodeError> {
    let header = Header::parse(message).ok_or(DecodeError::Truncated)?;
    let body = &message[HEADER_LEN..];
    let body_size = header.body_size as usize;
    if body.len() < body_size {
        return Err(DecodeError::BodyOverrun);
    }
    match Kind::from_raw(header.kind) {
        Some(Kind::ServerData) => {
            if body_size > SERVER_DATA_SIZE || (strict && body_size != SERVER_DATA_SIZE) {
                return Err(DecodeError::BadSize);
            }
            server[..body_size].copy_from_slice(&body[..body_size]);
            Ok(Kind::ServerData)
        }
        Some(Kind::ClientData) => {
            if body_size > CLIENT_DATA_SIZE || (strict && body_size != CLIENT_DATA_SIZE) {
                return Err(DecodeError::BadSize);
            }
            client[..body_size].copy_from_slice(&body[..body_size]);
            Ok(Kind::ClientData)
        }
        Some(Kind::Combined) => {
            let client_size = header.client_size as usize;
            let server_size = header.server_size as usize;
            let sizes_ok = client_size <= CLIENT_DATA_SIZE
                && server_size <= SERVER_DATA_SIZE
                && body_size == client_size + server_size
                && (!strict || (client_size == CLIENT_DATA_SIZE && server_size == SERVER_DATA_SIZE));
            if !sizes_ok {
                return Err(DecodeError::BadSize);
            }
            client[..client_size].copy_from_slice(&body[..client_size]);
            server[..server_size].copy_from_slice(&body[client_size..body_size]);
            Ok(Kind::Combined)
        }
        Some(Kind::Hello) | None => Err(DecodeError::UnsupportedKind(header.kind)),
    }
}

/// Reused send vector (`std::vector<u8>` at `Impl+0x36678` / `+0x52718`).
///
/// Resizing keeps existing bytes and zero-fills growth (`0x140024250`), so header bytes `8..16` of kinds
/// 0–2 carry the previous Combined sizes exactly like the original.
#[derive(Debug, Clone)]
pub struct SendBuffer {
    bytes: Vec<u8>,
}

/// Initial size of each send vector (`0x14006C6B0`).
pub const SEND_BUFFER_INITIAL_LEN: usize = 0x1C080;

impl Default for SendBuffer {
    fn default() -> Self {
        Self { bytes: vec![0; SEND_BUFFER_INITIAL_LEN] }
    }
}

impl SendBuffer {
    /// Current message bytes.
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    fn put_u32(&mut self, offset: usize, value: u32) {
        self.bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    /// Writes the 24-byte hello (`0x140076580` phase 0).
    pub fn write_hello(&mut self) -> &[u8] {
        self.bytes.resize(HEADER_LEN + HELLO_BODY_LEN as usize, 0);
        self.put_u32(0, Kind::Hello as u32);
        self.put_u32(4, HELLO_BODY_LEN);
        self.bytes[HEADER_LEN..].copy_from_slice(&HELLO_TOKEN.to_le_bytes());
        &self.bytes
    }

    /// Writes a single-root prefix message (`0x140076C10`).
    pub fn write_single(&mut self, kind: Kind, body: &[u8]) -> &[u8] {
        self.bytes.resize(HEADER_LEN + body.len(), 0);
        self.put_u32(0, kind as u32);
        self.put_u32(4, len_u32(body.len()));
        self.bytes[HEADER_LEN..].copy_from_slice(body);
        &self.bytes
    }

    /// Writes a Combined message (`0x140076C90`): client prefix then server prefix.
    pub fn write_combined(&mut self, client: &[u8], server: &[u8]) -> &[u8] {
        let total = client.len() + server.len();
        self.bytes.resize(HEADER_LEN + total, 0);
        self.put_u32(0, Kind::Combined as u32);
        self.put_u32(4, len_u32(total));
        self.put_u32(8, len_u32(client.len()));
        self.put_u32(12, len_u32(server.len()));
        self.bytes[HEADER_LEN..HEADER_LEN + client.len()].copy_from_slice(client);
        self.bytes[HEADER_LEN + client.len()..].copy_from_slice(server);
        &self.bytes
    }
}

fn len_u32(len: usize) -> u32 {
    // Bodies are bounded by the root sizes (< 0x1C070 bytes), so this conversion is lossless.
    u32::try_from(len).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_matches_recovered_bytes() {
        let mut buf = SendBuffer::default();
        let hello = buf.write_hello();
        assert_eq!(
            hello,
            &[0, 0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01, 0x88, 0x1A, 0x00, 0xE8, 0xA5, 0x01, 0x00]
        );
    }
}
