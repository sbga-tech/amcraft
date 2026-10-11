//! `amdaemon::server::SequenceController` (vtable `0x1405DC9F8`, ctor `0x1400435E0`).
//!
//! Publishes the eight players' play state and libam's play bookkeeping, and executes the client's `Clear`,
//! `PlayBegin`, `PlayContinue`, `PlayEnd`, `TestBegin` and `TestEnd` requests (credit, ALL.Net accounting and
//! Aime play log included). Evidence: `spec/controllers/error_sequence.md` Part 2 (claims
//! `error_sequence.sequence.*`).

use std::fmt::Write as _;

use crate::executor::{RequestExecutor, RequestState};
use crate::schema::sequence::{
    ClientSequenceRef, PLAYER_COUNT, PlayAccountingTiming, PlayErrorId, PlayInfo, SequenceBookkeepingPatch,
    SequenceTask, ServerPlayerDataPatch, TimeHistogramItemPatch,
};
use libam::sequence::{AimeLogStatus, CREDIT_AFFORDABLE, CREDIT_RULE_AIME_AMOUNT, SequencePort};

use crate::controllers::controller::{Controller, ControllerId, Ctx, commit_check};
use libam::config::ProcessConfig;

/// Invariant of every player-slot access: [`Operation::validate`] checked the index against `PLAYER_COUNT`.
const VALIDATED_PLAYER: &str = "the player index was validated below PLAYER_COUNT";

/// Aime id that means "no Aime" besides 0 (`AimeId` is reset to this when a play stops).
const NO_AIME: u32 = 0xFFFF_FFFF;

/// libam play accounting timing → `ServerData.sequence.playAccountingTiming` (config listener `0x1400468E0`).
impl From<libam::sequence::PlayAccountingTiming> for PlayAccountingTiming {
    fn from(timing: libam::sequence::PlayAccountingTiming) -> Self {
        match timing {
            libam::sequence::PlayAccountingTiming::Normal => Self::Normal,
            libam::sequence::PlayAccountingTiming::Begin => Self::Begin,
            libam::sequence::PlayAccountingTiming::End => Self::End,
        }
    }
}

/// `amdaemon::server::SequenceController`.
pub struct SequenceController {
    port: Box<dyn SequencePort>,
    executor: RequestExecutor,
}

impl std::fmt::Debug for SequenceController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SequenceController").field("executor", &self.executor).finish_non_exhaustive()
    }
}

impl SequenceController {
    /// Constructs the controller (`0x1400435E0`).
    pub fn new(port: Box<dyn SequencePort>) -> Self {
        Self { port, executor: RequestExecutor::default() }
    }
}

impl Controller for SequenceController {
    fn id(&self) -> ControllerId {
        ControllerId::Sequence
    }

    /// `0x140043720`.
    fn is_busy(&self, ctx: &Ctx<'_>) -> bool {
        self.executor.is_busy(ctx.client.sequence().generation(), ctx.server.sequence().response().generation())
    }

    // vf5 (`0x140043750`, `nullsub_154`) is empty: the trait default.

    /// vf6 (`0x140043760`): samples the players and the bookkeeping, then `checkResult`.
    fn update_server_data(&mut self, ctx: &mut Ctx<'_>) {
        let mut block = ctx.server.sequence_mut();
        block.test_mut().set(ctx.manager.test_mode()).expect("a bool accepts every value");
        let mut players = block.players_mut();
        for (id, slot) in (0..).zip(0..PLAYER_COUNT) {
            let mut player = players.get_mut(slot).expect("the slot is below PLAYER_COUNT");
            let was_playing = player.playing();
            let playing = is_playing(self.port.as_ref(), id);
            let mut patch = ServerPlayerDataPatch { playing: Some(playing), ..ServerPlayerDataPatch::default() };
            if was_playing && !playing {
                patch.accounting_handle = Some(0);
                patch.aime_id = Some(NO_AIME);
            }
            player.copy_from(&patch).expect("scalar fields accept every value");
        }
        get_data(self.port.as_ref(), ctx);
        // The checker is the constant lambda `0x1400468D0`: an accepted request always succeeds.
        let request_generation = ctx.client.sequence().generation();
        let mut block = ctx.server.sequence_mut();
        let response_generation = block.response().generation();
        let outcome = self.executor.check_result(request_generation, response_generation, || RequestState::Succeeded);
        commit_check(outcome, &mut block.response_mut());
    }

    /// vf7 (`0x140043820`): the accept edge.
    fn accept_requests(&mut self, ctx: &mut Ctx<'_>) {
        let Self { port, executor } = self;
        let request_generation = ctx.client.sequence().generation();
        let response_generation = ctx.server.sequence().response().generation();
        executor.accept(request_generation, response_generation, || Operation { port: port.as_mut(), ctx }.accept());
    }

    /// Config listener lambda `0x1400468E0`: `Srv+0x204 = Config+0x4ED60`.
    fn on_process_config(&mut self, config: &ProcessConfig, ctx: &mut Ctx<'_>) {
        ctx.server
            .sequence_mut()
            .play_accounting_timing_mut()
            .set(config.sequence.play_accounting_timing.into())
            .expect("a closed enum accepts every declared value");
    }
}

/// `SequenceController::isPlaying` (`0x140045440`): `false` when the sequence facade is absent.
fn is_playing(port: &dyn SequencePort, index: u32) -> bool {
    if !port.sequence_facade_available() {
        tracing::warn!("libam is not ready");
        return false;
    }
    port.is_playing(index)
}

/// `SequenceController::getData` (`0x140045540`): copies libam's bookkeeping into the wire block.
///
/// libam keeps the histogram at most 32 rows long; the original trusts the count and would overrun the block
/// otherwise, here rows beyond the 32 slots are not fetched.
fn get_data(port: &dyn SequencePort, ctx: &mut Ctx<'_>) {
    if !port.sequence_facade_available() {
        tracing::warn!("libam is not ready");
        return;
    }
    let totals = port.bookkeeping_totals();
    let count = port.histogram_count();
    let mut block = ctx.server.sequence_mut();
    let mut bookkeeping = block.bookkeeping_mut();
    let patch = SequenceBookkeepingPatch {
        number_of_games: Some(totals.number_of_games),
        total_time: Some(totals.total_time),
        play_time: Some(totals.play_time),
        average_play_time: Some(totals.average_play_time),
        longest_play_time: Some(totals.longest_play_time),
        shortest_play_time: Some(totals.shortest_play_time),
        time_histogram_count: Some(count),
        ..SequenceBookkeepingPatch::default()
    };
    bookkeeping.copy_from(&patch).expect("scalar fields accept every value");
    let mut rows = bookkeeping.time_histogram_mut();
    for index in 0..count {
        let Some(mut row) = rows.get_mut(index as usize) else {
            break;
        };
        match port.histogram_row(index) {
            Ok(item) => {
                let patch = TimeHistogramItemPatch {
                    lower: Some(item.lower),
                    upper: Some(item.upper),
                    count: Some(item.count),
                };
                row.copy_from(&patch).expect("scalar fields accept every value");
            }
            Err(_) => {
                tracing::warn!("libamw version does not match");
                row.clear();
            }
        }
    }
}

