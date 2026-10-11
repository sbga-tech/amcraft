//! The keychip (dongle) manager (`am::dongle::Manager`, `spec/libam/platform.md` part B 2-4 and 7).
//!
//! The manager drives the keychip C layer ([`Link`], over the backend's `KeychipDriver`) through the setup state
//! machine, publishes the validated values libam reads (`ModelType`, region, game id, keychip serial, develop
//! byte, the flag bytes the cabinet-type fix-ups use) and watches the keychip afterwards. It raises error 949
//! when the keychip is unusable and 4105 when the device cannot be opened.

use crate::nvram::GameId;
use crate::platform::LibamSerial;

use super::keychip::{KeychipError, Link, Properties, SetupPoll};
use super::serial::{Serial, is_valid_serial};
use crate::module::{Cx, ErrorCell, Lifecycle, StateWord};

/// Bit of the dongle flag byte (`+147`) that allows ALL.Net (`+108`, `core.md` 6).
pub const FLAG_ALLNET: u8 = 0x04;
/// Bit that allows accounting (`+109`).
pub const FLAG_ACCOUNTING: u8 = 0x20;
/// Bit that selects the title-router usage (`+110`).
pub const FLAG_TITLE_ROUTER: u8 = 0x10;

/// The keychip is unusable (setup failed, a value is invalid, or it was lost).
pub const ERROR_KEYCHIP: u16 = 949;
/// The keychip device could not be opened.
pub const ERROR_DEVICE: u16 = 4105;

/// Valid `modelType` range (`0x1402137F0`).
const MODEL_RANGE: std::ops::RangeInclusive<u8> = 1..=0x62;

/// What the keychip reports once the setup succeeded (the values `loadInfo` `0x14029ADF0` publishes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DongleInfo {
    /// `am::ModelType` (`ctx+26278`), `None` when the keychip reports none.
    pub model: Option<u8>,
    /// `am::Region` (`ctx+26280`): 1, 2 or 3; `None` for a raw value without a region (not an error).
    pub region: Option<u32>,
    /// The keychip game id (`ctx+26206`), valid only for four alphabetic characters.
    pub game_id: Option<GameId>,
    /// The keychip serial (`ctx+26218`) and its short form.
    pub serial: Option<LibamSerial>,
    /// The develop byte (`ctx+26204`).
    pub develop: bool,
    /// The dongle flag byte (`+147`).
    pub flags: u8,
    /// The raw `serverIpIpv4` dword (`+104`).
    pub network_address: u32,
}

/// `am::dongle::Manager`.
#[derive(Debug, Default)]
pub struct Dongle {
    life: Lifecycle,
    /// `+8`: this manager holds a reference on the keychip context.
    acquired: bool,
    link: Link,
    /// The manager's raw cache (`+0x70..`): kept across a restart.
    raw: Properties,
    info: DongleInfo,
    short_serial: Option<String>,
}

impl Dongle {
    /// The constructed manager (`0x14029A830`).
    pub fn new() -> Self {
        Self::default()
    }

    /// `start` (`0x14029A970`), from `CheckingCabinetTypeState::enter`: state 1.
    pub fn enter(&mut self) {
        self.life.state = StateWord::INITIALIZED;
    }

    /// `execute` (`0x14029A9D0`), every frame (4.1): nothing while an error is latched, the setup while the state
    /// is not 2 or 3, the health poll afterwards.
    pub fn update(&mut self, cx: &mut Cx<'_>) {
        if !self.life.error.is_empty() {
            return;
        }
        if self.life.state.is_done() {
            let now = cx.clock.tick_ms();
            // A failed poll shows as "not ready" below.
            let _ = self.link.update(&mut *cx.backend.keychip, now);
            if !self.link.is_ready() {
                self.life.error.set(ERROR_KEYCHIP, 0);
            }
            return;
        }
        self.setup(cx);
        if self.life.state == StateWord::FAILED {
            self.reset(true);
        }
    }

