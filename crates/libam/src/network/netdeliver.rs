//! NetDeliver (`am::netdeliver::Manager`, `spec/libam/network.md` part D): the manager that starts and stops the
//! download engine ([`Fetcher`]) and publishes its records.

use crate::appimage::{AppImageProperty, PropertyKind};
use crate::config::NetDeliverConfig;
use crate::network::{DELIVERY_SOURCE_CAPACITY, DownloadProgress, ImageInfo, ImageKind, TimeInfo};
use crate::result::{AmError, AmResult};
use crate::time::Tm;

use super::deliver_fetcher::{APP, AuthParams, DownloadUi, DownloadUiParams, Fetcher, Io, Item, OPT, utc_seconds};
use crate::master::Master;
use crate::module::{Cx, ErrorCell, Lifecycle, StateWord};
use crate::registry_keys::MOUNT_KEY;
use backend::{KeyMode, RegValue, Registry};

/// Registry key of the DownloadUi mailbox (`amsDownloadUi*`). [INFERENCE] The binary's storage medium is not
/// recovered (`network.md` L.9.7); a volatile key next to the other system properties is used.
const DOWNLOAD_UI_KEY: &str = r"System\SEGA\SystemProperty\DownloadUi";
/// The stopwatch of the publish step (`elapsed > 1` whole seconds).
const PUBLISH_MS: u64 = 2_000;
/// The idle time-out before the fetcher is started without parameters (D.4.3).
const IDLE_TIMEOUT_S: u64 = 120;
/// Longest `app|option` string and `auth_data` the auth block holds (258 and 1536 bytes with the NUL).
const URI_MAX: usize = 257;
const AUTH_DATA_MAX: usize = 1535;

/// What the manager reads from other modules each frame.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeliverInputs {
    /// Keychip serial text (HTTP user agent, report `serial`).
    pub serial: String,
    /// The system game id.
    pub game_id: String,
    /// The system platform id (three characters).
    pub platform_id: String,
    /// The RTC server-time flag (`amRtcIsServerSync`).
    pub server_time_synced: bool,
    /// `isLanAvailable` of the active network manager (local mode only).
    pub lan_available: bool,
}

/// The eight slots of `am::netdeliver::Param` (D.5) as plain values; the ALL.Net module fills them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeliverAuthParam {
    /// Slot 1: ALL.Net auth was (re)started.
    pub auth_restarted: bool,
    /// Slot 2: the parameters are ready.
    pub ready: bool,
    /// Slot 3: the download-instruction URI is being fetched again.
    pub uri_restarted: bool,
    /// Slots 4 and 5: the last-auth time when valid.
    pub auth_time: Option<Tm>,
    /// Slot 6: line type code (2 -> 2, 8 -> 1, other -> 0).
    pub line_code: u32,
    /// Slot 7: the `auth_data` blob.
    pub auth_data: String,
    /// Slot 8: the `app|option` instruction URIs.
    pub uri: Option<String>,
}

/// `am::netdeliver::LocalParam` (D.5.2).
#[derive(Debug, Clone, Default)]
struct LocalParam {
    network_ready: bool,
    param_set: bool,
    changed: bool,
    key: String,
    start: Tm,
}

impl LocalParam {
    fn as_param(&self) -> DeliverAuthParam {
        DeliverAuthParam {
            auth_restarted: false,
            ready: self.network_ready && self.param_set,
            uri_restarted: self.changed,
            auth_time: self.network_ready.then_some(self.start),
            line_code: 2,
            auth_data: String::new(),
            uri: Some(self.key.clone()),
        }
    }
}

/// The DownloadUi registry mailbox.
struct RegistryDownloadUi<'a> {
    registry: &'a mut dyn Registry,
}

impl DownloadUi for RegistryDownloadUi<'_> {
    fn write(&mut self, params: &DownloadUiParams) -> bool {
        let values = [
            ("InitialState", params.initial_state),
            ("DownloadVersion", params.download_version),
            ("UpdateVersion", params.update_version),
            ("IsServer", params.is_server),
            ("ShowWarning", params.show_warning),
        ];
        values.iter().all(|(name, value)| {
            self.registry.write(DOWNLOAD_UI_KEY, KeyMode::CreateVolatile, name, &RegValue::Dword(*value)).is_ok()
        })
    }
}

