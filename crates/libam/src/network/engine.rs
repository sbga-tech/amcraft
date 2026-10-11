//! The amNetwork / amDns / altr engine (`spec/libam/network.md` part P).
//!
//! One process-wide engine, owned by [`super::Network`] and shared with ALL.Net's netinfo. Enumeration,
//! read-back and `netsh` are [`Adapters`](backend::Adapters); name lookup is [`Dns`](backend::Dns);
//! traceroute probes are [`Icmp`](backend::Icmp). Registry reads and the `DisableDhcpOnConnect` write go
//! through [`Registry`](backend::Registry) on the frame thread. The 10 s settle is measured on libam's internal
//! [`Clock`](crate::clock::Clock), not slept inside the worker.
//!
//! * amNetwork (P.4 - P.8): adapter choice (Sega OUI first, lowest MAC first, `main_nic` registry flag), the
//!   property words, validation, the settle and the APIPA check. A get runs on a worker. A set is staged:
//!   the frame plans it (validation, DHCP registry reads, and the link-down registry write, which the adapter
//!   contract requires before `netsh`), a worker applies, the frame starts the settle, the clock waits 10 s,
//!   a worker reads back, and the frame does the link and APIPA checks.
//! * amDns (P.10): an A lookup on a worker (system DNS, 30 s limit).
//! * altr (P.11): an ICMP echo traceroute, one worker per trace, hop rows through [`crate::allnet::trace`].

use std::fmt;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::thread;
use std::time::Duration;

use crate::allnet::trace::{self, Reply};
use backend::{AdapterId, Adapters, Backend, DhcpApply, Dns, Icmp, InterfaceKind, KeyMode, RegValue, Registry};

/// The fixed wait after a write before the read-back (P.5 `0x1402CCE30`).
const SETTLE: Duration = Duration::from_secs(10);
/// amDns timeout (`0x18`, 30 000 000 us).
const DNS_TIMEOUT: Duration = Duration::from_secs(30);
/// `amNetworkExit` waits this long for the request in flight (`amyAsyncWaitForObject(obj, 3000)`).
const EXIT_WAIT: Duration = Duration::from_millis(3000);
/// First two bytes of a link-local (APIPA) network, read as the property words are (`0x0000FEA9`).
const LINK_LOCAL: u32 = 0x0000_FEA9;
/// MAC prefix of the Sega NIC (`0x140318B80`).
const SEGA_OUI: [u8; 3] = [0x00, 0xD0, 0xF1];
/// The Context's altr parameters (P.2): 9 hops, 3 probes per hop, altr's default 5000 ms probe timeout.
const MAX_TTL: u8 = 9;
const PROBES_PER_HOP: u8 = 3;
const PROBE_TIMEOUT: Duration = Duration::from_millis(5000);

const MEDIA_KEY: &str = r"System\Sega\SystemProperty\wirelessNetwork";
const KEYCHIP_KEY: &str = r"System\Sega\SystemProperty\keychip";
const TCPIP_INTERFACES: &str = r"SYSTEM\CurrentControlSet\services\Tcpip\Parameters\Interfaces";

/// The raw amNetwork property buffer, first six dwords (`0x1402CBFC0`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PropertyBuffer {
    /// Bit 0 DHCP, bit 1 link down.
    pub flags: u32,
    /// IPv4 address (first octet in the low byte).
    pub ip: u32,
    /// Subnet mask.
    pub mask: u32,
    /// Default gateway.
    pub gateway: u32,
    /// Primary DNS.
    pub dns1: u32,
    /// Secondary DNS.
    pub dns2: u32,
}

impl PropertyBuffer {
    /// Bit 0: DHCP enabled.
    pub const DHCP: u32 = 1;
    /// Bit 1: link down.
    pub const LINK_DOWN: u32 = 2;
}

/// The altr engine state (`0x1403BA260`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceState {
    /// Not initialised.
    Uninitialized,
    /// Idle.
    Idle,
    /// A traceroute is running.
    Running,
    /// A traceroute finished; [`AmNetwork::trace_hops`] reads it.
    Finished,
}

/// A DWORD of an existing `HKLM` key; `None` when it cannot be read.
fn read_dword(registry: &mut dyn Registry, key: &str, name: &str) -> Option<u32> {
    match registry.read(key, KeyMode::Open, name) {
        Ok(RegValue::Dword(value)) => Some(value),
        _ => None,
    }
}

