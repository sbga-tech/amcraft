//! [`Console`] on Win32: `AllocConsole`, the standard streams rebound to `CONOUT$`, and `FreeConsole`
//! (`spec/controllers/lifecycle.md` §3.14).
//!
//! This implements the daemon's own UI, not an external interface. The narrow Win32 wrapper remains
//! hand-written because no maintained crate safely wraps this allocation/rebinding lifecycle.
//! Safe Win32 wrappers such as `winsafe` still require unsafe window-class registration, and do not
//! cover safe console allocation and standard-stream rebinding.

use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
use windows_sys::Win32::System::Console::{
    AllocConsole, FreeConsole, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetConsoleTitleW, SetStdHandle,
};

use super::Console;

/// The process console. Dropping it closes the `CONOUT$` streams, as `Console::~Console` (`0x1400521F0`) does
/// whether or not this process allocated the console.
#[derive(Debug, Default)]
pub struct ProcessConsole {
    /// `CONOUT$` handles the standard streams were rebound to (stdout, stderr).
    handles: Vec<HANDLE>,
}

impl ProcessConsole {
    /// Opens `CONOUT$` and makes it the standard handle `which` (`freopen_s(&stream, "CONOUT$", "w", …)`).
    fn rebind_to_console(&mut self, which: u32) {
        let name = widestring::u16cstr!("CONOUT$");
        // SAFETY: `name` is a NUL-terminated UTF-16 buffer; a null security descriptor and template are allowed.
        // Console buffers require read+write sharing (an inherited console may already hold a read handle).
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                0,
                null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            // SAFETY: `handle` is a valid console output handle owned by this object.
            unsafe { SetStdHandle(which, handle) };
            self.handles.push(handle);
        }
    }

    /// `fclose(err); fclose(out)` of `Console::~Console`.
    fn close_streams(&mut self) {
        if self.handles.is_empty() {
            return;
        }
        for handle in self.handles.drain(..).rev() {
            // SAFETY: the handle was opened by `rebind_to_console` and is closed exactly once.
            unsafe { CloseHandle(handle) };
        }
        // SAFETY: detaching the standard streams from the handles that were just closed.
        unsafe {
            SetStdHandle(STD_ERROR_HANDLE, null_mut());
            SetStdHandle(STD_OUTPUT_HANDLE, null_mut());
        }
    }
}

impl Console for ProcessConsole {
    fn allocate(&mut self, title: &str) -> bool {
        // SAFETY: `AllocConsole` has no preconditions.
        let allocated = unsafe { AllocConsole() } != 0;
        self.rebind_to_console(STD_OUTPUT_HANDLE);
        self.rebind_to_console(STD_ERROR_HANDLE);
        let title = widestring::U16CString::from_str_truncate(title);
        // SAFETY: `title` is a NUL-terminated UTF-16 buffer that outlives the call.
        unsafe { SetConsoleTitleW(title.as_ptr()) };
        allocated
    }

    fn free(&mut self) {
        self.close_streams();
        // SAFETY: the standard streams no longer refer to the closed handles; detaching from the console this
        // object allocated.
        unsafe { FreeConsole() };
    }
}

impl Drop for ProcessConsole {
    fn drop(&mut self) {
        self.close_streams();
    }
}
