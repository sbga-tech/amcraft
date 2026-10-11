//! The Manager poll (payment.md thinca section 5), the command layer and the sequencers (PayEmCmd), and the facade
//! operations that start them (PayEmMgr). Command objects of the original are the [`Kind`] variants here; the
//! SDK callbacks reach them through the poll, one element per ring per frame.

use libam_api::emoney::{EMoneyDealInfo, EMoneyEvent, LibamBrand};
use libam_api::result::{AmError, AmResult};

use super::card::{self, CardPoll, CardRefusal};
use super::deal_log::{self, DealRecord, FLAG_CASH};
use super::sms;
use super::thinca::ResultRecord;
use super::{CoinGate, CreditLink, DisplayOp, EMoney, FLAG_AUTH, FLAG_USABLE, NoCredit};
use crate::module::Cx;
use crate::nvram::{NvramContext, Record};
use backend::thinca::{CODE_BUSY, kind};

/// Where the marker block keeps the Thinca brand of the pending deal: the tail of the opaque part (`+0..35`,
/// `payment.md` DL.6), which neither the terminal identity (`+0..14`) nor the card id and item id (`+15..30`) of the
/// TASMS extra record use.
const BRAND_AT: usize = 32;

/// Runner status of a flow (`+8` of every sequencer / command in the original).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Run {
    Started,
    Running,
    Cancelling,
    Done,
}

#[derive(Debug, Clone)]
struct Pay {
    entry: usize,
    sdk_brand: u32,
    price: u32,
    count: u32,
    coin: bool,
    /// The credit unit of a coin deal (`PD+40`).
    player: u32,
    item_id: u64,
    record: ResultRecord,
    deal_notified: bool,
    error_latch: bool,
    awaiting_complete: bool,
}

/// The sub-command of the DeactivationSequencer that is current (PayEmSvc 10).
#[derive(Debug, Clone, Copy)]
enum Wait {
    /// `W` (5.2): waits up to 30 s for the network, fails at once on a hub error.
    Gate { since: u64 },
    /// The RemoveTerm commands, one brand after the other.
    No,
    /// The alive run with the three reports after the last brand.
    Alive,
    /// The error wait: after 3 s `CompleteTask(2, status)`.
    Error { since: u64, status: u32 },
}

/// The sub-command of the ActivationSequencer that is current (PayEmSvc 5.3).
#[derive(Debug, Clone)]
enum Act {
    /// `W`: waits up to 30 s for the network, the reader and a hub without error.
    Wait { since: u64 },
    /// `S`: the store-card read; `message` is the time a 3 s refusal message was shown.
    Card { message: Option<u64> },
    /// `I`: `initAuthTerm` runs; `applied` is the outcome of the preliminary-result handler, `had_mask` whether the
    /// terminal was authenticated before.
    Init { applied: Option<bool>, had_mask: bool },
    /// The alive run with the three reports after a successful `initAuthTerm`.
    Alive,
    /// The error wait: after 3 s `CompleteTask(1, status)`.
    Error { since: u64, status: u32 },
}

#[derive(Debug, Clone)]
enum Kind {
    Balance {
        entry: usize,
    },
    Payment(Pay),
    Deactivation {
        removing: Option<u32>,
        wait: Wait,
    },
    Activation {
        act: Act,
    },
    /// VerifyUnconfirmedDeal (recovery phases 3 and 4): the marker of the unfinished deal.
    Verify {
        deal_number: String,
        block: [u8; 48],
    },
}

/// The running flow (`M+27472`).
#[derive(Debug, Clone)]
pub(crate) struct Flow {
    kind: Kind,
    run: Run,
    /// An SDK command is active (listener-state `+0x210`).
    cmd: bool,
    /// Command `+0x24`: the SDK allows cancelling.
    cmd_cancellable: bool,
    cancel_retry: u32,
}

impl Flow {
    fn new(kind: Kind) -> Self {
        Self { kind, run: Run::Started, cmd: false, cmd_cancellable: false, cancel_retry: 0 }
    }

    pub(crate) fn is_running(&self) -> bool {
        matches!(self.run, Run::Started | Run::Running | Run::Cancelling)
    }

    fn task(&self) -> u32 {
        match &self.kind {
            Kind::Activation { .. } => 1,
            Kind::Deactivation { .. } => 2,
            Kind::Balance { .. } | Kind::Verify { .. } => 3,
            Kind::Payment(pay) => 4 + u32::from(pay.coin),
        }
    }

    fn is_payment_screen(&self) -> bool {
        matches!(self.kind, Kind::Payment(_) | Kind::Balance { .. })
    }

    fn is_activation_screen(&self) -> bool {
        matches!(self.kind, Kind::Activation { .. } | Kind::Deactivation { .. })
    }
}

fn text_of(units: &[u16]) -> String {
    crate::util::wide_to_string(units)
}

/// The last four characters of a card number (`0x14026E4C0`).
fn card_suffix(card: &str) -> &str {
    let start = card.char_indices().rev().nth(3).map_or(0, |(index, _)| index);
    &card[start..]
}

impl EMoney {
    fn emit(&mut self, event: EMoneyEvent) {
        let app = &self.config.app;
        let installed = match &event {
            EMoneyEvent::CompleteTask { .. } => app.complete_task_listener,
            EMoneyEvent::NotifyDealResult { .. } => app.notify_deal_result_listener,
            EMoneyEvent::PlaySound { .. } => app.play_sound_listener,
            EMoneyEvent::NotifyCancel { .. } => app.notify_cancel_listener,
            EMoneyEvent::HoldCard => app.hold_card_listener,
        };
        if installed {
            self.events.push(event);
        }
    }

    /// `0x1402A2BE0`.
    fn raise_complete(&mut self, task: u32, status: u32) {
        self.emit(EMoneyEvent::CompleteTask { task, status });
        if self.config.app.complete_task_listener {
            self.hub.hold_fired = false;
        }
    }

