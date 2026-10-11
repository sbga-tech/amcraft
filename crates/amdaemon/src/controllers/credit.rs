//! `amdaemon::server::CreditController` (vtable `0x1405CC698`, ctor `0x14002B490`, `App+0xA98`).
//!
//! Publishes the libam credit module into `ServerData+0x1B88` and executes the credit requests
//! (`ClientData+0xC40`). Behaviour: `spec/controllers/credit_emoney_aimepay.md` §2 and the shared machinery
//! in §1; claims `credit_emoney_aimepay.credit.*`.
//!
//! The response only acknowledges submission: the checker lambda is the constant `2`
//! (`credit_emoney_aimepay.exec.checker_constant_success`), so an accepted request is acknowledged as
//! succeeded one tick after acceptance and a rejected one as failed, independent of the credit operation.

use std::collections::VecDeque;

use crate::executor::{RequestExecutor, RequestState};
use crate::schema::credit::{
    ClientCreditRequestRef, CreditBookkeepingMut, CreditBookkeepingPatch, CreditConfig, CreditConfigMut,
    CreditConfigPatch, CreditSound, CreditTask, ServerCoinInPatch, ServerCreditMut, ServerPlayerMut, ServerPlayerPatch,
    ServerSpecialDeviceMut, ServerSpecialDevicePatch,
};
use libam_api::credit::{CreditPort, InsertCoinEvent, PLAYERS};
use libam_api::result::{AmError, AmResult};

use crate::controllers::controller::{Controller, ControllerId, Ctx, commit_check};
use libam_api::config::AppConfig;

/// Defaults written into `config` at clear time before libam is consulted (`0x14002B6A4`–`0x14002B703`).
const CLEAR_CONFIG: CreditConfig = CreditConfig {
    coin_chute_common: false,
    service_common: false,
    freeplay: false,
    multipliers: [1, 1],
    bonus_adder: 0,
    coin_to_credit: 1,
    game_cost: [1; 8],
    coin_amount: 100,
};

/// `CreditController`.
pub struct CreditController {
    port: Box<dyn CreditPort>,
    executor: RequestExecutor,
    /// `std::deque<InsertCoinEvent>` filled by the `CreditEventListener` (unbounded, never dropped).
    events: VecDeque<InsertCoinEvent>,
}

impl std::fmt::Debug for CreditController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CreditController")
            .field("executor", &self.executor)
            .field("queued_events", &self.events.len())
            .finish_non_exhaustive()
    }
}

impl CreditController {
    /// Constructs the controller (ctor `0x14002B490`).
    pub fn new(port: Box<dyn CreditPort>) -> Self {
        Self { port, executor: RequestExecutor::default(), events: VecDeque::new() }
    }

    /// `loadAvailable` (`0x14002BB80`).
    fn load_available(&self, credit: &mut ServerCreditMut<'_>) {
        if self.port.handle().is_err() {
            tracing::warn!("libam is not ready");
            return;
        }
        credit.available_mut().set(self.port.is_enabled()).expect("a bool accepts both values");
        credit
            .special_mut()
            .available_mut()
            .set(self.port.special_device_available())
            .expect("a bool accepts both values");
    }

    /// `loadData(CreditConfig&)` (`0x14002BC80`).
    fn load_config(&self, config: &mut CreditConfigMut<'_>) {
        if self.port.handle().is_err() {
            tracing::warn!("libam is not ready");
            return;
        }
        let port = &*self.port;
        let coin_chute_common = port.config_chute_share_type() == 1;
        let service_common = port.config_service_share_type() == 1;
        let freeplay = port.config_operation() == 2;
        let mut multipliers = [0; 2];
        for (index, multiplier) in (0..).zip(multipliers.iter_mut()) {
            *multiplier = port.config_multiplier(index).unwrap_or_else(|_| {
                tracing::warn!("libamw version does not match");
                0
            });
        }
        let bonus_adder = port.config_bonus_adder();
        let coin_to_credit = port.config_coin_to_credit();
        let mut game_cost = [0; 8];
        for (index, cost) in (0..).zip(game_cost.iter_mut()) {
            *cost = port.config_game_cost(index).unwrap_or_else(|_| {
                tracing::warn!("libamw version does not match");
                0
            });
        }
        let patch = CreditConfigPatch {
            coin_chute_common: Some(coin_chute_common),
            service_common: Some(service_common),
            freeplay: Some(freeplay),
            multipliers: Some(multipliers),
            bonus_adder: Some(bonus_adder),
            coin_to_credit: Some(coin_to_credit),
            game_cost: Some(game_cost),
            coin_amount: Some(port.config_coin_amount()),
        };
        config.copy_from(&patch).expect("scalar fields accept every value");
    }

