//! The accounting report protocol (`allnet.md` part 9 "Report composition and upload", "Server response"): request
//! bytes, response parsing and one whole report exchange against a [`ReportLink`].

use std::io::Write as _;
use std::net::Ipv4Addr;

use flate2::Compression;
use flate2::write::DeflateEncoder;

use super::driver::Counters;
use super::link::{Exchange, LinkError, ReportLink};
use super::records::{Identity, Record, render};

/// TCP port of the accounting server.
pub const REPORT_PORT: u16 = 8443;
/// Entries per request until the server says otherwise.
pub const DEFAULT_LINE_LIMIT: usize = 100;

/// Which report: the daily foreground one or the interval background ("shime") one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Daily report (`operatingfix=0`, strings `Auth ...`).
    Foreground,
    /// Interval report (`operatingfix=1`, strings `Shime ...`).
    Background,
}

/// What one report needs; a snapshot so the exchange can run on a worker thread.
#[derive(Debug, Clone)]
pub struct ReportJob {
    /// Foreground or background.
    pub role: Role,
    /// The background object speaks as `Auth` when the first foreground report was skipped.
    pub first_skipped: bool,
    /// Keychip, board, game identity.
    pub identity: Identity,
    /// The auth-server address.
    pub server: Ipv4Addr,
    /// The tenpo address, `None` while unspecified.
    pub tenpo: Option<Ipv4Addr>,
    /// `Host:` and TLS name.
    pub host: String,
    /// PEM trust anchor.
    pub ca_pem: Vec<u8>,
    /// Keychip counters at the report start.
    pub counters: Counters,
    /// The trace log, oldest first, the `Auth start` message included.
    pub records: Vec<Record>,
    /// Entries per request.
    pub line_limit: usize,
    /// Time-zone bias in minutes (Windows sign, JST -540).
    pub bias_minutes: i32,
    /// Server clock `YYYYMMDDhhmmss`.
    pub date: String,
    /// The server clock is synchronised.
    pub date_synced: bool,
    /// Operating mode 1 or 2.
    pub operating_mode: u32,
    /// `memrepair=`.
    pub mem_repair: u32,
    /// The global `sn` counter at the start.
    pub serial: u32,
}

/// Fields the server sent (`ARParseDelivery`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    /// `result=`, -1 when absent.
    pub result: i32,
    /// `requestno=`.
    pub request_no: i32,
    /// `traceerase=`.
    pub trace_erase: i32,
    /// `waittime=` seconds, 0..=86400.
    pub wait_s: u32,
    /// `linelimit=` as `(foreground, background)` limits.
    pub line_limit: Option<(usize, usize)>,
    /// `message=` (33 characters at most).
    pub message: String,
    /// `playlimit=`.
    pub play_limit: Option<u32>,
    /// `playlimitsig=`.
    pub play_limit_sig: String,
    /// `playhistory=` as `(year, month, count)` x 3.
    pub history: Option<[(i32, i32, i32); 3]>,
    /// `fixinterval=` minutes.
    pub fix_interval: Option<u32>,
    /// `fixlogcnt=`.
    pub fix_log_count: Option<u32>,
    /// `nearfull=`.
    pub near_full: Option<u32>,
    /// `nearfullsig=`.
    pub near_full_sig: String,
}

impl Default for Delivery {
    fn default() -> Self {
        Self {
            result: -1,
            request_no: 0,
            trace_erase: 0,
            wait_s: 0,
            line_limit: None,
            message: String::new(),
            play_limit: None,
            play_limit_sig: String::new(),
            history: None,
            fix_interval: None,
            fix_log_count: None,
            near_full: None,
            near_full_sig: String::new(),
        }
    }
}

fn leading_number(value: &str) -> Option<i64> {
    let digits: String = value.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() { None } else { digits.parse().ok() }
}

fn parse_history(value: &str) -> Option<[(i32, i32, i32); 3]> {
    let mut out = [(0, 0, 0); 3];
    let mut parts = value.split(':');
    for slot in &mut out {
        let part = parts.next()?;
        let (year_month, count) = part.split_once('/')?;
        if year_month.len() != 6 {
            return None;
        }
        *slot = (year_month.get(..4)?.parse().ok()?, year_month.get(4..)?.parse().ok()?, leading_number(count)? as i32);
    }
    Some(out)
}

