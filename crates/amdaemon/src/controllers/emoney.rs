//! `amdaemon::server::EMoneyController` (vtable `0x1405CE178`, ctor `0x14002EF30`, `App+0xF30`).
//!
//! Publishes the libam e-money module into `ServerData+0x1C68` and executes the e-money requests
//! (`ClientData+0xC84`). Behaviour: `spec/controllers/credit_emoney_aimepay.md` §3 and the shared machinery
//! in §1; claims `credit_emoney_aimepay.emoney.*`.
//!
//! The response only acknowledges submission (checker constant `2`). Completion of a payment or display test
//! is reported through the published `operation` state, driven by the libam listeners: [`EMoneyEvent`]s are
//! applied in [`Controller::on_libam_events`], the display-complete hub callback in
//! [`Controller::on_display_complete`].

use std::collections::VecDeque;

use widestring::U16CStr;

use crate::executor::{RequestExecutor, RequestState};
use crate::schema::emoney::{
    BRAND_COUNT, ClientEMoneyRef, DEAL_RESULT_CAPACITY, EMoneyBrandId, EMoneyTask, ServerBrandMut, ServerBrandPatch,
    ServerDealResultDataMut, ServerEMoneyMut, ServerEMoneyPatch, ServerOperationPatch, ServerResultMut,
    ServerResultPatch, ServerSoundPatch,
};
use crate::schema::image::ServerImage;
use crate::schema::text::fit_wide;
use libam_api::emoney::{
    EMoneyAppConfig, EMoneyBrandInfo, EMoneyDealInfo, EMoneyEvent, EMoneyPort, LibamBrand, non_empty,
};
use libam_api::result::{AmError, AmResult};

use crate::controllers::controller::{Controller, ControllerId, Ctx, commit_check};
use libam_api::config::{AppConfig, ProcessConfig};

/// libam e-money task id of the AmountPay sequencer (`byte[seq+0x60] = 0`, `0x14026EAD0`). CoinPay is
/// task 5, so only AmountPay can raise the pay-completion event.
const LIBAM_TASK_AMOUNT_PAY: u32 = 4;
/// libam deal status of a deal that needs the client's completion (`payCompletion`).
const LIBAM_DEAL_STATUS_INCOMPLETE: u32 = 4;

/// amdaemon → libam brand for a known brand id (`0x140052360`). amdaemon's [`EMoneyBrandId`] orders the
/// brands differently from libam's numbering.
const fn brand_from_id(id: EMoneyBrandId) -> LibamBrand {
    LibamBrand(match id {
        EMoneyBrandId::Nanaco => 1,
        EMoneyBrandId::Edy => 2,
        EMoneyBrandId::Transport => 4,
        EMoneyBrandId::Waon => 5,
        EMoneyBrandId::Paseli => 6,
        EMoneyBrandId::Id => 3,
        EMoneyBrandId::Sapica => 7,
    })
}

/// libam → amdaemon brand (`EMoneyBrand::convert`, `0x1400523D0`). Libam brand `1` (nanaco) is `0`
/// silently; any other unknown value (including `0`) is logged and also becomes nanaco.
fn brand_to_wire(brand: LibamBrand) -> EMoneyBrandId {
    match brand.0 {
        1 => EMoneyBrandId::Nanaco,
        2 => EMoneyBrandId::Edy,
        3 => EMoneyBrandId::Id,
        4 => EMoneyBrandId::Transport,
        5 => EMoneyBrandId::Waon,
        6 => EMoneyBrandId::Paseli,
        7 => EMoneyBrandId::Sapica,
        _ => {
            tracing::warn!(brand = brand.0, "invalid emoney brand id");
            EMoneyBrandId::Nanaco
        }
    }
}

/// Clears a result like `0x140030300` (minus the operation flags): every field but the padding is written.
/// The strings only get their terminator, so the units behind it keep their previous values, as in the
/// original.
fn clear_result(result: &mut ServerResultMut<'_>) {
    let patch = ServerResultPatch {
        valid: Some(false),
        status: Some(0),
        time: Some(0),
        brand_id: Some(EMoneyBrandId::Nanaco.into()),
        deal_number: Some(<&U16CStr>::default()),
        card_number: Some(<&U16CStr>::default()),
        amount: Some(0),
        balance_before: Some(0),
        balance_after: Some(0),
    };
    result.copy_from(&patch).expect("scalars and the empty string fit");
}

/// `convertDeal(DealInfo → ServerResult)` (`0x14002FCD0`).
fn convert_deal(deal: &EMoneyDealInfo, result: &mut ServerResultMut<'_>) {
    let deal_number = non_empty(&deal.deal_number).map(|number| fit_wide(number, result.deal_number_mut().capacity()));
    let card_number = non_empty(&deal.card_number).map(|number| fit_wide(number, result.card_number_mut().capacity()));
    let patch = ServerResultPatch {
        valid: Some(true),
        status: Some(match deal.status {
            0 => 1,
            2 => 4,
            3 => 2,
            4 => 3,
            _ => 0,
        }),
        time: deal.time.to_date_time().map(|time| time.0),
        brand_id: Some(brand_to_wire(deal.brand).into()),
        deal_number: deal_number.as_deref(),
        card_number: card_number.as_deref(),
        amount: Some(deal.amount),
        balance_before: Some(deal.balance_before),
        balance_after: Some(deal.balance_after),
    };
    result.copy_from(&patch).expect("fitted strings and scalars fit");
}

/// libam deal statuses that mean "error" (`((s - 1) & ~2) == 0`).
const fn is_error_status(status: u32) -> bool {
    matches!(status, 1 | 3)
}

/// Updates the published operation flags (`SD+0x2A88`).
fn update_operation(server: &mut ServerImage, patch: &ServerOperationPatch<'_>) {
    server.emoney_mut().operation_mut().copy_from(patch).expect("flags accept every value");
}

/// `convert(DealLog → ServerDealResultData)` (`0x14002FDE0`): rebuilds the whole history, newest first.
fn convert_deal_log(port: &dyn EMoneyPort, history: &mut ServerDealResultDataMut<'_>) {
    let mut count = 0;
    let mut items = history.items_mut();
    for index in 0..DEAL_RESULT_CAPACITY {
        let mut slot = items.get_mut(index).expect("one slot per history index");
        clear_result(&mut slot);
        if index >= port.deal_log_count() {
            continue;
        }
        match port.deal_log_item(index) {
            Some(deal) => {
                convert_deal(&deal, &mut slot);
                count += 1;
            }
            None => tracing::warn!("libamw version does not match"),
        }
    }
    history.count_mut().set(count).expect("u32 accepts every value");
}

/// `EMoneyController`.
pub struct EMoneyController {
    port: Box<dyn EMoneyPort>,
    executor: RequestExecutor,
    /// `std::deque<std::wstring>` filled by the `PlaySoundEventListener` (unbounded).
    sounds: VecDeque<Vec<u16>>,
    /// `controller+0xE0`: a deal result (other than the incomplete AmountPay one) was delivered.
    deal_result_seen: bool,
    /// `controller+0xE1`: libam completed the task.
    complete_task_seen: bool,
    /// `controller+0x100`: pay completion was pending at the previous update.
    last_pay_pending: bool,
    /// `controller+0x108`: a display test started by DisplayCheck is running.
    display_check_active: bool,
}