    /// `loadData(unsigned, CreditValue&)` (`0x14002BF40`): the destination is left unchanged when libam has
    /// no value for the player.
    fn load_player_value(&self, index: u32, player: &mut ServerPlayerMut<'_>) {
        if self.port.handle().is_err() {
            tracing::warn!("libam is not ready");
            return;
        }
        if let Some(value) = self.port.player_value(index) {
            let patch = ServerPlayerPatch {
                credit: Some(value.credit),
                remain: Some(value.remain),
                ..ServerPlayerPatch::default()
            };
            player.copy_from(&patch).expect("scalar fields accept every value");
        }
    }

    /// `loadData(unsigned, unsigned&)` (`0x14002C070`).
    fn load_addable_coin(&self, index: u32, player: &mut ServerPlayerMut<'_>) {
        if self.port.handle().is_err() {
            tracing::warn!("libam is not ready");
            return;
        }
        player.addable_coin_mut().set(self.port.addable_coin(index)).expect("u32 accepts every value");
    }

    /// `loadData(CreditBookkeeping&)` (`0x14002C170`).
    fn load_bookkeeping(&self, bookkeeping: &mut CreditBookkeepingMut<'_>) {
        if self.port.handle().is_err() {
            tracing::warn!("libam is not ready");
            return;
        }
        let mut coin = [0; 4];
        for (index, counter) in (0..).zip(coin.iter_mut()) {
            *counter = self.port.bookkeeping_coin(index).unwrap_or_else(|_| {
                tracing::warn!("libamw version does not match");
                0
            });
        }
        let totals = self.port.bookkeeping_totals();
        let patch = CreditBookkeepingPatch {
            coin: Some(coin),
            total_coin: Some(totals.total_coin),
            coin_credit: Some(totals.coin_credit),
            service_credit: Some(totals.service_credit),
            e_money_coin: Some(totals.e_money_coin),
            e_money_credit: Some(totals.e_money_credit),
            total_credit: Some(totals.total_credit),
        };
        bookkeeping.copy_from(&patch).expect("scalar fields accept every value");
    }

    /// `loadData(ServerSpecialDeviceData&)` (`0x14002C370`): `available` is only written at clear time.
    fn load_special(&self, special: &mut ServerSpecialDeviceMut<'_>) {
        if self.port.handle().is_err() {
            tracing::warn!("libam is not ready");
            return;
        }
        let patch = ServerSpecialDevicePatch {
            lockout_on: Some(self.port.lockout_on()),
            can_lockout: Some(self.port.can_lockout()),
            ..ServerSpecialDevicePatch::default()
        };
        special.copy_from(&patch).expect("a bool accepts both values");
    }

    /// `loadData(bool&)` (`0x14002C470`).
    fn load_coin_in_ignored(&self, credit: &mut ServerCreditMut<'_>) {
        if self.port.handle().is_err() {
            tracing::warn!("libam is not ready");
            return;
        }
        credit.coin_in_ignored_mut().set(self.port.coin_in_ignored()).expect("a bool accepts both values");
    }

    /// Coin-in publication (`0x14002B7D2`–`0x14002B8A1`): one queued event per tick, only once the client
    /// acknowledged the previous event by echoing its generation.
    fn publish_coin_in(&mut self, ctx: &mut Ctx<'_>) {
        let mut credit = ctx.server.credit_mut();
        let mut coin_in = credit.coin_in_mut();
        if coin_in.generation() != ctx.client.credit().coin_in().generation() {
            return;
        }
        let Some(event) = self.events.pop_front() else {
            return;
        };
        let sound = if event.service { CreditSound::Service } else { CreditSound::Coin };
        let patch = ServerCoinInPatch {
            generation: Some(coin_in.generation().wrapping_add(1)),
            sound: Some(Some(sound.into())),
            players: Some(event.players),
        };
        coin_in.copy_from(&patch).expect("scalar fields accept every value");
    }

