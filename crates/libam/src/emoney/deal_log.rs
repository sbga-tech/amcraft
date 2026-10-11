//! The deal log (`spec/libam/payment.md` DL): the in-memory ring of 8 `DealInfo`, the 68-byte persisted record
//! and the `EmoneyInfo` NVRAM image (system file `0x2200` / mirror `0x5200`, `0x600` bytes).

use crate::emoney::{EMoneyDealInfo, LibamBrand};
use crate::time::Tm;

use crate::util::{tm_from_date_time, wide_array, wide_to_string};

/// Size of a persisted deal record.
pub const RECORD_SIZE: usize = 68;
/// Size of one persisted ring.
pub const RING_SIZE: usize = 548;
/// Capacity of the memory ring.
pub const MEMORY_RING: usize = 8;

/// An empty `DealInfo` (`0x140223930`).
pub fn empty_deal() -> EMoneyDealInfo {
    EMoneyDealInfo {
        status: 0,
        brand: LibamBrand::NONE,
        time: Tm::default(),
        deal_number: [0; 16],
        card_number: [0; 21],
        amount: 0,
        balance_before: 0,
        balance_after: 0,
    }
}

/// `0x140223A70`: stores the card number unless it needs 21 units or more.
pub fn set_card(deal: &mut EMoneyDealInfo, text: &str) {
    if text.encode_utf16().count() < 21 {
        deal.card_number = wide_array(text);
    }
}

/// `0x140223A00`: stores the deal number unless it needs 16 units or more.
pub fn set_deal_number(deal: &mut EMoneyDealInfo, text: &str) {
    if text.encode_utf16().count() < 16 {
        deal.deal_number = wide_array(text);
    }
}

/// `L <- T` (`0x14027F360`): libam brand of a Thinca brand number.
pub fn libam_brand_of_sdk(thinca: u32) -> LibamBrand {
    LibamBrand(match thinca {
        1 | 7 => 1,
        2 => 2,
        3 => 3,
        5 => 4,
        6 => 5,
        8 => 6,
        9 => 7,
        _ => 0,
    })
}

/// `L <- E` (`0x14027F200`): libam brand of a brand-list entry.
pub fn libam_brand_of_entry(entry: usize) -> LibamBrand {
    LibamBrand(match entry {
        1 | 7 => 1,
        2..=6 => entry as u32,
        8 => 7,
        _ => 0,
    })
}

/// `T <- E` (`0x14027F3E0`): Thinca brand number of a brand-list entry.
pub fn sdk_brand_of_entry(entry: usize) -> Option<u32> {
    Some(match entry {
        1 => 1,
        2 => 2,
        3 => 3,
        4 => 5,
        5 => 6,
        6 => 8,
        7 => 7,
        8 => 9,
        _ => return None,
    })
}

/// `E <- T` (`0x14027F460`).
pub fn entry_of_sdk(thinca: u32) -> usize {
    match thinca {
        1 => 1,
        2 => 2,
        3 => 3,
        5 => 4,
        6 => 5,
        7 => 7,
        8 => 6,
        9 => 8,
        _ => 0,
    }
}

/// Persisted brand byte of a Thinca brand (`0x14027F4E0`).
fn brand_byte_of_sdk(thinca: u32) -> u8 {
    match thinca {
        1 => 1,
        2 => 2,
        3 => 7,
        5 => 3,
        6 => 4,
        7 => 6,
        8 => 5,
        9 => 8,
        _ => 0,
    }
}

/// Thinca brand of a persisted brand byte (inverse of `brand_byte_of_sdk`).
pub fn sdk_brand_of_byte(byte: u8) -> Option<u32> {
    Some(match byte {
        1 => 1,
        2 => 2,
        7 => 3,
        3 => 5,
        4 => 6,
        6 => 7,
        5 => 8,
        8 => 9,
        _ => return None,
    })
}

/// `0x14027F300` (8.5): the bit of the legacy brand mask of a brand-list entry.
pub fn mask_bit_of_entry(entry: usize) -> Option<u32> {
    Some(match entry {
        1 => 1,
        2 => 2,
        3 => 7,
        4 => 3,
        5 => 4,
        6 => 5,
        7 => 6,
        8 => 8,
        _ => return None,
    })
}

