//! The Aime DB client through `AimeLib` against a loopback TCP server (`spec/libam/aime.md` B.1 to B.4): an account
//! lookup followed by an AccountGet (access code -> Aime id and SEGA ID) exchange. The server below knows the wire
//! format on its own (header, AES-128-ECB with `Copyright(C)SEGA`), so the bytes the client sends and the bytes it
//! accepts are checked against the spec, not against the client's own encoder.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use aes::Aes128;
use backend::aime_db::{AimeDbTransport, TcpTransport};
use ecb::cipher::{BlockDecryptMut, BlockEncryptMut, KeyInit, block_padding::NoPadding};
use libam::aime::lib::{AimeLib, BoardInfo, InternalCode, LibConfig, LibManager, Op, PollKind, ResultBlock, flags};

const KEY: &[u8; 16] = b"Copyright(C)SEGA";
const ACCESS_CODE: [u8; 10] = [0x00, 0x05, 0x90, 0x12, 0x34, 0x56, 0x78, 0x90, 0x12, 0x34];
const SERIAL: &str = "A63E01E1234";
const LOOKUP_ID: i32 = 1_111;
const ACCOUNT_ID: i32 = 1_234_567;

fn encrypt(plain: &[u8]) -> Vec<u8> {
    let mut buffer = plain.to_vec();
    let length = buffer.len();
    ecb::Encryptor::<Aes128>::new(KEY.into())
        .encrypt_padded_mut::<NoPadding>(&mut buffer, length)
        .expect("whole blocks");
    buffer
}

fn decrypt(cipher: &[u8]) -> Vec<u8> {
    let mut buffer = cipher.to_vec();
    ecb::Decryptor::<Aes128>::new(KEY.into()).decrypt_padded_mut::<NoPadding>(&mut buffer).expect("whole blocks");
    buffer
}

/// One frame the server received: the bytes on the wire and the decrypted frame.
struct Wire {
    raw: Vec<u8>,
    plain: Vec<u8>,
}

impl Wire {
    fn command(&self) -> u16 {
        u16::from_le_bytes([self.plain[4], self.plain[5]])
    }
}

/// B.3 request sizes of the commands this test sends: lookup (4), lookup 2 (15) and goodbye (102).
fn request_size(command: u16) -> usize {
    match command {
        4 | 15 => 48,
        102 => 32,
        other => panic!("unexpected request command {other}"),
    }
}

/// The answer to a decrypted request: cmd 6 (48 bytes) to a lookup, cmd 16 (304 bytes) to a lookup 2.
fn answer(request: &[u8]) -> Vec<u8> {
    let (command, size, id) = match u16::from_le_bytes([request[4], request[5]]) {
        4 => (6u16, 48usize, LOOKUP_ID),
        15 => (16, 304, ACCOUNT_ID),
        other => panic!("no answer for command {other}"),
    };
    let mut frame = vec![0u8; size];
    frame[..32].copy_from_slice(&request[..32]);
    frame[4..6].copy_from_slice(&command.to_le_bytes());
    frame[6..8].copy_from_slice(&(size as u16).to_le_bytes());
    frame[8..10].copy_from_slice(&1u16.to_le_bytes());
    frame[32..36].copy_from_slice(&id.to_le_bytes());
    frame[36] = 1;
    if command == 16 {
        frame[40..49].copy_from_slice(b"SEGA-ID-1");
        frame[296..300].copy_from_slice(&2_222i32.to_le_bytes());
        frame[300..304].copy_from_slice(&(-1i32).to_le_bytes());
    }
    frame
}

/// Serves `connections` connections (one request and a goodbye each) and returns every frame received.
fn serve(listener: TcpListener, connections: usize) -> JoinHandle<Vec<Wire>> {
    thread::spawn(move || {
        let mut frames = Vec::new();
        for _ in 0..connections {
            let (mut stream, _) = listener.accept().expect("accept");
            stream.set_read_timeout(Some(Duration::from_secs(10))).expect("read timeout");
            let mut pending = Vec::new();
            let mut chunk = [0u8; 1024];
            loop {
                let count = stream.read(&mut chunk).expect("read");
                if count == 0 {
                    break;
                }
                pending.extend_from_slice(&chunk[..count]);
                while pending.len() >= 16 {
                    let first = decrypt(&pending[..16]);
                    let size = request_size(u16::from_le_bytes([first[4], first[5]]));
                    if pending.len() < size {
                        break;
                    }
                    let raw: Vec<u8> = pending.drain(..size).collect();
                    let wire = Wire { plain: decrypt(&raw), raw };
                    if wire.command() != 102 {
                        stream.write_all(&encrypt(&answer(&wire.plain))).expect("write");
                    }
                    frames.push(wire);
                }
            }
        }
        frames
    })
}