/// An adapter chosen by the enumeration (`amyNetwork` handle: ifIndex and GUID name).
#[derive(Debug, Clone)]
struct Adapter {
    index: u32,
    guid: String,
    mac: [u8; 6],
    /// First IPv4 address (`0.0.0.0` when unset).
    ip: Ipv4Addr,
    /// The operational state is "up" (`IsLinkup`).
    up: bool,
}

/// The two sub-lists of one adapter group (P.4 step 3): Sega OUI first, at most four each.
#[derive(Default)]
struct Group {
    sega: Vec<Adapter>,
    other: Vec<Adapter>,
}

impl Group {
    fn push(&mut self, adapter: Adapter) {
        let list = if adapter.mac[..3] == SEGA_OUI { &mut self.sega } else { &mut self.other };
        if list.len() < 4 {
            list.push(adapter);
        }
    }

    /// Sub-list A then B, each sorted by MAC (big-endian order = lexicographic order of the bytes).
    fn ordered(mut self) -> Vec<Adapter> {
        self.sega.sort_by_key(|adapter| adapter.mac);
        self.other.sort_by_key(|adapter| adapter.mac);
        self.sega.extend(self.other);
        self.sega
    }
}

/// The adapter behind Eth0: the first entry of the wired group, or of the wireless group when `wireless`.
///
/// Adapters of type "other" or "loopback" and those without a 6-byte MAC are skipped; IEEE 802.11 adapters form
/// the wireless group, everything else the wired one (P.4 step 2).
fn select_eth0(adapters: &dyn Adapters, wireless: bool) -> Option<Adapter> {
    let (mut wired, mut wifi) = (Group::default(), Group::default());
    for nic in adapters.list() {
        let Some(mac) = nic.mac else { continue };
        let group = match nic.kind {
            InterfaceKind::Unknown | InterfaceKind::Loopback => continue,
            InterfaceKind::Wireless => &mut wifi,
            InterfaceKind::Other => &mut wired,
        };
        group.push(Adapter { index: nic.index, guid: nic.name, mac, ip: nic.ip, up: nic.up });
    }
    let (mut wired, mut wifi) = (wired.ordered(), wifi.ordered());
    // P.4 step 6: the merged list holds at most eight entries.
    if wired.len() + wifi.len() > 8 {
        if wired.len() > 4 && wifi.len() > 4 {
            wired.truncate(4);
            wifi.truncate(4);
        } else if wired.len() >= wifi.len() {
            wifi.truncate(8 - wired.len());
        } else {
            wired.truncate(8 - wifi.len());
        }
    }
    let chosen = if wireless { wifi } else { wired };
    chosen.into_iter().next()
}

/// The address and link state of the adapter LAN install binds to (`amyNetwork`, `0x1402F6EE0`): the same
/// adapter choice as Eth0. `None` when there is no such adapter.
pub(crate) fn lan_adapter(adapters: &dyn Adapters, wireless: bool) -> Option<(Ipv4Addr, bool)> {
    select_eth0(adapters, wireless).map(|adapter| (adapter.ip, adapter.up))
}

/// `IsLinkup`: `Some(operational)`, `None` when the adapter is not enumerated any more.
fn link_up(adapters: &dyn Adapters, index: u32) -> Option<bool> {
    adapters.list().into_iter().find(|nic| nic.index == index).map(|nic| nic.up)
}

/// An IPv4 address as the property words keep it (first octet in the low byte).
fn word(address: Ipv4Addr) -> u32 {
    u32::from_le_bytes(address.octets())
}

fn address(word: u32) -> Ipv4Addr {
    Ipv4Addr::from(word.to_le_bytes())
}

/// `htonl(0xFFFFFFFF << (32 - prefix))`; the x86 shift count wraps, so prefix 0 gives all ones. Any other prefix
/// uses the mask the enumeration reported.
fn mask_word(prefix: u8, mask: Ipv4Addr) -> u32 {
    if prefix == 0 { u32::MAX } else { word(mask) }
}

fn id_of(adapter: &Adapter) -> AdapterId {
    AdapterId { index: adapter.index, name: adapter.guid.clone(), mac: adapter.mac }
}