    /// `0x1402A2CC0`.
    fn raise_result(&mut self, task: u32, status: u32, brand: LibamBrand, with_deal: bool) {
        if with_deal {
            let mut deal = deal_log::empty_deal();
            deal.status = status;
            deal.brand = brand;
            deal.time = self.now;
            self.emit(EMoneyEvent::NotifyDealResult { task, deal });
        }
        self.raise_complete(task, status);
    }

    /// `0x1402A2E30`.
    fn raise_deal(&mut self, task: u32, deal: EMoneyDealInfo) {
        self.emit(EMoneyEvent::NotifyDealResult { task, deal });
    }

    /// `0x14026B160`: `NotifyCancel` only when the value changes.
    fn raise_cancel(&mut self, value: bool) {
        if self.hub.cancellable != value {
            self.hub.cancellable = value;
            self.emit(EMoneyEvent::NotifyCancel { cancellable: value });
        }
    }

    /// `0x1402A2EF0`.
    fn raise_hold(&mut self, value: bool) {
        if value && !self.hub.hold_fired {
            self.hub.hold_fired = true;
            self.emit(EMoneyEvent::HoldCard);
        }
    }

    fn line(&mut self, line: usize, text: &str, scroll: bool) {
        self.display_ops.push(DisplayOp::SetLine { line, text: text.to_owned(), scroll });
    }

    /// The Manager poll `0x140231150`: eleven steps, one element per ring.
    pub(super) fn poll(&mut self, cx: &mut Cx<'_>, credit: &mut dyn CreditLink) {
        let callbacks = self.callbacks.clone();
        let (activation, payment, cmd) = self
            .flow
            .as_ref()
            .map_or((false, false, false), |flow| (flow.is_activation_screen(), flow.is_payment_screen(), flow.cmd));
        if let Some(text) = callbacks.pop_activation_line0()
            && activation
        {
            self.line(0, text.as_deref().unwrap_or(""), true);
        }
        if let Some(text) = callbacks.pop_activation_line1()
            && activation
        {
            self.line(1, &text, false);
        }
        if let Some(text) = callbacks.pop_payment_line0()
            && payment
        {
            self.line(0, text.as_deref().unwrap_or(""), true);
        }
        if let Some(text) = callbacks.pop_payment_line1()
            && payment
        {
            self.line(1, &text, false);
        }
        if let Some(id) = callbacks.pop_button()
            && id != 0
        {
            let code = cx.backend.thinca.select_button(id);
            if code != 0 {
                tracing::info!(code, "emoney: select button failed");
            }
        }
        if let Some(value) = callbacks.pop_cancellable()
            && cmd
        {
            if let Some(flow) = &mut self.flow {
                flow.cmd_cancellable = value;
            }
            self.raise_cancel(value);
        }
        if let Some(value) = callbacks.pop_hold()
            && cmd
        {
            self.raise_hold(value);
        }
        if let Some(path) = callbacks.pop_sound()
            && cmd
        {
            let path = path.filter(|path| !path.is_empty()).map(|path| path.encode_utf16().collect());
            self.emit(EMoneyEvent::PlaySound { path });
        }
        let deal_id = callbacks.pop_deal_id();
        self.save_deal_info(cx, deal_id);
        if let Some(result) = callbacks.pop_progress()
            && cmd
        {
            let record = ResultRecord::build(&result, self.now);
            self.command_record(cx, credit, record, false);
        }
        if let Some(result) = callbacks.pop_finished()
            && cmd
        {
            let record = ResultRecord::build(&result, self.now);
            self.command_record(cx, credit, record, true);
        }
    }

    /// Step 9 (5.1): the deal id (or the legacy Nanaco trigger) goes into the marker of the `EmoneyInfo` record.
    fn save_deal_info(&mut self, cx: &Cx<'_>, deal_id: Option<String>) {
        let legacy_trigger = deal_id.is_none()
            && !self.saved_deal_info
            && self.hub.cancellable
            && matches!(&self.flow, Some(Flow { kind: Kind::Payment(pay), .. }) if pay.entry == 1);
        if deal_id.is_none() && !legacy_trigger {
            return;
        }
        let mut block = [0u8; 48];
        if let Some(Flow { kind: Kind::Payment(pay), .. }) = &self.flow {
            block[BRAND_AT..BRAND_AT + 4].copy_from_slice(&pay.sdk_brand.to_le_bytes());
            block[36..40].copy_from_slice(&pay.price.to_le_bytes());
            block[40..44].copy_from_slice(&pay.count.to_le_bytes());
            block[44] = if pay.coin { FLAG_CASH } else { 0 };
        }
        // `0x1402323F0` (9.4): the keychip serial block's 11 characters at `+0`, the 4-letter game id at `+11`; a
        // block that is not valid leaves its bytes as they are.
        let mut patch = |at: usize, text: &str, width: usize| {
            let bytes = &text.as_bytes()[..text.len().min(width)];
            block[at..at + bytes.len()].copy_from_slice(bytes);
        };
        patch(0, &self.terminal.keychip, 11);
        if let Some(title) = &self.title {
            patch(11, title, 4);
        }
        let id = deal_id.unwrap_or_default();
        if let Some(info) = &mut self.info
            && info.set_marker(&id, &block)
        {
            self.save_info(cx);
        }
        self.saved_deal_info = true;
    }

    /// The command-event slots 3 and 4 (`0x14026B390`, `0x14026B3B0`).
    fn command_record(&mut self, cx: &mut Cx<'_>, credit: &mut dyn CreditLink, record: ResultRecord, last: bool) {
        let Some(mut flow) = self.flow.take() else { return };
        match &mut flow.kind {
            Kind::Balance { entry } => {
                let entry = *entry;
                self.balance_record(&mut flow, entry, &record, last);
            }
            Kind::Payment(_) => self.payment_record(cx, credit, &mut flow, record, last),
            Kind::Deactivation { .. } => self.remove_record(cx, &mut flow, &record, last),
            Kind::Activation { .. } => self.init_record(cx, &mut flow, &record, last),
            Kind::Verify { deal_number, block } => {
                if last {
                    let (deal_number, block) = (deal_number.clone(), *block);
                    flow.cmd = false;
                    flow.run = Run::Done;
                    self.finish_verify(cx, credit, &deal_number, &block, Some(&record));
                }
            }
        }
        self.flow = Some(flow);
    }

