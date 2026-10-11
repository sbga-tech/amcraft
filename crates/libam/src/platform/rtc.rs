//! The RTC (`amRtc`, `spec/libam/platform.md` part D 2 and 5): the TimezoneInfo record of `sysfile.dat`, the
//! time getters ALL.Net and the network delivery read, and `amRtcSetServerTime`, the only call that changes the
//! operating-system clock (ALL.Net time synchronisation).
//!
//! There is no manager: the modules that use the RTC (ALL.Net, network delivery) each hold an [`Rtc`] and call
//! [`Rtc::init`] when their context is created. The unused asynchronous API of the original (`amRtcUpdate`, the
//! location, clock-setting and start-up time-zone setters) has no caller in the binary and is not reproduced.
//!
//! The clock reads go through the [`Clock`] interface, which only knows the local wall clock. The UTC time is that
//! clock shifted by the bias of the operating-system time zone, which this module tracks: the location bias of the
//! stored record at start (the cabinet's zone, `-540` by default) and the bias it last set `[I]`.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::time::DateTime;

use crate::clock::Clock;
use crate::nvram::NvramContext;
use backend::system::TransitionRule;
use backend::{SystemActions, TimeZoneInformation, WallTime};

/// Size of the TimezoneInfo record.
const RECORD_SIZE: usize = 0x200;
/// Offset of the primary copy in `sysfile.dat`.
const PRIMARY_OFFSET: u64 = 0x1400;
/// Offset of the mirror copy.
const MIRROR_OFFSET: u64 = 0x4400;
/// The earliest instant the OS clock is set to: 2010-01-01 00:00:00 UTC (`0x140300750`).
const ORIGIN: RtcTime = RtcTime { year: 2010, month: 1, day: 1, hour: 0, minute: 0, second: 0 };
/// Year the `time_t` epoch starts in.
const UNIX_EPOCH_YEAR: i32 = 1970;
/// The default location and server bias: JST, `-540` minutes.
const DEFAULT_BIAS: i32 = -540;

/// Why an `amRtc*` call did not succeed; [`RtcError::code`] is the original's return value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RtcError {
    /// `-3`: the RTC is not initialised.
    #[error("the RTC is not initialised")]
    NotInitialised,
    /// `-4`: initialised already.
    #[error("the RTC is initialised already")]
    AlreadyInitialised,
    /// `-8`: the backup (system file) layer is not initialised.
    #[error("the system file is not initialised")]
    NoSystemFile,
    /// `-2`: a field is out of range.
    #[error("invalid argument")]
    InvalidArgument,
    /// `-1`: a conversion or an OS call failed.
    #[error("the operation failed")]
    Failed,
    /// `-5`: the record cannot be read or written.
    #[error("the time-zone record cannot be accessed")]
    Storage,
}

impl RtcError {
    /// The return value of the original API.
    pub const fn code(self) -> i32 {
        match self {
            Self::NotInitialised => -3,
            Self::AlreadyInitialised => -4,
            Self::NoSystemFile => -8,
            Self::InvalidArgument => -2,
            Self::Failed => -1,
            Self::Storage => -5,
        }
    }
}

/// `RtcTime`: the calendar fields of a time-zone-naive instant.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RtcTime {
    /// Year.
    pub year: u16,
    /// Month `1..=12`.
    pub month: u8,
    /// Day `1..=31`.
    pub day: u8,
    /// Hour `0..=23`.
    pub hour: u8,
    /// Minute `0..=59`.
    pub minute: u8,
    /// Second `0..=60`; 60 is read as 59.
    pub second: u8,
}

/// Seconds between 0001-01-01 and 1970-01-01 in [`DateTime`] units.
fn epoch_micros() -> i64 {
    DateTime::make(UNIX_EPOCH_YEAR, 1, 1, 0, 0, 0, 0).map_or(0, |epoch| epoch.0)
}

