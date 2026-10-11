//! A scriptable simulated Aime reader (`spec/libam/aime.md` D.7).
//!
//! Requests are answered synchronously in [`SerialPort::write`], so a response is readable at once. One
//! [`SimAimeReader`] is one COM port; [`crate::mock::SimSerial`] attaches it by number.

use std::any::Any;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::io;
use std::rc::Rc;

use crate::aime::code::lite_mac;
use crate::aime::device::init::{KEY_A, KEY_B};
use crate::aime::device::wire::{self, Request, RequestAssembler, cmd};
use crate::aime::tables::BANAPASS_KEYS;
use crate::mock::serial::SerialDevice;
use backend::serial::{SerialPort, SerialSettings};

/// The card name the simulated Aime card carries in block 1.
pub const SIM_CARD_NAME: [u8; 4] = *b"SBSD";
/// The number the simulated Aime card carries in block 1 bytes 12..15.
pub const SIM_CARD_VALUE: u32 = 0x0012_D687;
/// The version byte of the simulated LED board.
pub const SIM_LED_VERSION: u8 = 0x11;

/// A card the simulated reader holds in its field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SimCard {
    /// Aime card: MIFARE, authenticates with key slot B, access code in block 2.
    Aime {
        /// 4-byte UID.
        uid: [u8; 4],
        /// 20 digits, packed BCD.
        access_code: [u8; 10],
    },
    /// Banapass: MIFARE, authenticates with key slot A, block 1 is a Blowfish block.
    Banapass {
        /// 4-byte UID.
        uid: [u8; 4],
        /// Banapass card id.
        value: u32,
        /// Key slot 0..8.
        slot: usize,
    },
    /// FeliCa Lite with the Banapass block in S_PAD13.
    FeliCaLite {
        /// IDm.
        idm: [u8; 8],
        /// Banapass card id.
        value: u32,
        /// Key slot 0..8.
        slot: usize,
    },
    /// FeliCa Standard (system 8CA2) with the Banapass block in block 0.
    FeliCaStandard {
        /// IDm.
        idm: [u8; 8],
        /// Banapass card id.
        value: u32,
        /// Key slot 0..8.
        slot: usize,
    },
    /// FeliCa Lite-S: no local access code; block 0 is scrambled.
    FeliCaLiteS {
        /// IDm.
        idm: [u8; 8],
        /// The 16-byte block 0.
        block0: [u8; 16],
    },
    /// A store authentication card (`spec/libam/payment.md` 6): MIFARE Classic whose sectors 1 to 3 open with key
    /// A `thinca`; the reader detects its whole ID, the key diversifier is the last four bytes.
    Store {
        /// The card ID the reader detects (4 or 7 bytes).
        id: Vec<u8>,
        /// The nine blocks `(1,0), (1,1), (1,2), (2,0) .. (3,2)`.
        blocks: Vec<[u8; 16]>,
    },
}

impl SimCard {
    fn uid(&self) -> Option<[u8; 4]> {
        match self {
            Self::Aime { uid, .. } | Self::Banapass { uid, .. } => Some(*uid),
            _ => None,
        }
    }

    fn idm(&self) -> Option<[u8; 8]> {
        match self {
            Self::FeliCaLite { idm, .. } | Self::FeliCaStandard { idm, .. } | Self::FeliCaLiteS { idm, .. } => {
                Some(*idm)
            }
            _ => None,
        }
    }

    /// PMm byte 1 selects the chip class (E.4.2).
    fn pmm(&self) -> [u8; 8] {
        let class = match self {
            Self::FeliCaLiteS { .. } => 0xF1,
            Self::FeliCaStandard { .. } => 0x01,
            _ => 0xF0,
        };
        [0x00, class, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]
    }

    fn system_code(&self) -> [u8; 2] {
        if matches!(self, Self::FeliCaStandard { .. }) { [0x8C, 0xA2] } else { [0x88, 0xB4] }
    }

    /// The ID block 0x82 of a Lite card: the IDm and an all-zero DFC.
    fn id_block(idm: [u8; 8]) -> [u8; 16] {
        let mut block = [0u8; 16];
        block[..8].copy_from_slice(&idm);
        block
    }

