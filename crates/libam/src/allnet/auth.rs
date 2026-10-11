//! The alw authentication phases (`spec/libam/allnet.md` parts 3 and 4): network bring-up after the network
//! module, the DNS service, the Auth 2.0 leg, the alAbEx launch, the response parse and the latched error codes.
//! One phase handler runs per frame, then the alAbEx pump and the alAuth pump (part 2.5).

use std::net::Ipv4Addr;

use libam_api::allnet::{AuthLocation, TimeSlot};
use libam_api::am_config::AllnetAuthConfig;
use libam_api::time::{DateTime, Tm};

use super::abex::{Mode, Notice, ServerTime, Session};
use super::alauth::{self, AlAuth, Env, Kind, State};
use super::billing::{AuthLink, BillingToAuth, Identity};
use super::client::{Abex, Pump, stat};
use super::services::KeychipIdentity;
use super::trace::{Trace, Tracer};
use crate::clock::Clock;
use crate::module::Cx;
use crate::platform::keychip::Link;
use crate::platform::rtc::{Rtc, RtcTime};
use backend::Dns;

const NETWORK_TIMEOUT_MS: u64 = 120_000;
const LOC_WAIT_MS: u64 = 20_000;
/// Seconds from 0001-01-01 to 1970-01-01.
const UNIX_OFFSET: i64 = 62_135_596_800;
/// The JST offset the RTC call carries (`+32400` s, `-540` min).
const JST_SECONDS: i64 = 32_400;
const JST_MINUTES: i32 = 540;

/// The alw phases this machine runs (`ALWE_AUTH_PHASE_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Waits for the keychip identity.
    Setup,
    /// Waits for the network module.
    WaitNetwork,
    /// 9: router discovery by the `.loc` names.
    DnsLan,
    /// 11: traceroute to the router.
    Traceroute,
    /// 10: detected line against the configured mask.
    LineType,
    /// 12: WAN names.
    DnsWan,
    /// 13.
    Auth20Init,
    /// 14.
    Auth20Wait,
    /// 15.
    Auth20Dongle,
    /// 16.
    Auth20Server,
    /// 17.
    AuthInit,
    /// 18.
    Authentication,
    /// 22.
    AuthOk,
    /// 27: the WAN is unusable.
    Offline,
    /// 29.
    Fatal,
}

/// The authentication of `am::allnet::Manager`.
pub struct Auth {
    cfg: AllnetAuthConfig,
    phase: Phase,
    sub: u8,
    entered: u64,
    allnet_type: u8,
    identity: Option<KeychipIdentity>,
    local_ip: Ipv4Addr,
    tenpo: Option<Ipv4Addr>,
    mobi: Option<Ipv4Addr>,
    bb: Option<Ipv4Addr>,
    dsl: Option<Ipv4Addr>,
    auth_host: Option<Ipv4Addr>,
    op_host: Option<Ipv4Addr>,
    title_host: Option<Ipv4Addr>,
    line: u8,
    hops: i32,
    trace_failures: u8,
    /// Background altr traceroute; one worker, replaced when the target changes.
    tracer: Tracer,
    al: AlAuth,
    round: u8,
    packet: Vec<u8>,
    auth_text: String,
    abex: Abex,
    done: bool,
    good: bool,
    latched: u32,
    offline_from_billing: bool,
    location: Option<AuthLocation>,
    time: Option<TimeSlot>,
    text: Option<[u8; 1536]>,
    client_id: u32,
    wan_latched: bool,
    date_synced: bool,
    bias_minutes: i32,
    rtc: Rtc,
    generation: u32,
    restarted: bool,
    /// `AW+52`: a download order may be requested (set by a finished authentication or download order).
    dl_allowed: bool,
    /// `AW+56`: a download order was requested and not started yet.
    dl_requested: bool,
    /// Phase 23: the download order runs.
    dl_waiting: bool,
    /// `AW+426232`: `22` no order yet, `24` refused, `25` timed out, `26` answered.
    dl_result: u32,
    /// `AW+5104`: the `uri=` text of the last answered download order; the original only serves it while
    /// `AW+20 == 1 && AW+24 == 1`, which the failure paths clear.
    dl_uri: Option<String>,
    /// `AW+426300`: the network-retry option (`alwSetOption` kind 0) is on; libam sets it for accounting mode 3.
    recovery_enabled: bool,
    /// `AW+426304`: the wait-recovery mode; `alwGetStatus(0)` answers 2 while it lasts.
    recovering: bool,
    /// `AW+426216`: the Auth 2.0 progress code (18 working, 20 communication error, 21 timeout, 0 none).
    al_progress: u32,
    /// `R+272 & 0x100`: the alw test-mode flag. The init parameter sets it; every network-mode restart sets it
    /// again from the process state, and billing reads it as the test mode.
    test_flag: bool,
    /// `R+276`: the latched fatal error code (0 none). Only a restart clears it.
    fatal: u32,
}

