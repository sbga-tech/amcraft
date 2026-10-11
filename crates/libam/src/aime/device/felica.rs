//! The FeliCa Lite / Lite-S / Standard / mobile steps of the scan machine (`spec/libam/aime.md` E.4.4, SEQ 32..65).
//!
//! Every FeliCa frame travels as the payload of command `0x71`: the 8 IDm bytes of the card, then the FeliCa frame
//! `len code params..`. A send state has an even SEQ; the wait state for its answer is the SEQ after it.

use super::FollowUp;
use super::hub::Hub;
use super::lites;
use super::poll::seq;
use super::wire::{Response, cmd};
use crate::aime::code::{access_code_of_block, lite_mac};

/// Largest mail/web frame by OS type (E.4.4): type 1 up to 168 bytes, types 2 and 3 up to 200.
const fn frame_limit(os: u8) -> usize {
    if os == 1 { 168 } else { 200 }
}

/// Field caps of the tagged parameter strings (`0x14034AD80`, `0x14034AE80`): short fields 100 bytes, long 399.
const SHORT_FIELD: usize = 100;
const LONG_FIELD: usize = 399;

/// FeliCa error codes written to `[41964]`.
mod error {
    pub const FIRMWARE: u32 = 1;
    pub const SYSTEM_CODE: u32 = 2;
    pub const ID_BLOCK: u32 = 3;
    pub const RANDOM_WRITE: u32 = 5;
    pub const MAC: u32 = 7;
    pub const LITE_READ: u32 = 8;
    pub const LITE_DECODE: u32 = 9;
    pub const STANDARD_SYSTEMS: u32 = 10;
    pub const STANDARD_READ: u32 = 11;
    pub const STANDARD_DECODE: u32 = 12;
    pub const TOO_LONG: u32 = 14;
    pub const STATUS: u32 = 15;
}

/// `len code IDm params`: a FeliCa request addressed to `idm`.
fn frame(code: u8, idm: &[u8], params: &[u8]) -> Vec<u8> {
    let mut out = vec![(2 + idm.len() + params.len()) as u8, code];
    out.extend_from_slice(idm);
    out.extend_from_slice(params);
    out
}

/// Read Without Encryption of `blocks` of service `0x000B` (block list elements `80 nn`).
fn read_frame(idm: &[u8], blocks: &[u8]) -> Vec<u8> {
    let mut params = vec![0x01, 0x0B, 0x00, blocks.len() as u8];
    for &block in blocks {
        params.extend_from_slice(&[0x80, block]);
    }
    frame(0x06, idm, &params)
}

/// The data of a Read Without Encryption response (`0x140348A00`): code 7, flags bytes 10/11 zero, the block count
/// byte 12, data from byte 13.
fn read_data(payload: &[u8], blocks: usize) -> Option<&[u8]> {
    let data = payload.get(13..)?;
    (payload.get(1) == Some(&0x07)
        && payload.get(10) == Some(&0)
        && payload.get(11) == Some(&0)
        && payload.get(12) == Some(&(blocks as u8))
        && data.len() >= 16 * blocks)
        .then_some(data)
}

/// A 16-byte block at `index` of `data`.
fn block_at(data: &[u8], index: usize) -> [u8; 16] {
    let mut block = [0u8; 16];
    block.copy_from_slice(&data[16 * index..16 * index + 16]);
    block
}

/// The text between `<TAG>` and `</TAG>`, cut at `cap` bytes; empty when the tag is absent.
fn tagged<'a>(text: &'a str, tag: &str, cap: usize) -> &'a [u8] {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let Some(start) = text.find(&open).map(|at| at + open.len()) else { return &[] };
    let end = text[start..].find(&close).map_or(text.len(), |at| start + at);
    let bytes = &text.as_bytes()[start..end];
    &bytes[..bytes.len().min(cap)]
}

/// Appends `u16 le length` and the bytes.
fn push_field(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
    out.extend_from_slice(bytes);
}

/// The big-endian `-(sum of the bytes from offset 8)` that closes a mail/web frame.
fn push_checksum(out: &mut Vec<u8>) {
    let sum = out[8..].iter().fold(0u16, |sum, &byte| sum.wrapping_add(u16::from(byte)));
    out.extend_from_slice(&sum.wrapping_neg().to_be_bytes());
}

