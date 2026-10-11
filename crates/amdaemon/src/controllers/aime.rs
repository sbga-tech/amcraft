//! `amdaemon::server::AimeController` (vtable `0x1405C0138`, `App+0xE28`): the Aime card reader units, the
//! play-log / campaign-progress requests and the Aime server status.
//!
//! Evidence: `spec/controllers/aime.md` (claims `aime.*`), `spec/controllers/lifecycle.md` §4 / §6.
//!
//! The controller owns one main [`RequestExecutor`] (play log, campaign progress) and four unit executors
//! (scan, LED, ...). libam is the [`AimePort`]; its listener callbacks arrive as [`AimeEvent`]s through
//! [`AimePort::take_events`] and are applied in [`Controller::on_libam_events`], i.e. between libam's
//! `execute` and the controllers' `preExecute`, exactly where the original callbacks fire.

use std::fmt::{self, Write as _};

use widestring::U16CStr;

use crate::executor::{RequestExecutor, RequestState};
use crate::manager::Manager;
use crate::schema::aime::{
    AIME_ID_INVALID, AccessCode, AimeCampaignInfoPatch, AimeCampaignProgressPatch, AimeCommand, AimeConfirm,
    AimeErrorCategory, AimeErrorId, AimeLedStatus, AimeLogStatus, AimeOfflineId, AimeTask, AimeUnitErrorPatch,
    AimeUnitResultMut, AimeUnitResultPatch, AimeUnitTask, ClientAimeRequestRef, ClientAimeUnitRequestRef,
    ServerAimeMut, ServerAimeUnitMut, ServerAimeUnitPatch, ServerFirmPatch,
};
use crate::schema::text::fit_wide;
use libam_api::aime::{
    AimeEvent, AimePort, CardInfo, FinishedStatus, FinishedTask, LedPattern, LibamAimeId, PlayLogStatus,
    UnitTaskFinished,
};
use libam_api::result::{AmError, AmResult};
use libam_api::time::{DateTime, Tm};

use crate::controllers::controller::{Controller, ControllerId, Ctx, commit_check};
use crate::controllers::error::publish_fatal_error;
use libam_api::config::AppConfig;

/// Number of card reader units the controller owns executors for (`i < 0x30 step 12` in `0x140015000`).
pub const UNIT_COUNT: usize = 4;

/// Why a unit index below [`UNIT_COUNT`] always selects an element of the four-unit blocks.
const UNIT_SLOT: &str = "the block holds one entry per unit below UNIT_COUNT";

// ---------------------------------------------------------------------------------------------------------
// Converters (anonymous namespace of the original `AimeController`)
// ---------------------------------------------------------------------------------------------------------

/// libam `ErrorCode` (index) → [`AimeErrorId`] raw value (`0x140015C50`). Codes 0 and 1 both mean "no error";
/// 22 and 23 share an id.
const ERROR_ID_BY_CODE: [u32; 25] =
    [0, 0, 1, 2, 4, 5, 6, 7, 8, 9, 10, 11, 13, 14, 16, 23, 24, 25, 26, 27, 28, 29, 30, 30, 31];

/// `convert(TaskFinished::ErrorCode)` (`0x140015C50`); `None` for codes above 24 (those become id 0).
pub fn error_id_from_code(code: u32) -> Option<AimeErrorId> {
    let raw = *ERROR_ID_BY_CODE.get(usize::try_from(code).ok()?)?;
    AimeErrorId::new(raw)
}

/// `convert(NeedAcceptation::Question)` (`0x140015E20`); `None` for questions other than 1 and 2.
pub fn confirm_from_question(question: u32) -> Option<AimeConfirm> {
    match question {
        1 => Some(AimeConfirm::Question1),
        2 => Some(AimeConfirm::Question2),
        _ => None,
    }
}

/// `convert(LedLightPattern)` (`0x140015EB0`); `None` for patterns above 4.
pub fn led_status_from_pattern(pattern: u32) -> Option<AimeLedStatus> {
    AimeLedStatus::try_from(pattern).ok()
}

/// Request side `AimeLedStatus → LedLightPattern` (`0x140015FB0`): `1..=4` are kept, everything else
/// (including 0) silently becomes `None`.
pub fn led_pattern_for_request(raw: u32) -> LedPattern {
    match AimeLedStatus::try_from(raw) {
        Ok(AimeLedStatus::Scanning) => LedPattern::Scanning,
        Ok(AimeLedStatus::Success) => LedPattern::Success,
        Ok(AimeLedStatus::Warning) => LedPattern::Warning,
        Ok(AimeLedStatus::Error) => LedPattern::Error,
        _ => LedPattern::None,
    }
}

// ---------------------------------------------------------------------------------------------------------
// Date helpers (`util::DateTime` tm adapters `0x14005D580`, `0x1400159D0`, `0x14005EFE0`)
// ---------------------------------------------------------------------------------------------------------

/// `0x1400159D0`: `DateTime` → libam `struct tm` (no time-zone handling).
fn tm_from_datetime(time: DateTime) -> Tm {
    let civil = time.civil();
    Tm {
        sec: civil.second as i32,
        min: civil.minute as i32,
        hour: civil.hour as i32,
        mday: civil.day as i32,
        mon: civil.month as i32 - 1,
        year: civil.year as i32 - 1900,
        wday: 0,
        yday: 0,
        isdst: 0,
    }
}

/// `DateTime` formatter `0x14005EFE0` with its default format `%t` = `%x %X` =
/// `%Y/%02M/%02D %02h:%02m:%02s`.
fn format_datetime(time: DateTime) -> String {
    let civil = time.civil();
    format!(
        "{}/{:02}/{:02} {:02}:{:02}:{:02}",
        civil.year, civil.month, civil.day, civil.hour, civil.minute, civil.second
    )
}

// ---------------------------------------------------------------------------------------------------------
// Request traces (`0x140016240`, `0x140016000`)
// ---------------------------------------------------------------------------------------------------------

/// Text of the main request trace (`0x140016240`). The original streams the name tables' null pointer for an
/// out-of-range enum value, which faults; here the text simply ends where the name would have followed.
fn main_request_trace(request: &ClientAimeRequestRef<'_>) -> String {
    let mut text = String::from("Aime - task: ");
    let Some(task) = request.task_kind() else { return text };
    text.push_str(task.name());
    match task {
        AimeTask::Log => {
            let log = request.log_info();
            let _ = write!(text, ", AimeID: {}, status: ", log.aime_id);
            let Ok(status) = AimeLogStatus::try_from(log.status) else { return text };
            text.push_str(status.name());
            let _ = write!(text, ", credit: {}, time: {}", log.credit, format_datetime(DateTime(request.time())));
        }
        AimeTask::CampaignProgress => {
            let _ = write!(text, ", AimeID: {}", request.campaign_progress_info().aime_id);
        }
    }
    text
}

/// Text of the unit request trace (`0x140016000`); same out-of-range rule as [`main_request_trace`].
fn unit_request_trace(unit: usize, request: &ClientAimeUnitRequestRef<'_>) -> String {
    let mut text = format!("Aime - unit[{unit}] task: ");
    let Some(task) = request.task_kind() else { return text };
    text.push_str(task.name());
    match task {
        AimeUnitTask::Start => {
            text.push_str(", command: ");
            if let Ok(command) = AimeCommand::try_from(request.start_command()) {
                text.push_str(command.name());
            }
        }
        AimeUnitTask::Led => {
            let [red, green, blue] = request.led_bytes();
            let _ = write!(text, ", color(R, G, B): ({red}, {green}, {blue})");
        }
        AimeUnitTask::LedStatus => {
            text.push_str(", status: ");
            if let Ok(status) = AimeLedStatus::try_from(request.led_status()) {
                text.push_str(status.name());
            }
        }
        AimeUnitTask::Cancel | AimeUnitTask::Confirm => {}
    }
    text
}

/// Unit index of a libam callback; indices `>= 4` select no executor and drop the event.
fn unit_slot(unit: u64) -> Option<usize> {
    usize::try_from(unit).ok().filter(|&slot| slot < UNIT_COUNT)
}

/// `am::Result` code 5 of an unknown task / command ("Request unknown task." / "Requested unknown command.").
fn unknown_request() -> AmError {
    AmError::code(AmError::UNKNOWN_TASK)
}

/// Acceptor result: `1` when libam took the request, `3` otherwise.
fn accepted_or_failed(result: &AmResult) -> RequestState {
    if result.is_ok() { RequestState::Pending } else { RequestState::Failed }
}

// ---------------------------------------------------------------------------------------------------------
// Executors
// ---------------------------------------------------------------------------------------------------------

/// `AimeRequestExecutor` (`ctrl+0x70`): play log / campaign progress.
#[derive(Debug)]
struct MainExecutor {
    exec: RequestExecutor,
    /// `+0x70`: campaign query state (0 initially, 1 running, 2 / 3 set by the global TaskFinished callback).
    campaign_state: RequestState,
}

/// `AimeUnitRequestExecutor::fwState` (`+0x78`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FirmwareState {
    Idle,
    Updating,
    Done,
}

/// `AimeUnitRequestExecutor` (heap object, 0x80 bytes): one card reader unit.
#[derive(Debug)]
struct UnitExecutor {
    /// `+0x00` unit index.
    index: usize,
    exec: RequestExecutor,
    /// `+0x78`, set by Task 7.
    fw_state: FirmwareState,
    /// `+0x7C`, 0..=100.
    fw_progress: f32,
}

impl UnitExecutor {
    fn new(index: usize) -> Self {
        Self { index, exec: RequestExecutor::default(), fw_state: FirmwareState::Idle, fw_progress: 0.0 }
    }
}

/// `AimeRequestExecutor::accept` (`0x1400136F0`).
fn accept_main(
    port: &mut dyn AimePort,
    campaign_state: &mut RequestState,
    manager: &mut Manager,
    request: &ClientAimeRequestRef<'_>,
    aime: &mut ServerAimeMut<'_>,
) -> RequestState {
    if manager.debug_dump() {
        let text = main_request_trace(request);
        tracing::info!("{text}");
    }
    if !port.facade_available() {
        tracing::warn!("libam is not ready");
        return RequestState::Failed;
    }
    let result = match request.task_kind() {
        Some(AimeTask::Log) => {
            let log = request.log_info();
            let aime_id = LibamAimeId { id: log.aime_id, flag: false };
            let time = tm_from_datetime(DateTime(request.time()));
            port.send_play_log(PlayLogStatus::from_request(log.status), aime_id, &time, log.credit)
        }
        Some(AimeTask::CampaignProgress) => {
            let aime_id = LibamAimeId { id: request.campaign_progress_info().aime_id, flag: false };
            let result = port.query_campaign_progress(aime_id);
            clear_campaign_progress(aime);
            *campaign_state = RequestState::Pending;
            result
        }
        None => Err(unknown_request()),
    };
    accepted_or_failed(&result)
}