    fn balance_record(&mut self, flow: &mut Flow, entry: usize, record: &ResultRecord, last: bool) {
        let brand = deal_log::libam_brand_of_entry(entry);
        let mut status = record.deal.status;
        if status == 2 && !self.hub.operation_allowed {
            status = 1;
        }
        if last {
            self.raise_result(3, status, brand, false);
            flow.cmd = false;
            flow.run = Run::Done;
        } else {
            let mut deal = record.deal.clone();
            deal.status = status;
            self.raise_deal(3, deal);
        }
    }

    /// `0x14026CC30`: announces the payment's deal.
    fn notify_deal(&mut self, pay: &mut Pay, task: u32) {
        let mut deal = pay.record.deal.clone();
        deal.brand = deal_log::libam_brand_of_entry(pay.entry);
        deal.amount = (pay.price * pay.count) as i32;
        self.raise_deal(task, deal);
        pay.deal_notified = true;
    }

    /// `0x14026CCC0`: the CompleteTask of the payment.
    fn complete_payment(&mut self, pay: &Pay, task: u32) {
        let mut status = pay.record.deal.status;
        if pay.error_latch && status != 3 {
            status = 1;
        }
        self.raise_complete(task, status);
        self.hub.sales_pending = true;
    }

    /// DL.3 `0x14026F0D0`: the persisted record and the memory ring.
    fn store_deal(&mut self, cx: &Cx<'_>, record: &ResultRecord, cash: bool, clear_marker: bool) {
        let Some(info) = &mut self.info else { return };
        let mut persisted = DealRecord::from_deal(&record.deal, record.sdk_brand, if cash { FLAG_CASH } else { 0 });
        let same = info.newest_is(&persisted.deal_number);
        if same {
            if let Some(newest) = info.ring_get(0, 0) {
                persisted.flags |= newest.flags;
                persisted.price = newest.price;
                persisted.count = newest.count;
            }
            info.ring_update(0, 0, &persisted);
        } else {
            if let Some(marker) = info.marker() {
                persisted.price =
                    u32::from_le_bytes([marker.block[36], marker.block[37], marker.block[38], marker.block[39]]);
                persisted.count =
                    u32::from_le_bytes([marker.block[40], marker.block[41], marker.block[42], marker.block[43]]);
                persisted.flags |= marker.block[44];
            }
            info.ring_append(0, &persisted);
        }
        if clear_marker {
            info.clear_marker();
        }
        let deal = persisted.to_deal();
        if same && self.ring.count() != 0 {
            self.ring.overwrite(0, deal);
        } else {
            self.ring.append(deal);
        }
        self.save_info(cx);
    }

    fn payment_record(
        &mut self,
        cx: &Cx<'_>,
        credit: &mut dyn CreditLink,
        flow: &mut Flow,
        record: ResultRecord,
        last: bool,
    ) {
        let task = flow.task();
        let Kind::Payment(mut pay) = std::mem::replace(&mut flow.kind, Kind::Activation { act: Act::Alive }) else {
            return;
        };
        let mut record = record;
        if record.has_deal {
            if pay.coin && record.deal.status == 4 {
                // `0x14026D0E0`: the paid coins go into the credit (`credit.addCoin`, `0x14022BAC0`); a refusal
                // latches the error (`PD+298`) and the deal fails.
                if credit.add_coin(pay.player, pay.count) {
                    record.deal.status = 0;
                } else {
                    pay.error_latch = true;
                    record.deal.status = 1;
                }
            }
            record.cash = pay.coin;
            self.store_deal(cx, &record, pay.coin, pay.coin);
        }
        let code = record.code;
        let status = record.deal.status;
        let brand = record.sdk_brand;
        pay.record = record;
        let continues = code == 801 || (matches!(code, 802 | 803) && matches!(brand, 5..=7 | 9));
        let pending_amount = status == 4 && !pay.coin;
        let notify = self.hub.abort || !(continues || pending_amount);
        if notify {
            self.notify_deal(&mut pay, task);
        }
        if last {
            flow.cmd = false;
            self.hub.operation_allowed = false;
            if pending_amount {
                pay.awaiting_complete = true;
            } else {
                if !pay.deal_notified {
                    self.notify_deal(&mut pay, task);
                }
                self.complete_payment(&pay, task);
                flow.run = Run::Done;
            }
        }
        flow.kind = Kind::Payment(pay);
    }

    fn remove_record(&mut self, cx: &Cx<'_>, flow: &mut Flow, record: &ResultRecord, last: bool) {
        if !last {
            return;
        }
        flow.cmd = false;
        let Kind::Deactivation { removing, .. } = &mut flow.kind else { return };
        let Some(brand) = removing.take() else { return };
        // `0x14026BE80`: the codes 0, 301 and 304 remove the brand; any other result ends the command with status 5.
        if !matches!(record.code, 0 | 301 | 304) {
            self.raise_complete(2, 1);
            flow.run = Run::Done;
            return;
        }
        if let Some(info) = &mut self.info {
            info.remove_brand(brand);
        }
        for entry in 0..9 {
            if deal_log::sdk_brand_of_entry(entry) == Some(brand) {
                self.brands.set_flag(entry, FLAG_AUTH, false);
            }
        }
        self.save_info(cx);
    }

