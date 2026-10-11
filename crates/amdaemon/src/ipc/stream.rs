//! Root publication state machines of `Server::Impl` and the peer (client) contract.
//!
//! Evidence: primary publisher `0x140076580`, primary inbound `0x1400767B0`, secondary publisher
//! `0x140076D40`, secondary inbound `0x140076850`, client template `0x140076980` / `0x1400768E0`.

use crate::schema::image::{CLIENT_DATA_SIZE, SERVER_DATA_SIZE};

use crate::ipc::delta::{client_delta, server_delta};
use crate::ipc::header::{DecodeError, HEADER_LEN, HELLO_BODY_LEN, HELLO_TOKEN, Header, Kind, SendBuffer, decode_into};

/// Destination of outbound plaintext messages (`Messenger::putMention`, `0x140075330`).
pub trait MessageSink {
    /// Queues one plaintext message; `false` when the Messenger refused it.
    fn put_mention(&mut self, message: &[u8]) -> bool;
}

/// Primary (game client, `SC<idx>`) publication phase (`Impl+0x1C088`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum PrimaryPhase {
    /// Next publication sends the hello.
    Hello = 0,
    /// Next publication sends the full Combined image.
    Snapshot = 1,
    /// Streaming `ServerData` deltas.
    Streaming = 2,
}

/// Primary stream: hello → full Combined image → `ServerData` prefixes.
#[derive(Debug)]
pub struct PrimaryStream {
    phase: PrimaryPhase,
    mirror: Box<[u8]>,
    buffer: SendBuffer,
}

impl Default for PrimaryStream {
    fn default() -> Self {
        Self {
            phase: PrimaryPhase::Hello,
            mirror: vec![0; SERVER_DATA_SIZE].into_boxed_slice(),
            buffer: SendBuffer::default(),
        }
    }
}

impl PrimaryStream {
    /// Current phase.
    pub fn phase(&self) -> PrimaryPhase {
        self.phase
    }

    /// Messenger state-change callback (`0x14006D060`): restart from the hello. The caller also resets the
    /// ServerIO sender.
    pub fn reset(&mut self) {
        self.phase = PrimaryPhase::Hello;
    }

    /// One publication step (`0x140076580`).
    pub fn publish(&mut self, sink: &mut (impl MessageSink + ?Sized), server: &[u8], client: &[u8]) {
        match self.phase {
            PrimaryPhase::Hello => {
                if sink.put_mention(self.buffer.write_hello()) {
                    self.phase = PrimaryPhase::Snapshot;
                }
            }
            PrimaryPhase::Snapshot => {
                if sink.put_mention(self.buffer.write_combined(client, server)) {
                    self.mirror.copy_from_slice(server);
                    self.phase = PrimaryPhase::Streaming;
                }
            }
            PrimaryPhase::Streaming => {
                let n = server_delta(server, &self.mirror);
                if n != 0 && sink.put_mention(self.buffer.write_single(Kind::ServerData, &server[..n])) {
                    self.mirror[..n].copy_from_slice(&server[..n]);
                }
            }
        }
    }
}

/// Primary inbound handler (`0x1400767B0`): only kind 2 is applied, as a non-strict `ClientData` prefix,
/// regardless of phase. Other kinds are ignored (`Ok(false)`).
pub fn apply_primary_inbound(message: &[u8], client: &mut [u8]) -> Result<bool, DecodeError> {
    let header = Header::parse(message).ok_or(DecodeError::Truncated)?;
    if header.kind != Kind::ClientData as u32 {
        return Ok(false);
    }
    // Kind 2 never touches the server image, so an empty server slice is exact.
    decode_into(message, &mut [], client, false)?;
    Ok(true)
}

/// Secondary (browser, `BS<idx>`) publication phase (`Impl+0x366A0`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum SecondaryPhase {
    /// Next publication sends the hello.
    Hello = 0,
    /// Next publication sends the full Combined image.
    Snapshot = 1,
    /// Waiting for a zero-body request.
    Idle = 2,
    /// Kind 1 requested: send `ServerData` delta.
    ServerRequested = 3,
    /// Kind 2 requested: send `ClientData` delta.
    ClientRequested = 4,
    /// Kind 3 requested: send Combined deltas.
    CombinedRequested = 5,
}

/// Secondary stream: hello → full Combined → request/response deltas.
#[derive(Debug)]
pub struct SecondaryStream {
    phase: SecondaryPhase,
    server_mirror: Box<[u8]>,
    client_mirror: Box<[u8]>,
    buffer: SendBuffer,
}

impl Default for SecondaryStream {
    fn default() -> Self {
        Self {
            phase: SecondaryPhase::Hello,
            server_mirror: vec![0; SERVER_DATA_SIZE].into_boxed_slice(),
            client_mirror: vec![0; CLIENT_DATA_SIZE].into_boxed_slice(),
            buffer: SendBuffer::default(),
        }
    }
}

impl SecondaryStream {
    /// Messenger state-change callback (`0x14006D040`).
    pub fn reset(&mut self) {
        self.phase = SecondaryPhase::Hello;
    }

