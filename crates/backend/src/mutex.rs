//! Named mutexes of the IPC layer (`OpenMutexW` / `CreateMutexW`, `0x140063BD0`).

use std::fmt::Debug;

/// Named mutex handle (`OpenMutexW` / `CreateMutexW`, `0x140063BD0`).
///
/// The raw operations only; recursion counting and release on drop are provided by `ipc::mutex::MutexHolder`.
/// On Windows ownership is per thread, so a mutex must be locked and unlocked on the same thread.
pub trait NamedMutex: Debug {
    /// `WaitForSingleObject(h, INFINITE)`; `true` on `WAIT_OBJECT_0` or `WAIT_ABANDONED`.
    fn lock(&mut self) -> bool;
    /// `WaitForSingleObject(h, 0)`; `true` on `WAIT_OBJECT_0` or `WAIT_ABANDONED`.
    fn try_lock(&mut self) -> bool;
    /// `ReleaseMutex`.
    fn unlock(&mut self);
}
