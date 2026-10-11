//! The alpbEx state manager (`allnet.md` part 8): the foreground exec status machine, the credit list and the
//! play-count and trace-log checks that decide whether play continues, goes offline or stops.

use std::net::Ipv4Addr;

use super::driver::{Counters, DriverDongle};
use super::records::{Identity, RECORD_CAPACITY, Record, Stamp, message_record, play_record};

/// Largest kind code (`0x28E`).
pub const MAX_KIND: u32 = 654;

/// Exec status values (`alpbExGetExecStatus`).
pub mod status {
    /// `STATUS_NG`.
    pub const NG: i32 = -1;
    /// Before `Initialize`.
    pub const UNINITIALIZED: i32 = -2;
    /// `AR Wait`.
    pub const AR_WAIT: i32 = 1;
    /// `AR Ready`.
    pub const AR_READY: i32 = 2;
    /// `AR Busy`.
    pub const AR_BUSY: i32 = 3;
    /// Initialising.
    pub const INITIALIZING: i32 = 4;
    /// `StartPlay`.
    pub const START_PLAY: i32 = 5;
    /// `EndPlay`.
    pub const END_PLAY: i32 = 6;
    /// Playing.
    pub const PLAYING: i32 = 7;
    /// `Game Test Ready`.
    pub const TEST_READY: i32 = 8;
    /// Item record being written.
    pub const ITEM: i32 = 9;
    /// `AR Wait Offline`.
    pub const OFFLINE: i32 = 10;
}

/// Writes records into the keychip trace log and numbers them (`rn`, bytes 56..60).
#[derive(Debug, Default)]
pub struct TraceWriter {
    next: u32,
}

impl TraceWriter {
    /// Continues numbering after the records already in the log.
    pub fn resume(&mut self, dongle: &mut DriverDongle) {
        self.next = dongle
            .trace_records()
            .last()
            .map_or(0, |record| u32::from_le_bytes([record[56], record[57], record[58], record[59]]).wrapping_add(1));
    }

    /// Appends `record`; `false` when the log is full.
    pub fn put(&mut self, dongle: &mut DriverDongle, mut record: Record) -> bool {
        record[56..60].copy_from_slice(&self.next.to_le_bytes());
        let stored = dongle.put_trace(record);
        if stored {
            self.next = self.next.wrapping_add(1);
        }
        stored
    }
}

/// Everything the state manager reads or writes outside itself in one tick.
pub struct Env<'a> {
    /// The keychip.
    pub dongle: &'a mut DriverDongle,
    /// The trace-log record numbering.
    pub trace: &'a mut TraceWriter,
    /// Keychip, board and game identity.
    pub identity: &'a Identity,
    /// The tenpo address.
    pub tenpo: Option<Ipv4Addr>,
    /// The local time of this frame.
    pub stamp: Stamp,
    /// The server clock is synchronised.
    pub synced: bool,
    /// Accounting reporter status (`AR+576`).
    pub ar_status: i32,
    /// The first accounting report finished since boot.
    pub first_auth_done: bool,
    /// Operating mode, 1 or 2.
    pub operating_mode: u32,
    /// The software near-full property is not `0`.
    pub near_full_enabled: bool,
    /// The report connection failed (`TermDateError`).
    pub conn_failed: bool,
}

