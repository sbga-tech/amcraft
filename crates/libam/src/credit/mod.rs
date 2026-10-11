//! The credit manager (`am::credit::Manager`, `spec/libam/credit.md`): coin counters, the credit rule, the
//! AS-6DB coin selector and `CreditPort`.
//!
//! # Cross-module methods
//!
//! * [`Credit::update`] (`0x14022B7B0`, M.3): `adapter` is the node adapter the flow selected by enabled flag,
//!   JVS > USB I/O > CAN ([`CoinAdapter::select`]); the coin counters and the lock-out go through [`CoinSlots`]
//!   (credit.md "selection paragraph"). `mode` is derived from the sequence state: advertise/play = state 3 or 4
//!   with no error, test = state 2 or 5.
//! * [`Credit::check`] / [`Credit::pay`] / [`Credit::rule`] / [`Credit::cost`]: what `SequencePort::credit_*`
//!   forwards to (credit.md part 2, `sequence.md` 4).
//! * The AS-6DB device is reached through [`backend::Backend::serial`].

pub mod as6db;
mod library;
mod persist;
mod port;

use std::collections::VecDeque;

use crate::config::{CreditConfig, CreditRule, UNSET_U32, UNSET_U64};
use crate::result::{AmError, AmResult};

use self::as6db::{InputNode, Io, SlotCondition, SpecialDevice};
use self::library::{EventKind, InitArgs, Library, NodeFlags, Rule, book};
use self::persist::Records;
use crate::module::{Cx, ErrorCell, Lifecycle, StateWord};
use crate::nvram::NvramContext;

pub use port::{BookkeepingTotals, CreditAppConfig, CreditHandle, CreditPort, InsertCoinEvent, PLAYERS, PlayerCredit};

/// The coin node interface of the adapter the credit manager reads (JVS, USB I/O or CAN module): the node
/// virtual slots of `credit.md` part I 3.9 and part III 2. Nodes are numbered from 0 in the adapter's node order;
/// the manager maps at most the first two nodes that have 1..=4 coin slots. Every query with an out-of-range
/// node, slot or switch index answers `0` / `false`.
pub trait CoinSlots {
    /// The adapter module is enabled and set up (the selection flag): `false` selects the local-input path.
    fn enabled(&self) -> bool;

    /// Whether the adapter's node manager is ready (vtable +16): the nodes can be mapped.
    fn ready(&self) -> bool;

    /// Number of coin nodes the adapter has.
    fn node_count(&self) -> usize;

    /// Number of coin slots of `node` (vtable +16).
    fn slot_count(&self, node: usize) -> usize;

    /// `shareCoinSlot` (vtable +24): the node's slots count as one player.
    fn share_coin_slot(&self, node: usize) -> bool;

    /// Coin jam of `slot` (vtable +32).
    fn jam(&self, node: usize, slot: usize) -> bool;

    /// Coin counter of `slot` disconnected (vtable +40).
    fn disconnected(&self, node: usize, slot: usize) -> bool;

    /// The 16-bit coin counter of `slot` (vtable +48); the USB I/O board reports 0..=255.
    fn coin_counter(&self, node: usize, slot: usize) -> u16;

    /// Number of service-switch words of `node` (vtable +64).
    fn switch_count(&self, node: usize) -> usize;

    /// Service switch `k` of `node` is pressed (vtable +72).
    fn service(&self, node: usize, k: usize) -> bool;

    /// Drives the coin lock-out output of `node`.
    fn set_lockout(&mut self, node: usize, on: bool);
}

/// No adapter is enabled: the credit manager runs the local-input path.
#[derive(Debug, Default)]
pub struct NoCoinSlots;

impl CoinSlots for NoCoinSlots {
    fn enabled(&self) -> bool {
        false
    }

    fn ready(&self) -> bool {
        false
    }

    fn node_count(&self) -> usize {
        0
    }

    fn slot_count(&self, _node: usize) -> usize {
        0
    }

    fn share_coin_slot(&self, _node: usize) -> bool {
        false
    }

    fn jam(&self, _node: usize, _slot: usize) -> bool {
        false
    }

    fn disconnected(&self, _node: usize, _slot: usize) -> bool {
        false
    }

    fn coin_counter(&self, _node: usize, _slot: usize) -> u16 {
        0
    }

    fn switch_count(&self, _node: usize) -> usize {
        0
    }

