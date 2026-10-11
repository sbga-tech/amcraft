//! The Aime DB wire format (`spec/libam/aime.md` B.2 to B.4): the 32-byte header, the frame size table, the
//! AES-128-ECB cipher and the request/response layouts. Pure functions; the client in `db.rs` and the server
//! emulator in `mock/aime_db.rs` share them.
//!
//! The cipher is the `aes` + `ecb` crates (B.2: standard Rijndael, key `Copyright(C)SEGA`, no IV, no MAC). The
//! framing and the size table are specific to this protocol, so they are written here.

use aes::Aes128;
use ecb::cipher::{BlockDecryptMut, BlockEncryptMut, KeyInit, block_padding::NoPadding};

use super::{InternalCode, RawCampaignClear, RawCampaignInfo, flags};

/// Header magic (bytes `3E A1`).
pub const MAGIC: u16 = 0xA13E;
/// Header version (bytes `23 40`).
pub const VERSION: u16 = 0x4023;
/// The AES key: the first 16 bytes of `Copyright(C)SEGA` (`0x14035ED80`).
pub const KEY: &[u8; 16] = b"Copyright(C)SEGA";
/// Header length.
pub const HEADER_LEN: usize = 32;
/// Receive buffer of the client (`recv(len-1)` into 1024 bytes).
pub const RECV_BUFFER: usize = 1024;

/// Request and response commands (B.4).
pub mod cmd {
    /// FeliCa lookup.
    pub const FELICA_LOOKUP: u16 = 1;
    /// FeliCa register.
    pub const FELICA_REGISTER: u16 = 2;
    /// Answer to 1 and 2.
    pub const FELICA_REPLY: u16 = 3;
    /// Aime lookup.
    pub const AIME_LOOKUP: u16 = 4;
    /// Aime register.
    pub const AIME_REGISTER: u16 = 5;
    /// Answer to 4 and 5.
    pub const AIME_REPLY: u16 = 6;
    /// Log status.
    pub const LOG_STATUS: u16 = 7;
    /// Answer to 7.
    pub const LOG_STATUS_REPLY: u16 = 8;
    /// Single play log.
    pub const SINGLE_LOG: u16 = 9;
    /// Answer to 9.
    pub const SINGLE_LOG_REPLY: u16 = 10;
    /// Campaign info.
    pub const CAMPAIGN_INFO: u16 = 11;
    /// Answer to 11.
    pub const CAMPAIGN_INFO_REPLY: u16 = 12;
    /// Campaign clear info.
    pub const CAMPAIGN_CLEAR: u16 = 13;
    /// Answer to 13.
    pub const CAMPAIGN_CLEAR_REPLY: u16 = 14;
    /// Aime lookup 2 (access code to SEGA ID).
    pub const AIME_LOOKUP2: u16 = 15;
    /// Answer to 15.
    pub const AIME_LOOKUP2_REPLY: u16 = 16;
    /// Lite-S lookup.
    pub const LITE_S_LOOKUP: u16 = 17;
    /// Answer to 17.
    pub const LITE_S_REPLY: u16 = 18;
    /// Multi play log.
    pub const MULTI_LOG: u16 = 19;
    /// Answer to 19.
    pub const MULTI_LOG_REPLY: u16 = 20;
    /// Alive check.
    pub const ALIVE: u16 = 100;
    /// Answer to 100.
    pub const ALIVE_REPLY: u16 = 101;
    /// Goodbye, sent before every close and never answered.
    pub const GOODBYE: u16 = 102;
}

/// The frame size table (`0x14035F0A0`, B.3); 0 for an unknown command.
pub const fn frame_size(command: u16) -> usize {
    match command {
        1..=7 | 11 | 13 | 15 => 48,
        8 | 10 | 100..=102 => 32,
        9 | 20 => 64,
        12 => 512,
        14 => 80,
        16 => 304,
        17 => 112,
        18 => 320,
        19 => 1008,
        _ => 0,
    }
}

