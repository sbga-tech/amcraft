//! The keychip device protocol (`spec/libam/platform.md` part B 5, the `amDongle*` C layer).
//!
//! Two OS interfaces carry the keychip: the kernel device `\??\FddDriver` (overlapped IOCTLs) and the registry
//! store `HKLM\System\Sega\SystemProperty\keychip` (game id, keychip id, model, region, system flag, server
//! address). Both sit behind the backend interface [`KeychipDriver`]. Everything above it, the request frame rules,
//! the completion rules, the setup state machine, the health poll and the API checks, is in [`Link`] and runs the
//! same way on a simulated and on a real keychip.

use backend::keychip::{Failure, KeychipDriver, OpenError, Poll, Property, PropertyError, Request};

/// `CTL_CODE(FILE_DEVICE_UNKNOWN 0x22, function, METHOD_BUFFERED, access)`: `FILE_WRITE_ACCESS` (2) for the
/// functions `0x843..=0x845`, read and write (3) for every other one.
const fn ctl_code(function: u32) -> u32 {
    let access = if matches!(function, 0x843..=0x845) { 2 } else { 3 };
    (0x22 << 16) | (access << 14) | (function << 2)
}

/// The IOCTL codes of the table in part B 5.3.
pub mod ioctl {
    use super::ctl_code;

    /// `amyDongleIsAvailable`.
    pub const IS_AVAILABLE: u32 = ctl_code(0x845);
    /// `amyDongleGetGeometryNvram`.
    pub const GEOMETRY_NVRAM: u32 = ctl_code(0x893);
    /// `amyDongleGetBillingMode` and `amDongleBillingGetNearfull`.
    pub const BILLING_MODE: u32 = ctl_code(0x883);
    /// `amyDongleReadNvram`.
    pub const READ_NVRAM: u32 = ctl_code(0x891);
    /// `amyDongleWriteNvram`.
    pub const WRITE_NVRAM: u32 = ctl_code(0x892);
    /// `amyDongleSetIv`.
    pub const SET_IV: u32 = ctl_code(0x856);
    /// `amyDongleEncrypt`.
    pub const ENCRYPT: u32 = ctl_code(0x857);
    /// `amyDongleDecrypt`.
    pub const DECRYPT: u32 = ctl_code(0x858);
    /// `amyDongleBillingGetPlayCount`.
    pub const GET_PLAY_COUNT: u32 = ctl_code(0x854);
    /// `amyDongleBillingAddPlayCount`.
    pub const ADD_PLAY_COUNT: u32 = ctl_code(0x855);
    /// `amyDongleBillingGetPlaylimit`.
    pub const GET_PLAYLIMIT: u32 = ctl_code(0x881);
    /// `amyDongleBillingUpdatePlaylimit`.
    pub const UPDATE_PLAYLIMIT: u32 = ctl_code(0x882);
    /// `amyDongleBillingUpdateNearfull`.
    pub const UPDATE_NEARFULL: u32 = ctl_code(0x884);
    /// `amyDongleBillingGetCaCertification`.
    pub const GET_CA_CERTIFICATION: u32 = ctl_code(0x871);
    /// `amyDongleBillingGetSignaturePubKey`.
    pub const GET_SIGNATURE_PUB_KEY: u32 = ctl_code(0x872);
    /// `amyDongleBillingGetTraceData`.
    pub const GET_TRACE_DATA: u32 = ctl_code(0x865);
    /// `amyDongleBillingPutTraceData`.
    pub const PUT_TRACE_DATA: u32 = ctl_code(0x864);
    /// `amyDongleBillingTdRestore`.
    pub const TD_RESTORE: u32 = ctl_code(0x861);
    /// `amyDongleBillingTdLogicalErase`.
    pub const TD_LOGICAL_ERASE: u32 = ctl_code(0x862);
    /// `amyDongleBillingTdEraseUsedSector`.
    pub const TD_ERASE_USED_SECTOR: u32 = ctl_code(0x863);
    /// `amyDongleBillingTdEraseAll`.
    pub const TD_ERASE_ALL: u32 = ctl_code(0x869);
    /// `amyDongleBillingTdGetFreeLogNum`.
    pub const TD_GET_FREE_LOG_NUM: u32 = ctl_code(0x867);
    /// `amyDongleBillingTdGetLogNum`.
    pub const TD_GET_LOG_NUM: u32 = ctl_code(0x866);
    /// `amyDongleAlAuthStart`.
    pub const AUTH_START: u32 = ctl_code(0x8B0);
    /// `amyDongleAlAuthPacket`.
    pub const AUTH_PACKET: u32 = ctl_code(0x8B2);
}

