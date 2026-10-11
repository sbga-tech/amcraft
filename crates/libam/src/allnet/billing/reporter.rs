//! The daily accounting reporter (the foreground "accounting" AR object of `allnet.md` part 9): initialisation,
//! the daily schedule, the start-request gate, one report per day and the application of the server's answer to
//! the keychip, the trace log and the stream files in the ALPB temp directory.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError, channel};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use libam_api::time::{CivilTime, Tm};

use super::AuthLink;
use super::driver::DriverDongle;
use super::link::ReportLink;
use super::playbook::TraceWriter;
use super::properties;
use super::records::{Record, Stamp, credit_config_record, message_record};
use super::report::{DEFAULT_LINE_LIMIT, ReportJob, ReportOutcome, Role, run_report};
use super::sama;
use super::schedule;
use crate::module::Cx;

/// The ALPB temp directory every stream file lives in (cabinet path).
const TEMP_DIR: &str = r"C:\Users\AppUser\temp\";
/// Default `accounting-report.host`.
const DEFAULT_HOST: &str = "ib.naominet.jp";

/// Status codes of `ARGetStatus` the reporter itself sets (the negative ones are `run_report`'s).
pub mod ar_status {
    /// Initialising.
    pub const INITIALIZING: i32 = 5;
    /// Waiting for the daily time.
    pub const WAIT_TIMING: i32 = 4;
    /// Waiting for the start request.
    pub const WAIT_START: i32 = 1;
    /// A report is running.
    pub const BUSY: i32 = 2;
    /// A report finished.
    pub const FINISHED: i32 = 3;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Stage {
    #[default]
    Uninitialized,
    WaitTiming,
    WaitStart,
    Running,
    Stopped,
}

/// The foreground accounting reporter.
#[derive(Debug, Default)]
pub struct Reporter {
    stage: Stage,
    status: i32,
    enabled: bool,
    start_requested: bool,
    /// Next daily report, local-clock seconds (`AR+544`).
    next: i64,
    /// When the current or last report started (`AR+560`).
    last_report: i64,
    /// `accounting-report.start-time`; zero until the settings are re-applied (the `ARInitialize` quirk).
    start_hour: u32,
    start_minute: u32,
    host: String,
    first_done: bool,
    serial: u32,
    mem_repair: u32,
    line_limit: usize,
    term_date_error: bool,
    receiver: Option<Receiver<ReportOutcome>>,
    /// The record numbering of the keychip trace log.
    pub trace: TraceWriter,
    /// `accounting-report.near-full` is not `0`.
    pub near_full_enabled: bool,
}

/// Local clock seconds ("fields read as UTC") and the calendar stamp of this frame.
pub fn now_of(cx: &Cx<'_>) -> (i64, CivilTime, Stamp) {
    let civil = cx.clock.local_now().civil();
    (schedule::seconds_of(civil), civil, schedule::stamp_of_civil(civil))
}

fn file(cx: &Cx<'_>, name: &str) -> PathBuf {
    cx.backend.fs.map(&format!("{TEMP_DIR}{name}"))
}

fn try_write_file(cx: &Cx<'_>, name: &str, content: &[u8]) -> std::io::Result<()> {
    let path = file(cx, name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)
}

fn write_file(cx: &Cx<'_>, name: &str, content: &[u8]) {
    // A failed write only loses a cache file; the original ignores it as well.
    let _ = try_write_file(cx, name, content);
}

fn pem(der: &[u8]) -> Vec<u8> {
    let encoded = STANDARD.encode(der);
    let mut text = String::from("-----BEGIN CERTIFICATE-----\n");
    for line in encoded.as_bytes().chunks(64) {
        text.push_str(std::str::from_utf8(line).unwrap_or_default());
        text.push('\n');
    }
    text.push_str("-----END CERTIFICATE-----");
    text.into_bytes()
}

/// `ARLoadPlayHistory`: the three `{year, month, count}` triples of `play_history.csv`, `None` when the file is
/// missing or still invalidated.
pub fn load_play_history_from(paths: &dyn backend::Fs) -> Option<[(i32, i32, i32); 3]> {
    let path = paths.map(&format!("{TEMP_DIR}play_history.csv"));
    parse_history_file(&std::fs::read_to_string(path).ok()?)
}

fn parse_history_file(text: &str) -> Option<[(i32, i32, i32); 3]> {
    let mut out = [(-1, -1, -1); 3];
    let mut lines = text.lines();
    for slot in &mut out {
        let line = lines.next()?;
        let (year_month, count) = line.split_once('/')?;
        *slot = (
            year_month.get(..4)?.trim().parse().ok()?,
            year_month.get(4..)?.trim().parse().ok()?,
            count.trim().parse().ok()?,
        );
    }
    if out.iter().all(|&triple| triple == (-1, -1, -1)) { None } else { Some(out) }
}

fn history_text(history: &[(i32, i32, i32); 3]) -> String {
    let mut text = String::new();
    for &(year, month, count) in history {
        let _ = writeln!(text, "{year:04}{month:02}/{count}");
    }
    text
}

/// `ARLoadLastAccountingReportTime`: the 19-character `YYYY/MM/DD hh:mm:ss` of the report-time file as a `Tm`.
pub fn load_report_time_from(paths: &dyn backend::Fs) -> Option<Tm> {
    let text = std::fs::read_to_string(paths.map(&format!("{TEMP_DIR}last_pras.log"))).ok()?;
    let text = text.get(..19)?;
    let number = |range: std::ops::Range<usize>| text.get(range)?.parse::<i32>().ok();
    Some(Tm {
        sec: number(17..19)?,
        min: number(14..16)?,
        hour: number(11..13)?,
        mday: number(8..10)?,
        mon: number(5..7)? - 1,
        year: number(0..4)? - 1900,
        ..Tm::default()
    })
}

impl Reporter {
    /// The initialisation chain: temp files, the keychip values, the credit-configuration record and the first
    /// report time (`SetFirstAuthTime`: now). `Err` carries the display number of the stop.
    pub fn initialize(&mut self, cx: &mut Cx<'_>, link: &AuthLink, dongle: &mut DriverDongle) -> Result<(), u32> {
        self.trace_resume(dongle);
        self.status = ar_status::INITIALIZING;
        self.host = DEFAULT_HOST.to_owned();
        self.line_limit = DEFAULT_LINE_LIMIT;
        self.near_full_enabled = true;
        self.enabled = true;
        self.start_requested = false;
        self.first_done = false;
        self.term_date_error = false;
        self.start_hour = 0;
        self.start_minute = 0;
        self.receiver = None;
        write_file(cx, "cacert.pem", &[0; 2048]);
        let history = file(cx, "play_history.csv");
        if !history.exists() {
            write_file(cx, "play_history.csv", b"");
        }
        if dongle.counters().is_none() {
            self.stage = Stage::Stopped;
            self.status = -2;
            return Err(949);
        }
        self.restore_trace(cx, dongle)?;
        let (now, _, stamp) = now_of(cx);
        let config = [0u8; 15];
        let record = credit_config_record(&link.identity, stamp, link.date_synced, &config);
        // A full trace log is the one failure the initialisation survives.
        self.trace.put(dongle, record);
        self.next = now;
        self.stage = Stage::WaitTiming;
        self.status = ar_status::WAIT_TIMING;
        Ok(())
    }

