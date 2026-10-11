//! The e-money module (`am::emoney::Manager`, `spec/libam/payment.md` parts thinca, PayEmMgr, PayEmCmd, DL and
//! events): the brand terminals and the payment state machine.
//!
//! # Cross-module methods
//!
//! * [`EMoney::update`] (`0x14022EFE0`, M.3): after the Aime module. [`EMoney::update_with`] takes the values the
//!   original gets from other modules ([`EMoneyInputs`]).
//! * [`EMoney::set_permit`] (`0x14022FE00`, from `0x140254C40` and the Operating step): `permit` while the
//!   sequence is not playing; `suspended` while the error manager holds an error (`suspended` wins).
//! * [`EMoney::is_enabled`]: the `emoney.enable` flag the display module's e-payment screen reads.
//! * [`EMoney::take_display_ops`]: the screen-A and lane operations of the frame; the caller applies them to the
//!   display module with [`DisplayOp::apply`].

mod aime_reader;
pub(crate) mod card;
pub mod deal_log;
mod env;
mod flows;
mod link;
mod port;
mod service;
mod sms;
mod terminal;
mod thinca;

use libam_api::am_config::EMoneyConfig;
use libam_api::emoney::{EMoneyBrandInfo, EMoneyEvent, LibamBrand};
use libam_api::time::Tm;

use crate::display::{Display, Lane};
use crate::module::{Cx, ErrorCell, Lifecycle, StateWord};
use crate::nvram::{NvramContext, Record};
use crate::util::tm_from_date_time;
use backend::ThincaSdk;
use backend::thinca::{CODE_BUSY, Callbacks, ThincaInit};

use deal_log::{DealRecord, DealRing, EmoneyInfo};
use flows::Flow;

pub use card::{CardPoll, CardRead, Lease, StoreCard};
pub use link::{CoinGate, CreditLink, NoCredit};
pub use port::EMoneyHandle;

/// What the Manager frame needs from other modules (`0x14029D590` builds them in the original).
#[derive(Debug, Clone)]
pub struct EMoneyInputs {
    /// `a2`: the modules the Manager frame depends on are ready (dongle, credit, storage, platform, Aime, display).
    pub modules_ready: bool,
    /// `a3`: the 11-character terminal serial (`None` when the serial block is invalid).
    pub serial: Option<String>,
    /// `a7`: the 11-character derived string of the dongle's keychip serial block (`ctx+26218`; `None` when the
    /// block is invalid). The terminal info copies the block (`+0x4AA`, `0x140270430`).
    pub keychip: Option<String>,
    /// The 8-character platform model text sent to the activation request (`owner+324`).
    pub model: Option<String>,
    /// `a6`: the active network manager reports the LAN or the location LAN as available (slots 4 and 5).
    pub online: bool,
    /// The Aime reader lease is available (the lease token byte `+16`).
    pub reader_available: bool,
    /// Coin counters per player (`credit+1520`).
    pub coin_counters: [u32; 8],
    /// Coin prices per player (`credit+1248`).
    pub coin_prices: [u32; 8],
    /// The display module is ready (`0x14022DF70`).
    pub display_ready: bool,
    /// A display session (screen A) is active (`0x14022E010`).
    pub display_busy: bool,
}

impl Default for EMoneyInputs {
    fn default() -> Self {
        Self {
            modules_ready: true,
            online: true,
            serial: None,
            keychip: None,
            model: None,
            reader_available: true,
            coin_counters: [0; 8],
            coin_prices: [0; 8],
            display_ready: true,
            display_busy: false,
        }
    }
}

/// An operation on the display module the e-money flows ask for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisplayOp {
    /// `0x14022DC50`: write a screen-A line.
    SetLine {
        /// Line 0 or 1.
        line: usize,
        /// Text.
        text: String,
        /// Scroll flag.
        scroll: bool,
    },
    /// `0x14022DB50`.
    ClearLines,
    /// `0x14022DB00`.
    EnterModeA,
    /// `0x14022DB20`.
    LeaveModeA,
    /// `0x140231730`: the e-money lane state (0..4).
    LaneState(u32),
    /// The brand menu of the e-money lane (entry id, name).
    LaneEntries(Vec<(u32, String)>),
}

impl DisplayOp {
    /// Applies the operation to the display module.
    pub fn apply(self, display: &mut Display) {
        match self {
            Self::SetLine { line, text, scroll } => display.set_line(line, &text, scroll),
            Self::ClearLines => display.clear_lines(),
            Self::EnterModeA => display.enter_mode_a(),
            Self::LeaveModeA => display.leave_mode_a(),
            Self::LaneState(state) => display.lane_set_state(Lane::EMoney, state),
            Self::LaneEntries(entries) => {
                display.lane_clear(Lane::EMoney);
                for (id, name) in entries {
                    display.lane_set_entry(Lane::EMoney, id, &name);
                }
            }
        }
    }
}

