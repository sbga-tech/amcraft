//! The alAbEx power-on / download-order client (`spec/libam/allnet.md` part 5): request building, the
//! zlib + base64 body transform, the HTTP/1.0 framing, response acceptance, the `stat=` / `token=` / `res_ver=`
//! checks, the key tables and the serial-derived retry delays. Everything here is pure; the exchange itself runs
//! through [`super::exchange`] over the backend's TLS client.

use std::io::{Read, Write};
use std::net::Ipv4Addr;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use md5::{Digest, Md5};

/// Size limit of both response buffers and of the encoded request bodies' source text.
pub const RESPONSE_LIMIT: usize = 4096;

/// The request mode (`root+140`): which URL, User-Agent, key table and body format a session uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Plain HTTP to `naominet.jp` (format 3).
    Plain,
    /// Title-router mode: TLS power-on to `at.sys-all.net` (format 5).
    Title,
    /// Auth 2.0: TLS to `op.auth.sys-all.net` with `auth_data` (format 6).
    AuthData,
}

impl Mode {
    /// The `format_ver` number.
    pub fn format(self) -> u32 {
        match self {
            Self::Plain => 3,
            Self::Title => 5,
            Self::AuthData => 6,
        }
    }

    /// The `res_ver` the power-on answer must echo.
    fn expected_res_ver(self) -> u32 {
        if self == Self::Title { 5 } else { 3 }
    }

    /// `(host, path)` of the power-on request.
    pub fn power_on_url(self) -> (&'static str, &'static str) {
        match self {
            Self::Plain => ("naominet.jp", "/sys/servlet/PowerOn"),
            Self::Title => ("at.sys-all.net", "/net/activate"),
            Self::AuthData => ("op.auth.sys-all.net", "/net/operation"),
        }
    }

    /// `(host, path)` of the download-order request.
    pub fn dl_order_url(self) -> (&'static str, &'static str) {
        match self {
            Self::AuthData => ("op.auth.sys-all.net", "/net/downloadorder"),
            _ => ("naominet.jp", "/sys/servlet/DownloadOrder"),
        }
    }

    /// Whether the power-on is sent over TLS (port 443).
    pub fn power_on_tls(self) -> bool {
        self != Self::Plain
    }

    /// Whether the download order is sent over TLS.
    pub fn dl_order_tls(self) -> bool {
        self == Self::AuthData
    }

    fn power_on_agent(self) -> u32 {
        self.format()
    }

    fn dl_order_agent(self) -> u32 {
        if self == Self::AuthData { 6 } else { 3 }
    }
}

/// The parameter block alw hands to `alAbExInit` (`allnet.md` 5 1.2).
#[derive(Debug, Clone)]
pub struct Session {
    /// Request mode.
    pub mode: Mode,
    /// Game id (`game_id=`).
    pub game_id: String,
    /// Game version (`ver=`).
    pub version: String,
    /// Keychip serial (`serial=`).
    pub serial: String,
    /// Client IPv4 (`ip=`).
    pub client_ip: Ipv4Addr,
    /// Power-on target.
    pub server_ip: Ipv4Addr,
    /// Download-order target (`root+1972`).
    pub server_ip2: Ipv4Addr,
    /// Traceroute hop count (`hops=`).
    pub hops: i32,
    /// `auth_data` text (mode 6).
    pub auth_data: Option<String>,
    /// `encode=` name; this build always reports UTF-8.
    pub encode: &'static str,
}

impl Session {
    /// The power-on text body, ending with CR LF.
    pub fn power_on_text(&self, token: u32) -> String {
        let o = self.client_ip.octets();
        let encode = self.encode;
        let (game, ver, serial) = (&self.game_id, &self.version, &self.serial);
        match self.mode {
            Mode::Title => format!(
                "title_id={game}&title_ver={ver}&machine={serial}&firm_ver=60001&boot_ver=0000&encode={encode}&format_ver=5&token={token}\r\n"
            ),
            mode => {
                let mut text = format!(
                    "game_id={game}&ver={ver}&serial={serial}&ip={}.{}.{}.{}&firm_ver=60001&boot_ver=0000&encode={encode}&format_ver={}&hops={}&token={token}",
                    o[0],
                    o[1],
                    o[2],
                    o[3],
                    mode.format(),
                    self.hops
                );
                if let (Mode::AuthData, Some(data)) = (mode, &self.auth_data) {
                    text.push_str("&auth_data=");
                    text.push_str(data);
                }
                text.push_str("\r\n");
                text
            }
        }
    }

