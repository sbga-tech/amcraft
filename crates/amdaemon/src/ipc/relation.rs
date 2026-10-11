//! The relation: a process-wide table that pairs a daemon with the game that talks to it
//! (`spec/ipc/server.md` §1.2).
//!
//! Daemon and game (and several instances of each) share the section `daemon_table_150805` holding two `u32`
//! bit masks, one bit per relation index `0..32`: `server_mask @ +0` and `client_mask @ +4`. A process of a
//! given role claims the lowest free index of its own mask, preferring an index the peer role has already
//! claimed (a peer is waiting there). Liveness of a claim is a named mutex per index and role that the owner
//! holds for its lifetime; a set bit whose mutex can be locked belongs to a dead process and is reclaimed.
//!
//! The index names the pipes of both Messengers (`SC<k>` and `BS<k>`/`BC<k>`), the ServerIO section and the
//! Backup section.

use crate::ipc::messenger::Messenger;
use crate::ipc::mutex::MutexHolder;
use crate::ipc::shared_memory::SharedMemory;
use backend::ipc::IpcPlatform;

/// Number of relation indices.
pub const RELATION_COUNT: u32 = 32;
/// Section name of the table (`0x1405F1868`); the section itself is `amdipc_shm_daemon_table_150805`.
pub const TABLE_NAME: &str = "daemon_table_150805";
/// Payload size of the table: two `u32` masks.
const TABLE_LEN: u32 = 8;
/// Mutex serialising index allocation and Messenger construction (`0x1405F1890`).
const TABLE_MUTEX_NAME: &str = "daemon_table_150805_mtx";
/// Prefix of the per-index liveness mutexes (`0x1405F1670`).
const LIVE_MUTEX_PREFIX: &str = "daemon_live_201223_";

/// Result of one index allocation (`0x140070980`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Allocation {
    /// The claimed index, `None` when every index is taken or the liveness mutex could not be locked.
    pub index: Option<u32>,
    /// The chosen index was already claimed by the peer role.
    pub peer_waiting: bool,
}

/// Name of the liveness mutex of `index` for a role (`0x140070860`).
pub fn live_mutex_name(is_server: bool, index: u32) -> String {
    format!("{LIVE_MUTEX_PREFIX}{}{index}", if is_server { "server" } else { "client" })
}

/// Claims a relation index in `table` (`0x140070980`). The caller holds the table mutex.
///
/// Returns the allocation together with the liveness mutex that is now held; it must be kept for as long as the
/// claim is wanted.
pub fn allocate_index(
    platform: &dyn IpcPlatform,
    table: &mut SharedMemory,
    is_server: bool,
) -> (Allocation, Option<MutexHolder>) {
    let (own_offset, peer_offset) = if is_server { (0, 4) } else { (4, 0) };
    let (Some(mut own), Some(peer)) = (read_mask(table, own_offset), read_mask(table, peer_offset)) else {
        return (Allocation { index: None, peer_waiting: false }, None);
    };
    let mut chosen: Option<u32> = None;
    let mut peer_waiting = false;
    for index in 0..RELATION_COUNT {
        let bit = 1u32 << index;
        if own & bit != 0 {
            // A mutex that can be locked has no living owner: the claim is stale.
            let mut live = MutexHolder::open(platform, Some(&live_mutex_name(is_server, index)));
            if let Some(live) = live.as_mut()
                && live.try_lock()
            {
                own &= !bit;
                write_mask(table, own_offset, own);
                live.unlock();
            }
        }
        if own & bit == 0 {
            if peer & bit != 0 {
                chosen = Some(index);
                peer_waiting = true;
                break;
            }
            chosen.get_or_insert(index);
        }
    }
    let Some(index) = chosen else { return (Allocation { index: None, peer_waiting }, None) };
    let mut live = MutexHolder::open(platform, Some(&live_mutex_name(is_server, index)));
    if live.as_mut().is_some_and(MutexHolder::lock) {
        own |= 1 << index;
        write_mask(table, own_offset, own);
        return (Allocation { index: Some(index), peer_waiting }, live);
    }
    (Allocation { index: None, peer_waiting: false }, None)
}

fn read_mask(table: &SharedMemory, offset: usize) -> Option<u32> {
    let mut bytes = [0u8; 4];
    table.read(offset, &mut bytes).then(|| u32::from_le_bytes(bytes))
}

