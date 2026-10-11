//! The sales management site (SMS): the JSON-over-HTTP protocol of `spec/libam/payment.md` PayEmSvc section 7 (the five
//! requests, the InitSettings parser, the certificate check), and the service state the Manager keeps for it.

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use md5::{Digest, Md5};
use serde_json::Value;

use backend::{HttpClient, HttpRequest, HttpResponse, Method};

/// Largest request body the HTTP wrapper sends (7.1).
pub const MAX_BODY: usize = 4096;
/// Largest response body the wrapper keeps (7.1).
pub const MAX_RESPONSE: usize = 8192;
/// Connect and total timeout of a request (the original relies on the WinHTTP defaults).
const TIMEOUT: Duration = Duration::from_secs(10);

/// Why a request of the site failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SmsError {
    /// The body would not be sent (too large, or text the code page cannot hold).
    #[error("request body refused")]
    Refused,
    /// No HTTP status came back.
    #[error("transfer failed: {0}")]
    Transport(String),
    /// The HTTP status was not accepted.
    #[error("HTTP status {0}")]
    Status(u16),
    /// The response failed the validation of 7.3.
    #[error("invalid response: {0}")]
    Invalid(&'static str),
}

/// `InitSettings` (7.4), as the site hands it out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InitSettings {
    /// `endpoints.terminals.uri` (`+0`).
    pub terminals_url: String,
    /// `endpoints.statuses.uri` (`+256`).
    pub statuses_url: String,
    /// `endpoints.sales.uri` (`+512`); empty = disabled.
    pub sales_url: String,
    /// `endpoints.counters.uri` (`+768`); empty = disabled.
    pub counters_url: String,
    /// `intervals.checkSetting`, minutes (`+1024`).
    pub check_minutes: u32,
    /// `intervals.sendStatus`, minutes (`+1028`).
    pub status_minutes: u32,
    /// `settigsType == "AmusementTerminalSettings"`.
    pub settings_type: bool,
    /// `status` (at most two digits).
    pub status: String,
    /// `terminalId` (30 digits).
    pub terminal_id: String,
    /// `version`.
    pub version: String,
    /// `availableElectronicMoney` (whitelisted codes; other codes are stored as 0).
    pub brands: Vec<u32>,
    /// The code 91 was listed.
    pub brand_91: bool,
    /// `cashAvailability`.
    pub cash: bool,
    /// `productCode`.
    pub product_code: Option<u32>,
}

impl InitSettings {
    /// The status as the five report strings use it (`"00"`, `"10"`, ... or empty).
    pub fn status_code_text(&self) -> String {
        self.status.parse::<u32>().map_or_else(|_| String::new(), |code| format!("{code:02}"))
    }

    /// The status the Manager's gates read (`M+29316`): the number without leading zeros, empty when absent.
    pub fn status_number(&self) -> String {
        self.status.parse::<u32>().map_or_else(|_| String::new(), |code| code.to_string())
    }
}

fn interval(value: Option<&Value>) -> Option<u32> {
    let number = value?.as_i64()?;
    (1..=i64::from(i32::MAX)).contains(&number).then_some(number as u32)
}

/// 7.4: `version` must start with `YYYY-MM-DDTHH:MM:SS` (year >= 1900, month != 0) and be shorter than 30 characters.
fn version_ok(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() >= 30 || bytes.len() < 19 {
        return false;
    }
    let digits = |range: std::ops::Range<usize>| bytes[range].iter().all(u8::is_ascii_digit);
    let number = |range: std::ops::Range<usize>| text[range].parse::<u32>().unwrap_or(0);
    digits(0..4)
        && bytes[4] == b'-'
        && digits(5..7)
        && bytes[7] == b'-'
        && digits(8..10)
        && bytes[10] == b'T'
        && digits(11..13)
        && bytes[13] == b':'
        && digits(14..16)
        && bytes[16] == b':'
        && digits(17..19)
        && number(0..4) >= 1900
        && number(5..7) != 0
}