/// `GetLastError` values the completion rules look at.
pub mod last_error {
    /// `ERROR_NOT_READY`: the keychip is not available (still a successful availability check).
    pub const NOT_READY: u32 = 21;
    /// A billing reply failed signature verification.
    pub const BILLING_SIGNATURE: u32 = 23;
    /// `NTE_BAD_SIGNATURE` (`0x80090006`).
    pub const BAD_SIGNATURE: u32 = 0x8009_0006;
    /// No trace record at that index.
    pub const NO_TRACE: u32 = 1106;
}

/// What the `amDongle*` calls can answer besides success; [`KeychipError::code`] is the original's return value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum KeychipError {
    /// `-3`: the C layer is not initialised.
    #[error("the keychip layer is not initialised")]
    NotInitialised,
    /// `-4`: initialised twice.
    #[error("the keychip layer is already initialised")]
    AlreadyInitialised,
    /// `-1`: setup has not finished, or it failed.
    #[error("the keychip is not ready")]
    NotReady,
    /// `-12`: the health poll found the keychip gone.
    #[error("the keychip was lost")]
    Lost,
    /// `1`: an asynchronous request is pending, the call did nothing.
    #[error("a request is pending")]
    Busy,
    /// `-2`: an argument is out of range.
    #[error("invalid argument")]
    InvalidArgument,
    /// `-7`: the keychip rejected the signature.
    #[error("signature rejected")]
    Signature,
    /// `-8`: the trace record could not be written.
    #[error("trace record rejected")]
    TraceWrite,
    /// `-9`: there is no trace record at the index.
    #[error("no trace record")]
    NoTraceData,
    /// `-16`: the device could not be opened.
    #[error("the keychip device cannot be opened")]
    Open,
    /// `-1`: the request failed.
    #[error("the keychip request failed")]
    Failed,
}

impl KeychipError {
    /// The return value of the original API.
    pub const fn code(self) -> i32 {
        match self {
            Self::NotInitialised => -3,
            Self::AlreadyInitialised => -4,
            Self::NotReady | Self::Failed => -1,
            Self::Lost => -12,
            Self::Busy => 1,
            Self::InvalidArgument => -2,
            Self::Signature => -7,
            Self::TraceWrite => -8,
            Self::NoTraceData => -9,
            Self::Open => -16,
        }
    }
}

/// How long the setup may take before it is given up (`amDongleSetupKeychip`).
const SETUP_TIMEOUT_MS: u64 = 45_000;
/// Interval of the health poll (`amDongleUpdate`).
const HEALTH_INTERVAL_MS: u64 = 10_000;

/// Result of one `amDongleSetupKeychip` poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupPoll {
    /// Finished (the original's return 0).
    Done,
    /// Needs more polls (return 2).
    InProgress,
    /// Failed for good (return -1).
    Failed,
}

/// The values of the registry store the setup caches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Properties {
    /// `systemFlag & 1`.
    pub develop: bool,
    /// `gameId`, four raw bytes.
    pub game_id: [u8; 4],
    /// `systemFlag`, low byte.
    pub system_flag: u8,
    /// `modelType`, low byte.
    pub model: u8,
    /// `region`, low byte.
    pub region: u8,
    /// `serverIpIpv4`, the raw dword.
    pub server_ip: u32,
    /// `keychipId`, sixteen raw bytes.
    pub keychip_id: [u8; 16],
}

/// The request in flight and what to do with its reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    /// The availability check of setup state 0/1.
    SetupAvailability,
    /// Geometry of the NVRAM, setup state 2.
    SetupGeometry,
    /// Billing mode, setup state 3.
    SetupBillingMode,
    /// The health poll (`op` 64).
    Health,
}