impl RtcTime {
    /// `amRtcConvertStructToTimet` (`0x1402D9390`): the range check (`-2`), then the seconds since 1970 of the
    /// naive instant (`-1` for an impossible date or a year before 1601).
    pub fn to_time_t(self) -> Result<i64, RtcError> {
        if self.month > 12 || self.day > 31 || self.hour > 23 || self.minute > 59 || self.second > 60 {
            return Err(RtcError::InvalidArgument);
        }
        if self.year < 1601 {
            return Err(RtcError::Failed);
        }
        let second = i32::from(self.second.min(59));
        let instant = DateTime::make(
            i32::from(self.year),
            i32::from(self.month),
            i32::from(self.day),
            i32::from(self.hour),
            i32::from(self.minute),
            second,
            0,
        )
        .ok_or(RtcError::Failed)?;
        Ok((instant.0 - epoch_micros()).div_euclid(DateTime::MICROS_PER_SECOND))
    }

    /// `amRtcConvertTimetToStruct` (`0x1402D9680`).
    pub fn from_time_t(time: i64) -> Result<Self, RtcError> {
        let micros = time
            .checked_mul(DateTime::MICROS_PER_SECOND)
            .and_then(|micros| micros.checked_add(epoch_micros()))
            .ok_or(RtcError::Failed)?;
        if micros < 0 {
            return Err(RtcError::Failed);
        }
        let civil = DateTime(micros).civil();
        let year = u16::try_from(civil.year).map_err(|_| RtcError::Failed)?;
        if year < 1601 {
            return Err(RtcError::Failed);
        }
        // The civil fields are below 60, so the narrowing is exact.
        Ok(Self {
            year,
            month: civil.month as u8,
            day: civil.day as u8,
            hour: civil.hour as u8,
            minute: civil.minute as u8,
            second: civil.second as u8,
        })
    }

    fn wall_time(self) -> WallTime {
        WallTime {
            year: self.year,
            month: u16::from(self.month),
            day: u16::from(self.day),
            hour: u16::from(self.hour),
            minute: u16::from(self.minute),
            second: u16::from(self.second.min(59)),
        }
    }
}

/// A daylight-saving rule of the record: month, week, day of week, hour, minute.
pub type DstRule = [u8; 5];

/// The persisted TimezoneInfo record (2.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeZoneRecord {
    /// Start of daylight saving (`0x08`).
    pub dst_start: DstRule,
    /// End of daylight saving (`0x10`).
    pub dst_end: DstRule,
    /// Location bias in minutes (`UTC = local + bias`), `0x18`.
    pub location_bias: i32,
    /// Server bias in minutes, `0x1C`.
    pub server_bias: i32,
    /// UTC `time_t` of the last server synchronisation, 0 for never (`0x20`).
    pub sync_stamp: i64,
}

impl Default for TimeZoneRecord {
    /// The record `0x1402C7F10` builds when both stored copies fail their CRC.
    fn default() -> Self {
        Self {
            dst_start: [0; 5],
            dst_end: [0; 5],
            location_bias: DEFAULT_BIAS,
            server_bias: DEFAULT_BIAS,
            sync_stamp: 0,
        }
    }
}

impl TimeZoneRecord {
    /// The record image with its CRC filled in.
    fn to_bytes(self) -> [u8; RECORD_SIZE] {
        let mut bytes = [0u8; RECORD_SIZE];
        bytes[0x08..0x0D].copy_from_slice(&self.dst_start);
        bytes[0x10..0x15].copy_from_slice(&self.dst_end);
        bytes[0x18..0x1C].copy_from_slice(&self.location_bias.to_le_bytes());
        bytes[0x1C..0x20].copy_from_slice(&self.server_bias.to_le_bytes());
        bytes[0x20..0x28].copy_from_slice(&self.sync_stamp.to_le_bytes());
        let crc = crc32(&bytes[4..]);
        bytes[..4].copy_from_slice(&crc.to_le_bytes());
        bytes
    }

    /// The record of an image whose CRC matches.
    fn from_bytes(bytes: &[u8; RECORD_SIZE]) -> Option<Self> {
        let stored = u32::from_le_bytes(bytes[..4].try_into().ok()?);
        if stored != crc32(&bytes[4..]) {
            return None;
        }
        Some(Self {
            dst_start: bytes[0x08..0x0D].try_into().ok()?,
            dst_end: bytes[0x10..0x15].try_into().ok()?,
            location_bias: i32::from_le_bytes(bytes[0x18..0x1C].try_into().ok()?),
            server_bias: i32::from_le_bytes(bytes[0x1C..0x20].try_into().ok()?),
            sync_stamp: i64::from_le_bytes(bytes[0x20..0x28].try_into().ok()?),
        })
    }