/// 7.4 (`0x1402B3420`): the whole object is invalid when any required member is missing or wrong.
pub fn parse_init_settings(root: &Value) -> Option<InitSettings> {
    let endpoints = root.get("endpoints")?;
    let settings_endpoint = |name: &str, required: bool| match endpoints.get(name) {
        None if required => None,
        None => Some(String::new()),
        Some(object) => {
            let uri = object.as_object()?.get("uri")?.as_str()?;
            (uri.encode_utf16().count() <= 127).then(|| uri.to_owned())
        }
    };
    let terminals_url = settings_endpoint("terminals", true)?;
    let statuses_url = settings_endpoint("statuses", true)?;
    let sales_url = settings_endpoint("sales", false)?;
    let counters_url = settings_endpoint("counters", false)?;
    let intervals = root.get("intervals")?;
    let check_minutes = interval(intervals.get("checkSetting"))?;
    let status_minutes = interval(intervals.get("sendStatus"))?;
    let settings_type = root.get("settigsType")?.as_str()? == "AmusementTerminalSettings";
    let status = root.get("status")?.as_str()?;
    if status.len() > 2 || !status.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let terminal_id = root.get("terminalId")?.as_str()?;
    if terminal_id.len() != 30 || !terminal_id.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let version = match root.get("version") {
        None => String::new(),
        Some(value) => {
            let text = value.as_str()?;
            if !version_ok(text) {
                return None;
            }
            text.to_owned()
        }
    };
    let mut brands = Vec::new();
    let mut brand_91 = false;
    for element in root.get("availableElectronicMoney")?.as_array()? {
        let code = i32::try_from(element.as_i64()?).ok()?;
        match code {
            1 | 2 | 3 | 5 | 6 | 8 | 9 => brands.push(code as u32),
            91 => brand_91 = true,
            _ => brands.push(0),
        }
    }
    let cash = match root.get("cashAvailability") {
        None => false,
        Some(value) => value.as_bool()?,
    };
    let product_code = match root.get("productCode") {
        None => None,
        Some(value) => Some(u32::try_from(i32::try_from(value.as_i64()?).ok()?).ok()?),
    };
    Some(InitSettings {
        terminals_url,
        statuses_url,
        sales_url,
        counters_url,
        check_minutes,
        status_minutes,
        settings_type,
        status: status.to_owned(),
        terminal_id: terminal_id.to_owned(),
        version,
        brands,
        brand_91,
        cash,
        product_code,
    })
}

/// The status-report block (`service+1152`, 7.3.3). Times are `(year, month, day, hour, minute, second)` local.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatusReport {
    /// `appVersion` (always empty).
    pub app_version: String,
    /// `kernelVersion` (always empty).
    pub kernel_version: String,
    /// `moduleVersion` (the ThincaPayment version).
    pub module_version: String,
    /// `rwAdapterVersion`.
    pub rw_adapter_version: String,
    /// `scheduledAcquisitionTimeOfSetting`.
    pub next_check: [u16; 6],
    /// `settingsReflectionTime`.
    pub reflection: [u16; 6],
    /// `settingsVersion`.
    pub settings_version: String,
    /// `statusCode`.
    pub status_code: String,
}

/// Days from 1970-01-01 of a civil date (proleptic Gregorian).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// `time + seconds` on a local wall-clock stamp (no time zone adjustment, as the original).
pub fn add_seconds(time: [u16; 6], seconds: u64) -> [u16; 6] {
    let [year, month, day, hour, minute, second] = time.map(i64::from);
    let base = days_from_civil(year, month.max(1), day.max(1)) * 86_400 + hour * 3600 + minute * 60 + second;
    let total = base + seconds as i64;
    let (year, month, day) = civil_from_days(total.div_euclid(86_400));
    let rest = total.rem_euclid(86_400);
    [year as u16, month as u16, day as u16, (rest / 3600) as u16, (rest % 3600 / 60) as u16, (rest % 60) as u16]
}

fn stamp(time: [u16; 6]) -> String {
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}", time[0], time[1], time[2], time[3], time[4], time[5])
}

