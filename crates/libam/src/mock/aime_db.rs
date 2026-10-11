//! Script handle of the simulated Aime DB server. It is the in-memory transport (`SimBackend::aime_db`: requests run
//! inline at submit, and the server knows no account until some are added) and, through [`SimDb::serve`], a real
//! listener on `127.0.0.1`.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::aime::lib::proto::{self, DbParams, cmd};
use crate::aime::lib::{RawCampaignClear, RawCampaignInfo};
use backend::aime_db::{AimeDbStream, AimeDbTransport, ServerAddr};

#[derive(Default)]
struct SimState {
    accounts: HashMap<[u8; 10], (i32, u8)>,
    felica: HashMap<[u8; 16], [u8; 10]>,
    campaigns: Vec<RawCampaignInfo>,
    clears: Vec<RawCampaignClear>,
    requests: Vec<Vec<u8>>,
    goodbyes: usize,
    down: bool,
    alive_status: Option<u16>,
    lookup_status: Option<u16>,
    next_id: i32,
    next_felica: u64,
}

/// Script handle of the simulated Aime DB server. It is the in-memory transport (`SimBackend::aime_db`: requests run
/// inline at submit, and the server knows no account until some are added) and, through [`SimDb::serve`], a real
/// listener on `127.0.0.1`.
#[derive(Clone, Default)]
pub struct SimDb {
    inner: Arc<Mutex<SimState>>,
}

impl std::fmt::Debug for SimDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimDb").finish_non_exhaustive()
    }
}

impl SimDb {
    fn state(&self) -> std::sync::MutexGuard<'_, SimState> {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Registers an Aime account: `access_code` (packed BCD) maps to `aime_id` with the server `kind` byte.
    pub fn add_account(&self, access_code: [u8; 10], aime_id: i32, kind: u8) {
        self.state().accounts.insert(access_code, (aime_id, kind));
    }

    /// Registers a FeliCa card (`IDm`, `PMm`) with its access code.
    pub fn add_felica(&self, idm: [u8; 8], pmm: [u8; 8], access_code: [u8; 10]) {
        let mut key = [0u8; 16];
        key[..8].copy_from_slice(&idm);
        key[8..].copy_from_slice(&pmm);
        self.state().felica.insert(key, access_code);
    }

    /// Adds a campaign (answered by cmd 11, at most three).
    pub fn add_campaign(&self, id: u32, name: &str, stamps: [u32; 5]) {
        let mut entry = RawCampaignInfo { id, name: [0; 128], stamps };
        let bytes = name.as_bytes();
        let len = bytes.len().min(127);
        entry.name[..len].copy_from_slice(&bytes[..len]);
        self.state().campaigns.push(entry);
    }

    /// Sets the campaign-clear entries answered by cmd 13.
    pub fn set_campaign_clears(&self, entries: Vec<RawCampaignClear>) {
        self.state().clears = entries;
    }

    /// The server refuses connections.
    pub fn set_down(&self, down: bool) {
        self.state().down = down;
    }

    /// The `status` of the alive answer (default 1).
    pub fn set_alive_status(&self, status: u16) {
        self.state().alive_status = Some(status);
    }

    /// Forces the `status` of Aime lookup answers (2 = invalid id, 4..=10 = BAN states).
    pub fn set_lookup_status(&self, status: u16) {
        self.state().lookup_status = Some(status);
    }

    /// Every request received so far, decrypted (header and payload, unpadded); goodbye frames are not listed.
    pub fn requests(&self) -> Vec<Vec<u8>> {
        self.state().requests.clone()
    }

    /// Number of goodbye frames (cmd 102) received.
    pub fn goodbyes(&self) -> usize {
        self.state().goodbyes
    }

