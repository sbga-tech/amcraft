//! The user-started tasks of the Aime Pay module (`payment.md` PayApMgr 4.5, 5.7-5.12, 7, 8): QueryUserSetting,
//! Activation, Deactivation and the two payments, each a small state machine advanced once per frame by
//! [`AimePay::tick_runner`], with the events of PayApMgr 6.
//!
//! A server command is issued in one frame and its answer is applied by the next one (the hub's poll runs first in
//! the frame), so every command takes at least one frame.

use libam_api::aime_pay::AimePayEvent;

use super::proto::Call;
use super::record::{Compact, Entry};
use super::text;
use super::{AimePay, Links};
use crate::module::Cx;
use crate::nvram::Record;

/// WaitReadyEnvironment timeout (`payment.apmgr.seq.waitready`).
const WAIT_READY_MS: u64 = 30_000;

/// The payment variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `PaymentToCoin` (task 5).
    Coin {
        /// Credit unit receiving the coins.
        player: u32,
        /// Coins.
        coin: u32,
    },
    /// `PaymentToItem` (task 4).
    Item {
        /// Option number of the PayToItem command.
        option_number: i64,
    },
}

/// The step a task is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    ActWaitReady {
        start: u64,
    },
    ActCall,
    ActAccept,
    QusCall,
    PaySave,
    PayCheckUser,
    PayPinWait,
    PayPinCall,
    PayMain,
    PayCoin,
    PayLog,
    PayComplete,
    /// A result display (the timer of its `WaitCommand`); `ok` is the final status of the task.
    Result {
        until: u64,
        ok: bool,
    },
    Done(bool),
}

/// Which payment steps succeeded (`isAllOk`, `0x140289980`).
#[derive(Debug, Clone, Copy, Default)]
struct Steps {
    info: bool,
    checkin: bool,
    pay: bool,
    coin: bool,
    log: bool,
}

/// The running user task (`current`).
#[derive(Debug)]
pub struct Run {
    task: u32,
    stage: Stage,
    sent: bool,
    aime_id: u32,
    pin: String,
    card_id: [u8; 8],
    item_id: String,
    count: u32,
    kind: Option<Kind>,
    cancelled_by_user: bool,
    rejected: bool,
    ready_ok: bool,
    cancelling: bool,
    log_done: bool,
    steps: Steps,
}

impl Run {
    fn new(task: u32, stage: Stage) -> Self {
        Self {
            task,
            stage,
            sent: false,
            aime_id: 0,
            pin: String::new(),
            card_id: [0; 8],
            item_id: String::new(),
            count: 0,
            kind: None,
            cancelled_by_user: false,
            rejected: false,
            ready_ok: false,
            cancelling: false,
            log_done: false,
            steps: Steps::default(),
        }
    }

    /// The Activation task (2).
    pub fn activation(start: u64) -> Self {
        Self::new(2, Stage::ActWaitReady { start })
    }

    /// The Deactivation task (3); the caller shows its result.
    pub fn deactivation() -> Self {
        Self::new(3, Stage::Done(false))
    }

    /// The QueryUserSetting task (1).
    pub fn query_user(aime_id: u32) -> Self {
        Self { aime_id, ..Self::new(1, Stage::QusCall) }
    }

    /// A payment task (4 for items, 5 for coins).
    pub fn pay(kind: Kind, aime_id: u32, card_id: [u8; 8], item_id: String, count: u32) -> Self {
        let task = if matches!(kind, Kind::Coin { .. }) { 5 } else { 4 };
        Self { aime_id, card_id, item_id, count, kind: Some(kind), ..Self::new(task, Stage::PaySave) }
    }

    /// Shows a result for `until` and then ends with status `ok`.
    pub fn show_result(&mut self, until: u64, failed: bool) {
        self.stage = Stage::Result { until, ok: !failed };
    }

    /// Not finished (statuses 1..3).
    pub fn running(&self) -> bool {
        !matches!(self.stage, Stage::Done(_))
    }

    /// Waiting for the user (status 2).
    pub fn waiting(&self) -> bool {
        matches!(self.stage, Stage::ActAccept | Stage::PayPinWait)
    }