/// Entry flag bits (8.1).
const FLAG_CONFIG: u8 = 0x01;
const FLAG_KNOWN: u8 = 0x02;
const FLAG_AUTH: u8 = 0x04;
const FLAG_USABLE: u8 = 0x08;
const FLAG_SITE: u8 = 0x10;
const FLAG_READY: u8 = 0x1F;

#[derive(Debug, Clone, Default)]
struct BrandEntry {
    id: u32,
    name: String,
    icon: String,
    balance: bool,
    flags: u8,
    priority: u8,
}

/// The brand list (8.1): nine entries and the available list.
#[derive(Debug, Clone, Default)]
struct BrandList {
    entries: [BrandEntry; 9],
    available: Vec<usize>,
}

impl BrandList {
    fn reset(&mut self) {
        *self = Self::default();
    }

    /// `0x1402664A0`: the two refresh passes.
    fn refresh(&mut self, config: &EMoneyConfig, sdk: &mut dyn ThincaSdk) {
        for (index, entry) in self.entries.iter_mut().enumerate() {
            let id = deal_log::libam_brand_of_entry(index).0;
            entry.id = id;
            entry.name.clear();
            entry.icon.clear();
            entry.balance = id != 3 && id != 0;
            if deal_log::sdk_brand_of_entry(index).is_some() {
                entry.flags &= !FLAG_KNOWN;
                let enabled = config.brand_enabled.get(id as usize).copied().unwrap_or(true);
                if enabled {
                    entry.flags |= FLAG_CONFIG;
                } else {
                    entry.flags &= !FLAG_CONFIG;
                }
            }
        }
        for (index, entry) in self.entries.iter_mut().enumerate() {
            let Some(brand) = deal_log::sdk_brand_of_entry(index) else { continue };
            if let (Some(name), Some(icon)) = (sdk.brand_name(brand), sdk.brand_icon(brand)) {
                entry.flags |= FLAG_KNOWN;
                if name.chars().count() <= 64 {
                    entry.name = name;
                }
                if icon.chars().count() <= 259 {
                    entry.icon = icon;
                }
            }
        }
        self.rebuild();
    }

    fn set_flag(&mut self, entry: usize, flag: u8, value: bool) {
        if let Some(entry) = self.entries.get_mut(entry) {
            if value {
                entry.flags |= flag;
            } else {
                entry.flags &= !flag;
            }
        }
        self.rebuild();
    }

    /// `0x140266AD0`: the available list.
    fn rebuild(&mut self) {
        let mut list: Vec<usize> = Vec::new();
        for index in 0..9 {
            if self.entries[index].flags & FLAG_READY != FLAG_READY {
                continue;
            }
            let entry = &self.entries[index];
            let same = list
                .iter()
                .position(|&known| self.entries[known].id == entry.id && entry.priority < self.entries[known].priority);
            match same {
                Some(slot) => list[slot] = index,
                None => list.push(index),
            }
        }
        for i in 0..list.len() {
            for j in i + 1..list.len() {
                if self.entries[list[j]].priority < self.entries[list[i]].priority {
                    list.swap(i, j);
                }
            }
        }
        self.available = list;
    }

    /// `0x1402669A0`: the entry of a facade brand.
    fn entry_of_brand(&self, brand: LibamBrand) -> usize {
        match brand.0 {
            1 => {
                if self.entries[7].flags & FLAG_READY == FLAG_READY {
                    7
                } else {
                    1
                }
            }
            2..=6 => brand.0 as usize,
            7 => 8,
            _ => 0,
        }
    }
}

/// The state the sales-management-site service keeps for the Manager (`M+29316`, `M+29424`, `M+29426`, the alive
/// object bytes). Filled by the SMS service flows (`PayEmSvc`).
#[derive(Debug, Clone, Default)]
pub struct SiteState {
    /// Service status text (`"0"`, `"10"`, `"11"`, `"12"`, `"80"`, `"90"`).
    pub status: String,
    /// `cashAvailability` (`M+29424`).
    pub cash_available: bool,
    /// `M+29426`.
    pub usable: bool,
    /// The alive object is up (bytes `+881`, `+528`, `+720`).
    pub alive: bool,
}

impl SiteState {
    /// `0x14027EE20`.
    pub fn status_code(&self) -> u32 {
        match self.status.as_str() {
            "0" => 1,
            "10" => 2,
            "11" => 3,
            "12" => 4,
            "80" => 5,
            "90" => 6,
            _ => 0,
        }
    }
}

/// The listener-state record (4.7) and the hub flags.
#[derive(Debug, Clone, Copy, Default)]
struct Hub {
    first_error: u16,
    abort: bool,
    cancellable: bool,
    operation_allowed: bool,
    blocked: bool,
    sales_pending: bool,
    hold_fired: bool,
}

