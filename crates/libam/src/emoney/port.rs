//! The e-money facade: the port amdaemon's e-money controller reaches the e-money module and its display module
//! through ([`EMoneyPort`]), the values and events the port returns, the parts of amdaemon's process configuration
//! and `am::AppConfig` the controller touches, and libam's [`EMoneyHandle`].

use super::{CoinGate, EMoney};
use crate::core::Core;
use crate::nvram::NvramContext;
use crate::result::AmResult;
use crate::shared::Shared;
use crate::time::Tm;
use crate::util::wide_array;

/// `am::emoney::Brand`: libam's brand numbering (`1..=7`, `0` = none). amdaemon's own brand ids order the
/// brands differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LibamBrand(pub u32);

impl LibamBrand {
    /// No brand (`0`): libam rejects it for every brand operation.
    pub const NONE: Self = Self(0);
}

/// A fixed libam string accessor: `None` when the array is empty (`wcsnlen != 0`, `0x1402284E0`).
pub fn non_empty(units: &[u16]) -> Option<&[u16]> {
    units.first().is_some_and(|&unit| unit != 0).then_some(units)
}

/// libam `am::emoney::BrandInfo` (stride `0x298` in `root+0x36AC0`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EMoneyBrandInfo {
    /// `+0x000` libam brand id.
    pub id: u32,
    /// `+0x004` brand name, NUL-terminated.
    pub name: [u16; 65],
    /// `+0x086` icon file path, NUL-terminated.
    pub icon: [u16; 260],
    /// `+0x28E` `balanceFlag`.
    pub balance_flag: u8,
}

impl EMoneyBrandInfo {
    /// Builds a record from text (names longer than the arrays are cut).
    pub fn new(id: u32, name: &str, icon: &str, balance_flag: u8) -> Self {
        let mut info = Self { id, name: [0; 65], icon: [0; 260], balance_flag };
        for (slot, unit) in info.name.iter_mut().take(64).zip(name.encode_utf16()) {
            *slot = unit;
        }
        for (slot, unit) in info.icon.iter_mut().take(259).zip(icon.encode_utf16()) {
            *slot = unit;
        }
        info
    }

    /// Brand name; `None` when empty (`0x1402241B0`).
    pub fn name(&self) -> Option<&[u16]> {
        non_empty(&self.name)
    }

    /// Icon file path; `None` when empty (`0x1402241E0`).
    pub fn icon_file_path(&self) -> Option<&[u16]> {
        non_empty(&self.icon)
    }
}

/// libam `am::emoney::DealInfo` (0x88 bytes, `0x140223B20`..`0x140223BD0`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EMoneyDealInfo {
    /// `+0x00` libam deal status (`0` .. `4`).
    pub status: u32,
    /// `+0x04` libam brand.
    pub brand: LibamBrand,
    /// `+0x08` deal time.
    pub time: Tm,
    /// `+0x2C` deal number, NUL-terminated.
    pub deal_number: [u16; 16],
    /// `+0x4C` card number, NUL-terminated.
    pub card_number: [u16; 21],
    /// `+0x78` amount.
    pub amount: i32,
    /// `+0x7C` balance before the deal.
    pub balance_before: i32,
    /// `+0x80` balance after the deal.
    pub balance_after: i32,
}

/// Events libam delivers to the five `EMoneyEventListener`s the controller installs
/// (`credit_emoney_aimepay.emoney.appconfig_callback`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EMoneyEvent {
    /// `CompleteTaskEventListener` (`0x14002EC50`): libam finished a task.
    CompleteTask {
        /// libam task id.
        task: u32,
        /// libam status of the completed task (`1` and `3` are errors).
        status: u32,
    },
    /// `NotifyDealResultEventListener` (`0x14002ECC0`): a deal result is available.
    NotifyDealResult {
        /// libam task id.
        task: u32,
        /// The deal.
        deal: EMoneyDealInfo,
    },
    /// `PlaySoundEventListener` (`0x14002ED40`): queue a sound file (`None` is queued as the empty path).
    PlaySound {
        /// Sound file path in UTF-16 code units, without terminator.
        path: Option<Vec<u16>>,
    },
    /// `NotifyCancelEventListener` (`0x14002EDF0`): the running operation became (un)cancellable.
    NotifyCancel {
        /// New value of `operation.cancellable`.
        cancellable: bool,
    },
    /// `HoldCardEventListener` (`0x14002EE10`): the card has to be held over the reader.
    HoldCard,
}

