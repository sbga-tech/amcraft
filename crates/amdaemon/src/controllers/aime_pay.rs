//! `amdaemon::server::AimePayController` (vtable `0x1405C2918`, ctor `0x140019F60`, `App+0x1040`).
//!
//! Publishes the libam Aime Pay module into `ServerData+0x3478` and executes the Aime Pay requests
//! (`ClientData+0xCC0`). Behaviour: `spec/controllers/credit_emoney_aimepay.md` §4 and the shared machinery
//! in §1; claims `credit_emoney_aimepay.aimepay.*`.
//!
//! The response only acknowledges submission (checker constant `2`). The outcome of the asynchronous libam
//! work is reported through the published `operation` state, driven by [`AimePayEvent`]s applied in
//! [`Controller::on_libam_events`] and the display-complete hub callback.

use std::fmt::{self, Write as _};

use widestring::U16CStr;

use crate::executor::{RequestExecutor, RequestState};
use crate::schema::aime_pay::{
    AimePayConfirm, AimePayTask, ClientAimePayRef, DEAL_RESULT_CAPACITY, DEAL_SUMMARY_CAPACITY,
    ServerDealResultDataMut, ServerDealResultMut, ServerDealResultPatch, ServerDealSummaryDataMut,
    ServerDealSummaryPatch, ServerLocationMut, ServerLocationPatch, ServerOperationPatch, ServerUserSettingMut,
    ServerUserSettingPatch,
};
use crate::schema::image::ServerImage;
use crate::schema::text::fit_wide;
use libam_api::aime::AccessCode;
use libam_api::aime_pay::{
    AimeId, AimePayAppConfig, AimePayDeal, AimePayEvent, AimePayLocation, AimePayPort, AimePayUserSetting,
};
use libam_api::emoney::non_empty;
use libam_api::result::{AmError, AmResult};

use crate::controllers::controller::{Controller, ControllerId, Ctx, commit_check};
use libam_api::config::AppConfig;

/// libam Aime Pay task id of `QueryUserSetting` (`0x1402865A0`); the other ids are 2 Activation,
/// 3 Deactivation, 4 PaymentToItem and 5 PaymentToCoin.
const LIBAM_TASK_QUERY_USER_SETTING: u32 = 1;
/// libam `ErrorInfo` kind that marks an error (`operation.errorOccurred`).
const LIBAM_ERROR_KIND_ERROR: u32 = 1;
/// libam deal status that marks an error (`operation.errorOccurred` after `NotifyDealResult`).
const LIBAM_DEAL_STATUS_ERROR: u32 = 1;
/// Longest `errorCode` text (`wcsncpy_s(dst, 19, src, 18)`).
const ERROR_CODE_UNITS: usize = 18;

/// Fixed-capacity UTF-16 buffer that truncates like the `errorCode` copy.
struct ErrorCodeText {
    units: [u16; ERROR_CODE_UNITS],
    len: usize,
}

impl ErrorCodeText {
    fn push_units(&mut self, units: impl IntoIterator<Item = u16>) {
        for unit in units {
            if let Some(slot) = self.units.get_mut(self.len) {
                *slot = unit;
                self.len += 1;
            }
        }
    }

    fn as_units(&self) -> &[u16] {
        &self.units[..self.len]
    }
}

impl fmt::Write for ErrorCodeText {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.push_units(text.encode_utf16());
        Ok(())
    }
}

/// `convertDeal` (`0x14001A900`): converts one libam deal into the published result.
///
/// The original formats `"%u - %s"` into a 19-unit buffer with `vswprintf_s`, which invokes the CRT invalid
/// parameter handler when the text does not fit; the port truncates to the 18 characters that are copied.
fn convert_deal(deal: &AimePayDeal, result: &mut ServerDealResultMut<'_>) {
    let mut text = ErrorCodeText { units: [0; ERROR_CODE_UNITS], len: 0 };
    if deal.status == 0 || deal.error_code != 0 {
        // Writing to the truncating buffer cannot fail.
        let _ = write!(text, "{} - ", deal.error_code);
        text.push_units(non_empty(&deal.error_message).unwrap_or_default().iter().copied().take_while(|&u| u != 0));
    }
    let error_code = fit_wide(text.as_units(), result.error_code_mut().capacity());
    let access_code = if AccessCode::is_valid(&deal.access_code) { deal.access_code } else { [0; 10] };
    let item_id = fit_wide(non_empty(&deal.item_id).unwrap_or_default(), result.item_id_mut().capacity());
    let patch = ServerDealResultPatch {
        valid: Some(true),
        coin: Some(deal.is_coin),
        status: Some(match deal.status {
            0 => 1,
            2 => 3,
            3 => 2,
            _ => 0,
        }),
        time: deal.time.to_date_time().map(|time| time.0),
        error_code: Some(&error_code),
        access_code: Some(&access_code),
        item_id: Some(&item_id),
        item_count: Some(deal.item_count),
        // libam reports "no amount" as 0xFFFFFFFF, which is -1 as a signed value.
        amount: Some(deal.amount.cast_signed()),
        receipt_id: Some(deal.receipt_id),
    };
    result.copy_from(&patch).expect("fitted strings and scalars fit");
}

/// Accept-prelude clear of `operation.dealResult` (`0x14001B450`): every field but `status` is written. The
/// strings only get their terminator, so the units behind it keep their previous values, as in the original.
fn clear_deal_result(result: &mut ServerDealResultMut<'_>) {
    let patch = ServerDealResultPatch {
        valid: Some(false),
        coin: Some(false),
        status: None,
        time: Some(0),
        error_code: Some(<&U16CStr>::default()),
        access_code: Some(&[0; 10]),
        item_id: Some(<&U16CStr>::default()),
        item_count: Some(0),
        amount: Some(0),
        receipt_id: Some(0),
    };
    result.copy_from(&patch).expect("scalars and the empty strings fit");
}

/// Updates the published operation state (`SD+0x3580`).
fn update_operation(server: &mut ServerImage, patch: &ServerOperationPatch<'_>) {
    server.aime_pay_mut().operation_mut().copy_from(patch).expect("flags and the question accept every value");
}

/// `convert(DealLog → ServerDealResultData)` (`0x14001AAF0`): rebuilds the whole history, newest first.
fn convert_deal_log(port: &dyn AimePayPort, history: &mut ServerDealResultDataMut<'_>) {
    let mut count = 0;
    let mut items = history.items_mut();
    for index in 0..DEAL_RESULT_CAPACITY {
        let mut slot = items.get_mut(index).expect("one slot per history index");
        clear_deal_result(&mut slot);
        if index >= port.deal_log_count() {
            continue;
        }
        match port.deal_log_item(index) {
            Some(deal) => {
                convert_deal(&deal, &mut slot);
                count += 1;
            }
            None => tracing::warn!("libam version does not match"),
        }
    }
    history.count_mut().set(count).expect("u32 accepts every value");
}