/// The sources of the property loader, in the order of the original's sub-states (4.2).
#[derive(Debug, Clone, Copy)]
enum Source {
    Develop,
    GameId,
    SystemFlag,
    ModelType,
    Region,
    ServerIp,
    KeychipId,
}

const SOURCES: [Source; 7] = [
    Source::Develop,
    Source::GameId,
    Source::SystemFlag,
    Source::ModelType,
    Source::Region,
    Source::ServerIp,
    Source::KeychipId,
];

/// The C layer state (`off_1408ADBB0`, 2.3).
#[derive(Debug, Default)]
pub struct Link {
    initialised: bool,
    ready: bool,
    in_flight: Option<Op>,
    setup_state: u8,
    property_step: usize,
    setup_started_ms: u64,
    last_check_ms: u64,
    billing_mode: bool,
    properties: Properties,
    /// NVRAM geometry: `(area count, area size)`.
    nvram: (u32, u32),
}

impl Link {
    /// `amDongleInit`: opens the device. The state is cleared first.
    pub fn init(&mut self, keychip: &mut dyn KeychipDriver) -> Result<(), KeychipError> {
        if self.initialised {
            return Err(KeychipError::AlreadyInitialised);
        }
        *self = Self::default();
        keychip.open().map_err(|OpenError| KeychipError::Open)?;
        self.initialised = true;
        Ok(())
    }

    /// `amDongleExit`.
    pub fn exit(&mut self, keychip: &mut dyn KeychipDriver) -> Result<(), KeychipError> {
        if !self.initialised {
            return Err(KeychipError::NotInitialised);
        }
        keychip.close();
        *self = Self::default();
        Ok(())
    }

    /// `initialised && ready`.
    pub fn is_ready(&self) -> bool {
        self.initialised && self.ready
    }

    /// The API checks of the cached getters: initialised, ready, nothing pending.
    fn check(&self) -> Result<(), KeychipError> {
        if !self.initialised {
            Err(KeychipError::NotInitialised)
        } else if !self.ready {
            Err(KeychipError::NotReady)
        } else if self.in_flight.is_some() {
            Err(KeychipError::Busy)
        } else {
            Ok(())
        }
    }

    /// The cached values of the registry store (`amDongleGetGameId` and its siblings).
    pub fn properties(&self) -> Result<Properties, KeychipError> {
        self.check()?;
        Ok(self.properties)
    }

    /// `amDongleIsDevelop`: false while not ready.
    pub fn is_develop(&self) -> bool {
        self.is_ready() && self.properties.develop
    }

    /// `amDongleGetBillingMode`: no pending check, the keychip need only be ready.
    pub fn billing_mode(&self) -> Result<bool, KeychipError> {
        if !self.initialised {
            Err(KeychipError::NotInitialised)
        } else if !self.ready {
            Err(KeychipError::NotReady)
        } else {
            Ok(self.billing_mode)
        }
    }

    /// The NVRAM geometry read at setup: `(area count, area size)`.
    pub fn nvram_geometry(&self) -> (u32, u32) {
        self.nvram
    }

    /// One `amDongleSetupKeychip` poll (4.2). `now_ms` is the monotonic clock.
    pub fn setup_keychip(&mut self, keychip: &mut dyn KeychipDriver, now_ms: u64) -> Result<SetupPoll, KeychipError> {
        if !self.initialised {
            return Err(KeychipError::NotInitialised);
        }
        if (1..=4).contains(&self.setup_state) && now_ms.saturating_sub(self.setup_started_ms) >= SETUP_TIMEOUT_MS {
            self.setup_state = 6;
        }
        match self.setup_state {
            0 | 1 => self.poll_availability(keychip, now_ms),
            2 => self.poll_geometry(keychip),
            3 => self.poll_billing_mode(keychip),
            4 => self.load_properties(keychip, now_ms),
            5 => return Ok(SetupPoll::Done),
            _ => return Ok(SetupPoll::Failed),
        }
        Ok(SetupPoll::InProgress)
    }

