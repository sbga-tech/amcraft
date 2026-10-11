//! The reader serial wire protocol (`spec/libam/aime.md` part D): frames, byte stuffing, checksum and the receive
//! assembler. There is no encryption on this link.

/// Frame start byte; never escaped.
pub const SYNC: u8 = 0xE0;
/// Escape byte.
pub const ESC: u8 = 0xD0;

/// Command ids (D.4).
pub mod cmd {
    /// Get firmware version.
    pub const GET_FW_VERSION: u8 = 0x30;
    /// Get hardware version.
    pub const GET_HW_VERSION: u8 = 0x32;
    /// Start polling (one payload byte: the card-type filter).
    pub const START_POLLING: u8 = 0x40;
    /// Stop polling.
    pub const STOP_POLLING: u8 = 0x41;
    /// Card detect.
    pub const CARD_DETECT: u8 = 0x42;
    /// Select card by UID.
    pub const SELECT_CARD: u8 = 0x43;
    /// Halt card.
    pub const HALT_CARD: u8 = 0x44;
    /// MIFARE set key A.
    pub const SET_KEY_A: u8 = 0x50;
    /// MIFARE authenticate with key A.
    pub const AUTH_KEY_A: u8 = 0x51;
    /// MIFARE read block.
    pub const READ_BLOCK: u8 = 0x52;
    /// MIFARE set key B.
    pub const SET_KEY_B: u8 = 0x54;
    /// MIFARE authenticate with key B.
    pub const AUTH_KEY_B: u8 = 0x55;
    /// Reader to updater mode.
    pub const TO_UPDATER: u8 = 0x60;
    /// Hex line.
    pub const HEX_DATA: u8 = 0x61;
    /// Reader to normal mode (reset).
    pub const TO_NORMAL: u8 = 0x62;
    /// Start binary transfer.
    pub const BIN_START: u8 = 0x63;
    /// Binary block.
    pub const BIN_BLOCK: u8 = 0x64;
    /// Mobile FeliCa mail/web open.
    pub const MOBILE_OPEN: u8 = 0x70;
    /// FeliCa through.
    pub const FELICA_THROUGH: u8 = 0x71;
    /// LED set colour.
    pub const LED_COLOUR: u8 = 0x81;
    /// LED blink: `[board, r, g, b, mode u16be, on-ms u16be, off-ms u16be]` (the e-money `SetLedBlink`, no reply).
    pub const LED_BLINK: u8 = 0x82;
    /// LED / extension board info.
    pub const LED_BOARD_INFO: u8 = 0xF0;
    /// LED firmware data line.
    pub const LED_DATA: u8 = 0xF3;
    /// LED board to boot mode.
    pub const LED_BOOT: u8 = 0xF4;
    /// LED board to normal mode / probe.
    pub const LED_PROBE: u8 = 0xF5;
}

/// 8-bit sum of `bytes` (the caller passes `len` through the last payload byte).
pub fn checksum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |sum, &byte| sum.wrapping_add(byte))
}

/// Byte-stuffs a complete unescaped frame (checksum included): everything after the sync byte is escaped.
pub fn stuff(frame: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(frame.len() + 4);
    for (index, &byte) in frame.iter().enumerate() {
        if index > 0 && (byte == ESC || byte == SYNC) {
            out.push(ESC);
            out.push(byte.wrapping_sub(1));
        } else {
            out.push(byte);
        }
    }
    out
}

/// The unit address a frame carries (D.3.4): `unit` for commands below `0x80`, the extension board `8 + (unit & 7)`
/// above.
pub fn dst_for(cmd: u8, unit: u8) -> u8 {
    if cmd >= 0x80 { (unit & 7) | 8 } else { unit }
}

/// Builds an unescaped request: `E0 len dst seq cmd paylen payload sum`. The binary block command `0x64` carries
/// 256 data bytes with a truncated length (`len = 5`, `paylen = 0`).
pub fn request(dst: u8, seq: u8, cmd: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(payload.len() + 8);
    // `len` and `paylen` are single bytes: a 256 byte payload wraps to the documented truncated header.
    frame.extend_from_slice(&[SYNC, (payload.len() + 5) as u8, dst, seq, cmd, payload.len() as u8]);
    frame.extend_from_slice(payload);
    let sum = checksum(&frame[1..]);
    frame.push(sum);
    frame
}

/// A decoded response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// Unit address the reader answered from.
    pub dst: u8,
    /// Echoed sequence byte.
    pub seq: u8,
    /// Echoed command.
    pub cmd: u8,
    /// Status byte (D.4.1).
    pub status: u8,
    /// Payload.
    pub payload: Vec<u8>,
}

/// The manager-path receive assembler (D.3.2): bytes arrive in chunks, a frame starts with `E0` and is complete when
/// `len + 2` logical bytes (escapes counted) are in.
#[derive(Debug, Default)]
pub struct Assembler {
    buf: Vec<u8>,
}