    /// The Activation accept step waits.
    pub fn accepting(&self) -> bool {
        self.stage == Stage::ActAccept
    }

    fn is_pay(&self) -> bool {
        self.task >= 4
    }

    fn question(&self) -> u32 {
        match self.stage {
            Stage::ActAccept => 1,
            Stage::PayPinWait => 2,
            _ => 0,
        }
    }

    /// `confirmWithParam` on a payment waiting for its PIN: resumes with `pin`.
    pub fn accept_pin(&mut self, pin: String) -> bool {
        if self.stage != Stage::PayPinWait {
            return false;
        }
        self.pin = pin;
        self.stage = Stage::PayPinCall;
        self.sent = false;
        true
    }
}

impl AimePay {
    /// `cancel` (`0x140239840`): only a task waiting for the user or for the environment reacts.
    pub fn cancel_waiting(&mut self) {
        if self.run.as_ref().is_some_and(|run| run.waiting() || matches!(run.stage, Stage::ActWaitReady { .. })) {
            self.abort_req = false;
            self.apply_cancel();
        }
    }

    /// The runner cancel (`payment.apmgr.state.tick2`): the running step fails at the next poll.
    fn apply_cancel(&mut self) {
        self.abort_req = false;
        self.cancel_req = true;
        if let Some(run) = self.run.as_mut() {
            match run.stage {
                Stage::PayPinWait => run.cancelled_by_user = true,
                Stage::ActAccept => run.rejected = true,
                _ => {}
            }
            if !matches!(run.stage, Stage::Result { .. } | Stage::Done(_)) {
                run.cancelling = true;
            }
        }
    }

    /// `0x140286910`: the accepted store information is saved and becomes the activated location.
    pub(super) fn accept_location(&mut self, cx: &mut Cx<'_>, links: &mut dyn Links) {
        let Some(mut run) = self.run.take() else { return };
        let location = self.server.location.clone();
        if let Some(rec) = self.rec.as_mut()
            && rec.set_location(&location)
        {
            let status = self.store_record(cx);
            self.fail_record(status);
        }
        if !self.has_error() {
            self.activated = location;
        }
        self.enter_act_result(cx, links, &mut run);
        self.run = Some(run);
    }

    fn store_record(&self, cx: &Cx<'_>) -> crate::nvram::RecordStatus {
        match &self.rec {
            Some(rec) => cx.nvram.store_record(Record::AimePayInfo, rec.image()),
            None => crate::nvram::RecordStatus::ContextFailed,
        }
    }

    /// `tick2` (`0x14023A960`): the runner supervision and the events.
    pub(super) fn tick_runner(&mut self, cx: &mut Cx<'_>, links: &mut dyn Links) {
        let was_waiting = self.run.as_ref().is_some_and(Run::waiting);
        self.advance_run(cx, links);
        if self.abort_req {
            self.apply_cancel();
        }
        if !self.cancel_req
            && let Some(run) = self.run.as_ref().filter(|run| run.running())
        {
            let display_ready = links.display_ready();
            self.allowed =
                if run.is_pay() { self.deal_available(display_ready) } else { self.service_ok(display_ready) };
            if !self.allowed {
                self.apply_cancel();
            } else if !was_waiting && run.waiting() {
                let question = run.question();
                self.push(AimePayEvent::NeedAcceptation { question });
            }
        }
        if let Some(run) = self.run.as_ref().filter(|run| run.is_pay()) {
            let task = run.task;
            if !self.deal_notified && self.deal.has_time() {
                if !self.allowed && self.deal.status == 2 {
                    self.deal.status = 1;
                }
                let deal = self.deal.to_api();
                self.push(AimePayEvent::NotifyDealResult { task, deal });
            }
            self.deal_notified = self.deal.has_time();
        }
        if let Some(Stage::Done(ok)) = self.run.as_ref().map(|run| run.stage) {
            let task = self.run.as_ref().map_or(0, |run| run.task);
            let kind = if task >= 4 {
                if self.has_error() { 1 } else { self.deal.status }
            } else if self.cancel_req {
                u32::from(self.allowed) + 1
            } else {
                u32::from(!ok)
            };
            self.push(AimePayEvent::CompleteTask { task, error_kind: kind });
            if task != 1 {
                links.display().leave_mode_a();
            }
            if task == 2 {
                self.need_recheck = true;
            }
            self.run = None;
            self.abort_req = false;
            self.cancel_req = false;
            self.skip_completion = false;
        }
    }