    /// `amBackupRecordReadDup`: the primary copy, else the mirror (which repairs the primary). `Ok(None)` when
    /// both fail their CRC (the caller keeps the default and writes nothing).
    pub fn load(path: &Path) -> Result<Option<Self>, RtcError> {
        let mut file = OpenOptions::new().read(true).write(true).open(path).map_err(|_| RtcError::Storage)?;
        let primary = read_copy(&mut file, PRIMARY_OFFSET);
        if let Some(record) = primary.as_ref().and_then(Self::from_bytes) {
            return Ok(Some(record));
        }
        let mirror = read_copy(&mut file, MIRROR_OFFSET);
        let Some(record) = mirror.as_ref().and_then(Self::from_bytes) else {
            return Ok(None);
        };
        // Repair the primary; a failure is not the caller's problem.
        let _ = write_copy(&mut file, PRIMARY_OFFSET, &record.to_bytes());
        Ok(Some(record))
    }

    /// `amBackupRecordWriteDup`: the mirror first, then the primary.
    pub fn store(self, path: &Path) -> Result<(), RtcError> {
        let mut file = OpenOptions::new().read(true).write(true).open(path).map_err(|_| RtcError::Storage)?;
        let bytes = self.to_bytes();
        write_copy(&mut file, MIRROR_OFFSET, &bytes)?;
        write_copy(&mut file, PRIMARY_OFFSET, &bytes)
    }

    /// The start-up stamp of a local network-delivery install (`platform.md` 5.7): loads the record (the default
    /// when both copies fail their CRC) and writes it again with `sync_stamp = utc_time_t`, only when that
    /// differs. The OS clock is not touched.
    pub fn stamp_local_install(path: &Path, utc_time_t: i64) -> Result<(), RtcError> {
        let record = Self::load(path)?.unwrap_or_default();
        if record.sync_stamp == utc_time_t {
            return Ok(());
        }
        Self { sync_stamp: utc_time_t, ..record }.store(path)
    }
}

fn read_copy(file: &mut std::fs::File, offset: u64) -> Option<[u8; RECORD_SIZE]> {
    let mut bytes = [0u8; RECORD_SIZE];
    file.seek(SeekFrom::Start(offset)).ok()?;
    file.read_exact(&mut bytes).ok()?;
    Some(bytes)
}

fn write_copy(file: &mut std::fs::File, offset: u64, bytes: &[u8; RECORD_SIZE]) -> Result<(), RtcError> {
    file.seek(SeekFrom::Start(offset)).map_err(|_| RtcError::Storage)?;
    file.write_all(bytes).map_err(|_| RtcError::Storage)
}

/// CRC-32 (IEEE, reflected polynomial `0xEDB88320`, initial value and final xor `0xFFFFFFFF`).
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// `amRtcGetTimeZone`: the daylight-saving rules and the two biases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeZone {
    /// Start of daylight saving.
    pub dst_start: DstRule,
    /// End of daylight saving.
    pub dst_end: DstRule,
    /// Location bias in minutes.
    pub location_bias: i32,
    /// Server bias in minutes.
    pub server_bias: i32,
}

/// The `amRtc` module state (`S`, 2.3).
#[derive(Debug, Default)]
pub struct Rtc {
    initialised: bool,
    path: Option<PathBuf>,
    record: TimeZoneRecord,
    /// `contOffset`: seconds added to the OS clock for the continuous time.
    continuous_offset: i64,
    /// Bias of the operating-system time zone, minutes (`UTC = local + bias`).
    os_bias: i32,
}

impl Rtc {
    /// An RTC that is not initialised.
    pub fn new() -> Self {
        Self::default()
    }