    fn service(&self, _node: usize, _k: usize) -> bool {
        false
    }

    fn set_lockout(&mut self, _node: usize, _on: bool) {}
}

/// Which module provides the coin counters this frame (`0x14029D590`: JVS `ctx+32512` > USB I/O `ctx+55168` >
/// CAN `ctx+35488`, by their enabled flag).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoinAdapter {
    /// None enabled.
    None,
    /// The JVS module.
    Jvs,
    /// The USB I/O module.
    UsbIo,
    /// The CAN module.
    Can,
}

impl CoinAdapter {
    /// The selection rule.
    pub fn select(jvs: bool, usbio: bool, can: bool) -> Self {
        if jvs {
            Self::Jvs
        } else if usbio {
            Self::UsbIo
        } else if can {
            Self::Can
        } else {
            Self::None
        }
    }
}

/// What the credit manager is told about the main flow each frame (credit.md "selection paragraph").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CreditMode {
    /// Advertise or play: coins are credited (sequence state 3 or 4 with no error).
    pub play: bool,
    /// Test mode (sequence state 2 or 5).
    pub test: bool,
}

/// Credit ceiling when the configuration leaves `max_credit` unset (`credit.md` 3.9).
const DEFAULT_MAX_CREDIT: u8 = 24;
/// Service credits allowed between two coins (`credit.md` 3.9).
const SERVICE_LIMIT: u8 = 9;
/// Coin boxes the manager mirrors (`credit.md` 3.11).
const COIN_BOXES: usize = 4;
/// Coin nodes the manager maps (`credit.md` 3.4).
const MAPPED_NODES: usize = 2;
/// Players the mapped nodes can feed (`credit.md` 3.4).
const MAPPED_PLAYERS: usize = 4;
/// Entries of the insert-coin dispatch ring (`credit.md` 3.7).
const EVENT_RING: usize = 32;
/// Message of a refused payment (`CreditPort::pay`).
const PAY_FAILED: &str = "Failed to use credit.";

/// Substate of the running manager (`Credit+0x4B8`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Substate {
    /// Initialisation failed: nothing runs until the manager restarts.
    Dead,
    /// Waiting for the node adapter to become ready.
    WaitingForNodes,
    /// Nodes mapped.
    Mapped,
}

/// The special device's view of the first mapped coin node (`updateInputInfo`).
struct AdapterNode<'a> {
    adapter: &'a dyn CoinSlots,
    node: usize,
}

impl InputNode for AdapterNode<'_> {
    fn slot_count(&self) -> usize {
        self.adapter.slot_count(self.node)
    }

    fn condition(&self, slot: usize) -> SlotCondition {
        if self.adapter.jam(self.node, slot) {
            SlotCondition::Jam
        } else if self.adapter.disconnected(self.node, slot) {
            SlotCondition::Disconnected
        } else {
            SlotCondition::Ok
        }
    }

    fn counter(&self, slot: usize) -> u16 {
        self.adapter.coin_counter(self.node, slot)
    }
}

/// `0x140264B60`: the configured rule fields over the persisted rule. A field that is `0` or unset keeps the
/// persisted value; shares and the operation are `!= 1`, numbers are truncated to the library's width.
fn overlay_rule(rule: &mut Rule, config: &CreditRule) {
    let set = |value: u32| (value != 0 && value != UNSET_U32).then_some(value);
    if let Some(value) = set(config.chute_share_type) {
        rule.chute_share = u8::from(value != 1);
    }
    if let Some(value) = set(config.service_share_type) {
        rule.service_share = u8::from(value != 1);
    }
    if let Some(value) = set(config.operation) {
        rule.freeplay = u8::from(value != 1);
    }
    for (slot, value) in rule.mult.iter_mut().zip(config.coin_chute_multiplier) {
        if let Some(value) = set(value) {
            *slot = value as u8;
        }
    }
    if let Some(value) = set(config.coin_to_credit) {
        rule.coin_to_credit = value as u8;
    }
    if let Some(value) = set(config.bonus_adder) {
        rule.bonus_adder = value as u8;
    }
    for (slot, value) in rule.cost.iter_mut().zip(config.game_cost) {
        if let Some(value) = set(value) {
            *slot = value as u8;
        }
    }
    if let Some(value) = set(config.coin_amount) {
        rule.coin_amount = value as u16;
    }
}

