//! The installer index file (ICF) reader (`spec/libam/storage.md` 4.10): where the file is, the three-file
//! recovery, the AES-128-CBC stream, the CRC chain, and the filtering/sorting of `amAppImageInit` (4.10.8).
//!
//! The internal result codes are the original's (4.12): `-1` generic, `+1` locked, `-3` invalid, `-5` not found,
//! `-6` corrupt, `-9` registry, `-14` too many records.

use std::io::Read;
use std::path::Path;

use aes::Aes128;
use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::NoPadding};

use crate::registry_keys::MOUNT_KEY;
use backend::{Backend, Fs, KeyMode, RegValue};

type Decryptor = cbc::Decryptor<Aes128>;

const X: [u8; 32] = [
    0xf1, 0xdb, 0x35, 0x9d, 0x7a, 0x6e, 0x69, 0x5a, 0x52, 0x67, 0x6c, 0x3a, 0x07, 0x79, 0xba, 0x23, 0xe1, 0xca, 0x55,
    0x2b, 0x6d, 0x45, 0x09, 0x1b, 0x9f, 0x4d, 0x4f, 0x81, 0x28, 0x75, 0x1c, 0xab,
];
const Y: [u8; 32] = [
    0xff, 0x8b, 0x48, 0xa5, 0xfd, 0xd9, 0xae, 0xee, 0xe9, 0x02, 0x79, 0xcf, 0x78, 0x2f, 0xfa, 0x30, 0x25, 0x25, 0xbf,
    0xd7, 0x2d, 0x3d, 0x69, 0x92, 0xd7, 0x4e, 0xa2, 0x35, 0xfb, 0x83, 0x9a, 0x5e,
];
const Z: [u8; 32] = [
    0x8d, 0x11, 0x1c, 0x4d, 0xf2, 0xaa, 0x6c, 0x84, 0xd9, 0x64, 0xf2, 0x01, 0x2d, 0x97, 0xbd, 0x84, 0xa7, 0xe1, 0x0a,
    0x39, 0x81, 0xde, 0x13, 0x4f, 0xb0, 0x97, 0xe6, 0x90, 0xd5, 0xb4, 0x91, 0xa0,
];

/// Records a catalogue read accepts (`amAppImageInit` passes capacity 511).
const PROVIDER_CAPACITY: usize = 511;
/// Records of one ICF (the node pool of the original).
const MAX_RECORDS: u32 = 512;
const BLOCK: usize = 64;

/// AES key of the ICF (`0x140307D80`).
pub fn key() -> [u8; 16] {
    std::array::from_fn(|k| X[k & 31] ^ Y[(k + 10) & 31] ^ Z[(k + 20) & 31])
}

/// CBC initialisation vector of the ICF.
pub fn iv() -> [u8; 16] {
    std::array::from_fn(|j| X[(j + 16) & 31] ^ Y[(j + 26) & 31] ^ Z[(j + 4) & 31])
}

/// What an ICF record describes (record byte 4, entry `kind` of 4.2.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// Application image.
    App,
    /// Option image.
    Option,
    /// Operating-system image.
    Os,
}

/// One 20-byte static catalogue entry (4.2.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    /// App, option or OS.
    pub kind: EntryKind,
    /// Timestamp year.
    pub year: u16,
    /// Month, 1-based.
    pub month: u8,
    /// Day of month.
    pub day: u8,
    /// Hour.
    pub hour: u8,
    /// Minute.
    pub minute: u8,
    /// Second.
    pub second: u8,
    /// Raw version (App, OS); 0 for options.
    pub version: u32,
    /// The four option-id bytes (options); zero otherwise.
    pub option_id: [u8; 4],
}

impl Entry {
    /// The sort key of `CompareFunction` (`0x1402CA0D0`).
    fn key(&self) -> u32 {
        match self.kind {
            EntryKind::Option => u32::from_le_bytes(self.option_id),
            EntryKind::App | EntryKind::Os => self.version,
        }
    }
}