    /// `amRtcInit` (`0x1402D9D30`) over the system file of the NVRAM context.
    pub fn init(&mut self, nvram: &NvramContext) -> Result<(), RtcError> {
        if self.initialised {
            return Err(RtcError::AlreadyInitialised);
        }
        let path = nvram.system_file().ok_or(RtcError::NoSystemFile)?;
        self.init_file(path)
    }

    /// `amRtcInit` over the system file at `path`.
    pub fn init_file(&mut self, path: &Path) -> Result<(), RtcError> {
        if self.initialised {
            return Err(RtcError::AlreadyInitialised);
        }
        self.record = TimeZoneRecord::load(path)?.unwrap_or_default();
        self.path = Some(path.to_owned());
        self.continuous_offset = 0;
        self.os_bias = self.record.location_bias;
        self.initialised = true;
        Ok(())
    }

    /// `amRtcFinalize` (`0x1402D9890`).
    pub fn exit(&mut self) -> Result<(), RtcError> {
        if !self.initialised {
            return Err(RtcError::NotInitialised);
        }
        *self = Self::new();
        Ok(())
    }

    /// Whether `init` succeeded.
    pub fn is_initialised(&self) -> bool {
        self.initialised
    }

    fn check(&self) -> Result<(), RtcError> {
        if self.initialised { Ok(()) } else { Err(RtcError::NotInitialised) }
    }

    /// The UTC time as seconds since 1970.
    fn utc_now(&self, clock: &dyn Clock) -> Result<i64, RtcError> {
        let local = (clock.local_now().0 - epoch_micros()).div_euclid(DateTime::MICROS_PER_SECOND);
        Ok(local + 60 * i64::from(self.os_bias))
    }

    /// `amRtcGetServerTime` (`0x1402D9A90`): `UTC - 60 * serverBias` and whether the clock was synchronised.
    pub fn server_time(&self, clock: &dyn Clock) -> Result<(RtcTime, bool), RtcError> {
        self.check()?;
        let utc = self.utc_now(clock)?;
        let time = RtcTime::from_time_t(utc - 60 * i64::from(self.record.server_bias))?;
        Ok((time, self.is_server_synchronised(clock)))
    }

    /// `amRtcIsServerSync` (`0x1402D9EA0`): a stamp exists and the clock has not been set back below it.
    pub fn is_server_synchronised(&self, clock: &dyn Clock) -> bool {
        let stamp = self.record.sync_stamp;
        stamp != 0 && self.utc_now(clock).is_ok_and(|utc| utc >= stamp)
    }

    /// `amRtcGetContinuousTime` (`0x1402D9930`): the UTC time plus the continuous offset.
    pub fn continuous_time(&self, clock: &dyn Clock) -> Result<RtcTime, RtcError> {
        self.check()?;
        RtcTime::from_time_t(self.utc_now(clock)? + self.continuous_offset)
    }

    /// `amRtcGetTimeZone` (`0x1402D9C90`).
    pub fn time_zone(&self) -> Result<TimeZone, RtcError> {
        self.check()?;
        Ok(TimeZone {
            dst_start: self.record.dst_start,
            dst_end: self.record.dst_end,
            location_bias: self.record.location_bias,
            server_bias: self.record.server_bias,
        })
    }

    /// The record as read at `init` or last written.
    pub fn record(&self) -> &TimeZoneRecord {
        &self.record
    }

    /// `amRtcSetServerTime(0, server_time, location_bias, server_bias)` (`0x1402DAD10`, 5.2): sets the time zone
    /// and the system clock to the UTC instant `server_time + 60 * server_bias` and stamps the record.
    pub fn set_server_time(
        &mut self,
        clock: &dyn Clock,
        system: &mut dyn SystemActions,
        server_time: &RtcTime,
        location_bias: i32,
        server_bias: i32,
    ) -> Result<(), RtcError> {
        self.check()?;
        let seconds = server_time.to_time_t()?;
        let utc = RtcTime::from_time_t(seconds + 60 * i64::from(server_bias))?;
        let record = TimeZoneRecord { location_bias, server_bias, ..self.record };
        let mut saved_stamp = 0;
        let outcome = self.set_clock_and_zone(clock, system, &utc, &record, &mut saved_stamp);
        if outcome == Err(RtcError::Failed)
            && let Some(path) = &self.path
        {
            // `amRtcRestoreTimeZone`: the old record, stamped with what the transaction saved (the previous stamp
            // before the zone change succeeded, 0 after it, the new stamp once the clock was set). The OS zone and
            // clock are not restored; a failure to write is not reported.
            let _ = TimeZoneRecord { sync_stamp: saved_stamp, ..self.record }.store(path);
        }
        outcome
    }

