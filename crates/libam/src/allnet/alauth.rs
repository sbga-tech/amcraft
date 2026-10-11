//! alAuth, the Auth 2.0 client (`spec/libam/allnet.md` part 6 sections 2 to 5): the single-instance state machine
//! that sends `packet_data` to `at.auth.sys-all.net` twice and verifies the signed `auth_data` that comes back.

use std::net::Ipv4Addr;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use super::abex::lenient_base64;
use super::exchange::{Exchange, Poll, Target, parse_http};
use super::verify::verify_auth_data;
use backend::tls::{TlsError, TlsProvider};

/// The host of the Auth 2.0 server.
pub const HOST: &str = "at.auth.sys-all.net";
/// The certificate name pattern.
pub const NAME_PATTERN: &str = "*.auth.sys-all.net";
/// Re-arm interval after a failure or an answer (15 minutes).
const REARM_MS: u64 = 900_000;
/// `alAuthSetPacketData` accepts at most this many raw bytes.
pub const MAX_PACKET: usize = 0x80;

/// alAuth's states (`0x1409D2370`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Not initialised.
    Uninitialised = 0,
    /// Idle.
    Idle = 1,
    /// A request is running.
    Running = 2,
    /// The `packet_data` answer is ready.
    PacketReady = 3,
    /// Failed.
    Failed = 4,
    /// Finished: the `auth_data` answer is ready.
    Finished = 5,
}

/// What an `alAuthGetData` call can return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The server's `packet_data`.
    Packet,
    /// The verbatim `auth_data` text.
    AuthData,
}

/// The outside of one `exec`: sessions, the tick and the UTC clock.
pub struct Env<'a> {
    /// The TLS provider.
    pub tls: &'a dyn TlsProvider,
    /// `GetTickCount`.
    pub tick: u64,
    /// UTC seconds since 1970.
    pub now: u64,
}

/// The parsed signed content: `[year, month, day, hour, minute, second]` pairs.
type Date = [i32; 6];

/// alAuth.
pub struct AlAuth {
    state: State,
    substep: u8,
    error: u8,
    server: Ipv4Addr,
    body: String,
    kind: Kind,
    data: Vec<u8>,
    session: String,
    status: u8,
    renewal: Date,
    limit: Date,
    limit_valid: bool,
    tick: u64,
    operation: bool,
    exchange: Option<Exchange>,
    raw: Vec<u8>,
    chain: Vec<Vec<u8>>,
    pending_cookie: Option<String>,
}

impl Default for AlAuth {
    fn default() -> Self {
        Self {
            state: State::Uninitialised,
            substep: 0,
            error: 0,
            server: Ipv4Addr::UNSPECIFIED,
            body: String::new(),
            kind: Kind::Packet,
            data: Vec::new(),
            session: String::new(),
            status: 0,
            renewal: [0; 6],
            limit: [0; 6],
            limit_valid: false,
            tick: 0,
            operation: false,
            exchange: None,
            raw: Vec::new(),
            chain: Vec::new(),
            pending_cookie: None,
        }
    }
}