    /// The download-order text body.
    pub fn dl_order_text(&self) -> String {
        let mut text =
            format!("game_id={}&ver={}&serial={}&encode={}", self.game_id, self.version, self.serial, self.encode);
        if let (Mode::AuthData, Some(data)) = (self.mode, &self.auth_data) {
            text.push_str("&auth_data=");
            text.push_str(data);
        }
        text.push_str("\r\n");
        text
    }

    /// The HTTP request bytes of the power-on (`compress`: modes 3 and 6; mode 5 is sent plain).
    pub fn power_on_request(&self, token: u32) -> Result<Vec<u8>, i32> {
        let text = self.power_on_text(token);
        let compress = self.mode != Mode::Title;
        let body = if compress { encode_body(&text)? } else { text };
        let (host, path) = self.mode.power_on_url();
        Ok(http_request(host, path, self.mode.power_on_agent(), compress, &body))
    }

    /// The HTTP request bytes of the download order (always compressed).
    pub fn dl_order_request(&self) -> Result<Vec<u8>, i32> {
        let body = encode_body(&self.dl_order_text())?;
        let (host, path) = self.mode.dl_order_url();
        Ok(http_request(host, path, self.mode.dl_order_agent(), true, &body))
    }

    /// The `(address, tls)` target of the power-on.
    pub fn power_on_target(&self) -> (Ipv4Addr, bool) {
        (self.server_ip, self.mode.power_on_tls())
    }

    /// The `(address, tls)` target of the download order (always address #2).
    pub fn dl_order_target(&self) -> (Ipv4Addr, bool) {
        (self.server_ip2, self.mode.dl_order_tls())
    }
}

/// zlib + base64 of a request body (`-11` when too large, `-20` on a deflate failure).
pub fn encode_body(text: &str) -> Result<String, i32> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(text.as_bytes()).map_err(|_| -20)?;
    let packed = encoder.finish().map_err(|_| -20)?;
    if packed.len() * 2 > RESPONSE_LIMIT {
        return Err(-11);
    }
    let encoded = STANDARD.encode(packed);
    if encoded.len() >= RESPONSE_LIMIT {
        return Err(-11);
    }
    Ok(encoded)
}

