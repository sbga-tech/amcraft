//! The bookkeeping data of the sequence manager: the working totals block (`sequence.md` 2.5) and the published
//! object (2.6) with the flush, publish and rebuild rules of 3.3 and 3.4.

use libam_api::sequence::{BookkeepingTotals, HistogramRow};

/// 32-bit words of the working block.
const WORDS: usize = 71;
/// Bytes of the working block inside a PlayInfo image.
pub(super) const WORK_BYTES: usize = WORDS * 4;
/// Where the working block starts inside the 512-byte image.
pub(super) const WORK_OFFSET: usize = 8;
/// Maximum number of histogram rows.
const MAX_ROWS: usize = 32;

const TOTAL_TIME: usize = 0;
const PLAY_TIME: usize = 1;
const GAMES: usize = 2;
const SUM_DURATION: usize = 3;
const LONGEST: usize = 4;
const SHORTEST: usize = 5;
const BUCKETS: usize = 6;

/// Seconds between two timer readings in milliseconds (`0x1402C7FD0`, whole seconds).
pub(super) fn elapsed(from_ms: u64, to_ms: u64) -> u32 {
    // Truncation to 32 bits is the original's unsigned arithmetic.
    (to_ms.saturating_sub(from_ms) / 1000) as u32
}

/// The working totals block (`seq+0x6F0`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Work([u32; WORDS]);

impl Default for Work {
    /// The default block (`0x14025A7C0`): zero except the shortest play time, `0xFFFFFFFF`.
    fn default() -> Self {
        let mut words = [0; WORDS];
        words[SHORTEST] = u32::MAX;
        Self(words)
    }
}

impl Work {
    /// The block stored at `+8` of a PlayInfo image.
    pub(super) fn from_image(image: &[u8]) -> Self {
        let mut words = [0; WORDS];
        let payload = image.get(WORK_OFFSET..WORK_OFFSET + WORK_BYTES).unwrap_or(&[]);
        for (word, bytes) in words.iter_mut().zip(payload.chunks_exact(4)) {
            *word = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        }
        Self(words)
    }

    /// Writes the block into a PlayInfo image.
    pub(super) fn write_into(&self, image: &mut [u8]) {
        if let Some(payload) = image.get_mut(WORK_OFFSET..WORK_OFFSET + WORK_BYTES) {
            for (bytes, word) in payload.chunks_exact_mut(4).zip(self.0) {
                bytes.copy_from_slice(&word.to_le_bytes());
            }
        }
    }

    fn bucket_count(&self) -> usize {
        (self.0[BUCKETS] as usize).min(MAX_ROWS)
    }

    fn width(&self, bucket: usize) -> u32 {
        self.0[7 + 2 * bucket]
    }

    fn hits(&self, bucket: usize) -> u32 {
        self.0[8 + 2 * bucket]
    }

    fn hits_mut(&mut self, bucket: usize) -> &mut u32 {
        &mut self.0[8 + 2 * bucket]
    }

    /// Restores the bucket count and the widths of the default block's layout after a clear (`0x140235DF0`):
    /// the totals and the hit counts become zero.
    pub(super) fn cleared(&self) -> Self {
        let mut fresh = Self::default();
        fresh.0[BUCKETS] = self.0[BUCKETS];
        for bucket in 0..self.bucket_count() {
            fresh.0[7 + 2 * bucket] = self.width(bucket);
        }
        fresh
    }

    /// Counts a finished play of `duration` seconds that fell into `bucket` (when any).
    fn count_play(&mut self, play_elapsed: u32, duration: u32, bucket: Option<usize>) {
        self.0[PLAY_TIME] = self.0[PLAY_TIME].wrapping_add(play_elapsed);
        self.0[GAMES] = self.0[GAMES].wrapping_add(1);
        self.0[SUM_DURATION] = self.0[SUM_DURATION].wrapping_add(duration);
        self.0[SHORTEST] = self.0[SHORTEST].min(duration);
        self.0[LONGEST] = self.0[LONGEST].max(duration);
        if let Some(bucket) = bucket {
            let hits = self.hits_mut(bucket);
            *hits = hits.wrapping_add(1);
        }
    }
}

/// The published bookkeeping object (`seq+0x550`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Published {
    totals: BookkeepingTotals,
    count: u32,
    rows: [HistogramRow; MAX_ROWS],
}

