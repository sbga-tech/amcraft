//! `amdaemon::ipc::Backup`: the protected shared-memory object that carries NVRAM records between the daemon
//! and the game (`spec/controllers/backup_appimage.md` §3).
//!
//! Section `amdipc_shm_daemon_backup_220401_<k>` has payload `0x110 + body` bytes (`body` = total NVRAM
//! capacity, 4 186 112 in the original build):
//!
//! ```text
//! payload[0 .. 0x110)   header, protected with the same-logon memory protector
//!     0x000  u32 crc32          CRC-32 of the next 0x104 bytes
//!     0x004  u32 record_count   <= 64
//!     0x008  u32 sizes[64]
//!     0x108  8 bytes padding    not covered by the CRC
//! payload[0x110 ..)     record slots, slot i at body + offsets[i]
//!     slot_i = (size_i + 19) & !15 bytes, protected:  u32 crc32(payload) ‖ payload[size_i] ‖ padding
//! ```
//!
//! Neither header nor slots are locked against concurrent access (the daemon writes, the game reads); integrity
//! rests on the CRC checks. [`Backup::create`] keeps a valid header found in an existing section, so a restarted
//! daemon attaches to the records of the previous run; a corrupt header is re-initialised to zero records.

use std::cell::RefCell;

use crate::ipc::relation::RELATION_COUNT;
use crate::ipc::shared_memory::SharedMemory;
use crate::schema::backup::Record;
use backend::ipc::{IpcPlatform, Services};

/// Most records a Setup can describe (`0x40`).
pub use crate::schema::backup::MAX_RECORDS;
/// Size of the protected header region (`(0x104 + 19) & !15`).
pub const HEADER_LEN: usize = 0x110;
/// Size of the CRC-covered header content: `record_count` plus `sizes[64]`.
const HEADER_BODY_LEN: usize = 4 + 4 * MAX_RECORDS;
/// Prefix of the section name (`0x1405EFB18`).
const NAME_PREFIX: &str = "daemon_backup_220401_";

/// Name of the section (without the `amdipc_shm_` prefix) for relation index `index`.
pub fn section_name(index: u32) -> String {
    format!("{NAME_PREFIX}{index}")
}

/// Slot size of a record: `ceil16(size + 4)`, at least 16 (32-bit arithmetic like the original).
const fn slot_len(size: u32) -> u32 {
    size.wrapping_add(19) & !15
}

/// Attached section plus the decoded header (`Backup::Data`, `0x148` bytes).
#[derive(Debug)]
struct Data {
    shm: SharedMemory,
    record_count: u32,
    sizes: [u32; MAX_RECORDS],
    /// Per-record slot offsets into the body (`0x140070240`).
    offsets: Vec<u32>,
    /// Crypt/CRC working buffer; resizing keeps old bytes and zero-fills growth.
    scratch: RefCell<Vec<u8>>,
}

impl Data {
    fn new(shm: SharedMemory) -> Self {
        Self { shm, record_count: 0, sizes: [0; MAX_RECORDS], offsets: Vec::new(), scratch: RefCell::new(Vec::new()) }
    }

    /// Bytes available for record slots.
    fn body_capacity(&self) -> u32 {
        self.shm.size().saturating_sub(HEADER_LEN as u32)
    }

    /// `(offset, length)` of record `index`'s slot inside the body when it lies within the section.
    ///
    /// Deviation: the original trusts a CRC-valid header whose slots exceed the body; here such slots fail.
    fn slot_range(&self, index: u32) -> Option<(usize, usize)> {
        let offset = *self.offsets.get(index as usize)? as usize;
        let len = slot_len(*self.sizes.get(index as usize)?) as usize;
        (offset.checked_add(len)? <= self.body_capacity() as usize).then_some((offset, len))
    }

