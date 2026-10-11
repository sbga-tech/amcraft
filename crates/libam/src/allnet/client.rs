//! The alAbEx job machine (`spec/libam/allnet.md` part 5 sections 2.1 to 2.4): the power-on and the download
//! order, one HTTP stage per pump, the retry wait and the daily re-authentication.

use super::abex::{self, Kind, Mode, Response, Session};
use super::exchange::{Exchange, Poll, Target};
use backend::tls::{TlsError, TlsProvider};

/// `ALE_ABEX_STAT_*` values.
pub mod stat {
    /// Not initialised.
    pub const NOINIT: i32 = -199;
    /// Idle, not authenticated.
    pub const START_WAIT: i32 = 101;
    /// Power-on in progress.
    pub const AUTH_BUSY: i32 = 102;
    /// Idle after a finished operation.
    pub const AUTH_OK: i32 = 103;
    /// Waiting for the retry timer.
    pub const RETRY_WAIT: i32 = 105;
    /// Download order in progress.
    pub const DLINSTR_BUSY: i32 = 106;
    /// Download order succeeded.
    pub const DLINSTR_OK: i32 = 107;
    /// Power-on refused by the server answer.
    pub const AUTH_NG: i32 = -499;
    /// Power-on socket timeout.
    pub const AUTH_TIMEOUT: i32 = -498;
    /// Download order refused by the server answer.
    pub const DLINSTR_NG: i32 = -799;
    /// Download order socket timeout.
    pub const DLINSTR_TIMEOUT: i32 = -798;
    /// Download order other socket error.
    pub const DLINSTR_COMMUNICATION: i32 = -797;
    /// Power-on other socket error.
    pub const AUTH_COMMUNICATION: i32 = -497;
    /// Any other internal error.
    pub const SYSTEM: i32 = -198;
}

/// Start return codes of the public entry points.
pub mod rc {
    /// Library not initialised.
    pub const NOT_INIT: i32 = -99;
    /// The power-on is running.
    pub const BUSY: i32 = -93;
    /// The retry wait is running.
    pub const RETRY: i32 = -92;
    /// Not authenticated.
    pub const NOT_AUTHENTICATED: i32 = -94;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    PowerOn,
    DlOrder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Start,
    Build,
    Create,
    Connect,
    Send,
    Receive,
    Parse,
    Failed,
    Wait,
    /// The finished download order: the state returns to `AUTH_OK` one pump after it showed the result.
    Done,
}

struct Job {
    op: Op,
    step: Step,
    exchange: Option<Exchange>,
    request: Vec<u8>,
    raw: Vec<u8>,
}

/// The outside of one pump.
pub struct Pump<'a> {
    /// The TLS provider.
    pub tls: &'a dyn TlsProvider,
    /// `GetTickCount`.
    pub tick: u64,
    /// Local epoch seconds (`seconds since 1970 from the local fields`).
    pub epoch: i64,
}

const REVAL_SECONDS: i64 = 7 * 3600;

/// The alAbEx session state.
pub struct Abex {
    session: Option<Session>,
    state: i32,
    authenticated: bool,
    jobs: Vec<Job>,
    paused: bool,
    wait_ms: u64,
    wait_from: u64,
    rand_off: u32,
    rand_wait: u32,
    response: Option<Response>,
    dl: Option<Response>,
    token: u32,
    next_reauth: i64,
    triggered: bool,
    auth_mode: u32,
    socket_timeout: bool,
    failed_kind: Option<bool>,
}

impl Default for Abex {
    fn default() -> Self {
        Self {
            session: None,
            state: stat::NOINIT,
            authenticated: false,
            jobs: Vec::new(),
            paused: false,
            wait_ms: 0,
            wait_from: 0,
            rand_off: 60,
            rand_wait: 0,
            response: None,
            dl: None,
            token: 0,
            next_reauth: 0,
            triggered: false,
            auth_mode: 1,
            socket_timeout: false,
            failed_kind: None,
        }
    }
}

impl Abex {
    /// `alAbExInit` plus the options: `auth_mode` 1 queues the daily re-authentication itself.
    pub fn init(&mut self, session: Session, auth_mode: u32) {
        let (rand_off, rand_wait) = abex::serial_delays(&session.serial);
        *self =
            Self { session: Some(session), state: stat::START_WAIT, auth_mode, rand_off, rand_wait, ..Self::default() };
    }

    /// `alAbExTerminate`.
    pub fn terminate(&mut self) {
        *self = Self::default();
    }

    /// Replaces the `auth_data` text of the session (the Auth 2.0 token).
    pub fn set_auth_data(&mut self, text: Option<String>) {
        if let Some(session) = self.session.as_mut() {
            session.auth_data = text;
        }
    }