/// `AimeUnitRequestExecutor::accept` (`0x1400140B0`).
fn accept_unit(
    port: &mut dyn AimePort,
    manager: &mut Manager,
    unit: usize,
    request: &ClientAimeUnitRequestRef<'_>,
    response: &mut ServerAimeUnitMut<'_>,
) -> RequestState {
    if manager.debug_dump() {
        let text = unit_request_trace(unit, request);
        tracing::info!("{text}");
    }
    if !port.facade_available() {
        tracing::warn!("libam is not ready");
        return RequestState::Failed;
    }
    let result = match request.task_kind() {
        Some(AimeUnitTask::Start) => {
            let command = request.start_command();
            let result = match AimeCommand::try_from(command) {
                Ok(AimeCommand::ScanOffline) => port.start_scan_offline(unit),
                Ok(AimeCommand::Scan) => port.start_scan(unit),
                Err(_) => Err(unknown_request()),
            };
            // Written after the libam call and even when it failed.
            let patch = ServerAimeUnitPatch {
                busy: Some(result.is_ok()),
                busy_command: Some(command),
                confirm: Some(AimeConfirm::None.into()),
                ..ServerAimeUnitPatch::default()
            };
            response.copy_from(&patch).expect("scalar fields and a closed enum value");
            // Error id and category back to 0, error data zeroed.
            response.error_mut().clear();
            response.result_mut().reset();
            result
        }
        Some(AimeUnitTask::Cancel) => {
            let result = port.cancel(unit);
            clear_confirm(response);
            result
        }
        Some(AimeUnitTask::Confirm) => {
            let result = port.confirm(unit);
            clear_confirm(response);
            result
        }
        Some(AimeUnitTask::Led) => {
            let [red, green, blue] = request.led_bytes().map(|on| if on != 0 { 0xFF } else { 0 });
            port.set_led(unit, red, green, blue)
        }
        Some(AimeUnitTask::LedStatus) => port.set_led_pattern(unit, led_pattern_for_request(request.led_status())),
        None => Err(unknown_request()),
    };
    accepted_or_failed(&result)
}

/// A new campaign-progress query wipes the previous result (`0x1400136F0`): the count and all three entries
/// go back to zero.
fn clear_campaign_progress(aime: &mut ServerAimeMut<'_>) {
    let mut campaign = aime.campaign_mut();
    campaign.progress_count_mut().set(0).expect("a scalar accepts every value");
    let mut progresses = campaign.progresses_mut();
    for index in 0..3 {
        progresses.get_mut(index).expect("the block holds three progress entries").clear();
    }
}

/// Cancel and Confirm answer the pending question: `confirm` goes back to `None`.
fn clear_confirm(unit: &mut ServerAimeUnitMut<'_>) {
    unit.confirm_mut().set(AimeConfirm::None).expect("a closed enum value");
}

/// Loaders of one unit (`AimeUnitRequestExecutor::load*`): each returns early when the libam facade is unusable.
struct UnitLibam<'a> {
    port: &'a mut dyn AimePort,
    unit: usize,
}

impl UnitLibam<'_> {
    /// `loadReaderData` (`0x1400144C0`).
    fn load_reader_data(&mut self, result: &mut AimeUnitResultMut<'_>) -> bool {
        if !self.port.facade_available() {
            tracing::warn!("libam version does not match");
            return false;
        }
        let Some(info) = self.port.reader_info(self.unit) else { return false };
        let hard_capacity = result.hard_version_mut().capacity();
        let firm_capacity = result.firm_version_mut().capacity();
        let hard_version = fit_wide(&info.hard_version, hard_capacity);
        let firm_version = fit_wide(&info.firm_version, firm_capacity);
        let patch = AimeUnitResultPatch {
            reader_detected: Some(info.detected),
            hard_version: Some(&hard_version),
            firm_version: Some(&firm_version),
            ..AimeUnitResultPatch::default()
        };
        result.copy_from(&patch).expect("scalars and fitted strings");
        true
    }

    /// `loadAccessCode` (`0x140014820`) without the store: an invalid BCD code reads as `00×10`.
    fn read_access_code(&mut self) -> Option<AccessCode> {
        if !self.port.facade_available() {
            tracing::warn!("libam version does not match");
            return None;
        }
        let code = AccessCode(self.port.access_code(self.unit)?.0);
        Some(if code.is_valid_bcd() { code } else { AccessCode([0; 10]) })
    }

    /// `loadAccessCode` (`0x140014820`).
    fn load_access_code(&mut self, result: &mut AimeUnitResultMut<'_>) {
        if let Some(code) = self.read_access_code() {
            result.access_code_mut().set(&code.0).expect("the code has the field's ten bytes");
        }
    }

    /// `loadOfflineId` (`0x140014630`): the access code (read into a temporary) wins over FeliCa IDm / PMm.
    fn load_offline_id(&mut self, result: &mut AimeUnitResultMut<'_>) {
        if !self.port.facade_available() {
            tracing::warn!("libam version does not match");
            return;
        }
        let Some(card) = self.port.card_info(self.unit) else { return };
        let offline_id = if let Some(code) = self.read_access_code() {
            AimeOfflineId::from_access_code(&code)
        } else if card.card_type == CardInfo::FELICA && card.idm != 0 && card.pmm != 0 {
            AimeOfflineId::from_felica(card.idm, card.pmm)
        } else {
            AimeOfflineId::NONE
        };
        let patch = AimeUnitResultPatch {
            mobile: Some(card.mobile),
            offline_id: Some(&offline_id.0),
            ..AimeUnitResultPatch::default()
        };
        result.copy_from(&patch).expect("a flag and seventeen bytes");
    }

    /// `loadAimeId` (`0x140014980`).
    fn load_aime_id(&mut self, result: &mut AimeUnitResultMut<'_>) {
        if !self.port.facade_available() {
            tracing::warn!("libam version does not match");
            return;
        }
        if let Some(id) = self.port.aime_id(self.unit) {
            let patch = AimeUnitResultPatch {
                aime_id: Some(id.id),
                sega_id_registered: Some(id.flag),
                ..AimeUnitResultPatch::default()
            };
            result.copy_from(&patch).expect("scalar fields accept every value");
        }
    }

    /// `loadInfoRelatedSegaId` (`0x140014AB0`): libam list slot 0 is not copied; slots `1..count` feed
    /// `relatedAimeIds[0..]`, a failing slot stores 0 without counting.
    fn load_info_related_sega_id(&mut self, result: &mut AimeUnitResultMut<'_>) {
        if !self.port.facade_available() {
            tracing::warn!("libam version does not match");
            return;
        }
        let Some(info) = self.port.info_related_sega_id(self.unit) else { return };
        let auth_key_capacity = result.sega_id_auth_key_mut().capacity();
        let auth_key = fit_wide(&info.auth_key, auth_key_capacity);
        let mut related_aime_ids = result.related_aime_ids().copy_into();
        let mut related_aime_id_count = 0;
        let wanted = usize::try_from(info.count).unwrap_or(usize::MAX).saturating_sub(1);
        for (index, slot) in related_aime_ids.iter_mut().enumerate().take(wanted) {
            match info.items.get(index + 1).copied().flatten() {
                Some(id) => {
                    *slot = id;
                    related_aime_id_count += 1;
                }
                None => *slot = 0,
            }
        }
        let patch = AimeUnitResultPatch {
            sega_id_auth_key: Some(&auth_key),
            related_aime_id_count: Some(related_aime_id_count),
            related_aime_ids: Some(related_aime_ids),
            ..AimeUnitResultPatch::default()
        };
        result.copy_from(&patch).expect("a fitted string, a count and two ids");
    }
}

/// `convert(ErrorCode)`: unlisted codes map to [`AimeErrorId::NONE`].
fn convert_error_code(code: u32) -> AimeErrorId {
    error_id_from_code(code).unwrap_or_else(|| {
        tracing::warn!("libam version does not match");
        AimeErrorId::NONE
    })
}

