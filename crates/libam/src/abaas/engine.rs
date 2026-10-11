//! The libabaas manager behind the log manager: configuration, the scheduler and the six sequencers
//! (`spec/libam/storage.md` 6.7, 6.9).
//!
//! One [`Engine::update`] is one `Manager::update` (`0x140369C50`): the running sequencer advances by exactly one
//! step (one directory entry of the file search, one file read, one encode, one HTTP transaction, one decode), then
//! the scheduler decides what starts next. The HTTP transaction goes through the backend's `HttpClient`, so it
//! is synchronous here.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::time::Duration;

use super::files::{self, FileEntry, Search, UploadFail, UploadFile};
use super::json::{self, Segment};
use super::wire::{self, WireError};
use crate::clock::Clock;
use backend::{Fs, HttpClient, HttpRequest, Method};

/// Prefix of every message libabaas queues about itself (`0x140725370`).
const LIB_PREFIX: &str = "[LIB MESSAGE] ";
/// `Retry` waits by `retryCount` (table `0x1407261C0`).
const RETRY_WAITS: [u64; 5] = [60, 120, 240, 480, 900];
/// Size limit of the application message queue.
const APP_QUEUE_LIMIT: usize = 100;
/// Messages per notice request.
const NOTICE_BATCH: usize = 10;
/// Longest accepted message in bytes (`0x400`).
pub(super) const MAX_MESSAGE: usize = 0x400;

/// The services the engine needs while it runs a step.
pub(super) struct Env<'a> {
    pub clock: &'a dyn Clock,
    pub http: &'a mut dyn HttpClient,
    pub paths: &'a dyn Fs,
}

/// The 136-byte libabaas configuration the log manager keeps (`L+0x348`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct SysConfig {
    pub game_id: String,
    pub major: u8,
    pub minor: u8,
    pub location: u16,
    pub keychip: String,
    pub main_id: String,
    /// The main folder as cabinet text with its trailing backslash.
    pub folder: String,
}

/// The `Client` object: identity, AES key and log folder.
#[derive(Debug, Clone)]
pub(super) struct Client {
    pub serial: String,
    pub main_id: String,
    pub game_id: String,
    pub major: u8,
    pub minor: u8,
    pub location: i32,
    pub key: [u8; 32],
    pub folder: String,
}

#[cfg(test)]
impl Client {
    pub(super) fn for_test(serial: &str, main_id: &str, game_id: &str, major: u8, minor: u8, location: i32) -> Self {
        Self {
            serial: serial.into(),
            main_id: main_id.into(),
            game_id: game_id.into(),
            major,
            minor,
            location,
            key: wire::derive_key(game_id),
            folder: String::new(),
        }
    }
}

/// `Setting` (`0x14036AAA0`..): the values the polling answer overwrites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Setting {
    pub interval: u32,
    pub max_bandwidth: u32,
    pub max_datasize: u32,
}

impl Default for Setting {
    fn default() -> Self {
        Self { interval: 3600, max_bandwidth: 1024, max_datasize: 20 }
    }
}

