//! The aimelib layer of the Aime module (`spec/libam/aime.md` parts B and C): the per-unit aimelib core (poll state
//! machine, statuses 0..28, result block `R`, internal error codes), the Aime DB client (AES-128-ECB frames, one
//! TCP connection per request, worker thread) and the NetworkClient scheduler (`0x14025A3A0`).
//!
//! # How the facade drives it
//!
//! One [`AimeLib`] per reader unit (the SubContext `0x1402557F0`) and one reader-less [`AimeLib`] inside the
//! [`NetworkClient`] (`0x140259F40`). A command (Detect, Scan, ...) calls [`AimeLib::start`] once and then
//! [`AimeLib::poll`] every frame until it returns `true`; the outcome is [`AimeLib::result`]. Nothing here blocks or
//! reads a clock: every call that needs time takes `now_ms`, the same monotonic millisecond count the reader layer
//! uses. DB traffic runs on a worker thread; its result is applied inside `poll`.
//!
//! Command table (`aime.md` C 10): the facade's command, the [`Op`] it starts and the [`PollKind`] it polls.
//!
//! | Command | `start(Op::..)` | `poll(PollKind::..)` | outcome |
//! |---|---|---|---|
//! | DetectCardReader | `UnitCheck` | `UnitCheck` | [`AimeLib::reader_strings`], `result().error` |
//! | ScanNfcChip | `Scan { filter: 3 }` | `Scan` | `result()` card fields |
//! | CheckFelicaRegistered | `FelicaIdGet` | `FelicaIdGet` | `result().error` (12 = not registered) |
//! | RegisterFelica | `FelicaIdRegist` | `FelicaIdRegist` | `result().error` |
//! | CheckAimeIdRegistered | `AccountGet` | `AccountGet` | `result().error` (15 = unknown), id, SEGA ID |
//! | RegisterAimeId | `AccountRegist` | `AccountRegist` | id, `result().error` |
//! | UpdateFirmware | [`AimeLib::start_firmware`] | `NfcFirmware` | `result().error`, [`AimeLib::firmware_progress`] |
//! | ResolveServer | `KeepResolveServer` after [`AimeLib::clear_resolve`] | `KeepResolveServer` | `result().error` |
//! | SendPlayLog | `SendAimeLogex` | `SendAimeLogex` | `result().log_thread_error`, [`AimeLib::log_results`] |
//! | QueryCampaignInfo | `CampaignInfoGet` | `CampaignInfoGet` | `result().db_thread_error`, `result().campaign_info` |
//! | QueryCampaignProgress | `CampaignClearInfoGet { aime_id }` | `CampaignClearInfoGet` | `result().error`, `campaign_clear` |
//!
//! The NetworkClient commands (ResolveServer, SendPlayLog, QueryCampaign*) are run by [`NetworkClient`] itself;
//! the facade only calls its methods.

use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;

use backend::aime_db::ServerAddr;

mod client;
mod db;
pub(crate) mod proto;
mod unit;

pub use client::{CampaignError, NetworkClient, PlayLogRecord, PlayLogStore};
pub use unit::AimeLib;

/// The Aime DB host name (`0x14071D520`, B.1).
pub const DB_HOST: &str = "aime.naominet.jp";
/// The Aime DB TCP port (B.1).
pub const DB_PORT: u16 = 22345;

// ---------------------------------------------------------------------------------------------------------
// Error codes
// ---------------------------------------------------------------------------------------------------------

/// The aimelib internal error code (`R+0x80`, `0..=33`, C 11.1). The facade maps it to the libam `ErrorCode`
/// (`0x140257ED0`) and the fatal number (`0x140258010`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct InternalCode(pub u32);

