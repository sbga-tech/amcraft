//! Sequence blocks (`ServerData+0x58`, `ClientData+0xBB8`).
//!
//! Evidence: `spec/controllers/error_sequence.md` Part 2 §2 (`error_sequence.sequence.layout.*`).

use num_enum::TryFromPrimitive;
use zero_schema::zero;

use crate::schema::shared::Response;
use crate::schema::{client_block, server_block};

/// Number of players tracked by the daemon (`playerIndex` must be below this).
pub const PLAYER_COUNT: usize = 8;
/// Capacity of the published time histogram (libam width list limit, `0x140213A40`).
pub const HISTOGRAM_CAPACITY: usize = 32;

/// `amdaemon::PlayErrorId` (string `0x1405D65E0`); values are exact, names are inferred.
#[zero(endian = "little")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum PlayErrorId {
    /// The play operation succeeded.
    Success = 0,
    /// `playerIndex >= 8`, or the accounting `valid` flags of begin and end disagree.
    InvalidArgument = 1,
    /// A libam facade is missing, a fatal error is pending, or the player is in the wrong play state.
    InvalidState = 2,
    /// The credit check failed or `libam.credit.pay` failed.
    CreditPay = 3,
    /// libam rejected the ALL.Net begin-accounting call.
    AccountingBegin = 4,
    /// libam rejected the ALL.Net end-accounting call.
    AccountingEnd = 5,
}

/// `sequence.play_accounting_timing` (parser `0x1401CB0B0`): when ALL.Net accounting is reported.
#[zero(endian = "little")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum PlayAccountingTiming {
    /// `"normal"`: begin accounting at play begin, end-then-begin at every continue, end accounting
    /// at play end.
    Normal = 0,
    /// `"begin"`: begin and end accounting are both reported at play begin and at every continue;
    /// nothing at play end.
    Begin = 1,
    /// `"end"`: nothing at play begin; begin and end accounting are both reported at every continue
    /// and at play end (suppressed when the play is canceled).
    End = 2,
}

/// `TimeHistogramItem` (12 bytes): one bucket of the play-time histogram.
#[zero(endian = "little")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeHistogramItem {
    /// Inclusive lower bound in seconds.
    pub lower: u32,
    /// Inclusive upper bound in seconds.
    pub upper: u32,
    /// Number of counted plays inside the bucket.
    pub count: u32,
}

/// `amdaemon::SequenceBookkeeping` (0x19C bytes, alignment 4): libam play statistics in whole seconds.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct SequenceBookkeeping {
    /// Count of counted plays (libam `pub+8`).
    pub number_of_games: u32,
    /// Accumulated total time (libam `pub+0`).
    pub total_time: u32,
    /// Seconds since the session timer was last reset (libam `pub+4`).
    pub play_time: u32,
    /// Mean play duration (libam `pub+12`).
    pub average_play_time: u32,
    /// Longest play duration (libam `pub+16`).
    pub longest_play_time: u32,
    /// Shortest play duration (libam `pub+20`).
    pub shortest_play_time: u32,
    /// Number of valid histogram rows as reported by libam (0..=32).
    pub time_histogram_count: u32,
    /// Histogram rows.
    pub time_histogram: [TimeHistogramItem; HISTOGRAM_CAPACITY],
}

/// `ServerPlayerData` (0xC bytes): per-player play state.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerPlayerData {
    /// `libam.sequence.isPlaying(i)`, re-sampled every update.
    pub playing: bool,
    /// ALL.Net accounting handle + 1; 0 means none.
    pub accounting_handle: u32,
    /// Aime id stored at play begin; `0xFFFFFFFF` once the play stopped.
    pub aime_id: u32,
}

/// `ServerData.sequence`: the executor response, the last play result, libam's bookkeeping and the
/// per-player play state.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerSequence {
    /// Executor response (`checkResult`).
    pub response: Response,
    /// Result of the last play task (written by tasks 1-3 only).
    pub error_id: PlayErrorId,
    /// libam bookkeeping, republished every update.
    pub bookkeeping: SequenceBookkeeping,
    /// Copy of `Manager.test`.
    pub test: bool,
    /// Written only by the config listener.
    pub play_accounting_timing: PlayAccountingTiming,
    /// Per-player state.
    pub players: [ServerPlayerData; PLAYER_COUNT],
}

