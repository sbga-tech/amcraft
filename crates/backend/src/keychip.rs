//! The keychip device interface (`spec/libam/platform.md` part B 5): the kernel driver `\??\FddDriver` that answers
//! overlapped IOCTL requests, and the registry store `HKLM\System\Sega\SystemProperty\keychip`.
//!
//! The protocol above it (the request frame rules, the completion rules, the setup state machine, the health poll
//! and the billing and Auth 2.0 calls) is libam's own (`libam::platform::keychip::Link`) and runs the same way on
//! a simulated and on a real keychip.

use crate::Unsupported;

/// `ERROR_FILE_NOT_FOUND`: the driver device is not there.
const FILE_NOT_FOUND: u32 = 2;

/// The driver refused to open: `amyDongleOpen` status `0x81000008`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the keychip device cannot be opened")]
pub struct OpenError;

/// One overlapped `DeviceIoControl` request (the 64-byte request frame of 5.3, reduced to what the driver sees).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// The IOCTL code.
    pub code: u32,
    /// The input buffer.
    pub input: Vec<u8>,
    /// The capacity of the output buffer.
    pub output_len: usize,
}

/// A failed request: the `GetLastError` value the completion rules test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Failure {
    /// `GetLastError` after the failed `GetOverlappedResult`.
    pub last_error: u32,
}

/// Why `DeviceIoControl` did not accept the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SubmitError {
    /// `ERROR_INVALID_FUNCTION`: status `0x81000016`.
    #[error("the driver does not know this function")]
    InvalidFunction,
    /// Any other submit failure: status `0x81000002`.
    #[error("the request was rejected with code {0}")]
    Failed(u32),
}

/// State of the one request in flight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Poll {
    /// `WaitForSingleObject(event, 0)` timed out.
    Pending,
    /// The request finished: the reply bytes (as many as the driver returned) or the failure.
    Done(Result<Vec<u8>, Failure>),
}

/// A value of the keychip registry store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Property {
    /// `REG_BINARY`.
    Binary(Vec<u8>),
    /// `REG_DWORD`.
    Dword(u32),
}

/// Why a property read failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PropertyError {
    /// The guard mutex of the value is held by someone else: status `0x01000001`, retried at the next poll.
    #[error("the property is being accessed by someone else")]
    Busy,
    /// Missing key or value, or any registry failure.
    #[error("the property cannot be read")]
    Failed,
}

/// The keychip as the operating system presents it: one handle on the device, with one request in flight at a time.
pub trait KeychipDriver {
    /// `amyDongleOpen`: `CreateFileW(L"\??\FddDriver", GENERIC_READ | GENERIC_WRITE, FILE_SHARE_READ |
    /// FILE_SHARE_WRITE, OPEN_EXISTING, FILE_FLAG_OVERLAPPED)`.
    fn open(&mut self) -> Result<(), OpenError>;

    /// Closes the device handle.
    fn close(&mut self);

    /// Starts `request`; only one request is in flight at a time.
    fn submit(&mut self, request: Request) -> Result<(), SubmitError>;

    /// Polls the request in flight without blocking.
    fn poll(&mut self) -> Poll;

    /// Reads value `name` of `HKLM\System\Sega\SystemProperty\keychip` (open only, never created), under the
    /// value's named mutex, one attempt.
    fn property(&mut self, name: &str) -> Result<Property, PropertyError>;

    /// A second handle on the same device and store, not open yet and with a request pipeline of its own: the
    /// accounting reporter drives its own request layer next to the dongle manager's (`allnet.md` part 10), and a
    /// request on one handle must not cancel the request in flight on the other.
    fn duplicate(&self) -> Box<dyn KeychipDriver>;
}

/// No keychip driver: opening fails, and so does the registry store.
impl KeychipDriver for Unsupported {
    fn open(&mut self) -> Result<(), OpenError> {
        Err(OpenError)
    }

    fn close(&mut self) {}

    fn submit(&mut self, _request: Request) -> Result<(), SubmitError> {
        Err(SubmitError::Failed(FILE_NOT_FOUND))
    }

    fn poll(&mut self) -> Poll {
        Poll::Done(Err(Failure { last_error: 0 }))
    }

    fn property(&mut self, _name: &str) -> Result<Property, PropertyError> {
        Err(PropertyError::Failed)
    }

    fn duplicate(&self) -> Box<dyn KeychipDriver> {
        Box::new(Self)
    }
}
