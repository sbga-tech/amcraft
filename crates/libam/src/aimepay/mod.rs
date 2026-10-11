//! The Aime Pay module (`am::aimepay::Manager`, `spec/libam/payment.md` PayApMgr and PayApSrv): the module state
//! machine, the periodic server exchanges, the user-started tasks (see [`run`]) and the facade the port exposes.
//!
//! # Cross-module methods
//!
//! * [`AimePay::update`] (`0x140238CB0`, M.3): after the abaas log; the flow passes [`Inputs`] and the
//!   [`Links`] to the credit, Aime and display modules.
//! * [`AimePay::set_permit`] (`0x1402398F0`, from `0x140254C40` and the Operating step): `suspended` wins.
//! * [`AimePay::set_shutdown`] (`0x140238D90`, from the module-stop sequence `0x1402544A0` with `flag` = the
//!   sequence manager is in test mode).
//!
//! The HTTP exchange of every server command is blocking (`cx.backend.http`) and its answer is applied by the
//! next frame, like the asynchronous original.

mod links;
mod model;
mod port;
mod proto;
mod record;
mod run;
mod server;
#[cfg(test)]
mod tests;
mod text;

use crate::aime::AccessCode;
use crate::config::AimePayConfig;
use crate::result::{AmError, AmResult};

use crate::display::{Display, Lane};
use crate::module::{Cx, ErrorCell, Lifecycle, StateWord};
use crate::nvram::{GameId, Record, RecordStatus};
use crate::util::tm_from_date_time;

use self::model::{Deal, DealRing, Location, is_item_id};
use self::proto::{Call, Serial};
use self::record::{Compact, Entry, Info, Pending};
use self::run::{Kind, Run};
use self::server::Server;

pub use links::{drive, set_shutdown, update};
pub use port::{
    AimeId, AimePayAppConfig, AimePayDeal, AimePayEvent, AimePayHandle, AimePayLocation, AimePayPort,
    AimePaySummaryItem, AimePayUserSetting,
};

/// Seconds between idle runs while the machine is bootstrapping (state 2).
const PERIOD_BOOT: u64 = 60;
/// Seconds between idle runs in the steady state (state 3).
const PERIOD_STEADY: u64 = 900;
/// `err.code` of a failed `env.json` load.
const ERROR_ENV: u16 = 4104;
/// `netErr.code`: the communication with the server failed.
const MAX_ENDPOINT_UNITS: usize = 255;

/// What the main flow hands to [`AimePay::update`] (`0x14029D590`, `payment.apmgr.life.exec_inputs`).
#[derive(Debug, Clone, Default)]
pub struct Inputs {
    /// Credit state is 2, platform and dongle states are 2 or 3 and the display state is 2 or 3.
    pub prereq: bool,
    /// The ALL.Net authentication result is good.
    pub net_ready: bool,
    /// The platform's board serial (`XXXX-XXXNNNNNNNN`).
    pub board: Option<String>,
    /// The dongle's keychip serial.
    pub keychip: Option<String>,
    /// The dongle's four-letter game id.
    pub game_id: Option<String>,
    /// `clientId` of the ALL.Net authentication (0 until authenticated).
    pub client_id: u32,
    /// The EmoneyInfo record carries a non-zero identifier (`0x14025FB30`, `payment.apmgr.nv.dealblocked`).
    pub emoney_registered: bool,
    /// The platform's "AMFS mount drive is not D:" flag (maps NVRAM failures to 87).
    pub develop: bool,
}

/// What the Aime Pay module needs from the other modules.
pub trait Links {
    /// The customer display.
    fn display(&mut self) -> &mut Display;
    /// The display module is usable (`0x14022DF70`).
    fn display_ready(&self) -> bool;
    /// The sequence manager is in test mode (state 2 or 5).
    fn test_mode(&self) -> bool;
    /// The credit adapter's "ignore coin" byte.
    fn ignore_coin(&self) -> bool;
    /// Coins still addable for credit unit `player` (`0x14022BDD0`).
    fn addable(&self, player: u32) -> u32;
    /// `credit.addCoin(player, coin)` (`0x14022BAC0`): `true` when no credit error remains.
    fn add_coin(&mut self, player: u32, coin: u32) -> bool;
    /// The card id of the Aime card of `aime_id` in the Aime user-info list (`payment.apmgr.fac.cardid`).
    fn card_id(&self, aime_id: u32) -> Option<[u8; 8]>;
}