/// `am::emoney::Manager` (`ctx+218624`): state word, error cell, configuration copy.
#[derive(Debug)]
pub struct EMoney {
    life: Lifecycle,
    config: EMoneyConfig,
    /// The record flag byte of the platform (`M+90956`, `ctx+26180`): error 87 instead of 84.
    develop: bool,
    permit: bool,
    suspended: bool,
    /// `M+32`.
    mode: u8,
    /// `M+36`.
    state: u8,
    /// `M+40`.
    recovery: u8,
    hub: Hub,
    info: Option<EmoneyInfo>,
    ring: DealRing,
    brands: BrandList,
    callbacks: Callbacks,
    sdk_open: bool,
    flow: Option<Flow>,
    site: SiteState,
    baselines: [u32; 8],
    saved_deal_info: bool,
    events: Vec<EMoneyEvent>,
    display_ops: Vec<DisplayOp>,
    now: Tm,
    tick_ms: u64,
    inputs: EMoneyInputs,
    serial_sent: bool,
    /// `M+90920`.
    stop_requested: bool,
    /// The 4-letter game id of `apm_change` (terminal info `+0x49E`).
    title: Option<String>,
    /// The terminal info (PayEmSvc 9.2).
    terminal: terminal::Terminal,
    /// `env.json` as Manager state 2 loaded it.
    env: Option<env::Env>,
    /// The sales-management-site service object (`M+28280`).
    service: sms::Service,
    /// The alive object (`M+88096`).
    alive_run: service::Alive,
    /// The ThincaPayment version text of the status report.
    module_version: String,
    /// The resource directory of the SDK (4.1).
    resource_dir: String,
    /// The sales-reporting flag the last CheckSetting stored (`service+1145`, code 91 listed). The main flow copies
    /// it to `AimePayInfo +1112` (`0x1402611F0`); the reset clears it (`0x140230160`).
    sms_enabled: bool,
    /// The store-card reader (PayEmSvc 6.4) over the Aime unit `emoney.aime_unit` names.
    card_reader: aime_reader::AimeStoreCardReader,
}

impl Default for EMoney {
    fn default() -> Self {
        Self::new()
    }
}

impl EMoney {
    /// The constructed manager.
    pub fn new() -> Self {
        Self {
            life: Lifecycle::default(),
            config: EMoneyConfig::default(),
            develop: false,
            permit: false,
            suspended: false,
            mode: 0,
            state: 0,
            recovery: 0,
            hub: Hub::default(),
            info: None,
            ring: DealRing::default(),
            brands: BrandList::default(),
            callbacks: Callbacks::default(),
            sdk_open: false,
            flow: None,
            site: SiteState::default(),
            baselines: [0; 8],
            saved_deal_info: true,
            events: Vec::new(),
            display_ops: Vec::new(),
            now: Tm::default(),
            tick_ms: 0,
            inputs: EMoneyInputs::default(),
            serial_sent: false,
            stop_requested: false,
            title: None,
            terminal: terminal::Terminal::default(),
            env: None,
            service: sms::Service::default(),
            alive_run: service::Alive::default(),
            module_version: String::new(),
            resource_dir: String::new(),
            sms_enabled: false,
            card_reader: aime_reader::AimeStoreCardReader::new(),
        }
    }

    /// `OperatingState::enter` (`0x14022EC80` / `0x14022F1E0`): takes `AppConfig.emoney` and `aime.unit`, state 1;
    /// the reset `0x140230160` (argument 0) runs here.
    pub fn initialize(&mut self, cx: &mut Cx<'_>, develop: bool) {
        self.config = cx.config.emoney.clone();
        self.develop = develop;
        self.life.begin();
        self.flow = None;
        self.reset(false);
    }

    /// `0x140230160(keep_gate)`: clears the first error, the gate latches (`operation_allowed` only when
    /// `keep_gate` is false), the lists, the deal log, the alive object, the service object and the terminal info
    /// (id, serial, directory, certificate; the 4-letter title block and the "ran once" byte of SendCounter only when
    /// `keep_gate` is false) and the site view; mode 1. The set-up then runs again from state 0. The abort and
    /// cancel flags of the hub and the running flow are not touched: the runner outlives the reset.
    fn reset(&mut self, keep_gate: bool) {
        let operation = self.hub.operation_allowed && keep_gate;
        self.hub = Hub {
            abort: self.hub.abort,
            cancellable: self.hub.cancellable,
            hold_fired: self.hub.hold_fired,
            operation_allowed: operation,
            ..Hub::default()
        };
        self.brands.reset();
        self.ring.clear();
        self.baselines = [0; 8];
        self.saved_deal_info = true;
        self.site = SiteState::default();
        self.alive_run.reset(!keep_gate);
        self.service = sms::Service::default();
        self.sms_enabled = false;
        self.terminal = terminal::Terminal::default();
        if !keep_gate {
            self.title = None;
        }
        self.mode = 1;
        self.state = 0;
        self.recovery = 0;
        self.permit = true;
        self.suspended = false;
        self.serial_sent = false;
        // `0x140231730` with the mode at 1.
        self.publish_lane();
    }