/// `0x14022C840`: the library rule as the runtime rule block (`share byte -> 1 + (byte != 0)`, `freeplay -> 1 +
/// (byte == 1)`).
fn runtime_rule(rule: &Rule) -> CreditRule {
    CreditRule {
        chute_share_type: 1 + u32::from(rule.chute_share != 0),
        service_share_type: 1 + u32::from(rule.service_share != 0),
        operation: 1 + u32::from(rule.freeplay == 1),
        coin_chute_multiplier: rule.mult.map(u32::from),
        coin_to_credit: u32::from(rule.coin_to_credit),
        bonus_adder: u32::from(rule.bonus_adder),
        game_cost: rule.cost.map(u32::from),
        coin_amount: u32::from(rule.coin_amount),
    }
}

/// The runtime rule block before the library has been started.
const NO_RULE: CreditRule = CreditRule {
    chute_share_type: 0,
    service_share_type: 0,
    operation: 0,
    coin_chute_multiplier: [0; 2],
    coin_to_credit: 0,
    bonus_adder: 0,
    game_cost: [0; 8],
    coin_amount: 0,
};

/// `am::credit::Manager` (`ctx+129336`).
#[derive(Debug)]
pub struct Credit {
    life: Lifecycle,
    config: CreditConfig,
    /// Players the game addresses (`max_player`, at most 8).
    player_total: usize,
    substate: Substate,
    coin_in_ignore: bool,
    records: Records,
    /// The amCredit library once the init step has run.
    library: Option<Library>,
    /// Adapter node indices of the mapped coin nodes (library nodes 1 and 2).
    mapped: Vec<usize>,
    /// The runtime rule block (`Credit+0x5B0`).
    rule: CreditRule,
    /// The mirrored node records, one per coin box (`Credit+0x500`).
    boxes: [PlayerCredit; COIN_BOXES],
    /// The mirrored bookkeeping object (`Credit+0x5F0`).
    coins: [u32; 8],
    totals: BookkeepingTotals,
    /// The dispatch ring and the frames the oldest entry has waited.
    ring: VecDeque<InsertCoinEvent>,
    retry: u32,
    /// Events handed to the listener and not yet taken.
    delivered: Vec<InsertCoinEvent>,
    special: SpecialDevice,
}

impl Default for Credit {
    fn default() -> Self {
        Self::new()
    }
}

impl Credit {
    /// The constructed manager.
    pub fn new() -> Self {
        Self {
            life: Lifecycle::default(),
            config: CreditConfig::default(),
            player_total: 0,
            substate: Substate::Dead,
            coin_in_ignore: false,
            records: Records::default(),
            library: None,
            mapped: Vec::new(),
            rule: NO_RULE,
            boxes: [PlayerCredit { credit: 0, remain: 0 }; COIN_BOXES],
            coins: [0; 8],
            totals: BookkeepingTotals::default(),
            ring: VecDeque::with_capacity(EVENT_RING),
            retry: 0,
            delivered: Vec::new(),
            special: SpecialDevice::default(),
        }
    }

    /// `OperatingState::enter` (`0x14022B5B0`): takes the configuration (the special device even when the module
    /// is disabled) and starts initialising; a disabled module is running at once. `sub_storage` is the platform's
    /// sub-storage byte (`ctx[26180]`), which the records' error mapping reads (84 or 87).
    pub fn initialize(&mut self, cx: &mut Cx<'_>, sub_storage: bool) {
        self.forget_runtime();
        self.records.set_sub_storage(sub_storage);
        self.config = cx.config.credit.clone();
        self.player_total = usize::try_from(cx.config.common.max_player).map_or(PLAYERS, |total| total.min(PLAYERS));
        self.special.set_config(&self.config.coin_selector_as6db, cx.clock);
        self.life.begin();
        if !self.config.enable {
            self.life.state = StateWord::READY;
        }
    }

    /// Drops everything the running manager built: the library (the handle release), the mirrors, the rings and
    /// the record images, so the next start reads the records again.
    fn forget_runtime(&mut self) {
        self.substate = Substate::Dead;
        self.coin_in_ignore = false;
        self.records.forget();
        self.library = None;
        self.mapped.clear();
        self.rule = NO_RULE;
        self.boxes = [PlayerCredit { credit: 0, remain: 0 }; COIN_BOXES];
        self.coins = [0; 8];
        self.totals = BookkeepingTotals::default();
        self.ring.clear();
        self.retry = 0;
        self.delivered.clear();
    }

