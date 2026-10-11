//! Port handles of the credit module.

use libam_api::credit::{BookkeepingTotals, CreditPort, InsertCoinEvent, PlayerCredit};
use libam_api::result::{AmError, AmResult};

use super::PAY_FAILED;
use crate::shared::Shared;

/// `CreditPort` handle over the shared libam state.
#[derive(Debug)]
pub struct CreditHandle(pub(crate) Shared);

impl CreditPort for CreditHandle {
    fn handle(&self) -> AmResult<()> {
        self.0.guard()
    }

    fn is_enabled(&self) -> bool {
        self.0.read(|core| core.credit.enabled())
    }

    fn special_device_available(&self) -> bool {
        self.0.read(|core| core.credit.special_device_available())
    }

    fn config_chute_share_type(&self) -> u32 {
        self.0.read(|core| core.credit.rule_block().chute_share_type)
    }

    fn config_service_share_type(&self) -> u32 {
        self.0.read(|core| core.credit.rule_block().service_share_type)
    }

    fn config_operation(&self) -> u32 {
        self.0.read(|core| core.credit.rule_block().operation)
    }

    fn config_multiplier(&self, index: u32) -> AmResult<u32> {
        self.0.guard()?;
        self.0.read(|core| {
            let index = usize::try_from(index).ok();
            index
                .and_then(|index| core.credit.rule_block().coin_chute_multiplier.get(index).copied())
                .ok_or_else(|| AmError::code(AmError::INVALID_ARGUMENT))
        })
    }

    fn config_bonus_adder(&self) -> u32 {
        self.0.read(|core| core.credit.rule_block().bonus_adder)
    }

    fn config_coin_to_credit(&self) -> u32 {
        self.0.read(|core| core.credit.rule_block().coin_to_credit)
    }

    fn config_game_cost(&self, index: u32) -> AmResult<u32> {
        self.0.guard()?;
        self.0.read(|core| {
            let index = usize::try_from(index).ok();
            index
                .and_then(|index| core.credit.rule_block().game_cost.get(index).copied())
                .ok_or_else(|| AmError::code(AmError::INVALID_ARGUMENT))
        })
    }

    fn config_coin_amount(&self) -> u32 {
        self.0.read(|core| core.credit.rule_block().coin_amount)
    }

    fn player_value(&self, index: u32) -> Option<PlayerCredit> {
        self.0.read(|core| core.credit.player_value(index))
    }

    fn addable_coin(&self, index: u32) -> u32 {
        self.0.read(|core| core.credit.addable(index))
    }

    fn bookkeeping_coin(&self, index: u32) -> AmResult<u32> {
        self.0.guard()?;
        self.0.read(|core| core.credit.bookkeeping_coin(index)).ok_or_else(|| AmError::code(AmError::INVALID_ARGUMENT))
    }

    fn bookkeeping_totals(&self) -> BookkeepingTotals {
        self.0.read(|core| core.credit.bookkeeping_totals())
    }

    fn lockout_on(&self) -> bool {
        self.0.read(|core| core.credit.lockout_on())
    }

    fn can_lockout(&self) -> bool {
        self.0.read(|core| core.credit.can_lockout())
    }

    fn coin_in_ignored(&self) -> bool {
        self.0.read(|core| core.credit.ignore_coin())
    }

    fn clear(&mut self) {
        self.0.with(|core| core.credit.clear());
    }

    fn pay(&mut self, player_index: u32, game_cost_index: u16, count: u16) -> AmResult<()> {
        self.0.guard()?;
        self.0.with(|core| {
            // The root gate of `0x140253640`: advertise or play, no pending root error.
            if !core.sequence.is_game() || core.error.current().number != 0 {
                return Err(AmError::with_message(AmError::FAILED, PAY_FAILED));
            }
            let Ok(cost_index) = i16::try_from(game_cost_index) else {
                return Err(AmError::with_message(AmError::FAILED, PAY_FAILED));
            };
            core.credit.pay(player_index, cost_index, count)
        })
    }

    fn special_lockout(&mut self, on: bool) -> AmResult<()> {
        self.0.guard()?;
        if self.0.with(|core| core.credit.special_lockout(on)) {
            Ok(())
        } else {
            Err(AmError::code(AmError::NOT_INITIALIZED))
        }
    }

    fn set_coin_in_ignore(&mut self, ignore: bool) {
        self.0.with(|core| core.credit.set_coin_in_ignore(ignore));
    }

    fn take_events(&mut self) -> Vec<InsertCoinEvent> {
        self.0.with(|core| core.credit.take_events())
    }
}
