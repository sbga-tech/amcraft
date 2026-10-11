//! The amsGfetcher engine (`spec/libam/network.md` D.9, Appendix F): the download client behind the NetDeliver
//! manager.
//!
//! The original runs one worker thread that loops `step; sleep 16 ms (service requests); periodic report`. Here
//! the loop is [`Fetcher::step`], called once per frame by the manager; the mailbox (`ReqResume`, `ReqPause`) is
//! serviced at the start of each step like the original's interruptible sleep. Every HTTP exchange goes through
//! the [`HttpClient`] backend interface; the files live below the AMFS directory through [`Fs`].

use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use crate::time::DateTime;

use super::deliver_dli::{Dli, DliImage};
use super::deliver_icf::{Icf, IcfRecord, STATUS_INSTALLED, STATUS_PENDING};
use super::deliver_image::{
    BOOTID_SIZE, BootId, ImageType, SIGNED_SEGMENTS, segment_crc, stored_segment_crc, verify_signature,
};
use backend::{Fs, HttpClient, HttpError, HttpRequest, Method};

#[path = "deliver_report.rs"]
mod report;

pub use report::{Report, parse_signed_url, signed_url_request};

/// Index of the application set / record.
pub const APP: usize = 0;
/// Index of the option set / record.
pub const OPT: usize = 1;

/// Engine status values of a record (D.9.7).
pub mod status {
    /// Initialised / DLI fetched.
    pub const INITIALISED: u32 = 1;
    /// Downloads pending.
    pub const PENDING: u32 = 2;
    /// An item is downloading.
    pub const DOWNLOADING: u32 = 3;
    /// No instruction URL for the kind.
    pub const NO_URL: u32 = 100;
    /// Nothing to install for the application set.
    pub const NOTHING_LISTED: u32 = 101;
    /// Something was queued or released (option).
    pub const OPTION_QUEUED: u32 = 102;
    /// Something was queued or released (application).
    pub const APP_QUEUED: u32 = 103;
    /// All queued images of the kind are downloaded.
    pub const DOWNLOADED: u32 = 110;
    /// Release time not reached.
    pub const NOT_RELEASED: u32 = 120;
    /// Server time not synchronised.
    pub const TIME_UNSYNCED: u32 = 121;
    /// Release due, engine paused.
    pub const PAUSED: u32 = 122;
    /// Release due, waiting for the loader.
    pub const WAIT_LOADER: u32 = 123;
    /// Release due, immediate.
    pub const IMMEDIATE: u32 = 124;
    /// Everything is present.
    pub const UP_TO_DATE: u32 = 130;
    /// No valid last-auth time.
    pub const NO_AUTH: u32 = 200;
    /// Signed URL or DLI GET failed.
    pub const HTTP_FAILED: u32 = 300;
    /// DLI parse failed.
    pub const DLI_FORMAT: u32 = 302;
    /// DLI game id mismatch.
    pub const DLI_GAME_ID: u32 = 303;
    /// Local EXIST image unreadable, ICF out of space.
    pub const PARTS: u32 = 304;
    /// BootId errors.
    pub const BOOTID_OTHER: u32 = 400;
    /// BootId HTTP failure.
    pub const BOOTID_HTTP: u32 = 401;
    /// BootId 404.
    pub const BOOTID_404: u32 = 402;
    /// BootId wrong length.
    pub const BOOTID_LENGTH: u32 = 403;
    /// Download in its retry state (application).
    pub const APP_RETRY: u32 = 500;
    /// Download in its retry state (option).
    pub const OPTION_RETRY: u32 = 501;
    /// Verification or install failure (application).
    pub const APP_INSTALL: u32 = 602;
    /// Verification or install failure (option).
    pub const OPTION_INSTALL: u32 = 603;
    /// DLI temp file write failed.
    pub const DLI_WRITE: u32 = 900;
    /// ICF create/merge/commit failed.
    pub const ICF_FAILED: u32 = 901;
    /// ICF image info missing.
    pub const ICF_MISSING: u32 = 902;
}

/// Engine result codes (the internal ones, before the wrapper remap).
pub mod code {
    /// Busy: the previous request is not finished.
    pub const BUSY: i32 = 1;
    /// Not inited or not running.
    pub const NOT_RUNNING: i32 = -4;
    /// Already paused.
    pub const ALREADY: i32 = -23;
    /// Not ready.
    pub const NOT_READY: i32 = -24;
    /// No data.
    pub const NO_DATA: i32 = -25;
}

/// The 1816-byte auth block of `ReqSetAuthParams` as the manager builds it (D.4.3).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AuthParams {
    /// Seconds since 1970 of the last auth, `-1` when none.
    pub auth_time: i64,
    /// Line type 0..=2.
    pub line_type: u32,
    /// `app|option` instruction URIs.
    pub uri: String,
    /// ALL.Net `auth_data`.
    pub auth_data: String,
    /// `use_aws_signature`.
    pub aws: bool,
}

/// One 36-byte image item (D.6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Item {
    /// State mask: 1 code 1, 2/4 code 2, 8 code 3, 16 unknown.
    pub state: u32,
    /// 0 application image, 1 option image.
    pub kind: u32,
    /// Image timestamp.
    pub timestamp: [u8; 8],
    /// Version of an application image.
    pub version: u32,
    /// Option id characters of an option image.
    pub option_id: [u8; 4],
    /// `image+8` flag of the DLI entry.
    pub flag: u32,
    /// Total segments, `u32::MAX` when unknown.
    pub total: u32,
    /// Segments on disk.
    pub done: u32,
}

/// The status record of one kind (`rec[0..30]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record {
    /// `rec[0]`.
    pub status: u32,
    /// `rec[5..14]`: the image being worked on.
    pub item: Item,
    /// `rec[14..26]`: pair A (three pairs) and pair B (three pairs), `u32::MAX` = unset.
    pub pairs: [(u32, u32); 6],
    /// `rec[26..28]`: DLI `ORDER_TIME`.
    pub order_time: [u8; 8],
    /// `rec[28..30]`: DLI `RELEASE_TIME`.
    pub release_time: [u8; 8],
}

impl Default for Record {
    fn default() -> Self {
        Self {
            status: 0,
            item: Item::default(),
            pairs: [(u32::MAX, u32::MAX); 6],
            order_time: [0; 8],
            release_time: [0; 8],
        }
    }
}