/// `0x1403F7110`: the 32-bit second counter of a date record.
pub fn date_seconds(d: &Date) -> u32 {
    const CUM: [i32; 13] = [0, 0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let [y, m, day, h, mi, s] = *d;
    let cum = usize::try_from(m).ok().and_then(|m| CUM.get(m)).copied().unwrap_or(0);
    let days = 365 * (y - 1970) + ((y - 1968) >> 2) - i32::from(y % 4 == 0 && m <= 2) + day + cum - 1;
    (s as u32).wrapping_add(60u32.wrapping_mul(
        (mi as u32).wrapping_add(60u32.wrapping_mul((h as u32).wrapping_add(24u32.wrapping_mul(days as u32)))),
    ))
}

impl AlAuth {
    /// `alAuthInit`: `Err(code)` 4 when already initialised.
    pub fn init(&mut self, server: Ipv4Addr) -> Result<(), u32> {
        if self.state != State::Uninitialised {
            return Err(4);
        }
        *self = Self { state: State::Idle, server, ..Self::default() };
        Ok(())
    }

    /// `alAuthReset`: `Err(2)` when not initialised. Status and dates are kept.
    pub fn reset(&mut self) -> Result<(), u32> {
        if self.state == State::Uninitialised {
            return Err(2);
        }
        self.exchange = None;
        self.state = State::Uninitialised;
        self.error = 0;
        self.limit_valid = false;
        Ok(())
    }

    /// `alAuthGetStatus`.
    pub fn state(&self) -> State {
        self.state
    }

    /// `alAuthGetError`: 0 none, 1 handle failure, 2 timeout, 3 other.
    pub fn error(&self) -> u8 {
        self.error
    }

    /// `alAuthIsLimitValid`.
    pub fn limit_valid(&self) -> bool {
        self.limit_valid
    }

    /// `alAuthSetOperation`.
    pub fn set_operation(&mut self, on: bool) {
        self.operation = on;
    }

    /// The server's status word: 0 unset, 1 OK, 2 NG.
    pub fn status(&self) -> u8 {
        self.status
    }

    /// `alAuthSetPacketData`: `Err` 2 not initialised, 4 wrong state, 3 bad argument.
    pub fn set_packet_data(&mut self, data: &[u8]) -> Result<(), u32> {
        match self.state {
            State::Uninitialised => return Err(2),
            State::Idle | State::PacketReady => {}
            _ => return Err(4),
        }
        if data.len() > MAX_PACKET {
            return Err(3);
        }
        let mut body = format!("packet_data={}", STANDARD.encode(data));
        body.truncate(255);
        self.body = body;
        self.substep = 1;
        self.error = 0;
        self.state = State::Running;
        Ok(())
    }

    /// `alAuthGetData(kind, out, &size)` with a capacity of `cap`.
    pub fn get_data(&self, kind: Kind, cap: usize) -> Result<Vec<u8>, u32> {
        if self.state == State::Uninitialised {
            return Err(2);
        }
        if self.data.is_empty() || self.state == State::Running {
            return Err(4);
        }
        let needs = if kind == Kind::Packet { State::PacketReady } else { State::Finished };
        if self.state != needs || self.kind != kind {
            return Err(4);
        }
        let mut out = self.data.clone();
        if kind == Kind::AuthData {
            out.push(0);
        }
        if cap < out.len() { Err(3) } else { Ok(out) }
    }

    /// `0x1403DB010`.
    fn needs_reauth(&self, env: &Env<'_>) -> bool {
        let elapsed = env.tick.wrapping_sub(self.tick);
        match self.state {
            State::Idle => self.operation && (self.status != 0 || self.error != 0),
            State::Failed => !self.operation && elapsed >= REARM_MS,
            State::Finished => {
                !self.operation
                    && self.status != 0
                    && elapsed >= REARM_MS
                    && (self.status != 1 || date_seconds(&utc_now(env.now)) >= date_seconds(&self.renewal))
            }
            _ => false,
        }
    }

    /// `alAuthExec`, once per frame.
    pub fn exec(&mut self, env: &Env<'_>) {
        if self.state == State::Uninitialised {
            return;
        }
        if self.status != 0 && self.limit[0] != 0 {
            self.limit_valid = date_seconds(&utc_now(env.now)) < date_seconds(&self.limit);
        }
        if self.needs_reauth(env) {
            if self.state == State::Idle {
                self.state = State::Failed;
            } else {
                self.state = State::Idle;
                self.substep = 0;
            }
            return;
        }
        if self.state == State::Running {
            self.run_substep(env);
        }
    }

    fn fail_poll(&mut self, e: TlsError) {
        self.error = if e.is_timeout() { 2 } else { 3 };
        self.substep = 7;
    }

    fn run_substep(&mut self, env: &Env<'_>) {
        match self.substep {
            1 => self.substep = 2,
            2 => {
                let request = self.request_bytes();
                let target = Target { addr: self.server, port: 443, tls: true, server_name: NAME_PATTERN.to_owned() };
                self.exchange = Some(Exchange::new(env.tls, target, request));
                self.substep = 3;
            }
            3..=5 => {
                let Some(exchange) = self.exchange.as_mut() else {
                    self.error = 1;
                    self.substep = 7;
                    return;
                };
                match exchange.step() {
                    Poll::Pending => self.substep += 1,
                    Poll::Done(raw) => {
                        self.chain = exchange.chain();
                        self.raw = raw;
                        self.substep = 6;
                    }
                    Poll::Failed(e) => self.fail_poll(e),
                }
            }
            6 => {
                if !self.check_response(env) {
                    self.error = 3;
                }
                self.substep = 7;
            }
            7 => self.finish(env),
            _ => {}
        }
    }

    fn request_bytes(&self) -> Vec<u8> {
        let mut text = format!(
            "POST /net/certify HTTP/1.1\r\nConnection: Close\r\nUser-Agent: Windows/ver.6.0\r\nHost: {HOST}\r\nContent-Type: application/x-www-form-urlencoded\r\n"
        );
        if !self.session.is_empty() {
            text.push_str(&format!("Cookie: {}\r\n", self.session));
        }
        text.push_str(&format!("Content-Length: {}\r\n\r\n{}", self.body.len(), self.body));
        text.into_bytes()
    }

    fn finish(&mut self, env: &Env<'_>) {
        if let Some(mut exchange) = self.exchange.take() {
            exchange.close();
        }
        if self.error != 0 {
            self.state = if self.error == 1 { State::Finished } else { State::Failed };
            self.tick = env.tick;
            self.session.clear();
        } else if self.kind == Kind::Packet {
            self.state = State::PacketReady;
            self.substep = 0;
            if let Some(cookie) = self.pending_cookie.take() {
                self.session = cookie;
            }
        } else {
            self.error = 0;
            self.state = State::Finished;
            self.tick = env.tick;
            self.session.clear();
        }
    }

    /// `alAuthExecServerCheckResponse` (`0x1403DA5A0`): fills the staging data, `false` on any failure.
    fn check_response(&mut self, env: &Env<'_>) -> bool {
        let Ok(answer) = parse_http(&self.raw) else { return false };
        if answer.status != 200 {
            return false;
        }
        if answer.body.len() > 0x800 {
            tracing::error!("allnet auth 2.0: the response is too large");
            return false;
        }
        let cookie = answer.header("Set-Cookie").map(str::to_owned);
        if cookie.as_ref().is_some_and(|c| c.len() >= 128) {
            return false;
        }
        let Some((packet, auth)) = parse_response(&answer.body) else {
            tracing::error!("allnet auth 2.0: the response has no valid key");
            return false;
        };
        if auth.is_empty() {
            let Some(cookie) = cookie else {
                tracing::error!("allnet auth 2.0: the response carries no session id");
                return false;
            };
            let decoded = lenient_base64(&packet);
            if decoded.is_empty() {
                tracing::error!("allnet auth 2.0: the packet data is invalid");
                return false;
            }
            self.pending_cookie = Some(cookie);
            self.data = decoded;
            self.kind = Kind::Packet;
            return true;
        }
        let der = lenient_base64(&auth);
        let anchors = env.tls.trust_anchors();
        let Some(content) = verify_auth_data(&der, &self.chain, anchors.as_deref(), env.now) else {
            tracing::error!("allnet auth 2.0: the auth data failed verification");
            return false;
        };
        let Some((status, renewal, limit)) = parse_content(&content) else {
            tracing::error!("allnet auth 2.0: the auth data is invalid");
            return false;
        };
        self.status = status;
        if status == 1 {
            self.renewal = renewal;
            self.limit = limit;
            self.limit_valid = date_seconds(&utc_now(env.now)) < date_seconds(&limit);
        }
        self.data = auth;
        self.kind = Kind::AuthData;
        true
    }
}

/// UTC fields of `now` (Unix seconds).
pub fn utc_now(now: u64) -> Date {
    let days = (now / 86_400) as i64;
    let rem = now % 86_400;
    // Civil from days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    [year as i32, month as i32, day as i32, (rem / 3600) as i32, (rem % 3600 / 60) as i32, (rem % 60) as i32]
}

/// `alAuthParseAuthResponse`: the `packet_data` and `auth_data` values of the first two pairs, exactly one of
/// them non-empty.
fn parse_response(body: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let mut pos = 0;
    let (mut packet, mut auth): (Option<Vec<u8>>, Option<Vec<u8>>) = (None, None);
    for _ in 0..2 {
        let eq = pos + body.get(pos..)?.iter().position(|&c| c == b'=')?;
        let key = &body[pos..eq];
        let end = body[eq + 1..].iter().position(|&c| c == b'&').map_or(body.len(), |at| eq + 1 + at);
        let mut value = body[eq + 1..end].to_vec();
        if value.len() >= 0x800 {
            value.clear();
        }
        let slot = match key {
            b"packet_data" => &mut packet,
            b"auth_data" => &mut auth,
            _ => {
                pos = end + 1;
                continue;
            }
        };
        if slot.is_some() {
            return None;
        }
        *slot = Some(value);
        pos = end + 1;
        if pos > body.len() {
            break;
        }
    }
    let (packet, auth) = (packet?, auth?);
    (packet.is_empty() != auth.is_empty()).then_some((packet, auth))
}

/// `alAuthParseAuthData`: `(status, renewal, limit)` of the signed content.
fn parse_content(content: &[u8]) -> Option<(u8, Date, Date)> {
    let text = std::str::from_utf8(content).ok()?.trim_end_matches('\0');
    let (mut result, mut renewal, mut limit) = (None, None, None);
    for pair in text.split('&').take(100) {
        let Some((key, value)) = pair.split_once('=') else { continue };
        let slot = match key {
            "result" => &mut result,
            "renewal" => &mut renewal,
            "limit" => &mut limit,
            _ => continue,
        };
        if slot.replace(value).is_some() {
            return None;
        }
    }
    let result = result?;
    if result.len() > 7 || result.is_empty() || !result.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let status = u8::from(result.parse::<u32>().ok()? != 0) + 1;
    Some((status, parse_date(renewal?)?, parse_date(limit?)?))
}

/// `sscanf("%d-%d-%dT%d:%d:%dZ")` on at most 23 characters, six conversions required.
fn parse_date(text: &str) -> Option<Date> {
    let text = text.get(..text.len().min(23))?;
    let mut out = [0i32; 6];
    let mut rest = text;
    for (i, sep) in ['-', '-', 'T', ':', ':', 'Z'].into_iter().enumerate() {
        let digits = rest
            .char_indices()
            .find(|&(at, c)| !(c.is_ascii_digit() || (at == 0 && (c == '-' || c == '+'))))
            .map_or(rest.len(), |(at, _)| at);
        out[i] = rest[..digits].parse().ok()?;
        rest = &rest[digits..];
        if i < 5 {
            rest = rest.strip_prefix(sep)?;
        }
    }
    Some(out)
}