    fn fail(&mut self, number: u16) {
        self.life.error.set(number, 0);
        self.life.state = StateWord::FAILED;
    }

    /// `setup` (`0x14029AB50`, 4.1).
    fn setup(&mut self, cx: &mut Cx<'_>) {
        let now = cx.clock.tick_ms();
        let keychip = &mut *cx.backend.keychip;
        if !self.acquired {
            if self.link.init(keychip).is_err() {
                let _ = self.link.exit(keychip);
                tracing::warn!("keychip: the device could not be opened");
                self.fail(ERROR_DEVICE);
                return;
            }
            self.acquired = true;
        }
        if self.raw.game_id[0] == 0 {
            match self.link.setup_keychip(keychip, now) {
                Ok(SetupPoll::Done) => {}
                Ok(SetupPoll::InProgress) => return,
                Ok(SetupPoll::Failed) | Err(_) => {
                    tracing::info!("keychip: setup failed");
                    self.fail(ERROR_KEYCHIP);
                    return;
                }
            }
            if !self.load_info() {
                self.fail(ERROR_KEYCHIP);
                return;
            }
        }
        self.publish();
    }

    /// `loadInfo` (`0x14029ADF0`): the values of the C layer into the raw cache; a keychip id that is not a valid
    /// serial (a lower-case one included) fails the load.
    fn load_info(&mut self) -> bool {
        let Ok(properties) = self.link.properties() else {
            return false;
        };
        if !is_valid_serial(&properties.keychip_id) {
            return false;
        }
        self.raw = properties;
        if self.raw.system_flag & FLAG_ALLNET == 0 {
            self.raw.system_flag &= 7;
        }
        true
    }

    /// Steps 6 to 8 of the setup: validate the raw values, publish them, state 2.
    fn publish(&mut self) {
        let raw = self.raw;
        let game_id = game_id_of(&raw.game_id);
        let serial = serial_of(&raw.keychip_id);
        let model = MODEL_RANGE.contains(&raw.model).then_some(raw.model);
        if game_id.is_none() || serial.is_none() || model.is_none() {
            self.fail(ERROR_KEYCHIP);
            return;
        }
        let flags = raw.system_flag;
        self.info = DongleInfo {
            model,
            region: region_of(raw.region),
            game_id,
            serial,
            develop: raw.develop,
            flags,
            network_address: raw.server_ip,
        };
        self.short_serial = std::str::from_utf8(&raw.keychip_id.to_ascii_uppercase())
            .ok()
            .map(|standard| format!("{}{}", &standard[..4], &standard[5..12]));
        self.life.state = StateWord::READY;
    }

    /// `0x14029AA90`: clears the published values, with `full` also the raw cache.
    fn reset(&mut self, full: bool) {
        self.info = DongleInfo::default();
        self.short_serial = None;
        if full {
            self.raw = Properties::default();
        }
    }

    /// `restart` (`0x14029AA50`): published values reset (the raw cache kept), error cleared, state 1 (4.3). Run for
    /// every forced refresh and when the error class is 2 or 4.
    pub fn restart(&mut self) {
        self.reset(false);
        self.life.error.clear();
        self.life.state = StateWord::INITIALIZED;
    }

    /// `stop` (`0x14029A980`), from the main-flow terminate: releases the keychip context.
    pub fn terminate(&mut self, cx: &mut Cx<'_>) {
        if self.life.state == StateWord::UNINITIALIZED {
            return;
        }
        if self.acquired {
            let _ = self.link.exit(&mut *cx.backend.keychip);
            self.acquired = false;
        }
        self.reset(true);
        self.life.end();
    }

    /// State word (`ctx+26200`).
    pub fn state(&self) -> StateWord {
        self.life.state
    }

    /// `allReady` term and the "dongle done" input of other modules: state in {2, 3}.
    pub fn is_done(&self) -> bool {
        self.life.state.is_done()
    }

    /// The error cell (`ctx+26340`).
    pub fn error_cell(&self) -> ErrorCell {
        self.life.error
    }