/// Eth0's IP configuration read back (P.8). `Err` is the request result: -17 for a duplicate address, -1
/// otherwise.
fn read_property(adapters: &dyn Adapters, adapter: &Adapter) -> Result<PropertyBuffer, i32> {
    let config = adapters.read(&id_of(adapter)).map_err(|_| -1)?;
    if config.duplicate {
        return Err(-17);
    }
    let mut property = PropertyBuffer::default();
    if config.dhcp {
        property.flags = PropertyBuffer::DHCP;
    }
    if let Some(ip) = config.ip {
        property.ip = word(ip);
        property.mask = mask_word(config.prefix, config.mask);
    }
    property.gateway = config.gateway.map(word).unwrap_or(0);
    property.dns1 = config.dns.first().copied().map(word).unwrap_or(0);
    property.dns2 = config.dns.get(1).copied().map(word).unwrap_or(0);
    Ok(property)
}

/// The Tcpip registry key of an adapter.
fn tcpip_key(guid: &str) -> String {
    format!(r"{TCPIP_INTERFACES}\{guid}")
}

/// `amyNetworkIsEnableDhcp`: `EnableDHCP != 0 && DisableDhcpOnConnect == 0`; `None` when `EnableDHCP` cannot be
/// read.
fn dhcp_active(registry: &mut dyn Registry, guid: &str) -> Option<bool> {
    let key = tcpip_key(guid);
    let enable = read_dword(registry, &key, "EnableDHCP")?;
    let disable = read_dword(registry, &key, "DisableDhcpOnConnect").unwrap_or(0);
    Some(enable != 0 && disable == 0)
}

/// Why a write did not happen.
#[derive(Debug, PartialEq, Eq)]
enum WriteError {
    /// Validation failed (`0x81000007`): request result -2.
    Invalid,
    /// Anything else (`0x81000002`): request result -1.
    Failed,
}

/// `0x1402F84D0`: the first byte of an address must be 1..=223.
fn valid_host(address: u32) -> bool {
    (1..=223).contains(&address.to_le_bytes()[0])
}

fn validate(wanted: &PropertyBuffer) -> bool {
    if wanted.flags != 0 {
        return true;
    }
    valid_host(wanted.ip)
        && (wanted.gateway == 0 || valid_host(wanted.gateway))
        && (wanted.dns1 == 0 || valid_host(wanted.dns1))
        && (wanted.dns2 == 0 || valid_host(wanted.dns2))
}

/// What the worker applies. The frame has already validated and, for DHCP, read the Tcpip keys.
enum ApplyPlan {
    Dhcp(DhcpApply),
    Static { ip: Ipv4Addr, mask: Ipv4Addr, gateway: Option<Ipv4Addr>, dns: Vec<Ipv4Addr> },
}

/// `amyNetworkSetPropertyIpv4` (`0x1402F8FE0`), the frame's half: validation and the DHCP decision.
///
/// The link-down `DisableDhcpOnConnect` write happens here, before [`ApplyPlan`] is handed to the worker.
/// [`DhcpApply::AddressBestEffort`] is specified to run after that write.
fn plan_apply(
    registry: &mut dyn Registry,
    adapters: &dyn Adapters,
    adapter: &Adapter,
    wanted: &PropertyBuffer,
) -> Result<ApplyPlan, WriteError> {
    if !validate(wanted) {
        return Err(WriteError::Invalid);
    }
    let dhcp = wanted.flags & PropertyBuffer::DHCP != 0;
    if !dhcp {
        let mut dns = Vec::new();
        // `run_set` always configures Eth0 as the main adapter, so both DNS servers are written.
        if wanted.dns1 != 0 {
            dns.push(address(wanted.dns1));
        }
        if wanted.dns2 != 0 && wanted.dns2 != wanted.dns1 {
            dns.push(address(wanted.dns2));
        }
        let gateway = (wanted.gateway != 0).then(|| address(wanted.gateway));
        return Ok(ApplyPlan::Static { ip: address(wanted.ip), mask: address(wanted.mask), gateway, dns });
    }
    match dhcp_active(registry, &adapter.guid) {
        None => Err(WriteError::Failed),
        Some(true) => Ok(ApplyPlan::Dhcp(DhcpApply::Dns)),
        Some(false) => {
            let up = link_up(adapters, adapter.index) == Some(true);
            if !up {
                // The link is down: DHCP is switched on for the next connect, before `netsh`.
                let _ = registry.write(
                    &tcpip_key(&adapter.guid),
                    KeyMode::Open,
                    "DisableDhcpOnConnect",
                    &RegValue::Dword(0),
                );
            }
            let how = if up { DhcpApply::Address } else { DhcpApply::AddressBestEffort };
            Ok(ApplyPlan::Dhcp(how))
        }
    }
}

