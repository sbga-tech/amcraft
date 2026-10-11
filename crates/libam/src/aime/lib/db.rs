//! The Aime DB client (`spec/libam/aime.md` B.1 to B.5): one TCP connection per request with a 5 s deadline, one
//! retry after a failed connect or a closed receive, the goodbye frame before every close, and the per-operation
//! request/response handlers. Requests run as jobs on a worker thread ([`spawn`]); a transport that reports
//! [`AimeDbTransport::inline`] runs them at submit so a frame is deterministic.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant};

use super::proto::{self, AimeReply, DbParams, cmd};
use super::{InternalCode, LogEntry, RawCampaignClear, RawCampaignInfo, SingleLog, flags};
use backend::aime_db::{AimeDbTransport, ServerAddr};

/// Overall deadline of one request (B.5, `client+32`).
pub(super) const DEADLINE: Duration = Duration::from_millis(5000);
/// Pause before the single retry (B.5).
const RETRY_PAUSE: Duration = Duration::from_millis(500);
/// Receive slice: how often the abort flag and the deadline are looked at.
const RECV_SLICE: Duration = Duration::from_millis(20);

/// A request failed: `9` for a response with an unexpected command, `10` for everything else (B.5).
type Failure = InternalCode;

/// The response command that answers `request`.
const fn reply_of(request: u16) -> u16 {
    match request {
        cmd::FELICA_LOOKUP | cmd::FELICA_REGISTER => cmd::FELICA_REPLY,
        cmd::AIME_LOOKUP | cmd::AIME_REGISTER => cmd::AIME_REPLY,
        cmd::LOG_STATUS => cmd::LOG_STATUS_REPLY,
        cmd::SINGLE_LOG => cmd::SINGLE_LOG_REPLY,
        cmd::CAMPAIGN_INFO => cmd::CAMPAIGN_INFO_REPLY,
        cmd::CAMPAIGN_CLEAR => cmd::CAMPAIGN_CLEAR_REPLY,
        cmd::AIME_LOOKUP2 => cmd::AIME_LOOKUP2_REPLY,
        cmd::LITE_S_LOOKUP => cmd::LITE_S_REPLY,
        cmd::MULTI_LOG => cmd::MULTI_LOG_REPLY,
        cmd::ALIVE => cmd::ALIVE_REPLY,
        _ => 0,
    }
}

/// A finished or running background job.
pub(super) struct Pending<T> {
    rx: Receiver<T>,
    abort: Arc<AtomicBool>,
    done: Option<T>,
}

impl<T> Pending<T> {
    /// `true` once the job produced its result.
    pub(super) fn is_done(&mut self) -> bool {
        if self.done.is_none() {
            match self.rx.try_recv() {
                Ok(value) => self.done = Some(value),
                Err(TryRecvError::Empty) => {}
                // The worker died without a result: abandon it as failed by never completing is wrong, so the
                // job reports done with no value; callers treat `take` = None as a failure.
                Err(TryRecvError::Disconnected) => return true,
            }
        }
        self.done.is_some()
    }

    /// The result, once.
    pub(super) fn take(&mut self) -> Option<T> {
        self.is_done();
        self.done.take()
    }

    /// Asks the worker to give up (`db+976`, `0x14034F6B0`).
    pub(super) fn abort(&self) {
        self.abort.store(true, Ordering::Relaxed);
    }

    /// Waits for the result up to `timeout` (terminate, `0x140353440`).
    pub(super) fn wait(&mut self, timeout: Duration) -> bool {
        if self.done.is_some() {
            return true;
        }
        match self.rx.recv_timeout(timeout) {
            Ok(value) => {
                self.done = Some(value);
                true
            }
            Err(RecvTimeoutError::Timeout) => false,
            Err(RecvTimeoutError::Disconnected) => true,
        }
    }
}

/// Runs `job` on a worker thread.
pub(super) fn spawn<T, F>(job: F) -> Pending<T>
where
    T: Send + 'static,
    F: FnOnce(Arc<AtomicBool>) -> T + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    let abort = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&abort);
    std::thread::spawn(move || {
        let _ = tx.send(job(flag));
    });
    Pending { rx, abort, done: None }
}

/// Name resolution of `aime.naominet.jp:22345` as a job (state 13, the worker threads' resolve step).
pub(super) fn resolve(transport: &Arc<dyn AimeDbTransport>) -> Pending<Result<ServerAddr, InternalCode>> {
    let t = Arc::clone(transport);
    spawn(move |_| t.resolve(super::DB_HOST, super::DB_PORT).map_err(|_| InternalCode::RESOLVE_FAILED))
}