    /// `AimePayInfo +1112` as the e-money module last left it: on after a CheckSetting that listed the code 91
    /// (`0x14027B7B0`), off after a reset (`0x140230160`). The main flow copies it to the Aime Pay module.
    pub fn sms_enabled(&self) -> bool {
        self.sms_enabled
    }

    /// `0x14022EFE0`, once per Operating frame, with the default inputs and no credit module.
    pub fn update(&mut self, cx: &mut Cx<'_>) {
        self.update_with(cx, &EMoneyInputs::default(), &mut NoCredit);
    }

    /// `0x14022EFE0` with the values the original gets from other modules; `credit` takes the coins of a paid coin
    /// deal.
    pub fn update_with(&mut self, cx: &mut Cx<'_>, inputs: &EMoneyInputs, credit: &mut dyn CreditLink) {
        self.life.finish_setup();
        if !self.config.enable {
            return;
        }
        self.inputs = inputs.clone();
        self.tick_ms = cx.clock.tick_ms();
        self.card_reader.tick(self.tick_ms);
        self.now = tm_from_date_time(cx.clock.local_now());
        self.poll(cx, credit);
        self.service_tick(cx);
        self.sync_site();
        self.coin_accounting(cx);
        if self.mode != 3 && self.mode != 4 {
            self.step_state_machine(cx);
        }
        self.frame_tail(cx);
    }

    /// `0x14022EE50`: the abort flag is set and the runner drained (up to 3750 passes of 1 ms), then the SDK is
    /// disposed.
    pub fn terminate(&mut self, cx: &mut Cx<'_>) {
        if self.mode != 0 && self.config.enable {
            self.hub.abort = true;
            let mut passes = 0;
            while self.flow.as_ref().is_some_and(|flow| flow.is_running()) && passes < 3750 {
                self.poll(cx, &mut NoCredit);
                self.frame_tail(cx);
                cx.clock.sleep_ms(1);
                passes += 1;
            }
            self.hub.abort = false;
            self.card_reader.release();
            if self.sdk_open {
                cx.backend.thinca.dispose();
                self.sdk_open = false;
            }
            self.brands.reset();
            self.mode = 0;
            self.state = 0;
            self.recovery = 0;
        }
        self.life.end();
    }

    /// `0x14022F140(stop)`, the module notification of the refresh sweep (`0x1402544A0`, payment.md PayEmMgr):
    /// `stop` is the sequence's test flag; it is `M+90920` and suppresses the recovery until [`Self::set_permit`]
    /// lifts it. While the mode is 2..4 a running flow is told to stop and the module starts over (reset with the
    /// gate kept; the module is not ready until the state machine is back at mode 2). The service commands the
    /// original stops here run inside the alive step in libam, so none is pending between frames.
    pub fn notify_module(&mut self, stop: bool) {
        self.stop_requested = stop;
        if !self.config.enable || !(2..=4).contains(&self.mode) {
            return;
        }
        if self.flow.as_ref().is_some_and(Flow::is_running) {
            self.hub.abort = true;
        }
        self.life.begin();
        self.reset(true);
    }

    /// `0x14022FE00(permit, suspended)`: stores `reportSuspend` (`M+90922`) and `reportEnable` (`M+90921`,
    /// `suspended` wins), then publishes the display lane.
    pub fn set_permit(&mut self, permit: bool, suspended: bool) {
        self.permit = permit && !suspended;
        self.suspended = suspended;
        self.publish_lane();
    }

    /// `0x140230120` (step 8): enabled, the mode is non-zero and the runner has finished its last operation.
    pub fn apm_idle(&self) -> bool {
        self.config.enable && self.mode != 0 && self.flow.is_none()
    }

    /// `0x14022F1E0` (step 14): the 4-letter game id for the deal-info identity (9.4). Effective only while enabled,
    /// ready, the mode is non-zero and no deal info is pending.
    pub fn apm_change(&mut self, game_id: libam_api::backup::GameId) {
        if self.config.enable && self.life.state == StateWord::READY && self.mode != 0 && self.saved_deal_info {
            self.title = Some(game_id.to_string());
        }
    }

    /// `emoney.enable`.
    pub fn is_enabled(&self) -> bool {
        self.config.enable
    }

    /// `emoney.aime_unit`: the index of the Aime reader unit the store-card reader runs on.
    pub fn aime_unit(&self) -> usize {
        usize::try_from(self.config.aime_unit).unwrap_or(usize::MAX)
    }

    /// Whether the store-card reader still needs the Aime unit handed over.
    pub(crate) fn store_reader_wants_unit(&self) -> bool {
        self.card_reader.wants_unit()
    }

    /// Hands the store-card reader the Aime unit the flows run on.
    pub(crate) fn attach_store_reader(&mut self, unit: crate::aime::device::UnitCommands) {
        self.card_reader.attach(unit);
    }