/// `am::aimepay::Manager` (`ctx+309592`).
#[derive(Debug, Default)]
pub struct AimePay {
    life: Lifecycle,
    config: AimePayConfig,
    /// `sub` of the module state.
    sub: u32,
    permit: bool,
    suspended: bool,
    shutdown: bool,
    /// The platform's "AMFS mount drive is not D:" flag, last seen.
    develop: bool,
    /// The loaded `AimePayInfo` record.
    rec: Option<Info>,
    activated: Location,
    history: DealRing,
    deal: Deal,
    /// `env.json` `root_endpoint`.
    env_url: String,
    game_id: Option<String>,
    board: Option<Serial>,
    keychip: Option<Serial>,
    server: Server,
    client_id: u32,
    sa_client_id: u32,
    client_id_changed: bool,
    net_ready: bool,
    net_changed: bool,
    recheck: bool,
    need_recheck: bool,
    alive_ok: bool,
    maintenance: bool,
    net_err: bool,
    items_loaded: bool,
    deal_blocked: bool,
    /// `AimePayInfo +1112` (`0x1402611F0`): the sales-management reporting of the e-money module is on.
    sms_enabled: bool,
    /// The Activation sequencer's WaitReadyEnvironment flag: 0 not ready, 1 ready, 2 abort.
    wait_ready: u8,
    timer_ms: u64,
    idle: Option<Call>,
    run: Option<Run>,
    abort_req: bool,
    cancel_req: bool,
    allowed: bool,
    deal_notified: bool,
    skip_completion: bool,
    events: Vec<AimePayEvent>,
}

impl AimePay {
    /// The constructed manager (`0x1402386D0`): permit set, nothing running.
    pub fn new() -> Self {
        Self { permit: true, ..Self::default() }
    }

    /// `OperatingState::enter` (`0x14029CF30`, `0x140238A70`): a disabled module is state 3 and does nothing else.
    pub fn initialize(&mut self, cx: &mut Cx<'_>) {
        self.config = cx.config.aimepay.clone();
        self.life.begin();
        if !self.config.enable {
            self.life.state = StateWord::FAILED;
            return;
        }
        self.server.init();
        self.game_id = None;
        self.shutdown = false;
        self.reset();
        self.permit = true;
        self.suspended = false;
        self.timer_ms = cx.clock.tick_ms();
        self.life.state = StateWord::INITIALIZED;
        self.sub = 0;
    }

    /// `0x140238BD0`: a running task is cancelled, the session closes and the module is reset.
    pub fn terminate(&mut self, _cx: &mut Cx<'_>) {
        if self.life.state != StateWord::UNINITIALIZED {
            self.drop_runs();
            self.server.shutdown();
            self.reset();
            self.config.enable = false;
        }
        self.life.end();
    }

    /// `0x1402398F0(permit, suspended)`: `suspended` wins.
    pub fn set_permit(&mut self, permit: bool, suspended: bool) {
        self.permit = permit && !suspended;
        self.suspended = suspended;
    }

    /// `0x140238EA0(game id)` (`changeApp` step 14): the sub game id replaces the dongle's game id, and when both
    /// serials are known the server object takes the new ids.
    pub(crate) fn apm_change(&mut self, game_id: GameId) {
        self.game_id = Some(game_id.to_string());
        if self.board.is_some() && self.keychip.is_some() {
            self.server.set_ids(self.game_id.clone(), self.board.clone(), self.keychip.clone());
        }
    }

    /// `setShutdown(flag)` (`0x140238D90`): an enabled module past loading reloads everything; a disabled one only
    /// stores the flag (the `cfg.enabled` test `0x14021BF90`).
    pub fn set_shutdown(&mut self, flag: bool, cx: &mut Cx<'_>, links: &mut dyn Links) {
        self.shutdown = flag;
        if !self.config.enable || !self.life.state.is_done() {
            return;
        }
        if self.idle.as_ref().is_some_and(|call| !matches!(call, Call::LoadDealSummary { .. })) {
            self.drop_idle();
            self.skip_completion = true;
            if self.run.as_ref().is_some_and(|run| !run.running()) {
                self.cancel_req = true;
            }
            links.display().clear_lines();
        } else if self.run.as_ref().is_some_and(Run::running) && !self.cancel_req {
            self.abort_req = true;
            self.skip_completion = true;
        }
        self.reset();
        self.permit = true;
        self.suspended = false;
        self.timer_ms = cx.clock.tick_ms();
        self.life.state = StateWord::INITIALIZED;
        self.sub = 0;
        self.indicator(links);
    }

