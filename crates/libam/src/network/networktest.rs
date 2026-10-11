//! The network test controller (`spec/libam/network.md` part T): nine checkers run in order, one step per
//! frame, reporting through [`NetworkTestEvent`]s that the port hands to the daemon.

use libam_api::error::FatalError;
use libam_api::network::{NetworkTestEvent, TestItem, TestStatus};

use super::Network;
use crate::core::Core;
use crate::module::{Cx, ErrorCell};
use crate::util::tm_from_date_time;

const ITEMS: usize = 9;

/// What the controller reads of the modules this crate owns elsewhere (ALL.Net manager, aime, emoney,
/// aimepay); each field is the value of the original's getter named in network.md T.8. The default is "nothing
/// reported yet": ALL.Net stages pending, modules not done, emoney/aimepay not applicable.
#[derive(Debug, Clone, Copy)]
pub struct PeerView {
    /// `ALLNET.state` in {2, 3}.
    pub allnet_ready: bool,
    /// The ALL.Net current error record (`ALLNET+1212`).
    pub allnet_error: ErrorCell,
    /// The ALL.Net manager-level error (`ALLNET+24`).
    pub allnet_manager_error: ErrorCell,
    /// `ALLNET+1204`: restart accepted.
    pub allnet_started: bool,
    /// `ALLNET+1205`: initial pass done.
    pub allnet_setup_done: bool,
    /// Network-state block fields `f0`, `f1`, `f2`, `f4` (hop count, -1 pending), `f5`.
    pub ns: [i32; 5],
    /// `f7 == 1 && f8 == 4`: authenticated.
    pub authenticated: bool,
    /// Aime: `Some(error)` once the gate passed (empty record = good).
    pub aime: Option<ErrorCell>,
    /// Emoney is registered with a stored record.
    pub emoney_applicable: bool,
    /// Emoney gate result.
    pub emoney: Option<ErrorCell>,
    /// Aimepay is registered with a stored record.
    pub aimepay_applicable: bool,
    /// Aimepay gate result.
    pub aimepay: Option<ErrorCell>,
}

impl Default for PeerView {
    fn default() -> Self {
        Self {
            allnet_ready: false,
            allnet_error: ErrorCell::EMPTY,
            allnet_manager_error: ErrorCell::EMPTY,
            allnet_started: false,
            allnet_setup_done: false,
            ns: [0, 0, 0, -1, 0],
            authenticated: false,
            aime: None,
            emoney_applicable: false,
            emoney: None,
            aimepay_applicable: false,
            aimepay: None,
        }
    }
}