impl Env<'_> {
    fn counters(&mut self) -> Option<Counters> {
        self.dongle.counters()
    }

    fn strict(&self, counters: Counters) -> bool {
        counters.billing_type != 0 || self.operating_mode == 2
    }

    /// Play-count manager `GetStatus` (`0x1403E6690`).
    fn play_status(&mut self) -> i32 {
        let Some(counters) = self.counters() else { return 1 };
        if !self.first_auth_done {
            return 0;
        }
        if counters.play_limit == 0 {
            return -2;
        }
        let strict = self.strict(counters);
        let free = i64::from(counters.play_limit) - i64::from(counters.play_count);
        if free <= 0 {
            return if strict { 6 } else { 7 };
        }
        if free < i64::from(counters.near_full) {
            return if strict { 4 } else { 5 };
        }
        0
    }

    /// Trace-log manager status (`0x1403E9AD0`) with the same threshold applied to the free record count.
    fn trace_status(&mut self) -> i32 {
        let Some(counters) = self.counters() else { return -1 };
        let strict = self.strict(counters);
        let free = RECORD_CAPACITY.saturating_sub(self.dongle.trace_records().len());
        if free == 0 {
            return if strict { 6 } else { 7 };
        }
        if free < usize::from(counters.near_full) {
            return if strict { 4 } else { 5 };
        }
        0
    }

    fn tenpo_bytes(&self) -> [u8; 4] {
        self.tenpo.map_or([255; 4], |ip| ip.octets())
    }

    fn put_play(&mut self, code: u16, count: u8, player: u8) -> Option<Record> {
        let counters = self.counters()?;
        let record = play_record(
            self.identity,
            self.stamp,
            self.synced,
            counters.play_count,
            counters.play_limit,
            self.tenpo_bytes(),
            code,
            count,
            player,
        );
        self.trace.put(self.dongle, record).then_some(record)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    PlayCounter,
    TraceLog,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ItemMode {
    #[default]
    Item,
    GeneralId,
}

/// The state manager.
#[derive(Debug)]
pub struct PlayBook {
    initialised: bool,
    state: i32,
    err_no: u32,
    test_mode: bool,
    /// Started credits: `(handle, packed code)`.
    credits: Vec<(u32, u32)>,
    next_handle: u32,
    end_code: u32,
    item_code: u32,
    item_mode: ItemMode,
    words: Vec<u32>,
    word_index: usize,
    remaining: u8,
    play_full: bool,
    no_server_time: bool,
    tenpo_invalid: bool,
    check: Option<Stage>,
    game_test_log: u8,
}

impl Default for PlayBook {
    fn default() -> Self {
        Self {
            initialised: false,
            state: status::UNINITIALIZED,
            err_no: 0,
            test_mode: false,
            credits: Vec::new(),
            next_handle: 0,
            end_code: 0,
            item_code: 0,
            item_mode: ItemMode::Item,
            words: Vec::new(),
            word_index: 0,
            remaining: 0,
            play_full: false,
            no_server_time: false,
            tenpo_invalid: false,
            check: None,
            game_test_log: 0,
        }
    }
}

/// The packed credit code: `kind * 100 + status`, item count and 1-based player.
pub fn pack_code(code: u16, count: u8, player: u8) -> u32 {
    u32::from(code) | (u32::from(count) << 16) | (u32::from(player) << 24)
}

impl PlayBook {
    /// `StateManager.Initialize`: INITIALIZING.
    pub fn initialize(&mut self, test_mode: bool) {
        *self = Self { initialised: true, state: status::INITIALIZING, test_mode, ..Self::default() };
    }

    /// `alpbExGetExecStatus`.
    pub fn exec_status(&self) -> i32 {
        if self.initialised { self.state } else { status::UNINITIALIZED }
    }

    /// `alpbExGetDisplayErrorNo`.
    pub fn display_error(&self) -> u32 {
        self.err_no
    }

    /// A credit has been started and not ended (the game is playing).
    pub fn is_playing(&self) -> bool {
        !self.credits.is_empty()
    }

    /// `alpbExGetOfflineCauseErrorNo`.
    pub fn offline_cause(&self, conn_failed: bool) -> u32 {
        if self.state != status::OFFLINE {
            0
        } else if conn_failed {
            8111
        } else if self.play_full {
            8103
        } else if self.tenpo_invalid {
            8005
        } else {
            8114
        }
    }

    /// `alpbExGetLogFullStatus` over the state manager's own latch.
    pub fn game_test_log_full(&self) -> bool {
        self.game_test_log == 2
    }

    /// `alpbExSetHostIpAddress` kind 0 / 2: the tenpo address is valid / invalid.
    pub fn set_tenpo_invalid(&mut self, invalid: bool) {
        self.tenpo_invalid = invalid;
    }

    fn started(&self, player: u8) -> bool {
        self.credits.iter().any(|&(_, code)| (code >> 24) as u8 == player)
    }

    /// `alpbExStartCredit`: the credit handle, 0 on any failure.
    pub fn start_credit(&mut self, first_auth_done: bool, code: u32) -> u32 {
        let low = code & 0xFFFF;
        if !self.initialised
            || !first_auth_done
            || low / 100 > MAX_KIND
            || (self.state != status::AR_WAIT && self.state != status::PLAYING)
            || self.credits.iter().any(|&(_, existing)| existing >> 24 == code >> 24 && existing & 0xFFFF == low)
        {
            return 0;
        }
        self.next_handle += 1;
        let handle = self.next_handle;
        self.credits.push((handle, code));
        self.end_code = code;
        self.state = status::START_PLAY;
        handle
    }

    /// `alpbExEndCredit`.
    pub fn end_credit(&mut self, handle: u32, code: u32) -> i32 {
        if handle == 0 {
            return -3;
        }
        if !self.initialised {
            return -2;
        }
        if self.state != status::PLAYING {
            return -4;
        }
        if (code & 0xFFFF) / 100 > MAX_KIND {
            return -3;
        }
        if !self.credits.iter().any(|&(existing, _)| existing == handle) {
            return -5;
        }
        self.credits.retain(|&(existing, _)| existing != handle);
        self.end_code = code;
        self.state = status::END_PLAY;
        0
    }

    /// `alpbExItemAccount`.
    pub fn item_account(&mut self, code: u32) -> i32 {
        if !self.initialised {
            return -2;
        }
        if self.state != status::PLAYING {
            return -4;
        }
        if (code & 0xFFFF) / 100 > MAX_KIND {
            return -3;
        }
        if !self.started((code >> 24) as u8) {
            return -6;
        }
        self.item_code = code;
        self.item_mode = ItemMode::Item;
        self.state = status::ITEM;
        0
    }

    /// `alpbExPutGeneralID`.
    pub fn put_general_id(&mut self, kind: u32, player: u8, digits: &str) -> i32 {
        if !self.initialised {
            return -2;
        }
        if self.state != status::PLAYING {
            return -4;
        }
        if kind > MAX_KIND {
            return -3;
        }
        if !self.started(player) {
            return -6;
        }
        if digits.is_empty() || digits.len() > 2048 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return -3;
        }
        let padded = format!("{}{digits}", "0".repeat((8 - digits.len() % 8) % 8));
        self.words = padded
            .as_bytes()
            .chunks(8)
            .map(|group| {
                std::str::from_utf8(group).ok().and_then(|text| u32::from_str_radix(text, 16).ok()).unwrap_or(0)
            })
            .collect();
        self.word_index = 0;
        self.remaining = u8::try_from(self.words.len() - 1).unwrap_or(u8::MAX);
        self.item_code = pack_code((kind * 100 + 8) as u16, self.remaining, player);
        self.item_mode = ItemMode::GeneralId;
        self.state = status::ITEM;
        0
    }

    /// `alpbExCancelPlay`.
    pub fn cancel_play(&mut self, env: &mut Env<'_>) -> i32 {
        if !self.initialised {
            return -2;
        }
        match self.state {
            -2 | status::INITIALIZING => -2,
            status::NG | status::START_PLAY | status::END_PLAY | status::ITEM => -4,
            status::PLAYING => {
                let mut message = format!("Cancel {}", env.identity.game_id);
                for &(_, code) in &self.credits {
                    message.push_str(&format!(" P{}", code >> 24));
                }
                message.truncate(32);
                let record = message_record(env.identity, env.stamp, env.synced, &message);
                env.trace.put(env.dongle, record);
                self.credits.clear();
                self.state = status::END_PLAY;
                self.check = Some(Stage::TraceLog);
                0
            }
            _ => 0,
        }
    }

    fn fail(&mut self, err_no: u32) {
        self.err_no = err_no;
        self.check = None;
        self.state = status::NG;
    }

    fn run_check(&mut self) {
        self.check = Some(Stage::PlayCounter);
        self.play_full = false;
        self.no_server_time = false;
    }

    /// One frame (`0x1403D5B30`).
    pub fn tick(&mut self, env: &mut Env<'_>) {
        if !self.initialised || self.state == status::NG {
            return;
        }
        match self.check {
            Some(Stage::TraceLog) => self.trace_check(env),
            Some(Stage::PlayCounter) => self.play_check(env),
            None => self.dispatch(env),
        }
    }

    fn play_check(&mut self, env: &mut Env<'_>) {
        match env.play_status() {
            -2 => self.fail(8111),
            -1 => self.fail(949),
            0 | 5 => self.check = Some(Stage::TraceLog),
            1 => {}
            4 => {
                if env.near_full_enabled && self.state == status::AR_BUSY {
                    self.fail(8106);
                } else {
                    self.check = Some(Stage::TraceLog);
                }
            }
            6 => self.fail(8103),
            7 => {
                self.play_full = true;
                self.check = Some(Stage::TraceLog);
            }
            _ => self.fail(800),
        }
    }

    fn trace_check(&mut self, env: &mut Env<'_>) {
        match env.trace_status() {
            -1 => self.fail(949),
            0 | 5 => self.proceed(env),
            1 => {}
            4 => {
                if env.near_full_enabled && self.state == status::AR_BUSY {
                    self.fail(8106);
                } else {
                    self.proceed(env);
                }
            }
            6 => {
                if self.test_mode {
                    self.check = None;
                    self.game_test_log = 2;
                } else {
                    self.fail(8103);
                }
            }
            7 => {
                if self.test_mode {
                    self.check = None;
                    self.game_test_log = 2;
                } else {
                    self.play_full = true;
                    self.proceed(env);
                }
            }
            _ => self.fail(800),
        }
    }

    fn proceed(&mut self, env: &mut Env<'_>) {
        self.check = None;
        self.no_server_time = false;
        if !self.test_mode {
            if !env.synced {
                self.no_server_time = true;
                let strict = env.counters().is_some_and(|counters| env.strict(counters));
                if strict {
                    self.fail(8114);
                    return;
                }
            }
            if env.conn_failed {
                self.no_server_time = true;
            }
        }
        let flags = self.play_full || self.no_server_time || self.tenpo_invalid;
        match self.state {
            status::AR_READY => {}
            status::AR_BUSY => self.state = if flags { status::OFFLINE } else { status::AR_WAIT },
            status::START_PLAY | status::ITEM => self.state = status::PLAYING,
            status::END_PLAY => {
                self.state = if self.is_playing() {
                    status::PLAYING
                } else if flags {
                    status::OFFLINE
                } else {
                    status::AR_WAIT
                };
            }
            status::TEST_READY => self.game_test_log = 1,
            _ => {
                if flags {
                    self.state = status::OFFLINE;
                }
            }
        }
    }

    fn dispatch(&mut self, env: &mut Env<'_>) {
        let ar = env.ar_status;
        let ready = if self.test_mode { status::TEST_READY } else { status::AR_READY };
        match self.state {
            status::INITIALIZING => match ar {
                1 => self.state = ready,
                4 if self.test_mode => self.state = ready,
                4 | 5 => {}
                -8 => self.fail(27),
                -7 => self.fail(8113),
                -2 => self.fail(949),
                -9 => self.fail(84),
                -801 => self.fail(801),
                -12 => self.fail(8008),
                _ => self.fail(800),
            },
            status::AR_WAIT => match ar {
                1 => self.state = ready,
                3 | 4 => {}
                -801 => self.fail(801),
                -12 => self.fail(8008),
                -9 => self.fail(84),
                _ => self.fail(800),
            },
            status::AR_READY => match ar {
                1 => {}
                2 => self.state = status::AR_BUSY,
                4 => self.state = status::AR_WAIT,
                -12 => self.fail(8008),
                _ => self.fail(800),
            },
            status::AR_BUSY => match ar {
                2 | 1 | 4 => {}
                3 => self.run_check(),
                -11 => self.fail(8114),
                -10 => self.fail(8111),
                -9 => self.fail(84),
                -7 => self.fail(8113),
                -2 => self.fail(949),
                -12 => self.fail(8008),
                -802 => self.fail(802),
                -801 => self.fail(801),
                _ => self.fail(800),
            },
            status::START_PLAY => self.start_play(env),
            status::END_PLAY => self.end_play(env),
            status::ITEM => self.item(env),
            status::OFFLINE => match ar {
                1 => self.state = ready,
                2 | 3 => {}
                4 | 5 => {
                    if !env.conn_failed && !self.play_full && !self.tenpo_invalid && !env.synced {
                        self.no_server_time = true;
                    }
                    self.state = status::AR_WAIT;
                }
                -801 => self.fail(801),
                -9 => self.fail(84),
                _ => self.fail(800),
            },
            _ => {}
        }
    }

    fn start_play(&mut self, env: &mut Env<'_>) {
        match env.play_status() {
            6 => self.fail(8103),
            -1 => self.fail(949),
            7 => self.run_check(),
            1 => {}
            _ => {
                let code = self.end_code;
                if env.put_play(code as u16, (code >> 16) as u8, (code >> 24) as u8).is_some() {
                    self.run_check();
                }
            }
        }
    }

    fn end_play(&mut self, env: &mut Env<'_>) {
        let count = (self.end_code >> 16) as u8;
        match env.play_status() {
            6 => self.fail(8103),
            -1 => self.fail(949),
            1 => {}
            7 => {
                if count != 0 {
                    self.play_full = true;
                    self.check = Some(Stage::PlayCounter);
                    self.no_server_time = false;
                }
            }
            _ => {
                let code = self.end_code;
                if env.put_play(code as u16, count, (code >> 24) as u8).is_some() {
                    if count != 0 {
                        env.dongle.add_play_count(1);
                    }
                    self.run_check();
                }
            }
        }
    }

    fn item(&mut self, env: &mut Env<'_>) {
        match self.item_mode {
            ItemMode::Item => {
                let code = self.item_code;
                // A full trace log answers -2: the check runs anyway and decides about offline play.
                env.put_play(code as u16, (code >> 16) as u8, (code >> 24) as u8);
                self.run_check();
            }
            ItemMode::GeneralId => {
                let word = self.words.get(self.word_index).copied().unwrap_or(0);
                let code = self.item_code;
                let status_code = (code as u16).wrapping_add(u16::from(self.word_index > 0));
                if let Some(mut record) = env.put_play_record(status_code, self.remaining, (code >> 24) as u8, word) {
                    record[51..55].copy_from_slice(&word.to_le_bytes());
                }
                if self.remaining == 0 {
                    self.run_check();
                } else {
                    self.word_index += 1;
                    self.remaining -= 1;
                }
            }
        }
    }
}