    /// `AimePayInfo::setSmsEnabled` (`0x1402611F0`): the e-money module copies its sales-reporting flag here and
    /// clears it at its own reset (`0x140230160`).
    pub fn set_sms_enabled(&mut self, enabled: bool) {
        self.sms_enabled = enabled;
    }

    /// `aimepay.enable`.
    pub fn is_enabled(&self) -> bool {
        self.config.enable
    }

    /// `allReady` term (`0x140239930`).
    pub fn is_ready(&self) -> bool {
        self.life.state.is_done()
    }

    /// The error cell (`0x140239CF0`).
    pub fn error_cell(&self) -> ErrorCell {
        self.life.error
    }

    /// The network test's `applicable` (`0x140296CF0`): the module state (`AIMEPAY+179020`) is not 0 and the
    /// `AimePayInfo` record holds a shop.
    pub fn network_test_applicable(&self) -> bool {
        self.life.state != StateWord::UNINITIALIZED && self.has_stored_location()
    }

    /// The network test's check (`0x140296D10`, gate `0x140239A20`): `None` while the gate is closed (not
    /// applicable or not online, and no error), else the module's error record (`AIMEPAY+964`), empty when good.
    pub fn network_test(&self) -> Option<ErrorCell> {
        (self.has_error() || (self.network_test_applicable() && self.online())).then_some(self.life.error)
    }

    /// The module state word.
    pub fn state(&self) -> StateWord {
        self.life.state
    }

    /// Events delivered to the listeners since the last call, in firing order.
    pub fn take_events(&mut self) -> Vec<AimePayEvent> {
        std::mem::take(&mut self.events)
    }

    fn push(&mut self, event: AimePayEvent) {
        let app = self.config.app;
        let installed = match event {
            AimePayEvent::CompleteTask { .. } => app.complete_task_listener,
            AimePayEvent::NotifyDealResult { .. } => app.notify_deal_result_listener,
            AimePayEvent::NeedAcceptation { .. } => app.need_acceptation_listener,
        };
        if installed {
            self.events.push(event);
        }
    }

    fn has_error(&self) -> bool {
        !self.life.error.is_empty()
    }

    fn set_error(&mut self, number: u16) {
        self.life.error.set(number, 0);
    }

    /// `reset` (`0x140239D60`).
    fn reset(&mut self) {
        self.deal_blocked = false;
        self.life.error.clear();
        self.net_err = false;
        self.activated = Location::default();
        self.history.clear();
        self.board = None;
        self.keychip = None;
        self.sa_client_id = 0;
        self.client_id_changed = false;
        self.net_ready = false;
        self.recheck = false;
        self.alive_ok = false;
        self.maintenance = false;
        self.need_recheck = false;
        self.wait_ready = 0;
        if self.idle.is_none() && self.run.is_none() {
            self.server.reset(true);
            self.items_loaded = false;
            self.allowed = false;
        }
        self.life.state = StateWord::UNINITIALIZED;
        self.sub = 0;
    }

    fn drop_idle(&mut self) {
        self.server.cancel();
        self.server.poll();
        self.idle = None;
    }

    fn drop_runs(&mut self) {
        self.run = None;
        self.drop_idle();
        self.abort_req = false;
        self.cancel_req = false;
    }

    fn runner_idle(&self) -> bool {
        self.idle.is_none() && self.run.is_none()
    }

    fn ids_valid(&self) -> bool {
        self.game_id.is_some() && self.board.is_some() && self.keychip.is_some()
    }

    fn has_stored_location(&self) -> bool {
        self.rec.as_ref().is_some_and(Info::has_location)
    }

    fn stored_matches_current(&self) -> bool {
        self.has_stored_location() && self.activated.same_shop(&self.server.location)
    }

    fn online(&self) -> bool {
        self.net_ready && self.alive_ok
    }