/// The service object of the Manager (`M+28280`): the settings the site gave out and the report block.
#[derive(Debug, Clone, Default)]
pub struct Service {
    /// The InitSettings block.
    pub settings: InitSettings,
    /// The stored `ETag` of the last CheckSetting answer.
    pub etag: String,
    /// The status report.
    pub report: StatusReport,
}

impl Service {
    /// 7.3: `/serial` goes behind the terminals, statuses and (when set) counters URL.
    pub fn append_serial(&mut self, serial: &str) {
        if serial.is_empty() {
            return;
        }
        self.settings.terminals_url.push_str(&format!("/{serial}"));
        self.settings.statuses_url.push_str(&format!("/{serial}"));
        if !self.settings.counters_url.is_empty() {
            self.settings.counters_url.push_str(&format!("/{serial}"));
        }
    }

    /// Adopts new settings (the serial suffix is appended).
    pub fn adopt(&mut self, settings: InitSettings, serial: &str) {
        self.settings = settings;
        self.append_serial(serial);
    }

    /// 7.3.1 / 7.3.2: the report block after an Activate or CheckSetting answer.
    pub fn stamp(&mut self, now: [u16; 6]) {
        self.report.reflection = now;
        self.report.next_check = add_seconds(now, 60 * u64::from(self.settings.check_minutes));
        self.report.settings_version = self.settings.version.clone();
        self.report.status_code = self.settings.status_code_text();
    }
}

fn js(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_owned())
}

fn host_of(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split('/').next().unwrap_or("");
    authority.split(':').next().unwrap_or("").to_owned()
}

fn narrow(text: &str) -> Result<&str, SmsError> {
    if text.is_ascii() && text.len() < 32 { Ok(text) } else { Err(SmsError::Refused) }
}

fn header<'a>(response: &'a HttpResponse, name: &str) -> Option<&'a str> {
    response.headers.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
}

/// 7.1 / 7.2: one request; any 2xx (and `also`) is accepted, the body is cut at 8192 bytes.
fn call(
    http: &mut dyn HttpClient,
    method: Method,
    url: &str,
    extra: Option<(&str, &str)>,
    body: Vec<u8>,
    also: Option<u16>,
) -> Result<HttpResponse, SmsError> {
    if body.len() > MAX_BODY {
        return Err(SmsError::Refused);
    }
    let mut headers = vec![("Host".to_owned(), host_of(url)), ("Accept".to_owned(), "application/json".to_owned())];
    if let Some((name, value)) = extra.filter(|(name, value)| !name.is_empty() && !value.is_empty()) {
        headers.push((name.to_owned(), value.to_owned()));
    }
    let request = HttpRequest { method, url: url.to_owned(), headers, body, timeout: TIMEOUT };
    let mut response = http.request(&request).map_err(|error| SmsError::Transport(error.to_string()))?;
    if !(200..300).contains(&response.status) && Some(response.status) != also {
        return Err(SmsError::Status(response.status));
    }
    response.body.truncate(MAX_RESPONSE);
    Ok(response)
}

const JSON_TYPE: Option<(&str, &str)> = Some(("Content-Type", "application/json"));

/// What a successful Activate request yields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activated {
    /// The settings of the answer.
    pub settings: InitSettings,
    /// The decoded PKCS#12 certificate.
    pub certificate: Vec<u8>,
}

