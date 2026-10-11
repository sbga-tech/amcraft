//! [`NamedMutex`] on Win32 mutexes.

use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::ptr;

use windows_sys::Win32::Foundation::FALSE;
use windows_sys::Win32::System::Threading::{
    CreateMutexW, INFINITE, MUTEX_ALL_ACCESS, OpenMutexW, ReleaseMutex, WaitForSingleObject,
};

use crate::mutex::NamedMutex;
use crate::windows::util::{own_or_null, wide};

/// A Win32 mutex handle (`0x140063BD0`); closed on drop.
#[derive(Debug)]
pub(super) struct WindowsMutex {
    handle: OwnedHandle,
}

impl WindowsMutex {
    /// `OpenMutexW(MUTEX_ALL_ACCESS, FALSE, name)` falling back to `CreateMutexW(NULL, FALSE, name)`; `None` for
    /// the name creates an anonymous mutex. The mutex is not initially owned.
    pub(super) fn open(name: Option<&str>) -> Option<Self> {
        let wide_name = name.map(wide);
        let name_ptr = wide_name.as_deref().map_or(ptr::null(), <[u16]>::as_ptr);
        let existing = if name_ptr.is_null() {
            None
        } else {
            // SAFETY: `name_ptr` points to the NUL-terminated `wide_name`, which outlives the call; the result is
            // NULL or a fresh handle.
            unsafe { own_or_null(OpenMutexW(MUTEX_ALL_ACCESS, FALSE, name_ptr)) }
        };
        let handle = match existing {
            Some(handle) => handle,
            // SAFETY: default security (NULL), not initially owned; `name_ptr` is NULL or as above; the result is
            // NULL or a fresh handle.
            None => unsafe { own_or_null(CreateMutexW(ptr::null(), FALSE, name_ptr)) }?,
        };
        Some(Self { handle })
    }

    /// `WaitForSingleObject`; succeeds on `WAIT_OBJECT_0` and `WAIT_ABANDONED` (`(ret & 0xFFFFFF7F) == 0`).
    fn acquire(&self, timeout_ms: u32) -> bool {
        // SAFETY: `self.handle` is an open mutex handle.
        let result = unsafe { WaitForSingleObject(self.handle.as_raw_handle(), timeout_ms) };
        result & 0xFFFF_FF7F == 0
    }
}

impl NamedMutex for WindowsMutex {
    fn lock(&mut self) -> bool {
        self.acquire(INFINITE)
    }

    fn try_lock(&mut self) -> bool {
        self.acquire(0)
    }

    fn unlock(&mut self) {
        // SAFETY: `self.handle` is an open mutex handle. A failure (not the owner) is ignored like the original.
        unsafe { ReleaseMutex(self.handle.as_raw_handle()) };
    }
}
