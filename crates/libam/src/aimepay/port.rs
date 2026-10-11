//! The Aime Pay facade: the port amdaemon's Aime Pay controller reaches the Aime Pay module and its display module
//! through ([`AimePayPort`]), the values and events the port returns, the part of `am::AppConfig` the controller
//! sets, and libam's [`AimePayHandle`]. [`AccessCode`] is shared with the Aime card reader module.

use super::drive;
use crate::aime::AccessCode;
use crate::result::AmResult;
use crate::shared::Shared;
use crate::time::Tm;
use crate::util::wide_to_string;

/// `am::aimepay::AimeId`: `{u32 id, u8 flag}`. amdaemon always builds it from a request id with flag 0
/// (`0x140220A10(id, 0)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AimeId {
    /// Aime id.
    pub id: u32,
    /// Second constructor argument (meaning unresolved), always 0 here.
    pub flag: u8,
}

/// libam `am::aimepay::DealLog` entry (`root+0x4BEF8` ring of 5; accessors `0x140221B80`..`0x140221C50`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AimePayDeal {
    /// `+0x00` libam deal status (`1` marks an error).
    pub status: u32,
    /// `+0x04` deal time.
    pub time: Tm,
    /// `+0x28` libam error code.
    pub error_code: u32,
    /// `+0x2C` libam error message, NUL-terminated.
    pub error_message: [u16; 16],
    /// `+0x4C` item count.
    pub item_count: u32,
    /// `+0x50` item id, NUL-terminated.
    pub item_id: [u16; 14],
    /// `+0x6C` packed-BCD access code.
    pub access_code: [u8; 10],
    /// `+0xA8` receipt id.
    pub receipt_id: u64,
    /// `+0xB0` amount (`0xFFFFFFFF` when unknown).
    pub amount: u32,
    /// `+0xB4` the deal was a coin payment.
    pub is_coin: bool,
}

/// libam `am::aimepay::DealSummary` entry (ring of 7; accessors `0x140222180`..`0x1402221A0`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AimePaySummaryItem {
    /// `+0x00` day.
    pub date: Tm,
    /// `+0x24` number of deals.
    pub count: u32,
    /// `+0x28` total amount.
    pub amount: u32,
}

/// libam `am::aimepay::Location` as seen by `convertLoc` (`0x14001AF60`): `wchar_t[11]` at `+0x00`, the name
/// `wchar_t[21]` at `+0x16`, `wchar_t[11]` at `+0x40` and the company name `wchar_t[41]` at `+0x56`, the four
/// `wcsnlen` bounds of the validity check `0x1402226A0`. The texts are NUL-terminated or fill their array.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AimePayLocation {
    /// All four libam fields (ids, name, company) are non-empty (`0x1402226A0`).
    pub valid: bool,
    /// Location name (`+0x16`, `wchar_t[21]`: bound of `0x140222740`); `None` when empty.
    pub name: Option<[u16; 21]>,
    /// Company name (`+0x56`, `wchar_t[41]`: bound of `0x1402227A0`); `None` when empty.
    pub company_name: Option<[u16; 41]>,
}

/// libam `am::aimepay::UserSetting` (`0x140215340` → `0x140239B20`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AimePayUserSetting {
    /// Aime id (`0x1402209D0`).
    pub aime_id: u32,
    /// Credit card payment available.
    pub credit_card_available: bool,
    /// Aime hold step can be skipped.
    pub aime_hold_skip: bool,
}

/// Events libam delivers to the three `AimePayEventListener`s the controller installs
/// (`credit_emoney_aimepay.aimepay.appconfig_callback`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AimePayEvent {
    /// `CompleteTaskEventListener` (`0x140019D00`): an asynchronous libam task finished.
    CompleteTask {
        /// libam task id.
        task: u32,
        /// First dword of libam's `ErrorInfo` (`1` marks an error).
        error_kind: u32,
    },
    /// `NotifyDealResultEventListener` (`0x140019ED0`): a deal result is available.
    NotifyDealResult {
        /// libam task id (not read by the listener).
        task: u32,
        /// The deal.
        deal: AimePayDeal,
    },
    /// `NeedAcceptationEventListener` (`0x140019F20`): libam waits for the user's confirmation.
    NeedAcceptation {
        /// libam `Question` (`1` or `2`).
        question: u32,
    },
}

