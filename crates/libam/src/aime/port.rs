//! The aime facade (`am::aime`): card readers, Aime ids, play logs and campaigns. [`AimePort`], the values it
//! exchanges and libam's [`AimeHandle`].

use crate::error::FatalError;
use crate::result::AmResult;
use crate::shared::Shared;
use crate::time::Tm;

/// Wide-string capacity of the libam campaign name (`entry+4 .. entry+260`).
pub const CAMPAIGN_NAME_UNITS: usize = 128;

// ---------------------------------------------------------------------------------------------------------
// libam value types (port boundary)
// ---------------------------------------------------------------------------------------------------------

/// libam play-log status (`sendPlayLog` argument); `Invalid` makes libam answer result code 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayLogStatus {
    /// A request status outside `0..=3`.
    Invalid = 0,
    /// Request status 0: the player entered.
    Enter = 1,
    /// Request status 1: the player continues.
    Continue = 2,
    /// Request status 2: the player left.
    Leave = 3,
    /// Request status 3: an event.
    Event = 4,
}

impl PlayLogStatus {
    /// Request status → libam status (`0x1400136F0`: `0→1, 1→2, 2→3, 3→4`, anything else `0`).
    pub fn from_request(status: u32) -> Self {
        match status {
            0 => Self::Enter,
            1 => Self::Continue,
            2 => Self::Leave,
            3 => Self::Event,
            _ => Self::Invalid,
        }
    }
}

/// libam `AimeId` object: `{u32 id; u8 flag}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LibamAimeId {
    /// Aime id; `0xFFFFFFFF` when none.
    pub id: u32,
    /// Flag byte at `+4` (published as `segaIdRegistered`).
    pub flag: bool,
}

/// libam access code: the ten raw bytes of libam's access-code string (packed BCD, 20 digits), as the card reader
/// module hands them out and as the Aime Pay module takes them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct AccessCode(pub [u8; 10]);

impl AccessCode {
    /// The default-constructed `am::aimepay::AccessCode` (`0x1402207B0`): all bytes `0xFF`.
    pub const UNSET: Self = Self([0xFF; 10]);

    /// Packed-BCD validity (`0x140061370`): every nibble is a decimal digit.
    pub fn is_valid(bytes: &[u8; 10]) -> bool {
        bytes.iter().all(|&byte| byte <= 0x99 && byte & 0x0F <= 9)
    }

    /// `0x140220860`: the code for valid BCD bytes, `None` otherwise.
    pub fn parse(bytes: &[u8; 10]) -> Option<Self> {
        Self::is_valid(bytes).then_some(Self(*bytes))
    }
}

/// libam `LedLightPattern`: the reader LED pattern `setLedLightPattern` selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedPattern {
    /// LED off.
    None = 0,
    /// Scanning.
    Scanning = 1,
    /// Success.
    Success = 2,
    /// Warning.
    Warning = 3,
    /// Error (libam category-1 errors select it).
    Error = 4,
}

/// libam card reader info of one unit (`getReaderInfo`, `unit+2904`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReaderInfo {
    /// Byte 0: a card reader was detected.
    pub detected: bool,
    /// `wchar_t[64]` at `+4`.
    pub hard_version: [u16; 64],
    /// `wchar_t[64]` at `+0x84`.
    pub firm_version: [u16; 64],
}

/// libam card info of one unit (`getCardInfo`, `unit+3436`); the port returns it only when `card_type != 0`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CardInfo {
    /// `u32` at `+0`: 2 = FeliCa.
    pub card_type: u32,
    /// FeliCa IDm, `u64` at `+32`.
    pub idm: u64,
    /// FeliCa PMm, `u64` at `+40`.
    pub pmm: u64,
    /// Byte at `+48`: the card is a mobile device.
    pub mobile: bool,
}

impl CardInfo {
    /// `card_type` of a FeliCa card.
    pub const FELICA: u32 = 2;
}

/// libam SEGA ID info of one unit (`getInfoRelatedSegaId`, `unit+3616`); the port returns it only when the
/// first character of `auth_key` is not NUL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelatedSegaId {
    /// `wchar_t[257]` at `+0`.
    pub auth_key: [u16; 257],
    /// `u64` at `+520`: number of related ids in `items`.
    pub count: u64,
    /// Three `{u32 id; u8 flag}` slots at `+528 + 8·i`; `None` where libam's `get(i)` reports an error.
    pub items: [Option<u32>; 3],
}

