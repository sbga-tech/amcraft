//! The `is_restored` property of `alib.conf` (`allnet.md` "Properties").
//!
//! The settings file is parsed by the original's own routine (`0x1403DD010`) with quirks no general INI crate has:
//! keys are matched by prefix, values are not trimmed (a CR stays in the value), a final line without terminator is
//! ignored, and a value of 64 characters or more aborts the load. Only the `is_restored` slot is consumed by this
//! build (the start-up trace-log restore), so the parser follows the original exactly but keeps that one value.

/// Property keys in slot order (`0x140739C80`, 64-byte stride).
const KEYS: [&[u8]; 5] = [
    b"accounting-report.host",
    b"accounting-report.start-time",
    b"accounting-report.delay-time",
    b"accounting-report.near-full",
    b"is_restored",
];
/// The slot of `is_restored`.
const IS_RESTORED: usize = 4;
/// A key table entry is a 64-byte zero-padded field and the lookup compares that many bytes of it.
const ENTRY_SIZE: usize = 64;
/// The read buffer of the loader: a file of 4096 bytes or more is not loaded.
const BUFFER: usize = 4096;
/// Longest stored value; a longer one makes the load fail.
const VALUE_LIMIT: usize = 63;

/// `0x1403DCEA0`: the slot whose key starts with `key` (first match wins), `None` for an empty or unknown key.
fn lookup(key: &[u8]) -> Option<usize> {
    if key.is_empty() {
        return None;
    }
    KEYS.iter().position(|entry| {
        key.len() <= ENTRY_SIZE
            && key.iter().enumerate().all(|(index, &byte)| byte == entry.get(index).copied().unwrap_or(0))
    })
}

/// Whitespace as C `isspace`.
fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t'..=b'\r')
}

/// The index of the next LF at or after `from`, or the end of the text.
fn line_end(text: &[u8], from: usize) -> usize {
    text.get(from..).and_then(|rest| rest.iter().position(|&byte| byte == b'\n')).map_or(text.len(), |at| from + at)
}

/// The value the loader stores for `is_restored`, `None` while the file does not set it.
fn is_restored(text: &[u8]) -> Option<&[u8]> {
    if text.len() >= BUFFER {
        return None;
    }
    let mut stored = None;
    let mut pos = 0;
    while pos < text.len() {
        let byte = text[pos];
        if byte == b';' {
            pos = line_end(text, pos + 1) + 1;
            continue;
        }
        if is_space(byte) {
            pos += 1;
            continue;
        }
        // The key runs to `=`; a line feed on the way restarts it.
        let mut key_start = pos;
        let mut at = pos;
        while at < text.len() && text[at] != b'=' {
            if text[at] == b'\n' {
                key_start = at + 1;
            }
            at += 1;
        }
        if at >= text.len() {
            break;
        }
        let Some(slot) = lookup(&text[key_start..at]) else {
            // An unknown key skips the line, counted from the second character after `=`.
            pos = line_end(text, at + 2);
            continue;
        };
        let value_start = at + 1;
        let Some(length) = text[value_start..].iter().position(|&byte| byte == b'\n' || byte == b';') else {
            break;
        };
        if length > VALUE_LIMIT {
            break;
        }
        let end = value_start + length;
        if slot == IS_RESTORED {
            stored = Some(&text[value_start..end]);
        }
        pos = if text[end] == b'\n' { end + 1 } else { line_end(text, end + 1) };
    }
    stored
}

/// Whether the start-up trace-log restore runs: the property must be exactly `"0"` (`0x1403EA2A0`). The seeded
/// default is `"1"`, so only a settings file that says `is_restored=0` asks for it.
pub fn restore_requested(text: &[u8]) -> bool {
    is_restored(text) == Some(b"0")
}
