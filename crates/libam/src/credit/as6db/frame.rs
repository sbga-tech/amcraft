//! AS-6DB framing: header, the two CRC-8 variants, the 16-bit LFSR keystream and the receive validator
//! (`credit.md` part II 3.4 to 3.6).
//!
//! The CRC-8 variants are plain table CRCs (poly `0x07` and `0x85`, init 0, no reflection, no final xor), so the
//! `crc` crate computes them; the keystream, the frame layout and the receive resynchronisation are specific to
//! this selector protocol and hand-written.

use crc::{Algorithm, CRC_8_SMBUS, Crc};

/// First sync byte.
pub const SYNC0: u8 = 0x4A;
/// Second sync byte.
pub const SYNC1: u8 = 0xFC;
/// Bus address of the master (the credit module).
pub const MASTER: u8 = 0x01;

/// Plain mode: poly `0x07` (the table at `0x140747D70`).
const PLAIN: Crc<u8> = Crc::<u8>::new(&CRC_8_SMBUS);
/// Encrypted mode: poly `0x85` (the table at `0x140747E70`).
const SCRAMBLED: Crc<u8> = Crc::<u8>::new(&Algorithm {
    width: 8,
    poly: 0x85,
    init: 0,
    refin: false,
    refout: false,
    xorout: 0,
    check: 0,
    residue: 0,
});

/// Command byte of SETUP.
pub const CMD_SETUP: u8 = 0x05;
/// Command byte of CONTROL.
pub const CMD_CONTROL: u8 = 0x06;
/// Command byte of STATUS.
pub const CMD_STATUS: u8 = 0x07;
/// Command byte of INFO.
pub const CMD_INFO: u8 = 0x09;
/// Command byte of RESET (broadcast and addressed).
pub const CMD_RESET: u8 = 0x10;
/// Command byte of the LOCK broadcast.
pub const CMD_LOCK: u8 = 0x04;
/// Command byte of the master's ACK to INFO/STATUS data.
pub const CMD_ACK: u8 = 0x01;

/// Response code: ACK.
pub const RESP_ACK: u8 = 1;
/// Response code: NAK.
pub const RESP_NAK: u8 = 2;
/// Response code: STATUS data.
pub const RESP_STATUS: u8 = 0x08;
/// Response code: INFO data.
pub const RESP_INFO: u8 = 0x0A;

/// One step of the keystream LFSR (`0x140403CC0`): feedback is the xor of bits 4, 5, 8 and 10.
pub const fn step(state: u16) -> u16 {
    let feedback = ((state >> 4) ^ (state >> 5) ^ (state >> 8) ^ (state >> 10)) & 1;
    (state >> 1) | (feedback << 15)
}

/// Xors `buf` with the keystream, advancing `state`. Byte `i` uses the high byte of the state when `i` is even
/// (after two steps) and the low byte when it is odd; a zero state yields an all-zero stream.
pub fn scramble(state: &mut u16, buf: &mut [u8]) {
    let mut low = 0;
    for (index, byte) in buf.iter_mut().enumerate() {
        if index % 2 == 0 {
            *state = step(step(*state));
            low = *state as u8;
            *byte ^= (*state >> 8) as u8;
        } else {
            *byte ^= low;
        }
    }
}

/// How a frame is protected on the wire.
#[derive(Debug)]
pub enum Key<'a> {
    /// Poly-7 CRC, no scrambling (seed 0, broadcasts, NEW_SEED).
    Plain,
    /// Poly-0x85 CRC over the plaintext, then bytes `seq..=crc` scrambled with the running state.
    Scrambled(&'a mut u16),
}

/// Builds a frame: `4A FC dst 01 len seq body.. crc`.
pub fn build(dst: u8, seq: u8, body: &[u8], key: Key<'_>) -> Vec<u8> {
    let len = body.len() as u8;
    let mut frame = Vec::with_capacity(body.len() + 7);
    frame.extend_from_slice(&[SYNC0, SYNC1, dst, MASTER, len, seq]);
    frame.extend_from_slice(body);
    match key {
        Key::Plain => frame.push(PLAIN.checksum(&frame[5..])),
        Key::Scrambled(state) => {
            frame.push(SCRAMBLED.checksum(&frame[5..]));
            scramble(state, &mut frame[5..]);
        }
    }
    frame
}

/// NEW_SEED request (`0x1404081D0`): `len` is 0 yet the frame carries the seed as `lo, hi`; the poly-7 CRC covers
/// `seq, lo, hi` and the frame is never scrambled.
pub fn new_seed(dst: u8, seq: u8, seed: u16) -> [u8; 9] {
    let [lo, hi] = seed.to_le_bytes();
    let mut frame = [SYNC0, SYNC1, dst, MASTER, 0, seq, lo, hi, 0];
    frame[8] = PLAIN.checksum(&frame[5..8]);
    frame
}