impl std::fmt::Debug for EMoneyController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EMoneyController")
            .field("executor", &self.executor)
            .field("queued_sounds", &self.sounds.len())
            .field("deal_result_seen", &self.deal_result_seen)
            .field("complete_task_seen", &self.complete_task_seen)
            .field("last_pay_pending", &self.last_pay_pending)
            .field("display_check_active", &self.display_check_active)
            .finish_non_exhaustive()
    }
}

impl EMoneyController {
    /// Constructs the controller (ctor `0x14002EF30`).
    pub fn new(port: Box<dyn EMoneyPort>) -> Self {
        Self {
            port,
            executor: RequestExecutor::default(),
            sounds: VecDeque::new(),
            deal_result_seen: false,
            complete_task_seen: false,
            last_pay_pending: false,
            display_check_active: false,
        }
    }

    /// Steps 1–6 of `updateServerData`: terminal, brands, deal history and operation flags.
    fn publish_terminal_state(&self, emoney: &mut ServerEMoneyMut<'_>) {
        let port = &*self.port;
        let id = port.terminal_id();
        let terminal_id =
            fit_wide(id.as_ref().map(|id| id.as_slice()).unwrap_or_default(), emoney.terminal_id_mut().capacity());
        let serial = port.terminal_serial();
        let terminal_serial = fit_wide(
            serial.as_ref().map(|serial| serial.as_slice()).unwrap_or_default(),
            emoney.terminal_serial_mut().capacity(),
        );
        let patch = ServerEMoneyPatch {
            terminal_id: Some(&terminal_id),
            terminal_serial: Some(&terminal_serial),
            ..ServerEMoneyPatch::default()
        };
        emoney.copy_from(&patch).expect("fitted strings fit");

        let mut brand_ids = [EMoneyBrandId::Nanaco; BRAND_COUNT];
        let mut brand_count = 0;
        for index in 0..port.brand_count() {
            let Some(brand) = port.brand(index) else {
                break;
            };
            if brand == LibamBrand::NONE {
                continue;
            }
            brand_ids[usize::from(brand_count)] = brand_to_wire(brand);
            brand_count += 1;
            if usize::from(brand_count) >= BRAND_COUNT {
                break;
            }
        }
        let patch = ServerEMoneyPatch {
            available_brand_count: Some(brand_count),
            available_brand_ids: Some(brand_ids),
            ..ServerEMoneyPatch::default()
        };
        emoney.copy_from(&patch).expect("scalars and enums accept every value");

        let mut brands = emoney.brands_mut();
        for (index, id) in EMoneyBrandId::ALL.into_iter().enumerate() {
            let mut slot = brands.get_mut(index).expect("one slot per brand");
            publish_brand(port, brand_from_id(id), &mut slot);
        }

        convert_deal_log(port, &mut emoney.deal_result_mut());
        let patch = ServerEMoneyPatch {
            operation: Some(ServerOperationPatch {
                deal_available: Some(port.deal_available()),
                can_operate_deal: Some(port.can_operate(LIBAM_TASK_AMOUNT_PAY)),
                ..ServerOperationPatch::default()
            }),
            alive: Some(port.alive()),
            ..ServerEMoneyPatch::default()
        };
        emoney.copy_from(&patch).expect("flags accept every value");
    }

    /// Step 7: one queued sound per tick, once the client acknowledged the previous one.
    fn publish_sound(&mut self, ctx: &mut Ctx<'_>) {
        let mut emoney = ctx.server.emoney_mut();
        let generation = emoney.sound().generation();
        if generation != ctx.client.emoney().sound_generation() {
            return;
        }
        let Some(path) = self.sounds.pop_front() else {
            return;
        };
        let mut sound = emoney.sound_mut();
        let file_path = fit_wide(&path, sound.file_path_mut().capacity());
        let patch = ServerSoundPatch { generation: Some(generation.wrapping_add(1)), file_path: Some(&file_path) };
        sound.copy_from(&patch).expect("fitted path and generation fit");
    }

    /// Step 8: notifies the pay-completion observers when `pending` changed. The only observer registered
    /// in the constructor is [`Self::complete_pay`] (`0x14002FAD0`).
    fn notify_pay_completion(&mut self, ctx: &mut Ctx<'_>) {
        let pending =
            ctx.client.emoney().pay_completion_generation() != ctx.server.emoney().pay_completion().generation();
        if pending != self.last_pay_pending {
            self.complete_pay(pending, ctx);
        }
        self.last_pay_pending = pending;
    }

    /// `completePay(const bool& pending)` (`0x14002FAD0`): acts when the client acknowledged the latest
    /// pay-completion generation.
    fn complete_pay(&mut self, pending: bool, ctx: &mut Ctx<'_>) {
        if pending {
            return;
        }
        self.deal_result_seen = true;
        if self.port.handle().is_err() {
            tracing::warn!("libam is not ready");
            return;
        }
        let result = self.port.complete_pay();
        if result.is_err() {
            tracing::warn!("libam is not ready");
        }
        if self.complete_task_seen {
            convert_deal_log(&*self.port, &mut ctx.server.emoney_mut().deal_result_mut());
            let patch = ServerOperationPatch { busy: Some(false), held_over: Some(false), ..Default::default() };
            update_operation(ctx.server, &patch);
        }
        if result.is_ok() {
            ctx.server.emoney_mut().operation_mut().result_mut().status_mut().set(1).expect("u32 accepts every value");
        }
    }

    /// Applies one libam listener event (`0x14002EC50`–`0x14002EE10`).
    fn handle_event(&mut self, event: EMoneyEvent, ctx: &mut Ctx<'_>) {
        match event {
            EMoneyEvent::CompleteTask { task, status } => {
                self.complete_task_seen = true;
                let finished = task != LIBAM_TASK_AMOUNT_PAY || self.deal_result_seen;
                let patch = ServerOperationPatch {
                    error_occurred: Some(is_error_status(status)),
                    busy: finished.then_some(false),
                    held_over: finished.then_some(false),
                    ..Default::default()
                };
                update_operation(ctx.server, &patch);
            }
            EMoneyEvent::NotifyDealResult { task, deal } => {
                let mut emoney = ctx.server.emoney_mut();
                convert_deal(&deal, &mut emoney.operation_mut().result_mut());
                if task == LIBAM_TASK_AMOUNT_PAY && deal.status == LIBAM_DEAL_STATUS_INCOMPLETE {
                    let generation = emoney.pay_completion().generation().wrapping_add(1);
                    emoney.pay_completion_mut().generation_mut().set(generation).expect("u32 accepts every value");
                } else {
                    self.deal_result_seen = true;
                }
                let error = is_error_status(deal.status);
                emoney.operation_mut().error_occurred_mut().set(error).expect("bool accepts every value");
            }
            EMoneyEvent::PlaySound { path } => self.sounds.push_back(path.unwrap_or_default()),
            EMoneyEvent::NotifyCancel { cancellable } => {
                update_operation(
                    ctx.server,
                    &ServerOperationPatch { cancellable: Some(cancellable), ..Default::default() },
                );
            }
            EMoneyEvent::HoldCard => {
                update_operation(ctx.server, &ServerOperationPatch { held_over: Some(true), ..Default::default() });
            }
        }
    }

    /// `RequestExecutor::checkResult` tail of `update`.
    fn check_result(&mut self, ctx: &mut Ctx<'_>) {
        let mut emoney = ctx.server.emoney_mut();
        let outcome =
            self.executor.check_result(ctx.client.emoney().generation(), emoney.response().generation(), || {
                RequestState::Succeeded
            });
        commit_check(outcome, &mut emoney.response_mut());
    }