impl InternalCode {
    /// 0 None Error.
    pub const NONE: Self = Self(0);
    /// 1 NFC R/W Initialize Fault.
    pub const NFC_INIT_FAULT: Self = Self(1);
    /// 4 NFC R/W Access Error.
    pub const NFC_ACCESS_ERROR: Self = Self(4);
    /// 5 Card Detect Timeout.
    pub const CARD_DETECT_TIMEOUT: Self = Self(5);
    /// 6 Incompatible Card Found.
    pub const INCOMPATIBLE_CARD: Self = Self(6);
    /// 7 Two Cards Found.
    pub const TWO_CARDS: Self = Self(7);
    /// 8 Server Name Resolve Failed.
    pub const RESOLVE_FAILED: Self = Self(8);
    /// 9 Server Access Timeout.
    pub const SERVER_TIMEOUT: Self = Self(9);
    /// 10 Server Access Error.
    pub const SERVER_ERROR: Self = Self(10);
    /// 11 Invalid FelicaID.
    pub const INVALID_FELICA_ID: Self = Self(11);
    /// 12 FelicaID Not Found.
    pub const FELICA_ID_NOT_FOUND: Self = Self(12);
    /// 13 Cannot Regist FelicaID.
    pub const CANNOT_REGIST_FELICA_ID: Self = Self(13);
    /// 14 Invalid AimeID.
    pub const INVALID_AIME_ID: Self = Self(14);
    /// 15 AimeID Not Found.
    pub const AIME_ID_NOT_FOUND: Self = Self(15);
    /// 16 Cannot Regist AimeID.
    pub const CANNOT_REGIST_AIME_ID: Self = Self(16);
    /// 17 Not Detect Felica.
    pub const NOT_DETECT_FELICA: Self = Self(17);
    /// 19 Not Set Board Information.
    pub const BOARD_INFO_NOT_SET: Self = Self(19);
    /// 20 Cannot Regist LogStatus.
    pub const CANNOT_REGIST_LOG: Self = Self(20);
    /// 21 Already Regist FelicaID.
    pub const ALREADY_REGIST_FELICA_ID: Self = Self(21);
    /// 22 Already Regist AimeID.
    pub const ALREADY_REGIST_AIME_ID: Self = Self(22);
    /// 23..=29 BAN states (server status 4..=10 plus 19).
    pub const BAN_FIRST: Self = Self(23);
    /// 30 NFC R/W Firmup Error or Timeout.
    pub const FIRMUP_ERROR: Self = Self(30);
    /// 31 Campaign Information Get Error.
    pub const CAMPAIGN_ERROR: Self = Self(31);
    /// 32 Card Detect Error.
    pub const CARD_DETECT_ERROR: Self = Self(32);
    /// 33 Felica Control Error.
    pub const FELICA_CONTROL_ERROR: Self = Self(33);

    /// The raw number.
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// The libam `ErrorCode` of this code (`0x140257ED0`, C 11.1); codes without a libam equivalent map to 0.
    pub const fn libam_code(self) -> u32 {
        match self.0 {
            1 => 2,
            2 => 3,
            4 | 32 => 4,
            5 => 5,
            6 => 6,
            7 => 7,
            8 => 8,
            9 | 20 => 9,
            10 => 10,
            11 => 11,
            13 => 12,
            14 => 13,
            16 => 14,
            23 => 15,
            24 => 16,
            25 => 17,
            26 => 18,
            27 => 19,
            28 => 20,
            29 => 21,
            30 => 22,
            31 => 24,
            _ => 0,
        }
    }

    /// Code 0.
    pub const fn is_ok(self) -> bool {
        self.0 == 0
    }
}

/// Why a start function refused (C 9, the non-zero results). `Ok(())` is result 0 (started).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartError {
    /// 1: the shared manager was replaced, impl init failed, or a worker slot is still busy.
    Busy = 1,
    /// 2: the cancel loop is running or the worker thread could not be created.
    CancelRunning = 2,
    /// 4: the status is not idle (or a cancel is pending), or no board info for a DB/log operation.
    NotIdle = 4,
    /// 8: terminating, firmware start refused, or (KeepResolve) the manager already holds a server.
    Refused = 8,
    /// 9: out of memory copying the mailer/web string.
    OutOfMemory = 9,
    /// 16: not initialised.
    NotInitialized = 16,
    /// 32: bad parameter (SendAimeLogex).
    BadParam = 32,
    /// 64: log-type conflict.
    LogTypeConflict = 64,
}

impl StartError {
    /// The original's `int` result.
    pub const fn code(self) -> u32 {
        self as u32
    }
}

/// The result of a start function.
pub type StartResult = Result<(), StartError>;

// ---------------------------------------------------------------------------------------------------------
// Result block R (C 5)
// ---------------------------------------------------------------------------------------------------------