impl Default for Auth {
    fn default() -> Self {
        Self {
            cfg: AllnetAuthConfig::default(),
            phase: Phase::Setup,
            sub: 0,
            entered: 0,
            allnet_type: 1,
            identity: None,
            local_ip: Ipv4Addr::UNSPECIFIED,
            tenpo: None,
            mobi: None,
            bb: None,
            dsl: None,
            auth_host: None,
            op_host: None,
            title_host: None,
            line: 0,
            hops: 0,
            trace_failures: 0,
            tracer: Tracer::default(),
            al: AlAuth::default(),
            round: 0,
            packet: Vec::new(),
            auth_text: String::new(),
            abex: Abex::default(),
            done: false,
            good: false,
            latched: 0,
            offline_from_billing: false,
            location: None,
            time: None,
            text: None,
            client_id: 0,
            wan_latched: false,
            date_synced: false,
            rtc: Rtc::new(),
            generation: 0,
            restarted: false,
            dl_allowed: false,
            dl_requested: false,
            dl_waiting: false,
            dl_result: 0,
            dl_uri: None,
            recovery_enabled: false,
            recovering: false,
            al_progress: 0,
            test_flag: false,
            fatal: 0,
            bias_minutes: -540,
        }
    }
}

/// `[u16; N]` text of `s`, truncated and NUL-filled.
fn wide<const N: usize>(s: &[u8]) -> [u16; N] {
    let mut out = [0u16; N];
    for (slot, ch) in out.iter_mut().zip(String::from_utf8_lossy(s).chars().take(N - 1)) {
        *slot = u32::from(ch) as u16;
    }
    out
}

fn local_secs(clock: &dyn Clock) -> i64 {
    clock.local_now().0 / 1_000_000
}

fn number(bytes: &[u8]) -> u32 {
    std::str::from_utf8(bytes).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
}

impl Auth {
    /// `OperatingState::enter`; the RTC is initialised here.
    pub fn initialize(&mut self, cx: &Cx<'_>, cfg: &AllnetAuthConfig, test_mode: bool) {
        *self = Self {
            cfg: cfg.clone(),
            recovery_enabled: cx.config.allnet_accounting.mode == 3,
            test_flag: test_mode,
            ..Self::default()
        };
        // `amRtcInit` over the system file; without one the RTC stays uninitialised and never reports a sync.
        let _ = self.rtc.init(cx.nvram);
    }

    /// Terminate: closes alAuth and alAbEx (`0x1403C5BD0`).
    pub fn terminate(&mut self) {
        let _ = self.al.reset();
        self.abex.terminate();
        let _ = self.rtc.exit();
        *self = Self { cfg: std::mem::take(&mut self.cfg), ..Self::default() };
    }

    fn auth20(&self) -> bool {
        self.cfg.auth_type == 1
    }

    /// The auth result is good (`LM+0x4B8 == 0`).
    pub fn is_good(&self) -> bool {
        self.good
    }

    /// The detected line mask.
    pub fn line_mask(&self) -> u32 {
        u32::from(self.line & 0x0E)
    }

    /// The location answer.
    pub fn location(&self) -> Option<AuthLocation> {
        self.location
    }

    /// The last authentication time.
    pub fn time(&self) -> Option<TimeSlot> {
        self.time
    }

    /// The Auth 2.0 text.
    pub fn text(&self) -> Option<[u8; 1536]> {
        self.text
    }

    /// `AuthInfo +0x0C`: the client id (`allnet_id=`), 0 until authenticated.
    pub fn client_id(&self) -> u32 {
        self.client_id
    }

    /// The latched alw error code (0 none).
    pub fn latched(&self) -> u32 {
        self.latched
    }

    /// The current phase.
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// Whether the server time was applied (`amRtcIsServerSync`: the RTC takes the server time of the answer).
    pub fn date_synced(&self) -> bool {
        self.date_synced
    }

    /// The allnet type (1 allnet, 2 title router).
    pub fn allnet_type(&self) -> u8 {
        self.allnet_type
    }

    /// The alw status kind 0 (`0x1403B3330`, allnet.md "Status kinds"): `0` not started, `1` network bring-up,
    /// `2` wait-recovery mode, `3` authenticating, `5` / `6` timeout / refusal, `4` authenticated, `7` offline,
    /// `25` fatal. Phase 14 (Auth 2.0 wait) answers by the Auth 2.0 progress code: `5` after a timeout, `6`
    /// after a communication error, else `3`.
    pub fn status(&self) -> u32 {
        if self.recovering && self.phase != Phase::Fatal {
            return 2;
        }
        match self.phase {
            Phase::Setup => 0,
            Phase::WaitNetwork | Phase::DnsLan => 1,
            Phase::Traceroute
            | Phase::LineType
            | Phase::DnsWan
            | Phase::Auth20Init
            | Phase::Auth20Dongle
            | Phase::Auth20Server
            | Phase::AuthInit => 3,
            Phase::Auth20Wait => match self.al_progress {
                21 => 5,
                20 => 6,
                _ => 3,
            },
            Phase::Authentication => match self.abex.state() {
                stat::AUTH_NG => 6,
                stat::AUTH_TIMEOUT => 5,
                stat::RETRY_WAIT if self.abex.last_failure_timeout() == Some(true) => 5,
                stat::RETRY_WAIT => 6,
                _ => 3,
            },
            Phase::AuthOk => 4,
            Phase::Offline => 7,
            Phase::Fatal => 25,
        }
    }