    /// The runner step (`0x140256500`): starts, advances and finishes the current flow.
    pub(super) fn run_flow(&mut self, cx: &mut Cx<'_>) {
        let Some(mut flow) = self.flow.take() else { return };
        match flow.run {
            Run::Started => self.start_flow(cx, &mut flow),
            Run::Running => self.tick_flow(cx, &mut flow),
            Run::Cancelling => self.tick_cancel(cx, &mut flow),
            Run::Done => {}
        }
        let waiting = matches!(&flow.kind, Kind::Payment(pay) if pay.awaiting_complete);
        if flow.run == Run::Done && !waiting {
            self.card_reader.release();
            self.hub.abort = false;
            self.hub.cancellable = false;
            if !matches!(flow.kind, Kind::Verify { .. }) {
                self.display_ops.push(DisplayOp::ClearLines);
                self.display_ops.push(DisplayOp::LeaveModeA);
            }
            self.saved_deal_info = true;
            self.hub.operation_allowed = false;
        } else {
            self.flow = Some(flow);
        }
    }

    fn next_slip(&mut self, cx: &Cx<'_>) -> Option<u32> {
        let info = self.info.as_mut()?;
        let current = info.slip();
        let next = if current > 999_998 { 1 } else { current + 1 };
        info.set_slip(next);
        self.save_info(cx).then_some(next)
    }

    fn start_flow(&mut self, cx: &mut Cx<'_>, flow: &mut Flow) {
        let task = flow.task();
        match &mut flow.kind {
            Kind::Balance { entry } => {
                let entry = *entry;
                let brand = deal_log::libam_brand_of_entry(entry);
                if self.hub.abort {
                    self.raise_result(3, 2, brand, true);
                    flow.run = Run::Done;
                    return;
                }
                cx.backend.thinca.prelude();
                let slip = self.next_slip(cx);
                let code = match (slip, deal_log::sdk_brand_of_entry(entry)) {
                    (Some(slip), Some(sdk_brand)) if self.hub.first_error == 0 => {
                        cx.backend.thinca.balance_inquiry(slip, sdk_brand)
                    }
                    _ => 1,
                };
                if code != 0 {
                    self.raise_result(3, 1, brand, true);
                    flow.run = Run::Done;
                } else {
                    self.callbacks.payment_errors(&[]);
                    self.callbacks.payment_line0(None);
                    flow.cmd = true;
                    flow.run = Run::Running;
                }
            }
            Kind::Payment(pay) => {
                let brand = deal_log::libam_brand_of_entry(pay.entry);
                if self.hub.abort {
                    self.raise_result(task, 2, brand, true);
                    flow.run = Run::Done;
                    return;
                }
                cx.backend.thinca.prelude();
                let slip = self.next_slip(cx);
                let code = match (slip, deal_log::sdk_brand_of_entry(pay.entry)) {
                    (Some(slip), Some(sdk_brand)) if self.hub.first_error == 0 => {
                        cx.backend.thinca.payment(slip, pay.price * pay.count, sdk_brand)
                    }
                    _ => 1,
                };
                if code != 0 {
                    // `0x14026CE30`: the payment failed before the device was asked.
                    pay.record.deal.status = 1;
                    pay.record.deal.time = self.now;
                    let mut pay_copy = pay.clone();
                    self.notify_deal(&mut pay_copy, task);
                    self.complete_payment(&pay_copy, task);
                    flow.run = Run::Done;
                } else {
                    flow.cmd = true;
                    flow.run = Run::Running;
                }
            }
            Kind::Deactivation { wait, .. } => {
                if self.info.as_ref().is_none_or(deal_log::EmoneyInfo::is_blank) {
                    self.raise_complete(2, 0);
                    flow.run = Run::Done;
                    return;
                }
                *wait = Wait::Gate { since: self.tick_ms };
                self.line(0, "しばらくお待ちください", true);
                self.line(1, "", false);
                flow.run = Run::Running;
            }
            Kind::Activation { act } => {
                if self.hub.abort {
                    self.raise_complete(1, 2);
                    flow.run = Run::Done;
                    return;
                }
                self.line(0, "しばらくお待ちください", true);
                self.line(1, "", false);
                *act = Act::Wait { since: self.tick_ms };
                flow.run = Run::Running;
            }
            Kind::Verify { deal_number, block } => {
                let sdk_brand = u32::from_le_bytes([
                    block[BRAND_AT],
                    block[BRAND_AT + 1],
                    block[BRAND_AT + 2],
                    block[BRAND_AT + 3],
                ]);
                cx.backend.thinca.prelude();
                let slip = self.next_slip(cx);
                let code = match slip {
                    Some(slip) if sdk_brand != 0 => cx.backend.thinca.balance_inquiry(slip, sdk_brand),
                    _ => 1,
                };
                if code == 0 {
                    flow.cmd = true;
                    flow.run = Run::Running;
                } else {
                    let (deal_number, block) = (deal_number.clone(), *block);
                    self.finish_verify(cx, &mut NoCredit, &deal_number, &block, None);
                    flow.run = Run::Done;
                }
            }
        }
    }

    fn tick_flow(&mut self, cx: &mut Cx<'_>, flow: &mut Flow) {
        match flow.kind {
            Kind::Activation { .. } => self.tick_activation(cx, flow),
            Kind::Deactivation { .. } => self.tick_deactivation(cx, flow),
            Kind::Payment(_) | Kind::Balance { .. } => {
                if self.hub.abort && flow.cmd {
                    flow.run = Run::Cancelling;
                }
            }
            Kind::Verify { .. } => {}
        }
    }

    /// The cancelling state (`0x14026B580`): `cancelRequest` is retried while the SDK answers busy.
    fn tick_cancel(&mut self, cx: &mut Cx<'_>, flow: &mut Flow) {
        if !flow.cmd {
            if matches!(flow.kind, Kind::Activation { act: Act::Card { message: None } }) {
                self.card_reader.stop(false);
            }
            let task = flow.task();
            if matches!(flow.kind, Kind::Activation { .. } | Kind::Deactivation { .. }) {
                self.raise_cancel(false);
                self.raise_complete(task, 2);
            }
            flow.run = Run::Done;
            return;
        }
        if flow.cmd_cancellable {
            if flow.cancel_retry == 0 {
                flow.cancel_retry = 1;
            }
            if flow.cancel_retry > 0 {
                match cx.backend.thinca.cancel_request() {
                    0 => flow.cancel_retry = 0,
                    CODE_BUSY if flow.cancel_retry <= 100 => flow.cancel_retry += 1,
                    _ => flow.cancel_retry = 0,
                }
            }
        }
    }