/// Parses a response body (`0x1403DE8F0`): `key=value` pairs separated by `&`, ending at the first CR or LF.
pub fn parse_delivery(body: &str) -> Delivery {
    let mut delivery = Delivery::default();
    let body = body.split(['\r', '\n']).next().unwrap_or("");
    for pair in body.split('&') {
        if let Some(value) = pair.strip_prefix("result=") {
            delivery.result = leading_number(value).map_or(-1, |v| v as i32);
        } else if let Some(value) = pair.strip_prefix("requestno=") {
            delivery.request_no = leading_number(value).map_or(0, |v| v as i32);
        } else if let Some(value) = pair.strip_prefix("traceerase=") {
            delivery.trace_erase = leading_number(value).map_or(0, |v| v as i32);
        } else if let Some(value) = pair.strip_prefix("waittime=") {
            delivery.wait_s = leading_number(value).map_or(0, |v| v.clamp(0, 86_400) as u32);
        } else if let Some(value) = pair.strip_prefix("linelimit=") {
            delivery.line_limit = Some(match leading_number(value) {
                Some(v) if v > 0 => ((v as usize).min(1000), (v as usize).min(300)),
                _ => (DEFAULT_LINE_LIMIT, DEFAULT_LINE_LIMIT),
            });
        } else if let Some(value) = pair.strip_prefix("message=") {
            delivery.message = value.chars().take(33).collect();
        } else if let Some(value) = pair.strip_prefix("playlimitsig=") {
            delivery.play_limit_sig = value.chars().take(256).collect();
        } else if let Some(value) = pair.strip_prefix("playlimit=") {
            delivery.play_limit = leading_number(value).and_then(|v| u32::try_from(v).ok());
        } else if let Some(value) = pair.strip_prefix("playhistory=") {
            delivery.history = parse_history(value);
        } else if let Some(value) = pair.strip_prefix("fixinterval=") {
            delivery.fix_interval = Some(match leading_number(value) {
                Some(v) if v > 9999 => 86_400,
                Some(v) => v as u32,
                None => 120,
            });
        } else if let Some(value) = pair.strip_prefix("fixlogcnt=") {
            delivery.fix_log_count = Some(match leading_number(value) {
                Some(v) => v.min(9999) as u32,
                None => 1,
            });
        } else if let Some(value) = pair.strip_prefix("nearfullsig=") {
            delivery.near_full_sig = value.chars().take(256).collect();
        } else if let Some(value) = pair.strip_prefix("nearfull=") {
            delivery.near_full = leading_number(value).and_then(|v| u32::try_from(v).ok());
        }
    }
    delivery
}

/// The base query (`ARStepGetTraceData`), built once per report.
pub fn base_query(job: &ReportJob) -> String {
    let tenpo = job.tenpo.map_or_else(|| "255.255.255.255".to_owned(), |ip| ip.to_string());
    let identity = &job.identity;
    format!(
        "keychipid={}&functype={}&gameid={}&gamever={}&boardid={}&tenpoip={tenpo}&libalibver=1.314&datamax=7154&billingtype={}&protocolver=1.000&operatingfix={}",
        identity.keychip,
        identity.function_type,
        identity.game_id,
        identity.game_version,
        identity.board,
        job.counters.billing_type,
        u8::from(job.role == Role::Background),
    )
}

/// The query line of one request, CR LF terminated.
pub fn query_line(job: &ReportJob, trace_left: usize, request_no: u32, crc_errors: u32) -> String {
    let timezone = super::schedule::timezone_number(job.bias_minutes);
    let timezone = if timezone < 0 { format!("{timezone:04}") } else { format!("{timezone:03}") };
    format!(
        "{}&traceleft={trace_left}&requestno={request_no}&datesync={}&timezone={timezone}&date={}&crcerrcnt={crc_errors}&memrepair={}&playcnt={}&playlimit={}&nearfull={}\r\n",
        base_query(job),
        u8::from(job.date_synced),
        job.date,
        job.mem_repair,
        job.counters.play_count,
        job.counters.play_limit,
        job.counters.near_full,
    )
}