    /// `AuthInfo +0x20`: the alw status generation (kind 0) as of the last frame.
    pub fn generation(&self) -> u32 {
        self.generation
    }

    /// The hop count of the traceroute to the router (`0x1403B4480`), 0 when it failed or never ran.
    pub fn hops(&self) -> i32 {
        self.hops
    }

    /// `AuthInfo +0x24`: the generation became 3 in the last frame (an authentication (re)started).
    pub fn restarted(&self) -> bool {
        self.restarted
    }

    /// `amRtcIsServerSync` (`0x1402D9EA0`).
    pub fn server_time_synced(&self, clock: &dyn Clock) -> bool {
        self.rtc.is_server_synchronised(clock)
    }

    /// `alwGetDlUri` (`0x1403B9BB0`): the `uri=` text of the last download order, or the alw error code: `6`
    /// when no download order was answered since the last authentication, `10` when the text is empty or `null`.
    pub fn download_uri(&self) -> Result<&str, u32> {
        let uri = self.dl_uri.as_deref().ok_or(6u32)?;
        if uri.is_empty() || uri == "null" { Err(10) } else { Ok(uri) }
    }

    /// `alwRequestDownloadInstruction` (`0x1403B31C0`): asks for a download order. Refused with `6` unless the
    /// last authentication or download order finished (`AW+52`); the order starts at the next AUTH_OK frame.
    pub fn request_download(&mut self) -> Result<(), u32> {
        if !self.dl_allowed {
            return Err(6);
        }
        self.dl_requested = true;
        self.dl_allowed = false;
        Ok(())
    }

    /// The alw status kind 1 (`0x1403B36C0`): `9` not authenticated, `10` authenticated, `11` download order
    /// running, `12` / `13` / `14` answered / timed out / refused, `25` fatal.
    pub fn download_status(&self) -> u32 {
        match self.phase {
            Phase::Setup => 0,
            Phase::Fatal => 25,
            Phase::AuthOk if self.dl_waiting => 11,
            Phase::AuthOk => match self.dl_result {
                24 => 14,
                25 => 13,
                26 => 12,
                _ => 10,
            },
            _ => 9,
        }
    }

    /// `AW+20 = AW+24 = 0`, `AW+44 = AW+52 = 0` (7.1, 6.5): the authentication failed, so neither a download
    /// order nor its URI is available.
    fn lose_authentication(&mut self) {
        self.dl_uri = None;
        self.dl_allowed = false;
    }

    /// What billing reads from this module each frame.
    pub fn link(&self, operating_mode: u32) -> AuthLink {
        let identity = self.identity.clone().unwrap_or_default();
        AuthLink {
            done: self.done,
            offline: self.phase == Phase::Offline || self.offline_from_billing,
            good: self.good,
            line_byte: self.line,
            test_mode: self.test_flag,
            server: self.op_host,
            tenpo: if self.line & 0x0B != 0 { self.tenpo } else { self.mobi },
            identity: Identity {
                keychip: identity.keychip,
                board: identity.board,
                game_id: identity.game_id,
                game_version: identity.game_version,
                function_type: identity.function_type,
            },
            date_synced: self.date_synced,
            bias_minutes: self.bias_minutes,
            operating_mode,
        }
    }

    /// Applies what billing reported back.
    pub fn apply(&mut self, back: &BillingToAuth) {
        if let Some(t) = back.allnet_type {
            self.allnet_type = t;
        }
        if let Some(offline) = back.offline {
            self.offline_from_billing = offline;
        }
        if let Some(cause) = back.offline_cause {
            self.latched = cause;
        }
    }

    fn latch(&mut self, code: u32) {
        self.latched = code;
        self.good = false;
    }

    /// `0x1403C6EA0`: recovery is allowed while the accounting side has not been handed a tenpo address yet
    /// (`billing+2098084 == 0`; `billing+20 != 4` is the same DnsProc progress) [INFERENCE: billing's DnsProc
    /// state is not modelled separately, it follows the address `link()` publishes].
    fn recovery_allowed(&self) -> bool {
        (if self.line & 0x0B != 0 { self.tenpo } else { self.mobi }).is_none()
    }