    /// Executes the request task and returns the `am::Result` the original ends up with.
    ///
    /// A payload that fails its proof (unknown brand, item id not terminated within its 14 units) is
    /// answered with `Result` code 1 without calling libam: libam rejects the brand `0` an unknown id maps to
    /// and an item id that is not exactly 13 characters with the same code.
    fn run_task(&mut self, request: &ClientEMoneyRef<'_>, server: &mut ServerImage) -> AmResult<()> {
        let invalid_payload = || AmError::code(AmError::INVALID_ARGUMENT);
        match request.task_kind() {
            Some(EMoneyTask::Cancel) => {
                if self.display_check_active {
                    self.port.display_cancel();
                    update_operation(server, &ServerOperationPatch { cancellable: Some(false), ..Default::default() });
                } else {
                    self.port.cancel();
                }
                Ok(())
            }
            Some(EMoneyTask::DisplayCheck) => {
                self.port.display_start_test()?;
                self.display_check_active = true;
                update_operation(server, &ServerOperationPatch { cancellable: Some(true), ..Default::default() });
                Ok(())
            }
            Some(EMoneyTask::TerminalAuth) => self.port.terminal_auth(),
            Some(EMoneyTask::TerminalRemove) => self.port.terminal_remove(),
            Some(EMoneyTask::Balance) => {
                let info = request.balance_info().ok_or_else(invalid_payload)?;
                self.port.balance(brand_from_id(info.brand_id()))
            }
            Some(EMoneyTask::AmountPay) => {
                let info = request.amount_pay_info().ok_or_else(invalid_payload)?;
                let brand = brand_from_id(info.brand_id());
                self.port.amount_pay(info.amount(), info.item_count(), brand, info.item_id().as_slice())
            }
            Some(EMoneyTask::CoinPay) => {
                let info = request.coin_pay_info().ok_or_else(invalid_payload)?;
                self.port.coin_pay(info.player_index(), info.coin(), brand_from_id(info.brand_id()))
            }
            None => Err(AmError::with_message(AmError::UNKNOWN_TASK, "Requested unknown task.")),
        }
    }

    /// `EMoneyController::accept` (`0x14002F720`): the acceptor of the executor.
    fn accept(&mut self, ctx: &mut Ctx<'_>) -> RequestState {
        let client = ctx.client;
        let request = client.emoney();
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
        // Every task but Cancel (raw value 0, which includes no unknown task) starts a fresh operation. The
        // prelude also runs for unknown tasks: it is applied before the task switch.
        if request.task() != 0 {
            reset_operation(&mut ctx.server.emoney_mut());
            self.deal_result_seen = false;
            self.complete_task_seen = false;
            update_operation(ctx.server, &ServerOperationPatch { busy: Some(true), ..Default::default() });
        }
        let result = self.run_task(&request, ctx.server);
        if result.is_err() {
            update_operation(ctx.server, &ServerOperationPatch { busy: Some(false), ..Default::default() });
        }
        match result {
            Ok(()) => RequestState::Pending,
            Err(_) => RequestState::Failed,
        }
    }
}

/// One brand slot of `brands[]` (`0x14002F280`): defaults, then libam's brand info.
fn publish_brand(port: &dyn EMoneyPort, libam: LibamBrand, slot: &mut ServerBrandMut<'_>) {
    let info = (libam != LibamBrand::NONE).then(|| port.brand_info(libam));
    let name = fit_wide(info.as_ref().and_then(EMoneyBrandInfo::name).unwrap_or_default(), slot.name_mut().capacity());
    let icon_file_path = fit_wide(
        info.as_ref().and_then(EMoneyBrandInfo::icon_file_path).unwrap_or_default(),
        slot.icon_file_path_mut().capacity(),
    );
    let patch = ServerBrandPatch {
        name: Some(&name),
        icon_file_path: Some(&icon_file_path),
        balance: Some(info.map_or(0, |info| info.balance_flag)),
    };
    slot.copy_from(&patch).expect("fitted strings and a byte fit");
}

/// Accept prelude `0x140030300`: clears the operation state and result, keeping `dealAvailable` and
/// `canOperateDeal`.
fn reset_operation(emoney: &mut ServerEMoneyMut<'_>) {
    let mut operation = emoney.operation_mut();
    let flags = ServerOperationPatch {
        busy: Some(false),
        cancellable: Some(false),
        held_over: Some(false),
        error_occurred: Some(false),
        ..Default::default()
    };
    operation.copy_from(&flags).expect("flags accept every value");
    clear_result(&mut operation.result_mut());
}

/// Request trace line (`0x14002FF70`). `None` for an unknown task or brand: the original looks the name up
/// in a table that yields NULL there and dereferences it, so it never produces a line. A payload that fails
/// its proof produces no line either.
fn request_trace(request: &ClientEMoneyRef<'_>) -> Option<String> {
    let task = request.task_kind()?;
    let name = task.name();
    Some(match task {
        EMoneyTask::Cancel | EMoneyTask::DisplayCheck | EMoneyTask::TerminalAuth | EMoneyTask::TerminalRemove => {
            format!("EMoney - task: {name}")
        }
        EMoneyTask::Balance => {
            format!("EMoney - task: {name}, brand: {}", request.balance_info()?.brand_id().name())
        }
        EMoneyTask::AmountPay => {
            let info = request.amount_pay_info()?;
            format!(
                "EMoney - task: {name}, brand: {}, itemID: {}, amount: {}, item count: {}",
                info.brand_id().name(),
                String::from_utf16_lossy(info.item_id().as_slice()),
                info.amount().cast_unsigned(),
                info.item_count()
            )
        }
        EMoneyTask::CoinPay => {
            let info = request.coin_pay_info()?;
            format!(
                "EMoney - task: {name}, brand: {}, coin: {}, player: {}",
                info.brand_id().name(),
                info.coin(),
                info.player_index()
            )
        }
    })
}

impl Controller for EMoneyController {
    fn id(&self) -> ControllerId {
        ControllerId::EMoney
    }

    /// vf3 `0x14002F160`.
    fn is_busy(&self, ctx: &Ctx<'_>) -> bool {
        self.executor.is_busy(ctx.client.emoney().generation(), ctx.server.emoney().response().generation())
    }

    /// vf5 `0x14002F190`: only `available` is initialised.
    fn clear_server_data(&mut self, ctx: &mut Ctx<'_>) {
        if self.port.handle().is_err() {
            tracing::warn!("libam is not ready");
            return;
        }
        let available = self.port.is_enabled();
        ctx.server.emoney_mut().available_mut().set(available).expect("bool accepts every value");
    }

    /// vf6 `0x14002F280`: a failing handle guard skips everything including `checkResult`.
    fn update_server_data(&mut self, ctx: &mut Ctx<'_>) {
        if self.port.handle().is_err() {
            tracing::warn!("libam is not ready");
            return;
        }
        self.publish_terminal_state(&mut ctx.server.emoney_mut());
        self.publish_sound(ctx);
        self.notify_pay_completion(ctx);
        self.check_result(ctx);
    }

    /// vf7 `0x14002F6C0`.
    fn accept_requests(&mut self, ctx: &mut Ctx<'_>) {
        let request_generation = ctx.client.emoney().generation();
        let response_generation = ctx.server.emoney().response().generation();
        let mut executor = std::mem::take(&mut self.executor);
        executor.accept(request_generation, response_generation, || self.accept(ctx));
        self.executor = executor;
    }