/// Persisted brand byte of a libam brand (inverse of [`libam_brand_of_byte`]).
fn brand_byte_of_libam(brand: LibamBrand) -> u8 {
    match brand.0 {
        1 => 1,
        2 => 2,
        3 => 7,
        4 => 3,
        5 => 4,
        6 => 5,
        7 => 8,
        _ => 0,
    }
}

/// libam brand of a persisted brand byte (`0x14027F180`).
pub fn libam_brand_of_byte(byte: u8) -> LibamBrand {
    LibamBrand(match byte {
        1 | 6 => 1,
        2 => 2,
        3 => 4,
        4 => 5,
        5 => 6,
        7 => 3,
        8 => 7,
        _ => 0,
    })
}

/// A persisted 68-byte record (DL.4): an e-money deal (ring 0) or a cash coin sale (ring 1).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DealRecord {
    /// `+0` persisted status.
    pub status: u8,
    /// `+1` brand byte.
    pub brand: u8,
    /// `+2` deal number (UTF-8, at most 15 bytes).
    pub deal_number: String,
    /// `+18` card number (UTF-8, at most 20 bytes).
    pub card: String,
    /// `+39` flags: `0x01` reported, `0x02` status was 3, `0x08` cash, `0x10` open cash-sale entry.
    pub flags: u8,
    /// `+40` time `(year, month, day, hour, minute, second)`.
    pub time: [u16; 6],
    /// `+48` amount.
    pub amount: i32,
    /// `+52` balance before.
    pub balance_before: i32,
    /// `+56` balance after.
    pub balance_after: i32,
    /// `+60` unit price.
    pub price: u32,
    /// `+64` count.
    pub count: u32,
}

/// Flag bit: reported.
pub const FLAG_REPORTED: u8 = 0x01;
/// Flag bit: the status was 3 (unknown).
pub const FLAG_UNKNOWN: u8 = 0x02;
/// Flag bit: cash payment.
pub const FLAG_CASH: u8 = 0x08;
/// Flag bit: open cash-sale accumulation entry.
pub const FLAG_OPEN: u8 = 0x10;

fn narrow(text: &str, capacity: usize) -> Vec<u8> {
    let mut bytes = text.as_bytes().to_vec();
    bytes.truncate(capacity - 1);
    while std::str::from_utf8(&bytes).is_err() {
        bytes.pop();
    }
    bytes
}

