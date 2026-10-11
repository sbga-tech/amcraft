//! The ALL.Net authentication and AiR facades (`am::allnet`) and libam's handles.
//!
//! - Auth: [`AuthPort`], [`AuthLocation`] and the time value [`TimeSlot`] (shared with accounting); [`AuthHandle`].
//! - AiR: [`AirPort`] and the process-config part [`AirProcessConfig`]; [`AirHandle`].
//!
//! Accounting is [`billing`](super::billing), Wi-Fi is [`wifi`](super::wifi).

use crate::result::AmResult;
use crate::shared::Shared;
use crate::time::{DateTime, Tm};

/// libam `{u8 valid; struct tm tm}` (`AuthInfo.time`, `AccountingInfo` time slots).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TimeSlot {
    /// The slot holds a time.
    pub valid: bool,
    /// The time.
    pub tm: Tm,
}

impl TimeSlot {
    /// The slot's time as a `DateTime`: `None` unless the slot is valid *and* the validator accepts `tm`.
    pub fn to_datetime(&self) -> Option<DateTime> {
        if self.valid { self.tm.to_date_time() } else { None }
    }
}

/// libam `LocationInfo` (`0x560` bytes, spec §8.3) as `AuthController` reads it. `None` stands for a
/// getter that returned null or an error `Result` (`Unmatch libamw version.` guards). The texts are
/// libam's NUL-terminated (or array-filling) `wchar_t` arrays.
#[derive(Debug, Clone, Copy)]
pub struct AuthLocation {
    /// `LocationInfo.id` (`+0x000`, getter `0x140227330`); a location only exists when this is non-zero.
    pub id: u32,
    /// `LocationInfo.develop` (`+0x004`, `0x140227340`).
    pub develop: bool,
    /// `LocationInfo.name` (`+0x006`, `0x140227350`).
    pub name: Option<[u16; 61]>,
    /// `LocationInfo.nicknames[i]` (`+0x124`, `0x140227370`).
    pub nicknames: [Option<[u16; 41]>; 2],
    /// `RegionInfo.code` (`+0x1C8`, `0x140227570`).
    pub region_code: i32,
    /// `RegionInfo.names[i]` (`+0x1CC`, `0x140227580`).
    pub region_names: [Option<[u16; 49]>; 4],
    /// `LocationInfo.country` (`+0x354`, `0x1402276B0`).
    pub country: Option<[u16; 4]>,
    /// Game-server URI (`+0x35C`, `0x140227840`).
    pub uri: Option<[u16; 129]>,
    /// Game-server host (`+0x45E`, `0x140227850`).
    pub host: Option<[u16; 129]>,
}

/// libam port of the Auth controller (spec §2.7; evidence `allnet.auth.port.table`). Every method reads
/// the libam `am::allnet::Manager` singleton.
pub trait AuthPort {
    /// Auth facade handle (`0x140219110`): `Err` (code 2) when the libam singleton does not exist yet.
    fn acquire_handle(&self) -> AmResult<()>;

    /// `0x140215770`: libam's auth result is "good" (`LM+0x4B8 == 0`).
    fn is_good(&self) -> bool;

    /// `0x140215790`: detected line mask `2`/`4`/`8`; `0` when libam is not running (state not in `{2,3}`)
    /// or its module gate (`LM+0x10`) is off.
    fn line_mask(&self) -> u32;

    /// `0x1402157F0`: a copy of the location, `None` while libam has no place id yet.
    fn location(&self) -> Option<AuthLocation>;

    /// `0x1402157D0`: a copy of the last-auth time slot, `None` unless its valid byte is set.
    fn time(&self) -> Option<TimeSlot>;

    /// `0x1402157B0` (`0x14023D3A0`): the Auth 2.0 `auth_data` text, `AuthInfo.text` (`LM+1512+37`), `None` when
    /// empty. A `char[1536]`, NUL-terminated or filling the array: its setter `0x140226F80` rejects `0x600` or more
    /// characters and copies with `0x140228380(dst, 1536, src)`.
    fn text(&self) -> Option<[u8; 1536]>;
}

/// The part of `amdaemon::process::Config` the Air controller observes (`@0x14004FEEC`, lambda
/// `0x140050EF0`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AirProcessConfig {
    /// `allnet_air.enable` (`Config+0x4FF9E`): `false` by default, `true` when the `allnet_air` section
    /// exists without an `enable` key.
    pub enable: bool,
}

/// Port of the Air controller. The original makes no libam call at all (spec §5.4: no handle
/// acquisition, no `am::` interaction), so the port has no methods; it exists only to keep the uniform
/// `new(port)` constructor of the controller family.
pub trait AirPort {}

/// `AuthPort` handle over the shared libam state.
#[derive(Debug)]
pub struct AuthHandle(pub(crate) Shared);

/// `AirPort` handle.
#[derive(Debug)]
pub struct AirHandle;

impl AuthPort for AuthHandle {
    fn acquire_handle(&self) -> AmResult<()> {
        self.0.guard()
    }

    fn is_good(&self) -> bool {
        self.0.read(|core| core.allnet.is_good())
    }

    fn line_mask(&self) -> u32 {
        self.0.read(|core| if core.allnet.running() { core.allnet.auth().line_mask() } else { 0 })
    }

    fn location(&self) -> Option<AuthLocation> {
        self.0.read(|core| core.allnet.auth().location())
    }

    fn time(&self) -> Option<TimeSlot> {
        self.0.read(|core| core.allnet.auth().time())
    }

    fn text(&self) -> Option<[u8; 1536]> {
        self.0.read(|core| core.allnet.auth().text())
    }
}

impl AirPort for AirHandle {}