    /// `0x1402399D0`: connected to the server.
    fn alive(&self) -> bool {
        self.life.state != StateWord::UNINITIALIZED && self.rec.is_some() && self.online()
    }

    /// `dealAvailable` (`0x140239A80`).
    pub(crate) fn deal_available(&self, display_ready: bool) -> bool {
        self.life.state == StateWord::FAILED
            && self.stored_matches_current()
            && self.alive()
            && !self.maintenance
            && !self.has_error()
            && !self.suspended
            && display_ready
            && !self.deal_blocked
    }

    /// The service is usable for a task that is not a payment (`0x14023B550`).
    fn service_ok(&self, display_ready: bool) -> bool {
        self.life.state.is_done() && self.config.enable && !self.has_error() && display_ready
    }

    /// `0x14023B7F0` (`payment.apmgr.nv.dealblocked`): sales are reported and the ring of five deals holds no
    /// reported one, so a new deal would overwrite an unreported one.
    fn compute_deal_blocked(&self, emoney_registered: bool) -> bool {
        emoney_registered
            && self.sms_enabled
            && self.rec.as_ref().is_some_and(|rec| {
                rec.entry_count() == 5 && rec.entries().first().is_some_and(|entry| !entry.is_reported())
            })
    }

    /// The indicator of the e-payment screen (`0x14023B290`).
    fn indicator(&self, links: &mut dyn Links) {
        let available = self.deal_available(links.display_ready());
        let display = links.display();
        if self.shutdown {
            display.lane_set_state(Lane::AimePay, 4);
        } else if !self.life.state.is_done() || !self.has_stored_location() {
            display.lane_set_state(Lane::AimePay, 0);
        } else {
            display.lane_clear(Lane::AimePay);
            if available {
                display.lane_set_entry(Lane::AimePay, 1, "AimePay");
            }
            display.lane_set_state(Lane::AimePay, 1);
        }
    }

    fn now_tm(cx: &Cx<'_>) -> crate::time::Tm {
        tm_from_date_time(cx.clock.local_now())
    }

    /// `execute` (`0x140238CB0`), once per frame.
    pub fn update(&mut self, cx: &mut Cx<'_>, inputs: &Inputs, links: &mut dyn Links) {
        if !self.config.enable {
            return;
        }
        self.develop = inputs.develop;
        self.net_changed = inputs.net_ready != self.net_ready;
        self.net_ready = inputs.net_ready;
        self.client_id = inputs.client_id;
        if self.game_id.is_none() {
            self.game_id.clone_from(&inputs.game_id);
        }
        self.tick_scheduler(cx, links);
        self.tick_runner(cx, links);
        match self.life.state.0 {
            3 => {
                let blocked = self.compute_deal_blocked(inputs.emoney_registered);
                if blocked != self.deal_blocked {
                    self.deal_blocked = blocked;
                }
            }
            1 => self.state_loading(cx, inputs),
            _ => self.state_bootstrap(cx, inputs, links),
        }
        if self.life.state != StateWord::FAILED {
            self.wait_ready = if self.has_error() {
                2
            } else if !self.ids_valid_inputs(inputs) || inputs.client_id == 0 {
                0
            } else {
                1
            };
        }
        self.indicator(links);
    }

    fn ids_valid_inputs(&self, inputs: &Inputs) -> bool {
        self.game_id.is_some()
            && inputs.board.as_deref().and_then(Serial::parse).is_some()
            && inputs.keychip.as_deref().and_then(Serial::parse).is_some()
    }

