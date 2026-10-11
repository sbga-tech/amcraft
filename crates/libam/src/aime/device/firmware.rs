//! Firmware file loading (`spec/libam/aime.md` E.3.3) and the reader and LED board update machines (E.3.5, E.3.6).

use super::hub::Hub;
use super::init::{store_board, store_fw, store_hw};
use super::wire::{Response, cmd};
use super::{FirmwareFileError, FirmwareFiles, FirmwareStep, LedFirmwareStep};

/// Intel-HEX lines held at most (`+424` table).
const MAX_LINES: usize = 350;
/// Longest accepted line without the newline.
const MAX_LINE: usize = 43;
/// Size of a second/third generation image: 48 blocks of 256 bytes.
const IMAGE_SIZE: usize = 12288;
/// Block size of the binary transfer.
const BLOCK: usize = 256;
/// Offset of the HW version string in the first generation image.
const HW_OFFSET: usize = 104;
/// Pause after an accepted line or block (`Sleep(30)`).
const PAUSE_MS: u64 = 30;

/// Parsed Intel-HEX: the line table (raw ASCII without newline) and the flat data image.
struct Hex {
    lines: Vec<String>,
    image: Vec<u8>,
}

/// Parses the hex file the way the loader does: lines end in a newline, start with `:`, have at most 43 characters
/// and every record sums to 0 modulo 256.
fn parse_hex(bytes: &[u8]) -> Result<Hex, FirmwareFileError> {
    let mut hex = Hex { lines: Vec::new(), image: Vec::new() };
    for raw in bytes.split_inclusive(|&byte| byte == b'\n') {
        let line = raw.strip_suffix(b"\n").ok_or(FirmwareFileError::BadHex)?;
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.first() != Some(&b':') || line.len() > MAX_LINE || hex.lines.len() >= MAX_LINES {
            return Err(FirmwareFileError::BadHex);
        }
        let digits = &line[1..];
        if digits.len() % 2 != 0 || digits.len() < 10 {
            return Err(FirmwareFileError::BadHex);
        }
        let record = digits
            .chunks_exact(2)
            .map(|pair| {
                let text = std::str::from_utf8(pair).ok()?;
                u8::from_str_radix(text, 16).ok()
            })
            .collect::<Option<Vec<u8>>>()
            .ok_or(FirmwareFileError::BadHex)?;
        if record.iter().fold(0u8, |sum, &byte| sum.wrapping_add(byte)) != 0 {
            return Err(FirmwareFileError::BadHex);
        }
        hex.image.extend_from_slice(&record[4..record.len() - 1]);
        hex.lines.push(String::from_utf8_lossy(line).into_owned());
    }
    Ok(hex)
}

/// The C string at the start of `bytes`.
fn c_string(bytes: &[u8]) -> &[u8] {
    &bytes[..bytes.iter().position(|&byte| byte == 0).unwrap_or(bytes.len())]
}

/// The LED firmware version byte from the file name tail: `16 * <digit before "_NN.hex"> + NN`
/// (`837_15084_1_01.hex` is `0x11`, `0x140348C10`).
fn led_version(name: &str) -> Option<u8> {
    let stem = name.strip_suffix(".hex")?;
    let (head, tail) = stem.rsplit_once('_')?;
    let minor: u8 = tail.parse().ok()?;
    let major = head.chars().last()?.to_digit(10)? as u8;
    Some(major.wrapping_mul(16).wrapping_add(minor))
}

impl Hub {
    /// The reader firmware loader (`0x140347E90`, flag 0).
    pub(super) fn load_firmware(
        &mut self,
        u: usize,
        files: &FirmwareFiles,
        force: bool,
    ) -> Result<(), FirmwareFileError> {
        let mut lines = Vec::new();
        let (mut hw, mut fw) = (String::new(), String::new());
        let mut images = [None, None];
        let mut versions = [0xFF; 4];
        if let Some(bytes) = &files.hex {
            let hex = parse_hex(bytes)?;
            let tail = hex.image.get(HW_OFFSET..).unwrap_or(&[]);
            let hw_bytes = c_string(&tail[..tail.len().min(63)]);
            let fw_bytes = c_string(tail.get(hw_bytes.len() + 1..).unwrap_or(&[]));
            if hw_bytes.is_empty() || fw_bytes.is_empty() {
                return Err(FirmwareFileError::BadHex);
            }
            hw = String::from_utf8_lossy(hw_bytes).into_owned();
            fw = String::from_utf8_lossy(fw_bytes).into_owned();
            lines = hex.lines;
        }
        for (slot, image) in [&files.bin_gen2, &files.bin_gen3].into_iter().enumerate() {
            if let Some(bytes) = image {
                if bytes.len() != IMAGE_SIZE {
                    return Err(FirmwareFileError::BadImage);
                }
                versions[2 + slot] = bytes[IMAGE_SIZE - 3];
                images[slot] = Some(bytes.clone());
            }
        }
        if (files.hex.is_none() && images.iter().all(Option::is_none))
            || (lines.is_empty() && versions[2] == 0xFF && versions[3] == 0xFF)
        {
            return Err(FirmwareFileError::Missing);
        }
        let unit = &mut self.units[u];
        (unit.hex_lines, unit.file_hw, unit.file_fw, unit.images, unit.file_version) =
            (lines, hw, fw, images, versions);
        unit.force = force;
        unit.fw_state = 0;
        unit.fw_index = 0;
        Ok(())
    }

