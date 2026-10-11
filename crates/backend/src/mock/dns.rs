//! Scripted A lookups. The default answers `tenporouter.loc`, which the network test resolves without scripting.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::Dns;

/// `tenporouter.loc` in a healthy cabinet.
const TENPO_ROUTER: &str = "tenporouter.loc";

#[derive(Debug, Default)]
struct State {
    hosts: HashMap<String, Ipv4Addr>,
    /// Every name looked up, in order, including failures.
    resolved: Vec<String>,
}

/// Scripted DNS. [`SimDns::set_host`] with `None` makes the name fail; an unscripted name fails too. Lookups
/// answer at once and ignore the time limit.
#[derive(Debug, Clone)]
pub struct SimDns {
    state: Arc<Mutex<State>>,
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|err| err.into_inner())
}

impl SimDns {
    /// `tenporouter.loc` resolves to `10.0.0.1`.
    pub fn new() -> Self {
        let mut hosts = HashMap::new();
        hosts.insert(TENPO_ROUTER.to_owned(), Ipv4Addr::new(10, 0, 0, 1));
        Self { state: Arc::new(Mutex::new(State { hosts, resolved: Vec::new() })) }
    }

    /// Makes `name` resolve to `address`, or fail when `None`.
    pub fn set_host(&self, name: &str, address: Option<Ipv4Addr>) {
        let mut state = lock(&self.state);
        match address {
            Some(address) => {
                state.hosts.insert(name.to_owned(), address);
            }
            None => {
                state.hosts.remove(name);
            }
        }
    }

    /// Every name passed to [`Dns::lookup_a`], in order.
    pub fn resolved(&self) -> Vec<String> {
        lock(&self.state).resolved.clone()
    }
}

impl Default for SimDns {
    fn default() -> Self {
        Self::new()
    }
}

impl Dns for SimDns {
    fn lookup_a(&self, host: &str, _limit: Option<Duration>) -> Option<Ipv4Addr> {
        let mut state = lock(&self.state);
        state.resolved.push(host.to_owned());
        state.hosts.get(host).copied()
    }
}