/// The three static arrays after `amAppImageInit`.
#[derive(Debug, Default, Clone)]
pub struct Entries {
    /// Applications, ascending by version (at most 10).
    pub apps: Vec<Entry>,
    /// Options, ascending by id value (at most 500).
    pub options: Vec<Entry>,
    /// The installed OS image (at most 1).
    pub os: Vec<Entry>,
}

/// `amAppImageInit` (`0x1402C9C90`): reads the ICF and builds the sorted arrays. `Err` is the original's code
/// (`-5` or `-6`), which makes the catalogue context creation fail.
pub fn init(backend: &mut Backend) -> Result<Entries, i32> {
    let records = match load_records(backend) {
        Ok(records) => records,
        Err(-5) => Vec::new(),
        Err(-14) => return Err(-6),
        Err(_) => return Err(-5),
    };
    let mut entries = Entries::default();
    for record in &records {
        if record[0] != 2 || record[1] != 1 {
            continue;
        }
        let version = u32::from_le_bytes([record[0x20], record[0x21], record[0x22], record[0x23]]);
        let mut entry = Entry {
            kind: EntryKind::App,
            year: u16::from_le_bytes([record[0x24], record[0x25]]),
            month: record[0x26],
            day: record[0x27],
            hour: record[0x28],
            minute: record[0x29],
            second: record[0x2A],
            version,
            option_id: [0; 4],
        };
        match record[4] {
            0 => {
                if !entries.os.is_empty() {
                    return Err(-6);
                }
                entry.kind = EntryKind::Os;
                entries.os.push(entry);
            }
            1 => {
                if entries.apps.len() >= 10 {
                    return Err(-6);
                }
                entries.apps.push(entry);
            }
            2 => {
                if entries.options.len() >= 500 {
                    return Err(-6);
                }
                entry.kind = EntryKind::Option;
                entry.version = 0;
                entry.option_id = [record[0x20], record[0x21], record[0x22], record[0x23]];
                entries.options.push(entry);
            }
            _ => {}
        }
    }
    entries.apps.sort_by_key(Entry::key);
    entries.options.sort_by_key(Entry::key);
    Ok(entries)
}

/// The provider `0x1402F3FF0` + `amsIcfBackupLoad` in read-only mode: the 64-byte records in file order.
fn load_records(backend: &mut Backend) -> Result<Vec<[u8; BLOCK]>, i32> {
    let amfs = mount_value(backend, "AMFS")?;
    let appdata = mount_value(backend, "APPDATA")?;
    let paths = [
        backend.fs.map(&format!("{amfs}ICF1")),
        backend.fs.map(&format!("{amfs}ICF2")),
        backend.fs.map(&format!("{appdata}ICF3")),
    ];
    let have_third = paths[2].exists();
    let mut result = load(backend.fs.as_ref(), &paths[0]);
    if result.is_ok() && have_third {
        let _ = std::fs::copy(&paths[0], &paths[2]);
    }
    if matches!(result, Err(-5 | -6)) {
        let _ = std::fs::copy(&paths[1], &paths[0]);
        result = load(backend.fs.as_ref(), &paths[0]);
        if result.is_ok() && have_third {
            let _ = std::fs::copy(&paths[0], &paths[2]);
        }
    }
    if matches!(result, Err(-5 | -6)) {
        let _ = std::fs::copy(&paths[2], &paths[0]);
        let _ = std::fs::copy(&paths[2], &paths[1]);
        result = load(backend.fs.as_ref(), &paths[0]);
    }
    let records = result?;
    if records.len() > PROVIDER_CAPACITY {
        return Err(-14);
    }
    Ok(records)
}

/// `0x1402E3DA0` / `0x1402E3F10`: a `REG_SZ` of at most 15 characters.
fn mount_value(backend: &mut Backend, name: &str) -> Result<String, i32> {
    match backend.registry.read(MOUNT_KEY, KeyMode::CreateVolatile, name) {
        Ok(RegValue::Text(text)) if text.encode_utf16().count() <= 15 => Ok(text),
        _ => Err(-9),
    }
}

