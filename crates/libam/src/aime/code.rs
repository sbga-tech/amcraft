//! The card-side algorithms of the reader driver (`spec/libam/aime.md` part E, 5.3, 5.4 and 5.6): the Banapass
//! block decoder, the access code builder, packed-BCD helpers and the FeliCa Lite authentication.

use blowfish::Blowfish;
use blowfish::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use des::{TdesEde2, TdesEde3};

use super::tables::{BANAPASS_KEYS, ROWS};

/// Why a Banapass block did not decode (`0x14035C040` returns -1 or -2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// No slot tag `NBGICn` matches.
    NoSlot,
    /// Wrong version word or checksum.
    Corrupt,
}

/// A decoded Banapass block: the numeric card id and the key slot that decrypted it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decoded {
    /// The card id `V`.
    pub value: u32,
    /// Key slot index, `0..8`.
    pub slot: usize,
}

/// `0x14035C040`: decodes a 16-byte block (`00 02 "NBGICn" enc8`).
pub fn decode_block(block: &[u8; 16]) -> Result<Decoded, DecodeError> {
    let slot =
        (0..8u8).find(|&n| block[2..7] == *b"NBGIC" && block[7] == b'0' + n).ok_or(DecodeError::NoSlot)? as usize;
    if block[0] != 0x00 || block[1] != 0x02 {
        return Err(DecodeError::Corrupt);
    }
    let cipher: Blowfish = Blowfish::new_from_slice(&BANAPASS_KEYS[slot]).map_err(|_| DecodeError::Corrupt)?;
    // The words of the block are little endian; the cipher works on big-endian words, so swap around it.
    let mut bytes = [0u8; 8];
    for (half, chunk) in block[8..16].chunks_exact(4).enumerate() {
        let word = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        bytes[half * 4..half * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    cipher.decrypt_block((&mut bytes).into());
    let mut plain = [0u8; 8];
    for half in 0..2 {
        let word = u32::from_be_bytes([bytes[half * 4], bytes[half * 4 + 1], bytes[half * 4 + 2], bytes[half * 4 + 3]]);
        plain[half * 4..half * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    if plain[..7].iter().fold(0, |sum, &byte| sum ^ byte) != plain[7] {
        return Err(DecodeError::Corrupt);
    }
    Ok(Decoded { value: u32::from_le_bytes([plain[0], plain[1], plain[2], plain[3]]), slot })
}

/// `0x14035C1A0`: the 20-digit access code of `(V, slot)` as packed BCD.
pub fn access_code(value: u32, slot: usize) -> [u8; 10] {
    let row = &ROWS[slot];
    let value_bytes = value.to_le_bytes();
    let seed_bytes = row.seed.to_le_bytes();
    let chk = value_bytes.iter().chain(&seed_bytes).fold(0u8, |sum, &byte| sum ^ byte);
    let mut bits = [0u8; 56];
    let selector = chk % 11;
    bits[0] = (selector >> 1) & 1;
    bits[1] = selector & 1;
    let field = ((row.seed & 3) << 8) | ((row.seed >> 2) & 0xFF);
    for index in 0..10 {
        bits[2 + index] = ((field >> (9 - index)) & 1) as u8;
    }
    for (index, &byte) in value_bytes.iter().enumerate() {
        for bit in 0..8 {
            bits[16 + index * 8 + bit] = (byte >> (7 - bit)) & 1;
        }
    }
    for bit in 0..8 {
        bits[48 + bit] = (chk >> (7 - bit)) & 1;
    }
    let mut out = [0u8; 56];
    for (index, &bit) in bits.iter().enumerate() {
        out[usize::from(row.perm[index]) % 56] = bit;
    }
    let number = out.iter().fold(0u64, |acc, &bit| (acc << 1) | u64::from(bit));
    let text = format!("{}{:017}", row.prefix, number.wrapping_add(row.addend));
    let digits: Vec<u8> = text.bytes().map(|digit| digit - b'0').collect();
    let mut bcd = [0u8; 10];
    for (index, pair) in digits.chunks_exact(2).enumerate().take(10) {
        bcd[index] = (pair[0] << 4) | pair[1];
    }
    bcd
}

/// `0x14035D370`: decodes a block and builds the access code; `None` on any failure.
pub fn access_code_of_block(block: &[u8; 16]) -> Option<[u8; 10]> {
    decode_block(block).ok().map(|decoded| access_code(decoded.value, decoded.slot))
}

/// Converts 20 ASCII digits to packed BCD (even index = high nibble); `None` when a character is not a digit.
#[allow(dead_code)] // consumed by the AimeLib/AimeFacade layers
pub fn bcd_from_text(text: &[u8]) -> Option<[u8; 10]> {
    if text.len() != 20 || !text.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut bcd = [0u8; 10];
    for (index, pair) in text.chunks_exact(2).enumerate() {
        bcd[index] = ((pair[0] - b'0') << 4) | (pair[1] - b'0');
    }
    Some(bcd)
}

/// The 20 digits of a packed-BCD access code.
#[allow(dead_code)] // consumed by the AimeLib/AimeFacade layers
pub fn bcd_text(code: &[u8; 10]) -> String {
    code.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The FeliCa Lite master key: three DES keys (`0x14071F700`).
const LITE_MASTER_KEY: [u8; 24] = [
    0x6d, 0x20, 0x37, 0xaf, 0x1c, 0x49, 0x23, 0x12, 0xa2, 0x58, 0xdd, 0xba, 0x16, 0x1d, 0x3f, 0x5f, 0xe7, 0x5e, 0x75,
    0x6a, 0x94, 0xea, 0xf2, 0xc2,
];

fn reversed(bytes: [u8; 8]) -> [u8; 8] {
    let mut out = bytes;
    out.reverse();
    out
}

fn ede3(key: &[u8; 24], block: [u8; 8]) -> [u8; 8] {
    let cipher = TdesEde3::new(key.into());
    let mut out = block;
    cipher.encrypt_block((&mut out).into());
    out
}

fn ede2(key_a: [u8; 8], key_b: [u8; 8], block: [u8; 8]) -> [u8; 8] {
    let mut key = [0u8; 16];
    key[..8].copy_from_slice(&key_a);
    key[8..].copy_from_slice(&key_b);
    let cipher = TdesEde2::new((&key).into());
    let mut out = block;
    cipher.encrypt_block((&mut out).into());
    out
}

fn first8(bytes: &[u8]) -> [u8; 8] {
    let mut out = [0u8; 8];
    out.copy_from_slice(&bytes[..8]);
    out
}

/// The three keys of one FeliCa Lite authentication (5.6): card key `CK` and session keys `SK1`, `SK2`.
pub fn lite_keys(id: &[u8; 16], rc: &[u8; 16]) -> ([u8; 16], [u8; 8], [u8; 8]) {
    let zero = ede3(&LITE_MASTER_KEY, [0; 8]);
    let mut k1 = [0u8; 8];
    for index in 0..8 {
        k1[index] = (zero[index] << 1) | zero.get(index + 1).map_or(0, |next| next >> 7);
    }
    if zero[0] & 0x80 != 0 {
        k1[7] ^= 0x1B;
    }
    let x = first8(&id[..8]);
    let mut y = first8(&id[8..]);
    for (byte, key) in y.iter_mut().zip(k1) {
        *byte ^= key;
    }
    let step = |x: [u8; 8]| {
        let mut inner = ede3(&LITE_MASTER_KEY, x);
        for (byte, other) in inner.iter_mut().zip(y) {
            *byte ^= other;
        }
        ede3(&LITE_MASTER_KEY, inner)
    };
    let mut x_flipped = x;
    x_flipped[0] ^= 0x80;
    let mut ck = [0u8; 16];
    ck[..8].copy_from_slice(&step(x));
    ck[8..].copy_from_slice(&step(x_flipped));
    let key_a = reversed(first8(&ck[..8]));
    let key_b = reversed(first8(&ck[8..]));
    let sk1 = ede2(key_a, key_b, reversed(first8(&rc[..8])));
    let mut mixed = sk1;
    for index in 0..8 {
        mixed[7 - index] ^= rc[8 + index];
    }
    let sk2 = ede2(key_a, key_b, mixed);
    (ck, sk1, sk2)
}

/// `0x14035D2C0`: the MAC of the 16-byte ID block `data` under the random challenge `rc`.
pub fn lite_mac(id: &[u8; 16], rc: &[u8; 16], data: &[u8; 16]) -> [u8; 8] {
    let (_, sk1, sk2) = lite_keys(id, rc);
    let mut m = [0u8; 8];
    for index in 0..8 {
        m[7 - index] = data[index] ^ rc[index];
    }
    let mut v = ede2(sk1, sk2, m);
    for index in 0..8 {
        v[7 - index] ^= data[8 + index];
    }
    v = ede2(sk1, sk2, v);
    reversed(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len() / 2).map(|index| u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).unwrap()).collect()
    }

    fn block(text: &str) -> [u8; 16] {
        hex(text).try_into().unwrap()
    }

    #[test]
    fn banapass_vectors_decode_and_build() {
        let vectors = [
            ("00024e4247494330e4faf2a8b9b364c0", 0, 0x0000_0000, "30036992530854399410"),
            ("00024e42474943308d441df4c22f304c", 0, 0x1234_5678, "30028550227180294100"),
            ("00024e4247494330f0adcf193b1f459d", 0, 0x0000_0001, "30037062622573186482"),
            ("00024e4247494331e7fc7fe18526ec82", 1, 0xFFFF_FFFF, "30267301267997378875"),
            ("00024e4247494333a8495836cab346f3", 3, 0xDEAD_BEEF, "30458881161934160218"),
            ("00024e42474943357d96c59ed369307b", 5, 0x0001_E240, "30670442765573123272"),
            ("00024e4247494337d7b768173dd972bc", 7, 0xFFFF_FFFF, "30883490102008002192"),
        ];
        for (text, slot, value, code) in vectors {
            let decoded = decode_block(&block(text)).expect("decodes");
            assert_eq!(decoded, Decoded { value, slot }, "{text}");
            assert_eq!(bcd_text(&access_code(value, slot)), code, "{text}");
        }
    }

    #[test]
    fn lite_authentication_vectors() {
        let id: [u8; 16] = block("0102030405060708a1a2a3a4a5a6a7a8");
        let rc: [u8; 16] = block("000102030405060708090a0b0c0d0e0f");
        let (ck, sk1, sk2) = lite_keys(&id, &rc);
        assert_eq!(ck.to_vec(), hex("302f30b09dcc1d575a262f4cd399b23b"));
        assert_eq!(sk1.to_vec(), hex("3ce472e2f82f7a09"));
        assert_eq!(sk2.to_vec(), hex("4ad00d3a0bdc3f05"));
        assert_eq!(lite_mac(&id, &rc, &id).to_vec(), hex("a5f5402294c57324"));
        let id: [u8; 16] = block("0123456789abcdef0011223344556677");
        let rc: [u8; 16] = block("f0e1d2c3b4a5968778695a4b3c2d1e0f");
        assert_eq!(lite_keys(&id, &rc).0.to_vec(), hex("3fa30c79d76f44b077f2f90dea2f93db"));
        assert_eq!(lite_mac(&id, &rc, &id).to_vec(), hex("523bf01770771937"));
    }

    #[test]
    fn bcd_text_round_trip() {
        let code = bcd_from_text(b"01234567890123456789").unwrap();
        assert_eq!(bcd_text(&code), "01234567890123456789");
        assert!(bcd_from_text(b"0123456789012345678x").is_none());
    }
}
