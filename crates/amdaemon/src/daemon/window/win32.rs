//! [`MessageWindow`] on Win32: the hidden window and the message queue of its thread
//! (`spec/controllers/lifecycle.md` §3.5, §3.7).
//!
//! This is the daemon's own UI and quit path. The narrow Win32 wrapper remains hand-written because no
//! maintained crate safely wraps this hidden class/window lifecycle;
//! [WinSafe's `RegisterClassEx`](https://docs.rs/winsafe/latest/aarch64-pc-windows-msvc/winsafe/fn.RegisterClassEx.html)
//! is unsafe too.

use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::HBRUSH;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExA, DefWindowProcA, DispatchMessageA, GetMessageA, IDC_ARROW,
    IDI_APPLICATION, LoadCursorA, LoadIconA, MSG, PM_NOREMOVE, PeekMessageA, PostQuitMessage, RegisterClassA,
    WM_DESTROY, WNDCLASSA, WS_OVERLAPPEDWINDOW,
};

use super::{MessagePump, MessageWindow};

/// `WndProc` (`0x140002BB0`): `WM_DESTROY` posts `WM_QUIT`, everything else is the default.
unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if message == WM_DESTROY {
        // SAFETY: called on the thread that owns the window's message queue.
        unsafe { PostQuitMessage(0) };
        0
    } else {
        // SAFETY: forwards the arguments the system passed to this window procedure unchanged.
        unsafe { DefWindowProcA(hwnd, message, wparam, lparam) }
    }
}

/// The daemon's window, registered with the executable's `HINSTANCE` (`PlatformHelper`).
#[derive(Debug)]
pub struct HiddenWindow {
    instance: HINSTANCE,
}

impl HiddenWindow {
    /// Takes the module handle of the executable; the window is created by [`MessageWindow::create_hidden`].
    pub fn new() -> Self {
        // SAFETY: a null module name returns the handle of the calling process's executable.
        let instance = unsafe { GetModuleHandleW(null()) };
        Self { instance }
    }
}

impl Default for HiddenWindow {
    fn default() -> Self {
        Self::new()
    }
}

impl MessageWindow for HiddenWindow {
    fn create_hidden(&mut self, class_name: &str, title: &str) -> bool {
        let class_name = format!("{class_name}\0");
        let title = format!("{title}\0");
        // SAFETY: IDI_APPLICATION/IDC_ARROW are predefined resource identifiers; a null instance selects the
        // system resources.
        let (icon, cursor) = unsafe {
            (LoadIconA(null_mut(), IDI_APPLICATION.cast::<u8>()), LoadCursorA(null_mut(), IDC_ARROW.cast::<u8>()))
        };
        let class = WNDCLASSA {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(window_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: self.instance,
            hIcon: icon,
            hCursor: cursor,
            hbrBackground: null_mut::<std::ffi::c_void>() as HBRUSH,
            lpszMenuName: null(),
            lpszClassName: class_name.as_ptr(),
        };
        // SAFETY: `class` is fully initialised and `class_name` is NUL terminated and outlives the call.
        if unsafe { RegisterClassA(&class) } == 0 {
            return false;
        }
        // SAFETY: the class is registered; both strings are NUL terminated and outlive the call. The window is
        // never shown (no WS_VISIBLE, no ShowWindow).
        let window = unsafe {
            CreateWindowExA(
                0,
                class_name.as_ptr(),
                title.as_ptr(),
                WS_OVERLAPPEDWINDOW,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                null_mut(),
                null_mut(),
                self.instance,
                null(),
            )
        };
        !window.is_null()
    }

    fn pump(&mut self) -> MessagePump {
        // SAFETY: MSG is plain data for which all-zero is a valid value.
        let mut message: MSG = unsafe { std::mem::zeroed() };
        // SAFETY: `message` is a valid, writable MSG; a null window handle selects the thread's messages.
        if unsafe { PeekMessageA(&mut message, null_mut(), 0, 0, PM_NOREMOVE) } == 0 {
            return MessagePump::Idle;
        }
        // SAFETY: as above. A result of 0 is WM_QUIT; -1 (error) is dispatched like the original does.
        if unsafe { GetMessageA(&mut message, null_mut(), 0, 0) } == 0 {
            return MessagePump::Quit;
        }
        // SAFETY: `message` was filled by `GetMessageA`.
        unsafe { DispatchMessageA(&message) };
        MessagePump::Dispatched
    }
}
