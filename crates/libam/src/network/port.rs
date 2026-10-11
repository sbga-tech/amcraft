//! The network facade (`am::network`): network properties and network tests. [`NetworkPort`] is the facade
//! amdaemon's `NetworkController` talks to; the network test reports through [`NetworkPort::take_test_events`].
//! libam's implementation is [`NetworkHandle`].

use super::networktest::PeerView;
use crate::core::cx;
use crate::error::FatalError;
use crate::result::AmResult;
use crate::sequence::ProcessState;
use crate::shared::Shared;

/// `am::network::TestItem`: checkers in libam construction order (`0x140247EA0`) plus the "all finished" marker.
///
/// libam only emits the values `0..=9` (the `ItemCheckerBase` helpers and the tester's final event), so the
/// enumeration is closed. Item names are the libam checker class names (`am::networktest::*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum TestItem {
    /// `IpAddressChecker`; its Done event captures the property and `lanAvailable`.
    IpAddress = 0,
    /// `GatewayChecker`.
    Gateway = 1,
    /// `DnsLanChecker`.
    DnsLan = 2,
    /// `HopsChecker`; its Done/Good text carries the hop count and Done refreshes `locationLanAvailable`.
    Hops = 3,
    /// `LineTypeChecker`.
    LineType = 4,
    /// `AllnetAuthChecker`; its Done event refreshes `wanAvailable`.
    AllnetAuth = 5,
    /// `AimeChecker`.
    Aime = 6,
    /// `EmoneyChecker`.
    Emoney = 7,
    /// `AimePayChecker`.
    AimePay = 8,
    /// Pseudo item 9: the whole test finished (not a checker).
    AllFinished = 9,
}

impl TestItem {
    /// The nine libam checkers, in slot order.
    pub const CHECKERS: [Self; 9] = [
        Self::IpAddress,
        Self::Gateway,
        Self::DnsLan,
        Self::Hops,
        Self::LineType,
        Self::AllnetAuth,
        Self::Aime,
        Self::Emoney,
        Self::AimePay,
    ];

    /// `ServerTestData.items` slot of a checker (`0x14003E610`); `None` for [`Self::AllFinished`].
    pub const fn slot(self) -> Option<usize> {
        match self {
            Self::AllFinished => None,
            checker => Some(checker as usize),
        }
    }
}

/// `am::network::TestStatus` as handed to the listener: libam `state`/`result` numbers and the status text.
///
/// `state` is `0` idle, `1` running, `2` done; `result` is `0` none, `1` N/A, `2` good, `3` bad. Other values
/// are kept raw because the original ignores them when copying (§2.2.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TestStatus {
    /// libam state number.
    pub state: u32,
    /// libam result number.
    pub result: u32,
    /// libam text (`wchar_t[16]`), NUL-terminated or filling the array; only the HOPS item's text is
    /// interpreted (hop count).
    pub text: [u16; 16],
}

impl TestStatus {
    /// The text units before the first NUL (the whole array when unterminated).
    pub fn text_units(&self) -> &[u16] {
        let end = self.text.iter().position(|&unit| unit == 0).unwrap_or(self.text.len());
        &self.text[..end]
    }
}

/// One `TestEventListener::onTestEvent` call (`0x14003E1C0`): the power-on flag, the reporting item, its status
/// and the current error record.
#[derive(Debug, Clone)]
pub struct NetworkTestEvent {
    /// Power-on test (`powerOnTest` block) rather than an operator-started test (`test` block).
    pub is_power_on: bool,
    /// Reporting checker, or [`TestItem::AllFinished`].
    pub item: TestItem,
    /// Item status.
    pub status: TestStatus,
    /// libam error record current at the time of the event.
    pub error: FatalError,
}

/// libam network property record as read by `0x14003E750` (the active network manager's `mgr+0x698`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetworkProperty {
    /// Property byte `+0x00` (DHCP in use) [I name].
    pub dhcp: bool,
    /// Raw word `+0x04`; amdaemon does not transform byte order.
    pub address: u32,
    /// Raw word `+0x08`.
    pub subnet_mask: u32,
    /// Raw word `+0x0C`.
    pub gateway: u32,
    /// Raw word `+0x10`.
    pub primary_dns: u32,
    /// Raw word `+0x14`.
    pub secondary_dns: u32,
}

/// libam calls made by amdaemon's `NetworkController` and its test event listener.
pub trait NetworkPort {
    /// Network facade handle (`0x140218FD0` / `0x140219020`, `Mgr+8+18`).
    ///
    /// `Err` is the original's `Result != OK || token == NULL` ("libam singleton absent"); every caller
    /// reports the `isReady()` precondition exception for it.
    fn network_handle(&self) -> AmResult<()>;

