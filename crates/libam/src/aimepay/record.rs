//! The `AimePayInfo` NVRAM record (`payment.md` PayApMgr 10.1-10.3, 10.5): the stored store location, the ring
//! of five deal entries and the pending deal. The image is the 1024-byte record the NVRAM library frames (CRC and
//! id in the first eight bytes, data from `+8`); this module only reads and writes the data area.

use crate::aime::AccessCode;
use crate::time::{DateTime, Tm};

use super::model::{CODE_LEN, COMPANY_NAME_LEN, Deal, Location, STORE_NAME_LEN, units};
use crate::util::tm_from_date_time;

/// Size of the record image.
pub const IMAGE_SIZE: usize = 0x400;
/// Capacity of the deal ring.
const RING: usize = 5;
const LOCATION: usize = 0x08;
const LOCATION_SIZE: usize = 208;
const COUNT: usize = 0xE0;
const HEAD: usize = 0xE2;
const ENTRIES: usize = 0xE8;
const ENTRY_SIZE: usize = 72;
const PENDING: usize = 0x250;

/// Flag bit 1 of an entry: coin deal.
const FLAG_COIN: u8 = 0x02;
/// Flag bit 0 of an entry: legacy "unfinished" mark migrated at boot.
const FLAG_LEGACY: u8 = 0x01;
/// Flag bit 2 of an entry: reported to the sales-management site.
const FLAG_REPORTED: u8 = 0x04;

/// The compact 8-byte time (`0x1402634E0`): `{u16 year, u8 month 1..12, day, hour, minute, second, 0}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Compact([u8; 8]);

impl Compact {
    /// The time of a `struct tm` (local wall clock encoded as UTC).
    pub fn from_tm(tm: &Tm) -> Self {
        let year = u16::try_from(tm.year + 1900).unwrap_or(0);
        let byte = |value: i32| u8::try_from(value).unwrap_or(0);
        let [year_low, year_high] = year.to_le_bytes();
        Self([year_low, year_high, byte(tm.mon + 1), byte(tm.mday), byte(tm.hour), byte(tm.min), byte(tm.sec), 0])
    }

    /// The year word; zero means "not set".
    fn year(self) -> u16 {
        u16::from_le_bytes([self.0[0], self.0[1]])
    }

    /// Back to a `struct tm` (`0x140263400`: second 60/61 clamp to 59); an unset or impossible time is the zero
    /// `Tm`.
    pub fn to_tm(self) -> Tm {
        if self.year() == 0 {
            return Tm::default();
        }
        let second = i32::from(self.0[6]).min(59);
        DateTime::make(
            i32::from(self.year()),
            i32::from(self.0[2]),
            i32::from(self.0[3]),
            i32::from(self.0[4]),
            i32::from(self.0[5]),
            second,
            0,
        )
        .map(tm_from_date_time)
        .unwrap_or_default()
    }
}

/// One deal entry of the NVRAM ring (72 bytes, `0x140288090` / `0x140288270`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Bit 1 coin deal, bit 2 reported to the sales site, bit 0 legacy.
    pub flags: u8,
    /// Stored status: 0 ok, 1 failed, 2 unknown.
    pub status: u8,
    /// Card id of the Aime.
    pub card_id: [u8; 8],
    /// Server `result_code`.
    pub error_code: u32,
    /// `detail_code`, at most 15 bytes.
    pub error_message: String,
    /// Time of the deal.
    pub time: Compact,
    /// Packed-BCD access code.
    pub access_code: [u8; 10],
    /// Item id as a number.
    pub item_id: u64,
    /// Receipt id.
    pub receipt_id: u64,
    /// Item count.
    pub item_count: u32,
    /// Amount.
    pub amount: u32,
}

impl Entry {
    /// The deal was reported to the sales-management site.
    pub fn is_reported(&self) -> bool {
        self.flags & FLAG_REPORTED != 0
    }

