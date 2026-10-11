//! Named-pipe endpoints of the IPC layer (`amdaemon::ipc::NamedPipe`, `spec/ipc/transport.md` §2).

use std::fmt::Debug;

/// Which side of a named pipe an endpoint is (`NamedPipe` factory, `0x140082590` / `0x140082660`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipeRole {
    /// Creates the (single-instance) pipe and waits for a client (`ServerImpl`).
    Server,
    /// Connects to an existing pipe (`ClientImpl`).
    Client,
}

/// Result of [`PipeEndpoint::wait`] (`0x140083C90`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum WaitResult {
    /// No I/O outstanding (the wait was skipped).
    Idle = 0,
    /// The wait itself failed.
    Failed = 1,
    /// The event was signalled or the I/O completed.
    Completed = 2,
    /// Completion routines ran but this endpoint's I/O is still outstanding.
    CompletionRan = 3,
    /// Timed out.
    Timeout = 4,
}

/// One named-pipe endpoint with the single-threaded, caller-driven overlapped I/O model of
/// `amdaemon::ipc::NamedPipe` (`spec/ipc/transport.md` §2): at most one read or write is outstanding;
/// completions are only delivered inside [`wait`](Self::wait) / [`pump`](Self::pump) (alertable waits).
///
/// Every transfer must complete with exactly the requested byte count; anything else (error, short
/// transfer, broken pipe) marks the endpoint disconnected, closes (client) or disconnects (server) the OS
/// pipe and records `last_io_ok == false`.
pub trait PipeEndpoint: Debug {
    /// Name accepted and, for a server, the pipe instance exists (`isValid`).
    fn is_valid(&self) -> bool;
    /// `pump` (`0x140083B90`): connect-if-needed, otherwise an alertable `wait(0)`. Returns `false` when the
    /// endpoint is invalid or the connect attempt failed.
    fn pump(&mut self) -> bool;
    /// Connected and valid.
    fn is_connected(&mut self) -> bool;
    /// A read or write is outstanding.
    fn is_io_pending(&mut self) -> bool;
    /// The last finished transfer completed with the full byte count.
    fn last_io_ok(&mut self) -> bool;
    /// This endpoint reads, nothing is outstanding and it is connected.
    fn can_read(&mut self) -> bool;
    /// This endpoint writes, nothing is outstanding and it is connected.
    fn can_write(&mut self) -> bool;
    /// Starts reading exactly `len` bytes into the endpoint's buffer; `false` (and disconnect) when the OS
    /// call fails.
    fn read(&mut self, len: usize) -> bool;
    /// Bytes delivered by the last successfully completed [`read`](Self::read) (length = requested length).
    fn read_data(&self) -> &[u8];
    /// Copies `data` into the endpoint's buffer and starts writing it; `false` (and disconnect) when the OS
    /// call fails.
    fn write(&mut self, data: &[u8]) -> bool;
    /// Alertable wait of `timeout_ms` on the endpoint's I/O event (always called with 0 by the Messenger).
    fn wait(&mut self, timeout_ms: u32) -> WaitResult;
}
