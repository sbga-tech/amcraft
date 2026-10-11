//! The ABaaS log manager (`am::abaas::LogManager`, `spec/libam/storage.md` part 6): log roots, the set-up state
//! machine, the operate step and the error-message path. The libabaas protocol (scheduler, sequencers, request
//! cipher) lives in `engine.rs`; it talks to the server through `cx.backend.http`.
//!
//! # Cross-module methods
//!
//! * [`Abaas::update`] (`0x140237750`, M.3): the flow passes the set-up inputs it collects from the platform,
//!   AppImage, storage and ALL.Net modules as [`LogInputs`] (`SystemInfo`, 6.5.3).
//! * [`Abaas::refresh_idle`] (`0x140237800`, called by `0x140254C40`): `idle = (processState != 4)` gates
//!   uploads (6.5.6).
//! * [`Abaas::change_app`] (`0x1402379F0`), [`Abaas::main_folder`], [`Abaas::sub_folder`], [`Abaas::enabled`]:
//!   the APM hand-over (`apm/changeapp.rs`).
//! * [`Abaas::send_error_message`]: the error-message path (6.8); `LogPort::send_error_message` is its caller.
//! * [`Abaas::restart`] (`stop`, `0x1402377C0`): re-arms the set-up after a forced module restart.

mod engine;
mod files;
mod inputs;
mod json;
mod port;
mod wire;

#[cfg(test)]
mod tests;

use libam_api::am_config::AbaasLogConfig;
use libam_api::app_image::{AppImageProperty, PropertyKind};
use libam_api::backup::GameId;

use self::engine::{Engine, Env, MAX_MESSAGE, SysConfig};
use crate::clock::Clock;
use crate::module::{Cx, ErrorCell, Lifecycle, StateWord};
use backend::Fs;

pub use port::LogHandle;

/// The code of the init-error record when `buildSysInfo` fails (6.5.5).
const INIT_ERROR: u16 = 4105;
/// Longest cabinet path (`wchar_t[260]` minus the terminator).
const MAX_PATH_UNITS: usize = 259;
/// Seconds without a live connection after which the upload is reported idle (6.5.6).
const IDLE_AFTER_SECONDS: u64 = 240;
/// `Manager::update` runs this often per operate step (`L+0x210`, 6.5.6).
const UPDATES_PER_FRAME: usize = 4;
/// Bytes kept of an error message before the UTF-8 tail trim (`0x401`, 6.8 step 3).
const KEEP_MESSAGE_BYTES: usize = MAX_MESSAGE + 1;
/// Size of the UTF-8 conversion buffer of an error message (6.8 step 2).
const CONVERT_BUFFER: usize = 6145;

/// What `0x14029D590` hands to the log manager each frame (`update(L, setupReady, appAllowed, SystemInfo)`,
/// 6.5.3). `None` stands for an absent pointer of the `SystemInfo`.
#[derive(Debug, Clone, Copy, Default)]
pub struct LogInputs<'a> {
    /// Dongle, platform, AppImage, storage and ALL.Net managers are all in state 2 or 3.
    pub setup_ready: bool,
    /// The ALL.Net network state is 0 (`allnet+0x4B8 == 0`).
    pub app_allowed: bool,
    /// The keychip game id (`ctx+0x665E`).
    pub game_id: Option<GameId>,
    /// The 11-character short keychip serial (`ctx+0x666A`).
    pub keychip: Option<&'a str>,
    /// The 11-character short board serial, the "main id" (`ctx+0x6600`).
    pub main_id: Option<&'a str>,
    /// The storage manager's base path `Y:\<GAMEID>\` (`storage+0x130`).
    pub base_path: Option<&'a str>,
    /// `(major, minor)` of the selected AppImage, see [`LogInputs::version_of`].
    pub version: Option<(u8, u8)>,
    /// The ALL.Net auth-result word (`allnet+0xC38`), the location id when it fits 16 bits.
    pub place_id: Option<u32>,
}

impl LogInputs<'_> {
    /// The version block of the selected AppImage entry: valid unless it is an option image or not initialised;
    /// `minor = V[5]`, `major = V[6]` truncated to a byte (`version` is `major << 16 | minor << 8 | patch`).
    pub fn version_of(property: Option<AppImageProperty>) -> Option<(u8, u8)> {
        let property = property?;
        (property.kind != PropertyKind::Option && property.initialized)
            .then_some((((property.version >> 16) & 0xFF) as u8, ((property.version >> 8) & 0xFF) as u8))
    }
}