    /// `AW+426304 := 1` when the option is on, the mode is not set yet and the predicate holds.
    fn enter_recovery(&mut self, router: &str) {
        if self.recovery_enabled && !self.recovering && self.recovery_allowed() {
            self.recovering = true;
            tracing::error!(router, "allnet: router is unreachable; waiting for the network to recover");
        }
    }

    /// `AW+426304 := 0`.
    fn leave_recovery(&mut self) {
        if self.recovering {
            self.recovering = false;
            tracing::error!("allnet: the network has recovered");
        }
    }

    /// One frame; returns an alw fatal error code when the machine entered the fatal phase. `link` is the dongle
    /// manager's keychip request layer, lent for the Auth 2.0 calls; `supplied` is the identity the platform
    /// managers publish, which the set-up phase waits for.
    pub fn update(
        &mut self,
        cx: &mut Cx<'_>,
        link: &mut Link,
        network_ready: bool,
        supplied: Option<&KeychipIdentity>,
    ) -> Option<u32> {
        if !self.cfg.enable || self.phase == Phase::Fatal {
            return None;
        }
        let tick = cx.clock.tick_ms();
        let fatal = self.run_phase(cx, link, network_ready, tick, supplied);
        let epoch = local_secs(cx.clock) - UNIX_OFFSET;
        let tls = cx.backend.tls.as_ref();
        self.abex.pump(&Pump { tls, tick, epoch });
        let env = Env { tls, tick, now: u64::try_from(epoch).unwrap_or(0) };
        self.al.exec(&env);
        if let Some(code) = fatal {
            self.phase = Phase::Fatal;
            self.fatal = code;
            self.latch(code);
        }
        // The generation snapshot of `AuthInfo +0x20` and its "just became 3" flag (`+0x24`).
        let generation = self.status();
        self.restarted = generation == 3 && self.generation != 3;
        self.generation = generation;
        fatal
    }

    /// `amRtcSetServerTime(0, t + 32400, -tz_minutes, -540)` (`0x1403BA9C0`, allnet.md 5.6): the RTC keeps the
    /// server time. A failure is only logged by the original.
    fn set_rtc(&mut self, cx: &mut Cx<'_>, server: &ServerTime) {
        let field = |value: i32| u8::try_from(value).ok();
        let utc = (|| {
            Some(RtcTime {
                year: u16::try_from(server.year).ok()?,
                month: field(server.month)?,
                day: field(server.day)?,
                hour: field(server.hour)?,
                minute: field(server.minute)?,
                second: field(server.second)?,
            })
        })();
        let Some(shifted) =
            utc.and_then(|t| t.to_time_t().ok()).and_then(|t| RtcTime::from_time_t(t + JST_SECONDS).ok())
        else {
            return;
        };
        let clock = cx.clock;
        let system = &mut *cx.backend.system;
        let _ = self.rtc.set_server_time(clock, system, &shifted, -server.tz_minutes, -JST_MINUTES);
    }

    fn run_phase(
        &mut self,
        cx: &mut Cx<'_>,
        link: &mut Link,
        network_ready: bool,
        tick: u64,
        supplied: Option<&KeychipIdentity>,
    ) -> Option<u32> {
        match self.phase {
            Phase::Setup => {
                if let Some(identity) = supplied {
                    self.identity = Some(identity.clone());
                    self.local_ip = cx.backend.adapters.local_address().unwrap_or(Ipv4Addr::UNSPECIFIED);
                    self.phase = Phase::WaitNetwork;
                }
            }
            Phase::WaitNetwork => {
                if network_ready {
                    self.sub = 0;
                    self.entered = tick;
                    if self.allnet_type == 2 {
                        self.line = 2;
                        self.phase = Phase::DnsWan;
                    } else {
                        self.phase = Phase::DnsLan;
                    }
                }
            }
            Phase::DnsLan => self.dns_lan(cx, tick),
            Phase::Traceroute => self.traceroute(cx),
            Phase::LineType => self.line_type(),
            Phase::DnsWan => self.dns_wan(cx, tick),
            Phase::Auth20Init => {
                let _ = self.al.reset();
                let Some(server) = self.auth_host else { return Some(800) };
                if self.al.init(server).is_err() {
                    return Some(800);
                }
                self.phase = Phase::Auth20Wait;
            }
            Phase::Auth20Wait => {
                self.al_progress = 18;
                if matches!(self.al.state(), State::Idle | State::PacketReady) {
                    self.round = 0;
                    self.phase = Phase::Auth20Dongle;
                } else if self.al.state() != State::Running {
                    return self.al_error();
                }
            }
            Phase::Auth20Dongle => return self.dongle_round(cx, link),
            Phase::Auth20Server => return self.server_round(),
            Phase::AuthInit => return self.auth_init(cx),
            Phase::Authentication => return self.authentication(cx),
            Phase::AuthOk => return self.auth_ok(),
            Phase::Offline | Phase::Fatal => {}
        }
        None
    }