    fn set_act(flow: &mut Flow, value: Act) {
        flow.kind = Kind::Activation { act: value };
    }

    /// The error wait of the ActivationSequencer: the message now, `CompleteTask(1, status)` after 3 s.
    fn act_error(&mut self, flow: &mut Flow, message: &str, status: u32) {
        self.line(0, message, true);
        self.line(1, "", false);
        self.raise_cancel(false);
        Self::set_act(flow, Act::Error { since: self.tick_ms, status });
    }

    /// The ActivationSequencer dispatch (5.3), once per frame.
    fn tick_activation(&mut self, cx: &mut Cx<'_>, flow: &mut Flow) {
        let Kind::Activation { act } = &flow.kind else { return };
        let act = act.clone();
        let now = self.tick_ms;
        if self.hub.abort {
            match act {
                Act::Wait { .. } => return self.act_error(flow, "ただいまお取扱いできません", 2),
                Act::Card { message: None } | Act::Init { .. } => {
                    flow.run = Run::Cancelling;
                    return;
                }
                _ => {}
            }
        }
        match act {
            Act::Wait { since } => {
                let lease = self.card_reader.lease();
                if self.hub.first_error != 0 {
                    self.act_error(flow, "ただいまお取扱いできません", 1);
                } else if self.inputs.modules_ready
                    && self.inputs.online
                    && self.inputs.reader_available
                    && lease.present
                    && self.card_reader.acquire()
                {
                    self.activation_ready(cx, flow);
                } else if now.saturating_sub(since) > 30_000 {
                    self.act_error(flow, "ただいまお取扱いできません", 1);
                }
            }
            Act::Card { message: None } => self.poll_card(cx, flow),
            Act::Card { message: Some(since) } => {
                // The card sequence ended without a store id: `CompleteTask(1, 1)` once the message is gone.
                if now.saturating_sub(since) > 3000 {
                    self.display_ops.push(DisplayOp::ClearLines);
                    self.raise_complete(1, 1);
                    flow.run = Run::Done;
                }
            }
            Act::Init { .. } => {}
            Act::Alive => {
                let ok = self.start_alive(cx, true);
                self.activation_alive_done(flow, ok);
            }
            Act::Error { since, status } => {
                if now.saturating_sub(since) > 3000 {
                    self.display_ops.push(DisplayOp::ClearLines);
                    self.raise_complete(1, status);
                    flow.run = Run::Done;
                }
            }
        }
    }

    /// `W` finished: the reader lease is taken (`0x1402563B0`) and its state byte must be 3 (`0x140256480`); a stored
    /// URL without authentication restarts the alive run, otherwise the card is read.
    fn activation_ready(&mut self, cx: &mut Cx<'_>, flow: &mut Flow) {
        if !self.card_reader.lease().ready {
            self.act_error(flow, "カードリーダーが対応していません", 1);
        } else if self.pending_url() {
            self.line(0, "しばらくお待ちください", true);
            self.line(1, "", false);
            self.raise_cancel(false);
            let ok = self.start_alive(cx, false);
            self.activation_alive_done(flow, ok);
        } else {
            self.enter_card(flow);
        }
    }

    /// The alive run of the sequencer has ended (5.3, the private-sequencer row).
    fn activation_alive_done(&mut self, flow: &mut Flow, ok: bool) {
        if self.is_registered() {
            self.raise_complete(1, 0);
            flow.run = Run::Done;
        } else if ok {
            self.enter_card(flow);
        } else {
            self.act_error(flow, "端末初期認証処理に失敗しました", 1);
        }
    }

    /// `S` starts (6.1 state 0): polling and the blue LED run on the reader.
    fn enter_card(&mut self, flow: &mut Flow) {
        self.line(0, "カードをかざしてください", true);
        self.line(1, "", false);
        self.raise_cancel(true);
        self.card_reader.start();
        Self::set_act(flow, Act::Card { message: None });
    }

    /// `S` (6.1 states 1 to 4): the answer of the reader decides the next step.
    fn poll_card(&mut self, cx: &mut Cx<'_>, flow: &mut Flow) {
        match self.card_reader.poll() {
            CardPoll::Waiting => {}
            CardPoll::Failed => self.refuse_card(flow, "このカードはお取扱いできません"),
            CardPoll::ReadFailed => {
                self.raise_hold(true);
                self.raise_cancel(true);
            }
            CardPoll::Read(read) => {
                self.raise_hold(true);
                let kind = if self.env.as_ref().is_some_and(|env| env.use_proxy) { 3 } else { 2 };
                match card::read_store_card(&read, kind) {
                    Ok(card) => {
                        self.card_reader.stop(false);
                        self.raise_cancel(false);
                        self.activate_terminal(cx, flow, &card);
                    }
                    Err(refusal) => {
                        let message = match refusal {
                            CardRefusal::Header => "このカードはお取扱いできません",
                            CardRefusal::Type => "カードタイプが一致していません",
                            CardRefusal::Decrypt => "ただいまお取扱いできません",
                        };
                        self.refuse_card(flow, message);
                    }
                }
            }
        }
    }

    /// 6.1 state 4: the red LED and a 3 s message.
    fn refuse_card(&mut self, flow: &mut Flow, message: &str) {
        self.card_reader.stop(true);
        self.line(0, message, true);
        self.line(1, "", false);
        self.raise_cancel(false);
        Self::set_act(flow, Act::Card { message: Some(self.tick_ms) });
    }