/// What the engine needs from the outside for one step.
pub struct Io<'a> {
    /// The HTTP client.
    pub http: &'a mut dyn HttpClient,
    /// The cabinet path mapping.
    pub paths: &'a dyn Fs,
    /// The AMFS directory as a cabinet path ending in `\`.
    pub base: &'a str,
    /// Monotonic milliseconds.
    pub now_ms: u64,
    /// Local time.
    pub now: DateTime,
    /// Keychip serial (user agent, report `serial`).
    pub serial: &'a str,
    /// The system game id.
    pub game_id: &'a str,
    /// The system platform id (3 characters).
    pub platform_id: &'a str,
    /// The server-time flag of the RTC module.
    pub server_time_synced: bool,
    /// The application-loader count (`0x1402E3450`).
    pub app_loader_count: u32,
    /// The DownloadUi registry mailbox.
    pub download_ui: &'a mut dyn DownloadUi,
}

/// The DownloadUi parameter record the engine hands to the external download UI (`amsDownloadUi*`).
pub trait DownloadUi {
    /// Writes the record; `false` when the registry refuses.
    fn write(&mut self, params: &DownloadUiParams) -> bool;
}

/// The DownloadUi parameters (D.9.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DownloadUiParams {
    /// 0/1/2 for the decision 2/3/4.
    pub initial_state: u32,
    /// Highest application image version.
    pub download_version: u32,
    /// The same version once released, else 0.
    pub update_version: u32,
    /// Always 1.
    pub is_server: u32,
    /// Always 1.
    pub show_warning: u32,
}

/// The worker states (D.9.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Worker {
    Paused,
    FetchDli,
    ReleaseCheck,
    Download,
    Idle,
    Exit,
}

/// Why a GET failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fail {
    NotFound,
    Failed,
    Length,
}

enum Poll<T> {
    Wait,
    Ready(T),
}

#[derive(Debug, Clone)]
struct Job {
    url: String,
    range: Option<(u64, u64)>,
    expect: Option<usize>,
    attempts: u8,
    tried: u8,
    ready_ms: u64,
}

/// One image of a DLI with what is known about it.
#[derive(Debug, Clone)]
struct Entry {
    image: DliImage,
    file: String,
    boot: Option<BootId>,
    /// ICF code: 0 unknown, 1 to download, 2 downloaded, 3 released.
    code: u32,
    done: u32,
}

#[derive(Debug, Default)]
struct Set {
    url: String,
    dli: Option<Dli>,
    entries: Vec<Entry>,
    queue: Vec<usize>,
    release_flag: bool,
}

#[derive(Debug)]
struct Download {
    set: usize,
    entry: usize,
    path: PathBuf,
    segment_size: usize,
    segments: u32,
    done: u32,
    head: Vec<u8>,
    retry_index: usize,
    wait_until: u64,
    /// When the request of the current chunk started (`Downloader` state 0 stamp), kept while it is in flight.
    request_started_ms: Option<u64>,
    bad: u8,
}

/// Back-off between failed segment requests (D.10.1).
const RETRY_WAIT_MS: [u64; 5] = [60_000, 120_000, 240_000, 480_000, 960_000];
/// Wait between download passes (state 3).
const PASS_WAIT_MS: u64 = 3_000;
/// Re-fetch interval of the DLIs, seconds (`UTC now + 1 h`).
const REFRESH_SECONDS: i64 = 3_600;
/// Largest chunk of one request.
const MAX_CHUNK: u64 = 32 * 1024 * 1024;

/// The engine.
#[derive(Debug)]
pub struct Fetcher {
    worker: Worker,
    request: Option<u8>,
    request_done: bool,
    result: i32,
    paused_flag: bool,
    params: AuthParams,
    records: [Record; 2],
    sets: [Set; 2],
    descriptions: [String; 2],
    phase: u8,
    release_flags: [bool; 2],
    exit_reason: u32,
    is_done: u32,
    next_refresh: i64,
    job: Option<Job>,
    cursor: usize,
    refresh: bool,
    any_changed: bool,
    download: Option<Download>,
    wait_until: u64,
    last_report: [i64; 2],
    entry_cursor: [usize; 2],
    now: DateTime,
}

const REQ_EXIT: u8 = 1;
const REQ_PAUSE: u8 = 2;
const REQ_RESUME: u8 = 3;

impl Fetcher {
    /// `amsGfetcherInitEx(0)`: initialised, worker thread running, paused.
    pub fn new(is_done: u32) -> Self {
        Self {
            worker: Worker::Paused,
            request: None,
            request_done: false,
            result: 0,
            paused_flag: true,
            params: AuthParams { auth_time: -1, ..AuthParams::default() },
            records: [Record::default(); 2],
            sets: [Set::default(), Set::default()],
            descriptions: [String::new(), String::new()],
            phase: 0,
            release_flags: [false; 2],
            exit_reason: 0,
            is_done,
            next_refresh: 0,
            job: None,
            cursor: 0,
            refresh: false,
            any_changed: false,
            download: None,
            wait_until: 0,
            last_report: [0; 2],
            entry_cursor: [0; 2],
            now: DateTime::default(),
        }
    }

    /// The worker loop has not ended.
    pub fn is_running(&self) -> bool {
        self.worker != Worker::Exit
    }

    /// `ReqSetAuthParams`: `1` while a request is pending.
    pub fn set_auth_params(&mut self, params: AuthParams) -> i32 {
        if self.request.is_some() {
            return code::BUSY;
        }
        self.params = params;
        0
    }

    fn post(&mut self, kind: u8) -> i32 {
        if !self.is_running() {
            return code::NOT_RUNNING;
        }
        if self.request.is_some() {
            return code::BUSY;
        }
        self.request = Some(kind);
        self.request_done = false;
        0
    }

    /// `ReqResume` (asynchronous).
    pub fn request_resume(&mut self) -> i32 {
        self.post(REQ_RESUME)
    }

    /// `ReqPause` (asynchronous).
    pub fn request_pause(&mut self) -> i32 {
        self.post(REQ_PAUSE)
    }

    /// `ReqExit`: the synchronous exit of `amsGfetcherExit`.
    pub fn exit(&mut self) {
        self.request = Some(REQ_EXIT);
        self.service_request();
        self.request = None;
        self.request_done = false;
    }

    /// `amsGfetcherPump`: latches a completed request.
    pub fn pump(&mut self) -> i32 {
        if !self.is_running() {
            return code::NOT_RUNNING;
        }
        if self.request.is_some() && self.request_done {
            self.request = None;
            self.request_done = false;
        }
        0
    }

    /// `GetResult`: `1` while the request is unanswered or unlatched.
    pub fn result(&self) -> i32 {
        if !self.is_running() {
            code::NOT_RUNNING
        } else if self.request.is_some() {
            code::BUSY
        } else {
            self.result
        }
    }

