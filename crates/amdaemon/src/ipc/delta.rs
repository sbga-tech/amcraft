//! Shortest-changed-prefix computation used by every publisher.
//!
//! Evidence: `findLastMismatchBinaryBlock` `0x1404096F0`, ServerData refinement `0x140077160`, ClientData
//! refinement `0x1400770B0`.

use crate::schema::image::{CLIENT_DATA_SIZE, SERVER_DATA_SIZE};

/// Initial comparison block for `ServerData` (`0x140077160`).
pub const SERVER_INITIAL_BLOCK: usize = 0x4000;
/// Initial comparison block for `ClientData` (`0x1400770B0`).
pub const CLIENT_INITIAL_BLOCK: usize = 0x400;
/// Refinement stops once the block is no longer larger than this.
pub const MIN_BLOCK: usize = 0x40;

/// Returns the end offset of the last differing `block`-aligned block of `a[..size]` vs `b[..size]`
/// (`size` itself when the unaligned tail differs), or 0 when equal.
///
/// `block` must be non-zero; the original throws and returns 0 for a zero block.
pub fn last_mismatch_block_end(a: &[u8], b: &[u8], size: usize, block: usize) -> usize {
    if block == 0 {
        return 0;
    }
    let tail = size % block;
    let aligned = size - tail;
    if tail != 0 && a[aligned..size] != b[aligned..size] {
        return size;
    }
    let mut end = aligned;
    while end != 0 {
        let start = end - block;
        if a[start..end] != b[start..end] {
            return end;
        }
        end = start;
    }
    0
}

fn refine(live: &[u8], mirror: &[u8], size: usize, initial_block: usize) -> usize {
    let mut block = initial_block;
    let mut limit = size;
    let mut n = last_mismatch_block_end(live, mirror, size, block);
    if n == 0 {
        return 0;
    }
    while block + n <= limit && block > MIN_BLOCK {
        let divisor = if block < 0x100 { 2 } else { 4 };
        limit = n;
        block /= divisor;
        n = last_mismatch_block_end(live, mirror, n, block);
        if n == 0 {
            return 0;
        }
    }
    n
}

/// Length of the `ServerData` prefix that must be republished (`0x140077160`).
pub fn server_delta(live: &[u8], mirror: &[u8]) -> usize {
    refine(live, mirror, SERVER_DATA_SIZE, SERVER_INITIAL_BLOCK)
}

/// Length of the `ClientData` prefix that must be republished (`0x1400770B0`).
pub fn client_delta(live: &[u8], mirror: &[u8]) -> usize {
    refine(live, mirror, CLIENT_DATA_SIZE, CLIENT_INITIAL_BLOCK)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server_pair(change_at: &[usize]) -> (Vec<u8>, Vec<u8>) {
        let mirror = vec![0u8; SERVER_DATA_SIZE];
        let mut live = mirror.clone();
        for &i in change_at {
            live[i] = 1;
        }
        (live, mirror)
    }

    #[test]
    fn change_in_first_byte_refines_to_one_min_block() {
        let (live, mirror) = server_pair(&[0]);
        assert_eq!(server_delta(&live, &mirror), 0x40);
    }

    #[test]
    fn change_in_unaligned_tail_publishes_whole_root() {
        // 0x1A5E8 % 0x4000 = 0x25E8: the tail block differs, so the first pass returns the full size and the
        // refinement loop never runs (block + n > limit).
        let (live, mirror) = server_pair(&[SERVER_DATA_SIZE - 1]);
        assert_eq!(server_delta(&live, &mirror), SERVER_DATA_SIZE);
    }

    #[test]
    fn refinement_stops_when_block_reaches_limit() {
        // Change at 0x3FFF: pass 1 (0x4000) -> 0x4000; block+n = 0x8000 > limit? no (limit 0x1A5E8) -> refine.
        // pass 2 (0x1000) -> 0x4000; 0x1000+0x4000 > 0x4000 -> stop.
        let (live, mirror) = server_pair(&[0x3FFF]);
        assert_eq!(server_delta(&live, &mirror), 0x4000);
    }

    #[test]
    fn client_delta_uses_its_own_block_ladder() {
        let mirror = vec![0u8; CLIENT_DATA_SIZE];
        let mut live = mirror.clone();
        live[0x41] = 7;
        assert_eq!(client_delta(&live, &mirror), 0x80);
    }
}
