//! The `amsBeacon` protocol of LAN install (`spec/libam/network.md` L.9): the `amyItemList` text codec, the
//! beacon datagram (build and `CheckBeacon` validation) and the per-role beacon context.

use std::net::Ipv4Addr;
use std::str::FromStr;

use backend::lan::MAX_DATAGRAM;

/// Largest server list a beacon carries.
pub const MAX_SERVER_IPS: usize = 22;
/// Addresses per `iplist` line.
const IPS_PER_LINE: usize = 15;
/// Largest record before NUL (`254` bytes).
const MAX_RECORD_BYTES: usize = 254;
/// Records the receiver decodes.
const MAX_RECORDS: usize = 4;
/// Items one record holds.
const MAX_ITEMS: usize = 32;
/// Record capacity in wide characters (`key + value + terminator`).
const RECORD_CAPACITY: usize = 255;
/// Beacon version the senders use.
const BEACON_VERSION: u8 = 2;

/// Whether `c` may appear in a key or value (`amyItemList` add rule).
fn item_char_ok(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@')
}

/// `amyItemList` encode: `key=value&key=value\r\n`. `None` where `Add` rejects an item (duplicate key, more than
/// 32 items, capacity, forbidden characters). A value equal to `?` is exempt from the character rule.
pub fn encode_record(items: &[(&str, &str)]) -> Option<String> {
    if items.len() > MAX_ITEMS {
        return None;
    }
    let mut out = String::new();
    let mut used = 2usize;
    for (index, (key, value)) in items.iter().enumerate() {
        if items[..index].iter().any(|(other, _)| other == key) {
            return None;
        }
        used += key.len() + value.len();
        if used > RECORD_CAPACITY
            || !key.chars().all(item_char_ok)
            || (*value != "?" && !value.chars().all(item_char_ok))
        {
            return None;
        }
        if index != 0 {
            out.push('&');
        }
        out.push_str(key);
        out.push('=');
        out.push_str(value);
    }
    out.push_str("\r\n");
    Some(out)
}

/// One decoded record: the key/value pairs in order.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Record(Vec<(String, String)>);

impl Record {
    /// Value of `key` (`0x1402FC1D0`).
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.iter().find(|(name, _)| name == key).map(|(_, value)| value.as_str())
    }
}

/// `amyItemList` decode of one NUL-delimited line: the first terminator among `\r\n`, `\r`, `\n`, `#` ends the
/// record; tokens split at `&`, each at its first `=`; a token without `=` that is not `?` is a parse error.
fn decode_record(line: &[u8]) -> Option<Record> {
    let end = line.iter().position(|b| matches!(b, b'\r' | b'\n' | b'#')).unwrap_or(line.len());
    let text = std::str::from_utf8(&line[..end]).ok()?;
    let mut items = Vec::new();
    for token in text.split('&') {
        if token == "?" {
            continue;
        }
        let (key, value) = token.split_once('=')?;
        items.push((key.to_owned(), value.to_owned()));
    }
    Some(Record(items))
}

/// Splits a datagram into at most four NUL-terminated records. `None` when a record exceeds 254 bytes or does not
/// parse (the receiver then marks the data incompatible).
pub fn decode_datagram(data: &[u8]) -> Option<Vec<Record>> {
    let mut records = Vec::new();
    let mut rest = data;
    while !rest.is_empty() && records.len() < MAX_RECORDS {
        let end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
        if end > MAX_RECORD_BYTES {
            return None;
        }
        records.push(decode_record(&rest[..end])?);
        rest = rest.get(end + 1..).unwrap_or(&[]);
    }
    Some(records)
}

/// Dotted quad as `inet_addr` accepts it for the beacon: at most 15 characters, and not the broadcast address.
fn parse_beacon_ip(text: &str) -> Option<Ipv4Addr> {
    if text.len() > 15 {
        return None;
    }
    Ipv4Addr::from_str(text).ok().filter(|ip| *ip != Ipv4Addr::BROADCAST)
}