    /// `alDns`: a dotted IPv4 literal completes without DNS traffic; anything else is one A lookup with no
    /// time limit (`alDnsSetHostName`). Blocking, on the frame thread, as the name service was.
    fn resolve(dns: &dyn Dns, host: &str) -> Option<Ipv4Addr> {
        host.parse().ok().or_else(|| dns.lookup_a(host, None))
    }

    fn dns_lan(&mut self, cx: &mut Cx<'_>, tick: u64) {
        match self.sub {
            0 => {
                self.entered = tick;
                self.sub = 1;
            }
            1 => {
                self.tenpo = self.tenpo.or_else(|| Self::resolve(cx.backend.dns.as_ref(), "tenporouter.loc"));
                self.mobi = self.mobi.or_else(|| Self::resolve(cx.backend.dns.as_ref(), "mobirouter.loc"));
                let waited = tick.wrapping_sub(self.entered);
                if (self.tenpo.is_some() || self.mobi.is_some()) && waited > LOC_WAIT_MS {
                    self.sub = 2;
                } else if waited > NETWORK_TIMEOUT_MS {
                    if !self.wan_latched {
                        self.wan_latched = true;
                        self.latch(8008);
                    }
                    self.enter_recovery("ALL.Net router");
                }
            }
            _ => {
                if self.tenpo.is_some() {
                    self.bb = Self::resolve(cx.backend.dns.as_ref(), "bbrouter.loc");
                    self.dsl = Self::resolve(cx.backend.dns.as_ref(), "dslrouter.loc");
                }
                self.line = self.classify();
                self.leave_recovery();
                self.good = false;
                self.sub = 0;
                self.phase = Phase::Traceroute;
            }
        }
    }

    /// `0x1403C56D0`: bit 1 tenporouter only, 2 tenpo == bbrouter, 8 tenpo == dslrouter, 4 mobirouter present.
    fn classify(&self) -> u8 {
        let mut f = 0;
        if let Some(tenpo) = self.tenpo {
            f = if self.bb == Some(tenpo) {
                2
            } else if self.dsl == Some(tenpo) {
                8
            } else {
                1
            };
        }
        if self.mobi.is_some() {
            f |= 4;
        }
        f
    }

    fn traceroute(&mut self, cx: &mut Cx<'_>) {
        let target = if self.line == 4 { self.mobi } else { self.tenpo };
        let hops = match target {
            Some(t) => match self.tracer.poll(&cx.backend.icmp, t) {
                Trace::Running => return,
                Trace::Done(hops) => hops,
            },
            None => None,
        };
        match hops {
            Some(n) if n > 0 => {
                self.hops = i32::try_from(n).unwrap_or(99).min(99);
                self.trace_failures = 0;
                self.phase = Phase::LineType;
            }
            _ if self.trace_failures == 0 => self.trace_failures = 1,
            _ => {
                self.hops = 0;
                self.trace_failures = 0;
                self.phase = Phase::LineType;
            }
        }
    }

    fn line_type(&mut self) {
        let f = self.line;
        let mask = (self.cfg.support_line & 0x0E) as u8;
        if (f & 4 != 0 && f & 0xFB != 0) || (f & mask) == 0 {
            self.latch(8005);
            self.phase = Phase::Offline;
        } else {
            self.sub = 0;
            self.phase = Phase::DnsWan;
        }
    }

    fn dns_wan(&mut self, cx: &mut Cx<'_>, tick: u64) {
        if self.sub == 0 {
            self.entered = tick;
            self.sub = 1;
        }
        let auth20 = self.auth20();
        if auth20 && self.auth_host.is_none() {
            self.auth_host = Self::resolve(cx.backend.dns.as_ref(), alauth::HOST);
        }
        if self.op_host.is_none() {
            self.op_host =
                Self::resolve(cx.backend.dns.as_ref(), if auth20 { "op.auth.sys-all.net" } else { "naominet.jp" });
        }
        if self.allnet_type == 2 && self.title_host.is_none() {
            self.title_host = Self::resolve(cx.backend.dns.as_ref(), "at.sys-all.net");
        }
        let ready = (!auth20 || self.auth_host.is_some())
            && self.op_host.is_some()
            && (self.allnet_type != 2 || self.title_host.is_some());
        if ready {
            self.sub = 0;
            self.phase = if auth20 { Phase::Auth20Init } else { Phase::AuthInit };
        } else if tick.wrapping_sub(self.entered) > NETWORK_TIMEOUT_MS && !self.wan_latched {
            self.wan_latched = true;
            self.latch(8007);
        }
    }

