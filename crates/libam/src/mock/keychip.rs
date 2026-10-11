//! The simulated keychip: the registry store, the IOCTL replies of a healthy keychip and the faults a test can
//! inject (`spec/libam/platform.md` part B 5).
//!
//! Every handle ([`SimKeychip::duplicate`](KeychipDriver::duplicate), [`Clone`]) is another open of the same device:
//! the registry store, the counters, the trace log and the NVRAM are shared, each handle has its own request
//! pipeline. The device answers what libam sends: the availability check, the NVRAM geometry and access, the
//! billing counters and their signed updates, the CA certificate and public key requests, the trace log
//! (read, write, logical erase, sector erase, restore) and the Auth 2.0 start and packet requests.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::platform::keychip::{TRACE_CAPACITY, ioctl, last_error};
use backend::keychip::{Failure, KeychipDriver, OpenError, Poll, Property, PropertyError, Request, SubmitError};

/// One trace record.
type Record = [u8; 60];

/// DER of the CA certificate the simulated keychip holds: a placeholder the simulated TLS never verifies.
const CA_CERTIFICATE: [u8; 5] = [0x30, 0x03, 0x02, 0x01, 0x01];

/// What every handle of the simulated device shares.
#[derive(Debug)]
struct Device {
    properties: HashMap<String, Property>,
    busy: Vec<String>,
    open_error: bool,
    /// Handles that are open.
    open: u32,
    available: bool,
    /// `poll` answers `Pending` this many times before a request completes.
    latency: u32,
    requests: Vec<u32>,
    play_count: u32,
    playlimit: u32,
    /// The near-full threshold, the low half word of the billing-mode reply.
    nearfull: u16,
    billing_mode: bool,
    nvram: Vec<Vec<u8>>,
    trace: Vec<Record>,
    restores: u32,
    fail_codes: HashMap<u32, u32>,
}

/// The request pipeline of one handle.
#[derive(Debug, Default)]
struct Pipeline {
    opened: bool,
    waited: u32,
    in_flight: Option<Request>,
}

/// The simulated keychip handle.
#[derive(Debug)]
pub struct SimKeychip {
    device: Rc<RefCell<Device>>,
    pipeline: Pipeline,
}

impl Clone for SimKeychip {
    /// Another handle on the same device, not open yet.
    fn clone(&self) -> Self {
        Self { device: Rc::clone(&self.device), pipeline: Pipeline::default() }
    }
}

impl Default for SimKeychip {
    fn default() -> Self {
        Self::new()
    }
}

impl SimKeychip {
    /// The default cabinet keychip: game id `SDAA`, keychip id `A63E-01E12345678`, model 1, region 1 (raw), system
    /// flags allowing ALL.Net, accounting and the title router, two NVRAM areas of 4096 bytes, play count 0, play
    /// limit 1000, near-full threshold 10 and an empty trace log.
    pub fn new() -> Self {
        let mut properties = HashMap::new();
        properties.insert("gameId".to_owned(), Property::Binary(b"SDAA".to_vec()));
        properties.insert("keychipId".to_owned(), Property::Binary(b"A63E-01E12345678".to_vec()));
        properties.insert("modelType".to_owned(), Property::Dword(1));
        properties.insert("region".to_owned(), Property::Dword(1));
        properties.insert("systemFlag".to_owned(), Property::Dword(0x34));
        properties.insert("serverIpIpv4".to_owned(), Property::Binary(vec![192, 168, 0, 1]));
        Self {
            device: Rc::new(RefCell::new(Device {
                properties,
                busy: Vec::new(),
                open_error: false,
                open: 0,
                available: true,
                latency: 0,
                requests: Vec::new(),
                play_count: 0,
                playlimit: 1000,
                nearfull: 10,
                billing_mode: false,
                nvram: vec![vec![0; 4096]; 2],
                trace: Vec::new(),
                restores: 0,
                fail_codes: HashMap::new(),
            })),
            pipeline: Pipeline::default(),
        }
    }

    /// Sets (or replaces) a registry value of the keychip store.
    pub fn set_property(&self, name: &str, value: Property) {
        self.device.borrow_mut().properties.insert(name.to_owned(), value);
    }

    /// Removes a registry value.
    pub fn remove_property(&self, name: &str) {
        self.device.borrow_mut().properties.remove(name);
    }

    /// Makes reads of `name` answer "busy" (the guard mutex is held) until [`Self::release_property`].
    pub fn hold_property(&self, name: &str) {
        self.device.borrow_mut().busy.push(name.to_owned());
    }

    /// Releases the held value.
    pub fn release_property(&self, name: &str) {
        self.device.borrow_mut().busy.retain(|held| held != name);
    }

    /// Makes the device refuse to open.
    pub fn set_open_error(&self, fail: bool) {
        self.device.borrow_mut().open_error = fail;
    }