/// `R.flags` bits (C 5).
pub mod flags {
    /// The card is FeliCa.
    pub const FELICA: u32 = 0x1;
    /// The Aime id was auto-registered.
    pub const AIME_REGISTERED: u32 = 0x2;
    /// The FeliCa id was (auto-)registered.
    pub const FELICA_REGISTERED: u32 = 0x4;
    /// FeliCa Lite.
    pub const FELICA_LITE: u32 = 0x8;
    /// FeliCa Standard.
    pub const FELICA_STANDARD: u32 = 0x10;
    /// Mobile FeliCa (card-info `mobile`).
    pub const MOBILE: u32 = 0x20;
    /// Unsupported FeliCa sub-kind.
    pub const UNSUPPORTED: u32 = 0x40;
    /// FeliCa Lite-S.
    pub const LITE_S: u32 = 0x80;
    /// Server account kind 2 (libam `AimeId.flag`).
    pub const KIND_2: u32 = 0x4000;
    /// Server account kind 1.
    pub const KIND_1: u32 = 0x8000;
}

/// A campaign-info entry as the server sends it (160 bytes, 5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawCampaignInfo {
    /// `u32` at 0 (0 = empty).
    pub id: u32,
    /// ANSI name at 4.
    pub name: [u8; 128],
    /// Five decimal YYMMDDHH stamps at 132..152.
    pub stamps: [u32; 5],
}

/// A campaign-clear entry as the server sends it (16 bytes, 5.3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RawCampaignClear {
    /// `u32` at 0 (0 = skip).
    pub id: u32,
    /// `u32` at 4.
    pub flag: u32,
    /// `u32` at 8.
    pub value: u32,
    /// `u32` at 12 (unused).
    pub unused: u32,
}

/// The core's result block `R` (996 bytes in the original, typed here; offsets in the field docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultBlock {
    /// `R+0`: access code, packed BCD (default `FF`×10).
    pub access_code: [u8; 10],
    /// `R+10`: FeliCa IDm.
    pub felica_idm: [u8; 8],
    /// `R+18`: FeliCa PMm.
    pub felica_pmm: [u8; 8],
    /// `R+26`: Lite-S extra data (75 bytes).
    pub lite_s_extra: [u8; 75],
    /// `R+104`: second access code (Lite-S, default `FF`×10).
    pub access_code2: [u8; 10],
    /// `R+120`: Aime id (default -1).
    pub aime_id: i32,
    /// `R+124`: [`flags`].
    pub flags: u32,
    /// `R+128`: internal error code of the last operation.
    pub error: InternalCode,
    /// `R+132`: result of the DB worker slot (campaign info / alive).
    pub db_thread_error: InternalCode,
    /// `R+136`: result of the log worker slot.
    pub log_thread_error: InternalCode,
    /// `R+140`: the four-character card code.
    pub card_code: [u8; 4],
    /// `R+144`: the four-character game code.
    pub game_code: [u8; 4],
    /// `R+148`: card UID.
    pub card_uid: [u8; 16],
    /// `R+164`: card kind (1 Aime/Banapass, 3 Amiibo, others FeliCa).
    pub card_kind: u32,
    /// `R+168`: aux/mobile byte.
    pub aux: u8,
    /// `R+172`: reader card value (`nfc+41332`).
    pub card_value: u32,
    /// `R+176`: campaign info as sent by the server (3 entries).
    pub campaign_info: [RawCampaignInfo; 3],
    /// `R+656`: campaign clear info (3 entries).
    pub campaign_clear: [RawCampaignClear; 3],
    /// `R+704`: Amiibo words.
    pub amiibo: [u32; 5],
    /// `R+724`: related Aime ids (default -1).
    pub related_aime_ids: [i32; 2],
    /// `R+732`: SEGA ID string (NUL terminated, 257 bytes).
    pub sega_id: [u8; 257],
    /// `R+992`: server found the Aime id (1 + number of related ids).
    pub aime_found: u32,
}

impl Default for ResultBlock {
    /// The default constructor `0x140329D50`.
    fn default() -> Self {
        Self {
            access_code: [0xFF; 10],
            felica_idm: [0; 8],
            felica_pmm: [0; 8],
            lite_s_extra: [0; 75],
            access_code2: [0xFF; 10],
            aime_id: -1,
            flags: 0,
            error: InternalCode::NONE,
            db_thread_error: InternalCode::NONE,
            log_thread_error: InternalCode::NONE,
            card_code: [0; 4],
            game_code: [0; 4],
            card_uid: [0; 16],
            card_kind: 0,
            aux: 0,
            card_value: 0,
            campaign_info: [RawCampaignInfo { id: 0, name: [0; 128], stamps: [0; 5] }; 3],
            campaign_clear: [RawCampaignClear::default(); 3],
            amiibo: [0; 5],
            related_aime_ids: [-1; 2],
            sega_id: [0; 257],
            aime_found: 0,
        }
    }
}