    /// Answers one decrypted request frame: the plaintext response, `None` for a goodbye.
    fn handle(&self, request: &[u8]) -> Option<Vec<u8>> {
        let header = proto::Header::parse(request)?;
        let mut st = self.state();
        if header.cmd == cmd::GOODBYE {
            st.goodbyes += 1;
            return None;
        }
        let size = proto::frame_size(header.cmd).min(request.len());
        st.requests.push(request[..size].to_vec());
        let mut params = DbParams { game_id: [0; 5], serial: [0; 11], board_arg: 0, fw_kind: 0 };
        params.game_id.copy_from_slice(&request[10..15]);
        params.board_arg = u32::from_le_bytes(request[16..20].try_into().ok()?);
        params.serial.copy_from_slice(&request[20..31]);
        let reply = |command: u16, status: u16, payload: &[u8]| proto::build_frame(&params, command, status, payload);
        let access_at = |at: usize| -> [u8; 10] { request[at..at + 10].try_into().unwrap_or([0; 10]) };
        Some(match header.cmd {
            cmd::ALIVE => reply(cmd::ALIVE_REPLY, st.alive_status.unwrap_or(1), &[]),
            cmd::FELICA_LOOKUP | cmd::FELICA_REGISTER => {
                let key: [u8; 16] = request[32..48].try_into().ok()?;
                let found = st.felica.get(&key).copied();
                let mut payload = [0u8; 14];
                match (header.cmd, found) {
                    (cmd::FELICA_REGISTER, Some(_)) => return Some(reply(cmd::FELICA_REPLY, 3, &payload)),
                    (_, Some(code)) => {
                        payload[0..4].copy_from_slice(&1i32.to_le_bytes());
                        payload[4..14].copy_from_slice(&code);
                    }
                    (cmd::FELICA_REGISTER, None) => {
                        st.next_felica += 1;
                        let digits = format!("{:020}", st.next_felica);
                        let mut code = [0u8; 10];
                        for (index, byte) in code.iter_mut().enumerate() {
                            let hi = digits.as_bytes()[index * 2] - b'0';
                            let lo = digits.as_bytes()[index * 2 + 1] - b'0';
                            *byte = hi << 4 | lo;
                        }
                        st.felica.insert(key, code);
                        payload[0..4].copy_from_slice(&1i32.to_le_bytes());
                        payload[4..14].copy_from_slice(&code);
                    }
                    (_, None) => payload[0..4].copy_from_slice(&(-1i32).to_le_bytes()),
                }
                reply(cmd::FELICA_REPLY, 1, &payload)
            }
            cmd::AIME_LOOKUP | cmd::AIME_REGISTER | cmd::AIME_LOOKUP2 => {
                let code = access_at(32);
                let reply_cmd = if header.cmd == cmd::AIME_LOOKUP2 { cmd::AIME_LOOKUP2_REPLY } else { cmd::AIME_REPLY };
                if let Some(status) = st.lookup_status {
                    return Some(reply(reply_cmd, status, &(-1i32).to_le_bytes()));
                }
                let known = st.accounts.get(&code).copied();
                let mut payload = [0u8; 272];
                payload[0..4].copy_from_slice(&(-1i32).to_le_bytes());
                payload[264..268].copy_from_slice(&(-1i32).to_le_bytes());
                payload[268..272].copy_from_slice(&(-1i32).to_le_bytes());
                match (header.cmd, known) {
                    (cmd::AIME_REGISTER, Some(_)) => return Some(reply(reply_cmd, 3, &payload)),
                    (cmd::AIME_REGISTER, None) => {
                        st.next_id += 1;
                        let id = 10_000 + st.next_id;
                        st.accounts.insert(code, (id, 1));
                        payload[0..4].copy_from_slice(&id.to_le_bytes());
                        payload[4] = 1;
                    }
                    (_, Some((id, kind))) => {
                        payload[0..4].copy_from_slice(&id.to_le_bytes());
                        payload[4] = kind;
                    }
                    (_, None) => {}
                }
                reply(reply_cmd, 1, &payload)
            }
            cmd::LITE_S_LOOKUP => {
                let mut payload = [0u8; 288];
                payload[0..4].copy_from_slice(&(-1i32).to_le_bytes());
                reply(cmd::LITE_S_REPLY, 1, &payload)
            }
            cmd::LOG_STATUS => reply(cmd::LOG_STATUS_REPLY, 1, &[]),
            cmd::SINGLE_LOG => reply(cmd::SINGLE_LOG_REPLY, 1, &[]),
            cmd::MULTI_LOG => {
                let count = u32::from_le_bytes(request[992..996].try_into().ok()?).min(20) as usize;
                let mut payload = vec![0u8; 32];
                payload[..count].fill(1);
                reply(cmd::MULTI_LOG_REPLY, 1, &payload)
            }
            cmd::CAMPAIGN_INFO => {
                let mut payload = vec![0u8; 480];
                for (index, entry) in st.campaigns.iter().take(3).enumerate() {
                    let base = index * 160;
                    payload[base..base + 4].copy_from_slice(&entry.id.to_le_bytes());
                    payload[base + 4..base + 132].copy_from_slice(&entry.name);
                    for (k, stamp) in entry.stamps.iter().enumerate() {
                        payload[base + 132 + 4 * k..base + 136 + 4 * k].copy_from_slice(&stamp.to_le_bytes());
                    }
                }
                reply(cmd::CAMPAIGN_INFO_REPLY, 1, &payload)
            }
            cmd::CAMPAIGN_CLEAR => {
                let mut payload = vec![0u8; 48];
                for (index, entry) in st.clears.iter().take(3).enumerate() {
                    let base = index * 16;
                    payload[base..base + 4].copy_from_slice(&entry.id.to_le_bytes());
                    payload[base + 4..base + 8].copy_from_slice(&entry.flag.to_le_bytes());
                    payload[base + 8..base + 12].copy_from_slice(&entry.value.to_le_bytes());
                }
                reply(cmd::CAMPAIGN_CLEAR_REPLY, 1, &payload)
            }
            _ => return None,
        })
    }

