//! The Aime play-log queue (`AppContext+0x2E48`, appended by `0x140261630`, counted by `0x1402618C0`) and its
//! NVRAM record `AimePlayLogInfo` (`appfile.dat` `0x1000`, `0x400` bytes, `storage.md` 3.6).
//!
//! The record image is `[0..4)` the CRC/placeholder dword the record layer owns, `[4..8)` the entry count, then up to
//! 30 entries of 32 bytes: `u8 0, u8 status, u16 region, u32 aime id, u32 credit, i64 time, u8 store byte,
//! 11 B serial`. The entry layout inside the record is not part of the binary-derived spec [INFERENCE]; only the
//! limit of 30 queued entries is.

use super::lib::{PlayLogRecord, PlayLogStore};

/// Maximum number of queued play logs (`0x1402618C0(store) >= 30` refuses, A.3).
pub(super) const CAPACITY: usize = 30;

const HEADER: usize = 8;
const ENTRY: usize = 32;

/// The queue of play logs waiting for the Aime DB.
#[derive(Debug, Default)]
pub(super) struct LogQueue {
    items: Vec<PlayLogRecord>,
    dirty: bool,
}

impl LogQueue {
    /// Loads the queue from a record image (a blank or damaged image gives an empty queue).
    pub(super) fn from_image(image: &[u8]) -> Self {
        let count = image
            .get(4..8)
            .map_or(0, |bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize)
            .min(CAPACITY);
        let mut items = Vec::with_capacity(count);
        for slot in 0..count {
            let at = HEADER + slot * ENTRY;
            let Some(raw) = image.get(at..at + ENTRY) else { break };
            let mut serial = [0u8; 11];
            serial.copy_from_slice(&raw[21..32]);
            items.push(PlayLogRecord {
                status: raw[1],
                region: u16::from_le_bytes([raw[2], raw[3]]),
                aime_id: u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]),
                credit: u32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]]),
                time: i64::from_le_bytes([raw[12], raw[13], raw[14], raw[15], raw[16], raw[17], raw[18], raw[19]]),
                store_byte: raw[20],
                serial,
            });
        }
        Self { items, dirty: false }
    }

    /// Writes the queue into `image` (the CRC dword is left to the record layer).
    pub(super) fn write_image(&self, image: &mut [u8]) {
        image[HEADER..].fill(0);
        image[4..8].copy_from_slice(&(self.items.len() as u32).to_le_bytes());
        for (slot, record) in self.items.iter().enumerate() {
            let raw = &mut image[HEADER + slot * ENTRY..HEADER + (slot + 1) * ENTRY];
            raw[1] = record.status;
            raw[2..4].copy_from_slice(&record.region.to_le_bytes());
            raw[4..8].copy_from_slice(&record.aime_id.to_le_bytes());
            raw[8..12].copy_from_slice(&record.credit.to_le_bytes());
            raw[12..20].copy_from_slice(&record.time.to_le_bytes());
            raw[20] = record.store_byte;
            raw[21..32].copy_from_slice(&record.serial);
        }
    }

    /// `0x140261630`: appends a record; `false` when 30 are queued.
    pub(super) fn push(&mut self, record: PlayLogRecord) -> bool {
        if self.items.len() >= CAPACITY {
            return false;
        }
        self.items.push(record);
        self.dirty = true;
        true
    }

    /// Forgets every record (`0x14022A5E0`).
    pub(super) fn clear(&mut self) {
        self.items.clear();
        self.dirty = true;
    }

    /// The queue changed since the last [`Self::take_dirty`].
    pub(super) fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }
}

impl PlayLogStore for LogQueue {
    fn len(&self) -> usize {
        self.items.len()
    }

    fn get(&self, index: usize) -> Option<PlayLogRecord> {
        self.items.get(index).copied()
    }

    fn remove(&mut self, index: usize) {
        if index < self.items.len() {
            self.items.remove(index);
            self.dirty = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(aime_id: u32) -> PlayLogRecord {
        PlayLogRecord {
            status: 2,
            region: 1,
            aime_id,
            credit: 3,
            time: 1_700_000_000,
            store_byte: 0,
            serial: *b"A63E01E1234",
        }
    }

    #[test]
    fn queue_round_trips_through_the_record_image_and_stops_at_thirty() {
        let mut queue = LogQueue::default();
        for id in 0..40 {
            let accepted = queue.push(record(id));
            assert_eq!(accepted, id < 30);
        }
        let mut image = vec![0u8; 0x400];
        queue.write_image(&mut image);
        let loaded = LogQueue::from_image(&image);
        assert_eq!(PlayLogStore::len(&loaded), 30);
        assert_eq!(loaded.get(29), Some(record(29)));
    }
}