/// The command `0x70` payload of a follow-up action (`0x14034AD80` web, `0x14034AE80` mail): `IDm 01 02|03`, the
/// length of the rest as u16 le (the frame length minus the 12 byte head, checksum included), the length-prefixed
/// fields, the parameter text without prefix and the checksum. Larger than the OS limit: `None`.
pub(super) fn mobile_frame(idm: &[u8], action: &FollowUp, os: u8) -> Option<Vec<u8>> {
    let mut out = idm.to_vec();
    match action {
        FollowUp::Mail(text) => {
            out.extend_from_slice(&[0x01, 0x03, 0, 0]);
            for (tag, cap) in
                [("MAILTO", SHORT_FIELD), ("MAILCC", SHORT_FIELD), ("MAILSUB", SHORT_FIELD), ("MAILDTL", LONG_FIELD)]
            {
                push_field(&mut out, tagged(text, tag, cap));
            }
            out.extend_from_slice(tagged(text, "MAILPARAM", LONG_FIELD));
        }
        FollowUp::Web(text) => {
            out.extend_from_slice(&[0x01, 0x02, 0, 0]);
            push_field(&mut out, tagged(text, "URL", LONG_FIELD));
            out.extend_from_slice(tagged(text, "WEBPARAM", LONG_FIELD));
        }
    }
    if out.len() + 2 > frame_limit(os) {
        return None;
    }
    let rest = (out.len() + 2 - 12) as u16;
    out[10..12].copy_from_slice(&rest.to_le_bytes());
    push_checksum(&mut out);
    Some(out)
}

impl Hub {
    /// Sends a FeliCa frame as command `0x71` with the IDm prefix and enters the wait state.
    fn through(&mut self, u: usize, felica: &[u8]) {
        let timeout = self.units[u].config.command_timeout_ms;
        let mut payload = self.units[u].cards[0].idm_pmm[..8].to_vec();
        payload.extend_from_slice(felica);
        self.send_armed(u, timeout, cmd::FELICA_THROUGH, &payload);
        self.units[u].seq += 1;
    }

    fn idm(&self, u: usize) -> [u8; 8] {
        let mut idm = [0u8; 8];
        idm.copy_from_slice(&self.units[u].cards[0].idm_pmm[..8]);
        idm
    }

    /// A send state of SEQ 32..65 (the wait states have nothing to do until their response arrives).
    pub(super) fn felica_step(&mut self, u: usize, now: u64) {
        let idm = self.idm(u);
        match self.units[u].seq {
            seq::FELICA_LITE_CHECK => self.through(u, &[0x06, 0x00, 0xFF, 0xFF, 0x01, 0x0F]),
            seq::FELICA_DFC => self.through(u, &read_frame(&idm, &[0x82])),
            seq::FELICA_RANDOM => {
                let unit = &mut self.units[u];
                let mut rc = [0u8; 16];
                for byte in &mut rc {
                    *byte = unit.random_byte();
                }
                unit.rc = rc;
                unit.cards[0].lite_s.rc = rc;
                let mut params = vec![0x01, 0x09, 0x00, 0x01, 0x80, 0x80];
                params.extend_from_slice(&rc);
                self.through(u, &frame(0x08, &idm, &params));
            }
            seq::FELICA_MAC => self.through(u, &read_frame(&idm, &[0x82, 0x81])),
            seq::FELICA_LITE_READ => self.through(u, &read_frame(&idm, &[0x0D])),
            seq::FELICA_LITES_BLOCKS => self.through(u, &read_frame(&idm, &[0x82, 0x86, 0x90, 0x91])),
            seq::FELICA_LITES_BLOCK0 | seq::FELICA_STD_READ => self.through(u, &read_frame(&idm, &[0x00])),
            seq::FELICA_STD_CHECK => self.through(u, &frame(0x0C, &idm, &[])),
            seq::FELICA_MOBILE_CHECK => self.through(u, &frame(0xA4, &idm, &[0x00])),
            seq::FELICA_MOBILE_OS => self.through(u, &frame(0xA4, &idm, &[0x01])),
            step @ (seq::FELICA_MAIL | seq::FELICA_WEB) => self.send_mobile(u, step, &idm, now),
            _ => {}
        }
    }

    /// SEQ 54 / 56: the mailer or browser open request (`0x70`).
    fn send_mobile(&mut self, u: usize, step: u8, idm: &[u8; 8], now: u64) {
        let unit = &self.units[u];
        let action = match (&unit.follow_up, step) {
            (Some(action @ FollowUp::Mail(_)), seq::FELICA_MAIL)
            | (Some(action @ FollowUp::Web(_)), seq::FELICA_WEB) => action.clone(),
            _ => {
                self.felica_done(u, 0, now);
                return;
            }
        };
        match mobile_frame(idm, &action, unit.mobile_os.max(1)) {
            Some(payload) => {
                let timeout = unit.config.command_timeout_ms;
                self.send_armed(u, timeout, cmd::MOBILE_OPEN, &payload);
                self.units[u].seq += 1;
            }
            None => self.felica_done(u, error::TOO_LONG, now),
        }
    }

