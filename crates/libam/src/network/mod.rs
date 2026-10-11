//! The network manager (`am::network::Manager`, `spec/libam/network.md` part M), the network test controller
//! (part T), the LAN-install / net-delivery composite (parts L and D).
//!
//! # Cross-module methods
//!
//! * [`Network::update`] (`0x140246E20`, M.3), then [`networktest::NetworkTest::update`] (`0x140248300`) last.
//! * [`Network::is_ready`] / [`Network::error_cell`] (`0x140247030`, `0x140247050`).
//! * [`laninstall::NetInstall::download_ui`] (`0x1402442F0`): the download-UI flag the main-flow terminate hands
//!   to the master hand-off (`core.md` A.6).
//! * [`laninstall::NetInstall::update`] (`0x140244130`): `active_net_ready` is the allnet or network manager
//!   (network.md L.13).

mod deliver_beacon;
mod deliver_dli;
mod deliver_fetcher;
mod deliver_icf;
mod deliver_image;
mod delivery_port;
mod engine;
mod net_manager;
mod port;

pub mod laninstall;
pub mod netdeliver;
pub mod networktest;

use crate::config::NetworkConfig;

use crate::module::{Cx, ErrorCell};
use crate::nvram::{NvramContext, Record};

pub use delivery_port::{
    DELIVERY_SOURCE_CAPACITY, DownloadProgress, ImageInfo, ImageKind, LanInstallHandle, LanInstallMode, LanInstallPort,
    NetDeliveryHandle, NetDeliveryPort, TimeInfo,
};
pub use engine::{AmNetwork, PropertyBuffer, TraceState};
pub use laninstall::NetInstall;
pub use netdeliver::NetDeliver;
pub use networktest::NetworkTest;
pub use port::{
    NetworkAppConfig, NetworkHandle, NetworkPort, NetworkProcessConfig, NetworkProperty, NetworkTestEvent, TestItem,
    TestStatus,
};

use net_manager::{
    DnsChecker, FAILED, GatewayChecker, IDLE, Io, IpChecker, LocationRouterChecker, OK, Property, PropertySetter,
    RUNNING, finished,
};

/// An item of the top-level sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Run {
    PropertySetter,
    Tester,
}

/// Where the setup tester (`am::network::Tester`) is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TesterPhase {
    Idle,
    Ip,
    Gateway,
    Dns,
    Location,
}

/// `am::network::Manager` (`ctx+145840`).
#[derive(Debug)]
pub struct Network {
    config: NetworkConfig,
    /// `M+692`: 0 not set up, 1 initial pass pending, 2 done.
    state: u8,
    /// `M+696`: the stage (M.5.3).
    stage: u8,
    allnet_enabled: bool,
    /// `ctx[26180]`, the platform's sub-storage byte: a failed record operation maps to error 87 instead of 84
    /// (`0x14025AEC0`).
    sub_storage: bool,
    ctx_held: bool,
    /// `M+88`: the latched error.
    error: ErrorCell,
    /// `M+388`: the latest sub-command error.
    current_error: ErrorCell,
    check_started: bool,
    setup_done: bool,
    stored: Option<Property>,
    property_setter: PropertySetter,
    ip: IpChecker,
    gateway: GatewayChecker,
    dns: DnsChecker,
    location: LocationRouterChecker,
    queue: Vec<Run>,
    queue_index: usize,
    tester: TesterPhase,
    /// The process-wide amNetwork / amDns / altr engine. ALL.Net's netinfo borrows it for the same request slot.
    engine: AmNetwork,
}

impl Default for Network {
    fn default() -> Self {
        Self {
            config: NetworkConfig::default(),
            state: 0,
            stage: 0,
            allnet_enabled: false,
            sub_storage: false,
            ctx_held: false,
            error: ErrorCell::EMPTY,
            current_error: ErrorCell::EMPTY,
            check_started: false,
            setup_done: false,
            stored: None,
            property_setter: PropertySetter::default(),
            ip: IpChecker::default(),
            gateway: GatewayChecker::default(),
            dns: DnsChecker::default(),
            location: LocationRouterChecker::default(),
            queue: Vec::new(),
            queue_index: 0,
            tester: TesterPhase::Idle,
            engine: AmNetwork::default(),
        }
    }
}

