//! The libam credit module: the port amdaemon's credit controller reaches it through, the values the port
//! returns and the part of libam's `am::AppConfig` the controller sets.

use crate::result::AmResult;

/// Number of players the credit module publishes.
pub const PLAYERS: usize = 8;

/// Value pair of a libam player node (`CreditValue`, accessors `0x140223350` / `0x140223360`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayerCredit {
    /// Credit the player holds.
    pub credit: u32,
    /// Coins still needed for the next credit.
    pub remain: u32,
}

/// Aggregate counters of the libam bookkeeping (`0x140223600`..`0x140223650`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BookkeepingTotals {
    /// Libam `+0x20` (`0x140223600`).
    pub total_coin: u32,
    /// Libam `+0x28` (`0x140223620`).
    pub coin_credit: u32,
    /// Libam `+0x2C` (`0x140223630`).
    pub service_credit: u32,
    /// Libam `+0x24` (`0x140223610`).
    pub e_money_coin: u32,
    /// Libam `+0x30` (`0x140223640`).
    pub e_money_credit: u32,
    /// Libam `+0x34` (`0x140223650`).
    pub total_credit: u32,
}

/// One libam insert-coin event (`am::credit::InsertCoinEvent`, 12 bytes: `u32 kindFlag; bool player[8]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InsertCoinEvent {
    /// Libam `kindFlag` (`nodeEvent.kind != 1`): the event came from the service switch, not a coin chute.
    pub service: bool,
    /// Players the event applies to. Libam sets every player that shares the chute (common chute) or only
    /// the player whose node raised the event.
    pub players: [bool; PLAYERS],
}

/// The libam credit module as used by amdaemon's credit controller.
///
/// Method docs give the facade wrapper VA in `0x140216xxx`. Every query goes through the handle guard in
/// the original; [`Self::handle`] models that guard, which the controller evaluates once per loader.
pub trait CreditPort {
    /// Handle guard (`0x140218B70` / `0x140218BC0`): `Err` (code 2) while the libam global is not
    /// initialised.
    fn handle(&self) -> AmResult<()>;
    /// `credit.isEnabled()` (`0x1402162E0`, libam byte `credit+8`).
    fn is_enabled(&self) -> bool;
    /// `credit.specialDevice.isAvailable()` (`0x140216440`).
    fn special_device_available(&self) -> bool;
    /// Config `chuteShareType` (`0x14021D1A0`).
    fn config_chute_share_type(&self) -> u32;
    /// Config `serviceShareType` (`0x14021D1B0`).
    fn config_service_share_type(&self) -> u32;
    /// Config `operation` (`0x14021D1C0`).
    fn config_operation(&self) -> u32;
    /// Config multiplier `index` (`0x14021D1D0`); `Err` for an index outside the table.
    fn config_multiplier(&self, index: u32) -> AmResult<u32>;
    /// Config bonus adder (`0x14021D280`).
    fn config_bonus_adder(&self) -> u32;
    /// Config coin to credit (`0x14021D270`).
    fn config_coin_to_credit(&self) -> u32;
    /// Config game cost `index` (`0x14021D290`); `Err` for an index outside the table.
    fn config_game_cost(&self, index: u32) -> AmResult<u32>;
    /// Config coin amount (`0x14021D350`).
    fn config_coin_amount(&self) -> u32;
    /// `credit.player(index)` value (`0x140216320` → `0x140223350` / `0x140223360`); `None` when the lookup
    /// fails or the player has no node.
    fn player_value(&self, index: u32) -> Option<PlayerCredit>;
    /// `credit.addableCoin(index)` (`0x1402163C0`); `0` when the player has no node.
    fn addable_coin(&self, index: u32) -> u32;
    /// Bookkeeping coin counter `index` (`0x140223540`); `Err` for index ≥ 8.
    fn bookkeeping_coin(&self, index: u32) -> AmResult<u32>;
    /// Bookkeeping aggregate counters (`0x140216400` and its getters).
    fn bookkeeping_totals(&self) -> BookkeepingTotals;
    /// Special device lockout state (`0x140216480`).
    fn lockout_on(&self) -> bool;
    /// Whether the special device may change its lockout now (`0x140216460`).
    fn can_lockout(&self) -> bool;
    /// Coin-in-ignore flag (`0x140216420`).
    fn coin_in_ignored(&self) -> bool;

    /// `credit.clear()` (`0x140216250`).
    fn clear(&mut self);
    /// `credit.pay(player, gameCostIndex, count)` (`0x1402161B0`); libam failure is `Result(6, "Failed to
    /// use credit.")`.
    fn pay(&mut self, player_index: u32, game_cost_index: u16, count: u16) -> AmResult<()>;
    /// `credit.specialLockout(on)` (`0x140216290`); libam failure is `Result` code 2.
    fn special_lockout(&mut self, on: bool) -> AmResult<()>;
    /// `credit.setCoinInIgnore(flag)` (`0x140216270`).
    fn set_coin_in_ignore(&mut self, ignore: bool);

    /// Insert-coin events libam delivered to the installed `InsertCoinEventListener` since the last call,
    /// oldest first. They fire inside `Manager::execute`, before every `preExecute`.
    fn take_events(&mut self) -> Vec<InsertCoinEvent>;
}

/// Part of libam `am::AppConfig` touched by the credit controller (`AppConfig+0x1F58`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CreditAppConfig {
    /// `credit.insertCoinListener`: the controller's `CreditEventListener` (`controller+0x88`) is installed.
    pub insert_coin_listener: bool,
}