    /// Issues `request` for `op`, or polls it when `op` is already in flight. `None` while it is not finished.
    fn step(
        &mut self,
        keychip: &mut dyn KeychipDriver,
        op: Op,
        request: impl FnOnce() -> Request,
    ) -> Option<Result<Vec<u8>, Fail>> {
        if self.in_flight == Some(op) {
            let Poll::Done(result) = keychip.poll() else {
                return None;
            };
            self.in_flight = None;
            return Some(complete(request().code, result));
        }
        match keychip.submit(request()) {
            Ok(()) => self.in_flight = Some(op),
            Err(_) => return Some(Err(Fail::Failed)),
        }
        None
    }

    fn poll_availability(&mut self, keychip: &mut dyn KeychipDriver, now_ms: u64) {
        let issued = self.in_flight.is_some();
        let outcome = self.step(keychip, Op::SetupAvailability, availability_request);
        if !issued && self.in_flight.is_some() {
            self.setup_started_ms = now_ms;
            self.setup_state = 1;
            return;
        }
        match outcome {
            Some(Ok(reply)) if reply.first() == Some(&1) => self.setup_state = 2,
            Some(_) => self.setup_state = 6,
            None => {}
        }
    }

    fn poll_geometry(&mut self, keychip: &mut dyn KeychipDriver) {
        match self.step(keychip, Op::SetupGeometry, || Request {
            code: ioctl::GEOMETRY_NVRAM,
            input: Vec::new(),
            output_len: 8,
        }) {
            Some(Ok(reply)) if reply.len() >= 8 => {
                self.nvram = (le32(&reply, 0), le32(&reply, 4));
                self.setup_state = 3;
            }
            Some(_) => self.setup_state = 6,
            None => {}
        }
    }

    fn poll_billing_mode(&mut self, keychip: &mut dyn KeychipDriver) {
        match self.step(keychip, Op::SetupBillingMode, billing_mode_request) {
            Some(Ok(reply)) => {
                self.billing_mode = reply.len() >= 4 && u16::from_le_bytes([reply[2], reply[3]]) & 1 != 0;
                self.setup_state = 4;
                self.property_step = 0;
            }
            Some(Err(_)) => self.setup_state = 6,
            None => {}
        }
    }

    /// The registry property loader (`0x1402D7450`): several sources per poll until one is busy.
    fn load_properties(&mut self, keychip: &mut dyn KeychipDriver, now_ms: u64) {
        while let Some(&source) = SOURCES.get(self.property_step) {
            match self.load(keychip, source) {
                Ok(()) => self.property_step += 1,
                Err(PropertyError::Busy) => return,
                Err(PropertyError::Failed) => {
                    self.setup_state = 6;
                    return;
                }
            }
        }
        self.ready = true;
        self.setup_state = 5;
        self.last_check_ms = now_ms;
    }

    fn load(&mut self, keychip: &mut dyn KeychipDriver, source: Source) -> Result<(), PropertyError> {
        match source {
            Source::Develop => self.properties.develop = dword(keychip, "systemFlag")? & 1 != 0,
            Source::GameId => self.properties.game_id = binary(keychip, "gameId")?,
            Source::SystemFlag => self.properties.system_flag = dword(keychip, "systemFlag")? as u8,
            Source::ModelType => self.properties.model = dword(keychip, "modelType")? as u8,
            Source::Region => self.properties.region = dword(keychip, "region")? as u8,
            Source::ServerIp => self.properties.server_ip = u32::from_le_bytes(binary(keychip, "serverIpIpv4")?),
            Source::KeychipId => self.properties.keychip_id = binary(keychip, "keychipId")?,
        }
        Ok(())
    }

    /// `amDongleUpdate` (4.3), once per frame while the manager is in state 2 or 3.
    pub fn update(&mut self, keychip: &mut dyn KeychipDriver, now_ms: u64) -> Result<(), KeychipError> {
        if !self.initialised {
            return Err(KeychipError::NotInitialised);
        }
        if !self.ready {
            return Err(KeychipError::Lost);
        }
        match self.in_flight {
            None => {
                if now_ms.saturating_sub(self.last_check_ms) >= HEALTH_INTERVAL_MS {
                    match keychip.submit(availability_request()) {
                        Ok(()) => self.in_flight = Some(Op::Health),
                        Err(_) => self.ready = false,
                    }
                }
            }
            Some(Op::Health) => {
                if let Poll::Done(result) = keychip.poll() {
                    self.in_flight = None;
                    match complete(ioctl::IS_AVAILABLE, result) {
                        Ok(reply) if reply.first() == Some(&1) => self.last_check_ms = now_ms,
                        _ => self.ready = false,
                    }
                }
            }
            Some(_) => {}
        }
        Ok(())
    }

