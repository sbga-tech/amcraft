//! `amdaemon::server::allnet::AccountingController` (vtable `0x1405E21E8`, `App+0xD00`, `0x80` bytes).
//!
//! Publishes libam's accounting state into `ServerData+0x0` and executes the client's accounting
//! requests (`ClientData+0xD00`): begin/end/continue a credit, item accounting, quantity, general ID and
//! near-full. The request executor's checker is the constant `Succeeded`, so `succeeded` acknowledges the
//! *submission* of the libam call, never its completion (spec `controllers/allnet.md` §3; evidence ids
//! `allnet.accounting.*`, `allnet.common.executor.*`).

use std::fmt::Write as _;

use crate::executor::{RequestExecutor, RequestState};
use crate::manager::Manager;
use crate::schema::allnet::{
    AccountingPlayCountItemPatch, AccountingPlayCountPatch, AccountingTask, ClientAccountingRef, ServerAccounting,
    ServerAccountingPatch, ServerAccountingResponseMut,
};
use libam_api::allnet::{AccountingPort, PlayCountItem, TimeSlot};
use libam_api::result::{AmError, AmResult};
use libam_api::time::DateTime;

use crate::controllers::controller::{Controller, ControllerId, Ctx, commit_check};

/// Header of the request trace.
const TRACE_HEADER: &str = "ALL.Net Accounting - task: ";

/// `AccountingController`.
pub struct AccountingController {
    port: Box<dyn AccountingPort>,
    executor: RequestExecutor,
}

impl std::fmt::Debug for AccountingController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountingController").field("executor", &self.executor).finish_non_exhaustive()
    }
}

impl AccountingController {
    /// Constructs the controller (ctor `0x14004E460`): idle executor, acceptor = `accept_task`, checker
    /// = constant `Succeeded`.
    pub fn new(port: Box<dyn AccountingPort>) -> Self {
        Self { port, executor: RequestExecutor::default() }
    }

    /// `updateMode` (`0x14004E9D0`).
    fn update_mode(&self, ctx: &mut Ctx<'_>) {
        if self.port.acquire_handle().is_err() {
            tracing::warn!("libam is not ready");
            return;
        }
        let mode = match self.port.accounting_mode() {
            mode @ 1..=3 => mode,
            _ => 0,
        };
        ctx.server.allnet_accounting_mut().mode_mut().set(mode).expect("u32 accepts every value");
    }

    /// `updateInfo` (`0x14004EAF0`); runs only while the published mode is non-zero.
    fn update_info(&self, ctx: &mut Ctx<'_>) {
        if self.port.acquire_handle().is_err() {
            tracing::warn!("libam is not ready");
            return;
        }
        let playable = self.port.accounting_enabled();
        let info = self.port.accounting_info();
        let seconds_until_report =
            self.port.seconds_until_report().unwrap_or(ServerAccounting::UNKNOWN_SECONDS_UNTIL_REPORT);
        // An absent entry leaves the published value alone, which is what an unwritten item or time needs.
        let mut patch = ServerAccountingPatch {
            playable: Some(playable),
            seconds_until_report: Some(seconds_until_report),
            ..ServerAccountingPatch::default()
        };
        if let Ok(Some(info)) = info {
            patch.log_full = Some(info.log_full);
            patch.near_full_enabled = Some(info.near_full);
            if info.play_count.valid {
                let [current, last, before_last] = &info.play_count.items;
                patch.play_count = Some(AccountingPlayCountPatch {
                    current: item_patch(current),
                    last: item_patch(last),
                    before_last: item_patch(before_last),
                });
            }
            patch.report_time = time_micros(&info.report_time);
            patch.bg_report_time = time_micros(&info.bg_report_time);
        }
        ctx.server.allnet_accounting_mut().copy_from(&patch).expect("scalar fields accept every value");
    }

    /// `RequestExecutor::checkResult` (`0x14004FC20`) with the constant-`Succeeded` checker.
    fn check_result(&mut self, ctx: &mut Ctx<'_>) {
        let generation = ctx.client.allnet_accounting().generation();
        let mut block = ctx.server.allnet_accounting_mut();
        let response_generation = block.response().header().generation();
        let outcome = self.executor.check_result(generation, response_generation, || RequestState::Succeeded);
        commit_check(outcome, &mut block.response_mut().header_mut());
    }
}

/// `item_to_wire` (`0x14004ED00`): the item is written only when `(year, month)` is a valid date.
fn item_patch(item: &PlayCountItem) -> Option<AccountingPlayCountItemPatch> {
    let month = DateTime::make(item.year.cast_signed(), item.month.cast_signed(), 1, 0, 0, 0, 0)?;
    Some(AccountingPlayCountItemPatch { month: Some(month.0), count: Some(item.count as u32) })
}

