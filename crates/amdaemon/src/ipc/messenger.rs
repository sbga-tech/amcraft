//! `amdaemon::ipc::Messenger`: the encrypted, framed, bidirectional message channel
//! (`spec/ipc/messenger_crypto.md` §4).
//!
//! A Messenger owns one inbound and one outbound pipe endpoint, a [`Crypto`] session and four small pieces of
//! state: the connection [`State`], the key-send progress, a FIFO of decrypted inbound mentions (at most
//! [`MAX_QUEUED_MENTIONS`]) and a single outbound slot (the newest unsent mention wins).
//!
//! # Callbacks
//!
//! The original registers two `std::function`s: a state-change callback invoked as `(new, old)` on every
//! transition except 1→1, and a `void()` publish callback invoked from `execute` when the Messenger is
//! operational, the writer idle and nothing in flight. Rust cannot let a closure reach back into the owner of
//! the Messenger, so the owner passes a [`MessengerEvents`] implementation to [`Messenger::execute`]:
//! transitions are queued while they happen and delivered in order before the publish callback and at the end
//! of `execute`; the publish callback receives the Messenger as a [`MessageSink`] so it can `put_mention`.
//!
//! # Not implemented
//!
//! `Crypto::Mode::Cbc` and the shared-memory Messenger channel (`mode == 1`) exist in the binary but are never
//! selected: both Messenger factories pass mode 0 and every `encode`/`decode` caller passes mode 0.
//! The unreferenced helpers `takeFront`, `popN`, `isSending` and `clearPendingMention` are omitted too.

use std::collections::VecDeque;

use crate::ipc::crypto::{Crypto, DecodeResult};
use crate::ipc::frame::{FrameReader, FrameType, FrameWriter, MAX_BODY_LEN, MAX_MESSAGES_PER_TICK};
use crate::ipc::stream::MessageSink;
use backend::ipc::IpcPlatform;
use backend::pipe::PipeRole;

/// Largest mention plaintext (`0x24000`).
pub const MAX_MENTION_LEN: usize = 0x24000;
/// Reads stop while this many decrypted mentions wait (`0x40`).
pub const MAX_QUEUED_MENTIONS: usize = 0x40;

/// `Messenger::State` (values direct, names inferred).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u32)]
pub enum State {
    /// Construction failed; terminal.
    Invalid = 0,
    /// Endpoints exist but are not both connected.
    Disconnected = 1,
    /// Both pipes connected; exchanging public keys.
    KeyExchange = 2,
    /// Session established; mentions flow both ways.
    Operational = 3,
}

/// Progress of sending our Key frame (`Impl+0x28`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeySend {
    NotSent,
    InProgress,
    Sent,
}

/// Receiver of Messenger callbacks, passed to [`Messenger::execute`].
pub trait MessengerEvents {
    /// State-change callback (`std::function<void(State, State)>`): `(new, old)`, after the new state is
    /// stored. Never called for 1→1.
    fn state_changed(&mut self, new: State, old: State);

    /// Publish callback (`std::function<void()>`): the Messenger is operational and idle. `sink` is the
    /// Messenger itself; a mention put here is sent immediately.
    fn idle(&mut self, sink: &mut dyn MessageSink);
}

/// `amdaemon::ipc::Messenger`.
#[derive(Debug)]
pub struct Messenger {
    state: State,
    read_in_flight: bool,
    key_send: KeySend,
    crypto: Option<Crypto>,
    reader: Option<FrameReader>,
    writer: Option<FrameWriter>,
    queue: VecDeque<Vec<u8>>,
    mention_pending: bool,
    out_plain: Vec<u8>,
    out_encoded: Vec<u8>,
    write_in_flight: bool,
    pending_events: Vec<(State, State)>,
}