    /// `0x14022B7B0`, once per Operating frame (M.3): the special device, the error propagation, the state
    /// machine of `credit.md` 3.9, the coin processing and the insert-coin dispatch.
    pub fn update(&mut self, cx: &mut Cx<'_>, adapter: &mut dyn CoinSlots, mode: CreditMode) {
        if !self.config.enable {
            return;
        }
        self.step_special(cx, adapter);
        if self.life.error.is_empty()
            && let Some(code) = self.special.error()
        {
            self.life.error.set(code, 0);
        }
        match self.life.state {
            StateWord::INITIALIZED => self.initialize_library(cx),
            StateWord::READY if self.substate == Substate::WaitingForNodes && adapter.ready() => {
                self.map_nodes(adapter);
            }
            _ => {}
        }
        if self.library.is_some() {
            self.process_coins(cx, adapter, mode);
            self.dispatch_events();
        }
    }

    /// `special.execute`: one frame of the AS-6DB driver and its supervision.
    fn step_special(&mut self, cx: &mut Cx<'_>, adapter: &dyn CoinSlots) {
        let node = if adapter.enabled() { self.mapped.first() } else { None };
        let node = node.map(|&node| AdapterNode { adapter, node });
        let backend = &mut *cx.backend;
        let mut io =
            Io { ports: &*backend.serial, clock: cx.clock, node: node.as_ref().map(|node| node as &dyn InputNode) };
        self.special.execute(&mut io);
    }

    /// An error of the manager's record: the first one stays until a manager reset.
    fn raise(&mut self, code: u16) {
        if self.life.error.is_empty() {
            self.life.error.set(code, 0);
        }
    }

    /// Whether any player has a coin box (`Context[262]`).
    fn has_coin_boxes(&self) -> bool {
        self.config.coin_box_assign.iter().any(|&coin_box| coin_box != UNSET_U64)
    }

    /// State 1 (`credit.md` 3.9): both records are read (the original has read them in the root's bulk read
    /// already), the library is started, the rule persisted and the mirrors filled. A manager error afterwards
    /// leaves the manager running but dead (substate 0).
    fn initialize_library(&mut self, cx: &mut Cx<'_>) {
        if self.library.is_some() {
            // The library is still held after a restart: nothing is read or configured again (`0x14022BF60`).
            self.life.state = StateWord::READY;
            self.substate = Substate::WaitingForNodes;
            return;
        }
        let nvram = cx.nvram;
        match self.records.load_credit(nvram).and_then(|()| self.records.load_backup(nvram)) {
            Ok(()) => self.start_library(nvram),
            Err(code) => self.raise(code),
        }
        self.life.state = StateWord::READY;
        self.substate = if self.life.error.is_empty() && self.library.is_some() {
            Substate::WaitingForNodes
        } else {
            Substate::Dead
        };
    }

    /// The init step with both records loaded: `Context::configure`, the library init, `CreditInfo.setConfig`,
    /// the runtime rule block and the first sync.
    fn start_library(&mut self, nvram: &NvramContext) {
        let mut rule = Rule::from_bytes(&self.records.rule_bytes());
        overlay_rule(&mut rule, &self.config.rule);
        let max_credit = match self.config.max_credit {
            UNSET_U32 => DEFAULT_MAX_CREDIT,
            value => value as u8,
        };
        let players = self
            .config
            .coin_box_assign
            .iter()
            .filter(|&&coin_box| coin_box != UNSET_U64)
            .map(|&coin_box| u8::try_from(coin_box.saturating_add(1)).unwrap_or(u8::MAX))
            .max()
            .unwrap_or(1);
        let args = InitArgs {
            rule: Some(rule),
            state: Some(self.records.state_bytes()),
            bookkeeping: Some(self.records.book_dwords()),
            max_credit,
            players,
            service_limit: SERVICE_LIMIT,
        };
        // The original ignores a refused `amCreditInit`: the module runs without a library and refuses everything.
        let Some(library) = Library::init(args) else {
            return;
        };
        let rule = *library.rule();
        self.rule = runtime_rule(&rule);
        self.library = Some(library);
        if let Err(code) = self.records.set_config(nvram, &rule.to_bytes()) {
            self.raise(code);
        }
        self.sync(Some(nvram), true);
    }