/// `amsIcfLoad` (`0x140309C40`) of one file: open exclusively, decrypt the single CBC stream, check the size,
/// the count, both CRCs.
fn load(fs: &dyn Fs, path: &Path) -> Result<Vec<[u8; BLOCK]>, i32> {
    let mut file = fs.open_exclusive(path).map_err(|error| match error.raw_os_error() {
        Some(32) => 1,
        _ if error.kind() == std::io::ErrorKind::NotFound => -5,
        _ => -1,
    })?;
    let mut data = Vec::new();
    file.read_to_end(&mut data).map_err(|_| -1)?;
    parse(data)
}

/// Decrypts and validates the ICF image (4.10.5, 4.10.6).
pub fn parse(mut data: Vec<u8>) -> Result<Vec<[u8; BLOCK]>, i32> {
    if data.len() < BLOCK || !data.len().is_multiple_of(16) {
        return Err(-6);
    }
    Decryptor::new(&key().into(), &iv().into()).decrypt_padded_mut::<NoPadding>(&mut data).map_err(|_| -1)?;
    let word = |at: usize| u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]);
    let (crc, size, count, fg_crc) = (word(0), word(4), word(0x10), word(0x20));
    if usize::try_from(size) != Ok(data.len()) || (data.len() - BLOCK) / BLOCK != count as usize {
        return Err(-6);
    }
    if !(data.len() - BLOCK).is_multiple_of(BLOCK) || count > MAX_RECORDS {
        return Err(-6);
    }
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&data[4..BLOCK]);
    let mut fg = 0u32;
    let mut records = Vec::with_capacity(count as usize);
    for chunk in data[BLOCK..].chunks_exact(BLOCK) {
        let record: [u8; BLOCK] = chunk.try_into().map_err(|_| -6)?;
        hasher.update(&record);
        if record[0] == 2 && record[1] == 1 {
            fg ^= crc32fast::hash(&record);
        }
        records.push(record);
    }
    if hasher.finalize() != crc || fg != fg_crc {
        return Err(-6);
    }
    Ok(records)
}

/// Encrypts a plain ICF image (a fixture writer: the installer's `0x140308B00` + `0x140308D50`): header and
/// records with recomputed `size`, `count`, `crc` and `fgCrc`.
pub fn build(records: &[[u8; BLOCK]]) -> Vec<u8> {
    use aes::cipher::BlockEncryptMut;
    let mut data = vec![0u8; BLOCK + BLOCK * records.len()];
    for (slot, record) in data[BLOCK..].chunks_exact_mut(BLOCK).zip(records) {
        slot.copy_from_slice(record);
    }
    let size = u32::try_from(data.len()).unwrap_or(u32::MAX);
    let count = u32::try_from(records.len()).unwrap_or(u32::MAX);
    let fg = records.iter().filter(|r| r[0] == 2 && r[1] == 1).fold(0u32, |acc, r| acc ^ crc32fast::hash(r));
    data[4..8].copy_from_slice(&size.to_le_bytes());
    data[0x10..0x14].copy_from_slice(&count.to_le_bytes());
    data[0x20..0x24].copy_from_slice(&fg.to_le_bytes());
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&data[4..]);
    data[0..4].copy_from_slice(&hasher.finalize().to_le_bytes());
    let len = data.len();
    cbc::Encryptor::<Aes128>::new(&key().into(), &iv().into())
        .encrypt_padded_mut::<NoPadding>(&mut data, len)
        .map(<[u8]>::to_vec)
        .unwrap_or_default()
}

/// A 64-byte ICF record (`rec[0]` status, `rec[1]` slot, `rec[4]` kind, version or option id at `0x20`,
/// timestamp at `0x24`; 4.10.6) for fixtures.
pub fn record(status: u8, slot: u8, kind: u8, version: [u8; 4], time: (u16, u8, u8, u8, u8, u8)) -> [u8; BLOCK] {
    let mut record = [0u8; BLOCK];
    record[0] = status;
    record[1] = slot;
    record[4] = kind;
    record[0x20..0x24].copy_from_slice(&version);
    record[0x24..0x26].copy_from_slice(&time.0.to_le_bytes());
    record[0x26..0x2B].copy_from_slice(&[time.1, time.2, time.3, time.4, time.5]);
    record
}