fn io<'a>(engine: &'a mut AmNetwork, backend: &'a mut backend::Backend, now_s: u64) -> Io<'a> {
    Io { net: engine, backend, now_s }
}

impl Network {
    /// The constructed manager.
    pub fn new() -> Self {
        Self::default()
    }

    /// `OperatingState::enter` (`setup`, M.6.1): takes `AppConfig.network`, the ALL.Net flag and the platform's
    /// sub-storage byte (`ctx[26180]`).
    pub fn initialize(&mut self, cx: &mut Cx<'_>, sub_storage: bool) {
        self.config = cx.config.network;
        self.allnet_enabled = cx.config.allnet_auth.enable;
        self.sub_storage = sub_storage;
        if !self.config.enable {
            self.stage = 7;
            self.state = 2;
            return;
        }
        if !self.ctx_held && self.engine.init(cx.backend, !self.allnet_enabled).is_ok() {
            self.ctx_held = true;
        }
        self.stage = 1;
        self.state = 1;
    }

    /// `0x140246E20`, once per Operating frame. `keychip_done` is the keychip state in {2, 3} and
    /// `keychip_error` its error cell (M.6.3).
    pub fn update(&mut self, cx: &mut Cx<'_>, keychip_done: bool, keychip_error: ErrorCell) {
        if !self.config.enable {
            return;
        }
        let ready = self.keychip_gate(keychip_done, keychip_error);
        if self.ctx_held {
            self.engine.poll(cx.backend, cx.clock);
            self.step_queue(cx);
            self.stage_machine();
            self.aggregate_errors();
        }
        if ready && self.state != 2 {
            self.finish_setup(cx);
        }
    }

    fn keychip_gate(&mut self, done: bool, error: ErrorCell) -> bool {
        if !done {
            return false;
        }
        if error.is_empty() {
            return true;
        }
        self.error = error;
        self.stage = 9;
        self.setup_done = true;
        self.state = 2;
        false
    }

    /// `0x140247430`.
    fn finish_setup(&mut self, cx: &mut Cx<'_>) {
        self.setup_done = true;
        if !self.ctx_held {
            self.error = net_manager::cell(4105);
            self.stage = 9;
        } else {
            let load = cx.nvram.load_record(Record::NetworkEth0Info, || default_record().to_vec());
            match load {
                Ok(image) => {
                    self.stored = Some(Property::from_words(record_words(&image)));
                    self.ip.keychip_net = self.engine.keychip_address(&mut *cx.backend.registry);
                    self.restart(cx);
                    self.stage = 2;
                }
                Err(status) => {
                    let (number, sub_number) = NvramContext::status_error(status, self.sub_storage).unwrap_or((84, 0));
                    self.error = net_manager::cell(0);
                    self.error.set(number, sub_number);
                    self.stage = 9;
                }
            }
        }
        self.state = 2;
    }

    /// `0x140247B10`: rebuilds and starts the top-level sequence.
    fn restart(&mut self, cx: &mut Cx<'_>) {
        self.stop(cx);
        let need_ps = self.property_setter.state != OK;
        self.queue.clear();
        if need_ps {
            self.queue.push(Run::PropertySetter);
        }
        if !self.allnet_enabled {
            self.queue.push(Run::Tester);
        } else if !need_ps {
            return;
        }
        self.check_started = true;
        self.queue_index = 0;
        self.start_current(cx);
    }

    fn start_current(&mut self, cx: &mut Cx<'_>) {
        match self.queue.get(self.queue_index).copied() {
            Some(Run::PropertySetter) => self.start_property_setter(cx),
            Some(Run::Tester) => {
                self.ip.start();
                self.tester = TesterPhase::Ip;
            }
            None => {}
        }
    }