/// The libam Aime Pay module and display module as used by amdaemon's Aime Pay controller.
///
/// Method docs give the facade wrapper VA in `0x1402151xx`–`0x1402153xx` / `0x1402164xx`.
pub trait AimePayPort {
    /// Handle guard (`0x1402194D0`): `Err` (code 2) while the libam global is not initialised.
    fn handle(&self) -> AmResult<()>;
    /// Second handle guard (`0x140218F30`, token `bundle+17`) checked by `accept`.
    fn shared_handle(&self) -> AmResult<()>;
    /// `aimepay.isEnabled()` (`0x1402152C0`, libam byte `aimepay+0x20`).
    fn is_enabled(&self) -> bool;
    /// Deal log entry count (`0x140222000`, ring of 5).
    fn deal_log_count(&self) -> usize;
    /// Deal log entry `index`, newest first (`0x140222010`); `None` when the lookup fails.
    fn deal_log_item(&self, index: usize) -> Option<AimePayDeal>;
    /// `aimepay.activatedLocation()` (`0x140215360`, `root+0x4BE50`): a copy of the location.
    fn activated_location(&self) -> AimePayLocation;
    /// `aimepay.currentLocation()` (`0x140215380`): a copy of the location; empty unless the module state is 2 or 3.
    fn current_location(&self) -> AimePayLocation;
    /// `aimepay.dealAvailable()` (`0x140215320`).
    fn deal_available(&self) -> bool;
    /// Deal summary entry count (`0x140222360`, ring of 7).
    fn deal_summary_count(&self) -> usize;
    /// Deal summary entry `index`, newest first (`0x140222370`); `None` when the lookup fails.
    fn deal_summary_item(&self, index: usize) -> Option<AimePaySummaryItem>;
    /// `aimepay.userSetting()` (`0x140215340`); `None` when libam's setting is not valid.
    fn user_setting(&self) -> Option<AimePayUserSetting>;

    /// `display.startTest()` (`0x1402164C0`); fails with code 6 (module disabled) or 3 (busy).
    fn display_start_test(&mut self) -> AmResult<()>;
    /// `display.cancel()` (`0x1402164E0`).
    fn display_cancel(&mut self);
    /// `aimepay.cancel()` (`0x1402152A0`).
    fn cancel(&mut self);
    /// `aimepay.confirmWithParam(param)` (`0x140215230`); `param` is the parameter without its terminator.
    fn confirm_with_param(&mut self, param: &[u16]) -> AmResult<()>;
    /// `aimepay.confirmAccept()` (`0x140215280`); the original ignores any result.
    fn confirm_accept(&mut self);
    /// `aimepay.activate()` (`0x1402151F0`): needs module state 2 or 3, enabled and no error (code 6), not
    /// busy (code 3).
    fn activate(&mut self) -> AmResult<()>;
    /// `aimepay.deactivate()` (`0x140215210`).
    fn deactivate(&mut self) -> AmResult<()>;
    /// `aimepay.queryUserSetting(aimeId)` (`0x140215190`).
    fn query_user_setting(&mut self, aime_id: AimeId) -> AmResult<()>;
    /// `aimepay.coinPay(aimeId, accessCode, player, coin)` (`0x1402151B0`). `access_code` is
    /// [`AccessCode::UNSET`] when the request's code is not valid BCD: the original forwards it anyway.
    fn coin_pay(&mut self, aime_id: AimeId, access_code: &AccessCode, player_index: u32, coin: u32) -> AmResult<()>;
    /// `aimepay.amountPay(aimeId, accessCode, itemId, itemCount, optionNumber)` (`0x1402151D0`). `item_id` is
    /// the id without its terminator; `access_code` is [`AccessCode::UNSET`] when the request's code is not
    /// valid BCD.
    fn amount_pay(
        &mut self,
        aime_id: AimeId,
        access_code: &AccessCode,
        item_id: &[u16],
        item_count: u32,
        option_number: i64,
    ) -> AmResult<()>;

    /// Events libam delivered to the installed listeners since the last call, in firing order. They fire
    /// inside `Manager::execute`, before every `preExecute`.
    fn take_events(&mut self) -> Vec<AimePayEvent>;
}

