//! The Windows service control manager.
//!
//! `windows-service` (`ServiceManager::local_computer` with `SC_MANAGER_CONNECT`, `open_service` with
//! `SERVICE_USER_DEFINED_CONTROL`, `Service::notify`; `core.md` B.6). A code outside `128..=255` cannot be a
//! `UserEventCode` and fails with `ERROR_INVALID_PARAMETER` (the SCM would refuse it too); the status the crate
//! parses from a successful `ControlService` is not needed, so a state it cannot parse is not an error.

use windows_service::Error as ServiceApiError;
use windows_service::service::{ServiceAccess, UserEventCode};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, ERROR_SERVICE_DOES_NOT_EXIST};

use crate::{ServiceControl, ServiceError};

/// Sends user-defined control codes to services.
#[derive(Debug, Clone, Copy, Default)]
pub struct WindowsServices;

/// The Win32 error behind a `windows-service` failure.
fn service_error_code(error: &ServiceApiError) -> u32 {
    match error {
        ServiceApiError::Winapi(io) => io.raw_os_error().map_or(0, i32::cast_unsigned),
        // A name with an embedded NUL (never produced by the callers) is an invalid argument.
        _ => ERROR_INVALID_PARAMETER,
    }
}

impl ServiceControl for WindowsServices {
    fn control(&mut self, service: &str, code: u32) -> Result<(), ServiceError> {
        let failed = |error: &ServiceApiError| ServiceError::Failed(service_error_code(error));
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
            .map_err(|error| failed(&error))?;
        let service =
            manager.open_service(service, ServiceAccess::USER_DEFINED_CONTROL).map_err(
                |error| match service_error_code(&error) {
                    ERROR_SERVICE_DOES_NOT_EXIST => ServiceError::NotFound,
                    code => ServiceError::Failed(code),
                },
            )?;
        let code = UserEventCode::from_raw(code).map_err(|_| ServiceError::Failed(ERROR_INVALID_PARAMETER))?;
        match service.notify(code) {
            Ok(_) => Ok(()),
            // `ControlService` succeeded; only the crate's parse of the returned status failed.
            Err(ServiceApiError::ParseValue(..)) => Ok(()),
            Err(error) => Err(failed(&error)),
        }
    }
}