/// A connected-per-request client for one server record.
pub(super) struct Conn {
    transport: Arc<dyn AimeDbTransport>,
    server: ServerAddr,
    params: DbParams,
    abort: Arc<AtomicBool>,
}

impl Conn {
    pub(super) fn new(
        transport: Arc<dyn AimeDbTransport>,
        server: ServerAddr,
        params: DbParams,
        abort: Arc<AtomicBool>,
    ) -> Self {
        Self { transport, server, params, abort }
    }

    fn aborted(&self) -> bool {
        self.abort.load(Ordering::Relaxed)
    }

    /// One request/response exchange with the retry rule of B.5; returns the decrypted response frame of the
    /// command that answers `command`.
    fn exchange(&mut self, command: u16, payload: &[u8]) -> Result<Vec<u8>, Failure> {
        let request = proto::encrypt(&proto::build_frame(&self.params, command, 0, payload));
        let deadline = Instant::now() + DEADLINE;
        for attempt in 0..2 {
            match self.once(&request, deadline) {
                Ok(frame) => {
                    let header = proto::Header::parse(&frame).ok_or(InternalCode::SERVER_ERROR)?;
                    if header.status == 0 {
                        return Err(InternalCode::SERVER_ERROR);
                    }
                    if header.cmd != reply_of(command) {
                        return Err(InternalCode::SERVER_TIMEOUT);
                    }
                    return Ok(frame);
                }
                Err(()) if attempt == 0 && !self.aborted() && Instant::now() + RETRY_PAUSE < deadline => {
                    std::thread::sleep(RETRY_PAUSE);
                }
                Err(()) => break,
            }
        }
        Err(InternalCode::SERVER_ERROR)
    }

    /// Connect, send, receive one response, goodbye, close.
    fn once(&mut self, request: &[u8], deadline: Instant) -> Result<Vec<u8>, ()> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let mut stream = self.transport.connect(self.server, remaining).map_err(|_| ())?;
        stream.send(request).map_err(|_| ())?;
        let mut data: Vec<u8> = Vec::with_capacity(proto::RECV_BUFFER);
        let mut expected = 0usize;
        let result = loop {
            if self.aborted() || Instant::now() >= deadline {
                break Err(());
            }
            let mut chunk = [0u8; proto::RECV_BUFFER - 1];
            let room = (proto::RECV_BUFFER - 1 - data.len()).min(chunk.len());
            if room == 0 {
                break Err(());
            }
            match stream.recv(&mut chunk[..room], RECV_SLICE) {
                Ok(0) => break Err(()),
                Ok(count) => data.extend_from_slice(&chunk[..count]),
                Err(error) if matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                }
                Err(_) => break Err(()),
            }
            if expected == 0 && data.len() >= 16 {
                let mut first = [0u8; 16];
                first.copy_from_slice(&data[..16]);
                proto::decrypt(&mut first);
                expected = proto::frame_size(u16::from_le_bytes([first[4], first[5]]));
            }
            if expected != 0 && data.len() >= expected {
                data.truncate(expected);
                proto::decrypt(&mut data);
                break Ok(data);
            }
        };
        let goodbye = proto::encrypt(&proto::build_frame(&self.params, cmd::GOODBYE, 0, &[]));
        let _ = stream.send(&goodbye);
        result
    }
}

/// What an operation brings back; the core applies it to `R` (C 6.4).
#[derive(Debug, Clone, Default)]
pub(super) struct DbOutput {
    /// The DB error code (`db+960`).
    pub code: InternalCode,
    /// The access code of a FeliCa reply or Lite-S reply (`db+68`).
    pub access_code: Option<[u8; 10]>,
    /// The Aime reply fields.
    pub reply: Option<AimeReply>,
    /// `R.flags` bits the operation sets (auto-registration).
    pub flags: u32,
    /// The campaign entries (`db+120`).
    pub campaign_info: Option<[RawCampaignInfo; 3]>,
    /// The campaign-clear entries (`db+600`).
    pub campaign_clear: Option<[RawCampaignClear; 3]>,
    /// The per-entry results of a multi log (`db+1944`).
    pub log_bytes: Vec<u8>,
    /// The record resolved inside the job, when it had none.
    pub resolved: Option<ServerAddr>,
}

impl DbOutput {
    fn failed(code: InternalCode) -> Self {
        Self { code, ..Self::default() }
    }
}

/// The Aime-card request fields (cmds 4, 5, 15).
#[derive(Debug, Clone, Copy)]
pub(super) struct AimeReq {
    pub access_code: [u8; 10],
    pub aux: u8,
    pub card_code: [u8; 4],
}