impl Assembler {
    /// Appends received bytes and returns every complete, valid response. Garbage before a sync byte is dropped; a
    /// frame with a wrong sum is dropped, except commands `0x70`/`0x71`, which continue with status 1.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Response> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        loop {
            match self.buf.iter().position(|&byte| byte == SYNC) {
                None => {
                    self.buf.clear();
                    break;
                }
                Some(start) if start > 0 => {
                    self.buf.drain(..start);
                }
                Some(_) => {}
            }
            let Some((frame, used)) = Self::unstuff_one(&self.buf) else { break };
            self.buf.drain(..used);
            if let Some(response) = Self::parse(&frame) {
                out.push(response);
            }
        }
        out
    }

    /// Unstuffs the first frame of `raw` (which starts with `E0`): the logical frame and the raw bytes used, or
    /// `None` while incomplete.
    fn unstuff_one(raw: &[u8]) -> Option<(Vec<u8>, usize)> {
        let len = *raw.get(1)? as usize;
        let want = len + 2;
        let mut frame = Vec::with_capacity(want);
        frame.push(SYNC);
        frame.push(len as u8);
        let mut index = 2;
        while frame.len() < want {
            let byte = *raw.get(index)?;
            index += 1;
            if byte == ESC {
                let next = *raw.get(index)?;
                index += 1;
                frame.push(next.wrapping_add(1));
            } else {
                frame.push(byte);
            }
        }
        Some((frame, index))
    }

    fn parse(frame: &[u8]) -> Option<Response> {
        if frame.len() < 8 {
            return None;
        }
        let (body, sum) = frame[1..].split_at(frame.len() - 2);
        let cmd = frame[4];
        let mut status = frame[5];
        if checksum(body) != sum[0] {
            if cmd != 0x70 && cmd != 0x71 {
                return None;
            }
            status = 1;
        }
        let paylen = frame[6] as usize;
        if frame.len() != paylen + 8 {
            return None;
        }
        Some(Response { dst: frame[2], seq: frame[3], cmd, status, payload: frame[7..7 + paylen].to_vec() })
    }
}

/// A request as the reader (or its simulation) sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// Destination address.
    pub dst: u8,
    /// Sequence byte.
    pub seq: u8,
    /// Command.
    pub cmd: u8,
    /// Payload (the 256 data bytes of `0x64`).
    pub payload: Vec<u8>,
}

/// Request decoder for the reader side (simulations): handles the truncated `0x64` header.
#[derive(Debug, Default)]
pub struct RequestAssembler {
    buf: Vec<u8>,
}

impl RequestAssembler {
    /// Appends bytes and returns the complete requests.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Request> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        loop {
            match self.buf.iter().position(|&byte| byte == SYNC) {
                None => {
                    self.buf.clear();
                    break;
                }
                Some(start) if start > 0 => {
                    self.buf.drain(..start);
                }
                Some(_) => {}
            }
            // Logical bytes needed: 6 header bytes first, then paylen + 1 (or 257 for the 0x64 exception).
            let mut logical = vec![SYNC];
            let mut index = 1;
            let mut need = 6usize;
            let mut complete = false;
            while let Some(&byte) = self.buf.get(index) {
                index += 1;
                let value = if byte == ESC {
                    let Some(&next) = self.buf.get(index) else { break };
                    index += 1;
                    next.wrapping_add(1)
                } else {
                    byte
                };
                logical.push(value);
                if logical.len() == 6 {
                    let paylen = if logical[4] == cmd::BIN_BLOCK { 256 } else { logical[5] as usize };
                    need = 6 + paylen + 1;
                }
                if logical.len() == need {
                    complete = true;
                    break;
                }
            }
            if !complete {
                break;
            }
            self.buf.drain(..index);
            let body = &logical[1..need - 1];
            if checksum(body) == logical[need - 1] {
                out.push(Request {
                    dst: logical[2],
                    seq: logical[3],
                    cmd: logical[4],
                    payload: logical[6..need - 1].to_vec(),
                });
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_examples() {
        assert_eq!(request(0, 1, 0x62, &[]), [0xE0, 0x05, 0x00, 0x01, 0x62, 0x00, 0x68]);
        assert_eq!(request(0, 16, 0x42, &[]), [0xE0, 0x05, 0x00, 0x10, 0x42, 0x00, 0x57]);
        assert_eq!(
            stuff(&request(0, 4, 0x50, &[0x60, 0x90, 0xD0, 0x06, 0x32, 0xF5])),
            [0xE0, 0x0B, 0x00, 0x04, 0x50, 0x06, 0x60, 0x90, 0xD0, 0xCF, 0x06, 0x32, 0xF5, 0x52]
        );
        assert_eq!(request(8, 9, 0x81, &[0xFF, 0, 0]), [0xE0, 0x08, 0x08, 0x09, 0x81, 0x03, 0xFF, 0x00, 0x00, 0x9C]);
    }

    #[test]
    fn assembler_round_trips_split_escaped_frames() {
        let frame = [0xE0, 0x0D, 0x00, 0x05, 0x42, 0x00, 0x07, 0x01, 0x10, 0x04, 0x04, 0xD0, 0xE0, 0x11, 0x35];
        let wire = stuff(&frame);
        assert_eq!(wire.len(), 17);
        let mut assembler = Assembler::default();
        assert!(assembler.push(&wire[..9]).is_empty());
        let got = assembler.push(&wire[9..]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].payload, [0x01, 0x10, 0x04, 0x04, 0xD0, 0xE0, 0x11]);
    }

    #[test]
    fn block_command_uses_truncated_header() {
        let data: Vec<u8> = (0..=255u8).collect();
        let frame = request(0, 12, cmd::BIN_BLOCK, &data);
        assert_eq!(&frame[..6], [0xE0, 0x05, 0x00, 0x0C, 0x64, 0x00]);
        assert_eq!(frame.len(), 263);
        let mut assembler = RequestAssembler::default();
        let got = assembler.push(&stuff(&frame));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].payload, data);
    }
}
