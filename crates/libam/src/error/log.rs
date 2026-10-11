//! The two persisted records of the error manager (`spec/libam/error.md` 18): `ErrorLog` (system file, the
//! first error after a reset) and `AppErrorLogInfo` (application file, the per-number occurrence counters and
//! the application ring).
//!
//! Both are plain images in the layout of the original, so a record written by the original reads back here and
//! the other way round. A change is built on a copy of the image and adopted only when the store succeeded.
//!
//! The persisted time is a Windows `FILETIME` of the item's local `tm` read as if it were UTC. The original
//! converts the local time to UTC with the system time zone before it stores it (`LocalFileTimeToFileTime`) and
//! back when it reads it; the backend's clock carries no time zone, so local times round-trip unchanged (inferred).

use crate::time::Tm;

use crate::nvram::{NvramContext, Record, RecordStatus};

/// Slots of both rings.
const RING: usize = 15;
/// `ErrorLog`: head byte.
const LOG_HEAD: usize = 8;
/// `ErrorLog`: count byte.
const LOG_COUNT: usize = 9;
/// `ErrorLog`: unresolved flag.
const LOG_FLAG: usize = 10;
/// `ErrorLog`: first entry.
const LOG_ENTRIES: usize = 32;
/// `ErrorLog`: bytes per entry.
const LOG_ENTRY_SIZE: usize = 32;
/// `AppErrorLogInfo`: row count (u32).
const APP_ROWS: usize = 8;
/// `AppErrorLogInfo`: first row.
const APP_ROW_BASE: usize = 16;
/// `AppErrorLogInfo`: row capacity.
pub const APP_ROW_CAPACITY: usize = 222;
/// `AppErrorLogInfo`: ring count byte.
const APP_RING_COUNT: usize = 1792;
/// `AppErrorLogInfo`: ring head byte.
const APP_RING_HEAD: usize = 1793;
/// `AppErrorLogInfo`: first ring entry.
const APP_RING_BASE: usize = 1808;
/// `AppErrorLogInfo`: bytes per ring entry.
const APP_RING_ENTRY: usize = 16;
/// Game id of an entry that was never written.
const DEFAULT_GAME_ID: [u8; 4] = *b"____";

/// Seconds between 1601-01-01 and 1970-01-01.
const FILETIME_EPOCH_SECONDS: i64 = 11_644_473_600;
/// `FILETIME` ticks (100 ns) per second.
const TICKS_PER_SECOND: u64 = 10_000_000;

/// Days from 1970-01-01 to a civil date (proleptic Gregorian, `month` 1..=12).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The civil date of a day count since 1970-01-01.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// `tm` to the persisted `FILETIME` (`0x1402631F0`): seconds 60 and 61 clamp to 59. A `tm` before 1601 stores 0.
pub fn to_filetime(tm: &Tm) -> u64 {
    let days = days_from_civil(i64::from(tm.year) + 1900, i64::from(tm.mon) + 1, i64::from(tm.mday));
    let seconds = days * 86_400
        + i64::from(tm.hour) * 3600
        + i64::from(tm.min) * 60
        + i64::from(tm.sec.min(59))
        + FILETIME_EPOCH_SECONDS;
    u64::try_from(seconds).map_or(0, |seconds| seconds * TICKS_PER_SECOND)
}

/// The persisted `FILETIME` back to a `tm` (`0x140262FD0`); 0 is the empty `tm`.
pub fn from_filetime(filetime: u64) -> Tm {
    if filetime == 0 {
        return Tm::default();
    }
    let seconds = (filetime / TICKS_PER_SECOND) as i64 - FILETIME_EPOCH_SECONDS;
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    Tm {
        sec: (rest % 60) as i32,
        min: (rest / 60 % 60) as i32,
        hour: (rest / 3600) as i32,
        mday: day as i32,
        mon: month as i32 - 1,
        year: (year - 1900) as i32,
        wday: (days + 4).rem_euclid(7) as i32,
        yday: (days - days_from_civil(year, 1, 1)) as i32,
        isdst: 0,
    }
}

fn read_u16(image: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([image[offset], image[offset + 1]])
}

fn read_u32(image: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([image[offset], image[offset + 1], image[offset + 2], image[offset + 3]])
}