/// The identity fields of the request header (`db+8`, `db+12`, `db+20`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbParams {
    /// Game id, 4 characters and a NUL (`db+12`).
    pub game_id: [u8; 5],
    /// Serial, 11 characters (`db+20`).
    pub serial: [u8; 11],
    /// Board argument (`db+8`).
    pub board_arg: u32,
    /// Reader firmware kind (`db+977`), sent in the Aime lookups.
    pub fw_kind: u8,
}

impl DbParams {
    /// Builds the parameters from the strings `setBoardInfo` stores (`strncpy` into zeroed buffers).
    pub fn new(game_id: &str, serial: &str, board_arg: u32, fw_kind: u8) -> Self {
        let mut params = Self { game_id: [0; 5], serial: [0; 11], board_arg, fw_kind };
        let id = game_id.as_bytes();
        let id_len = id.len().min(5);
        params.game_id[..id_len].copy_from_slice(&id[..id_len]);
        let sn = serial.as_bytes();
        let sn_len = sn.len().min(11);
        params.serial[..sn_len].copy_from_slice(&sn[..sn_len]);
        params
    }
}

/// The decoded 32-byte header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// `cmd` at 4.
    pub cmd: u16,
    /// `len` at 6.
    pub len: u16,
    /// `status` at 8.
    pub status: u16,
}

impl Header {
    /// Reads `cmd`, `len` and `status`; the other fields are not checked by the client (B.2).
    pub fn parse(frame: &[u8]) -> Option<Self> {
        if frame.len() < HEADER_LEN {
            return None;
        }
        Some(Self {
            cmd: u16::from_le_bytes([frame[4], frame[5]]),
            len: u16::from_le_bytes([frame[6], frame[7]]),
            status: u16::from_le_bytes([frame[8], frame[9]]),
        })
    }
}

/// Builds a plaintext frame of `cmd` (size from the table, zero filled) with the header of `params`; `status` is
/// 0 in requests and the result in responses.
pub fn build_frame(params: &DbParams, command: u16, status: u16, payload: &[u8]) -> Vec<u8> {
    let size = frame_size(command);
    let mut frame = vec![0u8; size.max(HEADER_LEN + payload.len())];
    frame[0..2].copy_from_slice(&MAGIC.to_le_bytes());
    frame[2..4].copy_from_slice(&VERSION.to_le_bytes());
    frame[4..6].copy_from_slice(&command.to_le_bytes());
    frame[6..8].copy_from_slice(&(size as u16).to_le_bytes());
    frame[8..10].copy_from_slice(&status.to_le_bytes());
    frame[10..15].copy_from_slice(&params.game_id);
    frame[16..20].copy_from_slice(&params.board_arg.to_le_bytes());
    frame[20..31].copy_from_slice(&params.serial);
    frame[HEADER_LEN..HEADER_LEN + payload.len()].copy_from_slice(payload);
    frame
}

/// Zero-pads to a multiple of 16 and encrypts block by block (`0x140363B60`).
pub fn encrypt(frame: &[u8]) -> Vec<u8> {
    let mut buffer = frame.to_vec();
    buffer.resize(frame.len().next_multiple_of(16), 0);
    let length = buffer.len();
    let cipher = ecb::Encryptor::<Aes128>::new(KEY.into());
    // The buffer is a multiple of the block size, so no padding is ever written.
    cipher.encrypt_padded_mut::<NoPadding>(&mut buffer, length).expect("whole blocks");
    buffer
}

/// Decrypts whole 16-byte blocks in place (`0x140363510`); a trailing partial block is left untouched.
pub fn decrypt(data: &mut [u8]) {
    let whole = data.len() / 16 * 16;
    let cipher = ecb::Decryptor::<Aes128>::new(KEY.into());
    cipher.decrypt_padded_mut::<NoPadding>(&mut data[..whole]).expect("whole blocks");
}

fn u32_at(frame: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(frame[offset..offset + 4].try_into().expect("four bytes"))
}

