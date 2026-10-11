//! Port handles of the aime module.

use libam_api::aime::{
    AccessCode, AimeEvent, AimePort, CampaignInfoList, CampaignProgressList, CardInfo, LedPattern, LibamAimeId,
    PlayLogStatus, ReaderInfo, RelatedSegaId,
};
use libam_api::result::AmResult;
use libam_api::time::Tm;

use crate::shared::Shared;

/// `AimePort` handle over the shared libam state.
#[derive(Debug)]
pub struct AimeHandle(pub(crate) Shared);

impl AimePort for AimeHandle {
    fn facade_available(&self) -> bool {
        self.0.guard().is_ok()
    }

    fn unit_count(&self) -> u64 {
        self.0.read(|core| core.aime.unit_count())
    }

    fn is_db_alive(&self) -> bool {
        self.0.read(|core| core.aime.is_db_alive())
    }

    fn start_scan_offline(&mut self, unit: usize) -> AmResult {
        self.0.guard()?;
        self.0.with(|core| core.aime.start_scan(unit, 2, &core.backend, &*core.clock))
    }

    fn start_scan(&mut self, unit: usize) -> AmResult {
        self.0.guard()?;
        self.0.with(|core| core.aime.start_scan(unit, 5, &core.backend, &*core.clock))
    }

    fn confirm(&mut self, unit: usize) -> AmResult {
        self.0.guard()?;
        self.0.with(|core| core.aime.confirm(unit))
    }

    fn cancel(&mut self, unit: usize) -> AmResult {
        self.0.guard()?;
        self.0.with(|core| core.aime.cancel(unit))
    }

    fn set_led(&mut self, unit: usize, red: u8, green: u8, blue: u8) -> AmResult {
        self.0.guard()?;
        self.0.with(|core| core.aime.set_led(unit, [red, green, blue]))
    }

    fn set_led_pattern(&mut self, unit: usize, pattern: LedPattern) -> AmResult {
        self.0.guard()?;
        self.0.with(|core| core.aime.set_led_pattern(unit, pattern))
    }

    fn reader_info(&self, unit: usize) -> Option<ReaderInfo> {
        self.0.read(|core| core.aime.reader_info(unit))
    }

    fn card_info(&self, unit: usize) -> Option<CardInfo> {
        self.0.read(|core| core.aime.card_info(unit))
    }

    fn access_code(&self, unit: usize) -> Option<AccessCode> {
        self.0.read(|core| core.aime.access_code(unit))
    }

    fn aime_id(&self, unit: usize) -> Option<LibamAimeId> {
        self.0.read(|core| core.aime.aime_id(unit))
    }

    fn info_related_sega_id(&self, unit: usize) -> Option<RelatedSegaId> {
        self.0.read(|core| core.aime.related_sega_id(unit))
    }

    fn send_play_log(&mut self, status: PlayLogStatus, aime_id: LibamAimeId, time: &Tm, credit: u32) -> AmResult {
        self.0.guard()?;
        self.0.with(|core| core.aime.enqueue_log(status as u32, aime_id.id, time, credit))
    }

    fn query_campaign_progress(&mut self, aime_id: LibamAimeId) -> AmResult {
        self.0.guard()?;
        self.0.with(|core| {
            let now = core.clock.tick_ms();
            core.aime.query_campaign_progress(aime_id, now)
        })
    }

    fn campaign_info_list(&self) -> Option<CampaignInfoList> {
        self.0.read(|core| core.aime.campaign_info_list())
    }

    fn campaign_progress_list(&self) -> Option<CampaignProgressList> {
        self.0.read(|core| core.aime.campaign_progress_list())
    }

    fn take_events(&mut self) -> Vec<AimeEvent> {
        self.0.with(|core| core.aime.take_events())
    }
}