/// The three play operations differ only in their required player state and facades.
struct Validation {
    /// The player must already be playing (`continuePlay`, `endPlay`); otherwise it must not be.
    must_be_playing: bool,
    /// The credit facade is needed too (`beginPlay`, `continuePlay`).
    needs_credit: bool,
}

const BEGIN_VALIDATION: Validation = Validation { must_be_playing: false, needs_credit: true };
const CONTINUE_VALIDATION: Validation = Validation { must_be_playing: true, needs_credit: true };
const END_VALIDATION: Validation = Validation { must_be_playing: true, needs_credit: false };

/// Credit action of a play request: `gameCostIndex >= 0 && gameCostCount != 0`.
fn credit_cost(info: &PlayInfo) -> Option<(i16, u16)> {
    (info.game_cost_index >= 0 && info.game_cost_count != 0).then_some((info.game_cost_index, info.game_cost_count))
}

/// An Aime id other than `0` and `0xFFFFFFFF` identifies a card.
fn has_aime(aime_id: u32) -> bool {
    !matches!(aime_id, 0 | NO_AIME)
}

/// One acceptor run: the port and the tick context the request executes against.
struct Operation<'a, 'c> {
    port: &'a mut dyn SequencePort,
    ctx: &'a mut Ctx<'c>,
}

impl Operation<'_, '_> {
    /// `SequenceController::accept` (`0x140043880`).
    fn accept(&mut self) -> RequestState {
        let request = self.ctx.client.sequence();
        if self.ctx.manager.debug_dump() {
            tracing::info!("{}", trace_line(&request));
        }
        if !self.port.sequence_facade_available() {
            tracing::warn!("libam is not ready");
            return RequestState::Failed;
        }
        let outcome = match request.task_kind() {
            Some(SequenceTask::Clear) => {
                self.port.clear_bookkeeping();
                return RequestState::Pending;
            }
            // A play payload that fails its proof fails the request before any libam call.
            Some(SequenceTask::PlayBegin) => match request.play_info() {
                Some(info) => self.begin_play(&info),
                None => return RequestState::Failed,
            },
            Some(SequenceTask::PlayContinue) => match request.play_info() {
                Some(info) => self.continue_play(&info),
                None => return RequestState::Failed,
            },
            Some(SequenceTask::PlayEnd) => match request.play_info() {
                Some(info) => self.end_play(info),
                None => return RequestState::Failed,
            },
            Some(SequenceTask::TestBegin) => {
                if self.ctx.manager.test_mode() {
                    tracing::warn!("already in test mode");
                } else {
                    self.port.toggle_mode();
                }
                return RequestState::Pending;
            }
            Some(SequenceTask::TestEnd) => {
                if self.ctx.manager.test_mode() {
                    self.port.toggle_mode();
                } else {
                    tracing::warn!("already in game mode");
                }
                return RequestState::Pending;
            }
            None => return RequestState::Failed,
        };
        let error_id = outcome.err().unwrap_or(PlayErrorId::Success);
        self.ctx
            .server
            .sequence_mut()
            .error_id_mut()
            .set(error_id)
            .expect("a closed enum accepts every declared value");
        if outcome.is_ok() { RequestState::Pending } else { RequestState::Failed }
    }

    /// `beginPlay` (`0x140043B90`): credit check → accounting → credit pay → libam begin → Aime log.
    fn begin_play(&mut self, info: &PlayInfo) -> Result<(), PlayErrorId> {
        let timing = self.ctx.server.sequence().play_accounting_timing();
        let player = self.validate(&BEGIN_VALIDATION, info)?;
        self.check_credit(info)?;
        match timing {
            PlayAccountingTiming::Normal => self.begin_accounting(info, player)?,
            PlayAccountingTiming::Begin => {
                self.require_matching_flags(info)?;
                self.begin_accounting(info, player)?;
                self.end_accounting(info, player)?;
            }
            PlayAccountingTiming::End => {}
        }
        self.pay_credit(info)?;
        if self.port.begin_play(info.player_index).is_err() {
            tracing::warn!("beginning play failed");
        }
        self.set_aime_id(player, info.aime_id);
        self.send_aime_log(info, player, AimeLogStatus::Begin)
    }

    /// `continuePlay` (`0x1400441A0`): credit check → accounting → credit pay → libam continue → Aime log with
    /// the stored Aime id.
    fn continue_play(&mut self, info: &PlayInfo) -> Result<(), PlayErrorId> {
        let timing = self.ctx.server.sequence().play_accounting_timing();
        let player = self.validate(&CONTINUE_VALIDATION, info)?;
        self.check_credit(info)?;
        match timing {
            PlayAccountingTiming::Normal => {
                self.end_accounting(info, player)?;
                self.begin_accounting(info, player)?;
            }
            PlayAccountingTiming::Begin | PlayAccountingTiming::End => {
                self.require_matching_flags(info)?;
                self.begin_accounting(info, player)?;
                self.end_accounting(info, player)?;
            }
        }
        self.pay_credit(info)?;
        if self.port.continue_play(info.player_index).is_err() {
            tracing::warn!("continuing play failed");
        }
        self.send_aime_log(info, player, AimeLogStatus::Continue)
    }

    /// `endPlay` (`0x1400447C0`): accounting → libam end/cancel → Aime log (not for a canceled play). Works on its
    /// own copy of the request's [`PlayInfo`].
    fn end_play(&mut self, mut info: PlayInfo) -> Result<(), PlayErrorId> {
        let timing = self.ctx.server.sequence().play_accounting_timing();
        let player = self.validate(&END_VALIDATION, &info)?;
        let cancel = info.cancel;
        match timing {
            PlayAccountingTiming::Normal => {
                if cancel {
                    info.accounting_end.item_count = 0;
                }
                self.end_accounting(&info, player)?;
            }
            PlayAccountingTiming::Begin => {}
            PlayAccountingTiming::End => {
                if cancel {
                    info.accounting_begin.valid = false;
                    info.accounting_end.valid = false;
                } else {
                    self.require_matching_flags(&info)?;
                }
                self.begin_accounting(&info, player)?;
                self.end_accounting(&info, player)?;
            }
        }
        let ended =
            if cancel { self.port.cancel_play(info.player_index) } else { self.port.end_play(info.player_index) };
        if ended.is_err() {
            tracing::warn!("ending play failed");
        }
        if cancel { Ok(()) } else { self.send_aime_log(&info, player, AimeLogStatus::End) }
    }