impl Default for Published {
    fn default() -> Self {
        Self {
            totals: BookkeepingTotals::default(),
            count: 0,
            rows: [HistogramRow { lower: u32::MAX, upper: u32::MAX, count: 0 }; MAX_ROWS],
        }
    }
}

impl Published {
    pub(super) fn totals(&self) -> BookkeepingTotals {
        self.totals
    }

    pub(super) fn count(&self) -> u32 {
        self.count
    }

    pub(super) fn row(&self, index: u32) -> Option<HistogramRow> {
        if index >= self.count { None } else { self.rows.get(index as usize).copied() }
    }

    /// `publish` (`0x140236C40`): copies the totals and the hit counts; with `bounds` also the row limits and
    /// the row count.
    pub(super) fn publish(&mut self, work: &Work, bounds: Option<&[(u32, u32)]>) {
        let games = work.0[GAMES];
        self.totals = BookkeepingTotals {
            number_of_games: games,
            total_time: work.0[TOTAL_TIME],
            play_time: work.0[PLAY_TIME],
            average_play_time: work.0[SUM_DURATION].checked_div(games).unwrap_or(0),
            longest_play_time: if games != 0 { work.0[LONGEST] } else { 0 },
            shortest_play_time: if games != 0 { work.0[SHORTEST] } else { 0 },
        };
        for bucket in 0..work.bucket_count() {
            self.rows[bucket].count = work.hits(bucket);
        }
        if let Some(bounds) = bounds {
            for (row, &(lower, upper)) in self.rows.iter_mut().zip(bounds) {
                row.lower = lower;
                row.upper = upper;
            }
            let count = work.0[BUCKETS];
            if (1..=MAX_ROWS as u32).contains(&count) {
                self.count = count;
            }
        }
    }

    /// The first row of the work block's buckets that contains `duration` (`0x140236AC0`).
    fn bucket_of(&self, work: &Work, duration: u32) -> Option<usize> {
        (0..work.bucket_count()).find(|&bucket| {
            let row = &self.rows[bucket];
            row.lower <= duration && duration <= row.upper
        })
    }

    /// Resets everything the clear of `0x140235DF0` publishes: totals 0 and every row count 0, the bounds kept.
    pub(super) fn clear_counts(&mut self) {
        self.totals = BookkeepingTotals::default();
        for row in &mut self.rows {
            row.count = 0;
        }
    }
}

/// The result of a flush with a counted play: the seconds the play timer advanced and the play's duration.
pub(super) struct CountedPlay {
    pub(super) play_elapsed: u32,
    pub(super) duration: u32,
}

/// `flush` (`0x140236AC0`): adds the counted play (when any), then the elapsed total time, and publishes.
pub(super) fn flush(
    work: &mut Work,
    published: &mut Published,
    total_timer: &mut u64,
    now_ms: u64,
    play: Option<CountedPlay>,
) {
    if let Some(play) = play {
        let bucket = published.bucket_of(work, play.duration);
        work.count_play(play.play_elapsed, play.duration, bucket);
    }
    work.0[TOTAL_TIME] = work.0[TOTAL_TIME].wrapping_add(elapsed(*total_timer, now_ms));
    *total_timer = now_ms;
    published.publish(work, None);
}

/// `rebuild` (`0x1402367E0`) after the cached block was copied into `work`: aligns the buckets with the
/// configured widths and publishes the rows. Returns whether the block changed and must be stored.
pub(super) fn rebuild(work: &mut Work, published: &mut Published, widths: &[u32]) -> bool {
    // The validators cap the total at 31 so the open-ended last bucket always fits.
    let widths = &widths[..widths.len().min(MAX_ROWS - 1)];
    let count = widths.len();
    let mut changed = count as u32 + 1 != work.0[BUCKETS];
    let mut bounds = Vec::with_capacity(count + 1);
    let mut lower = 0u32;
    for (bucket, &width) in widths.iter().enumerate() {
        if work.width(bucket) != width {
            work.0[7 + 2 * bucket] = width;
            changed = true;
        }
        bounds.push((lower, lower.wrapping_add(width).wrapping_sub(1)));
        lower = lower.wrapping_add(width);
    }
    work.0[7 + 2 * count] = u32::MAX;
    bounds.push((lower, u32::MAX));
    work.0[BUCKETS] = count as u32 + 1;
    if changed {
        for bucket in 0..=count {
            *work.hits_mut(bucket) = 0;
        }
    }
    published.publish(work, Some(&bounds));
    changed
}
