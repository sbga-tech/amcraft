//! Port handle of the can module (`can.md` 2.3): the facade over a manager with zero ports.

use libam_api::can::{CanEvent, CanPacket, CanPort, CanTargetInfo};
use libam_api::result::{AmError, AmResult};

use crate::shared::Shared;

/// `CanPort` handle over the shared libam state.
#[derive(Debug)]
pub struct CanHandle(pub(crate) Shared);

/// The port count (`manager+0xB78`): written only by the constructor, so 0 for the life of the process.
const PORT_COUNT: usize = 0;

impl CanPort for CanHandle {
    /// `0x140218850`: a token while libam is initialised, else result code 2.
    fn can_facade(&self) -> AmResult<()> {
        self.0.guard()
    }

    /// `0x140216080`.
    fn port_count(&self) -> usize {
        PORT_COUNT
    }

    /// `0x1402160A0`: `port < portCount ? backend.targetCount : 0`.
    fn target_count(&self, _port: usize) -> usize {
        0
    }

    /// `0x1402160D0`: `port >= portCount` gives result code 1.
    fn target(&self, _port: usize, _index: usize) -> AmResult<CanTargetInfo> {
        Err(AmError::code(AmError::INVALID_ARGUMENT))
    }

    /// `0x140215F40`: needs `portCount > port`, which never holds, so the backend is never reached and the
    /// result is code 1 (whatever the priority).
    fn send(&mut self, _port: usize, _target_index: usize, _priority: u8, _packet: &CanPacket) -> AmResult<()> {
        Err(AmError::code(AmError::INVALID_ARGUMENT))
    }

    /// No send-complete or received-packet event is ever dispatched (`can.md` 2.4).
    fn take_events(&mut self) -> Vec<CanEvent> {
        Vec::new()
    }
}