/// The beacon context of one role (`ctx` of `amsBeacon`, L.9.1): identity, local foreground CRC, server list and
/// the flags of the last datagram.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BeaconContext {
    /// `id`: the game id (4 characters).
    pub id: String,
    /// `hw`: the platform id (compared on 3 characters).
    pub hw: String,
    /// Local foreground CRC.
    pub crc: u32,
    /// Server: own address. Client: the address learnt from the last beacon.
    pub sender_ip: Option<Ipv4Addr>,
    /// Client: own address. Server: unset.
    pub own_ip: Option<Ipv4Addr>,
    /// Server list (installation sources), at most 22.
    pub ips: Vec<Ipv4Addr>,
    /// Server state of the last beacon (0..=5).
    pub server_state: u8,
    /// Sticky: an incompatible datagram was received.
    pub incompatible: bool,
    /// The last datagram was valid.
    pub valid: bool,
    /// Server CRC differs from the local one.
    pub need_sync: bool,
}

impl BeaconContext {
    /// A fresh context (`amsBeaconInit` after the registry values are known).
    pub fn new(id: String, hw: String, crc: u32) -> Self {
        Self {
            id,
            hw,
            crc,
            sender_ip: None,
            own_ip: None,
            ips: Vec::new(),
            server_state: 0,
            incompatible: false,
            valid: false,
            need_sync: false,
        }
    }

    /// `AddServerIpAddress` (`0x140303D30`): rejects 0.0.0.0 and duplicates, at most 22 entries.
    pub fn add_server_ip(&mut self, ip: Ipv4Addr) -> bool {
        if ip.is_unspecified() || self.ips.contains(&ip) || self.ips.len() >= MAX_SERVER_IPS {
            return false;
        }
        self.ips.push(ip);
        true
    }

    /// The datagram a server sends: header, foreground CRC and the `iplist` lines (`0x140304480`). Each record is
    /// followed by its NUL. `None` when the identity does not fit the item grammar.
    pub fn build_beacon(&self, server_ip: Ipv4Addr, state: u8) -> Option<Vec<u8>> {
        let ip = server_ip.to_string();
        let state = state.to_string();
        let version = BEACON_VERSION.to_string();
        let mut out = Vec::new();
        let mut push = |record: String| {
            out.extend_from_slice(record.as_bytes());
            out.push(0);
        };
        push(encode_record(&[
            ("beacon", "2"),
            ("version", &version),
            ("ip", &ip),
            ("hw", &self.hw),
            ("id", &self.id),
            ("state", &state),
        ])?);
        push(encode_record(&[("foregroundcrc", &self.crc.to_string())])?);
        for line in self.ips.chunks(IPS_PER_LINE) {
            let list: Vec<String> = line.iter().map(Ipv4Addr::to_string).collect();
            push(encode_record(&[("iplist", &list.join("_"))])?);
        }
        (out.len() <= MAX_DATAGRAM).then_some(out)
    }

    /// `CheckBeacon` (`0x140303E20`): validates a datagram and, on success, adopts its sender, list and state.
    /// Every failure sets the sticky `incompatible` flag and leaves `valid` clear.
    pub fn check_beacon(&mut self, data: &[u8]) {
        self.valid = false;
        if self.try_check(data).is_none() {
            self.incompatible = true;
        }
    }

    fn try_check(&mut self, data: &[u8]) -> Option<()> {
        let records = decode_datagram(data)?;
        if records.len() < 3 {
            return None;
        }
        let head = &records[0];
        if head.get("beacon")? != "2" {
            return None;
        }
        let version: u8 = head.get("version")?.parse().ok()?;
        let ip = parse_beacon_ip(head.get("ip")?)?;
        let hw = head.get("hw")?;
        let id = head.get("id")?;
        if hw.is_empty() || hw.len() > 4 || id.is_empty() || id.len() > 4 {
            return None;
        }
        let state = match (version >= 2, head.get("state")) {
            (true, Some(text)) => Some(text.parse::<u8>().ok().filter(|state| *state <= 5)?),
            (false, None) => None,
            _ => return None,
        };
        if id != self.id || hw.chars().take(3).ne(self.hw.chars().take(3)) {
            return None;
        }
        let crc: u32 = records[1].get("foregroundcrc")?.parse().ok()?;
        let mut ips = Vec::new();
        for record in &records[2..] {
            for part in record.get("iplist")?.split('_') {
                if let Some(ip) = parse_beacon_ip(part)
                    && ips.len() < MAX_SERVER_IPS
                    && !ips.contains(&ip)
                {
                    ips.push(ip);
                }
            }
        }
        if ips.is_empty() {
            return None;
        }
        self.sender_ip = Some(ip);
        self.ips = ips;
        self.server_state = state.unwrap_or(0);
        self.valid = true;
        self.need_sync = crc != self.crc;
        Some(())
    }

