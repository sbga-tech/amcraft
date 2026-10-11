//! The ALL.Net accounting facade: [`AccountingPort`], [`AccountingInfo`], [`PlayCountInfo`] and [`PlayCountItem`],
//! and libam's [`AccountingHandle`].

use super::reporter::{load_play_history_from, load_report_time_from};
use super::schedule;
use crate::allnet::TimeSlot;
use crate::result::{AmError, AmResult};
use crate::shared::Shared;

/// One play-count record of libam's history file (`{u32 year; u32 month; u64 count}`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlayCountItem {
    /// Calendar year.
    pub year: u32,
    /// Calendar month, 1-based.
    pub month: u32,
    /// Play count.
    pub count: u64,
}

/// libam `PlayCountInfo` (`AccountingInfo+0x08`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlayCountInfo {
    /// The history file was read; otherwise the items are skipped.
    pub valid: bool,
    /// Items 0, 1 and 2 = `current`, `last`, `beforeLast` on the wire (libam item 0 is the third record of
    /// the history file).
    pub items: [PlayCountItem; 3],
}

/// libam `AccountingInfo` (`0x90` bytes at `LM+0x11A0`, spec §8.4). libam rebuilds it, including a file
/// read, on every `accountingInfo` call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AccountingInfo {
    /// `+0x00` `alpbExGetNearFullEnable == 1`.
    pub near_full: bool,
    /// `+0x01` `alpbExGetLogFullStatus == 1`.
    pub log_full: bool,
    /// `+0x08` play-count history.
    pub play_count: PlayCountInfo,
    /// `+0x40` `alpbExLoadLastAccountingReportTime`.
    pub report_time: TimeSlot,
    /// `+0x68` `alpbExLoadLastBgAccountingReportTime`.
    pub bg_report_time: TimeSlot,
}

/// libam port of the Accounting controller (spec §3.6; evidence `allnet.accounting.port.table`).
///
/// The write methods are libam's facade wrappers: they do their own precondition checks (accounting
/// enabled, player is playing, slot valid) and report failures as `am::Result` codes 2 and 6, which the
/// controller only maps to "task failed".
pub trait AccountingPort {
    /// Accounting facade handle (`0x1402191B0`): `Err` (code 2) when the libam singleton does not exist.
    fn acquire_handle(&self) -> AmResult<()>;

    /// `0x140215C30`: raw accounting mode (`LM+0x1198`).
    fn accounting_mode(&self) -> u32;

    /// `0x140215C10`: accounting is enabled and the play-log module is ready.
    fn accounting_enabled(&self) -> bool;

    /// `0x140215CC0`: the rebuilt accounting info; `Ok(None)` or `Err` when libam has none.
    fn accounting_info(&self) -> AmResult<Option<AccountingInfo>>;

    /// `0x140215C50`: seconds until the next accounting report.
    fn seconds_until_report(&self) -> AmResult<u32>;

    /// `0x140215B80`: enables or disables near-full reporting.
    fn set_near_full(&mut self, enable: bool) -> AmResult<()>;

    /// `0x140215830`: starts a credit and returns its libam slot index.
    fn start_credit(&mut self, player: u32, kind_code: u32, status_code: u32) -> AmResult<u32>;

    /// `0x1402158E0`: ends the credit in `slot`.
    fn end_credit(&mut self, player: u32, kind_code: u32, status_code: u32, item_count: u8, slot: u32) -> AmResult<()>;

    /// `0x140215990`: accounts items.
    fn item_account(&mut self, player: u32, kind_code: u32, status_code: u32, item_count: u8) -> AmResult<()>;

    /// `0x140215A40`: puts a quantity.
    fn put_quantity(&mut self, player: u32, kind_code: u32, quantity: u16) -> AmResult<()>;

    /// `0x140215AE0`: puts a general ID, the request's `wchar_t[128]` without its terminator (libam converts it
    /// to CP932).
    fn put_general_id(&mut self, player: u32, kind_code: u32, general_id: &[u16]) -> AmResult<()>;
}

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
