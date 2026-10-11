//! Aime blocks: `ServerData+0x3D0` (0x17B8) and `ClientData+0xBF0` (0x50).
//!
//! Evidence: `spec/controllers/aime.md` §2 (server layout), §3 (client layout), §8 (enumerations),
//! claims `aime.wire.*`, `aime.type.*`, `aime.enum.*`.
//!
//! Field names that are not literal in the binary are inferred from the code (spec tag "I");
//! offsets, sizes and numeric semantics are read from the binary.

use num_enum::TryFromPrimitive;
use widestring::U16CStr;
use zero_schema::zero;

use crate::schema::shared::{ErrorData, Response};
use crate::schema::{client_block, server_block};

/// `AimeId` value that means "no Aime" (`0xFFFFFFFF`).
pub const AIME_ID_INVALID: u32 = 0xFFFF_FFFF;

/// `AimeConfirm`: the question libam asks the player (enumerator names are not in the binary; the numbers
/// are libam's `NeedAcceptation::Question`).
#[zero(endian = "little")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum AimeConfirm {
    /// No question pending.
    None = 0,
    /// Question 1 (posted by the FeliCa registration step).
    Question1 = 1,
    /// Question 2 (posted by the Aime id registration step).
    Question2 = 2,
}

/// `AimeLedStatus` (name table `0x1405EBC20`, helper `0x140062470`); equals libam's `LedLightPattern`.
#[zero(endian = "little")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, TryFromPrimitive)]
#[repr(u32)]
pub enum AimeLedStatus {
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

impl AimeLedStatus {
    /// Name from the log table `0x1405EBC20`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Scanning => "Scanning",
            Self::Success => "Success",
            Self::Warning => "Warning",
            Self::Error => "Error",
        }
    }
}

/// `AimeErrorCategory` as returned by `getCategory` (`0x140062200`); enumerator names are not in the
/// binary. Category 1 is the only one with behaviour (LED pattern 4 on a unit's sequence end).
#[zero(endian = "little")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum AimeErrorCategory {
    /// Only [`AimeErrorId::NONE`].
    Category0 = 0,
    /// Ids 4, 5, 6, 7, 11, 14, 17, 18, 23..=29.
    Category1 = 1,
    /// Ids 8, 9, 10, 13, 16, 31.
    Category2 = 2,
    /// Ids 1, 2, 30.
    Category3 = 3,
    /// Ids 3, 12, 15, 19..=22, 32, 33 (and every id outside the domain).
    Category4 = 4,
}

/// `ipc::aime::Task` of the main request (name table `0x1405EED60`, helper `0x14006D680`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, TryFromPrimitive)]
#[repr(u32)]
pub enum AimeTask {
    /// Send a play log to the Aime server.
    Log = 0,
    /// Query the campaign progress of an Aime.
    CampaignProgress = 1,
}

impl AimeTask {
    /// Name from the log table `0x1405EED60`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Log => "Log",
            Self::CampaignProgress => "CampaignProgress",
        }
    }
}

/// `ipc::aime::UnitTask` of a unit request (name table `0x1405EED70`, helper `0x14006D6B0`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, TryFromPrimitive)]
#[repr(u32)]
pub enum AimeUnitTask {
    /// Start a card scan (payload: [`StartInfo`], an [`AimeCommand`]).
    Start = 0,
    /// Cancel the running scan.
    Cancel = 1,
    /// Confirm the pending question.
    Confirm = 2,
    /// Drive the card reader LED (payload: [`LedInfo`]).
    Led = 3,
    /// Select a card reader LED pattern (payload: [`LedStatusInfo`], an [`AimeLedStatus`]).
    LedStatus = 4,
}

impl AimeUnitTask {
    /// Name from the log table `0x1405EED70`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Start => "Start",
            Self::Cancel => "Cancel",
            Self::Confirm => "Confirm",
            Self::Led => "Led",
            Self::LedStatus => "LedStatus",
        }
    }
}

/// `AimeCommand` (name table `0x1405EBAC0`, helper `0x1400623B0`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, TryFromPrimitive)]
#[repr(u32)]
pub enum AimeCommand {
    /// Offline scan: detect reader, scan NFC chip.
    ScanOffline = 0,
    /// Scan including the network registration steps.
    Scan = 1,
}