/// `am::abaas::LogManager` (`ctx+488640`).
#[derive(Debug)]
pub struct Abaas {
    /// `setupState` (`L+0x340`) as the state word; the cell holds the init-error record (`L+0x214`).
    life: Lifecycle,
    config: AbaasLogConfig,
    /// The libabaas configuration (`L+0x348`).
    sys: SysConfig,
    /// `L+0x3E8`, cabinet text with the trailing backslash.
    main_folder: String,
    /// `L+0x5F0`.
    sub_folder: String,
    /// `L+0x3E1`.
    allowed: bool,
    /// `L+0x3E2`.
    allowed_changed: bool,
    /// `L+0x3E4`.
    app_flag: bool,
    /// `L+0x3E5`.
    app_flag_changed: bool,
    /// `L+0x3E6`.
    running: bool,
    /// `L+0x7F9`.
    alive: bool,
    /// `L+0x7FA`.
    alive_changed: bool,
    /// `L+0x7FB`.
    upload_idle: bool,
    /// The tick of the last timer reset (`L+0x7FC`), milliseconds.
    timer: u64,
    engine: Engine,
}

impl Default for Abaas {
    fn default() -> Self {
        Self {
            life: Lifecycle::default(),
            config: AbaasLogConfig::default(),
            sys: SysConfig::default(),
            main_folder: String::new(),
            sub_folder: String::new(),
            allowed: false,
            allowed_changed: false,
            app_flag: true,
            app_flag_changed: false,
            running: false,
            alive: false,
            alive_changed: false,
            upload_idle: false,
            timer: 0,
            engine: Engine::default(),
        }
    }
}

/// `MakeSureDirectoryPathExists` (`0x140284FB0`) on a cabinet path: `true` when the directory exists afterwards.
fn make_sure_directory(paths: &dyn Fs, cabinet: &str) -> bool {
    if cabinet.is_empty() || cabinet.encode_utf16().count() > MAX_PATH_UNITS {
        return false;
    }
    let host = paths.map(cabinet);
    std::fs::create_dir_all(&host).is_ok() && host.is_dir()
}

/// Appends `part` after a separating backslash, which is added only when the text does not end in one.
fn push_component(text: &mut String, part: &str) {
    if !text.is_empty() && !text.ends_with('\\') {
        text.push('\\');
    }
    text.push_str(part);
}

/// Ensures a trailing backslash.
fn close_folder(text: &mut String) {
    if !text.ends_with('\\') {
        text.push('\\');
    }
}

/// Number of CP932 bytes of `text`, `None` when it has no CP932 form (`WideCharToMultiByte(932)` failing).
fn cp932_len(text: &str) -> Option<usize> {
    let (bytes, _, failed) = encoding_rs::SHIFT_JIS.encode(text);
    (!failed).then_some(bytes.len())
}

fn game_id_text(game_id: &GameId) -> String {
    String::from_utf16_lossy(game_id.units())
}

/// The UTF-8 text of a wide error message (6.8 steps 2 and 3): `None` when the conversion fails (an unpaired
/// surrogate, or more than the buffer holds); otherwise at most 0x401 bytes with an incomplete tail removed.
fn convert_message(message: &[u16]) -> Option<String> {
    let end = message.iter().position(|&unit| unit == 0).unwrap_or(message.len());
    let text = String::from_utf16(&message[..end]).ok()?;
    if text.len() + 1 > CONVERT_BUFFER {
        return None;
    }
    let bytes = text.as_bytes();
    let kept = &bytes[..bytes.len().min(KEEP_MESSAGE_BYTES)];
    let valid = match std::str::from_utf8(kept) {
        Ok(valid) => valid.len(),
        Err(error) => error.valid_up_to(),
    };
    Some(text[..valid].to_owned())
}

impl Abaas {
    /// The constructed manager.
    pub fn new() -> Self {
        Self::default()
    }

    /// `ready(L)` (6.2): set up, enabled and no init error.
    fn ready(&self) -> bool {
        self.life.state == StateWord::READY && self.config.enable && self.life.error.is_empty()
    }