    /// Whether the keychip answers the availability check (`ERROR_NOT_READY` otherwise).
    pub fn set_available(&self, available: bool) {
        self.device.borrow_mut().available = available;
    }

    /// Requests complete after this many pending polls.
    pub fn set_latency(&self, polls: u32) {
        self.device.borrow_mut().latency = polls;
    }

    /// Makes the IOCTL `code` fail with `last_error`.
    pub fn fail_ioctl(&self, code: u32, last_error: u32) {
        self.device.borrow_mut().fail_codes.insert(code, last_error);
    }

    /// The IOCTL codes submitted so far, by every handle.
    pub fn requests(&self) -> Vec<u32> {
        self.device.borrow().requests.clone()
    }

    /// Sets the play count the keychip reports.
    pub fn set_play_count(&self, count: u32) {
        self.device.borrow_mut().play_count = count;
    }

    /// The play count in the keychip.
    pub fn play_count(&self) -> u32 {
        self.device.borrow().play_count
    }

    /// Sets the play limit the keychip reports.
    pub fn set_playlimit(&self, limit: u32) {
        self.device.borrow_mut().playlimit = limit;
    }

    /// Sets the near-full threshold the keychip reports.
    pub fn set_nearfull(&self, threshold: u16) {
        self.device.borrow_mut().nearfull = threshold;
    }

    /// Sets the billing type the keychip reports (the mode bit of the billing-mode reply).
    pub fn set_billing_mode(&self, billing: bool) {
        self.device.borrow_mut().billing_mode = billing;
    }

    /// The trace log in the keychip, oldest first.
    pub fn trace_records(&self) -> Vec<[u8; 60]> {
        self.device.borrow().trace.clone()
    }

    /// How many times the trace-log restore ran (`amyDongleBillingTdRestore`).
    pub fn restore_count(&self) -> u32 {
        self.device.borrow().restores
    }

    /// Whether any handle of the device is open.
    pub fn is_open(&self) -> bool {
        self.device.borrow().open > 0
    }
}

fn le32(bytes: &[u8], at: usize) -> u32 {
    bytes.get(at..at + 4).and_then(|chunk| <[u8; 4]>::try_from(chunk).ok()).map_or(0, u32::from_le_bytes)
}

fn word(value: u32) -> Result<Vec<u8>, Failure> {
    Ok(value.to_le_bytes().to_vec())
}

/// A reply of the Auth 2.0 requests: `{u16 length; payload}`.
fn auth_reply(payload: &[u8]) -> Vec<u8> {
    let mut reply = u16::try_from(payload.len()).unwrap_or(u16::MAX).to_le_bytes().to_vec();
    reply.extend_from_slice(payload);
    reply
}

