//! Named shared sections of the IPC layer (`CreateFileMappingW` + `MapViewOfFile`).

use std::fmt::Debug;

/// A mapped view of a named section object (`CreateFileMappingW` + `MapViewOfFile`, `FILE_MAP_WRITE`).
///
/// Access is by copy-in/copy-out because other processes mutate the memory concurrently.
pub trait Section: Debug {
    /// Number of mapped bytes.
    fn byte_len(&self) -> usize;
    /// Copies `out.len()` bytes starting at `offset`; `false` (nothing copied) when out of range.
    fn read(&self, offset: usize, out: &mut [u8]) -> bool;
    /// Copies `data` to `offset`; `false` (nothing copied) when out of range.
    fn write(&mut self, offset: usize, data: &[u8]) -> bool;
}
