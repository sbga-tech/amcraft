//! The AimePay server protocol: request bodies, answer parsing, serial-number derivation and URL handling
//! (`payment.md` PayApSrv 3-6).
//!
//! JSON is built and read with `serde_json`; the two places where the original's RapidJSON differs from
//! `serde_json` are reproduced here: members are written in the documented order (no `preserve_order` feature
//! is needed because the body is assembled by hand from ordered pairs) and the FIRST occurrence of a duplicated
//! member wins when an answer is read (`payment.apsrv.json.rapidjson`).

use std::fmt;

use libam_api::time::Tm;
use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::Value;

use super::model::{
    CODE_LEN, COMPANY_NAME_LEN, ITEM_RING, Item, Location, STORE_NAME_LEN, SUMMARY_RING, Summary, units,
};
use crate::util::tm_from_date_time;

/// The answer is read from at most this many bytes (`payment.apsrv.http.response_buffer`).
pub const RESPONSE_LIMIT: usize = 0x2000;
/// The request body must stay below this many characters (`payment.apsrv.json.body_limit`).
pub const BODY_LIMIT: usize = 0x4000;
/// Longest `detail_code` (`wchar[81]`).
const DETAIL_LEN: usize = 80;

/// A parsed JSON value whose objects keep every member in document order.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    /// `null`.
    Null,
    /// `true` / `false`.
    Bool(bool),
    /// A negative integer.
    Int(i64),
    /// A non-negative integer.
    UInt(u64),
    /// Any other number.
    Float(f64),
    /// A string.
    Str(String),
    /// An array.
    Arr(Vec<Json>),
    /// An object.
    Obj(Vec<(String, Json)>),
}

impl Json {
    /// The first member called `key` (RapidJSON's `FindMember`).
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(members) => members.iter().find(|(name, _)| name == key).map(|(_, value)| value),
            _ => None,
        }
    }

    /// RapidJSON "int": an integer that fits `i32`.
    fn as_i32(&self) -> Option<i32> {
        match *self {
            Json::Int(value) => i32::try_from(value).ok(),
            Json::UInt(value) => i32::try_from(value).ok(),
            _ => None,
        }
    }

    /// RapidJSON "uint" that is also at most `0x7FFFFFFF` (every counter the parsers read).
    fn as_u31(&self) -> Option<u32> {
        match *self {
            Json::UInt(value) => u32::try_from(value).ok().filter(|&value| value <= 0x7FFF_FFFF),
            _ => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(text) => Some(text),
            _ => None,
        }
    }

    fn as_bool(&self) -> Option<bool> {
        match *self {
            Json::Bool(value) => Some(value),
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for Json {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct JsonVisitor;

        impl<'de> Visitor<'de> for JsonVisitor {
            type Value = Json;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON value")
            }

            fn visit_bool<E>(self, value: bool) -> Result<Json, E> {
                Ok(Json::Bool(value))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Json, E> {
                Ok(if value < 0 { Json::Int(value) } else { Json::UInt(value as u64) })
            }

            fn visit_u64<E>(self, value: u64) -> Result<Json, E> {
                Ok(Json::UInt(value))
            }

            fn visit_f64<E>(self, value: f64) -> Result<Json, E> {
                Ok(Json::Float(value))
            }

            fn visit_str<E>(self, value: &str) -> Result<Json, E> {
                Ok(Json::Str(value.to_owned()))
            }

            fn visit_unit<E>(self) -> Result<Json, E> {
                Ok(Json::Null)
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Json, A::Error> {
                let mut items = Vec::new();
                while let Some(item) = seq.next_element()? {
                    items.push(item);
                }
                Ok(Json::Arr(items))
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Json, A::Error> {
                let mut members = Vec::new();
                while let Some((key, value)) = map.next_entry::<String, Json>()? {
                    members.push((key, value));
                }
                Ok(Json::Obj(members))
            }
        }

        deserializer.deserialize_any(JsonVisitor)
    }
}

/// The nine server commands with their parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    /// `/api/chkamp`.
    CheckUser {
        /// Aime id.
        aime_id: u32,
    },
    /// `/api/chkpin`.
    CheckIn {
        /// Aime id.
        aime_id: u32,
        /// Four decimal digits.
        pin: String,
    },
    /// `/api/amountprice`.
    LoadDealSummary {
        /// 1..=7.
        days: u32,
    },
    /// `/api/payment/playfee`.
    PayToCoin {
        /// Aime id.
        aime_id: u32,
        /// 13 digits.
        item_id: String,
        /// 1..=0x7FFFFFFF.
        count: u32,
    },
    /// `/api/payment/success`.
    CompletePayToCoin {
        /// Aime id.
        aime_id: u32,
        /// 13 digits.
        item_id: String,
        /// Coin count.
        count: u32,
        /// Receipt of the PayToCoin answer.
        receipt_id: u64,
    },
    /// `/api/payment/item`.
    PayToItem {
        /// Aime id.
        aime_id: u32,
        /// 13 digits.
        item_id: String,
        /// 1..=0x7FFFFFFF.
        count: u32,
        /// Option number.
        option_number: i64,
    },
    /// `/api/serveralive`.
    CheckAlive,
    /// `/api/activation`.
    Activate,
    /// `/api/cabitem/list`.
    LoadItemList,
}

impl Call {
    /// The path appended to `root_endpoint` (`payment.apsrv.cmd.paths`).
    pub fn path(&self) -> &'static str {
        match self {
            Call::CheckUser { .. } => "/api/chkamp",
            Call::CheckIn { .. } => "/api/chkpin",
            Call::LoadDealSummary { .. } => "/api/amountprice",
            Call::PayToCoin { .. } => "/api/payment/playfee",
            Call::CompletePayToCoin { .. } => "/api/payment/success",
            Call::PayToItem { .. } => "/api/payment/item",
            Call::CheckAlive => "/api/serveralive",
            Call::Activate => "/api/activation",
            Call::LoadItemList => "/api/cabitem/list",
        }
    }
}

/// A board or keychip serial number (`XXXX-XXXNNNNNNNN`) and the 11-character id derived from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Serial {
    /// The 16 characters.
    pub id: String,
    /// `board_id` / `keychip_id` of the requests.
    pub derived: String,
}