    /// The reader lease is free: the `a6` input of the module.
    pub(crate) fn store_reader_free(&self) -> bool {
        self.card_reader.lease().free
    }

    /// The display module's state for the gates (`0x14022DF70`, `0x14022E010`).
    pub fn set_display_state(&mut self, ready: bool, busy: bool) {
        self.inputs.display_ready = ready;
        self.inputs.display_busy = busy;
    }

    /// `allReady` term (`0x14022FE40`).
    pub fn is_ready(&self) -> bool {
        self.life.state.is_done()
    }

    /// The `EmoneyInfo` record is `registered` (`0x14025FB30`, the "E registered" term of DL.9 and PayEmMgr): the raw
    /// server brand mask is non-zero. `false` while the record is not loaded.
    pub fn is_registered(&self) -> bool {
        self.info.as_ref().is_some_and(EmoneyInfo::is_registered)
    }

    /// The error cell (`0x14022FE60`).
    pub fn error_cell(&self) -> ErrorCell {
        self.life.error
    }

    /// The network test's `applicable` (`0x140296AC0`): the module state (`EMONEY+32`) is not 0 and the
    /// `EmoneyInfo` record is set.
    pub fn network_test_applicable(&self) -> bool {
        self.life.state != StateWord::UNINITIALIZED && self.is_registered()
    }

    /// The network test's check (`0x140296AF0`): `None` while the gate is closed (the alive object's three bytes
    /// are not all set and its error stamp is empty), else the error stamp (`EMONEY+88980`), empty when good.
    pub fn network_test(&self) -> Option<ErrorCell> {
        let stamp = self.alive_run.stamp;
        (self.alive_run.stamp != 0 || self.site.alive).then(|| {
            let mut record = ErrorCell::EMPTY;
            if stamp != 0 {
                record.set(stamp, 0);
            }
            record
        })
    }

    /// Takes the events delivered to the listeners since the last call.
    pub fn take_events(&mut self) -> Vec<EMoneyEvent> {
        std::mem::take(&mut self.events)
    }

    /// Takes the display operations of the frame.
    pub fn take_display_ops(&mut self) -> Vec<DisplayOp> {
        std::mem::take(&mut self.display_ops)
    }

    /// `0x140266800`: the site's ordered brand list (Thinca brand numbers) enables entries with priority = position
    /// (server brand 1 maps to entry 7 and also entry 1).
    pub fn set_site_brands(&mut self, order: &[u32]) {
        for entry in &mut self.brands.entries {
            entry.flags &= !FLAG_SITE;
        }
        let mut extra = 0u8;
        for (position, &brand) in order.iter().enumerate() {
            let priority = position as u8 + extra;
            let mut enable = |entry: usize, priority: u8| {
                self.brands.entries[entry].flags |= FLAG_SITE;
                self.brands.entries[entry].priority = priority;
            };
            match brand {
                1 => {
                    enable(7, priority);
                    extra += 1;
                    enable(1, priority + 1);
                }
                2 | 3 | 5 | 6 | 8 | 9 => enable(deal_log::entry_of_sdk(brand), priority),
                _ => {}
            }
        }
        self.brands.rebuild();
    }

    /// A copy of the brand record for a facade brand (`0x140266890`).
    pub(crate) fn brand_record(&self, brand: LibamBrand) -> EMoneyBrandInfo {
        let entry = &self.brands.entries[self.brands.entry_of_brand(brand)];
        EMoneyBrandInfo::new(entry.id, &entry.name, &entry.icon, u8::from(entry.balance))
    }

    pub(crate) fn brand_count(&self) -> usize {
        self.brands.available.len()
    }

    pub(crate) fn brand_at(&self, index: usize) -> Option<LibamBrand> {
        self.brands.available.get(index).map(|&entry| LibamBrand(self.brands.entries[entry].id))
    }

    pub(crate) fn deal_log_count(&self) -> usize {
        self.ring.count()
    }

    pub(crate) fn deal_log_item(&self, index: usize) -> Option<libam_api::emoney::EMoneyDealInfo> {
        self.ring.item(index)
    }

    /// The terminal id and serial strings (`0x14022FFA0` / `0x14022FFF0`): only with mode != 0 and a valid record.
    pub(crate) fn terminal_strings(&self) -> Option<(String, String)> {
        (self.mode != 0 && self.info.is_some()).then(|| (self.terminal.id.clone(), self.terminal.serial.clone()))
    }

    /// `0x140231FB0`: enabled, mode 3 or 4, the EmoneyInfo record registered (`0x14025FB30`), no first error, the
    /// report not suspended (`M+90922`), no result pending (`+303`), no cabinet information mismatch, the Aime
    /// reader lease and the display ready, and the sales queue not blocked (`0x140232200`).
    fn base_gate(&self) -> bool {
        self.config.enable
            && matches!(self.mode, 3 | 4)
            && self.info.as_ref().is_some_and(EmoneyInfo::is_registered)
            && self.hub.first_error == 0
            && !self.suspended
            && !self.hub.blocked
            && !self.backend_mismatch()
            && self.inputs.reader_available
            && self.inputs.display_ready
            && !self.sales_queue_blocked()
    }