    /// `S` produced a store card (5.3 row S, case b): the Activate request, the certificate and `initAuthTerm`.
    fn activate_terminal(&mut self, cx: &mut Cx<'_>, flow: &mut Flow, card: &card::StoreCard) {
        let Ok(branch) = card.branch.parse::<u32>() else {
            self.raise_complete(1, 1);
            flow.run = Run::Done;
            return;
        };
        if card.store_card_id.is_empty() {
            self.raise_complete(1, 1);
            flow.run = Run::Done;
            return;
        }
        self.line(0, "しばらくお待ちください", true);
        self.line(1, "", false);
        let url = self.env.as_ref().map(|env| env.root_endpoint.clone()).unwrap_or_default();
        let model = self.inputs.model.clone().unwrap_or_default();
        let serial = self.terminal.serial.clone();
        let answer = sms::activate(
            &mut *cx.backend.http,
            &url,
            &model,
            &serial,
            &card.merchant_code,
            branch,
            &card.store_card_id,
        );
        let activated = match answer {
            Ok(activated) => activated,
            Err(error) => {
                tracing::warn!(%error, "emoney: activate failed");
                return self.act_error(flow, "端末初期認証処理に失敗しました", 1);
            }
        };
        self.service.adopt(activated.settings.clone(), &serial);
        let now = self.record_time();
        self.service.stamp(now);
        if !self.terminal.store(&*cx.backend.fs, &activated.settings.terminal_id, &activated.certificate) {
            tracing::warn!("emoney: the client certificate cannot be stored");
            return self.act_error(flow, "端末初期認証処理に失敗しました", 1);
        }
        if !self.send_certificate(cx) {
            tracing::warn!("emoney: the sdk refused the client certificate");
            return self.act_error(flow, "端末初期認証処理に失敗しました", 1);
        }
        let had_mask = self.info.as_ref().is_some_and(|info| info.raw_mask() != 0);
        cx.backend.thinca.prelude();
        if cx.backend.thinca.init_auth_term(&serial, &card.aux) != 0 {
            return self.act_error(flow, "ただいまお取扱いできません", 1);
        }
        flow.cmd = true;
        Self::set_act(flow, Act::Init { applied: None, had_mask });
    }

    /// `setClientCertificate` (9.3): `false` when the SDK failed; busy (202) is retried by Manager state 4.
    pub(super) fn send_certificate(&mut self, cx: &mut Cx<'_>) -> bool {
        match cx.backend.thinca.set_client_certificate(&self.terminal.certificate, &self.terminal.password) {
            0 => {
                self.terminal.certificate_accepted = true;
                true
            }
            CODE_BUSY => true,
            _ => false,
        }
    }

    /// The InitAuthTerm result handlers (`0x14026C3F0`, `0x14026C690`).
    fn init_record(&mut self, cx: &mut Cx<'_>, flow: &mut Flow, record: &ResultRecord, last: bool) {
        let Kind::Activation { act: Act::Init { applied, had_mask } } = &flow.kind else { return };
        let (mut applied, had_mask) = (*applied, *had_mask);
        if record.code == 0 && applied.is_none() {
            applied = Some(self.apply_activation(cx, record, had_mask));
        }
        if !last {
            Self::set_act(flow, Act::Init { applied, had_mask });
            return;
        }
        flow.cmd = false;
        if record.code == 0 && applied == Some(true) {
            self.hub.first_error = 0;
            self.life.error.clear();
            Self::set_act(flow, Act::Alive);
        } else {
            self.raise_complete(1, 1);
            flow.run = Run::Done;
        }
    }

    /// 8.5: the brand mask the SDK reported becomes the stored masks and the brand-list flags; `false` when the
    /// save failed.
    fn apply_activation(&mut self, cx: &Cx<'_>, record: &ResultRecord, had_mask: bool) -> bool {
        let raw = record.brand_mask;
        let tasms = self.env.as_ref().is_some_and(|env| env.use_proxy);
        let url = self.service.settings.terminals_url.clone();
        let mut legacy = 0u32;
        for entry in 0..9 {
            let Some(brand) = deal_log::sdk_brand_of_entry(entry) else { continue };
            let present = raw & (1 << brand) != 0;
            if let Some(bit) = deal_log::mask_bit_of_entry(entry).filter(|_| present) {
                legacy |= 1 << bit;
            }
            self.brands.set_flag(entry, FLAG_AUTH | FLAG_USABLE, present);
        }
        let counters = self.inputs.coin_counters;
        let Some(info) = self.info.as_mut() else { return true };
        let mut changed = info.save_activation_info(legacy, raw, &url, tasms);
        if !had_mask {
            changed |= info.set_reported_counters(counters);
        }
        !changed || self.save_info(cx)
    }

    fn set_wait(flow: &mut Flow, value: Wait) {
        if let Kind::Deactivation { wait, .. } = &mut flow.kind {
            *wait = value;
        }
    }

    /// The error wait of the DeactivationSequencer.
    fn deact_error(&mut self, flow: &mut Flow, message: &str) {
        self.line(0, message, true);
        self.line(1, "", false);
        Self::set_wait(flow, Wait::Error { since: self.tick_ms, status: 1 });
    }

    /// The recovery has settled: no unconfirmed deal remains (`DeactivationSequencer+288`).
    fn unconfirmed_remains(&self) -> bool {
        !matches!(self.recovery, 1 | 5)
    }