    /// Ends the FeliCa flow with result `code` (`[41964]`); the follow-up action is consumed.
    fn felica_done(&mut self, u: usize, code: u32, now: u64) {
        let unit = &mut self.units[u];
        unit.felica_error = code;
        unit.felica_follow = 0;
        if matches!(unit.seq, seq::FELICA_MAIL | seq::FELICA_WEB | 55 | 57) {
            unit.follow_up = None;
        }
        self.goto(u, seq::SEND_STOP, now);
    }

    /// A step failed: the first transport error repeats the step (`0x14034B260`), then the error code is final.
    fn felica_fail(&mut self, u: usize, status: u8, retry: bool, code: u32, now: u64) {
        let unit = &mut self.units[u];
        if (status != 0 || retry) && unit.felica_retry < 1 {
            unit.felica_retry += 1;
            unit.seq -= 1;
            return;
        }
        let code = match status {
            0 => code,
            1 => error::SYSTEM_CODE,
            3 => error::FIRMWARE,
            _ => error::STATUS,
        };
        self.felica_done(u, code, now);
    }

    /// The access code of a decoded Banapass block, or `None` (`0x14035D370` + the BCD conversion).
    fn set_access_code(&mut self, u: usize, block: &[u8; 16]) -> bool {
        match access_code_of_block(block) {
            Some(code) => {
                let card = &mut self.units[u].cards[0];
                card.access_code = code;
                card.valid = true;
                true
            }
            None => false,
        }
    }

