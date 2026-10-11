//! `amdaemon::ipc::Server`: the daemon end of the IPC (`spec/ipc/server.md` §1, §4–§6).
//!
//! A `Server` owns the live `ServerData`, `ClientData` and ServerIO images, the relation (index claim plus the
//! primary and browser Messengers), the two publication state machines and the ServerIO sender. Controllers
//! read and write the images between ticks ([`Server::roots_mut`]); [`Server::execute`] then publishes the
//! changes and merges what the game sent.

use crate::schema::image::{ClientImage, ServerImage, ServerIoImage};

use crate::ipc::messenger::{MessengerEvents, State};
use crate::ipc::relation::Relation;
use crate::ipc::serverio::Sender;
use crate::ipc::stream::{MessageSink, PrimaryPhase, PrimaryStream, SecondaryStream, apply_primary_inbound};
use backend::ipc::IpcPlatform;

/// Mutable view of the three images handed to controllers each tick.
#[derive(Debug)]
pub struct RootsMut<'a> {
    /// Live `ServerData`.
    pub server: &'a mut ServerImage,
    /// Live `ClientData` (written only by inbound kind-2 messages).
    pub client: &'a ClientImage,
    /// ServerIO body.
    pub server_io: &'a mut ServerIoImage,
}

/// Callbacks of the primary Messenger (lambdas `e0016311` / `54ce01f1`, `0x14006D060` / `0x140076580`).
struct PrimaryEvents<'a> {
    stream: &'a mut PrimaryStream,
    sender: &'a mut Sender,
    server: &'a [u8],
    client: &'a [u8],
}

impl MessengerEvents for PrimaryEvents<'_> {
    /// Every transition restarts the stream from the hello and blanks the ServerIO region.
    fn state_changed(&mut self, _new: State, _old: State) {
        self.stream.reset();
        self.sender.reset();
    }

    fn idle(&mut self, sink: &mut dyn MessageSink) {
        self.stream.publish(sink, self.server, self.client);
    }
}

/// Callbacks of the secondary (browser) Messenger (lambdas `9b526473` / `b8e5c8fc`, `0x14006D040` /
/// `0x14006D050`).
struct SecondaryEvents<'a> {
    stream: &'a mut SecondaryStream,
    server: &'a [u8],
    client: &'a [u8],
}

impl MessengerEvents for SecondaryEvents<'_> {
    fn state_changed(&mut self, _new: State, _old: State) {
        self.stream.reset();
    }

    fn idle(&mut self, sink: &mut dyn MessageSink) {
        self.stream.publish(sink, self.server, self.client);
    }
}

/// `amdaemon::ipc::Server`.
#[derive(Debug)]
pub struct Server {
    // Declaration order is destruction order: the original frees the secondary vector, the ServerIO sender,
    // the primary vector and finally the relation (which destroys the Messengers and releases the index).
    secondary_stream: SecondaryStream,
    sender: Sender,
    primary_stream: PrimaryStream,
    relation: Relation,
    server: ServerImage,
    client: ClientImage,
}

impl Server {
    /// Constructs the server (`0x14006C6B0`): zeroed images, relation (index claim and Messengers, inert when
    /// no index could be claimed) and the ServerIO sender for the claimed index.
    pub fn new(platform: Box<dyn IpcPlatform>) -> Self {
        let relation = Relation::new(platform.as_ref(), true);
        let sender = Sender::new(platform.as_ref(), relation.index());
        Self {
            secondary_stream: SecondaryStream::default(),
            sender,
            primary_stream: PrimaryStream::default(),
            relation,
            server: ServerImage::new(),
            client: ClientImage::new(),
        }
    }

    /// Claimed relation index (`0..32`), `None` when the relation table could not be claimed.
    pub fn relation_index(&self) -> Option<u32> {
        self.relation.index()
    }

