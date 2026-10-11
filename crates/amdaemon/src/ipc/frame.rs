//! Transport frames of a Messenger (`amdaemon::ipc::message::{Writer, Reader}`, `spec/ipc/messenger_crypto.md` §5
//! and `spec/ipc/transport.md` §2.10–§2.11).
//!
//! One frame is exactly one message on the byte-stream pipe:
//!
//! ```text
//! frame       = P16(header16) ‖ body_padded
//! header16    = u32 sequence ‖ u32 type ‖ u32 size ‖ u32 pad     (little endian, no magic)
//! body_padded = body ‖ zero pad                                   (length ceil16(size), size <= 0x30000)
//! P16         = MemoryProtector::protect over the 16 header bytes (always)
//! body        = Type 0 (Mention): AES-GCM record, sent as is
//!               Type 1 (Key):     MemoryProtector::protect over body_padded
//! ```
//!
//! Deviation: the original leaves header bytes 12..16 and the body padding as stale bytes of the previous
//! frame; receivers never read them, and this implementation sends zeros.

use backend::ipc::Services;
use backend::pipe::{PipeEndpoint, WaitResult};

/// Size of the frame header.
pub const FRAME_HEADER_LEN: usize = 16;
/// Largest logical body (`0x140082430`).
pub const MAX_BODY_LEN: usize = 0x30000;
/// Frames the pipe reader hands to the Messenger per `execute` (`0x14007B8C0`).
pub const MAX_MESSAGES_PER_TICK: usize = 3;

/// `message::Type` (values direct, names inferred).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum FrameType {
    /// AES-GCM protected application message; the body is not additionally protected.
    Mention = 0,
    /// Public-key message; the body is protected with the logon-session key.
    Key = 1,
}

impl FrameType {
    /// Maps a raw header value; unknown types are ignored by receivers.
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            0 => Some(Self::Mention),
            1 => Some(Self::Key),
            _ => None,
        }
    }
}

/// `message::Header` (plaintext form).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    /// Per-writer sequence number, never 0, never reset.
    pub sequence: u32,
    /// Raw `message::Type`.
    pub kind: u32,
    /// Logical body length.
    pub size: u32,
    /// Unused by receivers.
    pub pad: u32,
}

impl FrameHeader {
    /// Little-endian wire form.
    pub fn to_bytes(self) -> [u8; FRAME_HEADER_LEN] {
        let mut bytes = [0u8; FRAME_HEADER_LEN];
        for (chunk, word) in bytes.chunks_exact_mut(4).zip([self.sequence, self.kind, self.size, self.pad]) {
            chunk.copy_from_slice(&word.to_le_bytes());
        }
        bytes
    }

    /// Parses the little-endian wire form.
    pub fn from_bytes(bytes: &[u8; FRAME_HEADER_LEN]) -> Self {
        let word = |i: usize| u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
        Self { sequence: word(0), kind: word(4), size: word(8), pad: word(12) }
    }

    /// Body length on the wire.
    pub fn padded_size(&self) -> usize {
        padded_len(self.size as usize)
    }
}

/// `ceil16(size)`: bodies travel padded to the `CryptProtectMemory` block size.
pub const fn padded_len(size: usize) -> usize {
    (size + 15) & !15
}

/// Writer half (`message::Writer` over `AsyncIOWriterBase`, `0x14007B900`, `0x140082200..0x140082380`).
#[derive(Debug)]
pub struct FrameWriter {
    pipe: Box<dyn PipeEndpoint>,
    services: Services,
    sequence: u32,
    frame: Vec<u8>,
    last_start_ok: bool,
}

impl FrameWriter {
    /// Wraps the outbound pipe endpoint.
    pub fn new(pipe: Box<dyn PipeEndpoint>, services: Services) -> Self {
        Self { pipe, services, sequence: 0, frame: Vec::new(), last_start_ok: false }
    }

    /// The pipe was accepted by the OS layer.
    pub fn is_valid(&self) -> bool {
        self.pipe.is_valid()
    }

    /// Pipe pump (vtable slot 2).
    pub fn tick(&mut self) -> bool {
        self.pipe.pump()
    }

    /// Pipe connected (slot 3).
    pub fn is_connected(&mut self) -> bool {
        self.pipe.is_connected()
    }

