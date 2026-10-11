//! `am::master`: the process-exit hand-off to the launcher (`spec/libam/core.md` parts A and B).
//!
//! The master context is the *last act of libam*: while libam runs it only collects three inputs (the daemon's
//! "next process" mode, the sequence test flag, the LAN-install download-UI flag) plus a copy of the current fatal
//! error; at shutdown [`Master::finalize`] turns them into the two registry values the launcher reads
//! (`NextProcess`, `SystemError`) through the embedded amMaster library ([`exit_request`], B.3).
//!
//! The original keeps one process-global record. libam here keeps one record per [`crate::core::Core`] (that is,
//! per initialised libam), which is the same lifetime: it is created at `initialize` and read at `finalize`.

use crate::registry_keys::MASTER_KEY;
use backend::{Backend, KeyMode, RegValue, Registry, RegistryError};

/// Next-process mode values stored by `ProcessPort::set_next_process` (A.6).
pub mod mode {
    /// Nothing requested.
    pub const NONE: u32 = 0;
    /// "Auto" (daemon wire 0 and every wire value at or above 4).
    pub const AUTO: u32 = 1;
    /// "SegaBoot".
    pub const SEGA_BOOT: u32 = 2;
    /// "SegaBootError": the fatal error is handed over as a system message.
    pub const SEGA_BOOT_ERROR: u32 = 3;
    /// "SystemTest".
    pub const SYSTEM_TEST: u32 = 4;
}

/// `amMasterExit` return codes (B.2).
pub mod exit_result {
    /// Invalid code, error number not found or invalid system message.
    pub const INVALID_PARAMETER: i32 = -2;
    /// The library is not initialised.
    pub const NOT_INITIALIZED: i32 = -3;
    /// The message has 200 or more code units while code 1 is requested.
    pub const MESSAGE_TOO_LONG: i32 = -10;
    /// A registry write failed.
    pub const FAILURE: i32 = -14;
}

/// Longest line the system-error encoder accepts, `\r\n` included (B.5).
const MAX_SYSTEM_ERROR_LINE: usize = 255;
/// Registry value holding the next process (REG_SZ).
const NEXT_PROCESS_VALUE: &str = "NextProcess";
/// Registry value holding the system error (REG_SZ).
const SYSTEM_ERROR_VALUE: &str = "SystemError";
/// The characters the item-list alphabet accepts besides letters and digits (`0x140702890`).
const ALLOWED_SYMBOLS: &str = "._-@'()*+,/<=>?\u{3002}\u{3001}\u{ff3f}\u{ff0d}\u{ff20}\u{3000}\u{ff08}\u{ff09}\u{2019}\u{ff0a}\u{ff0b}\u{ff0f}\u{ff1c}\u{ff1d}\u{ff1e}\u{ff1f}";

/// The copy of the error manager's current error the master context keeps (`Context+16`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MasterError {
    /// Error number, 0 for none.
    pub number: u16,
    /// Message text.
    pub text: String,
}

/// `am::master::Context` plus the handle protocol of the error manager (A.3-A.6).
#[derive(Debug, Default)]
pub struct Master {
    /// `Context+8` bit 0: `amMasterInit` succeeded.
    initialized: bool,
    /// The error manager's handle is attached (`error_mgr+11152`, the "kill guard"): every writer tests it.
    attached: bool,
    /// amMaster library state: `state[0]`.
    library_ready: bool,
    error: MasterError,
    /// `Context+316`.
    test_flag: bool,
    /// `Context+320`.
    mode: u32,
    /// `Context+324`.
    download_ui: bool,
}

impl Master {
    /// A detached master context (constructed by the context constructor).
    pub fn new() -> Self {
        Self::default()
    }

    /// The error manager's attach (`0x140232BF0`, A.5): `Context::initialize` (`amMasterInit`, which has no
    /// observable effect and cannot fail on a Rust host) then the attach itself. Returns whether the handle is
    /// attached; `false` makes the error manager's setup raise 4105.
    pub fn attach(&mut self) -> bool {
        if self.attached {
            return true;
        }
        self.library_ready = true;
        self.initialized = true;
        self.attached = true;
        self.test_flag = false;
        self.mode = mode::NONE;
        self.download_ui = false;
        true
    }

    /// Whether the error manager's handle is attached.
    pub fn is_attached(&self) -> bool {
        self.attached
    }