    /// `network.available` source: the libam network manager "enabled" byte (`0x1402171A0`).
    fn is_enabled(&self) -> bool;

    /// libam tester `isItemEnabled` (`0x140217280` → `0x1402487D0`) for a checker item.
    fn is_item_enabled(&self, item: TestItem) -> bool;

    /// libam sequence process state (`0x140236100`, `libam+0x209B8+0x4A8`); `startTest` requires state 5.
    fn sequence_process_state(&self) -> ProcessState;

    /// Body of libam `startTest` after the sequence gate (`0x1402539F0`): resets both network managers and
    /// starts the tester. Events follow asynchronously through [`Self::take_test_events`].
    fn begin_test(&mut self);

    /// Active network manager property (`0x140217250`); `None` while libam has no usable property.
    fn property(&self) -> Option<NetworkProperty>;

    /// `isLanAvailable` of the active manager (`0x1402171C0`).
    fn is_lan_available(&self) -> bool;

    /// `isLocationLanAvailable` of the active manager (`0x1402171F0`).
    fn is_location_lan_available(&self) -> bool;

    /// `isWanAvailable` of the active manager (`0x140217220`).
    fn is_wan_available(&self) -> bool;

    /// Drains the `onTestEvent` calls libam made since the last call (they fire inside `Manager::execute`).
    fn take_test_events(&mut self) -> Vec<NetworkTestEvent>;
}

/// The part of `amdaemon::process::Config` the controller's config observer reads (`0x1400400C0`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NetworkProcessConfig {
    /// `allnet_air.enable` (`process::Config+0x4FF9E`): published as the Air pseudo item's `available`.
    /// A release configuration always has it `false` (the config loader rejects `true`).
    pub allnet_air_enable: bool,
}

/// The part of libam `am::AppConfig` the controller fills in (`0x140040090`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NetworkAppConfig {
    /// `AppConfig+0x2590` points at the controller's `NetworkEventListener`: libam delivers its test events
    /// to this controller (through [`NetworkPort::take_test_events`]).
    pub test_event_listener: bool,
}

/// `NetworkPort` handle over the shared libam state.
#[derive(Debug)]
pub struct NetworkHandle(pub(crate) Shared);

impl NetworkPort for NetworkHandle {
    fn network_handle(&self) -> AmResult<()> {
        self.0.guard()
    }

    fn is_enabled(&self) -> bool {
        self.0.read(|core| core.network.enabled())
    }

    fn is_item_enabled(&self, item: TestItem) -> bool {
        self.0.read(|core| core.networktest.is_item_enabled(item))
    }

    fn sequence_process_state(&self) -> ProcessState {
        self.0.read(|core| ProcessState(core.sequence.process_state()))
    }

    fn begin_test(&mut self) {
        self.0.with(|core| {
            let mut cx = cx!(core);
            core.network.retest(&mut cx);
            // `0x1402539F0`: slot 1 of the ALL.Net manager follows the plain manager's.
            core.allnet.restart_test(core.sequence.is_game_mode(), core.network.property_setter_running());
            let peers = PeerView::of(core);
            core.networktest.start(&core.network, &peers);
        });
    }

    /// `0x140217250`: the active manager's property (`ALL.Net` when `allnet_auth.enable` was adopted at Operating
    /// entry, the plain manager otherwise; network.md M.1 active-manager byte).
    fn property(&self) -> Option<NetworkProperty> {
        self.0.read(|core| {
            if core.config.allnet_auth.enable { core.allnet.network_property() } else { core.network.property() }
        })
    }

    /// `0x1402171C0` on the active manager.
    fn is_lan_available(&self) -> bool {
        self.0.read(|core| {
            if core.config.allnet_auth.enable {
                core.allnet.is_lan_available()
            } else {
                core.network.is_lan_available()
            }
        })
    }

    /// `0x1402171F0` on the active manager.
    fn is_location_lan_available(&self) -> bool {
        self.0.read(|core| {
            if core.config.allnet_auth.enable {
                core.allnet.is_location_lan_available()
            } else {
                core.network.is_location_lan_available()
            }
        })
    }

    /// `0x140217220` on the active manager: the plain manager has no WAN notion (constant `false`, network.md M.4
    /// slot 6); the ALL.Net manager reports a good authentication.
    fn is_wan_available(&self) -> bool {
        self.0.read(|core| core.config.allnet_auth.enable && core.allnet.is_wan_available())
    }

    fn take_test_events(&mut self) -> Vec<NetworkTestEvent> {
        self.0.with(|core| core.networktest.take_events())
    }
}