    /// `OperatingState::enter` (`configure`, `0x1402375A0`): takes `AppConfig.abaas_log`; state 1, or 2 when the
    /// module is disabled.
    pub fn initialize(&mut self, cx: &mut Cx<'_>) {
        self.config = cx.config.abaas_log.clone();
        self.life.begin();
        if !self.config.enable {
            self.life.state = StateWord::READY;
        }
        self.timer = cx.clock.tick_ms();
    }

    /// `0x140237750`, once per Operating frame (6.5.3).
    pub fn update(&mut self, cx: &mut Cx<'_>, inputs: &LogInputs<'_>) {
        if !self.config.enable {
            return;
        }
        if self.life.state == StateWord::READY {
            self.operate(cx, inputs);
        } else {
            self.setup(cx, inputs);
        }
    }

    /// `setup` (`0x140237D40`, 6.5.4).
    fn setup(&mut self, cx: &mut Cx<'_>, inputs: &LogInputs<'_>) {
        if !inputs.setup_ready {
            return;
        }
        if self.build_system_info(inputs) {
            // A failure to create the folder or to configure libabaas is not fatal.
            make_sure_directory(cx.backend.fs.as_ref(), &self.main_folder);
            self.engine.set_config(&self.sys, cx.backend.fs.as_ref());
        } else {
            self.life.error.set(INIT_ERROR, 0);
        }
        self.life.state = StateWord::READY;
    }

    /// `buildSysInfo` (`0x140236FF0`): the libabaas configuration and the main folder.
    fn build_system_info(&mut self, inputs: &LogInputs<'_>) -> bool {
        let (Some(game_id), Some(keychip), Some(main_id), Some(base)) =
            (inputs.game_id, inputs.keychip, inputs.main_id, inputs.base_path)
        else {
            return false;
        };
        let root = self.config.root_path.as_str();
        if base.is_empty() || base.encode_utf16().count() + root.encode_utf16().count() > MAX_PATH_UNITS {
            return false;
        }
        let game_text = game_id_text(&game_id);
        let mut folder = base.to_owned();
        push_component(&mut folder, root);
        close_folder(&mut folder);
        if cp932_len(&game_text).is_none_or(|len| len > 4)
            || cp932_len(keychip).is_none_or(|len| len > 16)
            || cp932_len(main_id).is_none_or(|len| len > 16)
            || cp932_len(&folder).is_none_or(|len| len > MAX_PATH_UNITS)
        {
            return false;
        }
        let (major, minor) = inputs.version.unwrap_or((0, 0));
        self.sys = SysConfig {
            game_id: game_text,
            major,
            minor,
            location: 0,
            keychip: keychip.to_owned(),
            main_id: main_id.to_owned(),
            folder: folder.clone(),
        };
        self.main_folder = folder;
        true
    }

    /// `operate` (`0x140237EE0`, 6.5.6).
    fn operate(&mut self, cx: &mut Cx<'_>, inputs: &LogInputs<'_>) {
        let now = cx.clock.tick_ms();
        let allowed = inputs.app_allowed;
        self.allowed_changed = self.allowed != allowed;
        self.allowed = allowed;
        if self.allowed_changed {
            self.sys.location = inputs.place_id.and_then(|place| u16::try_from(place).ok()).unwrap_or(0);
            self.engine.set_config(&self.sys, cx.backend.fs.as_ref());
        }
        if self.config.use_network && (self.allowed_changed || self.app_flag_changed) {
            self.running = self.allowed && self.app_flag;
            self.engine.allow_execution(self.running, now);
        }
        for _ in 0..UPDATES_PER_FRAME {
            let backend = &mut *cx.backend;
            let mut env = Env { clock: cx.clock, http: backend.http.as_mut(), paths: backend.fs.as_ref() };
            self.engine.update(&mut env);
        }
        let ready = self.ready();
        let count = if ready { self.engine.file_count() } else { -1 };
        let alive = ready && self.engine.alive();
        self.alive_changed = self.alive != alive;
        self.alive = alive;
        if count == 0 {
            self.upload_idle = true;
        } else {
            if self.allowed_changed && self.allowed {
                self.timer = now;
            }
            if self.alive {
                self.upload_idle = false;
            } else {
                if self.alive_changed {
                    self.timer = now;
                }
                self.upload_idle = now.saturating_sub(self.timer) / 1000 >= IDLE_AFTER_SECONDS;
            }
        }
        self.app_flag_changed = false;
    }