impl Env<'_> {
    /// A general-ID record: a play record whose bytes 51..55 carry the 8-digit word.
    fn put_play_record(&mut self, code: u16, count: u8, player: u8, word: u32) -> Option<Record> {
        let counters = self.counters()?;
        let mut record = play_record(
            self.identity,
            self.stamp,
            self.synced,
            counters.play_count,
            counters.play_limit,
            self.tenpo_bytes(),
            code,
            count,
            player,
        );
        record[51..55].copy_from_slice(&word.to_le_bytes());
        self.trace.put(self.dongle, record).then_some(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::keychip::SimKeychip;

    fn ready_dongle() -> (SimKeychip, DriverDongle) {
        let sim = SimKeychip::new();
        let mut dongle = DriverDongle::default();
        dongle.attach(&sim);
        let start = std::time::Instant::now();
        while dongle.counters().is_none() {
            assert!(start.elapsed() < std::time::Duration::from_secs(2), "keychip setup");
        }
        (sim, dongle)
    }

    fn env<'a>(dongle: &'a mut DriverDongle, trace: &'a mut TraceWriter, identity: &'a Identity, ar: i32) -> Env<'a> {
        Env {
            dongle,
            trace,
            identity,
            tenpo: None,
            stamp: Stamp::default(),
            synced: true,
            ar_status: ar,
            first_auth_done: true,
            operating_mode: 1,
            near_full_enabled: true,
            conn_failed: false,
        }
    }

    #[test]
    fn initialising_waits_for_the_reporter_then_becomes_ready() {
        let (_sim, mut dongle) = ready_dongle();
        let (mut trace, identity) = (TraceWriter::default(), Identity::default());
        let mut book = PlayBook::default();
        book.initialize(false);
        book.tick(&mut env(&mut dongle, &mut trace, &identity, 5));
        assert_eq!(book.exec_status(), status::INITIALIZING);
        book.tick(&mut env(&mut dongle, &mut trace, &identity, 1));
        assert_eq!(book.exec_status(), status::AR_READY);
    }

    #[test]
    fn credit_goes_start_play_then_playing_and_end_increments_the_play_count() {
        let (sim, mut dongle) = ready_dongle();
        let (mut trace, identity) = (TraceWriter::default(), Identity::default());
        let mut book = PlayBook::default();
        book.initialize(false);
        book.state = status::AR_WAIT;
        let handle = book.start_credit(true, pack_code(1001, 0, 1));
        assert_ne!(handle, 0);
        assert_eq!(book.exec_status(), status::START_PLAY);
        for _ in 0..3 {
            book.tick(&mut env(&mut dongle, &mut trace, &identity, 1));
        }
        assert_eq!(book.exec_status(), status::PLAYING);
        assert_eq!(book.end_credit(handle, pack_code(1001, 1, 1)), 0);
        for _ in 0..3 {
            book.tick(&mut env(&mut dongle, &mut trace, &identity, 1));
        }
        assert_eq!(book.exec_status(), status::AR_WAIT);
        assert_eq!(sim.play_count(), 1);
        assert_eq!(sim.trace_records().len(), 2);
    }

    #[test]
    fn a_second_credit_with_the_same_player_and_code_is_refused() {
        let mut book = PlayBook::default();
        book.initialize(false);
        book.state = status::PLAYING;
        book.credits.push((1, pack_code(1001, 0, 1)));
        assert_eq!(book.start_credit(true, pack_code(1001, 0, 1)), 0);
        assert_ne!(book.start_credit(true, pack_code(1001, 0, 2)), 0);
    }

    #[test]
    fn full_play_limit_in_the_lenient_mode_goes_offline() {
        let (sim, mut dongle) = ready_dongle();
        sim.set_play_count(1000);
        let (mut trace, identity) = (TraceWriter::default(), Identity::default());
        let mut book = PlayBook::default();
        book.initialize(false);
        book.state = status::AR_BUSY;
        for _ in 0..4 {
            book.tick(&mut env(&mut dongle, &mut trace, &identity, 3));
        }
        assert_eq!(book.exec_status(), status::OFFLINE);
        assert_eq!(book.offline_cause(false), 8103);
    }

    #[test]
    fn full_play_limit_in_the_strict_mode_stops_with_8103() {
        let (sim, mut dongle) = ready_dongle();
        sim.set_play_count(1000);
        sim.set_billing_mode(true);
        let (mut trace, identity) = (TraceWriter::default(), Identity::default());
        let mut book = PlayBook::default();
        book.initialize(false);
        book.state = status::AR_BUSY;
        book.tick(&mut env(&mut dongle, &mut trace, &identity, 3));
        book.tick(&mut env(&mut dongle, &mut trace, &identity, 3));
        assert_eq!(book.exec_status(), status::NG);
        assert_eq!(book.display_error(), 8103);
    }

    #[test]
    fn general_id_pads_to_words_of_eight_digits() {
        let mut book = PlayBook::default();
        book.initialize(false);
        book.state = status::PLAYING;
        book.credits.push((1, pack_code(1001, 0, 1)));
        assert_eq!(book.put_general_id(10, 1, "123456789"), 0);
        assert_eq!(book.words, vec![0x1, 0x2345_6789]);
        assert_eq!(book.remaining, 1);
        assert_eq!(book.put_general_id(10, 1, "12a"), -4);
    }
}