/// `time_to_wire` (`0x14004EDB0`): written only when the slot is valid and its time passes the validator.
fn time_micros(slot: &TimeSlot) -> Option<i64> {
    slot.to_datetime().map(|time| time.0)
}

/// Label of a task in the request logger's string table (`0x1405EEDE0`).
const fn task_name(task: AccountingTask) -> &'static str {
    match task {
        AccountingTask::NearFull => "NearFull",
        AccountingTask::PlayBegin => "PlayBegin",
        AccountingTask::PlayEnd => "PlayEnd",
        AccountingTask::PlayContinue => "PlayContinue",
        AccountingTask::Item => "Item",
        AccountingTask::Quantity => "Quantity",
        AccountingTask::GeneralId => "GeneralId",
    }
}

/// The request trace text (`0x14004EE10`), built only while the Manager's debug dump flag is set.
///
/// An unknown task has no label: the original inserts a null `wchar_t*` there and faults, this port logs
/// the header alone.
fn request_trace(request: &ClientAccountingRef<'_>) -> String {
    let mut text = String::from(TRACE_HEADER);
    let Some(task) = request.task_kind() else {
        return text;
    };
    text.push_str(task_name(task));
    // Writing into a `String` cannot fail.
    let _ = match task {
        AccountingTask::NearFull => write!(text, ", enable: {}", request.near_full_enable()),
        AccountingTask::PlayBegin
        | AccountingTask::PlayEnd
        | AccountingTask::PlayContinue
        | AccountingTask::Item
        | AccountingTask::Quantity => {
            let unit = request.unit();
            let _ = write!(
                text,
                ", player: {}, kind code: {}, status code: {}, item count: {}",
                unit.player(),
                unit.kind_code(),
                unit.status_code(),
                unit.item_count()
            );
            if unit.handle() != 0 {
                let _ = write!(text, ", handle: {}", unit.handle());
            }
            write!(text, ", kind code(next): {}, status code(next): {}", unit.next_kind_code(), unit.next_status_code())
        }
        AccountingTask::GeneralId => {
            // `player` and `kind_code` lead both payloads, so they print even when the ID is unterminated.
            let unit = request.unit();
            let general_id =
                request.general_id().map(|general| String::from_utf16_lossy(general.general_id().as_slice()));
            write!(
                text,
                ", player: {}, kind code: {}, generalID: {}",
                unit.player(),
                unit.kind_code(),
                general_id.unwrap_or_default()
            )
        }
    };
    text
}

/// Starts a credit and publishes its handle: libam slot index + 1, or 0 when libam refused.
fn begin_credit(
    port: &mut dyn AccountingPort,
    response: &mut ServerAccountingResponseMut<'_>,
    player: u32,
    kind_code: u32,
    status_code: u32,
) -> AmResult<()> {
    let started = port.start_credit(player, kind_code, status_code);
    let handle = match &started {
        Ok(slot) => slot.wrapping_add(1),
        Err(_) => 0,
    };
    response.handle_mut().set(handle).expect("u32 accepts every value");
    started.map(|_| ())
}

/// The libam work of one request, by task. `Err` makes the request fail.
fn run_task(
    port: &mut dyn AccountingPort,
    request: &ClientAccountingRef<'_>,
    response: &mut ServerAccountingResponseMut<'_>,
) -> AmResult<()> {
    let Some(task) = request.task_kind() else {
        return Err(AmError::with_message(AmError::UNKNOWN_TASK, "Requested unknown task."));
    };
    match task {
        AccountingTask::NearFull => port.set_near_full(request.near_full_enable()),
        AccountingTask::PlayBegin => {
            let unit = request.unit();
            begin_credit(port, response, unit.player(), unit.kind_code(), unit.status_code())
        }
        AccountingTask::PlayEnd => {
            let unit = request.unit();
            // The wire handle is slot + 1; a request handle of 0 wraps to 0xFFFFFFFF (an invalid slot).
            let slot = unit.handle().wrapping_sub(1);
            port.end_credit(unit.player(), unit.kind_code(), unit.status_code(), unit.item_count() as u8, slot)?;
            response.handle_mut().set(0).expect("u32 accepts every value");
            Ok(())
        }
        AccountingTask::PlayContinue => {
            let unit = request.unit();
            let slot = unit.handle().wrapping_sub(1);
            port.end_credit(unit.player(), unit.kind_code(), unit.status_code(), unit.item_count() as u8, slot)?;
            begin_credit(port, response, unit.player(), unit.next_kind_code(), unit.next_status_code())
        }
        AccountingTask::Item => {
            let unit = request.unit();
            port.item_account(unit.player(), unit.kind_code(), unit.status_code(), unit.item_count() as u8)
        }
        AccountingTask::Quantity => {
            let unit = request.unit();
            port.put_quantity(unit.player(), unit.kind_code(), unit.item_count() as u16)
        }
        AccountingTask::GeneralId => {
            // libam converts the ID up to its terminator; one that fills the field unterminated is refused.
            let general = request.general_id().ok_or(AmError::code(AmError::INVALID_ARGUMENT))?;
            port.put_general_id(general.player(), general.kind_code(), general.general_id().as_slice())
        }
    }
}

