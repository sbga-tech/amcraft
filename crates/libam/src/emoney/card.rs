//! The store authentication card (`spec/libam/payment.md` PayEmSvc section 6): the card
//! header check and the card decryption. The reader that produces a [`CardRead`] is
//! [`super::aime_reader::AimeStoreCardReader`].

use aes::Aes256;
use cbc::Decryptor;
use cbc::cipher::{BlockDecryptMut, KeyIvInit, block_padding::NoPadding};
use hmac::{Hmac, Mac};
use sha2::Sha256;

/// `0x1406DF550` XOR `0x1C`: the HMAC key (6.3 step 2).
pub(crate) const KEY_TABLE: [u8; 64] = [
    0x2f, 0x14, 0x5e, 0xcf, 0x4f, 0x93, 0xd1, 0xdf, 0x0e, 0xf8, 0x88, 0x7f, 0xf8, 0xa6, 0xed, 0x7b, 0x64, 0x88, 0xe8,
    0xdc, 0xfc, 0xce, 0x99, 0x0c, 0x92, 0x0d, 0xf9, 0x1b, 0xb1, 0xd3, 0x3a, 0x5e, 0x6a, 0x33, 0x14, 0x8a, 0x53, 0x66,
    0xe7, 0x3d, 0x25, 0xef, 0x68, 0xb1, 0x21, 0x09, 0x88, 0x64, 0xb1, 0x60, 0x49, 0x07, 0xdd, 0xd8, 0x00, 0x66, 0x4a,
    0xa3, 0x43, 0x01, 0x1f, 0x1a, 0xca, 0x4e,
];

/// What a card read hands back: the card ID and the nine 16-byte blocks of 6.1 (header, then eight ciphertext
/// blocks) read with the key `"thinca"` from sectors 1..3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardRead {
    /// The card ID bytes (the last four are the key diversifier `uid4`).
    pub uid: Vec<u8>,
    /// The blocks `(1,0), (1,1), (1,2), (2,0) .. (3,2)`.
    pub blocks: Vec<[u8; 16]>,
}

/// One answer of the reader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CardPoll {
    /// No card yet.
    Waiting,
    /// A card was detected and its blocks read.
    Read(CardRead),
    /// A card was detected but the blocks could not be read (the reader re-arms CardDetect).
    ReadFailed,
    /// The reader failed (polling could not start, or the detection failed).
    Failed,
}

/// The reader lease the e-money module sees (`spec/libam/payment.md` DL.9: the lease object
/// `{owner, refcount, u8 present @+16, u8 state @+17}` arbitrating the Aime reader between the modules).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lease {
    /// No Aime sequence holds the reader (`0x140256420`): e-money may take it; the `a6` input of the module.
    pub free: bool,
    /// Byte `+16`: the Aime module has probed the reader unit.
    pub present: bool,
    /// Byte `+17 == 3` (`0x140256480`): the reader is initialised and takes commands.
    pub ready: bool,
}

impl Lease {
    /// No reader unit: nothing probed it.
    pub const ABSENT: Self = Self { free: true, present: false, ready: false };
}

/// The fields of a decrypted card (6.2).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StoreCard {
    /// `storeCardId` (16 characters).
    pub store_card_id: String,
    /// `merchantCode` (20 characters).
    pub merchant_code: String,
    /// `storeBranchNumber` text (5 characters).
    pub branch: String,
    /// The auxiliary code (16 characters), the second argument of `initAuthTerm`.
    pub aux: String,
}

/// Why a card was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardRefusal {
    /// The header block is not `'T' 'C' kind nonzero` (6.1 state 2).
    Header,
    /// The card type differs from the expected one ("カードタイプが一致していません").
    Type,
    /// The decryption failed ("ただいまお取扱いできません").
    Decrypt,
}

/// 6.1 state 2: the card type of the header block, `None` for a bad header.
pub fn header_kind(header: &[u8; 16]) -> Option<u8> {
    (header[0] == b'T' && header[1] == b'C' && matches!(header[2], 2 | 3) && header[3] != 0).then_some(header[2])
}

/// 6.3: decrypts the eight ciphertext blocks (plus the zero tail of the 256-byte buffer, 144 bytes in all).
pub fn decrypt(read: &CardRead) -> Option<Vec<u8>> {
    if read.uid.len() < 4 || read.blocks.len() < 9 {
        return None;
    }
    let uid4 = &read.uid[read.uid.len() - 4..];
    let message: Vec<u8> = uid4.iter().copied().cycle().take(32).collect();
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&KEY_TABLE).ok()?;
    mac.update(&message);
    let digest = mac.finalize().into_bytes();
    let mut iv = [0u8; 16];
    for (index, byte) in iv.iter_mut().enumerate() {
        *byte = digest[index] ^ digest[index + 16];
    }
    let mut buffer = Vec::with_capacity(144);
    for block in &read.blocks[1..9] {
        buffer.extend_from_slice(block);
    }
    buffer.extend_from_slice(&[0u8; 16]);
    Decryptor::<Aes256>::new_from_slices(&digest, &iv).ok()?.decrypt_padded_mut::<NoPadding>(&mut buffer).ok()?;
    Some(buffer)
}

fn field(plain: &[u8], start: usize, len: usize) -> String {
    let bytes = &plain[start..start + len];
    let end = bytes.iter().position(|&byte| byte == 0).unwrap_or(len);
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// 6.1 state 4 and 6.2: header check, type check, decryption and field split.
pub fn read_store_card(read: &CardRead, expected_kind: u8) -> Result<StoreCard, CardRefusal> {
    let header = read.blocks.first().ok_or(CardRefusal::Header)?;
    let kind = header_kind(header).ok_or(CardRefusal::Header)?;
    if kind != expected_kind {
        return Err(CardRefusal::Type);
    }
    let plain = decrypt(read).ok_or(CardRefusal::Decrypt)?;
    Ok(StoreCard {
        store_card_id: field(&plain, 0, 16),
        merchant_code: field(&plain, 16, 20),
        branch: field(&plain, 36, 5),
        aux: field(&plain, 48, 16),
    })
}
