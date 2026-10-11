//! ALL.Net blocks: Auth (`ServerData+0x4BE0`, 0xAD0), Accounting (`ServerData+0x0`, 0x58 and `ClientData+0xD00`,
//! 0x110), WiFi (`ServerData+0x47FC`, 0x3DC) and Air (`ServerData+0x47A4`, 0x58 and `ClientData+0x1A64`, 0x8).
//!
//! Layouts: `spec/controllers/allnet.md` §2.2, §3.2, §4.2 and §5.2; evidence ids `allnet.*.layout.*`. Strings are
//! published with `wcsncpy_s`/`strcpy_s` semantics through [`crate::schema::text`]; times are `DateTime`
//! microseconds (`crate::time::DateTime`) stored as `i64`.

use core::ffi::CStr;

use num_enum::TryFromPrimitive;
use widestring::U16CStr;
use zero_schema::zero;

use crate::schema::shared::Response;
use crate::schema::{client_block, server_block};

// ---------------------------------------------------------------------------------------------
// Auth: ServerData+0x4BE0, 0xAD0 bytes
// ---------------------------------------------------------------------------------------------

/// `ServerAuthData.lineType`: libam's detected line mask `2`/`4`/`8` mapped to `1`/`2`/`3`
/// (`AuthController::updateServerData`, evidence `allnet.auth.update.linetype`). The names follow the key order
/// of the `allnet_auth.support_line` validator and are inferred, not literals.
#[zero]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum AuthLineType {
    /// No line (libam mask `0`, or libam not running).
    NoLine = 0,
    /// libam mask `2` (`support_line.broadband`), inferred.
    Broadband = 1,
    /// libam mask `4` (`support_line.mobile`), inferred.
    Mobile = 2,
    /// libam mask `8` (`support_line.xdsl`), inferred.
    Xdsl = 3,
}

/// `ipc::allnet::ServerAuthData`.
///
/// Written only by `AuthController::updateServerData` (`0x1400513F0`); the block has no clear step, so everything
/// stays zero until the first successful publication.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerAuth<'b> {
    /// `LM.authResult == 0`; written on every update.
    pub good: bool,
    /// `LocationInfo.develop` (libam derives it from a negative `allnet_id`).
    pub develop: bool,
    /// Detected line type.
    pub line_type: AuthLineType,
    /// `LocationInfo.uri` (at most 128 characters).
    #[zero(capacity = 129)]
    pub game_server_uri: &'b U16CStr,
    /// `LocationInfo.host` (at most 128 characters).
    #[zero(capacity = 129)]
    pub game_server_host: &'b U16CStr,
    /// `LocationInfo.id` (libam stores a hexadecimal `place_id` of at most `0xFFFF`).
    pub location_id: u32,
    /// `LocationInfo.name` (at most 60 characters).
    #[zero(capacity = 61)]
    pub location_name: &'b U16CStr,
    /// `LocationInfo.nicknames[i]` (at most 40 characters each).
    #[zero(capacity = 41)]
    pub location_nicknames: [&'b U16CStr; 2],
    /// `RegionInfo.code`, copied as raw 32 bits.
    pub region_code: i32,
    /// `RegionInfo.names[i]` (at most 48 characters each).
    #[zero(capacity = 49)]
    pub region_names: [&'b U16CStr; 4],
    /// `LocationInfo.country` (at most 3 characters, ISO alpha-3).
    #[zero(capacity = 4)]
    pub country_code: &'b U16CStr,
    /// Auth 2.0 `auth_data` token string (ASCII, at most 1535 characters, empty when libam has none): the text
    /// libam re-presents in its ALL.Net authentication request (spec §2.6, inferred name).
    #[zero(capacity = 1536)]
    pub auth_text: &'b CStr,
    /// Local wall-clock `util::DateTime` microseconds libam handled the last successful auth response.
    pub last_auth_time: i64,
}

server_block!(allnet_auth, allnet_auth_mut: ServerAuth, ServerAuthRef, ServerAuthMut @ 0x4BE0);

// ---------------------------------------------------------------------------------------------
// Accounting: ServerData+0x0 (0x58) and ClientData+0xD00 (0x110)
// ---------------------------------------------------------------------------------------------

/// `ipc::allnet::ServerAccountingResponse` (`ServerData+0x0`, `0xC` bytes): the executor response plus the
/// accounting handle of the last begin/continue.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerAccountingResponse {
    /// Generation and `succeeded`, written only by `checkResult`.
    pub header: Response,
    /// Handle of the running accounting: `0` = none, otherwise libam slot index + 1.
    pub handle: u32,
}

/// `ipc::allnet::AccountingPlayCountItem`.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct AccountingPlayCountItem {
    /// `util::DateTime` microseconds of the first instant (`00:00:00`) of the item's `(year, month)`.
    pub month: i64,
    /// Low 32 bits of libam's 64-bit play count.
    pub count: u32,
}

/// The three most recent play-count records (`ServerData+0x18`).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct AccountingPlayCount {
    /// Newest history record (libam item 0, the third record in the history file).
    pub current: AccountingPlayCountItem,
    /// libam item 1.
    pub last: AccountingPlayCountItem,
    /// Oldest of the three (libam item 2, the first record in the history file).
    pub before_last: AccountingPlayCountItem,
}

