//! The server access hub (`payment.md` PayApMgr 2.6 and PayApSrv 3, 7, 8): the identification placed in the
//! requests, the stored answers (location, summary, item list, user setting, receipt) and the single in-flight
//! request.
//!
//! The HTTP exchange is blocking (`cx.backend.http`): it runs when the request is issued and its answer is
//! applied by the next [`Server::poll`], so a command needs at least one frame, like the asynchronous original.

use std::time::Duration;

use backend::{HttpClient, HttpError, HttpRequest, HttpResponse, Method};

use super::model::{Item, Location, NO_AMOUNT, Summary, UserSetting};
use super::proto::{self, Answer, Call, Ids, RESPONSE_LIMIT, Serial};

/// Connect and total timeout of a request (WinHTTP's defaults are 60 s connect and 30 s send/receive,
/// `payment.apsrv.http.no_timeouts_no_tls_options`).
const TIMEOUT: Duration = Duration::from_secs(60);
/// `User-Agent` of the original's session (`payment.apsrv.http.session_open`).
const AGENT: &str = "SEGA AMUSEMENT";
/// The content type of the request and the accepted answer.
const JSON: &str = "application/json";
/// Highest value of an unsigned counter parameter (`0x7FFFFFFF`).
const COUNT_LIMIT: u32 = 0x7FFF_FFFF;

/// The runner of the hub (one command at a time).
#[derive(Debug, Default)]
enum Run {
    #[default]
    Idle,
    /// Issued; `reply` is `None` once cancelled.
    Running { call: Call, reply: Option<Result<HttpResponse, HttpError>> },
    /// Finished: the command was accepted (`true`) or failed.
    Done(bool),
}

/// `AimePayServer` (`manager+3200`).
#[derive(Debug)]
pub struct Server {
    /// `SA+136`: the HTTP session is open.
    pub initialized: bool,
    ids: Ids,
    url: String,
    kind: u32,
    code: Option<String>,
    /// `SA.currentLocation`: the location the Activate answer returned.
    pub location: Location,
    summary: Vec<Summary>,
    items: Vec<Item>,
    user: Option<UserSetting>,
    /// `SA+19040`: the user is already checked in.
    pub checked_in: bool,
    /// `SA+19048`.
    pub receipt_id: u64,
    /// `SA+19056`.
    pub amount: u32,
    issued: bool,
    run: Run,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            initialized: false,
            ids: Ids::default(),
            url: String::new(),
            kind: 0,
            code: Some(String::new()),
            location: Location::default(),
            summary: Vec::new(),
            items: Vec::new(),
            user: None,
            checked_in: false,
            receipt_id: 0,
            amount: NO_AMOUNT,
            issued: false,
            run: Run::Idle,
        }
    }
}

impl Server {
    /// `SA.init`: the session is opened.
    pub fn init(&mut self) {
        self.initialized = true;
    }

    /// `SA.reset(all)` (`0x14028C530`): clears the answers; `all` also clears the ids and the client id.
    pub fn reset(&mut self, all: bool) {
        self.kind = 0;
        self.code = Some(String::new());
        self.location = Location::default();
        self.summary.clear();
        self.items.clear();
        self.user = None;
        self.receipt_id = 0;
        self.amount = NO_AMOUNT;
        self.checked_in = false;
        if all {
            self.ids = Ids::default();
            self.url.clear();
            self.issued = false;
            self.run = Run::Idle;
        }
    }

    /// `SA.shutdown`: the session closes and everything is cleared.
    pub fn shutdown(&mut self) {
        self.reset(true);
        self.initialized = false;
    }

    /// `SA.setIds`: game id and the two serials.
    pub fn set_ids(&mut self, game_id: Option<String>, board: Option<Serial>, keychip: Option<Serial>) {
        self.ids.game_id = game_id;
        self.ids.board = board;
        self.ids.keychip = keychip;
    }

    /// `SA.setUrl`: the root endpoint of `env.json`.
    pub fn set_url(&mut self, url: &str) {
        self.url = url.to_owned();
    }

    /// `SA.setClientId`: the `ip` of the Activate, PayToCoin and PayToItem bodies.
    pub fn set_client_id(&mut self, client_id: u32) {
        self.ids.ip = client_id;
    }

    /// The hub runner is idle (not running).
    pub fn is_idle(&self) -> bool {
        !matches!(self.run, Run::Running { .. })
    }

    /// "SA ok" (`0x14028E020`): a command was issued and finished with an accepted answer.
    pub fn ok(&self) -> bool {
        self.issued && matches!(self.run, Run::Done(true))
    }

    /// "SA kind" (`0x14028E060`): the answer's `result_code`, 0 while running or after a transport failure.
    pub fn kind(&self) -> u32 {
        if self.is_idle() { self.kind } else { 0 }
    }

    /// "SA code" (`0x14028E0A0`): the answer's `detail_code`, `None` while a command runs.
    pub fn code(&self) -> Option<&str> {
        if self.is_idle() { self.code.as_deref() } else { None }
    }

    /// The deal summary, newest first.
    pub fn summary(&self) -> impl ExactSizeIterator<Item = &Summary> {
        self.summary.iter().rev()
    }

    /// The item list.
    pub fn item_price(&self, item_id: &str) -> Option<u32> {
        self.items.iter().find(|item| item.item_id == item_id).map(|item| item.price)
    }

    /// The user setting of the last CheckUser.
    pub fn user_setting(&self) -> Option<UserSetting> {
        self.user
    }

    /// Cancels the running command: its answer is dropped and the command fails at the next poll.
    pub fn cancel(&mut self) {
        if let Run::Running { reply, .. } = &mut self.run {
            *reply = None;
        }
    }