fn read_u64(image: &[u8], offset: usize) -> u64 {
    let mut bytes = [0; 8];
    bytes.copy_from_slice(&image[offset..offset + 8]);
    u64::from_le_bytes(bytes)
}

/// The image of a freshly reset `ErrorLog` (`0x1402C7CE0`): placeholder word, game id `____` in every entry.
pub fn default_error_log() -> Vec<u8> {
    let mut image = Record::ErrorLog.blank_image();
    for slot in 0..RING {
        let at = LOG_ENTRIES + slot * LOG_ENTRY_SIZE + 8;
        image[at..at + 4].copy_from_slice(&DEFAULT_GAME_ID);
    }
    image
}

/// The image of a freshly reset `AppErrorLogInfo` (`0x14025A880`).
pub fn default_app_log() -> Vec<u8> {
    Record::AppErrorLogInfo.blank_image()
}

/// One `ErrorLog` entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogEntry {
    /// The persisted time.
    pub time: u64,
    /// Error number.
    pub number: u16,
    /// Error sub number.
    pub sub_number: u16,
}

/// The `ErrorLog` record object (`S+1168`): the loaded image plus what the record object keeps in memory.
#[derive(Debug, Default)]
pub struct ErrorLog {
    image: Option<Vec<u8>>,
    /// `rec+584`: the record knows the game id and loader count of this run.
    stamped: bool,
    /// `rec+606`: low byte of `AppLoaderCount`.
    loader: u8,
    /// `rec+600`: the current game id.
    game_id: [u8; 4],
    /// `rec+604` / `rec+608`: the last number and sub number logged.
    cache: (u16, u16),
    /// `rec+585`: the last `log` wrote a new entry.
    wrote_new: bool,
}

impl ErrorLog {
    /// `rec+65`: the image is loaded.
    pub fn is_loaded(&self) -> bool {
        self.image.is_some()
    }

    /// Record `load`: the image, with the default written back on a CRC mismatch.
    pub fn load(&mut self, nvram: &NvramContext) -> Result<(), RecordStatus> {
        self.image = Some(nvram.load_record(Record::ErrorLog, default_error_log)?);
        Ok(())
    }

    /// Forgets the loaded image (the application file changed under it); stamp state stays.
    pub fn unload(&mut self) {
        self.image = None;
    }

    /// Entries held, at most 15.
    pub fn count(&self) -> usize {
        self.image.as_ref().map_or(0, |image| usize::from(image[LOG_COUNT]).min(RING))
    }

    /// An error was logged and not reset since (`0x14025C470`).
    pub fn unresolved(&self) -> bool {
        self.image.as_ref().is_some_and(|image| image[LOG_FLAG] == 1)
    }

    /// Whether the last [`Self::log`] wrote a new entry (`rec+585`).
    pub fn wrote_new(&self) -> bool {
        self.wrote_new
    }

    fn slot(image: &[u8], newest_first: usize) -> usize {
        (usize::from(image[LOG_HEAD]) + RING - 1 + RING * 2 - newest_first) % RING
    }

    /// Entry `index` newest first (`0x14025C360`).
    pub fn entry(&self, index: usize) -> Option<LogEntry> {
        let image = self.image.as_ref()?;
        if index >= RING {
            return None;
        }
        let base = LOG_ENTRIES + Self::slot(image, index) * LOG_ENTRY_SIZE;
        Some(LogEntry {
            time: read_u64(image, base),
            number: read_u16(image, base + 12),
            sub_number: read_u16(image, base + 16),
        })
    }

    fn game_id_of_newest(image: &[u8]) -> [u8; 4] {
        let base = LOG_ENTRIES + Self::slot(image, 0) * LOG_ENTRY_SIZE + 8;
        [image[base], image[base + 1], image[base + 2], image[base + 3]]
    }

    fn store(&mut self, nvram: &NvramContext, image: Vec<u8>) -> Result<(), RecordStatus> {
        match nvram.store_record(Record::ErrorLog, &image) {
            RecordStatus::Ok => {
                self.image = Some(image);
                Ok(())
            }
            status => Err(status),
        }
    }

