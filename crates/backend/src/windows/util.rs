//! Small helpers shared by the Win32 files.

use std::iter;
use std::os::windows::io::{HandleOrInvalid, HandleOrNull, OwnedHandle};

use windows_sys::Win32::Foundation::{GetLastError, HANDLE};

/// UTF-16 encoding of `text` with a terminating NUL.
pub(super) fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(iter::once(0)).collect()
}

/// `GetLastError()` of the calling thread.
pub(super) fn last_error() -> u32 {
    // SAFETY: `GetLastError` has no preconditions.
    unsafe { GetLastError() }
}

/// Takes ownership of a handle returned by a call that reports failure with NULL.
///
/// # Safety
///
/// `raw` is NULL or an open handle that nothing else owns or closes.
pub(super) unsafe fn own_or_null(raw: HANDLE) -> Option<OwnedHandle> {
    // SAFETY: guaranteed by the caller.
    OwnedHandle::try_from(unsafe { HandleOrNull::from_raw_handle(raw) }).ok()
}

/// Takes ownership of a handle returned by a call that reports failure with `INVALID_HANDLE_VALUE`.
///
/// # Safety
///
/// `raw` is `INVALID_HANDLE_VALUE` or an open handle that nothing else owns or closes.
pub(super) unsafe fn own_or_invalid(raw: HANDLE) -> Option<OwnedHandle> {
    // SAFETY: guaranteed by the caller.
    OwnedHandle::try_from(unsafe { HandleOrInvalid::from_raw_handle(raw) }).ok()
}
