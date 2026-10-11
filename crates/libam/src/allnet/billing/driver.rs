//! The keychip of the accounting reporter (`spec/libam/platform.md` part B 5.3, `allnet.md` part 10).
//!
//! [`Billing`](super::Billing) attaches one [`DriverDongle`] to a second handle of `backend.keychip`
//! ([`KeychipDriver::duplicate`](backend::keychip::KeychipDriver::duplicate)). That handle runs the platform
//! [`Link`]: one setup poll per access, so the counters stay unavailable until the keychip is ready, and then the
//! billing and trace-log calls.

use std::time::Instant;

use super::records::{RECORD_CAPACITY, Record};
use crate::platform::keychip::{Link, SetupPoll};
use backend::keychip::KeychipDriver;

/// The keychip counters of the accounting reporter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counters {
    /// Play count (`ctx+1464`).
    pub play_count: u32,
    /// Play limit (`ctx+1476`).
    pub play_limit: u32,
    /// Near-full threshold (low 16 bits of the dongle word, `ctx+1488`).
    pub near_full: u16,
    /// Billing type (bit 16 of the dongle word, `ctx+1496`).
    pub billing_type: u32,
}

/// Largest CA certificate the reporter asks for (the ALL.Net buffer is 1024 bytes).
const CA_CAPACITY: usize = 1024;
/// Largest signature public key the reporter asks for (the ALL.Net buffer is 256 bytes).
const KEY_CAPACITY: usize = 256;
/// Records per `GetTraceData` call.
const READ_CHUNK: u32 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Setup {
    Closed,
    Running,
    Ready,
    Failed,
}

/// The keychip as the accounting reporter and the play-book use it.
pub struct DriverDongle {
    link: Link,
    keychip: Option<Box<dyn KeychipDriver>>,
    started: Instant,
    setup: Setup,
}

impl Default for DriverDongle {
    fn default() -> Self {
        Self { link: Link::default(), keychip: None, started: Instant::now(), setup: Setup::Closed }
    }
}

impl std::fmt::Debug for DriverDongle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DriverDongle")
            .field("attached", &self.keychip.is_some())
            .field("setup", &self.setup)
            .finish_non_exhaustive()
    }
}

impl DriverDongle {
    /// A second handle of `keychip`, opened by the first access. A later call does nothing: the reporter keeps
    /// the handle it already opened.
    pub fn attach(&mut self, keychip: &dyn KeychipDriver) {
        if self.keychip.is_none() {
            self.keychip = Some(keychip.duplicate());
            self.started = Instant::now();
            self.setup = Setup::Closed;
        }
    }

    /// The setup failed: the device cannot be opened, or it did not become ready.
    pub fn failed(&self) -> bool {
        self.setup == Setup::Failed
    }

    /// The play count, play limit and near-full threshold. `None` while the keychip is not ready.
    pub fn counters(&mut self) -> Option<Counters> {
        self.with(|link, keychip| {
            let play_count = link.play_count(keychip).ok()?;
            let play_limit = link.playlimit(keychip).ok()?;
            let word = link.nearfull(keychip).ok()?;
            Some(Counters {
                play_count,
                play_limit,
                near_full: u16::try_from(word & 0xFFFF).ok()?,
                billing_type: (word >> 16) & 1,
            })
        })
        .flatten()
    }

    /// Adds `delta` to the play count (one increment per count, the play start/end `Increment`).
    pub fn add_play_count(&mut self, delta: u32) -> bool {
        self.with(|link, keychip| (0..delta).all(|_| link.add_play_count(keychip).is_ok())).unwrap_or(false)
    }

    /// Stores a signed play limit. `false` when the signature is not 128 bytes or the keychip refuses it.
    pub fn set_play_limit(&mut self, value: u32, signature: &[u8]) -> bool {
        let Ok(signature) = <&[u8; 128]>::try_from(signature) else { return false };
        self.with(|link, keychip| link.update_playlimit(keychip, value, signature).is_ok()).unwrap_or(false)
    }

