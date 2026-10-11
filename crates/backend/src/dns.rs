//! Name resolution (`alDns` / amDns): one A lookup against the system DNS configuration.
//!
//! [`HickoryDns`] is the default implementation, `DnsQuery_A` (`allnet.md` "Worker thread and name resolution") on
//! `hickory-resolver`. It reads the system configuration (the adapters' DNS servers). One resolver per lookup, so
//! nothing is cached (`DNS_QUERY_BYPASS_CACHE`); the hosts file is honoured, as `DnsQuery_A` does with the options
//! `0x88` (`DNS_QUERY_NO_HOSTS_FILE` is not set); only the A records of the answer count, so a CNAME chain is
//! followed by the resolver and skipped here. There is no NetBT in a DNS resolver (`DNS_QUERY_NO_NETBT`).

use std::fmt;
use std::net::Ipv4Addr;
use std::time::Duration;

use hickory_resolver::TokioResolver;
use hickory_resolver::config::ResolveHosts;
use hickory_resolver::proto::rr::{RData, RecordType};

/// A lookup of the machine's DNS configuration. Blocking; callers that must not stall a frame run it
/// on a worker.
pub trait Dns: Send + Sync + fmt::Debug {
    /// The first A record for `host`.
    ///
    /// The hosts file is honoured and nothing is cached. `None` when the lookup failed or ran longer than
    /// `limit` (`None` waits without a limit).
    fn lookup_a(&self, host: &str, limit: Option<Duration>) -> Option<Ipv4Addr>;
}

/// System-resolver A lookup. One current-thread runtime per call, on the calling thread, so nothing is shared
/// and nothing is cached between lookups.
#[derive(Debug, Default, Clone, Copy)]
pub struct HickoryDns;

impl Dns for HickoryDns {
    fn lookup_a(&self, host: &str, limit: Option<Duration>) -> Option<Ipv4Addr> {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().ok()?;
        runtime.block_on(async {
            let mut builder = TokioResolver::builder_tokio().ok()?;
            let options = builder.options_mut();
            options.use_hosts_file = ResolveHosts::Always;
            options.cache_size = 0;
            let resolver = builder.build().ok()?;
            let lookup = resolver.lookup(host, RecordType::A);
            let answer = match limit {
                Some(limit) => tokio::time::timeout(limit, lookup).await.ok()?,
                None => lookup.await,
            };
            answer.ok()?.answers().iter().find_map(|record| match &record.data {
                RData::A(address) => Some(address.0),
                _ => None,
            })
        })
    }
}
