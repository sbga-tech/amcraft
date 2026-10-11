//! The sub-application manager (`am::apm::Manager`, `spec/libam/storage.md` part 5): the sub-game id, version and
//! state record, plus the `changeApp` hand-over and the log-folder link that run around `startApp` / `endApp`.
//!
//! # Cross-module methods
//!
//! * [`Apm::update`] (`0x14023FE10`, M.3): `ready` is the conjunction of 5.2 (keychip, sequence, app image,
//!   storage, `AppInfo`, ALL.Net, Aime, ABaaS setup and e-money), computed by the flow.
//! * [`Apm::sub_game_id`]: the APM sub-game id the app root path selection reads (`0x140240200`): path B of the
//!   storage manager while it is valid ([`crate::platform::storage::Storage::set_apm_game_id`]).
//! * [`Apm::property`]: the sub-application property the app image catalogue publishes while the sub-game id is
//!   valid (storage.md 4.11, 5.1).
//! * [`Apm::enabled`], [`Apm::is_started`]: the module state the hand-over reads.
//!
//! The hand-over itself (`am::Manager::changeApp`, 5.4) lives in `changeapp.rs` as methods of
//! [`crate::core::Core`]; it calls these methods of the other modules, which this module only calls:
//!
//! | Module | Methods |
//! |---|---|
//! | Aime | `enabled`, `apm_prepare`, `apm_begin_change`, `apm_stopped`, `apm_finish_change`, `set_permit` |
//! | E-money | `is_enabled`, `cancel`, `apm_idle`, `apm_change` |
//! | AimePay | `is_enabled`, `cancel`, `apm_change` |
//! | ALL.Net | `auth_enabled`, `apm_change_ready`, `cancel_credit`, `apm_change` |
//! | Storage | `set_apm_game_id` |
//! | ABaaS | [`crate::abaas::Abaas::change_app`], `main_folder`, `sub_folder` |
//! | Dongle | `game_id`, `keychip_short_serial` |

mod changeapp;
mod link;
mod port;

use crate::appimage::{AppImageProperty, PropertyKind};
use crate::config::ApmConfig;
use crate::nvram::GameId;
use crate::time::Tm;

use crate::module::{ErrorCell, Lifecycle, StateWord};

pub use port::{ApmHandle, ApmPort, AppVersion, version_parts};

/// `am::apm::State` after a sub-application ran (`endApp`, storage.md 5.1).
const STATE_RUNNING: u32 = 1;
const STATE_ENDED: u32 = 2;
const STATE_ENDED_IN_TEST_TESTED: u32 = 3;
const STATE_ENDED_IN_TEST: u32 = 4;
const STATE_ENDED_TESTED: u32 = 5;

/// `am::apm::Manager` (`ctx+494400`).
#[derive(Debug, Default)]
pub struct Apm {
    life: Lifecycle,
    config: ApmConfig,
    sub_game_id: Option<GameId>,
    property: Option<AppImageProperty>,
    with_aime: bool,
    state: u32,
}

impl Apm {
    /// The constructed manager (`0x14023FC50`): everything zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// `OperatingState::enter` (`0x14023FCF0`): takes `AppConfig.apm`; the setup state is 1 when enabled and
    /// 2 otherwise, so a disabled APM never blocks the start-up (storage.md 5.2).
    pub fn initialize(&mut self, cx: &mut crate::module::Cx<'_>) {
        self.config = cx.config.apm;
        self.life.begin();
        if !self.config.enable {
            self.life.state = StateWord::READY;
        }
    }

    /// `0x14023FE10`, once per Operating frame: an enabled APM becomes ready (2) when `ready` holds.
    pub fn update(&mut self, ready: bool) {
        if self.config.enable && self.life.state != StateWord::READY && ready {
            self.life.state = StateWord::READY;
        }
    }

    /// `0x14023FD20`: everything cleared, state 0.
    pub fn terminate(&mut self, cx: &mut crate::module::Cx<'_>) {
        let _ = cx;
        if self.life.state != StateWord::UNINITIALIZED {
            *self = Self::default();
        }
    }

    /// `0x14023FE60`: a ready, enabled APM drops the sub-application silently and waits for readiness again.
    pub fn restart(&mut self) {
        if self.life.state == StateWord::READY && self.config.enable {
            self.clear_application();
            self.life.begin();
        }
    }

    fn clear_application(&mut self) {
        self.sub_game_id = None;
        self.property = None;
        self.with_aime = false;
        self.state = 0;
    }

    /// `apm.enable`.
    pub fn enabled(&self) -> bool {
        self.config.enable
    }

    /// The setup state is 2 (`changeApp` step 4).
    pub fn is_ready(&self) -> bool {
        self.life.state.is_done()
    }

    /// A sub-application is running (state 1).
    pub fn is_started(&self) -> bool {
        self.state == STATE_RUNNING
    }

    /// `am::apm::State` (`0x140240260`).
    pub fn state(&self) -> u32 {
        self.state
    }

    /// The APM sub-game id when valid (`0x140240200`).
    pub fn sub_game_id(&self) -> Option<GameId> {
        self.sub_game_id
    }

    /// The sub-application property (storage.md 5.1): an App property with a valid version, `tm_mday = 1` (which
    /// makes it valid) and everything else zero.
    pub fn property(&self) -> Option<AppImageProperty> {
        self.property
    }

    /// Whether `startApp` asked for Aime to stay in use.
    pub fn with_aime(&self) -> bool {
        self.with_aime
    }

    /// `APM.startApp` (`0x14023FED0`) without the folder link: stores the sub-application. `false` (Result 6)
    /// when the module is not ready or disabled.
    pub(crate) fn start(&mut self, game_id: GameId, version_raw: u32, with_aime: bool) -> bool {
        if !self.life.state.is_done() || !self.config.enable {
            return false;
        }
        self.sub_game_id = Some(game_id);
        self.property = Some(AppImageProperty {
            kind: PropertyKind::App,
            initialized: true,
            version: version_raw,
            option_id: [0; 5],
            creation: Tm { mday: 1, ..Tm::default() },
        });
        self.with_aime = with_aime;
        self.state = STATE_RUNNING;
        true
    }

    /// `APM.endApp` (`0x1402400A0`): `false` (Result 6) when the module is not ready or disabled. The state is
    /// `inTest ? (4 - (test != 0)) : (test ? 5 : 2)`.
    pub(crate) fn end(&mut self, in_test: bool, test: bool) -> bool {
        if !self.life.state.is_done() || !self.config.enable {
            return false;
        }
        self.clear_application();
        self.state = match (in_test, test) {
            (true, true) => STATE_ENDED_IN_TEST_TESTED,
            (true, false) => STATE_ENDED_IN_TEST,
            (false, true) => STATE_ENDED_TESTED,
            (false, false) => STATE_ENDED,
        };
        true
    }

    /// The error cell (`0x1402401F0`); no APM code ever sets it.
    pub fn error_cell(&self) -> ErrorCell {
        self.life.error
    }
}

#[cfg(test)]
mod tests;