fn http_request(host: &str, path: &str, agent: u32, pragma: bool, body: &str) -> Vec<u8> {
    let mut out = format!("POST {path} HTTP/1.0\r\nConnection: Close\r\n");
    if pragma {
        out.push_str("Pragma: DFI\r\n");
    }
    out.push_str(&format!(
        "User-Agent: Windows/ver.{agent}.0\r\nHost: {host}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    ));
    out.into_bytes()
}

/// The request id (`0x1403F8360`): MD5 of the doubled 32-byte block folded to 32 bits. The block holds the
/// local epoch seconds, the game id, the serial and the client address; the original's raw single-block MD5 and
/// response-buffer pointer are not reproducible, and the server only echoes the value.
pub fn token(epoch: u32, game_id: &str, serial: &str, client_ip: Ipv4Addr, salt: u64) -> u32 {
    let mut block = [0u8; 32];
    block[..4].copy_from_slice(&epoch.to_le_bytes());
    for (slot, byte) in block[4..8].iter_mut().zip(game_id.bytes()) {
        *slot = byte;
    }
    for (slot, byte) in block[8..19].iter_mut().zip(serial.bytes()) {
        *slot = byte;
    }
    block[20..24].copy_from_slice(&client_ip.octets());
    block[24..].copy_from_slice(&salt.to_le_bytes());
    let mut input = Md5::new();
    input.update(block);
    input.update(block);
    let digest = input.finalize();
    digest.chunks(4).fold(0, |acc, quad| acc ^ u32::from_be_bytes([quad[0], quad[1], quad[2], quad[3]]))
}

/// `(rand_off, rand_wait)` in seconds from the serial digits (`allnet.md` 5 4.5). `rand_off` is 60..598 and
/// `rand_wait` 0..118 for a valid serial.
pub fn serial_delays(serial: &str) -> (u32, u32) {
    let bytes = serial.as_bytes();
    let at = |index: usize| i32::from(bytes.get(index).copied().unwrap_or(0));
    let x = at(9).wrapping_add(10_i32.wrapping_mul(at(8).wrapping_add(10_i32.wrapping_mul(at(7)))));
    let y = at(10).wrapping_add(2_i32.wrapping_mul(x.wrapping_add(4_i32.wrapping_mul(x.wrapping_sub(6666)))));
    let off = 2 * y.wrapping_rem(270) + 60;
    let wait = 2 * y.wrapping_rem(60);
    (off.unsigned_abs(), wait.unsigned_abs())
}

/// The kind of request a response answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Power-on.
    PowerOn,
    /// Download order.
    DlOrder,
}

/// The text of an accepted HTTP answer (`alAbExCmnCheckResponse`, `allnet.md` 5 5.1), or the library error.
pub fn check_response(raw: &[u8], kind: Kind) -> Result<Vec<u8>, i32> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|at| (at, 4))
        .or_else(|| raw.windows(2).position(|w| w == b"\n\n").map(|at| (at, 2)));
    let (head, body) = match split {
        Some((at, len)) => (&raw[..at], &raw[at + len..]),
        None => (raw, &raw[raw.len()..]),
    };
    let head = String::from_utf8_lossy(head);
    let mut lines = head.lines();
    let status = lines.next().unwrap_or_default();
    let mut parts = status.splitn(3, ' ');
    let version = parts.next().unwrap_or_default();
    if version != "HTTP/1.0" && version != "HTTP/1.1" {
        return Err(-30);
    }
    if parts.next() != Some("200") {
        return Err(-31);
    }
    if !parts.next().unwrap_or_default().trim().eq_ignore_ascii_case("OK") {
        return Err(-32);
    }
    let mut encoded = false;
    for line in lines.take(15) {
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("Pragma")
        {
            if !value.trim().eq_ignore_ascii_case("DFI") {
                return Err(-33);
            }
            encoded = true;
            break;
        }
    }
    if body.len() > RESPONSE_LIMIT {
        return Err(-11);
    }
    if !encoded {
        return Ok(body.to_vec());
    }
    if kind == Kind::PowerOn && !body.windows(2).any(|w| w == b"\r\n") {
        return Err(-33);
    }
    let mut staged = Vec::new();
    let mut rest = body;
    while !rest.is_empty() {
        let (line, next, complete) = match rest.windows(2).position(|w| w == b"\r\n") {
            Some(at) => (&rest[..at], &rest[at + 2..], true),
            None => (rest, &rest[rest.len()..], false),
        };
        if complete || line.contains(&b'\n') {
            staged.extend(lenient_base64(line.strip_prefix(b" ").unwrap_or(line)));
        }
        rest = next;
    }
    let mut text = Vec::new();
    ZlibDecoder::new(staged.as_slice()).take(RESPONSE_LIMIT as u64 + 1).read_to_end(&mut text).map_err(|_| -21)?;
    if text.len() > RESPONSE_LIMIT {
        return Err(-21);
    }
    Ok(text)
}

/// The lenient base64 decoder: a quad stops at the first character outside the alphabet.
pub(super) fn lenient_base64(line: &[u8]) -> Vec<u8> {
    let value = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0u32);
    for sextet in line.iter().map_while(|&c| value(c)) {
        acc = (acc << 6) | u32::from(sextet);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

/// The time and zone a power-on answer carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerTime {
    /// UTC year.
    pub year: i32,
    /// UTC month.
    pub month: i32,
    /// UTC day.
    pub day: i32,
    /// UTC hour.
    pub hour: i32,
    /// UTC minute.
    pub minute: i32,
    /// UTC second.
    pub second: i32,
    /// `client_timezone` in minutes.
    pub tz_minutes: i32,
}

/// What the power-on notifier callback receives (`allnet.md` 5 5.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    /// Code 1 with the server time.
    Time(ServerTime),
    /// The server's `stat` failure (-1..-4) or `0x80000000` for an unknown value, or `0x7FFFFFFF` for a
    /// validation failure.
    Code(i64),
}

/// A power-on answer after `0x1403F7BC0`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// The NUL-split `key=value` text (the `&` separators replaced by NUL, six keys percent-decoded).
    text: Vec<u8>,
    /// The notifier payload, `None` when the parser returned before the notifier.
    pub notice: Option<Notice>,
    /// The parser result: 0 ok or the `-40..-45` code.
    pub result: i32,
    /// Whether the key table is the title table.
    title: bool,
}

