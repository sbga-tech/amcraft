//! The alive checks and the reports to the sales-management site (`spec/libam/payment.md` PayEmSvc 8, 9 and 10).
//! The original runs them as sequencers on the Manager's runner; here every step runs to completion inside the frame
//! that starts it, which the blocking backend HTTP client allows. The timers are the stopwatches of the alive object.

use super::deal_log::{FLAG_REPORTED, FLAG_UNKNOWN};
use super::sms::{self, Check, SaleRecord, Service};
use super::{EMoney, FLAG_SITE};
use crate::module::Cx;

/// Fixed interval of SendSales and SendCounter (8.2).
const REPORT_INTERVAL_MS: u64 = 900_000;
/// Service check interval while it succeeds / fails (9.3).
const CHECK_OK_MS: u64 = 1_800_000;
const CHECK_FAILED_MS: u64 = 60_000;
/// Start-up retry (9.5).
const START_RETRY_MS: u64 = 60_000;
/// Capacity of the sales queue (11.1).
const QUEUE_CAPACITY: usize = 10;

/// The alive object `A` (`M+88096`): modes, the two alive bytes and the stopwatches.
#[derive(Debug, Clone, Default)]
pub(super) struct Alive {
    /// `A+1184`: 0 off, 1 start-up, 2 steady.
    pub(super) mode: u8,
    /// `A+528`: the service check (`checkBrands`) succeeded.
    pub(super) service_ok: bool,
    /// `A+720`: the CheckSetting succeeded.
    pub(super) tasms_ok: bool,
    /// `A+884`: the code of the error stamp, 8701 service check failed, 8702 CheckSetting failed, 0 none.
    pub(super) stamp: u16,
    /// CheckSetting `lastStatus` (`+64`).
    last_status: u32,
    /// `A+1188`: the last full alive start.
    start_at: u64,
    /// `A+520`: the last service check.
    check_at: u64,
    /// The last CheckSetting.
    setting_at: u64,
    status_at: u64,
    sales_at: u64,
    counter_at: u64,
    /// SendCounter `+81`: it ran once.
    counter_ran: bool,
    /// SendCounter `+82`: the previous run failed in baseline mode.
    counter_retry: bool,
    /// SendCounter `+76 == 1`.
    counter_notification: bool,
}

impl Alive {
    /// `0x14027D730(A, clearAll)`: every stopwatch, byte and the service check outcome go back to the start. The
    /// SendCounter "ran once" byte (`+81`) is cleared only with `clear_all`, so a module restart keeps it.
    pub(super) fn reset(&mut self, clear_all: bool) {
        let counter_ran = self.counter_ran && !clear_all;
        *self = Self { counter_ran, ..Self::default() };
    }
}

/// The decimal status text as the report block carries it.
fn interval_ms(minutes: u32) -> u64 {
    u64::from(minutes) * 60_000
}

impl EMoney {
    /// The values the Manager's gates read from the service (`M+29316`, `M+29424`, `M+29426`) and the alive bytes.
    pub(super) fn sync_site(&mut self) {
        let settings = &self.service.settings;
        self.site.status = settings.status_number();
        self.site.cash_available = settings.cash;
        self.site.usable = settings.product_code.is_some();
        self.site.alive = self.inputs.online && self.alive_run.service_ok && self.alive_run.tasms_ok;
    }

    /// `0x14027DBA0`, stamp upkeep after a check ran: an empty stamp takes 8701 while the service check failed;
    /// with the service check good a failed CheckSetting stamps 8702; both good clear the stamp.
    pub(super) fn stamp_upkeep(&mut self) {
        let alive = &mut self.alive_run;
        if !alive.service_ok {
            if alive.stamp == 0 {
                alive.stamp = 8701;
            }
        } else if !alive.tasms_ok {
            alive.stamp = 8702;
        } else {
            alive.stamp = 0;
        }
    }

    /// `0x14025FB70`: a URL is stored but the terminal is not registered yet (the activation is half done).
    pub(super) fn pending_url(&self) -> bool {
        self.info.as_ref().is_some_and(|info| !info.is_registered() && !info.url().is_empty())
    }