    /// `0x140293E60` and `needApply` (`0x140294160`).
    fn start_property_setter(&mut self, cx: &mut Cx<'_>) {
        let wanted = self.config.property.map(|p| Property {
            dhcp: p.dhcp,
            ip: p.address,
            mask: p.subnet_mask,
            gateway: p.gateway,
            dns1: p.primary_dns,
            dns2: p.secondary_dns,
        });
        let apply = match (wanted, self.stored) {
            (Some(wanted), Some(stored)) => {
                let differs = if wanted.dhcp { !stored.dhcp } else { stored.dhcp || stored != wanted };
                (self.property_setter.previously_failed() || differs).then(|| wanted.buffer())
            }
            _ => None,
        };
        self.property_setter.start(apply);
        let (Some(_), Some(wanted)) = (apply, wanted) else { return };
        // `property.save`: the record is written before the apply is attempted. The config field at `+9`
        // is "do not persist".
        if !self.config.dhcp_if_unset {
            let mut image = default_record();
            for (index, word) in wanted.to_words().iter().enumerate() {
                image[8 + 4 * index..12 + 4 * index].copy_from_slice(&word.to_le_bytes());
            }
            let status = cx.nvram.store_record(Record::NetworkEth0Info, &image);
            if let Some((number, _)) = NvramContext::status_error(status, self.sub_storage) {
                self.property_setter.fail(net_manager::cell(number));
            } else {
                self.stored = Some(wanted);
            }
        }
    }

    /// `0x140247BA0`: when the sequencer runs, the current command is cancelled (the property setter ends
    /// failed without an error; the setup tester cancels the checker that is current), then everything is cleared.
    fn stop(&mut self, cx: &mut Cx<'_>) {
        match self.queue.get(self.queue_index) {
            Some(Run::PropertySetter) => self.property_setter.cancel(),
            Some(Run::Tester) => {
                let now_s = cx.clock.tick_ms() / 1000;
                match self.tester {
                    TesterPhase::Gateway => {
                        self.gateway.cancel(&mut io(&mut self.engine, cx.backend, now_s));
                    }
                    TesterPhase::Dns => self.dns.cancel(&mut io(&mut self.engine, cx.backend, now_s)),
                    TesterPhase::Location => self.location.cancel(&mut io(&mut self.engine, cx.backend, now_s)),
                    TesterPhase::Ip | TesterPhase::Idle => {}
                }
            }
            None => {}
        }
        self.queue.clear();
        self.queue_index = 0;
        self.error = ErrorCell::EMPTY;
        self.current_error = ErrorCell::EMPTY;
        self.ip.reset();
        self.gateway.reset();
        self.dns.reset();
        self.location.reset();
        self.tester = TesterPhase::Idle;
        self.check_started = false;
    }

    fn step_queue(&mut self, cx: &mut Cx<'_>) {
        let Some(current) = self.queue.get(self.queue_index).copied() else { return };
        let mut ended = None;
        match current {
            Run::PropertySetter => {
                let now_s = cx.clock.tick_ms() / 1000;
                self.property_setter.step(&mut io(&mut self.engine, cx.backend, now_s));
                if finished(self.property_setter.state) {
                    ended = Some(self.property_setter.state);
                }
            }
            Run::Tester => ended = self.step_tester(cx),
        }
        if let Some(state) = ended {
            if state == OK && self.queue_index + 1 < self.queue.len() {
                self.queue_index += 1;
                self.start_current(cx);
            } else {
                self.queue_index = self.queue.len();
            }
        }
    }

    /// `0x140246940`: steps the setup tester; `Some(state)` when it finished.
    fn step_tester(&mut self, cx: &mut Cx<'_>) -> Option<u8> {
        let now_s = cx.clock.tick_ms() / 1000;
        match self.tester {
            TesterPhase::Idle => return None,
            TesterPhase::Ip => {
                self.ip.step(self.config.location_router_directly, &mut io(&mut self.engine, cx.backend, now_s));
                if self.ip.state == FAILED {
                    return Some(self.end_tester(FAILED));
                }
                if self.ip.state == OK {
                    if self.config.use_gateway {
                        self.gateway.start(self.ip.record, now_s);
                        self.tester = TesterPhase::Gateway;
                    } else {
                        return Some(self.end_tester(OK));
                    }
                }
            }
            TesterPhase::Gateway => {
                self.gateway.step(&mut io(&mut self.engine, cx.backend, now_s));
                if self.gateway.state == FAILED {
                    return Some(self.end_tester(FAILED));
                }
                if self.gateway.state == OK {
                    if self.gateway.gateway() != 0 && self.config.use_location_router && !finished(self.dns.state) {
                        self.dns.start(&mut io(&mut self.engine, cx.backend, now_s));
                        self.tester = TesterPhase::Dns;
                    } else {
                        return Some(self.end_tester(OK));
                    }
                }
            }
            TesterPhase::Dns => {
                self.dns.step(&mut io(&mut self.engine, cx.backend, now_s));
                if self.dns.state == FAILED {
                    return Some(self.end_tester(FAILED));
                }
                if self.dns.state == OK {
                    self.location.start(self.dns.resolved, &mut io(&mut self.engine, cx.backend, now_s));
                    self.tester = TesterPhase::Location;
                }
            }
            TesterPhase::Location => {
                self.location.step(&mut io(&mut self.engine, cx.backend, now_s));
                if finished(self.location.state) {
                    let state = self.location.state;
                    return Some(self.end_tester(state));
                }
            }
        }
        None
    }