impl Messenger {
    /// Creates a Messenger named `name` (`0x140073250`, mode 0). The daemon is the pipe server of every
    /// Messenger it owns (`is_server`); the peer is the client.
    ///
    /// The Messenger stays in [`State::Invalid`] when key generation fails or an endpoint is unusable (for
    /// example the first-instance pipe is owned by another process).
    pub fn new(platform: &dyn IpcPlatform, name: &str, is_server: bool) -> Self {
        let services = platform.services();
        let mut messenger = Self::inert();
        let Some(crypto) = Crypto::generate() else { return messenger };
        if name.is_empty() {
            tracing::error!("messenger name is empty");
        }
        let role = if is_server { PipeRole::Server } else { PipeRole::Client };
        let reader_name = resource_name(name, is_server, true);
        let writer_name = resource_name(name, is_server, false);
        let reader = FrameReader::new(platform.open_pipe(&reader_name, role, true), services.clone());
        let writer = FrameWriter::new(platform.open_pipe(&writer_name, role, false), services);
        messenger.crypto = Some(crypto);
        if reader.is_valid() && writer.is_valid() {
            messenger.out_plain.reserve(MAX_MENTION_LEN);
            messenger.out_encoded.reserve(MAX_BODY_LEN);
            messenger.state = State::Disconnected;
        }
        messenger.reader = Some(reader);
        messenger.writer = Some(writer);
        messenger
    }

    /// An inert Messenger (`0x140071CF0`): state 0, `execute` returns false, nothing is ever published.
    pub fn inert() -> Self {
        Self {
            state: State::Invalid,
            read_in_flight: false,
            key_send: KeySend::NotSent,
            crypto: None,
            reader: None,
            writer: None,
            queue: VecDeque::new(),
            mention_pending: false,
            out_plain: Vec::new(),
            out_encoded: Vec::new(),
            write_in_flight: false,
            pending_events: Vec::new(),
        }
    }

    /// Current state.
    pub fn state(&self) -> State {
        self.state
    }

    /// One service pass (`0x140075710`). Returns the pipe service result, which the Server ignores.
    pub fn execute(&mut self, events: &mut dyn MessengerEvents) -> bool {
        if self.state == State::Invalid {
            return false;
        }
        let (Some(reader), Some(writer)) = (self.reader.as_mut(), self.writer.as_mut()) else { return false };
        let reader_ok = reader.tick();
        let ok = writer.tick() & reader_ok;
        self.update();
        self.receive();
        match self.state {
            State::KeyExchange => self.handle_key_exchange(),
            State::Operational => self.handle_operational(events),
            State::Invalid | State::Disconnected => {}
        }
        self.flush_events(events);
        ok
    }

    /// `state == Operational && queue not empty`.
    pub fn has_message(&self) -> bool {
        self.state == State::Operational && !self.queue.is_empty()
    }

    /// Oldest decrypted mention; empty unless [`has_message`](Self::has_message).
    pub fn front(&self) -> &[u8] {
        match self.queue.front() {
            Some(front) if self.state == State::Operational => front,
            _ => &[],
        }
    }

    /// Drops the oldest mention; `false` unless [`has_message`](Self::has_message).
    pub fn pop(&mut self) -> bool {
        self.has_message() && self.queue.pop_front().is_some()
    }

    /// Reads up to `max` frames (`0x140075710`, the read loop).
    fn receive(&mut self) {
        let Some(reader) = self.reader.as_mut() else { return };
        if reader.is_reading() {
            return;
        }
        let max = MAX_MESSAGES_PER_TICK.max(1);
        let mut started = 0;
        while started < max {
            if self.read_in_flight {
                self.read_in_flight = false;
                if self.state <= State::Disconnected {
                    break;
                }
                if self.reader.as_ref().is_some_and(FrameReader::has_message) {
                    self.on_message();
                }
            }
            let Some(reader) = self.reader.as_mut() else { return };
            if self.state <= State::Disconnected || !reader.can_start_read() || self.queue.len() >= MAX_QUEUED_MENTIONS
            {
                break;
            }
            started += 1;
            self.read_in_flight = reader.start_read(started < max);
            if !self.read_in_flight {
                self.update();
                break;
            }
            if self.reader.as_ref().is_some_and(FrameReader::is_reading) {
                break;
            }
        }
    }

    /// Inbound dispatch (`0x1400744C0`).
    fn on_message(&mut self) {
        let Some(reader) = self.reader.as_mut() else { return };
        let header = reader.header();
        let body = reader.take_body();
        let connected = self.state > State::Disconnected;
        match FrameType::from_raw(header.kind) {
            Some(FrameType::Mention) if connected && self.crypto.as_ref().is_some_and(Crypto::is_established) => {
                self.apply_mention(&body);
                self.update();
            }
            Some(FrameType::Key) if connected => {
                if self.state != State::KeyExchange {
                    // The peer re-keyed while we were operational: restart our side too.
                    self.set_state(State::KeyExchange);
                }
                if self.crypto.as_mut().is_some_and(|crypto| crypto.process_key(&body)) {
                    self.update();
                }
                self.update();
            }
            // Mentions before the session exists, Keys while disconnected and unknown types are dropped.
            _ => {}
        }
        if let Some(reader) = self.reader.as_mut() {
            reader.restore_body(body);
        }
    }