    /// The DeactivationSequencer dispatch (PayEmSvc 10), once per frame.
    fn tick_deactivation(&mut self, cx: &mut Cx<'_>, flow: &mut Flow) {
        let Kind::Deactivation { wait, .. } = &flow.kind else { return };
        let wait = *wait;
        let now = self.tick_ms;
        if self.hub.abort && flow.cmd {
            flow.run = Run::Cancelling;
            return;
        }
        match wait {
            Wait::Gate { since } => {
                let up = self.inputs.modules_ready && self.inputs.online;
                if self.hub.first_error != 0 || (!up && now.saturating_sub(since) > 30_000) {
                    self.deact_error(flow, "ただいまお取扱いできません");
                } else if up {
                    if self.unconfirmed_remains() {
                        self.deact_error(flow, "未了処理が残っています");
                    } else {
                        Self::set_wait(flow, Wait::No);
                        self.remove_next(cx, flow);
                    }
                }
            }
            Wait::No => {
                if !flow.cmd {
                    self.remove_next(cx, flow);
                }
            }
            Wait::Alive => {
                if self.start_alive(cx, true) {
                    self.display_ops.push(DisplayOp::ClearLines);
                    self.raise_complete(2, 0);
                    flow.run = Run::Done;
                } else {
                    self.deact_error(flow, "撤去通知に失敗しました");
                }
            }
            Wait::Error { since, status } => {
                if now.saturating_sub(since) > 3000 {
                    self.display_ops.push(DisplayOp::ClearLines);
                    self.raise_complete(2, status);
                    flow.run = Run::Done;
                }
            }
        }
    }

    /// RemoveTerm (`0x14026BD60`): the lowest brand of the raw mask goes to the SDK; with the mask empty the
    /// removal is complete.
    fn remove_next(&mut self, cx: &mut Cx<'_>, flow: &mut Flow) {
        if self.hub.abort {
            self.raise_complete(2, 2);
            flow.run = Run::Done;
            return;
        }
        let mask = self.info.as_ref().map_or(0, deal_log::EmoneyInfo::raw_mask);
        if mask == 0 {
            return self.finish_removal(flow);
        }
        if self.hub.first_error != 0 {
            self.raise_complete(2, 1);
            flow.run = Run::Done;
            return;
        }
        let brand = mask.trailing_zeros();
        cx.backend.thinca.prelude();
        if cx.backend.thinca.remove_term(brand) != 0 {
            self.deact_error(flow, "ただいまお取扱いできません");
            return;
        }
        if let Kind::Deactivation { removing, .. } = &mut flow.kind {
            *removing = Some(brand);
        }
        flow.cmd = true;
    }

    /// The last brand is gone (`0x14026BEE0`, `0x140273270`): the deal log is cleared, a stored URL is reported to
    /// the site (the `"90"` status), else the sequencer ends.
    fn finish_removal(&mut self, flow: &mut Flow) {
        if !self.is_registered() {
            self.ring.clear();
        }
        if self.pending_url() {
            self.line(0, "しばらくお待ちください", true);
            self.line(1, "", false);
            Self::set_wait(flow, Wait::Alive);
            return;
        }
        self.display_ops.push(DisplayOp::ClearLines);
        self.raise_complete(2, 0);
        flow.run = Run::Done;
    }

    /// The end of VerifyUnconfirmedDeal (`0x14026E1F0`): the balance inquiry of the deal's brand tells whether the
    /// payment went through. The same card (last four characters) with the stored balance-after: the deal was not
    /// performed (status 1); the balance lower by the amount: confirmed (a coin deal adds its coins); anything else,
    /// and a request that failed: unknown (status 3, which blocks new deals). `0x14026CC30` announces the result.
    fn finish_verify(
        &mut self,
        cx: &Cx<'_>,
        credit: &mut dyn CreditLink,
        deal_number: &str,
        block: &[u8; 48],
        result: Option<&ResultRecord>,
    ) {
        let word = |at: usize| u32::from_le_bytes([block[at], block[at + 1], block[at + 2], block[at + 3]]);
        let sdk_brand = word(BRAND_AT);
        let (price, count) = (word(36), word(40));
        let coin = block[44] & FLAG_CASH != 0;
        let amount = price.wrapping_mul(count).cast_signed();
        let stored =
            self.info.as_ref().and_then(|info| info.ring_get(0, 0)).filter(|record| record.deal_number == deal_number);
        let mut deal = match &stored {
            Some(stored) => stored.to_deal(),
            None => {
                let mut deal = deal_log::empty_deal();
                deal.brand = deal_log::libam_brand_of_sdk(sdk_brand);
                deal_log::set_deal_number(&mut deal, deal_number);
                deal.time = self.now;
                deal.amount = amount;
                deal
            }
        };
        let mut status = 3;
        let verified = result.filter(|result| result.code == 0);
        if let (Some(result), Some(stored)) = (verified, &stored) {
            let card = text_of(&result.deal.card_number);
            let after = result.deal.balance_after;
            if !card.is_empty() && card_suffix(&card) == card_suffix(&stored.card) {
                if after == stored.balance_after {
                    status = 1;
                } else if after == stored.balance_before.wrapping_sub(amount) {
                    // `0x14026D0E0`: the coin path hands the coins to the credit; a refusal fails the deal.
                    status = if !coin {
                        4
                    } else if credit.add_coin(0, count) {
                        0
                    } else {
                        1
                    };
                }
                if status != 3 {
                    deal.balance_after = after;
                }
            }
        }
        deal.status = status;
        if result.is_some() {
            let mut record = ResultRecord::empty();
            record.sdk_brand = sdk_brand;
            record.deal = deal.clone();
            record.has_deal = true;
            self.store_deal(cx, &record, coin, coin || status == 3);
        }
        if status == 3 {
            self.hub.blocked = true;
        }
        self.raise_deal(4 + u32::from(coin), deal);
        self.hub.sales_pending = true;
    }

    fn start(&mut self, kind: Kind) {
        self.hub.operation_allowed = true;
        self.hub.cancellable = false;
        self.display_ops.push(DisplayOp::EnterModeA);
        self.flow = Some(Flow::new(kind));
    }

    /// Recovery phase 3 (`0x1402566E0`): the pending deal becomes a VerifyUnconfirmedDeal command.
    pub(super) fn start_verify(&mut self, deal_number: String, block: [u8; 48]) {
        self.start(Kind::Verify { deal_number, block });
    }