impl Device {
    fn reply(&mut self, request: &Request) -> Result<Vec<u8>, Failure> {
        if let Some(&last_error) = self.fail_codes.get(&request.code) {
            return Err(Failure { last_error });
        }
        match request.code {
            ioctl::IS_AVAILABLE if self.available => Ok(Vec::new()),
            ioctl::IS_AVAILABLE => Err(Failure { last_error: last_error::NOT_READY }),
            ioctl::GEOMETRY_NVRAM => {
                let size = u32::try_from(self.nvram.first().map_or(0, Vec::len)).unwrap_or(u32::MAX);
                let mut reply = u32::try_from(self.nvram.len()).unwrap_or(u32::MAX).to_le_bytes().to_vec();
                reply.extend_from_slice(&size.to_le_bytes());
                Ok(reply)
            }
            // The billing-mode reply: the mode is bit 0 of the upper half word, the rest is the nearfull value.
            ioctl::BILLING_MODE => word(u32::from(self.nearfull) | u32::from(self.billing_mode) << 16),
            ioctl::GET_PLAY_COUNT => word(self.play_count),
            ioctl::ADD_PLAY_COUNT => {
                self.play_count = self.play_count.saturating_add(1);
                word(self.play_count)
            }
            ioctl::GET_PLAYLIMIT => word(self.playlimit),
            ioctl::UPDATE_PLAYLIMIT => {
                self.playlimit = le32(&request.input, 0);
                Ok(Vec::new())
            }
            // The near-full value of the server: bit 16 is the billing type, bits 0..15 the threshold.
            ioctl::UPDATE_NEARFULL => {
                let value = le32(&request.input, 0);
                self.nearfull = u16::try_from(value & 0xFFFF).unwrap_or_default();
                self.billing_mode = (value >> 16) & 1 != 0;
                Ok(Vec::new())
            }
            ioctl::GET_CA_CERTIFICATION => Ok(CA_CERTIFICATE.to_vec()),
            ioctl::GET_SIGNATURE_PUB_KEY => Ok(Vec::new()),
            ioctl::READ_NVRAM => {
                let (index, address, length) =
                    (usize::from(request.input[0]), le32(&request.input, 4) as usize, le32(&request.input, 8) as usize);
                let area = self.nvram.get(index).ok_or(Failure { last_error: 1 })?;
                area.get(address..address + length).map(<[u8]>::to_vec).ok_or(Failure { last_error: 1 })
            }
            ioctl::WRITE_NVRAM => {
                let (index, length) = (usize::from(request.input[0]), le32(&request.input, 8) as usize);
                let area = self.nvram.get_mut(index).ok_or(Failure { last_error: 1 })?;
                let data = request.input.get(15..15 + length).ok_or(Failure { last_error: 1 })?;
                area.get_mut(..length).ok_or(Failure { last_error: 1 })?.copy_from_slice(data);
                Ok(Vec::new())
            }
            ioctl::PUT_TRACE_DATA => {
                let record = Record::try_from(request.input.as_slice()).map_err(|_| Failure { last_error: 87 })?;
                if self.trace.len() >= TRACE_CAPACITY {
                    return Err(Failure { last_error: last_error::NO_TRACE });
                }
                self.trace.push(record);
                Ok(Vec::new())
            }
            ioctl::GET_TRACE_DATA => {
                let (first, count) = (le32(&request.input, 0) as usize, le32(&request.input, 4) as usize);
                if first >= self.trace.len() {
                    return Err(Failure { last_error: last_error::NO_TRACE });
                }
                Ok(self.trace.iter().skip(first).take(count).flatten().copied().collect())
            }
            // The logical erase takes the number of oldest records to drop.
            ioctl::TD_LOGICAL_ERASE => {
                let count = (le32(&request.input, 0) as usize).min(self.trace.len());
                self.trace.drain(..count);
                Ok(Vec::new())
            }
            ioctl::TD_ERASE_ALL => {
                self.trace.clear();
                Ok(Vec::new())
            }
            ioctl::TD_RESTORE => {
                self.restores = self.restores.saturating_add(1);
                Ok(Vec::new())
            }
            ioctl::TD_GET_FREE_LOG_NUM => {
                word(u32::try_from(TRACE_CAPACITY.saturating_sub(self.trace.len())).unwrap_or(0))
            }
            ioctl::TD_GET_LOG_NUM => {
                let mut reply = u32::try_from(self.trace.len()).unwrap_or(u32::MAX).to_le_bytes().to_vec();
                reply.extend_from_slice(&0u32.to_le_bytes());
                Ok(reply)
            }
            ioctl::ENCRYPT | ioctl::DECRYPT => Ok(request.input.iter().map(|byte| !byte).collect()),
            ioctl::AUTH_START => Ok(auth_reply(b"start")),
            ioctl::AUTH_PACKET => {
                let length = usize::from(u16::from_le_bytes([request.input[0], request.input[1]]));
                Ok(auth_reply(request.input.get(2..2 + length).unwrap_or_default()))
            }
            _ => Ok(Vec::new()),
        }
    }
}

impl KeychipDriver for SimKeychip {
    fn open(&mut self) -> Result<(), OpenError> {
        let mut device = self.device.borrow_mut();
        if device.open_error {
            return Err(OpenError);
        }
        if !self.pipeline.opened {
            self.pipeline.opened = true;
            device.open += 1;
        }
        Ok(())
    }

    fn close(&mut self) {
        if self.pipeline.opened {
            self.device.borrow_mut().open -= 1;
        }
        self.pipeline = Pipeline::default();
    }

    fn submit(&mut self, request: Request) -> Result<(), SubmitError> {
        if !self.pipeline.opened {
            return Err(SubmitError::Failed(6));
        }
        self.device.borrow_mut().requests.push(request.code);
        self.pipeline.waited = 0;
        self.pipeline.in_flight = Some(request);
        Ok(())
    }

    fn poll(&mut self) -> Poll {
        let mut device = self.device.borrow_mut();
        if self.pipeline.waited < device.latency {
            self.pipeline.waited += 1;
            return Poll::Pending;
        }
        let Some(request) = self.pipeline.in_flight.take() else {
            return Poll::Done(Err(Failure { last_error: 6 }));
        };
        let mut reply = device.reply(&request);
        if let Ok(bytes) = &mut reply {
            bytes.truncate(request.output_len);
        }
        Poll::Done(reply)
    }

    fn property(&mut self, name: &str) -> Result<Property, PropertyError> {
        let device = self.device.borrow();
        if device.busy.iter().any(|held| held == name) {
            return Err(PropertyError::Busy);
        }
        device.properties.get(name).cloned().ok_or(PropertyError::Failed)
    }

    fn duplicate(&self) -> Box<dyn KeychipDriver> {
        Box::new(self.clone())
    }
}

impl Drop for SimKeychip {
    fn drop(&mut self) {
        self.close();
    }
}