// ---------------------------------------------------------------------------------------------------------
// Configuration, shared manager
// ---------------------------------------------------------------------------------------------------------

/// What the SubContext (`0x1402557F0`) takes from `am::aime::Config`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LibConfig {
    /// `Config+8` (`card_accept_time`), the scan timeout in seconds (`0x140327000`).
    pub scan_timeout_sec: u32,
    /// `Config+4` (`polling_interval`), the card poll period (`0x14032D430`); the reader layer's
    /// `ReaderConfig::repoll_wait_ms` is set by the facade.
    pub polling_interval: u32,
}

/// The board info of `setBoardInfo` (`0x14032D170`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BoardInfo {
    /// Game id, at most 4 characters are used by the DB client.
    pub game_id: String,
    /// Serial, at most 11 characters are used by the DB client.
    pub serial: String,
    /// Board argument (the u32 of the libam context).
    pub arg: u32,
}

/// The process-global reader-manager wrapper's shared state (C 1): the resolved server record. Clones share the
/// state; units and the NetworkClient of one module use the same one.
#[derive(Clone, Default)]
pub struct LibManager {
    inner: Rc<RefCell<ManagerState>>,
}

#[derive(Debug, Default)]
struct ManagerState {
    /// Bumped by [`LibManager::replace`]; a unit created under an older value is invalid (C 4).
    generation: u64,
    /// The resolved server record shared by every unit (`getResolved(mgr)`).
    resolved: Option<ServerAddr>,
}

impl LibManager {
    /// A fresh manager.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces the shared state (the original's "global manager was replaced"): every [`AimeLib`] created before
    /// answers [`StartError::Busy`] to its next start and drops its resolved server.
    pub fn replace(&self) {
        let mut state = self.inner.borrow_mut();
        state.generation += 1;
        state.resolved = None;
    }

    pub(crate) fn generation(&self) -> u64 {
        self.inner.borrow().generation
    }

    pub(crate) fn resolved(&self) -> Option<ServerAddr> {
        self.inner.borrow().resolved
    }

    pub(crate) fn set_resolved(&self, addr: Option<ServerAddr>) {
        self.inner.borrow_mut().resolved = addr;
    }
}

impl fmt::Debug for LibManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LibManager").finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------------------------------------
// The per-unit core
// ---------------------------------------------------------------------------------------------------------

/// One 48-byte multi-log entry (`SendAimeLogex`, 9.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogEntry {
    /// Aime id (@0).
    pub aime_id: u32,
    /// Status 1..=4 (@4).
    pub status: u32,
    /// Credit (@16).
    pub credit: u32,
    /// `time_t` of the record (@32).
    pub time: i64,
    /// Record byte at 20 (@40).
    pub record_byte: u32,
    /// Region (@44).
    pub region: u32,
}

impl LogEntry {
    /// The 48-byte wire entry (9.5): `u32 aime_id@0, u32 status@4, u64 0@8, u32 credit@16, 12 zero bytes,
    /// i64 time@32, u32 record_byte@40, u32 region@44`, little endian.
    pub(crate) fn encode(&self) -> [u8; 48] {
        let mut entry = [0u8; 48];
        entry[0..4].copy_from_slice(&self.aime_id.to_le_bytes());
        entry[4..8].copy_from_slice(&self.status.to_le_bytes());
        entry[16..20].copy_from_slice(&self.credit.to_le_bytes());
        entry[32..40].copy_from_slice(&self.time.to_le_bytes());
        entry[40..44].copy_from_slice(&self.record_byte.to_le_bytes());
        entry[44..48].copy_from_slice(&self.region.to_le_bytes());
        entry
    }
}

/// The 28-byte single log record (`SendAimeLog`, 9.1): `u32@0, u32@4, u64@8, u32@16, u32@20, u32@24`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SingleLog {
    /// The six fields in order.
    pub fields: (u32, u32, u64, u32, u32, u32),
}