fn apply_plan(adapters: &dyn Adapters, adapter: &Adapter, plan: ApplyPlan) -> Result<(), ()> {
    let id = id_of(adapter);
    let applied = match plan {
        ApplyPlan::Dhcp(how) => adapters.apply_dhcp(&id, how),
        ApplyPlan::Static { ip, mask, gateway, dns } => adapters.apply_static(&id, ip, mask, gateway, &dns),
    };
    applied.map_err(|_| ())
}

/// The outcome a worker thread sends back: the request result and the property a read produced.
type JobOutcome = (i32, Option<PropertyBuffer>);

/// The request in flight (amNetwork runs one at a time). A set moves Apply -> Settle -> Read; `Ready` is a
/// result decided on the frame that `poll` publishes, so `result` stays pending for this frame.
enum Job {
    Ready(JobOutcome),
    Get(Receiver<JobOutcome>),
    Apply(Receiver<Result<(), ()>>),
    Settle { started_ms: u64 },
    Read(Receiver<Result<PropertyBuffer, i32>>),
}

fn spawn<T: Send + 'static>(name: &str, work: impl FnOnce() -> T + Send + 'static) -> Result<Receiver<T>, ()> {
    let (sender, outcome) = channel();
    thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let _ = sender.send(work());
        })
        .map(|_| outcome)
        .map_err(|_| ())
}

/// GetProperty chain `slot-open -> read -> collect` (P.5).
fn run_get(adapters: &dyn Adapters, adapter: &Adapter) -> JobOutcome {
    match read_property(adapters, adapter) {
        Ok(mut property) => {
            if link_up(adapters, adapter.index) != Some(true) {
                property.flags |= PropertyBuffer::LINK_DOWN;
            }
            (0, Some(property))
        }
        Err(code) => (code, None),
    }
}

/// One hop: the probes of the row go out together, each on its own ICMP handle as in the original, and the row is
/// complete when the last probe has answered or timed out. `None` when a probe failed hard (no ICMP handle, or a
/// send error that is no reply class), which aborts the trace.
fn probe_row(icmp: &Arc<dyn Icmp>, target: Ipv4Addr, ttl: u8) -> Option<Vec<Reply>> {
    thread::scope(|scope| {
        let probes: Vec<_> = (0..PROBES_PER_HOP)
            .map(|_| {
                let icmp = Arc::clone(icmp);
                thread::Builder::new()
                    .spawn_scoped(scope, move || {
                        icmp.echo(target, ttl, PROBE_TIMEOUT).ok().map(|reply| Reply::from(reply.status))
                    })
                    .ok()
            })
            .collect();
        probes.into_iter().map(|probe| probe?.join().ok().flatten()).collect()
    })
}

/// The traceroute on its worker thread (`altrServerMain`): the result is the hop count (0: unknown), `None` when
/// the trace was aborted or cancelled.
fn run_trace(icmp: Arc<dyn Icmp>, target: Ipv4Addr, cancel: &AtomicBool) -> Option<u32> {
    let mut failed = false;
    let mut row = Vec::new();
    // `trace::run` asks for exactly `PROBES_PER_HOP` answers of a hop in a row; they come from one concurrent row.
    let hops = trace::run(MAX_TTL, PROBES_PER_HOP, |ttl| {
        if failed || cancel.load(Ordering::Relaxed) {
            return Reply::Silent;
        }
        if row.is_empty() {
            match probe_row(&icmp, target, ttl) {
                Some(answers) => row = answers,
                None => {
                    failed = true;
                    return Reply::Silent;
                }
            }
        }
        row.pop().unwrap_or(Reply::Silent)
    });
    (!failed && !cancel.load(Ordering::Relaxed)).then_some(hops.unwrap_or(0))
}