/// One entry of libam's campaign-info list (440 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CampaignInfoEntry {
    /// `u32` at `+0`; an entry with id 0 is skipped.
    pub id: u32,
    /// `wchar_t[128]` at `+4`.
    pub name: [u16; CAMPAIGN_NAME_UNITS],
    /// Five `tm` at `+260 + 36·k`.
    pub times: [Tm; 5],
}

/// libam's campaign-info list: `u64 count` followed by three entries (`+1392`). `get(i)` fails for `i >= 3`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CampaignInfoList {
    /// Number of entries libam claims.
    pub count: u64,
    /// Entry storage.
    pub entries: [CampaignInfoEntry; 3],
}

/// One entry of libam's campaign-progress list (12 bytes).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CampaignProgressEntry {
    /// `u32` at `+0`; an entry with id 0 is skipped.
    pub id: u32,
    /// Byte at `+4` (published as `flag`).
    pub flag: bool,
    /// `u32` at `+8`.
    pub value: u32,
}

/// libam's campaign-progress list: `u64 count` followed by three entries (`+2720`). `get(i)` fails for `i >= 3`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CampaignProgressList {
    /// Number of entries libam claims.
    pub count: u64,
    /// Entry storage.
    pub entries: [CampaignProgressEntry; 3],
}

/// libam `TaskFinishedEventListener::Task` (names from the libam emit sites).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishedTask {
    /// Commands without a task id (ForceFailure, SetCardReaderLed, ResolveServer, SendPlayLog, ...).
    None = 0,
    /// `DetectCardReader`.
    DetectCardReader = 1,
    /// `ScanNfcChip`.
    ScanNfcChip = 2,
    /// `CheckFelicaRegistered`.
    CheckFelicaRegistered = 3,
    /// `RegisterFelica`.
    RegisterFelica = 4,
    /// `CheckAimeIdRegistered`.
    CheckAimeIdRegistered = 5,
    /// `RegisterAimeId`.
    RegisterAimeId = 6,
    /// `UpdateFirmware`.
    UpdateFirmware = 7,
    /// `QueryCampaignProgress`.
    QueryCampaignProgress = 8,
    /// Whole sequence finished (raised per unit by `0x140259900`, globally by `0x14025A3A0`).
    SequenceFinished = 9,
}

/// libam `TaskFinishedEventListener::Status` (names from the libam emit sites).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishedStatus {
    /// Queued / reset.
    Queued = 0,
    /// Started.
    Started = 1,
    /// Succeeded.
    Succeeded = 2,
    /// Failed.
    Failed = 3,
    /// Cancelled.
    Cancelled = 4,
}

/// Unit `TaskFinished` callback payload (`0x140014E80`).
#[derive(Debug, Clone)]
pub struct UnitTaskFinished {
    /// libam unit index; indices `>= 4` are dropped.
    pub unit: u64,
    /// Finished task.
    pub task: FinishedTask,
    /// Task status.
    pub status: FinishedStatus,
    /// libam `TaskFinishedEventListener::ErrorCode` (0..=24 are converted).
    pub error_code: u32,
    /// Fatal error attached to the event (only Task 9 reads it).
    pub fatal: FatalError,
}

/// One libam listener callback destined for the Aime controller.
#[derive(Debug, Clone)]
pub enum AimeEvent {
    /// `TaskFinishedEventListener` unit overload (`0x140014E80`); boxed because the attached `FatalError` is
    /// much larger than every other payload.
    UnitTaskFinished(Box<UnitTaskFinished>),
    /// `TaskFinishedEventListener` global overload (`0x140014E50`); error code and fatal error are ignored.
    TaskFinished {
        /// Finished task.
        task: FinishedTask,
        /// Task status.
        status: FinishedStatus,
    },
    /// `NeedAcceptationEventListener` (`0x140014EF0`).
    NeedAcceptation {
        /// libam unit index.
        unit: u64,
        /// libam `NeedAcceptation::Question` (1 and 2 are converted).
        question: u32,
    },
    /// `FirmwareUpdatingProgressEventListener` (`0x140014F50`).
    FirmwareUpdatingProgress {
        /// libam unit index.
        unit: u64,
        /// Progress, 0..=100.
        progress: f32,
    },
    /// `ChangeLedLightPatternEventListener` (`0x140014FA0`).
    ChangeLedLightPattern {
        /// libam unit index.
        unit: u64,
        /// libam `LedLightPattern` (0..=4 are converted).
        pattern: u32,
    },
}