fn i32_at(frame: &[u8], offset: usize) -> i32 {
    u32_at(frame, offset) as i32
}

// ---------------------------------------------------------------------------------------------------------
// Requests (B.4)
// ---------------------------------------------------------------------------------------------------------

/// The Aime lookup/register payload: access code at 32, `R+168` at 42, the firmware kind at 43, `R+140` at 44.
pub fn aime_payload(access_code: &[u8; 10], aux: u8, fw_kind: u8, card_code: [u8; 4]) -> [u8; 16] {
    let mut payload = [0u8; 16];
    payload[0..10].copy_from_slice(access_code);
    payload[10] = aux;
    payload[11] = fw_kind;
    payload[12..16].copy_from_slice(&card_code);
    payload
}

/// The Lite-S payload (cmd 17): `R+27..R+99` blocks, `R+168`, the firmware kind and the two DFC bytes.
pub fn lite_s_payload(extra: &[u8; 75], aux: u8, fw_kind: u8) -> [u8; 76] {
    let mut payload = [0u8; 76];
    payload[0..72].copy_from_slice(&extra[1..73]);
    payload[72] = aux;
    payload[73] = fw_kind;
    payload[74] = extra[73];
    payload[75] = extra[74];
    payload
}

/// The FeliCa payload: IDm and PMm (`R+10..26`).
pub fn felica_payload(idm: &[u8; 8], pmm: &[u8; 8]) -> [u8; 16] {
    let mut payload = [0u8; 16];
    payload[0..8].copy_from_slice(idm);
    payload[8..16].copy_from_slice(pmm);
    payload
}

/// The multi-log payload: the entries from 32, the count at 992 (so at payload offset 960).
pub fn multi_log_payload(entries: &[[u8; 48]]) -> Vec<u8> {
    let mut payload = vec![0u8; 976];
    for slot in 0..20 {
        let entry = entries.get(slot).copied().unwrap_or_else(unused_log_entry);
        payload[slot * 48..slot * 48 + 48].copy_from_slice(&entry);
    }
    payload[960..964].copy_from_slice(&(entries.len().min(20) as u32).to_le_bytes());
    payload
}

/// An unused log table entry: `{i32 -1, i32 0, u64 -1, 0...}` (C 2.2).
pub fn unused_log_entry() -> [u8; 48] {
    let mut entry = [0u8; 48];
    entry[0..4].copy_from_slice(&(-1i32).to_le_bytes());
    entry[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
    entry
}

// ---------------------------------------------------------------------------------------------------------
// Responses (B.4)
// ---------------------------------------------------------------------------------------------------------

/// What the Aime replies (types 6, 16, 18) carry once mapped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AimeReply {
    /// `db+80`.
    pub aime_id: i32,
    /// `db+96`.
    pub found: u32,
    /// `R.flags` bits the reply sets.
    pub flag_bits: u32,
    /// `db+84`, `db+88`.
    pub related: [i32; 2],
    /// `db+652`, NUL terminated.
    pub sega_id: [u8; 257],
    /// `db+68` (reply 18 only).
    pub access_code: Option<[u8; 10]>,
    /// `db+100` (reply 18 only).
    pub aux: Option<u8>,
}

fn status_error(status: u16) -> Option<InternalCode> {
    match status {
        2 => Some(InternalCode::INVALID_AIME_ID),
        4..=10 => Some(InternalCode(u32::from(status) + 19)),
        _ => None,
    }
}

fn kind_flags(kind: u8) -> u32 {
    match kind {
        1 => flags::KIND_1,
        2 => flags::KIND_2,
        _ => 0,
    }
}

fn blank_aime_reply() -> AimeReply {
    AimeReply { aime_id: -1, found: 0, flag_bits: 0, related: [-1; 2], sega_id: [0; 257], access_code: None, aux: None }
}

