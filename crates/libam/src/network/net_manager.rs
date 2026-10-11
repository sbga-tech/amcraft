//! The plain network manager's setup commands and stage machine (`spec/libam/network.md` M.2-M.9).
//!
//! The original chains `am::Command` objects through `CommandSequencer`s; the observable behaviour (one step
//! per frame, retry waits of 60/120/240/480/900 s for the gateway and the DNS-LAN check, 300 s for the
//! location router, the error codes and their order) is kept here as direct state machines.

use libam_api::am_config::NetworkConfig;

use super::engine::{AmNetwork, PropertyBuffer, TraceState};
use crate::module::ErrorCell;
use backend::Backend;

/// `am::Command` state: idle.
pub const IDLE: u8 = 0;
/// Running.
pub const RUNNING: u8 = 1;
/// Finished OK.
pub const OK: u8 = 4;
/// Finished failed.
pub const FAILED: u8 = 5;

/// Whether a command state is "finished" (4 or 5).
pub const fn finished(state: u8) -> bool {
    state == OK || state == FAILED
}

/// The services a step may use.
pub struct Io<'a> {
    /// The amNetwork / amDns / altr engine.
    pub net: &'a mut AmNetwork,
    /// Backend services the engine's workers share (`adapters`, `dns`, `icmp`, `registry`).
    pub backend: &'a mut Backend,
    /// Whole seconds of the monotonic clock.
    pub now_s: u64,
}

pub fn cell(number: u16) -> ErrorCell {
    let mut cell = ErrorCell::EMPTY;
    cell.set(number, 0);
    cell
}

/// `inet_addr` dword of dotted octets.
pub const fn addr(a: u8, b: u8, c: u8, d: u8) -> u32 {
    u32::from_le_bytes([a, b, c, d])
}

fn in_net10(ip: u32) -> bool {
    ip.to_le_bytes()[0] == 10
}

fn link_local(ip: u32) -> bool {
    ip.to_le_bytes()[..2] == [169, 254]
}

/// The 24-byte property record (M.2.6).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Property {
    /// DHCP.
    pub dhcp: bool,
    /// Address.
    pub ip: u32,
    /// Mask.
    pub mask: u32,
    /// Gateway.
    pub gateway: u32,
    /// Primary DNS.
    pub dns1: u32,
    /// Secondary DNS.
    pub dns2: u32,
}

impl Property {
    pub fn buffer(&self) -> PropertyBuffer {
        PropertyBuffer {
            flags: u32::from(self.dhcp),
            ip: self.ip,
            mask: self.mask,
            gateway: self.gateway,
            dns1: self.dns1,
            dns2: self.dns2,
        }
    }

    /// The six dwords as stored in `NetworkEth0Info` (bytes 8..32).
    pub fn to_words(self) -> [u32; 6] {
        [u32::from(self.dhcp), self.ip, self.mask, self.gateway, self.dns1, self.dns2]
    }

    /// Reads the six dwords of the record.
    pub fn from_words(words: [u32; 6]) -> Self {
        Self {
            dhcp: words[0] & 1 != 0,
            ip: words[1],
            mask: words[2],
            gateway: words[3],
            dns1: words[4],
            dns2: words[5],
        }
    }
}

/// `NetworkPropertySetter` (M.9.1).
#[derive(Debug, Default)]
pub struct PropertySetter {
    pub(super) state: u8,
    pub(super) error: ErrorCell,
    issued: bool,
    buffer: PropertyBuffer,
    last: u8,
}

impl PropertySetter {
    /// Start; `apply` is the `needApply` decision with the buffer to push (`None` when nothing to do).
    pub fn start(&mut self, apply: Option<PropertyBuffer>) {
        self.last = self.state;
        self.state = RUNNING;
        self.error = ErrorCell::EMPTY;
        self.issued = false;
        match apply {
            Some(buffer) => self.buffer = buffer,
            None => self.state = OK,
        }
    }

    /// Whether the previous run failed (forces a re-apply).
    pub fn previously_failed(&self) -> bool {
        self.last == FAILED || self.state == FAILED
    }

