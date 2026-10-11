//! The report schedule arithmetic (`allnet.md` part 9 "Schedule") and the first-AR delay (part 7).
//!
//! Times are the "local clock read as UTC" seconds of the original (`time_t` of the local fields).

use std::net::Ipv4Addr;

use libam_api::time::CivilTime;

use super::records::Stamp;

/// Days from 1970-01-01 of a proleptic Gregorian date.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let shifted_month = (month + 9) % 12;
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Seconds of the local fields read as UTC.
pub fn seconds_of(time: CivilTime) -> i64 {
    days_from_civil(i64::from(time.year), i64::from(time.month), i64::from(time.day)) * 86_400
        + i64::from(time.hour) * 3600
        + i64::from(time.minute) * 60
        + i64::from(time.second)
}

/// The calendar stamp of `seconds` (local fields read as UTC).
#[cfg(test)]
pub fn stamp_of(seconds: i64) -> Stamp {
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    Stamp {
        year: u16::try_from(year).unwrap_or(0),
        month: u8::try_from(month).unwrap_or(1),
        day: u8::try_from(day).unwrap_or(1),
        hour: u8::try_from(rest / 3600).unwrap_or(0),
        minute: u8::try_from(rest % 3600 / 60).unwrap_or(0),
        second: u8::try_from(rest % 60).unwrap_or(0),
    }
}

/// The `Stamp` of a civil time.
pub fn stamp_of_civil(time: CivilTime) -> Stamp {
    Stamp {
        year: u16::try_from(time.year).unwrap_or(0),
        month: u8::try_from(time.month).unwrap_or(0),
        day: u8::try_from(time.day).unwrap_or(0),
        hour: u8::try_from(time.hour).unwrap_or(0),
        minute: u8::try_from(time.minute).unwrap_or(0),
        second: u8::try_from(time.second).unwrap_or(0),
    }
}

/// `ARUptadeNextFGARTime` (`0x1403E5E40`): the next daily time. Today when the hour has not reached `start_hour`,
/// else tomorrow (minutes are not compared), at `start_hour:start_minute:00` plus `delay_s`. `None` for a month
/// outside 1..=12 (status -800).
pub fn next_fg_time(now: CivilTime, start_hour: u32, start_minute: u32, delay_s: u32) -> Option<i64> {
    if !(1..=12).contains(&now.month) {
        return None;
    }
    let today = days_from_civil(i64::from(now.year), i64::from(now.month), i64::from(now.day));
    let day = if now.hour >= start_hour { today + 1 } else { today };
    Some(day * 86_400 + i64::from(start_hour) * 3600 + i64::from(start_minute) * 60 + i64::from(delay_s))
}

/// `timezone=` of the query line: the UTC offset as signed `hhmm` from the bias in minutes (Windows sign).
pub fn timezone_number(bias_minutes: i32) -> i32 {
    -bias_minutes - 40 * (bias_minutes / 60)
}

/// The first-AR delay in milliseconds (`0x1403DDEC0`): bit-by-bit CRC-16 of the first 11 board-id bytes, shifted
/// before the test as the original does (`quirk.crc_bit`), and a 4-bit window selected by the day of the month.
pub fn first_ar_delay_ms(board_id: &str, day_of_month: u32) -> u32 {
    let mut crc: u16 = 1;
    for &byte in board_id.as_bytes().iter().take(11) {
        let mut value = u16::from(byte);
        for _ in 0..8 {
            value >>= 1;
            let flip = (crc ^ value) & 1;
            crc >>= 1;
            if flip != 0 {
                crc ^= 0x8408;
            }
        }
    }
    let crc = (!crc).rotate_left(8);
    let start = day_of_month & 0xF;
    let window: u32 = (0..4).map(|bit| u32::from((crc >> ((bit + start) & 15)) & 1) << bit).sum();
    1000 * 4 * window
}

/// The default `accounting-report.delay-time` (`0x1403E6400`) from the tenpo address and the game id.
pub fn default_delay_s(tenpo: Ipv4Addr, game_id: &str) -> u32 {
    let octets = tenpo.octets();
    let t = u32::from(u16::from(octets[1]) + (u16::from(octets[2]) << 8));
    let mut a = if t < 0xFD20 { t % 3600 } else { u32::from((5 * (t % 3600)) as u16) };
    if t >= 0xFD20 && a >= 3600 {
        a = u32::from((50 * (a % 3600)) as u16);
    }
    let chars = game_id.as_bytes();
    let at = |index: usize| i32::from(chars.get(index).copied().unwrap_or(0) as i8);
    let g = (((3 * (at(3) - 65) + 100 * (at(2) - 65)) as u16) as u32) % 3600;
    (a + g) % 3600
}

#[cfg(test)]
mod tests {
    use super::*;

    fn civil(day: u32, hour: u32) -> CivilTime {
        CivilTime { year: 2024, month: 5, day, hour, minute: 30, second: 0 }
    }

    #[test]
    fn stamp_and_seconds_round_trip() {
        let seconds = seconds_of(civil(14, 16));
        let stamp = stamp_of(seconds);
        assert_eq!((stamp.year, stamp.month, stamp.day, stamp.hour, stamp.minute), (2024, 5, 14, 16, 30));
    }

    #[test]
    fn next_daily_time_is_tomorrow_once_the_start_hour_is_reached() {
        let tomorrow = next_fg_time(civil(14, 7), 7, 0, 90).unwrap();
        assert_eq!(stamp_of(tomorrow), Stamp { year: 2024, month: 5, day: 15, hour: 7, minute: 1, second: 30 });
        let today = next_fg_time(civil(14, 6), 7, 0, 0).unwrap();
        assert_eq!(stamp_of(today).day, 14);
    }

    #[test]
    fn month_end_rolls_over() {
        let time = CivilTime { year: 2024, month: 2, day: 29, hour: 23, minute: 0, second: 0 };
        let next = stamp_of(next_fg_time(time, 7, 0, 0).unwrap());
        assert_eq!((next.month, next.day), (3, 1));
    }

    #[test]
    fn first_delay_is_a_multiple_of_four_seconds_up_to_a_minute() {
        let delay = first_ar_delay_ms("ABCDEFGHIJK", 14);
        assert!(delay.is_multiple_of(4000) && delay <= 60_000);
    }
}