    /// Trace-log manager initialise states 0-2 (`0x1403E9E30`): the keychip restore runs only when the `alib.conf`
    /// property `is_restored` is `0` (seeded `1`, so by default it is skipped). A finished restore sets `memrepair`
    /// (reported in every request until a logical erase clears it) and records `is_restored=1` in
    /// `is_restored.conf`. A keychip failure stops the reporter like a missing keychip (`-1` -> status `-2`, 949);
    /// a failed write of the marker is `-5` -> status `-800`.
    fn restore_trace(&mut self, cx: &mut Cx<'_>, dongle: &mut DriverDongle) -> Result<(), u32> {
        let properties = std::fs::read(file(cx, "alib.conf")).unwrap_or_default();
        if !properties::restore_requested(&properties) {
            return Ok(());
        }
        if !dongle.restore_trace() {
            self.stage = Stage::Stopped;
            self.status = -2;
            return Err(949);
        }
        self.mem_repair = 1;
        if try_write_file(cx, "is_restored.conf", b"is_restored=1\n").is_err() {
            self.stage = Stage::Stopped;
            self.status = -800;
            return Err(800);
        }
        Ok(())
    }

    fn trace_resume(&mut self, dongle: &mut DriverDongle) {
        self.trace.resume(dongle);
    }

