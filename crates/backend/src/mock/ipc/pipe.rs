//! [`PipeEndpoint`] double.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, Weak};

use crate::mock::ipc::hub::{Hub, locked};
use crate::pipe::{PipeEndpoint, PipeRole, WaitResult};

/// State of one named pipe shared by its server and client endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Link {
    /// The server instance waits for a client.
    Listening,
    /// A client holds the session with this id.
    Connected(u64),
    /// The client closed its handle; the server finds out with its next I/O.
    ClientGone,
    /// The server endpoint was destroyed.
    ServerGone,
}

#[derive(Debug)]
pub(super) struct PipeCore {
    link: Link,
    /// Bytes written and not yet read.
    stream: VecDeque<u8>,
    /// Total bytes ever written / read in the current session.
    written: u64,
    consumed: u64,
    next_session: u64,
}

/// The outstanding transfer of an endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Io {
    None,
    Read(usize),
    /// Completes once `consumed` reaches this total.
    Write(u64),
}

#[derive(Debug)]
pub(super) struct LoopbackPipe {
    hub: Arc<Mutex<Hub>>,
    name: String,
    role: PipeRole,
    is_reader: bool,
    core: Option<Arc<Mutex<PipeCore>>>,
    /// `Some(session)` while this endpoint considers itself connected.
    session: Option<u64>,
    io: Io,
    last_ok: bool,
    data: Vec<u8>,
}

impl LoopbackPipe {
    /// Creates the endpoint: a server creates its instance, a client makes one connect attempt.
    pub(super) fn open(hub: Arc<Mutex<Hub>>, resource_name: &str, role: PipeRole, is_reader: bool) -> Self {
        let mut pipe = Self {
            hub,
            name: resource_name.to_owned(),
            role,
            is_reader,
            core: None,
            session: None,
            io: Io::None,
            last_ok: false,
            data: Vec::new(),
        };
        if !resource_name.is_empty() {
            match role {
                PipeRole::Server => pipe.create_instance(),
                PipeRole::Client => {
                    pipe.pump();
                }
            }
        }
        pipe
    }

    /// `CreateNamedPipeW(FILE_FLAG_FIRST_PIPE_INSTANCE)`: fails silently when a live server owns the name.
    fn create_instance(&mut self) {
        let mut hub = locked(&self.hub);
        if let Some(existing) = hub.pipes.get(&self.name).and_then(Weak::upgrade)
            && locked(&existing).link != Link::ServerGone
        {
            return;
        }
        let core = Arc::new(Mutex::new(PipeCore {
            link: Link::Listening,
            stream: VecDeque::new(),
            written: 0,
            consumed: 0,
            next_session: 0,
        }));
        hub.pipes.insert(self.name.clone(), Arc::downgrade(&core));
        self.core = Some(core);
    }

    fn connect(&mut self) -> bool {
        match self.role {
            PipeRole::Server => {
                if let Some(core) = &self.core {
                    let mut core = locked(core);
                    match core.link {
                        Link::Connected(session) => self.session = Some(session),
                        // A client that came and went before the server noticed: listen again.
                        Link::ClientGone => core.link = Link::Listening,
                        Link::Listening | Link::ServerGone => {}
                    }
                }
            }
            PipeRole::Client => {
                if self.core.is_none() {
                    self.core = locked(&self.hub).pipes.get(&self.name).and_then(Weak::upgrade);
                }
                if let Some(core) = &self.core {
                    let mut core = locked(core);
                    if core.link == Link::Listening {
                        core.next_session += 1;
                        let session = core.next_session;
                        core.link = Link::Connected(session);
                        core.stream.clear();
                        core.written = 0;
                        core.consumed = 0;
                        self.session = Some(session);
                    }
                }
            }
        }
        true
    }

    /// The connection of this endpoint is still the live session of the pipe.
    fn link_alive(&self) -> bool {
        match (&self.core, self.session) {
            (Some(core), Some(session)) => locked(core).link == Link::Connected(session),
            _ => false,
        }
    }

    /// Server: `DisconnectNamedPipe` (listen again); client: `CloseHandle`.
    fn disconnect(&mut self) {
        let Some(session) = self.session.take() else { return };
        let Some(core) = &self.core else { return };
        let mut core = locked(core);
        match self.role {
            PipeRole::Server => {
                core.link = Link::Listening;
                core.stream.clear();
            }
            PipeRole::Client => {
                if core.link == Link::Connected(session) {
                    core.link = Link::ClientGone;
                    core.stream.clear();
                }
            }
        }
    }

    /// A transfer failed: record it and drop the connection.
    fn fail(&mut self) {
        self.io = Io::None;
        self.last_ok = false;
        self.disconnect();
    }
}

impl PipeEndpoint for LoopbackPipe {
    fn is_valid(&self) -> bool {
        match self.role {
            PipeRole::Server => self.core.is_some(),
            PipeRole::Client => !self.name.is_empty(),
        }
    }

    fn pump(&mut self) -> bool {
        if !self.is_valid() {
            return false;
        }
        if self.session.is_some() {
            self.wait(0);
            return true;
        }
        self.connect()
    }

    fn is_connected(&mut self) -> bool {
        self.session.is_some() && self.is_valid()
    }

    fn is_io_pending(&mut self) -> bool {
        self.io != Io::None && self.is_connected()
    }

    fn last_io_ok(&mut self) -> bool {
        self.last_ok
    }

    fn can_read(&mut self) -> bool {
        self.is_reader && self.io == Io::None && self.is_connected()
    }

    fn can_write(&mut self) -> bool {
        !self.is_reader && self.io == Io::None && self.is_connected()
    }

    fn read(&mut self, len: usize) -> bool {
        if !self.can_read() {
            return false;
        }
        if !self.link_alive() {
            self.fail();
            return false;
        }
        self.io = Io::Read(len);
        true
    }

    fn read_data(&self) -> &[u8] {
        &self.data
    }

    fn write(&mut self, data: &[u8]) -> bool {
        if !self.can_write() {
            return false;
        }
        if !self.link_alive() {
            self.fail();
            return false;
        }
        let Some(core) = &self.core else { return false };
        let mut core = locked(core);
        core.stream.extend(data);
        core.written += data.len() as u64;
        self.io = Io::Write(core.written);
        true
    }

    fn wait(&mut self, _timeout_ms: u32) -> WaitResult {
        if self.io == Io::None || !self.is_connected() {
            return WaitResult::Idle;
        }
        if !self.link_alive() {
            self.fail();
            return WaitResult::Completed;
        }
        let Some(core) = self.core.clone() else { return WaitResult::Idle };
        let mut core = locked(&core);
        match self.io {
            Io::Read(len) if core.stream.len() >= len => {
                self.data.clear();
                self.data.extend(core.stream.drain(..len));
                core.consumed += len as u64;
            }
            Io::Write(end) if core.consumed >= end => {}
            _ => return WaitResult::Timeout,
        }
        self.io = Io::None;
        self.last_ok = true;
        WaitResult::Completed
    }
}

impl Drop for LoopbackPipe {
    fn drop(&mut self) {
        match self.role {
            PipeRole::Server => {
                if let Some(core) = &self.core {
                    let mut core = locked(core);
                    core.link = Link::ServerGone;
                    core.stream.clear();
                }
            }
            PipeRole::Client => self.disconnect(),
        }
    }
}