const NORMAL_KEYS: [&str; 16] = [
    "uri=",
    "host=",
    "name=",
    "nickname=",
    "region0=",
    "region_name0=",
    "region_name1=",
    "region_name2=",
    "region_name3=",
    "place_id=",
    "setting=",
    "country=",
    "allnet_id=",
    "client_timezone=",
    "res_ver=",
    "",
];

const TITLE_KEYS: [&str; 16] = [
    "title_uri=",
    "title_host=",
    "name=",
    "nickname=",
    "region0=",
    "region_name0=",
    "region_name1=",
    "region_name2=",
    "region_name3=",
    "place_id=",
    "setting=",
    "country=",
    "allnet_id=",
    "client_timezone=",
    "res_ver=",
    "location_ip=",
];

fn find(text: &[u8], key: &str) -> Option<usize> {
    if key.is_empty() {
        return None;
    }
    text.windows(key.len()).position(|w| w == key.as_bytes())
}

/// The `strtol`-style prefix of `text`: value and the rest.
fn strtol(text: &[u8]) -> (i64, &[u8]) {
    let mut at = 0;
    while text.get(at).is_some_and(|c| c.is_ascii_whitespace()) {
        at += 1;
    }
    let negative = text.get(at) == Some(&b'-');
    if matches!(text.get(at), Some(b'-' | b'+')) {
        at += 1;
    }
    let digits = at;
    let mut value: i64 = 0;
    while let Some(digit) = text.get(at).filter(|c| c.is_ascii_digit()) {
        value = value.saturating_mul(10).saturating_add(i64::from(digit - b'0'));
        at += 1;
    }
    if at == digits {
        return (0, text);
    }
    (if negative { -value } else { value }, &text[at..])
}

/// `0x1403F77E0`: the `stat=` / `result=` check.
fn check_result_key(text: &[u8], key: &str) -> i32 {
    let Some(at) = find(text, key) else { return -44 };
    let (value, rest) = strtol(&text[at + key.len()..]);
    if !matches!(rest.first(), None | Some(b'&' | 0)) {
        return -44;
    }
    match value {
        1 => 0,
        -1 => -40,
        -2 => -41,
        -3 => -42,
        -4 => -45,
        _ => -43,
    }
}

fn notice_code(value: i32, text: &[u8], key: &str) -> i64 {
    let at = find(text, key).map(|at| strtol(&text[at + key.len()..]).0);
    match (value, at) {
        (-40, _) => -1,
        (-41, _) => -2,
        (-42, _) => -3,
        (-45, _) => -4,
        _ => 0x8000_0000,
    }
}

fn unsigned_after(text: &[u8], key: &str) -> Option<u32> {
    let at = find(text, key)?;
    let rest = &text[at + key.len()..];
    let digits = rest.iter().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 || !matches!(rest.get(digits), Some(b'&' | b'\n' | b'\r')) {
        return None;
    }
    std::str::from_utf8(&rest[..digits]).ok()?.parse().ok()
}

fn timezone(text: &[u8]) -> Option<i32> {
    let at = find(text, "client_timezone=")?;
    let rest = &text[at + "client_timezone=".len()..];
    let sign = match rest.first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let digits = rest.get(1..5)?;
    if !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let number = |range: std::ops::Range<usize>| -> i32 {
        digits[range].iter().fold(0, |acc, d| acc * 10 + i32::from(d - b'0'))
    };
    Some(sign * (number(0..2) * 60 + number(2..4)))
}

fn utc_time(text: &[u8], tz_minutes: i32) -> Option<ServerTime> {
    let at = find(text, "utc_time=")?;
    let field = text.get(at + 9..at + 29)?;
    let digit_at = [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18];
    let shape = [(4, b'-'), (7, b'-'), (10, b'T'), (13, b':'), (16, b':'), (19, b'Z')];
    if !digit_at.iter().all(|&i| field[i].is_ascii_digit()) || !shape.iter().all(|&(i, c)| field[i] == c) {
        return None;
    }
    let number = |from: usize, to: usize| field[from..to].iter().fold(0, |acc, d| acc * 10 + i32::from(d - b'0'));
    Some(ServerTime {
        year: number(0, 4),
        month: number(5, 7),
        day: number(8, 10),
        hour: number(11, 13),
        minute: number(14, 16),
        second: number(17, 19),
        tz_minutes,
    })
}