    /// The entry of a finished deal.
    pub fn from_deal(deal: &Deal, card_id: [u8; 8]) -> Self {
        Self {
            flags: if deal.is_coin { FLAG_COIN } else { 0 },
            status: match deal.status {
                0 | 2 => 0,
                3 => 2,
                _ => 1,
            },
            card_id,
            error_code: deal.error_code,
            error_message: deal.error_message.chars().take(15).collect(),
            time: Compact::from_tm(&deal.time),
            access_code: deal.access_code.0,
            item_id: deal.item_id.parse().unwrap_or(0),
            receipt_id: deal.receipt_id,
            item_count: deal.item_count,
            amount: deal.amount,
        }
    }

    /// The deal of a stored entry (the boot rebuild of the history).
    pub fn to_deal(&self) -> Deal {
        Deal {
            status: match self.status {
                0 => 0,
                2 => 3,
                _ => 1,
            },
            time: self.time.to_tm(),
            error_code: self.error_code,
            error_message: self.error_message.clone(),
            item_count: self.item_count,
            item_id: item_text(self.item_id),
            access_code: AccessCode(self.access_code),
            receipt_id: self.receipt_id,
            amount: self.amount,
            is_coin: self.flags & FLAG_COIN != 0,
        }
    }

    fn encode(&self) -> [u8; ENTRY_SIZE] {
        let mut out = [0u8; ENTRY_SIZE];
        out[0] = self.flags;
        out[1] = self.status;
        out[2..4].copy_from_slice(&self.card_id[..2]);
        out[4..8].copy_from_slice(&self.error_code.to_le_bytes());
        let message = self.error_message.as_bytes();
        let length = message.len().min(15);
        out[8..8 + length].copy_from_slice(&message[..length]);
        out[0x18..0x20].copy_from_slice(&self.time.0);
        out[0x20..0x2A].copy_from_slice(&self.access_code);
        out[0x2A..0x32].copy_from_slice(&self.item_id.to_le_bytes());
        out[0x32..0x38].copy_from_slice(&self.card_id[2..]);
        out[0x38..0x40].copy_from_slice(&self.receipt_id.to_le_bytes());
        out[0x40..0x44].copy_from_slice(&self.item_count.to_le_bytes());
        out[0x44..0x48].copy_from_slice(&self.amount.to_le_bytes());
        out
    }

    fn decode(bytes: &[u8]) -> Self {
        let word = |from: usize| u32::from_le_bytes([bytes[from], bytes[from + 1], bytes[from + 2], bytes[from + 3]]);
        let wide = |from: usize| {
            let mut raw = [0u8; 8];
            raw.copy_from_slice(&bytes[from..from + 8]);
            u64::from_le_bytes(raw)
        };
        let message = &bytes[8..24];
        let end = message.iter().position(|&byte| byte == 0).unwrap_or(message.len());
        let mut card_id = [0u8; 8];
        card_id[..2].copy_from_slice(&bytes[2..4]);
        card_id[2..].copy_from_slice(&bytes[0x32..0x38]);
        let mut time = [0u8; 8];
        time.copy_from_slice(&bytes[0x18..0x20]);
        let mut access_code = [0u8; 10];
        access_code.copy_from_slice(&bytes[0x20..0x2A]);
        Self {
            flags: bytes[0],
            status: bytes[1],
            card_id,
            error_code: word(4),
            error_message: String::from_utf8_lossy(&message[..end]).into_owned(),
            time: Compact(time),
            access_code,
            item_id: wide(0x2A),
            receipt_id: wide(0x38),
            item_count: word(0x40),
            amount: word(0x44),
        }
    }
}

/// The item id text of a stored number: `%013llu` for ids up to 9999999999999, else empty.
fn item_text(id: u64) -> String {
    if id <= 9_999_999_999_999 { format!("{id:013}") } else { String::new() }
}