    /// Updates the traceroute hop count the request carries.
    pub fn set_hops(&mut self, hops: i32) {
        if let Some(session) = self.session.as_mut() {
            session.hops = hops;
        }
    }

    /// `alAbExGetState`.
    pub fn state(&self) -> i32 {
        self.state
    }

    /// Whether the first power-on finished and the session is authenticated.
    pub fn authenticated(&self) -> bool {
        self.authenticated
    }

    /// The last power-on answer (`alAbExGetValue` source).
    pub fn response(&self) -> Option<&Response> {
        self.response.as_ref()
    }

    /// The last download-order answer.
    pub fn dl_response(&self) -> Option<&Response> {
        self.dl.as_ref()
    }

    /// Whether the previous failure was a timeout (`8201`) rather than another failure (`8202`).
    pub fn last_failure_timeout(&self) -> Option<bool> {
        self.failed_kind
    }

    /// The next daily re-authentication, local epoch seconds (`alAbExGetInfo` +144).
    pub fn next_reauth(&self) -> i64 {
        self.next_reauth
    }

    /// Pause / resume (`root+26164`).
    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    fn push(&mut self, op: Op) {
        self.jobs.push(Job { op, step: Step::Start, exchange: None, request: Vec::new(), raw: Vec::new() });
    }

    /// `alAbExStart` (`0x1403D8B60`).
    pub fn start(&mut self) -> i32 {
        if self.session.is_none() {
            return rc::NOT_INIT;
        }
        match self.state {
            stat::AUTH_BUSY => return rc::BUSY,
            stat::RETRY_WAIT => return rc::RETRY,
            _ => {}
        }
        self.authenticated = false;
        self.push(Op::PowerOn);
        0
    }

    /// `alAbExStartDl` (`0x1403D8C30`).
    pub fn start_dl(&mut self) -> i32 {
        if self.session.is_none() {
            return rc::NOT_INIT;
        }
        if !self.authenticated {
            return rc::NOT_AUTHENTICATED;
        }
        self.push(Op::DlOrder);
        0
    }

    fn map_socket(&self, kind: Kind, e: TlsError) -> i32 {
        let timeout = e.is_timeout();
        match (kind, timeout) {
            (Kind::PowerOn, true) => stat::AUTH_TIMEOUT,
            (Kind::PowerOn, false) => stat::AUTH_COMMUNICATION,
            (Kind::DlOrder, true) => stat::DLINSTR_TIMEOUT,
            (Kind::DlOrder, false) => stat::DLINSTR_COMMUNICATION,
        }
    }

    /// `0x1403F8690`: the next re-authentication epoch after a successful power-on.
    fn schedule_reauth(&mut self, epoch: i64) {
        let time_of_day = epoch.rem_euclid(86_400);
        let today = epoch - time_of_day + REVAL_SECONDS;
        let mut next = today + i64::from(self.rand_off);
        if time_of_day >= REVAL_SECONDS {
            next += 86_400;
        }
        self.next_reauth = next;
    }

    /// One pump (`alAbExExecServer`): the re-authentication check, then one step of the head job.
    pub fn pump(&mut self, env: &Pump<'_>) {
        if self.session.is_none() {
            return;
        }
        if self.reauth_check(env) {
            return;
        }
        if self.jobs.is_empty() {
            return;
        }
        self.step_head(env);
    }

    /// Section 2.4: the retry-wait expiry and the daily trigger. `true` when the pump ends here.
    fn reauth_check(&mut self, env: &Pump<'_>) -> bool {
        if !self.authenticated
            && self.state == stat::RETRY_WAIT
            && !self.paused
            && env.tick.wrapping_sub(self.wait_from) > self.wait_ms
        {
            self.state = stat::START_WAIT;
        }
        if !self.authenticated {
            return false;
        }
        if env.epoch < self.next_reauth {
            self.triggered = false;
            return false;
        }
        let revalidating = env.epoch.rem_euclid(86_400) == REVAL_SECONDS;
        if !self.triggered && !self.paused && self.jobs.is_empty() && !revalidating {
            self.state = stat::START_WAIT;
            self.authenticated = false;
            self.triggered = true;
            if self.auth_mode == 1 {
                self.push(Op::PowerOn);
            }
            return true;
        }
        false
    }