    /// `0x14025BD30` (init stage 4): remembers the loader count and the game id; a record whose newest entry
    /// belongs to another game id is resolved. Nothing is stored when the record is not loaded.
    pub fn stamp(&mut self, nvram: &NvramContext, loader: u8, game_id: [u8; 4]) {
        self.loader = loader;
        self.game_id = game_id;
        self.stamped = true;
        let Some(image) = self.image.as_ref() else { return };
        if image[LOG_COUNT].min(RING as u8) != 0 && Self::game_id_of_newest(image) != game_id {
            // The result is not looked at (`0x14025BD30`).
            let _ = self.resolve(nvram, true);
        }
        if self.cache.0 == 0
            && let Some(image) = self.image.as_ref()
            && image[LOG_FLAG] == 1
            && let Some(newest) = self.entry(0)
        {
            self.cache = (newest.number, newest.sub_number);
        }
    }

    /// `0x14025C180`: logs the error unless the record is unstamped or already holds an unresolved error. A
    /// number and sub number equal to the last logged ones only set the flag.
    pub fn log(&mut self, nvram: &NvramContext, number: u16, sub_number: u16, time: u64) -> Result<(), RecordStatus> {
        self.wrote_new = false;
        let Some(current) = self.image.as_ref() else { return Ok(()) };
        if !self.stamped || current[LOG_FLAG] == 1 {
            return Ok(());
        }
        let is_new = self.cache.0 == 0 || self.cache != (number, sub_number);
        let mut image = current.clone();
        if is_new {
            let head = usize::from(image[LOG_HEAD]) % RING;
            let base = LOG_ENTRIES + head * LOG_ENTRY_SIZE;
            image[base..base + LOG_ENTRY_SIZE].fill(0);
            image[base..base + 8].copy_from_slice(&time.to_le_bytes());
            image[base + 8..base + 12].copy_from_slice(&self.game_id);
            image[base + 12..base + 14].copy_from_slice(&number.to_le_bytes());
            image[base + 14] = self.loader;
            image[base + 16..base + 18].copy_from_slice(&sub_number.to_le_bytes());
            image[LOG_HEAD] = ((head + 1) % RING) as u8;
            image[LOG_COUNT] = image[LOG_COUNT].saturating_add(1).min(RING as u8);
        }
        image[LOG_FLAG] = 1;
        self.store(nvram, image)?;
        if is_new {
            self.cache = (number, sub_number);
            self.wrote_new = true;
        }
        Ok(())
    }

    /// `0x14025BF10`: drops the unresolved flag of a stamped record. `clear_cache` forgets the last logged error.
    pub fn resolve(&mut self, nvram: &NvramContext, clear_cache: bool) -> Result<(), RecordStatus> {
        let result = match self.image.as_ref() {
            Some(image) if self.stamped && image[LOG_FLAG] == 1 => {
                let mut image = image.clone();
                image[LOG_FLAG] = 0;
                self.store(nvram, image)
            }
            _ => Ok(()),
        };
        if clear_cache {
            self.cache = (0, 0);
        }
        result
    }

    /// `0x14025C050`: writes this run's loader count into the newest entry of an unresolved record.
    pub fn rewrite_loader(&mut self, nvram: &NvramContext) -> Result<(), RecordStatus> {
        let Some(image) = self.image.as_ref() else { return Ok(()) };
        if !self.stamped || image[LOG_FLAG] != 1 {
            return Ok(());
        }
        let mut image = image.clone();
        let base = LOG_ENTRIES + Self::slot(&image, 0) * LOG_ENTRY_SIZE;
        image[base + 14] = self.loader;
        self.store(nvram, image)
    }

    /// `0x14025BE10`: writes the default image.
    pub fn clear(&mut self, nvram: &NvramContext) -> Result<(), RecordStatus> {
        self.cache = (0, 0);
        self.store(nvram, default_error_log())
    }
}

/// One entry of the `AppErrorLogInfo` ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppLogEntry {
    /// The persisted time.
    pub time: u64,
    /// Error number.
    pub number: u16,
    /// Error sub number.
    pub sub_number: u16,
}

/// The `AppErrorLogInfo` record object (`S+4128`).
#[derive(Debug, Default)]
pub struct AppLog {
    image: Option<Vec<u8>>,
}

impl AppLog {
    /// `rec+65`: the image is loaded.
    pub fn is_loaded(&self) -> bool {
        self.image.is_some()
    }

    /// Record `load` (fails while `appfile.dat` is not open).
    pub fn load(&mut self, nvram: &NvramContext) -> Result<(), RecordStatus> {
        self.image = Some(nvram.load_record(Record::AppErrorLogInfo, default_app_log)?);
        Ok(())
    }

