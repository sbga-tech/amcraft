//! The 60-byte trace-log records and their text lines (`allnet.md` part 9 "Trace-log record serialisation", part 10
//! section 4).

use std::fmt::Write as _;

/// Bytes in one trace-log record.
pub const RECORD_LEN: usize = 60;
/// Records the keychip trace log holds.
pub const RECORD_CAPACITY: usize = 7154;
/// `alib` library version word (`1.314`).
pub const ALIB_VERSION: u16 = 1314;

/// One trace-log record.
pub type Record = [u8; RECORD_LEN];

/// Local date and time as the records store it (year relative to 2000).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stamp {
    /// Year, 2000..=2099.
    pub year: u16,
    /// Month, 1..=12.
    pub month: u8,
    /// Day of the month.
    pub day: u8,
    /// Hour.
    pub hour: u8,
    /// Minute.
    pub minute: u8,
    /// Second.
    pub second: u8,
}

impl Stamp {
    /// The 14-digit text `YYYYMMDDhhmmss`.
    pub fn digits(self) -> String {
        format!(
            "20{:02}{:02}{:02}{:02}{:02}{:02}",
            self.year.saturating_sub(2000),
            self.month,
            self.day,
            self.hour,
            self.minute,
            self.second
        )
    }

    fn write(self, record: &mut Record) {
        record[1] = u8::try_from(self.year.saturating_sub(2000)).unwrap_or(0);
        record[2] = self.month;
        record[3] = self.day;
        record[4] = self.hour;
        record[5] = self.minute;
        record[6] = self.second;
    }
}

/// Packs an 11-character id (`kn`, `bn`) into 8 bytes (`0x1403E8AD0`): 4 ASCII, two BCD digits, 1 ASCII, two BCD
/// digits, two BCD digits. Missing characters pack as zero.
pub fn pack_id(id: &str) -> [u8; 8] {
    let bytes = id.as_bytes();
    let at = |index: usize| bytes.get(index).copied().unwrap_or(b'0');
    let bcd = |high: usize, low: usize| (at(high).wrapping_sub(b'0') << 4) | (at(low).wrapping_sub(b'0') & 0x0F);
    [at(0), at(1), at(2), at(3), bcd(4, 5), at(6), bcd(7, 8), bcd(9, 10)]
}

/// The 11-character id of [`pack_id`] bytes.
pub fn unpack_id(packed: &[u8]) -> String {
    let digit = |nibble: u8| char::from(b'0' + (nibble & 0x0F));
    let mut text = String::with_capacity(11);
    for (index, &byte) in packed.iter().take(8).enumerate() {
        match index {
            4 | 6 | 7 => {
                text.push(digit(byte >> 4));
                text.push(digit(byte));
            }
            _ => text.push(char::from(byte)),
        }
    }
    text
}

/// `alib` text: `1.314`.
pub fn alib_text(version: u16) -> String {
    format!("{}.{:03}", (u32::from(version) % 10000) / 1000, u32::from(version) % 1000)
}

/// The keychip, game and board identity every record carries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Identity {
    /// 11-character keychip id (`ctx+865560`).
    pub keychip: String,
    /// 11-character board id (`ctx+865572`).
    pub board: String,
    /// 4-character game id (`ctx+865584`).
    pub game_id: String,
    /// 5-character game version (`ctx+865594`).
    pub game_version: String,
    /// Function type byte (`ctx+865612`).
    pub function_type: u8,
}

fn header(kind: u8, synced: bool, stamp: Stamp) -> Record {
    let mut record = [0; RECORD_LEN];
    record[0] = kind | if synced { 0x80 } else { 0 };
    stamp.write(&mut record);
    record
}

/// A type-0 record: a play, item, quantity or general-id event (`0x1403EA5C0`).
#[allow(clippy::too_many_arguments)]
pub fn play_record(
    identity: &Identity,
    stamp: Stamp,
    synced: bool,
    play_count: u32,
    play_limit: u32,
    tenpo: [u8; 4],
    code: u16,
    count: u8,
    player: u8,
) -> Record {
    let mut record = header(0, synced, stamp);
    record[7..15].copy_from_slice(&pack_id(&identity.keychip));
    record[15..17].copy_from_slice(&ALIB_VERSION.to_le_bytes());
    record[17..21].copy_from_slice(&play_count.to_le_bytes());
    record[21..25].copy_from_slice(&play_limit.to_le_bytes());
    record[25..29].copy_from_slice(&tenpo);
    record[29..31].copy_from_slice(&code.to_le_bytes());
    record[31] = count;
    copy_text(&mut record[32..36], &identity.game_id);
    copy_text(&mut record[36..41], &identity.game_version);
    record[41..49].copy_from_slice(&pack_id(&identity.board));
    record[49] = identity.function_type % 100;
    record[50] = player;
    record
}

