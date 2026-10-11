//! Port handles of the allnet module.

use libam_api::allnet::{AirPort, AuthLocation, AuthPort, TimeSlot};
use libam_api::result::AmResult;

use crate::shared::Shared;

/// `AuthPort` handle over the shared libam state.
#[derive(Debug)]
pub struct AuthHandle(pub(crate) Shared);

/// `AirPort` handle.
#[derive(Debug)]
pub struct AirHandle;

impl AuthPort for AuthHandle {
    fn acquire_handle(&self) -> AmResult<()> {
        self.0.guard()
    }

    fn is_good(&self) -> bool {
        self.0.read(|core| core.allnet.is_good())
    }

    fn line_mask(&self) -> u32 {
        self.0.read(|core| if core.allnet.running() { core.allnet.auth().line_mask() } else { 0 })
    }

    fn location(&self) -> Option<AuthLocation> {
        self.0.read(|core| core.allnet.auth().location())
    }

    fn time(&self) -> Option<TimeSlot> {
        self.0.read(|core| core.allnet.auth().time())
    }

    fn text(&self) -> Option<[u8; 1536]> {
        self.0.read(|core| core.allnet.auth().text())
    }
}

impl AirPort for AirHandle {}