impl AimeCommand {
    /// Name from the log table `0x1405EBAC0`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::ScanOffline => "ScanOffline",
            Self::Scan => "Scan",
        }
    }
}

/// `AimeLogStatus` (name table `0x1405EBC48`, helper `0x1400624A0`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, TryFromPrimitive)]
#[repr(u32)]
pub enum AimeLogStatus {
    /// Enter.
    Enter = 0,
    /// Continue.
    Continue = 1,
    /// Leave.
    Leave = 2,
    /// Event.
    Event = 3,
}

impl AimeLogStatus {
    /// Name from the log table `0x1405EBC48`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Enter => "Enter",
            Self::Continue => "Continue",
            Self::Leave => "Leave",
            Self::Event => "Event",
        }
    }
}

/// `AimeErrorId`: closed domain `0..=33` (enumerator names are not in the binary).
///
/// An out-of-domain value cannot be built, so the `getCategory` fallback for such ids
/// (category 4) is not reachable through this type; the only producer of ids is
/// the libam `ErrorCode` converter, whose outputs all lie in the domain. [`AimeUnitError::id`] stores the
/// raw value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AimeErrorId(u32);

impl AimeErrorId {
    /// "No error" (`0`).
    pub const NONE: Self = Self(0);
    /// Largest id of the domain.
    pub const MAX: u32 = 33;

    /// Validated construction; `None` outside `0..=33`.
    pub const fn new(raw: u32) -> Option<Self> {
        if raw <= Self::MAX { Some(Self(raw)) } else { None }
    }

    /// `amdaemon::getCategory` (`0x140062200`).
    pub const fn category(self) -> AimeErrorCategory {
        match self.0 {
            0 => AimeErrorCategory::Category0,
            4..=7 | 11 | 14 | 17 | 18 | 23..=29 => AimeErrorCategory::Category1,
            8..=10 | 13 | 16 | 31 => AimeErrorCategory::Category2,
            1 | 2 | 30 => AimeErrorCategory::Category3,
            _ => AimeErrorCategory::Category4,
        }
    }
}

impl TryFrom<u32> for AimeErrorId {
    type Error = u32;

    fn try_from(raw: u32) -> Result<Self, u32> {
        Self::new(raw).ok_or(raw)
    }
}

impl From<AimeErrorId> for u32 {
    fn from(id: AimeErrorId) -> u32 {
        id.0
    }
}

/// `amdaemon::AccessCode`: 10 bytes of packed BCD (20 digits).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct AccessCode(pub [u8; 10]);

impl AccessCode {
    /// Value of the binary's default constructor (`0x140061730`): `FF×10`. The blocks themselves start
    /// zero-filled, so this is not [`Default::default`].
    pub const UNSET: Self = Self([0xFF; 10]);

    /// Validity test of `0x140061750` / `0x140061370`: every byte `<= 0x99` with a low nibble `<= 9`.
    pub const fn is_valid_bcd(&self) -> bool {
        let mut i = 0;
        while i < self.0.len() {
            if self.0[i] > 0x99 || (self.0[i] & 0xF) > 9 {
                return false;
            }
            i += 1;
        }
        true
    }
}

/// `amdaemon::AimeOfflineId`: a kind byte followed by 16 payload bytes; all-zero means "none".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct AimeOfflineId(pub [u8; 17]);

impl AimeOfflineId {
    /// No offline id (all zero).
    pub const NONE: Self = Self([0; 17]);

    /// Kind 1 (`0x140062000`): `{01, accessCode[10], 00×6}`.
    pub fn from_access_code(code: &AccessCode) -> Self {
        let mut bytes = [0; 17];
        bytes[0] = 1;
        bytes[1..11].copy_from_slice(&code.0);
        Self(bytes)
    }

    /// Kind 2 (`0x140062050`): `{02, IDm(8) ‖ PMm(8)}`, both stored as little-endian `u64`.
    pub fn from_felica(idm: u64, pmm: u64) -> Self {
        let mut bytes = [0; 17];
        bytes[0] = 2;
        bytes[1..9].copy_from_slice(&idm.to_le_bytes());
        bytes[9..17].copy_from_slice(&pmm.to_le_bytes());
        Self(bytes)
    }
}