    /// `0x140232200`: ring 0 of the EmoneyInfo record is full (`0x14025F9B0` count == `0x14025F7A0` capacity) and
    /// its oldest entry is not reported yet (`0x14025F880` == 0); new deals wait until the site took the reports.
    fn sales_queue_blocked(&self) -> bool {
        self.info.as_ref().is_some_and(|info| {
            let records = info.ring_oldest_first(0);
            info.ring_count(0) == info.ring_capacity()
                && records.first().is_none_or(|oldest| oldest.flags & deal_log::FLAG_REPORTED == 0)
        })
    }

    /// `0x14022FEB0`.
    pub(crate) fn deal_available(&self) -> bool {
        self.base_gate()
            && self.site.status_code() == 2
            && self.site.alive
            && self.site.usable
            && !self.brands.available.is_empty()
    }

    /// `0x1402322C0` for task 4: `deal_available`, no foreign owner, no flow, no display session.
    pub(crate) fn can_operate(&self) -> bool {
        self.deal_available() && self.flow.is_none() && !self.inputs.display_busy
    }

    /// `0x14027DDA0`.
    pub(crate) fn alive(&self) -> bool {
        self.site.alive
    }

    /// `0x1402320F0`: the gate of the operations that need no deal.
    fn operation_gate(&self, task_one: bool) -> bool {
        self.config.enable
            && (2..=4).contains(&self.mode)
            && self.recovery != 4
            && (self.hub.first_error == 0 || self.hub.first_error == 25)
            && !self.hub.blocked
            && (!task_one || self.inputs.display_ready)
    }

    fn flow_busy(&self) -> bool {
        self.flow.is_some() || self.inputs.display_busy
    }

    /// The first-error slot: first error wins (`0x140266E60`). The code also goes to the module's error cell,
    /// which the aggregator reads (PayEmSvc 12.2: the hub cell is the error the Manager reports).
    fn set_error(&mut self, code: u16) {
        if self.hub.first_error == 0 {
            self.hub.first_error = code;
            self.life.error.set(code, 0);
        }
    }

    /// Stores the `EmoneyInfo` record; a failure becomes the first error.
    fn save_info(&mut self, cx: &Cx<'_>) -> bool {
        let Some(info) = &self.info else { return false };
        let status = cx.nvram.store_record(Record::EmoneyInfo, info.image());
        match NvramContext::status_error(status, self.develop) {
            None => true,
            Some((code, _)) => {
                self.set_error(code);
                false
            }
        }
    }

    fn step_state_machine(&mut self, cx: &mut Cx<'_>) {
        match self.state {
            0 if self.terminal.dir.is_empty() => match terminal::resolve_dir(cx.backend) {
                Some(dir) => self.terminal.dir = dir,
                None => {
                    tracing::info!("emoney: the terminal directory cannot be resolved");
                    self.set_error(4105);
                }
            },
            0 => {
                let size = Record::EmoneyInfo.size();
                match cx.nvram.load_record(Record::EmoneyInfo, || EmoneyInfo::blank_image(size)) {
                    Ok(image) => {
                        let info = EmoneyInfo::from_image(image);
                        let mask = info.auth_mask();
                        for entry in 0..9 {
                            if let Some(brand) = deal_log::sdk_brand_of_entry(entry) {
                                self.brands.entries[entry].flags |=
                                    if mask & (1 << brand) != 0 { FLAG_AUTH } else { 0 };
                            }
                        }
                        self.ring.clear();
                        for record in info.ring_oldest_first(0) {
                            if (1..=8).contains(&record.brand) {
                                self.ring.append(record.to_deal());
                            }
                        }
                        if !self.terminal.ensure_dir(&*cx.backend.fs) {
                            self.set_error(84);
                            return;
                        }
                        self.info = Some(info);
                        self.state = 1;
                    }
                    Err(status) => {
                        if let Some((code, _)) = NvramContext::status_error(status, self.develop) {
                            self.set_error(code);
                        }
                    }
                }
            }
            // State 1 (`0x140231A40`) writes the HKLM values the SDK reads (the Aime reader adapter path, the CA
            // file and the client time-outs 20000 / 5000 / 60000 ms). The spec does not decode the value names
            // (PayEmMgr open item), so libam has nothing to write; the state only advances.
            1 => self.state = 2,
            2 => self.load_environment(cx),
            3 => {
                if self.inputs.modules_ready {
                    let rescan = self.info.as_ref().is_some_and(|info| !info.is_blank());
                    let password = self.env.as_ref().map(|env| env.password.clone()).unwrap_or_default();
                    let (serial, keychip) = (self.inputs.serial.as_deref(), self.inputs.keychip.as_deref());
                    if !self.terminal.set(serial, keychip, &password, rescan, &*cx.backend.fs) {
                        tracing::error!("emoney: terminal info needs the board serial");
                        self.set_error(4105);
                        return;
                    }
                    self.baselines = self.inputs.coin_counters;
                    self.state = 5;
                }
            }
            5 => self.open_sdk(cx),
            4 => self.state_four(cx),
            6 => {
                if self.flow.is_none() && self.site.alive {
                    self.state = 7;
                }
            }
            _ => {
                self.recovery_step(cx);
            }
        }
    }