impl Serial {
    /// `0x140226290`: exactly 16 characters, upper-cased, `XXXX-XXXNNNNNNNN` with letters `I` and `O` excluded
    /// (`payment.apsrv.id.serial_validate`); the derived id is `id[0..4] + id[5..12]`
    /// (`payment.apsrv.id.serial_derive`). The original converts to CP932 first; every accepted character is
    /// ASCII, so the check is done on ASCII directly.
    pub fn parse(text: &str) -> Option<Self> {
        let id = text.to_uppercase();
        let bytes = id.as_bytes();
        if bytes.len() != 16 || !id.is_ascii() {
            return None;
        }
        let tag = |byte: u8| byte.is_ascii_digit() || (byte.is_ascii_uppercase() && byte != b'I' && byte != b'O');
        let valid = bytes[..4].iter().all(|&byte| tag(byte))
            && bytes[4] == b'-'
            && bytes[5..8].iter().all(|&byte| tag(byte))
            && bytes[8..].iter().all(u8::is_ascii_digit);
        valid.then(|| Self { derived: format!("{}{}", &id[..4], &id[5..12]), id })
    }
}

/// The identification placed in requests (`payment.apsrv.id.*`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ids {
    /// Four-letter game id.
    pub game_id: Option<String>,
    /// Board serial.
    pub board: Option<Serial>,
    /// Keychip serial.
    pub keychip: Option<Serial>,
    /// `clientId` of the AllNet authentication, sent as `ip`.
    pub ip: u32,
}

