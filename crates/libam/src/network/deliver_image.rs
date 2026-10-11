//! The downloadHelper image library (`spec/libam/network.md` D.10.3 / D.10.4): the image file layout, the
//! BootId block, segment CRC-32, the HMAC-SHA1 signature and the file-name grammar.

use aes::Aes128;
use cbc::cipher::block_padding::NoPadding;
use cbc::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use hmac::{Hmac, Mac};
use sha1::Sha1;

/// Size of the BootId block at the start of an image file.
pub const BOOTID_SIZE: usize = 10240;
/// Offset of the HMAC signature area.
pub const SIGNATURE_OFFSET: usize = 10240;
#[cfg(test)]
/// Size of the signature area (only its first 20 bytes are compared).
const SIGNATURE_AREA: usize = 512;
/// Offset of the segment CRC table.
pub const CRC_TABLE_OFFSET: usize = 10752;
/// Segments covered by the signature.
pub const SIGNED_SEGMENTS: usize = 8;

/// AES-128 key shared by the BootId block and the ICF (`0x140320950` over the three 32-byte tables).
const AES_KEY: [u8; 16] =
    [0x09, 0xca, 0x5e, 0xfd, 0x30, 0xc9, 0xaa, 0xef, 0x38, 0x04, 0xd0, 0xa7, 0xe3, 0xfa, 0x71, 0x20];
/// Matching CBC initial value.
const AES_IV: [u8; 16] =
    [0xb1, 0x55, 0xc2, 0x2c, 0x2e, 0x7f, 0x04, 0x91, 0xfa, 0x7f, 0x0f, 0xdc, 0x21, 0x7a, 0xff, 0x90];
/// HMAC-SHA1 secret of the image signature: `K[j] = B[(30+j)&63] ^ C[j] ^ A[(40+j)&63]` (spec D.10.3; the
/// test derives it from the three source arrays).
const HMAC_KEY: [u8; 64] = hex64(
    "e1bdcb2d5e9ed3b5de234364dfa4d126849edff769fc6c28fba5f43bc482bd7479d676afce8188e1d3a6852f4ebce45cde46bd15e8ee5fe84d197f945a54518f",
);

/// Decodes 128 hex digits at compile time.
const fn hex64(text: &str) -> [u8; 64] {
    const fn digit(byte: u8) -> u8 {
        match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            _ => panic!("not a hex digit"),
        }
    }
    let bytes = text.as_bytes();
    assert!(bytes.len() == 128);
    let mut out = [0u8; 64];
    let mut i = 0;
    while i < 64 {
        out[i] = digit(bytes[2 * i]) << 4 | digit(bytes[2 * i + 1]);
        i += 1;
    }
    out
}

/// Why an image structure was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ImageError {
    /// The data is not a whole number of AES blocks or too short.
    #[error("image block has the wrong size")]
    Size,
    /// The decrypted block failed its CRC or magic check.
    #[error("BootId block is corrupt")]
    Corrupt,
}

/// Decrypts `data` (whole blocks) in place, one CBC stream starting at the shared IV.
pub fn aes_cbc_decrypt(data: &mut [u8]) -> Result<(), ImageError> {
    cbc::Decryptor::<Aes128>::new(&AES_KEY.into(), &AES_IV.into())
        .decrypt_padded_mut::<NoPadding>(data)
        .map(drop)
        .map_err(|_| ImageError::Size)
}

/// Encrypts `data` (whole blocks) in place; the inverse of [`aes_cbc_decrypt`].
pub fn aes_cbc_encrypt(data: &mut [u8]) -> Result<(), ImageError> {
    let len = data.len();
    cbc::Encryptor::<Aes128>::new(&AES_KEY.into(), &AES_IV.into())
        .encrypt_padded_mut::<NoPadding>(data, len)
        .map(drop)
        .map_err(|_| ImageError::Size)
}

/// Standard CRC-32 of `data`.
pub fn crc32(data: &[u8]) -> u32 {
    crc32fast::hash(data)
}

/// The kind of an image (also the ICF `kind` byte).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageType {
    /// `.pack`.
    Os = 0,
    /// `.app`.
    App = 1,
    /// `.opt`.
    Option = 2,
}

impl ImageType {
    /// From the BootId / ICF byte.
    pub fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Os),
            1 => Some(Self::App),
            2 => Some(Self::Option),
            _ => None,
        }
    }
}