/// `AccountingController::accept` (`0x14004E670`), the executor's acceptor: `Pending` when the libam call
/// was accepted, `Failed` otherwise.
fn accept_task(
    port: &mut dyn AccountingPort,
    manager: &mut Manager,
    request: &ClientAccountingRef<'_>,
    response: &mut ServerAccountingResponseMut<'_>,
) -> RequestState {
    if manager.debug_dump() {
        let text = request_trace(request);
        tracing::info!("{text}");
    }
    if port.acquire_handle().is_err() {
        tracing::warn!("libam is not ready");
        return RequestState::Failed;
    }
    match run_task(port, request, response) {
        Ok(()) => RequestState::Pending,
        Err(_) => RequestState::Failed,
    }
}

impl Controller for AccountingController {
    fn id(&self) -> ControllerId {
        ControllerId::AllnetAccounting
    }

    /// vf3 `isBusy` (`0x14004E550`): an accepted request has not been retired yet.
    fn is_busy(&self, ctx: &Ctx<'_>) -> bool {
        self.executor.is_busy(
            ctx.client.allnet_accounting().generation(),
            ctx.server.allnet_accounting().response().header().generation(),
        )
    }

    /// vf5 clear (`0x14004E580`), on the ready edge. The response, the pad at `+0x13` and the three item
    /// pads are not touched.
    fn clear_server_data(&mut self, ctx: &mut Ctx<'_>) {
        let zeroed_item = || AccountingPlayCountItemPatch { month: Some(0), count: Some(0) };
        let patch = ServerAccountingPatch {
            mode: Some(0),
            playable: Some(false),
            log_full: Some(false),
            near_full_enabled: Some(false),
            seconds_until_report: Some(ServerAccounting::UNKNOWN_SECONDS_UNTIL_REPORT),
            play_count: Some(AccountingPlayCountPatch {
                current: Some(zeroed_item()),
                last: Some(zeroed_item()),
                before_last: Some(zeroed_item()),
            }),
            report_time: Some(0),
            bg_report_time: Some(0),
            ..ServerAccountingPatch::default()
        };
        ctx.server.allnet_accounting_mut().copy_from(&patch).expect("scalar fields accept every value");
    }

    /// vf6 update (`0x14004E5D0`): `updateMode`, `updateInfo` while the mode is non-zero, `checkResult`.
    fn update_server_data(&mut self, ctx: &mut Ctx<'_>) {
        self.update_mode(ctx);
        if ctx.server.allnet_accounting().mode() != 0 {
            self.update_info(ctx);
        }
        self.check_result(ctx);
    }

    /// vf7 accept edge (`0x14004E610`).
    fn accept_requests(&mut self, ctx: &mut Ctx<'_>) {
        let request = ctx.client.allnet_accounting();
        let mut block = ctx.server.allnet_accounting_mut();
        let response_generation = block.response().header().generation();
        let mut response = block.response_mut();
        let manager = &mut *ctx.manager;
        let port = self.port.as_mut();
        self.executor
            .accept(request.generation(), response_generation, || accept_task(port, manager, &request, &mut response));
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    use crate::schema::allnet::ClientAccountingPatch;
    use crate::schema::shared::ResponsePatch;

    use super::*;
    use crate::controllers::controller::ControllerSlot;
    use crate::controllers::controller::testing::{Harness, civil};
    use crate::schema::allnet::{ServerAccountingRef, ServerAccountingResponsePatch};
    use libam_api::allnet::{AccountingInfo, PlayCountInfo};
    use libam_api::time::Tm;

    /// A libam call that changes state.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        NearFull(bool),
        Start { player: u32, kind: u32, status: u32 },
        End { player: u32, kind: u32, status: u32, items: u8, slot: u32 },
        Item { player: u32, kind: u32, status: u32, items: u8 },
        Quantity { player: u32, kind: u32, quantity: u16 },
        GeneralId { player: u32, kind: u32, text: String },
    }