/// An ordered JSON object under construction.
struct Body(Vec<(&'static str, Value)>);

impl Body {
    fn new() -> Self {
        Self(Vec::new())
    }

    fn put(mut self, key: &'static str, value: impl Into<Value>) -> Self {
        self.0.push((key, value.into()));
        self
    }

    /// Compact text in insertion order (RapidJSON `Writer`).
    fn finish(self) -> Option<String> {
        let mut out = String::from("{");
        for (index, (key, value)) in self.0.into_iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push_str(&serde_json::to_string(key).ok()?);
            out.push(':');
            out.push_str(&serde_json::to_string(&value).ok()?);
        }
        out.push('}');
        (out.chars().count() < BODY_LIMIT).then_some(out)
    }
}

/// The request body of `call` (`payment.apsrv.ep.*.request`); `None` when it cannot be built.
pub fn build_body(call: &Call, ids: &Ids, location: &Location) -> Option<String> {
    let board = ids.board.as_ref().map(|serial| serial.derived.clone()).unwrap_or_default();
    let keychip = ids.keychip.as_ref().map(|serial| serial.derived.clone()).unwrap_or_default();
    let game = ids.game_id.clone().unwrap_or_default();
    match call {
        Call::CheckUser { aime_id } => Body::new()
            .put("aime_id", *aime_id)
            .put("store_code", location.store_code.clone())
            .put("company_code", location.company_code.clone())
            .put("board_id", board)
            .put("keychip_id", keychip)
            .put("game_id", game),
        Call::CheckIn { aime_id, pin } => Body::new()
            .put("aime_id", *aime_id)
            .put("pin_code", pin.clone())
            .put("store_code", location.store_code.clone())
            .put("company_code", location.company_code.clone()),
        Call::LoadDealSummary { days } => Body::new()
            .put("store_code", location.store_code.clone())
            .put("board_id", board)
            .put("company_code", location.company_code.clone())
            .put("get_count", *days),
        Call::PayToCoin { aime_id, item_id, count } => pay_body(*aime_id, location, ids, &board, &keychip, &game)
            .put("item_id", item_id.clone())
            .put("count", *count),
        Call::PayToItem { aime_id, item_id, count, option_number } => {
            pay_body(*aime_id, location, ids, &board, &keychip, &game)
                .put("item_id", item_id.clone())
                .put("count", *count)
                .put("option_number", *option_number)
        }
        Call::CompletePayToCoin { aime_id, item_id, count, receipt_id } => Body::new()
            .put("receipt_id", *receipt_id)
            .put("aime_id", *aime_id)
            .put("store_code", location.store_code.clone())
            .put("board_id", board)
            .put("keychip_id", keychip)
            .put("game_id", game)
            .put("item_id", item_id.clone())
            .put("count", *count)
            .put("company_code", location.company_code.clone()),
        Call::CheckAlive => Body::new(),
        Call::Activate => {
            Body::new().put("ip", ids.ip).put("board_id", board).put("keychip_id", keychip).put("game_id", game)
        }
        Call::LoadItemList => Body::new().put("item_type", 0).put("game_id_list", vec![Value::from(game)]),
    }
    .finish()
}

fn pay_body(aime_id: u32, location: &Location, ids: &Ids, board: &str, keychip: &str, game: &str) -> Body {
    Body::new()
        .put("aime_id", aime_id)
        .put("store_code", location.store_code.clone())
        .put("company_code", location.company_code.clone())
        .put("ip", ids.ip)
        .put("board_id", board.to_owned())
        .put("keychip_id", keychip.to_owned())
        .put("game_id", game.to_owned())
}

/// The members a valid answer carries beyond the header (`payment.apsrv.ep.*.response`).
#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    /// Header only.
    Empty,
    /// CheckUser.
    CheckUser {
        /// `credit_flg`.
        credit: bool,
        /// `checkin_flg`.
        checked_in: bool,
        /// `touchskip_flg`.
        touch_skip: bool,
    },
    /// LoadDealSummary.
    Summary(Vec<Summary>),
    /// PayToCoin / PayToItem.
    Pay {
        /// `receipt_id`.
        receipt_id: u64,
        /// `amount`.
        amount: u32,
    },
    /// Activate.
    Location(Location),
    /// LoadItemList.
    Items {
        /// `tax_per`.
        tax_per: u32,
        /// The stored items.
        items: Vec<Item>,
    },
}