/// One DB operation.
#[derive(Debug, Clone)]
pub(super) enum DbOp {
    Alive,
    CampaignInfo,
    CampaignClear(u32),
    LogStatus(u32, u32),
    SingleLog(SingleLog),
    MultiLog(Vec<LogEntry>),
    /// State 14: FeliCa lookup (cmd 1); with `register_missing` an unknown card is registered (cmd 2).
    FelicaCheck {
        idm: [u8; 8],
        pmm: [u8; 8],
        register_missing: bool,
    },
    /// State 16: FeliCa lookup only.
    FelicaGet {
        idm: [u8; 8],
        pmm: [u8; 8],
    },
    /// State 17: FeliCa register.
    FelicaRegist {
        idm: [u8; 8],
        pmm: [u8; 8],
    },
    /// State 15: Aime lookup (cmd 4); with `register_missing` an unknown id is registered (cmd 5).
    AimeCheck {
        req: AimeReq,
        register_missing: bool,
    },
    /// State 18: Aime lookup (cmd 4).
    AimeGetex(AimeReq),
    /// State 19: Aime register (cmd 5).
    AimeRegist(AimeReq),
    /// State 21: lookup 2 (cmd 15).
    AimeGet(AimeReq),
    /// State 22: Lite-S lookup (cmd 17).
    AimeGetS {
        extra: [u8; 75],
        aux: u8,
    },
}

fn aime_output((code, reply): (InternalCode, AimeReply), flags_set: u32) -> DbOutput {
    let access_code = reply.access_code;
    let bits = reply.flag_bits;
    DbOutput { code, access_code, reply: Some(reply), flags: bits | flags_set, ..DbOutput::default() }
}

fn felica_output((code, access): (InternalCode, Option<[u8; 10]>), flags_set: u32) -> DbOutput {
    DbOutput { code, access_code: access, flags: flags_set, ..DbOutput::default() }
}

/// Runs `op` on `conn`. A transport failure is code 10 (9 for a wrong response command), except the alive check
/// which maps both to 10.
pub(super) fn run(conn: &mut Conn, op: &DbOp, fw_kind: u8) -> DbOutput {
    match op {
        DbOp::Alive => match conn.exchange(cmd::ALIVE, &[]) {
            Ok(frame) => DbOutput::failed(proto::parse_alive(&frame)),
            Err(_) => DbOutput::failed(InternalCode::SERVER_ERROR),
        },
        DbOp::CampaignInfo => match conn.exchange(cmd::CAMPAIGN_INFO, &[]) {
            Ok(frame) => {
                let (code, entries) = proto::parse_campaign_info(&frame);
                DbOutput { code, campaign_info: Some(entries), ..DbOutput::default() }
            }
            Err(code) => DbOutput::failed(code),
        },
        DbOp::CampaignClear(id) => match conn.exchange(cmd::CAMPAIGN_CLEAR, &id.to_le_bytes()) {
            Ok(frame) => {
                let (code, entries) = proto::parse_campaign_clear(&frame);
                DbOutput { code, campaign_clear: Some(entries), ..DbOutput::default() }
            }
            Err(code) => DbOutput::failed(code),
        },
        DbOp::LogStatus(a, b) => {
            let mut payload = [0u8; 8];
            payload[..4].copy_from_slice(&a.to_le_bytes());
            payload[4..].copy_from_slice(&b.to_le_bytes());
            match conn.exchange(cmd::LOG_STATUS, &payload) {
                Ok(frame) => DbOutput::failed(proto::parse_log_ack(&frame)),
                Err(code) => DbOutput::failed(code),
            }
        }
        DbOp::SingleLog(log) => {
            let (a, b, c, d, e, f) = log.fields;
            let mut payload = [0u8; 28];
            payload[0..4].copy_from_slice(&a.to_le_bytes());
            payload[4..8].copy_from_slice(&b.to_le_bytes());
            payload[8..16].copy_from_slice(&c.to_le_bytes());
            payload[16..20].copy_from_slice(&d.to_le_bytes());
            payload[20..24].copy_from_slice(&e.to_le_bytes());
            payload[24..28].copy_from_slice(&f.to_le_bytes());
            match conn.exchange(cmd::SINGLE_LOG, &payload) {
                Ok(frame) => DbOutput::failed(proto::parse_log_ack(&frame)),
                Err(code) => DbOutput::failed(code),
            }
        }
        DbOp::MultiLog(entries) => {
            let encoded: Vec<[u8; 48]> = entries.iter().take(20).map(LogEntry::encode).collect();
            match conn.exchange(cmd::MULTI_LOG, &proto::multi_log_payload(&encoded)) {
                Ok(frame) => {
                    let (code, bytes) = proto::parse_multi_log(&frame, encoded.len());
                    DbOutput { code, log_bytes: bytes, ..DbOutput::default() }
                }
                Err(code) => DbOutput::failed(code),
            }
        }
        DbOp::FelicaCheck { idm, pmm, register_missing } => {
            let payload = proto::felica_payload(idm, pmm);
            match conn.exchange(cmd::FELICA_LOOKUP, &payload) {
                Ok(frame) => {
                    let lookup = proto::parse_felica_lookup(&frame);
                    if lookup.0 == InternalCode::FELICA_ID_NOT_FOUND && *register_missing {
                        register_felica(conn, &payload)
                    } else {
                        felica_output(lookup, 0)
                    }
                }
                Err(code) => DbOutput::failed(code),
            }
        }
        DbOp::FelicaGet { idm, pmm } => match conn.exchange(cmd::FELICA_LOOKUP, &proto::felica_payload(idm, pmm)) {
            Ok(frame) => felica_output(proto::parse_felica_lookup(&frame), 0),
            Err(code) => DbOutput::failed(code),
        },
        DbOp::FelicaRegist { idm, pmm } => register_felica(conn, &proto::felica_payload(idm, pmm)),
        DbOp::AimeCheck { req, register_missing } => {
            let payload = aime_request(req, fw_kind);
            match conn.exchange(cmd::AIME_LOOKUP, &payload) {
                Ok(frame) => {
                    let lookup = proto::parse_aime_lookup(&frame);
                    if lookup.0 == InternalCode::AIME_ID_NOT_FOUND && *register_missing {
                        register_aime(conn, &payload, flags::AIME_REGISTERED)
                    } else {
                        aime_output(lookup, 0)
                    }
                }
                Err(code) => DbOutput::failed(code),
            }
        }
        DbOp::AimeGetex(req) => match conn.exchange(cmd::AIME_LOOKUP, &aime_request(req, fw_kind)) {
            Ok(frame) => aime_output(proto::parse_aime_lookup(&frame), 0),
            Err(code) => DbOutput::failed(code),
        },
        DbOp::AimeRegist(req) => register_aime(conn, &aime_request(req, fw_kind), 0),
        DbOp::AimeGet(req) => match conn.exchange(cmd::AIME_LOOKUP2, &aime_request(req, fw_kind)) {
            Ok(frame) => aime_output(proto::parse_aime_lookup2(&frame), 0),
            Err(code) => DbOutput::failed(code),
        },
        DbOp::AimeGetS { extra, aux } => {
            match conn.exchange(cmd::LITE_S_LOOKUP, &proto::lite_s_payload(extra, *aux, fw_kind)) {
                Ok(frame) => aime_output(proto::parse_lite_s(&frame), 0),
                Err(code) => DbOutput::failed(code),
            }
        }
    }
}