/// The decrypted BootId block fields the engine reads (`D.10.3`); the other bytes are unknown (`D.O4`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootId {
    /// `+13`: non-zero when `+16` holds the 4-character game id.
    pub has_game_id: u8,
    /// `+14`: image type byte.
    pub image_type: u8,
    /// `+16`: game id.
    pub game_id: [u8; 4],
    /// `+20`: timestamp `{u16 year, month, day, hour, minute, second, pad}`.
    pub timestamp: [u8; 8],
    /// `+28`: version.
    pub version: u32,
    /// `+32`: segment count.
    pub segment_count: u64,
    /// `+40`: segment size.
    pub segment_size: u64,
    /// `+64`: platform id.
    pub platform_id: [u8; 3],
    /// `+76`: compared with the file-name information.
    pub word76: u32,
    /// `+80`: compared with the file-name information.
    pub word80: u32,
}

fn le32(block: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(block[at..at + 4].try_into().expect("four bytes"))
}

fn le64(block: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(block[at..at + 8].try_into().expect("eight bytes"))
}

impl BootId {
    /// Decrypts and checks a stored 10 240-byte block (`0x140306CD0` / `0x140306DB0`): the CRC at `+0` covers
    /// `[4, 10240)` and `+8` must read `BTID`.
    pub fn decode(stored: &[u8]) -> Result<Self, ImageError> {
        if stored.len() < BOOTID_SIZE {
            return Err(ImageError::Size);
        }
        let mut block = stored[..BOOTID_SIZE].to_vec();
        aes_cbc_decrypt(&mut block)?;
        if le32(&block, 0) != crc32(&block[4..]) || &block[8..12] != b"BTID" {
            return Err(ImageError::Corrupt);
        }
        Ok(Self {
            has_game_id: block[13],
            image_type: block[14],
            game_id: block[16..20].try_into().expect("four bytes"),
            timestamp: block[20..28].try_into().expect("eight bytes"),
            version: le32(&block, 28),
            segment_count: le64(&block, 32),
            segment_size: le64(&block, 40),
            platform_id: block[64..67].try_into().expect("three bytes"),
            word76: le32(&block, 76),
            word80: le32(&block, 80),
        })
    }

    #[cfg(test)]
    /// Builds the stored (encrypted) block; the inverse of [`Self::decode`].
    pub fn encode(&self) -> Vec<u8> {
        let mut block = vec![0u8; BOOTID_SIZE];
        block[8..12].copy_from_slice(b"BTID");
        block[13] = self.has_game_id;
        block[14] = self.image_type;
        block[16..20].copy_from_slice(&self.game_id);
        block[20..28].copy_from_slice(&self.timestamp);
        block[28..32].copy_from_slice(&self.version.to_le_bytes());
        block[32..40].copy_from_slice(&self.segment_count.to_le_bytes());
        block[40..48].copy_from_slice(&self.segment_size.to_le_bytes());
        block[64..67].copy_from_slice(&self.platform_id);
        block[76..80].copy_from_slice(&self.word76.to_le_bytes());
        block[80..84].copy_from_slice(&self.word80.to_le_bytes());
        let crc = crc32(&block[4..]);
        block[0..4].copy_from_slice(&crc.to_le_bytes());
        aes_cbc_encrypt(&mut block).expect("whole blocks");
        block
    }
}

/// CRC-32 of segment `index` (`0x140324120`): the whole segment in 4096-byte blocks; for segment 0 the bytes
/// `[10240, 10756)` (signature area and the first table entry) are skipped.
pub fn segment_crc(index: usize, segment: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    if index == 0 {
        hasher.update(&segment[..BOOTID_SIZE.min(segment.len())]);
        if segment.len() > CRC_TABLE_OFFSET + 4 {
            hasher.update(&segment[CRC_TABLE_OFFSET + 4..]);
        }
    } else {
        hasher.update(segment);
    }
    hasher.finalize()
}

/// The CRC stored for segment `index` in the header segment's table (`10752 + 4i`).
pub fn stored_segment_crc(header: &[u8], index: usize) -> Option<u32> {
    let at = CRC_TABLE_OFFSET + 4 * index;
    header.get(at..at + 4).map(|bytes| u32::from_le_bytes(bytes.try_into().expect("four bytes")))
}

/// HMAC-SHA1 over `[10752, 8 * segment_size)` of the image head compared with the first 20 bytes at 10240
/// (`0x1403242C0` / `0x140315F00`). `head` holds at least the first 8 segments.
pub fn verify_signature(head: &[u8], segment_size: usize) -> bool {
    let end = SIGNED_SEGMENTS * segment_size;
    if segment_size == 0 || head.len() < end || end <= CRC_TABLE_OFFSET {
        return false;
    }
    let mut mac = Hmac::<Sha1>::new_from_slice(&HMAC_KEY).expect("any key length");
    mac.update(&head[CRC_TABLE_OFFSET..end]);
    mac.verify_slice(&head[SIGNATURE_OFFSET..SIGNATURE_OFFSET + 20]).is_ok()
}