fn read_text(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&byte| byte == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

fn le32(bytes: &[u8]) -> [u8; 4] {
    [bytes[0], bytes[1], bytes[2], bytes[3]]
}

impl DealRecord {
    /// The persisted form of a `DealInfo` (`0x14026EEC0`); `sdk_brand` is the Thinca brand of the result
    /// (`0` falls back to the libam brand of the deal).
    pub fn from_deal(deal: &EMoneyDealInfo, sdk_brand: u32, flags: u8) -> Self {
        let status = match deal.status {
            0 | 2 => 0,
            3 => 2,
            4 => 3,
            _ => 1,
        };
        let brand = match brand_byte_of_sdk(sdk_brand) {
            0 => brand_byte_of_libam(deal.brand),
            byte => byte,
        };
        let time = &deal.time;
        let mut flags = flags;
        if deal.status == 3 {
            flags |= FLAG_UNKNOWN;
        }
        Self {
            status,
            brand,
            deal_number: wide_to_string(&deal.deal_number),
            card: wide_to_string(&deal.card_number),
            flags,
            time: [
                (time.year + 1900) as u16,
                (time.mon + 1) as u16,
                time.mday as u16,
                time.hour as u16,
                time.min as u16,
                time.sec as u16,
            ],
            amount: deal.amount,
            balance_before: deal.balance_before,
            balance_after: deal.balance_after,
            price: 0,
            count: 0,
        }
    }

    /// `0x14027EF50`: the `DealInfo` of a persisted record.
    pub fn to_deal(&self) -> EMoneyDealInfo {
        let mut deal = empty_deal();
        deal.status = if self.flags & FLAG_UNKNOWN != 0 {
            3
        } else {
            match self.status {
                0 => 0,
                2 => 3,
                3 => 4,
                _ => 1,
            }
        };
        deal.brand = libam_brand_of_byte(self.brand);
        let [year, month, day, hour, minute, second] = self.time;
        if let Some(time) = crate::time::DateTime::make(
            i32::from(year),
            i32::from(month),
            i32::from(day),
            i32::from(hour),
            i32::from(minute),
            i32::from(second.min(59)),
            0,
        ) {
            deal.time = tm_from_date_time(time);
        }
        set_deal_number(&mut deal, &self.deal_number);
        set_card(&mut deal, &self.card);
        deal.amount = self.amount;
        deal.balance_before = self.balance_before;
        deal.balance_after = self.balance_after;
        deal
    }

    /// The 68 bytes.
    pub fn to_bytes(&self) -> [u8; RECORD_SIZE] {
        let mut out = [0u8; RECORD_SIZE];
        out[0] = self.status;
        out[1] = self.brand;
        let number = narrow(&self.deal_number, 16);
        out[2..2 + number.len()].copy_from_slice(&number);
        let card = narrow(&self.card, 21);
        out[18..18 + card.len()].copy_from_slice(&card);
        out[39] = self.flags;
        out[40..42].copy_from_slice(&self.time[0].to_le_bytes());
        for (slot, value) in out[42..47].iter_mut().zip(&self.time[1..]) {
            *slot = *value as u8;
        }
        out[48..52].copy_from_slice(&self.amount.to_le_bytes());
        out[52..56].copy_from_slice(&self.balance_before.to_le_bytes());
        out[56..60].copy_from_slice(&self.balance_after.to_le_bytes());
        out[60..64].copy_from_slice(&self.price.to_le_bytes());
        out[64..68].copy_from_slice(&self.count.to_le_bytes());
        out
    }

    /// Parses 68 bytes.
    pub fn from_bytes(bytes: &[u8]) -> Self {
        let mut time = [u16::from_le_bytes([bytes[40], bytes[41]]), 0, 0, 0, 0, 0];
        for (slot, value) in time[1..].iter_mut().zip(&bytes[42..47]) {
            *slot = u16::from(*value);
        }
        Self {
            status: bytes[0],
            brand: bytes[1],
            deal_number: read_text(&bytes[2..18]),
            card: read_text(&bytes[18..39]),
            flags: bytes[39],
            time,
            amount: i32::from_le_bytes(le32(&bytes[48..])),
            balance_before: i32::from_le_bytes(le32(&bytes[52..])),
            balance_after: i32::from_le_bytes(le32(&bytes[56..])),
            price: u32::from_le_bytes(le32(&bytes[60..])),
            count: u32::from_le_bytes(le32(&bytes[64..])),
        }
    }
}

/// `0x140223860`: `DealInfo` equality of the memory ring (the time compares as the nine `tm` fields).
fn same_deal(a: &EMoneyDealInfo, b: &EMoneyDealInfo) -> bool {
    a == b
}

/// The memory ring of 8 `DealInfo` (DL.1), newest first.
#[derive(Debug, Clone)]
pub struct DealRing {
    slots: Vec<EMoneyDealInfo>,
    count: usize,
    head: usize,
}

impl Default for DealRing {
    fn default() -> Self {
        Self { slots: vec![empty_deal(); MEMORY_RING], count: 0, head: 0 }
    }
}

impl DealRing {
    /// `0x140223CA0`.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// `0x140223F30`.
    pub fn count(&self) -> usize {
        self.count
    }

    /// `0x140223F40`: item `index`, newest first.
    pub fn item(&self, index: usize) -> Option<EMoneyDealInfo> {
        (index < self.count).then(|| self.slots[(self.head + MEMORY_RING - 1 - index) % MEMORY_RING].clone())
    }

    /// `0x140223D30`: appends unless the item equals the newest one.
    pub fn append(&mut self, deal: EMoneyDealInfo) {
        if self.count != 0 && same_deal(&deal, &self.slots[(self.head + MEMORY_RING - 1) % MEMORY_RING]) {
            return;
        }
        self.slots[self.head] = deal;
        self.head = (self.head + 1) % MEMORY_RING;
        self.count = (self.count + 1).min(MEMORY_RING);
    }

    /// `0x140223E30`: replaces item `index`; `false` when out of range.
    pub fn overwrite(&mut self, index: usize, deal: EMoneyDealInfo) -> bool {
        if index >= self.count {
            return false;
        }
        self.slots[(self.head + MEMORY_RING - 1 - index) % MEMORY_RING] = deal;
        true
    }
}

const ENABLE_MASK: usize = 8;
const TASMS_FLAG: usize = 11;
const URL: usize = 12;
const URL_SIZE: usize = 128;
const MARKER_DEAL: usize = 140;
const SLIP: usize = 156;
const RING0: usize = 164;
const RING1: usize = 712;
const MARKER_BLOCK: usize = 1440;
/// Object offsets `E+1508` and `E+1476` of PayEmSvc 2.5 minus the 72-byte header.
const RAW_MASK: usize = 1436;
const COUNTERS: usize = 1404;

/// The marker (DL.6): the deal that is in flight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    /// Narrow deal number, at most 15 bytes.
    pub deal_number: String,
    /// The 48-byte block: `+0` keychip serial (11 characters) and `+11` game id (4) as `0x1402323F0` patches them,
    /// `+32` the Thinca brand of the deal, `+36` price, `+40` count, `+44` flags.
    pub block: [u8; 48],
}