fn aime_request(req: &AimeReq, fw_kind: u8) -> [u8; 16] {
    proto::aime_payload(&req.access_code, req.aux, fw_kind, req.card_code)
}

fn register_felica(conn: &mut Conn, payload: &[u8; 16]) -> DbOutput {
    match conn.exchange(cmd::FELICA_REGISTER, payload) {
        Ok(frame) => {
            let result = proto::parse_felica_register(&frame);
            let registered = if result.0.is_ok() { flags::FELICA_REGISTERED } else { 0 };
            felica_output(result, registered)
        }
        Err(code) => DbOutput::failed(code),
    }
}

fn register_aime(conn: &mut Conn, payload: &[u8; 16], registered: u32) -> DbOutput {
    match conn.exchange(cmd::AIME_REGISTER, payload) {
        Ok(frame) => {
            let result = proto::parse_aime_register(&frame);
            let bits = if result.0.is_ok() { registered } else { 0 };
            aime_output(result, bits)
        }
        Err(code) => DbOutput::failed(code),
    }
}

/// A worker job: resolve when `server` is `None`, then run `op`.
pub(super) fn job(
    transport: Arc<dyn AimeDbTransport>,
    server: Option<ServerAddr>,
    params: DbParams,
    op: DbOp,
) -> impl FnOnce(Arc<AtomicBool>) -> DbOutput + Send + 'static {
    move |abort| {
        let mut resolved = None;
        let addr = match server {
            Some(addr) => addr,
            None => match transport.resolve(super::DB_HOST, super::DB_PORT) {
                Ok(addr) => {
                    resolved = Some(addr);
                    addr
                }
                Err(_) => return DbOutput::failed(InternalCode::RESOLVE_FAILED),
            },
        };
        let fw_kind = params.fw_kind;
        let mut conn = Conn::new(transport, addr, params, abort);
        let mut output = run(&mut conn, &op, fw_kind);
        output.resolved = resolved;
        output
    }
}