/// Part of libam `am::AppConfig` touched by the AimePay controller (`AppConfig+0x37E0..0x37F0`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AimePayAppConfig {
    /// `CompleteTaskEventListener` (`controller+0x80`) installed.
    pub complete_task_listener: bool,
    /// `NotifyDealResultEventListener` (`controller+0x88`) installed.
    pub notify_deal_result_listener: bool,
    /// `NeedAcceptationEventListener` (`controller+0x90`) installed.
    pub need_acceptation_listener: bool,
}

/// The location libam reports while the module holds none.
const NO_LOCATION: AimePayLocation = AimePayLocation { valid: false, name: None, company_name: None };

/// `AimePayPort` handle over the shared libam state.
#[derive(Debug)]
pub struct AimePayHandle(pub(crate) Shared);

impl AimePayPort for AimePayHandle {
    fn handle(&self) -> AmResult<()> {
        self.0.guard()
    }

    fn shared_handle(&self) -> AmResult<()> {
        self.0.guard()
    }

    fn is_enabled(&self) -> bool {
        self.0.read(|core| core.aimepay.is_enabled())
    }

    fn deal_log_count(&self) -> usize {
        self.0.read(|core| core.aimepay.history().len())
    }

    fn deal_log_item(&self, index: usize) -> Option<AimePayDeal> {
        self.0.read(|core| core.aimepay.history().get(index).map(super::model::Deal::to_api))
    }

    fn activated_location(&self) -> AimePayLocation {
        self.0.read(|core| core.aimepay.activated().to_api())
    }

    fn current_location(&self) -> AimePayLocation {
        self.0.read(|core| core.aimepay.current_location().map_or(NO_LOCATION, super::model::Location::to_api))
    }

    fn deal_available(&self) -> bool {
        self.0.read(|core| core.aimepay.deal_available(core.display.ready()))
    }

    fn deal_summary_count(&self) -> usize {
        self.0.read(|core| core.aimepay.summary_count())
    }

    fn deal_summary_item(&self, index: usize) -> Option<AimePaySummaryItem> {
        self.0.read(|core| core.aimepay.summary_item(index).map(super::model::Summary::to_api))
    }

    fn user_setting(&self) -> Option<AimePayUserSetting> {
        self.0.read(|core| core.aimepay.user_setting().map(super::model::UserSetting::to_api))
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
        self.0.with(|core| core.aimepay.cancel());
    }

    fn confirm_with_param(&mut self, param: &[u16]) -> AmResult<()> {
        self.0.guard()?;
        self.0.with(|core| drive(core, |module, _, links| module.confirm_with_param(links, param)));
        Ok(())
    }

    fn confirm_accept(&mut self) {
        self.0.with(|core| drive(core, |module, cx, links| module.confirm_accept(cx, links)));
    }

    fn activate(&mut self) -> AmResult<()> {
        self.0.guard()?;
        self.0.with(|core| drive(core, |module, cx, links| module.activate(cx, links)))
    }

    fn deactivate(&mut self) -> AmResult<()> {
        self.0.guard()?;
        self.0.with(|core| drive(core, |module, cx, links| module.deactivate(cx, links)))
    }

    fn query_user_setting(&mut self, aime_id: AimeId) -> AmResult<()> {
        self.0.guard()?;
        self.0.with(|core| core.aimepay.query_user_setting(aime_id.id))
    }

    fn coin_pay(&mut self, aime_id: AimeId, access_code: &AccessCode, player_index: u32, coin: u32) -> AmResult<()> {
        self.0.guard()?;
        self.0.with(|core| {
            drive(core, |module, cx, links| module.coin_pay(cx, links, aime_id.id, access_code, player_index, coin))
        })
    }

    fn amount_pay(
        &mut self,
        aime_id: AimeId,
        access_code: &AccessCode,
        item_id: &[u16],
        item_count: u32,
        option_number: i64,
    ) -> AmResult<()> {
        self.0.guard()?;
        let item_id = wide_to_string(item_id);
        self.0.with(|core| {
            drive(core, |module, _, links| {
                module.amount_pay(links, aime_id.id, access_code, &item_id, item_count, option_number)
            })
        })
    }

    fn take_events(&mut self) -> Vec<AimePayEvent> {
        self.0.with(|core| core.aimepay.take_events())
    }
}
