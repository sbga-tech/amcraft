//! The altr traceroute (`allnet.md` part 5 sections 7.3 and 7.4): 3 probes per hop, up to 10 hops, 5000 ms
//! each. [`Tracer`] runs the probes on a worker through [`backend::Icmp`]; [`run`] decides what the
//! answers mean. [`Reply`] is the same four outcomes as [`backend::EchoStatus`].

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use backend::Icmp;

/// What one probe was answered with (the reply class of `0x1403F08D0`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    /// Echo reply: the destination was reached (class 0).
    Reached,
    /// Net, host, protocol or port unreachable, also a local "network unreachable" (class 1).
    Unreachable,
    /// TTL expired in transit: an intermediate hop answered (class 2).
    Expired,
    /// No answer within the probe timeout, or any other status (classes 3 and 4).
    Silent,
}

impl Reply {
    /// The class of an IP status of the Windows ICMP helper (`IP_SUCCESS` 0, `IP_DEST_*_UNREACHABLE`
    /// 11002..11005, `IP_TTL_EXPIRED_TRANSIT` / `_REASSEM` 11013 / 11014).
    pub fn from_ip_status(status: u32) -> Self {
        match status {
            0 => Self::Reached,
            11002..=11005 => Self::Unreachable,
            11013 | 11014 => Self::Expired,
            _ => Self::Silent,
        }
    }
}

impl From<backend::EchoStatus> for Reply {
    fn from(status: backend::EchoStatus) -> Self {
        match status {
            backend::EchoStatus::Reached => Self::Reached,
            backend::EchoStatus::Unreachable => Self::Unreachable,
            backend::EchoStatus::Expired => Self::Expired,
            backend::EchoStatus::Silent => Self::Silent,
        }
    }
}

/// Runs the trace: `send(ttl)` sends one probe and returns its answer.
///
/// A hop is finished when one of its probes was answered by the destination or an unreachable report, or when it
/// is the last hop. The result is the number of that hop when the destination answered and the previous hop had
/// shown a TTL-expired answer (hop 1 has no previous hop); every other outcome is `None` (unknown, the hop count
/// 0 of the original).
pub fn run(max_hops: u8, probes_per_hop: u8, mut send: impl FnMut(u8) -> Reply) -> Option<u32> {
    let mut previous_expired = true;
    for hop in 1..=max_hops {
        let mut reached = false;
        let mut unreachable = false;
        let mut expired = false;
        for _ in 0..probes_per_hop {
            match send(hop) {
                Reply::Reached => reached = true,
                Reply::Unreachable => unreachable = true,
                Reply::Expired => expired = true,
                Reply::Silent => {}
            }
        }
        if reached || unreachable {
            return (reached && previous_expired).then_some(u32::from(hop));
        }
        previous_expired = expired;
    }
    None
}

/// Probes per hop, maximum hops and the probe timeout the authentication configures (`alwAuthPhaseGateway`).
const PROBES_PER_HOP: u8 = 3;
const MAX_HOPS: u8 = 10;
/// The probe timeout of altr.
const PROBE_TIMEOUT: Duration = Duration::from_millis(5000);

/// The state of a traceroute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trace {
    /// The probes are still out.
    Running,
    /// Finished: the hop count, `None` when the target was not reached (the original's "unknown ttls").
    Done(Option<u32>),
}

/// One altr traceroute on a worker thread (`allnet.md` part 5 section 7.3).
///
/// [`Tracer::poll`] is called once per frame with the same target until it answers. The first call starts the
/// probes, so the frame never waits. A different target drops that worker and starts another. A finished trace
/// is forgotten, so the next poll starts again (the authentication retries a failed trace once).
#[derive(Default)]
pub struct Tracer {
    job: Option<(Ipv4Addr, Receiver<Option<u32>>)>,
}

impl Tracer {
    /// Polls the trace to `target`: [`Trace::Running`] until the worker answers, then [`Trace::Done`].
    pub fn poll(&mut self, icmp: &Arc<dyn Icmp>, target: Ipv4Addr) -> Trace {
        let answer = match &mut self.job {
            Some((current, answer)) if *current == target => answer,
            _ => {
                self.job = Some((target, spawn(Arc::clone(icmp), target)));
                &mut self.job.as_mut().expect("the worker was just started").1
            }
        };
        let state = match answer.try_recv() {
            Ok(hops) => Trace::Done(hops),
            Err(mpsc::TryRecvError::Empty) => Trace::Running,
            Err(mpsc::TryRecvError::Disconnected) => Trace::Done(None),
        };
        if state != Trace::Running {
            self.job = None;
        }
        state
    }
}

/// Starts the trace to `target`; the answer (`hops`, or unknown) arrives on the returned channel.
fn spawn(icmp: Arc<dyn Icmp>, target: Ipv4Addr) -> Receiver<Option<u32>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        // The receiver is gone when the authentication was reset meanwhile; nobody wants the answer then.
        let _ = tx.send(trace_to(icmp.as_ref(), target));
    });
    rx
}

fn trace_to(icmp: &dyn Icmp, target: Ipv4Addr) -> Option<u32> {
    run(MAX_HOPS, PROBES_PER_HOP, |ttl| {
        icmp.echo(target, ttl, PROBE_TIMEOUT).map(|reply| Reply::from(reply.status)).unwrap_or(Reply::Silent)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A path of `routers` silent-or-expiring hops in front of the destination at hop `routers + 1`.
    fn path(routers: &[Reply], last: Reply) -> impl FnMut(u8) -> Reply + '_ {
        move |ttl| routers.get(usize::from(ttl) - 1).copied().unwrap_or(last)
    }

    #[test]
    fn destination_behind_two_routers_is_three_hops_away() {
        let routers = [Reply::Expired, Reply::Expired];
        assert_eq!(run(10, 3, path(&routers, Reply::Reached)), Some(3));
    }

    #[test]
    fn directly_attached_destination_is_one_hop_away() {
        assert_eq!(run(10, 3, path(&[], Reply::Reached)), Some(1));
    }

    #[test]
    fn a_silent_previous_hop_makes_the_count_unknown() {
        let routers = [Reply::Expired, Reply::Silent];
        assert_eq!(run(10, 3, path(&routers, Reply::Reached)), None);
    }

    #[test]
    fn unreachable_report_ends_the_trace_without_a_count() {
        let routers = [Reply::Expired];
        let mut probes = 0;
        let hops = run(10, 3, |ttl| {
            probes += 1;
            path(&routers, Reply::Unreachable)(ttl)
        });
        assert_eq!((hops, probes), (None, 6));
    }

    #[test]
    fn a_trace_that_never_arrives_stops_at_the_last_hop() {
        let mut probes = 0;
        let hops = run(10, 3, |_| {
            probes += 1;
            Reply::Expired
        });
        assert_eq!((hops, probes), (None, 30));
    }
}