/// What reading an answer produced.
#[derive(Debug, Clone, PartialEq)]
pub struct Parsed {
    /// `result_code` and `detail_code` when the header was read (stored even if a later member fails).
    pub header: Option<(u32, String)>,
    /// The members; `Err` carries what a failing Activate answer stored before it failed.
    pub outcome: Result<Answer, Option<Location>>,
}

impl Parsed {
    fn failed(header: Option<(u32, String)>) -> Self {
        Self { header, outcome: Err(None) }
    }
}

/// Reads the answer of `call` (`0x1402C2AD0` header, then the endpoint members).
pub fn parse_answer(call: &Call, body: &[u8]) -> Parsed {
    let Ok(root) = serde_json::from_slice::<Json>(body) else { return Parsed::failed(None) };
    if !matches!(root, Json::Obj(_)) {
        return Parsed::failed(None);
    }
    let code = match root.get("result_code") {
        None => 0,
        Some(value) => match value.as_i32() {
            Some(code) if code >= 0 => code as u32,
            _ => return Parsed::failed(None),
        },
    };
    let mut detail = String::new();
    if code != 0 {
        match root.get("detail_code").and_then(Json::as_str) {
            Some(text) if !text.is_empty() && units(text) <= DETAIL_LEN => detail = text.to_owned(),
            _ => return Parsed::failed(None),
        }
    }
    let header = Some((code, detail));
    if code != 0 {
        return Parsed { header, outcome: Ok(Answer::Empty) };
    }
    match members(call, &root) {
        Ok(answer) => Parsed { header, outcome: Ok(answer) },
        Err(partial) => Parsed { header, outcome: Err(partial) },
    }
}

fn members(call: &Call, root: &Json) -> Result<Answer, Option<Location>> {
    let fail = || Err(None);
    match call {
        Call::CheckUser { .. } => {
            let (Some(credit), Some(checked_in)) =
                (root.get("credit_flg").and_then(Json::as_bool), root.get("checkin_flg").and_then(Json::as_bool))
            else {
                return fail();
            };
            let touch_skip = match root.get("touchskip_flg") {
                None => false,
                Some(value) => match value.as_bool() {
                    Some(flag) => flag,
                    None => return fail(),
                },
            };
            Ok(Answer::CheckUser { credit, checked_in, touch_skip })
        }
        Call::LoadDealSummary { .. } => summary(root).map(Answer::Summary).ok_or(None),
        Call::PayToCoin { .. } | Call::PayToItem { .. } => {
            let receipt = match root.get("receipt_id") {
                Some(Json::UInt(value)) if (1..=0x7FFF_FFFF_FFFF_FFFF).contains(value) => *value,
                _ => return fail(),
            };
            let Some(amount) = root.get("amount").and_then(Json::as_u31) else { return fail() };
            Ok(Answer::Pay { receipt_id: receipt, amount })
        }
        Call::Activate => activation(root),
        Call::LoadItemList => items(root).ok_or(None),
        Call::CheckIn { .. } | Call::CompletePayToCoin { .. } | Call::CheckAlive => Ok(Answer::Empty),
    }
}

fn summary(root: &Json) -> Option<Vec<Summary>> {
    let Json::Arr(history) = root.get("history")? else { return None };
    let mut out = Vec::new();
    // Elements after the seventh are ignored without validation.
    for element in history.iter().take(SUMMARY_RING) {
        let date = parse_date(element.get("target_date")?.as_str()?, 3)?;
        let amount_price = element.get("amount_price")?.as_u31()?;
        let settlement_price = element.get("settlement_price")?.as_u31()?;
        let amount_count = element.get("amount_count")?.as_u31()?;
        let settlement_count = element.get("settlement_count")?.as_u31()?;
        out.push(Summary { date, amount_count, amount_price, settlement_count, settlement_price });
    }
    Some(out)
}

/// One Activate member: a non-empty string that fits the limit; longer names are truncated.
fn location_member(root: &Json, key: &str, limit: usize, truncate: bool) -> Option<String> {
    let text = root.get(key)?.as_str()?;
    if text.is_empty() {
        return None;
    }
    if units(text) <= limit {
        return Some(text.to_owned());
    }
    truncate.then(|| text.chars().take(limit).collect())
}