/// `ServerUnitData.result` (`unit+0x14`, `0x334` bytes): what the reader of one unit reported.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct AimeUnitResult<'b> {
    /// `+0x000` reader data valid (set by Task 1 / Task 7).
    pub valid: bool,
    /// `+0x001` card reader detected (libam reader info byte 0).
    pub reader_detected: bool,
    /// `+0x002` reader hardware version (`wcsncpy_s(dst, 64, src, 63)`).
    #[zero(capacity = 64)]
    pub hard_version: &'b U16CStr,
    /// `+0x082` reader firmware version (`wcsncpy_s(dst, 64, src, 63)`).
    #[zero(capacity = 64)]
    pub firm_version: &'b U16CStr,
    /// `+0x102` raw [`AimeOfflineId`] of the scanned card.
    pub offline_id: &'b [u8; 17],
    /// `+0x113` the card is a mobile device (libam card info byte `+48`).
    pub mobile: bool,
    /// `+0x114` raw [`AccessCode`] (BCD).
    pub access_code: &'b [u8; 10],
    /// `+0x120` Aime id, [`AIME_ID_INVALID`] when unknown.
    pub aime_id: u32,
    /// `+0x124` libam AimeId flag byte (SEGA ID registered; inferred name).
    pub sega_id_registered: bool,
    /// `+0x126` SEGA ID auth key string (`wcsncpy_s(dst, 257, src, 256)`; inferred name).
    #[zero(capacity = 257)]
    pub sega_id_auth_key: &'b U16CStr,
    /// `+0x328` number of valid entries of [`Self::related_aime_ids`] (0..=2).
    pub related_aime_id_count: u8,
    /// `+0x32C` Aime ids related to the SEGA ID (libam list slots 1..=2).
    pub related_aime_ids: [u32; 2],
}

impl AimeUnitResultMut<'_> {
    /// The reset image written by every Start accept (`0x1400158E0`): everything cleared and the
    /// "unknown" markers (`FF×10` access code, `0xFFFFFFFF` ids) set. Strings are reset with
    /// `wcsncpy_s(dst, N, L"", N - 1)`, i.e. only the terminator is written and the tail of the old string
    /// stays in place.
    pub fn reset(&mut self) {
        let empty = <&U16CStr>::default();
        let patch = AimeUnitResultPatch {
            valid: Some(false),
            reader_detected: Some(false),
            hard_version: Some(empty),
            firm_version: Some(empty),
            offline_id: Some(&AimeOfflineId::NONE.0),
            mobile: Some(false),
            access_code: Some(&AccessCode::UNSET.0),
            aime_id: Some(AIME_ID_INVALID),
            sega_id_registered: Some(false),
            sega_id_auth_key: Some(empty),
            related_aime_id_count: Some(0),
            related_aime_ids: Some([AIME_ID_INVALID; 2]),
        };
        self.copy_from(&patch).expect("scalars, byte arrays and the empty string fit their fields");
    }
}

/// `ServerUnitErrorData` (`unit+0x348`, `0x1A8` bytes): the error that ended the unit's sequence.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct AimeUnitError<'b> {
    /// `+0x000` raw [`AimeErrorId`] of the converted libam error code.
    pub id: u32,
    /// `+0x004` category of [`Self::id`].
    pub category: AimeErrorCategory,
    /// `+0x008` converted libam `FatalError` (written by `FatalError::publish`).
    pub data: ErrorData<'b>,
}

/// `ServerUnitData` (`0x4F8` bytes, alignment 8): published state of one card reader unit.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerAimeUnit<'b> {
    /// `+0x000` unit request response (`taskNo`, `succeeded`).
    pub response: Response,
    /// `+0x008` a scan sequence is running (Start accept writes `!error`, sequence end clears it; inferred name).
    pub busy: bool,
    /// `+0x00C` command of the last Start request, stored as the client sent it (raw [`AimeCommand`]).
    pub busy_command: u32,
    /// `+0x010` question libam asked, `0` when none.
    pub confirm: AimeConfirm,
    /// `+0x014` reader / card data.
    pub result: AimeUnitResult<'b>,
    /// `+0x348` error that ended the last sequence.
    pub error: AimeUnitError<'b>,
    /// `+0x4F0` last LED pattern libam reported.
    pub led_status: AimeLedStatus,
}