    /// `ProcessPort::set_next_process` (`0x140217700` -> `0x140280560`): stored iff attached, no validation.
    pub fn set_next_process(&mut self, code: u32) {
        if self.attached {
            self.mode = code;
        }
    }

    /// `0x140232F90`'s write of `Context+316`: stored iff attached.
    pub fn set_test_flag(&mut self, test: bool) {
        if self.attached {
            self.test_flag = test;
        }
    }

    /// `0x140233070`'s write of `Context+324` (LAN-install result): stored iff attached.
    pub fn set_download_ui(&mut self, download_ui: bool) {
        if self.attached {
            self.download_ui = download_ui;
        }
    }

    /// The copy of the current fatal error (`Context+16`): stored iff attached.
    pub fn copy_fatal_error(&mut self, number: u16, text: &str) {
        if self.attached {
            self.error = MasterError { number, text: text.to_owned() };
        }
    }

    /// `Context+320 == 3` while attached: a fatal error handled now raises NeedToExit(0) (A.9).
    pub fn wants_boot_error(&self) -> bool {
        self.attached && self.mode == mode::SEGA_BOOT_ERROR
    }

    /// `AppLoaderCount` (`amMasterGetProcessCount`, B.4.3): the launcher's REG_DWORD, 0 when unreadable.
    pub fn app_loader_count(registry: &mut dyn Registry) -> u32 {
        match registry.read(MASTER_KEY, KeyMode::CreateVolatile, "AppLoaderCount") {
            Ok(RegValue::Dword(count)) => count,
            _ => 0,
        }
    }

    /// The error manager's release (`0x140232E30` -> `Context::finalize`, A.7): maps the three inputs to an exit
    /// code and a system message, calls [`exit_request`] and retries once with "nothing" when the library
    /// rejects the message. Does nothing unless `initialize` succeeded; never fails.
    pub fn finalize(&mut self, backend: &mut Backend) {
        if !self.initialized {
            return;
        }
        let mut mode = self.mode;
        if mode == mode::AUTO {
            mode = if self.test_flag { mode::SYSTEM_TEST } else { mode::SEGA_BOOT };
        }
        let (code, text): (i32, Option<String>) = match mode {
            mode::SEGA_BOOT => (if self.download_ui { 5 } else { 1 }, Some(format!("ERROR {:04}\n", 0))),
            mode::SEGA_BOOT_ERROR => (1, Some(format!("ERROR {:04}\n{}", self.error.number, self.error.text))),
            mode::SYSTEM_TEST => (3, None),
            _ => (0, None),
        };
        let result = self.exit(backend, code, text.as_deref());
        if result == exit_result::INVALID_PARAMETER {
            self.exit(backend, 0, None);
        }
        self.initialized = false;
        self.attached = false;
    }

    /// `amMasterExit` wrapper (`0x1402C86D0`): -3 when the library is not initialised, else [`exit_request`];
    /// success shuts the library down.
    fn exit(&mut self, backend: &mut Backend, code: i32, text: Option<&str>) -> i32 {
        if !self.library_ready {
            return exit_result::NOT_INITIALIZED;
        }
        let result = exit_request(backend.registry.as_mut(), code, text);
        if result == 0 {
            self.library_ready = false;
        }
        result
    }
}

/// The path `amMasterExit` stores for `code` (B.3 table); `None` for codes it rejects.
fn next_process_path(code: i32) -> Option<&'static str> {
    Some(match code {
        0 => "",
        1 => r"s:\sgsegaboot.exe",
        2 => "@game",
        3 => r"s:\sgsegaboot.exe -s test",
        4 => r"x:\gametest",
        5 => r"c:\windows\sega\system\sgxdownloadui.exe",
        _ => return None,
    })
}

/// The error number of an exit message and the text after its first newline (B.3 step 5), `None` when the scan
/// finds no number: the first run of 4 digits (or of 5 digits) whose value fits `u16`.
fn parse_system_message(text: &str) -> Option<(u32, &str)> {
    let units: Vec<char> = text.chars().collect();
    let mut run = 0usize;
    let mut number = None;
    let mut index = 0usize;
    while index < units.len() {
        if units[index].is_ascii_digit() {
            run += 1;
        } else {
            run = 0;
        }
        let next_is_digit = units.get(index + 1).is_some_and(char::is_ascii_digit);
        if run == 5 || (run == 4 && !next_is_digit) {
            let digits: String = units[index + 1 - run..=index].iter().collect();
            let value: u32 = digits.parse().unwrap_or(u32::MAX);
            if value <= u32::from(u16::MAX) {
                number = Some(value);
                break;
            }
            run = 0;
        }
        index += 1;
        if units.get(index).is_none_or(|&c| c == '\n') {
            break;
        }
    }
    let number = number?;
    let message = text.split_once('\n').map_or("", |(_, rest)| rest);
    Some((number, message))
}