fn write_mask(table: &mut SharedMemory, offset: usize, mask: u32) {
    // The table is 8 bytes and both offsets are inside it, so a failed write cannot happen for an attached table.
    let _ = table.write(offset, &mask.to_le_bytes());
}

/// `amdaemon::ipc::Relation` (`0x40` bytes): the claimed index plus the two Messengers that depend on it.
///
/// When the table cannot be mapped or no index is free, both Messengers are inert (state 0) and nothing is ever
/// published. Dropping the relation destroys the Messengers, then releases the claim.
#[derive(Debug)]
pub struct Relation {
    is_server: bool,
    index: Option<u32>,
    peer_waiting: bool,
    primary: Messenger,
    secondary: Messenger,
    table_mutex: Option<MutexHolder>,
    live_mutex: Option<MutexHolder>,
    table: Option<SharedMemory>,
}

impl Relation {
    /// Maps the table, claims an index and creates the primary (`SC<k>`) and browser (`BS<k>` / `BC<k>`)
    /// Messengers while holding the table mutex (`0x1400711F0`). The daemon passes `is_server = true`.
    pub fn new(platform: &dyn IpcPlatform, is_server: bool) -> Self {
        let mut table = SharedMemory::create(platform, TABLE_LEN, TABLE_NAME, false).ok();
        let mut table_mutex = MutexHolder::open(platform, Some(TABLE_MUTEX_NAME));
        let mut relation = Self {
            is_server,
            index: None,
            peer_waiting: false,
            primary: Messenger::inert(),
            secondary: Messenger::inert(),
            table_mutex: None,
            live_mutex: None,
            table: None,
        };
        if let Some(table) = table.as_mut() {
            if let Some(mutex) = table_mutex.as_mut() {
                mutex.lock();
            }
            let (allocation, live) = allocate_index(platform, table, is_server);
            relation.index = allocation.index;
            relation.peer_waiting = allocation.peer_waiting;
            relation.live_mutex = live;
            if let Some(index) = allocation.index {
                relation.primary = Messenger::new(platform, &format!("SC{index}"), is_server);
                relation.secondary = browser_messenger(platform, index, is_server);
            }
            if let Some(mutex) = table_mutex.as_mut() {
                mutex.unlock();
            }
        }
        relation.table = table;
        relation.table_mutex = table_mutex;
        relation
    }

    /// The claimed relation index.
    pub fn index(&self) -> Option<u32> {
        self.index
    }

    /// The peer role had already claimed the index when it was allocated.
    pub fn peer_waiting(&self) -> bool {
        self.peer_waiting
    }

    /// Primary Messenger (game client, `SC<k>`).
    pub fn primary(&self) -> &Messenger {
        &self.primary
    }

    /// Primary Messenger, mutable.
    pub fn primary_mut(&mut self) -> &mut Messenger {
        &mut self.primary
    }

    /// Secondary (browser) Messenger (`BS<k>` / `BC<k>`).
    pub fn secondary(&self) -> &Messenger {
        &self.secondary
    }

    /// Secondary Messenger, mutable.
    pub fn secondary_mut(&mut self) -> &mut Messenger {
        &mut self.secondary
    }
}

impl Drop for Relation {
    /// `0x140071550`: destroy the Messengers (secondary first), then clear the own bit under the table mutex.
    fn drop(&mut self) {
        drop(std::mem::replace(&mut self.secondary, Messenger::inert()));
        drop(std::mem::replace(&mut self.primary, Messenger::inert()));
        let (Some(index), Some(table)) = (self.index, self.table.as_mut()) else { return };
        if let Some(mutex) = self.table_mutex.as_mut() {
            mutex.lock();
        }
        let own_offset = if self.is_server { 0 } else { 4 };
        if let Some(own) = read_mask(table, own_offset) {
            write_mask(table, own_offset, own & !(1 << index));
        }
        if let Some(mutex) = self.table_mutex.as_mut() {
            mutex.unlock();
        }
    }
}