#[cfg(test)]
/// Writes the signature of a head buffer in place (image builders and tests).
pub fn sign_head(head: &mut [u8], segment_size: usize) {
    let end = SIGNED_SEGMENTS * segment_size;
    let mut mac = Hmac::<Sha1>::new_from_slice(&HMAC_KEY).expect("any key length");
    mac.update(&head[CRC_TABLE_OFFSET..end]);
    let tag = mac.finalize().into_bytes();
    head[SIGNATURE_OFFSET..SIGNATURE_OFFSET + 20].copy_from_slice(&tag);
    head[SIGNATURE_OFFSET + 20..SIGNATURE_OFFSET + SIGNATURE_AREA].fill(0);
}

/// A parsed image file name (`amsImageParseFilename`, D.10.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageName {
    /// `.pack` / `.app` / `.opt`.
    pub kind: ImageType,
    /// Platform id (3 characters, `.pack`) or game id (4 characters).
    pub id: String,
    /// `(major << 16) | (minor << 8) | patch`, or the four option-id bytes (big-endian packed) for options.
    pub version: u32,
    /// `YYYYMMDDhhmmss` as `{u16 year, month, day, hour, minute, second, pad}`.
    pub timestamp: [u8; 8],
    /// Patch generation.
    pub generation: u8,
    /// Base version of a patch (generation != 0).
    pub base_version: u32,
    /// Declared file size.
    pub file_size: Option<u64>,
    /// Declared segment size.
    pub segment_size: Option<u32>,
}

fn parse_version(token: &str, max_major: u32) -> Option<u32> {
    let mut parts = token.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next()?.parse().ok()?;
    let patch: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || major > max_major || minor > 99 || patch > 99 {
        return None;
    }
    Some((major << 16) | (minor << 8) | patch)
}