    /// `0x14027D840` (`start_all` false) and `0x14027D900` (`start_all` true): `[C, T]` run to completion. The result
    /// is the status of the sequence: the service check, CheckSetting and, with `start_all`, the three reports.
    pub(super) fn start_alive(&mut self, cx: &mut Cx<'_>, start_all: bool) -> bool {
        let registered = self.info.as_ref().is_some_and(super::deal_log::EmoneyInfo::is_registered);
        if self.alive_run.mode == 0 || !registered {
            self.alive_run.mode = 1;
        }
        self.alive_run.start_at = self.tick_ms;
        self.restore_service();
        self.service_check(cx);
        let mut ok = self.alive_run.service_ok;
        if ok {
            ok = self.setting_check(cx);
        }
        if ok && start_all {
            ok = self.report_all(cx);
        }
        self.stamp_upkeep();
        if self.info.as_ref().is_some_and(super::deal_log::EmoneyInfo::is_blank) {
            self.service = Service::default();
            self.alive_run = Alive::default();
            self.restore_report_block();
        }
        self.sync_site();
        ok
    }

    /// 4.4: a registered terminal that just booted widens the stored URL into the service.
    fn restore_service(&mut self) {
        let Some(info) = self.info.as_ref().filter(|info| !info.is_blank()) else { return };
        if self.service.settings.terminals_url.is_empty() {
            self.service.settings.terminals_url = info.url();
        }
    }

    /// The versions the status report carries (4.4).
    pub(super) fn restore_report_block(&mut self) {
        self.service.report.module_version = self.module_version.clone();
        self.service.report.rw_adapter_version = "1.0.3.0".to_owned();
    }

    /// `SC` (9.3): the SDK's `checkBrands`; `A+528` takes its outcome.
    fn service_check(&mut self, cx: &mut Cx<'_>) {
        cx.backend.thinca.prelude();
        let code = cx.backend.thinca.check_brands();
        self.alive_run.service_ok = code == 0 && !self.hub.abort;
        self.alive_run.check_at = self.tick_ms;
    }

    /// `TC` (8.4): CheckSetting; `true` when it is not applicable (no terminals URL) or succeeded.
    fn setting_check(&mut self, cx: &mut Cx<'_>) -> bool {
        self.alive_run.setting_at = self.tick_ms;
        let url = self.service.settings.terminals_url.clone();
        if url.is_empty() {
            return true;
        }
        for entry in &mut self.brands.entries {
            entry.flags &= !FLAG_SITE;
        }
        let answer = sms::check_setting(&mut *cx.backend.http, &url, &self.service.etag);
        let ok = match answer {
            Ok(check) => {
                if let Check::Updated(settings, etag) = check {
                    let serial = self.terminal.serial.clone();
                    self.service.adopt(*settings, &serial);
                    self.service.etag = etag;
                    let now = self.record_time();
                    self.service.stamp(now);
                }
                self.sync_site();
                let status = self.site.status_code();
                let mut ok = true;
                if self.alive_run.last_status == 0 && status != 0 {
                    let cash = self.service.settings.cash;
                    let changed = self.info.as_mut().is_some_and(|info| {
                        let first = info.update_deal_log(0, cash);
                        info.update_deal_log(1, cash) | first
                    });
                    if changed {
                        ok = self.save_info(cx);
                    }
                }
                self.alive_run.last_status = status;
                let order = self.service.settings.brands.clone();
                self.set_site_brands(&order);
                // 8.4 step 5 (`0x14027B7B0`): AimePayInfo `smsEnabled` := "code 91 listed". A failed deal-log save
                // ends the command before this step.
                if ok {
                    self.sms_enabled = self.service.settings.brand_91;
                }
                ok
            }
            Err(error) => {
                tracing::warn!(%error, "emoney: check setting failed");
                if self.service.settings.check_minutes == 0 {
                    self.service.settings.check_minutes = 1;
                    self.service.settings.status_minutes = 1;
                }
                self.brands.rebuild();
                false
            }
        };
        self.alive_run.tasms_ok = ok && !self.hub.abort;
        ok
    }