    fn advance_run(&mut self, cx: &mut Cx<'_>, links: &mut dyn Links) {
        let Some(mut run) = self.run.take() else { return };
        self.step(cx, links, &mut run);
        self.run = Some(run);
    }

    fn now_ms(cx: &Cx<'_>) -> u64 {
        cx.clock.tick_ms()
    }

    fn issue(&mut self, cx: &mut Cx<'_>, run: &mut Run, call: Call) {
        self.server.issue(call, &mut *cx.backend.http);
        run.sent = true;
    }

    /// Whether the command issued by the current step has finished.
    fn answered(&self, run: &mut Run) -> bool {
        let done = run.sent && self.server.is_idle();
        if done {
            run.sent = false;
        }
        done
    }

    fn step(&mut self, cx: &mut Cx<'_>, links: &mut dyn Links, run: &mut Run) {
        loop {
            if run.cancelling {
                run.cancelling = false;
                if run.sent {
                    self.server.cancel();
                    run.sent = false;
                }
                self.fail_step(cx, links, run);
                continue;
            }
            match run.stage {
                Stage::ActWaitReady { start } => {
                    if self.wait_ready == 1 {
                        run.ready_ok = true;
                        links.display().set_line(0, text::FETCHING_STORE, false);
                        links.display().set_line(1, "", false);
                        run.stage = Stage::ActCall;
                    } else if self.wait_ready == 2 || Self::now_ms(cx).saturating_sub(start) >= WAIT_READY_MS {
                        self.enter_act_result(cx, links, run);
                    } else {
                        return;
                    }
                }
                Stage::ActCall => {
                    if !run.sent {
                        self.issue(cx, run, Call::Activate);
                        return;
                    }
                    if !self.answered(run) {
                        return;
                    }
                    if self.skip_completion {
                        self.skip_completion = false;
                    } else {
                        self.exchange_done();
                    }
                    if self.server.ok() && self.server.kind() == 0 {
                        run.stage = Stage::ActAccept;
                        links.display().set_line(0, text::CONFIRM_STORE, false);
                    } else {
                        self.enter_act_result(cx, links, run);
                    }
                    return;
                }
                Stage::ActAccept | Stage::PayPinWait | Stage::Done(_) => return,
                Stage::QusCall => {
                    if !run.sent {
                        self.issue(cx, run, Call::CheckUser { aime_id: run.aime_id });
                        return;
                    }
                    if !self.answered(run) {
                        return;
                    }
                    run.stage = Stage::Done(self.server.ok() && self.server.kind() == 0);
                    return;
                }
                Stage::PaySave => {
                    let time = Compact::from_tm(&Self::now_tm(cx));
                    let pending = Self::pending_of(&self.deal, run.card_id, time);
                    let changed = self.rec.as_mut().is_some_and(|rec| rec.set_pending(&pending));
                    if changed {
                        let status = self.store_record(cx);
                        if self.fail_record(status) {
                            self.deal.status = 1;
                            self.fail_step(cx, links, run);
                            continue;
                        }
                    }
                    run.steps.info = true;
                    run.stage = Stage::PayCheckUser;
                }
                Stage::PayCheckUser => {
                    if !run.sent {
                        self.issue(cx, run, Call::CheckUser { aime_id: run.aime_id });
                        return;
                    }
                    if !self.answered(run) {
                        return;
                    }
                    self.update_deal(run);
                    if !(self.server.ok() && self.server.kind() == 0) {
                        self.fail_step(cx, links, run);
                    } else if self.server.checked_in {
                        run.steps.checkin = true;
                        run.stage = Stage::PayMain;
                    } else {
                        run.stage = Stage::PayPinWait;
                        links.display().set_line(0, text::CHECK_IN, false);
                    }
                    return;
                }
                Stage::PayPinCall => {
                    if !run.sent {
                        if run.pin.chars().count() != 4 {
                            tracing::warn!("aimepay: the pin is not four digits");
                            self.fail_step(cx, links, run);
                            continue;
                        }
                        let call = Call::CheckIn { aime_id: run.aime_id, pin: run.pin.clone() };
                        self.issue(cx, run, call);
                        return;
                    }
                    if !self.answered(run) {
                        return;
                    }
                    self.update_deal(run);
                    if self.server.ok() && self.server.kind() == 0 {
                        run.steps.checkin = true;
                        run.stage = Stage::PayMain;
                    } else {
                        self.fail_step(cx, links, run);
                    }
                    return;
                }
                Stage::PayMain => {
                    if !run.sent {
                        let call = match run.kind {
                            Some(Kind::Item { option_number }) => Call::PayToItem {
                                aime_id: run.aime_id,
                                item_id: run.item_id.clone(),
                                count: run.count,
                                option_number,
                            },
                            _ => {
                                Call::PayToCoin { aime_id: run.aime_id, item_id: run.item_id.clone(), count: run.count }
                            }
                        };
                        self.issue(cx, run, call);
                        return;
                    }
                    if !self.answered(run) {
                        return;
                    }
                    self.update_deal(run);
                    if self.server.ok() && self.server.kind() == 0 {
                        self.deal.amount = self.server.amount;
                        self.deal.receipt_id = self.server.receipt_id;
                        run.steps.pay = true;
                        run.stage =
                            if matches!(run.kind, Some(Kind::Coin { .. })) { Stage::PayCoin } else { Stage::PayLog };
                    } else {
                        self.fail_step(cx, links, run);
                    }
                    return;
                }
                Stage::PayCoin => {
                    let Some(Kind::Coin { player, coin }) = run.kind else { return };
                    if links.add_coin(player, coin) {
                        run.steps.coin = true;
                        run.stage = Stage::PayLog;
                    } else {
                        self.deal.status = 1;
                        self.fail_step(cx, links, run);
                    }
                }
                Stage::PayLog => {
                    run.log_done = true;
                    if self.save_log(cx, run.card_id) {
                        run.steps.log = true;
                        if matches!(run.kind, Some(Kind::Coin { .. })) {
                            run.stage = Stage::PayComplete;
                        } else {
                            self.enter_pay_result(cx, links, run);
                        }
                    } else {
                        self.enter_pay_result(cx, links, run);
                    }
                }
                Stage::PayComplete => {
                    if self.server.receipt_id == 0 {
                        self.enter_pay_result(cx, links, run);
                        continue;
                    }
                    if !run.sent {
                        let call = Call::CompletePayToCoin {
                            aime_id: run.aime_id,
                            item_id: run.item_id.clone(),
                            count: run.count,
                            receipt_id: self.server.receipt_id,
                        };
                        self.issue(cx, run, call);
                        return;
                    }
                    if !self.answered(run) {
                        return;
                    }
                    self.enter_pay_result(cx, links, run);
                }
                Stage::Result { until, ok } => {
                    if Self::now_ms(cx) >= until {
                        run.stage = Stage::Done(ok);
                    } else {
                        return;
                    }
                }
            }
        }
    }