/// The `EmoneyInfo` NVRAM image (DL.7): the blob as stored, including the CRC and id header the NVRAM layer stamps.
#[derive(Debug, Clone)]
pub struct EmoneyInfo {
    image: Vec<u8>,
}

impl EmoneyInfo {
    /// Wraps a stored image.
    pub fn from_image(image: Vec<u8>) -> Self {
        Self { image }
    }

    /// The factory default image (`0x5555AAAA` placeholder, `0x1402C7CA0`).
    pub fn blank_image(size: usize) -> Vec<u8> {
        let mut image = vec![0; size];
        image[..4].copy_from_slice(&0x5555_AAAA_u32.to_le_bytes());
        image
    }

    /// The image to store.
    pub fn image(&self) -> &[u8] {
        &self.image
    }

    /// The stored brand mask `E+80..82` (the legacy libam numbering, or the raw server mask when the TASMS flag is set).
    pub fn auth_mask(&self) -> u32 {
        u32::from_le_bytes([self.image[ENABLE_MASK], self.image[ENABLE_MASK + 1], self.image[ENABLE_MASK + 2], 0])
    }

    /// `0x14026C080` (PayEmSvc 10): removes a Thinca brand from the raw server mask and its bit from the legacy mask
    /// next to it.
    pub fn remove_brand(&mut self, thinca: u32) {
        let raw = self.raw_mask() & !(1 << thinca);
        if self.tasms() {
            self.image[ENABLE_MASK..ENABLE_MASK + 3].copy_from_slice(&raw.to_le_bytes()[..3]);
            return;
        }
        let legacy_bit = mask_bit_of_entry(entry_of_sdk(thinca));
        let legacy = u32::from_le_bytes(le32(&self.image[ENABLE_MASK..])) & !legacy_bit.map_or(0, |bit| 1 << bit);
        self.image[ENABLE_MASK..ENABLE_MASK + 4].copy_from_slice(&legacy.to_le_bytes());
        self.image[RAW_MASK..RAW_MASK + 4].copy_from_slice(&raw.to_le_bytes());
    }

    /// The service URL (empty when none).
    pub fn url(&self) -> String {
        read_text(&self.image[URL..URL + URL_SIZE])
    }

    /// Stores the service URL (at most 127 bytes).
    pub fn set_url(&mut self, url: &str) {
        let bytes = narrow(url, URL_SIZE);
        self.image[URL..URL + URL_SIZE].fill(0);
        self.image[URL..URL + bytes.len()].copy_from_slice(&bytes);
    }

    /// `0x14025FBB0`: no authenticated brand and no URL.
    pub fn is_blank(&self) -> bool {
        self.raw_mask() == 0 && self.url().is_empty()
    }

    /// The raw server brand mask `v` of 2.5: `E+80..82` with the TASMS flag set, `E+1508` otherwise.
    pub fn raw_mask(&self) -> u32 {
        if self.tasms() { self.auth_mask() } else { u32::from_le_bytes(le32(&self.image[RAW_MASK..])) }
    }

    /// `0x14025FB30`: `registered`, the raw server brand mask is non-zero.
    pub fn is_registered(&self) -> bool {
        self.raw_mask() != 0
    }

    /// The payment sequence number (`E+228`).
    pub fn slip(&self) -> u32 {
        u32::from_le_bytes(le32(&self.image[SLIP..]))
    }