    /// A MIFARE block as the card holds it.
    fn mifare_block(&self, index: u8) -> [u8; 16] {
        let mut block = [0u8; 16];
        match (self, index) {
            (_, 0) => {
                let uid = self.uid().unwrap_or_default();
                block[..4].copy_from_slice(&uid);
                block[4] = uid.iter().fold(0, |bcc, byte| bcc ^ byte);
                block[5..8].copy_from_slice(&[0x08, 0x04, 0x00]);
            }
            (Self::Aime { .. }, 1) => {
                block[..4].copy_from_slice(&SIM_CARD_NAME);
                block[12..].copy_from_slice(&SIM_CARD_VALUE.to_be_bytes());
            }
            (Self::Aime { access_code, .. }, 2) => block[6..].copy_from_slice(access_code),
            (Self::Banapass { value, slot, .. }, 1) => block = encode_block(*value, [0; 3], *slot),
            _ => {}
        }
        block
    }
}

/// A request the simulated reader received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimRequest {
    /// COM port.
    pub com: u16,
    /// The decoded request.
    pub request: Request,
}

#[derive(Debug, Default)]
struct PortState {
    open: bool,
    silent: bool,
    asm: RequestAssembler,
    out: VecDeque<u8>,
    card: Option<SimCard>,
    polling: bool,
    selected: bool,
    /// 0 none, 1 key slot B, 2 key slot A.
    auth: u8,
    key_a: [u8; 6],
    key_b: [u8; 6],
    updater: bool,
    fw_blocks: u32,
    rc: [u8; 16],
    /// The sector a store card authenticated for.
    store_sector: Option<u8>,
}

#[derive(Debug)]
struct State {
    port: PortState,
    log: Vec<SimRequest>,
    generation: u8,
    led_board: bool,
    fail_open: bool,
    com: u16,
}

impl Default for State {
    fn default() -> Self {
        Self { port: PortState::default(), log: Vec::new(), generation: 1, led_board: true, fail_open: false, com: 0 }
    }
}

/// One simulated reader. Clones share the device, including with the port opened for it.
#[derive(Debug, Clone)]
pub struct SimAimeReader(Rc<RefCell<State>>);

impl SimAimeReader {
    /// A reader on `com`: generation 1, LED board present, no card.
    pub fn on_port(com: u16) -> Self {
        Self(Rc::new(RefCell::new(State { com, ..State::default() })))
    }

    /// Places `card` on the reader (replaces any card there).
    pub fn place_card(&self, card: SimCard) {
        self.0.borrow_mut().port.card = Some(card);
    }

    /// Places an Aime card whose block 2 holds `access_code`.
    pub fn place_aime_card(&self, access_code: [u8; 10]) {
        self.place_card(SimCard::Aime { uid: [0x04, 0xD0, 0xE0, 0x11], access_code });
    }

    /// Removes the card.
    pub fn remove_card(&self) {
        let port = &mut self.0.borrow_mut().port;
        port.card = None;
        port.selected = false;
        port.auth = 0;
        port.store_sector = None;
    }

    /// Opening this reader fails.
    pub fn fail_open(&self) {
        self.0.borrow_mut().fail_open = true;
    }

    /// The reader stops answering (a dead reader: every command times out).
    pub fn silence(&self) {
        self.0.borrow_mut().port.silent = true;
    }

    /// The hardware generation the reader reports: 1 (default), 2 or 3.
    pub fn set_generation(&self, generation: u8) {
        self.0.borrow_mut().generation = generation;
    }

    /// The reader has no LED board: the probe `0xF5` is not answered.
    pub fn remove_led_board(&self) {
        self.0.borrow_mut().led_board = false;
    }

    /// Every request this reader has received, in order.
    pub fn requests(&self) -> Vec<SimRequest> {
        self.0.borrow().log.clone()
    }
}

impl SerialDevice for SimAimeReader {
    fn open(&self, settings: &SerialSettings) -> io::Result<Box<dyn SerialPort>> {
        let mut state = self.0.borrow_mut();
        if state.fail_open {
            return Err(io::Error::new(io::ErrorKind::NotFound, format!("COM{}: access denied", state.com)));
        }
        if settings.baud != 38_400 && settings.baud != 115_200 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("COM{}: unsupported baud {}", state.com, settings.baud),
            ));
        }
        let (card, silent) = (state.port.card.take(), state.port.silent);
        state.port = PortState { open: true, card, silent, ..PortState::default() };
        drop(state);
        Ok(Box::new(AimePort(self.0.clone())))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// The answer of the card to a FeliCa through frame (`IDm | len code IDm params`); `None` when the frame is not for
