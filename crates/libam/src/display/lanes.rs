//! The e-payment status screen (screen B), `am::display::EpaymentManager` (`can.md` 3.8).

/// The two lanes of the e-payment manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    /// Lane 0: e-money.
    EMoney,
    /// Lane 1: Aime Pay.
    AimePay,
}

impl Lane {
    fn index(self) -> usize {
        match self {
            Self::EMoney => 0,
            Self::AimePay => 1,
        }
    }
}

/// Entries per lane (the record holds 16 `(id, text)` pairs).
const MAX_ENTRIES: usize = 16;
/// The joined brand line is a 128-character buffer (127 units plus the terminator).
const JOIN_UNITS: usize = 127;

const CALL_STAFF_MISMATCH: &str = "筐体情報が一致しません。スタッフをお呼びください";
const CALL_STAFF_FAILED: &str = "支払が失敗しました。スタッフをお呼びください";
const AVAILABLE: &str = "電子決済できます";
const UNAVAILABLE: &str = "電子決済できません";

/// A lane record: its state and its entries.
#[derive(Debug, Default)]
struct LaneRecord {
    state: u32,
    entries: Vec<(u32, String)>,
}

/// Both lanes.
#[derive(Debug, Default)]
pub(super) struct Lanes([LaneRecord; 2]);

/// What screen B shows for the lanes: a blank screen or the two lines.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Rendered {
    /// Line 0 text, always scrolling; `None` for a blank screen.
    pub line0: Option<&'static str>,
    /// Line 1 text (scroll off).
    pub line1: String,
    /// The value of `bMessagesEnabled` after the render.
    pub messages_enabled: bool,
}

impl Lanes {
    /// `0x14027ADA0`: removes every entry of the lane.
    pub fn clear(&mut self, lane: Lane) {
        self.0[lane.index()].entries.clear();
    }

    /// `0x14027ADD0`: adds the entry or replaces the text of the entry with the same `id`; a full lane ignores a
    /// new id.
    pub fn set_entry(&mut self, lane: Lane, id: u32, text: &str) {
        let entries = &mut self.0[lane.index()].entries;
        if let Some(entry) = entries.iter_mut().find(|(known, _)| *known == id) {
            entry.1 = text.to_owned();
        } else if entries.len() < MAX_ENTRIES {
            entries.push((id, text.to_owned()));
        }
    }

    /// Stores the state of the lane (the caller then renders).
    pub fn set_state(&mut self, lane: Lane, state: u32) {
        self.0[lane.index()].state = state;
    }

    /// `0x14027AE40` rendering: the effective state is the state of the first lane whose state is above 1
    /// (unsigned), else `state`.
    pub fn render(&self, state: u32) -> Rendered {
        let effective = self.0.iter().map(|lane| lane.state).find(|&s| s > 1).unwrap_or(state);
        let blank = Rendered { line0: None, line1: String::new(), messages_enabled: true };
        match effective {
            2 => Rendered { line0: Some(CALL_STAFF_MISMATCH), ..blank },
            3 => Rendered { line0: Some(CALL_STAFF_FAILED), ..blank },
            0 | 1 => self.render_availability(blank),
            _ => blank,
        }
    }

    fn render_availability(&self, blank: Rendered) -> Rendered {
        let available: Vec<&LaneRecord> = self.0.iter().filter(|lane| lane.state == 1).collect();
        if available.is_empty() {
            return blank;
        }
        let count: usize = available.iter().map(|lane| lane.entries.len()).sum();
        if count == 0 {
            return Rendered { line0: Some(UNAVAILABLE), ..blank };
        }
        let separator = " ".repeat((20 / count).max(6));
        let mut joined = String::new();
        for (_, text) in available.iter().flat_map(|lane| lane.entries.iter()) {
            joined.push_str(text);
            joined.push_str(&separator);
        }
        if joined.encode_utf16().count() > JOIN_UNITS {
            joined.clear();
        }
        Rendered { line0: Some(AVAILABLE), line1: joined, messages_enabled: false }
    }
}
