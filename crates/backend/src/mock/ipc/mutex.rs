//! [`NamedMutex`] double.

use std::sync::{Arc, Mutex};

use crate::mock::ipc::hub::locked;
use crate::mutex::NamedMutex;

#[derive(Debug, Default)]
pub(super) struct MutexState {
    owner: Option<u64>,
    depth: u32,
}

#[derive(Debug)]
pub(super) struct LoopbackMutex {
    pub(super) id: u64,
    pub(super) state: Arc<Mutex<MutexState>>,
}

impl LoopbackMutex {
    fn acquire(&mut self) -> bool {
        let mut state = locked(&self.state);
        match state.owner {
            None => {
                state.owner = Some(self.id);
                state.depth = 1;
                true
            }
            Some(owner) if owner == self.id => {
                state.depth += 1;
                true
            }
            Some(_) => false,
        }
    }
}

impl NamedMutex for LoopbackMutex {
    fn lock(&mut self) -> bool {
        self.acquire()
    }

    fn try_lock(&mut self) -> bool {
        self.acquire()
    }

    fn unlock(&mut self) {
        let mut state = locked(&self.state);
        if state.owner == Some(self.id) {
            state.depth -= 1;
            if state.depth == 0 {
                state.owner = None;
            }
        }
    }
}

impl Drop for LoopbackMutex {
    fn drop(&mut self) {
        let mut state = locked(&self.state);
        if state.owner == Some(self.id) {
            *state = MutexState::default();
        }
    }
}
