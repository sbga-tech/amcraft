//! The reader init sequence (`spec/libam/aime.md` E.3.4, `0x14034E310`) and the reader key material (E.5.1).

use super::InitStep;
use super::ReaderInfo;
use super::hub::Hub;
use super::wire::cmd;

/// MIFARE key slot A (`0x14071C5C0[0..6] ^ 0x8C`).
pub(crate) const KEY_A: [u8; 6] = [0x60, 0x90, 0xD0, 0x06, 0x32, 0xF5];
/// MIFARE key slot B (`0x14071C5C0[8..14] ^ 0x45`, "WCCFv2").
pub(crate) const KEY_B: [u8; 6] = *b"WCCFv2";

/// Obfuscated key bytes as stored in the image; decoding them here keeps the derivation checkable.
#[cfg(test)]
const RAW_KEYS: [u8; 14] = [0xEC, 0x1C, 0x5C, 0x8A, 0xBE, 0x79, 0x00, 0x00, 0x12, 0x06, 0x06, 0x03, 0x33, 0x77];

/// Init states `[324]` (even numbers wait for the response of the command sent by the state before).
mod state {
    pub const START: u8 = 0;
    pub const SEND_RESET: u8 = 3;
    pub const WAIT_RESET: u8 = 4;
    pub const SEND_FW: u8 = 5;
    pub const WAIT_FW: u8 = 6;
    pub const SEND_HW: u8 = 7;
    pub const WAIT_HW: u8 = 8;
    pub const SEND_PROBE: u8 = 9;
    pub const WAIT_PROBE: u8 = 10;
    pub const SEND_BOARD: u8 = 11;
    pub const WAIT_BOARD: u8 = 12;
    pub const SEND_KEY_B: u8 = 13;
    pub const WAIT_KEY_B: u8 = 14;
    pub const SEND_KEY_A: u8 = 15;
    pub const WAIT_KEY_A: u8 = 16;
    pub const DONE: u8 = 17;
}

/// ASCII text of a payload up to the first NUL.
fn text(payload: &[u8]) -> String {
    let end = payload.iter().position(|&byte| byte == 0).unwrap_or(payload.len());
    String::from_utf8_lossy(&payload[..end]).into_owned()
}

/// The firmware version answer (`0x30`): one byte on 2nd/3rd generation readers, text otherwise.
pub(super) fn store_fw(info: &mut ReaderInfo, payload: &[u8]) {
    if let [version] = payload[..] {
        info.fw_version = format!("{version:02X}");
        info.fw_kind = version;
    } else {
        info.fw_version = text(payload);
        info.fw_kind = if info.fw_version.contains("1.2") {
            2
        } else if info.fw_version.contains("1.0") {
            1
        } else if info.fw_version.is_empty() {
            0
        } else {
            9
        };
    }
}

/// The hardware version answer (`0x32`): the string and the generation it names (E.3.2).
pub(super) fn store_hw(info: &mut ReaderInfo, payload: &[u8]) {
    let hw = text(&payload[..payload.len().min(63)]);
    info.generation = if hw.starts_with("TN32MSEC003S H/W") {
        1
    } else if hw.contains("15286") {
        2
    } else if hw.contains("15396") {
        3
    } else {
        0
    };
    info.hw_version = hw;
}

/// The LED board info answer (`0xF0`): the name up to `0xFF`, then the version byte.
pub(super) fn store_board(info: &mut ReaderInfo, payload: &[u8]) {
    let split = payload.iter().position(|&byte| byte == 0xFF).unwrap_or(payload.len());
    info.led_board_name = String::from_utf8_lossy(&payload[..split]).into_owned();
    info.led_fw_version = payload.get(split + 1).copied().unwrap_or(0);
    info.led_present = true;
}