    /// `canStart` (slot 4, `0x140082260`): the pipe can write and nothing is outstanding.
    pub fn can_start_write(&mut self) -> bool {
        self.pipe.can_write() && !self.pipe.is_io_pending()
    }

    /// A frame is still being written (slot 5).
    pub fn is_writing(&mut self) -> bool {
        self.pipe.is_io_pending()
    }

    /// "The frame started by the last [`start`](Self::start) was written completely and successfully"
    /// (`0x140074FC0`).
    pub fn completed_ok(&mut self) -> bool {
        self.last_start_ok && !self.pipe.is_io_pending() && self.pipe.last_io_ok()
    }

    /// `Writer::start` (`0x14007B930`): builds, protects and starts writing one frame. `false` when the body is
    /// too large, the pipe cannot write now, protection fails or the OS write fails.
    pub fn start(&mut self, kind: FrameType, data: &[u8]) -> bool {
        self.last_start_ok = false;
        if data.len() > MAX_BODY_LEN {
            tracing::error!(len = data.len(), limit = MAX_BODY_LEN, "frame body is too large");
            return false;
        }
        if !self.can_start_write() {
            return false;
        }
        let padded = padded_len(data.len());
        self.frame.resize(FRAME_HEADER_LEN + padded, 0);
        self.sequence = self.sequence.wrapping_add(1);
        if self.sequence == 0 {
            self.sequence = 1;
        }
        let header = FrameHeader { sequence: self.sequence, kind: kind as u32, size: data.len() as u32, pad: 0 };
        self.frame[..FRAME_HEADER_LEN].copy_from_slice(&header.to_bytes());
        if !self.services.protect(&mut self.frame[..FRAME_HEADER_LEN]) {
            tracing::error!("protecting the frame header failed");
            return false;
        }
        if padded != 0 {
            let body = &mut self.frame[FRAME_HEADER_LEN..];
            body[..data.len()].copy_from_slice(data);
            body[data.len()..].fill(0);
            if kind == FrameType::Key && !self.services.protect(body) {
                tracing::error!("protecting the key frame body failed");
                return false;
            }
        }
        self.last_start_ok = self.pipe.write(&self.frame) && self.pipe.wait(0) != WaitResult::Failed;
        self.last_start_ok
    }
}

/// Reader progress (`AsyncIOReaderBase` impl state, `0x140081640`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadState {
    Idle,
    ReadingHeader,
    ReadingBody,
    Complete,
}

/// Reader half (`message::Reader` over `AsyncIOReaderBase`, `0x1400814A0`, `0x140081560`, `0x140081A20`).
#[derive(Debug)]
pub struct FrameReader {
    pipe: Box<dyn PipeEndpoint>,
    services: Services,
    state: ReadState,
    header: [u8; FRAME_HEADER_LEN],
    body: Vec<u8>,
}

impl FrameReader {
    /// Wraps the inbound pipe endpoint.
    pub fn new(pipe: Box<dyn PipeEndpoint>, services: Services) -> Self {
        Self { pipe, services, state: ReadState::Idle, header: [0; FRAME_HEADER_LEN], body: Vec::new() }
    }

    /// The pipe was accepted by the OS layer.
    pub fn is_valid(&self) -> bool {
        self.pipe.is_valid()
    }

    /// Pump then advance the frame that is being read (`0x1400814A0`).
    pub fn tick(&mut self) -> bool {
        let pumped = self.pipe.pump();
        match self.state {
            ReadState::ReadingHeader => self.finish_header(),
            ReadState::ReadingBody => self.finish_body(),
            ReadState::Idle | ReadState::Complete => {}
        }
        pumped
    }

    /// Pipe connected.
    pub fn is_connected(&mut self) -> bool {
        self.pipe.is_connected()
    }

    /// The pipe can read and no frame is half-read.
    pub fn can_start_read(&mut self) -> bool {
        self.pipe.can_read() && !self.is_reading()
    }

    /// A header or body read is outstanding (`state ∈ {1, 2}`).
    pub fn is_reading(&self) -> bool {
        matches!(self.state, ReadState::ReadingHeader | ReadState::ReadingBody)
    }

    /// A complete frame is available.
    pub fn has_message(&self) -> bool {
        self.state == ReadState::Complete
    }

    /// Header of the completed frame (unprotected).
    pub fn header(&self) -> FrameHeader {
        FrameHeader::from_bytes(&self.header)
    }

