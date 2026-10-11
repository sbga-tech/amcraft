//! The amCredit library (`credit.md` part I 2.1 to 3.8): rule bytes, per-player credit state, bookkeeping,
//! node coin inputs, service switches and the insert-coin ring.
//!
//! The original's locked/unlocked wrapper split and its `-2/-3/-4` result mapping are not observable here:
//! an uninitialised library is the absence of a [`Library`] value.

use std::collections::VecDeque;

/// Players the library holds (`credit state` is 8 pairs).
pub const MAX_PLAYERS: usize = 8;
/// Bytes of the persisted credit state (8 pairs plus 48 verbatim bytes).
pub const STATE_LEN: usize = 64;
/// Dwords of the bookkeeping block.
pub const BOOK_LEN: usize = 36;
/// Usable slots of the insert-coin ring (16 slots, one is the full marker).
const EVENT_SLOTS: usize = 15;
/// A service switch must be held longer than this before it pays (`> 100000` us).
const SERVICE_HOLD_MS: u64 = 100;

/// Bookkeeping dword indices (`2.3`).
pub mod book {
    /// `emoney_coin`.
    pub const EMONEY_COIN: usize = 30;
    /// `emoney_credit`.
    pub const EMONEY_CREDIT: usize = 31;
    /// `total_coin`.
    pub const TOTAL_COIN: usize = 32;
    /// `coin_credit`.
    pub const COIN_CREDIT: usize = 33;
    /// `service_credit`.
    pub const SERVICE_CREDIT: usize = 34;
    /// `total_credit`.
    pub const TOTAL_CREDIT: usize = 35;
}

/// The 32-byte rule config (`2.2`): the library form and the persisted form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rule {
    /// 0 common, 1 individual.
    pub chute_share: u8,
    /// 0 common, 1 individual.
    pub service_share: u8,
    /// 0 coin, 1 freeplay.
    pub freeplay: u8,
    /// Chute multipliers, 1..=9.
    pub mult: [u8; 2],
    /// 0 or 2..=9.
    pub bonus_adder: u8,
    /// 1..=99.
    pub coin_to_credit: u8,
    /// Game costs, 1..=255.
    pub cost: [u8; 8],
    /// Byte 15, copied verbatim.
    pub pad: u8,
    /// Non-zero.
    pub coin_amount: u16,
    /// Bytes 18..32, copied verbatim.
    pub spare: [u8; 14],
}

impl Rule {
    /// The defaults table (`0x140701B28`).
    pub const DEFAULT: Self = Self {
        chute_share: 0,
        service_share: 1,
        freeplay: 0,
        mult: [1, 1],
        bonus_adder: 0,
        coin_to_credit: 1,
        cost: [1; 8],
        pad: 0,
        coin_amount: 100,
        spare: [0; 14],
    };

    /// Reads the 32 persisted bytes.
    pub fn from_bytes(bytes: &[u8; 32]) -> Self {
        let mut cost = [0; 8];
        cost.copy_from_slice(&bytes[7..15]);
        let mut spare = [0; 14];
        spare.copy_from_slice(&bytes[18..32]);
        Self {
            chute_share: bytes[0],
            service_share: bytes[1],
            freeplay: bytes[2],
            mult: [bytes[3], bytes[4]],
            bonus_adder: bytes[5],
            coin_to_credit: bytes[6],
            cost,
            pad: bytes[15],
            coin_amount: u16::from_le_bytes([bytes[16], bytes[17]]),
            spare,
        }
    }

    /// The 32 persisted bytes.
    pub fn to_bytes(self) -> [u8; 32] {
        let mut bytes = [0; 32];
        bytes[0] = self.chute_share;
        bytes[1] = self.service_share;
        bytes[2] = self.freeplay;
        bytes[3] = self.mult[0];
        bytes[4] = self.mult[1];
        bytes[5] = self.bonus_adder;
        bytes[6] = self.coin_to_credit;
        bytes[7..15].copy_from_slice(&self.cost);
        bytes[15] = self.pad;
        bytes[16..18].copy_from_slice(&self.coin_amount.to_le_bytes());
        bytes[18..32].copy_from_slice(&self.spare);
        bytes
    }