    /// State 1 (`0x140239E70`).
    fn state_loading(&mut self, cx: &mut Cx<'_>, inputs: &Inputs) {
        match self.sub {
            0 => {
                if self.rec.is_none() {
                    match cx.nvram.load_record(Record::AimePayInfo, Info::zero_image) {
                        Ok(image) => self.rec = Some(Info::from_image(image)),
                        Err(status) => {
                            tracing::error!("aimepay: the info record could not be loaded");
                            self.fail_record(status);
                            return;
                        }
                    }
                }
                let Some(rec) = self.rec.as_mut() else { return };
                if rec.migrate() {
                    let image = rec.image().to_vec();
                    let status = cx.nvram.store_record(Record::AimePayInfo, &image);
                    if self.fail_record(status) {
                        return;
                    }
                }
                if let Some(rec) = self.rec.as_ref().filter(|rec| rec.has_location()) {
                    self.activated = rec.location();
                    self.history.clear();
                    for entry in rec.entries() {
                        self.history.push(entry.to_deal());
                    }
                }
                self.sub = 1;
            }
            1 => match self.load_env(cx) {
                Some(url) => {
                    self.env_url = url;
                    self.sub = 2;
                }
                None => {
                    tracing::error!("aimepay: the environment file could not be loaded");
                    self.set_error(ERROR_ENV);
                    self.life.state = StateWord::FAILED;
                }
            },
            _ => {
                if inputs.prereq && inputs.game_id.is_some() {
                    self.life.state = StateWord::READY;
                    self.sub = 3;
                }
            }
        }
    }

    fn fail_record(&mut self, status: RecordStatus) -> bool {
        match crate::nvram::NvramContext::status_error(status, self.develop) {
            Some((number, sub)) => {
                self.life.error.set(number, sub);
                self.life.state = StateWord::FAILED;
                true
            }
            None => false,
        }
    }

    /// `env.json` (`0x14028AE40`, `0x14028B4D0`): `aimepay.root_endpoint`.
    fn load_env(&self, cx: &Cx<'_>) -> Option<String> {
        use std::io::Read;
        let mut dir = self.config.resource_path.as_str().to_owned();
        if dir.is_empty() {
            dir.push('.');
        }
        if !dir.ends_with(['\\', '/']) {
            dir.push('\\');
        }
        let path = cx.backend.fs.map(&format!("{dir}env.json"));
        let mut bytes = Vec::new();
        std::fs::File::open(path).ok()?.take(4096).read_to_end(&mut bytes).ok()?;
        let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        let endpoint = value.get("aimepay")?.get("root_endpoint")?.as_str()?;
        (model::units(endpoint) <= MAX_ENDPOINT_UNITS).then(|| endpoint.to_owned())
    }

    /// State 0 and 2 (`0x14023A140`).
    fn state_bootstrap(&mut self, cx: &mut Cx<'_>, inputs: &Inputs, links: &mut dyn Links) {
        match self.sub {
            3 => {
                let board = inputs.board.as_deref().and_then(Serial::parse);
                let keychip = inputs.keychip.as_deref().and_then(Serial::parse);
                if self.net_ready && board.is_some() && keychip.is_some() && inputs.client_id != 0 && self.runner_idle()
                {
                    self.server.reset(true);
                    self.items_loaded = false;
                    self.need_recheck = false;
                    self.client_id_changed = inputs.client_id != self.sa_client_id;
                    self.sa_client_id = inputs.client_id;
                    self.board = board;
                    self.keychip = keychip;
                    self.server.set_ids(self.game_id.clone(), self.board.clone(), self.keychip.clone());
                    self.server.set_url(&self.env_url);
                    self.server.set_client_id(inputs.client_id);
                    if self.life.state != StateWord::UNINITIALIZED && self.rec.is_some() {
                        self.sub = 4;
                    } else {
                        self.life.state = StateWord::FAILED;
                    }
                }
            }
            4 => {
                if !self.runner_idle() {
                    return;
                }
                if self.life.state == StateWord::UNINITIALIZED || !self.has_stored_location() {
                    self.life.state = StateWord::FAILED;
                } else if self.net_ready && self.alive_ok {
                    if self.stored_matches_current() {
                        self.start_idle(cx, links);
                        self.sub = 5;
                    } else {
                        self.life.state = StateWord::FAILED;
                    }
                }
            }
            5 => {
                if self.runner_idle() {
                    self.start_idle_call(cx, links, Call::LoadDealSummary { days: 7 });
                    self.sub = 6;
                }
            }
            6 => {
                if self.runner_idle() {
                    self.sub = 7;
                }
            }
            7 => {
                self.recover_pending(cx);
                self.life.state = StateWord::FAILED;
            }
            _ => {}
        }
    }

