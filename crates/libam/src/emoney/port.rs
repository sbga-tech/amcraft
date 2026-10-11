//! Port handles of the emoney module.

use libam_api::emoney::{EMoneyBrandInfo, EMoneyDealInfo, EMoneyEvent, EMoneyPort, LibamBrand};
use libam_api::result::AmResult;

use super::{CoinGate, EMoney};
use crate::core::Core;
use crate::nvram::NvramContext;
use crate::shared::Shared;
use crate::util::wide_array;

/// `EMoneyPort` handle over the shared libam state.
#[derive(Debug)]
pub struct EMoneyHandle(pub(crate) Shared);

impl EMoneyHandle {
    /// Runs a facade operation: the display state the gates read is refreshed first, the display operations the
    /// operation queued are applied afterwards (the original writes the screen directly).
    fn operate<R>(&self, f: impl FnOnce(&mut EMoney, &NvramContext) -> R) -> R {
        self.0.with(|core| {
            sync_display(core);
            let result = f(&mut core.emoney, &core.nvram);
            flush_display(core);
            result
        })
    }

    fn query<R>(&self, f: impl FnOnce(&EMoney) -> R) -> R {
        self.0.read(|core| f(&core.emoney))
    }
}

/// The display module's state as the e-money gates see it (`0x14022DF70`, `0x14022E010`).
pub(crate) fn sync_display(core: &mut Core) {
    let (ready, busy) = (core.display.ready(), core.display.is_mode_a());
    core.emoney.set_display_state(ready, busy);
}

/// Applies the screen operations the e-money module queued to the display module.
pub(crate) fn flush_display(core: &mut Core) {
    for op in core.emoney.take_display_ops() {
        op.apply(&mut core.display);
    }
}

impl EMoneyPort for EMoneyHandle {
    fn handle(&self) -> AmResult<()> {
        self.0.guard()
    }

    fn shared_handle(&self) -> AmResult<()> {
        self.0.guard()
    }

    fn is_enabled(&self) -> bool {
        self.query(EMoney::is_enabled)
    }

    fn terminal_id(&self) -> Option<[u16; 31]> {
        self.query(|emoney| emoney.terminal_strings().map(|(id, _)| wide_array(&id)))
    }

    fn terminal_serial(&self) -> Option<[u16; 31]> {
        self.query(|emoney| emoney.terminal_strings().map(|(_, serial)| wide_array(&serial)))
    }

    fn brand_count(&self) -> usize {
        self.query(EMoney::brand_count)
    }

    fn brand(&self, index: usize) -> Option<LibamBrand> {
        self.query(|emoney| emoney.brand_at(index))
    }

    fn brand_info(&self, brand: LibamBrand) -> EMoneyBrandInfo {
        self.query(|emoney| emoney.brand_record(brand))
    }

    fn deal_log_count(&self) -> usize {
        self.query(EMoney::deal_log_count)
    }

    fn deal_log_item(&self, index: usize) -> Option<EMoneyDealInfo> {
        self.query(|emoney| emoney.deal_log_item(index))
    }

    fn deal_available(&self) -> bool {
        self.0.with(|core| {
            sync_display(core);
            core.emoney.deal_available()
        })
    }

    /// The wrapper ignores the caller's task and evaluates task 4 (`0x140216800` loads `edx = 4`).
    fn can_operate(&self, _task: u32) -> bool {
        self.0.with(|core| {
            sync_display(core);
            core.emoney.can_operate()
        })
    }

    fn alive(&self) -> bool {
        self.query(EMoney::alive)
    }

    fn display_start_test(&mut self) -> AmResult<()> {
        self.0.guard()?;
        self.0.with(|core| {
            let now = core.clock.tick_ms();
            core.display.start_test(now)
        })
    }

    fn display_cancel(&mut self) {
        self.0.with(|core| core.display.cancel_test());
    }

    fn cancel(&mut self) {
        self.operate(|emoney, _| emoney.cancel());
    }

    fn terminal_auth(&mut self) -> AmResult<()> {
        self.0.guard()?;
        self.operate(|emoney, _| emoney.terminal_auth())
    }

    fn terminal_remove(&mut self) -> AmResult<()> {
        self.0.guard()?;
        self.operate(|emoney, _| emoney.terminal_remove())
    }

    fn balance(&mut self, brand: LibamBrand) -> AmResult<()> {
        self.0.guard()?;
        self.operate(|emoney, _| emoney.balance(brand))
    }

    fn amount_pay(&mut self, amount: i32, item_count: u32, brand: LibamBrand, item_id: &[u16]) -> AmResult<()> {
        self.0.guard()?;
        self.operate(|emoney, _| emoney.amount_pay(amount, item_count, brand, item_id))
    }

    fn coin_pay(&mut self, player_index: u32, coin: u32, brand: LibamBrand) -> AmResult<()> {
        self.0.guard()?;
        self.0.with(|core| {
            sync_display(core);
            let rule = core.credit.rule_block();
            let gate = CoinGate {
                test_mode: core.sequence.is_test(),
                ignore_coin: core.credit.ignore_coin(),
                addable: core.credit.addable(player_index),
                multiplier: rule.coin_chute_multiplier[0],
                coin_amount: rule.coin_amount,
            };
            let result = core.emoney.coin_pay(player_index, coin, brand, &gate);
            flush_display(core);
            result
        })
    }

    fn complete_pay(&mut self) -> AmResult<()> {
        self.0.guard()?;
        self.operate(|emoney, nvram| emoney.complete_pay(nvram))
    }

    fn take_events(&mut self) -> Vec<EMoneyEvent> {
        self.0.with(|core| core.emoney.take_events())
    }
}
