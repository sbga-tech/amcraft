//! The serial block shared by the board serial (`platform.md` A.6) and the keychip serial (B.1): a standard
//! serial `XXXX-XXXdddddddd` and the short serial made of its first 11 significant characters.
//!
//! Hand-written because the validator is specific to this binary (`0x1402C6E60`, `0x1402C6AC0`, `0x140226290`).

use crate::platform::LibamSerial;

use crate::util::wide_array;

/// Length of a standard serial in characters.
const SERIAL_LEN: usize = 16;

/// A valid serial pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Serial {
    standard: [u16; 17],
    short: [u16; 12],
}

/// The serial validator (`0x1402C6E60`): 16 ASCII characters, index 4 is `-`, indices 0-3 and 5-7 are a digit or
/// an upper-case letter other than `I` and `O`, indices 8-15 are digits.
pub fn is_valid_serial(text: &[u8]) -> bool {
    text.len() == SERIAL_LEN
        && text.iter().enumerate().all(|(index, &c)| match index {
            4 => c == b'-',
            0..=3 | 5..=7 => c.is_ascii_digit() || (c.is_ascii_uppercase() && c != b'I' && c != b'O'),
            _ => c.is_ascii_digit(),
        })
}

/// The short serial (`0x1402C6AC0`): characters `[0..4)` and `[5..12)`, 11 characters. `None` when `text` is not
/// a valid serial.
pub fn short_serial(text: &[u8]) -> Option<String> {
    is_valid_serial(text).then(|| {
        let mut short = String::with_capacity(11);
        short.extend(text[..4].iter().chain(&text[5..12]).map(|&c| char::from(c)));
        short
    })
}

impl Serial {
    /// The serial setter (`0x140226290`): `text` must be exactly 16 characters; it is upper-cased (ASCII, the
    /// CRT "C" locale) and validated. `None` leaves the block invalid and raises no error.
    pub fn from_text(text: &str) -> Option<Self> {
        let upper = text.to_ascii_uppercase();
        let short = short_serial(upper.as_bytes())?;
        Some(Self { standard: wide_array(&upper), short: wide_array(&short) })
    }

    /// The short serial as text.
    pub fn short_text(&self) -> String {
        crate::util::wide_to_string(&self.short)
    }

    /// The `wchar_t[17]` + `wchar_t[12]` layout the port returns.
    pub fn to_api(self) -> LibamSerial {
        LibamSerial { standard: self.standard, short: self.short }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serial_is_upper_cased_and_shortened() {
        let serial = Serial::from_text("abcd-efg12345678").unwrap();
        assert_eq!(crate::util::wide_to_string(&serial.to_api().standard), "ABCD-EFG12345678");
        assert_eq!(serial.short_text(), "ABCDEFG1234");
    }

    #[test]
    fn letters_i_and_o_and_wrong_shapes_are_refused() {
        assert!(Serial::from_text("ABCD-EFI12345678").is_none());
        assert!(Serial::from_text("ABCD-EFG1234567O").is_none());
        assert!(Serial::from_text("ABCDEEFG12345678").is_none());
        assert!(Serial::from_text("ABCD-EFG1234567").is_none());
    }
}