    /// The five `EMoneyEventListener`s (`0x14002EC50`–`0x14002EE10`).
    fn on_libam_events(&mut self, ctx: &mut Ctx<'_>) {
        for event in self.port.take_events() {
            self.handle_event(event, ctx);
        }
    }

    /// Process-Config observer `bfb38c2d…` (`0x140032890`).
    fn on_process_config(&mut self, config: &ProcessConfig, ctx: &mut Ctx<'_>) {
        let aime_unit = u32::from(config.emoney.aime_unit);
        ctx.server.emoney_mut().aime_unit_index_mut().set(aime_unit).expect("u32 accepts every value");
    }

    /// AppConfig lambda `97949131…` (`0x140032840`): installs the five listeners.
    fn on_app_config(&mut self, config: &mut AppConfig, _ctx: &mut Ctx<'_>) {
        config.emoney = EMoneyAppConfig {
            complete_task_listener: true,
            notify_deal_result_listener: true,
            play_sound_listener: true,
            notify_cancel_listener: true,
            hold_card_listener: true,
        };
    }

    /// Display-complete lambda `aad7aa9e…` (`0x140032850`): ends a DisplayCheck (`cancellable` stays).
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
    use crate::controllers::controller::testing::{Harness, buffer, civil};
    use crate::schema::emoney::{ClientEMoneyPatch, ServerOperationRef, ServerResultRef};
    use crate::schema::text::fit_wide_str;
    use libam_api::time::{DateTime, Tm};

    /// `SD+0x1C78`: first unit of `eMoney.sound.filePath`, for the bytes behind its terminator.
    const SOUND_FILE_PATH: usize = 0x1C78;

    /// State of the fake libam e-money module shared between the test and the controller's port.
    #[derive(Default)]
    struct Libam {
        up: bool,
        enabled: bool,
        brands: Vec<u32>,
        deals: Vec<EMoneyDealInfo>,
        failing_deal_item: Option<usize>,
        deal_available: bool,
        can_operate: bool,
        can_operate_task: Option<u32>,
        alive: bool,
        command_error: Option<u32>,
        complete_pay_fails: bool,
        calls: Vec<String>,
        events: Vec<EMoneyEvent>,
    }

    struct FakePort {
        libam: Rc<RefCell<Libam>>,
        terminal_id: Option<Vec<u16>>,
        terminal_serial: Option<Vec<u16>>,
        infos: Vec<EMoneyBrandInfo>,
    }

    impl FakePort {
        fn new(libam: &Rc<RefCell<Libam>>) -> Self {
            Self {
                libam: Rc::clone(libam),
                terminal_id: Some(utf16("TERMINAL-ID")),
                terminal_serial: Some(utf16("SERIAL")),
                infos: (1..=7)
                    .map(|brand| {
                        EMoneyBrandInfo::new(
                            brand,
                            &format!("brand{brand}"),
                            &format!("icon{brand}.png"),
                            (brand % 2) as u8,
                        )
                    })
                    .collect(),
            }
        }

        fn command(&self, call: String) -> AmResult<()> {
            let mut libam = self.libam.borrow_mut();
            libam.calls.push(call);
            libam.command_error.map_or(Ok(()), |code| Err(AmError::code(code)))
        }
    }

    impl EMoneyPort for FakePort {
        fn handle(&self) -> AmResult<()> {
            if self.libam.borrow().up { Ok(()) } else { Err(AmError::code(2)) }
        }
        fn shared_handle(&self) -> AmResult<()> {
            Ok(())
        }
        fn is_enabled(&self) -> bool {
            self.libam.borrow().enabled
        }
        fn terminal_id(&self) -> Option<[u16; 31]> {
            self.terminal_id.as_deref().map(buffer)
        }
        fn terminal_serial(&self) -> Option<[u16; 31]> {
            self.terminal_serial.as_deref().map(buffer)
        }
        fn brand_count(&self) -> usize {
            self.libam.borrow().brands.len()
        }
        fn brand(&self, index: usize) -> Option<LibamBrand> {
            self.libam.borrow().brands.get(index).copied().map(LibamBrand)
        }
        fn brand_info(&self, brand: LibamBrand) -> EMoneyBrandInfo {
            self.infos[brand.0 as usize - 1]
        }
        fn deal_log_count(&self) -> usize {
            self.libam.borrow().deals.len()
        }
        fn deal_log_item(&self, index: usize) -> Option<EMoneyDealInfo> {
            let libam = self.libam.borrow();
            if libam.failing_deal_item == Some(index) { None } else { libam.deals.get(index).cloned() }
        }
        fn deal_available(&self) -> bool {
            self.libam.borrow().deal_available
        }
        fn can_operate(&self, task: u32) -> bool {
            let mut libam = self.libam.borrow_mut();
            libam.can_operate_task = Some(task);
            libam.can_operate
        }
        fn alive(&self) -> bool {
            self.libam.borrow().alive
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
        fn terminal_auth(&mut self) -> AmResult<()> {
            self.command("terminal_auth".into())
        }
        fn terminal_remove(&mut self) -> AmResult<()> {
            self.command("terminal_remove".into())
        }
        fn balance(&mut self, brand: LibamBrand) -> AmResult<()> {
            self.command(format!("balance({})", brand.0))
        }
        fn amount_pay(&mut self, amount: i32, item_count: u32, brand: LibamBrand, item_id: &[u16]) -> AmResult<()> {
            self.command(format!("amount_pay({amount},{item_count},{},{})", brand.0, String::from_utf16_lossy(item_id)))
        }
        fn coin_pay(&mut self, player_index: u32, coin: u32, brand: LibamBrand) -> AmResult<()> {
            self.command(format!("coin_pay({player_index},{coin},{})", brand.0))
        }
        fn complete_pay(&mut self) -> AmResult<()> {
            let mut libam = self.libam.borrow_mut();
            libam.calls.push("complete_pay".into());
            if libam.complete_pay_fails { Err(AmError::code(1)) } else { Ok(()) }
        }
        fn take_events(&mut self) -> Vec<EMoneyEvent> {
            std::mem::take(&mut self.libam.borrow_mut().events)
        }
    }

    fn utf16(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    fn libam_up() -> Rc<RefCell<Libam>> {
        Rc::new(RefCell::new(Libam { up: true, ..Libam::default() }))
    }

    fn controller(libam: &Rc<RefCell<Libam>>) -> EMoneyController {
        EMoneyController::new(Box::new(FakePort::new(libam)))
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
        let mut payload = [0; 0x28];
        payload[..info.len()].copy_from_slice(info);
        let patch = ClientEMoneyPatch {
            generation: Some(generation),
            task: Some(task),
            info: Some(&payload),
            ..Default::default()
        };
        harness.client.emoney_mut().copy_from(&patch).expect("plain fields");
    }

    /// The client acknowledges the published sound generation.
    fn acknowledge_sound(harness: &mut Harness, generation: u32) {
        let patch = ClientEMoneyPatch { sound_generation: Some(generation), ..Default::default() };
        harness.client.emoney_mut().copy_from(&patch).expect("plain fields");
    }

    /// The client acknowledges the published pay-completion generation.
    fn acknowledge_pay_completion(harness: &mut Harness, generation: u32) {
        let patch = ClientEMoneyPatch { pay_completion_generation: Some(generation), ..Default::default() };
        harness.client.emoney_mut().copy_from(&patch).expect("plain fields");
    }

    fn operation(harness: &Harness) -> ServerOperationRef<'_> {
        harness.server.emoney().operation()
    }

