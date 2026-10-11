//! `am::apm`: application management.

use crate::backup::GameId;
use crate::result::{AmError, AmResult};

/// `amdaemon::Version` as sent by the client (`0x14021F550`, format 0): `byte 0` patch, `byte 1` minor and the
/// high `u16` major, each within `0..=99` (major `0..=9999`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AppVersion(u32);

impl AppVersion {
    /// `Version::fromU32`: out-of-range components are `am::Result` code 1.
    pub fn from_raw(raw: u32) -> AmResult<Self> {
        let (major, minor, patch) = version_parts(raw);
        if major > 9999 || minor > 99 || patch > 99 {
            return Err(AmError::code(AmError::INVALID_ARGUMENT));
        }
        Ok(Self(raw))
    }

    /// The wire value.
    pub const fn raw(self) -> u32 {
        self.0
    }
}

/// `(major, minor, patch)` of a raw `amdaemon::Version`.
pub const fn version_parts(raw: u32) -> (u32, u32, u32) {
    (raw >> 16, (raw >> 8) & 0xFF, raw & 0xFF)
}

/// libam surface used by the APM controller (`am::Manager` start/end app and the apm module).
///
/// Every `AmResult` mirrors an `am::Result` out-parameter: `Ok` is code 0, [`AmError::BUSY`] (3) keeps the
/// request pending, any other code fails it.
pub trait ApmPort {
    /// `amw::apmHandle` (`0x140219610`): `Err` when libam is not initialised ("amw is uninitialized",
    /// code 2). The controller reports the `isReady()` precondition failure on `Err`.
    fn apm_handle(&mut self) -> AmResult<()>;

    /// `amw::startApp` (`0x140215E80` → `am::Manager::startApp` `0x140253A30`).
    fn start_app(&mut self, game_id: &GameId, version: AppVersion, with_aime: bool) -> AmResult<()>;

    /// `amw::endApp` (`0x140215EA0` → `am::Manager::endApp` `0x140253BC0`); `test` is the client's test flag.
    fn end_app(&mut self, test: bool) -> AmResult<()>;

    /// `amw::apmState` (`0x140215EC0` → `am::apm::Manager::state` `0x140240260`): the raw `am::apm::State`.
    fn state(&mut self) -> u32;
}
