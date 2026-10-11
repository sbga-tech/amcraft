//! Console printing helper `0x1400054B0(level, fmt, ...)`: level 1 goes to stdout, every other level to
//! stderr, followed by `fflush`.
//!
//! The binary is a GUI-subsystem program: unless a console was allocated, the text goes nowhere. Write errors
//! (including a missing standard handle) are ignored, as `vfwprintf_s` results are in the original.

use std::io::Write;

/// Prints to stdout (`level == 1`).
pub(crate) fn print_stdout(text: &str) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
}

/// Prints to stderr (every level other than 1).
pub(crate) fn print_stderr(text: &str) {
    let mut err = std::io::stderr().lock();
    let _ = err.write_all(text.as_bytes());
    let _ = err.flush();
}