fn percent_decode(value: &[u8]) -> Vec<u8> {
    let hex = |c: u8| char::from(c).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(value.len());
    let mut at = 0;
    while at < value.len() {
        match value[at] {
            b'+' => out.push(b' '),
            b'%' => match (value.get(at + 1).copied().and_then(hex), value.get(at + 2).copied().and_then(hex)) {
                (Some(high), Some(low)) => {
                    out.push(high * 16 + low);
                    at += 2;
                }
                _ => out.push(b'%'),
            },
            other => out.push(other),
        }
        at += 1;
    }
    out
}

/// Splits the text at `&` and percent-decodes the six listed values (`0x1403F6DB0`).
fn tokenize(text: &[u8]) -> Vec<u8> {
    const DECODED: [&str; 6] = ["name", "nickname", "region_name0", "region_name1", "region_name2", "region_name3"];
    let pieces: Vec<Vec<u8>> = text
        .split(|&c| c == b'&')
        .map(|piece| match piece.iter().position(|&c| c == b'=') {
            Some(eq) => {
                let key = &piece[..eq];
                if DECODED.iter().any(|literal| literal.as_bytes().starts_with(key)) {
                    let mut out = piece[..=eq].to_vec();
                    out.extend(percent_decode(&piece[eq + 1..]));
                    out
                } else {
                    piece.to_vec()
                }
            }
            None => piece.to_vec(),
        })
        .collect();
    pieces.join(&0u8)
}

/// The power-on parser (`0x1403F7BC0`) on the accepted text of a mode `mode` answer.
pub fn parse_power_on(text: &[u8], mode: Mode, sent_token: u32) -> Response {
    let title = mode == Mode::Title;
    let result_key = if title { "result=" } else { "stat=" };
    let verdict = check_result_key(text, result_key);
    if verdict == -44 {
        return Response { text: Vec::new(), notice: None, result: -44, title };
    }
    if verdict != 0 {
        let notice = Some(Notice::Code(notice_code(verdict, text, result_key)));
        return Response { text: tokenize(text), notice, result: verdict, title };
    }
    let uri_key = if title { "title_uri=" } else { "uri=" };
    let host_key = if title { "title_host=" } else { "host=" };
    let parsed = (|| {
        let tz = timezone(text)?;
        let time = utc_time(text, tz)?;
        for key in [uri_key, host_key, "allnet_id="] {
            find(text, key)?;
        }
        (unsigned_after(text, "token=")? == sent_token).then_some(())?;
        (unsigned_after(text, "res_ver=")? == mode.expected_res_ver()).then_some(())?;
        Some(time)
    })();
    let (notice, result) = match parsed {
        Some(time) => (Notice::Time(time), 0),
        None => (Notice::Code(0x7FFF_FFFF), -44),
    };
    Response { text: tokenize(text), notice: Some(notice), result, title }
}

/// The download-order check (`0x1403F8030`): the text split at `&`, or the result code.
pub fn parse_dl_order(text: &[u8]) -> Result<Response, i32> {
    let verdict = check_result_key(text, "stat=");
    if verdict != 0 {
        return Err(verdict);
    }
    if find(text, "uri=").is_none() {
        return Err(-44);
    }
    let split: Vec<u8> = text.iter().map(|&c| if c == b'&' { 0 } else { c }).collect();
    Ok(Response { text: split, notice: None, result: 0, title: false })
}

impl Response {
    /// `alAbExGetValue(index)`: the value after the first occurrence of the key, empty when absent.
    pub fn value(&self, index: usize) -> Vec<u8> {
        let key = if self.title { TITLE_KEYS } else { NORMAL_KEYS }.get(index).copied().unwrap_or_default();
        match find(&self.text, key) {
            Some(at) => {
                let rest = &self.text[at + key.len()..];
                rest[..rest.iter().position(|&c| c == 0).unwrap_or(rest.len())].to_vec()
            }
            None => Vec::new(),
        }
    }

    /// `alAbExGetDlUri`: the text after the first `uri=`.
    pub fn dl_uri(&self) -> Vec<u8> {
        match find(&self.text, "uri=") {
            Some(at) => {
                let rest = &self.text[at + 4..];
                rest[..rest.iter().position(|&c| c == 0).unwrap_or(rest.len())].to_vec()
            }
            None => Vec::new(),
        }
    }
}

/// The library state after an error code, per the error maps (`allnet.md` 5 2.1): `-499` / `-799` for protocol
/// results, `-198` for the rest.
pub fn map_protocol(kind: Kind, result: i32) -> i32 {
    if matches!(result, -45..=-40 | -33..=-30) { if kind == Kind::PowerOn { -499 } else { -799 } } else { -198 }
}
