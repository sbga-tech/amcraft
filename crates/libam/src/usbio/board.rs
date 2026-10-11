//! Board identification (`usbio.md` 1.3.2): the product string, the function block and the test that a board
//! satisfies a node configuration (1.4.2).

use libam_api::am_config::UsbIoFuncs;

use backend::hid::HidBoard;

/// What the board advertises in the eighth product field.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BoardFunctions {
    /// `SWIN`: players, buttons.
    pub switch: (u8, u8),
    /// `COININ`.
    pub coin: u8,
    /// `ADIN`: channels, valid bits.
    pub analog: (u8, u8),
    /// `ROTIN`.
    pub rotary: u8,
    /// `GOUT`.
    pub gout: u8,
    /// `PWMOUT`.
    pub pwm: u8,
    /// `UQ1..UQ9`: code and up to three parameter bytes.
    pub uniques: Vec<[u8; 4]>,
}

/// A parsed board description (the 452-byte record of `amUsbioGetNodeInfo`, reduced to what libam reads).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardDesc {
    /// Device path.
    pub path: String,
    /// Board number (`wchar_t[9]`).
    pub board_no: String,
    /// Mode byte: 0 boot loader, non-zero application.
    pub mode: u8,
    /// Firmware revision.
    pub revision: u8,
    /// Custom chip number (`wchar_t[6]`).
    pub custom_chip: String,
    /// Config byte; its low nibble is the node id.
    pub config: u8,
    /// Advertised functions.
    pub functions: BoardFunctions,
}

fn hex_field(text: &str, max: u32) -> Option<u32> {
    if text.is_empty() {
        return None;
    }
    u32::from_str_radix(text, 16).ok().filter(|v| *v <= max)
}

/// The comma separated hex parameters of one function key (at most 16 characters, each `<= 0xFF`).
fn params(value: &str, wanted: usize, max: usize) -> Option<Vec<u8>> {
    if value.chars().count() > 16 {
        return None;
    }
    let parsed: Vec<u8> =
        value.split(',').take(max).map(|p| hex_field(p, 0xFF).map(|v| v as u8)).collect::<Option<_>>()?;
    (parsed.len() >= wanted).then_some(parsed)
}

fn parse_functions(text: &str) -> Option<BoardFunctions> {
    let cut = text.find(['\r', '\n', '#']).unwrap_or(text.len());
    let items: Vec<&str> = text[..cut].split('_').filter(|item| !item.is_empty()).collect();
    if items.len() > 32 {
        return None;
    }
    let mut out = BoardFunctions::default();
    let mut seen: Vec<&str> = Vec::new();
    for item in items {
        let Some((key, value)) = item.split_once('=') else { break };
        if seen.contains(&key) {
            return None;
        }
        seen.push(key);
        match key {
            "GOUT" => out.gout = params(value, 1, 1)?[0],
            "PWMOUT" => out.pwm = params(value, 1, 1)?[0],
            "ADIN" => {
                let p = params(value, 2, 2)?;
                out.analog = (p[0], p[1]);
            }
            "ROTIN" => out.rotary = params(value, 1, 1)?[0],
            "COININ" => out.coin = params(value, 1, 1)?[0],
            "SWIN" => {
                let p = params(value, 2, 2)?;
                out.switch = (p[0], p[1]);
            }
            _ => {
                let digit = key.strip_prefix("UQ").and_then(|d| d.parse::<usize>().ok());
                if digit.is_some_and(|d| (1..=9).contains(&d)) {
                    let p = params(value, 1, 4)?;
                    let mut entry = [0; 4];
                    entry[..p.len()].copy_from_slice(&p);
                    out.uniques.push(entry);
                }
            }
        }
    }
    Some(out)
}

/// `setupNode` (`0x1402D3210`): `None` rejects the board.
pub fn parse_board(board: &HidBoard) -> Option<BoardDesc> {
    if board.manufacturer.chars().count() >= 33 || !board.manufacturer.starts_with("SEGA") {
        return None;
    }
    if board.product.chars().count() >= 127 + 64 {
        return None;
    }
    let fields: Vec<&str> = board.product.splitn(8, ';').collect();
    if fields.len() < 8 || fields[0].trim_end() != "I/O CONTROL BD" {
        return None;
    }
    let board_no = fields[1].trim_end_matches(' ');
    let custom_chip = fields[5].trim_end_matches(' ');
    if board_no.chars().count() >= 9 || custom_chip.chars().count() >= 6 {
        return None;
    }
    let function_text = fields[7].split(';').next().unwrap_or("");
    Some(BoardDesc {
        path: board.path.clone(),
        board_no: board_no.to_owned(),
        mode: hex_field(fields[2], 0xFF)? as u8,
        revision: hex_field(fields[3], 0xFF)? as u8,
        custom_chip: custom_chip.to_owned(),
        config: hex_field(fields[6], 0xFF)? as u8,
        functions: parse_functions(function_text)?,
    })
}

/// "Function satisfied" rule (`0x14024EBD0`).
pub fn satisfies(board: &BoardFunctions, required: &UsbIoFuncs) -> bool {
    let enough = |have: u8, need: u64| u64::from(have) >= need;
    if let Some(p) = required.switch_input
        && !(enough(board.switch.0, p.a) && enough(board.switch.1, p.b))
    {
        return false;
    }
    if let Some(n) = required.coin_input
        && !enough(board.coin, n)
    {
        return false;
    }
    if let Some(p) = required.analog_input
        && !(enough(board.analog.0, p.a) && enough(board.analog.1, p.b))
    {
        return false;
    }
    if let Some(n) = required.rotary_input
        && !enough(board.rotary, n)
    {
        return false;
    }
    if let Some(n) = required.general_output
        && !enough(board.gout, n)
    {
        return false;
    }
    if let Some(n) = required.pwm_output
        && !enough(board.pwm, n)
    {
        return false;
    }
    required.unique_io.iter().all(|want| {
        board.uniques.iter().any(|have| have[0] == want.command && (0..3).any(|k| want.params[k] <= have[1 + k]))
    })
}
