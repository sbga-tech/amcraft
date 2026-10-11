//! The ICF image catalogue file (`spec/libam/storage.md` 4.10.5 / 4.10.6): one AES-CBC stream made of a 64-byte
//! header and `count` 64-byte records. NetDeliver reads the foreground CRC from it (beacons) and records the
//! images it downloads in it.

use super::deliver_image::{ImageError, aes_cbc_decrypt, aes_cbc_encrypt, crc32};

/// Size of the header and of every record.
const BLOCK: usize = 64;
/// Records an ICF holds at most.
pub const MAX_RECORDS: usize = 512;

/// Record status: registered, not installed.
pub const STATUS_PENDING: u8 = 1;
/// Record status: installed and verified.
pub const STATUS_INSTALLED: u8 = 2;
#[cfg(test)]
/// Record status: known but not present (waiting for release).
pub const STATUS_WAITING: u8 = 3;

/// One 64-byte ICF record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IcfRecord(pub [u8; BLOCK]);

impl IcfRecord {
    /// A record with the given status, slot, kind, generation, version and timestamp.
    pub fn new(status: u8, slot: u8, kind: u8, generation: u8, version: u32, timestamp: [u8; 8]) -> Self {
        let mut bytes = [0u8; BLOCK];
        bytes[0] = status;
        bytes[1] = slot;
        bytes[4] = kind;
        bytes[5] = generation;
        bytes[0x20..0x24].copy_from_slice(&version.to_le_bytes());
        bytes[0x24..0x2C].copy_from_slice(&timestamp);
        Self(bytes)
    }

    /// `status` (`+0`).
    pub fn status(&self) -> u8 {
        self.0[0]
    }

    /// `slot` (`+1`): 1 foreground, 2 background.
    pub fn slot(&self) -> u8 {
        self.0[1]
    }

    /// `kind` (`+4`): 0 OS, 1 application, 2 option.
    pub fn kind(&self) -> u8 {
        self.0[4]
    }

    /// `version` (`+0x20`).
    pub fn version(&self) -> u32 {
        u32::from_le_bytes(self.0[0x20..0x24].try_into().expect("four bytes"))
    }

    /// `timestamp` (`+0x24`).
    pub fn timestamp(&self) -> [u8; 8] {
        self.0[0x24..0x2C].try_into().expect("eight bytes")
    }

    /// Sets the status.
    pub fn set_status(&mut self, status: u8) {
        self.0[0] = status;
    }

    /// Counts toward the foreground CRC (`record[0] == 2 && record[1] == 1`).
    fn is_foreground(&self) -> bool {
        self.0[0] == STATUS_INSTALLED && self.0[1] == 1
    }

    /// The identity `0x140309150`: options by slot and version, others by kind, version and timestamp bytes 0..7.
    pub fn same_image(&self, other: &Self) -> bool {
        if self.kind() == 2 && other.kind() == 2 {
            self.slot() == other.slot() && self.version() == other.version()
        } else {
            self.kind() == other.kind()
                && self.version() == other.version()
                && self.timestamp()[..7] == other.timestamp()[..7]
        }
    }
}

/// Why an ICF could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum IcfError {
    /// Size, count or CRC mismatch (`-6` in the original).
    #[error("ICF is corrupt")]
    Corrupt,
}

impl From<ImageError> for IcfError {
    fn from(_: ImageError) -> Self {
        Self::Corrupt
    }
}

/// A decoded ICF.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Icf {
    /// Header `gameId` (`+0x18`).
    pub game_id: [u8; 4],
    /// Header `hardwareCode` (`+0x1C`).
    pub hardware: [u8; 3],
    /// The records in file order.
    pub records: Vec<IcfRecord>,
}

impl Icf {
    /// XOR of `crc32(record)` over the foreground records: header `fgCrc` (`+0x20`).
    pub fn foreground_crc(&self) -> u32 {
        self.records.iter().filter(|record| record.is_foreground()).fold(0, |acc, record| acc ^ crc32(&record.0))
    }