    /// `NeedToSendSync` (`0x140304DB0`): a valid beacon of a server with our image whose list lacks us.
    pub fn need_send_sync(&self) -> bool {
        self.valid
            && !self.need_sync
            && self.server_state == 0
            && self.own_ip.is_some_and(|own| !self.ips.contains(&own))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> BeaconContext {
        let mut ctx = BeaconContext::new("SDEZ".into(), "AAV".into(), 0x1234_5678);
        ctx.add_server_ip(Ipv4Addr::new(192, 168, 10, 5));
        ctx.add_server_ip(Ipv4Addr::new(192, 168, 10, 6));
        ctx
    }

    #[test]
    fn build_matches_the_documented_example() {
        let bytes = server().build_beacon(Ipv4Addr::new(192, 168, 10, 5), 0).unwrap();
        assert_eq!(
            bytes,
            b"beacon=2&version=2&ip=192.168.10.5&hw=AAV&id=SDEZ&state=0\r\n\0foregroundcrc=305419896\r\n\0\
              iplist=192.168.10.5_192.168.10.6\r\n\0"
        );
    }

    #[test]
    fn long_server_lists_split_into_lines_of_fifteen() {
        let mut ctx = BeaconContext::new("SDEZ".into(), "AAV".into(), 1);
        for last in 1..=22 {
            assert!(ctx.add_server_ip(Ipv4Addr::new(10, 0, 0, last)));
        }
        assert!(!ctx.add_server_ip(Ipv4Addr::new(10, 0, 0, 23)));
        let bytes = ctx.build_beacon(Ipv4Addr::new(10, 0, 0, 1), 0).unwrap();
        assert_eq!(decode_datagram(&bytes).unwrap().len(), 4);
    }

    #[test]
    fn client_accepts_a_server_beacon_and_flags_a_different_crc() {
        let bytes = server().build_beacon(Ipv4Addr::new(192, 168, 10, 5), 0).unwrap();
        let mut client = BeaconContext::new("SDEZ".into(), "AAV".into(), 7);
        client.own_ip = Some(Ipv4Addr::new(192, 168, 10, 9));
        client.check_beacon(&bytes);
        assert!(client.valid && client.need_sync && !client.incompatible);
        assert_eq!(client.sender_ip, Some(Ipv4Addr::new(192, 168, 10, 5)));
        assert_eq!(client.ips.len(), 2);
        assert!(!client.need_send_sync());
    }

    #[test]
    fn matching_crc_asks_for_a_sync_until_listed() {
        let bytes = server().build_beacon(Ipv4Addr::new(192, 168, 10, 5), 0).unwrap();
        let mut client = BeaconContext::new("SDEZ".into(), "AAV".into(), 0x1234_5678);
        client.own_ip = Some(Ipv4Addr::new(192, 168, 10, 9));
        client.check_beacon(&bytes);
        assert!(client.need_send_sync());
        client.own_ip = Some(Ipv4Addr::new(192, 168, 10, 6));
        assert!(!client.need_send_sync());
    }

    #[test]
    fn foreign_game_or_broken_data_is_incompatible_and_sticky() {
        let bytes = server().build_beacon(Ipv4Addr::new(192, 168, 10, 5), 0).unwrap();
        let mut client = BeaconContext::new("SDEY".into(), "AAV".into(), 0);
        client.check_beacon(&bytes);
        assert!(client.incompatible && !client.valid);
        let mut same = BeaconContext::new("SDEZ".into(), "AAV".into(), 0);
        same.incompatible = true;
        same.check_beacon(&bytes);
        assert!(same.valid && same.incompatible);
        same.check_beacon(b"beacon=2&version=2&ip=255.255.255.255&hw=AAV&id=SDEZ&state=0\r\n\0foregroundcrc=1\r\n\0iplist=1.2.3.4\r\n\0");
        assert!(!same.valid);
    }

    #[test]
    fn platform_id_is_compared_on_three_characters() {
        let bytes = server().build_beacon(Ipv4Addr::new(192, 168, 10, 5), 0).unwrap();
        let mut client = BeaconContext::new("SDEZ".into(), "AAVX".into(), 0);
        client.check_beacon(&bytes);
        assert!(client.valid);
    }

    #[test]
    fn item_list_rejects_bad_items() {
        assert!(encode_record(&[("a", "1"), ("a", "2")]).is_none());
        assert!(encode_record(&[("a", "b c")]).is_none());
        assert_eq!(encode_record(&[("a", "?")]).as_deref(), Some("a=?\r\n"));
    }
}