/// libam as seen by the Aime controller: every call the original makes through the facade wrappers
/// (`spec/controllers/aime.md` §10). Getters that return a pointer in libam return `None` when libam answers a
/// non-zero result or a null / invalid object.
pub trait AimePort {
    /// `0x140219390(Manager+8)`: the libam facade handle is usable (the global libam context exists).
    fn facade_available(&self) -> bool;
    /// `0x140215590`: number of units the libam configuration provides.
    fn unit_count(&self) -> u64;
    /// `0x140215710`: the Aime server / database is reachable.
    fn is_db_alive(&self) -> bool;
    /// `0x140215440` (inner `0x140229980`): queue DetectCardReader and ScanNfcChip.
    fn start_scan_offline(&mut self, unit: usize) -> AmResult;
    /// `0x140215480` (inner `0x140229B00`): queue the full scan sequence.
    fn start_scan(&mut self, unit: usize) -> AmResult;
    /// `0x1402154A0` (inner `0x140229BC0`): answer the pending question.
    fn confirm(&mut self, unit: usize) -> AmResult;
    /// `0x1402154C0` (inner `0x140229C20`): cancel the running sequence.
    fn cancel(&mut self, unit: usize) -> AmResult;
    /// `0x1402154E0` (inner `0x140229670`): drive the reader LED; each channel is `0` or `0xFF`.
    fn set_led(&mut self, unit: usize, red: u8, green: u8, blue: u8) -> AmResult;
    /// `0x140215510` (inner `0x140229720`): select the reader LED pattern.
    fn set_led_pattern(&mut self, unit: usize, pattern: LedPattern) -> AmResult;
    /// `0x1402155B0`: reader info of a unit.
    fn reader_info(&self, unit: usize) -> Option<ReaderInfo>;
    /// `0x1402155D0`: card info of a unit (`None` while no card type is known).
    fn card_info(&self, unit: usize) -> Option<CardInfo>;
    /// `0x1402155F0`: the 10 raw access-code bytes (`None` while libam's access-code string is empty).
    fn access_code(&self, unit: usize) -> Option<AccessCode>;
    /// `0x140215610`: the Aime id of a unit (`None` while the id is `0xFFFFFFFF`).
    fn aime_id(&self, unit: usize) -> Option<LibamAimeId>;
    /// `0x140215630`: SEGA ID info of a unit (`None` while the auth key is empty).
    fn info_related_sega_id(&self, unit: usize) -> Option<RelatedSegaId>;
    /// `0x140215650` (inner `0x14022A0D0`): queue a play log.
    fn send_play_log(&mut self, status: PlayLogStatus, aime_id: LibamAimeId, time: &Tm, credit: u32) -> AmResult;
    /// `0x1402156B0` (inner `0x14022A340`): start the campaign-progress network query.
    fn query_campaign_progress(&mut self, aime_id: LibamAimeId) -> AmResult;
    /// `0x1402156D0` (inner `0x14022A3F0` → `0x14025A330`, `campaign+1392`): a copy of the campaign-info list,
    /// `u64` count and three 440-byte entries (`1328 = 8 + 3·440` bytes, up to the progress list at `+2720`).
    fn campaign_info_list(&self) -> Option<CampaignInfoList>;
    /// `0x1402156F0` (inner `0x14022A490` → `0x14025A340`, `campaign+2720`): a copy of the campaign-progress
    /// list, `u64` count and three 12-byte entries.
    fn campaign_progress_list(&self) -> Option<CampaignProgressList>;
    /// Listener callbacks libam raised since the last call, in order.
    fn take_events(&mut self) -> Vec<AimeEvent>;
}

// ---------------------------------------------------------------------------------------------------------
// Configuration (AppConfig hook `0x1405C0848` → `0x140016510`)
// ---------------------------------------------------------------------------------------------------------