/// `convert(DealSummary → ServerDealSummaryData)` (`0x14001ACE0`).
fn convert_deal_summary(port: &dyn AimePayPort, summary: &mut ServerDealSummaryDataMut<'_>) {
    let mut count = 0;
    let mut items = summary.items_mut();
    for index in 0..DEAL_SUMMARY_CAPACITY {
        let mut slot = items.get_mut(index).expect("one slot per summary index");
        slot.clear();
        if index >= port.deal_summary_count() {
            continue;
        }
        match port.deal_summary_item(index) {
            Some(item) => {
                let patch = ServerDealSummaryPatch {
                    date: item.date.to_date_time().map(|date| date.0),
                    amount: Some(item.amount),
                    count: Some(item.count),
                };
                slot.copy_from(&patch).expect("scalars accept every value");
                count += 1;
            }
            None => tracing::warn!("libam version does not match"),
        }
    }
    summary.count_mut().set(count).expect("u32 accepts every value");
}

/// `convertLoc` (`0x14001AF60`).
fn convert_location(location: AimePayLocation, out: &mut ServerLocationMut<'_>) {
    let name = location.name.as_ref().map(|name| name.as_slice()).unwrap_or_default();
    let name = fit_wide(name, out.name_mut().capacity());
    let company_name = location.company_name.as_ref().map(|name| name.as_slice()).unwrap_or_default();
    let company_name = fit_wide(company_name, out.company_name_mut().capacity());
    let patch =
        ServerLocationPatch { valid: Some(location.valid), name: Some(&name), company_name: Some(&company_name) };
    out.copy_from(&patch).expect("fitted strings and a flag fit");
}

/// `convert(Question → AimePayConfirm)` (`0x14001AED0`): unknown questions map to none.
fn convert_confirm(question: u32) -> AimePayConfirm {
    match question {
        1 => AimePayConfirm::Accept,
        2 => AimePayConfirm::WithParam,
        _ => {
            tracing::warn!("libam version does not match");
            AimePayConfirm::None
        }
    }
}

/// Copies libam's user setting (`0x14001AE60`); an invalid setting reads as all zero.
fn convert_user_setting(setting: Option<AimePayUserSetting>, out: &mut ServerUserSettingMut<'_>) {
    match setting {
        Some(setting) => {
            let patch = ServerUserSettingPatch {
                aime_id: Some(setting.aime_id),
                credit_card_available: Some(setting.credit_card_available),
                aime_hold_skip: Some(setting.aime_hold_skip),
            };
            out.copy_from(&patch).expect("scalars accept every value");
        }
        None => out.clear(),
    }
}

/// `AimePayController`.
pub struct AimePayController {
    port: Box<dyn AimePayPort>,
    executor: RequestExecutor,
    /// `controller+0xA8`: a display test started by DisplayCheck is running.
    display_check_active: bool,
}

impl fmt::Debug for AimePayController {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AimePayController")
            .field("executor", &self.executor)
            .field("display_check_active", &self.display_check_active)
            .finish_non_exhaustive()
    }
}

impl AimePayController {
    /// Constructs the controller (ctor `0x140019F60`).
    pub fn new(port: Box<dyn AimePayPort>) -> Self {
        Self { port, executor: RequestExecutor::default(), display_check_active: false }
    }

    /// Applies one libam listener event (`0x140019D00`, `0x140019ED0`, `0x140019F20`).
    fn handle_event(&self, event: AimePayEvent, ctx: &mut Ctx<'_>) {
        match event {
            AimePayEvent::CompleteTask { task, error_kind } => {
                let patch = ServerOperationPatch {
                    error_occurred: Some(error_kind == LIBAM_ERROR_KIND_ERROR),
                    busy: Some(false),
                    ..ServerOperationPatch::default()
                };
                update_operation(ctx.server, &patch);
                if self.port.handle().is_err() {
                    tracing::warn!("libam is not ready");
                    return;
                }
                convert_deal_log(&*self.port, &mut ctx.server.aime_pay_mut().deal_result_mut());
                if task != LIBAM_TASK_QUERY_USER_SETTING {
                    return;
                }
                if self.port.handle().is_err() {
                    tracing::warn!("libam is not ready");
                    return;
                }
                let mut aime_pay = ctx.server.aime_pay_mut();
                convert_user_setting(self.port.user_setting(), &mut aime_pay.operation_mut().user_setting_mut());
            }
            AimePayEvent::NotifyDealResult { deal, .. } => {
                let mut aime_pay = ctx.server.aime_pay_mut();
                let mut operation = aime_pay.operation_mut();
                convert_deal(&deal, &mut operation.deal_result_mut());
                let error = deal.status == LIBAM_DEAL_STATUS_ERROR;
                operation.error_occurred_mut().set(error).expect("bool accepts every value");
            }
            AimePayEvent::NeedAcceptation { question } => {
                let patch = ServerOperationPatch {
                    confirm: Some(convert_confirm(question).into()),
                    cancellable: Some(true),
                    ..ServerOperationPatch::default()
                };
                update_operation(ctx.server, &patch);
            }
        }
    }

    /// `RequestExecutor::checkResult` tail of `update`.
    fn check_result(&mut self, ctx: &mut Ctx<'_>) {
        let mut aime_pay = ctx.server.aime_pay_mut();
        let outcome =
            self.executor.check_result(ctx.client.aime_pay().generation(), aime_pay.response().generation(), || {
                RequestState::Succeeded
            });
        commit_check(outcome, &mut aime_pay.response_mut());
    }