    /// `0x140237690` (`finalize`): stops the access and returns to state 0.
    pub fn terminate(&mut self, cx: &mut Cx<'_>) {
        if self.life.state != StateWord::UNINITIALIZED {
            self.clear(cx.clock.tick_ms());
            self.config = AbaasLogConfig::default();
        }
        self.life.end();
    }

    /// `stop` (`0x1402377C0`): after a forced module restart the set-up runs again.
    pub fn restart(&mut self, now: u64) {
        if self.life.state == StateWord::READY && self.config.enable {
            self.clear(now);
            self.life.state = StateWord::INITIALIZED;
        }
    }

    /// `clear` (`0x140237C20`): stops the server access and returns the operate state to its baseline.
    fn clear(&mut self, now: u64) {
        if self.running {
            self.running = false;
            self.engine.allow_execution(false, now);
        }
        self.allowed = false;
        self.allowed_changed = false;
        self.app_flag = true;
        self.app_flag_changed = false;
        self.alive = false;
        self.alive_changed = false;
        self.upload_idle = false;
        self.timer = now;
        self.sys.major = 0;
        self.sys.minor = 0;
        self.sys.location = 0;
        self.life.error.clear();
    }

    /// `setAppFlag(idle)` (`0x140237800`): only while [`ready`](Self::ready).
    pub fn refresh_idle(&mut self, idle: bool) {
        if self.ready() {
            self.app_flag_changed = self.app_flag != idle;
            self.app_flag = idle;
        }
    }

    /// `changeApp(GameId)` (`0x1402379F0`, 6.6.2): rewrites the sub-application folder. `false` when the module
    /// is not set up or the folder could not be created.
    pub fn change_app(&mut self, paths: &dyn Fs, game_id: Option<GameId>) -> bool {
        if !self.ready() {
            return false;
        }
        let Some(game_id) = game_id else {
            self.sub_folder.clear();
            return true;
        };
        let mut folder: String = self.main_folder.chars().take(3).collect();
        push_component(&mut folder, &game_id_text(&game_id));
        push_component(&mut folder, self.config.root_path.as_str());
        close_folder(&mut folder);
        let created = make_sure_directory(paths, &folder);
        self.sub_folder = folder;
        created
    }

    /// `sendErrorMessage` (`0x140237860`, 6.8): converts, truncates and queues the message for the next
    /// upload. Silently ignored while the manager is not ready; the queue result is not reported.
    pub fn send_error_message(&mut self, message: &[u16], clock: &dyn Clock) {
        if !self.ready() {
            return;
        }
        if let Some(text) = convert_message(message) {
            self.engine.set_error_message(&text, clock);
        }
    }

    /// `ctx+489472`.
    pub fn state(&self) -> StateWord {
        self.life.state
    }

    /// `allReady` term: state == 2.
    pub fn is_ready(&self) -> bool {
        self.life.state == StateWord::READY
    }

    /// The error cell (`ctx+489172`): 4105 after a failed set-up.
    pub fn error_cell(&self) -> ErrorCell {
        self.life.error
    }

    /// `isEnabled`: `abaas_log.enable`.
    pub fn enabled(&self) -> bool {
        self.config.enable
    }

    /// The log root and level taken at initialize.
    pub fn config(&self) -> &AbaasLogConfig {
        &self.config
    }

    /// The main folder getter (`0x140237BB0`): `Y:\<GAMEID>\<root_path>\` while ready.
    pub fn main_folder(&self) -> Option<&str> {
        self.ready().then_some(self.main_folder.as_str())
    }

    /// The sub-application folder getter (`0x140237C00`): `None` while empty.
    pub fn sub_folder(&self) -> Option<&str> {
        (!self.sub_folder.is_empty()).then_some(self.sub_folder.as_str())
    }

    /// `isAlive` (`0x140237B00`).
    pub fn is_alive(&self) -> bool {
        self.ready() && self.engine.alive()
    }

    /// `fileCount` (`0x140237B50`): files the server still wants, `-1` when unknown.
    pub fn file_count(&self) -> i32 {
        if self.ready() { self.engine.file_count() } else { -1 }
    }

    /// The upload-idle byte (`L+0x7FB`).
    pub fn upload_idle(&self) -> bool {
        self.upload_idle
    }
}
