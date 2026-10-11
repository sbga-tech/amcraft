//! `am::allnet`: ALL.Net authentication, accounting, AiR and Wi-Fi.
//!
//! - Auth: [`AuthPort`], [`AuthLocation`] and the time value [`TimeSlot`] (shared with accounting).
//! - Accounting: [`AccountingPort`], [`AccountingInfo`], [`PlayCountInfo`] and [`PlayCountItem`].
//! - AiR: [`AirPort`] and the process-config part [`AirProcessConfig`].
//! - Wi-Fi: [`WiFiPort`] and [`WifiUnit`].

use crate::result::AmResult;
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

/// One play-count record of libam's history file (`{u32 year; u32 month; u64 count}`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlayCountItem {
    /// Calendar year.
    pub year: u32,
    /// Calendar month, 1-based.
    pub month: u32,
    /// Play count.
    pub count: u64,
}

/// libam `PlayCountInfo` (`AccountingInfo+0x08`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlayCountInfo {
    /// The history file was read; otherwise the items are skipped.
    pub valid: bool,
    /// Items 0, 1 and 2 = `current`, `last`, `beforeLast` on the wire (libam item 0 is the third record of
    /// the history file).
    pub items: [PlayCountItem; 3],
}

/// libam `AccountingInfo` (`0x90` bytes at `LM+0x11A0`, spec §8.4). libam rebuilds it, including a file
/// read, on every `accountingInfo` call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AccountingInfo {
    /// `+0x00` `alpbExGetNearFullEnable == 1`.
    pub near_full: bool,
    /// `+0x01` `alpbExGetLogFullStatus == 1`.
    pub log_full: bool,
    /// `+0x08` play-count history.
    pub play_count: PlayCountInfo,
    /// `+0x40` `alpbExLoadLastAccountingReportTime`.
    pub report_time: TimeSlot,
    /// `+0x68` `alpbExLoadLastBgAccountingReportTime`.
    pub bg_report_time: TimeSlot,
}

/// libam port of the Accounting controller (spec §3.6; evidence `allnet.accounting.port.table`).
///
/// The write methods are libam's facade wrappers: they do their own precondition checks (accounting
/// enabled, player is playing, slot valid) and report failures as `am::Result` codes 2 and 6, which the
/// controller only maps to "task failed".
pub trait AccountingPort {
    /// Accounting facade handle (`0x1402191B0`): `Err` (code 2) when the libam singleton does not exist.
    fn acquire_handle(&self) -> AmResult<()>;

    /// `0x140215C30`: raw accounting mode (`LM+0x1198`).
    fn accounting_mode(&self) -> u32;

    /// `0x140215C10`: accounting is enabled and the play-log module is ready.
    fn accounting_enabled(&self) -> bool;

    /// `0x140215CC0`: the rebuilt accounting info; `Ok(None)` or `Err` when libam has none.
    fn accounting_info(&self) -> AmResult<Option<AccountingInfo>>;

    /// `0x140215C50`: seconds until the next accounting report.
    fn seconds_until_report(&self) -> AmResult<u32>;

    /// `0x140215B80`: enables or disables near-full reporting.
    fn set_near_full(&mut self, enable: bool) -> AmResult<()>;

    /// `0x140215830`: starts a credit and returns its libam slot index.
    fn start_credit(&mut self, player: u32, kind_code: u32, status_code: u32) -> AmResult<u32>;

    /// `0x1402158E0`: ends the credit in `slot`.
    fn end_credit(&mut self, player: u32, kind_code: u32, status_code: u32, item_count: u8, slot: u32) -> AmResult<()>;

    /// `0x140215990`: accounts items.
    fn item_account(&mut self, player: u32, kind_code: u32, status_code: u32, item_count: u8) -> AmResult<()>;

    /// `0x140215A40`: puts a quantity.
    fn put_quantity(&mut self, player: u32, kind_code: u32, quantity: u16) -> AmResult<()>;

    /// `0x140215AE0`: puts a general ID, the request's `wchar_t[128]` without its terminator (libam converts it
    /// to CP932).
    fn put_general_id(&mut self, player: u32, kind_code: u32, general_id: &[u16]) -> AmResult<()>;
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

/// libam `WifiUnit` (`0x70` bytes at `LM+0x1A48`, stride `0x70`, spec §8.5) as the controller reads it. The
/// texts are libam's NUL-terminated (or array-filling) `wchar_t` arrays.
///
/// libam's getters for `serial` and `firm` never return null (they point into the unit), so the
/// controller's empty-string fallbacks for them cannot trigger and are not modelled.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WifiUnit {
    /// `+0x00` serial (`0x140227C40`); an empty serial marks an unpopulated unit.
    pub serial: [u16; 13],
    /// `+0x1A` libam's "auth good" flag: never read by amdaemon.
    pub auth_good: bool,
    /// `+0x1C` firmware version (`0x140227C60`).
    pub firm: [u16; 21],
    /// `+0x46` "server alive" flag (`0x140227C70`).
    pub server_alive: bool,
    /// `+0x48` access count (`0x140227C80`; libam stores a `u8`).
    pub access_count: u64,
    /// `+0x50` master serial; empty means none (`0x140227C90` returns null).
    pub master: [u16; 13],
}

/// libam port of the WiFi controller (spec §4.4; evidence `allnet.wifi.port.table`).
pub trait WiFiPort {
    /// WiFi facade handle (`0x140219250`): `Err` (code 2) when the libam singleton does not exist yet.
    fn acquire_handle(&self) -> AmResult<()>;

    /// `0x140215DA0`: `AuthConfig.enable && WifiConfig.enable`, a static configuration flag.
    fn available(&self) -> bool;

    /// `0x140215DC0`: number of units libam found.
    fn unit_count(&self) -> u64;

    /// `0x140215DE0`: a copy of unit `index`; `Err` (code 1) when `index >= unit_count()`.
    fn unit(&self, index: u64) -> AmResult<WifiUnit>;
}