    /// State 2 (`0x140231DE0`, PayEmSvc 4.1 / 4.2): the resource directory and `env.json`; a failure raises 4104.
    fn load_environment(&mut self, cx: &mut Cx<'_>) {
        let sega_am_log = cx.backend.system.env_var("SEGA_AM_LOG");
        let fs = &*cx.backend.fs;
        let read = |path: &str| std::fs::read(fs.map(path)).ok();
        let dir = env::resource_directory(sega_am_log.as_deref(), self.config.resource_path.as_str(), read);
        let parsed = read(&format!("{dir}env.json"))
            .ok_or("env.json cannot be read")
            .and_then(|bytes| env::parse_env(&bytes).map_err(|_| "env.json is not valid"));
        match parsed {
            Ok(env) => {
                self.resource_dir = dir;
                self.env = Some(env);
                self.state = 3;
            }
            Err(reason) => {
                tracing::error!(reason, "emoney: environment load failed");
                self.set_error(4104);
            }
        }
    }

    /// State 5 (7.7): context init, brand refresh.
    fn open_sdk(&mut self, cx: &mut Cx<'_>) {
        let config = &self.config;
        let log_level = match config.log_level {
            0 => 5,
            1 => 3,
            2 => 2,
            3 => 1,
            _ => 0,
        };
        let log_base = String::new();
        let init = ThincaInit {
            resource_dir: self.resource_dir.clone(),
            icas_log: Some((log_level, format!("{log_base}icasclient.log"))),
            thinca_log: Some((log_level, format!("{log_base}thincapayment.log"))),
            adapter: format!("TfpsAimeRwAdapter,\\\\.\\COM{}:{}:{:02}", 0, 38400, 0),
        };
        match cx.backend.thinca.open(&init, &self.callbacks) {
            Ok(version) => {
                self.sdk_open = true;
                self.module_version = format!(
                    "{}.{}.{}.{}",
                    version >> 24,
                    (version >> 16) & 0xFF,
                    (version >> 8) & 0xFF,
                    version & 0xFF
                );
                self.restore_report_block();
                self.brands.refresh(&self.config, cx.backend.thinca.as_mut());
                self.mode = 2;
                self.state = 4;
                self.publish_lane();
            }
            Err(error) => {
                self.sdk_open = false;
                tracing::error!(%error, "emoney: opening the sdk failed");
                cx.backend.thinca.dispose();
                self.set_error(4105);
            }
        }
    }

    /// State 4: terminal info to the SDK, then alive or blank.
    fn state_four(&mut self, cx: &mut Cx<'_>) {
        if !self.serial_sent {
            if let Some(serial) = self.inputs.serial.clone() {
                match cx.backend.thinca.set_terminal_serial(&serial) {
                    0 => self.serial_sent = true,
                    CODE_BUSY => return,
                    _ => {
                        self.set_error(4105);
                        return;
                    }
                }
            } else {
                self.serial_sent = true;
            }
        }
        if self.flow.is_some() || !self.inputs.reader_available || !self.inputs.online || self.inputs.display_busy {
            return;
        }
        self.hub.blocked = false;
        self.hub.sales_pending = false;
        let blank = self.info.as_ref().is_none_or(EmoneyInfo::is_blank);
        if blank {
            if self.config.b1 {
                self.set_error(25);
            } else {
                self.mode = 4;
            }
        } else if self.site.alive {
            self.state = 7;
        } else {
            self.start_alive(cx, false);
            self.state = 6;
        }
    }