    /// A response of command `0x70` / `0x71` (`0x14034A3F0`, `0x14034B980`), by the wait state it answers.
    pub(super) fn felica_response(&mut self, u: usize, response: &Response, now: u64) {
        let wait = self.units[u].seq;
        let payload = response.payload.as_slice();
        if response.status != 0 {
            self.felica_fail(u, response.status, false, error::STATUS, now);
            return;
        }
        match wait {
            seq::FELICA_LITE_CHECK_WAIT => {
                if payload.get(1) == Some(&0x01) && payload.get(18..20) == Some(&[0x88, 0xB4]) {
                    self.units[u].seq = seq::FELICA_DFC;
                } else {
                    self.felica_done(u, error::SYSTEM_CODE, now);
                }
            }
            seq::FELICA_DFC_WAIT => match read_data(payload, 1) {
                Some(data) => {
                    let id = block_at(data, 0);
                    let unit = &mut self.units[u];
                    unit.id_block = id;
                    unit.cards[0].lite_s.dfc = [id[8], id[9]];
                    unit.seq = seq::FELICA_RANDOM;
                }
                None => self.felica_done(u, error::ID_BLOCK, now),
            },
            seq::FELICA_RANDOM_WAIT => {
                let ok = payload.get(1) == Some(&0x09) && payload.get(10) == Some(&0) && payload.get(11) == Some(&0);
                if ok {
                    let unit = &mut self.units[u];
                    unit.seq = if unit.felica_class == 5 { seq::FELICA_LITES_BLOCKS } else { seq::FELICA_MAC };
                } else {
                    self.felica_done(u, error::RANDOM_WRITE, now);
                }
            }
            seq::FELICA_MAC_WAIT => match read_data(payload, 2) {
                Some(data) => {
                    let (id, mac_block) = (block_at(data, 0), block_at(data, 1));
                    let rc = self.units[u].rc;
                    if lite_mac(&id, &rc, &id)[..] == mac_block[..8] {
                        self.units[u].seq = seq::FELICA_LITE_READ;
                    } else {
                        self.felica_done(u, error::MAC, now);
                    }
                }
                None => self.felica_fail(u, 0, true, error::MAC, now),
            },
            seq::FELICA_LITE_READ_WAIT => match read_data(payload, 1) {
                Some(data) => {
                    if self.set_access_code(u, &block_at(data, 0)) {
                        self.felica_done(u, 0, now);
                    } else {
                        self.felica_done(u, error::LITE_DECODE, now);
                    }
                }
                None => self.felica_fail(u, 0, true, error::LITE_READ, now),
            },
            seq::FELICA_LITES_BLOCKS_WAIT => match read_data(payload, 4) {
                Some(data) => {
                    let lite_s = &mut self.units[u].cards[0].lite_s;
                    for (index, slot) in lite_s.blocks.iter_mut().enumerate() {
                        *slot = block_at(data, index);
                    }
                    lite_s.block91.copy_from_slice(&data[48..56]);
                    self.units[u].seq = seq::FELICA_LITES_BLOCK0;
                }
                None => self.felica_fail(u, 0, true, error::LITE_READ, now),
            },
            seq::FELICA_LITES_BLOCK0_WAIT => match read_data(payload, 1) {
                Some(data) => {
                    let (value, nibble) = lites::descramble(&block_at(data, 0));
                    let lite_s = &mut self.units[u].cards[0].lite_s;
                    lite_s.value = value;
                    lite_s.nibble = nibble;
                    self.felica_done(u, 0, now);
                }
                None => self.felica_fail(u, 0, true, error::LITE_READ, now),
            },
            seq::FELICA_STD_CHECK_WAIT => {
                let count = usize::from(payload.get(10).copied().unwrap_or(0));
                let codes = payload.get(11..11 + 2 * count);
                match (payload.get(1) == Some(&0x0D), codes) {
                    (true, Some(codes)) if codes.chunks_exact(2).any(|code| code == [0x8C, 0xA2]) => {
                        self.units[u].seq = seq::FELICA_STD_READ;
                    }
                    (true, Some(_)) => self.felica_done(u, error::STANDARD_SYSTEMS, now),
                    _ => self.felica_done(u, error::SYSTEM_CODE, now),
                }
            }
            seq::FELICA_STD_READ_WAIT => match read_data(payload, 1) {
                Some(data) => {
                    if self.set_access_code(u, &block_at(data, 0)) {
                        self.units[u].auth_kind = 2;
                        self.felica_done(u, 0, now);
                    } else {
                        self.felica_done(u, error::STANDARD_DECODE, now);
                    }
                }
                None => self.felica_fail(u, 0, true, error::STANDARD_READ, now),
            },
            seq::FELICA_MOBILE_CHECK_WAIT => {
                if payload.get(1) == Some(&0xA5) && payload.get(10) == Some(&0x9D) {
                    // A mobile chip that supports the OS version query: ask it after the (already finished) stop.
                    self.units[u].felica_follow = 14;
                }
                self.goto(u, seq::SEND_STOP, now);
            }
            seq::FELICA_MOBILE_OS_WAIT => {
                let unit = &mut self.units[u];
                unit.mobile_os = if payload.get(10) == Some(&0) { 2 } else { 1 };
                match unit.follow_up {
                    Some(FollowUp::Mail(_)) => unit.seq = seq::FELICA_MAIL,
                    Some(FollowUp::Web(_)) => unit.seq = seq::FELICA_WEB,
                    None => self.goto(u, seq::SEND_STOP, now),
                }
            }
            seq::FELICA_MAIL_WAIT | seq::FELICA_WEB_WAIT => self.felica_done(u, 0, now),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDM: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];

    #[test]
    fn web_frame_layout_and_checksum() {
        let action = FollowUp::Web("<URL>http://a</URL><WEBPARAM>x=1</WEBPARAM>".into());
        let frame = mobile_frame(&IDM, &action, 2).expect("fits");
        assert_eq!(&frame[..10], &[1, 2, 3, 4, 5, 6, 7, 8, 0x01, 0x02]);
        // 2 (url len) + 8 (url) + 3 (param) + 2 (checksum) = 15.
        assert_eq!(u16::from_le_bytes([frame[10], frame[11]]), 15);
        assert_eq!(&frame[12..14], &8u16.to_le_bytes());
        assert_eq!(&frame[14..22], b"http://a");
        assert_eq!(&frame[22..25], b"x=1");
        let sum: u16 = frame[8..frame.len() - 2].iter().map(|&byte| u16::from(byte)).sum();
        assert_eq!(&frame[frame.len() - 2..], &sum.wrapping_neg().to_be_bytes());
    }

    #[test]
    fn mail_frame_limits_depend_on_the_os_type() {
        let long = "z".repeat(150);
        let action = FollowUp::Mail(format!("<MAILTO>a@b</MAILTO><MAILDTL>{long}</MAILDTL>"));
        assert!(mobile_frame(&IDM, &action, 1).is_none());
        assert!(mobile_frame(&IDM, &action, 2).is_some());
    }
}