    /// Executes the request task and returns the `am::Result` the original ends up with.
    ///
    /// A payload that fails its proof (confirmation parameter or item id not terminated within its units) is
    /// answered with a `Result` code without calling libam: libam rejects such a parameter (`Result` 1) and
    /// such an item (`Result` 6).
    fn run_task(&mut self, request: &ClientAimePayRef<'_>, server: &mut ServerImage) -> AmResult<()> {
        match request.task_kind() {
            Some(AimePayTask::Cancel) => {
                if self.display_check_active {
                    self.port.display_cancel();
                } else {
                    self.port.cancel();
                }
                let patch = ServerOperationPatch {
                    confirm: Some(AimePayConfirm::None.into()),
                    cancellable: Some(false),
                    ..ServerOperationPatch::default()
                };
                update_operation(server, &patch);
                Ok(())
            }
            Some(AimePayTask::Confirm) => {
                let result = if server.aime_pay().operation().confirm() == AimePayConfirm::WithParam {
                    match request.confirm_info() {
                        Some(info) => self.port.confirm_with_param(info.param().as_slice()),
                        None => Err(AmError::code(AmError::INVALID_ARGUMENT)),
                    }
                } else {
                    self.port.confirm_accept();
                    Ok(())
                };
                if result.is_ok() {
                    let patch = ServerOperationPatch {
                        confirm: Some(AimePayConfirm::None.into()),
                        cancellable: Some(false),
                        ..ServerOperationPatch::default()
                    };
                    update_operation(server, &patch);
                }
                result
            }
            Some(AimePayTask::DisplayCheck) => {
                self.port.display_start_test()?;
                self.display_check_active = true;
                update_operation(server, &ServerOperationPatch { cancellable: Some(true), ..Default::default() });
                Ok(())
            }
            Some(AimePayTask::Activate) => self.port.activate(),
            Some(AimePayTask::Deactivate) => self.port.deactivate(),
            Some(AimePayTask::UserSetting) => {
                let aime_id = AimeId { id: request.user_setting_info().aime_id(), flag: 0 };
                self.port.query_user_setting(aime_id)?;
                server.aime_pay_mut().operation_mut().user_setting_mut().clear();
                Ok(())
            }
            // The original validates the access code and then calls libam unconditionally: libam's result
            // replaces the validator's `Result(1)`, and an invalid code reaches libam as the unset object.
            Some(AimePayTask::CoinPay) => {
                let info = request.coin_pay_info();
                let code = AccessCode::parse(info.access_code()).unwrap_or(AccessCode::UNSET);
                self.port.coin_pay(AimeId { id: info.aime_id(), flag: 0 }, &code, info.player_index(), info.coin())
            }
            Some(AimePayTask::AmountPay) => {
                let info = request.amount_pay_info().ok_or(AmError::code(AmError::FAILED))?;
                let code = AccessCode::parse(info.access_code()).unwrap_or(AccessCode::UNSET);
                let aime_id = AimeId { id: info.aime_id(), flag: 0 };
                self.port.amount_pay(aime_id, &code, info.item_id().as_slice(), info.item_count(), info.option_number())
            }
            None => Err(AmError::with_message(AmError::UNKNOWN_TASK, "Requested unknown task.")),
        }
    }

    /// `AimePayController::accept` (`0x14001A430`): the acceptor of the executor.
    fn accept(&mut self, ctx: &mut Ctx<'_>) -> RequestState {
        let client = ctx.client;
        let request = client.aime_pay();
        if ctx.manager.debug_dump()
            && let Some(text) = request_trace(&request)
        {
            tracing::info!("{text}");
        }
        if self.port.handle().is_err() {
            tracing::warn!("libam is not ready");
            return RequestState::Failed;
        }
        if self.port.shared_handle().is_err() {
            tracing::warn!("libam is not ready");
            return RequestState::Failed;
        }
        // Tasks 2..=7 start a fresh operation; Cancel, Confirm and unknown tasks leave the state alone.
        if (2..=7).contains(&request.task()) {
            let flags = ServerOperationPatch {
                busy: Some(false),
                cancellable: Some(false),
                confirm: Some(AimePayConfirm::None.into()),
                error_occurred: Some(false),
                ..ServerOperationPatch::default()
            };
            update_operation(ctx.server, &flags);
            clear_deal_result(&mut ctx.server.aime_pay_mut().operation_mut().deal_result_mut());
            update_operation(ctx.server, &ServerOperationPatch { busy: Some(true), ..Default::default() });
        }
        match self.run_task(&request, ctx.server) {
            Ok(()) => RequestState::Pending,
            Err(_) => {
                update_operation(ctx.server, &ServerOperationPatch { busy: Some(false), ..Default::default() });
                RequestState::Failed
            }
        }
    }
}

/// Access code text of the trace (`0x140061830`): the ten bytes as twenty upper-case hex digits.
fn access_code_text(bytes: &[u8; 10]) -> String {
    bytes.iter().fold(String::with_capacity(20), |mut text, byte| {
        let _ = write!(text, "{byte:02X}");
        text
    })
}

/// Request trace line (`0x14001B000`). `None` for an unknown task: the original looks the name up in a table
/// that yields NULL there and dereferences it, so it never produces a line. A payload that fails its proof
/// produces no line either.
fn request_trace(request: &ClientAimePayRef<'_>) -> Option<String> {
    let task = request.task_kind()?;
    let name = task.name();
    Some(match task {
        AimePayTask::Cancel | AimePayTask::DisplayCheck | AimePayTask::Activate | AimePayTask::Deactivate => {
            format!("AimePay - task: {name}")
        }
        AimePayTask::Confirm => {
            format!(
                "AimePay - task: {name}, param: {}",
                String::from_utf16_lossy(request.confirm_info()?.param().as_slice())
            )
        }
        AimePayTask::UserSetting => {
            format!("AimePay - task: {name}, AimeID: {}", request.user_setting_info().aime_id())
        }
        AimePayTask::CoinPay => {
            let info = request.coin_pay_info();
            format!(
                "AimePay - task: {name}, AimeID: {}, access code: {}, player: {}, coin: {}",
                info.aime_id(),
                access_code_text(info.access_code()),
                info.player_index(),
                info.coin()
            )
        }
        AimePayTask::AmountPay => {
            let info = request.amount_pay_info()?;
            format!(
                "AimePay - task: {name}, AimeID: {}, access code: {}, itemID: {}, item count: {}, option No: {}",
                info.aime_id(),
                access_code_text(info.access_code()),
                String::from_utf16_lossy(info.item_id().as_slice()),
                info.item_count(),
                info.option_number()
            )
        }
    })
}

impl Controller for AimePayController {
    fn id(&self) -> ControllerId {
        ControllerId::AimePay
    }

    /// vf3 `0x14001A150`.
    fn is_busy(&self, ctx: &Ctx<'_>) -> bool {
        self.executor.is_busy(ctx.client.aime_pay().generation(), ctx.server.aime_pay().response().generation())
    }

    /// vf5 `0x14001A180`: `available` and the deal history.
    fn clear_server_data(&mut self, ctx: &mut Ctx<'_>) {
        if self.port.handle().is_err() {
            tracing::warn!("libam is not ready");
            return;
        }
        let mut aime_pay = ctx.server.aime_pay_mut();
        aime_pay.available_mut().set(self.port.is_enabled()).expect("bool accepts every value");
        convert_deal_log(&*self.port, &mut aime_pay.deal_result_mut());
    }

    /// vf6 `0x14001A290`: a failing handle guard skips everything including `checkResult`.
    fn update_server_data(&mut self, ctx: &mut Ctx<'_>) {
        if self.port.handle().is_err() {
            tracing::warn!("libam is not ready");
            return;
        }
        let mut aime_pay = ctx.server.aime_pay_mut();
        convert_location(self.port.activated_location(), &mut aime_pay.activated_location_mut());
        convert_location(self.port.current_location(), &mut aime_pay.current_location_mut());
        let deal_available = self.port.deal_available();
        aime_pay.operation_mut().deal_available_mut().set(deal_available).expect("bool accepts every value");
        convert_deal_summary(&*self.port, &mut aime_pay.deal_summary_mut());
        self.check_result(ctx);
    }