impl UnitExecutor {
    /// `AimeUnitRequestExecutor::setResult` (`0x140013C70`), driven by the unit `TaskFinished` callback.
    fn set_result(&mut self, port: &mut dyn AimePort, unit: &mut ServerAimeUnitMut<'_>, event: &UnitTaskFinished) {
        use FinishedStatus::{Cancelled, Failed, Queued, Started, Succeeded};

        let mut libam = UnitLibam { port, unit: self.index };
        match (event.task, event.status) {
            (FinishedTask::DetectCardReader, Queued) => {
                let empty = <&U16CStr>::default();
                let patch = AimeUnitResultPatch {
                    valid: Some(false),
                    reader_detected: Some(false),
                    hard_version: Some(empty),
                    firm_version: Some(empty),
                    ..AimeUnitResultPatch::default()
                };
                unit.result_mut().copy_from(&patch).expect("flags and empty strings");
            }
            (FinishedTask::DetectCardReader, Succeeded | Failed) => {
                let mut result = unit.result_mut();
                if libam.load_reader_data(&mut result) {
                    result.valid_mut().set(true).expect("a flag accepts every value");
                }
            }
            (FinishedTask::ScanNfcChip, Queued | Cancelled) => {
                let patch = AimeUnitResultPatch {
                    offline_id: Some(&AimeOfflineId::NONE.0),
                    access_code: Some(&AccessCode::UNSET.0),
                    ..AimeUnitResultPatch::default()
                };
                unit.result_mut().copy_from(&patch).expect("byte arrays of the field sizes");
            }
            (FinishedTask::ScanNfcChip, Succeeded | Failed) => {
                let mut result = unit.result_mut();
                libam.load_reader_data(&mut result);
                libam.load_offline_id(&mut result);
                libam.load_access_code(&mut result);
            }
            (FinishedTask::CheckFelicaRegistered | FinishedTask::RegisterFelica, Queued | Cancelled) => {
                unit.result_mut().access_code_mut().set(&AccessCode::UNSET.0).expect("ten bytes");
            }
            (FinishedTask::CheckFelicaRegistered | FinishedTask::RegisterFelica, Succeeded | Failed) => {
                libam.load_access_code(&mut unit.result_mut());
            }
            (FinishedTask::CheckAimeIdRegistered | FinishedTask::RegisterAimeId, Queued | Cancelled) => {
                let patch = AimeUnitResultPatch {
                    sega_id_registered: Some(false),
                    aime_id: Some(AIME_ID_INVALID),
                    ..AimeUnitResultPatch::default()
                };
                unit.result_mut().copy_from(&patch).expect("scalar fields accept every value");
            }
            (FinishedTask::CheckAimeIdRegistered | FinishedTask::RegisterAimeId, Succeeded | Failed) => {
                let mut result = unit.result_mut();
                if !AccessCode(*result.access_code()).is_valid_bcd() {
                    libam.load_access_code(&mut result);
                }
                libam.load_aime_id(&mut result);
                libam.load_info_related_sega_id(&mut result);
            }
            (FinishedTask::UpdateFirmware, Queued | Cancelled) => {
                self.fw_state = FirmwareState::Idle;
                self.fw_progress = 0.0;
            }
            (FinishedTask::UpdateFirmware, Started) => self.fw_state = FirmwareState::Updating,
            (FinishedTask::UpdateFirmware, Succeeded) => {
                self.fw_state = FirmwareState::Done;
                self.fw_progress = 100.0;
            }
            (FinishedTask::UpdateFirmware, Failed) => {
                self.fw_state = FirmwareState::Done;
                unit.result_mut().valid_mut().set(true).expect("a flag accepts every value");
            }
            (FinishedTask::SequenceFinished, _) => {
                let id = convert_error_code(event.error_code);
                let category = id.category();
                let mut error = unit.error_mut();
                let patch = AimeUnitErrorPatch {
                    id: Some(u32::from(id)),
                    category: Some(category.into()),
                    ..AimeUnitErrorPatch::default()
                };
                error.copy_from(&patch).expect("an integer and a closed enum value");
                publish_fatal_error(&event.fatal, &mut error.data_mut());
                if category == AimeErrorCategory::Category1 {
                    if libam.port.facade_available() {
                        // The libam result is not inspected.
                        let _ = libam.port.set_led_pattern(self.index, LedPattern::Error);
                    } else {
                        tracing::warn!("libam is not ready");
                    }
                }
                unit.busy_mut().set(false).expect("a flag accepts every value");
            }
            // Unit path ignores Task 8 and Task 0; the remaining combinations carry no behaviour.
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------------------------------------
// Controller
// ---------------------------------------------------------------------------------------------------------

/// `amdaemon::server::AimeController`.
pub struct AimeController {
    port: Box<dyn AimePort>,
    main: MainExecutor,
    units: [UnitExecutor; UNIT_COUNT],
}

impl fmt::Debug for AimeController {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AimeController").field("main", &self.main).field("units", &self.units).finish_non_exhaustive()
    }
}

impl AimeController {
    /// Constructs the controller (`0x140015000`): one main executor and four unit executors.
    pub fn new(port: Box<dyn AimePort>) -> Self {
        Self {
            port,
            main: MainExecutor { exec: RequestExecutor::default(), campaign_state: RequestState::Idle },
            units: std::array::from_fn(UnitExecutor::new),
        }
    }

    /// `AimeRequestExecutor::loadCampaignProgress` (`0x140013990`).
    fn load_campaign_progress(&self, aime: &mut ServerAimeMut<'_>) {
        if !self.port.facade_available() {
            tracing::warn!("libam version does not match");
            return;
        }
        let Some(list) = self.port.campaign_progress_list() else { return };
        let mut campaign = aime.campaign_mut();
        campaign.progress_count_mut().set(list.count as u16).expect("a scalar accepts every value");
        let count = usize::try_from(list.count).unwrap_or(usize::MAX);
        let mut progresses = campaign.progresses_mut();
        for (index, entry) in list.entries.iter().enumerate().take(count) {
            if entry.id != 0 {
                let patch = AimeCampaignProgressPatch {
                    campaign_id: Some(entry.id),
                    flag: Some(entry.flag),
                    value: Some(entry.value),
                };
                let mut slot = progresses.get_mut(index).expect("the list and the block both hold three entries");
                slot.copy_from(&patch).expect("scalar fields accept every value");
            }
        }
    }

    /// `loadCampaignInfo` (`0x140015720`): slots that are not copied keep their previous contents.
    fn load_campaign_info(&self, aime: &mut ServerAimeMut<'_>) {
        if !self.port.facade_available() {
            tracing::warn!("libam version does not match");
            return;
        }
        let Some(list) = self.port.campaign_info_list() else { return };
        let mut campaign = aime.campaign_mut();
        campaign.info_count_mut().set(list.count as u16).expect("a scalar accepts every value");
        let count = usize::try_from(list.count).unwrap_or(usize::MAX);
        let mut infos = campaign.infos_mut();
        for (index, entry) in list.entries.iter().enumerate().take(count) {
            if entry.id == 0 {
                continue;
            }
            let mut info = infos.get_mut(index).expect("the list and the block both hold three entries");
            let capacity = info.name_mut().capacity();
            let name = fit_wide(&entry.name, capacity);
            let mut times = info.times().copy_into();
            for (slot, tm) in times.iter_mut().zip(&entry.times) {
                if let Some(time) = tm.to_date_time() {
                    *slot = time.0;
                }
            }
            let patch = AimeCampaignInfoPatch { campaign_id: Some(entry.id), name: Some(&name), times: Some(times) };
            info.copy_from(&patch).expect("an id, a fitted name and five times");
        }
    }

    /// `loadAlive` (`0x140015620`).
    fn load_alive(&self, aime: &mut ServerAimeMut<'_>) {
        if self.port.facade_available() {
            aime.db_alive_mut().set(self.port.is_db_alive()).expect("a flag accepts every value");
        }
    }

    /// Firmware aggregate over the first `unit_count` units (`0x140015450`, `aime.override.update_firm`).
    fn update_firmware_aggregate(&self, aime: &mut ServerAimeMut<'_>) {
        let count = aime.unit_count() as usize;
        let (mut all_idle, mut all_done, mut sum) = (true, true, 0.0f32);
        for unit in self.units.iter().take(count) {
            sum += unit.fw_progress;
            all_idle &= unit.fw_state == FirmwareState::Idle;
            all_done &= unit.fw_state == FirmwareState::Done;
        }
        let patch = ServerFirmPatch {
            updating: Some(count != 0 && !all_idle && !all_done),
            update_progress: Some(if count != 0 { sum / count as f32 } else { 0.0 }),
        };
        aime.firm_mut().copy_from(&patch).expect("scalar fields accept every value");
    }

    /// Applies one libam callback.
    fn apply_event(&mut self, event: AimeEvent, ctx: &mut Ctx<'_>) {
        let mut aime = ctx.server.aime_mut();
        match event {
            AimeEvent::UnitTaskFinished(finished) => {
                let Some(slot) = unit_slot(finished.unit) else { return };
                let mut units = aime.units_mut();
                let mut unit = units.get_mut(slot).expect(UNIT_SLOT);
                self.units[slot].set_result(self.port.as_mut(), &mut unit, &finished);
            }
            AimeEvent::TaskFinished { task, status } => match task {
                FinishedTask::QueryCampaignProgress if status == FinishedStatus::Succeeded => {
                    self.load_campaign_progress(&mut aime);
                }
                FinishedTask::SequenceFinished => {
                    self.main.campaign_state = if status == FinishedStatus::Succeeded {
                        RequestState::Succeeded
                    } else {
                        RequestState::Failed
                    };
                }
                _ => {}
            },
            AimeEvent::NeedAcceptation { unit, question } => {
                let Some(slot) = unit_slot(unit) else { return };
                let confirm = confirm_from_question(question).unwrap_or_else(|| {
                    tracing::warn!("libam version does not match");
                    AimeConfirm::None
                });
                let mut units = aime.units_mut();
                units.get_mut(slot).expect(UNIT_SLOT).confirm_mut().set(confirm).expect("a closed enum value");
            }
            AimeEvent::FirmwareUpdatingProgress { unit, progress } => {
                if let Some(slot) = unit_slot(unit) {
                    self.units[slot].fw_progress = progress;
                }
            }
            AimeEvent::ChangeLedLightPattern { unit, pattern } => {
                let Some(slot) = unit_slot(unit) else { return };
                let status = led_status_from_pattern(pattern).unwrap_or_else(|| {
                    tracing::warn!("libam version does not match");
                    AimeLedStatus::None
                });
                let mut units = aime.units_mut();
                units.get_mut(slot).expect(UNIT_SLOT).led_status_mut().set(status).expect("a closed enum value");
            }
        }
    }
}

impl Controller for AimeController {
    fn id(&self) -> ControllerId {
        ControllerId::Aime
    }

    /// vf3 (`0x140015270`): a main or unit request in flight, or a firmware update in progress.
    fn is_busy(&self, ctx: &Ctx<'_>) -> bool {
        let server = ctx.server.aime();
        let client = ctx.client.aime();
        self.main.exec.is_busy(client.request().generation(), server.response().generation())
            || server.firm().updating()
            || self
                .units
                .iter()
                .zip(client.units().iter())
                .zip(server.units().iter())
                .any(|((unit, request), data)| unit.exec.is_busy(request.generation(), data.response().generation()))
    }

    /// vf5 (`0x1400152F0`): only `dbAlive` and `unitCount` are written.
    fn clear_server_data(&mut self, ctx: &mut Ctx<'_>) {
        let mut aime = ctx.server.aime_mut();
        aime.db_alive_mut().set(false).expect("a flag accepts every value");
        if !self.port.facade_available() {
            tracing::warn!("libam is not ready");
            aime.unit_count_mut().set(0).expect("a scalar accepts every value");
            return;
        }
        let mut count = self.port.unit_count();
        if count > UNIT_COUNT as u64 {
            tracing::error!("too many units");
            count = UNIT_COUNT as u64;
        }
        aime.unit_count_mut().set(count as u32).expect("a scalar accepts every value");
    }

    /// vf6 (`0x140015450`): firmware aggregate, `dbAlive`, campaign infos, then `checkResult` of the main
    /// executor (`0x1405C0180`) and of all four units (`0x1405C0460`).
    fn update_server_data(&mut self, ctx: &mut Ctx<'_>) {
        let client = ctx.client.aime();
        let mut server = ctx.server.aime_mut();

        self.update_firmware_aggregate(&mut server);
        self.load_alive(&mut server);
        self.load_campaign_info(&mut server);

        let campaign_state = self.main.campaign_state;
        let request = client.request();
        let outcome = self.main.exec.check_result(request.generation(), server.response().generation(), || {
            if request.task_kind() == Some(AimeTask::CampaignProgress) {
                campaign_state
            } else {
                RequestState::Succeeded
            }
        });
        commit_check(outcome, &mut server.response_mut());

        let requests = client.units();
        let mut units = server.units_mut();
        for (index, unit) in self.units.iter_mut().enumerate() {
            let request = requests.get(index).expect(UNIT_SLOT);
            let mut data = units.get_mut(index).expect(UNIT_SLOT);
            let outcome =
                unit.exec.check_result(request.generation(), data.response().generation(), || RequestState::Succeeded);
            commit_check(outcome, &mut data.response_mut());
        }
    }

    /// vf7 (`0x140015550`): the main executor first, then units 0..3.
    fn accept_requests(&mut self, ctx: &mut Ctx<'_>) {
        let Self { port, main, units } = self;
        let client = ctx.client.aime();
        let mut server = ctx.server.aime_mut();
        let manager = &mut *ctx.manager;

        let request = client.request();
        main.exec.accept(request.generation(), server.response().generation(), || {
            accept_main(port.as_mut(), &mut main.campaign_state, manager, &request, &mut server)
        });
        let requests = client.units();
        let mut data_units = server.units_mut();
        for unit in units.iter_mut() {
            let index = unit.index;
            let request = requests.get(index).expect(UNIT_SLOT);
            let mut data = data_units.get_mut(index).expect(UNIT_SLOT);
            unit.exec.accept(request.generation(), data.response().generation(), || {
                accept_unit(port.as_mut(), manager, index, &request, &mut data)
            });
        }
    }

    fn on_libam_events(&mut self, ctx: &mut Ctx<'_>) {
        for event in self.port.take_events() {
            self.apply_event(event, ctx);
        }
    }

    /// AppConfig hook `0x140016510`: hands the four listeners to libam.
    fn on_app_config(&mut self, config: &mut AppConfig, _ctx: &mut Ctx<'_>) {
        config.aime.install_listeners();
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::controllers::controller::ControllerSlot;
    use crate::controllers::controller::testing::{Harness, civil};
    use crate::schema::aime::{ClientAimeRequestPatch, ClientAimeUnitRequestPatch, ServerAimeRef, ServerAimeUnitRef};
    use crate::schema::image::ServerImage;
    use crate::schema::shared::ErrorData;
    use crate::schema::text::fit_wide_str;
    use libam_api::aime::{
        AccessCode as LibamAccessCode, CAMPAIGN_NAME_UNITS, CampaignInfoEntry, CampaignInfoList, CampaignProgressEntry,
        CampaignProgressList, ReaderInfo, RelatedSegaId,
    };
    use libam_api::error::FatalError;

    /// One libam call as recorded by the test port.
    #[derive(Debug, Clone, PartialEq)]
    enum Call {
        StartScanOffline(usize),
        StartScan(usize),
        Confirm(usize),
        Cancel(usize),
        SetLed(usize, u8, u8, u8),
        SetLedPattern(usize, LedPattern),
        SendPlayLog { status: PlayLogStatus, aime_id: u32, time: Tm, credit: u32 },
        QueryCampaignProgress(u32),
    }

    /// Scriptable libam.
    #[derive(Default)]
    struct FakeLibam {
        facade: bool,
        unit_count: u64,
        db_alive: bool,
        command_error: Option<u32>,
        reader: Option<ReaderInfo>,
        card: Option<CardInfo>,
        access_code: Option<LibamAccessCode>,
        aime_id: Option<LibamAimeId>,
        sega: Option<RelatedSegaId>,
        events: Vec<AimeEvent>,
        calls: Vec<Call>,
    }

    impl FakeLibam {
        fn record(&mut self, call: Call) -> AmResult {
            self.calls.push(call);
            self.command_error.map_or(Ok(()), |code| Err(AmError::code(code)))
        }
    }

    /// The port handed to the controller: scripted state is shared with the test through `state`; the two
    /// campaign lists are returned by reference, so they are owned by the port itself.
    struct SharedLibam {
        state: Rc<RefCell<FakeLibam>>,
        info_list: Option<CampaignInfoList>,
        progress_list: Option<CampaignProgressList>,
    }

    impl AimePort for SharedLibam {
        fn facade_available(&self) -> bool {
            self.state.borrow().facade
        }
        fn unit_count(&self) -> u64 {
            self.state.borrow().unit_count
        }
        fn is_db_alive(&self) -> bool {
            self.state.borrow().db_alive
        }
        fn start_scan_offline(&mut self, unit: usize) -> AmResult {
            self.state.borrow_mut().record(Call::StartScanOffline(unit))
        }
        fn start_scan(&mut self, unit: usize) -> AmResult {
            self.state.borrow_mut().record(Call::StartScan(unit))
        }
        fn confirm(&mut self, unit: usize) -> AmResult {
            self.state.borrow_mut().record(Call::Confirm(unit))
        }
        fn cancel(&mut self, unit: usize) -> AmResult {
            self.state.borrow_mut().record(Call::Cancel(unit))
        }
        fn set_led(&mut self, unit: usize, red: u8, green: u8, blue: u8) -> AmResult {
            self.state.borrow_mut().record(Call::SetLed(unit, red, green, blue))
        }
        fn set_led_pattern(&mut self, unit: usize, pattern: LedPattern) -> AmResult {
            self.state.borrow_mut().record(Call::SetLedPattern(unit, pattern))
        }
        fn reader_info(&self, _unit: usize) -> Option<ReaderInfo> {
            self.state.borrow().reader.clone()
        }
        fn card_info(&self, _unit: usize) -> Option<CardInfo> {
            self.state.borrow().card
        }
        fn access_code(&self, _unit: usize) -> Option<LibamAccessCode> {
            self.state.borrow().access_code
        }
        fn aime_id(&self, _unit: usize) -> Option<LibamAimeId> {
            self.state.borrow().aime_id
        }
        fn info_related_sega_id(&self, _unit: usize) -> Option<RelatedSegaId> {
            self.state.borrow().sega.clone()
        }
        fn send_play_log(&mut self, status: PlayLogStatus, aime_id: LibamAimeId, time: &Tm, credit: u32) -> AmResult {
            assert!(!aime_id.flag, "requests always pass flag 0");
            self.state.borrow_mut().record(Call::SendPlayLog { status, aime_id: aime_id.id, time: *time, credit })
        }
        fn query_campaign_progress(&mut self, aime_id: LibamAimeId) -> AmResult {
            self.state.borrow_mut().record(Call::QueryCampaignProgress(aime_id.id))
        }
        fn campaign_info_list(&self) -> Option<CampaignInfoList> {
            self.info_list
        }
        fn campaign_progress_list(&self) -> Option<CampaignProgressList> {
            self.progress_list
        }
        fn take_events(&mut self) -> Vec<AimeEvent> {
            std::mem::take(&mut self.state.borrow_mut().events)
        }
    }

    struct Rig {
        harness: Harness,
        slot: ControllerSlot,
        libam: Rc<RefCell<FakeLibam>>,
    }

    impl Rig {
        fn new() -> Self {
            Self::with_lists(None, None)
        }

        fn with_info_list(list: CampaignInfoList) -> Self {
            Self::with_lists(Some(list), None)
        }

        fn with_progress_list(list: CampaignProgressList) -> Self {
            Self::with_lists(None, Some(list))
        }

        fn with_lists(info_list: Option<CampaignInfoList>, progress_list: Option<CampaignProgressList>) -> Self {
            let libam = Rc::new(RefCell::new(FakeLibam { facade: true, unit_count: 2, ..FakeLibam::default() }));
            let port = SharedLibam { state: Rc::clone(&libam), info_list, progress_list };
            let controller = AimeController::new(Box::new(port));
            Self { harness: Harness::running(), slot: ControllerSlot::new(Box::new(controller)), libam }
        }

        fn pre(&mut self) {
            self.slot.pre_execute(&mut self.harness.ctx());
        }

        fn post(&mut self) {
            self.slot.post_execute(&mut self.harness.ctx());
        }

        fn deliver(&mut self, events: impl IntoIterator<Item = AimeEvent>) {
            self.libam.borrow_mut().events.extend(events);
            self.slot.controller_mut().on_libam_events(&mut self.harness.ctx());
        }

        fn busy(&mut self) -> bool {
            self.slot.controller().is_busy(&self.harness.ctx())
        }

        fn aime(&self) -> ServerAimeRef<'_> {
            self.harness.server.aime()
        }

        fn calls(&self) -> Vec<Call> {
            self.libam.borrow().calls.clone()
        }

        fn unit(&self, index: usize) -> ServerAimeUnitRef<'_> {
            self.harness.server.aime().units().get(index).expect(UNIT_SLOT)
        }

        /// Edits the published state of one unit directly, as an earlier event or request would have left it.
        fn edit_unit(&mut self, index: usize, edit: impl FnOnce(&mut ServerAimeUnitMut<'_>)) {
            let mut aime = self.harness.server.aime_mut();
            let mut units = aime.units_mut();
            edit(&mut units.get_mut(index).expect(UNIT_SLOT));
        }

        fn unit_request(&mut self, index: usize, generation: u32, task: AimeUnitTask, info: [u8; 4]) {
            self.unit_request_raw(index, generation, task as u32, info);
        }

        /// A unit request with a task value that need not be a known [`AimeUnitTask`].
        fn unit_request_raw(&mut self, index: usize, generation: u32, task: u32, info: [u8; 4]) {
            let patch =
                ClientAimeUnitRequestPatch { generation: Some(generation), task: Some(task), info: Some(&info) };
            let mut aime = self.harness.client.aime_mut();
            let mut units = aime.units_mut();
            units.get_mut(index).expect(UNIT_SLOT).copy_from(&patch).expect("integers and bytes");
        }

        fn main_request(&mut self, generation: u32, task: AimeTask, info: [u8; 12], time: DateTime) {
            let patch = ClientAimeRequestPatch {
                generation: Some(generation),
                task: Some(task as u32),
                info: Some(&info),
                time: Some(time.0),
            };
            self.harness.client.aime_mut().request_mut().copy_from(&patch).expect("integers and bytes");
        }

        /// Delivers a unit `TaskFinished` callback with a default fatal error.
        fn unit_finished(&mut self, unit: u64, task: FinishedTask, status: FinishedStatus, error_code: u32) {
            self.unit_finished_with(unit, task, status, error_code, FatalError::default());
        }

        fn unit_finished_with(
            &mut self,
            unit: u64,
            task: FinishedTask,
            status: FinishedStatus,
            error_code: u32,
            fatal: FatalError,
        ) {
            self.deliver([AimeEvent::UnitTaskFinished(Box::new(UnitTaskFinished {
                unit,
                task,
                status,
                error_code,
                fatal,
            }))]);
        }
    }

    /// `src` as a NUL-padded `wchar_t` buffer of `N` units.
    fn wide<const N: usize>(src: &str) -> [u16; N] {
        let mut units = [0; N];
        for (slot, unit) in units.iter_mut().zip(src.encode_utf16()) {
            *slot = unit;
        }
        units
    }

    fn reader(detected: bool, hard: &str, firm: &str) -> ReaderInfo {
        ReaderInfo { detected, hard_version: wide(hard), firm_version: wide(firm) }
    }

    fn text(value: &U16CStr) -> String {
        String::from_utf16_lossy(value.as_slice())
    }

    /// A libam error whose timestamp is a valid calendar date, so `publish` keeps the record.
    fn fatal(number: u16, message: &str) -> FatalError {
        let time = Tm { sec: 56, min: 34, hour: 12, mday: 20, mon: 4, year: 124, ..Tm::default() };
        FatalError::new(number, 2, time, message, 1)
    }

    /// The bytes of one unit's published `ErrorData` (`ServerData+0x3D0`, unit at `+0x18 + 0x4F8·unit`, error at
    /// `+0x348`, data at `+8`).
    fn error_data_bytes(image: &ServerImage, unit: usize) -> &[u8] {
        let start = 0x3D0 + 0x18 + 0x4F8 * unit + 0x348 + 8;
        &image.bytes()[start..start + ErrorData::SCHEMA_SIZE]
    }

    /// What `FatalError::publish` writes into the all-zero `ErrorData` of unit 1.
    fn published_error_data(fatal: &FatalError) -> Vec<u8> {
        let mut image = ServerImage::new();
        publish_fatal_error(
            fatal,
            &mut image.aime_mut().units_mut().get_mut(1).expect(UNIT_SLOT).error_mut().data_mut(),
        );
        error_data_bytes(&image, 1).to_vec()
    }

    fn access_code(rig: &Rig, unit: usize) -> AccessCode {
        AccessCode(*rig.unit(unit).result().access_code())
    }

    fn offline_id(rig: &Rig, unit: usize) -> AimeOfflineId {
        AimeOfflineId(*rig.unit(unit).result().offline_id())
    }

    // --- converters ------------------------------------------------------------------------------------

    #[test]
    fn error_code_table_maps_every_libam_code() {
        let expected: [(u32, u32, AimeErrorCategory); 25] = [
            (0, 0, AimeErrorCategory::Category0),
            (1, 0, AimeErrorCategory::Category0),
            (2, 1, AimeErrorCategory::Category3),
            (3, 2, AimeErrorCategory::Category3),
            (4, 4, AimeErrorCategory::Category1),
            (5, 5, AimeErrorCategory::Category1),
            (6, 6, AimeErrorCategory::Category1),
            (7, 7, AimeErrorCategory::Category1),
            (8, 8, AimeErrorCategory::Category2),
            (9, 9, AimeErrorCategory::Category2),
            (10, 10, AimeErrorCategory::Category2),
            (11, 11, AimeErrorCategory::Category1),
            (12, 13, AimeErrorCategory::Category2),
            (13, 14, AimeErrorCategory::Category1),
            (14, 16, AimeErrorCategory::Category2),
            (15, 23, AimeErrorCategory::Category1),
            (16, 24, AimeErrorCategory::Category1),
            (17, 25, AimeErrorCategory::Category1),
            (18, 26, AimeErrorCategory::Category1),
            (19, 27, AimeErrorCategory::Category1),
            (20, 28, AimeErrorCategory::Category1),
            (21, 29, AimeErrorCategory::Category1),
            (22, 30, AimeErrorCategory::Category3),
            (23, 30, AimeErrorCategory::Category3),
            (24, 31, AimeErrorCategory::Category2),
        ];
        for (code, id, category) in expected {
            let converted = error_id_from_code(code).unwrap_or_else(|| panic!("code {code} is listed"));
            assert_eq!(u32::from(converted), id, "id of libam code {code}");
            assert_eq!(converted.category(), category, "category of libam code {code}");
        }
        assert_eq!(error_id_from_code(25), None);
        assert_eq!(error_id_from_code(u32::MAX), None);
    }

    #[test]
    fn play_log_status_is_shifted_by_one_and_invalid_beyond_event() {
        let statuses = [0, 1, 2, 3, 4, 99].map(PlayLogStatus::from_request);
        assert_eq!(
            statuses,
            [
                PlayLogStatus::Enter,
                PlayLogStatus::Continue,
                PlayLogStatus::Leave,
                PlayLogStatus::Event,
                PlayLogStatus::Invalid,
                PlayLogStatus::Invalid
            ]
        );
    }

    // --- clearServerData / isReady -----------------------------------------------------------------------

    #[test]
    fn more_than_four_units_are_clamped() {
        let mut rig = Rig::new();
        rig.libam.borrow_mut().unit_count = 9;
        rig.pre();
        assert_eq!(rig.aime().unit_count(), 4);
    }

    #[test]
    fn clear_keeps_everything_but_db_alive_and_unit_count() {
        let mut rig = Rig::new();
        rig.edit_unit(1, |unit| unit.result_mut().aime_id_mut().set(77).expect("a scalar accepts every value"));
        {
            let mut aime = rig.harness.server.aime_mut();
            aime.campaign_mut().info_count_mut().set(2).expect("a scalar accepts every value");
            aime.firm_mut().update_progress_mut().set(12.5).expect("a scalar accepts every value");
        }
        rig.slot.controller_mut().clear_server_data(&mut rig.harness.ctx());
        assert_eq!(rig.unit(1).result().aime_id(), 77);
        assert_eq!(rig.aime().campaign().info_count(), 2);
        assert_eq!(rig.aime().firm().update_progress(), 12.5);
    }

    // --- loadAlive / loadCampaignInfo --------------------------------------------------------------------

    #[test]
    fn db_alive_follows_libam_every_update() {
        let mut rig = Rig::new();
        rig.pre();
        assert!(!rig.aime().db_alive());
        rig.libam.borrow_mut().db_alive = true;
        rig.pre();
        assert!(rig.aime().db_alive());
        rig.libam.borrow_mut().db_alive = false;
        rig.pre();
        assert!(!rig.aime().db_alive());
    }

    fn info_entry(id: u32, name: &str, year: i32) -> CampaignInfoEntry {
        CampaignInfoEntry { id, name: wide(name), times: [Tm { mday: 1, year, ..Tm::default() }; 5] }
    }

    /// An entry libam left zero-filled.
    fn blank_entry() -> CampaignInfoEntry {
        CampaignInfoEntry { id: 0, name: [0; CAMPAIGN_NAME_UNITS], times: [Tm::default(); 5] }
    }

    #[test]
    fn campaign_infos_copy_valid_entries_and_keep_untouched_slots() {
        let mut list = CampaignInfoList {
            count: 2,
            entries: [info_entry(11, "first", 124), info_entry(0, "skipped", 124), blank_entry()],
        };
        list.entries[0].times[2].mon = 13; // invalid month: that slot is left alone
        let mut rig = Rig::with_info_list(list);
        {
            let mut aime = rig.harness.server.aime_mut();
            let mut campaign = aime.campaign_mut();
            let mut infos = campaign.infos_mut();
            infos.get_mut(1).expect("three infos").campaign_id_mut().set(99).expect("a scalar accepts every value");
            infos.get_mut(0).expect("three infos").times_mut().set(2, 5).expect("a scalar accepts every value");
        }
        rig.pre();

        let aime = rig.aime();
        let campaign = aime.campaign();
        let infos = campaign.infos();
        let first = infos.get(0).expect("three infos");
        assert_eq!(campaign.info_count(), 2);
        assert_eq!(first.campaign_id(), 11);
        assert_eq!(text(first.name()), "first");
        assert_eq!(first.times().get(0), Some(civil(2024, 1, 1, 0, 0, 0).0));
        assert_eq!(first.times().get(2), Some(5));
        assert_eq!(infos.get(1).expect("three infos").campaign_id(), 99);
    }

    #[test]
    fn campaign_info_name_is_cut_to_127_units_and_count_to_u16() {
        let mut list = CampaignInfoList { count: 0x1_0003, entries: [blank_entry(); 3] };
        list.entries[0] = CampaignInfoEntry { id: 5, name: [u16::from(b'x'); CAMPAIGN_NAME_UNITS], ..blank_entry() };
        let mut rig = Rig::with_info_list(list);
        rig.pre();
        let aime = rig.aime();
        let campaign = aime.campaign();
        let infos = campaign.infos();
        let name = infos.get(0).expect("three infos").name();
        assert_eq!(campaign.info_count(), 3);
        assert_eq!(name.len(), 127);
        assert_eq!(name.as_slice_with_nul().len(), 128, "terminated in the last unit of the field");
    }

    // --- main executor -----------------------------------------------------------------------------------

    fn send_log_request(rig: &mut Rig, generation: u32) {
        rig.main_request(
            generation,
            AimeTask::Log,
            word_info([1234, AimeLogStatus::Leave as u32, 3]),
            civil(2024, 5, 20, 12, 34, 56),
        );
    }

    /// Main request `info` holding the given little-endian words.
    fn word_info(words: [u32; 3]) -> [u8; 12] {
        let mut info = [0; 12];
        for (chunk, word) in info.chunks_exact_mut(4).zip(words) {
            chunk.copy_from_slice(&word.to_le_bytes());
        }
        info
    }

    #[test]
    fn log_request_is_accepted_after_exchange_and_completed_on_the_next_tick() {
        let mut rig = Rig::new();
        rig.pre();
        send_log_request(&mut rig, 1);

        assert!(!rig.busy(), "nothing owned before the accept edge");
        rig.post();
        assert_eq!(rig.aime().response().generation(), 0, "accepted but not yet completed");
        assert!(rig.busy());
        assert_eq!(
            rig.calls(),
            vec![Call::SendPlayLog {
                status: PlayLogStatus::Leave,
                aime_id: 1234,
                time: Tm { sec: 56, min: 34, hour: 12, mday: 20, mon: 4, year: 124, ..Tm::default() },
                credit: 3,
            }]
        );

        rig.pre();
        assert_eq!(rig.aime().response().generation(), 1);
        assert!(rig.aime().response().succeeded());
        assert!(!rig.busy());

        rig.post();
        assert_eq!(rig.calls().len(), 1, "a completed request is not accepted again");
    }

    #[test]
    fn main_request_with_unusable_facade_fails() {
        let mut rig = Rig::new();
        rig.pre();
        rig.libam.borrow_mut().facade = false;
        send_log_request(&mut rig, 1);
        rig.post();
        assert!(rig.calls().is_empty());
        rig.libam.borrow_mut().facade = true;
        rig.pre();
        assert_eq!(rig.aime().response().generation(), 1);
        assert!(!rig.aime().response().succeeded());
    }

    fn send_campaign_request(rig: &mut Rig, generation: u32, aime_id: u32) {
        rig.main_request(generation, AimeTask::CampaignProgress, word_info([aime_id, 0, 0]), DateTime(0));
    }

    fn progress_list(count: u64, entries: [(u32, bool, u32); 3]) -> CampaignProgressList {
        CampaignProgressList {
            count,
            entries: entries.map(|(id, flag, value)| CampaignProgressEntry { id, flag, value }),
        }
    }

    /// The campaign progress entry `index` as `(campaign_id, flag, value)`.
    fn progress(rig: &Rig, index: usize) -> (u32, bool, u32) {
        let aime = rig.aime();
        let campaign = aime.campaign();
        let entry = campaign.progresses().get(index).expect("three entries");
        (entry.campaign_id(), entry.flag(), entry.value())
    }

    #[test]
    fn campaign_progress_waits_for_the_libam_sequence_end_event() {
        let mut rig = Rig::with_progress_list(progress_list(2, [(7, true, 12), (0, true, 99), (8, false, 1)]));
        rig.pre();
        {
            let mut aime = rig.harness.server.aime_mut();
            let mut campaign = aime.campaign_mut();
            campaign.progress_count_mut().set(3).expect("a scalar accepts every value");
            let mut progresses = campaign.progresses_mut();
            let mut entry = progresses.get_mut(0).expect("three entries");
            entry.campaign_id_mut().set(9).expect("a scalar accepts every value");
        }
        send_campaign_request(&mut rig, 1, 555);
        rig.post();
        assert_eq!(rig.calls(), vec![Call::QueryCampaignProgress(555)]);
        assert_eq!(rig.aime().campaign().progress_count(), 0, "accept wipes the previous result");
        assert_eq!(progress(&rig, 0), (0, false, 0));

        for _ in 0..3 {
            rig.pre();
            assert_eq!(rig.aime().response().generation(), 0, "still pending");
            assert!(rig.busy());
        }

        rig.deliver([AimeEvent::TaskFinished {
            task: FinishedTask::QueryCampaignProgress,
            status: FinishedStatus::Succeeded,
        }]);
        assert_eq!(rig.aime().campaign().progress_count(), 2);
        assert_eq!(progress(&rig, 0), (7, true, 12));
        assert_eq!(progress(&rig, 1).0, 0, "entry with id 0 is skipped");
        assert_eq!(progress(&rig, 2).0, 0, "slots at or beyond the count are untouched");
        rig.pre();
        assert_eq!(rig.aime().response().generation(), 0, "data loaded but the sequence has not ended");

        rig.deliver([AimeEvent::TaskFinished {
            task: FinishedTask::SequenceFinished,
            status: FinishedStatus::Succeeded,
        }]);
        rig.pre();
        assert_eq!(rig.aime().response().generation(), 1);
        assert!(rig.aime().response().succeeded());
        assert!(!rig.busy());
    }

    #[test]
    fn campaign_progress_fails_when_the_sequence_ends_unsuccessfully() {
        let mut rig = Rig::new();
        rig.pre();
        send_campaign_request(&mut rig, 3, 1);
        rig.post();
        rig.deliver([AimeEvent::TaskFinished { task: FinishedTask::SequenceFinished, status: FinishedStatus::Failed }]);
        rig.pre();
        assert_eq!((rig.aime().response().generation(), rig.aime().response().succeeded()), (3, false));
    }

    #[test]
    fn pending_campaign_request_mirrors_a_bumped_generation_when_it_completes() {
        let mut rig = Rig::new();
        rig.pre();
        send_campaign_request(&mut rig, 1, 1);
        rig.post();
        rig.harness.client.aime_mut().request_mut().generation_mut().set(2).expect("integers accept every value");
        rig.post();
        assert_eq!(rig.calls().len(), 1, "the second request is never executed");
        rig.deliver([AimeEvent::TaskFinished {
            task: FinishedTask::SequenceFinished,
            status: FinishedStatus::Succeeded,
        }]);
        rig.pre();
        assert_eq!(rig.aime().response().generation(), 2);
        assert!(!rig.busy());
    }

    // --- unit executors ----------------------------------------------------------------------------------

    fn scan_ready_rig() -> Rig {
        let mut rig = Rig::new();
        rig.pre();
        rig
    }

    #[test]
    fn start_scan_publishes_busy_and_wipes_the_previous_result() {
        let mut rig = scan_ready_rig();
        rig.edit_unit(1, |unit| {
            let hard_version = fit_wide_str("HW", 64);
            let patch = ServerAimeUnitPatch {
                confirm: Some(AimeConfirm::Question2.into()),
                result: Some(AimeUnitResultPatch {
                    valid: Some(true),
                    aime_id: Some(5),
                    hard_version: Some(&hard_version),
                    ..AimeUnitResultPatch::default()
                }),
                error: Some(AimeUnitErrorPatch {
                    id: Some(u32::from(AimeErrorId::new(9).expect("id 9 is inside the domain"))),
                    category: Some(AimeErrorCategory::Category2.into()),
                    ..AimeUnitErrorPatch::default()
                }),
                led_status: Some(AimeLedStatus::Warning.into()),
                ..ServerAimeUnitPatch::default()
            };
            unit.copy_from(&patch).expect("scalars, closed enum values and a fitted string");
            publish_fatal_error(&fatal(6508, "stale message"), &mut unit.error_mut().data_mut());
        });
        rig.unit_request(1, 1, AimeUnitTask::Start, 1u32.to_le_bytes());
        rig.post();

        assert_eq!(rig.calls(), vec![Call::StartScan(1)]);
        let unit = rig.unit(1);
        assert!(unit.busy());
        assert_eq!(AimeCommand::try_from(unit.busy_command()).ok(), Some(AimeCommand::Scan));
        assert_eq!(unit.confirm(), AimeConfirm::None);
        assert_eq!((unit.error().id(), unit.error().category()), (0, AimeErrorCategory::Category0));
        assert!(error_data_bytes(&rig.harness.server, 1).iter().all(|&byte| byte == 0));
        assert!(!unit.result().valid());
        assert_eq!(unit.result().aime_id(), AIME_ID_INVALID);
        assert_eq!(unit.result().access_code(), &AccessCode::UNSET.0);
        assert!(unit.result().hard_version().is_empty());
        assert_eq!(unit.led_status(), AimeLedStatus::Warning, "led status is never reset");
        assert_eq!(unit.response().generation(), 0, "completion is published on the next update");

        rig.pre();
        assert_eq!(rig.unit(1).response().generation(), 1);
        assert!(rig.unit(1).response().succeeded(), "unit checker is constant success");
    }

    #[test]
    fn start_scan_offline_uses_the_offline_libam_entry() {
        let mut rig = scan_ready_rig();
        rig.unit_request(0, 1, AimeUnitTask::Start, 0u32.to_le_bytes());
        rig.post();
        assert_eq!(rig.calls(), vec![Call::StartScanOffline(0)]);
        assert_eq!(AimeCommand::try_from(rig.unit(0).busy_command()).ok(), Some(AimeCommand::ScanOffline));
    }

    #[test]
    fn failed_start_still_clears_busy_and_the_result_block() {
        let mut rig = scan_ready_rig();
        rig.edit_unit(2, |unit| {
            unit.result_mut().valid_mut().set(true).expect("a flag accepts every value");
            unit.busy_mut().set(true).expect("a flag accepts every value");
        });
        rig.libam.borrow_mut().command_error = Some(3);
        rig.unit_request(2, 1, AimeUnitTask::Start, 0u32.to_le_bytes());
        rig.post();
        assert!(!rig.unit(2).busy());
        assert!(!rig.unit(2).result().valid());
        rig.pre();
        assert_eq!((rig.unit(2).response().generation(), rig.unit(2).response().succeeded()), (1, false));
    }

    #[test]
    fn start_with_unknown_command_fails_without_calling_libam_but_wipes_state() {
        let mut rig = scan_ready_rig();
        rig.edit_unit(0, |unit| unit.busy_mut().set(true).expect("a flag accepts every value"));
        rig.unit_request(0, 1, AimeUnitTask::Start, 7u32.to_le_bytes());
        rig.post();
        assert!(rig.calls().is_empty());
        assert!(!rig.unit(0).busy());
        assert_eq!(rig.unit(0).busy_command(), 7, "the requested command is stored as is");
        rig.pre();
        assert!(!rig.unit(0).response().succeeded());
    }

    #[test]
    fn cancel_and_confirm_clear_the_question_even_when_libam_refuses() {
        for (task, call) in [(AimeUnitTask::Cancel, Call::Cancel(3)), (AimeUnitTask::Confirm, Call::Confirm(3))] {
            let mut rig = scan_ready_rig();
            rig.edit_unit(3, |unit| unit.confirm_mut().set(AimeConfirm::Question1).expect("a closed enum value"));
            rig.libam.borrow_mut().command_error = Some(1);
            rig.unit_request(3, 1, task, [0; 4]);
            rig.post();
            assert_eq!(rig.calls(), vec![call]);
            assert_eq!(rig.unit(3).confirm(), AimeConfirm::None);
            rig.pre();
            assert!(!rig.unit(3).response().succeeded());
        }
    }

    #[test]
    fn led_request_sends_full_intensity_for_every_non_zero_byte() {
        let mut rig = scan_ready_rig();
        rig.unit_request(0, 1, AimeUnitTask::Led, [1, 0, 200, 0]);
        rig.post();
        assert_eq!(rig.calls(), vec![Call::SetLed(0, 0xFF, 0, 0xFF)]);
    }

    #[test]
    fn led_status_request_clamps_to_the_none_pattern() {
        let mut rig = scan_ready_rig();
        rig.unit_request(1, 1, AimeUnitTask::LedStatus, 3u32.to_le_bytes());
        rig.unit_request(2, 1, AimeUnitTask::LedStatus, 9u32.to_le_bytes());
        rig.unit_request(3, 1, AimeUnitTask::LedStatus, 0u32.to_le_bytes());
        rig.post();
        assert_eq!(
            rig.calls(),
            vec![
                Call::SetLedPattern(1, LedPattern::Warning),
                Call::SetLedPattern(2, LedPattern::None),
                Call::SetLedPattern(3, LedPattern::None),
            ]
        );
    }

    #[test]
    fn unknown_unit_task_fails_and_leaves_the_response_data_alone() {
        let mut rig = scan_ready_rig();
        rig.edit_unit(0, |unit| unit.busy_mut().set(true).expect("a flag accepts every value"));
        rig.unit_request_raw(0, 1, 5, [0; 4]);
        rig.post();
        assert!(rig.calls().is_empty());
        assert!(rig.unit(0).busy());
        rig.pre();
        assert!(!rig.unit(0).response().succeeded());
        assert_eq!(rig.unit(0).response().generation(), 1);
    }

    #[test]
    fn units_are_accepted_independently_in_unit_order_even_beyond_unit_count() {
        let mut rig = scan_ready_rig();
        assert_eq!(rig.aime().unit_count(), 2);
        rig.unit_request(3, 1, AimeUnitTask::Cancel, [0; 4]);
        rig.unit_request(0, 1, AimeUnitTask::Confirm, [0; 4]);
        rig.post();
        assert_eq!(rig.calls(), vec![Call::Confirm(0), Call::Cancel(3)]);
        rig.pre();
        assert_eq!(rig.unit(0).response().generation(), 1);
        assert_eq!(rig.unit(3).response().generation(), 1, "all four units are checked every update");
    }

    // --- busy predicate ----------------------------------------------------------------------------------

    #[test]
    fn busy_covers_main_units_and_the_firmware_aggregate() {
        let mut rig = scan_ready_rig();
        assert!(!rig.busy());

        // A request nobody accepted yet does not keep the app alive.
        rig.unit_request(1, 1, AimeUnitTask::Cancel, [0; 4]);
        assert!(!rig.busy());
        rig.post();
        assert!(rig.busy(), "unit request in flight");
        rig.pre();
        assert!(!rig.busy());

        // Firmware update aggregate.
        rig.unit_finished(0, FinishedTask::UpdateFirmware, FinishedStatus::Started, 0);
        rig.pre();
        assert!(rig.aime().firm().updating());
        assert!(rig.busy());
        rig.unit_finished(0, FinishedTask::UpdateFirmware, FinishedStatus::Succeeded, 0);
        rig.unit_finished(1, FinishedTask::UpdateFirmware, FinishedStatus::Succeeded, 0);
        rig.pre();
        assert!(!rig.busy());
    }

    // --- firmware aggregate ------------------------------------------------------------------------------

    #[test]
    fn firmware_aggregate_matches_idle_done_and_mixed_units() {
        let mut rig = scan_ready_rig();
        rig.pre();
        assert_eq!((rig.aime().firm().updating(), rig.aime().firm().update_progress()), (false, 0.0));

        rig.unit_finished(0, FinishedTask::UpdateFirmware, FinishedStatus::Succeeded, 0);
        rig.pre();
        // {done, idle} reads as updating until the idle unit finishes too.
        assert!(rig.aime().firm().updating());
        assert_eq!(rig.aime().firm().update_progress(), 50.0);

        rig.unit_finished(1, FinishedTask::UpdateFirmware, FinishedStatus::Succeeded, 0);
        rig.pre();
        assert!(!rig.aime().firm().updating());
        assert_eq!(rig.aime().firm().update_progress(), 100.0);
    }

    #[test]
    fn firmware_progress_events_feed_the_mean_over_published_units_only() {
        let mut rig = scan_ready_rig();
        rig.unit_finished(0, FinishedTask::UpdateFirmware, FinishedStatus::Started, 0);
        rig.deliver([
            AimeEvent::FirmwareUpdatingProgress { unit: 0, progress: 30.0 },
            AimeEvent::FirmwareUpdatingProgress { unit: 1, progress: 10.0 },
            AimeEvent::FirmwareUpdatingProgress { unit: 3, progress: 90.0 },
            AimeEvent::FirmwareUpdatingProgress { unit: 4, progress: 50.0 },
        ]);
        rig.pre();
        assert!(rig.aime().firm().updating());
        assert_eq!(rig.aime().firm().update_progress(), 20.0);
    }

    #[test]
    fn firmware_task_status_transitions_follow_set_result() {
        let mut rig = scan_ready_rig();
        let progress = |rig: &mut Rig| {
            rig.pre();
            (rig.aime().firm().updating(), rig.aime().firm().update_progress())
        };
        rig.deliver([
            AimeEvent::FirmwareUpdatingProgress { unit: 0, progress: 40.0 },
            AimeEvent::FirmwareUpdatingProgress { unit: 1, progress: 40.0 },
        ]);
        rig.unit_finished(0, FinishedTask::UpdateFirmware, FinishedStatus::Started, 0);
        rig.unit_finished(1, FinishedTask::UpdateFirmware, FinishedStatus::Started, 0);
        assert_eq!(progress(&mut rig), (true, 40.0));
        // Failed: done, reader data valid, progress unchanged.
        rig.unit_finished(0, FinishedTask::UpdateFirmware, FinishedStatus::Failed, 0);
        rig.unit_finished(1, FinishedTask::UpdateFirmware, FinishedStatus::Failed, 0);
        assert_eq!(progress(&mut rig), (false, 40.0));
        assert!(rig.unit(0).result().valid());
        // Cancelled resets state and progress.
        rig.unit_finished(0, FinishedTask::UpdateFirmware, FinishedStatus::Cancelled, 0);
        rig.unit_finished(1, FinishedTask::UpdateFirmware, FinishedStatus::Queued, 0);
        assert_eq!(progress(&mut rig), (false, 0.0));
    }

    // --- setResult per task / status --------------------------------------------------------------------

    #[test]
    fn detect_card_reader_task_fills_and_resets_reader_data() {
        let mut rig = scan_ready_rig();
        rig.libam.borrow_mut().reader = Some(reader(true, "HW-1", "FW-2"));
        rig.unit_finished(2, FinishedTask::DetectCardReader, FinishedStatus::Started, 0);
        assert!(!rig.unit(2).result().valid(), "started changes nothing");
        rig.unit_finished(2, FinishedTask::DetectCardReader, FinishedStatus::Succeeded, 0);
        let unit = rig.unit(2);
        let result = unit.result();
        assert!(result.valid() && result.reader_detected());
        assert_eq!((text(result.hard_version()), text(result.firm_version())), ("HW-1".to_owned(), "FW-2".to_owned()));

        rig.unit_finished(2, FinishedTask::DetectCardReader, FinishedStatus::Queued, 0);
        let unit = rig.unit(2);
        let result = unit.result();
        assert!(!result.valid() && !result.reader_detected());
        assert!(result.hard_version().is_empty() && result.firm_version().is_empty());

        rig.unit_finished(2, FinishedTask::DetectCardReader, FinishedStatus::Failed, 0);
        assert!(rig.unit(2).result().valid(), "failed reads the reader data like success");
        rig.unit_finished(2, FinishedTask::DetectCardReader, FinishedStatus::Cancelled, 0);
        assert!(rig.unit(2).result().valid(), "cancelled is not handled for this task");
    }

    #[test]
    fn reader_data_is_cut_to_63_units_and_missing_info_keeps_the_valid_flag_clear() {
        let mut rig = scan_ready_rig();
        let info = ReaderInfo { detected: true, hard_version: [u16::from(b'h'); 64], firm_version: [0; 64] };
        rig.libam.borrow_mut().reader = Some(info);
        rig.unit_finished(0, FinishedTask::DetectCardReader, FinishedStatus::Succeeded, 0);
        assert_eq!(rig.unit(0).result().hard_version().len(), 63);

        rig.libam.borrow_mut().reader = None;
        rig.unit_finished(1, FinishedTask::DetectCardReader, FinishedStatus::Succeeded, 0);
        assert!(!rig.unit(1).result().valid());
    }

    #[test]
    fn scan_nfc_chip_task_loads_reader_offline_id_and_access_code() {
        let mut rig = scan_ready_rig();
        let code = AccessCode([0x01, 0x23, 0x45, 0x67, 0x89, 0x01, 0x23, 0x45, 0x67, 0x89]);
        {
            let mut libam = rig.libam.borrow_mut();
            libam.reader = Some(reader(true, "H", "F"));
            libam.card = Some(CardInfo { card_type: 2, idm: 1, pmm: 2, mobile: true });
            libam.access_code = Some(LibamAccessCode(code.0));
        }
        rig.edit_unit(0, |unit| unit.result_mut().valid_mut().set(false).expect("a flag accepts every value"));
        rig.unit_finished(0, FinishedTask::ScanNfcChip, FinishedStatus::Succeeded, 0);
        let unit = rig.unit(0);
        let result = unit.result();
        assert!(result.reader_detected());
        assert!(!result.valid(), "only DetectCardReader marks the reader data valid");
        assert!(result.mobile());
        assert_eq!(access_code(&rig, 0), code);
        assert_eq!(offline_id(&rig, 0), AimeOfflineId::from_access_code(&code));

        // Queued / cancelled reset the ids but not `mobile`.
        rig.unit_finished(0, FinishedTask::ScanNfcChip, FinishedStatus::Cancelled, 0);
        assert_eq!(offline_id(&rig, 0), AimeOfflineId::NONE);
        assert_eq!(access_code(&rig, 0), AccessCode::UNSET);
        assert!(rig.unit(0).result().mobile());
        // Started changes nothing.
        rig.unit_finished(0, FinishedTask::ScanNfcChip, FinishedStatus::Started, 0);
        assert_eq!(access_code(&rig, 0), AccessCode::UNSET);
    }

    #[test]
    fn felica_offline_id_requires_card_type_2_and_non_zero_idm_and_pmm() {
        let cases = [
            (CardInfo { card_type: 2, idm: 0x11, pmm: 0x22, mobile: false }, AimeOfflineId::from_felica(0x11, 0x22)),
            (CardInfo { card_type: 2, idm: 0, pmm: 0x22, mobile: false }, AimeOfflineId::NONE),
            (CardInfo { card_type: 2, idm: 0x11, pmm: 0, mobile: false }, AimeOfflineId::NONE),
            (CardInfo { card_type: 1, idm: 0x11, pmm: 0x22, mobile: false }, AimeOfflineId::NONE),
        ];
        for (card, want) in cases {
            let mut rig = scan_ready_rig();
            rig.libam.borrow_mut().card = Some(card);
            rig.edit_unit(1, |unit| {
                let offline_id = AimeOfflineId::from_felica(9, 9);
                unit.result_mut().offline_id_mut().set(&offline_id.0).expect("seventeen bytes");
            });
            rig.unit_finished(1, FinishedTask::ScanNfcChip, FinishedStatus::Succeeded, 0);
            assert_eq!(offline_id(&rig, 1), want, "{card:?}");
        }
    }

    #[test]
    fn invalid_bcd_access_code_is_stored_as_zeroes() {
        let mut rig = scan_ready_rig();
        rig.libam.borrow_mut().access_code = Some(LibamAccessCode([0xAB; 10]));
        rig.unit_finished(0, FinishedTask::RegisterFelica, FinishedStatus::Succeeded, 0);
        assert_eq!(access_code(&rig, 0), AccessCode([0; 10]));
        rig.libam.borrow_mut().access_code = None;
        rig.edit_unit(0, |unit| unit.result_mut().access_code_mut().set(&[0x11; 10]).expect("ten bytes"));
        rig.unit_finished(0, FinishedTask::CheckFelicaRegistered, FinishedStatus::Failed, 0);
        assert_eq!(access_code(&rig, 0), AccessCode([0x11; 10]), "no libam code: nothing is written");
        rig.unit_finished(0, FinishedTask::CheckFelicaRegistered, FinishedStatus::Queued, 0);
        assert_eq!(access_code(&rig, 0), AccessCode::UNSET);
    }

    #[test]
    fn aime_id_tasks_load_id_and_related_ids() {
        let mut rig = scan_ready_rig();
        let sega = RelatedSegaId { auth_key: wide("secret"), count: 3, items: [Some(100), Some(200), None] };
        {
            let mut libam = rig.libam.borrow_mut();
            libam.access_code = Some(LibamAccessCode([0x12; 10]));
            libam.aime_id = Some(LibamAimeId { id: 4242, flag: true });
            libam.sega = Some(sega);
        }
        // The stored code (zero-fill) is valid BCD, so the access code is not reloaded.
        rig.unit_finished(1, FinishedTask::RegisterAimeId, FinishedStatus::Succeeded, 0);
        assert_eq!(access_code(&rig, 1), AccessCode([0; 10]));
        let unit = rig.unit(1);
        let result = unit.result();
        assert_eq!(result.aime_id(), 4242);
        assert!(result.sega_id_registered());
        assert_eq!(text(result.sega_id_auth_key()), "secret");
        // Slot 0 of the libam list is not copied: ids come from slots 1 and 2 (the latter is an error → 0).
        assert_eq!(result.related_aime_ids().copy_into(), [200, 0]);
        assert_eq!(result.related_aime_id_count(), 1);

        // An unset (FF) stored code is reloaded first.
        rig.edit_unit(1, |unit| unit.result_mut().access_code_mut().set(&AccessCode::UNSET.0).expect("ten bytes"));
        rig.unit_finished(1, FinishedTask::CheckAimeIdRegistered, FinishedStatus::Failed, 0);
        assert_eq!(access_code(&rig, 1), AccessCode([0x12; 10]));

        // Queued / cancelled reset only the id and the flag.
        rig.unit_finished(1, FinishedTask::CheckAimeIdRegistered, FinishedStatus::Queued, 0);
        let unit = rig.unit(1);
        let result = unit.result();
        assert_eq!(result.aime_id(), AIME_ID_INVALID);
        assert!(!result.sega_id_registered());
        assert_eq!(text(result.sega_id_auth_key()), "secret");
        assert_eq!(result.related_aime_id_count(), 1);
    }

    #[test]
    fn related_ids_beyond_the_libam_count_stay_as_they_were() {
        let mut rig = scan_ready_rig();
        let sega = RelatedSegaId { auth_key: wide("k"), count: 2, items: [Some(1), Some(2), Some(3)] };
        rig.libam.borrow_mut().sega = Some(sega);
        rig.edit_unit(0, |unit| {
            unit.result_mut().related_aime_ids_mut().copy_from(&[55, 66]).expect("two ids");
        });
        rig.unit_finished(0, FinishedTask::RegisterAimeId, FinishedStatus::Succeeded, 0);
        assert_eq!(rig.unit(0).result().related_aime_ids().copy_into(), [2, 66]);
        assert_eq!(rig.unit(0).result().related_aime_id_count(), 1);
    }

    #[test]
    fn tasks_without_unit_behaviour_and_unknown_units_are_dropped() {
        let mut rig = scan_ready_rig();
        rig.libam.borrow_mut().reader = Some(reader(true, "H", "F"));
        let before = rig.harness.server.bytes().to_vec();
        rig.unit_finished(0, FinishedTask::QueryCampaignProgress, FinishedStatus::Succeeded, 0);
        rig.unit_finished(0, FinishedTask::None, FinishedStatus::Succeeded, 0);
        rig.unit_finished(4, FinishedTask::DetectCardReader, FinishedStatus::Succeeded, 0);
        rig.unit_finished(u64::MAX, FinishedTask::SequenceFinished, FinishedStatus::Succeeded, 3);
        assert_eq!(rig.harness.server.bytes(), before.as_slice());
    }

    #[test]
    fn sequence_finished_publishes_the_error_and_clears_busy() {
        let mut rig = scan_ready_rig();
        rig.edit_unit(1, |unit| {
            unit.busy_mut().set(true).expect("a flag accepts every value");
            publish_fatal_error(&fatal(1, "old"), &mut unit.error_mut().data_mut());
        });
        let error = fatal(6506, "Network error");
        let expected = published_error_data(&error);

        rig.unit_finished_with(1, FinishedTask::SequenceFinished, FinishedStatus::Failed, 3, error);
        let unit = rig.unit(1);
        assert_eq!((unit.error().id(), unit.error().category()), (2, AimeErrorCategory::Category3));
        assert!(!unit.busy());
        assert_eq!(unit.error().data().number(), 6506, "publish keeps a valid record");
        assert_eq!(error_data_bytes(&rig.harness.server, 1), expected.as_slice());
        assert!(rig.calls().is_empty(), "category 3 does not drive the LED");
    }

    #[test]
    fn sequence_finished_with_category_one_error_lights_the_error_led() {
        let mut rig = scan_ready_rig();
        rig.unit_finished(2, FinishedTask::SequenceFinished, FinishedStatus::Succeeded, 5);
        assert_eq!((rig.unit(2).error().id(), rig.unit(2).error().category()), (5, AimeErrorCategory::Category1));
        assert_eq!(rig.calls(), vec![Call::SetLedPattern(2, LedPattern::Error)]);

        // Any status ends the sequence; code 0 and 1 mean "no error".
        rig.unit_finished(2, FinishedTask::SequenceFinished, FinishedStatus::Queued, 1);
        assert_eq!((rig.unit(2).error().id(), rig.unit(2).error().category()), (0, AimeErrorCategory::Category0));
        assert_eq!(rig.calls().len(), 1);
    }

    // --- NeedAcceptation / LED pattern ------------------------------------------------------------------

    #[test]
    fn need_acceptation_sets_confirm_until_the_next_start_cancel_or_confirm() {
        let mut rig = scan_ready_rig();
        rig.deliver([AimeEvent::NeedAcceptation { unit: 1, question: 2 }]);
        assert_eq!(rig.unit(1).confirm(), AimeConfirm::Question2);
        rig.deliver([AimeEvent::NeedAcceptation { unit: 1, question: 1 }]);
        assert_eq!(rig.unit(1).confirm(), AimeConfirm::Question1);
        rig.pre();
        assert_eq!(rig.unit(1).confirm(), AimeConfirm::Question1, "updates keep it");

        rig.unit_request(1, 1, AimeUnitTask::Confirm, [0; 4]);
        rig.post();
        assert_eq!(rig.unit(1).confirm(), AimeConfirm::None);
    }

    #[test]
    fn unlisted_question_clears_confirm_while_unknown_units_are_ignored() {
        let mut rig = scan_ready_rig();
        rig.edit_unit(0, |unit| unit.confirm_mut().set(AimeConfirm::Question1).expect("a closed enum value"));
        rig.deliver([AimeEvent::NeedAcceptation { unit: 7, question: 9 }]);
        assert_eq!(rig.unit(0).confirm(), AimeConfirm::Question1, "unknown units are ignored");
        rig.deliver([AimeEvent::NeedAcceptation { unit: 0, question: 9 }]);
        assert_eq!(rig.unit(0).confirm(), AimeConfirm::None);
    }

    #[test]
    fn led_pattern_events_publish_the_led_status() {
        let mut rig = scan_ready_rig();
        rig.deliver([
            AimeEvent::ChangeLedLightPattern { unit: 3, pattern: 3 },
            AimeEvent::ChangeLedLightPattern { unit: 9, pattern: 3 },
        ]);
        assert_eq!(rig.unit(3).led_status(), AimeLedStatus::Warning);
        rig.deliver([AimeEvent::ChangeLedLightPattern { unit: 3, pattern: 5 }]);
        assert_eq!(rig.unit(3).led_status(), AimeLedStatus::None);
    }
}