/// 7.3.1: POST to the activation URL.
pub fn activate(
    http: &mut dyn HttpClient,
    url: &str,
    model: &str,
    serial: &str,
    merchant: &str,
    branch: u32,
    store_card_id: &str,
) -> Result<Activated, SmsError> {
    let body = format!(
        "{{\"modelName\":{},\"serialNumber\":{},\"merchantCode\":{},\"storeBranchNumber\":{branch},\"storeCardId\":{}}}",
        js(model),
        js(narrow(serial)?),
        js(merchant),
        js(store_card_id)
    );
    let response = call(http, Method::Post, url, JSON_TYPE, body.into_bytes(), None)?;
    let root: Value = serde_json::from_slice(&response.body).map_err(|_| SmsError::Invalid("not JSON"))?;
    let object = root.as_object().ok_or(SmsError::Invalid("not an object"))?;
    let text: String = object
        .get("certificate")
        .and_then(Value::as_str)
        .ok_or(SmsError::Invalid("certificate"))?
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let certificate = STANDARD.decode(text).map_err(|_| SmsError::Invalid("certificate encoding"))?;
    if certificate.len() > 4096 {
        return Err(SmsError::Invalid("certificate size"));
    }
    let settings = object.get("initSettings").and_then(parse_init_settings).ok_or(SmsError::Invalid("initSettings"))?;
    let expected =
        header(&response, "x-certificate-md5").and_then(parse_hex16).ok_or(SmsError::Invalid("certificate digest"))?;
    if Md5::digest(&certificate).as_slice() != expected {
        return Err(SmsError::Invalid("invalid certificate file data"));
    }
    Ok(Activated { settings, certificate })
}

/// The digest header: 32 hexadecimal digits (7.3.1).
fn parse_hex16(text: &str) -> Option<[u8; 16]> {
    let text = text.trim();
    if text.len() != 32 || !text.is_ascii() {
        return None;
    }
    let mut out = [0u8; 16];
    for (slot, pair) in out.iter_mut().zip(text.as_bytes().chunks(2)) {
        *slot = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(out)
}

/// The answer of CheckSetting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    /// HTTP 304: the settings stay.
    NotModified,
    /// New settings and the ETag of the answer (empty when the header is missing).
    Updated(Box<InitSettings>, String),
}

/// 7.3.2: GET of the terminals URL.
pub fn check_setting(http: &mut dyn HttpClient, url: &str, etag: &str) -> Result<Check, SmsError> {
    let response = call(http, Method::Get, url, Some(("If-None-Match", etag)), Vec::new(), Some(304))?;
    if response.status >= 300 {
        return Ok(Check::NotModified);
    }
    let root: Value = serde_json::from_slice(&response.body).map_err(|_| SmsError::Invalid("not JSON"))?;
    let settings = parse_init_settings(&root).ok_or(SmsError::Invalid("settings"))?;
    Ok(Check::Updated(Box::new(settings), header(&response, "etag").unwrap_or("").to_owned()))
}

/// 7.3.3: PUT of the status report.
pub fn send_status(http: &mut dyn HttpClient, url: &str, report: &StatusReport) -> Result<(), SmsError> {
    let body = format!(
        "{{\"appVersion\":{},\"kernelVersion\":{},\"moduleVersion\":{},\"rwAdapterVersion\":{},\"scheduledAcquisitionTimeOfSetting\":\"{}+09:00\",\"settingsReflectionTime\":\"{}+09:00\",\"settingsVersion\":{},\"statusCode\":{}}}",
        js(&report.app_version),
        js(&report.kernel_version),
        js(&report.module_version),
        js(&report.rw_adapter_version),
        stamp(report.next_check),
        stamp(report.reflection),
        js(&report.settings_version),
        js(&report.status_code)
    );
    call(http, Method::Put, url, JSON_TYPE, body.into_bytes(), None).map(drop)
}

/// One entry of the sales queue (7.3.4, `service+1336`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SaleRecord {
    /// `method`: the brand code, 101 for cash.
    pub method: u32,
    /// `paymentResult`.
    pub result: u32,
    /// `settleAmount` and `amount`.
    pub amount: u32,
    /// `tcDealNumber`.
    pub deal_number: String,
    /// `cardId`.
    pub card: String,
    /// `productCode`.
    pub product: u32,
    /// `quantity`.
    pub quantity: u32,
    /// `sic.itemId`.
    pub item_id: String,
    /// `soldAt`.
    pub time: [u16; 6],
    /// `sic.keychipId`.
    pub keychip: String,
    /// `sic.gameId`.
    pub game_id: String,
    /// `SE-ID`.
    pub se_id: String,
}