/// this card or not understood.
fn felica_answer(card: &SimCard, port_rc: &mut [u8; 16], payload: &[u8]) -> Option<Vec<u8>> {
    let idm = card.idm()?;
    let (head, frame) = payload.split_at_checked(8)?;
    if head != idm || frame.len() < 2 {
        return None;
    }
    let with_len = |code: u8, body: &[u8]| {
        let mut out = vec![(2 + body.len()) as u8, code];
        out.extend_from_slice(body);
        out
    };
    let params = frame.get(10..).unwrap_or(&[]);
    match frame[1] {
        0x00 => {
            let mut body = idm.to_vec();
            body.extend_from_slice(&card.pmm());
            body.extend_from_slice(&card.system_code());
            Some(with_len(0x01, &body))
        }
        0x06 => {
            let count = usize::from(*params.get(3)?);
            let mut body = idm.to_vec();
            body.extend_from_slice(&[0, 0, count as u8]);
            for element in params.get(4..4 + 2 * count)?.chunks_exact(2) {
                let block = element[1];
                body.extend_from_slice(&felica_block(card, port_rc, block)?);
            }
            Some(with_len(0x07, &body))
        }
        0x08 => {
            if params.get(..6)? != [0x01, 0x09, 0x00, 0x01, 0x80, 0x80] {
                return None;
            }
            port_rc.copy_from_slice(params.get(6..22)?);
            let mut body = idm.to_vec();
            body.extend_from_slice(&[0, 0]);
            Some(with_len(0x09, &body))
        }
        0x0C => {
            let mut body = idm.to_vec();
            body.push(1);
            body.extend_from_slice(&card.system_code());
            Some(with_len(0x0D, &body))
        }
        _ => None,
    }
}

/// A block of a FeliCa card.
fn felica_block(card: &SimCard, rc: &[u8; 16], block: u8) -> Option<[u8; 16]> {
    match (card, block) {
        (SimCard::FeliCaLite { idm, .. }, 0x82) | (SimCard::FeliCaLiteS { idm, .. }, 0x82) => {
            Some(SimCard::id_block(*idm))
        }
        (SimCard::FeliCaLite { idm, .. }, 0x81) => {
            let id = SimCard::id_block(*idm);
            let mut mac = [0u8; 16];
            mac[..8].copy_from_slice(&lite_mac(&id, rc, &id));
            Some(mac)
        }
        (SimCard::FeliCaLite { value, slot, .. }, 0x0D) => Some(encode_block(*value, [0; 3], *slot)),
        (SimCard::FeliCaStandard { value, slot, .. }, 0x00) => Some(encode_block(*value, [0; 3], *slot)),
        (SimCard::FeliCaLiteS { block0, .. }, 0x00) => Some(*block0),
        (SimCard::FeliCaLiteS { .. }, 0x86 | 0x90 | 0x91) => Some([block; 16]),
        _ => None,
    }
}