    /// Serialises `count` and `sizes` into the CRC-covered header body.
    fn header_body(count: u32, sizes: &[u32; MAX_RECORDS]) -> [u8; HEADER_BODY_LEN] {
        let mut bytes = [0u8; HEADER_BODY_LEN];
        bytes[..4].copy_from_slice(&count.to_le_bytes());
        for (chunk, size) in bytes[4..].chunks_exact_mut(4).zip(sizes) {
            chunk.copy_from_slice(&size.to_le_bytes());
        }
        bytes
    }

    /// Protects and stores a header (`0x14006FFF0` for the header, `setupRecords` tail). The scratch buffer is
    /// reused, so the 8 padding bytes keep whatever it held.
    fn store_header(&mut self, services: &Services, count: u32, sizes: &[u32; MAX_RECORDS]) -> bool {
        let body = Self::header_body(count, sizes);
        let mut scratch = self.scratch.borrow_mut();
        scratch.resize(HEADER_LEN, 0);
        scratch[..4].copy_from_slice(&crc32fast::hash(&body).to_le_bytes());
        scratch[4..4 + HEADER_BODY_LEN].copy_from_slice(&body);
        services.protect(&mut scratch) && self.shm.write(0, &scratch)
    }

    /// Reads, unprotects and CRC-checks the header (`0x14006FF40`); on success `record_count`/`sizes` hold it.
    fn load_header(&mut self, services: &Services) -> bool {
        let mut scratch = self.scratch.borrow_mut();
        scratch.resize(HEADER_LEN, 0);
        if !self.shm.read(0, &mut scratch) || !services.unprotect(&mut scratch) {
            return false;
        }
        let stored = u32::from_le_bytes([scratch[0], scratch[1], scratch[2], scratch[3]]);
        let body = &scratch[4..4 + HEADER_BODY_LEN];
        if crc32fast::hash(body) != stored {
            return false;
        }
        self.record_count = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
        for (i, size) in self.sizes.iter_mut().enumerate() {
            *size = u32::from_le_bytes([body[4 + 4 * i], body[5 + 4 * i], body[6 + 4 * i], body[7 + 4 * i]]);
        }
        true
    }

    /// `0x140070240`: slot offsets for `sizes[..count]`; returns the total, or `u32::MAX` on overflow (the
    /// offsets after the overflow stay 0).
    fn compute_offsets(sizes: &[u32; MAX_RECORDS], count: u32, offsets: &mut Vec<u32>) -> u32 {
        offsets.clear();
        offsets.resize(count as usize, 0);
        let mut total = 0u32;
        for (offset, &size) in offsets.iter_mut().zip(sizes) {
            *offset = total;
            let slot = slot_len(size);
            if total > u32::MAX - slot {
                return u32::MAX;
            }
            total += slot;
        }
        total
    }
}

/// `amdaemon::ipc::Backup`.
#[derive(Debug)]
pub struct Backup {
    platform: Box<dyn IpcPlatform>,
    services: Services,
    data: Option<Data>,
}

impl Backup {
    /// Creates an empty handle (`0x14006DC30`); [`create`](Self::create) or [`open`](Self::open) attaches it.
    pub fn new(platform: Box<dyn IpcPlatform>) -> Self {
        let services = platform.services();
        Self { platform, services, data: None }
    }

    /// Creates the section for `relation_index` with `size` bytes of record space, or attaches to the existing
    /// one (`0x14006DD00`). A valid existing header is kept; an invalid one is replaced by an empty header.
    /// Fails for an index outside `0..32`, `size == 0`, a section created by someone else, or an existing
    /// header announcing more than 64 records. Any previous attachment is released first.
    pub fn create(&mut self, relation_index: i32, size: u32) -> bool {
        self.data = None;
        let Some(index) = Self::valid_index(relation_index) else {
            tracing::error!(relation_index, "backup relation index is out of range");
            return false;
        };
        if size == 0 {
            tracing::error!("backup size is zero");
            return false;
        }
        let total = size.wrapping_add(HEADER_LEN as u32);
        let Ok(shm) = SharedMemory::create(self.platform.as_ref(), total, &section_name(index), false) else {
            return false;
        };
        if shm.size() as usize <= HEADER_LEN {
            tracing::error!("backup section belongs to another owner");
            return false;
        }
        let mut data = Data::new(shm);
        if data.load_header(&self.services) {
            if data.record_count > MAX_RECORDS as u32 {
                tracing::error!(
                    count = data.record_count,
                    limit = MAX_RECORDS,
                    "backup header announces too many records"
                );
                return false;
            }
        } else {
            data.record_count = 0;
            data.sizes = [0; MAX_RECORDS];
            if !data.store_header(&self.services, 0, &[0; MAX_RECORDS]) {
                tracing::error!("encrypting the backup header failed");
                return false;
            }
        }
        Data::compute_offsets(&data.sizes, data.record_count, &mut data.offsets);
        self.data = Some(data);
        true
    }