fn sale_json(record: &SaleRecord) -> String {
    let take = |text: &str, n: usize| js(&text.chars().take(n).collect::<String>());
    let mut payment = format!(
        "{{\"method\":{},\"paymentResult\":{},\"settleAmount\":{}",
        record.method, record.result, record.amount
    );
    if record.method != 101 && record.method != 91 {
        payment.push_str(&format!(",\"tcDealNumber\":{}", take(&record.deal_number, 15)));
    }
    if record.method != 101 && !record.card.is_empty() {
        payment.push_str(&format!(",\"cardId\":{}", take(&record.card, 18)));
    }
    payment.push('}');
    let mut sales = format!(
        "{{\"amount\":{},\"productCode\":{},\"quantity\":{},\"taxOthers\":0",
        record.amount, record.product, record.quantity
    );
    if !record.item_id.is_empty() {
        sales.push_str(&format!(",\"sic\":{{\"itemId\":{}}}", js(&record.item_id)));
    }
    sales.push('}');
    let t = record.time;
    let mut out = format!(
        "{{\"paymentDetails\":[{payment}],\"salesDetails\":[{sales}],\"soldAt\":\"{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}+09:00\"",
        t[0], t[1], t[2], t[3], t[4], t[5], record.method
    );
    if !record.keychip.is_empty() || !record.game_id.is_empty() {
        out.push_str(&format!(",\"sic\":{{\"keychipId\":{},\"gameId\":{}}}", js(&record.keychip), js(&record.game_id)));
    }
    if !record.se_id.is_empty() {
        out.push_str(&format!(",\"SE-ID\":{}", take(&record.se_id, 16)));
    }
    out.push_str(",\"ageGroup\":\"0000\",\"gender\":0}");
    out
}

/// 7.3.4: POST of the sales queue.
pub fn send_sales(http: &mut dyn HttpClient, url: &str, serial: &str, records: &[SaleRecord]) -> Result<(), SmsError> {
    let list: Vec<String> = records.iter().map(sale_json).collect();
    let body =
        format!("{{\"sales\":[{}],\"serialNumber\":{},\"businessCategory\":1}}", list.join(","), js(narrow(serial)?));
    call(http, Method::Post, url, JSON_TYPE, body.into_bytes(), None).map(drop)
}

