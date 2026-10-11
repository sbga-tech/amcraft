//! The firmware file (`usbio.md` 1.8.1): name tokens and Motorola S-record content.

/// A validated firmware file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirmwareFile {
    /// Product token (`T2`), compared with the board number.
    pub product: String,
    /// Revision token (`T4`), compared with the board's firmware revision.
    pub revision: u8,
    /// The non-empty lines, each with its trailing line feed.
    pub lines: Vec<String>,
}

impl FirmwareFile {
    /// Whether a board needs this file (`0x140299F40`: product or revision differ).
    pub fn differs_from(&self, board_no: &str, revision: u8) -> bool {
        self.product != board_no || self.revision != revision
    }
}

fn token(text: &str, max: u32) -> Option<u32> {
    u32::from_str_radix(text, 16).ok().filter(|v| *v <= max)
}

/// `0x140299FC0`: file name `T1_T2_T3_T4_T5.ext`.
fn parse_name(path: &str) -> Option<(String, u8)> {
    let name = path.rsplit(['\\', '/']).next()?;
    let (stem, _extension) = name.rsplit_once('.')?;
    let tokens: Vec<&str> = stem.split('_').collect();
    let [_, product, t3, t4, t5] = tokens.as_slice() else { return None };
    if product.chars().count() > 8 {
        return None;
    }
    token(t3, 0xFFFF)?;
    let revision = token(t4, 0xFF)?;
    token(t5, 0xFFFF)?;
    Some(((*product).to_owned(), revision as u8))
}

/// `0x14029A3C0`: one S-record line.
fn valid_record(line: &str) -> bool {
    let bytes = line.as_bytes();
    if bytes.len() < 14 || bytes[0] != b'S' || !line.is_ascii() {
        return false;
    }
    let Some(count) = line.get(2..4).and_then(|c| u8::from_str_radix(c, 16).ok()) else { return false };
    let count = usize::from(count);
    if count < 3 || line.len() < 4 + 2 * count {
        return false;
    }
    let mut sum = count as u8;
    for k in 0..count - 1 {
        let Some(byte) = line.get(4 + 2 * k..6 + 2 * k).and_then(|c| u8::from_str_radix(c, 16).ok()) else {
            return false;
        };
        sum = sum.wrapping_add(byte);
    }
    let Some(check) = line.get(4 + 2 * (count - 1)..6 + 2 * (count - 1)).and_then(|c| u8::from_str_radix(c, 16).ok())
    else {
        return false;
    };
    !sum == check
}

/// Validates the name and content of a firmware file; `None` rejects it.
pub fn parse_firmware(path: &str, text: &str) -> Option<FirmwareFile> {
    let (product, revision) = parse_name(path)?;
    let mut lines = Vec::new();
    for raw in text.replace("\r\n", "\n").replace('\r', "\n").split('\n') {
        if raw.is_empty() {
            continue;
        }
        if !valid_record(raw) {
            return None;
        }
        lines.push(format!("{raw}\n"));
    }
    (!lines.is_empty()).then_some(FirmwareFile { product, revision, lines })
}
