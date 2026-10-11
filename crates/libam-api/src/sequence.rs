//! `am::sequence`: the libam main-flow state machine.

use crate::result::AmResult;
use crate::time::Tm;

/// libam `am::sequence::ProcessState` as delivered to `Manager::onChangeState` (`0x14000AED0`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProcessState(pub u32);

impl ProcessState {
    /// Name logged as `Process state: <name>` (`0x14000AED0`).
    pub const fn name(self) -> &'static str {
        match self.0 {
            1 => "setup Game Mode",
            2 => "setup Test Mode",
            3 => "Advertise",
            4 => "Play",
            5 => "Test Mode",
            _ => "Unknown",
        }
    }
}

/// `libam.credit.check` result meaning "the credit is available".
pub const CREDIT_AFFORDABLE: u32 = 2;
/// Credit configuration rule for which the Aime play log carries the spent credit (`credit cfg+8 == 1`).
pub const CREDIT_RULE_AIME_AMOUNT: u32 = 1;

/// `AimeLogStatus` as passed to `sendAimeLog` (`0x140045100`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AimeLogStatus {
    /// Play begin (`0`).
    Begin,
    /// Play continue (`1`).
    Continue,
    /// Play end (`2`).
    End,
}

impl AimeLogStatus {
    /// libam log status the daemon passes to `aime.log` (`{0→1, 1→2, 2→3, 3→4, else→0}`, also used by
    /// `AimeRequestExecutor::accept` `0x1400136F0`).
    pub const fn libam_code(self) -> u32 {
        match self {
            Self::Begin => 1,
            Self::Continue => 2,
            Self::End => 3,
        }
    }
}

/// The six totals of libam's published bookkeeping object (`ctx+0x20F08`, accessors `0x1402261C0` …
/// `0x140226210`), in whole seconds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BookkeepingTotals {
    /// `pub+8` (`0x1402261E0`).
    pub number_of_games: u32,
    /// `pub+0` (`0x1402261C0`).
    pub total_time: u32,
    /// `pub+4` (`0x1402261D0`).
    pub play_time: u32,
    /// `pub+12` (`0x1402261F0`).
    pub average_play_time: u32,
    /// `pub+16` (`0x140226200`).
    pub longest_play_time: u32,
    /// `pub+20` (`0x140226210`).
    pub shortest_play_time: u32,
}

/// One `sequence.histogram_rule` entry (`{u32 width; u32 count}`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HistogramRule {
    /// Bucket width in seconds.
    pub width: u32,
    /// Number of consecutive buckets of that width.
    pub count: u32,
}

/// One row of libam's play-time histogram (`get(i)`, `0x140226020`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HistogramRow {
    /// Inclusive lower bound in seconds.
    pub lower: u32,
    /// Inclusive upper bound in seconds.
    pub upper: u32,
    /// Number of counted plays inside the bucket.
    pub count: u32,
}

/// `sequence.play_accounting_timing` (parser `0x1401CB0B0`): when ALL.Net accounting is reported.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(u32)]
pub enum PlayAccountingTiming {
    /// `"normal"`: begin accounting at play begin, end-then-begin at every continue, end accounting
    /// at play end.
    #[default]
    Normal = 0,
    /// `"begin"`: begin and end accounting are both reported at play begin and at every continue;
    /// nothing at play end.
    Begin = 1,
    /// `"end"`: nothing at play begin; begin and end accounting are both reported at every continue
    /// and at play end (suppressed when the play is canceled).
    End = 2,
}

/// `process::Config` section `sequence` (`Config+0x4ED60`, 0x104 bytes).
///
/// The controller only consumes [`Self::play_accounting_timing`] (`SequenceController` config listener
/// `0x1400468E0`); the other members feed libam through `SequenceConverter` (`0x140011B20`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SequenceConfig {
    /// `+0x00` key `play_accounting_timing`.
    pub play_accounting_timing: PlayAccountingTiming,
    /// `+0x04` key `save_total_time_on_playing`.
    pub save_total_time_on_playing: bool,
    /// `+0x08` key `histogram_rule` (at most 31 entries, total `count` at most 31).
    pub histogram_rules: Vec<HistogramRule>,
}