    /// Forgets the loaded image.
    pub fn unload(&mut self) {
        self.image = None;
    }

    fn rows(image: &[u8]) -> usize {
        (read_u32(image, APP_ROWS) as usize).min(APP_ROW_CAPACITY)
    }

    /// The counter of `number` (`0x14025D600`).
    pub fn times_of(&self, number: u16) -> Option<u32> {
        let image = self.image.as_ref()?;
        (0..Self::rows(image))
            .map(|row| APP_ROW_BASE + row * 8)
            .find(|&at| read_u16(image, at) == number)
            .map(|at| read_u32(image, at + 4))
    }

    /// Ring entries held, at most 15.
    pub fn count(&self) -> usize {
        self.image.as_ref().map_or(0, |image| usize::from(image[APP_RING_COUNT]).min(RING))
    }

    /// Ring entry `index` newest first (`0x14025D6F0`).
    pub fn entry(&self, index: usize) -> Option<AppLogEntry> {
        let image = self.image.as_ref()?;
        if index >= RING {
            return None;
        }
        let slot = (usize::from(image[APP_RING_HEAD]) + RING - 1 + RING * 2 - index) % RING;
        let base = APP_RING_BASE + slot * APP_RING_ENTRY;
        Some(AppLogEntry {
            time: read_u64(image, base),
            number: read_u16(image, base + 8),
            sub_number: read_u16(image, base + 10),
        })
    }

    /// `0x14025D4E0`: the ring entry and the counter row of one logged error (`times` is the new counter). A
    /// full table takes no new number; nothing is written then.
    pub fn log(
        &mut self,
        nvram: &NvramContext,
        number: u16,
        sub_number: u16,
        time: u64,
        times: u32,
    ) -> Result<(), RecordStatus> {
        let Some(current) = self.image.as_ref() else { return Ok(()) };
        let mut image = current.clone();
        let rows = Self::rows(&image);
        let existing = (0..rows).find(|&row| read_u16(&image, APP_ROW_BASE + row * 8) == number);
        match existing {
            Some(row) if read_u32(&image, APP_ROW_BASE + row * 8 + 4) == times => return Ok(()),
            Some(row) => {
                image[APP_ROW_BASE + row * 8 + 4..APP_ROW_BASE + row * 8 + 8].copy_from_slice(&times.to_le_bytes())
            }
            None if rows >= APP_ROW_CAPACITY => return Ok(()),
            None => {
                let at = (0..rows).find(|&row| read_u16(&image, APP_ROW_BASE + row * 8) > number).unwrap_or(rows);
                let start = APP_ROW_BASE + at * 8;
                let end = APP_ROW_BASE + rows * 8;
                image.copy_within(start..end, start + 8);
                image[start..start + 8].fill(0);
                image[start..start + 2].copy_from_slice(&number.to_le_bytes());
                image[start + 4..start + 8].copy_from_slice(&times.to_le_bytes());
                image[APP_ROWS..APP_ROWS + 4].copy_from_slice(&(rows as u32 + 1).to_le_bytes());
            }
        }
        let head = usize::from(image[APP_RING_HEAD]) % RING;
        let base = APP_RING_BASE + head * APP_RING_ENTRY;
        image[base..base + APP_RING_ENTRY].fill(0);
        image[base..base + 8].copy_from_slice(&time.to_le_bytes());
        image[base + 8..base + 10].copy_from_slice(&number.to_le_bytes());
        image[base + 10..base + 12].copy_from_slice(&sub_number.to_le_bytes());
        image[APP_RING_HEAD] = ((head + 1) % RING) as u8;
        image[APP_RING_COUNT] = image[APP_RING_COUNT].saturating_add(1).min(RING as u8);
        self.store(nvram, image)
    }

    /// `0x14025D400`: writes the default image.
    pub fn clear(&mut self, nvram: &NvramContext) -> Result<(), RecordStatus> {
        self.store(nvram, default_app_log())
    }

    fn store(&mut self, nvram: &NvramContext, image: Vec<u8>) -> Result<(), RecordStatus> {
        match nvram.store_record(Record::AppErrorLogInfo, &image) {
            RecordStatus::Ok => {
                self.image = Some(image);
                Ok(())
            }
            status => Err(status),
        }
    }
}