    fn set_operation(harness: &mut Harness, patch: &ServerOperationPatch<'_>) {
        update_operation(&mut harness.server, patch);
    }

    fn history_item(harness: &Harness, index: usize) -> ServerResultRef<'_> {
        harness.server.emoney().deal_result().items().get(index).expect("one slot per history index")
    }

    fn text(units: &U16CStr) -> String {
        String::from_utf16_lossy(units.as_slice())
    }

    /// Runs the task of the request the client currently has in place.
    fn run_task(controller: &mut EMoneyController, harness: &mut Harness) -> AmResult<()> {
        controller.run_task(&harness.client.emoney(), &mut harness.server)
    }

    fn amount_pay_info(brand: u32, item: &str, amount: i32, count: u32) -> Vec<u8> {
        let mut info = brand.to_le_bytes().to_vec();
        let mut item_id = [0u8; 28];
        for (chunk, unit) in item_id.chunks_exact_mut(2).zip(item.encode_utf16()) {
            chunk.copy_from_slice(&unit.to_le_bytes());
        }
        info.extend(item_id);
        info.extend(amount.to_le_bytes());
        info.extend(count.to_le_bytes());
        info
    }

    fn coin_pay_info(brand: u32, coin: u32, player: u32) -> Vec<u8> {
        [brand, coin, player].iter().flat_map(|value| value.to_le_bytes()).collect()
    }

    fn deal(status: u32, brand: u32, amount: i32) -> EMoneyDealInfo {
        EMoneyDealInfo {
            status,
            brand: LibamBrand(brand),
            time: Tm { sec: 5, min: 4, hour: 3, mday: 2, mon: 0, year: 124, ..Tm::default() },
            deal_number: wide("D0001"),
            card_number: wide("CARD-0001"),
            amount,
            balance_before: 1000,
            balance_after: 1000 - amount,
        }
    }

    fn wide<const N: usize>(text: &str) -> [u16; N] {
        let mut units = [0; N];
        for (slot, unit) in units.iter_mut().zip(text.encode_utf16()) {
            *slot = unit;
        }
        units
    }

    fn code_of(result: AmResult<()>) -> Option<u32> {
        result.err().map(|error| error.code)
    }

    fn calls(libam: &Rc<RefCell<Libam>>) -> Vec<String> {
        libam.borrow().calls.clone()
    }

    #[test]
    fn every_task_forwards_its_translated_payload_and_is_accepted_as_pending() {
        // `None`: the payload fails its proof (brand id ≥ 7), so the request is rejected before libam.
        let cases: [(u32, Vec<u8>, Option<&str>); 8] = [
            (1, vec![], Some("display_start_test")),
            (2, vec![], Some("terminal_auth")),
            (3, vec![], Some("terminal_remove")),
            (4, 5u32.to_le_bytes().to_vec(), Some("balance(3)")),
            (4, 99u32.to_le_bytes().to_vec(), None),
            (5, amount_pay_info(4, "ITEM000000001", -300, 2), Some("amount_pay(-300,2,6,ITEM000000001)")),
            (5, amount_pay_info(7, "X", 1, 1), None),
            (6, coin_pay_info(6, 9, 3), Some("coin_pay(3,9,7)")),
        ];
        for (task, info, expected) in cases {
            let libam = libam_up();
            let mut controller = controller(&libam);
            let mut harness = Harness::running();
            submit(&mut harness, 1, task, &info);
            match expected {
                Some(call) => {
                    assert_eq!(controller.accept(&mut harness.ctx()), RequestState::Pending, "task {task}");
                    assert_eq!(calls(&libam), [call], "task {task}");
                    assert!(operation(&harness).busy(), "task {task} starts an operation");
                }
                None => {
                    assert_eq!(controller.accept(&mut harness.ctx()), RequestState::Failed, "task {task}");
                    assert!(calls(&libam).is_empty(), "task {task}");
                    assert!(!operation(&harness).busy(), "task {task}: busy is reset after any error");
                }
            }
        }
    }

    #[test]
    fn tasks_from_seven_are_rejected_with_code_5_after_the_prelude_cleared_the_operation() {
        for task in [7, 8, u32::MAX] {
            let libam = libam_up();
            let mut controller = controller(&libam);
            let mut harness = Harness::running();
            submit(&mut harness, 1, task, &[]);
            let result = ServerResultPatch { valid: Some(true), amount: Some(5), ..Default::default() };
            let dirty = ServerOperationPatch {
                held_over: Some(true),
                error_occurred: Some(true),
                deal_available: Some(true),
                result: Some(result),
                ..Default::default()
            };
            set_operation(&mut harness, &dirty);
            assert_eq!(code_of(run_task(&mut controller, &mut harness)), Some(5));
            assert_eq!(controller.accept(&mut harness.ctx()), RequestState::Failed, "task {task}");
            let operation = operation(&harness);
            assert!(!operation.busy() && !operation.held_over() && !operation.error_occurred());
            assert!(!operation.result().valid() && operation.result().amount() == 0);
            assert!(operation.deal_available(), "the prelude keeps dealAvailable");
            assert!(libam.borrow().calls.is_empty());
        }
    }

    #[test]
    fn prelude_resets_the_operation_but_cancel_does_not_run_it() {
        let libam = libam_up();
        let mut controller = controller(&libam);
        let mut harness = Harness::running();
        let prepare = |harness: &mut Harness| {
            let deal_number = fit_wide_str("12345", 21);
            let card_number = fit_wide_str("67890", 21);
            let result = ServerResultPatch {
                valid: Some(true),
                status: Some(4),
                time: Some(77),
                brand_id: Some(EMoneyBrandId::Waon.into()),
                deal_number: Some(&deal_number),
                card_number: Some(&card_number),
                amount: Some(1),
                balance_before: Some(2),
                balance_after: Some(3),
            };
            let patch = ServerOperationPatch {
                busy: Some(true),
                cancellable: Some(true),
                held_over: Some(true),
                error_occurred: Some(true),
                can_operate_deal: Some(true),
                result: Some(result),
                ..Default::default()
            };
            set_operation(harness, &patch);
        };

        prepare(&mut harness);
        submit(&mut harness, 1, 0, &[]);
        assert_eq!(controller.accept(&mut harness.ctx()), RequestState::Pending);
        let kept = operation(&harness);
        assert!(kept.busy() && kept.cancellable() && kept.held_over());
        assert_eq!(kept.result().amount(), 1, "Cancel keeps the result");

        prepare(&mut harness);
        submit(&mut harness, 2, 4, &1u32.to_le_bytes());
        assert_eq!(controller.accept(&mut harness.ctx()), RequestState::Pending);
        let reset = operation(&harness);
        assert!(reset.busy());
        assert!(!reset.cancellable() && !reset.held_over() && !reset.error_occurred());
        assert!(reset.can_operate_deal(), "canOperateDeal is not part of the prelude");
        let result = reset.result();
        assert!(!result.valid());
        assert_eq!((result.status(), result.time(), result.brand_id()), (0, 0, EMoneyBrandId::Nanaco));
        assert_eq!((result.deal_number().len(), result.card_number().len()), (0, 0));
        assert_eq!((result.amount(), result.balance_before(), result.balance_after()), (0, 0, 0));
    }