/// One queued error message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Message {
    pub content: String,
    pub create_time: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UploadInstruction {
    path: String,
    next_segment: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DeleteInstruction {
    path: String,
    mtime: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum State {
    #[default]
    Idle,
    Running,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Job {
    Polling,
    LibMessages,
    AppMessages,
    Delete,
    Upload,
    Retry,
}

impl Job {
    const fn index(self) -> usize {
        self as usize
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Search,
    FileData,
    Encode,
    Send,
    Decode,
    Delete,
    Wait,
}

/// The sequencer that is running: its suites are the stages, taken in order.
#[derive(Debug)]
struct Run {
    job: Job,
    stage: Stage,
    search: Option<Search>,
    files: Vec<FileEntry>,
    /// Messages in the notice request.
    sent: usize,
    segment: Option<Segment>,
    body: Vec<u8>,
    response: Vec<u8>,
    decoded: Vec<u8>,
    started: u64,
    wait: u64,
}

impl Run {
    fn new(job: Job, stage: Stage) -> Self {
        Self {
            job,
            stage,
            search: None,
            files: Vec::new(),
            sent: 0,
            segment: None,
            body: Vec::new(),
            response: Vec::new(),
            decoded: Vec::new(),
            started: 0,
            wait: 0,
        }
    }
}

enum Progress {
    Continue,
    Done,
    Fail,
}

/// Whole seconds from `then` to `now` (milliseconds ticks), as the `{sec, usec}` timers count them.
fn seconds_since(now: u64, then: u64) -> u64 {
    now.saturating_sub(then) / 1000
}

/// `YYYYMMDDhhmmss` of the local wall clock.
pub(super) fn timestamp(clock: &dyn Clock) -> String {
    let c = clock.local_now().civil();
    format!("{:04}{:02}{:02}{:02}{:02}{:02}", c.year, c.month, c.day, c.hour, c.minute, c.second)
}

/// The `rand_s()` word of a request header.
fn nonce() -> u32 {
    (RandomState::new().build_hasher().finish() >> 16) as u32
}

/// `checkLogFolder` (`0x1403681E0`).
fn check_log_folder(folder: &str, paths: &dyn Fs) -> bool {
    let bytes = folder.as_bytes();
    if !(3..=200).contains(&bytes.len()) || !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' {
        return false;
    }
    if bytes[2..].iter().any(|&c| c < 0x20 || b"/:*?\"<>|".contains(&c) || !c.is_ascii()) {
        return false;
    }
    paths.map(folder).is_dir()
}

/// libabaas `Manager`.
#[derive(Debug, Default)]
pub(super) struct Engine {
    client: Option<Client>,
    enabled: bool,
    alive: bool,
    retry_count: u64,
    setting: Setting,
    /// `Polling.lastPollTime`; `None` is the original's zero.
    last_poll: Option<u64>,
    uploads: Vec<UploadInstruction>,
    deletes: Vec<DeleteInstruction>,
    app_messages: Vec<Message>,
    lib_messages: Vec<Message>,
    states: [State; 6],
    current: Option<Job>,
    run: Option<Run>,
    open_upload: Option<UploadFile>,
}

impl Engine {
    /// `setConfig` (`0x1403691B0`); `false` leaves the previous configuration untouched.
    pub fn set_config(&mut self, config: &SysConfig, paths: &dyn Fs) -> bool {
        if config.game_id.len() != 4
            || config.major > 99
            || config.minor > 99
            || config.keychip.len() != 11
            || config.main_id.len() != 11
            || !check_log_folder(&config.folder, paths)
        {
            return false;
        }
        let game_id = config.game_id.to_ascii_uppercase();
        self.client = Some(Client {
            serial: config.keychip.clone(),
            main_id: config.main_id.clone(),
            key: wire::derive_key(&game_id),
            game_id,
            major: config.major,
            minor: config.minor,
            location: i32::from(config.location),
            folder: config.folder.clone(),
        });
        true
    }

    /// `configured`.
    #[cfg(test)]
    pub fn is_configured(&self) -> bool {
        self.client.is_some()
    }

    /// The queued application messages (tests).
    #[cfg(test)]
    pub(super) fn app_messages(&self) -> &[Message] {
        &self.app_messages
    }

    /// The `alive` byte.
    pub fn alive(&self) -> bool {
        self.alive
    }

    /// `Manager::fileCount` (`0x140368F10`): the upload instructions once a poll succeeded, else `-1`.
    pub fn file_count(&self) -> i32 {
        if self.client.is_some() && self.states[Job::Polling.index()] == State::Succeeded {
            i32::try_from(self.uploads.len()).unwrap_or(i32::MAX)
        } else {
            -1
        }
    }

    /// `allowExecution` (`0x140368100`).
    pub fn allow_execution(&mut self, enable: bool, now: u64) {
        if self.client.is_none() || enable == self.enabled {
            return;
        }
        self.enabled = enable;
        if enable {
            self.start_polling(now);
        } else if let Some(job) = self.current
            && self.states[job.index()] == State::Running
        {
            self.states[job.index()] = State::Failed;
            self.run = None;
            self.alive = false;
        }
    }

    /// `setErrorMessage` (`0x140369760`): `0` queued, `-1` queue full, `-2` unusable text, `-3` not configured.
    pub fn set_error_message(&mut self, message: &str, clock: &dyn Clock) -> i32 {
        if self.client.is_none() {
            return -3;
        }
        if message.is_empty() || message.len() > MAX_MESSAGE {
            return -2;
        }
        if self.app_messages.len() >= APP_QUEUE_LIMIT {
            self.queue_lib(clock, "100 error messages were detected.");
            return -1;
        }
        self.app_messages.push(Message { content: message.to_owned(), create_time: timestamp(clock) });
        0
    }

    fn queue_lib(&mut self, clock: &dyn Clock, text: &str) {
        self.lib_messages.push(Message { content: format!("{LIB_PREFIX}{text}"), create_time: timestamp(clock) });
    }

    /// One `Manager::update`.
    pub fn update(&mut self, env: &mut Env<'_>) {
        if self.client.is_none() || !self.enabled {
            return;
        }
        if self.current.is_some_and(|job| self.states[job.index()] == State::Running) {
            self.step(env);
        }
        let now = env.clock.tick_ms();
        let Some(job) = self.current else { return };
        match self.states[job.index()] {
            State::Succeeded => {
                if job != Job::Retry {
                    self.retry_count = 0;
                    self.alive = true;
                }
                let due = self.last_poll.is_none_or(|then| seconds_since(now, then) > u64::from(self.setting.interval));
                if due {
                    self.start_polling(now);
                } else if !self.lib_messages.is_empty() {
                    self.start_messages(Job::LibMessages);
                } else if !self.app_messages.is_empty() {
                    self.start_messages(Job::AppMessages);
                } else if !self.deletes.is_empty() {
                    self.start(Job::Delete, Stage::Delete, now);
                } else if !self.uploads.is_empty() {
                    self.start(Job::Upload, Stage::FileData, now);
                }
            }
            State::Failed => {
                let index = usize::try_from(self.retry_count).map_or(4, |count| count.min(4));
                self.start(Job::Retry, Stage::Wait, now);
                if let Some(run) = &mut self.run {
                    run.wait = RETRY_WAITS[index];
                }
                self.retry_count += 1;
                self.alive = false;
            }
            State::Idle | State::Running => {}
        }
    }

    fn start(&mut self, job: Job, stage: Stage, now: u64) {
        let mut run = Run::new(job, stage);
        run.started = now;
        self.states[job.index()] = State::Running;
        self.current = Some(job);
        self.run = Some(run);
    }

    fn start_polling(&mut self, now: u64) {
        if self.current.is_some_and(|job| self.states[job.index()] == State::Running) {
            return;
        }
        self.start(Job::Polling, Stage::Search, now);
    }

    fn start_messages(&mut self, job: Job) {
        let list = if job == Job::LibMessages { &self.lib_messages } else { &self.app_messages };
        let sent = list.len().min(NOTICE_BATCH);
        self.start(job, Stage::Encode, 0);
        if let Some(run) = &mut self.run {
            run.sent = sent;
        }
    }

    fn step(&mut self, env: &mut Env<'_>) {
        let Some(mut run) = self.run.take() else { return };
        let job = run.job;
        match self.advance(&mut run, env) {
            Progress::Continue => self.run = Some(run),
            Progress::Done => {
                self.finish(run, env);
                self.states[job.index()] = State::Succeeded;
            }
            Progress::Fail => self.states[job.index()] = State::Failed,
        }
    }

    fn advance(&mut self, run: &mut Run, env: &mut Env<'_>) -> Progress {
        match run.stage {
            Stage::Search => self.search_step(run, env),
            Stage::FileData => self.file_data(run, env),
            Stage::Encode => self.encode(run, env),
            Stage::Send => self.send(run, env),
            Stage::Decode => match self.client.as_ref().map(|client| wire::decode_response(&client.key, &run.response))
            {
                Some(Ok(json)) => {
                    run.decoded = json;
                    Progress::Done
                }
                Some(Err(error)) => {
                    if matches!(error, WireError::Decrypt | WireError::Uncompress | WireError::Crc) {
                        self.queue_lib(env.clock, &error.to_string());
                    }
                    Progress::Fail
                }
                None => Progress::Fail,
            },
            Stage::Delete => {
                if let (Some(client), Some(front)) = (&self.client, self.deletes.first()) {
                    let cabinet = format!("{}{}", client.folder, front.path);
                    let host = env.paths.map(&cabinet);
                    if let Some(message) = files::delete_if_unchanged(&host, &cabinet, &front.mtime) {
                        self.queue_lib(env.clock, &message);
                    }
                }
                Progress::Done
            }
            Stage::Wait => {
                if seconds_since(env.clock.tick_ms(), run.started) > run.wait {
                    Progress::Done
                } else {
                    Progress::Continue
                }
            }
        }
    }

    fn search_step(&mut self, run: &mut Run, env: &mut Env<'_>) -> Progress {
        let mut search = match run.search.take() {
            Some(search) => search,
            None => {
                let Some(client) = &self.client else { return Progress::Fail };
                match Search::start(&client.folder, &env.paths.map(&client.folder)) {
                    Ok(search) => search,
                    Err(message) => {
                        self.queue_lib(env.clock, &message);
                        return Progress::Fail;
                    }
                }
            }
        };
        let mut messages = Vec::new();
        let done = search.step(&mut messages);
        for message in messages {
            self.queue_lib(env.clock, &message);
        }
        if done {
            run.files = search.into_files();
            run.stage = Stage::Encode;
        } else {
            run.search = Some(search);
        }
        Progress::Continue
    }

    /// `FileDataSuite`: open, check and hash the file of the front instruction on its first segment, then read the
    /// segment.
    fn file_data(&mut self, run: &mut Run, env: &mut Env<'_>) -> Progress {
        let (Some(client), Some(front)) = (&self.client, self.uploads.first()) else { return Progress::Fail };
        let cabinet = format!("{}{}", client.folder, front.path);
        let host = env.paths.map(&cabinet);
        let next = front.next_segment;
        let path = front.path.clone();
        let max_bytes = u64::from(self.setting.max_datasize) << 20;
        let reusable = self.open_upload.as_ref().is_some_and(|file| file.path == path && file.total_segments >= next);
        if !reusable {
            self.open_upload = None;
            match UploadFile::open(&host, &cabinet, &path, max_bytes, next) {
                Ok(file) => self.open_upload = Some(file),
                Err(UploadFail::Keep(message)) => {
                    self.queue_lib(env.clock, &message);
                    return Progress::Fail;
                }
                Err(UploadFail::Drop(message)) => {
                    self.queue_lib(env.clock, &message);
                    self.uploads.remove(0);
                    return Progress::Fail;
                }
            }
        }
        let Some(file) = &mut self.open_upload else { return Progress::Fail };
        match file.read_segment(next) {
            Ok(data) => {
                run.segment = Some(Segment {
                    path,
                    size: file.size,
                    mtime: file.mtime.clone(),
                    md5: file.md5.clone(),
                    total_segment: file.total_segments,
                    current_segment: next,
                    segment_size: file.size.min(files::SEGMENT_SIZE),
                    body: files::base64_crlf(&data),
                });
                run.stage = Stage::Encode;
                Progress::Continue
            }
            Err(_) => {
                self.open_upload = None;
                self.queue_lib(env.clock, &format!("cannot get encode data from{cabinet}"));
                self.uploads.remove(0);
                Progress::Fail
            }
        }
    }

    fn encode(&mut self, run: &mut Run, env: &mut Env<'_>) -> Progress {
        let Some(client) = &self.client else { return Progress::Fail };
        let created = timestamp(env.clock);
        let text = match run.job {
            Job::Polling => json::polling(client, &created, &self.setting, &run.files),
            Job::LibMessages => json::notice(client, &created, &self.lib_messages[..run.sent]),
            Job::AppMessages => json::notice(client, &created, &self.app_messages[..run.sent]),
            Job::Upload => match &run.segment {
                Some(segment) => json::upload(client, &created, segment),
                None => return Progress::Fail,
            },
            Job::Delete | Job::Retry => return Progress::Fail,
        };
        match wire::encode_request(&client.key, (client.major, client.minor), &client.game_id, text.as_bytes(), nonce())
        {
            Ok(body) => {
                run.body = body;
                run.stage = Stage::Send;
                Progress::Continue
            }
            Err(error) => {
                self.queue_lib(env.clock, &error.to_string());
                Progress::Fail
            }
        }
    }

    /// `HttpSuite` (6.9.3): one POST; anything but status 200 fails the sequence.
    fn send(&mut self, run: &mut Run, env: &mut Env<'_>) -> Progress {
        let Some(client) = &self.client else { return Progress::Fail };
        let suffix = match run.job {
            Job::Polling => ".amlog.sys-all.net/api/polling",
            Job::LibMessages | Job::AppMessages => ".amlog.sys-all.net/api/notice",
            Job::Upload => ".d-amlog.sys-all.net/up/logdata",
            Job::Delete | Job::Retry => return Progress::Fail,
        };
        let body = std::mem::take(&mut run.body);
        // `LOW_SPEED_LIMIT` 1024 B/s for 30 s plus the 30 s connect timeout bound the whole transfer.
        let timeout = Duration::from_secs(30 + body.len() as u64 / 1024);
        let request = HttpRequest {
            method: Method::Post,
            url: format!("http://{}{suffix}", client.game_id),
            headers: vec![
                ("Content-Type".to_owned(), "application/octet-stream".to_owned()),
                ("User-Agent".to_owned(), format!("ABS/{}/{}", client.game_id, client.serial)),
            ],
            body,
            timeout,
        };
        match env.http.request(&request) {
            Ok(response) if response.status == 200 => {
                run.response = response.body;
                run.stage = Stage::Decode;
                Progress::Continue
            }
            Ok(_) | Err(_) => Progress::Fail,
        }
    }

    /// The `checkResult` post-action of a sequencer that finished all its suites.
    fn finish(&mut self, run: Run, env: &Env<'_>) {
        match run.job {
            Job::Polling => {
                if let Some(answer) = json::analyze(&run.decoded) {
                    if let Some(value) = answer.interval {
                        self.setting.interval = value.clamp(1800, 7200) as u32;
                    }
                    if let Some(value) = answer.max_bandwidth {
                        self.setting.max_bandwidth = value.clamp(16, 2048) as u32;
                    }
                    if let Some(value) = answer.max_datasize {
                        self.setting.max_datasize = value.clamp(20, 50) as u32;
                    }
                    if let Some(items) = answer.upload {
                        self.uploads = items
                            .into_iter()
                            .map(|(path, next_segment)| UploadInstruction { path, next_segment })
                            .collect();
                    }
                    if let Some(items) = answer.delete {
                        self.deletes.extend(items.into_iter().map(|(path, mtime)| DeleteInstruction { path, mtime }));
                    }
                }
                self.last_poll = Some(env.clock.tick_ms());
            }
            Job::LibMessages => {
                self.lib_messages.drain(..run.sent.min(self.lib_messages.len()));
            }
            Job::AppMessages => {
                self.app_messages.drain(..run.sent.min(self.app_messages.len()));
            }
            Job::Upload => {
                let last = run.segment.as_ref().is_some_and(|s| s.current_segment >= s.total_segment);
                if last {
                    if !self.uploads.is_empty() {
                        self.uploads.remove(0);
                    }
                    self.open_upload = None;
                } else if let Some(front) = self.uploads.first_mut() {
                    front.next_segment += 1;
                }
            }
            Job::Delete => {
                if !self.deletes.is_empty() {
                    self.deletes.remove(0);
                }
            }
            Job::Retry => {}
        }
    }
}

#[cfg(test)]
mod tests;