    /// Inbound handler (`0x140076850`): zero-body kinds 1/2/3 select the next response while idle.
    pub fn on_inbound(&mut self, message: &[u8]) {
        let Some(header) = Header::parse(message) else { return };
        if self.phase != SecondaryPhase::Idle || header.body_size != 0 {
            return;
        }
        self.phase = match header.kind {
            1 => SecondaryPhase::ServerRequested,
            2 => SecondaryPhase::ClientRequested,
            3 => SecondaryPhase::CombinedRequested,
            _ => return,
        };
    }

    /// One publication step (`0x140076D40`). Requested deltas are sent even when empty.
    pub fn publish(&mut self, sink: &mut (impl MessageSink + ?Sized), server: &[u8], client: &[u8]) {
        match self.phase {
            SecondaryPhase::Hello => {
                if sink.put_mention(self.buffer.write_hello()) {
                    self.phase = SecondaryPhase::Snapshot;
                }
            }
            SecondaryPhase::Snapshot => {
                if sink.put_mention(self.buffer.write_combined(client, server)) {
                    self.server_mirror.copy_from_slice(server);
                    self.client_mirror.copy_from_slice(client);
                    self.phase = SecondaryPhase::Idle;
                }
            }
            SecondaryPhase::Idle => {}
            SecondaryPhase::ServerRequested => {
                let n = server_delta(server, &self.server_mirror);
                if sink.put_mention(self.buffer.write_single(Kind::ServerData, &server[..n])) {
                    self.server_mirror[..n].copy_from_slice(&server[..n]);
                    self.phase = SecondaryPhase::Idle;
                }
            }
            SecondaryPhase::ClientRequested => {
                let n = client_delta(client, &self.client_mirror);
                if sink.put_mention(self.buffer.write_single(Kind::ClientData, &client[..n])) {
                    self.client_mirror[..n].copy_from_slice(&client[..n]);
                    self.phase = SecondaryPhase::Idle;
                }
            }
            SecondaryPhase::CombinedRequested => {
                let c = client_delta(client, &self.client_mirror);
                let s = server_delta(server, &self.server_mirror);
                if sink.put_mention(self.buffer.write_combined(&client[..c], &server[..s])) {
                    self.client_mirror[..c].copy_from_slice(&client[..c]);
                    self.server_mirror[..s].copy_from_slice(&server[..s]);
                    self.phase = SecondaryPhase::Idle;
                }
            }
        }
    }
}

/// Client-side phase of the peer template (`DaemonIpcImplBase::onReplyRead` `0x140076980`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ClientPhase {
    /// Expecting the hello.
    Hello = 0,
    /// Expecting the strict Combined image.
    Snapshot = 1,
    /// Receiving `ServerData` prefixes and publishing `ClientData` deltas.
    Streaming = 2,
}

/// Error raised by the client template.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ClientError {
    /// Hello token mismatch ("am process version", line 530 of the source).
    #[error("daemon hello token {0:#018x} does not match {HELLO_TOKEN:#018x}")]
    VersionMismatch(u64),
    /// Body decoding failed.
    #[error(transparent)]
    Decode(#[from] DecodeError),
}

/// Peer (game-side) contract recovered from the unreferenced client template in the binary.
#[derive(Debug)]
pub struct ClientStream {
    phase: ClientPhase,
    client_mirror: Box<[u8]>,
    buffer: SendBuffer,
}

impl Default for ClientStream {
    fn default() -> Self {
        Self {
            phase: ClientPhase::Hello,
            client_mirror: vec![0; CLIENT_DATA_SIZE].into_boxed_slice(),
            buffer: SendBuffer::default(),
        }
    }
}

impl ClientStream {
    /// Current phase.
    pub fn phase(&self) -> ClientPhase {
        self.phase
    }

    /// Restart (Messenger state change).
    pub fn reset(&mut self) {
        self.phase = ClientPhase::Hello;
    }

    /// Applies one daemon message (`0x140076980`). Messages not valid in the current phase are ignored.
    pub fn on_reply(&mut self, message: &[u8], server: &mut [u8], client: &mut [u8]) -> Result<(), ClientError> {
        let header = Header::parse(message).ok_or(DecodeError::Truncated)?;
        match (header.kind, self.phase) {
            (0, ClientPhase::Hello) if header.body_size == HELLO_BODY_LEN => {
                let body: [u8; 8] = message
                    .get(HEADER_LEN..HEADER_LEN + 8)
                    .and_then(|b| b.try_into().ok())
                    .ok_or(DecodeError::BodyOverrun)?;
                let token = u64::from_le_bytes(body);
                if token != HELLO_TOKEN {
                    return Err(ClientError::VersionMismatch(token));
                }
                self.phase = ClientPhase::Snapshot;
            }
            (3, ClientPhase::Snapshot) => {
                decode_into(message, server, client, true)?;
                self.client_mirror.copy_from_slice(client);
                self.phase = ClientPhase::Streaming;
            }
            (1, ClientPhase::Streaming) => {
                decode_into(message, server, client, false)?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Client publisher (`0x1400768E0`): sends the changed `ClientData` prefix while streaming.
    pub fn publish(&mut self, sink: &mut (impl MessageSink + ?Sized), client: &[u8]) {
        if self.phase != ClientPhase::Streaming {
            return;
        }
        let n = client_delta(client, &self.client_mirror);
        if n != 0 && sink.put_mention(self.buffer.write_single(Kind::ClientData, &client[..n])) {
            self.client_mirror[..n].copy_from_slice(&client[..n]);
        }
    }
}