/// The traceroute in progress (`altr` context).
struct Trace {
    state: TraceState,
    hops: u32,
    outcome: Receiver<Option<u32>>,
    cancel: Arc<AtomicBool>,
}

impl Trace {
    fn new(icmp: Arc<dyn Icmp>, target: Ipv4Addr) -> Self {
        let (sender, outcome) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancel);
        // A thread that cannot start drops the sender: the first poll then sees an aborted trace.
        let _ = thread::Builder::new().name("altr".into()).spawn(move || {
            let _ = sender.send(run_trace(icmp, target, &flag));
        });
        Self { state: TraceState::Running, hops: 0, outcome, cancel }
    }

    /// `altrServerMain`: `Err` aborts the traceroute.
    fn step(&mut self) -> Result<(), ()> {
        match self.outcome.try_recv() {
            Ok(Some(hops)) => {
                self.hops = hops;
                self.state = TraceState::Finished;
                Ok(())
            }
            Ok(None) | Err(TryRecvError::Disconnected) => Err(()),
            Err(TryRecvError::Empty) => Ok(()),
        }
    }

    /// `altrGetHopCount`.
    fn hops(&self) -> u32 {
        if self.state == TraceState::Finished { self.hops } else { 0 }
    }
}

impl Drop for Trace {
    /// A probe in flight finishes on its own (an ICMP echo cannot be withdrawn); its answer is dropped.
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Adapter, DNS and traceroute services. The original runs one amNetwork request at a time process-wide,
/// advanced by [`Self::poll`]; `start_*` returns 0 accepted / 1 busy and [`Self::result`] is 1 while pending.
#[derive(Default)]
pub struct AmNetwork {
    initialised: bool,
    altr: bool,
    eth0: Option<Adapter>,
    job: Option<Job>,
    last_result: i32,
    property: PropertyBuffer,
    lookup: Option<Receiver<Option<Ipv4Addr>>>,
    dns_out: u32,
    trace: Option<Trace>,
}

impl fmt::Debug for AmNetwork {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AmNetwork")
            .field("initialised", &self.initialised)
            .field("eth0", &self.eth0)
            .field("busy", &self.job.is_some())
            .finish_non_exhaustive()
    }
}

impl AmNetwork {
    fn publish(&mut self, outcome: JobOutcome) {
        let (result, property) = outcome;
        self.last_result = result;
        if let Some(property) = property {
            self.property = property;
        }
    }

    /// The request-start checks shared by get and modify (P.5): `Some(code)` ends the call.
    fn start_check(&mut self) -> Option<i32> {
        if !self.initialised {
            Some(-3)
        } else if self.job.is_some() {
            Some(1)
        } else if self.eth0.is_none() {
            // The slot-open step ends the request with -1.
            self.last_result = -1;
            Some(0)
        } else {
            None
        }
    }

    fn finish_set(&mut self, adapters: &dyn Adapters, property: PropertyBuffer) {
        let Some(adapter) = &self.eth0 else {
            self.last_result = -1;
            return;
        };
        if link_up(adapters, adapter.index) != Some(true) {
            self.last_result = 0;
            return;
        }
        let static_link_local = property.flags & PropertyBuffer::DHCP == 0 && property.mask & property.ip == LINK_LOCAL;
        self.last_result = if static_link_local { -17 } else { 0 };
    }

    /// Drops a settle immediately. A worker still applying, reading or getting is given [`EXIT_WAIT`].
    fn join_job(&mut self) {
        let Some(job) = self.job.take() else { return };
        match job {
            Job::Get(outcome) => {
                let _ = outcome.recv_timeout(EXIT_WAIT);
            }
            Job::Apply(outcome) => {
                let _ = outcome.recv_timeout(EXIT_WAIT);
            }
            Job::Read(outcome) => {
                let _ = outcome.recv_timeout(EXIT_WAIT);
            }
            Job::Settle { .. } | Job::Ready(_) => {}
        }
    }