    /// Records a persistence failure (error 84/87, state 5).
    pub fn fail(&mut self, error: ErrorCell) {
        self.error = error;
        self.state = FAILED;
    }

    /// One frame.
    pub fn step(&mut self, io: &mut Io<'_>) {
        if !(RUNNING..=3).contains(&self.state) {
            return;
        }
        if !self.issued {
            let rc = io.net.start_modify_property(io.backend, &self.buffer);
            self.issued = rc != 1;
            return;
        }
        let result = io.net.result();
        if result == 1 {
            return;
        }
        self.state = OK;
        match result {
            0 => self.error = ErrorCell::EMPTY,
            -22 => {
                self.error = cell(8301);
                self.issued = false;
                self.state = RUNNING;
            }
            -17 => {
                self.error = cell(if self.buffer.flags & 1 != 0 { 8003 } else { 8004 });
                self.state = FAILED;
            }
            _ => {
                self.error = cell(32);
                self.state = FAILED;
            }
        }
    }

    /// Cancel (`0x140294010`): a running setter ends failed without an error (the next start re-applies).
    pub fn cancel(&mut self) {
        if (RUNNING..=3).contains(&self.state) {
            self.state = FAILED;
        }
    }

    /// Reset (`0x140294020`) without touching the state, used by exit.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// `IpAddressChecker` (M.9.2).
#[derive(Debug, Default)]
pub struct IpChecker {
    pub(super) state: u8,
    pub(super) error: ErrorCell,
    issued: bool,
    /// The record (valid once the checker classified an address).
    pub(super) record: Property,
    /// Keychip network address (`+460`).
    pub(super) keychip_net: u32,
}

impl IpChecker {
    /// Reset (`0x1402930F0`).
    pub fn reset(&mut self) {
        self.state = IDLE;
        self.error = ErrorCell::EMPTY;
        self.issued = false;
        self.record = Property::default();
    }

    /// Start.
    pub fn start(&mut self) {
        self.reset();
        self.state = RUNNING;
    }

    fn restart(&mut self) {
        self.issued = false;
        self.state = RUNNING;
        self.record = Property::default();
    }

    /// The property the facade reports: finished and `ip != 0`.
    pub fn property(&self) -> Option<Property> {
        (finished(self.state) && self.record.ip != 0).then_some(self.record)
    }

    /// `0x1402943E0`: finished, `ip != 0` and the address is in the keychip network.
    pub fn lan_ready(&self) -> bool {
        self.property().is_some_and(|p| self.keychip_net == (p.mask & p.ip))
    }