/// The complete HTTP request: head plus the raw-deflate body (`alpbZlibCompress`, windowBits -15).
pub fn http_request(host: &str, body: &str, keep_alive: bool) -> Vec<u8> {
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
    // Writing into a `Vec` cannot fail.
    let _ = encoder.write_all(body.as_bytes());
    let compressed = encoder.finish().unwrap_or_default();
    let mut request = format!(
        "POST /request/ HTTP/1.1\r\nContent-Type: application/octet-stream\r\nHost: {host}\r\nConnection: {}\r\nCache-Control: no-cache\r\nContent-Length: {}\r\n\r\n",
        if keep_alive { "Keep-Alive" } else { "close" },
        compressed.len(),
    )
    .into_bytes();
    request.extend_from_slice(&compressed);
    request
}

/// A parsed HTTP response.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HttpReply {
    body: String,
    server_closes: bool,
}

/// Header check (`0x1403FE180`): HTTP 200 and a `Content-Length` are required.
fn parse_http(bytes: &[u8]) -> Option<HttpReply> {
    let split = bytes.windows(4).position(|window| window == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&bytes[..split]).ok()?;
    let mut lines = head.split("\r\n");
    if lines.next()?.split_whitespace().nth(1)? != "200" {
        return None;
    }
    let mut length = None;
    let mut closes = false;
    for line in lines {
        let (name, value) = line.split_once(':')?;
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => length = value.trim().parse::<usize>().ok(),
            "connection" => closes = value.trim().eq_ignore_ascii_case("close"),
            "transfer-encoding" => return None,
            _ => {}
        }
    }
    let body = bytes.get(split + 4..split + 4 + length?)?;
    Some(HttpReply { body: String::from_utf8_lossy(body).into_owned(), server_closes: closes })
}

/// What a whole report produced.
#[derive(Debug, Clone, Default)]
pub struct ReportOutcome {
    /// `ARGetStatus` after the report: 3 finished, negative codes per the status table.
    pub status: i32,
    /// The final server `result`, -1 when none arrived.
    pub result: i32,
    /// Messages to put into the trace log, in order.
    pub messages: Vec<String>,
    /// Trace entries the server acknowledged.
    pub acknowledged: usize,
    /// Requests sent.
    pub requests: usize,
    /// The server asked to erase the acknowledged entries.
    pub trace_erase: bool,
    /// New play limit and its signature.
    pub play_limit: Option<(u32, String)>,
    /// New near-full value and its signature.
    pub near_full: Option<(u32, String)>,
    /// `playhistory=` content.
    pub history: Option<[(i32, i32, i32); 3]>,
    /// Last `waittime=`.
    pub wait_s: u32,
    /// Server `linelimit=` limits `(foreground, background)`.
    pub line_limit: Option<(usize, usize)>,
    /// `fixinterval=`.
    pub fix_interval: Option<u32>,
    /// `fixlogcnt=`.
    pub fix_log_count: Option<u32>,
    /// A certificate date error while the clock is synced (`TermDateError`).
    pub term_date_error: bool,
    /// The global `sn` counter after the report.
    pub serial: u32,
}