/// Reply 3 to a FeliCa lookup: `(error, access code)`.
pub fn parse_felica_lookup(frame: &[u8]) -> (InternalCode, Option<[u8; 10]>) {
    let status = Header::parse(frame).map_or(0, |h| h.status);
    if status == 2 {
        return (InternalCode::INVALID_FELICA_ID, None);
    }
    if i32_at(frame, 32) == -1 {
        return (InternalCode::FELICA_ID_NOT_FOUND, None);
    }
    (InternalCode::NONE, Some(frame[36..46].try_into().expect("ten bytes")))
}

/// Reply 3 to a FeliCa register.
pub fn parse_felica_register(frame: &[u8]) -> (InternalCode, Option<[u8; 10]>) {
    let status = Header::parse(frame).map_or(0, |h| h.status);
    if status == 3 {
        return (InternalCode::ALREADY_REGIST_FELICA_ID, None);
    }
    if i32_at(frame, 32) == -1 {
        return (InternalCode::CANNOT_REGIST_FELICA_ID, None);
    }
    (InternalCode::NONE, Some(frame[36..46].try_into().expect("ten bytes")))
}

/// Reply 6 to an Aime lookup (cmd 4).
pub fn parse_aime_lookup(frame: &[u8]) -> (InternalCode, AimeReply) {
    let status = Header::parse(frame).map_or(0, |h| h.status);
    let mut reply = blank_aime_reply();
    if let Some(code) = status_error(status) {
        return (code, reply);
    }
    let id = i32_at(frame, 32);
    if id == -1 {
        return (InternalCode::AIME_ID_NOT_FOUND, reply);
    }
    reply.aime_id = id;
    reply.found = 1;
    reply.flag_bits = kind_flags(frame[36]);
    (InternalCode::NONE, reply)
}

/// Reply 6 to an Aime register (cmd 5).
pub fn parse_aime_register(frame: &[u8]) -> (InternalCode, AimeReply) {
    let status = Header::parse(frame).map_or(0, |h| h.status);
    let mut reply = blank_aime_reply();
    if status == 3 {
        return (InternalCode::ALREADY_REGIST_AIME_ID, reply);
    }
    let id = i32_at(frame, 32);
    if id == -1 {
        return (InternalCode::CANNOT_REGIST_AIME_ID, reply);
    }
    reply.aime_id = id;
    reply.found = 1;
    (InternalCode::NONE, reply)
}

fn related_found(related: [i32; 2]) -> u32 {
    1 + related.iter().filter(|&&id| id != 0 && id != -1).count() as u32
}

fn copy_sega_id(frame: &[u8], offset: usize) -> [u8; 257] {
    let mut text = [0u8; 257];
    text[..256].copy_from_slice(&frame[offset..offset + 256]);
    text[256] = 0;
    text
}

/// Reply 16 to an Aime lookup 2 (cmd 15).
pub fn parse_aime_lookup2(frame: &[u8]) -> (InternalCode, AimeReply) {
    let status = Header::parse(frame).map_or(0, |h| h.status);
    let mut reply = blank_aime_reply();
    if let Some(code) = status_error(status) {
        return (code, reply);
    }
    let id = i32_at(frame, 32);
    if id == -1 {
        return (InternalCode::AIME_ID_NOT_FOUND, reply);
    }
    reply.aime_id = id;
    reply.sega_id = copy_sega_id(frame, 40);
    reply.related = [i32_at(frame, 296), i32_at(frame, 300)];
    reply.found = related_found(reply.related);
    reply.flag_bits = kind_flags(frame[36]);
    (InternalCode::NONE, reply)
}