    /// `0x14022FBD0`.
    pub(crate) fn terminal_auth(&mut self) -> AmResult<()> {
        if !self.operation_gate(true) {
            return Err(AmError::code(AmError::FAILED));
        }
        let foreign_owner = matches!(self.mode, 3 | 4) && !self.inputs.reader_available;
        if self.flow_busy() || foreign_owner {
            return Err(AmError::code(AmError::BUSY));
        }
        self.start(Kind::Activation { act: Act::Wait { since: self.tick_ms } });
        Ok(())
    }

    /// `0x14022FCC0`.
    pub(crate) fn terminal_remove(&mut self) -> AmResult<()> {
        if !self.operation_gate(false) {
            return Err(AmError::code(AmError::FAILED));
        }
        if self.flow_busy() {
            return Err(AmError::code(AmError::BUSY));
        }
        self.start(Kind::Deactivation { removing: None, wait: Wait::No });
        Ok(())
    }

    /// `0x14022F240`.
    pub(crate) fn balance(&mut self, brand: LibamBrand) -> AmResult<()> {
        let entry = self.brands.entry_of_brand(brand);
        if !self.brands.entries[entry].balance {
            return Err(AmError::code(AmError::INVALID_ARGUMENT));
        }
        if !self.deal_available() {
            return Err(AmError::code(AmError::FAILED));
        }
        if self.flow_busy() {
            return Err(AmError::code(AmError::BUSY));
        }
        self.start(Kind::Balance { entry });
        Ok(())
    }

    /// `0x14022F3F0`, with the item id as wide units (`0x14027F630`: 13 decimal digits).
    pub(crate) fn amount_pay(
        &mut self,
        amount: i32,
        item_count: u32,
        brand: LibamBrand,
        item_id: &[u16],
    ) -> AmResult<()> {
        if brand == LibamBrand::NONE {
            return Err(AmError::code(AmError::INVALID_ARGUMENT));
        }
        let text = text_of(item_id);
        let id = if item_id.is_empty() {
            0
        } else if item_id.len() == 13 && text.chars().all(|digit| digit.is_ascii_digit()) {
            text.parse::<u64>().map_err(|_| AmError::code(AmError::INVALID_ARGUMENT))?
        } else {
            return Err(AmError::code(AmError::INVALID_ARGUMENT));
        };
        self.start_payment(amount.cast_unsigned(), item_count, brand, id, None)
    }

    /// `0x14022F630` behind the credit gate `0x1402536E0`.
    pub(crate) fn coin_pay(&mut self, player: u32, coin: u32, brand: LibamBrand, gate: &CoinGate) -> AmResult<()> {
        gate.check(coin)?;
        if brand == LibamBrand::NONE {
            return Err(AmError::code(AmError::INVALID_ARGUMENT));
        }
        self.start_payment(gate.coin_amount, coin, brand, 0, Some(player))
    }

    fn start_payment(
        &mut self,
        price: u32,
        count: u32,
        brand: LibamBrand,
        item_id: u64,
        player: Option<u32>,
    ) -> AmResult<()> {
        if !self.deal_available() {
            return Err(AmError::code(AmError::FAILED));
        }
        if self.flow_busy() {
            return Err(AmError::code(AmError::BUSY));
        }
        let entry = self.brands.entry_of_brand(brand);
        let sdk_brand = deal_log::sdk_brand_of_entry(entry).unwrap_or(0);
        self.saved_deal_info = false;
        let pay = Pay {
            entry,
            sdk_brand,
            price,
            count,
            coin: player.is_some(),
            player: player.unwrap_or(0),
            item_id,
            record: ResultRecord::empty(),
            deal_notified: false,
            error_latch: false,
            awaiting_complete: false,
        };
        let _ = pay.item_id;
        self.start(Kind::Payment(pay));
        Ok(())
    }

    /// `0x14022FB70`: cancels the running flow while the SDK allows it.
    pub(crate) fn cancel(&mut self) {
        if !self.hub.cancellable {
            return;
        }
        if let Some(flow) = &mut self.flow
            && matches!(flow.run, Run::Started | Run::Running)
        {
            flow.run = Run::Cancelling;
        }
    }

    /// `0x14022F800` (DL.5).
    pub(crate) fn complete_pay(&mut self, nvram: &NvramContext) -> AmResult<()> {
        let paying = matches!(&self.flow, Some(Flow { kind: Kind::Payment(pay), .. }) if pay.awaiting_complete);
        if !self.config.enable || (!matches!(self.mode, 3 | 4) && !paying) {
            return Err(AmError::code(AmError::FAILED));
        }
        if self.flow.is_some() && !paying {
            return Err(AmError::code(AmError::BUSY));
        }
        let Some(info) = self.info.as_mut() else { return Err(AmError::code(AmError::FAILED)) };
        let pending = info.marker().is_some_and(|marker| info.newest_is(&marker.deal_number));
        let newest = info.ring_get(0, 0);
        let Some(mut record) = newest.filter(|record| pending && record.status == 3) else {
            return Err(AmError::with_message(4, "no pending deal"));
        };
        record.status = 0;
        info.ring_update(0, 0, &record);
        let deal = record.to_deal();
        if self.ring.count() != 0 {
            self.ring.overwrite(0, deal.clone());
        } else {
            self.ring.append(deal.clone());
        }
        info.clear_marker();
        let status = nvram.store_record(Record::EmoneyInfo, info.image());
        if let Some((code, _)) = NvramContext::status_error(status, self.develop) {
            self.set_error(code);
            return Err(AmError::code(AmError::FAILED));
        }
        if let Some(Flow { kind: Kind::Payment(pay), run, .. }) = &mut self.flow {
            pay.record.deal = deal;
            let mut pay = pay.clone();
            *run = Run::Done;
            self.flow = None;
            let task = 4 + u32::from(pay.coin);
            self.notify_deal(&mut pay, task);
            self.complete_payment(&pay, task);
            self.display_ops.push(DisplayOp::ClearLines);
            self.display_ops.push(DisplayOp::LeaveModeA);
            self.hub.operation_allowed = false;
        }
        Ok(())
    }
}

#[allow(dead_code)]
const _: u32 = kind::PAYMENT;
