//! `amdaemon::util::MutexHolder`: recursive-lock accounting over a backend [`NamedMutex`].

use backend::ipc::IpcPlatform;
use backend::mutex::NamedMutex;

/// `amdaemon::util::MutexHolder` (`{HANDLE h; i32 lockCount}`): counts recursive locks and releases every
/// remaining one when destroyed (`0x1400642C0`), then closes the handle.
#[derive(Debug)]
pub struct MutexHolder {
    mutex: Box<dyn NamedMutex>,
    lock_count: u32,
}

impl MutexHolder {
    /// Opens the named mutex, creating it when absent (`name == None` creates an anonymous one).
    pub fn open(platform: &dyn IpcPlatform, name: Option<&str>) -> Option<Self> {
        platform.open_mutex(name).map(|mutex| Self { mutex, lock_count: 0 })
    }

    /// Blocking lock (`0x140063D80`).
    pub fn lock(&mut self) -> bool {
        let locked = self.mutex.lock();
        if locked {
            self.lock_count += 1;
        }
        locked
    }

    /// Non-blocking lock (`0x140063DD0`).
    pub fn try_lock(&mut self) -> bool {
        let locked = self.mutex.try_lock();
        if locked {
            self.lock_count += 1;
        }
        locked
    }

    /// Unlock (`0x140063E20`): releases only when the holder owns at least one lock.
    pub fn unlock(&mut self) {
        if self.lock_count > 0 {
            self.mutex.unlock();
            self.lock_count -= 1;
        }
    }
}

impl Drop for MutexHolder {
    fn drop(&mut self) {
        while self.lock_count > 0 {
            self.unlock();
        }
    }
}
