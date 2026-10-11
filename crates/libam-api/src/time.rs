//! `amdaemon::util::DateTime`, the daemon's wall-clock value, its calendar arithmetic and libam's `struct tm`.
//!
//! Hand-rolled: the recovered encoding (microseconds from 0001-01-01 in an unsigned count, an hour range of
//! `0..=47`, the original's day-number formulas and limits) is specific to this binary and differs from every
//! calendar crate's validation and range.

/// `amdaemon::util::DateTime`: signed microseconds since 0001-01-01 00:00:00 (local time).
///
/// The original counts in an unsigned 64-bit integer; [`Self::make`] stores that bit pattern, so values
/// above `i64::MAX` read back negative.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DateTime(pub i64);

/// Largest hour `DateTime::make` accepts (`0x2F`, sic: the original allows `0..=47`).
const MAX_HOUR: i32 = 0x2F;
/// Microseconds per minute.
const MICROS_PER_MINUTE: u64 = 60 * 1_000_000;
/// Microseconds per hour.
const MICROS_PER_HOUR: u64 = 60 * MICROS_PER_MINUTE;
/// Microseconds per day.
const MICROS_PER_DAY: u64 = 24 * MICROS_PER_HOUR;
/// Largest day number (days since 0001-01-01) `make` accepts, `0xCB9CFEE` = `u64::MAX / MICROS_PER_DAY`
/// (`0x14005F970` rejects anything above; `0x14005F600` feeds the same constant to `0x14005F7C0`).
const MAX_DAY_NUMBER: u64 = 0xCB9_CFEE;
/// Largest year `make` accepts: the year of day [`MAX_DAY_NUMBER`] (`0x14005F600` derives it with
/// `0x14005F7C0(213503982)`; 584555-01-19).
const MAX_YEAR: i32 = civil_from_days(MAX_DAY_NUMBER).0 as i32;

impl DateTime {
    /// Microseconds per second.
    pub const MICROS_PER_SECOND: i64 = 1_000_000;

    /// `DateTime::make(year, month, day, hour, minute, second, millisecond, microsecond)` for the inputs the
    /// non-throwing builder `0x14005F970` accepts; `None` where it fails (the throwing `make_2`
    /// `0x14005D580` then raises an exception, and every caller here guards with the validity test
    /// `0x14005D520`, which is the same builder).
    ///
    /// All arguments are C `int`s. Accepted ranges (`0x14005F600` for the date, `0x14005F6F0` for the time):
    /// year `1..=584555` (the year of day `0xCB9CFEE`), month `1..=12`, day `1..=days_in_month`
    /// (`0x140060C90`: proleptic Gregorian, year 0 and below are never leap), hour `0..=47` (sic), minute and
    /// second `0..=59`. `micros` is `1000 * millisecond + microsecond`; the original range checks both parts
    /// with `0..=999`, which is exactly `0..=999_999` combined.
    ///
    /// The value is `86_400_000_000 * days + time`, with `days` counted from 0001-01-01 by `0x140060B40`
    /// (January and February count as months 13 and 14 of the previous year). It is rejected above
    /// `0xCB9CFEE` days and when the sum overflows an unsigned 64-bit integer; the result is stored as that
    /// integer's signed reinterpretation (evidence `allnet.common.datetime.encoding`).
    pub fn make(year: i32, month: i32, day: i32, hour: i32, minute: i32, second: i32, micros: i32) -> Option<Self> {
        if !(1..=MAX_YEAR).contains(&year)
            || !(1..=12).contains(&month)
            || !(1..=days_in_month(year, month)).contains(&day)
            || !(0..=MAX_HOUR).contains(&hour)
            || !(0..=59).contains(&minute)
            || !(0..=59).contains(&second)
            || !(0..=999_999).contains(&micros)
        {
            return None;
        }
        let days = u64::try_from(day_number(year, month, day)).ok().filter(|&days| days <= MAX_DAY_NUMBER)?;
        let seconds = i64::from((hour * 60 + minute) * 60 + second);
        let time = (seconds * Self::MICROS_PER_SECOND + i64::from(micros)).cast_unsigned();
        (days * MICROS_PER_DAY).checked_add(time).map(|total| Self(total.cast_signed()))
    }

    /// `DateTime::make(struct tm const*)` (`0x14005D580`, guarded by the validity test `0x14005D520`) over the
    /// first six `struct tm` members, in memory order: `make(tm_year + 1900, tm_mon + 1, tm_mday, tm_hour,
    /// tm_min, tm_sec, 0, 0)` with the original's 32-bit wrapping additions. `None` where [`Self::make`] is.
    pub fn from_tm(sec: i32, min: i32, hour: i32, mday: i32, mon: i32, year: i32) -> Option<Self> {
        Self::make(year.wrapping_add(1900), mon.wrapping_add(1), mday, hour, min, sec, 0)
    }

    /// Splits the value into its civil fields: day number → date by `0x14005F7C0`, time of day by the
    /// division chain of the `struct tm` converter `0x1400159D0` and the formatter `0x14005EFE0`. The unsigned
    /// interpretation is used, so the hour is always below 24 (a `make` hour of 24..=47 carries into the day).
    pub fn civil(self) -> CivilTime {
        let micros = self.0.cast_unsigned();
        let (year, month, day) = civil_from_days(micros / MICROS_PER_DAY);
        CivilTime {
            year: year as u32,
            month: month as u32,
            day: day as u32,
            hour: ((micros % MICROS_PER_DAY) / MICROS_PER_HOUR) as u32,
            minute: ((micros % MICROS_PER_HOUR) / MICROS_PER_MINUTE) as u32,
            second: ((micros % MICROS_PER_MINUTE) / Self::MICROS_PER_SECOND.cast_unsigned()) as u32,
        }
    }
}