    /// `0x1402DE600`.
    pub fn is_valid(&self) -> bool {
        self.chute_share <= 1
            && self.service_share <= 1
            && self.freeplay <= 1
            && self.mult.iter().all(|m| (1..=9).contains(m))
            && self.bonus_adder <= 9
            && self.bonus_adder != 1
            && (1..=99).contains(&self.coin_to_credit)
            && self.cost.iter().all(|&c| c != 0)
            && self.coin_amount != 0
    }
}

/// What the manager gives the library at init (`0x1402DDF30` arguments).
#[derive(Debug, Clone)]
pub struct InitArgs {
    /// Persisted rule; `None` or invalid selects the defaults.
    pub rule: Option<Rule>,
    /// Persisted credit state; `None` starts empty.
    pub state: Option<[u8; STATE_LEN]>,
    /// Persisted bookkeeping; `None` starts empty.
    pub bookkeeping: Option<[u32; BOOK_LEN]>,
    /// Credit ceiling, non-zero.
    pub max_credit: u8,
    /// Players, 1..=8.
    pub players: u8,
    /// Service credits allowed between coins.
    pub service_limit: u8,
}

/// The node-level inputs of one frame for one coin node (`0x1402DF5A0` arguments).
#[derive(Debug, Clone, Copy, Default)]
pub struct NodeFlags {
    /// `0x20000000`: coins are not allowed, counters are only re-baselined.
    pub inhibit: bool,
    /// `0x40000000`: discard the partial credit.
    pub discard: bool,
    /// Service switch `k` pressed (`0x10 << k`).
    pub service: [bool; 4],
}

#[derive(Debug, Clone, Copy, Default)]
struct NodeInput {
    prev: [u16; 8],
    service_t0: [u64; 4],
    service_held: [bool; 4],
    service_fired: [bool; 4],
}

/// Kind of an insert-coin event (`3.7`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// A coin was entered, no credit completed.
    Coin,
    /// The credit increased.
    Credit,
}

/// One entry of the library's insert-coin ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LibEvent {
    /// What happened.
    pub kind: EventKind,
    /// The library player (coin box).
    pub player: usize,
}

/// The initialised amCredit state block.
#[derive(Debug, Clone)]
pub struct Library {
    rule: Rule,
    state: [u8; STATE_LEN],
    book: [u32; BOOK_LEN],
    state_dirty: bool,
    book_dirty: bool,
    mapped_nodes: usize,
    node_map: [u32; 5],
    nodes: [NodeInput; 5],
    events: VecDeque<LibEvent>,
    max_credit: u8,
    service_limit: u8,
    players: u8,
    bonus_phase: [u8; MAX_PLAYERS],
    service_count: [u8; MAX_PLAYERS],
}

impl Library {
    /// `0x1402DDF30`. `None` when `max_credit` is zero or `players` is not 1..=8 (the original's `0x81000003`).
    pub fn init(args: InitArgs) -> Option<Self> {
        if args.max_credit == 0 || !(1..=MAX_PLAYERS as u8).contains(&args.players) {
            return None;
        }
        let mut state_dirty = false;
        let mut book_dirty = false;
        let rule = match args.rule.filter(Rule::is_valid) {
            Some(rule) => Some(rule),
            None => {
                state_dirty = true;
                None
            }
        };
        let defaulted = rule.is_none();
        let rule = rule.unwrap_or(Rule::DEFAULT);
        let mut state = [0; STATE_LEN];
        match args.state {
            None => state_dirty = true,
            Some(mut persisted) => {
                if defaulted {
                    persisted[..2 * usize::from(args.players)].fill(0);
                } else {
                    for player in 0..usize::from(args.players) {
                        if persisted[2 * player] > args.max_credit {
                            persisted[2 * player] = args.max_credit;
                            state_dirty = true;
                        }
                        if persisted[2 * player + 1] != 0 {
                            persisted[2 * player + 1] = 0;
                            state_dirty = true;
                        }
                    }
                }
                state = persisted;
            }
        }
        let book = match args.bookkeeping {
            Some(book) => book,
            None => {
                book_dirty = true;
                [0; BOOK_LEN]
            }
        };
        Some(Self {
            rule,
            state,
            book,
            state_dirty,
            book_dirty,
            mapped_nodes: 0,
            node_map: [0x20; 5],
            nodes: std::array::from_fn(|node| NodeInput {
                prev: if node == 0 { [0; 8] } else { [0x8000; 8] },
                ..NodeInput::default()
            }),
            events: VecDeque::with_capacity(EVENT_SLOTS),
            max_credit: args.max_credit,
            service_limit: args.service_limit,
            players: args.players,
            bonus_phase: [0; MAX_PLAYERS],
            service_count: [0; MAX_PLAYERS],
        })
    }

