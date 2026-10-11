//! The shared registry of named objects behind every [`LoopbackPlatform`](crate::mock::ipc::LoopbackPlatform)
//! handle.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError, Weak};

use crate::mock::ipc::mutex::MutexState;
use crate::mock::ipc::pipe::PipeCore;

/// Locks an internal mutex, ignoring poisoning (the guarded state stays consistent).
pub(super) fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Named mutexes, sections and pipes; entries die with their last handle.
#[derive(Debug, Default)]
pub(super) struct Hub {
    next_id: u64,
    pub(super) mutexes: HashMap<String, Weak<Mutex<MutexState>>>,
    pub(super) sections: HashMap<String, Weak<Mutex<Vec<u8>>>>,
    pub(super) pipes: HashMap<String, Weak<Mutex<PipeCore>>>,
}

impl Hub {
    pub(super) fn fresh_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }
}