    /// Decrypts and checks a whole file (`0x1403086F0` / `0x140308340`).
    pub fn decode(file: &[u8]) -> Result<Self, IcfError> {
        if file.len() < BLOCK || !file.len().is_multiple_of(BLOCK) {
            return Err(IcfError::Corrupt);
        }
        let mut plain = file.to_vec();
        aes_cbc_decrypt(&mut plain)?;
        let word = |at: usize| u32::from_le_bytes(plain[at..at + 4].try_into().expect("four bytes"));
        let count = word(0x10) as usize;
        if word(4) as usize != file.len() || (file.len() - BLOCK) / BLOCK != count || count > MAX_RECORDS {
            return Err(IcfError::Corrupt);
        }
        if word(0) != crc32(&plain[4..]) || word(0x20) != Self::fg_of(&plain[BLOCK..]) {
            return Err(IcfError::Corrupt);
        }
        Ok(Self {
            game_id: plain[0x18..0x1C].try_into().expect("four bytes"),
            hardware: plain[0x1C..0x1F].try_into().expect("three bytes"),
            records: plain[BLOCK..]
                .chunks_exact(BLOCK)
                .map(|chunk| IcfRecord(chunk.try_into().expect("64 bytes")))
                .collect(),
        })
    }

    fn fg_of(records: &[u8]) -> u32 {
        records
            .chunks_exact(BLOCK)
            .filter(|record| record[0] == STATUS_INSTALLED && record[1] == 1)
            .fold(0, |acc, record| acc ^ crc32(record))
    }

    /// The encrypted file.
    pub fn encode(&self) -> Vec<u8> {
        let mut plain = vec![0u8; BLOCK + BLOCK * self.records.len()];
        let size = u32::try_from(plain.len()).expect("at most 32 KiB");
        plain[4..8].copy_from_slice(&size.to_le_bytes());
        plain[0x10..0x14].copy_from_slice(&u32::try_from(self.records.len()).expect("small").to_le_bytes());
        plain[0x18..0x1C].copy_from_slice(&self.game_id);
        plain[0x1C..0x1F].copy_from_slice(&self.hardware);
        plain[0x20..0x24].copy_from_slice(&self.foreground_crc().to_le_bytes());
        for (index, record) in self.records.iter().enumerate() {
            plain[BLOCK * (index + 1)..BLOCK * (index + 2)].copy_from_slice(&record.0);
        }
        let crc = crc32(&plain[4..]);
        plain[0..4].copy_from_slice(&crc.to_le_bytes());
        aes_cbc_encrypt(&mut plain).expect("whole blocks");
        plain
    }

    /// Adds `record` unless an identical image is present (`0x140309150`); `false` when rejected or full.
    pub fn add(&mut self, record: IcfRecord) -> bool {
        if self.records.len() >= MAX_RECORDS || self.records.iter().any(|known| known.same_image(&record)) {
            return false;
        }
        self.records.push(record);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_keeps_records_and_foreground_crc() {
        let mut icf = Icf { game_id: *b"SDEZ", hardware: *b"AAV", records: Vec::new() };
        assert!(icf.add(IcfRecord::new(STATUS_INSTALLED, 1, 1, 0, 0x0001_0203, [0xE8, 7, 1, 2, 3, 4, 5, 0])));
        assert!(!icf.add(IcfRecord::new(STATUS_PENDING, 2, 1, 0, 0x0001_0203, [0xE8, 7, 1, 2, 3, 4, 5, 0])));
        assert!(icf.add(IcfRecord::new(STATUS_WAITING, 2, 1, 0, 0x0001_0204, [0xE8, 7, 2, 2, 3, 4, 5, 0])));
        let decoded = Icf::decode(&icf.encode()).unwrap();
        assert_eq!(decoded, icf);
        assert_eq!(decoded.foreground_crc(), crc32(&icf.records[0].0));
    }

    #[test]
    fn a_flipped_byte_is_corrupt() {
        let icf = Icf { game_id: *b"SDEZ", hardware: *b"AAV", records: Vec::new() };
        let mut file = icf.encode();
        file[20] ^= 1;
        assert_eq!(Icf::decode(&file), Err(IcfError::Corrupt));
    }
}