    /// `0x14022C180`: at most two adapter nodes with 1..=4 coin slots feed at most four players.
    fn map_nodes(&mut self, adapter: &dyn CoinSlots) {
        let common = self.rule.chute_share_type == 1;
        let mut offset = 0;
        let mut map = Vec::with_capacity(MAPPED_NODES);
        self.mapped.clear();
        for node in 0..adapter.node_count() {
            if self.mapped.len() == MAPPED_NODES || offset >= MAPPED_PLAYERS {
                break;
            }
            let slots = adapter.slot_count(node);
            if !(1..=4).contains(&slots) {
                continue;
            }
            let shared = adapter.share_coin_slot(node);
            let kind = if shared { 0x10 } else { 0x20 };
            map.push(if common { 0x10 } else { offset as u32 | kind });
            offset += if shared { 1 } else { slots };
            self.mapped.push(node);
        }
        if let Some(library) = self.library.as_mut() {
            library.set_node_mapping(&map);
        }
        self.substate = Substate::Mapped;
    }

    /// `0x14022C370`: the nodes' counters (or the local input), then `sync(persist)`.
    fn process_coins(&mut self, cx: &mut Cx<'_>, adapter: &dyn CoinSlots, mode: CreditMode) {
        let coin_allowed =
            mode.play && self.has_coin_boxes() && self.substate != Substate::WaitingForNodes && !self.coin_in_ignore;
        let now = cx.clock.tick_ms();
        if let Some(library) = self.library.as_mut() {
            if adapter.enabled() {
                for (index, &node) in self.mapped.iter().enumerate() {
                    let slots = adapter.slot_count(node).min(8);
                    let mut counters = [0u16; 8];
                    for (slot, counter) in counters.iter_mut().enumerate().take(slots) {
                        *counter = adapter.coin_counter(node, slot);
                        if adapter.jam(node, slot) || adapter.disconnected(node, slot) {
                            *counter |= 0x8000;
                        }
                    }
                    let mut flags = NodeFlags { inhibit: !coin_allowed, discard: mode.test, service: [false; 4] };
                    for (k, pressed) in flags.service.iter_mut().enumerate().take(adapter.switch_count(node)) {
                        *pressed = adapter.service(node, k);
                    }
                    library.node_input(index + 1, &counters[..slots], flags, now);
                }
            } else {
                library.local_input(mode.test);
            }
        }
        self.sync(Some(cx.nvram), false);
    }

    /// `0x14022C5F0`: the library state into the node records and the bookkeeping object, and, with an NVRAM
    /// context, a pending change into the backup record. Without one the change stays flagged and the next
    /// frame's sync writes it.
    fn sync(&mut self, nvram: Option<&NvramContext>, mirror: bool) {
        let Some(library) = self.library.as_mut() else {
            return;
        };
        let dirty = nvram.is_some() && library.take_dirty();
        let mut state = library.state();
        let book = library.bookkeeping();
        let players = library.players();
        if library.rule().chute_share == 0 {
            for player in 1..players {
                state.copy_within(0..2, 2 * player);
            }
        }
        if dirty
            && let Some(nvram) = nvram
            && let Err(code) = self.records.set_backup(nvram, &state, &book)
        {
            self.raise(code);
        }
        if mirror || dirty {
            for (index, record) in self.boxes.iter_mut().enumerate().take(players) {
                record.credit = u32::from(state[2 * index]);
                record.remain = u32::from(state[2 * index + 1]);
            }
            self.coins.copy_from_slice(&book[..8]);
            self.totals = BookkeepingTotals {
                total_coin: book[book::TOTAL_COIN],
                coin_credit: book[book::COIN_CREDIT],
                service_credit: book[book::SERVICE_CREDIT],
                e_money_coin: book[book::EMONEY_COIN],
                e_money_credit: book[book::EMONEY_CREDIT],
                total_credit: book[book::TOTAL_CREDIT],
            };
        }
    }