    /// vf7 `0x14001A3D0`.
    fn accept_requests(&mut self, ctx: &mut Ctx<'_>) {
        let request_generation = ctx.client.aime_pay().generation();
        let response_generation = ctx.server.aime_pay().response().generation();
        let mut executor = std::mem::take(&mut self.executor);
        executor.accept(request_generation, response_generation, || self.accept(ctx));
        self.executor = executor;
    }

    /// The three `AimePayEventListener`s (`0x140019D00`, `0x140019ED0`, `0x140019F20`).
    fn on_libam_events(&mut self, ctx: &mut Ctx<'_>) {
        for event in self.port.take_events() {
            self.handle_event(event, ctx);
        }
    }

    /// AppConfig lambda `ef92780a…` (`0x14001CAD0`): installs the three listeners.
    fn on_app_config(&mut self, config: &mut AppConfig, _ctx: &mut Ctx<'_>) {
        config.aime_pay = AimePayAppConfig {
            complete_task_listener: true,
            notify_deal_result_listener: true,
            need_acceptation_listener: true,
        };
    }

    /// Display-complete lambda `90a41aac…` (`0x14001CA70`): ends a DisplayCheck (`cancellable` stays).
    fn on_display_complete(&mut self, ctx: &mut Ctx<'_>) {
        if self.display_check_active {
            update_operation(ctx.server, &ServerOperationPatch { busy: Some(false), ..Default::default() });
            self.display_check_active = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::controllers::controller::ControllerSlot;
    use crate::controllers::controller::testing::{Harness, civil};
    use crate::schema::aime_pay::{ClientAimePayPatch, ServerDealResultRef, ServerOperationRef};
    use crate::schema::text::fit_wide_str;
    use libam_api::aime_pay::AimePaySummaryItem;
    use libam_api::time::Tm;

    const CODE: [u8; 10] = [0x12, 0x34, 0x56, 0x78, 0x90, 0x12, 0x34, 0x56, 0x78, 0x90];

    /// State of the fake libam Aime Pay module shared between the test and the controller's port.
    #[derive(Default)]
    struct Libam {
        up: bool,
        shared_up: bool,
        enabled: bool,
        deals: Vec<AimePayDeal>,
        summary: Vec<AimePaySummaryItem>,
        deal_available: bool,
        user_setting: Option<AimePayUserSetting>,
        command_error: Option<u32>,
        calls: Vec<String>,
        events: Vec<AimePayEvent>,
    }

    struct FakePort {
        libam: Rc<RefCell<Libam>>,
        activated: AimePayLocation,
        current: AimePayLocation,
    }

    impl FakePort {
        fn new(libam: &Rc<RefCell<Libam>>) -> Self {
            let location =
                AimePayLocation { valid: true, name: Some(wide("Game Center")), company_name: Some(wide("Company")) };
            Self { libam: Rc::clone(libam), activated: location, current: location }
        }

        fn command(&self, call: String) -> AmResult<()> {
            let mut libam = self.libam.borrow_mut();
            libam.calls.push(call);
            libam.command_error.map_or(Ok(()), |code| Err(AmError::code(code)))
        }
    }

    impl AimePayPort for FakePort {
        fn handle(&self) -> AmResult<()> {
            if self.libam.borrow().up { Ok(()) } else { Err(AmError::code(2)) }
        }
        fn shared_handle(&self) -> AmResult<()> {
            if self.libam.borrow().shared_up { Ok(()) } else { Err(AmError::code(2)) }
        }
        fn is_enabled(&self) -> bool {
            self.libam.borrow().enabled
        }
        fn deal_log_count(&self) -> usize {
            self.libam.borrow().deals.len()
        }
        fn deal_log_item(&self, index: usize) -> Option<AimePayDeal> {
            self.libam.borrow().deals.get(index).cloned()
        }
        fn activated_location(&self) -> AimePayLocation {
            self.activated
        }
        fn current_location(&self) -> AimePayLocation {
            self.current
        }
        fn deal_available(&self) -> bool {
            self.libam.borrow().deal_available
        }
        fn deal_summary_count(&self) -> usize {
            self.libam.borrow().summary.len()
        }
        fn deal_summary_item(&self, index: usize) -> Option<AimePaySummaryItem> {
            self.libam.borrow().summary.get(index).copied()
        }
        fn user_setting(&self) -> Option<AimePayUserSetting> {
            self.libam.borrow().user_setting
        }
        fn display_start_test(&mut self) -> AmResult<()> {
            self.command("display_start_test".into())
        }
        fn display_cancel(&mut self) {
            self.libam.borrow_mut().calls.push("display_cancel".into());
        }
        fn cancel(&mut self) {
            self.libam.borrow_mut().calls.push("cancel".into());
        }
        fn confirm_with_param(&mut self, param: &[u16]) -> AmResult<()> {
            self.command(format!("confirm_with_param({})", String::from_utf16_lossy(param)))
        }
        fn confirm_accept(&mut self) {
            self.libam.borrow_mut().calls.push("confirm_accept".into());
        }
        fn activate(&mut self) -> AmResult<()> {
            self.command("activate".into())
        }
        fn deactivate(&mut self) -> AmResult<()> {
            self.command("deactivate".into())
        }
        fn query_user_setting(&mut self, aime_id: AimeId) -> AmResult<()> {
            self.command(format!("query_user_setting({},{})", aime_id.id, aime_id.flag))
        }
        fn coin_pay(
            &mut self,
            aime_id: AimeId,
            access_code: &AccessCode,
            player_index: u32,
            coin: u32,
        ) -> AmResult<()> {
            self.command(format!("coin_pay({},{},{player_index},{coin})", aime_id.id, access_code_text(&access_code.0)))
        }
        fn amount_pay(
            &mut self,
            aime_id: AimeId,
            access_code: &AccessCode,
            item_id: &[u16],
            item_count: u32,
            option_number: i64,
        ) -> AmResult<()> {
            self.command(format!(
                "amount_pay({},{},{},{item_count},{option_number})",
                aime_id.id,
                access_code_text(&access_code.0),
                String::from_utf16_lossy(item_id)
            ))
        }
        fn take_events(&mut self) -> Vec<AimePayEvent> {
            std::mem::take(&mut self.libam.borrow_mut().events)
        }
    }

    fn wide<const N: usize>(text: &str) -> [u16; N] {
        let mut units = [0; N];
        for (slot, unit) in units.iter_mut().zip(text.encode_utf16()) {
            *slot = unit;
        }
        units
    }

    fn libam_up() -> Rc<RefCell<Libam>> {
        Rc::new(RefCell::new(Libam { up: true, shared_up: true, ..Libam::default() }))
    }

    fn controller(libam: &Rc<RefCell<Libam>>) -> AimePayController {
        AimePayController::new(Box::new(FakePort::new(libam)))
    }

    fn slot(libam: &Rc<RefCell<Libam>>) -> ControllerSlot {
        ControllerSlot::new(Box::new(controller(libam)))
    }

    /// One App frame for the controller: libam events, `preExecute`, `postExecute`.
    fn tick(slot: &mut ControllerSlot, harness: &mut Harness) {
        let mut ctx = harness.ctx();
        slot.controller_mut().on_libam_events(&mut ctx);
        slot.pre_execute(&mut ctx);
        slot.post_execute(&mut ctx);
    }

    /// The client writes a request (what the IPC exchange between pre and post delivers).
    fn submit(harness: &mut Harness, generation: u32, task: u32, info: &[u8]) {
        let mut payload = [0; 0x38];
        payload[..info.len()].copy_from_slice(info);
        let patch = ClientAimePayPatch { generation: Some(generation), task: Some(task), info: Some(&payload) };
        harness.client.aime_pay_mut().copy_from(&patch).expect("plain fields");
    }

    fn operation(harness: &Harness) -> ServerOperationRef<'_> {
        harness.server.aime_pay().operation()
    }

    fn deal_result(harness: &Harness) -> ServerDealResultRef<'_> {
        harness.server.aime_pay().operation().deal_result()
    }

