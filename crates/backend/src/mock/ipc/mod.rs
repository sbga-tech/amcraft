//! [`IpcPlatform`] double: the shared in-process hub of named objects behind [`LoopbackPlatform`].
//!
//! Every clone shares one hub, so two "processes" (a daemon `Server` and a peer `Messenger`) living in one test can
//! find each other's named objects. The hub models just enough of the Win32 behaviour the IPC layer relies on:
//!
//! * **mutexes** — named, recursive for the owning handle, blocked for any other handle. A blocking `lock`
//!   that would deadlock a single-threaded test fails instead of waiting. Dropping the owner releases the
//!   mutex like the death of its process (`WAIT_ABANDONED` is treated as success anyway).
//! * **sections** — zero-initialised byte buffers that live while any handle exists.
//! * **pipes** — byte streams with the overlapped, completion-on-alertable-wait model of `NamedPipe`:
//!   a read completes when its full length is buffered, a write completes only after the reader consumed all
//!   of it (zero-sized pipe buffers), and a broken connection is only noticed by the next I/O.
//! * **memory protection** — identity by default ([`protector::IdentityProtector`]); a different transform can be
//!   injected with [`LoopbackPlatform::with_protector`].
//!
//! [`ScriptedPipe`] provides an always-connected endpoint with immediate captured writes and manually
//! supplied input, independent of the shared hub.

pub mod hub;
pub mod mutex;
pub mod pipe;
pub mod protector;
pub mod scripted_pipe;
pub mod section;

pub use scripted_pipe::ScriptedPipe;

use std::sync::{Arc, Mutex, Weak};

use crate::ipc::{IpcPlatform, Services};
use crate::mock::ipc::hub::{Hub, locked};
use crate::mock::ipc::mutex::{LoopbackMutex, MutexState};
use crate::mock::ipc::pipe::LoopbackPipe;
use crate::mock::ipc::protector::IdentityProtector;
use crate::mock::ipc::section::LoopbackSection;
use crate::mutex::NamedMutex;
use crate::pipe::{PipeEndpoint, PipeRole};
use crate::protector::MemoryProtector;
use crate::section::Section;

/// The shared in-process platform. Cloning shares the hub and the protector.
#[derive(Debug, Clone)]
pub struct LoopbackPlatform {
    hub: Arc<Mutex<Hub>>,
    protector: Arc<dyn MemoryProtector>,
}

impl Default for LoopbackPlatform {
    fn default() -> Self {
        Self::new()
    }
}

impl LoopbackPlatform {
    /// A fresh hub with identity protection.
    pub fn new() -> Self {
        Self { hub: Arc::new(Mutex::new(Hub::default())), protector: Arc::new(IdentityProtector) }
    }

    /// Replaces the memory protector of this handle (and of clones made afterwards).
    #[must_use]
    pub fn with_protector(mut self, protector: Arc<dyn MemoryProtector>) -> Self {
        self.protector = protector;
        self
    }
}

impl IpcPlatform for LoopbackPlatform {
    fn services(&self) -> Services {
        Services::new(Arc::clone(&self.protector))
    }

    fn open_mutex(&self, name: Option<&str>) -> Option<Box<dyn NamedMutex>> {
        let mut hub = locked(&self.hub);
        let id = hub.fresh_id();
        let state = match name {
            Some(name) => match hub.mutexes.get(name).and_then(Weak::upgrade) {
                Some(state) => state,
                None => {
                    let state = Arc::new(Mutex::new(MutexState::default()));
                    hub.mutexes.insert(name.to_owned(), Arc::downgrade(&state));
                    state
                }
            },
            None => Arc::new(Mutex::new(MutexState::default())),
        };
        Some(Box::new(LoopbackMutex { id, state }))
    }

    fn create_section(&self, name: &str, mapping_len: usize) -> Option<(Box<dyn Section>, bool)> {
        let mut hub = locked(&self.hub);
        if let Some(bytes) = hub.sections.get(name).and_then(Weak::upgrade) {
            if locked(&bytes).len() < mapping_len {
                return None;
            }
            return Some((Box::new(LoopbackSection { bytes, view_len: mapping_len }), false));
        }
        let bytes = Arc::new(Mutex::new(vec![0; mapping_len]));
        hub.sections.insert(name.to_owned(), Arc::downgrade(&bytes));
        Some((Box::new(LoopbackSection { bytes, view_len: mapping_len }), true))
    }

    fn open_section(&self, name: &str) -> Option<Box<dyn Section>> {
        let hub = locked(&self.hub);
        let bytes = hub.sections.get(name).and_then(Weak::upgrade)?;
        let view_len = locked(&bytes).len();
        Some(Box::new(LoopbackSection { bytes, view_len }))
    }

    fn open_pipe(&self, resource_name: &str, role: PipeRole, is_reader: bool) -> Box<dyn PipeEndpoint> {
        Box::new(LoopbackPipe::open(Arc::clone(&self.hub), resource_name, role, is_reader))
    }
}