    /// The per-frame update of the hub runner (`0x14028C0B0`): applies the answer of the running command.
    pub fn poll(&mut self) {
        let Run::Running { call, reply } = std::mem::take(&mut self.run) else {
            return;
        };
        let accepted = match reply {
            None => false,
            Some(Err(_)) => {
                // Transport failure: code 0, message "" (`payment.apsrv.cmd.sequencer_semantics`).
                self.kind = 0;
                self.code = Some(String::new());
                false
            }
            Some(Ok(response)) => self.apply(&call, &response.body),
        };
        self.run = Run::Done(accepted);
    }

    fn apply(&mut self, call: &Call, body: &[u8]) -> bool {
        let parsed = proto::parse_answer(call, body);
        if let Some((kind, code)) = parsed.header {
            self.kind = kind;
            self.code = Some(code);
        } else {
            self.kind = 0;
            self.code = Some(String::new());
        }
        let answer = match parsed.outcome {
            Ok(answer) => answer,
            Err(partial) => {
                if let Some(location) = partial {
                    self.location = location;
                }
                return false;
            }
        };
        if self.kind != 0 {
            return true;
        }
        match (call, answer) {
            (Call::CheckUser { aime_id }, Answer::CheckUser { credit, checked_in, touch_skip }) => {
                self.user =
                    Some(UserSetting { aime_id: *aime_id, credit_card_available: credit, aime_hold_skip: touch_skip });
                self.checked_in = checked_in;
            }
            (Call::CheckIn { .. }, _) => self.checked_in = true,
            (_, Answer::Summary(entries)) => self.summary = entries,
            (_, Answer::Pay { receipt_id, amount }) => {
                self.receipt_id = receipt_id;
                self.amount = amount;
            }
            (_, Answer::Location(location)) => self.location = location,
            (_, Answer::Items { items, .. }) => self.items = items,
            _ => {}
        }
        true
    }

    /// Issues `call` through the hub: the wrapper preconditions of `payment.apmgr.seq.sa_wrappers`, then the
    /// blocking exchange. A failed precondition launches nothing (`issued` stays false).
    pub fn issue(&mut self, call: Call, http: &mut dyn HttpClient) {
        self.issued = false;
        // The wrappers' argument-independent clearing happens before their checks.
        match &call {
            Call::CheckUser { .. } => {
                self.user = None;
                self.checked_in = false;
            }
            Call::CheckIn { .. } => self.checked_in = false,
            Call::LoadDealSummary { .. } => self.summary.clear(),
            Call::PayToCoin { .. } | Call::PayToItem { .. } => {
                self.receipt_id = 0;
                self.amount = NO_AMOUNT;
            }
            Call::Activate => self.location = Location::default(),
            _ => {}
        }
        if !self.initialized || !self.is_idle() || !self.allowed(&call) {
            return;
        }
        self.issued = true;
        self.kind = 0;
        self.code = None;
        let reply = self.exchange(&call, http);
        self.run = Run::Running { call, reply: Some(reply) };
    }

    fn allowed(&self, call: &Call) -> bool {
        const UNSET: u32 = 0xFFFF_FFFF;
        let digits = |text: &str| text.len() == 13 && text.bytes().all(|byte| byte.is_ascii_digit());
        let counted = |count: u32| (1..=COUNT_LIMIT).contains(&count);
        match call {
            Call::CheckAlive => true,
            Call::Activate => {
                self.ids.game_id.is_some() && self.ids.board.is_some() && !self.url.is_empty() && self.ids.ip != 0
            }
            Call::LoadItemList => self.location.is_complete(),
            _ if !self.location.is_complete() => false,
            Call::CheckUser { aime_id } => *aime_id != UNSET,
            Call::CheckIn { aime_id, pin } => {
                *aime_id != UNSET && pin.len() == 4 && pin.bytes().all(|byte| byte.is_ascii_digit())
            }
            Call::LoadDealSummary { days } => (1..=7).contains(days),
            Call::PayToCoin { aime_id, item_id, count } | Call::PayToItem { aime_id, item_id, count, .. } => {
                *aime_id != UNSET && digits(item_id) && counted(*count)
            }
            Call::CompletePayToCoin { aime_id, item_id, .. } => *aime_id != UNSET && digits(item_id),
        }
    }

    /// Builds and sends the request (`payment.apsrv.http.send`). A body that cannot be built or a URL the
    /// cracker refuses sends nothing and fails the command.
    fn exchange(&self, call: &Call, http: &mut dyn HttpClient) -> Result<HttpResponse, HttpError> {
        let url = format!("{}{}", self.url, call.path());
        let body = proto::build_body(call, &self.ids, &self.location)
            .filter(|body| body.len() <= 0x1000)
            .ok_or_else(|| HttpError::Transfer("request body".into()))?;
        let cracked = proto::crack_url(&url).ok_or_else(|| HttpError::Transfer("url".into()))?;
        let request = HttpRequest {
            method: Method::Post,
            url,
            headers: vec![
                ("Host".to_owned(), cracked.host),
                ("Content-Type".to_owned(), JSON.to_owned()),
                ("Accept".to_owned(), JSON.to_owned()),
                ("User-Agent".to_owned(), AGENT.to_owned()),
            ],
            body: body.into_bytes(),
            timeout: TIMEOUT,
        };
        let mut response = http.request(&request)?;
        // The client keeps 0x2000 bytes of the answer (`payment.apsrv.http.response_buffer`).
        response.body.truncate(RESPONSE_LIMIT);
        Ok(response)
    }
}