    /// One frame.
    pub fn step(&mut self, mode: u32, io: &mut Io<'_>) {
        if !(RUNNING..=3).contains(&self.state) {
            return;
        }
        if !self.issued {
            let rc = io.net.start_get_property(&io.backend.adapters);
            self.issued = rc != 1;
            return;
        }
        let result = io.net.result();
        if result == 1 {
            return;
        }
        self.state = OK;
        if result == -22 {
            self.error = cell(8301);
            self.restart();
            return;
        }
        if result != 0 {
            self.error = cell(32);
            self.state = FAILED;
            return;
        }
        self.error = ErrorCell::EMPTY;
        let buffer = io.net.property();
        if buffer.flags & PropertyBuffer::LINK_DOWN != 0 {
            self.error = cell(8301);
            self.restart();
            return;
        }
        let record = Property {
            dhcp: buffer.flags & 1 != 0,
            ip: buffer.ip,
            mask: buffer.mask,
            gateway: buffer.gateway,
            dns1: buffer.dns1,
            dns2: buffer.dns2,
        };
        let lan = self.keychip_net == (record.ip & record.mask);
        let net10 = in_net10(record.ip);
        let ok = match mode {
            1 => net10,
            2 => lan || net10,
            _ => lan,
        };
        if ok {
            self.record = record;
        } else if link_local(record.ip) {
            self.error = cell(if record.dhcp { 8006 } else { 8301 });
            self.restart();
        } else {
            self.record = record;
            self.error = cell(if record.dhcp { 8001 } else { 8002 });
            self.state = FAILED;
        }
    }
}

/// The wait series shared by the gateway and DNS-LAN checks: 60, 120, 240, 480, 900, 900, ...
fn next_wait(wait: &mut i64) -> u64 {
    let used = u64::try_from(*wait).unwrap_or(0);
    *wait = (*wait * 2).min(900);
    used
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Phase {
    #[default]
    Check,
    Trace,
    Wait {
        since: u64,
        interval: u64,
    },
}

/// `GatewayChecker` (M.9.4) with its `GatewayPreChecker` and `HopsCounter`.
#[derive(Debug, Default)]
pub struct GatewayChecker {
    pub(super) state: u8,
    pub(super) error: ErrorCell,
    record: Property,
    wait: i64,
    cancelled: bool,
    phase: Phase,
}

impl GatewayChecker {
    /// Reset.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// The gateway address the check works on.
    pub fn gateway(&self) -> u32 {
        self.record.gateway
    }

    /// Start with the IP checker's record.
    pub fn start(&mut self, record: Property, now_s: u64) {
        self.reset();
        self.record = record;
        self.wait = 60;
        self.state = RUNNING;
        self.phase = if record.gateway != 0 { Phase::Check } else { Phase::Wait { since: now_s, interval: 0 } };
    }

    /// Cancels the retry loop.
    pub fn cancel(&mut self, io: &mut Io<'_>) {
        self.cancelled = true;
        io.net.trace_cancel();
        if self.state == RUNNING {
            self.state = FAILED;
        }
    }

    /// One frame.
    pub fn step(&mut self, io: &mut Io<'_>) {
        if self.state != RUNNING {
            return;
        }
        match self.phase {
            Phase::Check => {
                let r = self.record;
                if r.dhcp || (r.mask & r.ip) == (r.mask & r.gateway) {
                    self.phase = if io.net.trace_start(&io.backend.icmp, r.gateway) == 0 {
                        Phase::Trace
                    } else {
                        self.retry(io.now_s)
                    };
                } else {
                    self.error = cell(8002);
                    self.state = FAILED;
                }
            }
            Phase::Trace => {
                let state = io.net.trace_poll();
                if state != TraceState::Running {
                    if state == TraceState::Finished && io.net.trace_hops() != 0 {
                        self.error = ErrorCell::EMPTY;
                        self.state = OK;
                    } else {
                        self.phase = self.retry(io.now_s);
                    }
                }
            }
            Phase::Wait { since, interval } => {
                if io.now_s.saturating_sub(since) >= interval {
                    if self.record.gateway == 0 {
                        self.error = ErrorCell::EMPTY;
                        self.state = OK;
                    } else {
                        self.phase = Phase::Check;
                    }
                }
            }
        }
    }

    fn retry(&mut self, now_s: u64) -> Phase {
        self.error = cell(8301);
        if self.cancelled {
            self.state = FAILED;
            return Phase::Check;
        }
        Phase::Wait { since: now_s, interval: next_wait(&mut self.wait) }
    }
}

/// `DnsLanChecker` (M.9.5) with its two `NameResolver`s.
#[derive(Debug, Default)]
pub struct DnsChecker {
    pub(super) state: u8,
    pub(super) error: ErrorCell,
    /// Resolved router address.
    pub(super) resolved: u32,
    wait: i64,
    cancelled: bool,
    resolver: usize,
    results: [u32; 2],
    phase: Option<Phase>,
}

const ROUTER_NAMES: [&str; 2] = ["tenporouter.loc", "mobirouter.loc"];

impl DnsChecker {
    /// Reset hook: only the resolved address is cleared (`+320 := 0`) plus the base reset.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Start.
    pub fn start(&mut self, io: &mut Io<'_>) {
        self.wait = 60;
        self.state = RUNNING;
        self.error = ErrorCell::EMPTY;
        self.begin_resolving(io);
    }

    fn begin_resolving(&mut self, io: &mut Io<'_>) {
        self.resolver = 0;
        self.results = [0; 2];
        self.phase = Some(Phase::Trace);
        let _ = io.net.dns_resolve(&io.backend.dns, ROUTER_NAMES[0]);
    }

    /// Cancels the retry loop.
    pub fn cancel(&mut self, io: &mut Io<'_>) {
        self.cancelled = true;
        io.net.dns_cancel();
        if self.state == RUNNING {
            self.state = FAILED;
        }
    }

    /// One frame.
    pub fn step(&mut self, io: &mut Io<'_>) {
        if self.state != RUNNING {
            return;
        }
        match self.phase {
            Some(Phase::Trace) => {
                let r = io.net.dns_step();
                if r == 1 {
                    return;
                }
                // The finalize hook turns a failed lookup into "ok, result 0".
                self.results[self.resolver] = if r == 0 { io.net.dns_result() } else { 0 };
                if self.resolver == 0 {
                    self.resolver = 1;
                    let _ = io.net.dns_resolve(&io.backend.dns, ROUTER_NAMES[1]);
                    return;
                }
                let found = if self.results[0] != 0 { self.results[0] } else { self.results[1] };
                if found != 0 {
                    self.resolved = found;
                    self.error = ErrorCell::EMPTY;
                    self.state = OK;
                } else {
                    self.error = cell(8008);
                    if self.cancelled {
                        self.state = FAILED;
                    } else {
                        self.phase = Some(Phase::Wait { since: io.now_s, interval: next_wait(&mut self.wait) });
                    }
                }
            }
            Some(Phase::Wait { since, interval }) if io.now_s.saturating_sub(since) >= interval => {
                self.begin_resolving(io);
            }
            _ => {}
        }
    }
}

/// `LocationRouterChecker` (M.9.8).
#[derive(Debug, Default)]
pub struct LocationRouterChecker {
    pub(super) state: u8,
    pub(super) error: ErrorCell,
    /// Hop count of the last successful trace (`+328`).
    pub(super) hops: u32,
    target: u32,
    cancelled: bool,
    phase: Option<Phase>,
}

const LOCATION_WAIT_S: u64 = 300;

impl LocationRouterChecker {
    /// Reset hook: the hop count is cleared.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Start toward the resolved router address.
    pub fn start(&mut self, target: u32, io: &mut Io<'_>) {
        self.target = target;
        self.state = RUNNING;
        self.error = ErrorCell::EMPTY;
        self.phase = Some(self.begin_trace(io));
    }

    fn begin_trace(&mut self, io: &mut Io<'_>) -> Phase {
        if io.net.trace_start(&io.backend.icmp, self.target) == 0 { Phase::Trace } else { self.fail(io.now_s) }
    }

    fn fail(&mut self, now_s: u64) -> Phase {
        self.error = cell(8301);
        if self.cancelled {
            self.state = FAILED;
            return Phase::Check;
        }
        Phase::Wait { since: now_s, interval: LOCATION_WAIT_S }
    }

    /// Cancels the retry loop.
    pub fn cancel(&mut self, io: &mut Io<'_>) {
        self.cancelled = true;
        io.net.trace_cancel();
        if self.state == RUNNING {
            self.state = FAILED;
        }
    }

    /// One frame.
    pub fn step(&mut self, io: &mut Io<'_>) {
        if self.state != RUNNING {
            return;
        }
        match self.phase {
            Some(Phase::Trace) => {
                let state = io.net.trace_poll();
                if state != TraceState::Running {
                    let hops = io.net.trace_hops();
                    if state == TraceState::Finished && hops != 0 {
                        self.hops = hops;
                        self.error = ErrorCell::EMPTY;
                        self.state = OK;
                    } else {
                        self.phase = Some(self.fail(io.now_s));
                    }
                }
            }
            Some(Phase::Wait { since, interval }) if io.now_s.saturating_sub(since) >= interval => {
                self.phase = Some(self.begin_trace(io));
            }
            _ => {}
        }
    }
}

/// Whether `Config.location_router_directly` is deny (0) or allow (2): the LAN-available modes.
pub fn lan_modes(config: &NetworkConfig) -> bool {
    matches!(config.location_router_directly, 0 | 2)
}