/// The NetDeliver manager (`composite+1912`).
#[derive(Debug)]
pub struct NetDeliver {
    life: Lifecycle,
    config: NetDeliverConfig,
    fetcher: Option<Fetcher>,
    /// `paramValid` (`N+36`).
    param_valid: bool,
    /// `running` (`N+9160`): the fetcher is resumed.
    running: bool,
    /// Request-machine phase and request (`S0`, `S1`).
    phase: u8,
    request: u8,
    auth: AuthParams,
    /// `C+312` and the composite stopwatch.
    idle_armed: bool,
    idle_since_ms: u64,
    publish_since_ms: u64,
    /// "reason 2" (`N+352`): the engine asked for the download UI.
    reason2: bool,
    /// `N+348`: 0 unknown, 1 nothing to do, 2 exit required.
    exit_phase: u8,
    time: [TimeInfo; 2],
    images: [ImageInfo; 2],
    local: LocalParam,
}

impl Default for NetDeliver {
    fn default() -> Self {
        Self::new()
    }
}

/// An all-zero published image record.
fn empty_image_info() -> ImageInfo {
    ImageInfo {
        raw_status: 0,
        property: property_of(&Item::default()),
        progress_a: DownloadProgress { first: u32::MAX, second: u32::MAX },
        progress_b: DownloadProgress { first: u32::MAX, second: u32::MAX },
    }
}

/// `struct tm` of a 8-byte image timestamp `{u16 year, mon, day, hour, min, sec, pad}`.
fn tm_of(stamp: [u8; 8]) -> Tm {
    let year = i32::from(u16::from_le_bytes([stamp[0], stamp[1]]));
    let month = i32::from(stamp[2]);
    Tm {
        sec: i32::from(stamp[6]),
        min: i32::from(stamp[5]),
        hour: i32::from(stamp[4]),
        mday: i32::from(stamp[3]),
        mon: if month == 0 { 0 } else { month - 1 },
        year: if year < 1900 { 0 } else { year - 1900 },
        ..Tm::default()
    }
}

/// The `AppImageProperty` of one image item (`0x140243610`, D.6.3).
fn property_of(item: &Item) -> AppImageProperty {
    let option = item.kind == 1;
    let mut option_id = [0u16; 5];
    if option {
        for (slot, byte) in option_id.iter_mut().zip(item.option_id) {
            *slot = u16::from(byte);
        }
    }
    AppImageProperty {
        kind: if option { PropertyKind::Option } else { PropertyKind::App },
        initialized: !option,
        version: if option { 0 } else { item.version },
        option_id,
        creation: tm_of(item.timestamp),
    }
}

/// The raw statuses `0x140243100` keeps; everything else becomes 0 (D.9.7).
fn published_status(status: u32) -> i32 {
    match status {
        1..=3 | 100..=103 | 110 | 120..=124 | 130 | 200 | 300..=305 | 400..=403 | 500 | 501 | 600..=603 | 900..=902 => {
            status as i32
        }
        _ => 0,
    }
}

impl NetDeliver {
    /// The constructed manager.
    pub fn new() -> Self {
        let zero = TimeInfo { order: Tm::default(), release: Tm::default() };
        Self {
            life: Lifecycle::default(),
            config: NetDeliverConfig::default(),
            fetcher: None,
            param_valid: false,
            running: false,
            phase: 0,
            request: 0,
            auth: AuthParams { auth_time: -1, ..AuthParams::default() },
            idle_armed: false,
            idle_since_ms: 0,
            publish_since_ms: 0,
            reason2: false,
            exit_phase: 0,
            time: [zero.clone(), zero],
            images: [empty_image_info(), empty_image_info()],
            local: LocalParam::default(),
        }
    }

    /// `0x140242040` (composite start): takes `AppConfig.netdeliver`; a disabled manager is ready at once.
    pub fn initialize(&mut self, cx: &mut Cx<'_>) {
        *self = Self::new();
        self.config = cx.config.netdeliver;
        self.life.begin();
        if !self.config.enable {
            self.life.finish_setup();
            return;
        }
        let civil = cx.clock.local_now().civil();
        self.local.start = Tm {
            sec: civil.second as i32,
            min: civil.minute as i32,
            hour: civil.hour as i32,
            mday: civil.day as i32,
            mon: civil.month as i32 - 1,
            year: civil.year as i32 - 1900,
            ..Tm::default()
        };
    }