/// The pending deal (48 bytes, `0x140287E80` / `0x140287F70`): the payment that was started and not yet logged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    /// Packed-BCD access code.
    pub access_code: [u8; 10],
    /// Card id.
    pub card_id: [u8; 8],
    /// Item id as a number.
    pub item_id: u64,
    /// When the payment started.
    pub time: Compact,
    /// Amount.
    pub amount: u32,
    /// Item count.
    pub item_count: u32,
    /// Coin deal.
    pub is_coin: bool,
}

impl Pending {
    fn encode(&self) -> [u8; 48] {
        let mut out = [0u8; 48];
        out[..10].copy_from_slice(&self.access_code);
        out[10..16].copy_from_slice(&self.card_id[..6]);
        out[16..24].copy_from_slice(&self.item_id.to_le_bytes());
        out[24..32].copy_from_slice(&self.time.0);
        out[32..36].copy_from_slice(&self.amount.to_le_bytes());
        out[36..40].copy_from_slice(&self.item_count.to_le_bytes());
        out[40] = if self.is_coin { FLAG_COIN } else { 0 };
        out[41..43].copy_from_slice(&self.card_id[6..]);
        out
    }

    /// `None` when the year at `+24` is zero (no pending deal).
    fn decode(bytes: &[u8]) -> Option<Self> {
        let mut time = [0u8; 8];
        time.copy_from_slice(&bytes[24..32]);
        let time = Compact(time);
        if time.year() == 0 {
            return None;
        }
        let word = |from: usize| u32::from_le_bytes([bytes[from], bytes[from + 1], bytes[from + 2], bytes[from + 3]]);
        let mut access_code = [0u8; 10];
        access_code.copy_from_slice(&bytes[..10]);
        let mut card_id = [0u8; 8];
        card_id[..6].copy_from_slice(&bytes[10..16]);
        card_id[6..].copy_from_slice(&bytes[41..43]);
        let mut item = [0u8; 8];
        item.copy_from_slice(&bytes[16..24]);
        Some(Self {
            access_code,
            card_id,
            item_id: u64::from_le_bytes(item),
            time,
            amount: word(32),
            item_count: word(36),
            is_coin: bytes[40] & FLAG_COIN != 0,
        })
    }

    /// The failed deal a recovered pending record stands for (`0x14023A7C0`: status 1).
    pub fn to_deal(&self) -> Deal {
        Deal {
            status: 1,
            time: self.time.to_tm(),
            item_count: self.item_count,
            item_id: item_text(self.item_id),
            access_code: AccessCode(self.access_code),
            amount: self.amount,
            is_coin: self.is_coin,
            ..Deal::default()
        }
    }
}

/// The record image with typed access to its data area.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Info {
    image: Vec<u8>,
}

impl Info {
    /// The record of a loaded image; a short image is zero-extended.
    pub fn from_image(mut image: Vec<u8>) -> Self {
        image.resize(IMAGE_SIZE, 0);
        Self { image }
    }

    /// The image to store.
    pub fn image(&self) -> &[u8] {
        &self.image
    }

    /// The all-zero image written by a deactivation and by the first load of a blank record.
    pub fn zero_image() -> Vec<u8> {
        vec![0; IMAGE_SIZE]
    }

    /// A location is stored iff the first byte of its area is non-zero (`0x140261200`).
    pub fn has_location(&self) -> bool {
        self.image[LOCATION] != 0
    }

    /// The stored location, each field decoded as UTF-8 (invalid -> empty) and bounded like the wide setters.
    pub fn location(&self) -> Location {
        let area = &self.image[LOCATION..LOCATION + LOCATION_SIZE];
        let field = |from: usize, size: usize, limit: usize| {
            let bytes = &area[from..from + size];
            let end = bytes.iter().position(|&byte| byte == 0).unwrap_or(size);
            match std::str::from_utf8(&bytes[..end]) {
                Ok(text) if units(text) <= limit => text.to_owned(),
                _ => String::new(),
            }
        };
        Location {
            store_code: field(0, 11, CODE_LEN),
            store_name: field(11, 61, STORE_NAME_LEN),
            company_code: field(72, 11, CODE_LEN),
            company_name: field(83, 121, COMPANY_NAME_LEN),
        }
    }