    /// Stores the sequence number.
    pub fn set_slip(&mut self, value: u32) {
        self.image[SLIP..SLIP + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn base(ring: usize) -> usize {
        if ring == 0 { RING0 } else { RING1 }
    }

    /// Capacity of a ring (`0x14025F7A0`): 5 in the TASMS format, else 8.
    pub fn ring_capacity(&self) -> usize {
        if self.image[TASMS_FLAG] != 0 { 5 } else { 8 }
    }

    /// Entries of the ring (`0x14025FFE0`).
    pub fn ring_count(&self, ring: usize) -> usize {
        let base = Self::base(ring);
        usize::from(u16::from_le_bytes([self.image[base], self.image[base + 1]])).min(self.ring_capacity())
    }

    fn head(&self, ring: usize) -> usize {
        let base = Self::base(ring);
        usize::from(u16::from_le_bytes([self.image[base + 2], self.image[base + 3]])) % self.ring_capacity()
    }

    fn slot(&self, ring: usize, physical: usize) -> usize {
        Self::base(ring) + 4 + physical * RECORD_SIZE
    }

    /// Entry `index`, newest first (`0x140260050`).
    pub fn ring_get(&self, ring: usize, index: usize) -> Option<DealRecord> {
        if index >= self.ring_count(ring) {
            return None;
        }
        let cap = self.ring_capacity();
        let at = self.slot(ring, (self.head(ring) + cap - 1 - index) % cap);
        Some(DealRecord::from_bytes(&self.image[at..at + RECORD_SIZE]))
    }

    /// Appends (`0x14025E200`), overwriting the oldest entry of a full ring.
    pub fn ring_append(&mut self, ring: usize, record: &DealRecord) {
        let cap = self.ring_capacity();
        let head = self.head(ring);
        let at = self.slot(ring, head);
        self.image[at..at + RECORD_SIZE].copy_from_slice(&record.to_bytes());
        let count = (self.ring_count(ring) + 1).min(cap);
        let base = Self::base(ring);
        self.image[base..base + 2].copy_from_slice(&(count as u16).to_le_bytes());
        self.image[base + 2..base + 4].copy_from_slice(&(((head + 1) % cap) as u16).to_le_bytes());
    }

    /// Replaces entry `index`, newest first (`0x14025E6A0` / `0x14025E3C0`); `false` when out of range.
    pub fn ring_update(&mut self, ring: usize, index: usize, record: &DealRecord) -> bool {
        if index >= self.ring_count(ring) {
            return false;
        }
        let cap = self.ring_capacity();
        let at = self.slot(ring, (self.head(ring) + cap - 1 - index) % cap);
        self.image[at..at + RECORD_SIZE].copy_from_slice(&record.to_bytes());
        true
    }

    /// The records of a ring, oldest first.
    pub fn ring_oldest_first(&self, ring: usize) -> Vec<DealRecord> {
        (0..self.ring_count(ring)).rev().filter_map(|index| self.ring_get(ring, index)).collect()
    }

    /// The marker, `None` when no deal number is stored.
    pub fn marker(&self) -> Option<Marker> {
        let deal_number = read_text(&self.image[MARKER_DEAL..MARKER_DEAL + 16]);
        let mut block = [0u8; 48];
        block.copy_from_slice(&self.image[MARKER_BLOCK..MARKER_BLOCK + 48]);
        (!deal_number.is_empty() || block.iter().any(|&byte| byte != 0)).then_some(Marker { deal_number, block })
    }

    /// `0x14025EF70`: stores the marker; `false` (nothing written) when it equals the stored one.
    pub fn set_marker(&mut self, deal_number: &str, block: &[u8; 48]) -> bool {
        let bytes = narrow(deal_number, 16);
        let mut stored = [0u8; 16];
        stored[..bytes.len()].copy_from_slice(&bytes);
        if self.image[MARKER_DEAL..MARKER_DEAL + 16] == stored
            && self.image[MARKER_BLOCK..MARKER_BLOCK + 48] == block[..]
        {
            return false;
        }
        self.image[MARKER_DEAL..MARKER_DEAL + 16].copy_from_slice(&stored);
        self.image[MARKER_BLOCK..MARKER_BLOCK + 48].copy_from_slice(block);
        true
    }

    /// `0x14025F0B0`: clears the marker; `false` when it was already empty.
    pub fn clear_marker(&mut self) -> bool {
        self.set_marker("", &[0; 48])
    }

    /// `0x14027FA30`: ring 0 is not empty and its newest record has the deal number `number` (or `number` is empty
    /// and the newest record has brand byte 1).
    pub fn newest_is(&self, number: &str) -> bool {
        self.ring_get(0, 0)
            .is_some_and(|newest| if number.is_empty() { newest.brand == 1 } else { newest.deal_number == number })
    }

    /// The TASMS-format flag (`E+83`): the record was written by an activation with `tasms.use_proxy`.
    pub fn tasms(&self) -> bool {
        self.image[TASMS_FLAG] != 0
    }

    /// `0x1402321D0`: the record's TASMS flag disagrees with the environment's `tasms.use_proxy` (`M+28272`). On the
    /// proxy backend the flag byte must be non-zero; otherwise it must not be exactly 1.
    pub fn backend_mismatch(&self, use_proxy: bool) -> bool {
        let flag = self.image[TASMS_FLAG];
        if use_proxy { flag == 0 } else { flag == 1 }
    }

    /// `0x14025F120` (2.5): stores the activation result. With the TASMS flag clear the authenticated-brand mask and
    /// the raw server mask go to their own fields; with it set the low 24 bits of the raw mask replace the mask and the
    /// flag is stored. The URL is always stored. `false` (nothing to save) when nothing changed.
    pub fn save_activation_info(&mut self, auth_mask: u32, raw_mask: u32, url: &str, tasms: bool) -> bool {
        let before = self.image.clone();
        if tasms {
            self.image[ENABLE_MASK..ENABLE_MASK + 3].copy_from_slice(&raw_mask.to_le_bytes()[..3]);
            self.image[TASMS_FLAG] = 1;
        } else {
            self.image[ENABLE_MASK..ENABLE_MASK + 4].copy_from_slice(&auth_mask.to_le_bytes());
            self.image[RAW_MASK..RAW_MASK + 4].copy_from_slice(&raw_mask.to_le_bytes());
        }
        self.set_url(url);
        self.image != before
    }

    /// `0x14025F5E0`: zeroes the payload and keeps the TASMS flag.
    pub fn clear_data(&mut self) {
        let tasms = self.image[TASMS_FLAG];
        self.image[8..].fill(0);
        self.image[TASMS_FLAG] = tasms;
    }

    /// The coin counters of the last successful report (`E+1476`).
    pub fn reported_counters(&self) -> [u32; 8] {
        std::array::from_fn(|slot| u32::from_le_bytes(le32(&self.image[COUNTERS + 4 * slot..])))
    }

    /// `0x14025F2E0`: stores the reported counters; `false` when identical.
    pub fn set_reported_counters(&mut self, counters: [u32; 8]) -> bool {
        let stored = self.reported_counters();
        if stored == counters {
            return false;
        }
        for (slot, value) in counters.iter().enumerate() {
            self.image[COUNTERS + 4 * slot..COUNTERS + 4 * slot + 4].copy_from_slice(&value.to_le_bytes());
        }
        true
    }

    /// `0x14025E6C0` (2.5): flag 0x10 entries lose the flag (and count as reported when cash is not allowed); the
    /// unreported entries then come last-to-oldest, i.e. the reported ones keep the front. Returns `true` on a change.
    pub fn update_deal_log(&mut self, ring: usize, cash_allowed: bool) -> bool {
        let records = self.ring_oldest_first(ring);
        let mut changed = false;
        let mut fixed: Vec<DealRecord> = Vec::with_capacity(records.len());
        for mut record in records {
            if record.flags & FLAG_OPEN != 0 {
                record.flags &= !FLAG_OPEN;
                if !cash_allowed {
                    record.flags |= FLAG_REPORTED;
                }
                changed = true;
            }
            fixed.push(record);
        }
        if changed {
            for (index, record) in fixed.iter().rev().enumerate() {
                self.ring_update(ring, index, record);
            }
        }
        changed
    }

    /// Marks the oldest `count` unreported records of a ring as reported (`0x14025EDF0`, success of a sales send).
    pub fn mark_reported(&mut self, ring: usize, count: usize) -> bool {
        let mut changed = false;
        let mut left = count;
        for index in (0..self.ring_count(ring)).rev() {
            if left == 0 {
                break;
            }
            let Some(mut record) = self.ring_get(ring, index) else { continue };
            if record.flags & FLAG_REPORTED == 0 {
                record.flags |= FLAG_REPORTED;
                self.ring_update(ring, index, &record);
                changed = true;
                left -= 1;
            }
        }
        changed
    }
}