    /// The LED board firmware loader (`0x140347E90`, flag 1).
    pub(super) fn load_led_firmware(
        &mut self,
        u: usize,
        name: &str,
        hex: &[u8],
        force: bool,
    ) -> Result<(), FirmwareFileError> {
        let parsed = parse_hex(hex)?;
        let version = led_version(name).ok_or(FirmwareFileError::BadHex)?;
        let unit = &mut self.units[u];
        unit.hex_lines = parsed.lines;
        unit.file_version = [0xFF, version, 0xFF, 0xFF];
        unit.force = force;
        unit.led_state = 0;
        unit.fw_index = 0;
        Ok(())
    }

    /// `(current, total)` of the transfer (`+41308`, `+424`).
    pub(super) fn firmware_progress(&self, u: usize) -> (u32, u32) {
        let unit = &self.units[u];
        let total = if unit.info.generation >= 2 { (IMAGE_SIZE / BLOCK) as u32 } else { unit.hex_lines.len() as u32 };
        (unit.fw_index, total)
    }

    /// Ends the reader update with `result` and rearms the machine.
    fn firmware_end(&mut self, u: usize, result: FirmwareStep) -> FirmwareStep {
        let unit = &mut self.units[u];
        unit.fw_state = 0;
        unit.rw = false;
        result
    }

    /// A wait state ran out: the entry is dropped and the update ends with `result`.
    fn firmware_timeout(&mut self, u: usize, command: u8, result: FirmwareStep) -> FirmwareStep {
        self.clear_command(u, command);
        self.firmware_end(u, result)
    }

    /// One call of the reader update machine (`0x14034DAF0`).
    pub(super) fn firmware_step(&mut self, u: usize, now: u64) -> FirmwareStep {
        self.pump(now);
        let timeout = self.units[u].config.command_timeout_ms;
        let unit = &mut self.units[u];
        unit.rw = true;
        if now < unit.sleep_until {
            return FirmwareStep::Running;
        }
        let state = unit.fw_state;
        match state {
            0 => {
                unit.fw_index = 0;
                unit.fw_restarted = false;
                unit.fw_state = match (unit.force, self.any_sent) {
                    (true, true) => 7,
                    (true, false) => 9,
                    (false, _) => 1,
                };
            }
            1 => {
                self.send_armed(u, timeout, cmd::TO_NORMAL, &[]);
                self.units[u].fw_state = 2;
            }
            2 | 4 | 6 => {
                let command = [cmd::TO_NORMAL, cmd::GET_FW_VERSION, cmd::GET_HW_VERSION][usize::from(state / 2 - 1)];
                if let Some(response) = self.take_response(u, command) {
                    let info = &mut self.units[u].info;
                    match state {
                        4 => store_fw(info, &response.payload),
                        6 => store_hw(info, &response.payload),
                        _ => {}
                    }
                    self.units[u].fw_state = state + 1;
                } else if self.units[u].expired(now) {
                    return self.firmware_timeout(u, command, FirmwareStep::VersionFault);
                }
            }
            3 => {
                self.send_armed(u, timeout, cmd::GET_FW_VERSION, &[]);
                self.units[u].fw_state = 4;
            }
            5 => {
                self.send_armed(u, timeout, cmd::GET_HW_VERSION, &[]);
                self.units[u].fw_state = 6;
            }
            7 => return self.firmware_decide(u, timeout),
            8 => {
                if self.take_response(u, cmd::TO_UPDATER).is_some() {
                    self.units[u].fw_state = 9;
                } else if self.units[u].expired(now) {
                    return self.firmware_timeout(u, cmd::TO_UPDATER, FirmwareStep::VersionFault);
                }
            }
            9 => {
                if !unit.force && unit.info.generation == 1 && unit.hex_lines.is_empty() {
                    return self.firmware_end(u, FirmwareStep::Failed);
                }
                let line = unit.hex_lines.get(unit.fw_index as usize).cloned().unwrap_or_default();
                self.send_armed(u, timeout, cmd::HEX_DATA, line.as_bytes());
                self.units[u].fw_state = 10;
            }
            10 => {
                if let Some(response) = self.take_response(u, cmd::HEX_DATA) {
                    return self.hex_response(u, &response, now);
                }
                if self.units[u].expired(now) {
                    return self.firmware_timeout(u, cmd::HEX_DATA, FirmwareStep::VersionFault);
                }
            }
            11 => {
                let version = unit.file_version[usize::from(unit.info.generation) & 3];
                if version == 0xFF {
                    return self.firmware_end(u, FirmwareStep::Failed);
                }
                if unit.info.fw_kind == version {
                    return self.firmware_end(u, FirmwareStep::AlreadyNew);
                }
                self.send_armed(u, timeout, cmd::BIN_START, &[]);
                self.units[u].fw_state = 12;
            }
            12 => {
                if self.take_response(u, cmd::BIN_START).is_some() {
                    let unit = &mut self.units[u];
                    unit.fw_index = 0;
                    unit.fw_state = 13;
                } else if self.units[u].expired(now) {
                    return self.firmware_timeout(u, cmd::BIN_START, FirmwareStep::Failed);
                }
            }
            13 => {
                let slot = usize::from(unit.info.generation.saturating_sub(2).min(1));
                let start = unit.fw_index as usize * BLOCK;
                let Some(block) = unit.images[slot].as_ref().and_then(|image| image.get(start..start + BLOCK)) else {
                    return self.firmware_end(u, FirmwareStep::Done);
                };
                let block = block.to_vec();
                self.send_armed(u, timeout, cmd::BIN_BLOCK, &block);
                self.units[u].fw_state = 14;
            }
            14 => {
                if let Some(response) = self.take_response(u, cmd::BIN_BLOCK) {
                    let unit = &mut self.units[u];
                    match response.status {
                        7 => return self.firmware_end(u, FirmwareStep::Failed),
                        8 => return self.firmware_end(u, FirmwareStep::Done),
                        _ => {
                            unit.fw_index += 1;
                            unit.fw_state = 13;
                            unit.sleep_until = now + PAUSE_MS;
                        }
                    }
                } else if self.units[u].expired(now) {
                    return self.firmware_timeout(u, cmd::BIN_BLOCK, FirmwareStep::Failed);
                }
            }
            15 => return self.firmware_end(u, FirmwareStep::Done),
            _ => return self.firmware_end(u, FirmwareStep::Failed),
        }
        FirmwareStep::Running
    }