    /// The pending-deal recovery phases (state 7, `0x140230A80`).
    fn recovery_step(&mut self, cx: &mut Cx<'_>) {
        match self.recovery {
            0 => {
                // Phase 0 (`0x140231E60`): no pending deal number, or the newest log record is that deal.
                let resolved = self.info.as_ref().is_none_or(|info| {
                    info.marker()
                        .is_none_or(|marker| marker.deal_number.is_empty() || info.newest_is(&marker.deal_number))
                });
                self.recovery = if resolved { 1 } else { 2 };
            }
            1 => {
                if let Some(info) = &mut self.info {
                    info.clear_marker();
                }
                self.save_info(cx);
                self.recovery = 5;
            }
            2 => {
                // `0x14026BA30`: the pending record becomes the VerifyUnconfirmedDeal command.
                self.recovery = 3;
            }
            3 => {
                // `0x1402566E0` / `0x1402566C0` / `0x1402564D0`: the runner is idle, the command is queued and started.
                if self.flow.is_none() {
                    let marker = self.info.as_ref().and_then(|info| info.marker());
                    if let Some(marker) = marker {
                        self.start_verify(marker.deal_number, marker.block);
                        self.recovery = 4;
                    } else {
                        self.recovery = 5;
                    }
                }
            }
            4 => {
                if self.flow.is_none() {
                    self.recovery = 5;
                }
            }
            _ => self.mode = 3,
        }
    }

    /// The tail of every frame (`0x140230F10`, `0x1402302A0` tail).
    fn frame_tail(&mut self, cx: &mut Cx<'_>) {
        self.run_flow(cx);
        if self.hub.first_error != 0 && self.mode < 3 {
            self.mode = 4;
        }
        if self.hub.first_error != 0 && self.mode == 3 && self.recovery != 5 {
            self.mode = 4;
        }
        if self.recovery == 5 && self.state == 7 {
            self.mode = 3;
        }
        if matches!(self.mode, 3 | 4) {
            self.publish_lane();
        }
    }

    /// `0x1402321D0`: the stored record's TASMS flag against the environment's `use_proxy`.
    fn backend_mismatch(&self) -> bool {
        let use_proxy = self.env.as_ref().is_some_and(|env| env.use_proxy);
        self.info.as_ref().is_some_and(|info| info.backend_mismatch(use_proxy))
    }

    /// `0x140231730`: the state of the e-money lane of screen B. Stopped is 4; a module that is not in mode 2..4 or
    /// has no registered terminal is 0; a record whose TASMS flag disagrees with the environment is 2 (cabinet
    /// information mismatch); a blocked hub is 3; otherwise the lane is rebuilt (the brand menu when a deal is
    /// available, empty otherwise) and its state is 1.
    fn publish_lane(&mut self) {
        let state = if self.stop_requested {
            4
        } else if !(2..=4).contains(&self.mode) || !self.is_registered() {
            0
        } else if self.backend_mismatch() {
            2
        } else if self.hub.blocked {
            3
        } else {
            let entries = if self.deal_available() {
                self.brands
                    .available
                    .iter()
                    .map(|&entry| (self.brands.entries[entry].id, self.brands.entries[entry].name.clone()))
                    .collect()
            } else {
                Vec::new()
            };
            self.display_ops.push(DisplayOp::LaneEntries(entries));
            1
        };
        self.display_ops.push(DisplayOp::LaneState(state));
    }

    /// The cash coin sales log (DL.8, `0x140230BC0`): per player with a price, the coins inserted since the last
    /// frame go to ring 1 of the `EmoneyInfo` record (feeds the SMS sales report).
    fn coin_accounting(&mut self, cx: &mut Cx<'_>) {
        if !(2..=4).contains(&self.mode) || self.info.is_none() {
            return;
        }
        let status = self.site.status_code();
        for player in 0..8 {
            let price = self.inputs.coin_prices[player];
            if price == 0 {
                continue;
            }
            let counter = self.inputs.coin_counters[player];
            let previous = self.baselines[player];
            let allowed = self.mode != 0
                && self.info.as_ref().is_some_and(|info| info.auth_mask() != 0)
                && (status == 0 || self.site.cash_available);
            let delta = if previous <= counter { counter - previous } else { counter };
            self.baselines[player] = counter;
            if !allowed || delta == 0 {
                continue;
            }
            let time = self.record_time();
            let Some(info) = self.info.as_mut() else { continue };
            let open = info.ring_oldest_first(1).iter().rev().position(|record| {
                record.flags & deal_log::FLAG_REPORTED == 0
                    && record.price == price
                    && (status != 0 || record.flags & deal_log::FLAG_OPEN != 0)
            });
            let ok = match open {
                Some(index) => {
                    let mut record = info.ring_get(1, index).unwrap_or_default();
                    record.count += delta;
                    record.amount = (record.count * price) as i32;
                    record.time = time;
                    info.ring_update(1, index, &record)
                }
                None => {
                    let record = DealRecord {
                        flags: if status == 0 { deal_log::FLAG_OPEN } else { 0 },
                        time,
                        price,
                        count: delta,
                        amount: (delta * price) as i32,
                        ..DealRecord::default()
                    };
                    info.ring_append(1, &record);
                    true
                }
            };
            if ok {
                self.save_info(cx);
            }
        }
    }

    fn record_time(&self) -> [u16; 6] {
        let time = &self.now;
        [
            (time.year + 1900) as u16,
            (time.mon + 1) as u16,
            time.mday as u16,
            time.hour as u16,
            time.min as u16,
            time.sec as u16,
        ]
    }
}