    /// `applyMentionMessage` (`0x140074370`).
    fn apply_mention(&mut self, body: &[u8]) {
        let Some(crypto) = self.crypto.as_mut() else { return };
        let mut plain = Vec::new();
        match crypto.decode(body, &mut plain) {
            DecodeResult::Success => self.queue.push_back(plain),
            DecodeResult::Malformed | DecodeResult::AuthTagMismatch => {}
            DecodeResult::Reserved => tracing::error!("mention decoder returned a reserved result"),
            DecodeResult::NotReady => tracing::error!("mention arrived before the session key"),
        }
    }

    /// `update` (`0x140074700`): follows endpoint connectivity.
    fn update(&mut self) {
        if self.state == State::Invalid {
            return;
        }
        let (Some(reader), Some(writer)) = (self.reader.as_mut(), self.writer.as_mut()) else { return };
        let connected = reader.is_connected() && writer.is_connected();
        if connected {
            let established = self.crypto.as_ref().is_some_and(Crypto::is_established);
            match self.state {
                State::Disconnected => self.set_state(State::KeyExchange),
                State::KeyExchange if self.key_send == KeySend::Sent && established => {
                    self.set_state(State::Operational);
                }
                State::Operational if !established => self.set_state(State::KeyExchange),
                State::Invalid | State::KeyExchange | State::Operational => {}
            }
        } else {
            if self.state == State::Operational {
                self.clear_operational_state();
            }
            let old = std::mem::replace(&mut self.state, State::Disconnected);
            if old != State::Disconnected {
                self.pending_events.push((State::Disconnected, old));
            }
        }
    }

    /// Leaving the operational state drops everything in flight.
    fn clear_operational_state(&mut self) {
        self.queue.clear();
        self.mention_pending = false;
        self.write_in_flight = false;
    }

    /// `setState` (`0x140074620`).
    fn set_state(&mut self, new: State) {
        if self.state == State::Operational {
            self.clear_operational_state();
        }
        let old = std::mem::replace(&mut self.state, new);
        if !(new == old && new == State::Disconnected) {
            self.pending_events.push((new, old));
        }
        if self.state == State::KeyExchange {
            self.queue.clear();
            self.key_send = KeySend::NotSent;
            if self.crypto.as_ref().is_some_and(Crypto::is_established)
                && let Some(fresh) = Crypto::generate()
            {
                // A fresh context only replaces the old one when it is valid (`0x140079380`).
                self.crypto = Some(fresh);
            }
            self.write_key();
        }
    }

    /// Delivers queued transitions in order.
    fn flush_events(&mut self, events: &mut dyn MessengerEvents) {
        for (new, old) in std::mem::take(&mut self.pending_events) {
            events.state_changed(new, old);
        }
    }

    /// `handleState2` (`0x1400758A0`).
    fn handle_key_exchange(&mut self) {
        match self.key_send {
            KeySend::NotSent => {
                self.write_key();
            }
            KeySend::InProgress => {
                let Some(writer) = self.writer.as_mut() else { return };
                if !writer.is_writing() {
                    self.key_send = if writer.completed_ok() { KeySend::Sent } else { KeySend::NotSent };
                    self.update();
                }
            }
            KeySend::Sent => {}
        }
    }

    /// `writeKey` (`0x1400759F0`): sends our public-key message.
    fn write_key(&mut self) -> bool {
        if self.state != State::KeyExchange || self.key_send != KeySend::NotSent {
            return false;
        }
        let (Some(writer), Some(crypto)) = (self.writer.as_mut(), self.crypto.as_ref()) else { return false };
        if !writer.can_start_write() {
            return false;
        }
        if !writer.start(FrameType::Key, crypto.pub_message()) {
            if !writer.is_connected() {
                self.set_state(State::Disconnected);
            }
            return false;
        }
        if writer.is_writing() {
            self.key_send = KeySend::InProgress;
        } else {
            if !writer.completed_ok() {
                return false;
            }
            self.key_send = KeySend::Sent;
            self.update();
        }
        true
    }