/// The part of libam's `am::aime::Config` the controller fills (`AppConfig+9744`, slots `+0x908..+0x920`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AimeAppConfig {
    /// `cfg+0x908`: `TaskFinishedEventListener` installed (`ctrl+0x28`).
    pub task_finished_listener: bool,
    /// `cfg+0x910`: `NeedAcceptationEventListener` installed (`ctrl+0x30`).
    pub need_acceptation_listener: bool,
    /// `cfg+0x918`: `FirmwareUpdatingProgressEventListener` installed (`ctrl+0x38`).
    pub firmware_updating_progress_listener: bool,
    /// `cfg+0x920`: `ChangeLedLightPatternEventListener` installed (`ctrl+0x40`).
    pub change_led_light_pattern_listener: bool,
}

impl AimeAppConfig {
    /// Body of the hook `0x140016510`: hands all four listeners to libam.
    pub fn install_listeners(&mut self) {
        *self = Self {
            task_finished_listener: true,
            need_acceptation_listener: true,
            firmware_updating_progress_listener: true,
            change_led_light_pattern_listener: true,
        };
    }
}

/// `AimePort` handle over the shared libam state.
#[derive(Debug)]
pub struct AimeHandle(pub(crate) Shared);

impl AimePort for AimeHandle {
    fn facade_available(&self) -> bool {
        self.0.guard().is_ok()
    }

    fn unit_count(&self) -> u64 {
        self.0.read(|core| core.aime.unit_count())
    }

    fn is_db_alive(&self) -> bool {
        self.0.read(|core| core.aime.is_db_alive())
    }

    fn start_scan_offline(&mut self, unit: usize) -> AmResult {
        self.0.guard()?;
        self.0.with(|core| core.aime.start_scan(unit, 2, &core.backend, &*core.clock))
    }

    fn start_scan(&mut self, unit: usize) -> AmResult {
        self.0.guard()?;
        self.0.with(|core| core.aime.start_scan(unit, 5, &core.backend, &*core.clock))
    }

    fn confirm(&mut self, unit: usize) -> AmResult {
        self.0.guard()?;
        self.0.with(|core| core.aime.confirm(unit))
    }

    fn cancel(&mut self, unit: usize) -> AmResult {
        self.0.guard()?;
        self.0.with(|core| core.aime.cancel(unit))
    }

    fn set_led(&mut self, unit: usize, red: u8, green: u8, blue: u8) -> AmResult {
        self.0.guard()?;
        self.0.with(|core| core.aime.set_led(unit, [red, green, blue]))
    }

    fn set_led_pattern(&mut self, unit: usize, pattern: LedPattern) -> AmResult {
        self.0.guard()?;
        self.0.with(|core| core.aime.set_led_pattern(unit, pattern))
    }

    fn reader_info(&self, unit: usize) -> Option<ReaderInfo> {
        self.0.read(|core| core.aime.reader_info(unit))
    }

    fn card_info(&self, unit: usize) -> Option<CardInfo> {
        self.0.read(|core| core.aime.card_info(unit))
    }

    fn access_code(&self, unit: usize) -> Option<AccessCode> {
        self.0.read(|core| core.aime.access_code(unit))
    }

    fn aime_id(&self, unit: usize) -> Option<LibamAimeId> {
        self.0.read(|core| core.aime.aime_id(unit))
    }

    fn info_related_sega_id(&self, unit: usize) -> Option<RelatedSegaId> {
        self.0.read(|core| core.aime.related_sega_id(unit))
    }

    fn send_play_log(&mut self, status: PlayLogStatus, aime_id: LibamAimeId, time: &Tm, credit: u32) -> AmResult {
        self.0.guard()?;
        self.0.with(|core| core.aime.enqueue_log(status as u32, aime_id.id, time, credit))
    }

    fn query_campaign_progress(&mut self, aime_id: LibamAimeId) -> AmResult {
        self.0.guard()?;
        self.0.with(|core| {
            let now = core.clock.tick_ms();
            core.aime.query_campaign_progress(aime_id, now)
        })
    }

    fn campaign_info_list(&self) -> Option<CampaignInfoList> {
        self.0.read(|core| core.aime.campaign_info_list())
    }

    fn campaign_progress_list(&self) -> Option<CampaignProgressList> {
        self.0.read(|core| core.aime.campaign_progress_list())
    }

    fn take_events(&mut self) -> Vec<AimeEvent> {
        self.0.with(|core| core.aime.take_events())
    }
}