/// An operation to start (C 9.1). Each variant names the start function(s) it stands for.
#[derive(Debug, Clone)]
pub enum Op {
    /// `startNFCUnitCheck`: reader init only, mode 4.
    UnitCheck,
    /// `startNFCScan`: init and card poll, mode 5.
    Scan {
        /// Card-type mask (libam: 3).
        filter: u8,
    },
    /// `startMailerOpen`, mode 1.
    MailerOpen(String),
    /// `startWebOpen`, mode 2.
    WebOpen(String),
    /// `startAmiiboInfoGet`, mode 3.
    AmiiboInfoGet,
    /// `startAccountCheck`: DB lookup of the caller's `R`, mode 7.
    AccountCheck(Box<ResultBlock>),
    /// `startAccountScan`: scan and DB lookup with auto-registration, mode 9.
    AccountScan {
        /// Card-type mask.
        filter: u8,
    },
    /// `startFeliCaIDCheck`: scan and DB lookup without registration, mode 8.
    FelicaIdCheck,
    /// `startAccountGet`: mode 17 (18 when `R.flags & 0x80`).
    AccountGet,
    /// `startAccountGetS`: mode 18.
    AccountGetS,
    /// `startAccountGetex`: mode 12 (18 when `R.flags & 0x80`).
    AccountGetex,
    /// `startAccountRegist`: mode 13.
    AccountRegist,
    /// `startFeliCaIDGet`: mode 10.
    FelicaIdGet,
    /// `startFeliCaIDRegist`: mode 11.
    FelicaIdRegist,
    /// `startCampaignClearInfoGet`: mode 16.
    CampaignClearInfoGet {
        /// The Aime id (`impl+2276`).
        aime_id: u32,
    },
    /// `startKeepResolveServer`: mode 15.
    KeepResolveServer,
    /// `startDBAliveCheck`: DB worker slot, no gate.
    DbAliveCheck,
    /// `startCampaignInfoGet`: DB worker slot, no gate.
    CampaignInfoGet,
    /// `startSendLogStatus`: log slot type 1.
    SendLogStatus {
        /// `impl+2276`.
        a: u32,
        /// `impl+2280`.
        b: u32,
    },
    /// `startSendAimeLog`: log slot type 2.
    SendAimeLog(SingleLog),
    /// `startSendAimeLogex`: log slot type 3, at most 20 entries are meaningful.
    SendAimeLogex(Vec<LogEntry>),
}

/// Which poll wrapper to call (C 9.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollKind {
    /// Mode 4 (`0x14032C940`).
    UnitCheck,
    /// Mode 5 (`0x14032C8C0`).
    Scan,
    /// Mode 1 (`0x14032C590`).
    MailerOpen,
    /// Mode 2 (`0x14032CED0`).
    WebOpen,
    /// Mode 3 (`0x14032BD10`).
    AmiiboInfoGet,
    /// Mode 7 (`0x14032B9C0`).
    AccountCheck,
    /// Mode 8 (`0x14032C090`).
    FelicaIdCheck,
    /// Mode 9 (`0x14032BC80`).
    AccountScan,
    /// Mode 10 (`0x14032C110`).
    FelicaIdGet,
    /// Mode 11 (`0x14032C190`).
    FelicaIdRegist,
    /// Mode 12/18 (`0x14032BB60`).
    AccountGetex,
    /// Mode 13 (`0x14032BC00`).
    AccountRegist,
    /// Mode 17/18 (`0x14032BA40`).
    AccountGet,
    /// Mode 18 (`0x14032BAE0`).
    AccountGetS,
    /// Mode 16 (`0x14032BD90`).
    CampaignClearInfoGet,
    /// Mode 15 (`0x14032C230`).
    KeepResolveServer,
    /// Campaign-info worker slot (`0x14032BE10`).
    CampaignInfoGet,
    /// Alive-check worker slot (`0x14032BF70`).
    DbAliveCheck,
    /// Log slot type 1 (`0x14032CD40`).
    SendLogStatus,
    /// Log slot type 2 (`0x14032CA10`).
    SendAimeLog,
    /// Log slot type 3 (`0x14032CBA0`).
    SendAimeLogex,
    /// NFC firmware poller (`0x14032C610`).
    NfcFirmware,
    /// LED board firmware poller (`0x14032C2C0`).
    LedFirmware,
}

/// The reader identification strings the Detect command shows (C 5.1, 9.4).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReaderStrings {
    /// Hardware version text (`0x14032AB60`).
    pub hw: String,
    /// Firmware version text (`0x14032AAF0`).
    pub fw: String,
    /// LED board firmware name (`0x14032ABD0`, "No LED Board" without a board).
    pub led_fw: String,
    /// LED board version byte (`0x14032AC40`; libam formats `%u.%02u` of `v >> 4`, `v & 15`).
    pub led_version: u8,
    /// LED board present (`0x14032AFE0`).
    pub led_present: bool,
    /// Hardware generation (`0x14032ACE0`, reader-info byte 2).
    pub generation: u8,
}