    /// Sends `request` and waits for the reply (the original's synchronous calls block on the event).
    fn call(&mut self, keychip: &mut dyn KeychipDriver, request: Request) -> Result<Vec<u8>, KeychipError> {
        self.check()?;
        let code = request.code;
        keychip.submit(request).map_err(|_| KeychipError::Failed)?;
        let result = loop {
            match keychip.poll() {
                Poll::Pending => std::thread::yield_now(),
                Poll::Done(result) => break result,
            }
        };
        complete(code, result).map_err(Fail::error)
    }

    /// `amDongleBillingGetPlayCount`.
    pub fn play_count(&mut self, keychip: &mut dyn KeychipDriver) -> Result<u32, KeychipError> {
        let reply = self.call(keychip, Request { code: ioctl::GET_PLAY_COUNT, input: Vec::new(), output_len: 4 })?;
        word(&reply)
    }

    /// `amDongleBillingAddPlayCount`: increments and returns the new count.
    pub fn add_play_count(&mut self, keychip: &mut dyn KeychipDriver) -> Result<u32, KeychipError> {
        let request = Request { code: ioctl::ADD_PLAY_COUNT, input: 1u32.to_le_bytes().to_vec(), output_len: 4 };
        word(&self.call(keychip, request)?)
    }

    /// `amDongleBillingGetPlaylimit`.
    pub fn playlimit(&mut self, keychip: &mut dyn KeychipDriver) -> Result<u32, KeychipError> {
        let reply = self.call(keychip, Request { code: ioctl::GET_PLAYLIMIT, input: Vec::new(), output_len: 4 })?;
        word(&reply)
    }

    /// `amDongleBillingUpdatePlaylimit`: the value and its 128-byte signature.
    pub fn update_playlimit(
        &mut self,
        keychip: &mut dyn KeychipDriver,
        value: u32,
        signature: &[u8; 128],
    ) -> Result<(), KeychipError> {
        self.call(keychip, signed_update(ioctl::UPDATE_PLAYLIMIT, value, signature)).map(drop)
    }

    /// `amDongleBillingGetNearfull`: the raw dword of the billing-mode IOCTL.
    pub fn nearfull(&mut self, keychip: &mut dyn KeychipDriver) -> Result<u32, KeychipError> {
        let reply = self.call(keychip, Request { code: ioctl::BILLING_MODE, input: Vec::new(), output_len: 4 })?;
        word(&reply)
    }

    /// `amDongleBillingUpdateNearfull`.
    pub fn update_nearfull(
        &mut self,
        keychip: &mut dyn KeychipDriver,
        value: u32,
        signature: &[u8; 128],
    ) -> Result<(), KeychipError> {
        self.call(keychip, signed_update(ioctl::UPDATE_NEARFULL, value, signature)).map(drop)
    }

    /// `amDongleBillingGetSignaturePubKey`: at most `capacity` bytes.
    pub fn signature_pub_key(
        &mut self,
        keychip: &mut dyn KeychipDriver,
        capacity: usize,
    ) -> Result<Vec<u8>, KeychipError> {
        self.call(keychip, Request { code: ioctl::GET_SIGNATURE_PUB_KEY, input: Vec::new(), output_len: capacity })
    }

    /// `amDongleBillingGetCaCertification`: at most `capacity` bytes.
    pub fn ca_certification(
        &mut self,
        keychip: &mut dyn KeychipDriver,
        capacity: usize,
    ) -> Result<Vec<u8>, KeychipError> {
        self.call(keychip, Request { code: ioctl::GET_CA_CERTIFICATION, input: Vec::new(), output_len: capacity })
    }