    fn set_operation(harness: &mut Harness, patch: &ServerOperationPatch<'_>) {
        update_operation(&mut harness.server, patch);
    }

    fn text(units: &U16CStr) -> String {
        String::from_utf16_lossy(units.as_slice())
    }

    /// Converts `deal` into the operation's deal result.
    fn convert(harness: &mut Harness, deal: &AimePayDeal) {
        convert_deal(deal, &mut harness.server.aime_pay_mut().operation_mut().deal_result_mut());
    }

    /// `(date, amount, count)` of one summary slot.
    fn summary_slot(harness: &Harness, index: usize) -> (i64, u32, u32) {
        let summary = harness.server.aime_pay().deal_summary();
        let item = summary.items().get(index).expect("one slot per summary index");
        (item.date(), item.amount(), item.count())
    }

    fn param_info(param: &str) -> Vec<u8> {
        param.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    fn coin_pay_info(aime_id: u32, code: [u8; 10], player: u32, coin: u32) -> Vec<u8> {
        let mut info = aime_id.to_le_bytes().to_vec();
        info.extend(code);
        info.extend([0, 0]);
        info.extend(player.to_le_bytes());
        info.extend(coin.to_le_bytes());
        info
    }

    fn amount_pay_info(aime_id: u32, code: [u8; 10], item: &str, count: u32, option: i64) -> Vec<u8> {
        let mut info = aime_id.to_le_bytes().to_vec();
        info.extend(code);
        let mut item_id = [0u8; 28];
        for (chunk, unit) in item_id.chunks_exact_mut(2).zip(item.encode_utf16()) {
            chunk.copy_from_slice(&unit.to_le_bytes());
        }
        info.extend(item_id);
        info.extend([0, 0]);
        info.extend(count.to_le_bytes());
        info.extend(option.to_le_bytes());
        info
    }

    fn deal(status: u32, error_code: u32, message: &str) -> AimePayDeal {
        AimePayDeal {
            status,
            time: Tm { sec: 5, min: 4, hour: 3, mday: 2, mon: 0, year: 124, ..Tm::default() },
            error_code,
            error_message: wide(message),
            item_count: 3,
            item_id: wide("ITEM000000001"),
            access_code: CODE,
            receipt_id: 0x1122_3344_5566_7788,
            amount: 500,
            is_coin: false,
        }
    }

    fn code_of(result: AmResult<()>) -> Option<u32> {
        result.err().map(|error| error.code)
    }

    fn calls(libam: &Rc<RefCell<Libam>>) -> Vec<String> {
        libam.borrow().calls.clone()
    }

    #[test]
    fn every_task_forwards_its_payload_and_is_accepted_as_pending() {
        let cases: [(u32, Vec<u8>, &str); 5] = [
            (3, vec![], "activate"),
            (4, vec![], "deactivate"),
            (5, 77u32.to_le_bytes().to_vec(), "query_user_setting(77,0)"),
            (6, coin_pay_info(9, CODE, 2, 5), "coin_pay(9,12345678901234567890,2,5)"),
            (
                7,
                amount_pay_info(9, CODE, "ITEM000000001", 3, -4),
                "amount_pay(9,12345678901234567890,ITEM000000001,3,-4)",
            ),
        ];
        for (task, info, expected) in cases {
            let libam = libam_up();
            let mut controller = controller(&libam);
            let mut harness = Harness::running();
            submit(&mut harness, 1, task, &info);
            assert_eq!(controller.accept(&mut harness.ctx()), RequestState::Pending, "task {task}");
            assert_eq!(calls(&libam), [expected], "task {task}");
            assert!(operation(&harness).busy(), "task {task} starts an operation");
        }
    }

    #[test]
    fn tasks_from_eight_are_rejected_with_code_5_and_reset_busy_without_the_prelude() {
        for task in [8, 9, u32::MAX] {
            let libam = libam_up();
            let mut controller = controller(&libam);
            let mut harness = Harness::running();
            submit(&mut harness, 1, task, &[]);
            assert_eq!(code_of(controller.run_task(&harness.client.aime_pay(), &mut harness.server)), Some(5));
            let pending = ServerOperationPatch {
                busy: Some(true),
                cancellable: Some(true),
                confirm: Some(AimePayConfirm::WithParam.into()),
                ..Default::default()
            };
            set_operation(&mut harness, &pending);
            assert_eq!(controller.accept(&mut harness.ctx()), RequestState::Failed, "task {task}");
            let published = operation(&harness);
            assert!(!published.busy(), "busy is reset after any error");
            assert!(
                published.cancellable() && published.confirm() == AimePayConfirm::WithParam,
                "no prelude for task {task}"
            );
            assert!(libam.borrow().calls.is_empty());
        }
    }

    #[test]
    fn prelude_runs_for_tasks_two_to_seven_and_keeps_the_deal_status() {
        for task in [2, 3, 4, 5, 6, 7] {
            let libam = libam_up();
            let mut controller = controller(&libam);
            let mut harness = Harness::running();
            let error_code = fit_wide_str("9 - NG", 19);
            let item_id = fit_wide_str("ITEM", 14);
            let result = ServerDealResultPatch {
                valid: Some(true),
                coin: Some(true),
                status: Some(3),
                time: Some(5),
                error_code: Some(&error_code),
                access_code: Some(&CODE),
                item_id: Some(&item_id),
                item_count: Some(1),
                amount: Some(2),
                receipt_id: Some(3),
            };
            let dirty = ServerOperationPatch {
                cancellable: Some(true),
                confirm: Some(AimePayConfirm::Accept.into()),
                error_occurred: Some(true),
                deal_available: Some(true),
                user_setting: Some(ServerUserSettingPatch { aime_id: Some(5), ..Default::default() }),
                deal_result: Some(result),
                ..Default::default()
            };
            set_operation(&mut harness, &dirty);
            submit(&mut harness, 1, task, &coin_pay_info(1, CODE, 0, 1));
            controller.accept(&mut harness.ctx());

            let published = operation(&harness);
            assert!(published.busy(), "task {task}");
            // DisplayCheck arms cancellable and UserSetting zeroes the published setting after the prelude.
            assert_eq!(published.cancellable(), task == 2, "task {task}");
            assert!(published.confirm() == AimePayConfirm::None && !published.error_occurred(), "task {task}");
            let expected_aime_id = if task == 5 { 0 } else { 5 };
            assert!(
                published.deal_available() && published.user_setting().aime_id() == expected_aime_id,
                "task {task}"
            );
            let result = published.deal_result();
            assert!(!result.valid() && !result.coin());
            assert_eq!(result.status(), 3, "status is not cleared");
            assert_eq!((result.time(), *result.access_code()), (0, [0; 10]));
            assert_eq!((result.error_code().len(), result.item_id().len()), (0, 0));
            assert_eq!((result.item_count(), result.amount(), result.receipt_id()), (0, 0, 0));
        }
    }

    #[test]
    fn cancel_and_confirm_do_not_start_an_operation() {
        let libam = libam_up();
        let mut controller = controller(&libam);
        let mut harness = Harness::running();
        let question = ServerOperationPatch {
            busy: Some(true),
            cancellable: Some(true),
            confirm: Some(AimePayConfirm::Accept.into()),
            ..Default::default()
        };
        set_operation(&mut harness, &question);

        submit(&mut harness, 1, 1, &param_info("12345"));
        assert_eq!(controller.accept(&mut harness.ctx()), RequestState::Pending);
        let answered = operation(&harness);
        assert_eq!(calls(&libam), ["confirm_accept"], "confirm 1 is answered without the parameter");
        assert!(answered.busy() && !answered.cancellable() && answered.confirm() == AimePayConfirm::None);

        let question = ServerOperationPatch {
            cancellable: Some(true),
            confirm: Some(AimePayConfirm::WithParam.into()),
            ..Default::default()
        };
        set_operation(&mut harness, &question);
        submit(&mut harness, 2, 0, &[]);
        assert_eq!(controller.accept(&mut harness.ctx()), RequestState::Pending);
        let cancelled = operation(&harness);
        assert_eq!(calls(&libam), ["confirm_accept", "cancel"]);
        assert!(cancelled.busy() && !cancelled.cancellable() && cancelled.confirm() == AimePayConfirm::None);
    }

    #[test]
    fn confirm_with_param_passes_the_five_characters_and_keeps_the_question_on_failure() {
        let libam = libam_up();
        let mut controller = controller(&libam);
        let mut harness = Harness::running();
        let question = ServerOperationPatch {
            confirm: Some(AimePayConfirm::WithParam.into()),
            cancellable: Some(true),
            ..Default::default()
        };
        set_operation(&mut harness, &question);
        libam.borrow_mut().command_error = Some(1);
        submit(&mut harness, 1, 1, &param_info("12345"));
        assert_eq!(controller.accept(&mut harness.ctx()), RequestState::Failed);
        assert_eq!(calls(&libam), ["confirm_with_param(12345)"]);
        let kept = operation(&harness);
        assert!(kept.confirm() == AimePayConfirm::WithParam && kept.cancellable());

        libam.borrow_mut().command_error = None;
        submit(&mut harness, 2, 1, &param_info("67890"));
        assert_eq!(controller.accept(&mut harness.ctx()), RequestState::Pending);
        let answered = operation(&harness);
        assert!(answered.confirm() == AimePayConfirm::None && !answered.cancellable());
    }

    #[test]
    fn display_check_and_cancel_during_it() {
        let libam = libam_up();
        let mut controller = controller(&libam);
        let mut harness = Harness::running();
        submit(&mut harness, 1, 2, &[]);
        assert_eq!(controller.accept(&mut harness.ctx()), RequestState::Pending);
        assert!(operation(&harness).busy() && operation(&harness).cancellable());

        submit(&mut harness, 2, 0, &[]);
        controller.accept(&mut harness.ctx());
        assert_eq!(calls(&libam), ["display_start_test", "display_cancel"]);
        assert!(!operation(&harness).cancellable());

        controller.on_display_complete(&mut harness.ctx());
        assert!(!operation(&harness).busy());
        set_operation(&mut harness, &ServerOperationPatch { busy: Some(true), ..Default::default() });
        controller.on_display_complete(&mut harness.ctx());
        assert!(operation(&harness).busy(), "no display test running any more");
    }

    #[test]
    fn display_completion_keeps_cancellable_and_failed_display_test_is_not_armed() {
        let libam = libam_up();
        let mut controller = controller(&libam);
        let mut harness = Harness::running();
        submit(&mut harness, 1, 2, &[]);
        controller.accept(&mut harness.ctx());
        controller.on_display_complete(&mut harness.ctx());
        assert!(operation(&harness).cancellable(), "the callback does not clear cancellable");

        libam.borrow_mut().command_error = Some(6);
        submit(&mut harness, 2, 2, &[]);
        assert_eq!(controller.accept(&mut harness.ctx()), RequestState::Failed);
        set_operation(&mut harness, &ServerOperationPatch { busy: Some(true), ..Default::default() });
        controller.on_display_complete(&mut harness.ctx());
        assert!(operation(&harness).busy());
    }

    #[test]
    fn user_setting_request_zeroes_the_published_setting_on_success_only() {
        let libam = libam_up();
        let mut controller = controller(&libam);
        let mut harness = Harness::running();
        let setting =
            ServerUserSettingPatch { aime_id: Some(5), credit_card_available: Some(true), aime_hold_skip: Some(true) };
        set_operation(&mut harness, &ServerOperationPatch { user_setting: Some(setting), ..Default::default() });
        libam.borrow_mut().command_error = Some(1);
        submit(&mut harness, 1, 5, &9u32.to_le_bytes());
        assert_eq!(controller.accept(&mut harness.ctx()), RequestState::Failed);
        assert_eq!(operation(&harness).user_setting().aime_id(), 5);

        libam.borrow_mut().command_error = None;
        submit(&mut harness, 2, 5, &9u32.to_le_bytes());
        assert_eq!(controller.accept(&mut harness.ctx()), RequestState::Pending);
        let setting = operation(&harness).user_setting();
        assert_eq!((setting.aime_id(), setting.credit_card_available(), setting.aime_hold_skip()), (0, false, false));
    }

    #[test]
    fn invalid_access_code_reaches_libam_as_the_unset_code_and_libam_decides() {
        // `0x14001A6E4` validates, `0x14001A711` calls libam unconditionally; libam overwrites the Result.
        let libam = libam_up();
        let mut controller = controller(&libam);
        let mut harness = Harness::running();
        let mut bad = CODE;
        bad[3] = 0xAB;
        submit(&mut harness, 1, 6, &coin_pay_info(1, bad, 0, 1));
        assert_eq!(controller.accept(&mut harness.ctx()), RequestState::Pending);
        submit(&mut harness, 2, 7, &amount_pay_info(1, bad, "ITEM000000001", 1, 0));
        assert_eq!(controller.accept(&mut harness.ctx()), RequestState::Pending);
        assert_eq!(
            calls(&libam),
            ["coin_pay(1,FFFFFFFFFFFFFFFFFFFF,0,1)", "amount_pay(1,FFFFFFFFFFFFFFFFFFFF,ITEM000000001,1,0)"]
        );

        libam.borrow_mut().command_error = Some(1);
        submit(&mut harness, 3, 6, &coin_pay_info(1, bad, 0, 1));
        assert_eq!(controller.accept(&mut harness.ctx()), RequestState::Failed);
        assert!(!operation(&harness).busy());
    }

    #[test]
    fn accepted_request_is_acknowledged_one_tick_later_and_failed_one_too() {
        let libam = libam_up();
        let mut slot = slot(&libam);
        let mut harness = Harness::running();
        tick(&mut slot, &mut harness);
        submit(&mut harness, 1, 3, &[]);
        tick(&mut slot, &mut harness);
        assert_eq!(calls(&libam), ["activate"]);
        assert!(slot.controller().is_busy(&harness.ctx()));
        assert_eq!(harness.server.aime_pay().response().generation(), 0);
        tick(&mut slot, &mut harness);
        assert!(!slot.controller().is_busy(&harness.ctx()));
        assert_eq!(harness.server.aime_pay().response().generation(), 1);
        assert!(harness.server.aime_pay().response().succeeded());
        assert!(operation(&harness).busy(), "the operation itself is still running");

        libam.borrow_mut().command_error = Some(3);
        submit(&mut harness, 2, 4, &[]);
        tick(&mut slot, &mut harness);
        tick(&mut slot, &mut harness);
        assert_eq!(harness.server.aime_pay().response().generation(), 2);
        assert!(!harness.server.aime_pay().response().succeeded());
    }

    #[test]
    fn clear_publishes_availability_and_the_deal_history() {
        let libam = libam_up();
        libam.borrow_mut().enabled = true;
        libam.borrow_mut().deals = vec![deal(2, 0, ""), deal(0, 7, "NG")];
        let mut controller = controller(&libam);
        let mut harness = Harness::running();
        let stale = ServerDealResultPatch { status: Some(9), ..Default::default() };
        let mut aime_pay = harness.server.aime_pay_mut();
        let mut history = aime_pay.deal_result_mut();
        let mut items = history.items_mut();
        items.get_mut(3).expect("one slot per history index").copy_from(&stale).expect("scalars fit");
        controller.clear_server_data(&mut harness.ctx());
        let aime_pay = harness.server.aime_pay();
        assert!(aime_pay.available());
        let history = aime_pay.deal_result();
        assert_eq!(history.count(), 2);
        let items = history.items();
        let valid: Vec<bool> = items.iter().take(3).map(|item| item.valid()).collect();
        assert_eq!(valid, [true, true, false]);
        assert_eq!(items.get(3).expect("slot").status(), 9, "the slot clear keeps the status");

        libam.borrow_mut().up = false;
        libam.borrow_mut().enabled = false;
        controller.clear_server_data(&mut harness.ctx());
        assert!(harness.server.aime_pay().available());
    }

    #[test]
    fn deal_conversion_maps_status_error_text_code_amount_and_strings() {
        let mut harness = Harness::new();
        let mut item = deal(2, 0, "");
        item.is_coin = true;
        convert(&mut harness, &item);
        let result = deal_result(&harness);
        assert!(result.valid() && result.coin());
        assert_eq!(result.status(), 3, "libam 2 → 3");
        assert_eq!(result.time(), civil(2024, 1, 2, 3, 4, 5).0);
        assert_eq!(result.error_code().len(), 0, "no error text for a good deal");
        assert_eq!(*result.access_code(), CODE);
        assert_eq!(text(result.item_id()), "ITEM000000001");
        assert_eq!((result.item_count(), result.amount(), result.receipt_id()), (3, 500, 0x1122_3344_5566_7788));

        let statuses: Vec<u32> = (0..5)
            .map(|status| {
                convert(&mut harness, &deal(status, 1, ""));
                deal_result(&harness).status()
            })
            .collect();
        assert_eq!(statuses, [1, 0, 3, 2, 0]);
    }

    #[test]
    fn deal_error_text_is_code_dash_message_cut_to_eighteen_characters() {
        let mut harness = Harness::new();
        convert(&mut harness, &deal(0, 0, ""));
        assert_eq!(text(deal_result(&harness).error_code()), "0 - ", "status 0 always carries the text");
        convert(&mut harness, &deal(2, 5, "Declined"));
        assert_eq!(text(deal_result(&harness).error_code()), "5 - Declined");
        convert(&mut harness, &deal(2, 123_456, "ABCDEFGHIJKLMNO"));
        assert_eq!(text(deal_result(&harness).error_code()), "123456 - ABCDEFGHI");
        assert_eq!(text(deal_result(&harness).error_code()).len(), 18);
    }

    #[test]
    fn deal_conversion_zeroes_bad_access_codes_and_maps_the_unknown_amount() {
        let mut harness = Harness::new();
        let mut item = deal(2, 0, "");
        item.access_code[0] = 0xFF;
        item.amount = u32::MAX;
        item.item_id = [0; 14];
        item.time = Tm::default();
        convert(&mut harness, &item);
        let result = deal_result(&harness);
        assert_eq!(*result.access_code(), [0; 10]);
        assert_eq!(result.amount(), -1);
        assert_eq!(result.item_id().len(), 0);
        assert_eq!(result.time(), 0);
    }

    #[test]
    fn update_publishes_locations_availability_and_the_summary() {
        let libam = libam_up();
        libam.borrow_mut().deal_available = true;
        libam.borrow_mut().summary = vec![
            AimePaySummaryItem {
                date: Tm { sec: 0, min: 0, hour: 0, mday: 3, mon: 4, year: 124, ..Tm::default() },
                count: 2,
                amount: 900,
            },
            AimePaySummaryItem { date: Tm::default(), count: 1, amount: 100 },
        ];
        let mut port = FakePort::new(&libam);
        port.activated = AimePayLocation {
            valid: true,
            name: Some(wide(&"n".repeat(30))),
            company_name: Some(wide(&"c".repeat(60))),
        };
        port.current = AimePayLocation { valid: false, name: None, company_name: None };
        let mut controller = AimePayController::new(Box::new(port));
        let mut harness = Harness::running();
        let stale = fit_wide_str("stale", 21);
        let stale_location = ServerLocationPatch { name: Some(&stale), ..Default::default() };
        let mut aime_pay = harness.server.aime_pay_mut();
        aime_pay.current_location_mut().copy_from(&stale_location).expect("fitted name");
        let mut summary = aime_pay.deal_summary_mut();
        let mut items = summary.items_mut();
        let stale_slot = ServerDealSummaryPatch { amount: Some(7), ..Default::default() };
        items.get_mut(4).expect("one slot per summary index").copy_from(&stale_slot).expect("scalars fit");
        controller.update_server_data(&mut harness.ctx());

        let aime_pay = harness.server.aime_pay();
        assert!(aime_pay.activated_location().valid());
        assert_eq!(text(aime_pay.activated_location().name()), "n".repeat(20));
        assert_eq!(text(aime_pay.activated_location().company_name()), "c".repeat(40));
        assert!(!aime_pay.current_location().valid());
        assert_eq!(
            (aime_pay.current_location().name().len(), aime_pay.current_location().company_name().len()),
            (0, 0)
        );
        assert!(aime_pay.operation().deal_available());
        assert_eq!(aime_pay.deal_summary().count(), 2);
        assert_eq!(summary_slot(&harness, 0), (civil(2024, 5, 3, 0, 0, 0).0, 900, 2));
        assert_eq!(summary_slot(&harness, 1), (0, 100, 1));
        assert_eq!(summary_slot(&harness, 4), (0, 0, 0), "slots are rebuilt");
    }

    #[test]
    fn update_with_libam_down_skips_check_result() {
        let libam = libam_up();
        let mut slot = slot(&libam);
        let mut harness = Harness::running();
        tick(&mut slot, &mut harness);
        submit(&mut harness, 1, 3, &[]);
        tick(&mut slot, &mut harness);
        libam.borrow_mut().up = false;
        tick(&mut slot, &mut harness);
        assert_eq!(harness.server.aime_pay().response().generation(), 0, "pending requests stay pending");
        libam.borrow_mut().up = true;
        tick(&mut slot, &mut harness);
        assert_eq!(harness.server.aime_pay().response().generation(), 1);
    }

    #[test]
    fn need_acceptation_listener_publishes_the_question_and_allows_cancel() {
        let libam = libam_up();
        let controller = controller(&libam);
        let mut harness = Harness::running();
        controller.handle_event(AimePayEvent::NeedAcceptation { question: 2 }, &mut harness.ctx());
        let published = operation(&harness);
        assert_eq!((published.confirm(), published.cancellable()), (AimePayConfirm::WithParam, true));
        controller.handle_event(AimePayEvent::NeedAcceptation { question: 1 }, &mut harness.ctx());
        assert_eq!(operation(&harness).confirm(), AimePayConfirm::Accept);

        controller.handle_event(AimePayEvent::NeedAcceptation { question: 3 }, &mut harness.ctx());
        assert_eq!(operation(&harness).confirm(), AimePayConfirm::None);
        assert!(operation(&harness).cancellable());
    }

    #[test]
    fn deal_result_listener_converts_the_deal_and_flags_libam_status_one() {
        let libam = libam_up();
        let controller = controller(&libam);
        let mut harness = Harness::running();
        controller.handle_event(AimePayEvent::NotifyDealResult { task: 5, deal: deal(1, 3, "NG") }, &mut harness.ctx());
        assert!(operation(&harness).error_occurred());
        let result = deal_result(&harness);
        assert!(result.valid());
        assert_eq!(result.status(), 0, "libam 1 has no wire status");
        assert_eq!(text(result.error_code()), "3 - NG");
        controller.handle_event(AimePayEvent::NotifyDealResult { task: 5, deal: deal(2, 0, "") }, &mut harness.ctx());
        assert!(!operation(&harness).error_occurred());
    }

    #[test]
    fn complete_task_listener_ends_the_operation_and_refreshes_history_and_user_setting() {
        let libam = libam_up();
        libam.borrow_mut().deals = vec![deal(2, 0, "")];
        libam.borrow_mut().user_setting =
            Some(AimePayUserSetting { aime_id: 42, credit_card_available: true, aime_hold_skip: false });
        let controller = controller(&libam);
        let mut harness = Harness::running();
        set_operation(&mut harness, &ServerOperationPatch { busy: Some(true), ..Default::default() });
        controller.handle_event(AimePayEvent::CompleteTask { task: 2, error_kind: 1 }, &mut harness.ctx());
        let aime_pay = harness.server.aime_pay();
        assert!(aime_pay.operation().error_occurred() && !aime_pay.operation().busy());
        assert_eq!(aime_pay.deal_result().count(), 1);
        assert_eq!(aime_pay.operation().user_setting().aime_id(), 0, "only QueryUserSetting refreshes the setting");

        controller.handle_event(AimePayEvent::CompleteTask { task: 1, error_kind: 0 }, &mut harness.ctx());
        let setting = operation(&harness).user_setting();
        assert!(!operation(&harness).error_occurred());
        assert_eq!((setting.aime_id(), setting.credit_card_available(), setting.aime_hold_skip()), (42, true, false));

        libam.borrow_mut().user_setting = None;
        controller.handle_event(AimePayEvent::CompleteTask { task: 1, error_kind: 0 }, &mut harness.ctx());
        assert_eq!(operation(&harness).user_setting().aime_id(), 0, "invalid setting reads as zero");
    }

    #[test]
    fn confirm_async_flow_from_acceptation_to_completion() {
        let libam = libam_up();
        let mut slot = slot(&libam);
        let mut harness = Harness::running();
        tick(&mut slot, &mut harness);
        submit(&mut harness, 1, 7, &amount_pay_info(1, CODE, "ITEM000000001", 1, 0));
        tick(&mut slot, &mut harness);
        tick(&mut slot, &mut harness);
        assert!(operation(&harness).busy());

        libam.borrow_mut().events = vec![AimePayEvent::NeedAcceptation { question: 2 }];
        tick(&mut slot, &mut harness);
        assert_eq!(operation(&harness).confirm(), AimePayConfirm::WithParam);

        submit(&mut harness, 2, 1, &param_info("12345"));
        tick(&mut slot, &mut harness);
        tick(&mut slot, &mut harness);
        assert_eq!(calls(&libam).last().map(String::as_str), Some("confirm_with_param(12345)"));
        assert_eq!(operation(&harness).confirm(), AimePayConfirm::None);
        assert!(operation(&harness).busy(), "libam has not completed the task yet");

        libam.borrow_mut().deals = vec![deal(2, 0, "")];
        libam.borrow_mut().events = vec![
            AimePayEvent::NotifyDealResult { task: 4, deal: deal(2, 0, "") },
            AimePayEvent::CompleteTask { task: 4, error_kind: 0 },
        ];
        tick(&mut slot, &mut harness);
        let aime_pay = harness.server.aime_pay();
        assert!(!aime_pay.operation().busy() && !aime_pay.operation().error_occurred());
        assert!(aime_pay.operation().deal_result().valid());
        assert_eq!(aime_pay.deal_result().count(), 1);
    }
}