    /// The status record of `kind`.
    pub fn record(&self, kind: usize) -> &Record {
        &self.records[kind]
    }

    /// `ReqGetExitReason`.
    pub fn exit_reason(&self) -> Result<u32, i32> {
        if self.phase == 2 {
            Ok(self.exit_reason)
        } else if self.phase == 1 {
            let idle = |kind: usize| matches!(self.records[kind].status, 100 | 101);
            if idle(APP) && idle(OPT) { Ok(0) } else { Err(code::NOT_READY) }
        } else {
            Err(code::NO_DATA)
        }
    }

    /// `ReqGetImageInfo`: the items of `kind`.
    pub fn items(&self, kind: usize) -> Vec<Item> {
        let dli = self.sets[kind].dli.as_ref();
        self.sets[kind].entries.iter().map(|entry| item_of(kind, entry, dli, self.now)).collect()
    }

    /// Services the mailbox: the interruptible sleep of the original (F.3).
    fn service_request(&mut self) {
        let Some(kind) = self.request else { return };
        if self.request_done {
            return;
        }
        match kind {
            REQ_EXIT => {
                self.stop_work();
                self.worker = Worker::Exit;
                self.result = 0;
                self.finish_request();
            }
            REQ_PAUSE if self.worker != Worker::Paused => {
                self.stop_work();
                self.worker = Worker::Paused;
                self.result = 0;
                self.finish_request();
            }
            REQ_RESUME if self.worker == Worker::Paused => {
                self.paused_flag = false;
                self.reset();
                self.worker = Worker::FetchDli;
                self.result = 0;
                self.finish_request();
            }
            _ => {
                self.result = code::ALREADY;
                self.request_done = true;
            }
        }
    }

    fn finish_request(&mut self) {
        self.request_done = true;
    }

    /// The field clears of a pause / exit.
    fn stop_work(&mut self) {
        self.paused_flag = true;
        self.phase = 0;
        self.job = None;
        self.download = None;
        self.sets = [Set::default(), Set::default()];
        self.release_flags = [false; 2];
        self.records = [Record::default(); 2];
        self.descriptions = [String::new(), String::new()];
    }

    /// `0x1402F18C0`: both records `status = 1`, everything else cleared.
    fn reset(&mut self) {
        self.stop_work();
        self.paused_flag = false;
        for record in &mut self.records {
            record.status = status::INITIALISED;
        }
        self.entry_cursor = [0; 2];
        self.cursor = 0;
        self.refresh = false;
        self.any_changed = false;
        self.exit_reason = 0;
        self.last_report = [0; 2];
    }

    /// Sets a status unless the current one is sticky (`>= 100`).
    fn set_status(&mut self, kind: usize, value: u32) {
        if self.records[kind].status < 100 {
            self.records[kind].status = value;
        }
    }

    fn set_status_both(&mut self, value: u32) {
        self.set_status(APP, value);
        self.set_status(OPT, value);
    }

    /// One worker iteration: the mailbox, the state handler, the periodic report.
    pub fn step(&mut self, io: &mut Io<'_>) {
        self.now = io.now;
        self.service_request();
        if io.now_ms < self.wait_until {
            return;
        }
        match self.worker {
            Worker::Paused | Worker::Exit => {}
            Worker::FetchDli => self.fetch_dli(io),
            Worker::ReleaseCheck => self.release_check(io),
            Worker::Download => self.download_step(io),
            Worker::Idle => self.idle(io),
        }
        self.periodic_report(io);
    }

    // ----- HTTP -----

    fn http_get(
        &mut self,
        io: &mut Io<'_>,
        url: &str,
        range: Option<(u64, u64)>,
        expect: Option<usize>,
        attempts: u8,
    ) -> Poll<Result<Vec<u8>, Fail>> {
        let job =
            self.job.get_or_insert_with(|| Job { url: url.to_owned(), range, expect, attempts, tried: 0, ready_ms: 0 });
        if io.now_ms < job.ready_ms {
            return Poll::Wait;
        }
        let mut headers = Vec::new();
        if let Some((from, to)) = job.range {
            headers.push(("Range".to_owned(), format!("bytes={from}-{to}")));
        }
        let request = HttpRequest {
            method: Method::Get,
            url: job.url.clone(),
            headers,
            body: Vec::new(),
            timeout: report::REQUEST_TIMEOUT,
        };
        let outcome: Result<Vec<u8>, Fail> = match io.http.request(&request) {
            Ok(response) if (200..300).contains(&response.status) => match job.expect {
                Some(length) if response.body.len() != length => Err(Fail::Length),
                _ => Ok(response.body),
            },
            Ok(response) if response.status == 404 => Err(Fail::NotFound),
            Ok(_) | Err(HttpError::Connect | HttpError::Timeout | HttpError::Transfer(_)) => {
                job.tried += 1;
                if job.tried < job.attempts {
                    job.ready_ms = io.now_ms + 60_000;
                    return Poll::Wait;
                }
                Err(Fail::Failed)
            }
        };
        self.job = None;
        Poll::Ready(outcome)
    }

    /// The signed URL of `url` when the config and the auth data ask for it (`getSignedUrl`).
    fn signed(&self, io: &mut Io<'_>, url: &str) -> Result<String, ()> {
        if !self.params.aws || self.params.auth_data.is_empty() {
            return Ok(url.to_owned());
        }
        let request = signed_url_request(&self.params.auth_data, url);
        match io.http.request(&request) {
            Ok(response) if (200..300).contains(&response.status) => parse_signed_url(&response.body).ok_or(()),
            _ => Err(()),
        }
    }

    fn path(io: &Io<'_>, name: &str) -> PathBuf {
        io.paths.map(&format!("{}{name}", io.base))
    }

    // ----- state 1: fetch the DLIs -----

    fn fetch_dli(&mut self, io: &mut Io<'_>) {
        if self.cursor == 0 {
            let (left, right) = self.params.uri.split_once('|').unwrap_or((self.params.uri.as_str(), ""));
            let (left, right) = (left.to_owned(), right.to_owned());
            self.sets[APP].url = left;
            self.sets[OPT].url = right;
        }
        if self.cursor < 2 {
            let kind = self.cursor;
            match self.get_dli(io, kind) {
                Poll::Wait => {}
                Poll::Ready(changed) => {
                    self.any_changed |= changed;
                    self.cursor += 1;
                }
            }
            return;
        }
        // Delete the temporary files, arm the re-fetch.
        for name in ["tmpDliApp.ini", "tmpDliOpt.ini"] {
            let _ = fs::remove_file(Self::path(io, name));
        }
        self.next_refresh = utc_seconds(io.now) + REFRESH_SECONDS;
        let have = self.sets.iter().any(|set| set.dli.is_some());
        self.cursor = 0;
        let was_refresh = std::mem::take(&mut self.refresh);
        if have && (!was_refresh || self.any_changed) {
            self.any_changed = false;
            self.worker = Worker::ReleaseCheck;
            self.cursor = 0;
        } else {
            self.any_changed = false;
            self.worker = Worker::Idle;
            if !have {
                self.phase = 0;
            }
        }
    }

