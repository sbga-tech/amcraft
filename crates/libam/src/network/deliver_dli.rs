//! The download instruction (DLI) file of the fetcher (`spec/libam/network.md` D.10.5): an INI file with the
//! `[COMMON]` keys and the image lists `INSTALL<n>` / `EXIST<n>`.

use crate::time::DateTime;

use super::deliver_image::{ImageName, parse_image_filename};

/// Why a DLI was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DliError {
    /// A required key is missing or malformed (the engine status 302).
    #[error("download instruction has a bad key: {0}")]
    Format(&'static str),
    /// `GAME_ID` is not the system's (the engine status 303).
    #[error("download instruction is for another game")]
    GameId,
}

/// One image the DLI lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DliImage {
    /// The URL (`INSTALL`) or file name (`EXIST`) text.
    pub location: String,
    /// The file-name grammar result (D.10.4).
    pub name: ImageName,
    /// Only an existing local file is expected (`EXIST<n>`).
    pub exist_only: bool,
    /// Listed under `[OPTIONAL]`.
    pub optional: bool,
}

/// A parsed DLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dli {
    /// `DLFORMAT * 100`.
    pub format: u32,
    /// `GAME_ID`.
    pub game_id: String,
    /// `ORDER_TIME`.
    pub order_time: DateTime,
    /// `RELEASE_TIME`.
    pub release_time: DateTime,
    /// `PART_SIZE`: bytes per request for the three line types.
    pub part_size: [u32; 3],
    /// `INTERVAL`, `DSL_INTERVAL`, `BB_INTERVAL` rows of four levels.
    pub intervals: [[i32; 4]; 3],
    /// `CLOUD`: one level (0..=3) per half hour.
    pub cloud: [u8; 48],
    /// `REPORT_INTERVAL` seconds, 0 when absent.
    pub report_interval: u32,
    /// `REPORT` URL.
    pub report_url: String,
    /// `GAME_DESC`.
    pub game_desc: String,
    /// `IMMEDIATELY_RELEASE`.
    pub immediate_release: bool,
    /// `RELEASE_WITH_OPTION`.
    pub release_with_option: bool,
    /// The listed images, in file order.
    pub images: Vec<DliImage>,
}

/// `[section]` / `key=value` lines; names compare case-insensitively like `GetPrivateProfileString`.
struct Ini<'a> {
    lines: Vec<(String, &'a str, &'a str)>,
}

impl<'a> Ini<'a> {
    fn parse(text: &'a str) -> Self {
        let mut section = String::new();
        let mut lines = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with([';', '#']) {
                continue;
            }
            if let Some(name) = line.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')) {
                section = name.trim().to_ascii_uppercase();
            } else if let Some((key, value)) = line.split_once('=') {
                lines.push((section.clone(), key.trim(), value.trim()));
            }
        }
        Self { lines }
    }

    fn get(&self, section: &str, key: &str) -> Option<&'a str> {
        self.lines.iter().find(|(name, k, _)| name == section && k.eq_ignore_ascii_case(key)).map(|line| line.2)
    }
}

/// `YYYY-MM-DDTHH:MM:SS` with an optional `Z` or `+hh:mm` suffix, as the DLI writes times. Without a suffix the
/// time is taken as given (the system time zone is not known here [INFERENCE]).
fn parse_time(text: &str) -> Option<DateTime> {
    if text.len() < 19 || !text.is_char_boundary(19) {
        return None;
    }
    let (stamp, suffix) = text.split_at(19);
    let number = |from: usize, to: usize| stamp.get(from..to)?.parse::<i32>().ok();
    let (separators_ok, _) = (stamp.as_bytes()[4] == b'-' && stamp.as_bytes()[10] == b'T', ());
    if !separators_ok {
        return None;
    }
    let base = DateTime::make(
        number(0, 4)?,
        number(5, 7)?,
        number(8, 10)?,
        number(11, 13)?,
        number(14, 16)?,
        number(17, 19)?,
        0,
    )?;
    let bias_minutes = match suffix {
        "" | "Z" => 0,
        _ => {
            let sign = match suffix.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let (hours, minutes) = suffix.get(1..)?.split_once(':')?;
            let (hours, minutes): (i64, i64) = (hours.parse().ok()?, minutes.parse().ok()?);
            if hours > 14 || minutes >= 60 {
                return None;
            }
            -sign * (60 * hours + minutes)
        }
    };
    Some(DateTime(base.0 + bias_minutes * 60 * DateTime::MICROS_PER_SECOND))
}

fn parse_part_size(text: &str) -> Option<[u32; 3]> {
    let values: Vec<u32> = text.split(',').map(|part| part.trim().parse().ok()).collect::<Option<_>>()?;
    let values: [u32; 3] = values.try_into().ok()?;
    values.iter().all(|v| *v >= 2 && v.is_power_of_two()).then_some(values)
}

/// Up to four comma-separated values; missing ones repeat the previous (D.10.5).
fn parse_row(text: &str) -> Option<[i32; 4]> {
    let values: Vec<i32> = text.split(',').map(|part| part.trim().parse().ok()).collect::<Option<_>>()?;
    if values.is_empty() || values.len() > 4 || values.iter().all(|v| *v < 0) {
        return None;
    }
    let mut row = [0; 4];
    for (index, slot) in row.iter_mut().enumerate() {
        *slot = *values.get(index).unwrap_or_else(|| values.last().expect("not empty"));
    }
    Some(row)
}

fn parse_cloud(text: &str) -> Option<[u8; 48]> {
    let bytes = text.as_bytes();
    (bytes.len() == 48 && bytes.iter().all(|b| (b'0'..=b'3').contains(b)))
        .then(|| std::array::from_fn(|index| bytes[index] - b'0'))
}