    /// `amDongleBillingGetTraceData`: up to `count` records of 60 bytes from `first`.
    pub fn trace_data(
        &mut self,
        keychip: &mut dyn KeychipDriver,
        first: u32,
        count: u32,
    ) -> Result<Vec<[u8; 60]>, KeychipError> {
        let mut input = first.to_le_bytes().to_vec();
        input.extend_from_slice(&count.to_le_bytes());
        let output_len = count as usize * TRACE_RECORD;
        let reply = self.call(keychip, Request { code: ioctl::GET_TRACE_DATA, input, output_len })?;
        Ok(reply.chunks_exact(TRACE_RECORD).filter_map(|record| <[u8; 60]>::try_from(record).ok()).collect())
    }

    /// `amDongleBillingPutTraceData`.
    pub fn put_trace_data(&mut self, keychip: &mut dyn KeychipDriver, record: &[u8; 60]) -> Result<(), KeychipError> {
        self.call(keychip, Request { code: ioctl::PUT_TRACE_DATA, input: record.to_vec(), output_len: 0 }).map(drop)
    }

    /// `amDongleBillingTdRestore`.
    pub fn td_restore(&mut self, keychip: &mut dyn KeychipDriver) -> Result<(), KeychipError> {
        self.call(keychip, Request { code: ioctl::TD_RESTORE, input: Vec::new(), output_len: 0 }).map(drop)
    }

    /// `amDongleBillingTdLogicalErase`: the keychip's "no such record" answer counts as success.
    pub fn td_logical_erase(&mut self, keychip: &mut dyn KeychipDriver, logical_id: u32) -> Result<(), KeychipError> {
        let request =
            Request { code: ioctl::TD_LOGICAL_ERASE, input: logical_id.to_le_bytes().to_vec(), output_len: 0 };
        self.call(keychip, request).map(drop)
    }

    /// `amDongleBillingTdEraseUsedSector`.
    pub fn td_erase_used_sector(&mut self, keychip: &mut dyn KeychipDriver) -> Result<(), KeychipError> {
        self.call(keychip, Request { code: ioctl::TD_ERASE_USED_SECTOR, input: Vec::new(), output_len: 0 }).map(drop)
    }

    /// `amDongleBillingTdEraseAll`.
    pub fn td_erase_all(&mut self, keychip: &mut dyn KeychipDriver) -> Result<(), KeychipError> {
        self.call(keychip, Request { code: ioctl::TD_ERASE_ALL, input: Vec::new(), output_len: 0 }).map(drop)
    }

    /// `amDongleBillingTdGetFreeLogNum`.
    pub fn td_free_log_num(&mut self, keychip: &mut dyn KeychipDriver) -> Result<u32, KeychipError> {
        let reply =
            self.call(keychip, Request { code: ioctl::TD_GET_FREE_LOG_NUM, input: Vec::new(), output_len: 4 })?;
        word(&reply)
    }

    /// `amDongleBillingTdGetLogNum`: the two reply dwords.
    pub fn td_log_num(&mut self, keychip: &mut dyn KeychipDriver) -> Result<(u32, u32), KeychipError> {
        let reply = self.call(keychip, Request { code: ioctl::TD_GET_LOG_NUM, input: Vec::new(), output_len: 8 })?;
        if reply.len() < 8 {
            return Err(KeychipError::Failed);
        }
        Ok((le32(&reply, 0), le32(&reply, 4)))
    }

    /// The NVRAM argument checks of `amDongleReadNvram*` / `WriteNvram*`: the area index in
    /// `[min_index, areaCount - 1]`, a non-empty span inside the area.
    fn check_nvram(&self, index: u8, min_index: u8, address: u32, length: u32) -> Result<(), KeychipError> {
        let (areas, size) = self.nvram;
        let in_area = length != 0 && length <= size && length <= size.saturating_sub(address);
        if index < min_index || u32::from(index) >= areas || address >= size || !in_area {
            return Err(KeychipError::InvalidArgument);
        }
        Ok(())
    }