    /// `0x14023A7C0`: a payment that was started and never logged is logged as failed.
    fn recover_pending(&mut self, cx: &mut Cx<'_>) {
        let Some(rec) = self.rec.as_mut() else { return };
        let Some(pending) = rec.pending() else { return };
        let deal = pending.to_deal();
        rec.append(&Entry::from_deal(&deal, pending.card_id));
        let image = rec.image().to_vec();
        let status = cx.nvram.store_record(Record::AimePayInfo, &image);
        self.history.push(deal);
        self.fail_record(status);
    }

    /// `startIdle` (`0x14023B110`).
    fn start_idle(&mut self, cx: &mut Cx<'_>, links: &mut dyn Links) {
        let call = if !self.server.location.is_complete() {
            Call::Activate
        } else if self.items_loaded {
            Call::CheckAlive
        } else {
            Call::LoadItemList
        };
        self.start_idle_call(cx, links, call);
    }

    fn start_idle_call(&mut self, cx: &mut Cx<'_>, links: &mut dyn Links, call: Call) {
        links.display().set_line(0, text::PLEASE_WAIT, false);
        links.display().set_line(1, "", false);
        self.timer_ms = cx.clock.tick_ms();
        self.server.issue(call.clone(), &mut *cx.backend.http);
        self.idle = Some(call);
    }

    fn can_start(&self) -> bool {
        self.net_ready
            && self.ids_valid()
            && self.client_id != 0
            && self.life.state != StateWord::UNINITIALIZED
            && self.rec.is_some()
            && self.runner_idle()
    }

    /// The module-level exchange finished: `maintenance`, `netErr` and `aliveOk` (`0x14023AFA0`, `0x14023B030`).
    fn exchange_done(&mut self) {
        self.maintenance = matches!(self.server.code(), Some("AMPEWG1010" | "AMPEWG1011"));
        if self.net_err && self.server.ok() {
            self.net_err = false;
        } else if !self.net_err && !self.server.ok() {
            self.net_err = true;
        }
        self.alive_ok = !self.net_err;
    }

    /// `tick1` (`0x14023AC20`): the scheduler.
    fn tick_scheduler(&mut self, cx: &mut Cx<'_>, links: &mut dyn Links) {
        if self.net_changed {
            if self.net_ready {
                self.recheck = true;
            } else {
                self.alive_ok = false;
                self.net_err = true;
                self.recheck = false;
            }
        }
        let was_running = !self.server.is_idle();
        self.server.poll();
        if was_running && let Some(call) = self.idle.take_if(|_| self.server.is_idle()) {
            if !matches!(call, Call::LoadDealSummary { .. }) {
                if self.skip_completion {
                    self.skip_completion = false;
                } else {
                    self.exchange_done();
                }
            }
            if matches!(call, Call::LoadItemList) && self.server.ok() && self.server.kind() == 0 {
                self.items_loaded = true;
            }
            links.display().clear_lines();
        } else if self.idle.is_some() && self.server.is_idle() {
            // The command could not be issued: it failed at its first poll.
            let call = self.idle.take();
            if !matches!(call, Some(Call::LoadDealSummary { .. })) {
                self.exchange_done();
            }
        }
        if !self.server.is_idle() {
            return;
        }
        if self.client_id_changed && self.client_id != 0 && self.can_start() {
            self.start_idle(cx, links);
            self.recheck = false;
            self.client_id_changed = false;
        } else if self.need_recheck {
            if self.permit && self.can_start() && self.stored_matches_current() {
                self.start_idle(cx, links);
            }
            self.need_recheck = false;
        } else if self.recheck {
            if self.can_start() {
                self.start_idle(cx, links);
                self.recheck = false;
                self.net_err = false;
            }
        } else {
            let period = if self.life.state == StateWord::READY { PERIOD_BOOT } else { PERIOD_STEADY };
            let elapsed = cx.clock.tick_ms().saturating_sub(self.timer_ms) / 1000;
            if elapsed >= period && self.permit && self.can_start() && !self.shutdown {
                self.start_idle(cx, links);
            }
        }
    }

    // The facade ---------------------------------------------------------------------------------------------

    /// `busy` (`0x14022E010` and `current != 0`).
    fn busy(&self, links: &mut dyn Links) -> bool {
        self.run.is_some() || links.display().is_mode_a()
    }

    /// Takes the runner over for a user task.
    fn begin_task(&mut self, run: Run) {
        if self.idle.is_some() {
            self.drop_idle();
        }
        self.cancel_req = false;
        self.abort_req = false;
        self.run = Some(run);
    }