/// `ipc::allnet::ServerAccountingData`.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerAccounting {
    /// Executor response and handle; untouched by the clear step.
    pub response: ServerAccountingResponse,
    /// libam accounting mode `1..=3`, else `0`; the mode names are unresolved.
    pub mode: u32,
    /// `LM.accountingEnabled()`.
    pub playable: bool,
    /// `alpbExGetLogFullStatus == 1`.
    pub log_full: bool,
    /// `alpbExGetNearFullEnable == 1`.
    pub near_full_enabled: bool,
    /// Seconds to the next accounting report; [`Self::UNKNOWN_SECONDS_UNTIL_REPORT`] = unknown.
    pub seconds_until_report: u32,
    /// Play-count history.
    pub play_count: AccountingPlayCount,
    /// `util::DateTime` microseconds of `alpbExLoadLastAccountingReportTime`.
    pub report_time: i64,
    /// `util::DateTime` microseconds of `alpbExLoadLastBgAccountingReportTime`.
    pub bg_report_time: i64,
}

impl ServerAccounting {
    /// `secondsUntilReport` while unknown (after the clear step, or when libam reports an error).
    pub const UNKNOWN_SECONDS_UNTIL_REPORT: u32 = u32::MAX;
}

server_block!(
    allnet_accounting, allnet_accounting_mut: ServerAccounting, ServerAccountingRef, ServerAccountingMut @ 0x0
);

/// `ClientAccountingRequest.task` (labels: request logger table `0x1405EEDE0`, enum names inferred). Values
/// `>= 7` are rejected by the controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, TryFromPrimitive)]
#[repr(u32)]
pub enum AccountingTask {
    /// Enable or disable near-full reporting (`info.enable`).
    NearFull = 0,
    /// Start a credit; publishes the new handle.
    PlayBegin = 1,
    /// End the credit identified by `info.handle`.
    PlayEnd = 2,
    /// End the credit, then start the next one with the `next*` codes.
    PlayContinue = 3,
    /// Account an item.
    Item = 4,
    /// Put a quantity (`info.item_count` as 16 bits).
    Quantity = 5,
    /// Put a general ID string.
    GeneralId = 6,
}

/// `ClientAccountingRequest` (`ClientData+0xD00`, `0x110` bytes): generation, task and the per-task `info` union.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ClientAccounting<'b> {
    /// Request generation (`taskNo` in the spec): a new value submits a request.
    pub generation: u32,
    /// Raw [`AccountingTask`].
    pub task: u32,
    /// Per-task info union (`0x108` bytes), decoded by the [`ClientAccountingRef`] payload accessors.
    pub info: &'b [u8; 264],
}

client_block!(
    allnet_accounting, allnet_accounting_mut: ClientAccounting, ClientAccountingRef, ClientAccountingMut @ 0xD00
);

/// `NearFull` payload (`ClientAccountingNearFullInfo`, 1 byte): libam receives `enable != 0`.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ClientAccountingNearFullInfo {
    /// The `enable` byte.
    pub enable: u8,
}

/// `PlayBegin..=Quantity` payload (`ClientAccountingUnitInfo`, `0x1C` bytes).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ClientAccountingUnitInfo {
    /// Player index.
    pub player: u32,
    /// Kind code.
    pub kind_code: u32,
    /// Status code.
    pub status_code: u32,
    /// Item count (low 8 bits for `PlayEnd`/`PlayContinue`/`Item`) or the quantity (low 16 bits) for `Quantity`.
    pub item_count: u32,
    /// 1-based accounting handle (`PlayEnd`/`PlayContinue`).
    pub handle: u32,
    /// Kind code of the next credit (`PlayContinue`).
    pub next_kind_code: u32,
    /// Status code of the next credit (`PlayContinue`).
    pub next_status_code: u32,
}

/// `GeneralId` payload (`ClientAccountingGeneralIdInfo`, `0x108` bytes).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ClientAccountingGeneralIdInfo<'b> {
    /// Player index.
    pub player: u32,
    /// Kind code.
    pub kind_code: u32,
    /// General ID; the capacity follows from the union size.
    #[zero(capacity = 128)]
    pub general_id: &'b U16CStr,
}

impl<'w> ClientAccountingRef<'w> {
    /// The request's task, `None` for an unknown value (the request fails).
    pub fn task_kind(&self) -> Option<AccountingTask> {
        AccountingTask::try_from(self.task()).ok()
    }

    /// `NearFull` payload: libam's `enable != 0`.
    pub fn near_full_enable(&self) -> bool {
        let (near_full, _) =
            ClientAccountingNearFullInfo::access_prefix(self.info()).expect("a byte payload accepts every bit pattern");
        near_full.enable() != 0
    }