    #[test]
    fn the_prelude_forgets_what_the_listeners_saw_during_the_previous_operation() {
        let libam = libam_up();
        let mut controller = controller(&libam);
        let mut harness = Harness::running();
        controller.handle_event(EMoneyEvent::NotifyDealResult { task: 5, deal: deal(0, 1, 1) }, &mut harness.ctx());
        controller.handle_event(EMoneyEvent::CompleteTask { task: 4, status: 0 }, &mut harness.ctx());
        assert!(controller.deal_result_seen && controller.complete_task_seen);
        submit(&mut harness, 1, 3, &[]);
        controller.accept(&mut harness.ctx());
        assert!(!controller.deal_result_seen && !controller.complete_task_seen);
        controller.handle_event(EMoneyEvent::CompleteTask { task: 4, status: 0 }, &mut harness.ctx());
        assert!(operation(&harness).busy(), "AmountPay completion waits for its deal result");
    }

    #[test]
    fn display_check_cancel_and_completion() {
        let libam = libam_up();
        let mut controller = controller(&libam);
        let mut harness = Harness::running();

        submit(&mut harness, 1, 1, &[]);
        assert_eq!(controller.accept(&mut harness.ctx()), RequestState::Pending);
        assert!(operation(&harness).busy() && operation(&harness).cancellable());

        submit(&mut harness, 2, 0, &[]);
        assert_eq!(controller.accept(&mut harness.ctx()), RequestState::Pending);
        assert_eq!(calls(&libam), ["display_start_test", "display_cancel"]);
        let after_cancel = operation(&harness);
        assert!(after_cancel.busy() && !after_cancel.cancellable(), "busy is cleared by the completion callback");

        controller.on_display_complete(&mut harness.ctx());
        assert!(!operation(&harness).busy());

        set_operation(&mut harness, &ServerOperationPatch { busy: Some(true), ..Default::default() });
        controller.on_display_complete(&mut harness.ctx());
        assert!(operation(&harness).busy(), "no display test running any more");
        submit(&mut harness, 3, 0, &[]);
        controller.accept(&mut harness.ctx());
        assert_eq!(calls(&libam), ["display_start_test", "display_cancel", "cancel"]);
    }

    #[test]
    fn display_completion_keeps_cancellable_and_ignores_payments() {
        let libam = libam_up();
        let mut controller = controller(&libam);
        let mut harness = Harness::running();
        submit(&mut harness, 1, 1, &[]);
        controller.accept(&mut harness.ctx());
        controller.on_display_complete(&mut harness.ctx());
        assert!(operation(&harness).cancellable(), "the callback does not clear cancellable");

        submit(&mut harness, 2, 6, &coin_pay_info(0, 1, 0));
        controller.accept(&mut harness.ctx());
        controller.on_display_complete(&mut harness.ctx());
        assert!(operation(&harness).busy(), "a payment is not ended by the display callback");
    }

    #[test]
    fn accepted_request_is_acknowledged_one_tick_later_and_busy_for_one_tick() {
        let libam = libam_up();
        let mut slot = slot(&libam);
        let mut harness = Harness::running();
        tick(&mut slot, &mut harness);
        submit(&mut harness, 1, 3, &[]);

        tick(&mut slot, &mut harness);
        assert_eq!(calls(&libam), ["terminal_remove"]);
        assert!(slot.controller().is_busy(&harness.ctx()));
        assert_eq!(harness.server.emoney().response().generation(), 0);

        tick(&mut slot, &mut harness);
        assert!(!slot.controller().is_busy(&harness.ctx()));
        assert_eq!(harness.server.emoney().response().generation(), 1);
        assert!(harness.server.emoney().response().succeeded());
        assert!(operation(&harness).busy(), "the operation itself is still running");
    }

    #[test]
    fn rejected_request_is_acknowledged_failed() {
        let libam = libam_up();
        libam.borrow_mut().command_error = Some(1);
        let mut slot = slot(&libam);
        let mut harness = Harness::running();
        tick(&mut slot, &mut harness);
        submit(&mut harness, 4, 2, &[]);
        tick(&mut slot, &mut harness);
        tick(&mut slot, &mut harness);
        assert_eq!(harness.server.emoney().response().generation(), 4);
        assert!(!harness.server.emoney().response().succeeded());
    }

    #[test]
    fn clear_publishes_availability_and_keeps_it_when_the_guard_fails() {
        let libam = libam_up();
        libam.borrow_mut().enabled = true;
        let mut controller = controller(&libam);
        let mut harness = Harness::running();
        harness.server.emoney_mut().sound_mut().generation_mut().set(3).expect("u32 accepts every value");
        controller.clear_server_data(&mut harness.ctx());
        assert!(harness.server.emoney().available());
        assert_eq!(harness.server.emoney().sound().generation(), 3, "nothing else is initialised");

        libam.borrow_mut().up = false;
        libam.borrow_mut().enabled = false;
        controller.clear_server_data(&mut harness.ctx());
        assert!(harness.server.emoney().available(), "unchanged when the guard fails");
    }

    #[test]
    fn update_publishes_terminal_strings_and_operation_flags() {
        let libam = libam_up();
        {
            let mut libam = libam.borrow_mut();
            libam.deal_available = true;
            libam.can_operate = true;
            libam.alive = true;
        }
        let mut port = FakePort::new(&libam);
        port.terminal_id = Some(utf16(&"I".repeat(40)));
        port.terminal_serial = Some(utf16(&"S".repeat(40)));
        let mut controller = EMoneyController::new(Box::new(port));
        let mut harness = Harness::running();
        controller.update_server_data(&mut harness.ctx());
        let emoney = harness.server.emoney();
        assert_eq!(text(emoney.terminal_id()), "I".repeat(30));
        assert_eq!(text(emoney.terminal_serial()), "S".repeat(20));
        let operation = emoney.operation();
        assert!(operation.deal_available() && operation.can_operate_deal() && emoney.alive());
        assert_eq!(libam.borrow().can_operate_task, Some(4), "canOperate is asked about the AmountPay task");

        let mut port = FakePort::new(&libam);
        port.terminal_id = None;
        port.terminal_serial = None;
        let mut controller = EMoneyController::new(Box::new(port));
        controller.update_server_data(&mut harness.ctx());
        assert_eq!(harness.server.emoney().terminal_id().len(), 0);
        assert_eq!(harness.server.emoney().terminal_serial().len(), 0);
    }

    #[test]
    fn update_lists_available_brands_skipping_none_and_stopping_at_seven() {
        use EMoneyBrandId::{Edy, Id, Nanaco, Paseli, Sapica, Transport, Waon};

        let libam = libam_up();
        libam.borrow_mut().brands = vec![2, 0, 1, 3, 4, 5, 6, 7, 2];
        let mut controller = controller(&libam);
        let mut harness = Harness::running();
        controller.update_server_data(&mut harness.ctx());
        let emoney = harness.server.emoney();
        assert_eq!(emoney.available_brand_count(), 7);
        assert_eq!(
            emoney.available_brand_ids().copy_into(),
            [Edy, Nanaco, Id, Transport, Waon, Paseli, Sapica],
            "edy, nanaco, iD, transport, waon, paseli, sapica"
        );

        libam.borrow_mut().brands = vec![7, 1];
        controller.update_server_data(&mut harness.ctx());
        let emoney = harness.server.emoney();
        assert_eq!(emoney.available_brand_count(), 2);
        assert_eq!(
            emoney.available_brand_ids().copy_into(),
            [Sapica, Nanaco, Nanaco, Nanaco, Nanaco, Nanaco, Nanaco],
            "the list is rebuilt"
        );
    }