    /// Scripted libam state shared between the test and the port handed to the controller.
    #[derive(Debug, Default)]
    struct Libam {
        /// `false` entries make the next `acquire_handle` fail; an empty script succeeds.
        acquire_script: VecDeque<bool>,
        mode: u32,
        enabled: bool,
        info: Option<AccountingInfo>,
        seconds: Option<u32>,
        start_slot: Option<u32>,
        end_ok: bool,
        info_reads: u32,
        calls: Vec<Call>,
    }

    fn libam() -> Libam {
        Libam { start_slot: Some(0), end_ok: true, seconds: Some(0), ..Libam::default() }
    }

    struct FakePort(Rc<RefCell<Libam>>);

    fn outcome(ok: bool) -> AmResult<()> {
        if ok { Ok(()) } else { Err(AmError::with_message(6, "failed")) }
    }

    impl AccountingPort for FakePort {
        fn acquire_handle(&self) -> AmResult<()> {
            let ok = self.0.borrow_mut().acquire_script.pop_front().unwrap_or(true);
            if ok { Ok(()) } else { Err(AmError::code(2)) }
        }
        fn accounting_mode(&self) -> u32 {
            self.0.borrow().mode
        }
        fn accounting_enabled(&self) -> bool {
            self.0.borrow().enabled
        }
        fn accounting_info(&self) -> AmResult<Option<AccountingInfo>> {
            let mut libam = self.0.borrow_mut();
            libam.info_reads += 1;
            Ok(libam.info)
        }
        fn seconds_until_report(&self) -> AmResult<u32> {
            self.0.borrow().seconds.ok_or(AmError::code(2))
        }
        fn set_near_full(&mut self, enable: bool) -> AmResult<()> {
            self.0.borrow_mut().calls.push(Call::NearFull(enable));
            Ok(())
        }
        fn start_credit(&mut self, player: u32, kind: u32, status: u32) -> AmResult<u32> {
            let mut libam = self.0.borrow_mut();
            libam.calls.push(Call::Start { player, kind, status });
            libam.start_slot.ok_or(AmError::with_message(6, "Failed to start credit."))
        }
        fn end_credit(&mut self, player: u32, kind: u32, status: u32, items: u8, slot: u32) -> AmResult<()> {
            let mut libam = self.0.borrow_mut();
            libam.calls.push(Call::End { player, kind, status, items, slot });
            outcome(libam.end_ok)
        }
        fn item_account(&mut self, player: u32, kind: u32, status: u32, items: u8) -> AmResult<()> {
            self.0.borrow_mut().calls.push(Call::Item { player, kind, status, items });
            Ok(())
        }
        fn put_quantity(&mut self, player: u32, kind: u32, quantity: u16) -> AmResult<()> {
            self.0.borrow_mut().calls.push(Call::Quantity { player, kind, quantity });
            Ok(())
        }
        fn put_general_id(&mut self, player: u32, kind: u32, general_id: &[u16]) -> AmResult<()> {
            self.0.borrow_mut().calls.push(Call::GeneralId {
                player,
                kind,
                text: String::from_utf16_lossy(general_id),
            });
            Ok(())
        }
    }

    /// A running Manager, the controller in a slot (so `preExecute`/`postExecute` gating is real) and the
    /// shared libam script.
    struct Rig {
        h: Harness,
        slot: ControllerSlot,
        libam: Rc<RefCell<Libam>>,
    }

    impl Rig {
        fn new(libam: Libam) -> Self {
            let libam = Rc::new(RefCell::new(libam));
            let controller = AccountingController::new(Box::new(FakePort(Rc::clone(&libam))));
            Self { h: Harness::running(), slot: ControllerSlot::new(Box::new(controller)), libam }
        }

        fn pre(&mut self) {
            self.slot.pre_execute(&mut self.h.ctx());
        }

        fn post(&mut self) {
            self.slot.post_execute(&mut self.h.ctx());
        }

        fn busy(&mut self) -> bool {
            self.slot.controller().is_busy(&self.h.ctx())
        }

        fn sd(&self) -> ServerAccountingRef<'_> {
            self.h.server.allnet_accounting()
        }

        /// The published response `(generation, succeeded)`.
        fn response(&self) -> (u32, bool) {
            let header = self.sd().response().header();
            (header.generation(), header.succeeded())
        }

        /// The published accounting handle.
        fn handle(&self) -> u32 {
            self.sd().response().handle()
        }

        /// The daemon's published handle, as an earlier begin left it.
        fn set_handle(&mut self, handle: u32) {
            self.h
                .server
                .allnet_accounting_mut()
                .response_mut()
                .handle_mut()
                .set(handle)
                .expect("u32 accepts every value");
        }