    /// Body of the completed frame, truncated to the logical size.
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// Moves the body buffer out so the caller can process it while mutating other state; hand it back with
    /// [`restore_body`](Self::restore_body) to keep its allocation.
    pub fn take_body(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.body)
    }

    /// Returns a buffer taken with [`take_body`](Self::take_body).
    pub fn restore_body(&mut self, body: Vec<u8>) {
        self.body = body;
    }

    /// `startRead(sync)` (`0x140081560`): starts reading the 16-byte header; with `sync` also polls once and
    /// advances immediately.
    pub fn start_read(&mut self, sync: bool) -> bool {
        if !self.can_start_read() || !self.pipe.read(FRAME_HEADER_LEN) {
            return false;
        }
        self.state = ReadState::ReadingHeader;
        if sync {
            self.pipe.wait(0);
            self.finish_header();
        }
        true
    }

    /// `0x140081A20`: header read finished -> validate it and start the body read.
    fn finish_header(&mut self) {
        if self.pipe.is_io_pending() {
            return;
        }
        self.state = ReadState::Idle;
        if !self.pipe.last_io_ok() {
            return;
        }
        let Some(raw) = self.pipe.read_data().get(..FRAME_HEADER_LEN) else { return };
        self.header.copy_from_slice(raw);
        if !self.services.unprotect(&mut self.header) {
            return;
        }
        let header = self.header();
        if header.size as usize > MAX_BODY_LEN {
            return;
        }
        let padded = header.padded_size();
        if padded == 0 {
            self.body.clear();
            self.state = ReadState::Complete;
            return;
        }
        if !self.pipe.can_read() || !self.pipe.read(padded) {
            return;
        }
        self.state = ReadState::ReadingBody;
        self.pipe.wait(0);
        if self.pipe.is_io_pending() {
            return;
        }
        self.state = ReadState::Idle;
        if self.pipe.last_io_ok() {
            self.complete_body(header);
        }
    }

    /// `0x1400819C0`: body read finished.
    fn finish_body(&mut self) {
        if self.pipe.is_io_pending() {
            return;
        }
        self.state = ReadState::Idle;
        if self.pipe.last_io_ok() {
            self.complete_body(self.header());
        }
    }

    /// `Reader::decodeBody` (`0x140082490`) then truncation to the logical size.
    fn complete_body(&mut self, header: FrameHeader) {
        let padded = header.padded_size();
        let Some(raw) = self.pipe.read_data().get(..padded) else { return };
        self.body.clear();
        self.body.extend_from_slice(raw);
        if header.kind != FrameType::Mention as u32 && !self.services.unprotect(&mut self.body) {
            return;
        }
        self.body.truncate(header.size as usize);
        self.state = ReadState::Complete;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use backend::ipc::IpcPlatform;
    use backend::mock::ipc::protector::XorProtector;
    use backend::mock::ipc::{LoopbackPlatform, ScriptedPipe};

    const KEY: u8 = 0xA5;

    fn services() -> Services {
        LoopbackPlatform::new().with_protector(Arc::new(XorProtector(KEY))).services()
    }

    fn xor(bytes: &[u8]) -> Vec<u8> {
        bytes.iter().map(|b| b ^ KEY).collect()
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 253) as u8 + 1).collect()
    }

    /// Wire bytes of a frame as the original writer produces them (with zero padding).
    fn wire_frame(sequence: u32, kind: u32, body: &[u8], protect_body: bool) -> Vec<u8> {
        let header = FrameHeader { sequence, kind, size: body.len() as u32, pad: 0 };
        let mut frame = xor(&header.to_bytes());
        let mut padded = body.to_vec();
        padded.resize(padded_len(body.len()), 0);
        frame.extend(if protect_body { xor(&padded) } else { padded });
        frame
    }

    #[test]
    fn key_frame_protects_header_and_the_whole_padded_body() {
        let (pipe, written) = ScriptedPipe::writer();
        let mut writer = FrameWriter::new(pipe, services());
        let body = pattern(228);
        assert!(writer.start(FrameType::Key, &body));
        let frames = written.borrow();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].len(), 16 + 240, "228 bytes pad to 240");
        assert_eq!(frames[0], wire_frame(1, 1, &body, true));
        // Spot check against the documented layout: seq=1, type=1, size=228 before protection.
        assert_eq!(xor(&frames[0][..16]), [1, 0, 0, 0, 1, 0, 0, 0, 0xE4, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn mention_frame_protects_only_the_header() {
        let (pipe, written) = ScriptedPipe::writer();
        let mut writer = FrameWriter::new(pipe, services());
        let record = pattern(44);
        assert!(writer.start(FrameType::Mention, &record));
        let frames = written.borrow();
        assert_eq!(frames[0], wire_frame(1, 0, &record, false));
        assert_eq!(&frames[0][16..60], &record[..], "body bytes are sent as they are");
        assert_eq!(frames[0].len(), 64);
        assert_eq!(xor(&frames[0][..16])[4..8], [0, 0, 0, 0]);
    }

    #[test]
    fn sequence_numbers_are_shared_by_all_frames_and_skip_zero_on_wrap() {
        let (pipe, written) = ScriptedPipe::writer();
        let mut writer = FrameWriter::new(pipe, services());
        assert!(writer.start(FrameType::Key, &pattern(16)));
        assert!(writer.start(FrameType::Mention, &pattern(16)));
        writer.sequence = u32::MAX - 1;
        assert!(writer.start(FrameType::Mention, &[]));
        assert!(writer.start(FrameType::Mention, &[]));
        assert!(writer.start(FrameType::Mention, &[]));
        let sequences: Vec<u32> = written
            .borrow()
            .iter()
            .map(|frame| FrameHeader::from_bytes(&xor(&frame[..16]).try_into().unwrap()).sequence)
            .collect();
        assert_eq!(sequences, [1, 2, u32::MAX, 1, 2]);
    }

    #[test]
    fn body_limit_is_exactly_0x30000_and_larger_is_refused() {
        let (pipe, written) = ScriptedPipe::writer();
        let mut writer = FrameWriter::new(pipe, services());
        assert!(!writer.start(FrameType::Mention, &vec![0; MAX_BODY_LEN + 1]));
        assert!(!writer.completed_ok());
        assert!(writer.start(FrameType::Mention, &vec![0; MAX_BODY_LEN]));
        assert_eq!(written.borrow()[0].len(), 16 + MAX_BODY_LEN);
        assert!(writer.completed_ok());
        assert!(writer.start(FrameType::Mention, &[]));
        assert_eq!(written.borrow()[1].len(), 16, "an empty body has no padding");
    }

    #[test]
    fn reader_unprotects_the_header_and_key_bodies_and_truncates_to_the_logical_size() {
        let (pipe, input) = ScriptedPipe::reader();
        let mut reader = FrameReader::new(pipe, services());
        let body = pattern(228);
        input.borrow_mut().extend(wire_frame(7, 1, &body, true));
        assert!(reader.start_read(true));
        assert!(reader.has_message());
        assert_eq!(reader.header(), FrameHeader { sequence: 7, kind: 1, size: 228, pad: 0 });
        assert_eq!(reader.body(), &body[..]);
    }

    #[test]
    fn reader_delivers_mention_bodies_without_unprotecting_them() {
        let (pipe, input) = ScriptedPipe::reader();
        let mut reader = FrameReader::new(pipe, services());
        let record = pattern(44);
        input.borrow_mut().extend(wire_frame(2, 0, &record, false));
        assert!(reader.start_read(true));
        assert_eq!(reader.body(), &record[..]);

        // Any other type is unprotected like a Key (the Messenger then ignores it).
        input.borrow_mut().extend(wire_frame(3, 9, &record, true));
        assert!(reader.start_read(true));
        assert_eq!((reader.header().kind, reader.body()), (9, &record[..]));
    }

    #[test]
    fn oversized_and_empty_frames() {
        let (pipe, input) = ScriptedPipe::reader();
        let mut reader = FrameReader::new(pipe, services());
        let too_big = FrameHeader { sequence: 1, kind: 0, size: MAX_BODY_LEN as u32 + 1, pad: 0 };
        input.borrow_mut().extend(xor(&too_big.to_bytes()));
        assert!(reader.start_read(true));
        assert!(!reader.has_message() && !reader.is_reading(), "dropped; the stream is now desynchronised");

        input.borrow_mut().clear();
        input.borrow_mut().extend(wire_frame(2, 1, &[], true));
        assert!(reader.start_read(true));
        assert!(reader.has_message());
        assert!(reader.body().is_empty());
    }
}