    /// The `E` handling shared by the Auth 2.0 phases. `Some(code)` when fatal.
    fn al_error(&mut self) -> Option<u32> {
        match self.al.error() {
            0 => None,
            1 => Some(800),
            e => {
                if !self.al.limit_valid() {
                    // The title router mode enters the wait recovery, then the failure is latched.
                    if self.allnet_type == 2 {
                        self.enter_recovery("title router");
                    }
                    self.lose_authentication();
                    self.al_progress = if e == 2 { 21 } else { 20 };
                    self.latch(if e == 2 { 8201 } else { 8202 });
                }
                None
            }
        }
    }

    /// Round 0 asks the keychip for the first packet, later rounds answer the server's packet. The request
    /// layer is `link`; the device is `cx.backend.keychip`. A failed dongle call sends 64 (round 0) or 32 random
    /// bytes instead; a failed generator is fatal (800).
    fn dongle_round(&mut self, cx: &mut Cx<'_>, link: &mut Link) -> Option<u32> {
        let device = cx.backend.keychip.as_mut();
        let from_dongle = if self.round == 0 {
            link.auth_start(device).ok()
        } else {
            match self.al.get_data(Kind::Packet, 256) {
                Ok(server) => link.auth_packet(device, &server).ok(),
                Err(_) => None,
            }
        };
        let fallback = if self.round == 0 { 64 } else { 32 };
        let Some(bytes) = from_dongle.or_else(|| random_bytes(fallback).ok()) else { return Some(800) };
        self.packet = bytes;
        self.sub = 0;
        self.phase = Phase::Auth20Server;
        None
    }

    fn server_round(&mut self) -> Option<u32> {
        if self.sub == 0 {
            if matches!(self.al.state(), State::Idle | State::PacketReady)
                && self.al.set_packet_data(&self.packet).is_ok()
            {
                self.sub = 1;
            }
            return None;
        }
        if self.al.state() == State::Running {
            return None;
        }
        if let Some(code) = self.al_error() {
            return Some(code);
        }
        self.sub = 0;
        match self.al.state() {
            State::PacketReady => {
                self.round = 1;
                self.phase = Phase::Auth20Dongle;
            }
            State::Finished => {
                let data = self.al.get_data(Kind::AuthData, 1536).unwrap_or_default();
                let end = data.iter().position(|&c| c == 0).unwrap_or(data.len());
                self.auth_text = String::from_utf8_lossy(&data[..end]).into_owned();
                self.phase = Phase::AuthInit;
            }
            _ => self.phase = Phase::Auth20Wait,
        }
        None
    }

    fn auth_init(&mut self, cx: &mut Cx<'_>) -> Option<u32> {
        let identity = self.identity.clone().unwrap_or_default();
        let auth20 = self.auth20();
        let mode = if self.allnet_type == 2 {
            Mode::Title
        } else if auth20 {
            Mode::AuthData
        } else {
            Mode::Plain
        };
        let server_ip =
            if mode == Mode::Title { self.title_host } else { self.op_host }.unwrap_or(Ipv4Addr::UNSPECIFIED);
        let session = Session {
            mode,
            game_id: identity.game_id,
            version: identity.game_version,
            serial: identity.keychip,
            client_ip: self.local_ip,
            server_ip,
            server_ip2: self.op_host.unwrap_or(Ipv4Addr::UNSPECIFIED),
            hops: self.hops,
            auth_data: (auth20 && !self.auth_text.is_empty()).then(|| self.auth_text.clone()),
            encode: "UTF-8",
        };
        let _ = cx;
        self.abex.init(session, u32::from(!auth20));
        if self.abex.start() != 0 {
            return Some(800);
        }
        self.phase = Phase::Authentication;
        None
    }

    fn authentication(&mut self, cx: &mut Cx<'_>) -> Option<u32> {
        match self.abex.state() {
            stat::AUTH_BUSY => {}
            stat::AUTH_OK => self.succeed(cx),
            stat::START_WAIT => {
                if self.auth20() && self.al.state() == State::Idle {
                    self.phase = Phase::Auth20Wait;
                }
            }
            stat::AUTH_NG => {
                if self.allnet_type == 2 {
                    self.enter_recovery("title router");
                }
                self.lose_authentication();
                self.latch(8202);
            }
            stat::AUTH_TIMEOUT | stat::AUTH_COMMUNICATION => {
                if self.allnet_type == 2 {
                    self.enter_recovery("title router");
                }
                self.lose_authentication();
                self.latch(8201);
            }
            stat::RETRY_WAIT => {
                self.lose_authentication();
                let code = if self.abex.last_failure_timeout() == Some(true) { 8201 } else { 8202 };
                self.latch(code);
            }
            _ => return Some(800),
        }
        None
    }