    /// `handleState3` (`0x140075930`).
    fn handle_operational(&mut self, events: &mut dyn MessengerEvents) {
        let Some(writer) = self.writer.as_mut() else { return };
        let proceed = if !self.write_in_flight || writer.is_writing() {
            true
        } else {
            self.write_in_flight = false;
            writer.completed_ok()
        };
        if !proceed {
            // A mention write failed: renegotiate.
            self.set_state(State::KeyExchange);
            return;
        }
        if self.state == State::Operational && !self.write_in_flight && writer.can_start_write() {
            // Transitions that already happened must be visible to the publisher.
            self.flush_events(events);
            events.idle(self);
            self.send_pending();
        }
    }

    /// `sendPending` (`0x140075B10`): encrypts and starts writing the outbound slot.
    fn send_pending(&mut self) -> bool {
        if !self.mention_pending || self.state != State::Operational || self.write_in_flight {
            return false;
        }
        let (Some(writer), Some(crypto)) = (self.writer.as_mut(), self.crypto.as_mut()) else { return false };
        if !writer.can_start_write() {
            return false;
        }
        self.mention_pending = false;
        if !crypto.encode(&self.out_plain, &mut self.out_encoded) || self.out_encoded.len() > MAX_BODY_LEN {
            self.set_state(State::KeyExchange);
            return false;
        }
        if !writer.start(FrameType::Mention, &self.out_encoded) {
            let next = if writer.is_connected() { State::KeyExchange } else { State::Disconnected };
            self.set_state(next);
            return false;
        }
        self.write_in_flight = true;
        true
    }
}

impl MessageSink for Messenger {
    /// `putMention` (`0x140075330`): stores `message` in the single outbound slot (replacing an unsent one) and
    /// starts sending when the writer is idle.
    ///
    /// Returns `false`, storing nothing, when the Messenger is not operational or the mention exceeds
    /// [`MAX_MENTION_LEN`]; also `false` when the immediate send failed. `true` covers both "frame
    /// started" and "kept for the next idle tick".
    fn put_mention(&mut self, message: &[u8]) -> bool {
        if message.len() > MAX_MENTION_LEN {
            tracing::error!(len = message.len(), limit = MAX_MENTION_LEN, "mention is too large");
            return false;
        }
        if self.state != State::Operational {
            return false;
        }
        self.out_plain.clear();
        self.out_plain.extend_from_slice(message);
        self.mention_pending = true;
        let can_send = !self.write_in_flight && self.writer.as_mut().is_some_and(FrameWriter::can_start_write);
        !can_send || self.send_pending()
    }
}