/// `AimeCampaignInfo` (`0x130` bytes): one campaign announced by the Aime server.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct AimeCampaignInfo<'b> {
    /// `+0x000` campaign id (an entry with id 0 is never copied).
    pub campaign_id: u32,
    /// `+0x004` campaign name (`wcsncpy_s(dst, 128, src, 127)`).
    #[zero(capacity = 128)]
    pub name: &'b U16CStr,
    /// `+0x108`, `+0x110`, `+0x118`, `+0x120`, `+0x128`: `util::DateTime` microseconds of libam campaign
    /// `tm` 0..=4 (meaning unresolved); a slot is only written when its `tm` is a valid date.
    pub times: [i64; 5],
}

/// `AimeCampaignProgress` (`0xC` bytes): one entry of the campaign progress list.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct AimeCampaignProgress {
    /// `+0x0` campaign id (an entry with id 0 is never copied).
    pub campaign_id: u32,
    /// `+0x4` libam entry byte `+4` (meaning unresolved; plausibly "cleared").
    pub flag: bool,
    /// `+0x8` libam entry `+8` (meaning unresolved; plausibly the clear count).
    pub value: u32,
}

/// `ServerCampaignData` (`aime+0x13F8`, `0x3C0` bytes).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerCampaign<'b> {
    /// `+0x000` libam campaign-info list count (truncated to `u16`).
    pub info_count: u16,
    /// `+0x008` campaign infos.
    pub infos: [AimeCampaignInfo<'b>; 3],
    /// `+0x398` libam campaign-progress list count (truncated to `u16`).
    pub progress_count: u16,
    /// `+0x39C` campaign progress entries of the last query.
    pub progresses: [AimeCampaignProgress; 3],
}

/// Firmware-update aggregate (`aime+0x8`, 8 bytes).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerFirm {
    /// `+0x0` any unit is updating its firmware and not all are done.
    pub updating: bool,
    /// `+0x4` mean firmware progress of the units, 0..=100.
    pub update_progress: f32,
}

/// `ServerData.aime` (`SD+0x3D0`, `0x17B8` bytes).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerAime<'b> {
    /// `+0x000` main request response (`taskNo`, `succeeded`).
    pub response: Response,
    /// `+0x008` firmware-update aggregate.
    pub firm: ServerFirm,
    /// `+0x010` Aime server / database reachable.
    pub db_alive: bool,
    /// `+0x014` number of card reader units (0..=4).
    pub unit_count: u32,
    /// `+0x018` the four units, unit *i* at `+0x18 + 0x4F8·i`.
    pub units: [ServerAimeUnit<'b>; 4],
    /// `+0x13F8` campaign data.
    pub campaign: ServerCampaign<'b>,
}

/// `ipc::aime::ClientRequest` (`aime+0x0`, `0x20` bytes).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ClientAimeRequest<'b> {
    /// `+0x00` request generation (`taskNo`), mirrored into the response on completion.
    pub generation: u32,
    /// `+0x04` raw [`AimeTask`].
    pub task: u32,
    /// `+0x08` per-task payload, decoded by the [`ClientAimeRequestRef`] accessors.
    pub info: &'b [u8; 12],
    /// `+0x18` Log: `util::DateTime` microseconds since 0001-01-01 (reinterpreted as unsigned by the daemon).
    pub time: i64,
}

/// `ipc::aime::ClientUnitRequest` (`0xC` bytes).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ClientAimeUnitRequest<'b> {
    /// `+0x0` request generation (`taskNo`).
    pub generation: u32,
    /// `+0x4` raw [`AimeUnitTask`].
    pub task: u32,
    /// `+0x8` per-task payload (4 bytes), decoded by the [`ClientAimeUnitRequestRef`] accessors.
    pub info: &'b [u8; 4],
}