    /// `amRtcSetContinuousClockAndTimeZone` and `UpdateContinuousClockAndTimeZone` (`0x1402DA800`, `0x1402DBE50`).
    /// `saved_stamp` is the original's `savedStamp`.
    fn set_clock_and_zone(
        &mut self,
        clock: &dyn Clock,
        system: &mut dyn SystemActions,
        utc: &RtcTime,
        record: &TimeZoneRecord,
        saved_stamp: &mut i64,
    ) -> Result<(), RtcError> {
        let path = self.path.clone().ok_or(RtcError::Storage)?;
        let zone = TimeZoneInformation {
            bias: record.location_bias,
            standard_bias: 0,
            daylight_bias: -60,
            standard_date: rule(record.dst_end),
            daylight_date: rule(record.dst_start),
        };
        // The "not synchronised" marker is persisted before the clock is touched.
        *saved_stamp = self.record.sync_stamp;
        if *saved_stamp != 0 {
            TimeZoneRecord { sync_stamp: 0, ..self.record }.store(&path)?;
        }
        system.set_time_zone(&zone).map_err(|_| RtcError::Failed)?;
        self.os_bias = record.location_bias;
        *saved_stamp = 0;
        // `amRtcSetTime`: the instant must not be older than 2010-01-01 UTC; the OS call is made twice.
        if utc.to_time_t()? < ORIGIN.to_time_t()? {
            return Err(RtcError::Failed);
        }
        let base = self.utc_now(clock)?;
        for _ in 0..2 {
            system.set_system_time(&utc.wall_time()).map_err(|_| RtcError::Failed)?;
        }
        // `UpdateTime`: the clock set moves the OS clock; the continuous offset absorbs the jump.
        let now = self.utc_now(clock)?;
        self.continuous_offset += base - now;
        *saved_stamp = now;
        let stamped = TimeZoneRecord { sync_stamp: now, ..*record };
        stamped.store(&path).map_err(|_| RtcError::Failed)?;
        self.record = stamped;
        Ok(())
    }
}