/// The libam e-money module and display module as used by amdaemon's e-money controller.
///
/// Method docs give the facade wrapper VA in `0x140216xxx`.
pub trait EMoneyPort {
    /// Handle guard (`0x140219430`): `Err` (code 2) while the libam global is not initialised.
    fn handle(&self) -> AmResult<()>;
    /// Second handle guard (`0x140218F30`, token `bundle+17`) checked by `accept`.
    fn shared_handle(&self) -> AmResult<()>;
    /// `emoney.isEnabled()` (`0x140216740`, libam byte `emoney+0x30`).
    fn is_enabled(&self) -> bool;
    /// `emoney.terminalId()` (`0x1402167A0` → `0x14022FFA0`): the terminal id at `terminal+0` of the terminal
    /// record (`emoney+4016`); `None` unless the module has storage ready or the id is empty. A `wchar_t[31]`,
    /// NUL-terminated or filling the array: the record initialiser `0x140270190` fills it with
    /// `wcscpy_s(rec+0, 0x1F, ..)` and the serial starts `62` bytes (31 units) behind it.
    fn terminal_id(&self) -> Option<[u16; 31]>;
    /// `emoney.terminalSerial()` (`0x1402167C0` → `0x14022FFF0`): the terminal serial at `terminal+62` (`emoney+4078`);
    /// `None` under the same conditions. A `wchar_t[31]`, NUL-terminated or filling the array: `0x140270190` and the
    /// record setter `0x140270430` fill it with `wcscpy_s(rec+62, 0x1F, ..)`, and the next member (a `wchar_t[260]`
    /// path) starts `62` bytes behind it at `+124`.
    fn terminal_serial(&self) -> Option<[u16; 31]>;
    /// `emoney.brandCount()` (`0x140216840`).
    fn brand_count(&self) -> usize;
    /// `emoney.brand(index)` (`0x140216860`); `None` when the lookup fails.
    fn brand(&self, index: usize) -> Option<LibamBrand>;
    /// `emoney.brandInfo(brand)` (`0x140216820` → `0x140266890`, stride `0x298` = 664 bytes from `root+0x36AC0`) for
    /// a brand in `1..=7`: a copy of libam's record.
    fn brand_info(&self, brand: LibamBrand) -> EMoneyBrandInfo;
    /// Deal log entry count (`0x140223F30`, ring of 8).
    fn deal_log_count(&self) -> usize;
    /// Deal log entry `index`, newest first (`0x140223F40`); `None` when the lookup fails.
    fn deal_log_item(&self, index: usize) -> Option<EMoneyDealInfo>;
    /// `emoney.dealAvailable()` (`0x140216760`).
    fn deal_available(&self) -> bool;
    /// `emoney.canOperate(task)` (`0x140216800`).
    fn can_operate(&self, task: u32) -> bool;
    /// `emoney.alive()` (`0x1402167E0`): three libam flags are all set.
    fn alive(&self) -> bool;

    /// `display.startTest()` (`0x1402164C0`); fails with code 6 (module disabled) or 3 (busy).
    fn display_start_test(&mut self) -> AmResult<()>;
    /// `display.cancel()` (`0x1402164E0`).
    fn display_cancel(&mut self);
    /// `emoney.cancel()` (`0x1402166D0`).
    fn cancel(&mut self);
    /// `emoney.terminalAuth()` (`0x1402166F0`; libam also receives `root+0x659C`, purpose unresolved).
    fn terminal_auth(&mut self) -> AmResult<()>;
    /// `emoney.terminalRemove()` (`0x140216720`).
    fn terminal_remove(&mut self) -> AmResult<()>;
    /// `emoney.balance(brand)` (`0x140216650`); libam answers code 1 for no brand or a brand without balance.
    fn balance(&mut self, brand: LibamBrand) -> AmResult<()>;
    /// `emoney.amountPay(amount, itemCount, brand, itemId)` (`0x140216670`); libam requires a brand and a
    /// 13-character item id (code 1 otherwise). `item_id` is the id without its terminator.
    fn amount_pay(&mut self, amount: i32, item_count: u32, brand: LibamBrand, item_id: &[u16]) -> AmResult<()>;
    /// `emoney.coinPay(player, coin, brand)` (`0x140216690`); libam applies the test-mode / coin-in-ignore /
    /// credit-limit gates (codes 1 and 2).
    fn coin_pay(&mut self, player_index: u32, coin: u32, brand: LibamBrand) -> AmResult<()>;
    /// `emoney.completePay()` (`0x1402166B0`).
    fn complete_pay(&mut self) -> AmResult<()>;

    /// Events libam delivered to the installed listeners since the last call, in firing order. They fire
    /// inside `Manager::execute`, before every `preExecute`.
    fn take_events(&mut self) -> Vec<EMoneyEvent>;
}

/// Part of `amdaemon::process::Config` read by the EMoney config observer (`EMoneyConfig+408`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EMoneyProcessConfig {
    /// `aime_unit` (key inferred from the offset): copied to `SD.eMoney.aimeUnitIndex`.
    pub aime_unit: u8,
}

/// Part of libam `am::AppConfig` touched by the EMoney controller (`AppConfig+0x2F38+0x630..0x650`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EMoneyAppConfig {
    /// `CompleteTaskEventListener` (`controller+0x80`) installed.
    pub complete_task_listener: bool,
    /// `NotifyDealResultEventListener` (`controller+0x88`) installed.
    pub notify_deal_result_listener: bool,
    /// `PlaySoundEventListener` (`controller+0x90`) installed.
    pub play_sound_listener: bool,
    /// `NotifyCancelEventListener` (`controller+0x98`) installed.
    pub notify_cancel_listener: bool,
    /// `HoldCardEventListener` (`controller+0xA0`) installed.
    pub hold_card_listener: bool,
}

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