    /// `T` (9.4): SendSales, SendCounter, SendStatus in that order. A command of a registered terminal always ends
    /// OK (8.1); a failing one of an unregistered terminal aborts the rest.
    fn report_all(&mut self, cx: &mut Cx<'_>) -> bool {
        let registered = self.info.as_ref().is_some_and(super::deal_log::EmoneyInfo::is_registered);
        let steps: [fn(&mut EMoney, &mut Cx<'_>) -> bool; 3] =
            [Self::send_sales, Self::send_counter, Self::send_status];
        for step in steps {
            if !step(self, cx) && !registered {
                return false;
            }
        }
        true
    }

    /// 11.1 to 11.3: the queue records of the not yet reported entries of both rings.
    fn gather_sales(&self, product: u32) -> (Vec<SaleRecord>, [usize; 2]) {
        let mut queue = Vec::new();
        let mut counts = [0usize; 2];
        let Some(info) = &self.info else { return (queue, counts) };
        for (ring, count) in counts.iter_mut().enumerate() {
            let records = info.ring_oldest_first(ring);
            let Some(first) = records.iter().position(|record| record.flags & FLAG_REPORTED == 0) else { continue };
            for record in &records[first..] {
                let result = match record.status {
                    0 => 0,
                    2 => 3,
                    3 => 4,
                    _ => 1,
                };
                if result == 1 && record.flags & FLAG_UNKNOWN == 0 {
                    continue;
                }
                if queue.len() >= QUEUE_CAPACITY {
                    break;
                }
                let method = match record.brand {
                    1 | 6 => 1,
                    2 => 2,
                    3 => 5,
                    4 => 6,
                    5 => 8,
                    7 => 3,
                    8 => 9,
                    _ => 101,
                };
                queue.push(SaleRecord {
                    method,
                    result,
                    amount: record.amount.cast_unsigned(),
                    deal_number: record.deal_number.clone(),
                    card: record.card.clone(),
                    product,
                    quantity: record.count,
                    item_id: String::new(),
                    time: record.time,
                    keychip: self.terminal.keychip.clone(),
                    game_id: self.title.clone().unwrap_or_default(),
                    se_id: String::new(),
                });
                *count += 1;
            }
        }
        (queue, counts)
    }

    /// SendSalesData (8.4).
    fn send_sales(&mut self, cx: &mut Cx<'_>) -> bool {
        self.hub.sales_pending = false;
        self.alive_run.sales_at = self.tick_ms;
        let url = self.service.settings.sales_url.clone();
        let Some(product) = self.service.settings.product_code else { return true };
        if url.is_empty() {
            return true;
        }
        let (queue, counts) = self.gather_sales(product);
        if queue.is_empty() {
            return true;
        }
        let serial = self.terminal.serial.clone();
        let sent = sms::send_sales(&mut *cx.backend.http, &url, &serial, &queue);
        if let Err(error) = &sent {
            tracing::warn!(%error, "emoney: send sales failed");
        }
        let ok = sent.is_ok();
        if ok && let Some(info) = self.info.as_mut() {
            let changed = info.mark_reported(0, counts[0]) | info.mark_reported(1, counts[1]);
            if changed && !self.save_info(cx) {
                return false;
            }
        }
        ok
    }

    /// SendCounter (8.6).
    fn send_counter(&mut self, cx: &mut Cx<'_>) -> bool {
        self.alive_run.counter_at = self.tick_ms;
        let url = self.service.settings.counters_url.clone();
        if url.is_empty() || self.service.settings.product_code.is_none() {
            return true;
        }
        let pending = self.pending_url();
        let mut runs = 0;
        loop {
            let Some(info) = &self.info else { return true };
            let last: [u32; 3] = std::array::from_fn(|slot| info.reported_counters()[slot]);
            let current: [u32; 3] = std::array::from_fn(|slot| self.inputs.coin_counters[slot]);
            let snapshot = if (0..3).any(|slot| current[slot] < last[slot]) {
                [0; 3]
            } else if !self.alive_run.counter_ran || self.alive_run.counter_retry {
                last
            } else {
                self.alive_run.counter_notification = true;
                current
            };
            let notification = self.alive_run.counter_notification;
            self.alive_run.counter_ran = true;
            self.alive_run.counter_retry = false;
            let sent = sms::send_counter(
                &mut *cx.backend.http,
                &url,
                [snapshot[0], snapshot[1], snapshot[2], u32::from(notification)],
            );
            if let Err(error) = sent {
                tracing::warn!(%error, "emoney: send counter failed");
                if !notification {
                    self.alive_run.counter_retry = true;
                }
                return false;
            }
            let mut counters = [0u32; 8];
            counters[..3].copy_from_slice(&snapshot);
            if self.info.as_mut().is_some_and(|info| info.set_reported_counters(counters)) && !self.save_info(cx) {
                return false;
            }
            runs += 1;
            if pending && !notification && current != snapshot && runs < 2 {
                continue;
            }
            return true;
        }
    }

    /// SendStatus (8.3): the status `"90"` of a deactivated terminal ends in `blank`.
    fn send_status(&mut self, cx: &mut Cx<'_>) -> bool {
        self.alive_run.status_at = self.tick_ms;
        let url = self.service.settings.statuses_url.clone();
        if url.is_empty() {
            return true;
        }
        if self.pending_url() {
            self.service.report.status_code = "90".to_owned();
        } else if self.service.report.status_code == "00" {
            self.service.report.status_code = "10".to_owned();
        }
        let removal = self.service.report.status_code == "90";
        if let Err(error) = sms::send_status(&mut *cx.backend.http, &url, &self.service.report) {
            tracing::warn!(%error, "emoney: send status failed");
            return false;
        }
        if removal {
            for entry in &mut self.brands.entries {
                entry.flags &= !FLAG_SITE;
            }
            self.brands.rebuild();
            if let Some(info) = self.info.as_mut() {
                info.clear_data();
            }
            if !self.save_info(cx) {
                return false;
            }
            self.terminal.delete(&*cx.backend.fs);
        }
        true
    }

    /// `0x14027D920` (9.5), once per frame: the start-up retry and the steady schedule; at most one step per frame.
    pub(super) fn service_tick(&mut self, cx: &mut Cx<'_>) {
        let Some(info) = &self.info else { return };
        if info.is_blank() {
            self.alive_run.mode = 0;
            return;
        }
        if !self.inputs.online || self.alive_run.mode == 0 || self.flow.is_some() {
            return;
        }
        let registered = info.is_registered();
        let now = self.tick_ms;
        let elapsed = |since: u64| now.saturating_sub(since);
        let report_enable = !self.suspended;
        if self.hub.sales_pending && !self.service.settings.sales_url.is_empty() {
            self.send_sales(cx);
            return;
        }
        if self.alive_run.mode == 1 {
            if registered && self.alive_run.service_ok && self.alive_run.tasms_ok {
                self.alive_run.mode = 2;
                self.stamp_upkeep();
            } else if report_enable
                && elapsed(self.alive_run.start_at) >= START_RETRY_MS
                && !(registered && self.stop_requested)
            {
                self.start_alive(cx, false);
            }
            return;
        }
        if !report_enable || self.stop_requested {
            return;
        }
        let settings = &self.service.settings;
        let check_interval = if self.alive_run.service_ok { CHECK_OK_MS } else { CHECK_FAILED_MS };
        let service_due = elapsed(self.alive_run.check_at) >= check_interval;
        let setting_interval = interval_ms(settings.check_minutes);
        let setting_due = !settings.terminals_url.is_empty()
            && setting_interval != 0
            && elapsed(self.alive_run.setting_at) >= setting_interval;
        let status_due = !settings.statuses_url.is_empty()
            && interval_ms(settings.status_minutes) != 0
            && elapsed(self.alive_run.status_at) >= interval_ms(settings.status_minutes);
        let sales_due = !settings.sales_url.is_empty() && elapsed(self.alive_run.sales_at) >= REPORT_INTERVAL_MS;
        let counter_due = !settings.counters_url.is_empty()
            && settings.product_code.is_some()
            && elapsed(self.alive_run.counter_at) >= REPORT_INTERVAL_MS;
        if service_due {
            self.service_check(cx);
            if self.alive_run.service_ok {
                self.setting_check(cx);
            }
            self.stamp_upkeep();
        } else if setting_due {
            self.setting_check(cx);
            self.stamp_upkeep();
        } else if status_due {
            self.send_status(cx);
        } else if sales_due {
            self.send_sales(cx);
        } else if counter_due {
            self.send_counter(cx);
        }
    }
}
