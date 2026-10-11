//! Platform: the start-up gate manager (`am::platform::Manager`, `spec/libam/core.md` part C), the keychip
//! (`platform.md` part B), the video descriptors (part C 2), the storage manager (part C 4), the hardware monitor
//! and the RTC (part D), the system information facades and the process / common hand-offs (part A).
//!
//! # Cross-module methods
//!
//! * [`Platform::initialize`] (C.3): the gate. `Ok` always; the outcome is the manager state and its error cell.
//!   [`Platform::bailed_out`] is true for error 901 at `initialize`: the main flow is then never started.
//! * [`Platform::update`] (`0x14024B8B0`, C.5): runs `setup` while the state is not 2/3; every frame.
//! * [`Platform::refresh`] (`0x14024B8D0`, C.4): 2/3 -> 1; part of the forced module restart.
//! * [`Platform::reboot`] (C.7): requests the reboot, waits 20 s on the clock and latches 4105.
//! * [`dongle::Dongle`], [`storage::Storage`], [`video::Video`], [`hwm::Hwm`]: one manager each, driven by
//!   [`crate::flow`] in the order of `core.md` M.3.

pub mod dongle;
pub mod hwm;
pub mod keychip;
mod port;
pub mod rtc;
mod serial;
pub mod storage;
pub mod video;

use crate::module::{Cx, ErrorCell, Lifecycle, StateWord};
use crate::nvram::NvramContext;
use crate::registry_keys::{MOUNT_KEY, OS_VERSION_KEY, STATIC_KEY};
use backend::{Backend, KeyMode, RegValue};
use serial::Serial;

pub use dongle::{Dongle, DongleInfo};
pub use port::{
    CommonHandle, CommonPort, LibamSerial, NextProcessCode, ProcessHandle, ProcessPort, SystemInfoHandle,
    SystemInfoPort, VideoResolution,
};

/// The platform error codes of C.8.
pub mod error_code {
    /// Missing or invalid platform id (initialize) or OS version (setup).
    pub const PLATFORM_ID: u16 = 901;
    /// NVRAM context failure or reboot requested.
    pub const REBOOT_OR_NVRAM: u16 = 4105;
    /// The system record load failed (A.6.3).
    pub const RECORD_LOAD: u16 = 84;
    /// The system record load failed with the sub-storage flag set.
    pub const RECORD_SUB_STORAGE: u16 = 87;
}

/// Wait after the reboot request (C.7).
const REBOOT_WAIT_MS: u64 = 20_000;

/// Length of the computer name in bytes (`GetComputerNameA` with a 16-byte buffer, A.6.2).
const BOARD_NAME_LEN: usize = 15;

/// The 4-character platform id (`platform+324`): three characters and a numeric value of the fourth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformId {
    /// The first three characters.
    pub name: String,
    /// The fourth character: digit value for a digit, else its character code.
    pub number: u32,
}

/// `am::platform::Manager`.
#[derive(Debug, Default)]
pub struct Platform {
    life: Lifecycle,
    /// `+8`: the context handle is acquired.
    handle: bool,
    /// `+320`: `(a << 16) | (b << 8) | c` of `OSVersion`.
    version: u32,
    id: Option<PlatformId>,
    /// `+500`: the AMFS drive letter is not `d`.
    sub_storage: bool,
    /// The NVRAM system record holds a handle on the NVRAM context.
    record_acquired: bool,
    bailed_out: bool,
    /// The board information block (`+0x1B0`): valid only after a successful serial parse.
    board: Option<Serial>,
}

impl Platform {
    /// The constructed manager.
    pub fn new() -> Self {
        Self::default()
    }

    /// `initialize` (`0x14024B630`, C.3).
    pub fn initialize(&mut self, backend: &mut Backend, nvram: &mut NvramContext) {
        self.bailed_out = false;
        self.handle = true;
        // Step 2/3: the platform id.
        let id = Self::read_platform_id(backend);
        let Some(id) = id else {
            self.life.state = StateWord::FAILED;
            self.life.error.set(error_code::PLATFORM_ID, 0);
            self.bailed_out = true;
            return;
        };
        // Step 4: the sub-storage flag.
        self.sub_storage = Self::read_sub_storage(backend);
        // Step 5/6: the NVRAM system record.
        if !self.record_acquired {
            match nvram.acquire(backend) {
                Ok(()) => self.record_acquired = true,
                Err(_) => {
                    self.life.state = StateWord::FAILED;
                    self.life.error.set(error_code::REBOOT_OR_NVRAM, 0);
                    return;
                }
            }
        }
        self.id = Some(id);
        self.reset_state();
    }

    /// `0x14024B9C0`: state 1, error cleared, version 0.
    fn reset_state(&mut self) {
        self.life.state = StateWord::INITIALIZED;
        self.life.error.clear();
        self.version = 0;
        self.board = None;
    }

    /// `0x1402FAF90` + `0x1402CEE40`: the id is exactly four ASCII characters, the fourth graphic.
    fn read_platform_id(backend: &mut Backend) -> Option<PlatformId> {
        let RegValue::Text(text) = backend.registry.read(STATIC_KEY, KeyMode::CreateVolatile, "PlatformId").ok()?
        else {
            return None;
        };
        let units: Vec<u16> = text.encode_utf16().collect();
        if units.len() != 4 || !text.is_ascii() {
            return None;
        }
        let fourth = text.as_bytes()[3];
        if !fourth.is_ascii_graphic() {
            return None;
        }
        let number = if fourth.is_ascii_digit() { u32::from(fourth - b'0') } else { u32::from(fourth) };
        Some(PlatformId { name: text[..3].to_owned(), number })
    }