/// The SETUP body: command byte then the 17 setup bytes.
pub fn setup_body(setup: &[u8; 17]) -> [u8; 18] {
    let mut body = [0; 18];
    body[0] = CMD_SETUP;
    body[1..].copy_from_slice(setup);
    body
}

/// Verifies a complete frame (`len + 7` bytes). With a running `state` the bytes from the sequence on are
/// descrambled first; on success the frame is left in plaintext and the state advanced, on failure both are
/// untouched.
pub fn open(frame: &mut [u8], state: Option<&mut u16>) -> bool {
    let len = usize::from(frame[4]);
    if frame.len() != len + 7 {
        return false;
    }
    match state {
        None => PLAIN.checksum(&frame[5..6 + len]) == frame[6 + len],
        Some(state) => {
            let mut scratch = [0u8; 264];
            let body = &mut scratch[..len + 2];
            body.copy_from_slice(&frame[5..]);
            let mut next = *state;
            scramble(&mut next, body);
            if SCRAMBLED.checksum(&body[..len + 1]) == body[len + 1] {
                frame[5..].copy_from_slice(body);
                *state = next;
                true
            } else {
                false
            }
        }
    }
}

/// The key material the receive validator needs (`B+128` receive seed, `B+126` running state).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RxKey {
    /// Non-zero when responses are scrambled.
    pub seed: u16,
    /// The running keystream state.
    pub state: u16,
}

/// What a validated response changes in the key material.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Commit {
    /// New receive seed word.
    pub seed: u16,
    /// New running state.
    pub state: u16,
}

/// Result of validating the newest byte of a response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RxVerdict {
    /// More bytes are needed.
    Incomplete,
    /// The newest byte cannot belong to a response.
    Reject,
    /// A complete frame whose CRC matched. `Some` commits the key material; `None` is a NAK to a NEW_SEED that
    /// was still protected with the old key.
    Complete(Option<Commit>),
}

/// Validates the byte at the end of `buf` (`0x1404041C0`). On a complete frame the buffer is left descrambled.
///
/// `new_seed` is the seed carried by the NEW_SEED request being answered: the response is protected with it and
/// the initial state is its byte swap.
pub fn check_rx(buf: &mut [u8], addr: u8, expect_len: u8, key: RxKey, new_seed: Option<u16>) -> RxVerdict {
    let n = buf.len() - 1;
    let byte = buf[n];
    let accept = |ok: bool| if ok { RxVerdict::Incomplete } else { RxVerdict::Reject };
    match n {
        0 => accept(byte == SYNC0),
        1 => accept(byte == SYNC1),
        2 => accept(byte == MASTER),
        3 => accept(byte == addr),
        4 => accept(byte == expect_len || byte == 4),
        5 => RxVerdict::Incomplete,
        _ => {
            let len = usize::from(buf[4]);
            if n >= len + 7 {
                return RxVerdict::Reject;
            }
            if n < len + 6 {
                return RxVerdict::Incomplete;
            }
            let (seed, mut state) = match new_seed {
                Some(seed) => (seed, seed.swap_bytes()),
                None => (key.seed, key.state),
            };
            let matched = if seed == 0 { open(buf, None) } else { open(buf, Some(&mut state)) };
            if matched {
                return RxVerdict::Complete(Some(Commit { seed, state }));
            }
            if new_seed.is_some() && len == 4 {
                let mut old = key.state;
                let old_ok = if key.seed != 0 { open(buf, Some(&mut old)) } else { open(buf, None) };
                if old_ok && buf[6] == RESP_NAK {
                    return RxVerdict::Complete(None);
                }
            }
            RxVerdict::Reject
        }
    }
}

/// Feeds one received byte: validates, and on a rejection drops everything before the next sync byte and
/// re-validates the remainder from the start (`0x1404040D0`).
pub fn feed_rx(buf: &mut Vec<u8>, byte: u8, addr: u8, expect_len: u8, key: RxKey, new_seed: Option<u16>) -> RxVerdict {
    buf.push(byte);
    'resync: loop {
        for end in 1..=buf.len() {
            match check_rx(&mut buf[..end], addr, expect_len, key, new_seed) {
                RxVerdict::Incomplete => {}
                RxVerdict::Complete(commit) => return RxVerdict::Complete(commit),
                RxVerdict::Reject => match buf.iter().skip(1).position(|&b| b == SYNC0) {
                    Some(offset) => {
                        buf.drain(..offset + 1);
                        continue 'resync;
                    }
                    None => {
                        buf.clear();
                        return RxVerdict::Incomplete;
                    }
                },
            }
        }
        return RxVerdict::Incomplete;
    }
}