    fn dli_file(kind: usize) -> (&'static str, &'static str) {
        if kind == APP { ("dliApp.ini", "tmpDliApp.ini") } else { ("dliOpt.ini", "tmpDliOpt.ini") }
    }

    /// `getDliFromHttp` followed by the backup fallback: `Ready(changed)` when the kind is settled.
    fn get_dli(&mut self, io: &mut Io<'_>, kind: usize) -> Poll<bool> {
        let (saved, temp) = Self::dli_file(kind);
        if self.params.auth_time <= 0 {
            self.set_status_both(status::NO_AUTH);
            return Poll::Ready(self.load_backup(io, kind));
        }
        let url = self.sets[kind].url.clone();
        if url.is_empty() {
            self.set_status(kind, status::NO_URL);
            return Poll::Ready(self.load_backup(io, kind));
        }
        let url = match self.signed(io, &url) {
            Ok(url) => url,
            Err(()) => {
                self.set_status(kind, status::HTTP_FAILED);
                return Poll::Ready(self.load_backup(io, kind));
            }
        };
        let body = match self.http_get(io, &url, None, None, 3) {
            Poll::Wait => return Poll::Wait,
            Poll::Ready(Ok(body)) => body,
            Poll::Ready(Err(_)) => {
                self.set_status(kind, status::HTTP_FAILED);
                return Poll::Ready(self.load_backup(io, kind));
            }
        };
        let body = body.strip_prefix(&[0xEF, 0xBB, 0xBF][..]).unwrap_or(&body).to_vec();
        if fs::write(Self::path(io, temp), &body).is_err() {
            self.set_status(kind, status::DLI_WRITE);
            return Poll::Ready(self.load_backup(io, kind));
        }
        if self.sets[kind].dli.is_some() && fs::read(Self::path(io, saved)).is_ok_and(|old| old == body) {
            return Poll::Ready(false);
        }
        match Dli::parse(&body, io.game_id) {
            Ok(dli) => {
                let _ = fs::rename(Self::path(io, temp), Self::path(io, saved));
                self.install_dli(io, kind, dli);
                self.records[kind].status = status::INITIALISED;
                Poll::Ready(true)
            }
            Err(super::deliver_dli::DliError::GameId) => {
                self.set_status(kind, status::DLI_GAME_ID);
                Poll::Ready(self.load_backup(io, kind))
            }
            Err(super::deliver_dli::DliError::Format(_)) => {
                self.set_status(kind, status::DLI_FORMAT);
                Poll::Ready(self.load_backup(io, kind))
            }
        }
    }

    /// `getBackupedDli`: the DLI saved by an earlier run.
    fn load_backup(&mut self, io: &mut Io<'_>, kind: usize) -> bool {
        let (saved, _) = Self::dli_file(kind);
        let Ok(bytes) = fs::read(Self::path(io, saved)) else { return false };
        match Dli::parse(&bytes, io.game_id) {
            Ok(dli) => {
                self.install_dli(io, kind, dli);
                true
            }
            Err(_) => false,
        }
    }

    fn install_dli(&mut self, io: &mut Io<'_>, kind: usize, dli: Dli) {
        let _ = io;
        self.descriptions[kind] = dli.game_desc.chars().take(2047).collect();
        self.records[kind].order_time = timestamp_of(dli.order_time);
        self.records[kind].release_time = timestamp_of(dli.release_time);
        let entries = dli
            .images
            .iter()
            .filter(|image| image_matches_set(image, kind))
            .map(|image| Entry { file: file_name(&image.location), image: image.clone(), boot: None, code: 0, done: 0 })
            .collect();
        let set = &mut self.sets[kind];
        set.entries = entries;
        set.dli = Some(dli);
        set.queue.clear();
        set.release_flag = false;
    }

    // ----- state 2: release check -----

    fn release_check(&mut self, io: &mut Io<'_>) {
        // BootIds of the INSTALL images first, one request per frame.
        if self.cursor < 2 {
            let kind = self.cursor;
            match self.collect_boot_ids(io, kind) {
                Poll::Wait => {}
                Poll::Ready(true) => self.cursor += 1,
                Poll::Ready(false) => {
                    self.worker = Worker::Idle;
                    self.cursor = 0;
                    self.phase = 1;
                }
            }
            return;
        }
        self.cursor = 0;
        let Some(mut icf) = self.load_icf(io) else {
            self.set_status_both(status::ICF_MISSING);
            self.worker = Worker::Idle;
            return;
        };
        if !self.merge_icf(io, &mut icf) {
            self.set_status_both(status::PARTS);
            self.worker = Worker::Idle;
            return;
        }
        if !Self::store_icf(io, &icf) {
            self.set_status_both(status::ICF_FAILED);
            self.worker = Worker::Idle;
            return;
        }
        self.decide(io);
    }

    /// `getBootId` for every entry of `kind`: `Ready(true)` when done, `Ready(false)` when an error settled the
    /// record.
    fn collect_boot_ids(&mut self, io: &mut Io<'_>, kind: usize) -> Poll<bool> {
        let total = self.sets[kind].entries.len();
        while self.sets[kind].entries.get(self.cursor_entry(kind)).is_some_and(|entry| entry.boot.is_some()) {
            self.bump_entry(kind);
        }
        let index = self.cursor_entry(kind);
        if index >= total {
            self.reset_entry_cursor(kind);
            return Poll::Ready(true);
        }
        let entry = self.sets[kind].entries[index].clone();
        let outcome: Result<BootId, u32> = if entry.image.exist_only {
            Self::local_boot_id(io, &entry.file).ok_or(status::PARTS)
        } else {
            let url = if self.job.is_some() {
                String::new()
            } else {
                match self.signed(io, &entry.image.location) {
                    Ok(url) => url,
                    Err(()) => {
                        self.set_status(kind, status::HTTP_FAILED);
                        self.reset_entry_cursor(kind);
                        return Poll::Ready(false);
                    }
                }
            };
            match self.http_get(io, &url, Some((0, BOOTID_SIZE as u64 - 1)), Some(BOOTID_SIZE), 4) {
                Poll::Wait => return Poll::Wait,
                Poll::Ready(Ok(bytes)) => BootId::decode(&bytes).map_err(|_| status::BOOTID_OTHER),
                Poll::Ready(Err(Fail::NotFound)) => Err(status::BOOTID_404),
                Poll::Ready(Err(Fail::Failed)) => Err(status::BOOTID_HTTP),
                Poll::Ready(Err(Fail::Length)) => Err(status::BOOTID_LENGTH),
            }
        };
        let valid = outcome
            .and_then(|boot| if Self::boot_matches(io, &entry, &boot) { Ok(boot) } else { Err(status::BOOTID_OTHER) });
        match valid {
            Ok(boot) => {
                self.sets[kind].entries[index].boot = Some(boot);
                self.bump_entry(kind);
                Poll::Wait
            }
            Err(code) => {
                self.set_status(kind, code);
                self.reset_entry_cursor(kind);
                Poll::Ready(false)
            }
        }
    }