    /// State 7: the update decision after the version answers.
    fn firmware_decide(&mut self, u: usize, timeout: u32) -> FirmwareStep {
        let unit = &mut self.units[u];
        let generation = unit.info.generation;
        if generation == 0 || (generation == 1 && unit.hex_lines.is_empty()) {
            return self.firmware_end(u, FirmwareStep::VersionFault);
        }
        let up_to_date = generation == 1
            && !unit.force
            && unit.info.hw_version == unit.file_hw
            && unit.info.fw_version == unit.file_fw;
        if up_to_date {
            unit.fw_state = 15;
        } else {
            self.send_armed(u, timeout, cmd::TO_UPDATER, &[]);
            self.units[u].fw_state = 8;
        }
        FirmwareStep::Running
    }

    /// The answer of a hex line (`0x61`): the status names the generation.
    fn hex_response(&mut self, u: usize, response: &Response, now: u64) -> FirmwareStep {
        let unit = &mut self.units[u];
        match response.status {
            0 => {
                unit.info.generation = 1;
                unit.fw_index += 1;
                unit.fw_state = if (unit.fw_index as usize) < unit.hex_lines.len() { 9 } else { 15 };
                unit.sleep_until = now + PAUSE_MS;
            }
            16 | 32 => {
                unit.info.generation = if response.status == 16 { 2 } else { 3 };
                if unit.file_version[usize::from(unit.info.generation)] == 0xFF {
                    return self.firmware_end(u, FirmwareStep::Failed);
                }
                unit.fw_state = 11;
            }
            _ if unit.fw_restarted => return self.firmware_end(u, FirmwareStep::Failed),
            _ => {
                unit.fw_restarted = true;
                unit.force = false;
                unit.fw_state = 0;
            }
        }
        FirmwareStep::Running
    }