    fn step_head(&mut self, env: &Pump<'_>) {
        let (op, step) = (self.jobs[0].op, self.jobs[0].step);
        let kind = if op == Op::PowerOn { Kind::PowerOn } else { Kind::DlOrder };
        match step {
            Step::Start => {
                if op == Op::PowerOn {
                    self.state = stat::AUTH_BUSY;
                    self.wait_ms = 1000 * u64::from(self.rand_wait + 300);
                } else {
                    self.dl = None;
                    self.state = stat::DLINSTR_BUSY;
                }
                self.jobs[0].step = Step::Build;
            }
            Step::Build => self.build(env, op, kind),
            Step::Create => {
                let Some(session) = self.session.as_ref() else { return };
                let (addr, tls) = if op == Op::PowerOn { session.power_on_target() } else { session.dl_order_target() };
                let name = match (session.mode, tls) {
                    (Mode::AuthData, true) => "*.auth.sys-all.net".to_owned(),
                    (Mode::Title, true) => "at.sys-all.net".to_owned(),
                    _ => String::new(),
                };
                let target = Target { addr, port: if tls { 443 } else { 80 }, tls, server_name: name };
                let request = std::mem::take(&mut self.jobs[0].request);
                self.jobs[0].exchange = Some(Exchange::new(env.tls, target, request));
                self.jobs[0].step = Step::Connect;
            }
            Step::Connect | Step::Send | Step::Receive => {
                let Some(exchange) = self.jobs[0].exchange.as_mut() else {
                    self.fail(env, kind, stat::SYSTEM);
                    return;
                };
                match exchange.step() {
                    Poll::Pending => {
                        self.jobs[0].step = if step == Step::Connect { Step::Send } else { Step::Receive };
                    }
                    Poll::Done(raw) => {
                        exchange.close();
                        self.jobs[0].raw = raw;
                        self.jobs[0].step = Step::Parse;
                    }
                    Poll::Failed(e) => {
                        self.socket_timeout = e.is_timeout();
                        let code = self.map_socket(kind, e);
                        self.fail(env, kind, code);
                    }
                }
            }
            Step::Parse => self.parse(env, op, kind),
            Step::Failed => {
                if op == Op::PowerOn {
                    self.state = stat::RETRY_WAIT;
                    self.wait_from = env.tick;
                    self.authenticated = false;
                    self.jobs[0].step = Step::Wait;
                } else {
                    // The failed order shows its error state for exactly one pump, then the state is idle again.
                    self.state = stat::AUTH_OK;
                    self.jobs.remove(0);
                }
            }
            Step::Wait => {
                if self.state == stat::START_WAIT {
                    self.jobs[0].step = Step::Start;
                }
            }
            Step::Done => {
                self.state = stat::AUTH_OK;
                self.jobs.remove(0);
            }
        }
    }

    fn build(&mut self, env: &Pump<'_>, op: Op, kind: Kind) {
        let Some(session) = self.session.as_ref() else { return };
        let built = if op == Op::PowerOn {
            self.token = abex::token(env.epoch as u32, &session.game_id, &session.serial, session.client_ip, env.tick);
            session.power_on_request(self.token)
        } else {
            session.dl_order_request()
        };
        match built {
            Ok(request) => {
                self.jobs[0].request = request;
                self.jobs[0].step = Step::Create;
            }
            Err(rc) => {
                let code = abex::map_protocol(kind, rc);
                self.fail(env, kind, code);
            }
        }
    }

    fn fail(&mut self, env: &Pump<'_>, kind: Kind, code: i32) {
        let _ = env;
        self.state = code;
        if kind == Kind::PowerOn {
            self.failed_kind = Some(matches!(code, stat::AUTH_TIMEOUT));
        }
        self.jobs[0].exchange = None;
        self.jobs[0].step = Step::Failed;
    }

    fn parse(&mut self, env: &Pump<'_>, op: Op, kind: Kind) {
        let raw = std::mem::take(&mut self.jobs[0].raw);
        let text = match abex::check_response(&raw, kind) {
            Ok(text) => text,
            Err(rc) => {
                self.parse_failure(env, kind, rc);
                return;
            }
        };
        if op == Op::PowerOn {
            let mode = self.session.as_ref().map_or(Mode::Plain, |s| s.mode);
            let response = abex::parse_power_on(&text, mode, self.token);
            if response.result < 0 {
                let rc = response.result;
                self.response = Some(response);
                self.parse_failure(env, kind, rc);
                return;
            }
            self.response = Some(response);
            self.schedule_reauth(env.epoch);
            self.authenticated = true;
            self.failed_kind = None;
            self.state = stat::AUTH_OK;
            self.jobs.remove(0);
        } else {
            match abex::parse_dl_order(&text) {
                Ok(response) => {
                    self.dl = Some(response);
                    self.state = stat::DLINSTR_OK;
                    self.jobs[0].step = Step::Done;
                }
                Err(rc) => self.parse_failure(env, kind, rc),
            }
        }
    }

    fn parse_failure(&mut self, env: &Pump<'_>, kind: Kind, rc: i32) {
        let code = abex::map_protocol(kind, rc);
        if kind == Kind::PowerOn && matches!(rc, -45 | -43 | -42 | -41 | -40) {
            self.wait_ms = 1000 * u64::from(self.rand_wait + 1800);
        }
        self.fail(env, kind, code);
    }
}
