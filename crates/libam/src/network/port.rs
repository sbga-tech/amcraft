//! Port handles of the network module.

use libam_api::network::{NetworkPort, NetworkProperty, NetworkTestEvent, TestItem};
use libam_api::result::AmResult;
use libam_api::sequence::ProcessState;

use super::networktest::PeerView;
use crate::core::cx;
use crate::shared::Shared;

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