fn rule(bytes: DstRule) -> TransitionRule {
    TransitionRule {
        month: u16::from(bytes[0]),
        day: u16::from(bytes[1]),
        day_of_week: u16::from(bytes[2]),
        hour: u16::from(bytes[3]),
        minute: u16::from(bytes[4]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::SimClock;
    use backend::mock::SimSystem;

    fn sysfile(dir: &tempfile::TempDir) -> PathBuf {
        let path = dir.path().join("sysfile.dat");
        std::fs::write(&path, vec![0u8; 24_576]).expect("writes the system file");
        path
    }

    #[test]
    fn time_t_round_trips_and_checks_fields() {
        let time = RtcTime { year: 2024, month: 5, day: 14, hour: 7, minute: 0, second: 0 };
        let seconds = time.to_time_t().expect("valid");
        assert_eq!(seconds, 1_715_670_000);
        assert_eq!(RtcTime::from_time_t(seconds), Ok(time));
        assert_eq!(RtcTime { month: 13, ..time }.to_time_t(), Err(RtcError::InvalidArgument));
        assert_eq!(RtcTime { month: 0, ..time }.to_time_t(), Err(RtcError::Failed));
        assert_eq!(RtcTime { day: 30, month: 2, ..time }.to_time_t(), Err(RtcError::Failed));
        assert_eq!(RtcTime { year: 1600, ..time }.to_time_t(), Err(RtcError::Failed));
        assert_eq!(RtcTime { second: 60, ..time }.to_time_t(), Ok(seconds + 59));
    }

    #[test]
    fn an_unwritten_file_gives_the_default_record_and_writes_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = sysfile(&dir);
        let mut rtc = Rtc::new();
        rtc.init_file(&path).expect("init");
        assert_eq!(*rtc.record(), TimeZoneRecord::default());
        assert!(std::fs::read(&path).expect("reads").iter().all(|&byte| byte == 0));
        assert_eq!(rtc.init_file(&path), Err(RtcError::AlreadyInitialised));
    }

    #[test]
    fn a_damaged_primary_is_repaired_from_the_mirror() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = sysfile(&dir);
        let record = TimeZoneRecord { location_bias: -480, sync_stamp: 77, ..TimeZoneRecord::default() };
        record.store(&path).expect("stores");
        let mut image = std::fs::read(&path).expect("reads");
        image[PRIMARY_OFFSET as usize + 0x20] ^= 0xFF;
        std::fs::write(&path, image).expect("damages the primary");
        assert_eq!(TimeZoneRecord::load(&path), Ok(Some(record)));
        let repaired = std::fs::read(&path).expect("reads");
        assert_eq!(
            repaired[PRIMARY_OFFSET as usize..][..RECORD_SIZE],
            repaired[MIRROR_OFFSET as usize..][..RECORD_SIZE]
        );
    }

    #[test]
    fn set_server_time_moves_the_zone_and_clock_and_stamps_the_record() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = sysfile(&dir);
        let clock = SimClock::new();
        let mut system = SimSystem::new();
        let mut rtc = Rtc::new();
        assert_eq!(rtc.server_time(&clock), Err(RtcError::NotInitialised));
        rtc.init_file(&path).expect("init");
        assert!(!rtc.server_time(&clock).expect("reads").1);

        // AuthB: UTC 2024-05-14 07:00:00, `+0900`; the server notation is UTC + 9 h.
        let server = RtcTime { year: 2024, month: 5, day: 14, hour: 16, minute: 0, second: 0 };
        rtc.set_server_time(&clock, &mut system, &server, -540, -540).expect("sets");

        let state = system.state.borrow();
        assert_eq!(state.time_zones.len(), 1);
        assert_eq!(state.time_zones[0].bias, -540);
        assert_eq!(state.time_zones[0].daylight_bias, -60);
        let utc = WallTime { year: 2024, month: 5, day: 14, hour: 7, minute: 0, second: 0 };
        assert_eq!(state.system_times, vec![utc, utc]);
        drop(state);

        assert!(rtc.is_server_synchronised(&clock));
        let stored = TimeZoneRecord::load(&path).expect("loads").expect("a record");
        assert_eq!(stored, *rtc.record());
        assert_ne!(stored.sync_stamp, 0);
    }

    #[test]
    fn a_failing_zone_change_keeps_the_old_stamp_on_disk() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = sysfile(&dir);
        let clock = SimClock::new();
        let mut system = SimSystem::new();
        let stamped = TimeZoneRecord { sync_stamp: 1_700_000_000, ..TimeZoneRecord::default() };
        stamped.store(&path).expect("stores");
        let mut rtc = Rtc::new();
        rtc.init_file(&path).expect("init");
        system.state.borrow_mut().fail = true;
        let server = RtcTime { year: 2024, month: 5, day: 14, hour: 16, minute: 0, second: 0 };
        assert_eq!(rtc.set_server_time(&clock, &mut system, &server, -540, -540), Err(RtcError::Failed));
        assert_eq!(TimeZoneRecord::load(&path), Ok(Some(stamped)));
        assert_eq!(*rtc.record(), stamped);
    }

    #[test]
    fn an_instant_before_2010_is_refused() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = sysfile(&dir);
        let clock = SimClock::new();
        let mut system = SimSystem::new();
        let mut rtc = Rtc::new();
        rtc.init_file(&path).expect("init");
        let server = RtcTime { year: 2009, month: 12, day: 31, hour: 23, minute: 59, second: 59 };
        assert_eq!(rtc.set_server_time(&clock, &mut system, &server, -540, -540), Err(RtcError::Failed));
        assert!(system.state.borrow().system_times.is_empty());
    }
}