    /// The published values.
    pub fn info(&self) -> &DongleInfo {
        &self.info
    }

    /// The 11-character short keychip serial (`ctx+0x666A`) while the manager has published one.
    pub fn keychip_short_serial(&self) -> Option<&str> {
        self.short_serial.as_deref()
    }

    /// The model optional.
    pub fn model(&self) -> Option<u8> {
        self.info.model
    }

    /// The region optional (1, 2, 3).
    pub fn region(&self) -> Option<u32> {
        self.info.region
    }

    /// The game id optional.
    pub fn game_id(&self) -> Option<GameId> {
        self.info.game_id
    }

    /// The keychip serial optional.
    pub fn serial(&self) -> Option<LibamSerial> {
        self.info.serial
    }

    /// The develop byte.
    pub fn develop(&self) -> bool {
        self.info.develop
    }

    /// Bytes `+108..+110` as the cabinet-type fix-ups read them (`core.md` 6): ALL.Net allowed, accounting
    /// allowed, title router usage.
    pub fn permissions(&self) -> DonglePermissions {
        DonglePermissions {
            allnet: self.info.flags & FLAG_ALLNET != 0,
            accounting: self.info.flags & FLAG_ACCOUNTING != 0,
            title_router: self.info.flags & FLAG_TITLE_ROUTER != 0,
        }
    }

    /// The keychip C layer, for the modules that talk to the device (billing, NVRAM, ALL.Net authentication). Pass
    /// `&mut *backend.keychip` as the device.
    pub fn link(&mut self) -> &mut Link {
        &mut self.link
    }

    /// `amDongleGetBillingMode`: the billing mode learnt at setup.
    pub fn billing_mode(&self) -> Result<bool, KeychipError> {
        self.link.billing_mode()
    }
}

/// The three dongle flag bytes of `core.md` 6 (meaning `[I]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DonglePermissions {
    /// `+108`: ALL.Net allowed; clears `auth.enable` and friends when 0 (F1).
    pub allnet: bool,
    /// `+109`: accounting allowed; clears `accounting.mode` when 0 (F2).
    pub accounting: bool,
    /// `+110`: `network.use_location_router := !title_router` (F4).
    pub title_router: bool,
}

/// Region map (`0x140284EE0`): raw 1 -> 1, 4 -> 3, 8 -> 2, everything else has no region.
fn region_of(raw: u8) -> Option<u32> {
    match raw {
        1 => Some(1),
        4 => Some(3),
        8 => Some(2),
        _ => None,
    }
}

/// The game-id setter (`0x140223150`): exactly four alphabetic characters, upper-cased.
fn game_id_of(raw: &[u8; 4]) -> Option<GameId> {
    if !raw.iter().all(u8::is_ascii_alphabetic) {
        return None;
    }
    let text = std::str::from_utf8(raw).ok()?.to_ascii_uppercase();
    crate::util::game_id(&text)
}

/// The serial setter (`0x140226290`): upper-cases, validates, derives the short form (`[0..4)` + `[5..12)`).
fn serial_of(bytes: &[u8]) -> Option<LibamSerial> {
    std::str::from_utf8(bytes).ok().and_then(Serial::from_text).map(Serial::to_api)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serial_pattern_and_short_form() {
        let serial = serial_of(b"a63e-01e12345678").expect("lower case is upper-cased by the setter");
        assert_eq!(crate::util::wide_to_string(&serial.standard), "A63E-01E12345678");
        assert_eq!(crate::util::wide_to_string(&serial.short), "A63E01E1234");
        // The validator itself does not upper-case: `I` and `O` are excluded, the tail is digits only, the fifth
        // character is the dash.
        assert!(!is_valid_serial(b"A63I-01E12345678"));
        assert!(!is_valid_serial(b"A63E-01E1234567O"));
        assert!(!is_valid_serial(b"A63E001E12345678"));
        assert!(!is_valid_serial(b"a63e-01e12345678"));
    }
}