    fn end_tester(&mut self, state: u8) -> u8 {
        self.tester = TesterPhase::Idle;
        state
    }

    /// `0x1402478F0`.
    fn stage_machine(&mut self) {
        let (done, failed) = match self.stage {
            2 => (finished(self.property_setter.state), !self.property_setter.error.is_empty()),
            3 => (finished(self.ip.state), !self.ip.error.is_empty()),
            4 => (finished(self.gateway.state), !self.gateway.error.is_empty()),
            5 => (finished(self.dns.state), !self.dns.error.is_empty()),
            6 => (finished(self.location.state), !self.location.error.is_empty()),
            _ => return,
        };
        if !done {
            return;
        }
        self.stage = if failed {
            7
        } else {
            match self.stage {
                2 if self.allnet_enabled => 8,
                2 => 3,
                3 if self.config.use_gateway => 4,
                4 if self.config.use_location_router && self.gateway.gateway() != 0 => 5,
                5 => 6,
                _ => 8,
            }
        };
    }

    /// `0x1402474F0` / `0x1402475B0`.
    fn aggregate_errors(&mut self) {
        if self.error.is_empty() {
            let found = [self.property_setter.error, self.ip.error, self.gateway.error, self.dns.error]
                .into_iter()
                .find(|e| !e.is_empty());
            match found {
                Some(error) => self.publish(error),
                None => self.current_error = ErrorCell::EMPTY,
            }
        }
    }

    fn publish(&mut self, error: ErrorCell) {
        let number = error.get().map_or(0, |(number, _)| number);
        if number != self.current_error.get().map_or(0, |(number, _)| number) {
            // The error tables are external data; without them the reset type is 4 (T.10).
            self.current_error = error;
            let sub_error = !self.property_setter.error.is_empty() || !self.ip.error.is_empty();
            if number < 1000 || (!self.config.support_standalone && sub_error) {
                self.error = self.current_error;
            }
            if !self.error.is_empty() {
                self.stage = 9;
            }
        }
    }

    /// `Interface::finalize`: `0x140246DC0`.
    pub fn terminate(&mut self, cx: &mut Cx<'_>) {
        if self.state != 0 {
            self.stop(cx);
            self.property_setter.reset();
            if self.ctx_held {
                self.engine.exit();
                self.ctx_held = false;
            }
            self.setup_done = false;
            self.config = NetworkConfig::default();
            self.stored = None;
            self.state = 0;
            self.stage = 0;
        }
    }

    /// Module restart (`reinit`, `0x140246EB0`): the stop of `0x140247BA0` (running sub-commands cancelled, queue,
    /// error cells and sub-task records cleared), then the next frame re-runs the keychip gate and `finish_setup`
    /// (stage 1), or a disabled module is ready at once (stage 7).
    pub fn restart_module(&mut self, cx: &mut Cx<'_>) {
        self.stop(cx);
        if self.config.enable {
            self.stage = 1;
            self.state = 1;
        } else {
            self.stage = 7;
            self.state = 2;
        }
    }

    /// Slot 1 `retest` (`0x140246F10`), what `startTest` calls.
    pub fn retest(&mut self, cx: &mut Cx<'_>) {
        self.check_started = false;
        if self.config.enable && self.state == 2 {
            let top = !self.error.is_empty() || !self.current_error.is_empty();
            let sub = !self.property_setter.error.is_empty()
                || !self.ip.error.is_empty()
                || !self.gateway.error.is_empty()
                || !self.dns.error.is_empty()
                || !self.location.error.is_empty();
            if !top || sub {
                self.restart(cx);
                self.stage = 3;
            }
        }
    }

