//! Single-instance guard of `wWinMain` (`spec/controllers/lifecycle.md` §1.4).
//!
//! This is management of the daemon's own process, not an external backend. [`named_lock`] provides the
//! cross-platform lock and scoped release instead of a hand-written Win32 mutex wrapper. Its Windows
//! name is `Global\AM Daemon` (machine-wide), unlike the original session-local `AM Daemon`; on Unix it
//! uses `AM Daemon.lock` under `$TMPDIR`, or `/tmp` when unset.
//!
//! A successful immediate lock identifies the first instance. [`named_lock::Error::WouldBlock`] means
//! another instance is running; every other creation or locking error also means not first, as when the
//! original could not obtain a mutex handle. The guard never retries. `wWinMain` returns 10 in either case.

use named_lock::{NamedLock, NamedLockGuard};

/// Base single-instance lock name (`0x1405BCB70`), prefixed with `Global\` by `named-lock` on Windows.
pub const MUTEX_NAME: &str = "AM Daemon";

/// Holds the named lock and its ownership guard for the lifetime of the process entry routine.
#[derive(Debug)]
pub struct LaunchChecker {
    // Drop the ownership guard before closing the lock handle.
    guard: Option<NamedLockGuard>,
    _lock: Option<NamedLock>,
}

impl LaunchChecker {
    /// Creates or opens `name` and tries to acquire it without waiting.
    ///
    /// Contention and all lock errors produce a checker for which [`Self::is_first`] is false.
    pub fn new(name: &str) -> Self {
        let lock = NamedLock::create(name).ok();
        let guard = lock.as_ref().and_then(|lock| lock.try_lock().ok());
        Self { guard, _lock: lock }
    }

    /// Whether this process acquired and still owns the single-instance lock.
    pub fn is_first(&self) -> bool {
        self.guard.is_some()
    }
}