/// The escape of the message text (`0x1402E3C40`): space, LF, `@`, `=` and `?` become `@_`, `@n`, `@@`, `@-`, `@.`.
fn escape_message(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            ' ' => out.push_str("@_"),
            '\n' => out.push_str("@n"),
            '@' => out.push_str("@@"),
            '=' => out.push_str("@-"),
            '?' => out.push_str("@."),
            other => out.push(other),
        }
    }
    out
}

/// Whether every character is in the item-list alphabet (letters, digits and [`ALLOWED_SYMBOLS`]; non-ASCII
/// letters depend on the CRT locale in the original, `[U]`, and are rejected here).
fn is_item_text(text: &str) -> bool {
    text.chars().all(|c| c.is_ascii_alphanumeric() || ALLOWED_SYMBOLS.contains(c))
}

/// The encoded `SystemError` line (B.5) for a message record, or `None` when an item is rejected (-3 in the
/// original). Strings are accounted as the packed `name\0value\0` buffer: each addition needs
/// `used + len + 2 <= 255`.
fn encode_system_error(record: Option<(u32, &str)>) -> Option<String> {
    let mut items: Vec<(&str, String)> = Vec::new();
    match record {
        None => items.push(("system_error", "disable".to_owned())),
        Some((number, message)) => {
            if message.encode_utf16().count() >= 200 {
                return None;
            }
            items.push(("system_error", "enable".to_owned()));
            items.push(("error_number", number.to_string()));
            items.push(("message", escape_message(message)));
            items.push(("category", "error".to_owned()));
        }
    }
    let mut used = 0usize;
    for (name, value) in &items {
        if !is_item_text(name) || !is_item_text(value) {
            return None;
        }
        for string in [name.len(), value.len()] {
            if used + string + 2 > MAX_SYSTEM_ERROR_LINE {
                return None;
            }
            used += string + 1;
        }
    }
    let line = items.iter().map(|(name, value)| format!("{name}={value}")).collect::<Vec<_>>().join("&");
    Some(line + "\r\n")
}

/// `amMasterExitInternal` (`0x1402C8310`, B.3): selects the next-process path by `code`, writes `NextProcess`,
/// then writes `SystemError` from `text`. Returns 0, -2 (invalid code, no error number in the text, invalid
/// message), -10 (message of 200 or more units with code 1) or -14 (registry failure). On a failure after
/// `NextProcess` was written, `NextProcess` is reset to the empty string.
pub fn exit_request(registry: &mut dyn Registry, code: i32, text: Option<&str>) -> i32 {
    if code == 1 && text.is_some_and(|text| text.encode_utf16().count() >= 200) {
        return exit_result::MESSAGE_TOO_LONG;
    }
    let Some(path) = next_process_path(code) else {
        return exit_result::INVALID_PARAMETER;
    };
    if write_text(registry, NEXT_PROCESS_VALUE, path).is_err() {
        return exit_result::FAILURE;
    }
    let record = match text {
        None => None,
        Some(text) => match parse_system_message(text) {
            Some(parsed) => Some(parsed),
            None => return exit_result::INVALID_PARAMETER,
        },
    };
    let failure = match encode_system_error(record) {
        Some(line) => match write_text(registry, SYSTEM_ERROR_VALUE, &line) {
            Ok(()) => return 0,
            Err(_) => exit_result::FAILURE,
        },
        None => exit_result::INVALID_PARAMETER,
    };
    // Failure: the next process is reset to "" (the library stays initialised).
    let _ = write_text(registry, NEXT_PROCESS_VALUE, "");
    failure
}

fn write_text(registry: &mut dyn Registry, value: &str, text: &str) -> Result<(), RegistryError> {
    registry.write(MASTER_KEY, KeyMode::CreateVolatile, value, &RegValue::Text(text.to_owned()))
}

#[cfg(test)]
mod tests;