impl Hub {
    /// One call of the init machine.
    pub(super) fn init_step(&mut self, u: usize, now: u64) -> InitStep {
        self.pump(now);
        let timeout = self.units[u].config.command_timeout_ms;
        let board_timeout = self.units[u].config.board_timeout_ms;
        let unit = &mut self.units[u];
        unit.rw = true;
        if unit.stop
            && matches!(
                unit.init_state,
                state::SEND_RESET
                    | state::SEND_FW
                    | state::SEND_HW
                    | state::SEND_PROBE
                    | state::SEND_BOARD
                    | state::SEND_KEY_B
                    | state::SEND_KEY_A
            )
        {
            unit.init_state = state::DONE;
        }
        let unit_addr = unit.addr;
        match self.units[u].init_state {
            state::START => {
                self.units[u].arm(timeout);
                self.units[u].init_state = state::SEND_RESET;
            }
            state::SEND_RESET => {
                self.send_armed(u, timeout, cmd::TO_NORMAL, &[]);
                self.units[u].init_state = state::WAIT_RESET;
            }
            state::WAIT_RESET => {
                if let Some(response) = self.take_response(u, cmd::TO_NORMAL) {
                    if response.status == 0 {
                        self.units[u].initialised = false;
                    }
                    self.units[u].init_state = state::SEND_FW;
                } else if self.units[u].expired(now) {
                    return self.init_failed(u, cmd::TO_NORMAL);
                }
            }
            state::SEND_FW => {
                if self.units[u].initialised {
                    self.units[u].init_state = state::DONE;
                } else {
                    self.send_armed(u, timeout, cmd::GET_FW_VERSION, &[]);
                    self.units[u].init_state = state::WAIT_FW;
                }
            }
            state::WAIT_FW => {
                if let Some(response) = self.take_response(u, cmd::GET_FW_VERSION) {
                    store_fw(&mut self.units[u].info, &response.payload);
                    self.units[u].init_state = state::SEND_HW;
                } else if self.units[u].expired(now) {
                    return self.init_failed(u, cmd::GET_FW_VERSION);
                }
            }
            state::SEND_HW => {
                if self.units[u].info.fw_version.is_empty() {
                    return self.init_failed(u, cmd::GET_FW_VERSION);
                }
                self.send_armed(u, timeout, cmd::GET_HW_VERSION, &[]);
                self.units[u].init_state = state::WAIT_HW;
            }
            state::WAIT_HW => {
                if let Some(response) = self.take_response(u, cmd::GET_HW_VERSION) {
                    store_hw(&mut self.units[u].info, &response.payload);
                    self.units[u].init_state = state::SEND_PROBE;
                } else if self.units[u].expired(now) {
                    return self.init_failed(u, cmd::GET_HW_VERSION);
                }
            }
            state::SEND_PROBE => {
                self.send_armed(u, 1000, cmd::LED_PROBE, &[unit_addr]);
                self.units[u].init_state = state::WAIT_PROBE;
            }
            state::WAIT_PROBE => {
                if self.take_response(u, cmd::LED_PROBE).is_some() {
                    self.units[u].init_state = state::SEND_BOARD;
                } else if self.units[u].expired(now) {
                    // "EXT Board NotFound": go on without an LED board.
                    self.clear_command(u, cmd::LED_PROBE);
                    let info = &mut self.units[u].info;
                    info.led_present = false;
                    info.led_board_name = "No LED Board".into();
                    self.units[u].init_state = state::SEND_KEY_B;
                }
            }
            state::SEND_BOARD => {
                self.send_armed(u, board_timeout, cmd::LED_BOARD_INFO, &[unit_addr]);
                self.units[u].init_state = state::WAIT_BOARD;
            }
            state::WAIT_BOARD => {
                if let Some(response) = self.take_response(u, cmd::LED_BOARD_INFO) {
                    store_board(&mut self.units[u].info, &response.payload);
                    self.units[u].init_state = state::SEND_KEY_B;
                } else if self.units[u].expired(now) {
                    return self.init_failed(u, cmd::LED_BOARD_INFO);
                }
            }
            state::SEND_KEY_B => {
                self.send_armed(u, timeout, cmd::SET_KEY_B, &KEY_B);
                self.units[u].init_state = state::WAIT_KEY_B;
            }
            state::WAIT_KEY_B => {
                if self.take_response(u, cmd::SET_KEY_B).is_some() {
                    self.units[u].init_state = state::SEND_KEY_A;
                } else if self.units[u].expired(now) {
                    return self.init_failed(u, cmd::SET_KEY_B);
                }
            }
            state::SEND_KEY_A => {
                self.send_armed(u, timeout, cmd::SET_KEY_A, &KEY_A);
                self.units[u].init_state = state::WAIT_KEY_A;
            }
            state::WAIT_KEY_A => {
                if self.take_response(u, cmd::SET_KEY_A).is_some() {
                    self.units[u].init_state = state::DONE;
                } else if self.units[u].expired(now) {
                    return self.init_failed(u, cmd::SET_KEY_A);
                }
            }
            _ => {
                let unit = &mut self.units[u];
                unit.initialised = true;
                unit.init_state = state::START;
                unit.rw = false;
                return if std::mem::take(&mut unit.stop) { InitStep::Interrupted } else { InitStep::Done };
            }
        }
        InitStep::Running
    }

    /// Error exit: the driver goes inactive and the unit error flag is set.
    fn init_failed(&mut self, u: usize, command: u8) -> InitStep {
        self.clear_command(u, command);
        let unit = &mut self.units[u];
        unit.unit_error = true;
        unit.active = false;
        unit.init_state = state::START;
        unit.rw = false;
        InitStep::Failed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_decode_from_the_obfuscated_constants() {
        let a: Vec<u8> = RAW_KEYS[..6].iter().map(|byte| byte ^ 0x8C).collect();
        let b: Vec<u8> = RAW_KEYS[8..14].iter().map(|byte| byte ^ 0x45).collect();
        assert_eq!(a, KEY_A);
        assert_eq!(b, KEY_B);
    }
}
