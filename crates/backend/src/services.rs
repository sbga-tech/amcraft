//! The service-control interface: user-defined control codes sent to Windows services.

use crate::Unsupported;

/// Why a service control request failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ServiceError {
    /// The service does not exist (Win32 1060).
    #[error("service not found")]
    NotFound,
    /// Any other failure, with the Win32 error code.
    #[error("service control failed with code {0}")]
    Failed(u32),
}

/// Sends user-defined control codes to services (the USB reconnect path, `core.md` B.6).
pub trait ServiceControl {
    /// `OpenSCManagerW` + `OpenServiceW(SERVICE_USER_DEFINED_CONTROL)` + `ControlService(code)`.
    fn control(&mut self, service: &str, code: u32) -> Result<(), ServiceError>;
}

/// No service control manager: no service exists.
impl ServiceControl for Unsupported {
    fn control(&mut self, _service: &str, _code: u32) -> Result<(), ServiceError> {
        Err(ServiceError::NotFound)
    }
}
