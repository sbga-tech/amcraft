//! ServerIO (SVIO) sender: publishes the USB-IO / JVS body to the game through shared memory
//! (`amdaemon::ipc::serverIO::Sender`, `spec/ipc/transport.md` §6).
//!
//! Kernel objects for relation index `k`: section `amdipc_shm_daemon_svio_shm_<k>` (payload `0x1670` bytes) and
//! mutex `daemon_svio_mtx_<k>`. The payload is the packet
//!
//! ```text
//! 0x0000  u32 crc32      CRC-32/ISO-HDLC over body[0 .. 0x1668)
//! 0x0004  [u8; 0x1668]   ServerIO body, verbatim
//! 0x166C  [u8; 4]        tail: never written after construction (stale protected bytes once published)
//! ```
//!
//! protected in place with the same-logon memory protector. There is no change detection: every tick
//! republishes the whole body. A reader locks the mutex, copies the payload, unprotects it and checks the CRC.

use crate::schema::image::{SERVER_IO_BODY_SIZE, ServerIoImage};

use crate::ipc::mutex::MutexHolder;
use crate::ipc::relation::RELATION_COUNT;
use crate::ipc::shared_memory::SharedMemory;
use backend::ipc::{IpcPlatform, Services};

/// Payload size of the SVIO section: CRC + body + 4 tail bytes (`0x1670`).
pub const SERVER_IO_PACKET_LEN: usize = 4 + SERVER_IO_BODY_SIZE + 4;
/// Section name prefix (`0x1405F21C0`).
const SECTION_PREFIX: &str = "daemon_svio_shm_";
/// Mutex name prefix (`0x1405F73A8`).
const MUTEX_PREFIX: &str = "daemon_svio_mtx_";

/// Section name (without the `amdipc_shm_` prefix) of relation index `index`.
pub fn section_name(index: u32) -> String {
    format!("{SECTION_PREFIX}{index}")
}

/// Mutex name of relation index `index`.
pub fn mutex_name(index: u32) -> String {
    format!("{MUTEX_PREFIX}{index}")
}

/// `serverIO::Sender::Impl`: owns the live ServerIO body the JVS/USBIO controllers write into.
///
/// Without a valid relation index (or when the section cannot be mapped) the sender is inert: ticks and
/// resets do nothing, but the body stays usable.
#[derive(Debug)]
pub struct Sender {
    shm: Option<SharedMemory>,
    body: ServerIoImage,
    publish_count: u64,
    staging: Box<[u8; SERVER_IO_PACKET_LEN]>,
    mutex: Option<MutexHolder>,
    services: Services,
}

impl Sender {
    /// Creates the sender for `relation_index` (`0x140075DB0`): maps the section (zero-filled even when it
    /// already exists) and opens the publication mutex.
    pub fn new(platform: &dyn IpcPlatform, relation_index: Option<u32>) -> Self {
        let body = ServerIoImage::new();
        let mut sender = Self {
            shm: None,
            body,
            publish_count: 0,
            staging: Box::new([0; SERVER_IO_PACKET_LEN]),
            mutex: None,
            services: platform.services(),
        };
        if let Some(index) = relation_index.filter(|&index| index < RELATION_COUNT) {
            sender.shm = SharedMemory::create(platform, SERVER_IO_PACKET_LEN as u32, &section_name(index), true).ok();
            sender.mutex = MutexHolder::open(platform, Some(&mutex_name(index)));
        }
        sender
    }

    /// The section is mapped (`0x140075CB0`).
    pub fn is_valid(&self) -> bool {
        self.shm.is_some()
    }

    /// The live ServerIO body.
    pub fn body(&self) -> &ServerIoImage {
        &self.body
    }

    /// The live ServerIO body, for the controllers (`Server accessor 0x14006BD70`).
    pub fn body_mut(&mut self) -> &mut ServerIoImage {
        &mut self.body
    }

    /// Number of packets published since construction or the last reset (process-local, `0x140076190`).
    pub fn publish_count(&self) -> u64 {
        self.publish_count
    }

    /// Per-tick publication (`0x140075CF0`): republishes the whole body when the section is mapped.
    pub fn tick(&mut self) {
        if self.shm.is_some() {
            self.write_server_io_data_to_shm();
        }
    }

    /// `writeServerIODataToShm` (`0x1400762D0`).
    fn write_server_io_data_to_shm(&mut self) {
        let body = self.body.bytes();
        let crc = crc32fast::hash(body);
        self.staging[..4].copy_from_slice(&crc.to_le_bytes());
        self.staging[4..4 + SERVER_IO_BODY_SIZE].copy_from_slice(body);
        if !self.services.protect(&mut self.staging[..]) {
            tracing::error!("encrypting the serverio image failed");
            return;
        }
        let Some(shm) = self.shm.as_mut() else { return };
        if let Some(mutex) = self.mutex.as_mut() {
            mutex.lock();
        }
        // The payload is exactly the packet size, so the write cannot be out of range.
        let _ = shm.write(0, &self.staging[..]);
        if let Some(mutex) = self.mutex.as_mut() {
            mutex.unlock();
        }
        self.publish_count += 1;
    }

    /// Blanks the published region (`0x140076230`): the session restarted, so the previous packet is stale.
    /// The local body and staging packet are untouched.
    pub fn reset(&mut self) {
        let Some(shm) = self.shm.as_mut() else { return };
        self.publish_count = 0;
        if let Some(mutex) = self.mutex.as_mut() {
            mutex.lock();
        }
        let _ = shm.fill_zero(0, SERVER_IO_PACKET_LEN);
        if let Some(mutex) = self.mutex.as_mut() {
            mutex.unlock();
        }
    }
}

impl Drop for Sender {
    /// `0x140075C50`: blank the region before the mapping goes away.
    fn drop(&mut self) {
        self.reset();
    }
}
