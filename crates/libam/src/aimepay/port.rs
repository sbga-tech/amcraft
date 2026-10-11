//! Port handles of the aimepay module.

use libam_api::aime::AccessCode;
use libam_api::aime_pay::{
    AimeId, AimePayDeal, AimePayEvent, AimePayLocation, AimePayPort, AimePaySummaryItem, AimePayUserSetting,
};
use libam_api::result::AmResult;

use super::drive;
use crate::shared::Shared;
use crate::util::wide_to_string;

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