impl PeerView {
    /// The values of the original's getters, read from the modules as they stand now (network.md T.8, T.11.2,
    /// T.11.3). With the ALL.Net manager's own record empty, the record the items read is the network manager's
    /// latest sub-command error (`0x14023DA40`).
    pub fn of(core: &Core) -> Self {
        let allnet = &core.allnet;
        let allnet_error = match allnet.test_error() {
            error if error.is_empty() && allnet.is_ready() => core.network.current_error(),
            error => error,
        };
        let [f0, f1, f2, f4, f5] = allnet.test_stages();
        let started = allnet.test_started();
        let emoney_applicable = core.emoney.network_test_applicable();
        let aimepay_applicable = core.aimepay.network_test_applicable();
        Self {
            allnet_ready: allnet.is_ready(),
            allnet_error,
            allnet_manager_error: allnet.error_cell(),
            allnet_started: started,
            allnet_setup_done: allnet.test_setup_done(),
            ns: [f0, f1, f2, f4, f5],
            authenticated: allnet.test_authenticated(),
            aime: core.aime.network_test(),
            emoney_applicable,
            emoney: if emoney_applicable { core.emoney.network_test() } else { None },
            aimepay_applicable,
            aimepay: if aimepay_applicable { core.aimepay.network_test() } else { None },
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Item {
    enabled: bool,
    allnet_mode: bool,
    prereq: Option<usize>,
    state: u32,
    result: u32,
    hops_text: Option<u32>,
    error: ErrorCell,
    power_on: bool,
}

impl Default for Item {
    fn default() -> Self {
        Self {
            enabled: false,
            allnet_mode: false,
            prereq: None,
            state: 0,
            result: 0,
            hops_text: None,
            error: ErrorCell::EMPTY,
            power_on: false,
        }
    }
}

/// The controller (`ctx+490696`).
#[derive(Debug, Default)]
pub struct NetworkTest {
    items: [Item; ITEMS],
    /// 0 never armed, 1 armed idle, 2 running, 3 finished.
    state: u8,
    power_on: bool,
    listener: bool,
    allnet_auth: bool,
    events: Vec<NetworkTestEvent>,
    now: libam_api::time::Tm,
}

fn units(text: &str) -> [u16; 16] {
    let mut out = [0u16; 16];
    for (slot, unit) in out.iter_mut().zip(text.encode_utf16().take(15)) {
        *slot = unit;
    }
    out
}

impl NetworkTest {
    /// The constructed controller.
    pub fn new() -> Self {
        Self::default()
    }

    /// `OperatingState::enter` (`0x140248100`): computes the enabled bytes once.
    pub fn initialize(&mut self, cx: &mut Cx<'_>) {
        let net = cx.config.network;
        let an = cx.config.allnet_auth.enable;
        self.listener = net.app.test_event_listener;
        self.allnet_auth = an;
        let (e, gw, lr) = (net.enable, net.use_gateway, net.use_location_router);
        let route = an || (e && gw);
        let own = [
            an || e,
            route,
            lr && route,
            lr && route,
            lr && an,
            an,
            cx.config.aime.enable,
            cx.config.emoney.enable,
            cx.config.aimepay.enable,
        ];
        let prereq =
            [None, Some(0), Some(1), Some(2), Some(2), Some(if lr { 4 } else { 1 }), Some(5), Some(0), Some(5)];
        for index in 0..ITEMS {
            let enabled = own[index] && prereq[index].is_none_or(|p| self.items[p].enabled);
            self.items[index] = Item { enabled, allnet_mode: an, prereq: prereq[index], ..Item::default() };
        }
        self.state = 1;
        self.power_on = false;
    }

    /// `isItemEnabled` (`0x1402487D0`).
    pub fn is_item_enabled(&self, item: TestItem) -> bool {
        item.slot().is_some_and(|slot| self.items[slot].enabled)
    }

    /// `0x140248240`: stop without events.
    pub fn terminate(&mut self, _cx: &mut Cx<'_>) {
        if self.state != 0 {
            for item in &mut self.items {
                *item = Item {
                    enabled: item.enabled,
                    allnet_mode: item.allnet_mode,
                    prereq: item.prereq,
                    ..Item::default()
                };
            }
            self.power_on = false;
            self.state = 0;
            self.listener = false;
        }
    }

    /// Forced stop (`0x140248640`, end of the reset-all routine): finishes a running test with N/A, clears the
    /// power-on flag and re-arms so the next game-mode frame may start a new power-on test.
    pub fn forced_stop(&mut self, net: &Network, peers: &PeerView) {
        self.finish_not_applicable(net, peers);
        self.power_on = false;
        self.state = 1;
    }

    /// The `onTestEvent` calls made since the last call.
    pub fn take_events(&mut self) -> Vec<NetworkTestEvent> {
        std::mem::take(&mut self.events)
    }

    fn error_record(&self, error: ErrorCell) -> FatalError {
        if error.is_empty() {
            FatalError::default()
        } else {
            let (number, sub_number) = error.get().unwrap_or_default();
            FatalError::new(number, sub_number, self.now, "", 0)
        }
    }

    fn text(item: &Item) -> [u16; 16] {
        if let (Some(n), 2, 2) = (item.hops_text, item.state, item.result) {
            return units(&n.to_string());
        }
        match (item.result, item.state) {
            (1, _) => units("N/A"),
            (2, _) => units("GOOD"),
            (3, _) => units("BAD"),
            (0, 1) => units("CHECK"),
            _ => [0; 16],
        }
    }

    fn fire(&mut self, index: usize) {
        if !self.listener {
            return;
        }
        let item = self.items[index];
        let event = NetworkTestEvent {
            is_power_on: item.power_on,
            item: TestItem::CHECKERS[index],
            status: TestStatus { state: item.state, result: item.result, text: Self::text(&item) },
            error: self.error_record(item.error),
        };
        self.events.push(event);
    }

    fn fire_final(&mut self, result: u32, error: ErrorCell) {
        if !self.listener {
            return;
        }
        let event = NetworkTestEvent {
            is_power_on: self.power_on,
            item: TestItem::AllFinished,
            status: TestStatus {
                state: 2,
                result,
                text: units(match result {
                    1 => "N/A",
                    2 => "GOOD",
                    _ => "BAD",
                }),
            },
            error: self.error_record(error),
        };
        self.events.push(event);
    }

    fn manager_error(&self, net: &Network, peers: &PeerView) -> ErrorCell {
        let error = net.error_cell();
        if error.is_empty() { peers.allnet_manager_error } else { error }
    }

    /// `0x140248670`: arms every item and starts the run.
    pub fn start(&mut self, net: &Network, peers: &PeerView) {
        if !self.items.iter().any(|item| item.enabled) {
            return;
        }
        let started = if self.allnet_auth { peers.allnet_ready && peers.allnet_started } else { net.check_started() };
        for index in 0..ITEMS {
            let applicable = match index {
                7 => peers.emoney_applicable,
                8 => peers.aimepay_applicable,
                _ => true,
            };
            let power_on = self.power_on;
            let item = &mut self.items[index];
            item.power_on = power_on;
            item.error = ErrorCell::EMPTY;
            item.hops_text = None;
            let fire = if !item.enabled {
                (item.state, item.result) = (2, 0);
                started
            } else if applicable {
                (item.state, item.result) = if item.prereq.is_none() { (1, 0) } else { (0, 0) };
                started
            } else {
                (item.state, item.result) = (2, 1);
                true
            };
            if fire {
                self.fire(index);
            }
        }
        self.state = 2;
        if !started {
            self.finish_not_applicable(net, peers);
        }
    }

    /// `0x1402489F0`.
    fn finish_not_applicable(&mut self, net: &Network, peers: &PeerView) {
        if self.state != 2 {
            return;
        }
        for index in 0..ITEMS {
            if self.items[index].state != 2 {
                (self.items[index].state, self.items[index].result) = (2, 1);
                self.fire(index);
            }
        }
        let error = self.manager_error(net, peers);
        self.fire_final(1, error);
        self.state = 3;
    }

    /// `0x140248870`.
    fn abort(&mut self, error: ErrorCell) {
        let mut first = true;
        for index in 0..ITEMS {
            if self.items[index].state == 2 {
                continue;
            }
            if first {
                self.items[index].error = error;
                (self.items[index].state, self.items[index].result) = (2, 3);
                first = false;
            } else {
                (self.items[index].state, self.items[index].result) = (2, 1);
            }
            self.fire(index);
        }
        self.fire_final(3, error);
        self.state = 3;
    }

    /// `0x140248300`, last of the module updates.
    pub fn update(&mut self, cx: &mut Cx<'_>, game_mode: bool, modules_ready: bool, net: &Network, peers: &PeerView) {
        self.now = tm_from_date_time(cx.clock.local_now());
        let error = self.manager_error(net, peers);
        if !error.is_empty() && self.state == 2 {
            self.abort(error);
            return;
        }
        let setup_done =
            if self.allnet_auth { peers.allnet_ready && peers.allnet_setup_done } else { net.setup_done() };
        if game_mode && modules_ready && !self.power_on && setup_done {
            self.power_on = true;
            self.start(net, peers);
            return;
        }
        if self.state != 2 {
            return;
        }
        let mut all_done = true;
        for index in 0..ITEMS {
            if all_done {
                self.step(index, net, peers);
            }
            if self.items[index].state != 2 {
                all_done = false;
            }
        }
        if all_done {
            let bad = self.items.iter().find(|i| i.enabled && i.result == 3 && !i.error.is_empty()).map(|i| i.error);
            match bad {
                Some(error) => self.fire_final(3, error),
                None => self.fire_final(2, ErrorCell::EMPTY),
            }
            self.state = 3;
        }
    }

    /// `0x1402952D0`.
    fn step(&mut self, index: usize, net: &Network, peers: &PeerView) {
        let item = self.items[index];
        match item.state {
            0 => {
                let Some(p) = item.prereq.map(|p| self.items[p]).filter(|p| p.state == 2) else { return };
                (self.items[index].state, self.items[index].result) = if p.result == 2 { (1, 0) } else { (2, 1) };
                self.fire(index);
            }
            1 => {
                let Some(verdict) = Self::check(index, item.allnet_mode, net, peers) else { return };
                let target = &mut self.items[index];
                target.error = verdict.error;
                target.hops_text = verdict.hops;
                let mut next = (2, verdict.result);
                // The power-on quirk: a BAD item whose error carries no level keeps running (the error tables
                // are external data, so every record has level 0).
                if verdict.result == 3 && target.power_on && !target.error.is_empty() {
                    next = (1, 3);
                }
                if next != (target.state, target.result) {
                    (target.state, target.result) = next;
                    self.fire(index);
                }
            }
            _ => {}
        }
    }

    /// The per-item verdicts (T.8); `None` = wait.
    fn check(index: usize, allnet: bool, net: &Network, peers: &PeerView) -> Option<Verdict> {
        let good = Verdict { result: 2, error: ErrorCell::EMPTY, hops: None };
        let bad = |error| Verdict { result: 3, error, hops: None };
        let by_stage = |stage: i32| -> Option<Verdict> {
            if stage == 1 {
                Some(good)
            } else if !peers.allnet_error.is_empty() {
                Some(bad(peers.allnet_error))
            } else {
                None
            }
        };
        match (index, allnet) {
            (0, false) => {
                let error = net.current_error();
                if !error.is_empty() {
                    return Some(bad(error));
                }
                net.property().map(|_| good)
            }
            (0, true) => by_stage(peers.ns[0]),
            (1, false) | (2, false) => {
                let error = net.current_error();
                if !error.is_empty() {
                    return Some(bad(error));
                }
                let busy = if index == 1 { 4 } else { 5 };
                if net.phase() == busy {
                    return None;
                }
                if index == 1 && net.gateway_missing() {
                    return Some(Verdict { result: 1, error: ErrorCell::EMPTY, hops: None });
                }
                Some(good)
            }
            (1, true) => by_stage(peers.ns[1]),
            (2, true) => by_stage(peers.ns[2]),
            (3, false) => {
                if !net.location_done() {
                    return None;
                }
                let n = net.hops();
                Some(if n != 0 {
                    Verdict { result: 2, error: ErrorCell::EMPTY, hops: Some(n) }
                } else {
                    bad(ErrorCell::EMPTY)
                })
            }
            (3, true) => {
                let n = peers.ns[3];
                if n < 0 {
                    return None;
                }
                Some(if n != 0 {
                    Verdict { result: 2, error: ErrorCell::EMPTY, hops: Some(n.unsigned_abs()) }
                } else {
                    bad(ErrorCell::EMPTY)
                })
            }
            (4, _) => by_stage(peers.ns[4]),
            (5, _) => {
                if peers.authenticated {
                    Some(good)
                } else {
                    by_stage(0)
                }
            }
            (6, _) => peers.aime.map(|e| if e.is_empty() { good } else { bad(e) }),
            (7, _) => peers.emoney.map(|e| if e.is_empty() { good } else { bad(e) }),
            (8, _) => peers.aimepay.map(|e| if e.is_empty() { good } else { bad(e) }),
            _ => Some(bad(ErrorCell::EMPTY)),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Verdict {
    result: u32,
    error: ErrorCell,
    hops: Option<u32>,
}