    /// `PlayBegin..=Quantity` payload. Its leading `player` and `kind_code` words are shared with `GeneralId`.
    pub fn unit(&self) -> ClientAccountingUnitInfoRef<'w> {
        let info: &'w [u8; 264] = self.info();
        let (unit, _) =
            ClientAccountingUnitInfo::access_prefix(info).expect("an integer payload accepts every bit pattern");
        unit
    }

    /// `GeneralId` payload, `None` when the ID is not terminated within its 128 units (the request fails).
    pub fn general_id(&self) -> Option<ClientAccountingGeneralIdInfoRef<'w>> {
        let info: &'w [u8; 264] = self.info();
        ClientAccountingGeneralIdInfo::access_prefix(info).ok().map(|(payload, _)| payload)
    }
}

// ---------------------------------------------------------------------------------------------
// WiFi: ServerData+0x47FC, 0x3DC bytes
// ---------------------------------------------------------------------------------------------

/// `ipc::allnet::ServerWiFiUnitData` (`0x62` bytes, stride 98).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerWiFiUnit<'b> {
    /// Labelled `authGood` by the old docs; **faithful quirk:** it is the libam unit's `serverAlive` flag
    /// (`WifiUnit+0x46`, getter `0x140227C70`), the same value as [`Self::server_alive`].
    pub auth_good: bool,
    /// `serverAlive`: `WifiUnit+0x46` (`alw int +44 == 1`). libam's real `authGood` flag (`WifiUnit+0x1A`) is never
    /// read by amdaemon.
    pub server_alive: bool,
    /// Low 16 bits of the unit's access count.
    pub access_count: u16,
    /// Unit serial (at most 12 characters).
    #[zero(capacity = 13)]
    pub serial: &'b U16CStr,
    /// Firmware version (at most 20 characters).
    #[zero(capacity = 21)]
    pub firm_version: &'b U16CStr,
    /// Master serial (at most 12 characters), `N/A` when libam has none.
    #[zero(capacity = 13)]
    pub master_serial: &'b U16CStr,
}

/// Number of `ServerWiFiUnitData` entries (`memset(sd+8, 0, 0x3D4)` = 10 × 98 bytes).
pub const MAX_WIFI_UNITS: usize = 10;

/// `ipc::allnet::ServerWiFiData`.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerWiFi<'b> {
    /// `AuthConfig.enable && WifiConfig.enable` (static configuration, not link state).
    pub available: bool,
    /// `min(10, libam unit count)`; not reduced when a slot is skipped.
    pub unit_count: u32,
    /// Units; zeroed on every update, then the first `unit_count` entries are filled.
    pub units: [ServerWiFiUnit<'b>; MAX_WIFI_UNITS],
}

server_block!(allnet_wifi, allnet_wifi_mut: ServerWiFi, ServerWiFiRef, ServerWiFiMut @ 0x47FC);

// ---------------------------------------------------------------------------------------------
// Air: ServerData+0x47A4 (0x58) and ClientData+0x1A64 (0x8)
// ---------------------------------------------------------------------------------------------

/// `ClientAirRequest.task` (table `0x1405EEE18`, enum name inferred): the only known value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, TryFromPrimitive)]
#[repr(u32)]
pub enum AirTask {
    /// Clear: accepted, but without any effect in this build.
    Clear = 0,
}

/// `ipc::allnet::ServerAirData` (`ServerData+0x47A8`, `0x54` bytes): the executor response root.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerAirData<'b> {
    /// Executor response.
    pub response: Response,
    /// `0` after clear, `1` while Air is enabled.
    pub state: u32,
    /// `0` after clear, `4` while Air is enabled.
    pub error: u32,
    /// Empty after clear, 32 × `X` while Air is enabled.
    #[zero(capacity = 33)]
    pub text: &'b U16CStr,
}

/// `ServerData` Air block: the config-derived flag plus the payload.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerAir<'b> {
    /// `process::Config.allnet_air.enable`, stored by the AirController's config observer.
    pub enabled: bool,
    /// Payload and executor response.
    pub payload: ServerAirData<'b>,
}

server_block!(allnet_air, allnet_air_mut: ServerAir, ServerAirRef, ServerAirMut @ 0x47A4);

/// `ipc::allnet::ClientAirRequest` (`ClientData+0x1A64`, `8` bytes).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ClientAir {
    /// Request generation (`requestId` in the spec).
    pub generation: u32,
    /// Raw [`AirTask`].
    pub task: u32,
}

client_block!(allnet_air, allnet_air_mut: ClientAir, ClientAirRef, ClientAirMut @ 0x1A64);

impl ClientAirRef<'_> {
    /// The request's task, `None` for an unknown value (the request fails).
    pub fn task_kind(&self) -> Option<AirTask> {
        AirTask::try_from(self.task()).ok()
    }
}

const _: () = {
    assert!(ServerAuth::SCHEMA_SIZE == 0xAD0);
    assert!(ServerAccounting::SCHEMA_SIZE == 0x58);
    assert!(ClientAccounting::SCHEMA_SIZE == 0x110);
    assert!(ServerWiFi::SCHEMA_SIZE == 0x3DC);
    assert!(ServerAir::SCHEMA_SIZE == 0x58);
    assert!(ClientAir::SCHEMA_SIZE == 0x8);
};