    #[test]
    fn update_maps_an_unknown_libam_brand_id_to_nanaco() {
        let libam = libam_up();
        libam.borrow_mut().brands = vec![9];
        let mut controller = controller(&libam);
        let mut harness = Harness::running();
        controller.update_server_data(&mut harness.ctx());
        assert_eq!(harness.server.emoney().available_brand_count(), 1);
        assert_eq!(harness.server.emoney().available_brand_ids().get(0), Some(EMoneyBrandId::Nanaco));
    }

    #[test]
    fn update_fills_the_brand_slots_by_amdaemon_brand_id() {
        let libam = libam_up();
        let mut controller = controller(&libam);
        let mut harness = Harness::running();
        controller.update_server_data(&mut harness.ctx());
        let emoney = harness.server.emoney();
        let brands = emoney.brands();
        let names: Vec<String> = brands.iter().map(|brand| text(brand.name())).collect();
        assert_eq!(names, ["brand1", "brand2", "brand4", "brand5", "brand6", "brand3", "brand7"]);
        let icons: Vec<String> = brands.iter().map(|brand| text(brand.icon_file_path())).collect();
        assert_eq!(icons[5], "icon3.png");
        let balances: Vec<u8> = brands.iter().map(|brand| brand.balance()).collect();
        assert_eq!(balances, [1, 0, 0, 1, 0, 1, 1]);
    }

    #[test]
    fn brand_slots_cut_long_names_and_leave_empty_ones_empty() {
        let libam = libam_up();
        let mut port = FakePort::new(&libam);
        port.infos[0] = EMoneyBrandInfo::new(1, &"N".repeat(40), &"p".repeat(250), 1);
        port.infos[1] = EMoneyBrandInfo::new(2, "", "", 0);
        let mut controller = EMoneyController::new(Box::new(port));
        let mut harness = Harness::running();
        let stale = fit_wide_str("stale", 16);
        let stale = ServerBrandPatch { name: Some(&stale), ..Default::default() };
        let mut emoney = harness.server.emoney_mut();
        let mut brands = emoney.brands_mut();
        brands.get_mut(1).expect("one slot per brand").copy_from(&stale).expect("fitted name");
        controller.update_server_data(&mut harness.ctx());
        let emoney = harness.server.emoney();
        let brands = emoney.brands();
        assert_eq!(text(brands.get(0).expect("slot").name()), "N".repeat(15));
        assert_eq!(text(brands.get(0).expect("slot").icon_file_path()), "p".repeat(200));
        assert_eq!(brands.get(1).expect("slot").name().len(), 0);
    }

    #[test]
    fn update_converts_the_deal_log_newest_first_and_clears_unused_slots() {
        let libam = libam_up();
        libam.borrow_mut().deals =
            vec![deal(0, 1, 100), deal(2, 3, 200), deal(3, 7, 300), deal(4, 5, 400), deal(9, 2, 0)];
        let mut controller = controller(&libam);
        let mut harness = Harness::running();
        let stale = ServerResultPatch { valid: Some(true), amount: Some(99), ..Default::default() };
        let mut emoney = harness.server.emoney_mut();
        let mut history = emoney.deal_result_mut();
        let mut items = history.items_mut();
        items.get_mut(10).expect("one slot per history index").copy_from(&stale).expect("scalars fit");
        controller.update_server_data(&mut harness.ctx());
        assert_eq!(harness.server.emoney().deal_result().count(), 5);
        let statuses: Vec<u32> = (0..5).map(|index| history_item(&harness, index).status()).collect();
        assert_eq!(statuses, [1, 4, 2, 3, 0], "libam 0,2,3,4,other");
        let first = history_item(&harness, 0);
        assert!(first.valid());
        assert_eq!(DateTime(first.time()), civil(2024, 1, 2, 3, 4, 5));
        assert_eq!(first.brand_id(), EMoneyBrandId::Nanaco);
        assert_eq!((text(first.deal_number()).as_str(), text(first.card_number()).as_str()), ("D0001", "CARD-0001"));
        assert_eq!((first.amount(), first.balance_before(), first.balance_after()), (100, 1000, 900));
        assert_eq!(history_item(&harness, 1).brand_id(), EMoneyBrandId::Id, "libam iD is amdaemon 5");
        assert_eq!(history_item(&harness, 2).brand_id(), EMoneyBrandId::Sapica);
        assert!(!history_item(&harness, 5).valid());
        let unused = history_item(&harness, 10);
        assert!(!unused.valid() && unused.amount() == 0, "unused slots are cleared");
    }

    #[test]
    fn deal_log_lookup_failure_leaves_the_slot_cleared_and_uncounted() {
        let libam = libam_up();
        libam.borrow_mut().deals = vec![deal(0, 1, 100), deal(0, 1, 200), deal(0, 1, 300)];
        libam.borrow_mut().failing_deal_item = Some(1);
        let mut controller = controller(&libam);
        let mut harness = Harness::running();
        controller.update_server_data(&mut harness.ctx());
        assert_eq!(harness.server.emoney().deal_result().count(), 2);
        let valid: Vec<bool> = (0..3).map(|index| history_item(&harness, index).valid()).collect();
        assert_eq!(valid, [true, false, true]);
    }

    #[test]
    fn invalid_deal_time_keeps_the_cleared_time_and_empty_strings_stay_empty() {
        let libam = libam_up();
        let mut item = deal(0, 1, 5);
        item.time = Tm::default();
        item.deal_number = [0; 16];
        libam.borrow_mut().deals = vec![item];
        let mut controller = controller(&libam);
        let mut harness = Harness::running();
        controller.update_server_data(&mut harness.ctx());
        let first = history_item(&harness, 0);
        assert!(first.valid());
        assert_eq!(DateTime(first.time()), DateTime(0));
        assert_eq!(first.deal_number().len(), 0);
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
        assert_eq!(harness.server.emoney().response().generation(), 0, "pending requests stay pending");
        assert!(slot.controller().is_busy(&harness.ctx()));
        libam.borrow_mut().up = true;
        tick(&mut slot, &mut harness);
        assert_eq!(harness.server.emoney().response().generation(), 1);
    }

    #[test]
    fn sounds_are_published_fifo_one_per_tick_after_each_acknowledgement() {
        let libam = libam_up();
        let mut slot = slot(&libam);
        let mut harness = Harness::running();
        libam.borrow_mut().events = vec![
            EMoneyEvent::PlaySound { path: Some(utf16("first.wav")) },
            EMoneyEvent::PlaySound { path: None },
            EMoneyEvent::PlaySound { path: Some(utf16(&"x".repeat(250))) },
        ];
        tick(&mut slot, &mut harness);
        assert_eq!(harness.server.emoney().sound().generation(), 1);
        assert_eq!(text(harness.server.emoney().sound().file_path()), "first.wav");

        tick(&mut slot, &mut harness);
        assert_eq!(harness.server.emoney().sound().generation(), 1, "waiting for the acknowledgement");

        acknowledge_sound(&mut harness, 1);
        tick(&mut slot, &mut harness);
        assert_eq!(harness.server.emoney().sound().generation(), 2);
        assert_eq!(text(harness.server.emoney().sound().file_path()), "", "NULL path is the empty string");
        let bytes = harness.server.bytes();
        let second_unit = u16::from_le_bytes([bytes[SOUND_FILE_PATH + 2], bytes[SOUND_FILE_PATH + 3]]);
        assert_eq!(second_unit, u16::from(b'i'), "bytes behind the terminator stay");

        acknowledge_sound(&mut harness, 2);
        tick(&mut slot, &mut harness);
        assert_eq!(harness.server.emoney().sound().generation(), 3);
        assert_eq!(text(harness.server.emoney().sound().file_path()), "x".repeat(200));

        acknowledge_sound(&mut harness, 3);
        tick(&mut slot, &mut harness);
        assert_eq!(harness.server.emoney().sound().generation(), 3, "queue is empty");
    }