/// A type-1 record carrying a report message of at most 32 characters (`0x1403EACA0`).
pub fn message_record(identity: &Identity, stamp: Stamp, synced: bool, message: &str) -> Record {
    let mut record = header(1, synced, stamp);
    copy_text(&mut record[7..39], message);
    record[40..48].copy_from_slice(&pack_id(&identity.keychip));
    record[48..50].copy_from_slice(&ALIB_VERSION.to_le_bytes());
    record
}

/// A type-2 record carrying the 15-byte credit configuration.
pub fn credit_config_record(identity: &Identity, stamp: Stamp, synced: bool, config: &[u8; 15]) -> Record {
    let mut record = header(2, synced, stamp);
    record[7..22].copy_from_slice(config);
    record[22..24].copy_from_slice(&ALIB_VERSION.to_le_bytes());
    record[24..32].copy_from_slice(&pack_id(&identity.keychip));
    record
}

fn copy_text(target: &mut [u8], text: &str) {
    for (slot, byte) in target.iter_mut().zip(text.bytes()) {
        *slot = byte;
    }
}

fn read_text(bytes: &[u8]) -> String {
    bytes.iter().take_while(|&&byte| byte != 0).map(|&byte| char::from(byte)).collect()
}

fn le16(record: &Record, at: usize) -> u16 {
    u16::from_le_bytes([record[at], record[at + 1]])
}

fn le32(record: &Record, at: usize) -> u32 {
    u32::from_le_bytes([record[at], record[at + 1], record[at + 2], record[at + 3]])
}

/// The text line of `record` (`ToParam`, `0x1403EC230`), CR LF terminated, with the report serial `sn`; `None` for a
/// type the serialiser skips.
pub fn render(record: &Record, serial: u32) -> Option<String> {
    let kind = record[0];
    if kind == 0xFF {
        return Some(format!("rn={}&sn={serial}&cs=1\r\n", le32(record, 56)));
    }
    let tt = kind & 0x7F;
    let synced = u8::from(kind & 0x80 != 0);
    let dt =
        format!("20{:02}{:02}{:02}{:02}{:02}{:02}", record[1], record[2], record[3], record[4], record[5], record[6]);
    let rn = le32(record, 56);
    let mut line = String::new();
    match tt {
        0 => {
            let tenpo = le32(record, 25);
            let ti = if tenpo == u32::MAX {
                String::new()
            } else {
                format!("{}.{}.{}.{}", record[25], record[26], record[27], record[28])
            };
            let _ = write!(
                line,
                "tt=0&rn={rn}&sn={serial}&dt={dt}&ds={synced}&kn={}&alib={}&cs=0&gi={}&gv={}&bn={}&ti={ti}&pc={}&pl={}&ic={}&in={}&kk={}&playerno={}",
                unpack_id(&record[7..15]),
                alib_text(le16(record, 15)),
                read_text(&record[32..36]),
                read_text(&record[36..41]),
                unpack_id(&record[41..49]),
                le32(record, 17),
                le32(record, 21),
                le16(record, 29),
                record[31],
                record[49],
                record[50],
            );
        }
        1 => {
            let message: String = read_text(&record[7..39]).chars().take(32).collect();
            let _ = write!(
                line,
                "tt=1&rn={rn}&sn={serial}&dt={dt}&ds={synced}&kn={}&alib={}&cs=0&me={message}",
                unpack_id(&record[40..48]),
                alib_text(le16(record, 48)),
            );
        }
        2 => {
            let names = [
                "cct", "cst", "cop", "cr0", "cr1", "cba", "ccr", "cc0", "cc1", "cc2", "cc3", "cc4", "cc5", "cc6", "cc7",
            ];
            let _ = write!(
                line,
                "tt=2&rn={rn}&sn={serial}&dt={dt}&ds={synced}&kn={}&alib={}&cs=0",
                unpack_id(&record[24..32]),
                alib_text(le16(record, 22)),
            );
            for (name, value) in names.iter().zip(&record[7..22]) {
                let _ = write!(line, "&{name}={value}");
            }
        }
        _ => return None,
    }
    line.push_str("\r\n");
    Some(line)
}