/// `makeMessengerForBrowser(index, isServer, true)` (`0x140079070`): `BS<k>` for the daemon, `BC<k>` for the
/// game; the Messenger is always the pipe server. An index of 32 or more yields an inert Messenger.
fn browser_messenger(platform: &dyn IpcPlatform, index: u32, is_server: bool) -> Messenger {
    if index >= RELATION_COUNT {
        tracing::error!(index, "relation index is out of range");
        return Messenger::inert();
    }
    let prefix = if is_server { "BS" } else { "BC" };
    Messenger::new(platform, &format!("{prefix}{index}"), true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::messenger::State;
    use backend::mock::ipc::LoopbackPlatform;

    /// Keeps the table section alive and gives raw access to the two masks.
    struct Table(SharedMemory);

    impl Table {
        fn attach(platform: &LoopbackPlatform) -> Self {
            Self(SharedMemory::create(platform, TABLE_LEN, TABLE_NAME, false).unwrap())
        }

        fn masks(&self) -> (u32, u32) {
            (read_mask(&self.0, 0).unwrap(), read_mask(&self.0, 4).unwrap())
        }

        fn set(&mut self, server: u32, client: u32) {
            write_mask(&mut self.0, 0, server);
            write_mask(&mut self.0, 4, client);
        }
    }

    #[test]
    fn servers_claim_consecutive_indices_and_release_them_when_dropped() {
        let platform = LoopbackPlatform::new();
        let table = Table::attach(&platform);
        let first = Relation::new(&platform, true);
        let second = Relation::new(&platform, true);
        assert_eq!((first.index(), second.index()), (Some(0), Some(1)));
        assert!(!first.peer_waiting());
        assert_eq!(table.masks(), (0b11, 0));
        assert_eq!((first.primary().state(), first.secondary().state()), (State::Disconnected, State::Disconnected));

        drop(first);
        assert_eq!(table.masks(), (0b10, 0), "only the released bit is cleared");
        let mut live = MutexHolder::open(&platform, Some("daemon_live_201223_server0")).unwrap();
        assert!(live.try_lock(), "the liveness mutex went away with the relation");
        live.unlock();
        drop(live);
        let reused = Relation::new(&platform, true);
        assert_eq!(reused.index(), Some(0), "lowest free index is reused");
        assert_eq!(table.masks(), (0b11, 0));
    }

    #[test]
    fn a_claim_whose_liveness_mutex_is_free_is_stale_and_reclaimed() {
        let platform = LoopbackPlatform::new();
        let mut table = Table::attach(&platform);
        table.set(0b101, 0); // two crashed daemons
        let relation = Relation::new(&platform, true);
        assert_eq!(relation.index(), Some(0));
        assert_eq!(table.masks(), (0b001, 0), "stale bit 2 was reclaimed during the scan, bit 0 re-claimed");
    }

    #[test]
    fn a_claim_held_by_a_live_owner_is_skipped() {
        let platform = LoopbackPlatform::new();
        let mut table = Table::attach(&platform);
        table.set(0b1, 0);
        let mut owner = MutexHolder::open(&platform, Some("daemon_live_201223_server0")).unwrap();
        assert!(owner.lock());
        let relation = Relation::new(&platform, true);
        assert_eq!(relation.index(), Some(1));
        assert_eq!(table.masks(), (0b11, 0));
    }

    #[test]
    fn an_index_claimed_by_a_waiting_peer_is_preferred_over_a_lower_free_one() {
        let platform = LoopbackPlatform::new();
        let mut table = Table::attach(&platform);
        table.set(0, 1 << 5);
        let relation = Relation::new(&platform, true);
        assert_eq!(relation.index(), Some(5));
        assert!(relation.peer_waiting());
        assert_eq!(table.masks(), (1 << 5, 1 << 5));
    }

    #[test]
    fn the_client_role_uses_the_swapped_masks_and_its_own_mutex_names() {
        let platform = LoopbackPlatform::new();
        let mut table = Table::attach(&platform);
        table.set(1 << 3, 0);
        let relation = Relation::new(&platform, false);
        assert_eq!(relation.index(), Some(3));
        assert!(relation.peer_waiting());
        assert_eq!(table.masks(), (1 << 3, 1 << 3));
        let mut holder = MutexHolder::open(&platform, Some("daemon_live_201223_client3")).unwrap();
        assert!(!holder.try_lock(), "held by the relation");
        drop(relation);
        assert_eq!(table.masks(), (1 << 3, 0));
    }
}