/// Reply 18 to a Lite-S lookup (cmd 17).
pub fn parse_lite_s(frame: &[u8]) -> (InternalCode, AimeReply) {
    let status = Header::parse(frame).map_or(0, |h| h.status);
    let mut reply = blank_aime_reply();
    if let Some(code) = status_error(status) {
        return (code, reply);
    }
    let id = i32_at(frame, 32);
    if id == -1 {
        return (InternalCode::AIME_ID_NOT_FOUND, reply);
    }
    reply.aime_id = id;
    reply.related = [i32_at(frame, 36), i32_at(frame, 40)];
    reply.access_code = Some(frame[44..54].try_into().expect("ten bytes"));
    reply.flag_bits = kind_flags(frame[54]);
    reply.aux = Some(frame[55]);
    reply.sega_id = copy_sega_id(frame, 64);
    reply.found = related_found(reply.related);
    (InternalCode::NONE, reply)
}

/// Replies 8 and 10 (log status, single log): status 1 is success, anything else error 20.
pub fn parse_log_ack(frame: &[u8]) -> InternalCode {
    if Header::parse(frame).map_or(0, |h| h.status) == 1 { InternalCode::NONE } else { InternalCode::CANNOT_REGIST_LOG }
}

/// Reply 20 (multi log): the result bytes (one per entry) when the status is 1.
pub fn parse_multi_log(frame: &[u8], count: usize) -> (InternalCode, Vec<u8>) {
    if Header::parse(frame).map_or(0, |h| h.status) != 1 {
        return (InternalCode::CANNOT_REGIST_LOG, Vec::new());
    }
    (InternalCode::NONE, frame[32..32 + count.min(20)].to_vec())
}

/// Reply 12 (campaign info): the error and the three entries (copied even on failure).
pub fn parse_campaign_info(frame: &[u8]) -> (InternalCode, [RawCampaignInfo; 3]) {
    let mut entries = [RawCampaignInfo { id: 0, name: [0; 128], stamps: [0; 5] }; 3];
    for (index, entry) in entries.iter_mut().enumerate() {
        let base = 32 + 160 * index;
        entry.id = u32_at(frame, base);
        entry.name.copy_from_slice(&frame[base + 4..base + 132]);
        for (k, stamp) in entry.stamps.iter_mut().enumerate() {
            *stamp = u32_at(frame, base + 132 + 4 * k);
        }
    }
    let code = if Header::parse(frame).map_or(0, |h| h.status) == 1 {
        InternalCode::NONE
    } else {
        InternalCode::CAMPAIGN_ERROR
    };
    (code, entries)
}

/// Reply 14 (campaign clear): the error and the three entries.
pub fn parse_campaign_clear(frame: &[u8]) -> (InternalCode, [RawCampaignClear; 3]) {
    let mut entries = [RawCampaignClear::default(); 3];
    for (index, entry) in entries.iter_mut().enumerate() {
        let base = 32 + 16 * index;
        *entry = RawCampaignClear {
            id: u32_at(frame, base),
            flag: u32_at(frame, base + 4),
            value: u32_at(frame, base + 8),
            unused: u32_at(frame, base + 12),
        };
    }
    let code = if Header::parse(frame).map_or(0, |h| h.status) == 1 {
        InternalCode::NONE
    } else {
        InternalCode::CAMPAIGN_ERROR
    };
    (code, entries)
}

/// Reply 101 (alive).
pub fn parse_alive(frame: &[u8]) -> InternalCode {
    if Header::parse(frame).map_or(0, |h| h.status) == 1 { InternalCode::NONE } else { InternalCode::SERVER_ERROR }
}

/// `true` when every nibble of the packed BCD access code is `0..=9` (`0x14029FC90`).
pub fn is_bcd(code: &[u8; 10]) -> bool {
    code.iter().all(|&byte| byte <= 0x99 && byte & 0x0F <= 9)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aes_ecb_roundtrip_and_fips_vector() {
        let mut frame = encrypt(&[0x11; 48]);
        assert_ne!(frame, vec![0x11; 48]);
        decrypt(&mut frame);
        assert_eq!(frame, vec![0x11; 48]);
        // ECB: equal plaintext blocks give equal ciphertext blocks.
        let encrypted = encrypt(&[0x11; 32]);
        assert_eq!(encrypted[..16], encrypted[16..]);
    }
}