    /// Stores `location`; `false` (and nothing changed) when it equals the stored one (`0x140260910`).
    pub fn set_location(&mut self, location: &Location) -> bool {
        if self.has_location() && self.location() == *location {
            return false;
        }
        let mut area = [0u8; LOCATION_SIZE];
        let mut put = |from: usize, size: usize, text: &str| {
            let bytes = text.as_bytes();
            let length = bytes.len().min(size - 1);
            area[from..from + length].copy_from_slice(&bytes[..length]);
        };
        put(0, 11, &location.store_code);
        put(11, 61, &location.store_name);
        put(72, 11, &location.company_code);
        put(83, 121, &location.company_name);
        self.image[LOCATION..LOCATION + LOCATION_SIZE].copy_from_slice(&area);
        true
    }

    fn ring(&self) -> (usize, usize) {
        let count = usize::from(u16::from_le_bytes([self.image[COUNT], self.image[COUNT + 1]])).min(RING);
        let head = usize::from(u16::from_le_bytes([self.image[HEAD], self.image[HEAD + 1]])) % RING;
        (count, head)
    }

    fn slot(index: usize) -> std::ops::Range<usize> {
        ENTRIES + index * ENTRY_SIZE..ENTRIES + (index + 1) * ENTRY_SIZE
    }

    /// Number of entries.
    pub fn entry_count(&self) -> usize {
        self.ring().0
    }

    /// The ring slot of the `index`-th entry, oldest first.
    fn slot_of(&self, index: usize) -> usize {
        let (count, head) = self.ring();
        (head + RING - count + index) % RING
    }

    /// Entries oldest first.
    pub fn entries(&self) -> Vec<Entry> {
        (0..self.entry_count()).map(|index| Entry::decode(&self.image[Self::slot(self.slot_of(index))])).collect()
    }

    /// Rewrites the `index`-th entry (oldest first).
    pub fn set_entry(&mut self, index: usize, entry: &Entry) {
        let slot = Self::slot(self.slot_of(index));
        self.image[slot].copy_from_slice(&entry.encode());
    }

    /// Appends `entry` as the newest one (`0x140260BD0`) and clears the pending deal.
    pub fn append(&mut self, entry: &Entry) {
        let (count, head) = self.ring();
        self.image[Self::slot(head)].copy_from_slice(&entry.encode());
        let head = u16::try_from((head + 1) % RING).unwrap_or(0);
        let count = u16::try_from((count + 1).min(RING)).unwrap_or(0);
        self.image[HEAD..HEAD + 2].copy_from_slice(&head.to_le_bytes());
        self.image[COUNT..COUNT + 2].copy_from_slice(&count.to_le_bytes());
        self.clear_pending();
    }

    /// The pending deal.
    pub fn pending(&self) -> Option<Pending> {
        Pending::decode(&self.image[PENDING..PENDING + 48])
    }

    /// Stores the pending deal; `false` when it is already stored (`0x140260AB0`).
    pub fn set_pending(&mut self, pending: &Pending) -> bool {
        let bytes = pending.encode();
        if self.image[PENDING..PENDING + 48] == bytes {
            return false;
        }
        self.image[PENDING..PENDING + 48].copy_from_slice(&bytes);
        true
    }

    /// Clears the pending deal.
    pub fn clear_pending(&mut self) {
        self.image[PENDING..PENDING + 48].fill(0);
    }