/// `anon::makeResourceName(name, a, b, suffix = "")` (`0x140081F30`): `name + tag` with `_amdmsg_CS` when
/// `a == b` and `_amdmsg_SC` otherwise. An empty name yields an empty string.
fn resource_name(name: &str, is_server: bool, is_reader: bool) -> String {
    if name.is_empty() {
        return String::new();
    }
    let tag = if is_server == is_reader { "_amdmsg_CS" } else { "_amdmsg_SC" };
    format!("{name}{tag}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::frame::FrameHeader;
    use backend::mock::ipc::LoopbackPlatform;

    /// Records callbacks and optionally publishes queued mentions when idle.
    #[derive(Default)]
    struct Probe {
        transitions: Vec<(State, State)>,
        idle_calls: usize,
        outbox: VecDeque<Vec<u8>>,
    }

    impl MessengerEvents for Probe {
        fn state_changed(&mut self, new: State, old: State) {
            self.transitions.push((new, old));
        }

        fn idle(&mut self, sink: &mut dyn MessageSink) {
            self.idle_calls += 1;
            if let Some(message) = self.outbox.pop_front() {
                assert!(sink.put_mention(&message));
            }
        }
    }

    fn drain(messenger: &mut Messenger) -> Vec<Vec<u8>> {
        let mut messages = Vec::new();
        while messenger.has_message() {
            messages.push(messenger.front().to_vec());
            assert!(messenger.pop());
        }
        messages
    }

    /// A daemon-side and a game-side Messenger on one shared in-process hub.
    struct Pair {
        platform: LoopbackPlatform,
        server: Messenger,
        client: Messenger,
        server_probe: Probe,
        client_probe: Probe,
    }

    impl Pair {
        fn new() -> Self {
            let platform = LoopbackPlatform::new();
            let server = Messenger::new(&platform, "SC0", true);
            let client = Messenger::new(&platform, "SC0", false);
            Self { platform, server, client, server_probe: Probe::default(), client_probe: Probe::default() }
        }

        fn step(&mut self) {
            self.server.execute(&mut self.server_probe);
            self.client.execute(&mut self.client_probe);
        }

        fn run_until(&mut self, what: &str, done: impl Fn(&Self) -> bool) {
            for _ in 0..100 {
                if done(self) {
                    return;
                }
                self.step();
            }
            panic!("never reached: {what}");
        }

        fn operational() -> Self {
            let mut pair = Self::new();
            pair.run_until("both operational", |p| {
                p.server.state() == State::Operational && p.client.state() == State::Operational
            });
            pair
        }
    }

    /// A hand-driven game side: raw frames, own crypto, no state machine.
    struct ManualPeer {
        reader: FrameReader,
        writer: FrameWriter,
        crypto: Crypto,
    }

    impl ManualPeer {
        fn new(platform: &LoopbackPlatform, name: &str) -> Self {
            let services = platform.services();
            let reader = platform.open_pipe(&format!("{name}_amdmsg_SC"), PipeRole::Client, true);
            let writer = platform.open_pipe(&format!("{name}_amdmsg_CS"), PipeRole::Client, false);
            let reader = FrameReader::new(reader, services.clone());
            Self { reader, writer: FrameWriter::new(writer, services), crypto: Crypto::generate().unwrap() }
        }

        /// Next complete frame, if one arrived.
        fn receive(&mut self) -> Option<(FrameHeader, Vec<u8>)> {
            self.reader.tick();
            if self.reader.has_message() {
                let frame = (self.reader.header(), self.reader.body().to_vec());
                self.reader.start_read(true);
                return Some(frame);
            }
            if !self.reader.is_reading() {
                self.reader.start_read(true);
            }
            None
        }

        fn tick(&mut self) {
            self.writer.tick();
        }

        fn send_key(&mut self) -> bool {
            self.writer.start(FrameType::Key, self.crypto.pub_message())
        }

        fn send_mention(&mut self, plain: &[u8]) -> bool {
            let mut record = Vec::new();
            assert!(self.crypto.encode(plain, &mut record));
            self.writer.start(FrameType::Mention, &record)
        }
    }

    /// Runs the server against a hand-driven peer until the key exchange is complete on both sides.
    fn handshake(server: &mut Messenger, probe: &mut Probe, peer: &mut ManualPeer) {
        let mut key_sent = false;
        for _ in 0..100 {
            server.execute(probe);
            peer.tick();
            if let Some((header, body)) = peer.receive()
                && header.kind == FrameType::Key as u32
            {
                assert!(peer.crypto.process_key(&body));
            }
            if !key_sent {
                key_sent = peer.send_key();
            }
            if server.state() == State::Operational && peer.crypto.is_established() {
                return;
            }
        }
        panic!("handshake did not complete, server state {:?}", server.state());
    }

    #[test]
    fn a_lone_server_waits_in_disconnected_without_reporting_transitions() {
        let platform = LoopbackPlatform::new();
        let mut server = Messenger::new(&platform, "SC0", true);
        let mut probe = Probe::default();
        assert_eq!(server.state(), State::Disconnected);
        for _ in 0..5 {
            assert!(server.execute(&mut probe));
        }
        assert_eq!(server.state(), State::Disconnected);
        assert!(probe.transitions.is_empty(), "1 -> 1 is not a transition");
        assert!(!server.put_mention(b"nobody home"));
    }

    #[test]
    fn handshake_takes_both_sides_through_key_exchange_to_operational() {
        let pair = Pair::operational();
        let expected = [(State::KeyExchange, State::Disconnected), (State::Operational, State::KeyExchange)];
        assert_eq!(pair.server_probe.transitions, expected);
        assert_eq!(pair.client_probe.transitions, expected);
    }

    #[test]
    fn mentions_flow_both_ways_in_order() {
        let mut pair = Pair::operational();
        pair.server_probe.outbox.extend([b"daemon says hi".to_vec()]);
        pair.client_probe.outbox.extend([b"game says hi".to_vec(), b"and again".to_vec()]);
        let (mut at_server, mut at_client) = (Vec::new(), Vec::new());
        for _ in 0..60 {
            pair.step();
            at_server.extend(drain(&mut pair.server));
            at_client.extend(drain(&mut pair.client));
        }
        assert_eq!(at_client, [b"daemon says hi".to_vec()]);
        assert_eq!(at_server, [b"game says hi".to_vec(), b"and again".to_vec()]);
    }

    #[test]
    fn the_outbound_slot_keeps_only_the_newest_unsent_mention() {
        let mut pair = Pair::operational();
        pair.server_probe.outbox.push_back(b"first".to_vec());
        // The idle callback sends "first"; the frame stays in flight until the peer reads it.
        pair.server.execute(&mut pair.server_probe);
        assert!(pair.server.put_mention(b"second"), "stored for the next idle tick");
        assert!(pair.server.put_mention(b"third"), "replaces the unsent one");
        let mut received = Vec::new();
        for _ in 0..40 {
            pair.step();
            received.extend(drain(&mut pair.client));
        }
        assert_eq!(received, [b"first".to_vec(), b"third".to_vec()]);
    }

    #[test]
    fn put_mention_rejects_oversize_and_non_operational() {
        let mut pair = Pair::new();
        assert!(!pair.server.put_mention(b"too early"));
        assert!(!pair.server.put_mention(&vec![0; MAX_MENTION_LEN + 1]));

        pair.run_until("operational", |p| p.server.state() == State::Operational);
        assert!(pair.server.put_mention(&vec![7; MAX_MENTION_LEN]), "the limit itself is allowed");
    }

    #[test]
    fn the_largest_mention_survives_framing_and_encryption() {
        let mut pair = Pair::operational();
        let big: Vec<u8> = (0..MAX_MENTION_LEN).map(|i| (i % 251) as u8).collect();
        pair.server_probe.outbox.push_back(big.clone());
        let mut received = Vec::new();
        for _ in 0..40 {
            pair.step();
            received.extend(drain(&mut pair.client));
        }
        assert_eq!(received, [big]);
    }

    #[test]
    fn reading_stops_while_sixty_four_mentions_are_queued() {
        let platform = LoopbackPlatform::new();
        let mut server = Messenger::new(&platform, "SC0", true);
        let mut probe = Probe::default();
        let mut peer = ManualPeer::new(&platform, "SC0");
        handshake(&mut server, &mut probe, &mut peer);

        let mut sent = 0;
        for _ in 0..600 {
            server.execute(&mut probe);
            peer.tick();
            if sent < 80 && peer.send_mention(&[sent as u8; 20]) {
                sent += 1;
            }
            assert!(server.queue.len() <= MAX_QUEUED_MENTIONS);
        }
        assert_eq!(server.queue.len(), MAX_QUEUED_MENTIONS, "the consumer never popped");
        assert!(sent < 80, "the peer was held back by the full queue");
        let mut delivered = 0;
        while server.has_message() {
            assert_eq!(server.front(), [delivered as u8; 20]);
            assert!(server.pop());
            delivered += 1;
        }
        assert_eq!(delivered, MAX_QUEUED_MENTIONS);
        assert!(server.front().is_empty() && !server.pop());
    }

    #[test]
    fn a_tampered_mention_is_dropped_silently_and_desynchronises_the_stream_until_rekey() {
        let platform = LoopbackPlatform::new();
        let mut server = Messenger::new(&platform, "SC0", true);
        let mut probe = Probe::default();
        let mut peer = ManualPeer::new(&platform, "SC0");
        handshake(&mut server, &mut probe, &mut peer);
        let transitions = probe.transitions.len();

        // Send a record whose tag is corrupted, then a genuine one.
        let mut record = Vec::new();
        assert!(peer.crypto.encode(b"corrupt me", &mut record));
        *record.last_mut().unwrap() ^= 1;
        let delivered = std::cell::RefCell::new(Vec::<Vec<u8>>::new());
        let tick = |server: &mut Messenger, peer: &mut ManualPeer, probe: &mut Probe| {
            server.execute(probe);
            peer.tick();
            delivered.borrow_mut().extend(drain(server));
        };
        while !peer.writer.start(FrameType::Mention, &record) {
            tick(&mut server, &mut peer, &mut probe);
        }
        for _ in 0..10 {
            tick(&mut server, &mut peer, &mut probe);
        }
        while !peer.send_mention(b"genuine but late") {
            tick(&mut server, &mut peer, &mut probe);
        }
        for _ in 0..10 {
            tick(&mut server, &mut peer, &mut probe);
        }
        assert!(delivered.borrow().is_empty(), "nothing authenticates after a bad tag: nonces are out of step");
        assert_eq!(server.state(), State::Operational);
        assert_eq!(probe.transitions.len(), transitions, "no state change, no disconnect");

        // A fresh Key frame renegotiates and the stream works again.
        peer.crypto = Crypto::generate().unwrap();
        while !peer.send_key() {
            tick(&mut server, &mut peer, &mut probe);
        }
        for _ in 0..60 {
            tick(&mut server, &mut peer, &mut probe);
            if let Some((header, body)) = peer.receive()
                && header.kind == FrameType::Key as u32
            {
                assert!(peer.crypto.process_key(&body));
            }
            if server.state() == State::Operational
                && peer.crypto.is_established()
                && probe.transitions.len() > transitions
            {
                break;
            }
        }
        assert_eq!(
            &probe.transitions[transitions..],
            [(State::KeyExchange, State::Operational), (State::Operational, State::KeyExchange)]
        );
        while !peer.send_mention(b"after rekey") {
            tick(&mut server, &mut peer, &mut probe);
        }
        for _ in 0..10 {
            tick(&mut server, &mut peer, &mut probe);
        }
        assert_eq!(*delivered.borrow(), [b"after rekey".to_vec()]);
    }

    #[test]
    fn losing_the_peer_returns_to_disconnected_and_a_new_peer_gets_fresh_keys() {
        let mut pair = Pair::operational();
        pair.server_probe.outbox.push_back(b"queued".to_vec());
        let Pair { platform, mut server, client, mut server_probe, .. } = pair;
        drop(client);
        // The next I/O notices the broken pipe.
        for _ in 0..5 {
            server.execute(&mut server_probe);
        }
        assert_eq!(server.state(), State::Disconnected);
        assert_eq!(server_probe.transitions.last(), Some(&(State::Disconnected, State::Operational)));
        assert!(server.queue.is_empty());
        assert!(!server.put_mention(b"nobody"));

        let first_key = server.crypto.as_ref().unwrap().pub_message().to_vec();
        let mut peer = ManualPeer::new(&platform, "SC0");
        let mut seen_key = None;
        for _ in 0..60 {
            server.execute(&mut server_probe);
            peer.tick();
            if let Some((header, body)) = peer.receive() {
                assert_eq!(header.kind, FrameType::Key as u32);
                seen_key = Some(body);
                break;
            }
        }
        assert_eq!(server.state(), State::KeyExchange);
        let seen_key = seen_key.expect("the daemon sent a Key frame");
        assert_ne!(seen_key, first_key, "an established context is replaced before a new session");
        assert_eq!(seen_key, server.crypto.as_ref().unwrap().pub_message());
    }

    #[test]
    fn unusable_messengers_stay_inert() {
        let platform = LoopbackPlatform::new();
        let first = Messenger::new(&platform, "SC0", true);
        // The first-instance pipe is taken: construction fails like ERROR_ACCESS_DENIED.
        let mut second = Messenger::new(&platform, "SC0", true);
        assert_eq!(first.state(), State::Disconnected);
        assert_eq!(second.state(), State::Invalid);
        let mut probe = Probe::default();
        assert!(!second.execute(&mut probe));
        assert!(!second.put_mention(b"x"));
        assert!(!second.has_message() && second.front().is_empty() && !second.pop());
        assert!(probe.transitions.is_empty() && probe.idle_calls == 0);

        let unnamed = Messenger::new(&platform, "", true);
        assert_eq!(unnamed.state(), State::Invalid);

        let inert = Messenger::inert();
        assert_eq!(inert.state(), State::Invalid);
    }

    #[test]
    fn idle_runs_once_per_execute_only_while_operational_and_idle() {
        let mut pair = Pair::operational();
        let before = pair.server_probe.idle_calls;
        pair.server.execute(&mut pair.server_probe);
        assert_eq!(pair.server_probe.idle_calls, before + 1);
        // While a frame is in flight the writer is busy and the callback is skipped.
        pair.server_probe.outbox.push_back(b"busy".to_vec());
        pair.server.execute(&mut pair.server_probe);
        let during = pair.server_probe.idle_calls;
        pair.server.execute(&mut pair.server_probe);
        assert_eq!(pair.server_probe.idle_calls, during, "write in flight");
    }
}