    /// `0x1402421D0` and the step `0x1402443B0`, once per frame inside the composite update. `composite_done` is
    /// "the composite state is 2 or 3"; `param` is the ALL.Net `Param` (ignored in local mode).
    pub fn update(&mut self, cx: &mut Cx<'_>, composite_done: bool, inputs: &DeliverInputs, param: &DeliverAuthParam) {
        if !self.config.enable || self.life.state == StateWord::UNINITIALIZED {
            return;
        }
        let now_ms = cx.clock.tick_ms();
        if self.life.state == StateWord::INITIALIZED {
            let is_done = match cx.backend.registry.read(DOWNLOAD_UI_KEY, KeyMode::CreateVolatile, "IsDone") {
                Ok(RegValue::Dword(value)) => value,
                _ => 0,
            };
            self.fetcher = Some(Fetcher::new(is_done));
            self.publish_since_ms = now_ms;
            self.life.finish_setup();
        }
        let local_param;
        let param = if self.config.local {
            self.local.network_ready = inputs.lan_available;
            local_param = self.local.as_param();
            &local_param
        } else {
            param
        };
        if composite_done {
            self.start_stop_rule(now_ms, param);
        }
        self.request_machine();
        self.run_fetcher(cx, inputs, now_ms);
        if self.param_valid
            && self.running
            && self.phase == 0
            && now_ms.saturating_sub(self.publish_since_ms) >= PUBLISH_MS
        {
            self.publish();
            self.publish_since_ms = now_ms;
        }
    }

    /// D.4.3: when to start and stop the fetcher.
    fn start_stop_rule(&mut self, now_ms: u64, param: &DeliverAuthParam) {
        if self.running {
            let reason = if param.auth_restarted {
                1
            } else if param.uri_restarted {
                2
            } else {
                return;
            };
            let _ = reason;
            self.request = 2;
            self.idle_armed = true;
            self.idle_since_ms = now_ms;
            return;
        }
        if !param.ready {
            if self.config.local {
                return;
            }
            if !self.idle_armed {
                self.idle_armed = true;
                self.idle_since_ms = now_ms;
                return;
            }
            if now_ms.saturating_sub(self.idle_since_ms) / 1000 <= IDLE_TIMEOUT_S {
                return;
            }
            self.idle_armed = false;
        }
        self.copy_param(param);
        if self.config.local {
            self.local.changed = false;
        }
        self.request = 1;
    }

    /// `0x1402439C0`: the 1816-byte auth block.
    fn copy_param(&mut self, param: &DeliverAuthParam) {
        let auth_time = param.auth_time.and_then(|tm| tm.to_date_time()).map_or(-1, utc_seconds);
        let line_type = match param.line_code {
            2 => 2,
            8 => 1,
            _ => 0,
        };
        self.auth = AuthParams {
            auth_time,
            line_type,
            uri: param.uri.as_deref().unwrap_or_default().chars().take(URI_MAX).collect(),
            auth_data: param.auth_data.chars().take(AUTH_DATA_MAX).collect(),
            aws: self.config.use_aws_signature,
        };
        self.param_valid = true;
    }

    /// The request machine of D.4.2 over the fetcher's mailbox.
    fn request_machine(&mut self) {
        let Some(fetcher) = self.fetcher.as_mut() else { return };
        match (self.phase, self.request) {
            (0, 0) => {
                if !self.param_valid && self.running {
                    self.request = 2;
                }
            }
            (0, 1) => {
                self.request = 0;
                if self.param_valid && !self.running {
                    let mut result = fetcher.set_auth_params(self.auth.clone());
                    if result == 0 {
                        result = fetcher.request_resume();
                    }
                    if result == 0 {
                        self.phase = 1;
                    } else {
                        self.request = 1;
                    }
                }
            }
            (0, 2) => {
                self.request = 0;
                if self.running {
                    if fetcher.request_pause() == 0 {
                        self.phase = 2;
                    } else {
                        self.request = 2;
                    }
                }
            }
            (1 | 2, _) => {
                fetcher.pump();
                let result = fetcher.result();
                if result != 1 {
                    let starting = self.phase == 1;
                    if result == 0 {
                        self.running = starting;
                    } else if starting {
                        if self.param_valid && self.request == 0 {
                            self.request = 1;
                        }
                    } else if self.request == 0 {
                        self.request = 2;
                    }
                    self.phase = 0;
                }
            }
            _ => {}
        }
    }

    /// One worker iteration of the engine, against the backend.
    fn run_fetcher(&mut self, cx: &mut Cx<'_>, inputs: &DeliverInputs, now_ms: u64) {
        let Some(fetcher) = self.fetcher.as_mut() else { return };
        let backend = &mut *cx.backend;
        let Ok(RegValue::Text(mut base)) = backend.registry.read(MOUNT_KEY, KeyMode::CreateVolatile, "AMFS") else {
            return;
        };
        if !base.ends_with('\\') {
            base.push('\\');
        }
        let app_loader_count = Master::app_loader_count(&mut *backend.registry);
        let now = cx.clock.local_now();
        let mut ui = RegistryDownloadUi { registry: &mut *backend.registry };
        let mut io = Io {
            http: &mut *backend.http,
            paths: &*backend.fs,
            base: &base,
            now_ms,
            now,
            serial: &inputs.serial,
            game_id: &inputs.game_id,
            platform_id: &inputs.platform_id,
            server_time_synced: inputs.server_time_synced,
            app_loader_count,
            download_ui: &mut ui,
        };
        fetcher.step(&mut io);
    }