/// Runs one report: batches of `line_limit` entries, retries, result handling (`SendTraceData`, `RecvResult`,
/// `Connect`, `Disconnect`). The caller applies the outcome.
pub fn run_report(job: &ReportJob, link: &mut ReportLink) -> ReportOutcome {
    let prefix = if job.role == Role::Foreground || job.first_skipped { "Auth" } else { "Shime" };
    let crc_errors = u32::try_from(job.records.iter().filter(|record| record[0] == 0xFF).count()).unwrap_or(0);
    let mut outcome = ReportOutcome { result: -1, serial: job.serial, ..ReportOutcome::default() };
    let mut line_limit = job.line_limit.max(1);
    let (mut failures, mut resends) = (0, 0);
    let (mut request_no, mut sent) = (0u32, 0usize);
    let mut new_session = true;
    let mut serial = job.serial;

    macro_rules! restart {
        () => {{
            request_no = 0;
            sent = 0;
            outcome.acknowledged = 0;
            new_session = true;
        }};
    }
    macro_rules! finish {
        ($message:expr) => {{
            outcome.messages.push($message);
            outcome.status = 3;
            outcome.serial = serial;
            return outcome;
        }};
    }

    loop {
        let remaining = job.records.len() - sent;
        let batch = remaining.min(line_limit);
        let last = remaining <= line_limit;
        let mut body = query_line(job, remaining.saturating_sub(line_limit), request_no, crc_errors);
        for record in &job.records[sent..sent + batch] {
            if let Some(line) = render(record, serial) {
                body.push_str(&line);
                serial = (serial + 1) % 999_999;
            }
        }
        let exchange = Exchange {
            address: job.server,
            port: REPORT_PORT,
            host: job.host.clone(),
            ca_pem: job.ca_pem.clone(),
            new_session,
            request: http_request(&job.host, &body, !last),
        };
        outcome.requests += 1;
        let reply = match link.exchange(&exchange) {
            Ok(bytes) => bytes,
            Err(error) => {
                match error {
                    LinkError::CertificateRejected => {
                        outcome.status = -10;
                        outcome.serial = serial;
                        return outcome;
                    }
                    LinkError::CertificateDate => {
                        if !job.date_synced {
                            finish!("error auth term".to_owned());
                        } else if job.counters.billing_type == 0 && job.operating_mode == 1 {
                            outcome.term_date_error = true;
                            finish!("error auth term".to_owned());
                        }
                        outcome.status = -11;
                        outcome.serial = serial;
                        return outcome;
                    }
                    _ => {}
                }
                failures += 1;
                if failures == 1 {
                    restart!();
                    continue;
                }
                finish!(match error {
                    LinkError::ConnectTimeout | LinkError::Timeout => format!("{prefix} timeout"),
                    LinkError::ConnectFailed => format!("{prefix} NG connect"),
                    LinkError::Tls => format!("{prefix} NG ssl"),
                    _ => format!("{prefix} NG network"),
                });
            }
        };
        new_session = false;
        let Some(http) = parse_http(&reply) else {
            failures += 1;
            if failures == 1 {
                restart!();
                continue;
            }
            finish!(format!("{prefix} NG network"));
        };
        let delivery = parse_delivery(&http.body);
        request_no += 1;
        outcome.result = delivery.result;
        outcome.wait_s = delivery.wait_s;
        if let Some(limits) = delivery.line_limit {
            outcome.line_limit = Some(limits);
            line_limit = if job.role == Role::Foreground { limits.0 } else { limits.1 };
        }
        outcome.fix_interval = delivery.fix_interval.or(outcome.fix_interval);
        outcome.fix_log_count = delivery.fix_log_count.or(outcome.fix_log_count);
        if delivery.history.is_some() {
            outcome.history = delivery.history;
        }
        match delivery.result {
            0 | 6 => {
                outcome.acknowledged += batch;
                sent += batch;
                outcome.trace_erase = delivery.trace_erase == 1;
                if let Some(limit) = delivery.play_limit {
                    outcome.play_limit = Some((limit, delivery.play_limit_sig.clone()));
                }
                if let Some(near_full) = delivery.near_full {
                    outcome.near_full = Some((near_full, delivery.near_full_sig.clone()));
                }
                if last || http.server_closes {
                    if last || delivery.result != 6 {
                        if delivery.result == 0 {
                            outcome.messages.push(format!("{prefix} OK"));
                        }
                        outcome.status = 3;
                        outcome.serial = serial;
                        return outcome;
                    }
                    new_session = true;
                }
            }
            3 if resends < 3 => {
                resends += 1;
                restart!();
            }
            1 | 2 | 3 | 4 | 5 | 9 => {
                let message: String =
                    format!("{prefix} NG(RC={}):{}", delivery.result, delivery.message).chars().take(32).collect();
                finish!(message);
            }
            _ => finish!(format!("{prefix} NG decode")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_fields_are_parsed_with_the_documented_clamps() {
        let delivery = parse_delivery(
            "result=0&requestno=3&traceerase=1&waittime=999999&linelimit=5000&message=hello&playlimit=77&playlimitsig=AB&playhistory=202405/3:202404/2:202403/1&fixinterval=20000&fixlogcnt=x&nearfull=65546\r\nignored=1",
        );
        assert_eq!(delivery.result, 0);
        assert_eq!(delivery.wait_s, 86_400);
        assert_eq!(delivery.line_limit, Some((1000, 300)));
        assert_eq!(delivery.play_limit, Some(77));
        assert_eq!(delivery.history, Some([(2024, 5, 3), (2024, 4, 2), (2024, 3, 1)]));
        assert_eq!(delivery.fix_interval, Some(86_400));
        assert_eq!(delivery.fix_log_count, Some(1));
        assert_eq!(delivery.near_full, Some(65_546));
    }

    #[test]
    fn non_numeric_result_stays_unset() {
        assert_eq!(parse_delivery("result=x").result, -1);
    }
}