    /// Attaches to an existing section (`0x14006E580`, the game-side entry point): the header must decrypt
    /// and verify, nothing is initialised.
    pub fn open(&mut self, relation_index: i32) -> bool {
        self.data = None;
        let Some(index) = Self::valid_index(relation_index) else {
            tracing::error!(relation_index, "backup relation index is out of range");
            return false;
        };
        let Ok(shm) = SharedMemory::open(self.platform.as_ref(), &section_name(index)) else { return false };
        if shm.size() as usize <= HEADER_LEN {
            tracing::error!("backup section belongs to another owner");
            return false;
        }
        let mut data = Data::new(shm);
        if !data.load_header(&self.services) {
            tracing::error!("decrypting the backup header failed");
            return false;
        }
        if data.record_count > MAX_RECORDS as u32 {
            tracing::error!(count = data.record_count, limit = MAX_RECORDS, "backup header announces too many records");
            return false;
        }
        Data::compute_offsets(&data.sizes, data.record_count, &mut data.offsets);
        self.data = Some(data);
        true
    }

    /// Bytes available for record slots; 0 while nothing is attached (`0x14006E6D0`).
    pub fn body_capacity(&self) -> u32 {
        self.data.as_ref().map_or(0, Data::body_capacity)
    }

    /// Number of records of the current layout (`0x14006E720`).
    pub fn record_count(&self) -> u32 {
        self.data.as_ref().map_or(0, |data| data.record_count)
    }

    /// Replaces the record layout (`setupRecords`, `0x14006E0A0`): stores the record sizes in a fresh
    /// protected header. Slot contents are not touched, so unwritten slots fail their CRC until
    /// [`set_record`](Self::set_record) writes them. Only `Record::size` matters here: the device selects the
    /// NVRAM area in the controller, not the shared-memory layout.
    ///
    /// `false` for more than 64 records, when nothing is attached, when the slots do not fit the body (the
    /// previous layout is kept) or when the header cannot be protected.
    pub fn setup_records(&mut self, records: &[Record]) -> bool {
        if records.len() > MAX_RECORDS {
            tracing::error!(count = records.len(), limit = MAX_RECORDS, "too many backup records");
            return false;
        }
        let Some(data) = self.data.as_mut().filter(|data| data.shm.size() as usize > HEADER_LEN) else {
            tracing::warn!("backup section is not open");
            return false;
        };
        let mut sizes = [0u32; MAX_RECORDS];
        for (size, record) in sizes.iter_mut().zip(records) {
            *size = record.size;
        }
        let count = records.len() as u32;
        let mut offsets = std::mem::take(&mut data.offsets);
        let total = Data::compute_offsets(&sizes, count, &mut offsets);
        if total > data.body_capacity() {
            data.offsets = offsets;
            return false;
        }
        if !data.store_header(&self.services, count, &sizes) {
            tracing::error!("encrypting the backup header failed");
            data.offsets = offsets;
            return false;
        }
        data.record_count = count;
        data.sizes = sizes;
        data.offsets = offsets;
        true
    }