fn activation(root: &Json) -> Result<Answer, Option<Location>> {
    let mut location = Location::default();
    let Some(store_code) = location_member(root, "store_code", CODE_LEN, false) else { return Err(Some(location)) };
    location.store_code = store_code;
    let Some(store_name) = location_member(root, "store_name", STORE_NAME_LEN, true) else {
        return Err(Some(location));
    };
    location.store_name = store_name;
    let Some(company_code) = location_member(root, "company_code", CODE_LEN, false) else {
        return Err(Some(location));
    };
    location.company_code = company_code;
    let Some(company_name) = location_member(root, "company_name", COMPANY_NAME_LEN, true) else {
        return Err(Some(location));
    };
    location.company_name = company_name;
    Ok(Answer::Location(location))
}

fn items(root: &Json) -> Option<Answer> {
    let tax_per = root.get("tax_per")?.as_u31()?;
    let Json::Arr(list) = root.get("item_list")? else { return None };
    if list.len() > ITEM_RING {
        return None;
    }
    let mut items = Vec::new();
    for element in list {
        let item_id = element.get("item_id")?.as_str()?;
        if item_id.len() != 13 || !item_id.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        if element.get("item_name")?.as_str()?.is_empty() {
            return None;
        }
        let item_type = element.get("item_type")?.as_u31()?;
        let price = element.get("price")?.as_u31()?;
        element.get("price_extax")?.as_u31()?;
        parse_date(element.get("open_date")?.as_str()?, 6)?;
        parse_date(element.get("close_date")?.as_str()?, 6)?;
        // Elements with `item_type == 0` are validated but not stored.
        if item_type != 0 {
            items.push(Item { item_id: item_id.to_owned(), price });
        }
    }
    Some(Answer::Items { tax_per, items })
}

/// `sscanf("%04u-%02u-%02u %02u:%02u:%02u")` with exactly `fields` (3 or 6) converted fields, validated by a
/// round trip through the calendar (`payment.apsrv.json.date_parse`, year 1970..3000).
pub fn parse_date(text: &str, fields: usize) -> Option<Tm> {
    let bytes = text.as_bytes();
    let mut position = 0;
    let mut values = [0u32; 6];
    let mut converted = 0;
    let layout: [(usize, Option<u8>); 6] =
        [(4, Some(b'-')), (2, Some(b'-')), (2, Some(b' ')), (2, Some(b':')), (2, Some(b':')), (2, None)];
    for (index, (width, separator)) in layout.into_iter().enumerate() {
        while bytes.get(position).is_some_and(u8::is_ascii_whitespace) {
            position += 1;
        }
        let start = position;
        while position < bytes.len() && position - start < width && bytes[position].is_ascii_digit() {
            position += 1;
        }
        if position == start {
            break;
        }
        values[index] = text[start..position].parse().ok()?;
        converted += 1;
        match separator {
            Some(byte) if bytes.get(position) == Some(&byte) => position += 1,
            _ => break,
        }
    }
    if converted != fields {
        return None;
    }
    let [year, month, day, hour, minute, second] = values.map(|value| value as i32);
    if !(1970..=3000).contains(&year) {
        return None;
    }
    let time = libam_api::time::DateTime::make(year, month, day, hour, minute, second, 0)?;
    let tm = tm_from_date_time(time);
    let round_trip =
        (tm.year + 1900, tm.mon + 1, tm.mday, tm.hour, tm.min, tm.sec) == (year, month, day, hour, minute, second);
    round_trip.then_some(tm)
}

/// A request URL split the way `WinHttpCrackUrl` with fixed buffers does (`payment.apsrv.http.send`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrackedUrl {
    /// `https` scheme.
    pub secure: bool,
    /// Host name (at most 127 characters); the `Host` header carries only this.
    pub host: String,
}