    /// The shared prologue of the three play operations; returns the validated player slot.
    fn validate(&mut self, rules: &Validation, info: &PlayInfo) -> Result<usize, PlayErrorId> {
        let facades = self.port.error_facade_available()
            && (!rules.needs_credit || self.port.credit_facade_available())
            && self.port.sequence_facade_available();
        if !facades {
            tracing::warn!("libam is not ready");
            return Err(PlayErrorId::InvalidState);
        }
        let player = match usize::try_from(info.player_index) {
            Ok(player) if player < PLAYER_COUNT => player,
            _ => {
                tracing::warn!("invalid player index");
                return Err(PlayErrorId::InvalidArgument);
            }
        };
        if self.port.fatal_error_pending() {
            tracing::warn!("a fatal error is pending");
            return Err(PlayErrorId::InvalidState);
        }
        if self.port.is_playing(info.player_index) != rules.must_be_playing {
            if rules.must_be_playing {
                tracing::warn!("player has not started");
            } else {
                tracing::warn!("player has already started");
            }
            return Err(PlayErrorId::InvalidState);
        }
        Ok(player)
    }

    /// Credit availability check; skipped when the request carries no credit action.
    fn check_credit(&mut self, info: &PlayInfo) -> Result<(), PlayErrorId> {
        let Some((cost_index, cost_count)) = credit_cost(info) else {
            return Ok(());
        };
        if self.port.credit_check(info.player_index, cost_index, cost_count).ok() != Some(CREDIT_AFFORDABLE) {
            tracing::warn!("not enough credit");
            return Err(PlayErrorId::CreditPay);
        }
        Ok(())
    }

    /// Credit payment; skipped when the request carries no credit action.
    fn pay_credit(&mut self, info: &PlayInfo) -> Result<(), PlayErrorId> {
        let Some((cost_index, cost_count)) = credit_cost(info) else {
            return Ok(());
        };
        if self.port.credit_pay(info.player_index, cost_index, cost_count).is_err() {
            tracing::warn!("paying credit failed");
            return Err(PlayErrorId::CreditPay);
        }
        Ok(())
    }

    /// Both accounting records must be present or both absent.
    fn require_matching_flags(&mut self, info: &PlayInfo) -> Result<(), PlayErrorId> {
        if info.accounting_begin.valid != info.accounting_end.valid {
            // The original reused its invalid-player-index report for this mismatch.
            tracing::warn!("accounting flags do not match");
            return Err(PlayErrorId::InvalidArgument);
        }
        Ok(())
    }

    /// `beginAccounting` (`0x140044D10`): starts ALL.Net accounting and stores `handle + 1`.
    fn begin_accounting(&mut self, info: &PlayInfo, player: usize) -> Result<(), PlayErrorId> {
        let begin = info.accounting_begin;
        if !begin.valid {
            return Ok(());
        }
        if !self.port.accounting_facade_available() {
            tracing::warn!("libam is not ready");
            return Err(PlayErrorId::InvalidState);
        }
        if !self.port.accounting_available() {
            tracing::warn!("allnet accounting is busy");
            return Err(PlayErrorId::InvalidState);
        }
        match self.port.begin_accounting(info.player_index, begin.kind_code, begin.status_code) {
            Ok(handle) => {
                self.set_accounting_handle(player, handle.wrapping_add(1));
                Ok(())
            }
            Err(_) => {
                tracing::warn!("beginning allnet accounting failed");
                Err(PlayErrorId::AccountingBegin)
            }
        }
    }

    /// `endAccounting` (`0x140044F40`): ends the stored ALL.Net accounting; a no-op without a handle or record.
    fn end_accounting(&mut self, info: &PlayInfo, player: usize) -> Result<(), PlayErrorId> {
        let end = info.accounting_end;
        let handle = self.accounting_handle(player);
        if handle == 0 || !end.valid {
            return Ok(());
        }
        if !self.port.accounting_facade_available() {
            tracing::warn!("libam is not ready");
            return Err(PlayErrorId::InvalidState);
        }
        match self.port.end_accounting(info.player_index, end.kind_code, end.status_code, end.item_count, handle - 1) {
            Ok(()) => {
                self.set_accounting_handle(player, 0);
                Ok(())
            }
            Err(_) => {
                tracing::warn!("ending allnet accounting failed");
                Err(PlayErrorId::AccountingEnd)
            }
        }
    }

    /// `sendAimeLog` (`0x140045100`) with the player's stored Aime id. A log failure does not fail the request.
    fn send_aime_log(&mut self, info: &PlayInfo, player: usize, status: AimeLogStatus) -> Result<(), PlayErrorId> {
        let aime_id = self.aime_id(player);
        if !has_aime(aime_id) {
            return Ok(());
        }
        if !self.port.aime_facade_available() || !self.port.credit_facade_available() {
            tracing::warn!("libam is not ready");
            return Err(PlayErrorId::InvalidState);
        }
        let mut credit = 0;
        if status != AimeLogStatus::End
            && let Some((cost_index, cost_count)) = credit_cost(info)
            && self.port.credit_rule() == CREDIT_RULE_AIME_AMOUNT
        {
            match self.port.credit_cost(cost_index) {
                Ok(cost) => credit = u32::from(cost_count).wrapping_mul(cost),
                Err(_) => tracing::warn!("credit config is unavailable"),
            }
        }
        let time = self.port.local_time();
        if self.port.aime_log(status, aime_id, &time, credit).is_err() {
            tracing::warn!("sending the aime play log failed");
        }
        Ok(())
    }

    /// ALL.Net accounting handle + 1 stored for a validated `player`; 0 means none.
    fn accounting_handle(&self, player: usize) -> u32 {
        self.ctx.server.sequence().players().get(player).expect(VALIDATED_PLAYER).accounting_handle()
    }

    /// Stores the ALL.Net accounting handle + 1 of a validated `player`.
    fn set_accounting_handle(&mut self, player: usize, handle: u32) {
        let mut block = self.ctx.server.sequence_mut();
        let mut players = block.players_mut();
        let mut slot = players.get_mut(player).expect(VALIDATED_PLAYER);
        slot.accounting_handle_mut().set(handle).expect("a u32 accepts every value");
    }

    /// Aime id stored for a validated `player`.
    fn aime_id(&self, player: usize) -> u32 {
        self.ctx.server.sequence().players().get(player).expect(VALIDATED_PLAYER).aime_id()
    }

    /// Stores the Aime id of a validated `player`.
    fn set_aime_id(&mut self, player: usize, aime_id: u32) {
        let mut block = self.ctx.server.sequence_mut();
        let mut players = block.players_mut();
        let mut slot = players.get_mut(player).expect(VALIDATED_PLAYER);
        slot.aime_id_mut().set(aime_id).expect("a u32 accepts every value");
    }
}