/// The request header every frame of the test carries: magic, version, cmd, len, status 0, game id, board
/// argument and serial.
fn header(command: u16, len: u16) -> Vec<u8> {
    let mut frame = vec![0u8; usize::from(len)];
    frame[0..4].copy_from_slice(&[0x3E, 0xA1, 0x23, 0x40]);
    frame[4..6].copy_from_slice(&command.to_le_bytes());
    frame[6..8].copy_from_slice(&len.to_le_bytes());
    frame[10..14].copy_from_slice(b"SBZZ");
    frame[16..20].copy_from_slice(&0x0102_0304u32.to_le_bytes());
    frame[20..31].copy_from_slice(SERIAL.as_bytes());
    frame
}

/// The lookup payload: access code, `R+168`, the reader firmware kind and `R+140`.
fn lookup_frame(command: u16) -> Vec<u8> {
    let mut frame = header(command, 48);
    frame[32..42].copy_from_slice(&ACCESS_CODE);
    frame[42] = 2;
    frame[43] = 0;
    frame[44..48].copy_from_slice(b"SBSD");
    frame
}

/// Starts `op`, then polls every frame until the machine finishes.
fn run(lib: &mut AimeLib, op: Op, kind: PollKind, clock: &mut u64) {
    lib.start(op, *clock).expect("start");
    for _ in 0..5_000 {
        *clock += 20;
        if lib.poll(kind, *clock) {
            return;
        }
        thread::sleep(Duration::from_millis(2));
    }
    panic!("the exchange did not finish");
}

#[test]
fn account_get_exchange_over_tcp() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
    let SocketAddr::V4(address) = listener.local_addr().expect("address") else { panic!("IPv4 listener") };
    let server = serve(listener, 2);

    let transport: Arc<dyn AimeDbTransport> =
        Arc::new(TcpTransport::fixed(SocketAddrV4::new(*address.ip(), address.port())));
    let manager = LibManager::new();
    let config = LibConfig { scan_timeout_sec: 15, polling_interval: 100 };
    let mut lib = AimeLib::new(0, config, &manager, &transport, None);
    lib.set_board_info(&BoardInfo { game_id: "SBZZ".into(), serial: SERIAL.into(), arg: 0x0102_0304 });

    // A reader-less lib takes the access code from the caller's result block (AccountCheck), then asks for the
    // account (AccountGet).
    let mut clock = 0;
    let block = ResultBlock { access_code: ACCESS_CODE, aux: 2, card_code: *b"SBSD", ..ResultBlock::default() };
    run(&mut lib, Op::AccountCheck(Box::new(block)), PollKind::AccountCheck, &mut clock);
    let checked = lib.result();
    assert_eq!(checked.error, InternalCode::NONE);
    assert_eq!(checked.aime_id, LOOKUP_ID);

    run(&mut lib, Op::AccountGet, PollKind::AccountGet, &mut clock);
    let result = lib.result();

    let frames = server.join().expect("server thread");
    let commands: Vec<u16> = frames.iter().map(Wire::command).collect();
    assert_eq!(commands, [4, 102, 15, 102], "one connection per request, each closed by a goodbye");

    // The requests: padded to whole AES blocks, encrypted with the spec key, header and payload as in B.2/B.4.
    for (wire, command) in [(&frames[0], 4), (&frames[2], 15)] {
        let expected = lookup_frame(command);
        assert_eq!(wire.plain, expected, "plaintext of command {command}");
        assert_eq!(wire.raw, encrypt(&expected), "ciphertext of command {command}");
    }
    for goodbye in [&frames[1], &frames[3]] {
        assert_eq!(goodbye.plain, header(102, 32));
        assert_eq!(goodbye.raw, encrypt(&header(102, 32)));
    }

    // The decrypted answer of the lookup 2 reaches the result block.
    assert_eq!(result.error, InternalCode::NONE);
    assert_eq!(result.aime_id, ACCOUNT_ID);
    assert_eq!(result.related_aime_ids, [2_222, -1]);
    assert_eq!(result.aime_found, 2, "the id and one related id");
    assert_eq!(&result.sega_id[..10], b"SEGA-ID-1\0");
    assert_eq!(result.flags & (flags::KIND_1 | flags::KIND_2), flags::KIND_1);
}
