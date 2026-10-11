//! Network blocks: `ServerData+0x38D0` (0x470) and `ClientData+0x1808` (0x8).
//!
//! Evidence: `spec/controllers/network_install.md` §2.2 (`ipc::network::*`), claims `network_install.network.*`.

use num_enum::TryFromPrimitive;
use zero_schema::zero;

use crate::schema::shared::{ErrorData, Response};
use crate::schema::{client_block, server_block};

/// Published state of one test item (libam `TestStatus` state, copied by `0x14003E6B0`).
#[zero]
#[derive(Debug, Clone, Copy, PartialEq, Eq, TryFromPrimitive)]
#[repr(u32)]
pub enum NetworkTestState {
    /// Not started.
    Idle = 0,
    /// Checker running.
    Running = 1,
    /// Checker finished.
    Done = 2,
}

/// Published result of one test item (libam `TestStatus` result: `N/A`, `GOOD`, `BAD`).
#[zero]
#[derive(Debug, Clone, Copy, PartialEq, Eq, TryFromPrimitive)]
#[repr(u32)]
pub enum NetworkTestResult {
    /// No result yet (libam text `CHECK` while running).
    None = 0,
    /// Not applicable (`"N/A"`, `0x1406A1700`).
    NotApplicable = 1,
    /// Passed (`"GOOD"`, `0x1406A16E8`).
    Good = 2,
    /// Failed (`"BAD"`, `0x1406A16F8`).
    Bad = 3,
}

/// Number of items in a [`ServerTest`]: nine libam checkers plus the ALL.Net Air slot.
pub const TEST_ITEM_COUNT: usize = 10;

/// `ipc::network::ServerTestItemInfo` (0xC): one checker of the network test.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerTestItem {
    /// libam `isItemEnabled(i)` sampled at the ready edge (item 9: config `allnet_air.enable`).
    pub available: bool,
    /// Test state.
    pub state: NetworkTestState,
    /// Test result.
    pub result: NetworkTestResult,
}

/// `ipc::network::ServerTestData` (0x220) at `network+0x28` (power-on test) and `network+0x248` (test).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerTest<'b> {
    /// 1 from the item-0 start event until the final event [I name].
    pub running: bool,
    /// 1 after the final event, 0 at test start [I name].
    pub completed: bool,
    /// Per-item state, stride `0xC`.
    pub items: [ServerTestItem; TEST_ITEM_COUNT],
    /// Hop count parsed from the HOPS item text.
    pub hops: u32,
    /// Error of the most recent Good/Bad event.
    pub error: ErrorData<'b>,
}

/// `ipc::network::ServerPropertyData` (0x18): the active network property (`0x14003E750`).
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerProperty {
    /// Constant 1 once captured; never cleared.
    pub valid: bool,
    /// libam property byte `+0x00` [I name].
    pub dhcp_enabled: bool,
    /// Raw libam address word [I name].
    pub address: u32,
    /// Raw libam word [I name].
    pub subnet_mask: u32,
    /// Raw libam word [I name].
    pub gateway: u32,
    /// Raw libam word [I name].
    pub primary_dns: u32,
    /// Raw libam word [I name].
    pub secondary_dns: u32,
}

/// `ServerData.network`: availability, the shared response, the active property and the two test blocks.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ServerNetwork<'b> {
    /// libam network manager "enabled" flag, sampled at the ready edge.
    pub available: bool,
    /// `ipc::network::ServerResponse` (generation, succeeded).
    pub response: Response,
    /// Active network property.
    pub property: ServerProperty,
    /// Events with `isPowerOn != 0`.
    pub power_on_test: ServerTest<'b>,
    /// Events with `isPowerOn == 0`.
    pub test: ServerTest<'b>,
    /// `isLanAvailable` snapshot (item 0 Done).
    pub lan_available: bool,
    /// `isLocationLanAvailable` snapshot (item 3 Done).
    pub location_lan_available: bool,
    /// `isWanAvailable` snapshot (item 5 Done).
    pub wan_available: bool,
}

/// `ClientData.network`: `ipc::network::ClientRequest`.
#[zero(endian = "little")]
#[derive(Debug)]
pub struct ClientNetwork {
    /// Request generation (`taskNo`).
    pub generation: u32,
    /// Raw [`NetworkTask`]; only `TestStart` is accepted.
    pub task: u32,
}

server_block!(network, network_mut: ServerNetwork, ServerNetworkRef, ServerNetworkMut @ 0x38D0);
client_block!(network, network_mut: ClientNetwork, ClientNetworkRef, ClientNetworkMut @ 0x1808);

/// `ipc::network::Task` (name table `0x1405EEEE8`): only `TestStart` exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, TryFromPrimitive)]
#[repr(u32)]
pub enum NetworkTask {
    /// Start the network test (`"TestStart"`, `0x1405EED40`).
    TestStart = 0,
}

impl ClientNetworkRef<'_> {
    /// The request's task, `None` for an unknown value (the request fails).
    pub fn task_kind(&self) -> Option<NetworkTask> {
        NetworkTask::try_from(self.task()).ok()
    }
}

const _: () = {
    assert!(ServerTestItem::SCHEMA_SIZE == 0xC);
    assert!(ServerTest::SCHEMA_SIZE == 0x220);
    assert!(ServerProperty::SCHEMA_SIZE == 0x18);
    assert!(ServerNetwork::SCHEMA_SIZE == 0x470);
    assert!(ClientNetwork::SCHEMA_SIZE == 0x8);
};