    /// `0x140242E00..0x140243000`: the exit reason and the status, time and image records.
    fn publish(&mut self) {
        let Some(fetcher) = self.fetcher.as_ref() else { return };
        self.reason2 = matches!(fetcher.exit_reason(), Ok(2));
        self.exit_phase = match fetcher.exit_reason() {
            Ok(0) => 1,
            Ok(_) => 2,
            Err(_) => 0,
        };
        for kind in [APP, OPT] {
            let record = fetcher.record(kind);
            self.time[kind] = TimeInfo { order: tm_of(record.order_time), release: tm_of(record.release_time) };
            let pair = |index: usize| DownloadProgress { first: record.pairs[index].0, second: record.pairs[index].1 };
            self.images[kind] = ImageInfo {
                raw_status: published_status(record.status),
                property: property_of(&record.item),
                progress_a: pair(1),
                progress_b: pair(3),
            };
        }
    }

    /// `0x140242140`: stops the engine and zeroes the configuration copy.
    pub fn terminate(&mut self, cx: &mut Cx<'_>) {
        let _ = cx;
        if let Some(mut fetcher) = self.fetcher.take() {
            fetcher.exit();
        }
        self.config = NetDeliverConfig::default();
        self.param_valid = false;
        self.running = false;
        self.life.end();
    }

    /// Module restart (`0x140242380`): back to state 1 (2 when disabled), parameters and records cleared.
    pub fn restart(&mut self) {
        if self.life.state == StateWord::UNINITIALIZED {
            return;
        }
        if let Some(mut fetcher) = self.fetcher.take() {
            fetcher.exit();
        }
        let config = self.config;
        let local = std::mem::take(&mut self.local);
        *self = Self::new();
        self.config = config;
        self.local = local;
        self.life.begin();
        if !config.enable {
            self.life.finish_setup();
        }
    }

    /// `NetDeliveryPort::is_available` (`0x140242410`).
    pub fn is_available(&self) -> bool {
        self.config.enable
    }

    /// "reason 2" (`0x1402424D0`): the engine wants the download UI.
    pub fn download_ui(&self) -> bool {
        self.reason2
    }

    /// `0x140242480 == 1`: nothing to install or an error latched (or disabled); lets LAN install start.
    pub fn settled(&self) -> bool {
        !self.config.enable || !self.life.error.is_empty() || (self.life.state.is_done() && self.exit_phase == 1)
    }

    /// `0x140242440 && 0x140242480 == 2`: the fetcher runs and the engine requires an exit (NeedToExit kind 2).
    pub fn needs_exit(&self) -> bool {
        self.running && self.exit_phase == 2
    }
    /// The facade gate of `time_info` / `image_info`: enabled and state 2.
    fn publishing(&self) -> bool {
        self.config.enable && self.life.state == StateWord::READY
    }

    /// `time_info(kind)` (`0x140242610`).
    pub fn time_info(&self, kind: ImageKind) -> Option<TimeInfo> {
        self.publishing().then(|| self.time[usize::from(kind == ImageKind::Option)].clone())
    }

    /// `image_info(kind)` (`0x140242670`).
    pub fn image_info(&self, kind: ImageKind) -> Option<ImageInfo> {
        self.publishing().then(|| self.images[usize::from(kind == ImageKind::Option)].clone())
    }

    /// `set_local_param` (`0x140244250`): needs `enable` and `local`, else `Result 6`.
    pub fn set_local_param(&mut self, source: &[u16; DELIVERY_SOURCE_CAPACITY]) -> AmResult<()> {
        if !(self.config.enable && self.config.local) {
            return Err(AmError::with_message(6, "unsupport function\n"));
        }
        let length = source.iter().position(|&unit| unit == 0).unwrap_or(source.len());
        let key = String::from_utf16_lossy(&source[..length]);
        self.local.changed = key != self.local.key;
        self.local.key = key;
        self.local.param_set = true;
        Ok(())
    }

    /// The error cell.
    pub fn error_cell(&self) -> ErrorCell {
        self.life.error
    }

    /// The state word.
    pub fn state(&self) -> StateWord {
        self.life.state
    }
}