    /// `ARGetStatus`.
    pub fn status(&self) -> i32 {
        self.status
    }

    /// The first accounting report finished (or was skipped) since boot (`FirstARDone`).
    pub fn first_done(&self) -> bool {
        self.first_done
    }

    /// A certificate date error while the clock is synced (`TermDateError`).
    pub fn term_date_error(&self) -> bool {
        self.term_date_error
    }

    /// Operation enable (`AR+580`), switched by the app status.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// `alpbStartAccountingReport`: accepted only while the status is 1.
    pub fn start_request(&mut self) -> bool {
        if self.stage == Stage::WaitStart && self.status == ar_status::WAIT_START {
            self.start_requested = true;
            true
        } else {
            false
        }
    }

    /// Seconds until the next daily report (`0` when due or unknown).
    pub fn seconds_until(&self, now: i64) -> u32 {
        u32::try_from((self.next - now).max(0)).unwrap_or(u32::MAX)
    }

    /// `ARUpdateState` for one frame.
    pub fn execute(&mut self, cx: &mut Cx<'_>, link: &AuthLink, dongle: &mut DriverDongle) {
        if !self.enabled {
            return;
        }
        match self.stage {
            Stage::Uninitialized | Stage::Stopped => {}
            Stage::WaitTiming => {
                self.status = ar_status::WAIT_TIMING;
                let (now, ..) = now_of(cx);
                if self.next > now {
                    return;
                }
                self.status = ar_status::WAIT_START;
                self.start_requested = false;
                self.stage = Stage::WaitStart;
            }
            Stage::WaitStart => self.wait_start(cx, link, dongle),
            Stage::Running => self.running(cx, link, dongle),
        }
    }

    fn delay_s(link: &AuthLink) -> u32 {
        link.tenpo.map_or(0, |tenpo| schedule::default_delay_s(tenpo, &link.identity.game_id))
    }

    fn wait_start(&mut self, cx: &mut Cx<'_>, link: &AuthLink, dongle: &mut DriverDongle) {
        if !self.start_requested {
            return;
        }
        let (now, civil, stamp) = now_of(cx);
        let Some(next) = schedule::next_fg_time(civil, self.start_hour, self.start_minute, Self::delay_s(link)) else {
            // `SystemDate Year Error!!`: status -800 without stopping.
            self.status = -800;
            return;
        };
        self.next = next;
        self.last_report = now;
        let marker_exists = file(cx, "first_ar.conf").exists();
        let skip = link.server.is_none() || (!self.first_done && marker_exists);
        if skip {
            self.finish(cx, true);
            return;
        }
        self.status = ar_status::BUSY;
        self.stage = Stage::Running;
        // `ARStepInitialize` sub-step 1: the history file is invalidated until the server sends it.
        write_file(cx, "play_history.csv", history_text(&[(-1, -1, -1); 3]).as_bytes());
        self.trace.put(dongle, message_record(&link.identity, stamp, link.date_synced, "Auth start"));
        let Some(counters) = dongle.counters() else {
            self.status = -2;
            self.stage = Stage::Stopped;
            return;
        };
        let ca_pem = pem(&dongle.ca_certificate());
        write_file(cx, "cacert.pem", &ca_pem);
        let records = dongle.trace_records();
        let job = ReportJob {
            role: Role::Foreground,
            first_skipped: false,
            identity: link.identity.clone(),
            server: link.server.unwrap_or(std::net::Ipv4Addr::UNSPECIFIED),
            tenpo: link.tenpo,
            host: self.host.clone(),
            ca_pem,
            counters,
            records,
            line_limit: self.line_limit,
            bias_minutes: link.bias_minutes,
            date: stamp.digits(),
            date_synced: link.date_synced,
            operating_mode: link.operating_mode,
            mem_repair: self.mem_repair,
            serial: self.serial,
        };
        let report_link = ReportLink::new(cx.backend.tls.open());
        let (sender, receiver) = channel();
        self.receiver = Some(receiver);
        std::thread::spawn(move || {
            let mut report_link = report_link;
            // The receiver is gone when libam terminated meanwhile; nothing to deliver then.
            let _ = sender.send(run_report(&job, &mut report_link));
        });
    }