    fn cursor_entry(&self, kind: usize) -> usize {
        self.entry_cursor[kind]
    }

    fn bump_entry(&mut self, kind: usize) {
        self.entry_cursor[kind] += 1;
    }

    fn reset_entry_cursor(&mut self, kind: usize) {
        self.entry_cursor[kind] = 0;
    }

    /// The BootId of a local image file (`0x1402EF530`).
    fn local_boot_id(io: &Io<'_>, file: &str) -> Option<BootId> {
        let mut file = fs::File::open(Self::path(io, file)).ok()?;
        let mut block = vec![0u8; BOOTID_SIZE];
        file.read_exact(&mut block).ok()?;
        BootId::decode(&block).ok()
    }

    /// `isValidBootId` (`0x1402F0F00`): the platform id, the game id and the file-name information.
    ///
    /// The file-name comparison covers the image type, the timestamp and, for application and OS images, the
    /// version [INFERENCE: the binary compares two words whose meaning is open item D.O4].
    fn boot_matches(io: &Io<'_>, entry: &Entry, boot: &BootId) -> bool {
        let platform = io.platform_id.as_bytes();
        if platform.len() >= 3 && boot.platform_id[..] != platform[..3] {
            return false;
        }
        let game = io.game_id.as_bytes();
        if boot.has_game_id != 0 && game.len() >= 4 && boot.game_id[..] != game[..4] {
            return false;
        }
        let name = &entry.image.name;
        ImageType::from_byte(boot.image_type) == Some(name.kind)
            && boot.timestamp[..7] == name.timestamp[..7]
            && (name.kind == ImageType::Option || boot.version == name.version)
    }

    // ----- the ICF -----

    /// `ICF1`, then `ICF2`; a fresh catalogue when neither file exists, `None` when they exist but are corrupt.
    fn load_icf(&self, io: &Io<'_>) -> Option<Icf> {
        let mut found = false;
        for name in ["ICF1", "ICF2"] {
            if let Ok(bytes) = fs::read(Self::path(io, name)) {
                found = true;
                if let Ok(icf) = Icf::decode(&bytes) {
                    return Some(icf);
                }
            }
        }
        if found {
            return None;
        }
        let mut icf = Icf::default();
        fill(&mut icf.game_id, io.game_id.as_bytes());
        fill(&mut icf.hardware, io.platform_id.as_bytes());
        Some(icf)
    }

    /// Commits the catalogue to `ICF1` and `ICF2`.
    fn store_icf(io: &Io<'_>, icf: &Icf) -> bool {
        let bytes = icf.encode();
        ["ICF1", "ICF2"].into_iter().all(|name| {
            let path = Self::path(io, name);
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            fs::write(path, &bytes).is_ok()
        })
    }

    /// `MergeIcfCreatedByDliIntoOriginalIcf` and `updateDliFromIcf`: registers the DLI images in the catalogue and
    /// derives each image's code. `false` when the catalogue is full.
    fn merge_icf(&mut self, io: &Io<'_>, icf: &mut Icf) -> bool {
        for set in &mut self.sets {
            for entry in &mut set.entries {
                let Some(boot) = entry.boot.clone() else { continue };
                let segments = boot.segment_count as u32;
                if entry.image.exist_only {
                    entry.code = 3;
                    entry.done = segments;
                    continue;
                }
                let path = Self::path(io, &entry.file);
                let name = &entry.image.name;
                let probe =
                    IcfRecord::new(STATUS_PENDING, 2, name.kind as u8, name.generation, name.version, name.timestamp);
                // `checkLatestImages`: a file whose BootId changed on the server is deleted and queued again.
                let changed = Self::local_boot_id(io, &entry.file).is_some_and(|local| local != boot);
                if changed {
                    let _ = fs::remove_file(&path);
                }
                let size = boot.segment_size.max(1);
                let on_disk = fs::metadata(&path).map_or(0, |meta| meta.len());
                let complete = on_disk == boot.segment_count.saturating_mul(size);
                let code = match icf.records.iter().position(|known| known.same_image(&probe)) {
                    Some(index) => {
                        let known = &mut icf.records[index];
                        let released = known.status() == STATUS_INSTALLED && known.slot() == 1;
                        if !released && (changed || (known.status() == STATUS_INSTALLED && !complete)) {
                            known.set_status(STATUS_PENDING);
                        }
                        if released {
                            3
                        } else if known.status() == STATUS_INSTALLED {
                            2
                        } else {
                            1
                        }
                    }
                    None => {
                        if !icf.add(probe) {
                            return false;
                        }
                        1
                    }
                };
                entry.code = code;
                entry.done = if code == 1 { (on_disk / size).min(boot.segment_count) as u32 } else { segments };
            }
        }
        true
    }