    /// `0x14022B130`: the library's insert-coin events into the ring and at most one ring entry to the listener
    /// per frame (which always accepts).
    fn dispatch_events(&mut self) {
        if !self.config.app.insert_coin_listener {
            return;
        }
        let Some(library) = self.library.as_mut() else {
            return;
        };
        let common = self.rule.chute_share_type == 1;
        let mut into_empty = None;
        while let Some(event) = library.pop_event() {
            into_empty.get_or_insert(self.ring.is_empty());
            let mut players = [false; PLAYERS];
            for (player, applies) in players.iter_mut().enumerate() {
                let coin_box = self.config.coin_box_assign[player];
                *applies = if common { coin_box != UNSET_U64 } else { coin_box == event.player as u64 };
            }
            if self.ring.len() == EVENT_RING {
                self.ring.pop_front();
            }
            self.ring.push_back(InsertCoinEvent { service: event.kind != EventKind::Coin, players });
        }
        if self.ring.is_empty() {
            return;
        }
        self.retry = self.retry.saturating_add(1);
        if into_empty == Some(true) || self.retry >= self.config.notify_interval {
            self.delivered.extend(self.ring.pop_front());
            self.retry = 0;
        }
    }

    /// `0x14022B750`, credit shutdown: the special device closes, the library is released.
    pub fn terminate(&mut self, cx: &mut Cx<'_>) {
        let _ = cx;
        self.special.shutdown();
        self.forget_runtime();
        self.life.end();
    }

    /// Module restart (`0x14022B9A0` with the reset `0x14022BEE0`): the special device forgets its faults when it is
    /// configured, then the manager goes back to initialising with its substate, coin-in-ignore flag, mapped nodes
    /// and insert-event ring cleared. The library, the record images, the mirrored credits and the bookkeeping stay:
    /// the next init step finds the library held and only waits for the node adapter again (`0x14022BF60`).
    pub fn restart(&mut self) {
        if self.config.enable && self.life.state == StateWord::READY {
            if self.special.is_available() {
                self.special.reset();
            }
            self.substate = Substate::Dead;
            self.coin_in_ignore = false;
            self.mapped.clear();
            self.ring.clear();
            self.retry = 0;
            self.life.begin();
        }
    }

    /// Player to coin box (`0x14022CC00`): `None` when the module is disabled, not running, in error, or the
    /// player is outside the game or has no box.
    fn coin_box(&self, player: u32) -> Option<usize> {
        let player = usize::try_from(player).ok().filter(|&player| player < self.player_total)?;
        if !self.config.enable || self.life.state != StateWord::READY || !self.life.error.is_empty() {
            return None;
        }
        let coin_box = *self.config.coin_box_assign.get(player)?;
        (coin_box != UNSET_U64).then(|| usize::try_from(coin_box).ok()).flatten()
    }

    /// The coin box and the whole-credit price of `cost_count` games at `cost_index` (`check`/`pay` preconditions).
    fn price(&self, player: u32, cost_index: i16, cost_count: u16) -> Option<(usize, u32)> {
        let coin_box = self.coin_box(player)?;
        let library = self.library.as_ref()?;
        let index = usize::try_from(cost_index).ok().filter(|&index| index < 8)?;
        if cost_count == 0 {
            return None;
        }
        let cost = u32::from(library.rule().cost[index]) * u32::from(cost_count);
        (cost <= 255 && coin_box < library.players()).then_some((coin_box, cost))
    }

    /// `sequence.credit_check` (`libam.credit.check`, `0x14022BD20`): 0 not affordable, 1 partial credit held, 2
    /// affordable ([`crate::sequence::CREDIT_AFFORDABLE`]); never an error.
    pub fn check(&self, player: u32, cost_index: i16, cost_count: u16) -> AmResult<u32> {
        let state = self
            .price(player, cost_index, cost_count)
            .and_then(|(coin_box, cost)| self.library.as_ref().map(|library| library.check(coin_box, cost)));
        Ok(state.unwrap_or(0))
    }

    /// `sequence.credit_pay` after the root's sequence gate (`0x14022B9F0`): consumes the price when affordable.
    /// The change is mirrored at once and persisted by the next frame.
    pub fn pay(&mut self, player: u32, cost_index: i16, cost_count: u16) -> AmResult {
        let paid = match (self.price(player, cost_index, cost_count), self.library.as_mut()) {
            (Some((coin_box, cost)), Some(library)) => {
                library.check(coin_box, cost) == 2 && library.pay(coin_box, cost)
            }
            _ => false,
        };
        if !paid {
            return Err(AmError::with_message(AmError::FAILED, PAY_FAILED));
        }
        self.sync(None, true);
        Ok(())
    }