/// `YYYYMMDDhhmmss` into the 8-byte timestamp.
pub fn parse_timestamp14(token: &str) -> Option<[u8; 8]> {
    if token.len() != 14 || !token.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let field = |from: usize, to: usize| token[from..to].parse::<u32>().ok();
    let year = u16::try_from(field(0, 4)?).ok()?;
    let (month, day, hour, minute, second) =
        (field(4, 6)?, field(6, 8)?, field(8, 10)?, field(10, 12)?, field(12, 14)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let year = year.to_le_bytes();
    Some([year[0], year[1], month as u8, day as u8, hour as u8, minute as u8, second as u8, 0])
}

/// Parses the last path component of a URL or entry.
pub fn parse_image_filename(path: &str) -> Option<ImageName> {
    let name = path.rsplit('/').next()?;
    let (stem, kind, max_major) = if let Some(stem) = name.strip_suffix(".pack") {
        (stem, ImageType::Os, 9999)
    } else if let Some(stem) = name.strip_suffix(".app") {
        (stem, ImageType::App, 99)
    } else if let Some(stem) = name.strip_suffix(".opt") {
        (stem, ImageType::Option, 99)
    } else {
        return None;
    };
    let tokens: Vec<&str> = stem.split('_').collect();
    if !(4..=7).contains(&tokens.len()) {
        return None;
    }
    let id = tokens[0];
    let id_ok = match kind {
        ImageType::Os => id.len() == 3 && id.bytes().all(|b| b.is_ascii_alphanumeric()),
        _ => id.len() == 4 && id.starts_with(['S', 's']) && id.bytes().all(|b| b.is_ascii_alphanumeric()),
    };
    if !id_ok {
        return None;
    }
    let version = if kind == ImageType::Option {
        let option = tokens[1];
        if option.len() != 4 || !option.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return None;
        }
        u32::from_be_bytes(option.to_ascii_uppercase().as_bytes().try_into().ok()?)
    } else {
        parse_version(tokens[1], max_major)?
    };
    let timestamp = parse_timestamp14(tokens[2])?;
    let generation: u8 = tokens[3].parse().ok()?;
    let mut next = 4;
    let mut base_version = 0;
    if generation != 0 {
        base_version = parse_version(tokens.get(next)?, max_major)?;
        next += 1;
    }
    let (mut file_size, mut segment_size) = (None, None);
    if tokens.len() == next + 2 {
        file_size = Some(tokens[next].parse().ok()?);
        segment_size = Some(tokens[next + 1].parse::<u32>().ok().filter(|size| *size != 0)?);
        next += 2;
    }
    (tokens.len() == next).then(|| ImageName {
        kind,
        id: id.to_ascii_uppercase(),
        version,
        timestamp,
        generation,
        base_version,
        file_size,
        segment_size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;

    #[test]
    fn hmac_secret_matches_the_three_source_arrays() {
        let decode = |text: &str| -> Vec<u8> {
            (0..64).map(|i| u8::from_str_radix(&text[2 * i..2 * i + 2], 16).unwrap()).collect()
        };
        let a = decode(
            "21be3cf2397c9327fa63552ca6a4fbceb5e785d8b102c5ef00014cd470305442b57e5c1018dc06fe9e384a960d7c1af3d8f4857a1ed9f201338e91bb2dcffa77",
        );
        let b = decode(
            "7e43232dcc8f475ac7c5ce5c089ab996831b1e7ac3564ed738a48e3dcdd88c2bfb4cdf8caf356d0122621fd37116e8690026de02fc6e995374a30bf9e054278e",
        );
        let c = decode(
            "f3ae7af78c6e66736bd6e47cdeae52315f794e6a9a316a314348bc6af607ce07a43b5dc04b08bfa0211bc73231e2292967d172da86a4c8fcb6b01b20ccb59aa9",
        );
        let mut derived = String::new();
        for j in 0..64 {
            write!(derived, "{:02x}", b[(30 + j) & 63] ^ c[j] ^ a[(40 + j) & 63]).unwrap();
        }
        let key: String = HMAC_KEY.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(key, derived);
    }

    fn sample() -> BootId {
        BootId {
            has_game_id: 1,
            image_type: 1,
            game_id: *b"SDEZ",
            timestamp: [0xE8, 0x07, 3, 4, 5, 6, 7, 0],
            version: 0x0001_0203,
            segment_count: 12,
            segment_size: 65536,
            platform_id: *b"AAV",
            word76: 0,
            word80: 0,
        }
    }

    #[test]
    fn bootid_roundtrips_and_detects_damage() {
        let stored = sample().encode();
        assert_eq!(BootId::decode(&stored).unwrap(), sample());
        let mut broken = stored;
        broken[100] ^= 1;
        assert_eq!(BootId::decode(&broken), Err(ImageError::Corrupt));
        assert_eq!(BootId::decode(&[0; 16]), Err(ImageError::Size));
    }

    #[test]
    fn segment_zero_crc_skips_signature_area_and_first_entry() {
        let mut segment = vec![7u8; 65536];
        let before = segment_crc(0, &segment);
        segment[SIGNATURE_OFFSET..CRC_TABLE_OFFSET + 4].fill(0xAA);
        assert_eq!(segment_crc(0, &segment), before);
        segment[CRC_TABLE_OFFSET + 4] ^= 1;
        assert_ne!(segment_crc(0, &segment), before);
        assert_eq!(segment_crc(1, &segment), crc32(&segment));
    }

    #[test]
    fn signature_covers_the_table_and_first_segments() {
        let segment_size = 4096 * 4;
        let mut head = vec![3u8; SIGNED_SEGMENTS * segment_size];
        sign_head(&mut head, segment_size);
        assert!(verify_signature(&head, segment_size));
        head[CRC_TABLE_OFFSET + 8] ^= 1;
        assert!(!verify_signature(&head, segment_size));
    }

    #[test]
    fn filenames_follow_the_grammar() {
        let app = parse_image_filename("http://x/y/SDEZ_1.02.03_20240102030405_0.app").unwrap();
        assert_eq!((app.kind, app.id.as_str(), app.version, app.generation), (ImageType::App, "SDEZ", 0x01_02_03, 0));
        assert_eq!(app.timestamp, [0xE8, 0x07, 1, 2, 3, 4, 5, 0]);
        let patch = parse_image_filename("SDEZ_1.02.04_20240102030405_1_1.02.03_1000_4096.app").unwrap();
        assert_eq!((patch.base_version, patch.file_size, patch.segment_size), (0x01_02_03, Some(1000), Some(4096)));
        let option = parse_image_filename("SDEZ_abcd_20240102030405_0.opt").unwrap();
        assert_eq!(option.version, u32::from_be_bytes(*b"ABCD"));
        assert!(parse_image_filename("XDEZ_1.02.03_20240102030405_0.app").is_none());
        assert!(parse_image_filename("SDEZ_100.02.03_20240102030405_0.app").is_none());
        assert!(parse_image_filename("AAV_0001.00.00_20240102030405_0.pack").is_some());
    }
}