    /// Phases 22 and 23 (`0x1403C48A0`, `0x1403C4CB0`).
    fn auth_ok(&mut self) -> Option<u32> {
        if self.dl_waiting {
            self.download_wait();
            return None;
        }
        match self.abex.state() {
            stat::START_WAIT => {
                self.phase = if self.auth20() { Phase::Auth20Wait } else { Phase::Authentication };
            }
            stat::AUTH_BUSY => self.phase = Phase::Authentication,
            // 6.3: authenticated and idle; a requested download order starts.
            stat::AUTH_OK if self.dl_requested => {
                if self.abex.start_dl() != 0 {
                    return Some(800);
                }
                self.dl_requested = false;
                self.dl_allowed = false;
                self.dl_waiting = true;
            }
            _ => {}
        }
        None
    }

    /// 6.4: waits for the download order; any outcome returns to AUTH_OK without invalidating the
    /// authentication.
    fn download_wait(&mut self) {
        let result = match self.abex.state() {
            stat::START_WAIT | stat::DLINSTR_BUSY => return,
            stat::DLINSTR_OK => 26,
            stat::DLINSTR_TIMEOUT => 25,
            _ => 24,
        };
        if result == 26 {
            // `0x1403C60B0`: the `uri=` text, up to 0x102 bytes with the terminator.
            self.dl_uri = self.abex.dl_response().map(|response| {
                let text = String::from_utf8_lossy(&response.dl_uri()).into_owned();
                text.chars().take(257).collect()
            });
        } else {
            self.dl_uri = None;
        }
        self.dl_result = result;
        self.dl_allowed = true;
        self.dl_waiting = false;
    }

    /// `0x1403C64F0`: reads the power-on answer into the location, the time and the Auth 2.0 text.
    fn succeed(&mut self, cx: &mut Cx<'_>) {
        // 6.2 step 2: a committed authentication leaves the wait recovery.
        self.leave_recovery();
        let Some(response) = self.abex.response() else { return };
        let title = self.allnet_type == 2;
        let value = |i: usize| response.value(i);
        let nick = value(3);
        let location = AuthLocation {
            id: number(&value(9)),
            develop: false,
            name: Some(wide::<61>(&value(2))),
            nicknames: [Some(wide::<41>(nick.split(|&c| c == b',').next().unwrap_or_default())), None],
            region_code: i32::try_from(number(&value(4))).unwrap_or(0),
            region_names: [
                Some(wide::<49>(&value(5))),
                Some(wide::<49>(&value(6))),
                Some(wide::<49>(&value(7))),
                Some(wide::<49>(&value(8))),
            ],
            country: Some(wide::<4>(&value(11))),
            uri: Some(wide::<129>(&value(0))),
            host: Some(wide::<129>(&value(1))),
        };
        let _ = title;
        self.client_id = number(&value(12));
        self.location = Some(location).filter(|l| l.id != 0 || self.client_id != 0);
        let shown = match response.notice {
            Some(Notice::Time(server)) => {
                // `alwOnReceiveAuthResponse` (`0x1403BA9C0`): the RTC takes the server time and the negated zone.
                self.date_synced = true;
                self.bias_minutes = -server.tz_minutes;
                self.set_rtc(cx, &server);
                DateTime::make(server.year, server.month, server.day, server.hour, server.minute, server.second, 0)
                    .map(|utc| DateTime(utc.0 + i64::from(server.tz_minutes) * 60_000_000).civil())
            }
            _ => None,
        };
        let civil = shown.unwrap_or_else(|| cx.clock.local_now().civil());
        self.time = Some(TimeSlot {
            valid: true,
            tm: Tm {
                sec: civil.second as i32,
                min: civil.minute as i32,
                hour: civil.hour as i32,
                mday: civil.day as i32,
                mon: civil.month as i32 - 1,
                year: civil.year as i32 - 1900,
                ..Tm::default()
            },
        });
        if !self.auth_text.is_empty() {
            let mut text = [0u8; 1536];
            let bytes = self.auth_text.as_bytes();
            let n = bytes.len().min(1535);
            text[..n].copy_from_slice(&bytes[..n]);
            self.text = Some(text);
        }
        self.done = true;
        self.good = true;
        self.latched = 0;
        // 6.2 steps 5 and 6: a finished authentication allows a download order and forgets the last result.
        self.dl_allowed = true;
        self.dl_result = 22;
        self.phase = Phase::AuthOk;
    }
}

/// An `alwRestart` request (`0x1403B1F80`, allnet.md "alwRestart"): `{kind, testMode, reserved, keep}` plus the
/// flag `a2`. libam only issues these two shapes, both with `keep == 0` and outside the daily re-authentication, so
/// the result-keeping checkpoint of the original is not modelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restart {
    /// `{8, test_mode, 0, 0}`, the manager reset (`0x14023C620`): sets the test flag, and takes the authentication
    /// back to its start only when a fatal error that the retry covers is latched.
    Network {
        /// The process is not in game mode.
        test_mode: bool,
    },
    /// `{1, 1, 0, 0}`, the test restart (`0x14023D0D0`): the network is checked again from the beginning.
    Recheck,
}

