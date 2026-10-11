//! `amWorkaroundPreloadDatasec` (`spec/libam/platform.md` A.5): the page-in of every module of a process.

use crate::Unsupported;

/// Why the preload did not run: the process cannot be opened or the architecture gate failed (the worker's
/// return -1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the process cannot be preloaded")]
pub struct PreloadError;

/// The operating system side of `CommonPort::preload`.
pub trait Preloader {
    /// Touches every committed, readable page of every module of the process; `process_id` 0 is this process.
    fn preload(&mut self, process_id: u32) -> Result<(), PreloadError>;
}

/// No page-in of another process's modules.
impl Preloader for Unsupported {
    fn preload(&mut self, _process_id: u32) -> Result<(), PreloadError> {
        Err(PreloadError)
    }
}