    /// A step failed: the rest of the task is skipped and its result is shown.
    fn fail_step(&mut self, cx: &mut Cx<'_>, links: &mut dyn Links, run: &mut Run) {
        match run.stage {
            Stage::ActWaitReady { .. } | Stage::ActCall | Stage::ActAccept => self.enter_act_result(cx, links, run),
            Stage::QusCall => run.stage = Stage::Done(false),
            Stage::PaySave
            | Stage::PayCheckUser
            | Stage::PayPinWait
            | Stage::PayPinCall
            | Stage::PayMain
            | Stage::PayCoin
            | Stage::PayLog
            | Stage::PayComplete => {
                if !run.log_done {
                    run.log_done = true;
                    self.skip_log(cx, run.card_id);
                }
                self.enter_pay_result(cx, links, run);
            }
            Stage::Result { .. } | Stage::Done(_) => {}
        }
    }

    /// `0x1402861A0`: the answer of CheckUser, CheckIn, PayToCoin and PayToItem updates the deal.
    fn update_deal(&mut self, run: &Run) {
        let item = matches!(run.kind, Some(Kind::Item { .. }));
        self.deal.status =
            if !self.server.ok() { if item { 3 } else { 1 } } else { u32::from(self.server.kind() != 0) };
        self.deal.receipt_id = self.server.receipt_id;
        self.deal.error_code = self.server.kind();
        if let Some(code) = self.server.code() {
            let code = code.to_owned();
            self.deal.set_error_message(&code);
        }
    }

