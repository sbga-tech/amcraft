//! Scripted echo replies. A target `n` hops away answers `Expired` for `ttl < n` and `Reached` afterwards, which
//! is the hop count libam's traceroute (`libam::allnet::trace::run`) reports.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::{EchoReply, EchoStatus, Icmp, IcmpError};

#[derive(Debug)]
struct State {
    /// `None` (absent) is silent forever.
    hops: HashMap<Ipv4Addr, u8>,
    /// Distinct targets probed, in the order of the first echo.
    traced: Vec<Ipv4Addr>,
}

/// Scripted ICMP. The default path is the gateway at one hop and `10.0.0.1` at three, matching the network
/// test. The timeout is ignored; every echo answers at once.
#[derive(Debug, Clone)]
pub struct SimIcmp {
    state: Arc<Mutex<State>>,
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|err| err.into_inner())
}

impl SimIcmp {
    /// Gateway `192.168.0.1` is one hop away; `10.0.0.1` is three.
    pub fn new() -> Self {
        let mut hops = HashMap::new();
        hops.insert(Ipv4Addr::new(192, 168, 0, 1), 1);
        hops.insert(Ipv4Addr::new(10, 0, 0, 1), 3);
        Self { state: Arc::new(Mutex::new(State { hops, traced: Vec::new() })) }
    }

    /// `target` is `hops` away. `None` makes every echo to it [`EchoStatus::Silent`].
    pub fn set_hops(&self, target: Ipv4Addr, hops: Option<u8>) {
        let mut state = lock(&self.state);
        match hops {
            Some(hops) => {
                state.hops.insert(target, hops);
            }
            None => {
                state.hops.remove(&target);
            }
        }
    }

    /// Distinct targets [`Icmp::echo`] was called with, in order.
    pub fn traced(&self) -> Vec<Ipv4Addr> {
        lock(&self.state).traced.clone()
    }
}

impl Default for SimIcmp {
    fn default() -> Self {
        Self::new()
    }
}

impl Icmp for SimIcmp {
    fn echo(&self, target: Ipv4Addr, ttl: u8, _timeout: Duration) -> Result<EchoReply, IcmpError> {
        let mut state = lock(&self.state);
        if !state.traced.contains(&target) {
            state.traced.push(target);
        }
        let (status, from) = match state.hops.get(&target).copied() {
            Some(hops) if ttl < hops => (EchoStatus::Expired, Some(Ipv4Addr::new(192, 168, 0, 1))),
            Some(_) => (EchoStatus::Reached, Some(target)),
            None => (EchoStatus::Silent, None),
        };
        Ok(EchoReply { status, from, rtt: Duration::ZERO })
    }
}
