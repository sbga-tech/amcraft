//! `DisplayLines` (`can.md` 3.1.1): the two lines of a screen with their pending and committed text.

/// Number of lines of a screen (`0x140266220`).
const LINES: usize = 2;
/// A line copy of more than this many UTF-16 units fails and empties the line [display.line_buffer].
const LINE_UNITS: usize = 255;

/// One line: committed text, pending text and the scroll flag of the pending write.
#[derive(Debug, Default, Clone)]
pub(super) struct Line {
    pub current: String,
    pub pending: String,
    pub scroll: bool,
}

/// A screen (A: the payment prompts and the test; B: the e-payment status).
#[derive(Debug, Default)]
pub(super) struct Screen {
    pub lines: [Line; LINES],
    /// The lock a module restart sets while a test runs.
    pub active: bool,
    /// The test owns the screen.
    pub owned: bool,
}

impl Screen {
    /// `0x140266190`: writes `text` into the pending side of `line`, control characters `U+0001..U+001F` become
    /// spaces; text over 255 UTF-16 units empties the line [I].
    pub fn write(&mut self, line: usize, text: &str, scroll: bool) {
        let Some(slot) = self.lines.get_mut(line) else {
            return;
        };
        slot.scroll = scroll;
        if text.encode_utf16().count() > LINE_UNITS {
            slot.pending.clear();
            return;
        }
        slot.pending = text.chars().map(|c| if ('\u{1}'..='\u{1f}').contains(&c) { ' ' } else { c }).collect();
    }

    /// Empties the pending text of every line (scroll off).
    pub fn clear_pending(&mut self) {
        for line in &mut self.lines {
            line.pending.clear();
            line.scroll = false;
        }
    }

    /// Empties the committed and the pending text of every line.
    pub fn clear_all(&mut self) {
        for line in &mut self.lines {
            line.current.clear();
        }
        self.clear_pending();
    }

    /// Whether any line's pending text differs from its committed text.
    pub fn differs(&self) -> bool {
        self.lines.iter().any(|line| line.current != line.pending)
    }

    /// Commits the pending text of every line.
    pub fn commit(&mut self) {
        for line in &mut self.lines {
            line.current.clone_from(&line.pending);
        }
    }
}
