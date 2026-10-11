//! The Windows system actions, all raw `windows-sys`: `SeShutdownPrivilege` + `ExitWindowsEx(EWX_REBOOT |
//! EWX_FORCE, 0)` (`core.md` C.7; `system_shutdown::force_reboot` adds `EWX_FORCEIFHUNG` and a shutdown reason code
//! and leaks the token, so it is not the same call), `SetSystemTime` / `SetLocalTime` (each called twice, both must
//! succeed, `platform.md` 5.6), `SeTimeZonePrivilege` + `SetTimeZoneInformation` (no maintained crate sets the
//! clock or the zone), and `GetComputerNameA`: the NetBIOS name limited to 16 bytes, from which the board serial is
//! derived (`gethostname` / `hostname` return the DNS host name, a different string). The environment is
//! `std::env`.

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{HANDLE, LUID, SYSTEMTIME};
use windows_sys::Win32::Security::{
    AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED, SE_SHUTDOWN_NAME,
    SE_TIME_ZONE_NAME, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows_sys::Win32::System::Shutdown::{EWX_FORCE, EWX_REBOOT, ExitWindowsEx};
use windows_sys::Win32::System::SystemInformation::{SetLocalTime, SetSystemTime};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::System::Time::{SetTimeZoneInformation, TIME_ZONE_INFORMATION};
use windows_sys::Win32::System::WindowsProgramming::GetComputerNameA;

use crate::system::TransitionRule;
use crate::{SystemActions, SystemError, TimeZoneInformation, WallTime};

/// The calling thread's last Win32 error, read straight after a failed call.
fn last_error() -> u32 {
    std::io::Error::last_os_error().raw_os_error().map_or(0, i32::cast_unsigned)
}

/// A process token with one privilege looked up, so it can be enabled and disabled.
struct Privilege {
    token: OwnedHandle,
    luid: LUID,
}

impl Privilege {
    /// `OpenProcessToken(0x28)` + `LookupPrivilegeValueW` + `AdjustTokenPrivileges(enable)`. As in the original,
    /// only the return value of the adjust call is judged (`ERROR_NOT_ALL_ASSIGNED` still counts as success).
    fn enable(name: *const u16) -> Result<Self, SystemError> {
        let mut raw: HANDLE = null_mut();
        // SAFETY: `GetCurrentProcess` returns a pseudo handle; `raw` is a live, writable handle slot.
        let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut raw) };
        if opened == 0 {
            return Err(SystemError::Failed(last_error()));
        }
        // SAFETY: `OpenProcessToken` succeeded, so `raw` is an open token handle that nothing else owns.
        let token = unsafe { OwnedHandle::from_raw_handle(raw) };
        let mut luid = LUID::default();
        // SAFETY: `name` is one of the static NUL-terminated privilege names; `luid` is a live, writable local.
        if unsafe { LookupPrivilegeValueW(null(), name, &mut luid) } == 0 {
            return Err(SystemError::Failed(last_error()));
        }
        let privilege = Self { token, luid };
        privilege.set(true)?;
        Ok(privilege)
    }

    fn set(&self, enabled: bool) -> Result<(), SystemError> {
        let state = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: self.luid,
                Attributes: if enabled { SE_PRIVILEGE_ENABLED } else { 0 },
            }],
        };
        // SAFETY: the token is open with `TOKEN_ADJUST_PRIVILEGES`; `state` is a live, valid one-entry
        // `TOKEN_PRIVILEGES`; the previous-state outputs are null with a zero length.
        let adjusted =
            unsafe { AdjustTokenPrivileges(self.token.as_raw_handle(), 0, &state, 0, null_mut(), null_mut()) };
        if adjusted == 0 { Err(SystemError::Failed(last_error())) } else { Ok(()) }
    }
}

fn system_time(time: &WallTime) -> SYSTEMTIME {
    SYSTEMTIME {
        wYear: time.year,
        wMonth: time.month,
        wDayOfWeek: 0,
        wDay: time.day,
        wHour: time.hour,
        wMinute: time.minute,
        // A leap second (60) becomes 59 (`platform.md` 5.6).
        wSecond: time.second.min(59),
        wMilliseconds: 0,
    }
}

fn transition(rule: &TransitionRule) -> SYSTEMTIME {
    SYSTEMTIME {
        wYear: 0,
        wMonth: rule.month,
        wDayOfWeek: rule.day_of_week,
        wDay: rule.day,
        wHour: rule.hour,
        wMinute: rule.minute,
        wSecond: 0,
        wMilliseconds: 0,
    }
}

/// Reboot, clock, time zone and computer name of the machine.
#[derive(Debug, Clone, Copy, Default)]
pub struct WindowsSystem;

impl SystemActions for WindowsSystem {
    fn reboot(&mut self) -> Result<(), SystemError> {
        let _privilege = Privilege::enable(SE_SHUTDOWN_NAME)?;
        // SAFETY: `ExitWindowsEx` takes plain flags; the shutdown privilege is enabled on this process.
        if unsafe { ExitWindowsEx(EWX_REBOOT | EWX_FORCE, 0) } == 0 {
            return Err(SystemError::Failed(last_error()));
        }
        Ok(())
    }

    fn set_system_time(&mut self, time: &WallTime) -> Result<(), SystemError> {
        let st = system_time(time);
        // The original's worker calls `SetSystemTime` twice and both calls must succeed: the second one guards
        // against a daylight-saving bias change taking effect between the zone change and the clock set.
        for _ in 0..2 {
            // SAFETY: `st` is a live, fully initialised `SYSTEMTIME`.
            if unsafe { SetSystemTime(&st) } == 0 {
                return Err(SystemError::Failed(last_error()));
            }
        }
        Ok(())
    }

    fn set_local_time(&mut self, time: &WallTime) -> Result<(), SystemError> {
        let st = system_time(time);
        for _ in 0..2 {
            // SAFETY: `st` is a live, fully initialised `SYSTEMTIME`.
            if unsafe { SetLocalTime(&st) } == 0 {
                return Err(SystemError::Failed(last_error()));
            }
        }
        Ok(())
    }

    fn set_time_zone(&mut self, zone: &TimeZoneInformation) -> Result<(), SystemError> {
        let privilege = Privilege::enable(SE_TIME_ZONE_NAME)?;
        let info = TIME_ZONE_INFORMATION {
            Bias: zone.bias,
            StandardName: [0; 32],
            StandardDate: transition(&zone.standard_date),
            StandardBias: zone.standard_bias,
            DaylightName: [0; 32],
            DaylightDate: transition(&zone.daylight_date),
            DaylightBias: zone.daylight_bias,
        };
        // SAFETY: `info` is a live, fully initialised `TIME_ZONE_INFORMATION`; the time-zone privilege is enabled.
        let applied =
            if unsafe { SetTimeZoneInformation(&info) } == 0 { Err(SystemError::Failed(last_error())) } else { Ok(()) };
        // The original disables the privilege again; any failing step fails the call.
        applied.and(privilege.set(false))
    }

    fn computer_name(&mut self) -> Option<String> {
        // 16 bytes like the original: a name of 16 or more characters does not fit and fails the call.
        let mut buffer = [0_u8; 16];
        let mut size = 16_u32;
        // SAFETY: `buffer` holds `size` writable bytes and `size` is a live, writable `u32`.
        if unsafe { GetComputerNameA(buffer.as_mut_ptr(), &mut size) } == 0 {
            return None;
        }
        let name = buffer.get(..usize::try_from(size).ok()?)?;
        Some(String::from_utf8_lossy(name).into_owned())
    }

    fn env_var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}