    /// `allReady` term (`0x140247030`).
    pub fn is_ready(&self) -> bool {
        self.state == 2
    }

    /// `0x140247080`: the property setter runs (command state 1 to 3), whatever the manager state.
    pub fn property_setter_running(&self) -> bool {
        matches!(self.property_setter.state, 1..=3)
    }

    /// The engine ALL.Net's netinfo shares with this manager (one request slot, as in the original).
    pub(crate) fn engine_mut(&mut self) -> &mut AmNetwork {
        &mut self.engine
    }

    /// The error cell (`0x140247050`, `M+88`).
    pub fn error_cell(&self) -> ErrorCell {
        self.error
    }

    /// `network.enable`.
    pub fn enabled(&self) -> bool {
        self.config.enable
    }

    /// Slot 2: the latest sub-command error (`M+388`).
    pub fn current_error(&self) -> ErrorCell {
        self.current_error
    }

    /// Slot 3: the property of the IP command (`M+692 == 2`).
    pub fn property(&self) -> Option<NetworkProperty> {
        if self.state != 2 {
            return None;
        }
        self.ip.property().map(|p| NetworkProperty {
            dhcp: p.dhcp,
            address: p.ip,
            subnet_mask: p.mask,
            gateway: p.gateway,
            primary_dns: p.dns1,
            secondary_dns: p.dns2,
        })
    }

    /// Slot 4.
    pub fn is_lan_available(&self) -> bool {
        self.state == 2 && self.ip.lan_ready() && net_manager::lan_modes(&self.config)
    }

    /// `0x1402471C0`: the location-router command is done (or failing).
    pub fn location_done(&self) -> bool {
        self.state == 2
            && (finished(self.location.state)
                || ((RUNNING..=3).contains(&self.location.state) && !self.location.error.is_empty()))
    }

    /// Slot 5.
    pub fn is_location_lan_available(&self) -> bool {
        if !(self.location_done() && self.location.hops != 0) {
            return false;
        }
        let net10 = self.ip.property().is_some_and(|p| p.ip.to_le_bytes()[0] == 10);
        !net10 || matches!(self.config.location_router_directly, 1 | 2)
    }

    /// Slot 7: the check sequence started.
    pub fn check_started(&self) -> bool {
        self.state == 2 && self.check_started
    }

    /// Slot 8: the initial setup pass is done.
    pub fn setup_done(&self) -> bool {
        self.state == 2 && self.setup_done
    }

    /// `0x140247060`: the stage with 2 reported as 3.
    pub fn phase(&self) -> u8 {
        if self.stage == 2 { 3 } else { self.stage }
    }

    /// `0x140247190`: state 2 and no gateway address.
    pub fn gateway_missing(&self) -> bool {
        self.state == 2 && self.gateway.gateway() == 0
    }

    /// `0x140247240`: the location-router hop count when that command is done.
    pub fn hops(&self) -> u32 {
        if self.location_done() { self.location.hops } else { 0 }
    }

    /// Whether the IP command has run (`IDLE` otherwise).
    pub fn ip_idle(&self) -> bool {
        self.ip.state == IDLE
    }
}

/// The default record image: dhcp 1, 10.0.0.1/24 (`0x1402C7E50`).
fn default_record() -> [u8; 512] {
    let mut image = [0u8; 512];
    let words = Property {
        dhcp: true,
        ip: net_manager::addr(10, 0, 0, 1),
        mask: net_manager::addr(255, 255, 255, 0),
        ..Property::default()
    }
    .to_words();
    for (index, word) in words.iter().enumerate() {
        image[8 + 4 * index..12 + 4 * index].copy_from_slice(&word.to_le_bytes());
    }
    image
}

fn record_words(image: &[u8]) -> [u32; 6] {
    let mut words = [0u32; 6];
    for (index, word) in words.iter_mut().enumerate() {
        let at = 8 + 4 * index;
        *word = image.get(at..at + 4).map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    }
    words
}