    fn running(&mut self, cx: &mut Cx<'_>, link: &AuthLink, dongle: &mut DriverDongle) {
        let outcome = if let Some(receiver) = &self.receiver {
            match receiver.try_recv() {
                Ok(outcome) => outcome,
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    ReportOutcome { status: -800, result: -1, ..ReportOutcome::default() }
                }
            }
        } else {
            return;
        };
        self.receiver = None;
        self.apply(cx, link, dongle, outcome);
    }

    /// Applies one finished report: erase, signed limits, history, report time and the end-of-report bookkeeping
    /// (`Complete`, `LogicalEraseTraceLog`, `CompleteFinish`).
    fn apply(&mut self, cx: &mut Cx<'_>, link: &AuthLink, dongle: &mut DriverDongle, outcome: ReportOutcome) {
        let (_, _, stamp) = now_of(cx);
        self.serial = outcome.serial;
        self.term_date_error = outcome.term_date_error;
        if outcome.status != ar_status::FINISHED {
            self.status = outcome.status;
            self.stage = Stage::Stopped;
            return;
        }
        let keychip = link.identity.keychip.clone();
        let mut failed = false;
        let public_key = dongle.public_key();
        if let Some((value, signature)) = &outcome.play_limit {
            let stored = sama::verify(&public_key, &keychip, *value, signature).is_ok()
                && sama::decode_signature(signature).is_ok_and(|bytes| dongle.set_play_limit(*value, &bytes));
            failed |= !stored;
        }
        if let Some((value, signature)) = &outcome.near_full {
            let stored = sama::verify(&public_key, &keychip, *value, signature).is_ok()
                && sama::decode_signature(signature).is_ok_and(|bytes| dongle.set_near_full(*value, &bytes));
            failed |= !stored;
        }
        if failed {
            self.status = -2;
            self.stage = Stage::Stopped;
            return;
        }
        if let Some(history) = outcome.history {
            write_file(cx, "play_history.csv", history_text(&history).as_bytes());
        }
        if outcome.result == 0 {
            dongle.erase_oldest(outcome.acknowledged);
            self.mem_repair = 0;
            if outcome.trace_erase {
                dongle.erase_used_sectors();
                let text = format!(
                    "EraseTD {} {} {}",
                    link.identity.keychip, link.identity.game_id, link.identity.game_version
                );
                self.trace.put(dongle, message_record(&link.identity, stamp, link.date_synced, &text));
            }
            write_file(
                cx,
                "last_pras.log",
                format!(
                    "{:04}/{:02}/{:02} {:02}:{:02}:{:02}\n",
                    stamp.year, stamp.month, stamp.day, stamp.hour, stamp.minute, stamp.second
                )
                .as_bytes(),
            );
        }
        for message in &outcome.messages {
            let record: Record = message_record(&link.identity, stamp, link.date_synced, message);
            self.trace.put(dongle, record);
        }
        self.finish(cx, false);
    }

    /// `CompleteFinish` sub-step 3: the first-report marker and status 3.
    fn finish(&mut self, cx: &mut Cx<'_>, skipped: bool) {
        let _ = skipped;
        if !self.first_done {
            self.first_done = true;
            write_file(cx, "first_ar.conf", b"");
        }
        self.stage = Stage::WaitTiming;
        self.status = ar_status::FINISHED;
    }
}