    /// `amNetworkInit` + `amDnsInit` (+ `altrInitialize` when `use_altr`): `Err` carries the error code.
    pub fn init(&mut self, backend: &mut Backend, use_altr: bool) -> Result<(), i32> {
        if self.initialised {
            return Err(-4);
        }
        // A missing key or value counts as wired; an enumeration failure leaves Eth0 closed (P.4).
        let wireless = read_dword(&mut *backend.registry, MEDIA_KEY, "main_nic").unwrap_or(0) != 0;
        self.eth0 = select_eth0(&*backend.adapters, wireless);
        self.initialised = true;
        self.altr = use_altr;
        Ok(())
    }

    /// The matching exit. A worker still applying or reading is given 3 s (`EXIT_WAIT`); a settle has no worker and
    /// ends with the reset.
    pub fn exit(&mut self) {
        self.join_job();
        *self = Self::default();
    }

    /// The keychip network address (`amDongleGetNetworkAddress`).
    pub fn keychip_address(&self, registry: &mut dyn Registry) -> u32 {
        match registry.read(KEYCHIP_KEY, KeyMode::Open, "serverIpIpv4") {
            Ok(RegValue::Binary(bytes)) => <[u8; 4]>::try_from(bytes).map_or(0, u32::from_le_bytes),
            _ => 0,
        }
    }

    /// amNetwork poll (`0x1402CB7C0`): advances the pending request. The settle compares
    /// [`Clock::tick_ms`](crate::clock::Clock::tick_ms) with the tick at which the apply was observed; cancelling
    /// is [`Self::exit`] dropping the stage.
    pub fn poll(&mut self, backend: &mut Backend, clock: &dyn crate::clock::Clock) {
        let Some(job) = self.job.take() else { return };
        self.job = match job {
            Job::Ready(outcome) => {
                self.publish(outcome);
                None
            }
            Job::Get(outcome) => match outcome.try_recv() {
                Ok(outcome) => {
                    self.publish(outcome);
                    None
                }
                Err(TryRecvError::Empty) => Some(Job::Get(outcome)),
                Err(TryRecvError::Disconnected) => {
                    self.last_result = -1;
                    None
                }
            },
            Job::Apply(outcome) => match outcome.try_recv() {
                Ok(Ok(())) => Some(Job::Settle { started_ms: clock.tick_ms() }),
                Ok(Err(())) => {
                    self.last_result = -1;
                    None
                }
                Err(TryRecvError::Empty) => Some(Job::Apply(outcome)),
                Err(TryRecvError::Disconnected) => {
                    self.last_result = -1;
                    None
                }
            },
            Job::Settle { started_ms } => {
                let elapsed = clock.tick_ms().saturating_sub(started_ms);
                if elapsed >= SETTLE.as_millis() as u64 {
                    self.spawn_read(&backend.adapters)
                } else {
                    Some(Job::Settle { started_ms })
                }
            }
            Job::Read(outcome) => match outcome.try_recv() {
                Ok(Ok(property)) => {
                    self.finish_set(&*backend.adapters, property);
                    None
                }
                Ok(Err(code)) => {
                    self.last_result = code;
                    None
                }
                Err(TryRecvError::Empty) => Some(Job::Read(outcome)),
                Err(TryRecvError::Disconnected) => {
                    self.last_result = -1;
                    None
                }
            },
        };
    }

    fn spawn_read(&self, adapters: &Arc<dyn Adapters>) -> Option<Job> {
        let Some(adapter) = self.eth0.clone() else {
            return Some(Job::Ready((-1, None)));
        };
        let adapters = Arc::clone(adapters);
        match spawn("amNetwork", move || read_property(&*adapters, &adapter)) {
            Ok(outcome) => Some(Job::Read(outcome)),
            Err(()) => Some(Job::Ready((-1, None))),
        }
    }

    /// Get-property request (`0x1402CABC0`): 0 accepted, 1 busy, -3 not initialised.
    pub fn start_get_property(&mut self, adapters: &Arc<dyn Adapters>) -> i32 {
        if let Some(code) = self.start_check() {
            return code;
        }
        let Some(adapter) = self.eth0.clone() else { return 0 };
        let adapters = Arc::clone(adapters);
        match spawn("amNetwork", move || run_get(&*adapters, &adapter)) {
            Ok(outcome) => self.job = Some(Job::Get(outcome)),
            Err(()) => self.last_result = -1,
        }
        0
    }