    /// SaveDealLog, normal path (`0x140260BD0`): the deal is appended to the record and to the history.
    fn save_log(&mut self, cx: &mut Cx<'_>, card_id: [u8; 8]) -> bool {
        self.deal.time = Self::now_tm(cx);
        if self.has_error() {
            return false;
        }
        let entry = Entry::from_deal(&self.deal, card_id);
        let Some(rec) = self.rec.as_mut() else { return false };
        rec.append(&entry);
        let status = self.store_record(cx);
        if self.fail_record(status) {
            return false;
        }
        self.history.push(self.deal.clone());
        true
    }

    /// SaveDealLog, skip path: a failed deal is still logged, a cancelled one only clears the pending record.
    fn skip_log(&mut self, cx: &mut Cx<'_>, card_id: [u8; 8]) {
        if self.deal.status != 0 {
            self.save_log(cx, card_id);
            return;
        }
        self.deal.status = 2;
        self.deal.time = Self::now_tm(cx);
        if let Some(rec) = self.rec.as_mut() {
            rec.clear_pending();
            let status = self.store_record(cx);
            self.fail_record(status);
        }
    }

    /// DisplayActivateResult (`0x140286E50`, `0x140286FB0`).
    fn enter_act_result(&mut self, cx: &mut Cx<'_>, links: &mut dyn Links, run: &mut Run) {
        let accepted = self.server.ok() && self.server.kind() == 0;
        let (line0, line1, wait) = if !run.ready_ok {
            (text::UNAVAILABLE.to_owned(), None, text::RESULT_WAIT)
        } else if accepted && !run.rejected {
            let line = if self.has_error() { text::STORE_SAVE_FAILED } else { text::STORE_SAVED };
            (line.to_owned(), None, text::RESULT_WAIT)
        } else if accepted {
            (text::CANCELLED.to_owned(), None, text::RESULT_WAIT)
        } else {
            let (line0, line1) = text::server_error(self.server.kind(), self.server.code());
            (line0, Some(line1), text::ERROR_WAIT)
        };
        links.display().set_line(0, &line0, false);
        if let Some(line1) = line1 {
            links.display().set_line(1, &line1, false);
        }
        let ok = accepted && run.ready_ok && self.activated.is_complete() && !self.has_error();
        run.stage = Stage::Result { until: Self::now_ms(cx) + wait * 1000, ok };
    }

    /// DisplayPaymentResult (`0x14028A090`, `0x14028A1F0`).
    fn enter_pay_result(&mut self, cx: &mut Cx<'_>, links: &mut dyn Links, run: &mut Run) {
        let coin = matches!(run.kind, Some(Kind::Coin { .. }));
        let all_ok = run.steps.info && run.steps.checkin && run.steps.pay && run.steps.log && (run.steps.coin || !coin);
        let server_failed = !self.server.ok() || self.server.kind() != 0;
        let (line0, line1, wait) = if all_ok {
            (text::SUCCEEDED.to_owned(), None, text::RESULT_WAIT)
        } else if run.cancelled_by_user {
            (text::CANCELLED.to_owned(), None, text::RESULT_WAIT)
        } else if server_failed {
            let (line0, line1) = text::server_error(self.server.kind(), self.server.code());
            (line0, Some(line1), text::ERROR_WAIT)
        } else if self.has_error() {
            (text::HISTORY_SAVE_FAILED.to_owned(), None, text::RESULT_WAIT)
        } else {
            (text::COIN_ADD_FAILED.to_owned(), None, text::RESULT_WAIT)
        };
        links.display().set_line(0, &line0, false);
        if let Some(line1) = line1 {
            links.display().set_line(1, &line1, false);
        }
        run.stage = Stage::Result { until: Self::now_ms(cx) + wait * 1000, ok: self.server.checked_in && all_ok };
    }
}