    /// `0x1402D0080`: the AMFS path has the form `<letter>:\...` and the letter is not `d`.
    fn read_sub_storage(backend: &mut Backend) -> bool {
        let Ok(RegValue::Text(path)) = backend.registry.read(MOUNT_KEY, KeyMode::CreateVolatile, "AMFS") else {
            return false;
        };
        let mut chars = path.chars();
        match (chars.next(), chars.next(), chars.next()) {
            (Some(letter), Some(':'), Some('\\')) => letter.is_ascii_alphabetic() && !letter.eq_ignore_ascii_case(&'d'),
            _ => false,
        }
    }

    /// `0x14024B8B0`, every frame: `setup` unless the state is 2 or 3 (C.4).
    pub fn update(&mut self, cx: &mut Cx<'_>) {
        if self.life.state.is_done() || self.life.state == StateWord::UNINITIALIZED {
            return;
        }
        self.setup(cx);
    }

    /// `setup` (`0x14024BA90`, C.5).
    fn setup(&mut self, cx: &mut Cx<'_>) {
        match cx.backend.registry.read(OS_VERSION_KEY, KeyMode::Open, "OSVersion") {
            Ok(RegValue::Text(text)) if !text.is_empty() => {
                self.version = parse_version(&text);
            }
            _ => self.life.error.set(error_code::PLATFORM_ID, 0),
        }
        self.load_board_record(cx);
        self.life.state = if self.life.error.is_empty() { StateWord::READY } else { StateWord::FAILED };
    }

    /// The last step of `setup` (`0x14024BBF0`, A.6.2): the computer name of the machine becomes the board serial.
    /// A name that is not exactly 15 characters fails the record load: error 84, or 87 with the sub-storage flag
    /// (A.6.3), latched only when the cell is still empty. A name that is no serial leaves the block invalid and
    /// raises nothing.
    fn load_board_record(&mut self, cx: &mut Cx<'_>) {
        match cx.backend.system.computer_name() {
            Some(name) if name.len() == BOARD_NAME_LEN => {
                // `name[0..4)` + `-` + `name[4..15)`; a non-ASCII name is no serial.
                self.board = name
                    .is_ascii()
                    .then(|| format!("{}-{}", &name[..4], &name[4..]))
                    .and_then(|image| Serial::from_text(&image));
            }
            _ => {
                if self.life.error.is_empty() {
                    let number =
                        if self.sub_storage { error_code::RECORD_SUB_STORAGE } else { error_code::RECORD_LOAD };
                    self.life.error.set(number, 0);
                }
            }
        }
    }

    /// `0x14024B8D0` (C.4): 2/3 -> 1, error cleared.
    pub fn refresh(&mut self) {
        if self.life.state.is_done() {
            self.reset_state();
        }
    }

    /// `0x14024B870`: releases the handle and the system record; state 0.
    pub fn terminate(&mut self, nvram: &mut NvramContext) {
        if self.record_acquired {
            nvram.release();
            self.record_acquired = false;
        }
        *self = Self::new();
    }

    /// `reboot` (`0x14024B8F0`, C.7): the reboot request, the 20 s wait, then error 4105 whatever happened.
    pub fn reboot(&mut self, cx: &mut Cx<'_>) {
        if cx.backend.system.reboot().is_err() {
            tracing::error!("reboot request failed");
        }
        cx.clock.sleep_ms(REBOOT_WAIT_MS);
        self.life.error.set(error_code::REBOOT_OR_NVRAM, 0);
    }

    /// Error 901 at `initialize`: the main flow must not start (C.1).
    pub fn bailed_out(&self) -> bool {
        self.bailed_out
    }

    /// State word (`ctx+25996`).
    pub fn state(&self) -> StateWord {
        self.life.state
    }

    /// `allReady` term and the Checking gate: state in {2, 3}.
    pub fn is_ready(&self) -> bool {
        self.life.state.is_done()
    }

    /// The manager's error cell (`ctx+25696`).
    pub fn error_cell(&self) -> ErrorCell {
        self.life.error
    }

    /// The board serial block (`+0x1B0`, A.6): `None` while it is invalid.
    pub fn board_serial(&self) -> Option<LibamSerial> {
        self.board.map(Serial::to_api)
    }

    /// The short board serial (`0x1402263D0`), the e-money serial.
    pub fn board_short_serial(&self) -> Option<String> {
        self.board.map(|serial| serial.short_text())
    }

    /// The platform id read at `initialize`.
    pub fn id(&self) -> Option<&PlatformId> {
        self.id.as_ref()
    }

    /// The OS version dword (`+320`).
    pub fn version(&self) -> u32 {
        self.version
    }

    /// The sub-storage flag (`+500`).
    pub fn sub_storage(&self) -> bool {
        self.sub_storage
    }
}

/// `sscanf("%u_%u_%u")`: all three numbers, else 0.
fn parse_version(text: &str) -> u32 {
    let mut parts = text.split('_').map(|part| part.parse::<u32>());
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(Ok(a)), Some(Ok(b)), Some(Ok(c)), None) => (a << 16) | ((b & 0xFF) << 8) | (c & 0xFF),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_triple_packs_like_the_original() {
        assert_eq!(parse_version("6_1_7601"), (6 << 16) | (1 << 8) | (7601 & 0xFF));
        assert_eq!(parse_version("6_1"), 0);
        assert_eq!(parse_version("six_1_2"), 0);
    }
}