impl Dli {
    /// Parses a DLI (a UTF-8 byte-order mark is skipped) and checks `GAME_ID` against `system_game_id`.
    pub fn parse(bytes: &[u8], system_game_id: &str) -> Result<Self, DliError> {
        let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
        let text = String::from_utf8_lossy(bytes);
        let ini = Ini::parse(&text);
        let need = |section: &str, key: &'static str| ini.get(section, key).ok_or(DliError::Format(key));
        let format = need("COMMON", "DLFORMAT")?.parse::<f64>().map_err(|_| DliError::Format("DLFORMAT"))?;
        let format = (format * 100.0 + 0.5) as u32;
        if !(400..=500).contains(&format) {
            return Err(DliError::Format("DLFORMAT"));
        }
        let game_id = need("COMMON", "GAME_ID")?;
        if game_id.len() != 4 || !game_id.bytes().all(|b| b.is_ascii_alphabetic()) {
            return Err(DliError::Format("GAME_ID"));
        }
        let order_time = parse_time(need("COMMON", "ORDER_TIME")?).ok_or(DliError::Format("ORDER_TIME"))?;
        let release_time = parse_time(need("COMMON", "RELEASE_TIME")?).ok_or(DliError::Format("RELEASE_TIME"))?;
        let part_size = parse_part_size(need("COMMON", "PART_SIZE")?).ok_or(DliError::Format("PART_SIZE"))?;
        let mut intervals = [[0; 4]; 3];
        for (row, key) in intervals.iter_mut().zip(["INTERVAL", "DSL_INTERVAL", "BB_INTERVAL"]) {
            *row = parse_row(need("COMMON", key)?).ok_or(DliError::Format(key))?;
        }
        let cloud = parse_cloud(need("COMMON", "CLOUD")?).ok_or(DliError::Format("CLOUD"))?;
        let flag = |key| ini.get("COMMON", key).and_then(|v| v.parse::<u32>().ok()).is_some_and(|v| v != 0);
        let mut images = Vec::new();
        for (section, optional) in [("COMMON", false), ("OPTIONAL", true)] {
            for (prefix, exist_only) in [("INSTALL", false), ("EXIST", true)] {
                if optional && exist_only {
                    continue;
                }
                for index in 1.. {
                    let Some(location) = ini.get(section, &format!("{prefix}{index}")) else { break };
                    if !location.is_ascii() || location.len() >= 256 {
                        return Err(DliError::Format("image entry"));
                    }
                    let name = parse_image_filename(location).ok_or(DliError::Format("image name"))?;
                    images.push(DliImage { location: location.to_owned(), name, exist_only, optional });
                }
            }
        }
        if !game_id.eq_ignore_ascii_case(system_game_id) {
            return Err(DliError::GameId);
        }
        Ok(Self {
            format,
            game_id: game_id.to_ascii_uppercase(),
            order_time,
            release_time,
            part_size,
            intervals,
            cloud,
            report_interval: ini.get("COMMON", "REPORT_INTERVAL").and_then(|v| v.parse().ok()).unwrap_or(0),
            report_url: ini.get("COMMON", "REPORT").unwrap_or_default().to_owned(),
            game_desc: ini.get("COMMON", "GAME_DESC").unwrap_or_default().chars().take(2047).collect(),
            immediate_release: flag("IMMEDIATELY_RELEASE"),
            release_with_option: flag("RELEASE_WITH_OPTION"),
            images,
        })
    }

    /// The milliseconds to wait between chunk requests at local time `hour:minute` for line type `line`
    /// (`0x1402F0060`): a negative value means "no download in this slot".
    pub fn interval_ms(&self, line: usize, hour: u32, minute: u32) -> i32 {
        let level = usize::from(self.cloud[(2 * hour + minute / 30).min(47) as usize]);
        self.intervals[line.min(2)][level]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\u{feff}[COMMON]\r\nDLFORMAT=4.30\r\nGAME_ID=SDEZ\r\nORDER_TIME=2024-05-01T10:00:00Z\r\n\
RELEASE_TIME=2024-05-02T10:00:00+09:00\r\nPART_SIZE=1048576,2097152,4194304\r\nINTERVAL=-1,1000\r\n\
DSL_INTERVAL=2000\r\nBB_INTERVAL=3000,-1\r\nCLOUD=000000000000000000000000000000000000000000000000\r\n\
GAME_DESC=hello\r\nINSTALL1=http://x/SDEZ_1.02.03_20240102030405_0.app\r\n[OPTIONAL]\r\n\
INSTALL1=http://x/SDEZ_A001_20240102030405_0.opt\r\n";

    #[test]
    fn parses_the_common_block_and_both_image_lists() {
        let dli = Dli::parse(SAMPLE.as_bytes(), "SDEZ").unwrap();
        assert_eq!(dli.format, 430);
        assert_eq!(dli.part_size, [1 << 20, 2 << 20, 4 << 20]);
        assert_eq!(dli.intervals[0], [-1, 1000, 1000, 1000]);
        assert_eq!(dli.images.len(), 2);
        assert!(dli.images[1].optional);
        assert!(dli.release_time > dli.order_time);
        assert_eq!(dli.interval_ms(0, 3, 40), -1);
    }

    #[test]
    fn another_game_id_is_rejected_after_the_whole_file_parsed() {
        assert_eq!(Dli::parse(SAMPLE.as_bytes(), "SDAA"), Err(DliError::GameId));
        let broken = SAMPLE.replace("DLFORMAT=4.30", "DLFORMAT=9.00");
        assert_eq!(Dli::parse(broken.as_bytes(), "SDEZ"), Err(DliError::Format("DLFORMAT")));
    }
}
