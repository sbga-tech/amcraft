//! Deterministic wlan socket: datagrams queued by the test arrive while the socket is open.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

use crate::wlan::WlanSocket;

#[derive(Debug, Default)]
struct SimState {
    open: bool,
    queue: VecDeque<Vec<u8>>,
}

/// Deterministic wlan socket: datagrams queued by the test arrive while the socket is open.
#[derive(Debug, Clone, Default)]
pub struct SimWlan(Arc<Mutex<SimState>>);

impl SimWlan {
    fn lock(&self) -> std::sync::MutexGuard<'_, SimState> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues one announcement datagram.
    pub fn announce(&self, datagram: impl Into<Vec<u8>>) {
        self.lock().queue.push_back(datagram.into());
    }

    /// The socket is open.
    pub fn is_open(&self) -> bool {
        self.lock().open
    }
}

impl WlanSocket for SimWlan {
    fn open(&mut self) -> bool {
        self.lock().open = true;
        true
    }

    fn close(&mut self) {
        let mut state = self.lock();
        state.open = false;
        state.queue.clear();
    }

    fn receive(&mut self) -> Option<Vec<u8>> {
        let mut state = self.lock();
        if state.open { state.queue.pop_front() } else { None }
    }
}