impl Auth {
    /// The alw test-mode flag (`R+272 & 0x100`).
    pub fn test_flag(&self) -> bool {
        self.test_flag
    }

    /// `0x1403C6D20(kind, R+276)`: whether the latched error allows the soft restart. Kind 1 always does; kind 8
    /// does for the retryable codes, and for 8001/8002 when the network-retry option is on (accounting mode 3) in
    /// game mode while the accounting side has no tenpo address yet (class 0 of the fatal switches,
    /// `0x1403C7590`; the switches of the other classes are 0 in this configuration).
    fn recoverable(&self, request: Restart) -> bool {
        match request {
            Restart::Recheck => true,
            Restart::Network { .. } => {
                matches!(self.fatal, 800 | 901 | 8001..=8008 | 8201 | 8202)
                    || (self.recovery_enabled
                        && !self.test_flag
                        && matches!(self.latched, 8001 | 8002)
                        && self.recovery_allowed())
            }
        }
    }

    /// `alwRestart` (`0x1403B1F80`, wrapper `0x1403B2BC0`) on the authentication side. `accounting` is the alw
    /// accounting flag (`0x1403B1E50`: the accounting mode is not 0), which decides whether the billing host of the
    /// DNS service survives a restart. The restart bookkeeping of the original (`R+572`, `R+576`, `R+580`, the
    /// deferred request) only waits for the DNS service to stop; every call completes here, so a restart is
    /// never in progress when the next one arrives. Errors are the alw result codes: `6` for a test restart
    /// while the test flag is not set, `18` while a fatal error is latched.
    pub fn restart(&mut self, request: Restart, accounting: bool) -> Result<(), u32> {
        if request == Restart::Recheck {
            if !self.test_flag {
                return Err(6);
            }
            if self.fatal != 0 {
                return Err(18);
            }
        }
        if let Restart::Network { test_mode } = request {
            self.test_flag = test_mode;
        }
        // `0x1403C71E0`: a re-check is refused (6, which the caller treats as done) while the alw phase is
        // INITIALIZE, WAIT_START_SETUP or SETUP.
        let refused = request == Restart::Recheck && self.phase == Phase::Setup;
        if self.recoverable(request) && !refused {
            self.reset_session(request, accounting);
        }
        self.fatal = 0;
        Ok(())
    }

    /// `0x1403C5D80` (terminate without abort), `0x1403C6960` (`0x1403C5770` for kind 8, the network re-check
    /// for kind 1), `0x1403BCAB0` and the DNS stop / re-initialisation of the pump (`0x1403BE4F0`, `0x1403BDCC0`):
    /// the phase machine starts over, the results and the resolved hosts are dropped.
    fn reset_session(&mut self, request: Restart, accounting: bool) {
        self.done = false;
        self.good = false;
        self.abex.terminate();
        let _ = self.al.reset();
        // The alw root keeps the init parameters (the identity): the set-up phases are only waited for when the
        // identity never arrived.
        self.phase = if self.identity.is_some() { Phase::WaitNetwork } else { Phase::Setup };
        self.sub = 0;
        self.round = 0;
        self.packet.clear();
        self.line = 0;
        self.hops = 0;
        self.trace_failures = 0;
        self.latched = 0;
        self.wan_latched = false;
        self.recovering = false;
        self.al_progress = 0;
        self.dl_requested = false;
        self.dl_waiting = false;
        self.lose_authentication();
        match request {
            Restart::Network { .. } => self.auth_text.clear(),
            Restart::Recheck => self.dl_result = 0,
        }
        // The DNS region is initialised again; slot 5 (the billing host) is saved and restored unless the billing
        // side restarts as well (`R+580`: accounting and a network-mode restart) [INFERENCE: slot 5 is `op_host`].
        let billing_host = self.op_host.take();
        self.tenpo = None;
        self.mobi = None;
        self.bb = None;
        self.dsl = None;
        self.auth_host = None;
        self.title_host = None;
        if request == Restart::Recheck || !accounting {
            self.op_host = billing_host;
        }
    }

    /// The AuthInfo reset of `0x14023D860` (`0x14028E300`): client id, time, location and Auth 2.0 text, the status
    /// generation and its "became 3" flag.
    pub fn clear_info(&mut self) {
        self.client_id = 0;
        self.time = None;
        self.location = None;
        self.text = None;
        self.generation = 0;
        self.restarted = false;
    }
}

fn random_bytes(n: usize) -> Result<Vec<u8>, getrandom::Error> {
    let mut buf = vec![0u8; n];
    getrandom::fill(&mut buf)?;
    Ok(buf)
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Auth")
            .field("phase", &self.phase)
            .field("done", &self.done)
            .field("good", &self.good)
            .finish_non_exhaustive()
    }
}