    /// The end of state 2: the per-set release checks, the download queues, the exit reason (`0x14030F1D0`).
    fn decide(&mut self, io: &mut Io<'_>) {
        let paused = self.paused_flag;
        let mut immediate = false;
        for kind in [APP, OPT] {
            let (listed, waiting, release, force) = {
                let set = &mut self.sets[kind];
                set.release_flag = false;
                let Some(dli) = set.dli.as_ref() else {
                    set.queue.clear();
                    continue;
                };
                let (release, force) = (dli.release_time, dli.immediate_release);
                let mut queue: Vec<usize> =
                    (0..set.entries.len()).filter(|&index| set.entries[index].code == 1).collect();
                queue.sort_by_key(|&index| set.entries[index].image.optional);
                let waiting = set.entries.iter().any(|entry| entry.code == 2);
                let listed = !set.entries.is_empty();
                set.queue = queue;
                (listed, waiting, release, force)
            };
            let queued = !self.sets[kind].queue.is_empty();
            let due = if !listed {
                Some(if kind == APP { status::NOTHING_LISTED } else { status::UP_TO_DATE })
            } else if queued {
                None
            } else if !waiting {
                Some(status::UP_TO_DATE)
            } else if io.now < release {
                Some(status::NOT_RELEASED)
            } else if !io.server_time_synced {
                Some(status::TIME_UNSYNCED)
            } else if paused {
                Some(status::PAUSED)
            } else if force || io.app_loader_count == 1 {
                self.sets[kind].release_flag = true;
                immediate = true;
                Some(status::IMMEDIATE)
            } else {
                self.sets[kind].release_flag = true;
                Some(status::WAIT_LOADER)
            };
            if let Some(value) = due {
                self.set_status(kind, value);
            }
        }
        self.release_flags = [self.sets[APP].release_flag, self.sets[OPT].release_flag];
        let second_group = self.sets.iter().any(|set| set.queue.iter().any(|&index| set.entries[index].image.optional));
        let decision = if immediate {
            1
        } else if self.is_done != 0 {
            0
        } else if second_group {
            2
        } else if self.release_flags[APP] {
            4
        } else if self.release_flags[OPT] {
            3
        } else {
            0
        };
        self.exit_reason = match decision {
            0 => 0,
            1 => 1,
            _ => 2,
        };
        self.phase = 2;
        self.refresh_records();
        if decision != 0 {
            let params = self.download_ui_params(io, decision);
            if io.download_ui.write(&params) {
                self.set_status(APP, status::APP_QUEUED);
                self.set_status(OPT, status::OPTION_QUEUED);
            } else {
                self.set_status_both(status::ICF_MISSING);
            }
            self.worker = Worker::Idle;
        } else if self.sets.iter().any(|set| !set.queue.is_empty()) {
            self.worker = Worker::Download;
        } else {
            self.worker = Worker::Idle;
        }
    }

    fn download_ui_params(&self, io: &Io<'_>, decision: u32) -> DownloadUiParams {
        let app = &self.sets[APP];
        let version = app
            .entries
            .iter()
            .filter(|entry| entry.image.name.kind == ImageType::App)
            .map(|entry| entry.image.name.version)
            .max()
            .unwrap_or(0);
        let released = app.dli.as_ref().is_some_and(|dli| io.now >= dli.release_time);
        DownloadUiParams {
            initial_state: decision.saturating_sub(2),
            download_version: version,
            update_version: if io.server_time_synced && released { version } else { 0 },
            is_server: 1,
            show_warning: 1,
        }
    }

    /// `0x1402F2280` / `0x1402F2510`: the working item and the six progress pairs of both records (D.6.2).
    fn refresh_records(&mut self) {
        for kind in [APP, OPT] {
            let items = self.items(kind);
            let mut pairs = [(u32::MAX, u32::MAX); 6];
            if self.sets[kind].dli.is_some() {
                let sums = |keep: &dyn Fn(&Item) -> bool| {
                    let mut total = 0u32;
                    let mut done = 0u32;
                    let mut complete = 0u32;
                    for item in items.iter().filter(|item| keep(item)) {
                        let size = if item.total == u32::MAX { 0 } else { item.total };
                        total += size;
                        done += item.done;
                        if size > 0 && item.done == size {
                            complete += 1;
                        }
                    }
                    (total, done, complete)
                };
                let unstarted = |item: &Item| item.state == 1 || item.state == 2;
                let (total, done, complete) = sums(&|_| true);
                pairs[0] = (total, done);
                let (total, done, _) = sums(&unstarted);
                pairs[1] = (total, done);
                pairs[2] = (u32::MAX, complete);
                if kind == OPT {
                    let (total, done, complete) = sums(&|item| item.flag != 0);
                    pairs[3] = (total, done);
                    let (total, done, _) = sums(&|item| item.flag != 0 && unstarted(item));
                    pairs[4] = (total, done);
                    pairs[5] = (u32::MAX, complete);
                }
            }
            let working = match &self.download {
                Some(download) if download.set == kind => Some(download.entry),
                _ => self.sets[kind].queue.first().copied(),
            };
            let record = &mut self.records[kind];
            record.pairs = pairs;
            record.item = working.and_then(|index| items.get(index)).or(items.first()).copied().unwrap_or_default();
        }
    }

    // ----- state 3: the download loop -----

    /// Sets a record to a download status (`< 100`, or the transient 500/501).
    fn set_download_status(&mut self, kind: usize, value: u32) {
        let current = self.records[kind].status;
        if current < 100 || current == status::APP_RETRY || current == status::OPTION_RETRY {
            self.records[kind].status = value;
        }
    }

    /// Ends the download pass with `value` on every record that is still below 100 (state 5, `0x1402EDDC0`).
    fn abandon(&mut self, value: u32) {
        self.download = None;
        self.job = None;
        for record in &mut self.records {
            if record.status < 100 || record.status == status::APP_RETRY || record.status == status::OPTION_RETRY {
                record.status = value;
            }
        }
        self.refresh_records();
        self.worker = Worker::Idle;
    }

