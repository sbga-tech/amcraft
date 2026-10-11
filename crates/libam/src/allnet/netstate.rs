//! The network-state block of the ALL.Net manager (`ALLNET+1512`, `spec/libam/network.md` T.11.2) the network test
//! reads: the detection words of the alw phases (1 = ok, 2 = ng, 0 = pending), each sticky once it is ok.
//!
//! The original derives them from the alw getters (`0x14023E200`); here they follow the authentication phase
//! machine, which passes the same stages in the same order: network up (IP address and gateway), DNS-LAN, the
//! traceroute hop count, the line type, then the authentication itself.

use super::auth::{Auth, Phase};

/// The words the test items 0 to 5 consume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetState {
    /// `+0` IP address stage.
    pub ip: i32,
    /// `+4` gateway stage.
    pub gateway: i32,
    /// `+8` DNS-LAN stage.
    pub dns: i32,
    /// `+16` hop count, -1 until the traceroute ended.
    pub hops: i32,
    /// `+20` line-type stage.
    pub line: i32,
    /// `+28` / `+32`: the authentication sub-stage is ok and the alw status is 4.
    pub authenticated: bool,
}

impl Default for NetState {
    fn default() -> Self {
        Self { ip: 0, gateway: 0, dns: 0, hops: -1, line: 0, authenticated: false }
    }
}

/// How far the phase machine got; `None` for the two end phases, which keep what was reached.
fn progress(phase: Phase) -> Option<u8> {
    Some(match phase {
        Phase::Setup => 0,
        Phase::WaitNetwork => 1,
        Phase::DnsLan => 2,
        Phase::Traceroute => 3,
        Phase::LineType => 4,
        Phase::DnsWan => 5,
        Phase::Auth20Init | Phase::Auth20Wait | Phase::Auth20Dongle | Phase::Auth20Server | Phase::AuthInit => 6,
        Phase::Authentication => 7,
        Phase::AuthOk => 8,
        Phase::Offline | Phase::Fatal => return None,
    })
}

/// A stage word: once ok it stays ok; a pending word turns ng on one of its alw error codes.
fn word(current: &mut i32, reached: bool, failed: bool) {
    if *current != 1 {
        *current = if reached {
            1
        } else if failed {
            2
        } else {
            0
        };
    }
}

impl NetState {
    /// One frame of `0x14023E200`, after the authentication machine ran.
    pub fn observe(&mut self, auth: &Auth) {
        let phase = auth.phase();
        let reached = progress(phase);
        let at_least = |stage: u8| reached.is_some_and(|rank| rank >= stage);
        let latched = auth.latched();
        word(&mut self.ip, at_least(2), matches!(latched, 8001 | 8002));
        word(&mut self.gateway, self.ip == 1, matches!(latched, 8003 | 8004));
        word(&mut self.dns, at_least(3), latched == 8008);
        // The title router path (allnet type 2) has no traceroute: the counter stays unset.
        if self.hops <= 0 && auth.allnet_type() != 2 && at_least(4) {
            self.hops = auth.hops();
        }
        word(&mut self.line, at_least(5), phase == Phase::Offline && latched == 8005);
        self.authenticated = auth.status() == 4;
    }
}