impl State {
    /// The answer to one request: `(status, payload)`, or `None` when the reader does not answer.
    fn answer(&mut self, request: &Request) -> Option<(u8, Vec<u8>)> {
        let (generation, led_board) = (self.generation, self.led_board);
        let port = &mut self.port;
        let payload = request.payload.as_slice();
        let ok = |payload: Vec<u8>| Some((0, payload));
        let refuse = Some((1, Vec::new()));
        match request.cmd {
            cmd::TO_NORMAL => {
                (port.polling, port.selected, port.auth, port.updater) = (false, false, 0, false);
                ok(Vec::new())
            }
            cmd::GET_FW_VERSION if generation == 1 => ok(b"TN32MSEC003S F/W Ver1.2\0".to_vec()),
            cmd::GET_FW_VERSION => ok(vec![0x94]),
            cmd::GET_HW_VERSION => ok(match generation {
                1 => b"TN32MSEC003S H/W Ver3.0\0".to_vec(),
                2 => b"837-15286-01 Ver3.0\0".to_vec(),
                _ => b"837-15396-01 Ver3.0\0".to_vec(),
            }),
            cmd::LED_PROBE => led_board.then(|| (0, Vec::new())),
            cmd::LED_BOARD_INFO if led_board => {
                let mut info = b"15084\xFF".to_vec();
                info.push(SIM_LED_VERSION);
                ok(info)
            }
            cmd::LED_BOARD_INFO => None,
            cmd::SET_KEY_A => {
                port.key_a.copy_from_slice(payload.get(..6)?);
                ok(Vec::new())
            }
            cmd::SET_KEY_B => {
                port.key_b.copy_from_slice(payload.get(..6)?);
                ok(Vec::new())
            }
            cmd::START_POLLING => {
                port.polling = true;
                port.store_sector = None;
                ok(Vec::new())
            }
            cmd::STOP_POLLING => {
                port.polling = false;
                port.store_sector = None;
                ok(Vec::new())
            }
            cmd::CARD_DETECT if !port.polling => refuse,
            cmd::CARD_DETECT => ok(match &port.card {
                None => vec![0],
                Some(SimCard::Store { id, .. }) => [&[1, 0x10, id.len() as u8][..], id].concat(),
                Some(card) => match (card.uid(), card.idm()) {
                    (Some(uid), _) => [&[1, 0x10, 4][..], &uid].concat(),
                    (_, Some(idm)) => [&[1, 0x20, 16][..], &idm, &card.pmm()].concat(),
                    _ => vec![0],
                },
            }),
            cmd::SELECT_CARD => match port.card.as_ref().and_then(SimCard::uid) {
                Some(uid) if payload.get(..4) == Some(&uid[..]) => {
                    port.selected = true;
                    port.auth = 0;
                    ok(Vec::new())
                }
                _ => refuse,
            },
            cmd::HALT_CARD => {
                port.selected = false;
                port.auth = 0;
                ok(Vec::new())
            }
            cmd::AUTH_KEY_A if matches!(port.card, Some(SimCard::Store { .. })) => {
                let sector = match (&port.card, payload.get(..5)) {
                    (Some(SimCard::Store { id, .. }), Some(&[a, b, c, d, block]))
                        if port.polling
                            && port.key_a == *b"thinca"
                            && id[id.len() - 4..] == [a, b, c, d]
                            && block % 4 == 3
                            && (1..=3).contains(&(block / 4)) =>
                    {
                        Some(block / 4)
                    }
                    _ => None,
                };
                port.store_sector = sector;
                if sector.is_some() { ok(Vec::new()) } else { refuse }
            }
            cmd::READ_BLOCK if matches!(port.card, Some(SimCard::Store { .. })) => {
                match (&port.card, payload.get(..5), port.store_sector) {
                    (Some(SimCard::Store { id, blocks }), Some(&[a, b, c, d, block]), Some(sector))
                        if id[id.len() - 4..] == [a, b, c, d] && block / 4 == sector && block % 4 < 3 =>
                    {
                        match blocks.get(usize::from(sector - 1) * 3 + usize::from(block % 4)) {
                            Some(data) => ok(data.to_vec()),
                            None => refuse,
                        }
                    }
                    _ => refuse,
                }
            }
            cmd::AUTH_KEY_B | cmd::AUTH_KEY_A => {
                let (is_b, trailer) = (request.cmd == cmd::AUTH_KEY_B, payload.get(4) == Some(&3));
                let fits = match (&port.card, is_b) {
                    (Some(SimCard::Aime { .. }), true) => port.key_b == KEY_B,
                    (Some(SimCard::Banapass { .. }), false) => port.key_a == KEY_A,
                    _ => false,
                };
                if port.selected && trailer && fits {
                    port.auth = if is_b { 1 } else { 2 };
                    ok(Vec::new())
                } else {
                    refuse
                }
            }
            cmd::READ_BLOCK => match (&port.card, port.auth, payload.get(4)) {
                (Some(card), 1 | 2, Some(&block)) if block < 3 => ok(card.mifare_block(block).to_vec()),
                _ => refuse,
            },
            cmd::FELICA_THROUGH => {
                let card = port.card.clone()?;
                match felica_answer(&card, &mut port.rc, payload) {
                    Some(answer) => ok(answer),
                    None => refuse,
                }
            }
            cmd::TO_UPDATER => {
                port.updater = true;
                port.fw_blocks = 0;
                ok(Vec::new())
            }
            cmd::HEX_DATA if port.updater => Some((
                match generation {
                    1 => 0,
                    2 => 16,
                    _ => 32,
                },
                Vec::new(),
            )),
            cmd::BIN_START if port.updater => ok(Vec::new()),
            cmd::BIN_BLOCK if port.updater => {
                port.fw_blocks += 1;
                Some((if port.fw_blocks == 48 { 8 } else { 0 }, Vec::new()))
            }
            cmd::LED_BOOT | cmd::LED_DATA | cmd::LED_COLOUR | cmd::LED_BLINK => ok(Vec::new()),
            _ => refuse,
        }
    }
}

/// The open port of one reader. Dropping it closes the reader.
struct AimePort(Rc<RefCell<State>>);

impl std::fmt::Debug for AimePort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AimePort").field("com", &self.0.borrow().com).finish()
    }
}

impl Drop for AimePort {
    fn drop(&mut self) {
        let port = &mut self.0.borrow_mut().port;
        port.open = false;
        port.out.clear();
    }
}

impl SerialPort for AimePort {
    fn bytes_to_read(&mut self) -> io::Result<usize> {
        Ok(self.0.borrow().port.out.len())
    }