    /// Payload size of record `index` (`getRecordSize`, `0x14006E730`); `None` when out of range.
    pub fn record_size(&self, index: u32) -> Option<u32> {
        let size = self.data.as_ref().filter(|data| index < data.record_count).map(|data| data.sizes[index as usize]);
        if size.is_none() {
            tracing::error!(index, "backup record index is out of range");
        }
        size
    }

    /// Copies the payload of record `index` into the front of `out` (`getRecord`, `0x14006E7C0`): reads the
    /// slot, unprotects it and verifies its CRC. `false` on a bad index, a too-short `out`, a slot
    /// outside the section, or a decrypt/CRC failure.
    pub fn get_record(&self, index: u32, out: &mut [u8]) -> bool {
        let Some(data) = self.data.as_ref().filter(|data| index < data.record_count) else {
            tracing::error!(index, "backup record index is out of range");
            return false;
        };
        let size = data.sizes[index as usize];
        let Some((offset, len)) = data.slot_range(index) else { return false };
        let Some(out) = out.get_mut(..size as usize) else { return false };
        let mut scratch = data.scratch.borrow_mut();
        scratch.resize(len, 0);
        if !data.shm.read(HEADER_LEN + offset, &mut scratch) || !self.services.unprotect(&mut scratch) {
            return false;
        }
        let payload = &scratch[4..4 + size as usize];
        let stored = u32::from_le_bytes([scratch[0], scratch[1], scratch[2], scratch[3]]);
        if crc32fast::hash(payload) != stored {
            return false;
        }
        out.copy_from_slice(payload);
        true
    }

    /// Stores the first `size` bytes of `payload` as record `index` (`setRecord`, `0x14006E8D0`): CRC,
    /// protect, write the slot. `false` on a bad index, a too-short `payload`, a slot outside the
    /// section, or a protection failure.
    pub fn set_record(&mut self, index: u32, payload: &[u8]) -> bool {
        let Some(data) = self.data.as_mut().filter(|data| index < data.record_count) else {
            tracing::error!(index, "backup record index is out of range");
            return false;
        };
        let size = data.sizes[index as usize];
        let Some((offset, len)) = data.slot_range(index) else { return false };
        let Some(payload) = payload.get(..size as usize) else { return false };
        let mut scratch = data.scratch.borrow_mut();
        scratch.resize(len, 0);
        scratch[..4].copy_from_slice(&crc32fast::hash(payload).to_le_bytes());
        scratch[4..4 + size as usize].copy_from_slice(payload);
        if !self.services.protect(&mut scratch) {
            tracing::error!("encrypting the backup record failed");
            return false;
        }
        data.shm.write(HEADER_LEN + offset, &scratch)
    }