/// Request trace (`0x1400458E0`): `Sequence - task: <name>` and, for the play tasks, the payload.
///
/// A task without a name puts the original's stream into the failed state right after the prefix (it would
/// read a null name pointer); only the prefix is logged here. A play payload that fails its proof is not
/// printed. `cancel` is printed with `boolalpha`.
fn trace_line(request: &ClientSequenceRef<'_>) -> String {
    let mut line = String::from("Sequence - task: ");
    let Some(task) = request.task_kind() else {
        return line;
    };
    line.push_str(task.name());
    if task.is_play()
        && let Some(info) = request.play_info()
    {
        // Writing into a `String` cannot fail.
        let _ = write!(line, ", player: {}", info.player_index);
        if let Some((cost_index, cost_count)) = credit_cost(&info) {
            let _ = write!(line, ", cost index: {cost_index}, cost count: {cost_count}");
        }
        if info.accounting_begin.valid {
            let begin = &info.accounting_begin;
            let _ = write!(
                line,
                ", kind code(begin): {}, status code(begin): {}, item count(begin): {}",
                begin.kind_code, begin.status_code, begin.item_count
            );
        }
        if info.accounting_end.valid {
            let end = &info.accounting_end;
            let _ = write!(
                line,
                ", kind code(end): {}, status code(end): {}, item count(end): {}",
                end.kind_code, end.status_code, end.item_count
            );
        }
        if has_aime(info.aime_id) {
            let _ = write!(line, ", AimeID: {}", info.aime_id);
        }
        let _ = write!(line, ", cancel: {}", info.cancel);
    }
    line
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use crate::schema::sequence::{
        ClientSequencePatch, PlayAccounting, ServerPlayerDataRef, ServerSequenceRef, TimeHistogramItem,
    };
    use libam::result::{AmError, AmResult};
    use libam::sequence::{BookkeepingTotals, HistogramRow, ProcessState};
    use libam::time::Tm;

    use super::*;
    use crate::controllers::controller::ControllerSlot;
    use crate::controllers::controller::testing::Harness;

    const TIME: Tm = Tm { sec: 1, min: 2, hour: 3, mday: 4, mon: 5, year: 124, wday: 0, yday: 0, isdst: 0 };

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        CreditCheck { player: u32, cost_index: i16, cost_count: u16 },
        CreditPay { player: u32, cost_index: i16, cost_count: u16 },
        BeginAccounting { player: u32, kind: u32, status: u32 },
        EndAccounting { player: u32, kind: u32, status: u32, items: u32, handle: u32 },
        BeginPlay(u32),
        ContinuePlay(u32),
        EndPlay(u32),
        CancelPlay(u32),
        ClearBookkeeping,
        ToggleMode,
        AimeLog { status: AimeLogStatus, aime_id: u32, time: Tm, credit: u32 },
    }

    #[derive(Debug)]
    struct State {
        error_facade: bool,
        credit_facade: bool,
        accounting_facade: bool,
        fatal_pending: bool,
        playing: [bool; 8],
        fail_begin_play: bool,
        totals: BookkeepingTotals,
        histogram: Vec<HistogramRow>,
        histogram_fail_at: Option<u32>,
        credit_check: u32,
        credit_check_fails: bool,
        credit_pay_fails: bool,
        credit_rule: u32,
        costs: [u32; 8],
        credit_cost_fails: bool,
        accounting_available: bool,
        accounting_handle: u32,
        begin_accounting_fails: bool,
        end_accounting_fails: bool,
        aime_fails: bool,
        calls: Vec<Call>,
    }

    impl State {
        fn new() -> Self {
            Self {
                error_facade: true,
                credit_facade: true,
                accounting_facade: true,
                fatal_pending: false,
                playing: [false; 8],
                fail_begin_play: false,
                totals: BookkeepingTotals::default(),
                histogram: Vec::new(),
                histogram_fail_at: None,
                credit_check: CREDIT_AFFORDABLE,
                credit_check_fails: false,
                credit_pay_fails: false,
                credit_rule: CREDIT_RULE_AIME_AMOUNT,
                costs: [10, 20, 100, 0, 0, 0, 0, 0],
                credit_cost_fails: false,
                accounting_available: true,
                accounting_handle: 5,
                begin_accounting_fails: false,
                end_accounting_fails: false,
                aime_fails: false,
                calls: Vec::new(),
            }
        }
    }

    struct FakePort(Rc<RefCell<State>>);

    fn failure() -> AmError {
        AmError::code(AmError::FAILED)
    }

    impl SequencePort for FakePort {
        fn error_facade_available(&self) -> bool {
            self.0.borrow().error_facade
        }
        fn credit_facade_available(&self) -> bool {
            self.0.borrow().credit_facade
        }
        fn sequence_facade_available(&self) -> bool {
            true
        }
        fn accounting_facade_available(&self) -> bool {
            self.0.borrow().accounting_facade
        }
        fn aime_facade_available(&self) -> bool {
            true
        }
        fn fatal_error_pending(&self) -> bool {
            self.0.borrow().fatal_pending
        }
        fn is_playing(&self, index: u32) -> bool {
            self.0.borrow().playing.get(index as usize).copied().unwrap_or(false)
        }
        fn begin_play(&mut self, index: u32) -> AmResult {
            let mut state = self.0.borrow_mut();
            state.calls.push(Call::BeginPlay(index));
            if state.fail_begin_play {
                return Err(failure());
            }
            state.playing[index as usize] = true;
            Ok(())
        }
        fn continue_play(&mut self, index: u32) -> AmResult {
            self.0.borrow_mut().calls.push(Call::ContinuePlay(index));
            Ok(())
        }
        fn end_play(&mut self, index: u32) -> AmResult {
            let mut state = self.0.borrow_mut();
            state.calls.push(Call::EndPlay(index));
            state.playing[index as usize] = false;
            Ok(())
        }
        fn cancel_play(&mut self, index: u32) -> AmResult {
            let mut state = self.0.borrow_mut();
            state.calls.push(Call::CancelPlay(index));
            state.playing[index as usize] = false;
            Ok(())
        }
        fn clear_bookkeeping(&mut self) {
            self.0.borrow_mut().calls.push(Call::ClearBookkeeping);
        }
        fn toggle_mode(&mut self) {
            self.0.borrow_mut().calls.push(Call::ToggleMode);
        }
        fn bookkeeping_totals(&self) -> BookkeepingTotals {
            self.0.borrow().totals
        }
        fn histogram_count(&self) -> u32 {
            self.0.borrow().histogram.len() as u32
        }
        fn histogram_row(&self, index: u32) -> AmResult<HistogramRow> {
            let state = self.0.borrow();
            match state.histogram.get(index as usize) {
                Some(row) if state.histogram_fail_at != Some(index) => Ok(*row),
                _ => Err(AmError::code(AmError::INVALID_ARGUMENT)),
            }
        }
        fn credit_check(&self, player: u32, cost_index: i16, cost_count: u16) -> AmResult<u32> {
            let mut state = self.0.borrow_mut();
            state.calls.push(Call::CreditCheck { player, cost_index, cost_count });
            if state.credit_check_fails { Err(failure()) } else { Ok(state.credit_check) }
        }
        fn credit_pay(&mut self, player: u32, cost_index: i16, cost_count: u16) -> AmResult {
            let mut state = self.0.borrow_mut();
            state.calls.push(Call::CreditPay { player, cost_index, cost_count });
            if state.credit_pay_fails { Err(failure()) } else { Ok(()) }
        }
        fn credit_rule(&self) -> u32 {
            self.0.borrow().credit_rule
        }
        fn credit_cost(&self, cost_index: i16) -> AmResult<u32> {
            let state = self.0.borrow();
            match usize::try_from(cost_index).ok().and_then(|index| state.costs.get(index)) {
                Some(&cost) if !state.credit_cost_fails => Ok(cost),
                _ => Err(AmError::code(AmError::INVALID_ARGUMENT)),
            }
        }
        fn accounting_available(&self) -> bool {
            self.0.borrow().accounting_available
        }
        fn begin_accounting(&mut self, player: u32, kind_code: u32, status_code: u32) -> AmResult<u32> {
            let mut state = self.0.borrow_mut();
            state.calls.push(Call::BeginAccounting { player, kind: kind_code, status: status_code });
            if state.begin_accounting_fails { Err(failure()) } else { Ok(state.accounting_handle) }
        }
        fn end_accounting(
            &mut self,
            player: u32,
            kind_code: u32,
            status_code: u32,
            item_count: u32,
            handle: u32,
        ) -> AmResult {
            let mut state = self.0.borrow_mut();
            state.calls.push(Call::EndAccounting {
                player,
                kind: kind_code,
                status: status_code,
                items: item_count,
                handle,
            });
            if state.end_accounting_fails { Err(failure()) } else { Ok(()) }
        }
        fn local_time(&self) -> Tm {
            TIME
        }
        fn aime_log(&mut self, status: AimeLogStatus, aime_id: u32, time: &Tm, credit: u32) -> AmResult {
            let mut state = self.0.borrow_mut();
            state.calls.push(Call::AimeLog { status, aime_id, time: *time, credit });
            if state.aime_fails { Err(failure()) } else { Ok(()) }
        }
    }

    fn accounting(kind_code: u32, status_code: u32, item_count: u32) -> PlayAccounting {
        PlayAccounting { valid: true, kind_code, status_code, item_count }
    }

    /// A payload with every field zero: player 0, no credit action, no accounting, no Aime card, no cancel.
    fn no_info() -> PlayInfo {
        let no_accounting = PlayAccounting { valid: false, kind_code: 0, status_code: 0, item_count: 0 };
        PlayInfo {
            player_index: 0,
            game_cost_index: 0,
            game_cost_count: 0,
            accounting_begin: no_accounting,
            accounting_end: no_accounting,
            aime_id: 0,
            cancel: false,
        }
    }

    /// A request for `player` without credit action, accounting or Aime card.
    fn play_info(player: u32) -> PlayInfo {
        PlayInfo { player_index: player, game_cost_index: -1, ..no_info() }
    }

    /// Player 3 with credit action (cost 2 × 3), both accounting records and Aime card 0xA1.
    fn full_info() -> PlayInfo {
        PlayInfo {
            player_index: 3,
            game_cost_index: 2,
            game_cost_count: 3,
            accounting_begin: accounting(11, 22, 33),
            accounting_end: accounting(44, 55, 66),
            aime_id: 0xA1,
            ..no_info()
        }
    }

    /// The wire bytes of a play payload at the original's offsets (`ClientPlayInfo`, 0x30 bytes).
    fn info_bytes(info: &PlayInfo) -> [u8; 0x30] {
        fn put(bytes: &mut [u8; 0x30], offset: usize, value: &[u8]) {
            bytes[offset..offset + value.len()].copy_from_slice(value);
        }
        let mut bytes = [0; 0x30];
        put(&mut bytes, 0x00, &info.player_index.to_le_bytes());
        put(&mut bytes, 0x04, &info.game_cost_index.to_le_bytes());
        put(&mut bytes, 0x06, &info.game_cost_count.to_le_bytes());
        for (base, record) in [(0x08, &info.accounting_begin), (0x18, &info.accounting_end)] {
            put(&mut bytes, base, &[u8::from(record.valid)]);
            put(&mut bytes, base + 0x4, &record.kind_code.to_le_bytes());
            put(&mut bytes, base + 0x8, &record.status_code.to_le_bytes());
            put(&mut bytes, base + 0xC, &record.item_count.to_le_bytes());
        }
        put(&mut bytes, 0x28, &info.aime_id.to_le_bytes());
        put(&mut bytes, 0x2C, &[u8::from(info.cancel)]);
        bytes
    }

    fn credit_check() -> Call {
        Call::CreditCheck { player: 3, cost_index: 2, cost_count: 3 }
    }
    fn credit_pay() -> Call {
        Call::CreditPay { player: 3, cost_index: 2, cost_count: 3 }
    }
    fn begin_accounting() -> Call {
        Call::BeginAccounting { player: 3, kind: 11, status: 22 }
    }
    fn end_accounting(handle: u32) -> Call {
        Call::EndAccounting { player: 3, kind: 44, status: 55, items: 66, handle }
    }
    fn aime_log(status: AimeLogStatus, aime_id: u32, credit: u32) -> Call {
        Call::AimeLog { status, aime_id, time: TIME, credit }
    }

    struct Rig {
        harness: Harness,
        slot: ControllerSlot,
        state: Rc<RefCell<State>>,
        generation: u32,
    }

    impl Rig {
        fn new() -> Self {
            let state = Rc::new(RefCell::new(State::new()));
            let controller = SequenceController::new(Box::new(FakePort(Rc::clone(&state))));
            let harness = Harness::running();
            Self { harness, slot: ControllerSlot::new(Box::new(controller)), state, generation: 0 }
        }

        /// One `App::execute` iteration as far as this controller is concerned.
        fn frame(&mut self) {
            let mut ctx = self.harness.ctx();
            self.slot.pre_execute(&mut ctx);
            self.slot.post_execute(&mut ctx);
        }

        /// Sends a request with the next generation and lets it be accepted (frame 1) and completed (frame 2).
        /// Returns `succeeded` of the committed response.
        fn submit(&mut self, task: u32, info: PlayInfo) -> bool {
            self.generation += 1;
            self.write_request(self.generation, task, &info);
            self.frame();
            self.frame();
            let response = self.seq().response();
            assert_eq!(response.generation(), self.generation, "the request was acknowledged");
            response.succeeded()
        }

        /// The client writes a request (what the IPC exchange between pre and post delivers).
        fn write_request(&mut self, generation: u32, task: u32, info: &PlayInfo) {
            let bytes = info_bytes(info);
            let patch = ClientSequencePatch { generation: Some(generation), task: Some(task), info: Some(&bytes) };
            self.harness.client.sequence_mut().copy_from(&patch).expect("plain fields");
        }

        /// Runs a play task and returns its `PlayErrorId`; `succeeded` must agree with it.
        fn play(&mut self, task: SequenceTask, info: PlayInfo) -> PlayErrorId {
            let succeeded = self.submit(task as u32, info);
            let error_id = self.seq().error_id();
            assert_eq!(succeeded, error_id == PlayErrorId::Success);
            error_id
        }

        fn seq(&self) -> ServerSequenceRef<'_> {
            self.harness.server.sequence()
        }

        fn player(&self, index: usize) -> ServerPlayerDataRef<'_> {
            self.seq().players().get(index).expect("a player slot")
        }

        fn row(&self, index: usize) -> TimeHistogramItem {
            self.seq().bookkeeping().time_histogram().get(index).expect("a histogram row").copy_into()
        }

        fn set_error_id(&mut self, error_id: PlayErrorId) {
            self.harness.server.sequence_mut().error_id_mut().set(error_id).expect("a declared PlayErrorId");
        }

        /// Overwrites a player's stored ALL.Net handle and Aime id.
        fn set_player(&mut self, index: usize, accounting_handle: u32, aime_id: u32) {
            let patch = ServerPlayerDataPatch {
                accounting_handle: Some(accounting_handle),
                aime_id: Some(aime_id),
                ..ServerPlayerDataPatch::default()
            };
            let mut block = self.harness.server.sequence_mut();
            let mut players = block.players_mut();
            let mut player = players.get_mut(index).expect("a player slot");
            player.copy_from(&patch).expect("scalar fields accept every value");
        }

        fn calls(&self) -> Vec<Call> {
            std::mem::take(&mut self.state.borrow_mut().calls)
        }

        fn is_busy(&mut self) -> bool {
            let ctx = self.harness.ctx();
            self.slot.controller().is_busy(&ctx)
        }

        fn set_timing(&mut self, timing: PlayAccountingTiming) {
            self.harness.server.sequence_mut().play_accounting_timing_mut().set(timing).expect("a declared timing");
        }

        /// Player 3 is playing with ALL.Net handle 6 and Aime card 0xA1 (state left behind by a begin).
        fn playing_player_three(&mut self) {
            self.state.borrow_mut().playing[3] = true;
            self.set_player(3, 6, 0xA1);
        }
    }

    #[test]
    fn clear_task_clears_libam_bookkeeping_and_leaves_error_id_alone() {
        let mut rig = Rig::new();
        rig.set_error_id(PlayErrorId::CreditPay);
        assert!(rig.submit(0, no_info()));
        assert_eq!(rig.calls(), vec![Call::ClearBookkeeping]);
        assert_eq!(rig.seq().error_id(), PlayErrorId::CreditPay);
    }

    #[test]
    fn play_begin_runs_credit_check_accounting_credit_pay_libam_begin_aime_log_in_order() {
        let mut rig = Rig::new();
        rig.set_error_id(PlayErrorId::InvalidState);
        assert_eq!(rig.play(SequenceTask::PlayBegin, full_info()), PlayErrorId::Success);
        assert_eq!(
            rig.calls(),
            vec![
                credit_check(),
                begin_accounting(),
                credit_pay(),
                Call::BeginPlay(3),
                aime_log(AimeLogStatus::Begin, 0xA1, 300)
            ]
        );
        let player = rig.player(3);
        assert_eq!((player.playing(), player.accounting_handle(), player.aime_id()), (true, 6, 0xA1));
        assert_eq!(rig.seq().error_id(), PlayErrorId::Success, "errorId is written on success too");
    }

    #[test]
    fn play_begin_with_timing_begin_runs_both_accountings_before_paying() {
        let mut rig = Rig::new();
        rig.set_timing(PlayAccountingTiming::Begin);
        assert_eq!(rig.play(SequenceTask::PlayBegin, full_info()), PlayErrorId::Success);
        assert_eq!(
            rig.calls(),
            vec![
                credit_check(),
                begin_accounting(),
                end_accounting(5),
                credit_pay(),
                Call::BeginPlay(3),
                aime_log(AimeLogStatus::Begin, 0xA1, 300)
            ]
        );
        assert_eq!(rig.player(3).accounting_handle(), 0, "end accounting released the handle");
    }

    #[test]
    fn play_begin_without_credit_action_never_touches_credit() {
        for (index, count) in [(-1i16, 3u16), (2, 0), (-1, 0)] {
            let mut rig = Rig::new();
            let mut info = full_info();
            info.game_cost_index = index;
            info.game_cost_count = count;
            assert_eq!(rig.play(SequenceTask::PlayBegin, info), PlayErrorId::Success);
            assert_eq!(
                rig.calls(),
                vec![begin_accounting(), Call::BeginPlay(3), aime_log(AimeLogStatus::Begin, 0xA1, 0)],
                "cost index {index}, count {count}"
            );
        }
    }

    #[test]
    fn play_begin_rejects_player_index_eight_as_invalid_argument() {
        let mut rig = Rig::new();
        let mut info = full_info();
        info.player_index = 8;
        assert_eq!(rig.play(SequenceTask::PlayBegin, info), PlayErrorId::InvalidArgument);
        assert!(rig.calls().is_empty());
    }

    #[test]
    fn play_begin_in_the_wrong_state_is_invalid_state() {
        type Break = fn(&mut State);
        let cases: [(&str, Break); 4] = [
            ("error facade down", |s| s.error_facade = false),
            ("credit facade down", |s| s.credit_facade = false),
            ("fatal error pending", |s| s.fatal_pending = true),
            ("player already started", |s| s.playing[3] = true),
        ];
        for (label, break_it) in cases {
            let mut rig = Rig::new();
            break_it(&mut rig.state.borrow_mut());
            assert_eq!(rig.play(SequenceTask::PlayBegin, full_info()), PlayErrorId::InvalidState, "{label}");
            assert!(rig.calls().is_empty(), "nothing reaches libam, {label}");
        }
    }

    #[test]
    fn play_begin_credit_check_failures_are_credit_pay_errors() {
        for (rig_setup, label) in [
            ((|s: &mut State| s.credit_check = 1) as fn(&mut State), "not affordable"),
            (|s| s.credit_check_fails = true, "status error"),
        ] {
            let mut rig = Rig::new();
            rig_setup(&mut rig.state.borrow_mut());
            assert_eq!(rig.play(SequenceTask::PlayBegin, full_info()), PlayErrorId::CreditPay, "{label}");
            assert_eq!(rig.calls(), vec![credit_check()], "{label}: no accounting after a failed check");
        }
    }

    #[test]
    fn play_begin_credit_pay_failure_comes_after_accounting_and_stops_before_libam() {
        let mut rig = Rig::new();
        rig.state.borrow_mut().credit_pay_fails = true;
        assert_eq!(rig.play(SequenceTask::PlayBegin, full_info()), PlayErrorId::CreditPay);
        assert_eq!(rig.calls(), vec![credit_check(), begin_accounting(), credit_pay()]);
        assert_eq!(rig.player(3).accounting_handle(), 6, "accounting is not rolled back");
        assert_eq!(rig.player(3).aime_id(), 0, "the Aime id is stored only after libam begin");
    }

    #[test]
    fn play_begin_accounting_failures_map_to_their_error_ids() {
        type Break = fn(&mut State);
        let cases: [(PlayErrorId, &str, Break); 3] = [
            (PlayErrorId::InvalidState, "accounting facade down", |s| s.accounting_facade = false),
            (PlayErrorId::InvalidState, "accounting busy", |s| s.accounting_available = false),
            (PlayErrorId::AccountingBegin, "begin accounting fails", |s| s.begin_accounting_fails = true),
        ];
        for (expected, label, break_it) in cases {
            let mut rig = Rig::new();
            break_it(&mut rig.state.borrow_mut());
            assert_eq!(rig.play(SequenceTask::PlayBegin, full_info()), expected, "{label}");
            assert!(!rig.calls().contains(&Call::BeginPlay(3)), "{label}");
            assert_eq!(rig.player(3).accounting_handle(), 0);
        }
    }

    #[test]
    fn play_begin_libam_failure_still_sends_the_aime_log() {
        let mut rig = Rig::new();
        rig.state.borrow_mut().fail_begin_play = true;
        assert_eq!(rig.play(SequenceTask::PlayBegin, full_info()), PlayErrorId::Success);
        let calls = rig.calls();
        assert_eq!(calls.last(), Some(&aime_log(AimeLogStatus::Begin, 0xA1, 300)));
        assert!(!rig.player(3).playing(), "the client must watch `playing` for the real outcome");
    }

    #[test]
    fn play_continue_timing_normal_ends_the_previous_accounting_before_beginning_a_new_one() {
        let mut rig = Rig::new();
        assert_eq!(rig.play(SequenceTask::PlayBegin, full_info()), PlayErrorId::Success);
        rig.calls();
        rig.state.borrow_mut().accounting_handle = 9;
        let mut info = full_info();
        info.aime_id = 0xB2; // ignored: continue logs the id stored at begin
        assert_eq!(rig.play(SequenceTask::PlayContinue, info), PlayErrorId::Success);
        assert_eq!(
            rig.calls(),
            vec![
                credit_check(),
                end_accounting(5),
                begin_accounting(),
                credit_pay(),
                Call::ContinuePlay(3),
                aime_log(AimeLogStatus::Continue, 0xA1, 300)
            ]
        );
        assert_eq!(rig.player(3).accounting_handle(), 10);
    }

    #[test]
    fn play_continue_with_timing_begin_or_end_begins_then_ends() {
        for timing in [PlayAccountingTiming::Begin, PlayAccountingTiming::End] {
            let mut rig = Rig::new();
            rig.set_timing(timing);
            rig.playing_player_three();
            assert_eq!(rig.play(SequenceTask::PlayContinue, full_info()), PlayErrorId::Success);
            assert_eq!(
                rig.calls(),
                vec![
                    credit_check(),
                    begin_accounting(),
                    end_accounting(5),
                    credit_pay(),
                    Call::ContinuePlay(3),
                    aime_log(AimeLogStatus::Continue, 0xA1, 300)
                ],
                "timing {timing:?}"
            );
            let mut info = full_info();
            info.accounting_begin.valid = false;
            let mut rig = Rig::new();
            rig.set_timing(timing);
            rig.playing_player_three();
            assert_eq!(rig.play(SequenceTask::PlayContinue, info), PlayErrorId::InvalidArgument);
        }
    }

    #[test]
    fn play_end_timing_normal_ends_accounting_before_libam_and_logs_the_end_without_credit() {
        let mut rig = Rig::new();
        rig.playing_player_three();
        assert_eq!(rig.play(SequenceTask::PlayEnd, full_info()), PlayErrorId::Success);
        assert_eq!(
            rig.calls(),
            vec![end_accounting(5), Call::EndPlay(3), aime_log(AimeLogStatus::End, 0xA1, 0)],
            "no credit check or payment at the end, and no credit amount in the Aime end log"
        );
        let player = rig.player(3);
        assert_eq!((player.playing(), player.accounting_handle(), player.aime_id()), (false, 0, 0xFFFF_FFFF));
    }

    #[test]
    fn play_end_cancel_zeroes_the_item_count_cancels_and_skips_the_aime_log() {
        let mut rig = Rig::new();
        rig.playing_player_three();
        let mut info = full_info();
        info.cancel = true;
        assert_eq!(rig.play(SequenceTask::PlayEnd, info), PlayErrorId::Success);
        assert_eq!(
            rig.calls(),
            vec![Call::EndAccounting { player: 3, kind: 44, status: 55, items: 0, handle: 5 }, Call::CancelPlay(3)]
        );
    }

    #[test]
    fn play_end_timing_end_runs_both_accountings_unless_canceled() {
        let mut rig = Rig::new();
        rig.set_timing(PlayAccountingTiming::End);
        rig.state.borrow_mut().playing[3] = true;
        assert_eq!(rig.play(SequenceTask::PlayEnd, full_info()), PlayErrorId::Success);
        assert_eq!(
            rig.calls(),
            vec![begin_accounting(), end_accounting(5), Call::EndPlay(3)],
            "no stored Aime id, so no Aime log"
        );

        let mut rig = Rig::new();
        rig.set_timing(PlayAccountingTiming::End);
        rig.playing_player_three();
        let mut info = full_info();
        info.cancel = true;
        assert_eq!(rig.play(SequenceTask::PlayEnd, info), PlayErrorId::Success);
        assert_eq!(rig.calls(), vec![Call::CancelPlay(3)], "both records are suppressed on cancel");

        let mut rig = Rig::new();
        rig.set_timing(PlayAccountingTiming::End);
        rig.playing_player_three();
        let mut info = full_info();
        info.accounting_end.valid = false;
        assert_eq!(rig.play(SequenceTask::PlayEnd, info), PlayErrorId::InvalidArgument);
        assert!(rig.calls().is_empty(), "the play is not ended");

        let mut rig = Rig::new();
        rig.set_timing(PlayAccountingTiming::End);
        rig.playing_player_three();
        let mut info = full_info();
        info.accounting_end.valid = false;
        info.cancel = true;
        assert_eq!(rig.play(SequenceTask::PlayEnd, info), PlayErrorId::Success, "cancel overrides the flags");
    }

    #[test]
    fn play_end_accounting_failure_keeps_the_play_running() {
        let mut rig = Rig::new();
        rig.playing_player_three();
        rig.state.borrow_mut().end_accounting_fails = true;
        assert_eq!(rig.play(SequenceTask::PlayEnd, full_info()), PlayErrorId::AccountingEnd);
        assert_eq!(rig.calls(), vec![end_accounting(5)]);
        assert!(rig.player(3).playing());
        assert_eq!(rig.player(3).accounting_handle(), 6, "a failed end keeps the handle");
    }

    #[test]
    fn aime_log_is_skipped_without_a_card_and_its_failure_never_fails_the_request() {
        for aime_id in [0, 0xFFFF_FFFF] {
            let mut rig = Rig::new();
            let mut info = full_info();
            info.aime_id = aime_id;
            assert_eq!(rig.play(SequenceTask::PlayBegin, info), PlayErrorId::Success);
            assert!(!rig.calls().iter().any(|call| matches!(call, Call::AimeLog { .. })), "id {aime_id:#x}");
        }
        let mut rig = Rig::new();
        rig.state.borrow_mut().aime_fails = true;
        assert_eq!(rig.play(SequenceTask::PlayBegin, full_info()), PlayErrorId::Success);
    }

    #[test]
    fn aime_log_credit_amount_needs_rule_one_and_a_resolvable_cost() {
        let mut rig = Rig::new();
        rig.state.borrow_mut().credit_rule = 2;
        assert_eq!(rig.play(SequenceTask::PlayBegin, full_info()), PlayErrorId::Success);
        assert_eq!(rig.calls().last(), Some(&aime_log(AimeLogStatus::Begin, 0xA1, 0)));

        let mut rig = Rig::new();
        rig.state.borrow_mut().credit_cost_fails = true;
        assert_eq!(rig.play(SequenceTask::PlayBegin, full_info()), PlayErrorId::Success);
        assert_eq!(rig.calls().last(), Some(&aime_log(AimeLogStatus::Begin, 0xA1, 0)));
    }

    #[test]
    fn test_begin_toggles_the_mode_only_outside_test_mode() {
        let mut rig = Rig::new();
        rig.set_error_id(PlayErrorId::InvalidState);
        assert!(rig.submit(4, no_info()));
        assert_eq!(rig.calls(), vec![Call::ToggleMode]);
        assert_eq!(rig.seq().error_id(), PlayErrorId::InvalidState, "tasks 4/5 leave errorId alone");

        rig.harness.manager.on_change_state(ProcessState(5));
        assert!(rig.submit(4, no_info()), "the guard does not fail the request");
        assert!(rig.calls().is_empty());
        assert!(rig.seq().test());
    }

    #[test]
    fn test_end_toggles_the_mode_only_inside_test_mode() {
        let mut rig = Rig::new();
        assert!(rig.submit(5, no_info()));
        assert!(rig.calls().is_empty());

        let mut rig = Rig::new();
        rig.harness.manager.on_change_state(ProcessState(5));
        assert!(rig.submit(5, no_info()));
        assert_eq!(rig.calls(), vec![Call::ToggleMode]);
    }

    #[test]
    fn unknown_task_fails_and_leaves_error_id_alone() {
        let mut rig = Rig::new();
        rig.set_error_id(PlayErrorId::AccountingEnd);
        assert!(!rig.submit(6, full_info()));
        assert!(rig.calls().is_empty());
        assert_eq!(rig.seq().error_id(), PlayErrorId::AccountingEnd);
    }

    #[test]
    fn play_requests_are_accepted_after_the_update_and_acknowledged_by_the_next_one() {
        let mut rig = Rig::new();
        rig.generation += 1;
        rig.write_request(1, SequenceTask::PlayBegin as u32, &play_info(0));
        assert!(!rig.is_busy(), "not accepted yet");
        rig.frame();
        assert!(rig.is_busy());
        assert_eq!(rig.seq().response().generation(), 0);
        assert!(rig.calls().contains(&Call::BeginPlay(0)), "libam acts at accept time");
        rig.frame();
        assert!(!rig.is_busy());
        let response = rig.seq().response();
        assert_eq!((response.generation(), response.succeeded()), (1, true));
    }

    #[test]
    fn update_samples_players_and_clears_accounting_and_aime_when_a_play_stops() {
        let mut rig = Rig::new();
        rig.state.borrow_mut().playing = [true, false, false, false, false, true, false, false];
        rig.frame();
        let playing: [bool; 8] = std::array::from_fn(|index| rig.player(index).playing());
        assert_eq!(playing, [true, false, false, false, false, true, false, false]);
        for index in [0, 5] {
            rig.set_player(index, 7, 0x55);
        }
        rig.state.borrow_mut().playing[5] = false;
        rig.frame();
        assert_eq!((rig.player(0).accounting_handle(), rig.player(0).aime_id()), (7, 0x55), "still playing");
        assert_eq!((rig.player(5).accounting_handle(), rig.player(5).aime_id()), (0, 0xFFFF_FFFF), "stopped");
        assert_eq!(rig.player(2).aime_id(), 0, "never played: the initial zero stays");
    }

    #[test]
    fn update_publishes_bookkeeping_totals_and_histogram_rows() {
        let mut rig = Rig::new();
        {
            let mut state = rig.state.borrow_mut();
            state.totals = BookkeepingTotals {
                number_of_games: 1,
                total_time: 2,
                play_time: 3,
                average_play_time: 4,
                longest_play_time: 5,
                shortest_play_time: 6,
            };
            state.histogram = vec![
                HistogramRow { lower: 0, upper: 59, count: 1 },
                HistogramRow { lower: 60, upper: 119, count: 2 },
                HistogramRow { lower: 120, upper: 179, count: 3 },
            ];
            state.histogram_fail_at = Some(1);
        }
        let sentinel = TimeHistogramItemPatch { lower: Some(9), upper: Some(9), count: Some(9) };
        let mut block = rig.harness.server.sequence_mut();
        let mut bookkeeping = block.bookkeeping_mut();
        let mut rows = bookkeeping.time_histogram_mut();
        rows.get_mut(1).expect("a histogram row").copy_from(&sentinel).expect("scalar fields accept every value");
        rig.frame();
        let bookkeeping = rig.seq().bookkeeping();
        assert_eq!(
            [
                bookkeeping.number_of_games(),
                bookkeeping.total_time(),
                bookkeeping.play_time(),
                bookkeeping.average_play_time(),
                bookkeeping.longest_play_time(),
                bookkeeping.shortest_play_time(),
                bookkeeping.time_histogram_count()
            ],
            [1, 2, 3, 4, 5, 6, 3]
        );
        assert_eq!(rig.row(0), TimeHistogramItem { lower: 0, upper: 59, count: 1 });
        assert_eq!(rig.row(1), TimeHistogramItem { lower: 0, upper: 0, count: 0 }, "a failed row is zeroed");
        assert_eq!(rig.row(2), TimeHistogramItem { lower: 120, upper: 179, count: 3 });
    }
}