    /// `activate` (`0x1402394A0`).
    pub fn activate(&mut self, cx: &mut Cx<'_>, links: &mut dyn Links) -> AmResult<()> {
        if !self.life.state.is_done() || !self.config.enable || self.has_error() || !links.display_ready() {
            return Err(AmError::code(AmError::FAILED));
        }
        if self.busy(links) {
            return Err(AmError::code(AmError::BUSY));
        }
        links.display().enter_mode_a();
        let first = if self.wait_ready != 0 { text::FETCHING_STORE } else { text::PLEASE_WAIT };
        links.display().set_line(0, first, false);
        links.display().set_line(1, "", false);
        let start = cx.clock.tick_ms();
        self.begin_task(Run::activation(start));
        Ok(())
    }

    /// `deactivate` (`0x140239620`): no server contact; the record is wiped at once.
    pub fn deactivate(&mut self, cx: &mut Cx<'_>, links: &mut dyn Links) -> AmResult<()> {
        if !self.life.state.is_done() || !self.config.enable || self.has_error() {
            return Err(AmError::code(AmError::FAILED));
        }
        if self.busy(links) {
            return Err(AmError::code(AmError::BUSY));
        }
        links.display().enter_mode_a();
        self.begin_task(Run::deactivation());
        let status = cx.nvram.store_record(Record::AimePayInfo, &Info::zero_image());
        if status != crate::nvram::RecordStatus::Ok {
            self.set_error(if status == crate::nvram::RecordStatus::ContextFailed { 4105 } else { 84 });
        } else {
            self.rec = Some(Info::from_image(Info::zero_image()));
            self.history.clear();
            self.activated = Location::default();
            self.server.reset(false);
        }
        let line = if self.has_error() { text::STORE_DELETE_FAILED } else { text::STORE_DELETED };
        links.display().set_line(0, line, false);
        let until = cx.clock.tick_ms() + text::RESULT_WAIT * 1000;
        if let Some(run) = self.run.as_mut() {
            run.show_result(until, !self.life.error.is_empty());
        }
        Ok(())
    }

    /// `queryUserSetting` (`0x140238F40`).
    pub fn query_user_setting(&mut self, aime_id: u32) -> AmResult<()> {
        if !self.life.state.is_done()
            || !self.stored_matches_current()
            || !self.online()
            || self.maintenance
            || self.has_error()
            || self.suspended
        {
            return Err(AmError::code(AmError::FAILED));
        }
        if self.run.is_some() {
            return Err(AmError::code(AmError::BUSY));
        }
        self.begin_task(Run::query_user(aime_id));
        Ok(())
    }

    /// The coin item id of the configured coin amount (`payment.apmgr.cfg.coin_item_select`).
    fn coin_item(&self, coin_amount: u32) -> Option<String> {
        let index = match coin_amount {
            100 => 0,
            120 => 1,
            _ => return None,
        };
        self.config.coin_item_id.iter().map(|id| id.as_str()).filter(|id| !id.is_empty()).nth(index).map(str::to_owned)
    }

    /// `coinPay` after the shared gate `0x140253810` (`0x140239070`).
    pub fn coin_pay(
        &mut self,
        cx: &mut Cx<'_>,
        links: &mut dyn Links,
        aime_id: u32,
        access_code: &AccessCode,
        player: u32,
        coin: u32,
    ) -> AmResult<()> {
        if links.test_mode() {
            return Err(AmError::with_message(2, "test mode now"));
        }
        if links.ignore_coin() {
            return Err(AmError::with_message(2, "ignore coin in"));
        }
        let addable = links.addable(player);
        if addable == 0 {
            return Err(AmError::with_message(2, "can not add more coins"));
        }
        let multiplier = cx.config.credit.rule.coin_chute_multiplier[0];
        if u64::from(addable) < u64::from(coin) * u64::from(multiplier) {
            return Err(AmError::with_message(1, "too many coins to exceed max credit"));
        }
        if !self.deal_available(links.display_ready()) {
            return Err(AmError::code(AmError::FAILED));
        }
        if self.busy(links) {
            return Err(AmError::code(AmError::BUSY));
        }
        let Some(item) = self.coin_item(cx.config.credit.rule.coin_amount) else {
            return Err(AmError::with_message(AmError::FAILED, "there are no coin item"));
        };
        let card_id = links.card_id(aime_id).unwrap_or_default();
        let mut run = Run::pay(Kind::Coin { player, coin }, aime_id, card_id, item.clone(), coin);
        self.start_pay(links, &mut run, access_code, &item, coin, true);
        self.begin_task(run);
        Ok(())
    }

