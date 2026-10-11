//! Small value helpers shared by every module: UTF-16 buffers and broken-down time.

use crate::time::{DateTime, Tm};

/// Copies `text` into a NUL-terminated UTF-16 array, truncating at `N - 1` units (the original's bounded copies).
pub fn wide_array<const N: usize>(text: &str) -> [u16; N] {
    let mut out = [0u16; N];
    for (slot, unit) in out.iter_mut().take(N.saturating_sub(1)).zip(text.encode_utf16()) {
        *slot = unit;
    }
    out
}

/// The text of a NUL-terminated (or array-filling) UTF-16 buffer.
pub fn wide_to_string(units: &[u16]) -> String {
    let end = units.iter().position(|&unit| unit == 0).unwrap_or(units.len());
    String::from_utf16_lossy(&units[..end])
}

/// The game id of four alphabetic characters, `None` for anything else.
pub fn game_id(text: &str) -> Option<crate::nvram::GameId> {
    let mut units = [0u16; 5];
    for (count, unit) in text.encode_utf16().enumerate() {
        *units.get_mut(count)? = unit;
    }
    crate::nvram::GameId::parse(&units)
}

/// `struct tm` of a local [`DateTime`] (`tm_wday`, `tm_yday` derived; `tm_isdst` 0).
pub fn tm_from_date_time(time: DateTime) -> Tm {
    let civil = time.civil();
    let year = civil.year as i32;
    let jan_first = DateTime::make(year, 1, 1, 0, 0, 0, 0).map_or(0, |start| start.0);
    let this_day = DateTime::make(year, civil.month as i32, civil.day as i32, 0, 0, 0, 0).map_or(0, |day| day.0);
    let micros_per_day = 86_400_000_000i64;
    let yday = ((this_day - jan_first) / micros_per_day) as i32;
    // 0001-01-01 is a Monday.
    let days = (this_day.cast_unsigned() / micros_per_day.cast_unsigned()) as i64;
    let wday = ((days + 1) % 7) as i32;
    Tm {
        sec: civil.second as i32,
        min: civil.minute as i32,
        hour: civil.hour as i32,
        mday: civil.day as i32,
        mon: civil.month as i32 - 1,
        year: year - 1900,
        wday,
        yday,
        isdst: 0,
    }
}
