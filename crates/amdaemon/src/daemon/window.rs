//! The hidden window of the main loop and its message pump (`spec/controllers/lifecycle.md` §3.5, §3.7): the
//! window exists so the system can end the daemon with `WM_DESTROY` / `WM_QUIT`.
//!
//! This is the daemon's own UI and quit path, not communication with an external party. [`MessageWindow`]
//! is the App's internal seam; the process implementation is selected here.

use std::fmt::Debug;

#[cfg(windows)]
#[allow(unsafe_code)]
mod win32;
#[cfg(windows)]
pub use win32::HiddenWindow;

/// Result of one message-pump step of the main loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessagePump {
    /// `PeekMessageA` found no message.
    Idle,
    /// A message was fetched with `GetMessageA` and dispatched.
    Dispatched,
    /// `GetMessageA` returned 0: `WM_QUIT`.
    Quit,
}

/// The daemon's hidden window and the message queue of its thread.
pub trait MessageWindow: Debug {
    /// `createHiddenWindow` (`0x140002940`): registers the window class `class_name` and creates, but never
    /// shows, a window titled `title`. `false` on failure. Its `WM_DESTROY` posts `WM_QUIT`.
    fn create_hidden(&mut self, class_name: &str, title: &str) -> bool;

    /// One message-pump step: `PeekMessageA(PM_NOREMOVE)`, and when a message is waiting `GetMessageA` followed
    /// by `DispatchMessageA` (a result of 0, `WM_QUIT`, is reported as [`MessagePump::Quit`]).
    fn pump(&mut self) -> MessagePump;
}
