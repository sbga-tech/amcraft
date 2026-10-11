//! The system-actions interface: reboot, clock, time zone, computer name and the process environment.

use crate::Unsupported;

/// `ERROR_NOT_SUPPORTED`: the operating system cannot do it.
const NOT_SUPPORTED: u32 = 50;

/// A broken-down UTC or local time for the clock setters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WallTime {
    /// Year.
    pub year: u16,
    /// Month `1..=12`.
    pub month: u16,
    /// Day `1..=31`.
    pub day: u16,
    /// Hour.
    pub hour: u16,
    /// Minute.
    pub minute: u16,
    /// Second.
    pub second: u16,
}

/// One daylight-saving transition rule of `TIME_ZONE_INFORMATION` (all zero: no daylight saving).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TransitionRule {
    /// Month.
    pub month: u16,
    /// Day (week number for day-of-week rules).
    pub day: u16,
    /// Day of week.
    pub day_of_week: u16,
    /// Hour.
    pub hour: u16,
    /// Minute.
    pub minute: u16,
}

/// `TIME_ZONE_INFORMATION` without names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TimeZoneInformation {
    /// Minutes west of UTC.
    pub bias: i32,
    /// Standard-time bias.
    pub standard_bias: i32,
    /// Daylight-time bias.
    pub daylight_bias: i32,
    /// Start of standard time.
    pub standard_date: TransitionRule,
    /// Start of daylight time.
    pub daylight_date: TransitionRule,
}

/// Why an operating-system action failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SystemError {
    /// A privilege could not be enabled or the API call failed, with the Win32 error code.
    #[error("system call failed with code {0}")]
    Failed(u32),
}

/// System-wide actions.
pub trait SystemActions {
    /// Enables `SeShutdownPrivilege` and calls `ExitWindowsEx(EWX_REBOOT | EWX_FORCE)` (`core.md` C.7). Returns
    /// when the call returned; the 20 s wait is the caller's.
    fn reboot(&mut self) -> Result<(), SystemError>;

    /// `SetSystemTime` (UTC), called twice by the original (`platform.md` 5.6).
    fn set_system_time(&mut self, time: &WallTime) -> Result<(), SystemError>;

    /// `SetLocalTime`.
    fn set_local_time(&mut self, time: &WallTime) -> Result<(), SystemError>;

    /// `SeTimeZonePrivilege` + `SetTimeZoneInformation`.
    fn set_time_zone(&mut self, zone: &TimeZoneInformation) -> Result<(), SystemError>;

    /// `GetComputerNameA`; `None` on failure.
    fn computer_name(&mut self) -> Option<String>;

    /// The value of the process environment variable `name`; `None` when it is unset or not valid Unicode.
    fn env_var(&self, name: &str) -> Option<String>;
}

/// Neither reboot, clock nor time-zone setting is available, and there is no NetBIOS computer name. The process
/// environment exists everywhere, so variables are read from it.
impl SystemActions for Unsupported {
    fn reboot(&mut self) -> Result<(), SystemError> {
        Err(SystemError::Failed(NOT_SUPPORTED))
    }

    fn set_system_time(&mut self, _time: &WallTime) -> Result<(), SystemError> {
        Err(SystemError::Failed(NOT_SUPPORTED))
    }

    fn set_local_time(&mut self, _time: &WallTime) -> Result<(), SystemError> {
        Err(SystemError::Failed(NOT_SUPPORTED))
    }

    fn set_time_zone(&mut self, _zone: &TimeZoneInformation) -> Result<(), SystemError> {
        Err(SystemError::Failed(NOT_SUPPORTED))
    }

    fn computer_name(&mut self) -> Option<String> {
        None
    }

    fn env_var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}