/// `ClientData.aime` (`CD+0xBF0`, `0x50` bytes).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ClientAime<'b> {
    /// `+0x00` main request.
    pub request: ClientAimeRequest<'b>,
    /// `+0x20` unit requests, unit *i* at `+0x20 + 0xC·i`.
    pub units: [ClientAimeUnitRequest<'b>; 4],
}

server_block!(aime, aime_mut: ServerAime, ServerAimeRef, ServerAimeMut @ 0x3D0);
client_block!(aime, aime_mut: ClientAime, ClientAimeRef, ClientAimeMut @ 0xBF0);

/// Log payload (12 bytes of the main request's `info`).
#[zero(endian = "little")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogInfo {
    /// Aime id.
    pub aime_id: u32,
    /// Raw [`AimeLogStatus`].
    pub status: u32,
    /// Credit.
    pub credit: u32,
}

/// CampaignProgress payload (the first 4 bytes of the main request's `info`).
#[zero(endian = "little")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CampaignProgressInfo {
    /// Aime id.
    pub aime_id: u32,
}

/// Start payload.
#[zero(endian = "little")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartInfo {
    /// Raw [`AimeCommand`].
    pub command: u32,
}

/// Led payload: the on/off byte of each channel (non-zero means on).
#[zero(endian = "little")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LedInfo {
    /// Red channel.
    pub red: u8,
    /// Green channel.
    pub green: u8,
    /// Blue channel.
    pub blue: u8,
}

/// LedStatus payload.
#[zero(endian = "little")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LedStatusInfo {
    /// Raw [`AimeLedStatus`].
    pub status: u32,
}

impl ClientAimeRequestRef<'_> {
    /// The request's task, `None` for an unknown value (the request fails).
    pub fn task_kind(&self) -> Option<AimeTask> {
        AimeTask::try_from(self.task()).ok()
    }

    /// Log payload.
    pub fn log_info(&self) -> LogInfo {
        let (log, _) = LogInfo::access_prefix(self.info()).expect("an integer payload accepts every bit pattern");
        log.copy_into()
    }

    /// CampaignProgress payload.
    pub fn campaign_progress_info(&self) -> CampaignProgressInfo {
        let (progress, _) =
            CampaignProgressInfo::access_prefix(self.info()).expect("an integer payload accepts every bit pattern");
        progress.copy_into()
    }
}

impl ClientAimeUnitRequestRef<'_> {
    /// The request's task, `None` for an unknown value (the request fails).
    pub fn task_kind(&self) -> Option<AimeUnitTask> {
        AimeUnitTask::try_from(self.task()).ok()
    }

    /// Start payload: the raw [`AimeCommand`].
    pub fn start_command(&self) -> u32 {
        let (start, _) = StartInfo::access_prefix(self.info()).expect("an integer payload accepts every bit pattern");
        start.command()
    }

    /// Led payload: raw on/off bytes for R, G, B (non-zero means on).
    pub fn led_bytes(&self) -> [u8; 3] {
        let (led, _) = LedInfo::access_prefix(self.info()).expect("an integer payload accepts every bit pattern");
        [led.red(), led.green(), led.blue()]
    }

    /// LedStatus payload: the raw [`AimeLedStatus`].
    pub fn led_status(&self) -> u32 {
        let (status, _) =
            LedStatusInfo::access_prefix(self.info()).expect("an integer payload accepts every bit pattern");
        status.status()
    }
}