    #[test]
    fn deal_result_listener_converts_the_deal_and_flags_errors() {
        let libam = libam_up();
        let mut controller = controller(&libam);
        let mut harness = Harness::running();
        controller.handle_event(EMoneyEvent::NotifyDealResult { task: 5, deal: deal(1, 2, 250) }, &mut harness.ctx());
        let published = operation(&harness);
        assert!(published.error_occurred());
        assert!(published.result().valid());
        let result = published.result();
        assert_eq!((result.status(), result.brand_id(), result.amount()), (0, EMoneyBrandId::Edy, 250));
        assert!(controller.deal_result_seen);
        assert_eq!(harness.server.emoney().pay_completion().generation(), 0);

        controller.handle_event(EMoneyEvent::NotifyDealResult { task: 5, deal: deal(2, 2, 250) }, &mut harness.ctx());
        assert!(!operation(&harness).error_occurred());
        controller.handle_event(EMoneyEvent::NotifyDealResult { task: 5, deal: deal(3, 2, 250) }, &mut harness.ctx());
        assert!(operation(&harness).error_occurred());
    }

    #[test]
    fn complete_task_listener_flags_errors_and_ends_non_amount_pay_tasks() {
        let libam = libam_up();
        let mut controller = controller(&libam);
        let mut harness = Harness::running();
        for (status, error) in [(0, false), (1, true), (2, false), (3, true), (4, false)] {
            controller.handle_event(EMoneyEvent::CompleteTask { task: 5, status }, &mut harness.ctx());
            assert_eq!(operation(&harness).error_occurred(), error, "status {status}");
        }

        let running = ServerOperationPatch { busy: Some(true), held_over: Some(true), ..Default::default() };
        set_operation(&mut harness, &running);
        controller.handle_event(EMoneyEvent::CompleteTask { task: 5, status: 0 }, &mut harness.ctx());
        let published = operation(&harness);
        assert!(!published.busy() && !published.held_over(), "CoinPay completes immediately");
    }

    #[test]
    fn amount_pay_completion_waits_for_the_deal_result() {
        let libam = libam_up();
        let mut controller = controller(&libam);
        let mut harness = Harness::running();
        let running = ServerOperationPatch { busy: Some(true), held_over: Some(true), ..Default::default() };
        set_operation(&mut harness, &running);
        controller.handle_event(EMoneyEvent::CompleteTask { task: 4, status: 0 }, &mut harness.ctx());
        assert!(operation(&harness).busy() && operation(&harness).held_over());
        controller.handle_event(EMoneyEvent::NotifyDealResult { task: 4, deal: deal(2, 1, 1) }, &mut harness.ctx());
        controller.handle_event(EMoneyEvent::CompleteTask { task: 4, status: 0 }, &mut harness.ctx());
        assert!(!operation(&harness).busy() && !operation(&harness).held_over());
    }

    #[test]
    fn incomplete_amount_pay_completes_after_the_client_acknowledges_the_pay_completion() {
        let libam = libam_up();
        libam.borrow_mut().deals = vec![deal(4, 1, 100)];
        let mut slot = slot(&libam);
        let mut harness = Harness::running();
        tick(&mut slot, &mut harness);
        submit(&mut harness, 1, 5, &amount_pay_info(0, "ITEM000000001", 100, 1));
        tick(&mut slot, &mut harness);
        tick(&mut slot, &mut harness);
        assert!(operation(&harness).busy());

        libam.borrow_mut().events =
            vec![EMoneyEvent::HoldCard, EMoneyEvent::NotifyDealResult { task: 4, deal: deal(4, 1, 100) }];
        tick(&mut slot, &mut harness);
        assert!(operation(&harness).held_over());
        assert_eq!(operation(&harness).result().status(), 3, "libam status 4 is published as 3");
        assert_eq!(
            harness.server.emoney().pay_completion().generation(),
            1,
            "the client must show the incomplete deal"
        );
        assert!(!calls(&libam).contains(&"complete_pay".to_owned()));

        libam.borrow_mut().events = vec![EMoneyEvent::CompleteTask { task: 4, status: 0 }];
        tick(&mut slot, &mut harness);
        tick(&mut slot, &mut harness);
        let pending = operation(&harness);
        assert!(pending.busy() && pending.held_over(), "no deal result has completed yet");
        assert!(!calls(&libam).contains(&"complete_pay".to_owned()), "pending stays true: no new edge");

        acknowledge_pay_completion(&mut harness, 1);
        tick(&mut slot, &mut harness);
        assert_eq!(calls(&libam).iter().filter(|call| *call == "complete_pay").count(), 1);
        let emoney = harness.server.emoney();
        assert!(!emoney.operation().busy() && !emoney.operation().held_over());
        assert_eq!(emoney.operation().result().status(), 1, "completePay marks the result");
        assert_eq!(emoney.deal_result().count(), 1);

        tick(&mut slot, &mut harness);
        assert_eq!(calls(&libam).iter().filter(|call| *call == "complete_pay").count(), 1, "edge triggered");
    }

    #[test]
    fn pay_completion_acknowledged_before_the_task_completes_leaves_the_end_to_complete_task() {
        let libam = libam_up();
        let mut slot = slot(&libam);
        let mut harness = Harness::running();
        tick(&mut slot, &mut harness);
        submit(&mut harness, 1, 5, &amount_pay_info(0, "ITEM000000001", 100, 1));
        tick(&mut slot, &mut harness);
        libam.borrow_mut().events = vec![EMoneyEvent::NotifyDealResult { task: 4, deal: deal(4, 1, 100) }];
        tick(&mut slot, &mut harness);
        acknowledge_pay_completion(&mut harness, 1);
        tick(&mut slot, &mut harness);
        assert_eq!(calls(&libam).iter().filter(|call| *call == "complete_pay").count(), 1);
        assert!(operation(&harness).busy(), "libam has not completed the task yet");
        assert_eq!(operation(&harness).result().status(), 1);

        libam.borrow_mut().events = vec![EMoneyEvent::CompleteTask { task: 4, status: 0 }];
        tick(&mut slot, &mut harness);
        assert!(!operation(&harness).busy(), "dealResultSeen is set by completePay");
    }

    #[test]
    fn complete_pay_skips_libam_on_a_failed_guard_and_keeps_status_zero_when_libam_fails() {
        let libam = libam_up();
        let mut controller = controller(&libam);
        let mut harness = Harness::running();
        controller.complete_task_seen = true;
        libam.borrow_mut().up = false;
        controller.complete_pay(false, &mut harness.ctx());
        assert!(controller.deal_result_seen && libam.borrow().calls.is_empty());
        assert_eq!(operation(&harness).result().status(), 0);

        libam.borrow_mut().up = true;
        libam.borrow_mut().complete_pay_fails = true;
        set_operation(&mut harness, &ServerOperationPatch { busy: Some(true), ..Default::default() });
        controller.complete_pay(false, &mut harness.ctx());
        assert!(!operation(&harness).busy(), "the history rebuild does not depend on the result");
        assert_eq!(operation(&harness).result().status(), 0, "status 1 only after a successful completePay");
    }
}