    /// Starts a listener on `127.0.0.1` (a free port) that serves this simulated server over real TCP.
    pub fn serve(&self) -> std::io::Result<SimTcpServer> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let SocketAddr::V4(addr) = listener.local_addr()? else {
            return Err(std::io::Error::other("not IPv4"));
        };
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let sim = self.clone();
        let thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if flag.load(Ordering::Relaxed) {
                    break;
                }
                if let Ok(stream) = stream {
                    let sim = sim.clone();
                    std::thread::spawn(move || sim.serve_connection(stream));
                }
            }
        });
        Ok(SimTcpServer { addr, stop, thread: Some(thread) })
    }

    fn serve_connection(&self, mut stream: TcpStream) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut data = Vec::new();
        let mut chunk = [0u8; 1024];
        let mut served = false;
        while let Ok(count) = stream.read(&mut chunk) {
            if count == 0 {
                break;
            }
            data.extend_from_slice(&chunk[..count]);
            // Whole frames only: the first block says how long the request is.
            while data.len() >= 16 {
                let mut first = [0u8; 16];
                first.copy_from_slice(&data[..16]);
                proto::decrypt(&mut first);
                let size = proto::frame_size(u16::from_le_bytes([first[4], first[5]])).next_multiple_of(16);
                if size == 0 || data.len() < size {
                    break;
                }
                let mut frame: Vec<u8> = data.drain(..size).collect();
                proto::decrypt(&mut frame);
                if let Some(response) = self.handle(&frame) {
                    served = true;
                    if stream.write_all(&proto::encrypt(&response)).is_err() {
                        return;
                    }
                }
            }
            if served && data.is_empty() && self.state().goodbyes > 0 {
                // The client says goodbye after the answer; nothing more is expected.
            }
        }
    }
}

/// A running loopback listener of the simulated server; it stops when dropped.
#[derive(Debug)]
pub struct SimTcpServer {
    addr: SocketAddrV4,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl SimTcpServer {
    /// The listening address (`127.0.0.1:<port>`).
    pub fn addr(&self) -> SocketAddrV4 {
        self.addr
    }
}

impl Drop for SimTcpServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Wake the accept loop.
        let _ = TcpStream::connect(self.addr);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct SimStream {
    sim: SimDb,
    response: Vec<u8>,
}

impl AimeDbStream for SimStream {
    fn send(&mut self, data: &[u8]) -> std::io::Result<()> {
        let mut frame = data.to_vec();
        proto::decrypt(&mut frame);
        if let Some(response) = self.sim.handle(&frame) {
            self.response.extend_from_slice(&proto::encrypt(&response));
        }
        Ok(())
    }

    fn recv(&mut self, buf: &mut [u8], _timeout: Duration) -> std::io::Result<usize> {
        if self.response.is_empty() {
            return Err(std::io::ErrorKind::WouldBlock.into());
        }
        let count = buf.len().min(self.response.len());
        buf[..count].copy_from_slice(&self.response[..count]);
        self.response.drain(..count);
        Ok(count)
    }
}

impl AimeDbTransport for SimDb {
    /// Every name resolves to `127.0.0.1:<port>` while the server is up.
    fn resolve(&self, _host: &str, port: u16) -> std::io::Result<ServerAddr> {
        if self.state().down {
            return Err(std::io::ErrorKind::NotFound.into());
        }
        Ok(ServerAddr { addr: SocketAddrV4::new(Ipv4Addr::LOCALHOST, port) })
    }

    fn connect(&self, _addr: ServerAddr, _timeout: Duration) -> std::io::Result<Box<dyn AimeDbStream>> {
        if self.state().down {
            return Err(std::io::ErrorKind::ConnectionRefused.into());
        }
        Ok(Box::new(SimStream { sim: self.clone(), response: Vec::new() }))
    }
}