/// Calendar fields of a [`DateTime`] (see [`DateTime::civil`]); sub-second digits are dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CivilTime {
    /// Year, `1..=584555`.
    pub year: u32,
    /// Month, `1..=12`.
    pub month: u32,
    /// Day of the month, `1..=31`.
    pub day: u32,
    /// Hour, `0..=23`.
    pub hour: u32,
    /// Minute, `0..=59`.
    pub minute: u32,
    /// Second, `0..=59`.
    pub second: u32,
}

/// libam's C `struct tm` (MSVC, nine `int`s, 36 bytes): the broken-down time libam stores in its records and
/// takes by reference. The daemon only reads the first six members; the last three are carried as libam
/// delivered them, or `0` when the daemon builds the value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tm {
    /// `tm_sec`: seconds `0..=59`.
    pub sec: i32,
    /// `tm_min`: minutes `0..=59`.
    pub min: i32,
    /// `tm_hour`: hours since midnight.
    pub hour: i32,
    /// `tm_mday`: day of the month, 1-based.
    pub mday: i32,
    /// `tm_mon`: month, 0-based.
    pub mon: i32,
    /// `tm_year`: years since 1900.
    pub year: i32,
    /// `tm_wday`: days since Sunday.
    pub wday: i32,
    /// `tm_yday`: days since January 1st.
    pub yday: i32,
    /// `tm_isdst`: daylight saving time flag.
    pub isdst: i32,
}

impl Tm {
    /// The time validator (`0x14005D520`) followed by the `DateTime` builder (`0x14005D580`) over the first six
    /// members; `None` when the validator rejects the broken-down time.
    pub fn to_date_time(&self) -> Option<DateTime> {
        DateTime::from_tm(self.sec, self.min, self.hour, self.mday, self.mon, self.year)
    }
}

/// Days in `month` of `year` (`0x140060C90`).
fn days_in_month(year: i32, month: i32) -> i32 {
    if month == 2 {
        let leap = year > 0 && year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
        28 + i32::from(leap)
    } else {
        30 + i32::from((month <= 7) == (month & 1 == 1))
    }
}

/// Day number of a valid civil date, 0 for 0001-01-01 (`0x140060B40`).
fn day_number(year: i32, month: i32, day: i32) -> i64 {
    let (mut year, mut month) = (i64::from(year), i64::from(month));
    while month <= 2 {
        month += 12;
        year -= 1;
    }
    (year >> 2) - 429 + year / 400 + 306 * (month + 1) / 10 - year / 100 + i64::from(day) + 365 * year
}

/// `0x14005F7C0`: day number (0 = 0001-01-01) → `(year, month 1..=12, day 1..=31)`.
const fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let shifted = days + 306;
    let mut rest = shifted % 0x23AB1;
    let centuries = rest / 0x8EAC;
    rest %= 0x8EAC;
    let quad_years = rest / 0x5B5;
    rest %= 0x5B5;
    let leap_correction = (centuries >> 2) + rest / 0x5B4;
    let year_in_cycle = rest / 0x16D;
    let mut year =
        (year_in_cycle + 4 * (quad_years + 25 * (centuries + 4 * (shifted / 0x23AB1)))).wrapping_sub(leap_correction);
    let mut day_of_year = 365u64.wrapping_mul(leap_correction.wrapping_sub(year_in_cycle)).wrapping_add(rest);
    let five_months = day_of_year / 0x99;
    day_of_year %= 0x99;
    let two_months = day_of_year / 0x3D;
    let day_in_pair = day_of_year % 0x3D;
    let mut month = day_in_pair / 0x1F + 5 * five_months + 2 * two_months + 3;
    if month > 12 {
        year += 1;
        month -= 12;
    }
    (year, month, day_in_pair % 0x1F + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PER_DAY: i64 = 86_400_000_000;

    #[test]
    fn make_counts_microseconds_from_year_one() {
        assert_eq!(DateTime::make(1, 1, 1, 0, 0, 0, 0), Some(DateTime(0)));
        assert_eq!(DateTime::make(1970, 1, 1, 0, 0, 0, 0), Some(DateTime(719_162 * PER_DAY)));
        assert_eq!(DateTime::make(2000, 2, 29, 23, 59, 59, 0), Some(DateTime(730_178 * PER_DAY + 86_399_000_000)));
        let time_of_day = ((12 * 60 + 34) * 60 + 56) * 1_000_000 + 789_012;
        assert_eq!(DateTime::make(2024, 5, 20, 12, 34, 56, 789_012), Some(DateTime(739_025 * PER_DAY + time_of_day)));
    }

    #[test]
    fn civil_carries_hours_past_23_and_reads_the_unsigned_count() {
        let carried = DateTime::make(2024, 12, 31, 30, 1, 2, 3).expect("valid").civil();
        assert_eq!(
            carried,
            CivilTime { year: 2025, month: 1, day: 1, hour: 6, minute: 1, second: 2 },
            "hour 30 of 31 December is 06:00 of 1 January"
        );
        let last = DateTime(-1).civil();
        assert_eq!(last, CivilTime { year: 584_555, month: 1, day: 19, hour: 8, minute: 1, second: 49 });
    }
}