const _: () = {
    assert!(AimeUnitResult::SCHEMA_SIZE == 0x334);
    assert!(AimeUnitError::SCHEMA_SIZE == 0x1A8);
    assert!(ServerAimeUnit::SCHEMA_SIZE == 0x4F8);
    assert!(AimeCampaignInfo::SCHEMA_SIZE == 0x130);
    assert!(AimeCampaignProgress::SCHEMA_SIZE == 0xC);
    assert!(ServerCampaign::SCHEMA_SIZE == 0x3C0);
    assert!(ServerFirm::SCHEMA_SIZE == 0x8);
    assert!(ServerAime::SCHEMA_SIZE == 0x17B8);
    assert!(ClientAimeRequest::SCHEMA_SIZE == 0x20);
    assert!(ClientAimeUnitRequest::SCHEMA_SIZE == 0xC);
    assert!(ClientAime::SCHEMA_SIZE == 0x50);
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::image::ServerImage;
    use crate::schema::text::fit_wide_str;

    /// `ServerData+0x3D0` → unit 0 (`+0x18`) → result (`+0x14`) → `hard_version` (`+0x2`).
    const UNIT0_HARD_VERSION: usize = 0x3D0 + 0x18 + 0x14 + 0x2;

    #[test]
    fn access_code_validity_is_per_byte_bcd() {
        assert!(AccessCode([0; 10]).is_valid_bcd());
        assert!(AccessCode([0x99; 10]).is_valid_bcd());
        assert!(!AccessCode::UNSET.is_valid_bcd());
        let mut code = [0x12; 10];
        code[9] = 0x1A; // low nibble above 9
        assert!(!AccessCode(code).is_valid_bcd());
        code[9] = 0xA0; // high nibble above 9
        assert!(!AccessCode(code).is_valid_bcd());
    }

    #[test]
    fn offline_id_kinds_follow_the_binary_constructors() {
        let code = AccessCode([1, 2, 3, 4, 5, 6, 7, 8, 9, 0x10]);
        let kind1 = AimeOfflineId::from_access_code(&code);
        assert_eq!(kind1.0, [1, 1, 2, 3, 4, 5, 6, 7, 8, 9, 0x10, 0, 0, 0, 0, 0, 0]);
        let kind2 = AimeOfflineId::from_felica(0x0102_0304_0506_0708, 0x1112_1314_1516_1718);
        assert_eq!(kind2.0, [2, 8, 7, 6, 5, 4, 3, 2, 1, 0x18, 0x17, 0x16, 0x15, 0x14, 0x13, 0x12, 0x11]);
    }

    #[test]
    fn result_reset_restores_the_unknown_markers_only() {
        let mut image = ServerImage::new();
        {
            let mut aime = image.aime_mut();
            let mut units = aime.units_mut();
            let mut unit = units.get_mut(0).expect("four units");
            let hard_version = fit_wide_str("HW", 64);
            let auth_key = fit_wide_str("key", 257);
            let patch = ServerAimeUnitPatch {
                busy: Some(true),
                led_status: Some(AimeLedStatus::Warning.into()),
                result: Some(AimeUnitResultPatch {
                    valid: Some(true),
                    reader_detected: Some(true),
                    hard_version: Some(&hard_version),
                    offline_id: Some(&AimeOfflineId::from_felica(1, 2).0),
                    mobile: Some(true),
                    aime_id: Some(7),
                    sega_id_registered: Some(true),
                    sega_id_auth_key: Some(&auth_key),
                    related_aime_id_count: Some(2),
                    related_aime_ids: Some([5, 6]),
                    ..AimeUnitResultPatch::default()
                }),
                ..ServerAimeUnitPatch::default()
            };
            unit.copy_from(&patch).expect("scalars, byte arrays and fitted strings");

            unit.result_mut().reset();

            let result = unit.result();
            assert!(!result.valid() && !result.reader_detected() && !result.mobile());
            assert!(result.hard_version().is_empty() && result.sega_id_auth_key().is_empty());
            assert_eq!(AimeOfflineId(*result.offline_id()), AimeOfflineId::NONE);
            assert_eq!(AccessCode(*result.access_code()), AccessCode::UNSET);
            assert_eq!(result.aime_id(), AIME_ID_INVALID);
            assert!(!result.sega_id_registered());
            assert_eq!(result.related_aime_id_count(), 0);
            assert_eq!(result.related_aime_ids().copy_into(), [AIME_ID_INVALID; 2]);
            // Not part of the reset image.
            assert!(unit.busy());
            assert_eq!(unit.led_status(), AimeLedStatus::Warning);
        }
        // `wcsncpy_s` writes only the terminator: the tail of the old string stays on the wire.
        let tail = &image.bytes()[UNIT0_HARD_VERSION + 2..UNIT0_HARD_VERSION + 4];
        assert_eq!(tail, u16::from(b'W').to_le_bytes());
    }
}