    fn download_step(&mut self, io: &mut Io<'_>) {
        if self.download.is_none() && !self.start_download(io) {
            return;
        }
        let Some((kind, index, segment, done, segments)) = self.download.as_ref().and_then(|download| {
            (io.now_ms >= download.wait_until).then_some((
                download.set,
                download.entry,
                download.segment_size as u64,
                download.done,
                download.segments,
            ))
        }) else {
            return;
        };
        let (app_value, option_value) = (status::APP_INSTALL, status::OPTION_INSTALL);
        let failure = if kind == APP { app_value } else { option_value };
        let transient = if kind == APP { status::APP_RETRY } else { status::OPTION_RETRY };
        let Some(dli) = self.sets[kind].dli.as_ref() else {
            self.abandon(failure);
            return;
        };
        let line = (self.params.line_type as usize).min(2);
        let civil = io.now.civil();
        let interval = dli.interval_ms(line, civil.hour, civil.minute);
        let chunk = u64::from(dli.part_size[line]);
        if interval < 0 {
            // No download in this slot: the record stays 2 and the pass is tried again.
            self.set_download_status(kind, status::PENDING);
            self.wait_until = io.now_ms + PASS_WAIT_MS;
            return;
        }
        let count = (chunk / segment).clamp(1, u64::from(segments - done));
        let from = u64::from(done) * segment;
        let length = count * segment;
        let url = if self.job.is_some() {
            String::new()
        } else {
            let location = self.sets[kind].entries[index].image.location.clone();
            match self.signed(io, &location) {
                Ok(url) => url,
                Err(()) => {
                    self.abandon(transient);
                    return;
                }
            }
        };
        if let Some(dl) = self.download.as_mut() {
            dl.request_started_ms.get_or_insert(io.now_ms);
        }
        let outcome = match self.http_get(io, &url, Some((from, from + length - 1)), Some(length as usize), 1) {
            Poll::Wait => return,
            Poll::Ready(outcome) => outcome,
        };
        let Some(mut dl) = self.download.take() else { return };
        let started_ms = dl.request_started_ms.take().unwrap_or(io.now_ms);
        let Ok(data) = outcome else {
            dl.retry_index += 1;
            if dl.retry_index > RETRY_WAIT_MS.len() {
                self.abandon(transient);
            } else {
                dl.wait_until = io.now_ms + RETRY_WAIT_MS[dl.retry_index - 1];
                if self.records[kind].status < 100 {
                    self.records[kind].status = transient;
                }
                self.download = Some(dl);
            }
            return;
        };
        if !Self::verify_chunk(&mut dl, &data) {
            dl.bad += 1;
            if dl.bad > 2 {
                self.abandon(failure);
            } else {
                dl.wait_until = io.now_ms + PASS_WAIT_MS;
                self.download = Some(dl);
            }
            return;
        }
        if let Err(error) = write_segments(&dl.path, from, &data) {
            let value = if error.kind() == std::io::ErrorKind::StorageFull { 600 } else { failure };
            self.abandon(value);
            return;
        }
        dl.done += count as u32;
        dl.retry_index = 0;
        dl.bad = 0;
        self.sets[kind].entries[index].done = dl.done;
        self.set_download_status(kind, status::DOWNLOADING);
        if dl.done < dl.segments {
            // State 3: wait `max(interval - elapsed, interval / 4)` ms since the request started.
            if interval > 0 {
                let elapsed = i64::try_from(io.now_ms.saturating_sub(started_ms)).unwrap_or(i64::MAX);
                let wait = (i64::from(interval) - elapsed).max(i64::from(interval / 4));
                dl.wait_until = io.now_ms + wait.unsigned_abs();
            }
            self.download = Some(dl);
            self.refresh_records();
            return;
        }
        self.finish_image(io, dl);
    }

    /// Checks every segment of a received chunk against the CRC table of segment 0 and, once the eighth segment is
    /// in, the signature (`0x140315C80`, `0x140315F00`). The head buffer grows only when the chunk is good.
    fn verify_chunk(dl: &mut Download, data: &[u8]) -> bool {
        let size = dl.segment_size;
        if data.is_empty() || !data.len().is_multiple_of(size) {
            return false;
        }
        let kept = dl.head.len();
        for (offset, piece) in data.chunks(size).enumerate() {
            let index = dl.done as usize + offset;
            if index < SIGNED_SEGMENTS && dl.head.len() == index * size {
                dl.head.extend_from_slice(piece);
            }
            let good = stored_segment_crc(&dl.head, index) == Some(segment_crc(index, piece))
                && (index != SIGNED_SEGMENTS - 1 || verify_signature(&dl.head, size));
            if !good {
                dl.head.truncate(kept);
                return false;
            }
        }
        true
    }

    /// `downloadOne` returned 0: the ICF entry is installed, the queue pops.
    fn finish_image(&mut self, io: &mut Io<'_>, dl: Download) {
        let kind = dl.set;
        let name = {
            let entry = &mut self.sets[kind].entries[dl.entry];
            entry.code = 2;
            entry.done = dl.segments;
            entry.image.name.clone()
        };
        let committed = self.load_icf(io).is_some_and(|mut icf| {
            let probe =
                IcfRecord::new(STATUS_PENDING, 2, name.kind as u8, name.generation, name.version, name.timestamp);
            if let Some(known) = icf.records.iter_mut().find(|known| known.slot() == 2 && known.same_image(&probe)) {
                known.set_status(STATUS_INSTALLED);
            }
            Self::store_icf(io, &icf)
        });
        if !committed {
            self.abandon(status::ICF_FAILED);
            return;
        }
        self.sets[kind].queue.retain(|&index| index != dl.entry);
        if self.sets[kind].queue.is_empty() {
            self.records[kind].status = status::DOWNLOADED;
        }
        self.refresh_records();
        self.wait_until = io.now_ms + PASS_WAIT_MS;
        if self.sets.iter().all(|set| set.queue.is_empty()) {
            self.worker = Worker::ReleaseCheck;
            self.cursor = 0;
        }
    }

    /// Picks the next queue entry and verifies what is already on disk; `false` when there is nothing to request
    /// in this step.
    fn start_download(&mut self, io: &mut Io<'_>) -> bool {
        if self.sets.iter().all(|set| set.queue.is_empty()) {
            self.worker = Worker::ReleaseCheck;
            self.cursor = 0;
            return false;
        }
        for kind in [APP, OPT] {
            if !self.sets[kind].queue.is_empty() {
                self.set_download_status(kind, status::PENDING);
            }
        }
        // The common option queue always runs; the others wait for the set's ORDER_TIME.
        let ready = [APP, OPT].into_iter().find(|&kind| {
            let set = &self.sets[kind];
            let Some(&first) = set.queue.first() else { return false };
            (kind == OPT && !set.entries[first].image.optional)
                || set.dli.as_ref().is_some_and(|dli| io.now >= dli.order_time)
        });
        let Some(kind) = ready else {
            self.wait_until = io.now_ms + PASS_WAIT_MS;
            return false;
        };
        let index = self.sets[kind].queue[0];
        let entry = &self.sets[kind].entries[index];
        let failure = if kind == APP { status::APP_INSTALL } else { status::OPTION_INSTALL };
        let Some(boot) = entry.boot.as_ref() else {
            self.abandon(failure);
            return false;
        };
        let path = Self::path(io, &entry.file);
        let (segment_size, segments) = (boot.segment_size, boot.segment_count);
        if segment_size < BOOTID_SIZE as u64
            || segment_size > MAX_CHUNK
            || segments == 0
            || segments > u64::from(u32::MAX)
        {
            self.abandon(failure);
            return false;
        }
        let (segment_size, segments) = (segment_size as usize, segments as u32);
        let (done, head) = verify_existing(&path, segment_size, segments);
        self.set_download_status(kind, status::DOWNLOADING);
        self.sets[kind].entries[index].done = done;
        let download = Download {
            set: kind,
            entry: index,
            path,
            segment_size,
            segments,
            done,
            head,
            retry_index: 0,
            wait_until: 0,
            request_started_ms: None,
            bad: 0,
        };
        self.refresh_records();
        if done >= segments {
            self.finish_image(io, download);
            return false;
        }
        self.download = Some(download);
        true
    }