/// `ClientData.sequence`: the request header and the play payload's bytes.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ClientSequence<'b> {
    /// Request generation (`taskNo`); a request is outstanding while it differs from the response generation.
    pub generation: u32,
    /// Raw [`SequenceTask`].
    pub task: u32,
    /// The play payload (`ClientInfo` union, whose only member is [`PlayInfo`]), decoded by
    /// [`ClientSequenceRef::play_info`].
    pub info: &'b [u8; 0x30],
}

server_block!(sequence, sequence_mut: ServerSequence, ServerSequenceRef, ServerSequenceMut @ 0x58);
client_block!(sequence, sequence_mut: ClientSequence, ClientSequenceRef, ClientSequenceMut @ 0xBB8);

/// `ipc::sequence::Task` (name table `0x1405EEF10` read by `0x14006D980`; values >= 6 have no name).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, TryFromPrimitive)]
#[repr(u32)]
pub enum SequenceTask {
    /// Clear the libam bookkeeping totals.
    Clear = 0,
    /// `beginPlay`.
    PlayBegin = 1,
    /// `continuePlay`.
    PlayContinue = 2,
    /// `endPlay`.
    PlayEnd = 3,
    /// Switch libam into test mode (guarded by `Manager.test`).
    TestBegin = 4,
    /// Switch libam back into game mode (guarded by `Manager.test`).
    TestEnd = 5,
}

impl SequenceTask {
    /// Name printed by the request trace (`0x1405EE898` ... `0x1405EE908`).
    pub const fn name(self) -> &'static str {
        match self {
            Self::Clear => "Clear",
            Self::PlayBegin => "PlayBegin",
            Self::PlayContinue => "PlayContinue",
            Self::PlayEnd => "PlayEnd",
            Self::TestBegin => "TestBegin",
            Self::TestEnd => "TestEnd",
        }
    }

    /// Whether the task carries a [`PlayInfo`] payload (tasks 1-3, the only ones the trace expands).
    pub const fn is_play(self) -> bool {
        matches!(self, Self::PlayBegin | Self::PlayContinue | Self::PlayEnd)
    }
}

/// `ClientPlayAccounting` (0x10 bytes): one ALL.Net accounting record of a play request.
#[zero(endian = "little")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayAccounting {
    /// The record is present.
    pub valid: bool,
    /// Accounting kind code.
    pub kind_code: u32,
    /// Accounting status code.
    pub status_code: u32,
    /// Item count (libam keeps only the low byte at end accounting).
    pub item_count: u32,
}

/// `ClientPlayInfo` (0x30 bytes): the payload of tasks 1-3.
#[zero(endian = "little")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayInfo {
    /// Player index; must be below 8.
    pub player_index: u32,
    /// Credit cost table index; negative means no credit action.
    pub game_cost_index: i16,
    /// Credit cost multiplier; zero means no credit action.
    pub game_cost_count: u16,
    /// Accounting at play begin.
    pub accounting_begin: PlayAccounting,
    /// Accounting at play end.
    pub accounting_end: PlayAccounting,
    /// Aime id; `0` and `0xFFFFFFFF` mean "no Aime".
    pub aime_id: u32,
    /// The play is canceled instead of ended.
    pub cancel: bool,
}

impl<'w> ClientSequenceRef<'w> {
    /// The request's task, `None` for an unknown value (the request fails).
    pub fn task_kind(&self) -> Option<SequenceTask> {
        SequenceTask::try_from(self.task()).ok()
    }

    /// The play payload of tasks 1-3, `None` when a flag byte is neither 0 nor 1 (the request fails).
    pub fn play_info(&self) -> Option<PlayInfo> {
        let info: &'w [u8; 0x30] = self.info();
        PlayInfo::access_prefix(info).ok().map(|(payload, _)| payload.copy_into())
    }
}

const _: () = {
    assert!(TimeHistogramItem::SCHEMA_SIZE == 12);
    assert!(SequenceBookkeeping::SCHEMA_SIZE == 0x19C && SequenceBookkeeping::SCHEMA_ALIGN == 4);
    assert!(ServerPlayerData::SCHEMA_SIZE == 0xC);
    assert!(ServerSequence::SCHEMA_SIZE == 0x210);
    assert!(PlayAccounting::SCHEMA_SIZE == 0x10);
    assert!(PlayInfo::SCHEMA_SIZE == 0x30);
    assert!(ClientSequence::SCHEMA_SIZE == 0x38);
};