/// libam surface used by the daemon's `SequenceController`: the `Manager+8` facades error (`0x140218530`), credit
/// (`0x140218B70`), sequence (`0x140218C10` / const `0x140218C60`), ALL.Net accounting (`0x1402191B0`) and Aime
/// log (`0x140219390`), plus the wall clock the Aime log is stamped with (`DateTime::now(local)`, `0x14005D0F0`).
pub trait SequencePort {
    /// Error facade accessor succeeded (status OK and non-null).
    fn error_facade_available(&self) -> bool;
    /// Credit facade accessor succeeded.
    fn credit_facade_available(&self) -> bool;
    /// Sequence facade accessor succeeded (both the mutable and the const accessor).
    fn sequence_facade_available(&self) -> bool;
    /// ALL.Net accounting facade accessor succeeded.
    fn accounting_facade_available(&self) -> bool;
    /// Aime log facade accessor succeeded.
    fn aime_facade_available(&self) -> bool;

    /// `error.current()` (`0x140216990`) has a non-zero number: a fatal error is pending.
    fn fatal_error_pending(&self) -> bool;

    /// `sequence.isPlaying(i)` (`0x140217900` → `0x140236140`); `false` for `i >= 8`.
    fn is_playing(&self, index: u32) -> bool;
    /// `sequence.beginPlay(i)` (`0x140217760` → `0x140253450`): fails if already playing, an error is pending,
    /// libam is in state 2/5, or `i` is beyond the configured player count.
    fn begin_play(&mut self, index: u32) -> AmResult;
    /// `sequence.continuePlay(i)` (`0x1402177B0` → `0x1402534F0`): fails (status 6) if the player is not playing.
    fn continue_play(&mut self, index: u32) -> AmResult;
    /// `sequence.endPlay(i)` (`0x140217820` → `0x140253560`).
    fn end_play(&mut self, index: u32) -> AmResult;
    /// `sequence.cancelPlay(i)` (`0x140217890` → `0x1402535D0`).
    fn cancel_play(&mut self, index: u32) -> AmResult;
    /// `sequence.clearBookkeeping()` (`0x140217930` → `0x140235DF0`).
    fn clear_bookkeeping(&mut self);
    /// `sequence.toggleMode()` (`0x140217720` → `0x140253310`): game ↔ test mode switch.
    fn toggle_mode(&mut self);

    /// The six published totals of `sequence.bookkeeping()` (`0x140217950`).
    fn bookkeeping_totals(&self) -> BookkeepingTotals;
    /// Histogram `count()` (`0x140226010`); libam guarantees `<= 32`.
    fn histogram_count(&self) -> u32;
    /// Histogram `get(i)` (`0x140226020`): `{lo, hi, count}` of row `i`.
    fn histogram_row(&self, index: u32) -> AmResult<HistogramRow>;

    /// `credit.check(player, costIndex, costCount)` (`0x140216360` → `0x14022BD20`): [`CREDIT_AFFORDABLE`] iff
    /// the credit is available.
    fn credit_check(&self, player: u32, cost_index: i16, cost_count: u16) -> AmResult<u32>;
    /// `credit.pay(player, costIndex, costCount)` (`0x1402161B0` → `0x140253640`): succeeds only in libam state
    /// 3/4 without a pending error.
    fn credit_pay(&mut self, player: u32, cost_index: i16, cost_count: u16) -> AmResult;
    /// `credit.config().rule` (`0x14021D1C0`, config `0x140216300`).
    fn credit_rule(&self) -> u32;
    /// `credit.config().cost(i)` (`0x14021D290`): status 1 for `i >= 8`.
    fn credit_cost(&self, cost_index: i16) -> AmResult<u32>;

    /// `allnet.accountingAvailable()` (`0x140215C10` → `0x14023D470`): `false` while ALL.Net reports.
    fn accounting_available(&self) -> bool;
    /// `allnet.beginAccounting(player, kind, status)` (`0x140215830` → `0x14023C770`): the handle table slot.
    fn begin_accounting(&mut self, player: u32, kind_code: u32, status_code: u32) -> AmResult<u32>;
    /// `allnet.endAccounting(player, kind, status, itemCount, handle)` (`0x1402158E0` → `0x14023C880`); libam
    /// keeps only the low byte of `item_count`.
    fn end_accounting(
        &mut self,
        player: u32,
        kind_code: u32,
        status_code: u32,
        item_count: u32,
        handle: u32,
    ) -> AmResult;

    /// Local wall-clock time (`DateTime::now(1)` converted by `0x1400457A0`; `wday`, `yday` and `isdst` are zero).
    fn local_time(&self) -> Tm;
    /// `aime.log(status, AimeId{id, 0}, tm, credit)` (`0x140215650` → `0x14022A0D0`).
    fn aime_log(&mut self, status: AimeLogStatus, aime_id: u32, time: &Tm, credit: u32) -> AmResult;
}