/// 7.3.5: PUT of the counter snapshot `(coin1, coin2, coin3, notification word)`.
pub fn send_counter(http: &mut dyn HttpClient, url: &str, snapshot: [u32; 4]) -> Result<(), SmsError> {
    let body = format!(
        "{{\"coin1\":{},\"coin2\":{},\"coin3\":{},\"notificationType\":{},\"prize\":0}}",
        snapshot[0],
        snapshot[1],
        snapshot[2],
        u32::from(snapshot[3] == 1)
    );
    call(http, Method::Put, url, JSON_TYPE, body.into_bytes(), None).map(drop)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use backend::mock::SimHttp;

    /// A settings object as the site sends it.
    pub(crate) fn settings_json() -> Value {
        json!({
            "endpoints": {
                "terminals": {"uri": "https://sms.example/terminals"},
                "statuses": {"uri": "https://sms.example/statuses"},
                "sales": {"uri": "https://sms.example/sales"},
                "counters": {"uri": "https://sms.example/counters"}
            },
            "intervals": {"checkSetting": 15, "sendStatus": 30},
            "settigsType": "AmusementTerminalSettings",
            "status": "10",
            "terminalId": "123456789012345678901234567890",
            "version": "2024-05-01T09:00:00",
            "availableElectronicMoney": [1, 2, 4, 91],
            "cashAvailability": true,
            "productCode": 7
        })
    }

    #[test]
    fn settings_parse_and_reject() {
        let settings = parse_init_settings(&settings_json()).expect("valid");
        assert_eq!(settings.brands, [1, 2, 0]);
        assert!(settings.brand_91 && settings.cash && settings.settings_type);
        assert_eq!(settings.status_number(), "10");
        let mut bad = settings_json();
        bad["terminalId"] = json!("12345");
        assert!(parse_init_settings(&bad).is_none());
        let mut bad = settings_json();
        bad["intervals"]["checkSetting"] = json!(0);
        assert!(parse_init_settings(&bad).is_none());
        let mut bad = settings_json();
        bad["version"] = json!("1899-01-01T00:00:00");
        assert!(parse_init_settings(&bad).is_none());
    }

    #[test]
    fn activation_checks_the_certificate_digest() {
        let certificate = b"p12 bytes".to_vec();
        let digest = Md5::digest(&certificate).iter().map(|byte| format!("{byte:02x}")).collect::<String>();
        let body = json!({"certificate": STANDARD.encode(&certificate), "initSettings": settings_json()}).to_string();
        let http = SimHttp::new();
        http.push_answer(Ok(HttpResponse {
            status: 200,
            headers: vec![("X-Certificate-MD5".into(), digest)],
            body: body.clone().into_bytes(),
        }));
        let mut client = http.clone();
        let activated =
            activate(&mut client, "https://sms.example/root", "ABC1", "SERIAL00001", "MERCHANT", 12, "CARD")
                .expect("activated");
        assert_eq!(activated.certificate, certificate);
        let request = &http.requests()[0];
        assert_eq!(request.method, Method::Post);
        assert_eq!(
            String::from_utf8_lossy(&request.body),
            r#"{"modelName":"ABC1","serialNumber":"SERIAL00001","merchantCode":"MERCHANT","storeBranchNumber":12,"storeCardId":"CARD"}"#
        );
        http.push_answer(Ok(HttpResponse {
            status: 200,
            headers: vec![("x-certificate-md5".into(), "00".repeat(16))],
            body: body.into_bytes(),
        }));
        assert_eq!(
            activate(&mut client, "https://sms.example/root", "ABC1", "SERIAL00001", "M", 1, "C"),
            Err(SmsError::Invalid("invalid certificate file data"))
        );
    }

    #[test]
    fn requests_have_the_documented_shape() {
        let http = SimHttp::new();
        let mut client = http.clone();
        http.push_answer(Ok(HttpResponse { status: 304, headers: vec![], body: vec![] }));
        assert_eq!(
            check_setting(&mut client, "https://sms.example:8443/terminals/S1", "\"tag\""),
            Ok(Check::NotModified)
        );
        let request = &http.requests()[0];
        assert_eq!(request.method, Method::Get);
        assert_eq!(request.headers[0], ("Host".to_owned(), "sms.example".to_owned()));
        assert!(request.headers.contains(&("If-None-Match".to_owned(), "\"tag\"".to_owned())));
        let report = StatusReport {
            module_version: "1.4.11.0".into(),
            next_check: [2024, 5, 1, 9, 15, 0],
            reflection: [2024, 5, 1, 9, 0, 0],
            settings_version: "v".into(),
            status_code: "10".into(),
            ..StatusReport::default()
        };
        http.push_answer(Ok(HttpResponse { status: 204, headers: vec![], body: vec![] }));
        send_status(&mut client, "https://sms.example/statuses/S1", &report).expect("status");
        assert_eq!(
            String::from_utf8_lossy(&http.requests()[1].body),
            r#"{"appVersion":"","kernelVersion":"","moduleVersion":"1.4.11.0","rwAdapterVersion":"","scheduledAcquisitionTimeOfSetting":"2024-05-01T09:15:00+09:00","settingsReflectionTime":"2024-05-01T09:00:00+09:00","settingsVersion":"v","statusCode":"10"}"#
        );
        let record = SaleRecord {
            method: 101,
            amount: 300,
            product: 7,
            quantity: 3,
            time: [2024, 5, 1, 9, 0, 1],
            ..SaleRecord::default()
        };
        assert_eq!(
            sale_json(&record),
            r#"{"paymentDetails":[{"method":101,"paymentResult":0,"settleAmount":300}],"salesDetails":[{"amount":300,"productCode":7,"quantity":3,"taxOthers":0}],"soldAt":"2024-05-01T09:00:01.101+09:00","ageGroup":"0000","gender":0}"#
        );
        assert_eq!(add_seconds([2024, 12, 31, 23, 59, 30], 60), [2025, 1, 1, 0, 0, 30]);
    }
}