    /// Modify-property request (`0x1402CB6A0`).
    pub fn start_modify_property(&mut self, backend: &mut Backend, buffer: &PropertyBuffer) -> i32 {
        if let Some(code) = self.start_check() {
            return code;
        }
        let Some(adapter) = self.eth0.clone() else { return 0 };
        let plan = match plan_apply(&mut *backend.registry, &*backend.adapters, &adapter, buffer) {
            Ok(plan) => plan,
            Err(WriteError::Invalid) => {
                self.job = Some(Job::Ready((-2, None)));
                return 0;
            }
            Err(WriteError::Failed) => {
                self.job = Some(Job::Ready((-1, None)));
                return 0;
            }
        };
        let adapters = Arc::clone(&backend.adapters);
        match spawn("amNetwork", move || apply_plan(&*adapters, &adapter, plan)) {
            Ok(outcome) => self.job = Some(Job::Apply(outcome)),
            Err(()) => self.last_result = -1,
        }
        0
    }

    /// amNetworkGetResult: 1 pending, else the last result (0 ok, -17 duplicate/link-local, ...).
    pub fn result(&self) -> i32 {
        if !self.initialised {
            -3
        } else if self.job.is_some() {
            1
        } else {
            self.last_result
        }
    }

    /// The property the last get request produced.
    pub fn property(&self) -> PropertyBuffer {
        self.property
    }

    /// amDnsResolveName (async): 0 accepted.
    pub fn dns_resolve(&mut self, dns: &Arc<dyn Dns>, name: &str) -> i32 {
        if !self.initialised {
            return -3;
        }
        if self.lookup.is_some() {
            return 1;
        }
        // An empty name or a dotted IPv4 literal ("already binary") is rejected.
        if name.is_empty() || name.parse::<Ipv4Addr>().is_ok() {
            return -2;
        }
        let name = name.to_owned();
        let dns = Arc::clone(dns);
        let Ok(outcome) = spawn("amDns", move || dns.lookup_a(&name, Some(DNS_TIMEOUT))) else {
            return -1;
        };
        self.dns_out = 0;
        self.lookup = Some(outcome);
        0
    }

    /// amDns exec + status: 1 busy, 0 done, else an error.
    pub fn dns_step(&mut self) -> i32 {
        let answer = match self.lookup.as_ref().map(|outcome| outcome.try_recv()) {
            None => return 0,
            Some(Err(TryRecvError::Empty)) => return 1,
            Some(Ok(answer)) => answer,
            Some(Err(TryRecvError::Disconnected)) => None,
        };
        self.lookup = None;
        match answer {
            Some(address) => {
                self.dns_out = word(address);
                0
            }
            None => -1,
        }
    }

    /// The resolved address (0 when none).
    pub fn dns_result(&self) -> u32 {
        self.dns_out
    }

    /// amDnsCancel.
    pub fn dns_cancel(&mut self) {
        // The lookup ends on its own within its limit; dropping the receiver discards the answer.
        self.lookup = None;
    }

    /// altrStartTraceroute: 0 accepted.
    pub fn trace_start(&mut self, icmp: &Arc<dyn Icmp>, target: u32) -> i32 {
        if !self.altr {
            return 3;
        }
        if self.trace.as_ref().is_some_and(|trace| trace.state == TraceState::Running) {
            return 4;
        }
        self.trace = Some(Trace::new(Arc::clone(icmp), Ipv4Addr::from(target.to_le_bytes())));
        0
    }

    /// altr poll.
    pub fn trace_poll(&mut self) -> TraceState {
        if !self.altr {
            return TraceState::Uninitialized;
        }
        let failed =
            self.trace.as_mut().is_some_and(|trace| trace.state == TraceState::Running && trace.step().is_err());
        if failed {
            // An internal error closes everything and leaves the engine idle.
            self.trace = None;
            return TraceState::Idle;
        }
        self.trace.as_ref().map(|trace| trace.state).unwrap_or(TraceState::Idle)
    }

    /// The hop count of a finished trace (0 when the destination was not reached).
    pub fn trace_hops(&self) -> u32 {
        self.trace.as_ref().map_or(0, Trace::hops)
    }

    /// altr cancel.
    pub fn trace_cancel(&mut self) {
        if self.trace.as_ref().is_some_and(|trace| trace.state == TraceState::Running) {
            self.trace = None;
        }
    }
}
