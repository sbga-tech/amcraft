//! Section `credit` (`Config+0x4FEC0`, `spec/libam/config.md` Part E §1).

use super::json::Map;
use super::reader::{Obj, Res};

/// "Not given" marker of `max_credit` and of the `coin_box_assign` slots.
pub const UNSET: u32 = 0xFFFF_FFFF;

/// `credit.coin_selector_AS6DB`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CoinSelectorAs6db {
    /// `enable`: `true` when the object exists without the key.
    pub enable: bool,
    /// `port`, `1..=256` (required while enabled).
    pub port: u16,
    /// `counted_coin`, one code per coin slot: `0`, `10`, `100`, `500` yen map to `0..=3`.
    pub counted_coin: Vec<u32>,
}

/// Section `credit`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credit {
    pub enable: bool,
    /// `max_credit`: `1..=99`, or [`UNSET`].
    pub max_credit: u32,
    /// `coin_box_assign`: `0..=3` per player, [`UNSET`] when not given (the prelude fills the players' slots).
    pub coin_box_assign: [u32; 8],
    /// `config.coin_chute_type_common`; `None` when the key is absent.
    pub coin_chute_type_common: Option<bool>,
    /// `config.service_type_common`.
    pub service_type_common: Option<bool>,
    /// `config.freeplay`.
    pub freeplay: Option<bool>,
    /// `config.coin_chute_multiplier`: `1..=9` per chute, `-1` when not given.
    pub coin_chute_multiplier: [i32; 2],
    /// `config.bonus_adder`: `0..=9`, `-1` when not given.
    pub bonus_adder: i32,
    /// `config.coin_to_credit`: `1..=99`, `-1` when not given.
    pub coin_to_credit: i32,
    /// `config.game_cost`: `1..=99` per game mode, `-1` when not given.
    pub game_cost: [i32; 8],
    pub as6db: CoinSelectorAs6db,
}

impl Default for Credit {
    fn default() -> Self {
        Self {
            enable: false,
            max_credit: UNSET,
            coin_box_assign: [UNSET; 8],
            coin_chute_type_common: None,
            service_type_common: None,
            freeplay: None,
            coin_chute_multiplier: [-1; 2],
            bonus_adder: -1,
            coin_to_credit: -1,
            game_cost: [-1; 8],
            as6db: CoinSelectorAs6db::default(),
        }
    }
}

/// `-1` exactly, or a value in `lo..=hi`; the stored integer is the truncating cast.
fn minus_one_or(v: f64, lo: f64, hi: f64) -> Result<i32, &'static str> {
    if v == -1.0 {
        Ok(-1)
    } else if (lo..=hi).contains(&v) {
        Ok(v as i32)
    } else {
        Err("out of range")
    }
}

impl Credit {
    /// `max_player` is `common.max_player` after its own validation (`0x14014A150`: the size of
    /// `coin_box_assign` must equal it).
    pub(super) fn parse(root: &Map, max_player: u32) -> Res<Self> {
        let mut out = Self::default();
        let Some(section) = Obj::section(root, "credit")? else { return Ok(out) };
        out.enable = section.enable()?;
        if !out.enable {
            return Ok(out);
        }

        if let Some(v) = section.number_with("max_credit", |v| minus_one_or(v, 1.0, 99.0).map(|i| i as u32))? {
            out.max_credit = v;
        }
        if let Some(list) = section.array("coin_box_assign")? {
            if list.len() != max_player as usize {
                return Err(list.bad("unmatch coin box assign list size."));
            }
            for i in 0..list.len() {
                let v = list.number(i)?;
                if !(0.0..4.0).contains(&v) {
                    return Err(list.bad_at(i, "out of range."));
                }
                out.coin_box_assign[i] = v as u32;
            }
        }
        if let Some(config) = section.object("config")? {
            out.parse_config(&config)?;
        }
        if let Some(selector) = section.object("coin_selector_AS6DB")? {
            out.as6db = CoinSelectorAs6db::parse(&selector)?;
        }
        Ok(out)
    }

    fn parse_config(&mut self, config: &Obj<'_>) -> Res {
        self.coin_chute_type_common = config.bool("coin_chute_type_common")?;
        self.service_type_common = config.bool("service_type_common")?;
        self.freeplay = config.bool("freeplay")?;
        if let Some(list) = config.array("coin_chute_multiplier")? {
            if list.len() > 2 {
                return Err(list.bad("too many coin chute multipliers."));
            }
            for i in 0..list.len() {
                self.coin_chute_multiplier[i] =
                    minus_one_or(list.number(i)?, 1.0, 9.0).map_err(|_| list.bad_at(i, "out of range."))?;
            }
        }
        if let Some(v) = config.number_with("coin_to_credit", |v| minus_one_or(v, 1.0, 99.0))? {
            self.coin_to_credit = v;
        }
        if let Some(v) = config.number_with("bonus_adder", |v| minus_one_or(v, 0.0, 9.0))? {
            self.bonus_adder = v;
        }
        if let Some(list) = config.array("game_cost")? {
            if list.len() > 8 {
                return Err(list.bad("too many game costs."));
            }
            for i in 0..list.len() {
                self.game_cost[i] =
                    minus_one_or(list.number(i)?, 1.0, 99.0).map_err(|_| list.bad_at(i, "out of range."))?;
            }
        }
        Ok(())
    }

    /// The prelude of the loader (`0x14008DCC3`): with the section enabled and players configured, a slot that
    /// was not given is spread over the four coin boxes.
    pub(super) fn fill_coin_box_assign(&mut self, max_player: u32) {
        if !self.enable || max_player == 0 {
            return;
        }
        let divisor = ((max_player - 1) >> 2) + 1;
        for i in 0..max_player.min(8) {
            let slot = &mut self.coin_box_assign[i as usize];
            if *slot == UNSET {
                *slot = i / divisor;
            }
        }
    }
}

impl CoinSelectorAs6db {
    fn parse(selector: &Obj<'_>) -> Res<Self> {
        let mut out = Self { enable: selector.enable()?, ..Self::default() };
        if !out.enable {
            return Ok(out);
        }
        out.port = selector.require_number_with("port", |v| {
            if (1.0..=256.0).contains(&v) { Ok(v as u16) } else { Err("out of range") }
        })?;
        let coins = selector.require_array("counted_coin")?;
        if coins.len() == 0 {
            return Err(coins.bad("counted coin array is empty."));
        }
        if coins.len() > 2 {
            return Err(coins.bad("too many counted coin values."));
        }
        for i in 0..coins.len() {
            out.counted_coin.push(match coins.number(i)? {
                0.0 => 0,
                10.0 => 1,
                100.0 => 2,
                500.0 => 3,
                _ => return Err(coins.bad_at(i, "invalid coin value.")),
            });
        }
        Ok(out)
    }
}