    /// `amDongleReadNvram` (`min_index` 1) / `amDongleReadNvram*` (internal, `min_index` 0).
    pub fn read_nvram(
        &mut self,
        keychip: &mut dyn KeychipDriver,
        index: u8,
        address: u32,
        length: u32,
        min_index: u8,
    ) -> Result<Vec<u8>, KeychipError> {
        self.check()?;
        self.check_nvram(index, min_index, address, length)?;
        let mut input = vec![index, 0, 0, 0];
        input.extend_from_slice(&address.to_le_bytes());
        input.extend_from_slice(&length.to_le_bytes());
        input.extend_from_slice(&0u32.to_le_bytes());
        self.call(keychip, Request { code: ioctl::READ_NVRAM, input, output_len: length as usize })
    }

    /// `amDongleWriteNvram` / `WriteNvram*`: the address is always 0 (5.3).
    pub fn write_nvram(
        &mut self,
        keychip: &mut dyn KeychipDriver,
        index: u8,
        data: &[u8],
        flag: u8,
        min_index: u8,
    ) -> Result<(), KeychipError> {
        self.check()?;
        let length = u32::try_from(data.len()).map_err(|_| KeychipError::InvalidArgument)?;
        self.check_nvram(index, min_index, 0, length)?;
        let mut input = vec![index, flag, 0, 0];
        input.extend_from_slice(&0u32.to_le_bytes());
        input.extend_from_slice(&length.to_le_bytes());
        input.extend_from_slice(&[0, 0, 0]);
        input.extend_from_slice(data);
        self.call(keychip, Request { code: ioctl::WRITE_NVRAM, input, output_len: 0 }).map(drop)
    }

    /// `amDongleSetIv`.
    pub fn set_iv(&mut self, keychip: &mut dyn KeychipDriver) -> Result<(), KeychipError> {
        self.call(keychip, Request { code: ioctl::SET_IV, input: Vec::new(), output_len: 0 }).map(drop)
    }

    /// `amDongleEncrypt`: one block of 16 bytes.
    pub fn encrypt(&mut self, keychip: &mut dyn KeychipDriver, block: &[u8; 16]) -> Result<[u8; 16], KeychipError> {
        self.block_call(keychip, ioctl::ENCRYPT, block)
    }

    /// `amDongleDecrypt`.
    pub fn decrypt(&mut self, keychip: &mut dyn KeychipDriver, block: &[u8; 16]) -> Result<[u8; 16], KeychipError> {
        self.block_call(keychip, ioctl::DECRYPT, block)
    }

    fn block_call(
        &mut self,
        keychip: &mut dyn KeychipDriver,
        code: u32,
        block: &[u8; 16],
    ) -> Result<[u8; 16], KeychipError> {
        let reply = self.call(keychip, Request { code, input: block.to_vec(), output_len: 16 })?;
        <[u8; 16]>::try_from(reply.as_slice()).map_err(|_| KeychipError::Failed)
    }

    /// `amDongleAuthStart`: the reply of the ALL.Net authentication start (`u16` length + up to 256 bytes).
    pub fn auth_start(&mut self, keychip: &mut dyn KeychipDriver) -> Result<Vec<u8>, KeychipError> {
        let reply =
            self.call(keychip, Request { code: ioctl::AUTH_START, input: Vec::new(), output_len: AUTH_BUFFER })?;
        auth_payload(&reply)
    }

    /// `amDongleAuthPacket`: sends `packet` (up to 256 bytes) and returns the reply payload.
    pub fn auth_packet(&mut self, keychip: &mut dyn KeychipDriver, packet: &[u8]) -> Result<Vec<u8>, KeychipError> {
        if packet.len() > 256 {
            return Err(KeychipError::InvalidArgument);
        }
        let mut input = (packet.len() as u16).to_le_bytes().to_vec();
        input.extend_from_slice(packet);
        input.resize(AUTH_BUFFER, 0);
        let reply = self.call(keychip, Request { code: ioctl::AUTH_PACKET, input, output_len: AUTH_BUFFER })?;
        auth_payload(&reply)
    }
}

/// One trace record.
const TRACE_RECORD: usize = 60;
/// Records the keychip trace log holds (`datamax=7154` of the accounting request).
pub const TRACE_CAPACITY: usize = 7154;
/// `{u16 len; u8 data[256]}`.
const AUTH_BUFFER: usize = 258;

