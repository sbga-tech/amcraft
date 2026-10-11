//! The ICMP probe of the altr traceroute (`allnet.md` part 5 section 7.3, `network.md` P.11).
//!
//! `winping` is `IcmpSendEcho` with an echo without payload, the TTL of the hop and the caller's timeout. Its
//! `Error` classes are the IP statuses of the original's reply classes. Each probe opens its own handle, the way
//! amNetwork sends the probes of one hop.

use std::net::Ipv4Addr;
use std::ops::RangeInclusive;
use std::time::{Duration, Instant};

use winping::{Buffer, Error, Pinger};

use crate::{EchoReply, EchoStatus, Icmp, IcmpError};

/// `IP_STATUS_BASE` .. `MAX_IP_STATUS`: the range of the ICMP helper's IP status codes.
const IP_STATUS: RangeInclusive<u32> = 11000..=11050;

/// The class of an IP status of the Windows ICMP helper (`IP_SUCCESS` 0, `IP_DEST_*_UNREACHABLE`
/// 11002..11005, `IP_TTL_EXPIRED_TRANSIT` / `_REASSEM` 11013 / 11014). Same table as
/// [`crate::allnet::trace::Reply::from_ip_status`].
fn status_of(status: u32) -> EchoStatus {
    match status {
        0 => EchoStatus::Reached,
        11002..=11005 => EchoStatus::Unreachable,
        11013 | 11014 => EchoStatus::Expired,
        _ => EchoStatus::Silent,
    }
}

fn classify(error: Error) -> Result<EchoStatus, IcmpError> {
    Ok(match error {
        // `ERROR_NETWORK_UNREACHABLE` (the send failed) is a net-unreachable report too.
        Error::NetUnreachable | Error::HostUnreachable | Error::ProtocolUnreachable => EchoStatus::Unreachable,
        Error::TtlExpired | Error::ReassemblyExpired => EchoStatus::Expired,
        Error::Timeout | Error::NeedsFragmented => EchoStatus::Silent,
        Error::Other(status) if IP_STATUS.contains(&status) => status_of(status),
        Error::Other(code) => return Err(IcmpError::Send(code)),
    })
}

/// `IcmpSendEcho` with a caller-supplied timeout.
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsIcmp;

impl Icmp for WindowsIcmp {
    fn echo(&self, target: Ipv4Addr, ttl: u8, timeout: Duration) -> Result<EchoReply, IcmpError> {
        let mut pinger = Pinger::new_v4().ok_or(IcmpError::Unavailable)?;
        let timeout_ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        pinger.set_timeout(timeout_ms);
        pinger.set_ttl(ttl);
        let mut buffer = Buffer::new();
        let started = Instant::now();
        let sent = pinger.send4(target, &mut buffer);
        let rtt = started.elapsed();
        // Filled on success and on an IP-status failure; empty when the send itself failed.
        let from = buffer.responding_ipv4();
        let status = match sent {
            Ok(_) => EchoStatus::Reached,
            Err(error) => classify(error)?,
        };
        Ok(EchoReply { status, from, rtt })
    }
}