    /// Borrow of the three images for one controller pass.
    pub fn roots_mut(&mut self) -> RootsMut<'_> {
        RootsMut { server: &mut self.server, client: &self.client, server_io: self.sender.body_mut() }
    }

    /// Live `ServerData`.
    pub fn server_data(&self) -> &ServerImage {
        &self.server
    }

    /// Live `ClientData`.
    pub fn client_data(&self) -> &ClientImage {
        &self.client
    }

    /// One `Server::execute` transaction (`0x14006D410`).
    ///
    /// 1. While the primary stream is streaming and the primary Messenger is operational, republish ServerIO.
    /// 2. Run the primary Messenger (its idle callback publishes) and apply every queued kind-2 message to the
    ///    live `ClientData`.
    /// 3. Run the secondary Messenger and feed its queued messages to the request state machine.
    pub fn execute(&mut self) {
        if self.primary_stream.phase() == PrimaryPhase::Streaming
            && self.relation.primary().state() == State::Operational
        {
            self.sender.tick();
        }

        let mut events = PrimaryEvents {
            stream: &mut self.primary_stream,
            sender: &mut self.sender,
            server: self.server.bytes(),
            client: self.client.bytes(),
        };
        let primary = self.relation.primary_mut();
        primary.execute(&mut events);
        while primary.has_message() {
            let message = primary.front();
            if message.len() >= crate::ipc::header::HEADER_LEN {
                // A rejected message leaves the image untouched, exactly like the original decoder.
                let _ = apply_primary_inbound(message, self.client.bytes_mut());
            }
            if !primary.pop() {
                break;
            }
        }

        let mut events = SecondaryEvents {
            stream: &mut self.secondary_stream,
            server: self.server.bytes(),
            client: self.client.bytes(),
        };
        let secondary = self.relation.secondary_mut();
        secondary.execute(&mut events);
        while secondary.has_message() {
            let message = secondary.front();
            if message.len() >= crate::ipc::header::HEADER_LEN {
                self.secondary_stream.on_inbound(message);
            }
            if !secondary.pop() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::schema::image::{CLIENT_DATA_SIZE, SERVER_DATA_SIZE, SERVER_IO_BODY_SIZE};
    use crate::schema::text::fit_wide;

    use super::*;
    use crate::ipc::delta::{client_delta, server_delta};
    use crate::ipc::header::{HELLO_TOKEN, Header, Kind, decode_into};
    use crate::ipc::messenger::Messenger;
    use crate::ipc::stream::{ClientPhase, ClientStream};
    use backend::mock::ipc::LoopbackPlatform;
    use backend::mock::ipc::protector::XorProtector;

    /// Writes the accounting `mode` (`ServerData+0x0C`) through its checked block: a change near the start.
    fn set_head(server: &mut ServerImage, mode: u32) {
        server.allnet_accounting_mut().mode_mut().set(mode).expect("u32 accepts every value");
    }

    /// Offset of `ServerData.process.bootClock`.
    const BOOT_CLOCK_OFFSET: usize = 0x38C8;

    /// Writes the process boot clock (`ServerData+0x38C8`) through its checked block.
    fn set_boot_clock(server: &mut ServerImage, clock: i64) {
        server.process_mut().boot_clock_mut().set(clock).expect("i64 accepts every value");
    }

    /// Writes the common player count (`ServerData+0x902C`) through its checked block.
    fn set_player_count(server: &mut ServerImage, count: u32) {
        server.common_mut().player_count_mut().set(count).expect("u32 accepts every value");
    }

    /// Fills the option root path, the last field of `ServerData` (it ends at the end of the image).
    fn set_tail(server: &mut ServerImage) {
        let mut block = server.app_image_mut();
        let mut field = block.option_root_path_mut();
        let text: Vec<u16> = (0..).map(|i| 0x41 + (i % 26) as u16).take(field.capacity()).collect();
        let fitted = fit_wide(&text, field.capacity());
        field.set(&fitted).expect("fitted to the field");
    }

    fn message(kind: u32, body: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for word in [kind, body.len() as u32, 0, 0] {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        bytes.extend_from_slice(body);
        bytes
    }

    /// The game end of the primary pipe pair, speaking the recovered client contract.
    struct Game {
        messenger: Messenger,
        stream: ClientStream,
        server: Vec<u8>,
        client: Vec<u8>,
        /// `(kind, body_size)` of every inbound message.
        received: Vec<(u32, u32)>,
    }

    struct GameEvents<'a> {
        stream: &'a mut ClientStream,
        client: &'a [u8],
    }

    impl MessengerEvents for GameEvents<'_> {
        fn state_changed(&mut self, _new: State, _old: State) {
            self.stream.reset();
        }

        fn idle(&mut self, sink: &mut dyn MessageSink) {
            self.stream.publish(sink, self.client);
        }
    }

    impl Game {
        fn connect(platform: &LoopbackPlatform, index: u32) -> Self {
            Self {
                messenger: Messenger::new(platform, &format!("SC{index}"), false),
                stream: ClientStream::default(),
                server: vec![0; SERVER_DATA_SIZE],
                client: vec![0; CLIENT_DATA_SIZE],
                received: Vec::new(),
            }
        }

        fn step(&mut self) {
            let mut events = GameEvents { stream: &mut self.stream, client: &self.client };
            self.messenger.execute(&mut events);
            while self.messenger.has_message() {
                let message = self.messenger.front();
                if let Some(header) = Header::parse(message) {
                    self.received.push((header.kind, header.body_size));
                }
                self.stream.on_reply(message, &mut self.server, &mut self.client).unwrap();
                assert!(self.messenger.pop());
            }
        }
    }

    /// A browser peer of the secondary pipe pair: requests deltas with header-only messages.
    struct Browser {
        messenger: Messenger,
        server: Vec<u8>,
        client: Vec<u8>,
        snapshot_done: bool,
        request: Option<Vec<u8>>,
        /// `(kind, body_size, client_size, server_size)` of every reply after the snapshot.
        replies: Vec<(u32, u32, u32, u32)>,
    }

    struct BrowserEvents<'a> {
        request: &'a mut Option<Vec<u8>>,
    }

    impl MessengerEvents for BrowserEvents<'_> {
        fn state_changed(&mut self, _new: State, _old: State) {}

        fn idle(&mut self, sink: &mut dyn MessageSink) {
            if let Some(request) = self.request.take() {
                assert!(sink.put_mention(&request));
            }
        }
    }

    impl Browser {
        fn connect(platform: &LoopbackPlatform, index: u32) -> Self {
            Self {
                messenger: Messenger::new(platform, &format!("BS{index}"), false),
                server: vec![0; SERVER_DATA_SIZE],
                client: vec![0; CLIENT_DATA_SIZE],
                snapshot_done: false,
                request: None,
                replies: Vec::new(),
            }
        }

        fn step(&mut self) {
            self.messenger.execute(&mut BrowserEvents { request: &mut self.request });
            while self.messenger.has_message() {
                let message = self.messenger.front();
                let header = Header::parse(message).unwrap();
                match (Kind::from_raw(header.kind), self.snapshot_done) {
                    (Some(Kind::Hello), _) => {
                        assert_eq!(message[16..24], HELLO_TOKEN.to_le_bytes());
                    }
                    (Some(Kind::Combined), false) => {
                        decode_into(message, &mut self.server, &mut self.client, true).unwrap();
                        self.snapshot_done = true;
                    }
                    (Some(_), true) => {
                        decode_into(message, &mut self.server, &mut self.client, false).unwrap();
                        self.replies.push((header.kind, header.body_size, header.client_size, header.server_size));
                    }
                    other => panic!("unexpected message {other:?}"),
                }
                assert!(self.messenger.pop());
            }
        }
    }

    /// A daemon `Server` with a game attached to its primary pipes.
    struct Rig {
        platform: LoopbackPlatform,
        server: Server,
        game: Game,
    }

    impl Rig {
        fn new() -> Self {
            Self::on(LoopbackPlatform::new())
        }

        fn on(platform: LoopbackPlatform) -> Self {
            let server = Server::new(Box::new(platform.clone()));
            let index = server.relation_index().expect("a free relation index");
            let game = Game::connect(&platform, index);
            Self { platform, server, game }
        }

        fn step(&mut self) {
            self.server.execute();
            self.game.step();
        }

        fn run(&mut self, steps: usize) {
            for _ in 0..steps {
                self.step();
            }
        }

        fn streaming() -> Self {
            let mut rig = Self::new();
            rig.run_until_streaming();
            rig
        }

        fn run_until_streaming(&mut self) {
            for _ in 0..100 {
                if self.game.stream.phase() == ClientPhase::Streaming {
                    return;
                }
                self.step();
            }
            panic!("the game never reached the streaming phase");
        }

        fn live_server(&self) -> &[u8] {
            self.server.server_data().bytes()
        }
    }

    #[test]
    fn the_server_claims_an_index_and_the_game_receives_hello_then_the_full_image() {
        let mut rig = Rig::new();
        assert_eq!(rig.server.relation_index(), Some(0));
        {
            let roots = rig.server.roots_mut();
            set_head(roots.server, 0x0102_0304);
            set_boot_clock(roots.server, 0x1122_3344_5566_7788);
            set_tail(roots.server);
        }
        for (i, byte) in rig.server.client.bytes_mut().iter_mut().enumerate() {
            *byte = (i * 17 + 3) as u8;
        }
        rig.run_until_streaming();
        rig.run(4);

        assert_eq!(rig.game.received, [(0, 8), (3, (CLIENT_DATA_SIZE + SERVER_DATA_SIZE) as u32)]);
        assert_eq!(rig.game.server, rig.live_server());
        assert_eq!(rig.game.client, rig.server.client_data().bytes());
        assert_ne!(rig.game.server[SERVER_DATA_SIZE - 4], 0, "the image arrives up to its last field");
    }

    #[test]
    fn changed_server_data_is_streamed_as_the_shortest_changed_prefix() {
        let mut rig = Rig::streaming();
        rig.run(6);
        let sent = rig.game.received.len();
        rig.run(6);
        assert_eq!(rig.game.received.len(), sent, "an unchanged image is not republished");

        let mirror = rig.game.server.clone();
        {
            let roots = rig.server.roots_mut();
            set_boot_clock(roots.server, 0x77);
            set_head(roots.server, 0x11);
        }
        let expected = server_delta(rig.live_server(), &mirror);
        assert!(expected > BOOT_CLOCK_OFFSET && expected < SERVER_DATA_SIZE);
        rig.run(8);
        assert_eq!(&rig.game.received[sent..], [(1, expected as u32)]);
        assert_eq!(rig.game.server, rig.live_server());

        // Only the early part changes: a much shorter prefix.
        set_head(rig.server.roots_mut().server, 0x22);
        let expected = server_delta(rig.live_server(), &rig.game.server);
        assert!(expected < 0x100);
        rig.run(8);
        assert_eq!(rig.game.received.last(), Some(&(1, expected as u32)));
        assert_eq!(rig.game.server, rig.live_server());
    }

    #[test]
    fn client_data_written_by_the_game_is_merged_by_prefix_and_never_echoed() {
        let mut rig = Rig::streaming();
        rig.run(4);
        let inbound = rig.game.received.len();
        rig.game.client[0x1000] = 9;
        rig.game.client[0x10] = 3;
        rig.run(10);
        assert_eq!(rig.server.client_data().bytes(), rig.game.client);
        assert_eq!(rig.server.client_data().bytes()[0x1000], 9);
        assert_eq!(rig.game.received.len(), inbound, "ClientData is never republished on the primary stream");

        let before = client_delta(&rig.game.client, rig.server.client_data().bytes());
        assert_eq!(before, 0, "daemon and game agree");
    }

    #[test]
    fn only_bounded_kind_2_messages_change_the_live_client_data() {
        let mut rig = Rig::streaming();
        rig.run(4);
        let send = |rig: &mut Rig, bytes: &[u8]| {
            for _ in 0..20 {
                if rig.game.messenger.put_mention(bytes) {
                    break;
                }
                rig.step();
            }
            rig.run(8);
        };
        send(&mut rig, &message(2, &[0xEE; 0x40]));
        assert!(rig.server.client_data().bytes()[..0x40].iter().all(|&b| b == 0xEE));
        assert_eq!(rig.server.client_data().bytes()[0x40], 0);

        let snapshot = rig.server.client_data().bytes().to_vec();
        send(&mut rig, &message(2, &vec![0x55; CLIENT_DATA_SIZE + 1]));
        send(&mut rig, &message(1, &[0x55; 0x40]));
        send(&mut rig, &message(3, &[0x55; 0x40]));
        send(&mut rig, &message(7, &[0x55; 0x40]));
        let mut lying = message(2, &[0x55; 0x20]);
        lying[4..8].copy_from_slice(&0x40u32.to_le_bytes());
        send(&mut rig, &lying);
        send(&mut rig, &[2, 0, 0, 0]);
        assert_eq!(rig.server.client_data().bytes(), snapshot);
    }

    #[test]
    fn a_reconnecting_game_restarts_from_the_hello_and_sees_the_current_image() {
        let mut rig = Rig::streaming();
        rig.run(4);
        let platform = rig.platform.clone();
        let Rig { mut server, game, .. } = rig;
        drop(game);
        for _ in 0..6 {
            server.execute();
        }
        assert_eq!(server.primary_stream.phase(), PrimaryPhase::Hello);
        set_boot_clock(server.roots_mut().server, 0x42);

        let mut rig = Rig { platform: platform.clone(), game: Game::connect(&platform, 0), server };
        rig.run_until_streaming();
        assert_eq!(rig.game.received.iter().map(|m| m.0).collect::<Vec<_>>(), [0, 3]);
        assert_eq!(rig.game.server[BOOT_CLOCK_OFFSET..BOOT_CLOCK_OFFSET + 8], 0x42i64.to_le_bytes());
        assert_eq!(rig.game.server, rig.live_server());
    }

    #[test]
    fn server_io_is_published_with_crc_while_streaming_and_blanked_on_every_restart() {
        let platform = LoopbackPlatform::new().with_protector(Arc::new(XorProtector(0x5A)));
        let mut rig = Rig::on(platform.clone());
        let region = |platform: &LoopbackPlatform| {
            let section = platform.open_section("amdipc_shm_daemon_svio_shm_0").unwrap();
            let mut bytes = vec![0; section.byte_len()];
            assert!(section.read(0, &mut bytes));
            bytes
        };
        let blank = region(&platform);
        assert_eq!(blank.len(), 8 + 0x1670);
        assert_eq!(blank[..8], [0xE2, 0x0C, 0xF9, 0x15, 0x70, 0x16, 0, 0]);
        assert!(blank[8..].iter().all(|&b| b == 0));

        {
            let io = rig.server.roots_mut().server_io;
            io.usb_io_mut().firmware_progress_mut().set(0x0102_0304).expect("u32 accepts every value");
            io.jvs_mut().detect_counter_mut().set(0x0506_0708).expect("u32 accepts every value");
        }
        // Nothing is published before the hello and the full image have been sent.
        for _ in 0..100 {
            if rig.server.primary_stream.phase() == PrimaryPhase::Streaming {
                break;
            }
            assert_eq!(region(&platform), blank, "region stays blank before the stream is up");
            rig.step();
        }
        rig.run(4);
        assert!(rig.server.sender.publish_count() > 0);

        let published = region(&platform);
        let packet: Vec<u8> = published[8..].iter().map(|b| b ^ 0x5A).collect();
        let body = rig.server.sender.body().bytes().to_vec();
        assert_eq!(body.len(), SERVER_IO_BODY_SIZE);
        assert_eq!(packet[4..4 + SERVER_IO_BODY_SIZE], body[..]);
        assert_eq!(u32::from_le_bytes(packet[..4].try_into().unwrap()), crc32fast::hash(&body));

        // Every tick republishes the live body.
        rig.server
            .roots_mut()
            .server_io
            .usb_io_mut()
            .firmware_progress_mut()
            .set(0xFF)
            .expect("u32 accepts every value");
        rig.run(2);
        let packet: Vec<u8> = region(&platform)[8..].iter().map(|b| b ^ 0x5A).collect();
        let body = rig.server.sender.body().bytes();
        assert_eq!(packet[4..4 + SERVER_IO_BODY_SIZE], body[..]);
        assert_eq!(u32::from_le_bytes(packet[..4].try_into().unwrap()), crc32fast::hash(body));

        // Losing the game restarts the stream and blanks the region (primary state-change callback).
        let Rig { mut server, game, .. } = rig;
        drop(game);
        for _ in 0..6 {
            server.execute();
        }
        assert_eq!(region(&platform), blank);
        assert_eq!(server.sender.publish_count(), 0);
    }

    #[test]
    fn thirty_two_daemons_get_distinct_indices_and_a_released_one_is_reused() {
        let platform = LoopbackPlatform::new();
        let mut servers: Vec<Server> = (0..32).map(|_| Server::new(Box::new(platform.clone()))).collect();
        let indices: Vec<Option<u32>> = servers.iter().map(Server::relation_index).collect();
        assert_eq!(indices, (0..32).map(Some).collect::<Vec<_>>());

        let mut extra = Server::new(Box::new(platform.clone()));
        assert_eq!(extra.relation_index(), None, "no index left");
        set_head(extra.roots_mut().server, 1);
        extra.execute();
        assert_eq!(extra.server_data().allnet_accounting().mode(), 1, "an inert server still owns usable images");
        assert!(platform.open_section("amdipc_shm_daemon_svio_shm_0").is_some());
        assert!(extra.sender.body().bytes().iter().all(|&b| b == 0));

        drop(servers.remove(7));
        let reused = Server::new(Box::new(platform.clone()));
        assert_eq!(reused.relation_index(), Some(7));
    }

    #[test]
    fn secondary_requests_get_empty_or_delta_replies_and_malformed_ones_are_ignored() {
        let mut rig = Rig::streaming();
        let mut browser = Browser::connect(&rig.platform, 0);
        let run = |rig: &mut Rig, browser: &mut Browser, steps: usize| {
            for _ in 0..steps {
                rig.step();
                browser.step();
            }
        };
        for _ in 0..100 {
            if browser.snapshot_done {
                break;
            }
            run(&mut rig, &mut browser, 1);
        }
        assert!(browser.snapshot_done, "hello and the strict Combined image arrive first");
        assert_eq!(browser.server, rig.live_server());

        // kind 1 with nothing changed: still answered, with an empty prefix.
        browser.request = Some(message(1, &[]));
        run(&mut rig, &mut browser, 12);
        // Bytes 8..16 of single-root messages keep the sizes of the last Combined message (the send vector is
        // reused), here the initial snapshot.
        let (stale_client, stale_server) = (CLIENT_DATA_SIZE as u32, SERVER_DATA_SIZE as u32);
        assert_eq!(browser.replies, [(1, 0, stale_client, stale_server)]);

        // kind 1 after a ServerData change.
        set_boot_clock(rig.server.roots_mut().server, 0xAB);
        let n = server_delta(rig.live_server(), &browser.server);
        assert!(n > BOOT_CLOCK_OFFSET);
        browser.request = Some(message(1, &[]));
        run(&mut rig, &mut browser, 12);
        assert_eq!(browser.replies.last(), Some(&(1, n as u32, stale_client, stale_server)));
        assert_eq!(browser.server, rig.live_server());

        // kind 2 after the game changed ClientData.
        rig.game.client[0x300] = 0xCD;
        run(&mut rig, &mut browser, 12);
        let c = client_delta(rig.server.client_data().bytes(), &browser.client);
        assert!(c > 0x300);
        browser.request = Some(message(2, &[]));
        run(&mut rig, &mut browser, 12);
        assert_eq!(browser.replies.last(), Some(&(2, c as u32, stale_client, stale_server)));
        assert_eq!(browser.client, rig.server.client_data().bytes());

        // kind 3: both deltas in one Combined message, client part first.
        set_player_count(rig.server.roots_mut().server, 0x01);
        rig.game.client[0x900] = 0x02;
        run(&mut rig, &mut browser, 12);
        let c = client_delta(rig.server.client_data().bytes(), &browser.client);
        let s = server_delta(rig.live_server(), &browser.server);
        assert!(c > 0x900 && s > 0x902C);
        browser.request = Some(message(3, &[]));
        run(&mut rig, &mut browser, 12);
        assert_eq!(browser.replies.last(), Some(&(3, (c + s) as u32, c as u32, s as u32)));
        assert_eq!(browser.server, rig.live_server());
        assert_eq!(browser.client, rig.server.client_data().bytes());

        // Requests with a body, unknown kinds and replies-as-requests are ignored.
        let answered = browser.replies.len();
        for request in [message(1, &[0; 4]), message(0, &[]), message(4, &[])] {
            browser.request = Some(request);
            run(&mut rig, &mut browser, 12);
        }
        assert_eq!(browser.replies.len(), answered);
        browser.request = Some(message(1, &[]));
        run(&mut rig, &mut browser, 12);
        assert_eq!(browser.replies.len(), answered + 1, "the stream is still in its idle phase");
    }
}