    /// `amountPay` (`0x140239290`).
    pub fn amount_pay(
        &mut self,
        links: &mut dyn Links,
        aime_id: u32,
        access_code: &AccessCode,
        item_id: &str,
        item_count: u32,
        option_number: i64,
    ) -> AmResult<()> {
        if !self.deal_available(links.display_ready()) {
            return Err(AmError::code(AmError::FAILED));
        }
        if self.busy(links) {
            return Err(AmError::code(AmError::BUSY));
        }
        if !is_item_id(item_id) {
            return Err(AmError::code(AmError::FAILED));
        }
        let card_id = links.card_id(aime_id).unwrap_or_default();
        let mut run = Run::pay(Kind::Item { option_number }, aime_id, card_id, item_id.to_owned(), item_count);
        self.start_pay(links, &mut run, access_code, item_id, item_count, false);
        self.begin_task(run);
        Ok(())
    }

    /// The common launch of a payment: display busy, receipt reset, a fresh deal.
    fn start_pay(
        &mut self,
        links: &mut dyn Links,
        _run: &mut Run,
        access_code: &AccessCode,
        item_id: &str,
        count: u32,
        coin: bool,
    ) {
        links.display().enter_mode_a();
        links.display().set_line(0, text::IN_PROGRESS, false);
        links.display().set_line(1, "", false);
        self.server.receipt_id = 0;
        self.server.amount = model::NO_AMOUNT;
        self.deal_notified = false;
        self.deal = Deal {
            item_count: count,
            item_id: item_id.to_owned(),
            access_code: *access_code,
            is_coin: coin,
            amount: self
                .server
                .item_price(item_id)
                .and_then(|price| price.checked_mul(count))
                .unwrap_or(model::NO_AMOUNT),
            ..Deal::default()
        };
    }

    /// `cancel` (`0x140239840`).
    pub fn cancel(&mut self) {
        self.cancel_waiting();
    }

    /// `confirmWithParam` (`0x140239780`): the PIN of a waiting payment.
    pub fn confirm_with_param(&mut self, links: &mut dyn Links, param: &[u16]) {
        let pin = crate::util::wide_to_string(param);
        if let Some(run) = self.run.as_mut()
            && run.accept_pin(pin)
        {
            links.display().set_line(0, text::IN_PROGRESS, false);
        }
    }

    /// `confirmAccept` (`0x1402397F0`): the store information of a waiting Activation.
    pub fn confirm_accept(&mut self, cx: &mut Cx<'_>, links: &mut dyn Links) {
        if self.run.as_ref().is_some_and(Run::accepting) {
            self.accept_location(cx, links);
        }
    }

    /// Everything the facade reads (see [`port`]).
    pub(crate) fn history(&self) -> &DealRing {
        &self.history
    }

    pub(crate) fn activated(&self) -> &Location {
        &self.activated
    }

    pub(crate) fn current_location(&self) -> Option<&Location> {
        self.life.state.is_done().then_some(&self.server.location)
    }

    pub(crate) fn summary_count(&self) -> usize {
        if self.life.state.is_done() { self.server.summary().len() } else { 0 }
    }

    pub(crate) fn summary_item(&self, index: usize) -> Option<model::Summary> {
        if !self.life.state.is_done() {
            return None;
        }
        self.server.summary().nth(index).copied()
    }

    pub(crate) fn user_setting(&self) -> Option<model::UserSetting> {
        if self.life.state.is_done() { self.server.user_setting() } else { None }
    }

    pub(crate) fn pending_of(deal: &Deal, card_id: [u8; 8], time: Compact) -> Pending {
        Pending {
            access_code: deal.access_code.0,
            card_id,
            item_id: deal.item_id.parse().unwrap_or(0),
            time,
            amount: deal.amount,
            item_count: deal.item_count,
            is_coin: deal.is_coin,
        }
    }
}