    /// Units of coin that still fit in `player`'s credit (`0x14022BDD0`); `0` without a coin box.
    pub fn addable(&self, player: u32) -> u32 {
        match (self.coin_box(player), self.library.as_ref()) {
            (Some(coin_box), Some(library)) => library.addable(coin_box),
            _ => 0,
        }
    }

    /// Coins from outside the chutes (e-money, AimePay; `0x14022BAC0`): `false` when the player has no coin box
    /// or the manager is in error. The change is mirrored at once and persisted by the next frame.
    pub fn add_coin(&mut self, player: u32, coins: u32) -> bool {
        let added = match (self.coin_box(player), self.library.as_mut()) {
            (Some(coin_box), Some(library)) => library.add_coin(coin_box, coins),
            _ => false,
        };
        if added {
            self.sync(None, true);
        }
        added
    }

    /// `credit.clear()` (`0x14022BBC0`): zeroes credits and bookkeeping when the module runs without error.
    pub fn clear(&mut self) {
        if !self.config.enable || self.life.state != StateWord::READY || !self.life.error.is_empty() {
            return;
        }
        if let Some(library) = self.library.as_mut() {
            library.clear();
            self.sync(None, true);
        }
    }

    /// `credit.setCoinInIgnore` (`0x14022BC20`): only an enabled module stores it.
    pub fn set_coin_in_ignore(&mut self, ignore: bool) {
        if self.config.enable {
            self.coin_in_ignore = ignore;
        }
    }

    /// `credit.coinInIgnored` (`0x14022BE30`).
    pub fn ignore_coin(&self) -> bool {
        self.coin_in_ignore
    }

    /// `credit.player(index)` value (`0x14022BCE0`): the node record of the player's coin box.
    pub fn player_value(&self, player: u32) -> Option<PlayerCredit> {
        self.boxes.get(self.coin_box(player)?).copied()
    }

    /// Bookkeeping coin counter `index`; `None` for an index past the eight counters.
    pub fn bookkeeping_coin(&self, index: u32) -> Option<u32> {
        self.coins.get(usize::try_from(index).ok()?).copied()
    }

    /// The aggregate bookkeeping counters.
    pub fn bookkeeping_totals(&self) -> BookkeepingTotals {
        self.totals
    }

    /// The runtime rule block (`Credit+0x5B0`).
    pub fn rule_block(&self) -> &CreditRule {
        &self.rule
    }

    /// `sequence.credit_rule` (the rule block's `operation`).
    pub fn rule(&self) -> u32 {
        self.rule.operation
    }

    /// `sequence.credit_cost(i)`.
    pub fn cost(&self, cost_index: i16) -> AmResult<u32> {
        usize::try_from(cost_index)
            .ok()
            .and_then(|index| self.rule.game_cost.get(index).copied())
            .ok_or_else(|| AmError::code(AmError::INVALID_ARGUMENT))
    }

    /// `credit.specialDevice.isAvailable()` (`0x14022BE40`): the AS-6DB block's enable byte.
    pub fn special_device_available(&self) -> bool {
        self.special.is_available()
    }

    /// `credit.lockoutOn()` (`0x14022BEA0`).
    pub fn lockout_on(&self) -> bool {
        self.is_running() && self.special.lockout_on()
    }

    /// `credit.canLockout()` (`0x14022BE60`).
    pub fn can_lockout(&self) -> bool {
        self.is_running() && self.special.can_lockout()
    }

    /// `credit.specialLockout(on)` (`0x14022BC60`); `false` when the device cannot change its lockout now.
    pub fn special_lockout(&mut self, on: bool) -> bool {
        self.is_running() && self.special.special_lockout(on)
    }

    /// The module is enabled and in state 2.
    fn is_running(&self) -> bool {
        self.config.enable && self.life.state == StateWord::READY
    }

    /// The events the listener received since the last call, oldest first.
    pub fn take_events(&mut self) -> Vec<InsertCoinEvent> {
        std::mem::take(&mut self.delivered)
    }

    /// `ctx+130540`.
    pub fn state(&self) -> StateWord {
        self.life.state
    }

    /// `allReady` term: state == 2.
    pub fn is_ready(&self) -> bool {
        self.life.state == StateWord::READY
    }

    /// The error cell (`ctx+130240`).
    pub fn error_cell(&self) -> ErrorCell {
        self.life.error
    }

    /// `credit.enable`.
    pub fn enabled(&self) -> bool {
        self.config.enable
    }
}