    /// `RequestExecutor::checkResult` tail of `update`.
    ///
    /// The checker is the constant `2`: a request accepted in the previous `postExecute` is acknowledged here as
    /// succeeded, a rejected one as failed. The checker can never answer `Idle`.
    fn check_result(&mut self, ctx: &mut Ctx<'_>) {
        let request_generation = ctx.client.credit().request().generation();
        let mut credit = ctx.server.credit_mut();
        let response_generation = credit.response().generation();
        let outcome = self.executor.check_result(request_generation, response_generation, || RequestState::Succeeded);
        commit_check(outcome, &mut credit.response_mut());
    }

    /// Executes the request task and returns the `am::Result` the original ends up with.
    ///
    /// A payload that is not a valid `bool` fails the request without reaching libam.
    fn run_task(&mut self, request: &ClientCreditRequestRef<'_>) -> AmResult<()> {
        let invalid_payload = || AmError::code(AmError::INVALID_ARGUMENT);
        match request.task_kind() {
            Some(CreditTask::Config) => Err(AmError::with_message(AmError::FAILED, "Requested invalid task.")),
            Some(CreditTask::Clear) => {
                self.port.clear();
                Ok(())
            }
            Some(CreditTask::Pay) => {
                let info = request.pay_info();
                self.port.pay(info.player_index(), info.game_cost_index(), info.count())
            }
            Some(CreditTask::SpecialLockout) => {
                let lockout = request.special_lockout().ok_or_else(invalid_payload)?;
                self.port.special_lockout(lockout)
            }
            Some(CreditTask::CoinInIgnore) => {
                let ignore = request.coin_in_ignore().ok_or_else(invalid_payload)?;
                self.port.set_coin_in_ignore(ignore);
                Ok(())
            }
            None => Err(AmError::with_message(AmError::UNKNOWN_TASK, "Requested unknown task.")),
        }
    }

    /// `CreditController::accept` (`0x14002B970`): the acceptor of the executor.
    fn accept(&mut self, ctx: &mut Ctx<'_>) -> RequestState {
        let client = ctx.client;
        let request = client.credit().request();
        if ctx.manager.debug_dump()
            && let Some(text) = request_trace(&request)
        {
            tracing::info!("{text}");
        }
        if self.port.handle().is_err() {
            tracing::warn!("libam is not ready");
            return RequestState::Failed;
        }
        match self.run_task(&request) {
            Ok(()) => RequestState::Pending,
            Err(_) => RequestState::Failed,
        }
    }
}

/// Name of a credit task (table `0x1405EEE60`).
const fn task_name(task: CreditTask) -> &'static str {
    match task {
        CreditTask::Config => "Config",
        CreditTask::Clear => "Clear",
        CreditTask::Pay => "Pay",
        CreditTask::SpecialLockout => "SpecialLockout",
        CreditTask::CoinInIgnore => "CoinInIgnore",
    }
}

/// Request trace line (`0x14002C560`, prefix `Creidt` sic). `None` for unknown tasks: the original looks the
/// name up in a table that yields NULL there and dereferences it, so it never produces a line.
fn request_trace(request: &ClientCreditRequestRef<'_>) -> Option<String> {
    let task = request.task_kind()?;
    let name = task_name(task);
    Some(match task {
        CreditTask::Config => {
            let info = request.config_info()?;
            let sharing = |common: bool| if common { "common" } else { "individual" };
            let [multiplier_0, multiplier_1] = info.multipliers().copy_into();
            let game_cost = info.game_cost().copy_into().map(|cost| cost.to_string());
            format!(
                "Creidt - task: {name}, coin chute: {}, service: {}, freeplay: {}, multipliers: [{}, {}], bonus adder: {}, coin to credit: {}, game cost: [{}], coin amount: {}, ",
                sharing(info.coin_chute_common()),
                sharing(info.service_common()),
                info.freeplay(),
                multiplier_0,
                multiplier_1,
                info.bonus_adder(),
                info.coin_to_credit(),
                game_cost.join(", "),
                info.coin_amount(),
            )
        }
        CreditTask::Clear => format!("Creidt - task: {name}"),
        CreditTask::Pay => {
            let info = request.pay_info();
            format!(
                "Creidt - task: {name}, player: {}, game cost: {}, count{}",
                info.player_index(),
                info.game_cost_index(),
                info.count()
            )
        }
        CreditTask::SpecialLockout => format!("Creidt - task: {name}, lockout: {}", request.special_lockout()?),
        CreditTask::CoinInIgnore => format!("Creidt - task: {name}, ignored: {}", request.coin_in_ignore()?),
    })
}

