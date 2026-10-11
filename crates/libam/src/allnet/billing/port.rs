//! `AccountingPort` handle.

use libam_api::allnet::{AccountingInfo, AccountingPort, PlayCountInfo, PlayCountItem, TimeSlot};
use libam_api::result::{AmError, AmResult};

use super::reporter::{load_play_history_from, load_report_time_from};
use super::schedule;
use crate::shared::Shared;

/// `AccountingPort` handle over the shared libam state.
#[derive(Debug)]
pub struct AccountingHandle(pub(crate) Shared);

impl AccountingPort for AccountingHandle {
    fn acquire_handle(&self) -> AmResult<()> {
        self.0.guard()
    }

    fn accounting_mode(&self) -> u32 {
        self.0.read(|core| core.allnet.billing.mode())
    }

    fn accounting_enabled(&self) -> bool {
        self.0.read(|core| core.allnet.billing.accounting_available())
    }

    fn accounting_info(&self) -> AmResult<Option<AccountingInfo>> {
        self.0.guard()?;
        self.0.read(|core| {
            let billing = &core.allnet.billing;
            if !billing.initialized {
                return Ok(None);
            }
            let paths = &*core.backend.fs;
            let play_count = load_play_history_from(paths).map_or_else(PlayCountInfo::default, |history| {
                let item = |(year, month, count): (i32, i32, i32)| PlayCountItem {
                    year: u32::try_from(year).unwrap_or(0),
                    month: u32::try_from(month).unwrap_or(0),
                    count: u64::try_from(count).unwrap_or(0),
                };
                // Libam item 0 is the third record of the history file.
                PlayCountInfo { valid: true, items: [item(history[2]), item(history[1]), item(history[0])] }
            });
            let report_time =
                load_report_time_from(paths).map_or_else(TimeSlot::default, |tm| TimeSlot { valid: true, tm });
            Ok(Some(AccountingInfo {
                near_full: billing.reporter.near_full_enabled,
                log_full: billing.playbook.game_test_log_full(),
                play_count,
                report_time,
                bg_report_time: TimeSlot::default(),
            }))
        })
    }

    fn seconds_until_report(&self) -> AmResult<u32> {
        self.0.guard()?;
        self.0.read(|core| {
            let billing = &core.allnet.billing;
            if !billing.accounting_available() {
                return Err(AmError::code(AmError::NOT_INITIALIZED));
            }
            let now = schedule::seconds_of(core.clock.local_now().civil());
            Ok(billing.reporter.seconds_until(now))
        })
    }

    fn set_near_full(&mut self, enable: bool) -> AmResult<()> {
        self.0.guard()?;
        self.0.with(|core| {
            let billing = &mut core.allnet.billing;
            if !billing.initialized {
                return Err(AmError::code(AmError::NOT_INITIALIZED));
            }
            billing.reporter.near_full_enabled = enable;
            Ok(())
        })
    }

    fn start_credit(&mut self, player: u32, kind_code: u32, status_code: u32) -> AmResult<u32> {
        self.0.guard()?;
        self.0.with(|core| core.allnet.billing.begin_accounting(player, kind_code, status_code))
    }

    fn end_credit(&mut self, player: u32, kind_code: u32, status_code: u32, item_count: u8, slot: u32) -> AmResult<()> {
        self.0.guard()?;
        self.0.with(|core| {
            core.allnet.billing.end_accounting(player, kind_code, status_code, u32::from(item_count), slot)
        })
    }

    fn item_account(&mut self, player: u32, kind_code: u32, status_code: u32, item_count: u8) -> AmResult<()> {
        self.0.guard()?;
        self.0.with(|core| core.allnet.billing.facade_item_account(player, kind_code, status_code, item_count))
    }

    fn put_quantity(&mut self, player: u32, kind_code: u32, quantity: u16) -> AmResult<()> {
        self.0.guard()?;
        self.0.with(|core| core.allnet.billing.facade_put_quantity(player, kind_code, quantity))
    }

    fn put_general_id(&mut self, player: u32, kind_code: u32, general_id: &[u16]) -> AmResult<()> {
        self.0.guard()?;
        self.0.with(|core| core.allnet.billing.facade_put_general_id(player, kind_code, general_id))
    }
}
