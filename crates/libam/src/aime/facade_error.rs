//! Error-code mappings of the Aime facade (`spec/libam/aime.md` A.9).

use crate::nvram::RecordStatus;

/// A libam `ErrorCode` (`0..=24`) of a finished reader sequence.
pub(super) type ErrorCode = u32;

/// Fatal number of a reader error that lasts (`0x140258010(code, flag)`): `0` means "no error to display".
/// `develop` is the global flag byte that selects 87 instead of 84 for code 23.
pub(super) fn fatal_number(code: ErrorCode, develop: bool) -> u16 {
    match code {
        0 | 1 | 15..=21 => 0,
        2 | 4 => 6501,
        3 | 22 => 6506,
        5 | 7 | 14 => 6503,
        6 | 11 | 13 => 6504,
        8..=10 | 12 | 24 => 6508,
        23 => {
            if develop {
                87
            } else {
                84
            }
        }
        _ => 6502,
    }
}

/// Whether the fatal number carries the unit index as its sub-number (`0x140256B10`: 6501 and 6506).
pub(super) fn has_unit_sub_number(number: u16) -> bool {
    matches!(number, 6501 | 6506)
}

/// `0x14025AEC0` over a record status: the error a failed record load or store turns into.
pub(super) fn record_error(status: RecordStatus, develop: bool) -> Option<(u16, u16)> {
    crate::nvram::NvramContext::status_error(status, develop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reader_codes_map_to_the_displayed_numbers() {
        assert_eq!(fatal_number(0, false), 0);
        assert_eq!(fatal_number(19, false), 0);
        assert_eq!(fatal_number(2, false), 6501);
        assert_eq!(fatal_number(22, false), 6506);
        assert_eq!(fatal_number(14, false), 6503);
        assert_eq!(fatal_number(13, false), 6504);
        assert_eq!(fatal_number(24, false), 6508);
        assert_eq!(fatal_number(23, false), 84);
        assert_eq!(fatal_number(23, true), 87);
        assert_eq!(fatal_number(99, false), 6502);
    }
}