/// Splits `url`; `None` when the cracker fails (unknown scheme, empty or over-long host, over-long path).
pub fn crack_url(url: &str) -> Option<CrackedUrl> {
    let (scheme, rest) = url.split_once("://")?;
    let secure = match scheme.to_ascii_lowercase().as_str() {
        "http" => false,
        "https" => true,
        _ => return None,
    };
    let (authority, path) = match rest.find('/') {
        Some(index) => rest.split_at(index),
        None => (rest, "/"),
    };
    let host = match authority.rsplit_once(':') {
        Some((host, port)) if port.bytes().all(|byte| byte.is_ascii_digit()) => host,
        _ => authority,
    };
    (!host.is_empty() && units(host) <= 127 && units(path) <= 127).then(|| CrackedUrl { secure, host: host.to_owned() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn location() -> Location {
        Location {
            store_code: "1234567890".into(),
            store_name: "Store".into(),
            company_code: "0987654321".into(),
            company_name: "Company".into(),
        }
    }

    fn ids() -> Ids {
        Ids {
            game_id: Some("SDEZ".into()),
            board: Serial::parse("A69E-01A12345678"),
            keychip: Serial::parse("A69E-01A56789012"),
            ip: 3_232_235_778,
        }
    }

    #[test]
    fn serial_derivation_drops_the_dash_and_the_last_four_digits() {
        let serial = Serial::parse("a69e-01a12345678").unwrap();
        assert_eq!(serial.derived, "A69E01A1234");
        assert!(Serial::parse("A69E-01I12345678").is_none());
        assert!(Serial::parse("A69E001A12345678").is_none());
    }

    #[test]
    fn pay_to_coin_body_has_the_documented_member_order() {
        let call = Call::PayToCoin { aime_id: 12_345_678, item_id: "1234567890123".into(), count: 1 };
        let body = build_body(&call, &ids(), &location()).unwrap();
        assert_eq!(
            body,
            r#"{"aime_id":12345678,"store_code":"1234567890","company_code":"0987654321","ip":3232235778,"board_id":"A69E01A1234","keychip_id":"A69E01A5678","game_id":"SDEZ","item_id":"1234567890123","count":1}"#
        );
    }

    #[test]
    fn error_answer_stops_at_the_header_and_first_duplicate_member_wins() {
        let call = Call::PayToCoin { aime_id: 1, item_id: "1234567890123".into(), count: 1 };
        let parsed = parse_answer(&call, br#"{"result_code":20,"result_code":0,"detail_code":"AMPEWL2050"}"#);
        assert_eq!(parsed.header, Some((20, "AMPEWL2050".to_owned())));
        assert_eq!(parsed.outcome, Ok(Answer::Empty));
        // `result_code != 0` without a detail code is a parse failure.
        assert!(parse_answer(&call, br#"{"result_code":20}"#).outcome.is_err());
        // receipt ids start at 1.
        assert!(parse_answer(&call, br#"{"result_code":0,"receipt_id":0,"amount":1}"#).outcome.is_err());
        assert_eq!(
            parse_answer(&call, br#"{"receipt_id":7,"amount":100}"#).outcome,
            Ok(Answer::Pay { receipt_id: 7, amount: 100 })
        );
    }

    #[test]
    fn activation_keeps_the_members_stored_before_a_failure() {
        let parsed = parse_answer(&Call::Activate, br#"{"store_code":"1","store_name":"S","company_code":12}"#);
        let Err(Some(partial)) = parsed.outcome else { panic!("expected a partial location") };
        assert_eq!(
            (partial.store_code.as_str(), partial.store_name.as_str(), partial.company_code.as_str()),
            ("1", "S", "")
        );
    }

    #[test]
    fn dates_need_three_or_six_fields_and_a_valid_calendar_day() {
        assert!(parse_date("2024-05-20", 3).is_some());
        assert!(parse_date("2024-05-20", 6).is_none());
        assert!(parse_date("2024-02-30", 3).is_none());
        let tm = parse_date("2099-12-31 23:59:59", 6).unwrap();
        assert_eq!((tm.year, tm.mon, tm.mday, tm.sec), (199, 11, 31, 59));
    }

    #[test]
    fn url_cracking_limits_host_and_path() {
        assert_eq!(crack_url("http://127.0.0.1:8080/api").unwrap().host, "127.0.0.1");
        assert!(crack_url("ftp://host/").is_none());
        assert!(crack_url(&format!("http://h/{}", "p".repeat(130))).is_none());
    }
}