    /// One call of the LED board update machine (`0x14034EBC0`).
    pub(super) fn led_firmware_step(&mut self, u: usize, now: u64) -> LedFirmwareStep {
        self.pump(now);
        let unit = &mut self.units[u];
        let (timeout, board_timeout) = (unit.config.command_timeout_ms, unit.config.board_timeout_ms);
        let (state, force, addr) = (unit.led_state, unit.force, unit.addr);
        unit.rw = true;
        if now < unit.sleep_until {
            return LedFirmwareStep::Running;
        }
        match state {
            0 => {
                unit.fw_index = 0;
                unit.fw_restarted = false;
                unit.led_state = match (force, self.any_sent) {
                    (true, true) => 5,
                    (true, false) => 7,
                    (false, _) => 1,
                };
            }
            1 => {
                self.send_armed(u, timeout, cmd::LED_PROBE, &[addr]);
                self.units[u].led_state = 2;
            }
            2 => {
                if self.take_response(u, cmd::LED_PROBE).is_some() {
                    self.units[u].led_state = 3;
                } else if self.units[u].expired(now) {
                    return self.led_firmware_end(u, Some(cmd::LED_PROBE), LedFirmwareStep::Failed);
                }
            }
            3 => {
                self.send_armed(u, board_timeout, cmd::LED_BOARD_INFO, &[addr]);
                self.units[u].led_state = 4;
            }
            4 => {
                if let Some(response) = self.take_response(u, cmd::LED_BOARD_INFO) {
                    store_board(&mut self.units[u].info, &response.payload);
                    self.units[u].led_state = 5;
                } else if self.units[u].expired(now) {
                    if force {
                        self.clear_command(u, cmd::LED_BOARD_INFO);
                        self.units[u].led_state = 5;
                    } else {
                        return self.led_firmware_end(u, Some(cmd::LED_BOARD_INFO), LedFirmwareStep::Failed);
                    }
                }
            }
            5 => {
                let board = if unit.info.led_present { unit.info.led_fw_version } else { 0 };
                if !force && board == unit.file_version[1] {
                    unit.led_state = 9;
                } else {
                    self.send_armed(u, timeout, cmd::LED_BOOT, &[addr]);
                    self.units[u].led_state = 6;
                }
            }
            6 => {
                if self.take_response(u, cmd::LED_BOOT).is_some() {
                    let unit = &mut self.units[u];
                    unit.fw_index = 0;
                    unit.led_state = 7;
                } else if self.units[u].expired(now) {
                    self.clear_command(u, cmd::LED_BOOT);
                    if force {
                        self.units[u].led_state = 7;
                    } else {
                        return self.led_firmware_end(u, None, LedFirmwareStep::Failed);
                    }
                }
            }
            7 => {
                let mut payload = vec![addr];
                if let Some(line) = unit.hex_lines.get(unit.fw_index as usize) {
                    payload.extend_from_slice(line.as_bytes());
                }
                self.send_armed(u, timeout, cmd::LED_DATA, &payload);
                self.units[u].led_state = 8;
            }
            8 => {
                if let Some(response) = self.take_response(u, cmd::LED_DATA) {
                    let unit = &mut self.units[u];
                    if response.status == 0 {
                        unit.fw_index += 1;
                        unit.led_state = if (unit.fw_index as usize) < unit.hex_lines.len() { 7 } else { 8 };
                        unit.sleep_until = now + PAUSE_MS;
                    } else if unit.fw_restarted {
                        unit.led_state = 9;
                    } else {
                        unit.fw_restarted = true;
                        unit.force = false;
                        unit.led_state = 0;
                    }
                } else if self.units[u].expired(now) {
                    self.clear_command(u, cmd::LED_DATA);
                    let unit = &mut self.units[u];
                    if (unit.fw_index as usize) < unit.hex_lines.len() {
                        return self.led_firmware_end(u, None, LedFirmwareStep::Failed);
                    }
                    unit.led_state = 9;
                }
            }
            _ => {
                unit.info.led_fw_version = unit.file_version[1];
                return self.led_firmware_end(u, None, LedFirmwareStep::Done);
            }
        }
        LedFirmwareStep::Running
    }

    /// Ends the LED update with `result` and rearms the machine.
    fn led_firmware_end(&mut self, u: usize, command: Option<u8>, result: LedFirmwareStep) -> LedFirmwareStep {
        if let Some(command) = command {
            self.clear_command(u, command);
        }
        let unit = &mut self.units[u];
        unit.led_state = 0;
        unit.rw = false;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn led_version_comes_from_the_file_name() {
        assert_eq!(led_version("./firm/837_15084_1_01.hex".rsplit('/').next().unwrap_or("")), Some(0x11));
        assert_eq!(led_version("x_2_13.hex"), Some(0x2D));
        assert_eq!(led_version("nothing.bin"), None);
    }

    #[test]
    fn hex_records_must_sum_to_zero() {
        // `:020000040000FA` is a valid extended-address record.
        let good = parse_hex(b":020000040000FA\n:00000001FF\n").expect("valid");
        assert_eq!(good.lines.len(), 2);
        assert_eq!(good.image, vec![0, 0]);
        assert_eq!(parse_hex(b":020000040000FB\n").err(), Some(FirmwareFileError::BadHex));
        assert_eq!(parse_hex(b":00000001FF").err(), Some(FirmwareFileError::BadHex));
    }
}