impl Controller for CreditController {
    fn id(&self) -> ControllerId {
        ControllerId::Credit
    }

    /// vf3 `0x14002B640`.
    fn is_busy(&self, ctx: &Ctx<'_>) -> bool {
        self.executor.is_busy(ctx.client.credit().request().generation(), ctx.server.credit().response().generation())
    }

    /// vf5 `0x14002B670`: defaults first, then whatever libam reports.
    fn clear_server_data(&mut self, ctx: &mut Ctx<'_>) {
        let mut credit = ctx.server.credit_mut();
        credit
            .config_mut()
            .copy_from(&CreditConfigPatch::from(CLEAR_CONFIG))
            .expect("scalar fields accept every value");
        let mut players = credit.players_mut();
        for index in 0..PLAYERS {
            players.get_mut(index).expect("the index is below the player count").clear();
        }
        credit.bookkeeping_mut().clear();
        self.load_available(&mut credit);
        self.load_config(&mut credit.config_mut());
    }

    /// vf6 `0x14002B760`.
    fn update_server_data(&mut self, ctx: &mut Ctx<'_>) {
        let mut credit = ctx.server.credit_mut();
        {
            let mut players = credit.players_mut();
            for index in 0..PLAYERS as u32 {
                let mut player = players.get_mut(index as usize).expect("the index is below the player count");
                self.load_player_value(index, &mut player);
                self.load_addable_coin(index, &mut player);
            }
        }
        self.load_bookkeeping(&mut credit.bookkeeping_mut());
        self.publish_coin_in(ctx);
        let mut credit = ctx.server.credit_mut();
        self.load_special(&mut credit.special_mut());
        self.load_coin_in_ignored(&mut credit);
        self.check_result(ctx);
    }

    /// vf7 `0x14002B900`.
    fn accept_requests(&mut self, ctx: &mut Ctx<'_>) {
        let request_generation = ctx.client.credit().request().generation();
        let response_generation = ctx.server.credit().response().generation();
        let mut executor = std::mem::take(&mut self.executor);
        executor.accept(request_generation, response_generation, || self.accept(ctx));
        self.executor = executor;
    }

    /// `CreditEventListener::operator()` (`0x14002B3C0`): queues every event and reports it consumed.
    fn on_libam_events(&mut self, _ctx: &mut Ctx<'_>) {
        self.events.extend(self.port.take_events());
    }