    /// The boot migration (`0x14023A4B0`, 10.5), oldest first. Returns whether any entry changed.
    pub fn migrate(&mut self) -> bool {
        let mut changed = false;
        for (index, mut entry) in self.entries().into_iter().enumerate() {
            let before = entry.clone();
            if entry.flags & FLAG_LEGACY != 0 {
                entry.flags &= !FLAG_LEGACY;
                entry.status = if entry.flags & FLAG_COIN != 0 { 1 } else { 2 };
            } else if entry.status == 0 && entry.error_code != 0 {
                entry.status = 1;
            }
            if entry != before {
                self.set_entry(index, &entry);
                changed = true;
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deal(status: u32, coin: bool) -> Deal {
        Deal {
            status,
            time: Compact([0xE8, 0x07, 5, 20, 12, 30, 45, 0]).to_tm(),
            error_code: 20,
            error_message: "AMPEWL2050".into(),
            item_count: 3,
            item_id: "1234567890123".into(),
            access_code: AccessCode([0x01, 0x23, 0x45, 0x67, 0x89, 0x01, 0x23, 0x45, 0x67, 0x89]),
            receipt_id: 77,
            amount: 300,
            is_coin: coin,
        }
    }

    #[test]
    fn ring_keeps_five_entries_and_lists_them_oldest_first() {
        let mut info = Info::from_image(Info::zero_image());
        for receipt in 1..=7u64 {
            let mut deal = deal(0, true);
            deal.receipt_id = receipt;
            info.append(&Entry::from_deal(&deal, [9; 8]));
        }
        let receipts: Vec<u64> = info.entries().iter().map(|entry| entry.receipt_id).collect();
        assert_eq!(receipts, [3, 4, 5, 6, 7]);
    }

    #[test]
    fn entry_round_trips_a_deal_and_maps_the_status_code() {
        let original = deal(3, false);
        let entry = Entry::from_deal(&original, [1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(entry.status, 2);
        let mut info = Info::from_image(Info::zero_image());
        info.append(&entry);
        let stored = info.entries().remove(0);
        assert_eq!(stored.card_id, [1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(stored.to_deal(), original);
    }

    #[test]
    fn migration_rewrites_legacy_and_inconsistent_entries_only() {
        let mut info = Info::from_image(Info::zero_image());
        let mut legacy = Entry::from_deal(&deal(0, true), [0; 8]);
        legacy.flags |= FLAG_LEGACY;
        legacy.error_code = 0;
        let mut inconsistent = Entry::from_deal(&deal(0, false), [0; 8]);
        inconsistent.status = 0;
        let clean = Entry::from_deal(&deal(1, true), [0; 8]);
        for entry in [&legacy, &inconsistent, &clean] {
            info.append(entry);
        }
        assert!(info.migrate());
        let entries = info.entries();
        assert_eq!((entries[0].flags, entries[0].status), (FLAG_COIN, 1));
        assert_eq!(entries[1].status, 1);
        assert_eq!(entries[2], clean);
        assert!(!info.migrate());
    }

    #[test]
    fn pending_deal_is_valid_only_with_a_year_and_append_clears_it() {
        let mut info = Info::from_image(Info::zero_image());
        assert!(info.pending().is_none());
        let pending = Pending {
            access_code: [0x12; 10],
            card_id: [1, 2, 3, 4, 5, 6, 7, 8],
            item_id: 1_234_567_890_123,
            time: Compact([0xE8, 0x07, 5, 20, 12, 30, 45, 0]),
            amount: 100,
            item_count: 1,
            is_coin: true,
        };
        assert!(info.set_pending(&pending));
        assert!(!info.set_pending(&pending));
        assert_eq!(info.pending(), Some(pending));
        info.append(&Entry::from_deal(&deal(1, true), [0; 8]));
        assert!(info.pending().is_none());
    }

    #[test]
    fn location_is_stored_as_utf8_and_unchanged_saves_are_skipped() {
        let mut info = Info::from_image(Info::zero_image());
        assert!(!info.has_location());
        let location = Location {
            store_code: "1234567890".into(),
            store_name: "ゲームセンター".into(),
            company_code: "0987654321".into(),
            company_name: "株式会社".into(),
        };
        assert!(info.set_location(&location));
        assert!(!info.set_location(&location));
        assert!(info.has_location());
        assert_eq!(info.location(), location);
    }
}