    /// Stores a signed near-full threshold. `false` when the signature is not 128 bytes or the keychip refuses it.
    pub fn set_near_full(&mut self, value: u32, signature: &[u8]) -> bool {
        let Ok(signature) = <&[u8; 128]>::try_from(signature) else { return false };
        self.with(|link, keychip| link.update_nearfull(keychip, value, signature).is_ok()).unwrap_or(false)
    }

    /// The keychip's CA certificate (`cacert.pem`), empty when the keychip does not answer.
    pub fn ca_certificate(&mut self) -> Vec<u8> {
        self.with(|link, keychip| link.ca_certification(keychip, CA_CAPACITY).unwrap_or_default()).unwrap_or_default()
    }

    /// The signature public key, empty when the keychip does not answer.
    pub fn public_key(&mut self) -> Vec<u8> {
        self.with(|link, keychip| link.signature_pub_key(keychip, KEY_CAPACITY).unwrap_or_default()).unwrap_or_default()
    }

    /// The trace log, oldest first. Empty when the keychip does not answer.
    pub fn trace_records(&mut self) -> Vec<Record> {
        self.with(|link, keychip| {
            let (logged, _address) = link.td_log_num(keychip).ok()?;
            let total = logged.min(u32::try_from(RECORD_CAPACITY).unwrap_or(u32::MAX));
            let mut records = Vec::new();
            while let Ok(first) = u32::try_from(records.len()) {
                if first >= total {
                    break;
                }
                let chunk = link.trace_data(keychip, first, (total - first).min(READ_CHUNK)).ok()?;
                if chunk.is_empty() {
                    break;
                }
                records.extend(chunk);
            }
            Some(records)
        })
        .flatten()
        .unwrap_or_default()
    }

    /// Appends `record`. `false` when the log is full or the keychip is not ready.
    pub fn put_trace(&mut self, record: Record) -> bool {
        self.with(|link, keychip| link.put_trace_data(keychip, &record).is_ok()).unwrap_or(false)
    }

    /// Drops the `count` oldest trace records (`amDongleBillingTdLogicalErase`).
    pub fn erase_oldest(&mut self, count: usize) {
        let count = u32::try_from(count).unwrap_or(u32::MAX);
        self.with(|link, keychip| link.td_logical_erase(keychip, count));
    }

    /// Frees the sectors of the logically erased records.
    pub fn erase_used_sectors(&mut self) -> bool {
        self.with(|link, keychip| link.td_erase_used_sector(keychip).is_ok()).unwrap_or(false)
    }

    /// Repairs the trace-log area (`amDongleBillingTdRestore`). `false` when the keychip reports a failure.
    pub fn restore_trace(&mut self) -> bool {
        self.with(|link, keychip| link.td_restore(keychip).is_ok()).unwrap_or(false)
    }

    /// Advances the setup by one poll and reports whether the keychip is ready.
    fn ready(&mut self) -> bool {
        if self.keychip.is_none() {
            return false;
        }
        if self.setup == Setup::Closed {
            let keychip = self.keychip.as_mut().expect("attached");
            if self.link.init(keychip.as_mut()).is_err() {
                self.setup = Setup::Failed;
                return false;
            }
            self.setup = Setup::Running;
        }
        match self.setup {
            Setup::Ready => true,
            Setup::Failed => false,
            Setup::Running => self.poll_setup(),
            Setup::Closed => false,
        }
    }

    fn poll_setup(&mut self) -> bool {
        let Some(keychip) = self.keychip.as_mut() else { return false };
        let now = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        match self.link.setup_keychip(keychip.as_mut(), now) {
            Ok(SetupPoll::Done) => {
                self.setup = Setup::Ready;
                true
            }
            Ok(SetupPoll::InProgress) => false,
            Ok(SetupPoll::Failed) | Err(_) => {
                self.setup = Setup::Failed;
                false
            }
        }
    }

    /// Runs `request` once the keychip is ready. `None` while it is not.
    fn with<R>(&mut self, request: impl FnOnce(&mut Link, &mut dyn KeychipDriver) -> R) -> Option<R> {
        if !self.ready() {
            return None;
        }
        let DriverDongle { link, keychip, .. } = self;
        Some(request(link, keychip.as_mut()?.as_mut()))
    }
}