    /// Validates a relation index (`0 <= index < 32`).
    fn valid_index(relation_index: i32) -> Option<u32> {
        match u32::try_from(relation_index) {
            Ok(index) if index < RELATION_COUNT => Some(index),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::schema::backup::BackupDevice;

    use super::*;
    use backend::mock::ipc::LoopbackPlatform;
    use backend::mock::ipc::protector::XorProtector;

    const KEY: u8 = 0x3C;
    const SECTION: &str = "amdipc_shm_daemon_backup_220401_0";

    fn platform() -> LoopbackPlatform {
        LoopbackPlatform::new().with_protector(Arc::new(XorProtector(KEY)))
    }

    fn backup(platform: &LoopbackPlatform) -> Backup {
        Backup::new(Box::new(platform.clone()))
    }

    fn xor(bytes: &[u8]) -> Vec<u8> {
        bytes.iter().map(|b| b ^ KEY).collect()
    }

    fn spec(size: u32) -> Record {
        Record { device: BackupDevice::File, size }
    }

    /// Raw section bytes `[from, from + len)` counted from the start of the payload.
    fn payload(platform: &LoopbackPlatform, from: usize, len: usize) -> Vec<u8> {
        let section = platform.open_section(SECTION).unwrap();
        let mut bytes = vec![0; len];
        assert!(section.read(8 + from, &mut bytes));
        bytes
    }

    /// Writes a header with a valid CRC (as the game or a previous daemon would have left it).
    fn forge_header(platform: &LoopbackPlatform, count: u32, sizes: &[u32]) {
        let mut all = [0u32; MAX_RECORDS];
        all[..sizes.len()].copy_from_slice(sizes);
        let body = Data::header_body(count, &all);
        let mut plain = crc32fast::hash(&body).to_le_bytes().to_vec();
        plain.extend_from_slice(&body);
        plain.resize(HEADER_LEN, 0);
        let mut section = platform.open_section(SECTION).unwrap();
        assert!(section.write(8, &xor(&plain)));
    }

    #[test]
    fn slots_follow_the_documented_layout_and_protection() {
        let platform = platform();
        let mut backup = backup(&platform);
        assert!(backup.create(0, 0x3FE000));
        assert!(backup.setup_records(&[spec(0x100), spec(0x20), spec(0)]));
        assert_eq!(backup.record_count(), 3);
        assert_eq!([0, 1, 2, 3].map(|i| backup.record_size(i)), [Some(0x100), Some(0x20), Some(0), None]);

        // Header plaintext: crc, count 3, sizes {0x100, 0x20, 0, ...}.
        let plain = xor(&payload(&platform, 0, HEADER_LEN));
        assert_eq!(plain[4..8], 3u32.to_le_bytes());
        assert_eq!(plain[8..20], [0, 1, 0, 0, 0x20, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(u32::from_le_bytes(plain[..4].try_into().unwrap()), crc32fast::hash(&plain[4..0x108]));

        let data0: Vec<u8> = (0..0x100).map(|i| i as u8).collect();
        let data1 = [0xAB; 0x20];
        assert!(backup.set_record(0, &data0));
        assert!(backup.set_record(1, &data1));
        assert!(backup.set_record(2, &[]));
        // Slots of 0x110, 0x30 and 0x10 bytes at body offsets 0, 0x110, 0x140 (payload 0x110 + ...).
        let slot0 = xor(&payload(&platform, 0x110, 0x110));
        assert_eq!(slot0[..4], crc32fast::hash(&data0).to_le_bytes());
        assert_eq!(slot0[4..0x104], data0[..]);
        let slot1 = xor(&payload(&platform, 0x220, 0x30));
        assert_eq!(slot1[..4], crc32fast::hash(&data1).to_le_bytes());
        assert_eq!(slot1[4..0x24], data1);
        let slot2 = xor(&payload(&platform, 0x250, 0x10));
        assert_eq!(slot2[..4], 0u32.to_le_bytes(), "crc of the empty payload");
    }

    #[test]
    fn records_round_trip_and_unwritten_or_damaged_slots_fail_their_crc() {
        let platform = platform();
        let mut backup = backup(&platform);
        assert!(backup.create(0, 4096));
        assert!(backup.setup_records(&[spec(100), spec(40)]));

        let mut out = [0u8; 100];
        assert!(!backup.get_record(0, &mut out), "never written: the slot is zero, the CRC cannot match");
        let data: Vec<u8> = (1..=100).collect();
        assert!(backup.set_record(0, &data));
        assert!(backup.get_record(0, &mut out));
        assert_eq!(out[..], data[..]);

        // Flip one payload bit behind the backup's back.
        let mut section = platform.open_section(SECTION).unwrap();
        let mut byte = [0u8; 1];
        assert!(section.read(8 + 0x110 + 10, &mut byte));
        assert!(section.write(8 + 0x110 + 10, &[byte[0] ^ 1]));
        assert!(!backup.get_record(0, &mut out));

        // Larger destination buffers are fine, smaller ones are refused.
        assert!(backup.set_record(1, &[7; 64]));
        let mut big = [0u8; 64];
        assert!(backup.get_record(1, &mut big));
        assert_eq!(big[..40], [7; 40]);
        assert_eq!(big[40..], [0; 24], "bytes past the record are untouched");
        assert!(!backup.get_record(1, &mut [0u8; 39]));
        assert!(!backup.set_record(1, &[7; 39]));
    }

    #[test]
    fn the_game_side_opens_what_the_daemon_wrote() {
        let platform = platform();
        let mut daemon = backup(&platform);
        assert!(daemon.create(3, 1024));
        assert!(daemon.setup_records(&[spec(16), spec(5)]));
        assert!(daemon.set_record(1, b"hello"));

        let mut game = backup(&platform);
        assert!(game.open(3));
        assert_eq!((game.record_count(), game.record_size(0), game.record_size(1)), (2, Some(16), Some(5)));
        let mut out = [0u8; 5];
        assert!(game.get_record(1, &mut out));
        assert_eq!(&out, b"hello");
        assert_eq!(game.body_capacity(), 1024);

        let mut absent = backup(&platform);
        assert!(!absent.open(4));
    }

    #[test]
    fn create_keeps_the_valid_header_and_records_of_a_previous_run() {
        let platform = platform();
        let mut first = backup(&platform);
        assert!(first.create(0, 2048));
        assert!(first.setup_records(&[spec(8)]));
        assert!(first.set_record(0, b"persist!"));
        let mut game = backup(&platform);
        assert!(game.open(0)); // keeps the section alive while the daemon "restarts"
        drop(first);

        let mut second = backup(&platform);
        assert!(second.create(0, 2048));
        assert_eq!(second.record_count(), 1);
        let mut out = [0u8; 8];
        assert!(second.get_record(0, &mut out));
        assert_eq!(&out, b"persist!");
    }

    #[test]
    fn a_corrupt_header_is_reinitialised_by_create_but_refused_by_open() {
        let platform = platform();
        let mut first = backup(&platform);
        assert!(first.create(0, 2048));
        assert!(first.setup_records(&[spec(8)]));

        let mut section = platform.open_section(SECTION).unwrap();
        let mut byte = [0u8; 1];
        assert!(section.read(8 + 6, &mut byte));
        assert!(section.write(8 + 6, &[byte[0] ^ 0x40]));

        let mut game = backup(&platform);
        assert!(!game.open(0));

        let mut daemon = backup(&platform);
        assert!(daemon.create(0, 2048));
        assert_eq!(daemon.record_count(), 0, "re-initialised to zero records");
        assert!(game.open(0), "the new header is valid again");
        assert_eq!(game.record_count(), 0);
    }

    #[test]
    fn headers_announcing_more_than_64_records_are_refused() {
        let platform = platform();
        let mut first = backup(&platform);
        assert!(first.create(0, 2048));
        forge_header(&platform, 65, &[]);
        let mut again = backup(&platform);
        assert!(!again.create(0, 2048));
        assert!(!again.open(0));
        assert_eq!(again.body_capacity(), 0, "nothing stays attached after a failure");
    }

    #[test]
    fn layouts_that_do_not_fit_the_body_are_refused_and_keep_the_old_layout() {
        let platform = platform();
        let mut backup = backup(&platform);
        assert!(backup.create(0, 64));
        assert!(backup.setup_records(&[spec(60)])); // slot ceil16(60 + 4) = 64 = the whole body
        assert!(!backup.setup_records(&[spec(61)])); // slot 80 > 64
        assert!(!backup.setup_records(&[spec(20), spec(20), spec(20)])); // 3 * 32
        assert!(!backup.setup_records(&[spec(0x7FFF_FFFF)]));
        assert_eq!((backup.record_count(), backup.record_size(0)), (1, Some(60)));
    }

    #[test]
    fn a_forged_header_cannot_make_slots_leave_the_section() {
        let platform = platform();
        let mut first = backup(&platform);
        assert!(first.create(0, 64));
        forge_header(&platform, 2, &[40, 0x1000_0000]);
        let mut backup = backup(&platform);
        assert!(backup.create(0, 64), "the original also accepts a CRC-valid header without checking totals");
        assert_eq!(backup.record_count(), 2);
        assert!(!backup.get_record(1, &mut [0u8; 16]));
        assert!(!backup.set_record(1, &[0; 8]));
    }
}
