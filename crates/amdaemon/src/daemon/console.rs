//! The console window the daemon opens when `-w` or the keychip asks for it (`spec/controllers/lifecycle.md`
//! §3.14).
//!
//! This console belongs to the daemon itself, so it is an internal UI service, not an external backend.
//! [`Console`] is the App's seam; the process implementation is selected here.

use std::fmt::Debug;

#[cfg(windows)]
#[allow(unsafe_code)]
mod win32;
#[cfg(windows)]
pub use win32::ProcessConsole;

/// The process console.
pub trait Console: Debug {
    /// `Console::open` with a title (`0x140052250`): `AllocConsole`, rebind stdout/stderr to `CONOUT$`,
    /// `SetConsoleTitleW(title)`. Returns whether a console was allocated.
    fn allocate(&mut self, title: &str) -> bool;

    /// `FreeConsole` (`Console` destructor `0x1400521F0`, only when [`Self::allocate`] succeeded).
    fn free(&mut self);
}
