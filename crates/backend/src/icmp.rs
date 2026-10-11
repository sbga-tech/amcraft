//! One ICMP echo, the probe the altr traceroute sends (`allnet.md` part 5 section 7.3, `network.md` P.11).

use std::fmt;
use std::net::Ipv4Addr;
use std::time::Duration;

use crate::Unsupported;

/// What one echo was answered with: the four reply classes of `0x1403F08D0`. libam's traceroute reply
/// (`libam::allnet::trace::Reply`) is these same outcomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EchoStatus {
    /// Echo reply: the destination was reached (class 0).
    Reached,
    /// Net, host, protocol or port unreachable, also a local "network unreachable" (class 1).
    Unreachable,
    /// TTL expired in transit: an intermediate hop answered (class 2).
    Expired,
    /// No answer within the probe timeout, or any other status (classes 3 and 4).
    Silent,
}

/// One echo's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EchoReply {
    /// Which of the four reply classes came back.
    pub status: EchoStatus,
    /// The address that answered, when the probe reported one.
    pub from: Option<Ipv4Addr>,
    /// Time from send to the classified answer.
    pub rtt: Duration,
}

/// The ICMP handle could not be created, or the send failed in a way that is not one of the four reply classes.
/// A traceroute aborts on this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum IcmpError {
    /// `IcmpCreateFile` failed.
    #[error("icmp handle could not be created")]
    Unavailable,
    /// A Win32 error outside the IP-status range of the reply classes.
    #[error("icmp echo failed ({0:#x})")]
    Send(u32),
}

/// ICMP echo with a TTL and a timeout. Blocking; one call is one probe. Shared by the worker threads of a
/// traceroute, so it is [`Sync`].
pub trait Icmp: Send + Sync + fmt::Debug {
    /// Sends one echo to `target` with `ttl`, waiting at most `timeout`.
    ///
    /// `Err` aborts the trace (no handle, or a send failure that is not a reply class). Every reply class,
    /// including a timeout, is [`Ok`].
    fn echo(&self, target: Ipv4Addr, ttl: u8, timeout: Duration) -> Result<EchoReply, IcmpError>;
}

/// No ICMP handle can be created.
impl Icmp for Unsupported {
    fn echo(&self, _target: Ipv4Addr, _ttl: u8, _timeout: Duration) -> Result<EchoReply, IcmpError> {
        Err(IcmpError::Unavailable)
    }
}