    /// AppConfig lambda `129ed5d5…` (`0x14002DE50`): installs the insert-coin listener.
    fn on_app_config(&mut self, config: &mut AppConfig, _ctx: &mut Ctx<'_>) {
        config.credit.insert_coin_listener = true;
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use crate::schema::credit::{ClientCreditRequestPatch, CreditBookkeeping, ServerCreditPatch, ServerPlayer};
    use crate::schema::shared::ResponsePatch;
    use libam_api::credit::{BookkeepingTotals, PlayerCredit};

    use super::*;
    use crate::controllers::controller::ControllerSlot;
    use crate::controllers::controller::testing::Harness;

    /// State of the fake libam credit module shared between the test and the controller's port.
    #[derive(Default)]
    struct Libam {
        up: bool,
        enabled: bool,
        special_available: bool,
        chute_share: u32,
        service_share: u32,
        operation: u32,
        multipliers: [u32; 2],
        multiplier_fails_at: Option<u32>,
        bonus_adder: u32,
        coin_to_credit: u32,
        game_cost: [u32; 8],
        coin_amount: u32,
        players: [Option<(u32, u32)>; 8],
        addable: [u32; 8],
        lockout_on: bool,
        can_lockout: bool,
        ignored: bool,
        calls: Vec<String>,
        events: Vec<InsertCoinEvent>,
    }

    struct FakePort(Rc<RefCell<Libam>>);

    impl CreditPort for FakePort {
        fn handle(&self) -> AmResult<()> {
            if self.0.borrow().up { Ok(()) } else { Err(AmError::code(2)) }
        }
        fn is_enabled(&self) -> bool {
            self.0.borrow().enabled
        }
        fn special_device_available(&self) -> bool {
            self.0.borrow().special_available
        }
        fn config_chute_share_type(&self) -> u32 {
            self.0.borrow().chute_share
        }
        fn config_service_share_type(&self) -> u32 {
            self.0.borrow().service_share
        }
        fn config_operation(&self) -> u32 {
            self.0.borrow().operation
        }
        fn config_multiplier(&self, index: u32) -> AmResult<u32> {
            let libam = self.0.borrow();
            if libam.multiplier_fails_at == Some(index) {
                return Err(AmError::code(1));
            }
            Ok(libam.multipliers[index as usize])
        }
        fn config_bonus_adder(&self) -> u32 {
            self.0.borrow().bonus_adder
        }
        fn config_coin_to_credit(&self) -> u32 {
            self.0.borrow().coin_to_credit
        }
        fn config_game_cost(&self, index: u32) -> AmResult<u32> {
            Ok(self.0.borrow().game_cost[index as usize])
        }
        fn config_coin_amount(&self) -> u32 {
            self.0.borrow().coin_amount
        }
        fn player_value(&self, index: u32) -> Option<PlayerCredit> {
            self.0.borrow().players[index as usize].map(|(credit, remain)| PlayerCredit { credit, remain })
        }
        fn addable_coin(&self, index: u32) -> u32 {
            self.0.borrow().addable[index as usize]
        }
        fn bookkeeping_coin(&self, _index: u32) -> AmResult<u32> {
            Ok(0)
        }
        fn bookkeeping_totals(&self) -> BookkeepingTotals {
            BookkeepingTotals::default()
        }
        fn lockout_on(&self) -> bool {
            self.0.borrow().lockout_on
        }
        fn can_lockout(&self) -> bool {
            self.0.borrow().can_lockout
        }
        fn coin_in_ignored(&self) -> bool {
            self.0.borrow().ignored
        }
        fn clear(&mut self) {
            self.0.borrow_mut().calls.push("clear".into());
        }
        fn pay(&mut self, player_index: u32, game_cost_index: u16, count: u16) -> AmResult<()> {
            self.0.borrow_mut().calls.push(format!("pay({player_index},{game_cost_index},{count})"));
            Ok(())
        }
        fn special_lockout(&mut self, on: bool) -> AmResult<()> {
            self.0.borrow_mut().calls.push(format!("special_lockout({on})"));
            Ok(())
        }
        fn set_coin_in_ignore(&mut self, ignore: bool) {
            self.0.borrow_mut().calls.push(format!("set_coin_in_ignore({ignore})"));
        }
        fn take_events(&mut self) -> Vec<InsertCoinEvent> {
            std::mem::take(&mut self.0.borrow_mut().events)
        }
    }

    fn libam_up() -> Rc<RefCell<Libam>> {
        Rc::new(RefCell::new(Libam { up: true, ..Libam::default() }))
    }

    fn controller(libam: &Rc<RefCell<Libam>>) -> CreditController {
        CreditController::new(Box::new(FakePort(Rc::clone(libam))))
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

    fn submit(harness: &mut Harness, generation: u32, task: u32, info: &[u8]) {
        let mut payload = [0; 0x38];
        payload[..info.len()].copy_from_slice(info);
        let patch = ClientCreditRequestPatch { generation: Some(generation), task: Some(task), info: Some(&payload) };
        harness.client.credit_mut().request_mut().copy_from(&patch).expect("plain fields");
    }

    /// Executes a request written into a fresh client image.
    fn run(controller: &mut CreditController, task: u32, info: &[u8]) -> AmResult<()> {
        let mut harness = Harness::new();
        submit(&mut harness, 1, task, info);
        controller.run_task(&harness.client.credit().request())
    }

    /// The client acknowledges the coin-in event of `generation`.
    fn acknowledge_coin_in(harness: &mut Harness, generation: u32) {
        harness.client.credit_mut().coin_in_mut().generation_mut().set(generation).expect("u32 accepts every value");
    }

    /// The published response `(generation, succeeded)`.
    fn response(harness: &Harness) -> (u32, bool) {
        let credit = harness.server.credit();
        let response = credit.response();
        (response.generation(), response.succeeded())
    }

    fn set_response_succeeded(harness: &mut Harness, succeeded: bool) {
        let mut credit = harness.server.credit_mut();
        credit.response_mut().succeeded_mut().set(succeeded).expect("a bool accepts both values");
    }

    /// Overwrites the given fields of one published player slot.
    fn seed_player(harness: &mut Harness, index: usize, patch: ServerPlayerPatch) {
        let mut credit = harness.server.credit_mut();
        let mut players = credit.players_mut();
        let mut player = players.get_mut(index).expect("a player slot");
        player.copy_from(&patch).expect("scalar fields accept every value");
    }

    /// One published player slot.
    fn player(harness: &Harness, index: usize) -> ServerPlayer {
        harness.server.credit().players().get(index).expect("a player slot").copy_into()
    }

    fn pay_info(player: u32, cost_index: u16, count: u16) -> Vec<u8> {
        let mut info = player.to_le_bytes().to_vec();
        info.extend(cost_index.to_le_bytes());
        info.extend(count.to_le_bytes());
        info
    }

    fn code_of(result: AmResult<()>) -> Option<u32> {
        result.err().map(|error| error.code)
    }

    #[test]
    fn tasks_from_five_are_rejected_with_code_5_before_any_libam_call() {
        let libam = libam_up();
        let mut controller = controller(&libam);
        for task in [5, 6, 0x1_0000, u32::MAX] {
            assert_eq!(code_of(run(&mut controller, task, &[])), Some(5), "task {task}");
        }
        assert!(libam.borrow().calls.is_empty());
    }

    #[test]
    fn each_accepted_task_class_forwards_its_payload_to_libam() {
        let libam = libam_up();
        let mut controller = controller(&libam);
        assert_eq!(code_of(run(&mut controller, 1, &[])), None);
        assert_eq!(code_of(run(&mut controller, 2, &pay_info(3, 2, 4))), None);
        assert_eq!(code_of(run(&mut controller, 3, &[1])), None);
        assert_eq!(code_of(run(&mut controller, 3, &[0])), None);
        assert_eq!(code_of(run(&mut controller, 4, &[1])), None);
        assert_eq!(
            libam.borrow().calls,
            ["clear", "pay(3,2,4)", "special_lockout(true)", "special_lockout(false)", "set_coin_in_ignore(true)"]
        );
    }

    #[test]
    fn accepted_request_is_acknowledged_succeeded_one_tick_after_acceptance() {
        let libam = libam_up();
        let mut slot = slot(&libam);
        let mut harness = Harness::running();
        tick(&mut slot, &mut harness);
        submit(&mut harness, 1, 1, &[]);

        tick(&mut slot, &mut harness);
        assert_eq!(libam.borrow().calls, ["clear"], "accepted in postExecute of the first tick");
        assert_eq!(harness.server.credit().response().generation(), 0, "not acknowledged yet");
        assert!(slot.controller().is_busy(&harness.ctx()));

        tick(&mut slot, &mut harness);
        assert_eq!(response(&harness), (1, true));
        assert!(!slot.controller().is_busy(&harness.ctx()));
        assert_eq!(libam.borrow().calls, ["clear"], "the request is executed once");
    }

    #[test]
    fn rejected_request_is_acknowledged_failed_one_tick_after_acceptance() {
        let libam = libam_up();
        let mut slot = slot(&libam);
        let mut harness = Harness::running();
        tick(&mut slot, &mut harness);
        set_response_succeeded(&mut harness, true);
        submit(&mut harness, 7, 0, &[]);

        tick(&mut slot, &mut harness);
        assert!(slot.controller().is_busy(&harness.ctx()));
        tick(&mut slot, &mut harness);
        assert_eq!(response(&harness), (7, false));
        assert!(libam.borrow().calls.is_empty());
    }

    #[test]
    fn accept_with_libam_down_fails_without_calling_libam() {
        let libam = libam_up();
        let mut slot = slot(&libam);
        let mut harness = Harness::running();
        tick(&mut slot, &mut harness);
        libam.borrow_mut().up = false;
        submit(&mut harness, 1, 2, &pay_info(0, 0, 1));
        tick(&mut slot, &mut harness);
        assert!(libam.borrow().calls.is_empty());
        tick(&mut slot, &mut harness);
        assert_eq!(response(&harness), (1, false));
    }

    #[test]
    fn clear_publishes_defaults_and_then_the_libam_configuration() {
        let libam = libam_up();
        {
            let mut libam = libam.borrow_mut();
            libam.enabled = true;
            libam.special_available = true;
            libam.chute_share = 1;
            libam.service_share = 2;
            libam.operation = 2;
            libam.multipliers = [2, 3];
            libam.bonus_adder = 5;
            libam.coin_to_credit = 4;
            libam.game_cost = [10, 11, 12, 13, 14, 15, 16, 17];
            libam.coin_amount = 50;
        }
        let mut controller = controller(&libam);
        let mut harness = Harness::running();
        let seed = ServerCreditPatch {
            response: Some(ResponsePatch { generation: Some(9), ..Default::default() }),
            coin_in: Some(ServerCoinInPatch { generation: Some(4), ..Default::default() }),
            special: Some(ServerSpecialDevicePatch { lockout_on: Some(true), ..Default::default() }),
            coin_in_ignored: Some(true),
            ..Default::default()
        };
        harness.server.credit_mut().copy_from(&seed).expect("scalar fields accept every value");
        controller.clear_server_data(&mut harness.ctx());

        let credit = harness.server.credit();
        assert!(credit.available() && credit.special().available());
        assert_eq!(
            credit.config().copy_into(),
            CreditConfig {
                coin_chute_common: true,
                service_common: false,
                freeplay: true,
                multipliers: [2, 3],
                bonus_adder: 5,
                coin_to_credit: 4,
                game_cost: [10, 11, 12, 13, 14, 15, 16, 17],
                coin_amount: 50,
            }
        );
        assert_eq!((credit.response().generation(), credit.coin_in().generation()), (9, 4), "not part of clear");
        assert!(credit.special().lockout_on() && credit.coin_in_ignored(), "not part of clear");
    }

    #[test]
    fn clear_without_libam_keeps_defaults_and_zeroes_state() {
        let libam = libam_up();
        libam.borrow_mut().up = false;
        let mut controller = controller(&libam);
        let mut harness = Harness::running();
        seed_player(&mut harness, 3, ServerPlayerPatch { credit: Some(7), ..Default::default() });
        let seed = ServerCreditPatch {
            bookkeeping: Some(CreditBookkeepingPatch { total_credit: Some(8), ..Default::default() }),
            ..Default::default()
        };
        harness.server.credit_mut().copy_from(&seed).expect("scalar fields accept every value");
        controller.clear_server_data(&mut harness.ctx());

        let credit = harness.server.credit();
        assert_eq!(credit.config().copy_into(), CLEAR_CONFIG);
        assert_eq!(credit.config().coin_amount(), 100);
        assert_eq!(credit.players().copy_into().map(|player| player.copy_into()), [ServerPlayer::default(); 8]);
        assert_eq!(credit.bookkeeping().copy_into(), CreditBookkeeping::default());
        assert!(!credit.available());
    }

    #[test]
    fn config_multiplier_lookup_failure_stores_zero() {
        let libam = libam_up();
        libam.borrow_mut().multipliers = [2, 3];
        libam.borrow_mut().multiplier_fails_at = Some(1);
        let mut controller = controller(&libam);
        let mut harness = Harness::running();
        controller.clear_server_data(&mut harness.ctx());
        assert_eq!(harness.server.credit().config().multipliers().copy_into(), [2, 0]);
    }

    #[test]
    fn update_keeps_player_values_when_libam_has_no_node_but_always_writes_addable_coin() {
        let libam = libam_up();
        {
            let mut libam = libam.borrow_mut();
            libam.players[2] = Some((5, 3));
            libam.addable = [9, 8, 7, 0, 0, 0, 0, 1];
        }
        let mut controller = controller(&libam);
        let mut harness = Harness::running();
        seed_player(&mut harness, 5, ServerPlayerPatch { credit: Some(99), remain: Some(98), ..Default::default() });
        seed_player(&mut harness, 3, ServerPlayerPatch { addable_coin: Some(55), ..Default::default() });
        controller.update_server_data(&mut harness.ctx());

        let (p2, p3, p5, p7) = (player(&harness, 2), player(&harness, 3), player(&harness, 5), player(&harness, 7));
        assert_eq!((p2.credit, p2.remain, p2.addable_coin), (5, 3, 7));
        assert_eq!((p5.credit, p5.remain), (99, 98), "no node: destination unchanged");
        assert_eq!(p3.addable_coin, 0, "addable coin is overwritten with libam's 0");
        assert_eq!(p7.addable_coin, 1);
    }

    #[test]
    fn update_refreshes_lockout_and_ignore_flags_but_not_availability_or_config() {
        let libam = libam_up();
        libam.borrow_mut().multipliers = [1, 1];
        let mut controller = controller(&libam);
        let mut harness = Harness::running();
        controller.clear_server_data(&mut harness.ctx());
        {
            let mut libam = libam.borrow_mut();
            libam.special_available = true;
            libam.enabled = true;
            libam.multipliers = [7, 7];
            libam.lockout_on = true;
            libam.can_lockout = true;
            libam.ignored = true;
        }
        controller.update_server_data(&mut harness.ctx());
        let credit = harness.server.credit();
        assert!(credit.special().lockout_on() && credit.special().can_lockout() && credit.coin_in_ignored());
        assert!(!credit.special().available() && !credit.available(), "written only by clear");
        assert_eq!(credit.config().multipliers().copy_into(), [1, 1], "written only by clear");
    }

    #[test]
    fn update_with_libam_down_still_acknowledges_requests() {
        let libam = libam_up();
        let mut slot = slot(&libam);
        let mut harness = Harness::running();
        tick(&mut slot, &mut harness);
        submit(&mut harness, 1, 0, &[]);
        tick(&mut slot, &mut harness);
        libam.borrow_mut().up = false;
        tick(&mut slot, &mut harness);

        assert_eq!(harness.server.credit().response().generation(), 1, "checkResult runs after the failed loaders");
    }

    #[test]
    fn coin_in_events_are_published_fifo_one_per_tick_and_only_after_the_previous_ack() {
        let libam = libam_up();
        let mut slot = slot(&libam);
        let mut harness = Harness::running();
        let event = |service: bool, player: usize| {
            let mut players = [false; 8];
            players[player] = true;
            InsertCoinEvent { service, players }
        };
        libam.borrow_mut().events = vec![event(false, 0), event(true, 5), event(false, 7)];

        tick(&mut slot, &mut harness);
        let credit = harness.server.credit();
        let coin_in = credit.coin_in();
        assert_eq!(coin_in.generation(), 1);
        assert_eq!(coin_in.sound(), Some(CreditSound::Coin));
        assert_eq!(coin_in.players().copy_into(), [true, false, false, false, false, false, false, false]);

        tick(&mut slot, &mut harness);
        tick(&mut slot, &mut harness);
        assert_eq!(harness.server.credit().coin_in().generation(), 1, "client has not acknowledged generation 1");

        acknowledge_coin_in(&mut harness, 1);
        tick(&mut slot, &mut harness);
        let credit = harness.server.credit();
        let coin_in = credit.coin_in();
        assert_eq!(coin_in.generation(), 2);
        assert_eq!(coin_in.sound(), Some(CreditSound::Service));
        assert_eq!(coin_in.players().copy_into(), [false, false, false, false, false, true, false, false]);

        acknowledge_coin_in(&mut harness, 2);
        tick(&mut slot, &mut harness);
        let credit = harness.server.credit();
        let coin_in = credit.coin_in();
        assert_eq!(coin_in.generation(), 3);
        assert_eq!(coin_in.players().copy_into(), [false, false, false, false, false, false, false, true]);

        acknowledge_coin_in(&mut harness, 3);
        tick(&mut slot, &mut harness);
        assert_eq!(harness.server.credit().coin_in().generation(), 3, "queue is empty");
    }

    #[test]
    fn coin_in_event_waits_for_an_ack_that_equals_the_server_generation() {
        let libam = libam_up();
        let mut slot = slot(&libam);
        let mut harness = Harness::running();
        acknowledge_coin_in(&mut harness, 5);
        libam.borrow_mut().events = vec![InsertCoinEvent { service: false, players: [true; 8] }];
        tick(&mut slot, &mut harness);
        assert_eq!(harness.server.credit().coin_in().generation(), 0, "ack 5 != server generation 0");
        acknowledge_coin_in(&mut harness, 0);
        tick(&mut slot, &mut harness);
        assert_eq!(harness.server.credit().coin_in().generation(), 1, "the event was kept while unacknowledged");
    }
}