    fn bytes_to_write(&mut self) -> io::Result<usize> {
        Ok(0)
    }

    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut state = self.0.borrow_mut();
        if !state.port.open {
            return Err(io::Error::new(io::ErrorKind::NotConnected, format!("COM{}: not open", state.com)));
        }
        let count = buf.len().min(state.port.out.len());
        for (slot, byte) in buf.iter_mut().zip(state.port.out.drain(..count)) {
            *slot = byte;
        }
        Ok(count)
    }

    fn write(&mut self, data: &[u8]) -> io::Result<()> {
        let mut state = self.0.borrow_mut();
        if !state.port.open {
            return Err(io::Error::new(io::ErrorKind::NotConnected, format!("COM{}: not open", state.com)));
        }
        let requests = state.port.asm.push(data);
        for request in requests {
            let com = state.com;
            state.log.push(SimRequest { com, request: request.clone() });
            let silent = state.port.silent;
            let answer = state.answer(&request);
            if let (Some((status, payload)), false) = (answer, silent) {
                let frame = response(request.dst, request.seq, request.cmd, status, &payload);
                state.port.out.extend(wire::stuff(&frame));
            }
        }
        Ok(())
    }

    fn drain(&mut self, _timeout: std::time::Duration) -> io::Result<bool> {
        Ok(true)
    }

    fn purge(&mut self) -> io::Result<()> {
        let mut state = self.0.borrow_mut();
        state.port.out.clear();
        state.port.asm = RequestAssembler::default();
        Ok(())
    }
}

/// Builds an unescaped response: `E0 len dst seq cmd status paylen payload sum`.
fn response(dst: u8, seq: u8, cmd: u8, status: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(payload.len() + 9);
    frame.extend_from_slice(&[wire::SYNC, (payload.len() + 6) as u8, dst, seq, cmd, status, payload.len() as u8]);
    frame.extend_from_slice(payload);
    let sum = wire::checksum(&frame[1..]);
    frame.push(sum);
    frame
}

/// Encrypts the plain 8 bytes of a Banapass block (the inverse of `decode_block`).
fn encode_block(value: u32, extra: [u8; 3], slot: usize) -> [u8; 16] {
    use blowfish::Blowfish;
    use blowfish::cipher::{BlockEncrypt, KeyInit};
    let mut plain = [0u8; 8];
    plain[..4].copy_from_slice(&value.to_le_bytes());
    plain[4..7].copy_from_slice(&extra);
    plain[7] = plain[..7].iter().fold(0, |sum, &byte| sum ^ byte);
    let mut bytes = [0u8; 8];
    for half in 0..2 {
        let word = u32::from_le_bytes([plain[half * 4], plain[half * 4 + 1], plain[half * 4 + 2], plain[half * 4 + 3]]);
        bytes[half * 4..half * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    let cipher: Blowfish = Blowfish::new_from_slice(&BANAPASS_KEYS[slot]).expect("56-byte key");
    cipher.encrypt_block((&mut bytes).into());
    let mut block = [0u8; 16];
    block[..8].copy_from_slice(&[0x00, 0x02, b'N', b'B', b'G', b'I', b'C', b'0' + slot as u8]);
    for half in 0..2 {
        let word = u32::from_be_bytes([bytes[half * 4], bytes[half * 4 + 1], bytes[half * 4 + 2], bytes[half * 4 + 3]]);
        block[8 + half * 4..12 + half * 4].copy_from_slice(&word.to_le_bytes());
    }
    block
}

#[cfg(test)]
mod tests {
    use super::{encode_block, response};
    use crate::aime::code::{DecodeError, Decoded, decode_block};

    #[test]
    fn encode_is_the_inverse_of_decode() {
        let encoded = encode_block(0x00AB_CDEF, [1, 2, 3], 6);
        assert_eq!(decode_block(&encoded), Ok(Decoded { value: 0x00AB_CDEF, slot: 6 }));
        let mut broken = encoded;
        broken[9] ^= 1;
        assert_eq!(decode_block(&broken), Err(DecodeError::Corrupt));
        let mut untagged = encoded;
        untagged[7] = b'9';
        assert_eq!(decode_block(&untagged), Err(DecodeError::NoSlot));
    }

    #[test]
    fn response_matches_the_spec_example() {
        assert_eq!(
            response(0, 5, 0x42, 0, &[0x01, 0x10, 0x04, 0x04, 0xD0, 0xE0, 0x11]),
            [0xE0, 0x0D, 0x00, 0x05, 0x42, 0x00, 0x07, 0x01, 0x10, 0x04, 0x04, 0xD0, 0xE0, 0x11, 0x35]
        );
    }
}
