//! The credit manager's two NVRAM records (`credit.md` 2.8 and 3.10): the credit record (rule config) and the
//! backup record (credit state and bookkeeping), both read and written through [`NvramContext`].
//!
//! The manager keeps the image it last loaded or wrote. A write is skipped when the part of the image the
//! original compares is unchanged (`CreditInfo.setConfig` and `BackupInfo.set`), and a failed write leaves the
//! record "not loaded" so the next call writes again.

use super::library::{BOOK_LEN, Rule, STATE_LEN};
use crate::nvram::{NvramContext, Record, RecordStatus};

/// Size of both records.
const IMAGE_LEN: usize = 0x200;
/// Offset of the rule config in the credit record.
const RULE_AT: usize = 8;
/// Offset of the credit state in the backup record.
const STATE_AT: usize = 8;
/// Offset of the bookkeeping in the backup record.
const BOOK_AT: usize = 0x48;
/// Bytes of the rule config the write suppression compares (`bytes 0..14`).
const RULE_COMPARED: usize = 15;
/// Bytes of the credit state the write suppression compares.
const STATE_COMPARED: usize = 16;
/// Bookkeeping dwords the write suppression compares: `0..8` and `30..36`.
const BOOK_COMPARED: [std::ops::Range<usize>; 2] = [0..8, 30..36];

/// The in-memory image of one record.
#[derive(Debug, Clone)]
struct Image {
    bytes: Vec<u8>,
    /// `loaded`: set by a successful load or store, cleared by a failed store.
    loaded: bool,
}

impl Image {
    fn empty() -> Self {
        Self { bytes: vec![0; IMAGE_LEN], loaded: false }
    }
}

/// What the manager persisted and loaded.
#[derive(Debug, Clone)]
pub struct Records {
    credit: Image,
    backup: Image,
    /// `record+64`, copied from `ctx[26180]` (the platform's sub-storage byte): a failed operation maps to 87
    /// instead of 84 (`0x14025AEC0`, `credit.md` "Status -> error code").
    sub_storage: bool,
}

impl Default for Records {
    fn default() -> Self {
        Self { credit: Image::empty(), backup: Image::empty(), sub_storage: false }
    }
}

/// The error code of a failed record operation (`0x14025AEC0`), `None` when the status is success.
fn error_of(status: RecordStatus, sub_storage: bool) -> Option<u16> {
    NvramContext::status_error(status, sub_storage).map(|(code, _)| code)
}

impl Records {
    /// The credit record's default image (`0x1402C7B70`): the default rule config.
    fn default_credit_image() -> Vec<u8> {
        let mut image = Record::CreditInfo.blank_image();
        image[RULE_AT..RULE_AT + 32].copy_from_slice(&Rule::DEFAULT.to_bytes());
        image
    }

    /// Forgets both images, so the next load reads the records again. The sub-storage flag stays.
    pub fn forget(&mut self) {
        *self = Self { sub_storage: self.sub_storage, ..Self::default() };
    }

    /// Takes the platform's sub-storage byte (`ctx[26180]`) the error mapping reads.
    pub fn set_sub_storage(&mut self, sub_storage: bool) {
        self.sub_storage = sub_storage;
    }

    /// `load(CreditInfo)` unless it is loaded already. On failure the error code for the manager.
    pub fn load_credit(&mut self, nvram: &NvramContext) -> Result<(), u16> {
        if self.credit.loaded {
            return Ok(());
        }
        match nvram.load_record(Record::CreditInfo, Self::default_credit_image) {
            Ok(bytes) => {
                self.credit = Image { bytes, loaded: true };
                Ok(())
            }
            Err(status) => Err(error_of(status, self.sub_storage).unwrap_or(84)),
        }
    }

    /// `load(BackupInfo)` unless it is loaded already. A record that fails both copies becomes all zero.
    pub fn load_backup(&mut self, nvram: &NvramContext) -> Result<(), u16> {
        if self.backup.loaded {
            return Ok(());
        }
        match nvram.load_record(Record::BackupInfo, || vec![0; IMAGE_LEN]) {
            Ok(bytes) => {
                self.backup = Image { bytes, loaded: true };
                Ok(())
            }
            Err(status) => Err(error_of(status, self.sub_storage).unwrap_or(84)),
        }
    }

    /// The persisted rule config (credit record bytes 8..40).
    pub fn rule_bytes(&self) -> [u8; 32] {
        let mut rule = [0; 32];
        rule.copy_from_slice(&self.credit.bytes[RULE_AT..RULE_AT + 32]);
        rule
    }

    /// The persisted credit state (backup record bytes 8..72).
    pub fn state_bytes(&self) -> [u8; STATE_LEN] {
        let mut state = [0; STATE_LEN];
        state.copy_from_slice(&self.backup.bytes[STATE_AT..STATE_AT + STATE_LEN]);
        state
    }

    /// The persisted bookkeeping (backup record bytes 0x48..0xD8).
    pub fn book_dwords(&self) -> [u32; BOOK_LEN] {
        let mut book = [0; BOOK_LEN];
        for (dword, chunk) in book.iter_mut().zip(self.backup.bytes[BOOK_AT..].chunks_exact(4)) {
            *dword = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        book
    }

    /// `CreditInfo.setConfig` (`0x14025B530`): writes the 32 config bytes unless the compared prefix is unchanged.
    pub fn set_config(&mut self, nvram: &NvramContext, rule: &[u8; 32]) -> Result<(), u16> {
        let stored = &self.credit.bytes[RULE_AT..RULE_AT + RULE_COMPARED];
        if self.credit.loaded && stored == &rule[..RULE_COMPARED] {
            return Ok(());
        }
        let mut image = self.credit.bytes.clone();
        image[RULE_AT..RULE_AT + 32].copy_from_slice(rule);
        Self::store(nvram, Record::CreditInfo, &mut self.credit, image, self.sub_storage)
    }

    /// `BackupInfo.set` (`0x14025C780`): writes state and bookkeeping unless the compared parts are unchanged.
    pub fn set_backup(
        &mut self,
        nvram: &NvramContext,
        state: &[u8; STATE_LEN],
        book: &[u32; BOOK_LEN],
    ) -> Result<(), u16> {
        let stored = &self.backup.bytes;
        let same_state = stored[STATE_AT..STATE_AT + STATE_COMPARED] == state[..STATE_COMPARED];
        let same_book = BOOK_COMPARED.iter().all(|range| {
            range.clone().all(|index| {
                let at = BOOK_AT + 4 * index;
                stored[at..at + 4] == book[index].to_le_bytes()
            })
        });
        if self.backup.loaded && same_state && same_book {
            return Ok(());
        }
        let mut image = self.backup.bytes.clone();
        image[STATE_AT..STATE_AT + STATE_LEN].copy_from_slice(state);
        for (index, dword) in book.iter().enumerate() {
            let at = BOOK_AT + 4 * index;
            image[at..at + 4].copy_from_slice(&dword.to_le_bytes());
        }
        Self::store(nvram, Record::BackupInfo, &mut self.backup, image, self.sub_storage)
    }

    /// `store`: `loaded` goes down first; a successful write replaces the image and raises it again.
    fn store(
        nvram: &NvramContext,
        record: Record,
        slot: &mut Image,
        image: Vec<u8>,
        sub_storage: bool,
    ) -> Result<(), u16> {
        slot.loaded = false;
        match nvram.store_record(record, &image) {
            RecordStatus::Ok => {
                *slot = Image { bytes: image, loaded: true };
                Ok(())
            }
            status => Err(error_of(status, sub_storage).unwrap_or(84)),
        }
    }
}
