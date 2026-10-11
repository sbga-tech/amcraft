//! Port handles of the sequence module.

use libam_api::result::{AmError, AmResult};
use libam_api::sequence::{AimeLogStatus, BookkeepingTotals, HistogramRow, SequencePort};
use libam_api::time::Tm;

use super::PlayChange;
use crate::core::Core;
use crate::shared::Shared;

/// `Result` code of a failed play facade call (sequence.md 4.3, 4.4).
const PLAY_FAILED: u32 = 6;

fn not_playing() -> AmError {
    AmError::with_message(PLAY_FAILED, "no playing")
}

impl Core {
    /// `setPlayerState` at the clock's current reading.
    fn change_play(&mut self, index: usize, change: PlayChange) {
        let now = self.clock.tick_ms();
        self.sequence.change_play(index, change, now, &mut self.events);
    }

    /// The common part of `continue` / `end` / `cancel` (`0x1402534F0`, `0x140253560`, `0x1402535D0`).
    fn play_facade(&mut self, index: u32, change: PlayChange) -> AmResult {
        let index = index as usize;
        if !self.sequence.is_playing(index) {
            return Err(not_playing());
        }
        if self.error.current().number == 0 {
            self.change_play(index, change);
            if change != PlayChange::Continue {
                self.refresh_idle_busy();
            }
        }
        Ok(())
    }
}

/// `SequencePort` handle over the shared libam state.
#[derive(Debug)]
pub struct SequenceHandle(pub(crate) Shared);

impl SequencePort for SequenceHandle {
    fn error_facade_available(&self) -> bool {
        self.0.guard().is_ok()
    }

    fn credit_facade_available(&self) -> bool {
        self.0.guard().is_ok()
    }

    fn sequence_facade_available(&self) -> bool {
        self.0.guard().is_ok()
    }

    fn accounting_facade_available(&self) -> bool {
        self.0.guard().is_ok()
    }

    fn aime_facade_available(&self) -> bool {
        self.0.guard().is_ok()
    }

    fn fatal_error_pending(&self) -> bool {
        self.0.read(|core| core.error.current().number != 0)
    }

    fn is_playing(&self, index: u32) -> bool {
        self.0.read(|core| core.sequence.is_playing(index as usize))
    }

    fn begin_play(&mut self, index: u32) -> AmResult {
        self.0.guard()?;
        self.0.with(|core| {
            let slot = index as usize;
            if !core.sequence.is_playing(slot) && core.error.current().number == 0 {
                core.change_play(slot, PlayChange::Begin);
                core.refresh_idle_busy();
            }
            if core.sequence.is_playing(slot) {
                Ok(())
            } else {
                Err(AmError::with_message(PLAY_FAILED, "Failed to start play."))
            }
        })
    }

    fn continue_play(&mut self, index: u32) -> AmResult {
        self.0.guard()?;
        self.0.with(|core| core.play_facade(index, PlayChange::Continue))
    }

    fn end_play(&mut self, index: u32) -> AmResult {
        self.0.guard()?;
        self.0.with(|core| core.play_facade(index, PlayChange::End))
    }

    fn cancel_play(&mut self, index: u32) -> AmResult {
        self.0.guard()?;
        self.0.with(|core| core.play_facade(index, PlayChange::Cancel))
    }

    fn clear_bookkeeping(&mut self) {
        self.0.with(|core| core.sequence.clear_bookkeeping());
    }

    fn toggle_mode(&mut self) {
        self.0.with(|core| {
            if !core.exists {
                return;
            }
            let new = if matches!(core.sequence.process_state(), 1 | 3 | 4) { 2 } else { 1 };
            core.sequence.touch(core.clock.tick_ms());
            if core.sequence.set_state(new, &mut core.events) {
                core.sequence.store_now(&core.nvram);
            }
            core.refresh_modules(true);
        });
    }

    fn bookkeeping_totals(&self) -> BookkeepingTotals {
        self.0.read(|core| core.sequence.totals())
    }

    fn histogram_count(&self) -> u32 {
        self.0.read(|core| core.sequence.histogram_count())
    }

    fn histogram_row(&self, index: u32) -> AmResult<HistogramRow> {
        self.0.guard()?;
        self.0.read(|core| core.sequence.histogram_row(index)).ok_or_else(|| AmError::with_message(1, "Invalid num"))
    }

    fn credit_check(&self, player: u32, cost_index: i16, cost_count: u16) -> AmResult<u32> {
        self.0.guard()?;
        self.0.read(|core| core.credit.check(player, cost_index, cost_count))
    }

    fn credit_pay(&mut self, player: u32, cost_index: i16, cost_count: u16) -> AmResult {
        self.0.guard()?;
        self.0.with(|core| {
            // The sequence gate of `0x140253640`: advertise or play, no pending root error.
            if !core.sequence.is_game() || core.error.current().number != 0 {
                return Err(AmError::code(PLAY_FAILED));
            }
            core.credit.pay(player, cost_index, cost_count)
        })
    }

    fn credit_rule(&self) -> u32 {
        self.0.read(|core| core.credit.rule())
    }

    fn credit_cost(&self, cost_index: i16) -> AmResult<u32> {
        self.0.guard()?;
        self.0.read(|core| core.credit.cost(cost_index))
    }

    fn accounting_available(&self) -> bool {
        self.0.read(|core| core.allnet.billing.accounting_available())
    }

    fn begin_accounting(&mut self, player: u32, kind_code: u32, status_code: u32) -> AmResult<u32> {
        self.0.guard()?;
        self.0.with(|core| core.allnet.billing.begin_accounting(player, kind_code, status_code))
    }

    fn end_accounting(
        &mut self,
        player: u32,
        kind_code: u32,
        status_code: u32,
        item_count: u32,
        handle: u32,
    ) -> AmResult {
        self.0.guard()?;
        self.0.with(|core| core.allnet.billing.end_accounting(player, kind_code, status_code, item_count, handle))
    }

    fn local_time(&self) -> Tm {
        self.0.read(|core| core.now_tm())
    }

    fn aime_log(&mut self, status: AimeLogStatus, aime_id: u32, time: &Tm, credit: u32) -> AmResult {
        self.0.guard()?;
        self.0.with(|core| core.aime.log(status, aime_id, time, credit))
    }
}
