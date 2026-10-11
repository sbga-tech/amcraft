//! Fitting source text into fixed C-string fields.
//!
//! A `zero-schema` string setter accepts a value only when it fits the field. The daemon publishes text the way
//! the original's release-CRT `wcsncpy_s`/`strncpy_s` calls do: the source is cut at its first NUL or after
//! `capacity - 1` units, whichever comes first, and one terminator follows. Units after the terminator keep
//! their previous values, which the setter already guarantees.
//!
//! The fitted copy lives on the stack ([`arrayvec`]); the largest daemon-written fields are `wchar_t[257]`
//! (Aime SEGA ID) and `char[1536]` (ALL.Net auth data).

use core::ffi::CStr;
use core::ops::Deref;

use arrayvec::ArrayVec;
use widestring::U16CStr;

/// Largest wide C-string field capacity (terminator included) the daemon writes.
pub const MAX_WIDE_FIELD: usize = 257;
/// Largest narrow C-string field capacity (terminator included) the daemon writes.
pub const MAX_NARROW_FIELD: usize = 1536;

/// A wide C string cut to fit a field; dereferences to [`U16CStr`] for `StringMut::set`.
#[derive(Debug, Clone)]
pub struct FittedWide(ArrayVec<u16, MAX_WIDE_FIELD>);

impl Deref for FittedWide {
    type Target = U16CStr;

    fn deref(&self) -> &U16CStr {
        U16CStr::from_slice_truncate(&self.0).expect("a fitted string ends with its terminator")
    }
}

/// A narrow C string cut to fit a field; dereferences to [`CStr`] for `StringMut::set`.
#[derive(Debug, Clone)]
pub struct FittedNarrow(ArrayVec<u8, MAX_NARROW_FIELD>);

impl Deref for FittedNarrow {
    type Target = CStr;

    fn deref(&self) -> &CStr {
        CStr::from_bytes_until_nul(&self.0).expect("a fitted string ends with its terminator")
    }
}

/// Cuts `units` to fit a wide C-string field of `capacity` units (terminator included).
///
/// # Panics
/// If `capacity` is zero or larger than [`MAX_WIDE_FIELD`].
pub fn fit_wide(units: &[u16], capacity: usize) -> FittedWide {
    fit_wide_units(units.iter().copied(), capacity)
}

/// Encodes `text` as UTF-16 and cuts it like [`fit_wide`]; the cut is by code unit, as in the original.
///
/// # Panics
/// If `capacity` is zero or larger than [`MAX_WIDE_FIELD`].
pub fn fit_wide_str(text: &str, capacity: usize) -> FittedWide {
    fit_wide_units(text.encode_utf16(), capacity)
}

fn fit_wide_units(units: impl Iterator<Item = u16>, capacity: usize) -> FittedWide {
    assert!((1..=MAX_WIDE_FIELD).contains(&capacity), "wide field capacity {capacity} out of range");
    let mut fitted: ArrayVec<u16, MAX_WIDE_FIELD> = units.take_while(|&unit| unit != 0).take(capacity - 1).collect();
    fitted.push(0);
    FittedWide(fitted)
}

/// Cuts `bytes` to fit a narrow C-string field of `capacity` bytes (terminator included).
///
/// # Panics
/// If `capacity` is zero or larger than [`MAX_NARROW_FIELD`].
pub fn fit_narrow(bytes: &[u8], capacity: usize) -> FittedNarrow {
    assert!((1..=MAX_NARROW_FIELD).contains(&capacity), "narrow field capacity {capacity} out of range");
    let mut fitted: ArrayVec<u8, MAX_NARROW_FIELD> =
        bytes.iter().copied().take_while(|&byte| byte != 0).take(capacity - 1).collect();
    fitted.push(0);
    FittedNarrow(fitted)
}