    /// The rule bytes (`0x1402DE9E0`).
    pub fn rule(&self) -> &Rule {
        &self.rule
    }

    /// Number of players.
    pub fn players(&self) -> usize {
        usize::from(self.players)
    }

    /// The 64 state bytes (`0x1402DEB30`).
    pub fn state(&self) -> [u8; STATE_LEN] {
        self.state
    }

    /// The 36 bookkeeping dwords (`0x1402DE930`).
    pub fn bookkeeping(&self) -> [u32; BOOK_LEN] {
        self.book
    }

    /// Credit of player `p`.
    pub fn credit(&self, p: usize) -> u8 {
        self.state[2 * p]
    }

    /// Fractional coins of player `p`.
    pub fn remain(&self, p: usize) -> u8 {
        self.state[2 * p + 1]
    }

    fn set_credit(&mut self, p: usize, value: u8) {
        self.state[2 * p] = value;
    }

    fn set_remain(&mut self, p: usize, value: u8) {
        self.state[2 * p + 1] = value;
    }

    /// Pops and clears both dirty flags (`0x1402DE5A0`, `0x1402DE540`); `true` when either was set.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.state_dirty) | std::mem::take(&mut self.book_dirty)
    }

    /// `0x1402DEEC0`: how many coin nodes (0..=4) feed players and the map word of nodes 1.. .
    pub fn set_node_mapping(&mut self, map: &[u32]) {
        let count = map.len().min(4);
        self.mapped_nodes = count;
        self.node_map[1..=count].copy_from_slice(&map[..count]);
    }

    /// `0x1402DD120`.
    fn multiplier(&self, chute: usize) -> u32 {
        if self.rule.chute_share == 1 { u32::from(self.rule.mult[0]) } else { u32::from(self.rule.mult[chute & 1]) }
    }

    /// `0x1402DD9F0`.
    fn player_for(&self, node: usize, k: usize) -> usize {
        if self.rule.chute_share == 0 {
            return 0;
        }
        let map = self.node_map[if node > 4 { 1 } else { node }];
        let base = match map & !0xF {
            0x10 => (map & 0xF) as usize,
            0x20 => k + (map & 0xF) as usize,
            _ => 0,
        };
        if base >= self.players() { 0 } else { base }
    }

    fn push_event(&mut self, kind: EventKind, player: usize) {
        if self.events.len() < EVENT_SLOTS {
            self.events.push_back(LibEvent { kind, player });
        }
    }

    /// `0x1402DDB90`: the oldest insert-coin event.
    pub fn pop_event(&mut self) -> Option<LibEvent> {
        self.events.pop_front()
    }

    /// `0x1402DC910`: units -> bonus -> credit/remain, clamp, event. Returns the credit gained (a wrapping
    /// difference of the two `u8` values, as the original computes it).
    fn add_units(&mut self, p: usize, units: u32, emit: bool) -> u32 {
        if self.rule.freeplay != 0 {
            return 0;
        }
        let prev = self.credit(p);
        let mut bonus = 0u32;
        if self.rule.bonus_adder != 0 {
            let n = u32::from(self.rule.bonus_adder);
            let sum = units.wrapping_add(u32::from(self.bonus_phase[p]));
            bonus = sum / n;
            self.bonus_phase[p] = (sum % n) as u8;
        }
        let total = bonus.wrapping_add(units).wrapping_add(u32::from(self.remain(p)));
        let per_credit = u32::from(self.rule.coin_to_credit);
        let q = total / per_credit;
        self.set_remain(p, (total % per_credit) as u8);
        let credit = self.credit(p).wrapping_add(q as u8);
        self.set_credit(p, credit);
        if credit >= self.max_credit {
            self.set_credit(p, self.max_credit);
            self.set_remain(p, 0);
        }
        if self.rule.chute_share == 1 && self.rule.service_share == 0 {
            self.service_count[0] = 0;
        } else {
            self.service_count[p] = 0;
        }
        self.state_dirty = true;
        let kind = if emit && prev < self.max_credit {
            if q != 0 {
                Some(EventKind::Credit)
            } else if units != 0 {
                Some(EventKind::Coin)
            } else {
                None
            }
        } else {
            None
        };
        let delta = u32::from(self.credit(p)).wrapping_sub(u32::from(prev));
        if let Some(kind) = kind {
            self.push_event(kind, p);
        }
        delta
    }

    /// `0x1402DCAF0`: `service_add(p, n)`.
    fn service_add(&mut self, p: usize, n: u8) -> u32 {
        if self.rule.freeplay != 0 {
            return 0;
        }
        let mut gained = 0u32;
        if self.rule.chute_share == 1 && self.rule.service_share == 0 {
            for player in 0..self.players() {
                if self.service_count[0] < self.service_limit && self.credit(player) < self.max_credit {
                    gained += self.raise(player, n);
                }
            }
            if gained != 0 {
                self.service_count[0] += 1;
            }
        } else if self.service_count[p] < self.service_limit && self.credit(p) < self.max_credit {
            self.service_count[p] += 1;
            gained += self.raise(p, n);
        }
        self.state_dirty = true;
        if gained != 0 {
            self.push_event(EventKind::Credit, p);
        }
        gained
    }

    /// `credit += n` with the clamp; the gain.
    fn raise(&mut self, p: usize, n: u8) -> u32 {
        let before = self.credit(p);
        let mut after = before.wrapping_add(n);
        if after >= self.max_credit {
            after = self.max_credit;
            self.set_remain(p, 0);
        }
        self.set_credit(p, after);
        u32::from(after).wrapping_sub(u32::from(before))
    }

    /// `0x1402DC7C0`: coins from outside the chutes (e-money, AimePay). `false` for the original's non-zero
    /// result; never fails once the library exists.
    pub fn add_coin(&mut self, p: usize, coins: u32) -> bool {
        if self.rule.freeplay != 0 {
            return true;
        }
        let p = if self.rule.chute_share == 0 || p >= self.players() { 0 } else { p };
        let units = coins.wrapping_mul(u32::from(self.rule.mult[0]));
        let gained = self.add_units(p, units, false);
        self.book[book::EMONEY_COIN] = self.book[book::EMONEY_COIN].wrapping_add(coins);
        self.book[book::EMONEY_CREDIT] = self.book[book::EMONEY_CREDIT].wrapping_add(gained);
        self.book[book::TOTAL_CREDIT] = self.book[book::TOTAL_CREDIT].wrapping_add(gained);
        self.book_dirty = true;
        true
    }

    /// `0x1402DCFC0`: units of coin that still fit.
    pub fn addable(&self, p: usize) -> u32 {
        if p >= self.players() || self.rule.freeplay != 0 {
            return 0;
        }
        let p = if self.rule.chute_share == 0 { 0 } else { p };
        let per_credit = u32::from(self.rule.coin_to_credit);
        let remain = u32::from(self.remain(p));
        if per_credit <= remain {
            return 0;
        }
        let space = u32::from(self.max_credit).wrapping_sub(u32::from(self.credit(p)));
        let room = per_credit.wrapping_mul(space).wrapping_sub(remain);
        if self.rule.bonus_adder == 0 {
            return room;
        }
        let n = u32::from(self.rule.bonus_adder);
        let phase = u32::from(self.bonus_phase[p]);
        let mut best = (u64::from(room) * u64::from(n) / u64::from(n + 1)) as u32;
        if best < room {
            let mut cand = best;
            while cand < room {
                let s = cand + phase;
                if cand + s / n > room {
                    break;
                }
                best = cand;
                cand += 1;
            }
        }
        best
    }

    /// `0x1402DDD60`: 0 not affordable and nothing held, 1 partial credit, 2 affordable.
    pub fn check(&self, p: usize, cost: u32) -> u32 {
        if p >= self.players() {
            return 0;
        }
        let p = if self.rule.chute_share == 0 { 0 } else { p };
        if self.rule.freeplay != 0 || u32::from(self.credit(p)) >= cost {
            2
        } else if self.credit(p) != 0 || self.remain(p) != 0 {
            1
        } else {
            0
        }
    }

    /// `0x1402DE4C0`: consumes `cost` credits when affordable.
    pub fn pay(&mut self, p: usize, cost: u32) -> bool {
        if self.check(p, cost) != 2 {
            return false;
        }
        if self.rule.freeplay == 0 {
            let p = if self.rule.chute_share == 0 { 0 } else { p };
            self.set_credit(p, self.credit(p).wrapping_sub(cost as u8));
            self.bonus_phase[p] = 0;
            self.state_dirty = true;
        }
        true
    }

    /// `0x1402DCE10`: zero the credit state and the bookkeeping.
    pub fn clear(&mut self) {
        self.state = [0; STATE_LEN];
        self.book = [0; BOOK_LEN];
        self.state_dirty = true;
        self.book_dirty = true;
    }

    fn discard_partial(&mut self) {
        self.bonus_phase = [0; MAX_PLAYERS];
        self.service_count = [0; MAX_PLAYERS];
        for p in 0..MAX_PLAYERS {
            if self.remain(p) != 0 {
                self.set_remain(p, 0);
                self.state_dirty = true;
            }
        }
    }

    /// `0x1402DF110` with no node manager: only the partial-credit discard can happen (no local switch bit is
    /// ever set).
    pub fn local_input(&mut self, discard: bool) {
        if discard {
            self.discard_partial();
        }
    }

    /// `0x1402DF5A0`: the counters and service switches of coin node `node` (1..=4). Ignored for a node that is
    /// not mapped.
    pub fn node_input(&mut self, node: usize, counters: &[u16], flags: NodeFlags, now_ms: u64) {
        if node == 0 || node > self.mapped_nodes {
            return;
        }
        let count = counters.len().min(MAX_PLAYERS);
        let mut service = flags.service;
        if flags.inhibit || flags.discard {
            service = [false; 4];
            self.nodes[node].prev[..count].copy_from_slice(&counters[..count]);
        }
        if flags.discard {
            self.discard_partial();
        }
        if self.node_map[node] != 0 {
            self.counters_to_coins(node, &counters[..count]);
            self.service_switches(node, service, now_ms);
        }
    }

    /// `0x1402DF1D0`.
    fn counters_to_coins(&mut self, node: usize, counters: &[u16]) {
        for (j, &cur) in counters.iter().enumerate() {
            let prev = self.nodes[node].prev[j];
            if (cur | prev) & 0xC000 == 0 {
                let d = u32::from(cur.wrapping_sub(prev) & 0xF);
                if d != 0 {
                    let base = self.player_for(node, j);
                    let mut slot = (j + 2 * (node - 1)) as i64;
                    let map = i64::from(self.node_map[node]);
                    if map & 0x20 != 0 && (self.players() as i64) < (map ^ 0x20) + slot - 1 {
                        slot = 0;
                    }
                    let mut gained = 0u32;
                    for _ in 0..d {
                        gained = gained.wrapping_add(self.add_units(base, self.multiplier(j), true));
                    }
                    if slot <= 7 {
                        let slot = slot as usize;
                        self.book[slot] = self.book[slot].wrapping_add(d);
                        self.book[book::TOTAL_COIN] = self.book[book::TOTAL_COIN].wrapping_add(d);
                        self.book[book::COIN_CREDIT] = self.book[book::COIN_CREDIT].wrapping_add(gained);
                        self.book[book::TOTAL_CREDIT] = self.book[book::TOTAL_CREDIT].wrapping_add(gained);
                        self.book_dirty = true;
                    }
                }
            }
            self.nodes[node].prev[j] = cur;
        }
    }

    /// `0x1402DF6E0`.
    fn service_switches(&mut self, node: usize, pressed: [bool; 4], now_ms: u64) {
        for (k, &down) in pressed.iter().enumerate() {
            if down {
                if !self.nodes[node].service_held[k] {
                    self.nodes[node].service_t0[k] = now_ms;
                    self.nodes[node].service_held[k] = true;
                }
                let held = now_ms.wrapping_sub(self.nodes[node].service_t0[k]);
                if held > SERVICE_HOLD_MS && !self.nodes[node].service_fired[k] {
                    let player = self.player_for(node, k);
                    let d = self.service_add(player, 1);
                    self.book[book::SERVICE_CREDIT] = self.book[book::SERVICE_CREDIT].wrapping_add(d);
                    self.book[book::TOTAL_CREDIT] = self.book[book::TOTAL_CREDIT].wrapping_add(d);
                    self.book_dirty = true;
                    self.nodes[node].service_fired[k] = true;
                }
            } else {
                self.nodes[node].service_held[k] = false;
                self.nodes[node].service_fired[k] = false;
            }
        }
    }
}