fn auth_payload(reply: &[u8]) -> Result<Vec<u8>, KeychipError> {
    let len = usize::from(u16::from_le_bytes([
        *reply.first().ok_or(KeychipError::Failed)?,
        *reply.get(1).ok_or(KeychipError::Failed)?,
    ]));
    reply.get(2..2 + len).map(<[u8]>::to_vec).ok_or(KeychipError::Failed)
}

fn signed_update(code: u32, value: u32, signature: &[u8; 128]) -> Request {
    let mut input = value.to_le_bytes().to_vec();
    input.extend_from_slice(signature);
    Request { code, input, output_len: 0 }
}

fn availability_request() -> Request {
    Request { code: ioctl::IS_AVAILABLE, input: Vec::new(), output_len: 0 }
}

/// The billing-mode request of setup (the `+0x28 == 1` marker): its reply is reduced by [`complete`].
fn billing_mode_request() -> Request {
    Request { code: ioctl::BILLING_MODE, input: Vec::new(), output_len: 4 }
}

fn le32(bytes: &[u8], at: usize) -> u32 {
    bytes.get(at..at + 4).and_then(|chunk| <[u8; 4]>::try_from(chunk).ok()).map_or(0, u32::from_le_bytes)
}

fn word(reply: &[u8]) -> Result<u32, KeychipError> {
    if reply.len() < 4 { Err(KeychipError::Failed) } else { Ok(le32(reply, 0)) }
}

fn dword(keychip: &mut dyn KeychipDriver, name: &str) -> Result<u32, PropertyError> {
    match keychip.property(name)? {
        Property::Dword(value) => Ok(value),
        Property::Binary(_) => Err(PropertyError::Failed),
    }
}

fn binary<const N: usize>(keychip: &mut dyn KeychipDriver, name: &str) -> Result<[u8; N], PropertyError> {
    match keychip.property(name)? {
        Property::Binary(bytes) => <[u8; N]>::try_from(bytes.as_slice()).map_err(|_| PropertyError::Failed),
        Property::Dword(_) => Err(PropertyError::Failed),
    }
}

/// The amy status of a failed request (`0x8100000x`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fail {
    /// `0x81000002`.
    Failed,
    /// `0x8100000C`.
    Signature,
    /// `0x8100000D`.
    TraceWrite,
    /// `0x81000007`.
    NoTraceData,
}

impl Fail {
    fn error(self) -> KeychipError {
        match self {
            Self::Failed => KeychipError::Failed,
            Self::Signature => KeychipError::Signature,
            Self::TraceWrite => KeychipError::TraceWrite,
            Self::NoTraceData => KeychipError::NoTraceData,
        }
    }
}

/// The completion rules of `amyDongleDriverEnd` (5.3): what a finished request means for each IOCTL.
fn complete(code: u32, result: Result<Vec<u8>, Failure>) -> Result<Vec<u8>, Fail> {
    match (code, result) {
        (ioctl::IS_AVAILABLE, Ok(_)) => Ok(vec![1]),
        (ioctl::IS_AVAILABLE, Err(Failure { last_error: last_error::NOT_READY })) => Ok(vec![0]),
        (ioctl::TD_LOGICAL_ERASE, Err(Failure { last_error: last_error::NO_TRACE })) => Ok(Vec::new()),
        (ioctl::PUT_TRACE_DATA, Err(Failure { last_error: last_error::NO_TRACE })) => Err(Fail::TraceWrite),
        (ioctl::GET_TRACE_DATA, Err(Failure { last_error: last_error::NO_TRACE })) => Err(Fail::NoTraceData),
        (
            ioctl::GET_PLAYLIMIT | ioctl::BILLING_MODE,
            Err(Failure { last_error: last_error::BILLING_SIGNATURE | last_error::BAD_SIGNATURE }),
        )
        | (ioctl::UPDATE_PLAYLIMIT | ioctl::UPDATE_NEARFULL, Err(Failure { last_error: last_error::BAD_SIGNATURE })) => {
            Err(Fail::Signature)
        }
        (_, Ok(reply)) => Ok(reply),
        (_, Err(_)) => Err(Fail::Failed),
    }
}