        fn calls(&self) -> Vec<Call> {
            self.libam.borrow().calls.clone()
        }

        /// Submits a request of any raw task with a new generation.
        fn submit_raw(&mut self, task: u32, info: [u8; 264]) {
            let generation = self.h.client.allnet_accounting().generation() + 1;
            let patch = ClientAccountingPatch { generation: Some(generation), task: Some(task), info: Some(&info) };
            self.h.client.allnet_accounting_mut().copy_from(&patch).expect("plain fields");
        }

        /// Submits a request with a new generation.
        fn submit(&mut self, task: AccountingTask, info: [u8; 264]) {
            self.submit_raw(task as u32, info);
        }

        /// Submits a request and runs the accept edge (`postExecute`) of the same frame.
        fn accept(&mut self, task: AccountingTask, info: [u8; 264]) {
            self.pre();
            self.submit(task, info);
            self.post();
        }

        /// Runs the next frame's `preExecute`, which retires the request.
        fn retire(&mut self) {
            self.pre();
        }
    }

    /// The values of the `PlayBegin..=Quantity` payload, in wire order.
    #[derive(Debug, Default)]
    struct Unit {
        player: u32,
        kind_code: u32,
        status_code: u32,
        item_count: u32,
        handle: u32,
        next_kind_code: u32,
        next_status_code: u32,
    }

    fn unit(player: u32, kind: u32, status: u32, count: u32, handle: u32) -> Unit {
        Unit { player, kind_code: kind, status_code: status, item_count: count, handle, ..Unit::default() }
    }

    /// The request's `info` bytes for a `PlayBegin..=Quantity` payload.
    fn unit_info(unit: &Unit) -> [u8; 264] {
        let words = [
            unit.player,
            unit.kind_code,
            unit.status_code,
            unit.item_count,
            unit.handle,
            unit.next_kind_code,
            unit.next_status_code,
        ];
        let mut info = [0; 264];
        for (chunk, word) in info.chunks_exact_mut(4).zip(words) {
            chunk.copy_from_slice(&word.to_le_bytes());
        }
        info
    }

    /// The request's `info` bytes for a `NearFull` payload: the `enable` byte at `info+0`.
    fn near_full_info(enable: bool) -> [u8; 264] {
        let mut info = [0; 264];
        info[0] = u8::from(enable);
        info
    }

    /// The request's `info` bytes for a `GeneralId` payload; `units` is not terminated by this helper.
    fn general_id_info(player: u32, kind: u32, units: &[u16]) -> [u8; 264] {
        let mut info = [0; 264];
        info[..4].copy_from_slice(&player.to_le_bytes());
        info[4..8].copy_from_slice(&kind.to_le_bytes());
        for (pair, unit) in info[8..].chunks_exact_mut(2).zip(units) {
            pair.copy_from_slice(&unit.to_le_bytes());
        }
        info
    }

    fn tm(year: i32, mon: i32, mday: i32, hour: i32, min: i32, sec: i32) -> Tm {
        Tm { sec, min, hour, mday, mon, year, ..Tm::default() }
    }

    fn valid_time(year: i32, mon: i32, mday: i32) -> TimeSlot {
        TimeSlot { valid: true, tm: tm(year, mon, mday, 1, 2, 3) }
    }

    fn info() -> AccountingInfo {
        AccountingInfo {
            near_full: true,
            log_full: true,
            play_count: PlayCountInfo {
                valid: true,
                items: [
                    PlayCountItem { year: 2024, month: 5, count: 30 },
                    PlayCountItem { year: 2024, month: 4, count: 20 },
                    PlayCountItem { year: 2024, month: 3, count: 10 },
                ],
            },
            report_time: valid_time(124, 4, 20),
            bg_report_time: valid_time(124, 4, 21),
        }
    }

    fn running(mode: u32) -> Libam {
        Libam { mode, enabled: true, info: Some(info()), seconds: Some(900), ..libam() }
    }

    // ----------------------------------------------------------------- clear

    #[test]
    fn clear_resets_published_state_but_not_the_response_or_the_padding() {
        let mut rig = Rig::new(libam());
        // Every value the clear step writes or must keep holds a sentinel, the padding bytes included.
        let sentinel_item =
            || AccountingPlayCountItemPatch { month: Some(0x0101_0101_0101_0101), count: Some(0xABAB_ABAB) };
        let sentinel = ServerAccountingPatch {
            response: Some(ServerAccountingResponsePatch {
                header: Some(ResponsePatch { generation: Some(0xABAB_ABAB), succeeded: Some(true) }),
                handle: Some(0xABAB_ABAB),
            }),
            mode: Some(3),
            playable: Some(true),
            log_full: Some(true),
            near_full_enabled: Some(true),
            seconds_until_report: Some(5),
            play_count: Some(AccountingPlayCountPatch {
                current: Some(sentinel_item()),
                last: Some(sentinel_item()),
                before_last: Some(sentinel_item()),
            }),
            report_time: Some(0x0202_0202_0202_0202),
            bg_report_time: Some(0x0303_0303_0303_0303),
        };
        rig.h.server.allnet_accounting_mut().copy_from(&sentinel).expect("scalar fields accept every value");
        let pads = [(0x13, 1), (0x24, 4), (0x34, 4), (0x44, 4)];
        for (offset, len) in pads {
            rig.h.server.span_mut(offset, len).fill(0xAB);
        }
        let response_before = rig.h.server.bytes()[..12].to_vec();

        rig.slot.controller_mut().clear_server_data(&mut rig.h.ctx());

        let sd = rig.sd();
        assert_eq!(sd.mode(), 0);
        assert_eq!((sd.playable(), sd.log_full(), sd.near_full_enabled()), (false, false, false));
        assert_eq!(sd.seconds_until_report(), u32::MAX);
        let play_count = sd.play_count();
        for item in [play_count.current(), play_count.last(), play_count.before_last()] {
            assert_eq!((item.month(), item.count()), (0, 0));
        }
        assert_eq!((sd.report_time(), sd.bg_report_time()), (0, 0));
        let bytes = rig.h.server.bytes();
        assert_eq!(&bytes[..12], response_before.as_slice(), "response (generation, succeeded, handle) is untouched");
        for (offset, len) in pads {
            assert!(
                bytes[offset..offset + len].iter().all(|&byte| byte == 0xAB),
                "padding at {offset:#X} is not cleared"
            );
        }
    }

    // ----------------------------------------------------------------- publication

    #[test]
    fn mode_follows_libam_for_one_to_three_and_is_zero_otherwise() {
        for (raw, expected) in [(0, 0), (1, 1), (2, 2), (3, 3), (4, 0), (0xFFFF_FFFF, 0)] {
            let mut rig = Rig::new(Libam { mode: raw, ..libam() });
            rig.pre();
            assert_eq!(rig.sd().mode(), expected, "raw mode {raw}");
        }
    }

    #[test]
    fn mode_zero_skips_update_info() {
        let mut rig = Rig::new(running(0));
        rig.pre();
        assert_eq!(rig.libam.borrow().info_reads, 0);
        assert!(!rig.sd().playable());
        assert_eq!(rig.sd().seconds_until_report(), u32::MAX, "left at the value written by the clear step");
    }

    #[test]
    fn nonzero_mode_publishes_flags_history_report_times_and_countdown() {
        let mut rig = Rig::new(running(2));
        rig.pre();
        let sd = rig.sd();
        assert_eq!(sd.mode(), 2);
        assert!(sd.playable() && sd.log_full() && sd.near_full_enabled());
        assert_eq!(sd.seconds_until_report(), 900);
        let play_count = sd.play_count();
        assert_eq!(play_count.current().month(), civil(2024, 5, 1, 0, 0, 0).0);
        assert_eq!(play_count.current().count(), 30);
        assert_eq!(play_count.last().month(), civil(2024, 4, 1, 0, 0, 0).0);
        assert_eq!(play_count.last().count(), 20);
        assert_eq!(play_count.before_last().month(), civil(2024, 3, 1, 0, 0, 0).0);
        assert_eq!(play_count.before_last().count(), 10);
        assert_eq!(sd.report_time(), civil(2024, 5, 20, 1, 2, 3).0);
        assert_eq!(sd.bg_report_time(), civil(2024, 5, 21, 1, 2, 3).0);
    }

    #[test]
    fn play_count_count_keeps_the_low_32_bits() {
        let mut libam = running(1);
        let mut with_big_count = info();
        with_big_count.play_count.items[0].count = 0x1_0000_0007;
        libam.info = Some(with_big_count);
        let mut rig = Rig::new(libam);
        rig.pre();
        assert_eq!(rig.sd().play_count().current().count(), 7);
    }

    #[test]
    fn a_play_count_item_with_an_invalid_date_keeps_its_previous_value() {
        let mut rig = Rig::new(running(1));
        rig.pre();
        let mut changed = info();
        changed.play_count.items[0] = PlayCountItem { year: 2025, month: 1, count: 99 };
        changed.play_count.items[1] = PlayCountItem { year: 2025, month: 13, count: 77 };
        changed.play_count.items[2] = PlayCountItem { year: 0, month: 1, count: 66 };
        rig.libam.borrow_mut().info = Some(changed);
        rig.pre();
        let items = rig.sd().play_count();
        assert_eq!((items.current().month(), items.current().count()), (civil(2025, 1, 1, 0, 0, 0).0, 99));
        assert_eq!((items.last().month(), items.last().count()), (civil(2024, 4, 1, 0, 0, 0).0, 20));
        assert_eq!((items.before_last().month(), items.before_last().count()), (civil(2024, 3, 1, 0, 0, 0).0, 10));
    }

    #[test]
    fn an_invalid_play_count_block_skips_every_item() {
        let mut libam = running(1);
        let mut no_history = info();
        no_history.play_count.valid = false;
        libam.info = Some(no_history);
        let mut rig = Rig::new(libam);
        rig.pre();
        let items = rig.sd().play_count();
        for item in [items.current(), items.last(), items.before_last()] {
            assert_eq!((item.month(), item.count()), (0, 0));
        }
        assert!(rig.sd().log_full(), "the flags and times are still published");
        assert_ne!(rig.sd().report_time(), 0);
    }

    #[test]
    fn report_times_need_a_valid_flag_and_a_valid_time() {
        let mut libam = running(1);
        let mut broken = info();
        broken.report_time = TimeSlot { valid: false, ..valid_time(124, 4, 20) };
        broken.bg_report_time = valid_time(124, 12, 1);
        libam.info = Some(broken);
        let mut rig = Rig::new(libam);
        rig.pre();
        assert_eq!((rig.sd().report_time(), rig.sd().bg_report_time()), (0, 0));
    }

    #[test]
    fn missing_info_still_publishes_playable_and_the_countdown() {
        let mut rig = Rig::new(Libam { info: None, ..running(3) });
        rig.pre();
        let sd = rig.sd();
        assert!(sd.playable());
        assert!(!sd.log_full() && !sd.near_full_enabled());
        assert_eq!(sd.seconds_until_report(), 900);
        assert_eq!(rig.libam.borrow().info_reads, 1);
    }

    #[test]
    fn a_failing_countdown_publishes_the_unknown_marker() {
        let mut rig = Rig::new(Libam { seconds: None, ..running(1) });
        rig.pre();
        assert_eq!(rig.sd().seconds_until_report(), ServerAccounting::UNKNOWN_SECONDS_UNTIL_REPORT);
    }

    #[test]
    fn unavailable_libam_leaves_the_mode_alone() {
        let mut rig = Rig::new(Libam { acquire_script: [false].into(), ..running(2) });
        rig.h.server.allnet_accounting_mut().mode_mut().set(3).expect("u32 accepts every value");
        rig.slot.controller_mut().update_server_data(&mut rig.h.ctx());
        assert_eq!(rig.sd().mode(), 3, "mode unchanged, so updateInfo still runs on the stale mode");
    }

    // ----------------------------------------------------------------- executor timing

    #[test]
    fn a_request_is_accepted_after_the_exchange_and_retired_by_the_next_pre_execute() {
        let mut rig = Rig::new(libam());
        rig.accept(AccountingTask::NearFull, near_full_info(true));
        assert_eq!(rig.calls(), [Call::NearFull(true)]);
        assert!(rig.busy(), "accepted but not yet retired");
        assert_eq!(rig.sd().response().header().generation(), 0);

        rig.retire();
        assert_eq!(rig.response(), (1, true));
        assert!(!rig.busy());
    }

    #[test]
    fn a_request_is_executed_once_however_often_the_accept_edge_runs() {
        let mut rig = Rig::new(libam());
        rig.accept(AccountingTask::NearFull, near_full_info(false));
        rig.post();
        rig.post();
        rig.retire();
        rig.post();
        assert_eq!(rig.calls(), [Call::NearFull(false)]);
    }

    // ----------------------------------------------------------------- tasks

    #[test]
    fn play_begin_publishes_the_slot_plus_one_as_handle() {
        let mut rig = Rig::new(Libam { start_slot: Some(4), ..libam() });
        rig.accept(AccountingTask::PlayBegin, unit_info(&unit(2, 3, 4, 9, 0)));
        assert_eq!(rig.calls(), [Call::Start { player: 2, kind: 3, status: 4 }]);
        assert_eq!(rig.handle(), 5);
        rig.retire();
        assert!(rig.response().1);
    }

    #[test]
    fn play_begin_failure_resets_the_handle_and_fails_the_request() {
        let mut rig = Rig::new(Libam { start_slot: None, ..libam() });
        rig.set_handle(7);
        rig.accept(AccountingTask::PlayBegin, unit_info(&unit(0, 1, 2, 0, 0)));
        assert_eq!(rig.handle(), 0);
        rig.retire();
        assert!(!rig.response().1);
    }

    #[test]
    fn play_end_uses_the_zero_based_slot_and_clears_the_handle() {
        let mut rig = Rig::new(libam());
        rig.set_handle(5);
        rig.accept(AccountingTask::PlayEnd, unit_info(&unit(1, 2, 3, 0x1_05, 5)));
        assert_eq!(rig.calls(), [Call::End { player: 1, kind: 2, status: 3, items: 5, slot: 4 }]);
        assert_eq!(rig.handle(), 0);
    }

    #[test]
    fn play_end_failure_keeps_the_handle() {
        let mut rig = Rig::new(Libam { end_ok: false, ..libam() });
        rig.set_handle(5);
        rig.accept(AccountingTask::PlayEnd, unit_info(&unit(1, 2, 3, 0, 5)));
        assert_eq!(rig.handle(), 5);
        rig.retire();
        assert!(!rig.response().1);
    }

    #[test]
    fn play_end_with_request_handle_zero_passes_the_wrapped_slot() {
        let mut rig = Rig::new(libam());
        rig.accept(AccountingTask::PlayEnd, unit_info(&unit(1, 2, 3, 0, 0)));
        assert_eq!(rig.calls(), [Call::End { player: 1, kind: 2, status: 3, items: 0, slot: u32::MAX }]);
    }

    #[test]
    fn play_continue_ends_the_credit_then_begins_the_next_one() {
        let mut rig = Rig::new(Libam { start_slot: Some(1), ..libam() });
        let continue_unit = Unit { next_kind_code: 8, next_status_code: 9, ..unit(3, 4, 5, 2, 7) };
        rig.accept(AccountingTask::PlayContinue, unit_info(&continue_unit));
        assert_eq!(
            rig.calls(),
            [
                Call::End { player: 3, kind: 4, status: 5, items: 2, slot: 6 },
                Call::Start { player: 3, kind: 8, status: 9 },
            ]
        );
        assert_eq!(rig.handle(), 2);
        rig.retire();
        assert!(rig.response().1);
    }

    #[test]
    fn play_continue_does_not_begin_when_the_end_fails() {
        let mut rig = Rig::new(Libam { end_ok: false, ..libam() });
        rig.set_handle(7);
        rig.accept(AccountingTask::PlayContinue, unit_info(&unit(0, 1, 2, 0, 7)));
        assert_eq!(rig.calls().len(), 1, "only the end call was attempted");
        assert_eq!(rig.handle(), 7);
        rig.retire();
        assert!(!rig.response().1);
    }

    #[test]
    fn item_passes_the_item_count_as_a_byte() {
        let mut rig = Rig::new(libam());
        rig.accept(AccountingTask::Item, unit_info(&unit(1, 2, 3, 0x1234, 0)));
        assert_eq!(rig.calls(), [Call::Item { player: 1, kind: 2, status: 3, items: 0x34 }]);
    }

    #[test]
    fn quantity_passes_a_sixteen_bit_quantity_and_ignores_the_status_code() {
        let mut rig = Rig::new(libam());
        rig.accept(AccountingTask::Quantity, unit_info(&unit(1, 2, 0xDEAD, 0x1_2345, 0)));
        assert_eq!(rig.calls(), [Call::Quantity { player: 1, kind: 2, quantity: 0x2345 }]);
    }

    #[test]
    fn general_id_passes_the_wide_string() {
        let mut rig = Rig::new(libam());
        let id: Vec<u16> = "ID-ABC".encode_utf16().collect();
        rig.accept(AccountingTask::GeneralId, general_id_info(6, 7, &id));
        assert_eq!(rig.calls(), [Call::GeneralId { player: 6, kind: 7, text: "ID-ABC".into() }]);
    }

    #[test]
    fn unknown_tasks_fail_without_touching_libam() {
        for raw in [7, 8, u32::MAX] {
            let mut rig = Rig::new(libam());
            rig.pre();
            rig.submit_raw(raw, [0; 264]);
            rig.post();
            assert!(rig.calls().is_empty(), "task {raw}");
            rig.retire();
            assert_eq!(rig.response(), (1, false), "task {raw}");
        }
    }

    #[test]
    fn unavailable_libam_fails_the_request() {
        let mut rig = Rig::new(libam());
        rig.pre();
        rig.libam.borrow_mut().acquire_script = [false].into();
        rig.submit(AccountingTask::NearFull, [0; 264]);
        rig.post();
        assert!(rig.calls().is_empty());
        rig.retire();
        assert!(!rig.response().1);
    }
}