    // ----- state 5 and the report -----

    /// The periodic DLI re-fetch (`UTC now + 1 h`).
    fn idle(&mut self, io: &mut Io<'_>) {
        if self.next_refresh > 0 && utc_seconds(io.now) >= self.next_refresh {
            self.refresh = true;
            self.cursor = 0;
            self.worker = Worker::FetchDli;
        }
    }

    /// The four version texts of a report: newest application and OS image waiting for release / otherwise.
    fn versions(&self, kind: usize) -> [String; 4] {
        let newest = |waiting: bool, image: ImageType| {
            let version = self.sets[kind]
                .entries
                .iter()
                .filter(|entry| entry.image.name.kind == image && (entry.code == 2) == waiting)
                .map(|entry| entry.image.name.version)
                .max();
            version.map_or_else(
                || "0".to_owned(),
                |version| format!("{}.{:02}.{:02}", version >> 16, (version >> 8) & 0xFF, version & 0xFF),
            )
        };
        [
            newest(true, ImageType::App),
            newest(false, ImageType::App),
            newest(true, ImageType::Os),
            newest(false, ImageType::Os),
        ]
    }

    /// `deliverReportSend` every `REPORT_INTERVAL` seconds per set, once authenticated and past status 1.
    fn periodic_report(&mut self, io: &mut Io<'_>) {
        if self.params.auth_time <= 0 {
            return;
        }
        let now = utc_seconds(io.now);
        for kind in [APP, OPT] {
            let Some(dli) = self.sets[kind].dli.as_ref() else { continue };
            if dli.report_interval == 0
                || dli.report_url.is_empty()
                || self.records[kind].status == status::INITIALISED
                || now - self.last_report[kind] < i64::from(dli.report_interval)
            {
                continue;
            }
            let [dav, wdav, dov, wdov] = self.versions(kind);
            let files = |waiting: bool| -> Vec<String> {
                self.sets[kind]
                    .entries
                    .iter()
                    .filter(|entry| (entry.code == 2) == waiting)
                    .map(|entry| entry.file.clone())
                    .collect()
            };
            let record = &self.records[kind];
            let report = Report {
                option: kind == OPT,
                serial: io.serial.to_owned(),
                rf_state: record.status,
                auth_time: self.params.auth_time,
                order_time: utc_seconds(dli.order_time),
                release_time: utc_seconds(dli.release_time),
                segments: record.pairs[1],
                dav,
                wdav,
                dov,
                wdov,
                game_desc: self.descriptions[kind].clone(),
                waiting_release: files(true),
                others: files(false),
                auth_data: self.params.auth_data.clone(),
            };
            let request = report.request(&dli.report_url);
            let _ = io.http.request(&request);
            self.last_report[kind] = now;
        }
    }
}

/// The item the engine publishes for one image (`updateImageInfoListByDli`, D.6.3).
fn item_of(kind: usize, entry: &Entry, dli: Option<&Dli>, now: DateTime) -> Item {
    let name = &entry.image.name;
    let state = match (dli, entry.code) {
        (None, _) => 16,
        (Some(_), 1) => 1,
        (Some(dli), 2) => {
            if now >= dli.release_time {
                4
            } else {
                2
            }
        }
        (Some(_), 3) => 8,
        (Some(_), _) => 16,
    };
    let option = kind == OPT;
    Item {
        state,
        kind: u32::from(option),
        timestamp: name.timestamp,
        version: if option { 0 } else { name.version },
        option_id: if option { name.version.to_be_bytes() } else { [0; 4] },
        flag: u32::from(entry.image.optional),
        total: entry.boot.as_ref().map_or(u32::MAX, |boot| boot.segment_count as u32),
        done: entry.done,
    }
}

/// Seconds since 1970 of a [`DateTime`]; the cabinet clock is taken as UTC.
pub fn utc_seconds(time: DateTime) -> i64 {
    const EPOCH_1970: i64 = 719_162 * 86_400_000_000;
    (time.0 - EPOCH_1970).div_euclid(1_000_000)
}

/// `{u16 year, month, day, hour, minute, second, 0}` of a [`DateTime`].
fn timestamp_of(time: DateTime) -> [u8; 8] {
    let civil = time.civil();
    let year = (civil.year as u16).to_le_bytes();
    [year[0], year[1], civil.month as u8, civil.day as u8, civil.hour as u8, civil.minute as u8, civil.second as u8, 0]
}

/// The last path component of a URL without its query.
fn file_name(location: &str) -> String {
    let path = location.split(['?', '#']).next().unwrap_or(location);
    path.rsplit(['/', '\\']).next().unwrap_or(path).to_owned()
}

/// The application set holds `.pack` and `.app` images, the option set `.opt` images.
fn image_matches_set(image: &DliImage, kind: usize) -> bool {
    (image.name.kind == ImageType::Option) == (kind == OPT)
}

fn fill(target: &mut [u8], source: &[u8]) {
    let length = target.len().min(source.len());
    target[..length].copy_from_slice(&source[..length]);
}

/// Writes `data` at `offset` of `path`, creating the file (`Installer::step`).
fn write_segments(path: &std::path::Path, offset: u64, data: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).truncate(false).write(true).open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(data)?;
    file.sync_data()
}

/// `0x1402ECE60`: how many leading segments of an existing partial file are intact, and the head bytes read.
/// A file that is not a whole number of segments, or has no good segment, is deleted.
fn verify_existing(path: &std::path::Path, size: usize, segments: u32) -> (u32, Vec<u8>) {
    let Ok(mut file) = fs::File::open(path) else { return (0, Vec::new()) };
    let length = file.metadata().map_or(0, |meta| meta.len());
    let present = (length / size as u64).min(u64::from(segments)) as usize;
    let mut head = Vec::new();
    let mut piece = vec![0u8; size];
    let mut good = 0;
    if length % size as u64 == 0 {
        for index in 0..present {
            if file.read_exact(&mut piece).is_err() {
                break;
            }
            if index < SIGNED_SEGMENTS {
                head.extend_from_slice(&piece);
            }
            if stored_segment_crc(&head, index) != Some(segment_crc(index, &piece))
                || (index == SIGNED_SEGMENTS - 1 && !verify_signature(&head, size))
            {
                break;
            }
            good = index + 1;
        }
    }
    drop(file);
    if good == 0 {
        let _ = fs::remove_file(path);
        return (0, Vec::new());
    }
    head.truncate(good.min(SIGNED_SEGMENTS) * size);
    (good as u32, head)
}
